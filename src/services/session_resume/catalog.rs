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

#[cfg(test)]
pub(crate) async fn scope_claude_home_for_tests<F: std::future::Future>(
    home: PathBuf,
    future: F,
) -> F::Output {
    TEST_HOME.scope(home, future).await
}

#[cfg(test)]
static PREVIEW_DELAYS: OnceLock<Mutex<HashMap<PathBuf, Duration>>> = OnceLock::new();

#[cfg(test)]
pub(crate) struct PreviewDelay(PathBuf);

#[cfg(test)]
impl Drop for PreviewDelay {
    fn drop(&mut self) {
        PREVIEW_DELAYS
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .remove(&self.0);
    }
}

#[cfg(test)]
pub(crate) fn delay_previews_for_tests(root: PathBuf, delay: Duration) -> PreviewDelay {
    PREVIEW_DELAYS
        .get_or_init(Mutex::default)
        .lock()
        .unwrap()
        .insert(root.clone(), delay);
    PreviewDelay(root)
}

pub(crate) fn claude_home() -> Option<PathBuf> {
    #[cfg(test)]
    if let Ok(home) = TEST_HOME.try_with(Clone::clone) {
        return Some(home);
    }
    default_claude_home()
}

pub(super) fn scoped_worktrees(current_cwd: &str) -> Vec<PathBuf> {
    scoped_worktree_iter(current_cwd, None).collect()
}

fn scoped_worktree_iter(
    current_cwd: &str,
    deadline: Option<Instant>,
) -> impl Iterator<Item = PathBuf> + '_ {
    let current = Path::new(current_cwd);
    let entries = std::iter::once(current.parent())
        .filter_map(|parent| fs::read_dir(parent?).ok())
        .flatten();
    std::iter::once(current.to_path_buf())
        .chain(scoped_worktree_entries(current, entries, deadline))
}

// Check raw entries before filtering can consume them without yielding a candidate.
fn before_deadline<T>(
    entries: impl Iterator<Item = T>,
    deadline: Option<Instant>,
) -> impl Iterator<Item = T> {
    entries.take_while(move |_| deadline.is_none_or(|end| Instant::now() < end))
}

fn scoped_worktree_entries<'a>(
    current: &'a Path,
    entries: impl Iterator<Item = std::io::Result<fs::DirEntry>> + 'a,
    deadline: Option<Instant>,
) -> impl Iterator<Item = PathBuf> + 'a {
    before_deadline(entries, deadline).filter_map(move |entry| {
        let path = entry.ok()?.path();
        let current_name = current.file_name()?.to_str()?;
        let name = path.file_name()?.to_str()?;
        (path != current
            && worktree_lineage_stem(name) == worktree_lineage_stem(current_name)
            && path.is_dir())
        .then_some(path)
    })
}

type TranscriptMatch = (SystemTime, PathBuf, PathBuf);

/// Stat `<home>/projects/<cwd-key>/<id>.jsonl` for each cwd, returning (mtime, cwd, path) hits.
fn scan_home(
    cwds: &[PathBuf],
    id: &str,
    home: Option<&Path>,
) -> (Vec<TranscriptMatch>, Option<String>) {
    let mut matches = Vec::new();
    let mut lookup_error = None;
    for cwd in cwds {
        let path = match claude_transcript_path(cwd, id, home) {
            Ok(path) => path,
            Err(error) => {
                lookup_error = Some(error);
                continue;
            }
        };
        match fs::metadata(&path) {
            Ok(meta) if meta.is_file() => matches.push((
                meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                cwd.clone(),
                path,
            )),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                lookup_error = Some(format!(
                    "Cannot inspect transcript {}: {error}",
                    path.display()
                ))
            }
        }
    }
    matches.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    (matches, lookup_error)
}

fn same_dir(a: &Path, b: &Path) -> bool {
    a == b
        || fs::canonicalize(a)
            .ok()
            .zip(fs::canonicalize(b).ok())
            .is_some_and(|(a, b)| a == b)
}

