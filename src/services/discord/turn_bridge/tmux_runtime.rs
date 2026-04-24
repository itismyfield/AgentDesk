use super::super::*;
use crate::services::discord::InflightRestartMode;
use crate::services::provider::{CancelToken, ProviderKind};
#[cfg(unix)]
use crate::services::tmux_diagnostics::record_tmux_exit_reason;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TmuxCleanupPolicy {
    PreserveSession,
    PreserveSessionAndInflight {
        restart_mode: InflightRestartMode,
    },
    CleanupSession {
        termination_reason_code: Option<&'static str>,
    },
}

impl TmuxCleanupPolicy {
    pub(crate) const fn preserves_inflight(self) -> Option<InflightRestartMode> {
        match self {
            Self::PreserveSessionAndInflight { restart_mode } => Some(restart_mode),
            Self::PreserveSession | Self::CleanupSession { .. } => None,
        }
    }

    pub(crate) const fn should_cleanup_tmux(self) -> bool {
        matches!(self, Self::CleanupSession { .. })
    }

    pub(crate) const fn should_clear_inflight(self) -> bool {
        !matches!(self, Self::PreserveSessionAndInflight { .. })
    }
}

const PROVIDER_INTERRUPT_SETTLE: Duration = Duration::from_millis(750);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProviderTurnInterruptPlan {
    keys: &'static [&'static str],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::services::discord) struct ProviderTurnInterruptOutcome {
    pub tmux_session: Option<String>,
    pub sent_keys: bool,
    pub fallback_sigint_pid: Option<u32>,
}

fn provider_turn_interrupt_plan(provider: &ProviderKind) -> Option<ProviderTurnInterruptPlan> {
    match provider {
        ProviderKind::Claude | ProviderKind::Codex | ProviderKind::Qwen => {
            Some(ProviderTurnInterruptPlan { keys: &["C-c"] })
        }
        ProviderKind::Gemini | ProviderKind::Unsupported(_) => None,
    }
}

fn fallback_sigint_pid_for_provider(
    provider: &ProviderKind,
    ready_for_input: bool,
    provider_pid: Option<u32>,
) -> Option<u32> {
    match provider {
        ProviderKind::Claude => {
            if ready_for_input {
                None
            } else {
                provider_pid
            }
        }
        ProviderKind::Codex | ProviderKind::Qwen => provider_pid,
        ProviderKind::Gemini | ProviderKind::Unsupported(_) => None,
    }
}

