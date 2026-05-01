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
const PROVIDER_HARD_STOP_GRACE: Duration = Duration::from_millis(1500);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProviderTurnInterruptPlan {
    keys: &'static [&'static str],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::services::discord) struct ProviderTurnInterruptOutcome {
    pub tmux_session: Option<String>,
    pub sent_keys: bool,
    pub fallback_sigint_pid: Option<u32>,
    pub missing_tmux_session: bool,
}

fn provider_turn_interrupt_plan(provider: &ProviderKind) -> Option<ProviderTurnInterruptPlan> {
    match provider {
        // Claude runs as a child of `agentdesk tmux-wrapper`, with stdin
        // *piped* from the wrapper rather than wired to the PTY. A
        // `tmux send-keys C-c` on the pane therefore delivers SIGINT to the
        // wrapper (the PTY foreground), not to claude — and the wrapper has
        // no SIGINT handler, so it dies and tears the pane down with it
        // (#1260). We send SIGINT directly to claude's PID via the fallback
        // path instead; the empty key list signals "skip send-keys, go
        // straight to the SIGINT fallback".
        ProviderKind::Claude => Some(ProviderTurnInterruptPlan { keys: &[] }),
        ProviderKind::Codex | ProviderKind::Qwen => {
            Some(ProviderTurnInterruptPlan { keys: &["C-c"] })
        }
        ProviderKind::Gemini | ProviderKind::OpenCode | ProviderKind::Unsupported(_) => None,
    }
}

