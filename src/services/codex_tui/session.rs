use serde_json::Value;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use super::rollout_tail::default_codex_sessions_dir;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexTuiSessionFiles {
    pub codex_home_path: PathBuf,
    pub legacy_codex_home_path: PathBuf,
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
        }
    }

    pub fn cleanup_best_effort(&self) {
        let _ = std::fs::remove_dir_all(&self.codex_home_path);
        let _ = std::fs::remove_dir_all(&self.legacy_codex_home_path);
    }
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

fn matching_rollout_candidates(
    sessions_root: &Path,
    cwd: &Path,
    requested_id: &str,
) -> Vec<RolloutCandidate> {
    let canonical_cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    rollout_files_under(sessions_root)
        .into_iter()
        .filter_map(|path| {
            let metadata = std::fs::metadata(&path).ok()?;
            let modified = metadata.modified().ok()?;
            let len = metadata.len();
            let session = read_rollout_session_meta(&path)?;
            if session.id != requested_id {
                return None;
            }
            let session_cwd =
                std::fs::canonicalize(&session.cwd).unwrap_or_else(|_| PathBuf::from(&session.cwd));
            if session_cwd != canonical_cwd {
                return None;
            }
            Some(RolloutCandidate {
                path,
                modified,
                len,
            })
        })
        .collect()
}

fn rollout_files_under(root: &Path) -> Vec<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    let mut files = Vec::new();
    while let Some(path) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(".jsonl"))
            {
                files.push(path);
            }
        }
    }
    files
}

#[derive(Debug, Clone)]
struct RolloutSessionMeta {
    id: String,
    cwd: PathBuf,
}

fn read_rollout_session_meta(path: &Path) -> Option<RolloutSessionMeta> {
    let file = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().map_while(Result::ok).take(20) {
        let Ok(json) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if json.get("type").and_then(Value::as_str) != Some("session_meta") {
            continue;
        }
        let Some(payload) = json.get("payload") else {
            continue;
        };
        let Some(id) = payload.get("id").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        let Some(cwd) = payload.get("cwd").and_then(Value::as_str).map(str::trim) else {
            continue;
        };
        if id.is_empty() || cwd.is_empty() {
            return None;
        }
        return Some(RolloutSessionMeta {
            id: id.to_string(),
            cwd: PathBuf::from(cwd),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

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

    fn lock_test_env() -> MutexGuard<'static, ()> {
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
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
    fn multiple_candidates_select_newest_then_path_deterministically() {
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
}