pub(in crate::services::discord) async fn interrupt_provider_cli_turn(
    provider: &ProviderKind,
    token: &Arc<CancelToken>,
    reason: &str,
) -> ProviderTurnInterruptOutcome {
    let tmux_session = token
        .tmux_session
        .lock()
        .ok()
        .and_then(|guard| guard.clone());
    let tracked_child_pid = token.child_pid.lock().ok().and_then(|guard| *guard);
    let Some(tmux_session_name) = tmux_session.as_deref() else {
        return ProviderTurnInterruptOutcome {
            tmux_session,
            sent_keys: false,
            fallback_sigint_pid: None,
        };
    };
    let Some(plan) = provider_turn_interrupt_plan(provider) else {
        return ProviderTurnInterruptOutcome {
            tmux_session,
            sent_keys: false,
            fallback_sigint_pid: None,
        };
    };

    let session_for_send = tmux_session_name.to_string();
    let keys = plan.keys.to_vec();
    let send_result = tokio::task::spawn_blocking(move || {
        crate::services::platform::tmux::send_keys(&session_for_send, &keys)
    })
    .await;

    let sent_keys = match send_result {
        Ok(Ok(output)) if output.status.success() => true,
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            tracing::warn!(
                "provider turn interrupt send-keys failed: provider={} session={} reason={} status={} stderr={}",
                provider.as_str(),
                tmux_session_name,
                reason,
                output.status,
                stderr
            );
            false
        }
        Ok(Err(error)) => {
            tracing::warn!(
                "provider turn interrupt send-keys error: provider={} session={} reason={} error={}",
                provider.as_str(),
                tmux_session_name,
                reason,
                error
            );
            false
        }
        Err(error) => {
            tracing::warn!(
                "provider turn interrupt send-keys join error: provider={} session={} reason={} error={}",
                provider.as_str(),
                tmux_session_name,
                reason,
                error
            );
            false
        }
    };

    if !sent_keys {
        return ProviderTurnInterruptOutcome {
            tmux_session,
            sent_keys,
            fallback_sigint_pid: None,
        };
    }

    tokio::time::sleep(PROVIDER_INTERRUPT_SETTLE).await;

    let session_for_probe = tmux_session_name.to_string();
    let provider_for_probe = provider.clone();
    let probe = tokio::task::spawn_blocking(move || {
        let ready_for_input = if matches!(provider_for_probe, ProviderKind::Claude) {
            crate::services::provider::tmux_session_ready_for_input(&session_for_probe)
        } else {
            false
        };
        let provider_pid =
            provider_cli_pid_in_tmux(&session_for_probe, &provider_for_probe, tracked_child_pid);
        (ready_for_input, provider_pid)
    })
    .await;

    let (ready_for_input, provider_pid) = match probe {
        Ok(values) => values,
        Err(error) => {
            tracing::warn!(
                "provider turn interrupt probe join error: provider={} session={} reason={} error={}",
                provider.as_str(),
                tmux_session_name,
                reason,
                error
            );
            (false, None)
        }
    };

    let fallback_sigint_pid =
        fallback_sigint_pid_for_provider(provider, ready_for_input, provider_pid);
    if let Some(pid) = fallback_sigint_pid {
        if let Err(error) = send_sigint(pid) {
            tracing::warn!(
                "provider turn interrupt SIGINT fallback failed: provider={} session={} pid={} reason={} error={}",
                provider.as_str(),
                tmux_session_name,
                pid,
                reason,
                error
            );
        } else {
            tracing::info!(
                "provider turn interrupt SIGINT fallback sent: provider={} session={} pid={} reason={}",
                provider.as_str(),
                tmux_session_name,
                pid,
                reason
            );
        }
    }

    ProviderTurnInterruptOutcome {
        tmux_session,
        sent_keys,
        fallback_sigint_pid,
    }
}

pub(in crate::services::discord) fn cancel_active_token(
    token: &Arc<CancelToken>,
    cleanup_policy: TmuxCleanupPolicy,
    reason: &str,
) -> bool {
    token.cancelled.store(true, Ordering::Relaxed);
    token.set_restart_mode(cleanup_policy.preserves_inflight());
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
#[derive(Debug, Clone, Eq, PartialEq)]
struct ProcessRow {
    pid: u32,
    ppid: u32,
    command: String,
}

#[cfg(unix)]
fn provider_cli_pid_in_tmux(
    tmux_session_name: &str,
    provider: &ProviderKind,
    tracked_child_pid: Option<u32>,
) -> Option<u32> {
    let pane_pid = crate::services::platform::tmux::pane_pid(tmux_session_name)?;
    let rows = process_table();
    let descendants = descendant_processes(pane_pid, &rows);

    if let Some(pid) = tracked_child_pid
        && descendants.iter().any(|row| {
            row.pid == pid
                && !command_is_agentdesk_provider_wrapper(&row.command)
                && command_matches_provider(&row.command, provider)
        })
    {
        return Some(pid);
    }

    descendants
        .into_iter()
        .filter(|row| !command_is_agentdesk_provider_wrapper(&row.command))
        .filter(|row| command_matches_provider(&row.command, provider))
        .max_by_key(|row| provider_command_match_score(&row.command, provider))
        .map(|row| row.pid)
}

#[cfg(not(unix))]
fn provider_cli_pid_in_tmux(
    _tmux_session_name: &str,
    _provider: &ProviderKind,
    _tracked_child_pid: Option<u32>,
) -> Option<u32> {
    None
}

#[cfg(unix)]
fn process_table() -> Vec<ProcessRow> {
    let output = match std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,command="])
        .output()
    {
        Ok(output) if output.status.success() => output,
        _ => return Vec::new(),
    };

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(parse_process_row)
        .collect()
}

#[cfg(unix)]
fn parse_process_row(line: &str) -> Option<ProcessRow> {
    let mut parts = line.split_whitespace();
    let pid = parts.next()?.parse::<u32>().ok()?;
    let ppid = parts.next()?.parse::<u32>().ok()?;
    let command = parts.collect::<Vec<_>>().join(" ");
    if command.is_empty() {
        return None;
    }
    Some(ProcessRow { pid, ppid, command })
}

#[cfg(unix)]
fn descendant_processes(root_pid: u32, rows: &[ProcessRow]) -> Vec<ProcessRow> {
    let mut descendants = Vec::new();
    let mut stack = vec![root_pid];
    while let Some(parent) = stack.pop() {
        for row in rows.iter().filter(|row| row.ppid == parent) {
            descendants.push(row.clone());
            stack.push(row.pid);
        }
    }
    descendants
}

#[cfg(unix)]
fn command_matches_provider(command: &str, provider: &ProviderKind) -> bool {
    provider_command_match_score(command, provider) > 0
}

#[cfg(unix)]
fn provider_command_match_score(command: &str, provider: &ProviderKind) -> u8 {
    let Some(binary) = provider_cli_binary_name(provider) else {
        return 0;
    };
    let lower = command.to_ascii_lowercase();
    let first = lower
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|ch| ch == '\'' || ch == '"');
    let first_basename = std::path::Path::new(first)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(first);

    if first_basename == binary {
        return 3;
    }
    if lower.contains(&format!("/{binary} "))
        || lower.ends_with(&format!("/{binary}"))
        || lower.contains(&format!(" {binary} "))
    {
        return 2;
    }
    if lower.contains(binary) { 1 } else { 0 }
}

