//! Turn-end WIP warning facade (#3792).
//!
//! This module bridges the generic git detector in `utils::wip_detect` to the
//! Discord completion helpers. It is intentionally best-effort: failing to read
//! git state or failing to send the Discord warning must never fail the turn.

use std::future::Future;
use std::path::Path;

use poise::serenity_prelude as serenity;
use serenity::ChannelId;

use super::SharedData;
use super::gateway::TurnGateway;
use super::inflight::InflightTurnState;
use crate::services::provider::ProviderKind;
use crate::utils::wip_detect::{WipWarning, check_wip_uncommitted_files};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum TurnEndWipWarningOutcome {
    Suppressed,
    Sent,
    SendFailed,
}

pub(in crate::services::discord) fn load_matching_inflight_state(
    provider: &ProviderKind,
    channel_id: ChannelId,
    expected_user_msg_id: Option<u64>,
) -> Option<InflightTurnState> {
    let expected_user_msg_id = expected_user_msg_id?;
    if expected_user_msg_id == 0 {
        return None;
    }
    let state = super::inflight::load_inflight_state(provider, channel_id.get())?;
    if state.user_msg_id == expected_user_msg_id {
        Some(state)
    } else {
        None
    }
}

pub(in crate::services::discord) fn turn_end_wip_warning_text(
    inflight: Option<&InflightTurnState>,
) -> Option<String> {
    let worktree_path = inflight?.worktree_path.as_deref()?.trim();
    if worktree_path.is_empty() {
        return None;
    }
    let workspace = Path::new(worktree_path);
    if !workspace.is_dir() {
        return None;
    }
    let warning = check_wip_uncommitted_files(workspace)?;
    Some(format_turn_end_wip_warning(&warning))
}

fn format_turn_end_wip_warning(warning: &WipWarning) -> String {
    format!(
        "WARNING: WIP uncommitted files detected before turn completion.\n\
         Workspace: `{}`\n\
         Counts: {} staged, {} unstaged, {} untracked.\n\
         Commit or explicitly discard these changes before ending the turn.",
        warning.workspace.display(),
        warning.staged.len(),
        warning.unstaged.len(),
        warning.untracked.len()
    )
}

pub(in crate::services::discord) async fn send_turn_end_wip_warning_with<F, Fut>(
    inflight: Option<&InflightTurnState>,
    source: &'static str,
    mut send_warning: F,
) -> TurnEndWipWarningOutcome
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let Some(text) = turn_end_wip_warning_text(inflight) else {
        return TurnEndWipWarningOutcome::Suppressed;
    };
    match send_warning(text).await {
        Ok(()) => TurnEndWipWarningOutcome::Sent,
        Err(error) => {
            tracing::warn!(
                "[turn_end_wip_warning] failed to send WIP warning from {}: {}",
                source,
                error
            );
            TurnEndWipWarningOutcome::SendFailed
        }
    }
}

pub(in crate::services::discord) async fn warn_turn_end_wip_with_gateway<
    G: TurnGateway + ?Sized,
>(
    gateway: &G,
    channel_id: ChannelId,
    inflight: Option<&InflightTurnState>,
    source: &'static str,
) -> TurnEndWipWarningOutcome {
    send_turn_end_wip_warning_with(inflight, source, |text| async move {
        TurnGateway::send_message(gateway, channel_id, &text)
            .await
            .map(|_| ())
    })
    .await
}

pub(in crate::services::discord) async fn warn_turn_end_wip_with_http(
    http: &serenity::Http,
    channel_id: ChannelId,
    inflight: Option<&InflightTurnState>,
    source: &'static str,
) -> TurnEndWipWarningOutcome {
    send_turn_end_wip_warning_with(inflight, source, |text| async move {
        super::http::send_channel_message(http, channel_id, &text)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    })
    .await
}

