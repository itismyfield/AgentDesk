use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::rollout_index::cached_indexed_rollouts;
use super::rollout_tail::default_codex_sessions_dir;

const CODEX_TUI_LAUNCH_OPTIONS_FINGERPRINT_TEMP_EXT: &str = "codex-tui-launch-options.sha256";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexTuiSessionFiles {
    pub codex_home_path: PathBuf,
    pub legacy_codex_home_path: PathBuf,
    pub launch_options_fingerprint_path: PathBuf,
    pub legacy_launch_options_fingerprint_path: PathBuf,
}

impl CodexTuiSessionFiles {
    pub fn for_tmux_session(tmux_session_name: &str) -> Self {
        Self {
            codex_home_path: PathBuf::from(crate::services::tmux_common::session_temp_path(
                tmux_session_name,
                crate::services::tmux_common::CODEX_TUI_HOME_TEMP_EXT,
            )),
            legacy_codex_home_path: PathBuf::from(
                crate::services::tmux_common::legacy_tmp_session_path(
                    tmux_session_name,
                    crate::services::tmux_common::CODEX_TUI_HOME_TEMP_EXT,
                ),
            ),
            launch_options_fingerprint_path: PathBuf::from(
                crate::services::tmux_common::session_temp_path(
                    tmux_session_name,
                    CODEX_TUI_LAUNCH_OPTIONS_FINGERPRINT_TEMP_EXT,
                ),
            ),
            legacy_launch_options_fingerprint_path: PathBuf::from(
                crate::services::tmux_common::legacy_tmp_session_path(
                    tmux_session_name,
                    CODEX_TUI_LAUNCH_OPTIONS_FINGERPRINT_TEMP_EXT,
                ),
            ),
        }
    }

    pub fn cleanup_best_effort(&self) {
        let _ = std::fs::remove_dir_all(&self.codex_home_path);
        let _ = std::fs::remove_dir_all(&self.legacy_codex_home_path);
        let _ = std::fs::remove_file(&self.launch_options_fingerprint_path);
        let _ = std::fs::remove_file(&self.legacy_launch_options_fingerprint_path);
    }
}

pub fn write_codex_tui_launch_options_fingerprint(
    tmux_session_name: &str,
    fingerprint: &str,
) -> Result<(), String> {
    let files = CodexTuiSessionFiles::for_tmux_session(tmux_session_name);
    std::fs::write(&files.launch_options_fingerprint_path, fingerprint.trim())
        .map_err(|error| format!("failed to write Codex TUI launch-options fingerprint: {error}"))
}

pub fn read_codex_tui_launch_options_fingerprint(tmux_session_name: &str) -> Option<String> {
    let path = crate::services::tmux_common::resolve_session_temp_path(
        tmux_session_name,
        CODEX_TUI_LAUNCH_OPTIONS_FINGERPRINT_TEMP_EXT,
    )?;
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexTuiRolloutMarker {
    pub rollout_path: PathBuf,
    pub session_id: Option<String>,
    pub rollout_start_offset: Option<u64>,
}

pub fn write_codex_tui_rollout_marker(
    tmux_session_name: &str,
    rollout_path: &Path,
    session_id: Option<&str>,
) -> Result<(), String> {
    write_codex_tui_rollout_marker_with_start_offset(
        tmux_session_name,
        rollout_path,
        session_id,
        None,
    )
}

pub fn write_codex_tui_rollout_marker_with_start_offset(
    tmux_session_name: &str,
    rollout_path: &Path,
    session_id: Option<&str>,
    rollout_start_offset: Option<u64>,
) -> Result<(), String> {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return Ok(());
    }
    let rollout_start_offset = preserved_rollout_start_offset_for_marker(
        tmux_session_name,
        rollout_path,
        rollout_start_offset,
    );
    let path = crate::services::tmux_common::session_temp_path(
        tmux_session_name,
        crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
    );
    let value = serde_json::json!({
        "rollout_path": rollout_path.display().to_string(),
        "session_id": session_id
            .map(str::trim)
            .filter(|value| !value.is_empty()),
        "rollout_start_offset": rollout_start_offset,
    });
    std::fs::write(&path, format!("{value}\n"))
        .map_err(|error| format!("failed to write Codex TUI rollout marker: {error}"))
}