#[cfg(unix)]
fn command_is_agentdesk_provider_wrapper(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    lower.contains(" codex-tmux-wrapper")
        || lower.contains(" qwen-tmux-wrapper")
        || lower.contains(" tmux-wrapper")
}

#[cfg(unix)]
fn provider_cli_binary_name(provider: &ProviderKind) -> Option<&'static str> {
    match provider {
        ProviderKind::Claude => Some("claude"),
        ProviderKind::Codex => Some("codex"),
        ProviderKind::Qwen => Some("qwen"),
        ProviderKind::Gemini | ProviderKind::Unsupported(_) => None,
    }
}

#[cfg(unix)]
fn send_sigint(pid: u32) -> Result<(), String> {
    #[allow(unsafe_code)]
    let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

#[cfg(not(unix))]
fn send_sigint(_pid: u32) -> Result<(), String> {
    Err("SIGINT fallback is only supported on Unix".to_string())
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
    let trimmed = saved_response.trim();
    if trimmed.is_empty() {
        "⚠️ AgentDesk가 재시작되어 진행 중이던 응답을 이어붙이지 못했습니다.".to_string()
    } else {
        let formatted = format_for_discord(trimmed);
        format!("{formatted}\n\n[Interrupted by restart]")
    }
}

pub(in crate::services::discord) fn handoff_interrupted_message(
    restart_mode: InflightRestartMode,
    saved_response: &str,
) -> String {
    let trimmed = saved_response.trim();
    match restart_mode {
        InflightRestartMode::DrainRestart => {
            if trimmed.is_empty() {
                "⚠️ dcserver 재시작 중이던 turn을 다시 붙이지 못했습니다. 다음 메시지부터 새 turn으로 이어갑니다.".to_string()
            } else {
                let formatted = format_for_discord(trimmed);
                format!("{formatted}\n\n[Restart handoff incomplete]")
            }
        }
        InflightRestartMode::HotSwapHandoff => {
            if trimmed.is_empty() {
                "⚠️ 런타임 handoff 중 세션 재연결이 완료되지 않았습니다. 다음 메시지부터 새 turn으로 이어갑니다.".to_string()
            } else {
                let formatted = format_for_discord(trimmed);
                format!("{formatted}\n\n[Runtime handoff incomplete]")
            }
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
    use super::{TmuxCleanupPolicy, handoff_interrupted_message, stale_inflight_message};
    use crate::services::discord::InflightRestartMode;
    use crate::services::provider::{CancelToken, ProviderKind};
    use std::sync::Arc;

    #[test]
    fn stale_message_keeps_generic_interrupted_wording() {
        let empty = stale_inflight_message("");
        assert!(empty.contains("이어붙이지 못했습니다"));

        let partial = stale_inflight_message("partial response");
        assert!(partial.contains("partial response"));
        assert!(partial.contains("[Interrupted by restart]"));
        assert!(!partial.contains("[재시작 후 복구 진행 중]"));
    }

    #[test]
    fn preserve_session_and_inflight_sets_restart_mode_on_token() {
        let token = Arc::new(CancelToken::new());
        super::cancel_active_token(
            &token,
            TmuxCleanupPolicy::PreserveSessionAndInflight {
                restart_mode: InflightRestartMode::HotSwapHandoff,
            },
            "test preserve-session stop",
        );
        assert_eq!(
            token.restart_mode(),
            Some(InflightRestartMode::HotSwapHandoff)
        );
    }

    #[test]
    fn drain_restart_message_uses_restart_specific_wording() {
        let text = handoff_interrupted_message(InflightRestartMode::DrainRestart, "");
        assert!(text.contains("dcserver 재시작"));
        assert!(!text.contains("이어붙이지 못했습니다"));
    }

    #[test]
    fn provider_interrupt_plan_covers_managed_tmux_providers_only() {
        assert_eq!(
            super::provider_turn_interrupt_plan(&ProviderKind::Claude).map(|plan| plan.keys),
            Some(&["C-c"][..])
        );
        assert_eq!(
            super::provider_turn_interrupt_plan(&ProviderKind::Codex).map(|plan| plan.keys),
            Some(&["C-c"][..])
        );
        assert_eq!(
            super::provider_turn_interrupt_plan(&ProviderKind::Qwen).map(|plan| plan.keys),
            Some(&["C-c"][..])
        );
        assert!(super::provider_turn_interrupt_plan(&ProviderKind::Gemini).is_none());
    }

    #[test]
    fn provider_interrupt_fallback_respects_provider_idle_semantics() {
        assert_eq!(
            super::fallback_sigint_pid_for_provider(&ProviderKind::Claude, true, Some(42)),
            None
        );
        assert_eq!(
            super::fallback_sigint_pid_for_provider(&ProviderKind::Claude, false, Some(42)),
            Some(42)
        );
        assert_eq!(
            super::fallback_sigint_pid_for_provider(&ProviderKind::Codex, true, Some(42)),
            Some(42)
        );
        assert_eq!(
            super::fallback_sigint_pid_for_provider(&ProviderKind::Qwen, false, Some(42)),
            Some(42)
        );
    }

    #[test]
    fn preserve_session_cancel_keeps_tmux_session_reference() {
        let token = Arc::new(CancelToken::new());
        *token.tmux_session.lock().unwrap() = Some("AgentDesk-codex-test".to_string());

        super::cancel_active_token(&token, TmuxCleanupPolicy::PreserveSession, "test stop");

        assert!(token.cancelled.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(
            token.tmux_session.lock().unwrap().as_deref(),
            Some("AgentDesk-codex-test")
        );
    }

    #[cfg(unix)]
    #[test]
    fn provider_process_matching_prefers_binary_basename() {
        assert_eq!(
            super::provider_command_match_score("/opt/bin/codex exec --json", &ProviderKind::Codex),
            3
        );
        assert_eq!(
            super::provider_command_match_score("node /opt/claude/cli.js", &ProviderKind::Claude),
            1
        );
        assert_eq!(
            super::provider_command_match_score("/bin/bash -lc qwen helper", &ProviderKind::Qwen),
            2
        );
        assert_eq!(
            super::provider_command_match_score(
                "agentdesk codex-tmux-wrapper",
                &ProviderKind::Codex
            ),
            1
        );
        assert!(super::command_is_agentdesk_provider_wrapper(
            "/tmp/agentdesk codex-tmux-wrapper --codex-bin /opt/bin/codex"
        ));
    }
}