pub(in crate::services::discord) async fn warn_turn_end_wip_with_shared_http(
    shared: &SharedData,
    channel_id: ChannelId,
    inflight: Option<&InflightTurnState>,
    source: &'static str,
) -> TurnEndWipWarningOutcome {
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        if turn_end_wip_warning_text(inflight).is_some() {
            tracing::warn!(
                "[turn_end_wip_warning] failed to send WIP warning from {}: no Discord HTTP available",
                source
            );
            return TurnEndWipWarningOutcome::SendFailed;
        }
        return TurnEndWipWarningOutcome::Suppressed;
    };
    warn_turn_end_wip_with_http(&http, channel_id, inflight, source).await
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    use tempfile::TempDir;

    use super::*;
    use crate::services::git::GitCommand;

    struct RuntimeRootGuard {
        previous: Option<std::ffi::OsString>,
        _root: TempDir,
    }

    impl RuntimeRootGuard {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("runtime root tempdir");
            let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
            Self {
                previous,
                _root: root,
            }
        }
    }

    impl Drop for RuntimeRootGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    fn git_available() -> bool {
        GitCommand::new().arg("--version").run_output().is_ok()
    }

    fn init_git_repo() -> Option<TempDir> {
        if !git_available() {
            return None;
        }
        let temp = tempfile::tempdir().expect("tempdir");
        GitCommand::new()
            .repo(temp.path())
            .arg("init")
            .run_output()
            .expect("git init");
        Some(temp)
    }

    fn git(repo: &Path, args: &[&str]) {
        GitCommand::new()
            .repo(repo)
            .args(args.iter().copied())
            .run_output()
            .unwrap_or_else(|error| panic!("git {args:?} failed: {error}"));
    }

    fn committed_repo() -> Option<TempDir> {
        let temp = init_git_repo()?;
        fs::write(temp.path().join("tracked.txt"), "base\n").expect("seed tracked file");
        git(temp.path(), &["add", "tracked.txt"]);
        git(
            temp.path(),
            &[
                "-c",
                "user.name=AgentDesk Test",
                "-c",
                "user.email=agentdesk-test@example.invalid",
                "commit",
                "-m",
                "initial",
            ],
        );
        Some(temp)
    }

    fn inflight_for_worktree(worktree_path: &Path) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            42,
            Some("wip-warning-test".to_string()),
            1,
            2,
            3,
            "test".to_string(),
            None,
            None,
            None,
            None,
            0,
        );
        state.worktree_path = Some(worktree_path.display().to_string());
        state
    }

    #[test]
    fn dirty_warning_text_includes_path_and_counts() {
        let Some(temp) = committed_repo() else {
            return;
        };
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let _root = RuntimeRootGuard::new();
        fs::write(temp.path().join("tracked.txt"), "modified\n").expect("modify tracked file");
        fs::write(temp.path().join("staged.txt"), "staged\n").expect("write staged file");
        git(temp.path(), &["add", "staged.txt"]);
        fs::write(temp.path().join("untracked.txt"), "untracked\n").expect("write untracked");

        let state = inflight_for_worktree(temp.path());
        let text = turn_end_wip_warning_text(Some(&state)).expect("dirty warning text");

        assert!(text.contains("WIP uncommitted files detected"));
        assert!(text.contains(&format!("Workspace: `{}`", temp.path().display())));
        assert!(text.contains("Counts: 1 staged, 1 unstaged, 1 untracked."));
    }

    #[test]
    fn zero_expected_user_msg_id_never_matches_inflight() {
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let _root = RuntimeRootGuard::new();
        let temp = tempfile::tempdir().expect("worktree tempdir");
        let mut state = inflight_for_worktree(temp.path());
        state.user_msg_id = 0;
        crate::services::discord::inflight::save_inflight_state(&state)
            .expect("save zero-owned inflight state");

        assert!(
            load_matching_inflight_state(&ProviderKind::Claude, ChannelId::new(42), Some(0))
                .is_none()
        );
    }

    #[test]
    fn clean_non_git_missing_and_missing_workspace_suppress_warning() {
        let Some(clean) = committed_repo() else {
            return;
        };
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let _root = RuntimeRootGuard::new();
        let clean_state = inflight_for_worktree(clean.path());
        assert_eq!(turn_end_wip_warning_text(Some(&clean_state)), None);

        let non_git = tempfile::tempdir().expect("non git tempdir");
        let non_git_state = inflight_for_worktree(non_git.path());
        assert_eq!(turn_end_wip_warning_text(Some(&non_git_state)), None);

        let missing_path = non_git.path().join("missing");
        let missing_state = inflight_for_worktree(&missing_path);
        assert_eq!(turn_end_wip_warning_text(Some(&missing_state)), None);

        assert_eq!(turn_end_wip_warning_text(None), None);
    }

    #[tokio::test]
    async fn send_failure_is_non_fatal() {
        let Some(temp) = init_git_repo() else {
            return;
        };
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let _root = RuntimeRootGuard::new();
        fs::write(temp.path().join("untracked.txt"), "untracked\n").expect("write untracked");
        let state = inflight_for_worktree(temp.path());

        let outcome = send_turn_end_wip_warning_with(Some(&state), "unit_test", |_text| async {
            Err("discord unavailable".to_string())
        })
        .await;

        assert_eq!(outcome, TurnEndWipWarningOutcome::SendFailed);
    }

    #[tokio::test]
    async fn clean_warning_does_not_call_sender() {
        let Some(temp) = committed_repo() else {
            return;
        };
        let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
        let _root = RuntimeRootGuard::new();
        let state = inflight_for_worktree(temp.path());
        let called = Arc::new(Mutex::new(false));
        let called_for_send = called.clone();

        let outcome = send_turn_end_wip_warning_with(Some(&state), "unit_test", move |_text| {
            let called_for_send = called_for_send.clone();
            async move {
                *called_for_send.lock().expect("called lock") = true;
                Ok(())
            }
        })
        .await;

        assert_eq!(outcome, TurnEndWipWarningOutcome::Suppressed);
        assert!(!*called.lock().expect("called lock"));
    }
}