/// Copy a transcript that exists in exactly one other home into the same cwd key of the active home.
/// Returns whether a copy was made; several homes holding the id are refused rather than chosen.
fn adopt_from_other_home(
    cwds: &[PathBuf],
    requested: Option<&str>,
    id: &str,
    home: Option<&Path>,
    others: &[PathBuf],
) -> Result<bool, String> {
    let active = home.map(Path::to_path_buf).or_else(default_claude_home);
    let mut homes: Vec<&PathBuf> = Vec::new();
    let mut found = Vec::new();
    for other in others {
        if active
            .as_deref()
            .is_some_and(|active| same_dir(active, other))
            || homes.iter().any(|seen| same_dir(seen, other))
        {
            continue;
        }
        let (matches, _) = scan_home(cwds, id, Some(other));
        if !matches.is_empty() {
            homes.push(other);
            found.extend(matches.into_iter().map(|hit| (other, hit)));
        }
    }
    let id8: String = id.chars().take(8).collect();
    if homes.len() > 1 {
        return Err(format!(
            "세션 {id8} 트랜스크립트가 Claude config home {}곳에 있어 자동으로 고르지 않습니다. 하나만 남기거나 활성 home에 직접 두세요.",
            homes.len()
        ));
    }
    let pick = match requested {
        Some(cwd) => found.iter().find(|(_, hit)| hit.1 == Path::new(cwd)),
        None => found.first(),
    };
    let Some((source_home, (_, cwd, source))) = pick else {
        return Ok(false);
    };
    let target = claude_transcript_path(cwd, id, home)?;
    File::open(source)
        .and_then(|input| {
            let modified = input.metadata()?.modified()?;
            copy_without_partial(input, modified, &target)
        })
        .map_err(|error| {
            format!(
                "세션 {id8} 트랜스크립트를 {}에서 활성 Claude home으로 복사하지 못함: {error}",
                source_home.display()
            )
        })?;
    tracing::info!(
        session_id = %id,
        from = %source.display(),
        to = %target.display(),
        "copied Claude transcript from another config home for /resume"
    );
    Ok(true)
}

/// Write through a sibling temp file and publish without clobbering, so a failure leaves no partial transcript.
fn copy_without_partial(
    mut input: impl Read,
    modified: SystemTime,
    target: &Path,
) -> std::io::Result<()> {
    let dir = target
        .parent()
        .ok_or_else(|| std::io::Error::other("transcript path has no parent"))?;
    fs::create_dir_all(dir)?;
    let mut temp = tempfile::NamedTempFile::new_in(dir)?;
    std::io::copy(&mut input, temp.as_file_mut())?;
    temp.as_file().set_modified(modified)?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(target)
        .map_err(|error| error.error)?;
    Ok(())
}

pub(super) fn resolve_cwd(
    current: Option<&str>,
    requested: Option<&str>,
    id: &str,
    home: Option<&Path>,
    others: &[PathBuf],
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
    let mut cwds = current.map(scoped_worktrees).unwrap_or_default();
    if let Some(cwd) = requested {
        cwds.insert(0, PathBuf::from(cwd));
    }
    let (mut matches, mut lookup_error) = scan_home(&cwds, id, home);
    // Other homes are consulted only when the active home provably holds no copy anywhere in scope.
    if matches.is_empty() && lookup_error.is_none() {
        match adopt_from_other_home(&cwds, requested, id, home, others) {
            Ok(true) => (matches, lookup_error) = scan_home(&cwds, id, home),
            Ok(false) => {}
            Err(refusal) => lookup_error = Some(refusal),
        }
    }
    if let Some(cwd) = requested
        && matches.iter().any(|hit| hit.1 == Path::new(cwd))
    {
        return Ok(cwd.to_string());
    }
    if requested.is_none()
        && let Some((_, cwd, _)) = matches.first()
    {
        return Ok(cwd.to_string_lossy().into_owned());
    }
    let hint = matches.first().map_or_else(
        || lookup_error.unwrap_or_else(|| "No transcript was found in this channel's worktree lineage. Check the session id and Claude config home.".to_string()),
        |(_, cwd, _)| format!("Transcript is available at cwd={}; retry with that cwd or omit cwd.", cwd.display()),
    );
    Err(ResumeRebindError::TargetTranscriptMissing(format!(
        "Claude transcript unavailable for session_id={id}{}: {hint}",
        requested.map_or(String::new(), |cwd| format!(" at cwd={cwd}")),
    )))
}

