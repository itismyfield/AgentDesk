use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use super::{PreviousSessionCandidate, ResumeRebindError};
use crate::services::claude_tui::transcript_tail::{
    claude_project_dir_candidates_for_cwd, claude_transcript_path, default_claude_home,
};

const WINDOW: usize = 64 * 1024;
const CACHE_TTL: Duration = Duration::from_secs(10);
const DISCOVERY_BUDGET: Duration = Duration::from_secs(2);

#[derive(Clone, Default)]
struct Preview {
    title: String,
    tui: bool,
}

struct CachedPreview {
    modified: SystemTime,
    len: u64,
    cached_at: Instant,
    preview: Preview,
}

static PREVIEWS: OnceLock<Mutex<HashMap<PathBuf, CachedPreview>>> = OnceLock::new();

#[cfg(test)]
tokio::task_local! { pub(super) static TEST_HOME: PathBuf; }

pub(crate) fn claude_home() -> Option<PathBuf> {
    #[cfg(test)]
    if let Ok(home) = TEST_HOME.try_with(Clone::clone) {
        return Some(home);
    }
    default_claude_home()
}

pub(super) fn scoped_worktrees(current_cwd: &str) -> Vec<PathBuf> {
    let current = Path::new(current_cwd);
    let mut worktrees = vec![current.to_path_buf()];
    if let (Some(parent), Some(name)) = (
        current.parent(),
        current.file_name().and_then(|n| n.to_str()),
    ) && let Ok(entries) = fs::read_dir(parent)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path != current
                && path.is_dir()
                && path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(worktree_lineage_stem)
                    == Some(worktree_lineage_stem(name))
            {
                worktrees.push(path);
            }
        }
    }
    worktrees
}

pub(super) fn resolve_cwd(
    current: Option<&str>,
    requested: Option<&str>,
    id: &str,
    home: Option<&Path>,
) -> Result<String, ResumeRebindError> {
    if uuid::Uuid::parse_str(id).is_err() {
        return Err(ResumeRebindError::TargetTranscriptMissing(
            "Invalid Claude session_id UUID; choose a /resume session or enter a valid UUID."
                .to_string(),
        ));
    }
    if current.is_none() && requested.is_none() {
        return Err(ResumeRebindError::TargetTranscriptMissing(
            "Target cwd is unknown; supply cwd with session_id to locate its transcript."
                .to_string(),
        ));
    }
    let mut found = current.map(scoped_worktrees).unwrap_or_default();
    if let Some(cwd) = requested {
        found.insert(0, PathBuf::from(cwd));
    }
    let mut matches = Vec::new();
    let mut lookup_error = None;
    for cwd in found {
        let path = match claude_transcript_path(&cwd, id, home) {
            Ok(path) => path,
            Err(error) => {
                lookup_error = Some(error);
                continue;
            }
        };
        let meta = match fs::metadata(&path) {
            Ok(meta) => meta,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    lookup_error = Some(format!(
                        "Cannot inspect transcript {}: {error}",
                        path.display()
                    ));
                }
                continue;
            }
        };
        if meta.is_file() {
            if Some(cwd.as_path()) == requested.map(Path::new) {
                return Ok(cwd.to_string_lossy().into_owned());
            }
            matches.push((meta.modified().unwrap_or(SystemTime::UNIX_EPOCH), cwd));
        }
    }
    matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    if requested.is_none()
        && let Some((_, cwd)) = matches.first()
    {
        return Ok(cwd.to_string_lossy().into_owned());
    }
    let hint = matches.first().map_or_else(
        || lookup_error.unwrap_or_else(|| "No transcript was found in this channel's worktree lineage. Check the session id and Claude config home.".to_string()),
        |(_, cwd)| format!("Transcript is available at cwd={}; retry with that cwd or omit cwd.", cwd.display()),
    );
    Err(ResumeRebindError::TargetTranscriptMissing(format!(
        "Claude transcript unavailable for session_id={id}{}: {hint}",
        requested.map_or(String::new(), |cwd| format!(" at cwd={cwd}")),
    )))
}

pub(super) async fn resolve_cwd_off_runtime(
    current: Option<String>,
    requested: Option<String>,
    id: String,
) -> Result<String, ResumeRebindError> {
    let home = claude_home();
    tokio::task::spawn_blocking(move || {
        resolve_cwd(
            current.as_deref(),
            requested.as_deref(),
            &id,
            home.as_deref(),
        )
    })
    .await
    .map_err(|e| ResumeRebindError::Filesystem(format!("transcript lookup failed: {e}")))?
}