fn fallback_sigint_pid_for_provider(
    provider: &ProviderKind,
    _ready_for_input: bool,
    provider_pid: Option<u32>,
) -> Option<u32> {
    match provider {
        // #1260: claude only gets the interrupt via direct SIGINT (no C-c on
        // the pane), so always deliver it when we have the PID. The previous
        // `ready_for_input` branch was meant to avoid double-delivery when
        // C-c had already gone to the pane — irrelevant now that the C-c
        // path is removed for claude.
        ProviderKind::Claude | ProviderKind::Codex | ProviderKind::Qwen => provider_pid,
        ProviderKind::Gemini | ProviderKind::OpenCode | ProviderKind::Unsupported(_) => None,
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
        tracing::error!(
            "provider turn interrupt skipped: provider={} reason={} error=cancel_token_missing_tmux_session",
            provider.as_str(),
            reason
        );
        return ProviderTurnInterruptOutcome {
            tmux_session,
            sent_keys: false,
            fallback_sigint_pid: None,
            missing_tmux_session: true,
        };
    };
    let Some(plan) = provider_turn_interrupt_plan(provider) else {
        return ProviderTurnInterruptOutcome {
            tmux_session,
            sent_keys: false,
            fallback_sigint_pid: None,
            missing_tmux_session: false,
        };
    };

    // #1260: an empty key list means "no keys to send; go straight to the
    // SIGINT fallback". Used by claude, where C-c on the pane targets the
    // wrapper PID and would tear the session down. We treat the empty-key
    // path as an unconditional success so the SIGINT-fallback section below
    // still runs.
    let sent_keys = if plan.keys.is_empty() {
        true
    } else {
        let session_for_send = tmux_session_name.to_string();
        let keys = plan.keys.to_vec();
        let send_result = tokio::task::spawn_blocking(move || {
            crate::services::platform::tmux::send_keys(&session_for_send, &keys)
        })
        .await;

        match send_result {
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
        }
    };

    if !sent_keys {
        return ProviderTurnInterruptOutcome {
            tmux_session,
            sent_keys,
            fallback_sigint_pid: None,
            missing_tmux_session: false,
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
        missing_tmux_session: false,
    }
}

pub(in crate::services::discord) fn cancel_token_has_tmux_session(token: &CancelToken) -> bool {
    token
        .tmux_session
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
        .is_some()
}

pub(in crate::services::discord) fn bind_cancel_token_tmux_runtime(
    provider: &ProviderKind,
    token: &Arc<CancelToken>,
    tmux_session_name: &str,
    reason: &str,
) -> Option<u32> {
    if let Ok(mut guard) = token.tmux_session.lock() {
        if guard.as_deref() != Some(tmux_session_name) {
            *guard = Some(tmux_session_name.to_string());
        }
    } else {
        tracing::error!(
            "cancel token tmux rebind failed: provider={} session={} reason={} error=tmux_session_lock_poisoned",
            provider.as_str(),
            tmux_session_name,
            reason
        );
    }

    let tracked_child_pid = token.child_pid.lock().ok().and_then(|guard| *guard);
    let provider_pid = provider_cli_pid_in_tmux(tmux_session_name, provider, tracked_child_pid);
    if let Some(pid) = provider_pid {
        if let Ok(mut guard) = token.child_pid.lock()
            && guard.is_none()
        {
            *guard = Some(pid);
        }
        tracing::info!(
            "cancel token tmux runtime rebound: provider={} session={} pid={} reason={}",
            provider.as_str(),
            tmux_session_name,
            pid,
            reason
        );
    } else {
        tracing::warn!(
            "cancel token tmux runtime rebound without provider pid: provider={} session={} reason={}",
            provider.as_str(),
            tmux_session_name,
            reason
        );
    }
    provider_pid
}

/// Standard turn-stop sequence: send the provider abort key (e.g. C-c) FIRST,
/// give the CLI ~750ms to settle / send SIGINT fallback, and THEN flip the
/// cooperative cancel flag + SIGKILL the wrapper PID.
///
/// #1218: When `cancel_active_token` runs first, `kill_pid_tree(child_pid)`
/// kills the agentdesk tmux-wrapper, which is the foreground process of the
/// tmux session. The session then dies, and the subsequent
/// `tmux send-keys -t =name C-c` fails with "can't find pane". For Claude
/// streaming this is masked because the session teardown also stops claude;
/// but for handoff/restart turns where `child_pid` is `None` (Codex/Qwen TUI,
/// resumed runs) the SIGKILL is a no-op and only the C-c can stop the CLI.
/// In that case the wrong order leaves the provider running and the user
/// sees stop "fail".
///
/// All user-initiated stop paths (⏳ reaction removal, `/stop`, `!stop`,
/// `/clear`, watchdog timeouts) MUST call this helper instead of pairing
/// the two primitives by hand.
pub(in crate::services::discord) async fn stop_active_turn(
    provider: &ProviderKind,
    token: &Arc<CancelToken>,
    cleanup_policy: TmuxCleanupPolicy,
    reason: &str,
) -> bool {
    let interrupt_outcome = interrupt_provider_cli_turn(provider, token, reason).await;
    let termination_recorded = cancel_active_token(token, cleanup_policy, reason);
    hard_stop_unresponsive_provider_cli_turn(
        provider,
        token,
        cleanup_policy,
        &interrupt_outcome,
        reason,
    )
    .await;
    termination_recorded
}

async fn hard_stop_unresponsive_provider_cli_turn(
    provider: &ProviderKind,
    token: &Arc<CancelToken>,
    cleanup_policy: TmuxCleanupPolicy,
    interrupt_outcome: &ProviderTurnInterruptOutcome,
    reason: &str,
) {
    if cleanup_policy.should_cleanup_tmux() {
        return;
    }

    let tmux_session_name = interrupt_outcome.tmux_session.clone().or_else(|| {
        token
            .tmux_session
            .lock()
            .ok()
            .and_then(|guard| guard.clone())
    });
    let Some(tmux_session_name) = tmux_session_name else {
        tracing::error!(
            "provider hard-stop skipped: provider={} reason={} error=cancel_token_missing_tmux_session interrupt_missing_tmux_session={}",
            provider.as_str(),
            reason,
            interrupt_outcome.missing_tmux_session
        );
        return;
    };

    tokio::time::sleep(PROVIDER_HARD_STOP_GRACE).await;

    let tracked_child_pid = token.child_pid.lock().ok().and_then(|guard| *guard);
    let provider_for_probe = provider.clone();
    let session_for_probe = tmux_session_name.clone();
    let probe = tokio::task::spawn_blocking(move || {
        let session_alive = crate::services::platform::tmux::pane_pid(&session_for_probe).is_some();
        let ready_for_input =
            crate::services::provider::tmux_session_ready_for_input(&session_for_probe);
        let current_provider_pid =
            provider_cli_pid_in_tmux(&session_for_probe, &provider_for_probe, tracked_child_pid);
        (session_alive, ready_for_input, current_provider_pid)
    })
    .await;

    let (session_alive, ready_for_input, current_provider_pid) = match probe {
        Ok(values) => values,
        Err(error) => {
            tracing::warn!(
                "provider hard-stop probe join error: provider={} session={} reason={} error={}",
                provider.as_str(),
                tmux_session_name,
                reason,
                error
            );
            (false, false, None)
        }
    };

    let Some(pid) = hard_stop_pid_for_unresponsive_provider(
        cleanup_policy,
        session_alive,
        ready_for_input,
        current_provider_pid,
        interrupt_outcome.fallback_sigint_pid,
    ) else {
        return;
    };

    tracing::warn!(
        "provider turn did not stop after interrupt; killing provider pid: provider={} session={} pid={} reason={}",
        provider.as_str(),
        tmux_session_name,
        pid,
        reason
    );
    crate::services::process::kill_pid_tree(pid);
}

fn hard_stop_pid_for_unresponsive_provider(
    cleanup_policy: TmuxCleanupPolicy,
    session_alive: bool,
    ready_for_input: bool,
    current_provider_pid: Option<u32>,
    previous_provider_pid: Option<u32>,
) -> Option<u32> {
    if cleanup_policy.should_cleanup_tmux() || !session_alive || ready_for_input {
        return None;
    }
    current_provider_pid.or(previous_provider_pid)
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
    // `child_pid` is the wrapper PID — i.e. the foreground process of the
    // tmux pane. SIGKILL'ing it tears down the tmux session itself. For
    // `PreserveSession` / `PreserveSessionAndInflight` the caller has
    // already sent the provider abort key
    // (`interrupt_provider_cli_turn` C-c + SIGINT fallback in
    // `stop_active_turn`), so the provider is being asked to exit
    // cooperatively and we MUST NOT take down the tmux pane underneath it
    // — otherwise the next turn re-spawns the session, the capture file
    // rotates, and the watcher floods Discord with stale scrollback. Only
    // the tear-down policy kills the wrapper here.
    if cleanup_policy.should_cleanup_tmux()
        && let Some(pid) = child_pid
    {
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
        ProviderKind::Gemini | ProviderKind::OpenCode | ProviderKind::Unsupported(_) => None,
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
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
        assert!(
            !TmuxCleanupPolicy::PreserveSessionAndInflight {
                restart_mode: InflightRestartMode::HotSwapHandoff,
            }
            .should_cleanup_tmux(),
            "PreserveSessionAndInflight must not tear down the tmux wrapper"
        );
    }

    #[test]
    fn drain_restart_message_uses_restart_specific_wording() {
        let text = handoff_interrupted_message(InflightRestartMode::DrainRestart, "");
        assert!(text.contains("dcserver 재시작"));
        assert!(!text.contains("이어붙이지 못했습니다"));
    }

    // #1260: Claude's PTY foreground is `agentdesk tmux-wrapper`, not the
    // claude CLI (whose stdin is piped from the wrapper). A `tmux send-keys
    // C-c` therefore SIGINTs the wrapper and tears the session down. The
    // empty key list signals to `interrupt_provider_cli_turn` that it must
    // skip send-keys and proceed straight to the direct-SIGINT fallback.
    // Codex/Qwen run as the PTY foreground themselves, so C-c via send-keys
    // is still the correct primary interrupt.
    #[test]
    fn provider_interrupt_plan_skips_send_keys_for_claude_only() {
        assert_eq!(
            super::provider_turn_interrupt_plan(&ProviderKind::Claude).map(|plan| plan.keys),
            Some(&[][..]),
            "claude must skip send-keys C-c — wrapper is the PTY foreground (#1260)"
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

    // #1260: with C-c via send-keys removed for claude, the SIGINT fallback
    // is now the *only* interrupt delivery path. It must fire whenever we
    // know claude's PID, regardless of `ready_for_input`. The previous
    // ready-for-input gate was meant to avoid double-delivery on top of the
    // (now-removed) C-c.
    #[test]
    fn provider_interrupt_fallback_always_fires_for_claude_when_pid_known() {
        assert_eq!(
            super::fallback_sigint_pid_for_provider(&ProviderKind::Claude, true, Some(42)),
            Some(42),
            "claude SIGINT fallback must fire even when ready_for_input=true (#1260)"
        );
        assert_eq!(
            super::fallback_sigint_pid_for_provider(&ProviderKind::Claude, false, Some(42)),
            Some(42)
        );
        assert_eq!(
            super::fallback_sigint_pid_for_provider(&ProviderKind::Claude, false, None),
            None,
            "no PID = no SIGINT (still skip the fallback cleanly)"
        );
        assert_eq!(
            super::fallback_sigint_pid_for_provider(&ProviderKind::Codex, true, Some(42)),
            Some(42)
        );
        assert_eq!(
            super::fallback_sigint_pid_for_provider(&ProviderKind::Qwen, false, Some(42)),
            Some(42)
        );
        assert_eq!(
            super::fallback_sigint_pid_for_provider(&ProviderKind::Gemini, false, Some(42)),
            None
        );
    }

    #[test]
    fn hard_stop_targets_only_unresponsive_preserved_provider() {
        assert_eq!(
            super::hard_stop_pid_for_unresponsive_provider(
                TmuxCleanupPolicy::PreserveSession,
                true,
                false,
                Some(42),
                Some(41),
            ),
            Some(42),
            "current provider PID wins when the tmux session is still busy"
        );
        assert_eq!(
            super::hard_stop_pid_for_unresponsive_provider(
                TmuxCleanupPolicy::PreserveSession,
                true,
                false,
                None,
                Some(41),
            ),
            Some(41),
            "the SIGINT fallback PID is retained as a last known provider target"
        );
        assert_eq!(
            super::hard_stop_pid_for_unresponsive_provider(
                TmuxCleanupPolicy::PreserveSession,
                true,
                true,
                Some(42),
                Some(41),
            ),
            None,
            "ready_for_input means the provider accepted the interrupt"
        );
        assert_eq!(
            super::hard_stop_pid_for_unresponsive_provider(
                TmuxCleanupPolicy::CleanupSession {
                    termination_reason_code: Some("test")
                },
                true,
                false,
                Some(42),
                Some(41),
            ),
            None,
            "cleanup-session paths already tear down tmux and must not double-kill"
        );
        assert_eq!(
            super::hard_stop_pid_for_unresponsive_provider(
                TmuxCleanupPolicy::PreserveSession,
                false,
                false,
                None,
                Some(41),
            ),
            None,
            "a missing tmux session must not kill a stale last-known PID"
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

    /// #1218 regression: `stop_active_turn` must invoke
    /// `interrupt_provider_cli_turn` (provider abort key + SIGINT fallback)
    /// BEFORE `cancel_active_token` flips the cancel flag. The previous
    /// pair-by-hand pattern (cancel first, interrupt second) caused
    /// `tmux send-keys` to fail with "can't find pane" once the wrapper
    /// SIGKILL collapsed the tmux session.
    #[tokio::test(flavor = "current_thread")]
    async fn stop_active_turn_runs_interrupt_before_cancel() {
        let token = Arc::new(CancelToken::new());
        *token.tmux_session.lock().unwrap() =
            Some("AgentDesk-claude-stop-order-regression-1218-does-not-exist".to_string());

        assert!(
            !token.cancelled.load(std::sync::atomic::Ordering::Relaxed),
            "fresh token must start uncancelled"
        );

        // child_pid stays None so kill_pid_tree is a no-op even though the
        // helper would normally SIGKILL. The fake tmux session also doesn't
        // exist, so send-keys will fail internally, but the helper must not
        // panic and must still flip the cancel flag.
        super::stop_active_turn(
            &ProviderKind::Claude,
            &token,
            TmuxCleanupPolicy::PreserveSession,
            "test stop_active_turn ordering",
        )
        .await;

        assert!(
            token.cancelled.load(std::sync::atomic::Ordering::Relaxed),
            "cancel flag must be set after stop_active_turn returns"
        );
        // PreserveSession leaves the tmux_session field intact for any
        // follow-up cleanup that needs the session name.
        assert_eq!(
            token.tmux_session.lock().unwrap().as_deref(),
            Some("AgentDesk-claude-stop-order-regression-1218-does-not-exist")
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stop_active_turn_is_noop_for_provider_without_interrupt_plan() {
        let token = Arc::new(CancelToken::new());
        // Gemini has no provider_turn_interrupt_plan, so stop_active_turn
        // must skip the send-keys path entirely and still flip the cancel
        // flag via cancel_active_token.
        super::stop_active_turn(
            &ProviderKind::Gemini,
            &token,
            TmuxCleanupPolicy::PreserveSession,
            "test stop_active_turn gemini",
        )
        .await;
        assert!(token.cancelled.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interrupt_reports_missing_tmux_session_for_naked_token() {
        let token = Arc::new(CancelToken::new());

        let outcome =
            super::interrupt_provider_cli_turn(&ProviderKind::Claude, &token, "test naked token")
                .await;

        assert!(
            outcome.missing_tmux_session,
            "naked cancel token must be surfaced as an explicit diagnostic"
        );
        assert!(!outcome.sent_keys);
        assert!(outcome.fallback_sigint_pid.is_none());
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