/// Extra Claude homes declared in `providers.claude.transcript_homes`; relative entries are ignored.
pub(crate) fn transcript_homes() -> Vec<PathBuf> {
    crate::config::snapshot()
        .and_then(|config| config.providers.get("claude").cloned())
        .map(|claude| claude.transcript_homes)
        .unwrap_or_default()
        .iter()
        .map(|home| PathBuf::from(crate::utils::format::expand_tilde_string(home)))
        .filter(|home| home.is_absolute())
        .collect()
}

pub(super) async fn resolve_cwd_off_runtime(
    current: Option<String>,
    requested: Option<String>,
    id: String,
) -> Result<String, ResumeRebindError> {
    let home = claude_home();
    let others = transcript_homes();
    tokio::task::spawn_blocking(move || {
        resolve_cwd(
            current.as_deref(),
            requested.as_deref(),
            &id,
            home.as_deref(),
            &others,
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
    others: &[PathBuf],
    partial: &str,
    deadline: Instant,
) -> Vec<ResumeChoice> {
    // Reserve preview time even when directory discovery consumes its share of the budget.
    let discovery_deadline =
        Instant::now() + deadline.saturating_duration_since(Instant::now()) / 3;
    let mut files = Vec::new();
    // Entries from a non-active home carry its directory name so the label shows where they live.
    let others = others
        .iter()
        .filter(|other| !home.is_some_and(|home| same_dir(home, other)));
    let homes = std::iter::once((None, home)).chain(others.map(|other| {
        let name = other
            .file_name()
            .map(|name| name.to_string_lossy().into_owned());
        (name, Some(other.as_path()))
    }));
    'worktrees: for ((home_name, home), worktree) in homes.flat_map(|home| {
        scoped_worktree_iter(cwd, Some(discovery_deadline)).map(move |tree| (home.clone(), tree))
    }) {
        if Instant::now() >= discovery_deadline {
            break;
        }
        let Ok(projects) = claude_project_dir_candidates_for_cwd(&worktree, home) else {
            continue;
        };
        for project in projects {
            let Ok(entries) = fs::read_dir(project) else {
                continue;
            };
            for entry in before_deadline(entries, Some(discovery_deadline)).flatten() {
                if Instant::now() >= discovery_deadline {
                    break 'worktrees;
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
                // DirEntry metadata lags an appended file on Windows; stat it without following links.
                let Ok(meta) = std::fs::symlink_metadata(&path) else {
                    continue;
                };
                if !meta.is_file() {
                    continue;
                }
                files.push((
                    meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                    path.clone(),
                    meta.len(),
                    home_name.clone(),
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
    for (modified, path, len, home_name, candidate) in files {
        if Instant::now() >= deadline {
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
        let mut label = format!(
            "{title} · {relative} · {}",
            worktree.chars().take(35).collect::<String>()
        );
        if let Some(name) = home_name {
            label.push_str(&format!(
                " · home {}",
                name.chars().take(20).collect::<String>()
            ));
        }
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
    #[cfg(test)]
    if let Some(delays) = PREVIEW_DELAYS.get() {
        let delay = delays
            .lock()
            .unwrap()
            .iter()
            .find_map(|(root, delay)| path.starts_with(root).then_some(*delay));
        if let Some(delay) = delay {
            std::thread::sleep(delay);
        }
    }
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
