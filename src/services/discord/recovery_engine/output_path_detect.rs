//! Pure output-path detection helpers for rebind recovery (#3479 r8 split).
//!
//! Behavior-preserving extraction from `recovery_engine.rs`: the `lsof`-output
//! parsing, fallback-path matching, and candidate-identity logic used to detect
//! the live JSONL output path a re-bound tmux pane is writing to. The tmux/lsof
//! subprocess drivers (`tmux_pane_pid`, `detect_live_tmux_output_path`) stay in
//! the root module because they couple to `Command`/`binary_resolver`; these
//! helpers are pure (parsing + filesystem identity) so they live here.

use super::*;

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DetectedRebindOutputPath {
    pub(super) path: String,
    pub(super) initial_offset: u64,
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LsofOutputCandidate {
    fd: String,
    raw_path: String,
    inode: Option<u64>,
}

#[cfg(unix)]
impl LsofOutputCandidate {
    fn normalized_path(&self) -> &str {
        normalize_lsof_path(&self.raw_path)
    }

    fn is_deleted(&self) -> bool {
        self.raw_path.ends_with(" (deleted)")
    }

    fn as_stale(&self) -> StaleOutputCandidate {
        StaleOutputCandidate {
            fd: self.fd.clone(),
            raw_path: self.raw_path.clone(),
            inode: self.inode,
        }
    }
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct StaleOutputCandidate {
    pub(super) fd: String,
    pub(super) raw_path: String,
    pub(super) inode: Option<u64>,
}

#[cfg(unix)]
fn candidate_identity(path: &str) -> Option<(u64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.dev(), meta.ino()))
}

#[cfg(unix)]
fn normalize_lsof_path(raw: &str) -> &str {
    raw.trim_end_matches(" (deleted)")
}

#[cfg(unix)]
fn candidate_matches_fallback(fallback_path: &str, candidate_path: &str) -> bool {
    let Some(fallback_name) = Path::new(fallback_path)
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    let Some(candidate_name) = Path::new(candidate_path)
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    let fallback_stem = fallback_name
        .strip_suffix(".jsonl")
        .unwrap_or(fallback_name);
    candidate_name == fallback_name
        || (candidate_name.starts_with(fallback_stem) && candidate_name.contains(".jsonl"))
}

#[cfg(unix)]
pub(super) fn parse_lsof_output_candidates(stdout: &str) -> Vec<LsofOutputCandidate> {
    let mut candidates = Vec::new();
    let mut current_fd: Option<String> = None;
    let mut current_path: Option<String> = None;
    let mut current_inode: Option<u64> = None;

    let flush = |candidates: &mut Vec<LsofOutputCandidate>,
                 current_fd: &mut Option<String>,
                 current_path: &mut Option<String>,
                 current_inode: &mut Option<u64>| {
        if let (Some(fd), Some(raw_path)) = (current_fd.take(), current_path.take()) {
            candidates.push(LsofOutputCandidate {
                fd,
                raw_path,
                inode: *current_inode,
            });
        }
        *current_inode = None;
    };

    for line in stdout.lines() {
        let Some((field, value)) = line.split_at_checked(1) else {
            continue;
        };
        match field {
            "f" => {
                flush(
                    &mut candidates,
                    &mut current_fd,
                    &mut current_path,
                    &mut current_inode,
                );
                current_fd = Some(value.to_string());
            }
            "i" => current_inode = value.parse::<u64>().ok(),
            "n" => current_path = Some(value.to_string()),
            _ => {}
        }
    }

    flush(
        &mut candidates,
        &mut current_fd,
        &mut current_path,
        &mut current_inode,
    );
    candidates
}

#[cfg(unix)]
pub(super) fn detect_rebind_output_path_from_candidates(
    fallback_path: &str,
    candidates: impl IntoIterator<Item = LsofOutputCandidate>,
) -> Result<Option<DetectedRebindOutputPath>, StaleOutputCandidate> {
    let fallback_identity = candidate_identity(fallback_path);
    let mut first_stale_candidate: Option<StaleOutputCandidate> = None;
    for candidate in candidates {
        let candidate_path = candidate.normalized_path();
        if !candidate_matches_fallback(fallback_path, candidate_path) {
            continue;
        }
        if candidate.is_deleted() {
            first_stale_candidate.get_or_insert_with(|| candidate.as_stale());
            continue;
        }
        let meta = match std::fs::metadata(candidate_path) {
            Ok(meta) => meta,
            Err(_) => {
                first_stale_candidate.get_or_insert_with(|| candidate.as_stale());
                continue;
            }
        };
        if candidate.inode.is_some_and(|inode| inode != meta.ino()) {
            first_stale_candidate.get_or_insert_with(|| candidate.as_stale());
            continue;
        };
        let identity = (meta.dev(), meta.ino());
        if fallback_identity.is_some() && fallback_identity == Some(identity) {
            return Ok(None);
        }
        return Ok(Some(DetectedRebindOutputPath {
            path: candidate_path.to_string(),
            initial_offset: meta.len(),
        }));
    }
    if let Some(stale) = first_stale_candidate {
        Err(stale)
    } else {
        Ok(None)
    }
}