pub(crate) struct ResumeChoice {
    pub(crate) label: String,
    pub(crate) session_id: String,
}

pub(crate) fn choices(
    cwd: &str,
    current_id: Option<&str>,
    live_bound: &HashSet<String>,
    home: Option<&Path>,
    partial: &str,
) -> Vec<ResumeChoice> {
    let deadline = Instant::now() + DISCOVERY_BUDGET;
    let mut files = Vec::new();
    for worktree in scoped_worktrees(cwd) {
        let Ok(projects) = claude_project_dir_candidates_for_cwd(&worktree, home) else {
            continue;
        };
        for project in projects {
            let Ok(entries) = fs::read_dir(project) else {
                continue;
            };
            for entry in entries.flatten() {
                if Instant::now() >= deadline {
                    break;
                }
                let path = entry.path();
                let Some(id) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if path.extension().and_then(|s| s.to_str()) != Some("jsonl")
                    || uuid::Uuid::parse_str(id).is_err()
                    || Some(id) == current_id
                    || live_bound.contains(id)
                {
                    continue;
                }
                let Ok(meta) = entry.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                files.push((
                    meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    path.clone(),
                    meta.len(),
                    PreviousSessionCandidate {
                        cwd: worktree.to_string_lossy().into_owned(),
                        session_id: id.to_string(),
                    },
                ));
            }
        }
    }
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut seen = HashSet::new();
    let partial = partial.to_lowercase();
    let mut result = Vec::new();
    for (modified, path, len, candidate) in files {
        if result.len() == 25 || Instant::now() >= deadline {
            break;
        }
        if !seen.insert(candidate.session_id.clone()) {
            continue;
        }
        let preview = preview(&path, modified, len);
        if !preview.tui {
            continue;
        }
        let worktree = Path::new(&candidate.cwd)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let age = SystemTime::now()
            .duration_since(modified)
            .unwrap_or_default()
            .as_secs();
        let relative = if age < 60 {
            "방금".to_string()
        } else if age < 3600 {
            format!("{}분 전", age / 60)
        } else if age < 86400 {
            format!("{}시간 전", age / 3600)
        } else {
            format!("{}일 전", age / 86400)
        };
        let title = if preview.title.is_empty() {
            candidate.session_id.chars().take(8).collect()
        } else {
            preview.title
        };
        let label = format!(
            "{title} · {relative} · {}",
            worktree.chars().take(35).collect::<String>()
        );
        if !label.to_lowercase().contains(&partial) && !candidate.session_id.contains(&partial) {
            continue;
        }
        result.push(ResumeChoice {
            label,
            session_id: candidate.session_id,
        });
    }
    result
}

fn preview(path: &Path, modified: SystemTime, len: u64) -> Preview {
    let cache = PREVIEWS.get_or_init(Mutex::default);
    if let Ok(guard) = cache.lock()
        && let Some(cached) = guard.get(path)
        && cached.modified == modified
        && cached.len == len
        && cached.cached_at.elapsed() < CACHE_TTL
    {
        return cached.preview.clone();
    }
    let result = File::open(path).and_then(read_preview).unwrap_or_default();
    if let Ok(mut guard) = cache.lock() {
        guard.retain(|_, entry| entry.cached_at.elapsed() < CACHE_TTL);
        if guard.len() >= 128 {
            guard.clear();
        }
        guard.insert(
            path.to_path_buf(),
            CachedPreview {
                modified,
                len,
                cached_at: Instant::now(),
                preview: result.clone(),
            },
        );
    }
    result
}

