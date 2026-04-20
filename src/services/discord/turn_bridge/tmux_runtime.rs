use super::super::*;
use crate::services::provider::CancelToken;
#[cfg(unix)]
use crate::services::tmux_diagnostics::record_tmux_exit_reason;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TmuxCleanupPolicy {
    PreserveSession,
    CleanupSession {
        termination_reason_code: Option<&'static str>,
    },
}

pub(in crate::services::discord) fn cancel_active_token(
    token: &Arc<CancelToken>,
    cleanup_policy: TmuxCleanupPolicy,
    reason: &str,
) -> bool {
    token.cancelled.store(true, Ordering::Relaxed);
    let mut termination_recorded = false;

    let child_pid = token.child_pid.lock().ok().and_then(|guard| *guard);
    if let Some(pid) = child_pid {
        crate::services::process::kill_pid_tree(pid);
    }

    if let TmuxCleanupPolicy::CleanupSession {
        termination_reason_code,
    } = cleanup_policy
    {
        if child_pid.is_some() {
            if let Some(name) = token
                .tmux_session
                .lock()
                .ok()
                .and_then(|guard| guard.clone())
            {
                #[cfg(unix)]
                {
                    // #145: skip kill for unified-thread sessions with active runs
                    let is_unified =
                        crate::services::provider::parse_provider_and_channel_from_tmux_name(&name)
                            .map(|(_, ch)| {
                                crate::dispatch::is_unified_thread_channel_name_active(&ch)
                            })
                            .unwrap_or(false);
                    if !is_unified {
                        if let Some(reason_code) = termination_reason_code {
                            crate::services::termination_audit::record_termination_for_tmux(
                                &name,
                                None,
                                "turn_bridge",
                                reason_code,
                                Some(&format!("explicit cleanup via {reason}")),
                                None,
                            );
                            termination_recorded = true;
                        }
                        record_tmux_exit_reason(&name, &format!("explicit cleanup via {reason}"));
                        crate::services::platform::tmux::kill_session_with_reason(
                            &name,
                            &format!("explicit cleanup via {reason}"),
                        );
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = &name;
                }
            }
        } else {
            #[cfg(unix)]
            if let Some(name) = token
                .tmux_session
                .lock()
                .ok()
                .and_then(|guard| guard.clone())
            {
                record_tmux_exit_reason(&name, &format!("explicit cleanup via {reason}"));
            }
            token.cancel_with_tmux_cleanup();
        }
    }

    termination_recorded
}

#[cfg(unix)]
pub(crate) fn tmux_runtime_paths(tmux_session_name: &str) -> (String, String) {
    use crate::services::tmux_common::session_temp_path;
    (
        session_temp_path(tmux_session_name, "jsonl"),
        session_temp_path(tmux_session_name, "input"),
    )
}

#[cfg(not(unix))]
pub(crate) fn tmux_runtime_paths(tmux_session_name: &str) -> (String, String) {
    let tmp = std::env::temp_dir();
    (
        tmp.join(format!("agentdesk-{}.jsonl", tmux_session_name))
            .display()
            .to_string(),
        tmp.join(format!("agentdesk-{}.input", tmux_session_name))
            .display()
            .to_string(),
    )
}

pub(in crate::services::discord) fn stale_inflight_message(saved_response: &str) -> String {
    inflight_recovery_message(saved_response, false)
}

pub(in crate::services::discord) fn planned_restart_inflight_message(
    saved_response: &str,
) -> String {
    inflight_recovery_message(saved_response, true)
}

fn inflight_recovery_message(saved_response: &str, planned_restart: bool) -> String {
    let trimmed = saved_response.trim();
    if trimmed.is_empty() {
        if planned_restart {
            "♻️ AgentDesk가 재시작되었습니다. 진행 중이던 응답은 후속 턴에서 이어서 복구합니다."
                .to_string()
        } else {
            "⚠️ AgentDesk가 재시작되어 진행 중이던 응답을 이어붙이지 못했습니다.".to_string()
        }
    } else {
        let formatted = format_for_discord(trimmed);
        if planned_restart {
            format!("{}\n\n[재시작 후 복구 진행 중]", formatted)
        } else {
            format!("{}\n\n[Interrupted by restart]", formatted)
        }
    }
}

pub(super) fn is_dcserver_restart_command(input: &str) -> bool {
    let lower = input.to_lowercase();

    if lower.contains("restart-dcserver") || lower.contains("restart_agentdesk.sh") {
        return true;
    }

    if lower.contains("agentdesk-discord-smoke.sh") && lower.contains("--deploy-live") {
        return true;
    }

    lower.contains("launchctl")
        && lower.contains("com.agentdesk.dcserver")
        && (lower.contains("kickstart") || lower.contains("bootstrap") || lower.contains("bootout"))
}

pub(super) fn should_resume_watcher_after_turn(
    defer_watcher_resume: bool,
    has_local_queued_turns: bool,
    can_chain_locally: bool,
) -> bool {
    !defer_watcher_resume && !(has_local_queued_turns && can_chain_locally)
}

#[cfg(test)]
mod tests {
    use super::{planned_restart_inflight_message, stale_inflight_message};

    #[test]
    fn planned_restart_message_uses_restart_recovery_wording() {
        let empty = planned_restart_inflight_message("");
        assert!(empty.contains("재시작"));
        assert!(empty.contains("복구"));
        assert!(!empty.contains("이어붙이지 못했습니다"));

        let partial = planned_restart_inflight_message("partial response");
        assert!(partial.contains("partial response"));
        assert!(partial.contains("[재시작 후 복구 진행 중]"));
        assert!(!partial.contains("[Interrupted by restart]"));
    }

    #[test]
    fn stale_message_keeps_generic_interrupted_wording() {
        let empty = stale_inflight_message("");
        assert!(empty.contains("이어붙이지 못했습니다"));

        let partial = stale_inflight_message("partial response");
        assert!(partial.contains("partial response"));
        assert!(partial.contains("[Interrupted by restart]"));
        assert!(!partial.contains("[재시작 후 복구 진행 중]"));
    }
}