pub fn advance_codex_tui_rollout_marker_start_offset(
    tmux_session_name: &str,
    rollout_path: &Path,
    rollout_start_offset: u64,
) -> Result<(), String> {
    let existing_session_id = read_codex_tui_rollout_marker(tmux_session_name)
        .filter(|marker| codex_tui_rollout_paths_same(&marker.rollout_path, rollout_path))
        .and_then(|marker| marker.session_id);
    write_codex_tui_rollout_marker_with_start_offset(
        tmux_session_name,
        rollout_path,
        existing_session_id.as_deref(),
        Some(rollout_start_offset),
    )
}

fn preserved_rollout_start_offset_for_marker(
    tmux_session_name: &str,
    rollout_path: &Path,
    rollout_start_offset: Option<u64>,
) -> Option<u64> {
    let existing = read_codex_tui_rollout_marker(tmux_session_name)
        .filter(|marker| codex_tui_rollout_paths_same(&marker.rollout_path, rollout_path))
        .and_then(|marker| marker.rollout_start_offset);
    match (existing, rollout_start_offset) {
        (Some(existing), Some(current)) => Some(existing.max(current)),
        (Some(existing), None) => Some(existing),
        (None, current) => current,
    }
}

fn codex_tui_rollout_paths_same(left: &Path, right: &Path) -> bool {
    let left = std::fs::canonicalize(left).unwrap_or_else(|_| left.to_path_buf());
    let right = std::fs::canonicalize(right).unwrap_or_else(|_| right.to_path_buf());
    left == right
}