// Read at most two windows; incomplete edge records never trigger a longer read.
fn read_preview(mut reader: impl Read + Seek) -> std::io::Result<Preview> {
    let len = reader.seek(SeekFrom::End(0))?;
    reader.seek(SeekFrom::Start(0))?;
    let mut head = Vec::new();
    reader.by_ref().take(WINDOW as u64).read_to_end(&mut head)?;
    let mut tail = Vec::new();
    if len > WINDOW as u64 {
        reader.seek(SeekFrom::Start(
            len.saturating_sub(WINDOW as u64).max(WINDOW as u64),
        ))?;
        reader.take(WINDOW as u64).read_to_end(&mut tail)?;
    }
    let mut title = None;
    let mut prompt = None;
    let mut entrypoint = None;
    for (bytes, skip_first) in [(&head, false), (&tail, len > (2 * WINDOW) as u64)] {
        for line in bytes.split(|b| *b == b'\n').skip(usize::from(skip_first)) {
            let Ok(record) = serde_json::from_slice::<serde_json::Value>(line) else {
                continue;
            };
            if entrypoint.is_none() {
                entrypoint = record["entrypoint"].as_str().map(str::to_owned);
            }
            if record["type"] == "ai-title" {
                title = record["aiTitle"]
                    .as_str()
                    .or(record["title"].as_str())
                    .map(short_title);
            }
            if prompt.is_none() && record["type"] == "user" {
                let content = &record["message"]["content"];
                prompt = content
                    .as_str()
                    .or_else(|| {
                        content.as_array()?.iter().find_map(|block| {
                            if block["type"] == "text" {
                                block["text"].as_str()
                            } else {
                                None
                            }
                        })
                    })
                    .map(short_title);
            }
        }
    }
    Ok(Preview {
        title: title.or(prompt).unwrap_or_default(),
        tui: entrypoint.map_or(len <= WINDOW as u64, |value| !value.starts_with("sdk-")),
    })
}

fn short_title(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(40)
        .collect()
}

/// Strip a managed worktree timestamp so sibling discovery stays within the same channel lineage.
pub(super) fn worktree_lineage_stem(dir_name: &str) -> &str {
    let bytes = dir_name.as_bytes();
    // Only a shaped ASCII suffix can be removed from a UTF-8 directory name.
    if bytes.len() > 16 {
        let tb = &bytes[bytes.len() - 16..];
        let shaped = tb[0] == b'-'
            && tb[9] == b'-'
            && tb[1..9].iter().all(u8::is_ascii_digit)
            && tb[10..16].iter().all(u8::is_ascii_digit);
        if shaped {
            return &dir_name[..dir_name.len() - 16];
        }
    }
    dir_name
}

/// Discover on a blocking worker while preserving the channel lineage and live-binding exclusions.
pub(super) async fn discover_previous_claude_session_off_runtime(
    current_cwd: Option<String>,
    current_session_id: Option<String>,
    live_bound: std::collections::HashSet<String>,
    claude_home: Option<std::path::PathBuf>,
) -> Result<Option<PreviousSessionCandidate>, ResumeRebindError> {
    tokio::task::spawn_blocking(move || {
        discover_previous_claude_session_scoped(
            current_cwd.as_deref(),
            current_session_id.as_deref(),
            &live_bound,
            claude_home.as_deref(),
        )
    })
    .await
    .map_err(|error| {
        ResumeRebindError::Filesystem(format!("previous-session discovery task failed: {error}"))
    })
}

pub(crate) fn discover_previous_claude_session_scoped(
    current_cwd: Option<&str>,
    current_session_id: Option<&str>,
    live_bound: &std::collections::HashSet<String>,
    claude_home: Option<&std::path::Path>,
) -> Option<PreviousSessionCandidate> {
    let worktrees = scoped_worktrees(current_cwd?);

    let empty_exclude = std::collections::HashSet::new();
    let mut best: Option<(std::time::SystemTime, PreviousSessionCandidate)> = None;

    for worktree in worktrees {
        let transcripts =
            crate::services::claude_tui::transcript_tail::claude_transcripts_for_cwd_since(
                &worktree,
                std::time::UNIX_EPOCH,
                claude_home,
                &empty_exclude,
            );
        for transcript in transcripts {
            let Some(session_id) = transcript
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string)
            else {
                continue;
            };
            // Skip the channel's own current binding and any live-bound session.
            if Some(session_id.as_str()) == current_session_id || live_bound.contains(&session_id) {
                continue;
            }
            let Ok(modified) = std::fs::metadata(&transcript).and_then(|meta| meta.modified())
            else {
                continue;
            };
            let candidate = PreviousSessionCandidate {
                cwd: worktree.to_string_lossy().to_string(),
                session_id,
            };
            match &best {
                Some((best_mtime, _)) if *best_mtime >= modified => {}
                _ => best = Some((modified, candidate)),
            }
        }
    }

    best.map(|(_, candidate)| candidate)
}

#[cfg(test)]
pub(super) mod tests;
