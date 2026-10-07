//! Legacy discovery keeps its cwd, UUID and newest-mtime ordering.

use super::*;

pub(super) fn wait_for_latest_rollout_for_cwd(
    cwd: &Path,
    modified_since: SystemTime,
    sessions_dir: &Path,
    cancel_token: Option<&CancelToken>,
    is_alive: &mut impl FnMut() -> bool,
    timeout: Duration,
) -> Result<PathBuf, String> {
    let started = Instant::now();
    loop {
        if cancel_requested(cancel_token) {
            return Err("cancelled waiting for Codex rollout transcript".to_string());
        }
        if let Some(path) = latest_rollout_for_cwd_since(cwd, modified_since, sessions_dir) {
            return Ok(path);
        }
        if !is_alive() {
            return Err("Codex TUI exited before creating a rollout transcript".to_string());
        }
        if started.elapsed() > timeout {
            return Err(format!(
                "Timeout waiting for Codex rollout transcript under {}",
                sessions_dir.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub(super) fn wait_for_resumed_rollout_for_session(
    cwd: &Path,
    session_id: &str,
    previous_rollout_path: &Path,
    previous_start_offset: u64,
    modified_since: SystemTime,
    sessions_dir: &Path,
    cancel_token: Option<&CancelToken>,
    is_alive: &mut impl FnMut() -> bool,
    timeout: Duration,
) -> Result<PathBuf, String> {
    let started = Instant::now();
    loop {
        if cancel_requested(cancel_token) {
            return Err("cancelled waiting for Codex resumed rollout transcript".to_string());
        }
        if rollout_file_len(previous_rollout_path).is_some_and(|len| len > previous_start_offset) {
            return Ok(previous_rollout_path.to_path_buf());
        }
        if let Some(path) =
            latest_rollout_for_cwd_and_session_since(cwd, session_id, modified_since, sessions_dir)
        {
            return Ok(path);
        }
        if !is_alive() {
            return Err(
                "Codex TUI exited before updating a resumed rollout transcript".to_string(),
            );
        }
        if started.elapsed() > timeout {
            return Err(format!(
                "Timeout waiting for Codex resumed rollout transcript under {}",
                sessions_dir.display()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn latest_rollout_for_cwd_since(
    cwd: &Path,
    modified_since: SystemTime,
    sessions_dir: &Path,
) -> Option<PathBuf> {
    rollout_candidates_for_cwd_since(cwd, modified_since, sessions_dir)
        .into_iter()
        .next()
}

pub fn latest_unclaimed_rollout_for_cwd_since(
    cwd: &Path,
    modified_since: SystemTime,
    sessions_dir: &Path,
    claimed_rollout_paths: &HashSet<PathBuf>,
) -> Option<PathBuf> {
    rollout_candidates_for_cwd_since(cwd, modified_since, sessions_dir)
        .into_iter()
        .find(|path| !rollout_path_is_claimed(path, claimed_rollout_paths))
}

pub fn rollout_candidates_for_cwd_since(
    cwd: &Path,
    modified_since: SystemTime,
    sessions_dir: &Path,
) -> Vec<PathBuf> {
    let canonical_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut candidates: Vec<(SystemTime, PathBuf)> = Vec::new();
    for item in super::super::rollout_index::cached_indexed_rollouts(sessions_dir) {
        if item.modified < modified_since {
            continue;
        }
        let Some(meta) = item.meta.as_ref() else {
            continue;
        };
        if meta.is_subagent() {
            continue;
        }
        let session_cwd =
            std::fs::canonicalize(&meta.cwd).unwrap_or_else(|_| PathBuf::from(&meta.cwd));
        if session_cwd != canonical_cwd {
            continue;
        }
        candidates.push((item.modified, item.path));
    }
    candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
    candidates.into_iter().map(|(_, path)| path).collect()
}

fn rollout_path_is_claimed(path: &Path, claimed_rollout_paths: &HashSet<PathBuf>) -> bool {
    if claimed_rollout_paths.contains(path) {
        return true;
    }
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    claimed_rollout_paths.contains(&canonical)
}