pub fn read_codex_tui_rollout_marker(tmux_session_name: &str) -> Option<CodexTuiRolloutMarker> {
    let tmux_session_name = tmux_session_name.trim();
    if tmux_session_name.is_empty() {
        return None;
    }
    let path = crate::services::tmux_common::resolve_session_temp_path(
        tmux_session_name,
        crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
    )?;
    let raw = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(raw.trim()).ok()?;
    let rollout_path = value
        .get("rollout_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)?;
    let session_id = value
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);
    let rollout_start_offset = value.get("rollout_start_offset").and_then(Value::as_u64);
    Some(CodexTuiRolloutMarker {
        rollout_path,
        session_id,
        rollout_start_offset,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexTuiSessionSelection {
    pub requested_session_id: Option<String>,
    pub selected_session_id: Option<String>,
    pub resume: bool,
    pub reason: String,
    pub rollout_path: Option<PathBuf>,
    pub rollout_start_offset: Option<u64>,
    pub candidate_count: usize,
}

impl CodexTuiSessionSelection {
    pub fn resume_session_id(&self) -> Option<&str> {
        if self.resume {
            self.selected_session_id.as_deref()
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
struct RolloutCandidate {
    path: PathBuf,
    modified: SystemTime,
    len: u64,
}

pub fn resolve_codex_tui_session(
    requested_session_id: Option<&str>,
    cwd: &Path,
    sessions_dir: Option<&Path>,
    force_fresh_provider_session: bool,
) -> CodexTuiSessionSelection {
    let requested = requested_session_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string);

    if force_fresh_provider_session {
        return fresh(requested, "force fresh provider session requested");
    }

    let Some(requested_id) = requested.clone() else {
        return fresh(None, "no requested session id");
    };

    let sessions_root = sessions_dir
        .map(Path::to_path_buf)
        .or_else(default_codex_sessions_dir);
    let Some(sessions_root) = sessions_root else {
        return fresh(
            Some(requested_id),
            "Codex sessions directory unavailable; starting fresh",
        );
    };

    let mut candidates = matching_rollout_candidates(&sessions_root, cwd, &requested_id);
    let candidate_count = candidates.len();
    if candidates.is_empty() {
        tracing::info!(
            "Codex TUI session resolver starting fresh: no local rollout found for requested session {} under {}",
            requested_id,
            sessions_root.display()
        );
        return CodexTuiSessionSelection {
            requested_session_id: Some(requested_id),
            selected_session_id: None,
            resume: false,
            reason: "requested session id has no matching local rollout".to_string(),
            rollout_path: None,
            rollout_start_offset: None,
            candidate_count,
        };
    }

    candidates.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then_with(|| left.path.cmp(&right.path))
    });
    let selected = candidates.remove(0);
    if candidate_count > 1 {
        tracing::warn!(
            "Codex TUI session resolver found {} rollout candidates for {}; selected {} at offset {}",
            candidate_count,
            requested_id,
            selected.path.display(),
            selected.len
        );
    } else {
        tracing::info!(
            "Codex TUI session resolver resuming {} from {} at offset {}",
            requested_id,
            selected.path.display(),
            selected.len
        );
    }

    CodexTuiSessionSelection {
        requested_session_id: Some(requested_id.clone()),
        selected_session_id: Some(requested_id),
        resume: true,
        reason: "requested session id matched local rollout".to_string(),
        rollout_path: Some(selected.path),
        rollout_start_offset: Some(selected.len),
        candidate_count,
    }
}

fn fresh(
    requested_session_id: Option<String>,
    reason: impl Into<String>,
) -> CodexTuiSessionSelection {
    CodexTuiSessionSelection {
        requested_session_id,
        selected_session_id: None,
        resume: false,
        reason: reason.into(),
        rollout_path: None,
        rollout_start_offset: None,
        candidate_count: 0,
    }
}

/// Resolve the rollout candidates matching `requested_id` + `cwd` under
/// `sessions_root`, applying the exact legacy filters (TUI-compatible
/// `session_meta`, requested session id, canonical cwd equality). The directory
/// walk + header parse are served by the cache-backed
/// [`cached_indexed_rollouts`] (REQ-001/REQ-006); the per-candidate filtering
/// and `(modified, len)` projection are unchanged from the legacy scan so
/// selection semantics (REQ-002/REQ-003) are byte-for-byte identical.
fn matching_rollout_candidates(
    sessions_root: &Path,
    cwd: &Path,
    requested_id: &str,
) -> Vec<RolloutCandidate> {
    let canonical_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    cached_indexed_rollouts(sessions_root)
        .into_iter()
        .filter_map(|item| {
            let session = item.meta?;
            if !session.is_tui_compatible() {
                return None;
            }
            if session.id.as_deref() != Some(requested_id) {
                return None;
            }
            let session_cwd =
                std::fs::canonicalize(&session.cwd).unwrap_or_else(|_| PathBuf::from(&session.cwd));
            if session_cwd != canonical_cwd {
                return None;
            }
            Some(RolloutCandidate {
                path: item.path,
                modified: item.modified,
                len: item.len,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;

    struct EnvRestore {
        previous_root: Option<std::ffi::OsString>,
        previous_host: Option<std::ffi::OsString>,
    }

    impl EnvRestore {
        fn set_agentdesk_root_and_host(root: &Path, host: &str) -> Self {
            let restore = Self {
                previous_root: std::env::var_os("AGENTDESK_ROOT_DIR"),
                previous_host: std::env::var_os("HOSTNAME"),
            };
            unsafe {
                std::env::set_var("AGENTDESK_ROOT_DIR", root);
                std::env::set_var("HOSTNAME", host);
            }
            restore
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.previous_root.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
            match self.previous_host.take() {
                Some(value) => unsafe { std::env::set_var("HOSTNAME", value) },
                None => unsafe { std::env::remove_var("HOSTNAME") },
            }
        }
    }

    fn write_rollout(root: &Path, relative: &str, id: &str, cwd: &Path, suffix: &str) -> PathBuf {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{}\",\"cwd\":\"{}\"}}}}\n{}",
                id,
                cwd.display(),
                suffix
            ),
        )
        .unwrap();
        path
    }

    fn write_rollout_with_source(
        root: &Path,
        relative: &str,
        id: &str,
        cwd: &Path,
        source: &str,
        originator: &str,
    ) -> PathBuf {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{}\",\"cwd\":\"{}\",\"source\":\"{}\",\"originator\":\"{}\"}}}}\n",
                id,
                cwd.display(),
                source,
                originator
            ),
        )
        .unwrap();
        path
    }

    fn lock_test_env() -> MutexGuard<'static, ()> {
        crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Every resolver test here drives `resolve_codex_tui_session`, which reads
    /// AND writes the process-global rollout index via `cached_indexed_rollouts`.
    /// Without serialization a `session.rs` test could mutate `roots` between a
    /// `rollout_index` test's reset and its cache-state assertion, making the
    /// `codex_tui` suite nondeterministic under default parallel `cargo test`.
    /// Share the SAME lock the `rollout_index` tests use (it also resets the
    /// cache on acquisition) so the two modules can never interleave.
    fn lock_cache_test() -> MutexGuard<'static, ()> {
        super::super::rollout_index::lock_cache_for_tests()
    }

    #[test]
    fn cleanup_best_effort_removes_codex_tui_home() {
        let _lock = lock_test_env();
        let dir = tempfile::tempdir().unwrap();
        let _env = EnvRestore::set_agentdesk_root_and_host(dir.path(), "codex-tui-cleanup-host");
        let files = CodexTuiSessionFiles::for_tmux_session("AgentDesk-codex-cleanup-test");
        let nested_file = files.codex_home_path.join("nested/config.toml");
        std::fs::create_dir_all(nested_file.parent().unwrap()).unwrap();
        std::fs::write(&nested_file, "seed").unwrap();
        let legacy_nested_file = files.legacy_codex_home_path.join("nested/config.toml");
        std::fs::create_dir_all(legacy_nested_file.parent().unwrap()).unwrap();
        std::fs::write(&legacy_nested_file, "legacy-seed").unwrap();
        std::fs::write(&files.launch_options_fingerprint_path, "fingerprint").unwrap();
        std::fs::write(
            &files.legacy_launch_options_fingerprint_path,
            "legacy-fingerprint",
        )
        .unwrap();

        files.cleanup_best_effort();
        files.cleanup_best_effort();

        assert!(
            !files.codex_home_path.exists(),
            "cleanup_best_effort must remove the Codex TUI temp home recursively"
        );
        assert!(
            !files.legacy_codex_home_path.exists(),
            "cleanup_best_effort must remove the legacy Codex TUI temp home recursively"
        );
        assert!(!files.launch_options_fingerprint_path.exists());
        assert!(!files.legacy_launch_options_fingerprint_path.exists());
    }

    #[test]
    fn codex_tui_launch_options_fingerprint_round_trips() {
        let _lock = lock_test_env();
        let dir = tempfile::tempdir().unwrap();
        let _env =
            EnvRestore::set_agentdesk_root_and_host(dir.path(), "codex-tui-fingerprint-host");
        let tmux_session = "AgentDesk-codex-fingerprint";

        write_codex_tui_launch_options_fingerprint(tmux_session, " sha256-value \n").unwrap();

        assert_eq!(
            read_codex_tui_launch_options_fingerprint(tmux_session).as_deref(),
            Some("sha256-value")
        );
    }

    #[test]
    fn codex_tui_rollout_marker_round_trips() {
        let _lock = lock_test_env();
        let dir = tempfile::tempdir().unwrap();
        let _env = EnvRestore::set_agentdesk_root_and_host(dir.path(), "codex-tui-marker-host");
        let rollout_path = dir.path().join("rollout-session.jsonl");
        std::fs::write(&rollout_path, "{}\n").unwrap();

        write_codex_tui_rollout_marker("AgentDesk-codex-marker", &rollout_path, Some("sess-1"))
            .unwrap();

        assert_eq!(
            read_codex_tui_rollout_marker("AgentDesk-codex-marker"),
            Some(CodexTuiRolloutMarker {
                rollout_path,
                session_id: Some("sess-1".to_string()),
                rollout_start_offset: None,
            })
        );
    }

    #[test]
    fn codex_tui_rollout_marker_round_trips_start_offset() {
        let _lock = lock_test_env();
        let dir = tempfile::tempdir().unwrap();
        let _env =
            EnvRestore::set_agentdesk_root_and_host(dir.path(), "codex-tui-marker-offset-host");
        let rollout_path = dir.path().join("rollout-session.jsonl");
        std::fs::write(&rollout_path, "{}\n").unwrap();

        write_codex_tui_rollout_marker_with_start_offset(
            "AgentDesk-codex-marker-offset",
            &rollout_path,
            Some("sess-2"),
            Some(42),
        )
        .unwrap();

        assert_eq!(
            read_codex_tui_rollout_marker("AgentDesk-codex-marker-offset"),
            Some(CodexTuiRolloutMarker {
                rollout_path,
                session_id: Some("sess-2".to_string()),
                rollout_start_offset: Some(42),
            })
        );
    }

    #[test]
    fn codex_tui_rollout_marker_plain_refresh_preserves_start_offset() {
        let _lock = lock_test_env();
        let dir = tempfile::tempdir().unwrap();
        let _env =
            EnvRestore::set_agentdesk_root_and_host(dir.path(), "codex-tui-marker-refresh-host");
        let rollout_path = dir.path().join("rollout-session.jsonl");
        std::fs::write(&rollout_path, "{}\n").unwrap();

        write_codex_tui_rollout_marker_with_start_offset(
            "AgentDesk-codex-marker-refresh",
            &rollout_path,
            Some("sess-3"),
            Some(42),
        )
        .unwrap();
        write_codex_tui_rollout_marker(
            "AgentDesk-codex-marker-refresh",
            &rollout_path,
            Some("sess-3"),
        )
        .unwrap();

        assert_eq!(
            read_codex_tui_rollout_marker("AgentDesk-codex-marker-refresh")
                .and_then(|marker| marker.rollout_start_offset),
            Some(42),
            "plain marker refresh must not erase the durable raw replay cursor"
        );
    }

    #[test]
    fn codex_tui_rollout_marker_keeps_start_offset_monotonic() {
        let _lock = lock_test_env();
        let dir = tempfile::tempdir().unwrap();
        let _env =
            EnvRestore::set_agentdesk_root_and_host(dir.path(), "codex-tui-marker-monotonic-host");
        let rollout_path = dir.path().join("rollout-session.jsonl");
        std::fs::write(&rollout_path, "{}\n").unwrap();
        let tmux_session = "AgentDesk-codex-marker-monotonic";

        write_codex_tui_rollout_marker_with_start_offset(
            tmux_session,
            &rollout_path,
            Some("sess-4"),
            Some(88),
        )
        .unwrap();
        write_codex_tui_rollout_marker_with_start_offset(
            tmux_session,
            &rollout_path,
            Some("sess-4"),
            Some(12),
        )
        .unwrap();
        assert_eq!(
            read_codex_tui_rollout_marker(tmux_session)
                .and_then(|marker| marker.rollout_start_offset),
            Some(88)
        );

        write_codex_tui_rollout_marker_with_start_offset(
            tmux_session,
            &rollout_path,
            Some("sess-4"),
            Some(144),
        )
        .unwrap();
        assert_eq!(
            read_codex_tui_rollout_marker(tmux_session)
                .and_then(|marker| marker.rollout_start_offset),
            Some(144)
        );
    }

    #[test]
    fn codex_tui_rollout_marker_advance_preserves_session_and_monotonic_offset() {
        let _lock = lock_test_env();
        let dir = tempfile::tempdir().unwrap();
        let _env =
            EnvRestore::set_agentdesk_root_and_host(dir.path(), "codex-tui-marker-advance-host");
        let rollout_path = dir.path().join("rollout-session.jsonl");
        std::fs::write(&rollout_path, "{}\n").unwrap();
        let tmux_session = "AgentDesk-codex-marker-advance";

        write_codex_tui_rollout_marker_with_start_offset(
            tmux_session,
            &rollout_path,
            Some("sess-advance"),
            Some(88),
        )
        .unwrap();
        advance_codex_tui_rollout_marker_start_offset(tmux_session, &rollout_path, 12).unwrap();
        advance_codex_tui_rollout_marker_start_offset(tmux_session, &rollout_path, 144).unwrap();

        assert_eq!(
            read_codex_tui_rollout_marker(tmux_session),
            Some(CodexTuiRolloutMarker {
                rollout_path,
                session_id: Some("sess-advance".to_string()),
                rollout_start_offset: Some(144),
            })
        );
    }

    #[test]
    fn blank_requested_id_resolves_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let selection = resolve_codex_tui_session(Some("  "), dir.path(), Some(dir.path()), false);

        assert!(!selection.resume);
        assert_eq!(selection.reason, "no requested session id");
        assert!(selection.rollout_path.is_none());
    }

    #[test]
    fn requested_session_with_matching_rollout_resumes() {
        let _cache = lock_cache_test();
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let rollout = write_rollout(
            dir.path(),
            "2026/05/rollout-a.jsonl",
            "sess-1",
            cwd.path(),
            "",
        );
        let len = std::fs::metadata(&rollout).unwrap().len();

        let selection =
            resolve_codex_tui_session(Some(" sess-1 "), cwd.path(), Some(dir.path()), false);

        assert!(selection.resume);
        assert_eq!(selection.resume_session_id(), Some("sess-1"));
        assert_eq!(selection.rollout_path.as_deref(), Some(rollout.as_path()));
        assert_eq!(selection.rollout_start_offset, Some(len));
    }

    #[test]
    fn forced_fresh_ignores_requested_session_and_existing_rollout() {
        let _cache = lock_cache_test();
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        write_rollout(dir.path(), "rollout-a.jsonl", "sess-1", cwd.path(), "");

        let selection =
            resolve_codex_tui_session(Some("sess-1"), cwd.path(), Some(dir.path()), true);

        assert!(!selection.resume);
        assert_eq!(
            selection.reason,
            "force fresh provider session requested".to_string()
        );
        assert!(selection.rollout_path.is_none());
    }

    #[test]
    fn requested_session_without_matching_rollout_resolves_fresh() {
        let _cache = lock_cache_test();
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        write_rollout(dir.path(), "rollout-a.jsonl", "other", cwd.path(), "");

        let selection =
            resolve_codex_tui_session(Some("sess-1"), cwd.path(), Some(dir.path()), false);

        assert!(!selection.resume);
        assert_eq!(
            selection.reason,
            "requested session id has no matching local rollout"
        );
        assert_eq!(selection.candidate_count, 0);
    }

    #[test]
    fn requested_session_with_only_codex_exec_rollout_resolves_fresh() {
        let _cache = lock_cache_test();
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        write_rollout_with_source(
            dir.path(),
            "rollout-exec.jsonl",
            "sess-1",
            cwd.path(),
            "exec",
            "codex_exec",
        );

        let selection =
            resolve_codex_tui_session(Some("sess-1"), cwd.path(), Some(dir.path()), false);

        assert!(!selection.resume);
        assert_eq!(
            selection.reason,
            "requested session id has no matching local rollout"
        );
        assert_eq!(selection.candidate_count, 0);
    }

    #[test]
    fn multiple_candidates_select_newest_then_path_deterministically() {
        let _cache = lock_cache_test();
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let _first = write_rollout(dir.path(), "b/rollout-b.jsonl", "sess-1", cwd.path(), "old");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let second = write_rollout(dir.path(), "a/rollout-a.jsonl", "sess-1", cwd.path(), "new");

        let selection =
            resolve_codex_tui_session(Some("sess-1"), cwd.path(), Some(dir.path()), false);

        assert!(selection.resume);
        assert_eq!(selection.candidate_count, 2);
        assert_eq!(selection.rollout_path.as_deref(), Some(second.as_path()));
    }

    // TEST-002 / TEST-004 (REQ-001/REQ-002): resolving twice for the same root is
    // a warm-cache lookup the second time; a newer rollout created in between must
    // still be selected (the directory mtime bumps the tree signature). This pins
    // the high-severity "stale cache resumes an older rollout" risk.
    #[test]
    fn warm_lookup_picks_up_newer_rollout_after_first_resolve() {
        let _cache = lock_cache_test();
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let first = write_rollout(
            dir.path(),
            "old/rollout-a.jsonl",
            "sess-1",
            cwd.path(),
            "old",
        );

        // Cold resolve: selects the only candidate and warms the cache.
        let cold = resolve_codex_tui_session(Some("sess-1"), cwd.path(), Some(dir.path()), false);
        assert_eq!(cold.rollout_path.as_deref(), Some(first.as_path()));
        assert_eq!(cold.candidate_count, 1);

        // A newer rollout under a new leaf directory appears.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let newer = write_rollout(
            dir.path(),
            "new/rollout-b.jsonl",
            "sess-1",
            cwd.path(),
            "new",
        );

        // Warm resolve: signature changed, so the index rebuilds and the newer
        // rollout is selected — no stale hit.
        let warm = resolve_codex_tui_session(Some("sess-1"), cwd.path(), Some(dir.path()), false);
        assert!(warm.resume);
        assert_eq!(warm.candidate_count, 2);
        assert_eq!(
            warm.rollout_path.as_deref(),
            Some(newer.as_path()),
            "warm lookup must select the newest rollout, not the cached older one"
        );
    }

    // TEST-006 (REQ-006): the shared discovery primitive used by both `session.rs`
    // (via the index) and `rollout_tail.rs` agrees with a direct scan on which
    // files are rollout candidates.
    #[test]
    fn shared_discovery_primitive_matches_direct_scan() {
        let _cache = lock_cache_test();
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let a = write_rollout(dir.path(), "2026/05/rollout-a.jsonl", "s1", cwd.path(), "");
        let b = write_rollout(dir.path(), "2026/06/rollout-b.jsonl", "s2", cwd.path(), "");
        std::fs::write(dir.path().join("2026/05/ignore.txt"), "x").unwrap();

        let mut via_primitive = super::super::rollout_index::rollout_files_under(dir.path());
        via_primitive.sort();
        let mut via_index: Vec<_> =
            super::super::rollout_index::cached_indexed_rollouts(dir.path())
                .into_iter()
                .map(|item| item.path)
                .collect();
        via_index.sort();
        let mut expected = vec![a, b];
        expected.sort();

        assert_eq!(via_primitive, expected);
        assert_eq!(via_index, expected);
    }

    // TEST-003 (REQ-003): a non-TUI (codex_exec) rollout discovered by the index
    // is still excluded by the resolver filters, so the cache change does not
    // alter selection semantics.
    #[test]
    fn indexed_lookup_still_excludes_codex_exec_rollouts() {
        let _cache = lock_cache_test();
        let dir = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        write_rollout_with_source(
            dir.path(),
            "rollout-exec.jsonl",
            "sess-1",
            cwd.path(),
            "exec",
            "codex_exec",
        );
        let tui = write_rollout(dir.path(), "rollout-tui.jsonl", "sess-1", cwd.path(), "");

        let selection =
            resolve_codex_tui_session(Some("sess-1"), cwd.path(), Some(dir.path()), false);

        assert!(selection.resume);
        assert_eq!(
            selection.candidate_count, 1,
            "codex_exec rollout must remain excluded even through the index"
        );
        assert_eq!(selection.rollout_path.as_deref(), Some(tui.as_path()));
    }
}
