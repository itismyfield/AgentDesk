use super::gateway::DiscordGateway;
use super::settings::{
    load_last_remote_profile, load_last_session_path, resolve_role_binding,
    validate_bot_channel_routing_with_provider_channel,
};
use super::turn_bridge::stale_inflight_message;
use super::*;
use crate::db::turns::TurnTokenUsage;
use crate::services::agent_protocol::{
    RuntimeHandoff, RuntimeHandoffKind, StatusEvent, StreamMessage,
};
use crate::services::git::GitCommand;
#[cfg(unix)]
use crate::services::platform::binary_resolver;
#[cfg(unix)]
use crate::services::tmux_common::tmux_exact_target;
#[cfg(unix)]
use crate::services::tmux_diagnostics::{build_tmux_death_diagnostic, tmux_session_has_live_pane};
use crate::utils::format::tail_with_ellipsis;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::Command;

#[cfg(not(unix))]
fn tmux_session_has_live_pane(_name: &str) -> bool {
    false
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryPhase {
    Pending,
    WatcherReattach,
    InflightRestore,
    Done,
}

impl RecoveryPhase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::WatcherReattach => "watcher_reattach",
            Self::InflightRestore => "inflight_restore",
            Self::Done => "done",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "pending" => Some(Self::Pending),
            "watcher_reattach" => Some(Self::WatcherReattach),
            "inflight_restore" => Some(Self::InflightRestore),
            "done" => Some(Self::Done),
            _ => None,
        }
    }

    pub fn from_optional_str(value: Option<&str>) -> Option<Self> {
        value.and_then(Self::from_str)
    }
}

fn canonical_recovery_phase(phase: RecoveryPhase) -> RecoveryPhase {
    RecoveryPhase::from_optional_str(Some(phase.as_str())).unwrap_or(phase)
}

fn recovery_input_fifo_for_runtime(
    runtime_kind: RuntimeHandoffKind,
    input_fifo_path: Option<String>,
) -> Result<Option<String>, &'static str> {
    if runtime_kind.requires_input_fifo() {
        input_fifo_path
            .filter(|path| !path.is_empty())
            .map(Some)
            .ok_or("input fifo path missing during recovery")
    } else {
        Ok(input_fifo_path.filter(|path| !path.is_empty()))
    }
}

fn runtime_handoff_for_recovery(
    runtime_kind: RuntimeHandoffKind,
    output_path: String,
    input_fifo_path: Option<String>,
    tmux_session_name: String,
    session_id: Option<String>,
    last_offset: u64,
) -> RuntimeHandoff {
    match runtime_kind {
        RuntimeHandoffKind::LegacyTmuxWrapper => RuntimeHandoff::LegacyTmuxWrapper {
            output_path,
            input_fifo_path: input_fifo_path.unwrap_or_default(),
            tmux_session_name,
            last_offset,
        },
        RuntimeHandoffKind::ClaudeTui => RuntimeHandoff::ClaudeTui {
            transcript_path: output_path,
            tmux_session_name,
            last_offset,
        },
        RuntimeHandoffKind::CodexTui => RuntimeHandoff::CodexTui {
            rollout_path: output_path,
            thread_id: session_id,
            tmux_session_name,
            last_offset,
        },
        RuntimeHandoffKind::ProcessBackend => RuntimeHandoff::ProcessBackend {
            output_path,
            session_name: tmux_session_name,
            last_offset,
        },
    }
}

fn emit_recovery_quality_event(
    provider: &ProviderKind,
    channel_id: u64,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    reason: &str,
) {
    crate::services::observability::emit_agent_quality_event(
        crate::services::observability::AgentQualityEvent {
            source_event_id: session_key
                .map(str::to_string)
                .or_else(|| dispatch_id.map(str::to_string)),
            correlation_id: dispatch_id
                .map(str::to_string)
                .or_else(|| session_key.map(str::to_string)),
            agent_id: None,
            provider: Some(provider.as_str().to_string()),
            channel_id: Some(channel_id.to_string()),
            card_id: None,
            dispatch_id: dispatch_id.map(str::to_string),
            event_type: "recovery_fired".to_string(),
            payload: serde_json::json!({
                "reason": reason,
                "session_key": session_key,
            }),
        },
    );
}

fn should_advance_recovery_dispatch_after_relay(relay_ok: bool) -> bool {
    relay_ok
}

async fn relay_recovery_terminal_notice(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    state: &super::inflight::InflightTurnState,
    text: &str,
) -> bool {
    super::formatting::replace_long_message_raw(
        http,
        ChannelId::new(state.channel_id),
        MessageId::new(state.current_msg_id),
        text,
        shared,
    )
    .await
    .is_ok()
}

async fn complete_recovery_visible_turn(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &super::inflight::InflightTurnState,
    background: bool,
    source: &'static str,
) {
    let channel_id = ChannelId::new(state.channel_id);
    let user_msg_id = MessageId::new(state.user_msg_id);
    super::formatting::remove_reaction_raw(http, channel_id, user_msg_id, '⏳').await;
    super::formatting::add_reaction_raw(http, channel_id, user_msg_id, '✅').await;

    if !shared.status_panel_v2_enabled {
        return;
    }
    let Some(status_msg_id) = state.status_message_id.map(MessageId::new) else {
        return;
    };

    shared
        .placeholder_live_events
        .push_status_event(channel_id, StatusEvent::TurnCompleted { background });
    let started_at_unix = super::inflight::parse_started_at_unix(&state.started_at)
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    let panel_text =
        shared
            .placeholder_live_events
            .render_status_panel(channel_id, provider, started_at_unix);

    rate_limit_wait(shared, channel_id).await;
    if let Err(error) =
        super::http::edit_channel_message(http, channel_id, status_msg_id, &panel_text).await
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ recovery status-panel-v2 completion edit failed from {source} for msg {} in channel {}: {}",
            status_msg_id.get(),
            channel_id.get(),
            error
        );
    }
}

#[cfg(test)]
mod recovery_dispatch_gate_tests {
    #[test]
    fn recovery_dispatch_advance_requires_successful_relay() {
        assert!(super::should_advance_recovery_dispatch_after_relay(true));
        assert!(!super::should_advance_recovery_dispatch_after_relay(false));
    }
}

/// Retry-aware tmux session check for recovery after dcserver restart.
/// The first check can false-negative if tmux CLI hasn't fully initialized yet.
fn tmux_session_alive_with_retry(name: &str) -> bool {
    if tmux_session_has_live_pane(name) {
        return true;
    }
    // Retry up to 2 more times with 1-second gaps
    for attempt in 1..=2 {
        std::thread::sleep(std::time::Duration::from_secs(1));
        if tmux_session_has_live_pane(name) {
            tracing::info!(
                "  [recovery] tmux pane alive on retry {} for {}",
                attempt,
                name
            );
            return true;
        }
    }
    false
}

/// Retry-aware tmux has_session check.
fn tmux_has_session_with_retry(name: &str) -> bool {
    if crate::services::platform::tmux::has_session(name) {
        return true;
    }
    for attempt in 1..=2 {
        std::thread::sleep(std::time::Duration::from_secs(1));
        if crate::services::platform::tmux::has_session(name) {
            tracing::info!(
                "  [recovery] tmux session found on retry {} for {}",
                attempt,
                name
            );
            return true;
        }
    }
    false
}

#[cfg(not(unix))]
fn build_tmux_death_diagnostic(_name: &str, _output_path: Option<&str>) -> Option<String> {
    None
}

fn interrupted_recovery_message(
    state: &inflight::InflightTurnState,
    saved_response: &str,
) -> String {
    state
        .restart_mode
        .map(|mode| super::turn_bridge::handoff_interrupted_message(mode, saved_response))
        .unwrap_or_else(|| stale_inflight_message(saved_response))
}

fn save_missing_session_handoff(
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    best_response: &str,
) {
    let partial = best_response.trim();
    let partial_summary = if partial.is_empty() {
        "partial response unavailable".to_string()
    } else {
        tail_with_ellipsis(partial, 160)
    };
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] ⚠ recovery: suppressed auto post-restart handoff for channel {} (provider={}, user_msg_id={}, partial={})",
        state.channel_id,
        provider.as_str(),
        state.user_msg_id,
        partial_summary
    );
}

fn can_replace_stale_rebind_inflight(state: &inflight::InflightTurnState) -> bool {
    state.rebind_origin
        && state.full_response.trim().is_empty()
        && state.last_watcher_relayed_offset.is_none()
}

fn can_resume_existing_rebind_inflight(state: &inflight::InflightTurnState) -> bool {
    !state.rebind_origin
        && state.request_owner_user_id != 0
        && state.user_msg_id != 0
        && state.current_msg_id != 0
        && state.full_response.trim().is_empty()
        && state.last_watcher_relayed_offset.is_none()
}

fn recovery_phase_for_existing_inflight_rebind(
    state: &inflight::InflightTurnState,
) -> RecoveryPhase {
    let phase = match (
        can_replace_stale_rebind_inflight(state),
        can_resume_existing_rebind_inflight(state),
    ) {
        (true, _) => RecoveryPhase::WatcherReattach,
        (false, true) => RecoveryPhase::InflightRestore,
        (false, false) => RecoveryPhase::Pending,
    };
    canonical_recovery_phase(phase)
}

fn can_fast_path_captured_full_response(
    state: &inflight::InflightTurnState,
    output_already_completed: bool,
) -> bool {
    // Deliberately keep the gate narrow: only real user-authored inflights
    // with a captured but completely unsent response are eligible, and only
    // after authoritative completion evidence exists in the output stream.
    !state.rebind_origin
        && output_already_completed
        && !state.full_response.trim().is_empty()
        && state.response_sent_offset == 0
        && state.last_watcher_relayed_offset.is_none()
}

fn recovery_phase_after_output_scan(
    output_already_completed: bool,
    tmux_ready_without_new_output: bool,
) -> RecoveryPhase {
    let phase = match (output_already_completed, tmux_ready_without_new_output) {
        (true, _) | (_, true) => RecoveryPhase::Done,
        (false, false) => RecoveryPhase::Pending,
    };
    canonical_recovery_phase(phase)
}

fn recovery_has_post_work_ready_evidence(state: &inflight::InflightTurnState) -> bool {
    !state.full_response.trim().is_empty()
        || state.any_tool_used
        || state.has_post_tool_text
        || state
            .current_tool_line
            .as_deref()
            .is_some_and(|line| !line.trim().is_empty())
        || state
            .last_tool_name
            .as_deref()
            .is_some_and(|name| !name.trim().is_empty())
        || state
            .last_tool_summary
            .as_deref()
            .is_some_and(|summary| !summary.trim().is_empty())
}

fn recovery_phase_after_tmux_probe(can_recover: bool, pane_alive: Option<bool>) -> RecoveryPhase {
    let phase = match (can_recover, pane_alive) {
        (false, _) => RecoveryPhase::Done,
        (true, Some(true)) => RecoveryPhase::WatcherReattach,
        (true, Some(false)) => RecoveryPhase::InflightRestore,
        (true, None) => RecoveryPhase::Pending,
    };
    canonical_recovery_phase(phase)
}

fn recovery_worktree_path(state: &inflight::InflightTurnState) -> Option<&str> {
    state
        .worktree_path
        .as_deref()
        .filter(|path| !path.trim().is_empty())
}

fn recovery_worktree_branch(state: &inflight::InflightTurnState) -> Option<&str> {
    state
        .worktree_branch
        .as_deref()
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
}

fn recovery_dispatch_id(state: &inflight::InflightTurnState) -> Option<&str> {
    state
        .dispatch_id
        .as_deref()
        .map(str::trim)
        .filter(|dispatch_id| !dispatch_id.is_empty())
}

fn recovery_tmux_session_name(
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
) -> Option<String> {
    state
        .tmux_session_name
        .as_deref()
        .or_else(|| state.channel_name.as_deref())
        .map(|name| {
            if name.starts_with(&format!(
                "{}-",
                crate::services::provider::TMUX_SESSION_PREFIX
            )) {
                name.to_string()
            } else {
                provider.build_tmux_session_name(name)
            }
        })
}

fn recovery_requires_worktree_context(state: &inflight::InflightTurnState) -> bool {
    recovery_worktree_branch(state).is_some()
        || state
            .base_commit
            .as_deref()
            .is_some_and(|commit| !commit.trim().is_empty())
}

fn recovery_git_stdout(repo_path: &str, args: &[&str]) -> Option<String> {
    let output = GitCommand::new()
        .repo(repo_path)
        .args(args)
        .run_output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if stdout.is_empty() {
        return None;
    }
    Some(stdout)
}

fn recovery_worktree_original_path(worktree_path: &str) -> Option<String> {
    let git_common_dir = recovery_git_stdout(worktree_path, &["rev-parse", "--git-common-dir"])?;
    let common_dir = {
        let candidate = std::path::PathBuf::from(&git_common_dir);
        if candidate.is_absolute() {
            candidate
        } else {
            std::path::Path::new(worktree_path).join(candidate)
        }
    };
    let canonical = std::fs::canonicalize(common_dir).ok()?;
    canonical.parent()?.to_str().map(str::to_string)
}

fn recovery_worktree_info(state: &inflight::InflightTurnState) -> Option<WorktreeInfo> {
    let worktree_path = recovery_worktree_path(state)?;
    if !std::path::Path::new(worktree_path).is_dir() {
        return None;
    }

    let branch_name = recovery_worktree_branch(state)
        .map(str::to_string)
        .or_else(|| recovery_git_stdout(worktree_path, &["branch", "--show-current"]))?;
    let original_path = recovery_worktree_original_path(worktree_path)?;

    Some(WorktreeInfo {
        original_path,
        worktree_path: worktree_path.to_string(),
        branch_name,
    })
}

fn restore_recovered_session_worktree(
    session: &mut DiscordSession,
    state: &inflight::InflightTurnState,
) {
    if let Some(worktree) = recovery_worktree_info(state) {
        if session.current_path.is_none() {
            session.current_path = Some(worktree.worktree_path.clone());
        }
        session.worktree = Some(worktree);
    }
}

fn recovery_spawn_adk_cwd(
    state: &inflight::InflightTurnState,
    persisted_session_path: Option<String>,
) -> Result<Option<String>, String> {
    if let Some(worktree_path) = recovery_worktree_path(state) {
        if std::path::Path::new(worktree_path).is_dir() {
            return Ok(Some(worktree_path.to_string()));
        }
        return Err(format!(
            "recovery blocked: inflight worktree missing for channel {}: {}",
            state.channel_id, worktree_path
        ));
    }

    if recovery_requires_worktree_context(state) {
        let dispatch_suffix = recovery_dispatch_id(state)
            .map(|dispatch_id| format!(" (dispatch {dispatch_id})"))
            .unwrap_or_default();
        return Err(format!(
            "recovery blocked: inflight worktree state missing for channel {}{}",
            state.channel_id, dispatch_suffix
        ));
    }

    Ok(persisted_session_path)
}

async fn finish_recovered_turn_mailbox(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    stop_source: &'static str,
) {
    let finish = mailbox_finish_turn(shared, provider, channel_id).await;
    if let Some(removed_token) = finish.removed_token {
        removed_token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        shared
            .global_active
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }

    super::clear_watchdog_deadline_override(channel_id.get()).await;
    shared
        .dispatch_thread_parents
        .retain(|_, thread| *thread != channel_id);

    if !finish.has_pending {
        shared.dispatch_role_overrides.remove(&channel_id);
    }

    if finish.mailbox_online && finish.has_pending {
        super::schedule_deferred_idle_queue_kickoff(
            shared.clone(),
            provider.clone(),
            channel_id,
            stop_source,
        );
    }
}

pub(super) async fn reregister_active_turn_from_inflight(
    shared: &Arc<SharedData>,
    state: &inflight::InflightTurnState,
) -> bool {
    if state.current_msg_id == 0 || state.user_msg_id == 0 || state.request_owner_user_id == 0 {
        return false;
    }

    let channel_id = ChannelId::new(state.channel_id);
    let user_msg_id = MessageId::new(state.user_msg_id);
    let snapshot = super::mailbox_snapshot(shared, channel_id).await;
    let Some(provider) = ProviderKind::from_str(&state.provider) else {
        tracing::error!(
            "inflight reregister failed: provider={} channel_id={} error=unsupported_provider",
            state.provider,
            state.channel_id
        );
        return false;
    };
    if snapshot.cancel_token.is_some() {
        if let Some(token) = snapshot.cancel_token.as_ref()
            && snapshot.active_user_message_id == Some(user_msg_id)
        {
            super::ensure_cancel_token_bound_from_inflight_state(
                &provider,
                state,
                token,
                "inflight reregister existing active turn",
            );
        }
        return snapshot.active_user_message_id == Some(user_msg_id);
    }

    let cancel_token = Arc::new(CancelToken::new());
    super::ensure_cancel_token_bound_from_inflight_state(
        &provider,
        state,
        &cancel_token,
        "inflight reregister active turn",
    );

    super::mailbox_try_start_turn(
        shared,
        channel_id,
        cancel_token,
        UserId::new(state.request_owner_user_id),
        user_msg_id,
    )
    .await
}
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct DetectedRebindOutputPath {
    path: String,
    initial_offset: u64,
}

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct LsofOutputCandidate {
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
struct StaleOutputCandidate {
    fd: String,
    raw_path: String,
    inode: Option<u64>,
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
fn parse_lsof_output_candidates(stdout: &str) -> Vec<LsofOutputCandidate> {
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
fn detect_rebind_output_path_from_candidates(
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

#[cfg(unix)]
fn tmux_pane_pid(tmux_session_name: &str) -> Option<u32> {
    let mut cmd = Command::new("tmux");
    binary_resolver::apply_runtime_path(&mut cmd);
    let output = cmd
        .args([
            "display-message",
            "-p",
            "-t",
            &tmux_exact_target(tmux_session_name),
            "#{pane_pid}",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

#[cfg(unix)]
fn detect_live_tmux_output_path(
    tmux_session_name: &str,
    fallback_path: &str,
) -> Result<Option<DetectedRebindOutputPath>, StaleOutputCandidate> {
    let Some(pane_pid) = tmux_pane_pid(tmux_session_name) else {
        return Ok(None);
    };
    let mut cmd = Command::new("lsof");
    binary_resolver::apply_runtime_path(&mut cmd);
    let output = match cmd.args(["-Fn", "-p", &pane_pid.to_string()]).output() {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };
    if !output.status.success() {
        return Ok(None);
    }
    let stdout = match String::from_utf8(output.stdout) {
        Ok(stdout) => stdout,
        Err(_) => return Ok(None),
    };
    let candidates = parse_lsof_output_candidates(&stdout);
    detect_rebind_output_path_from_candidates(fallback_path, candidates)
}

/// Check whether a **successful** result record exists after the given offset.
/// Error results are not considered completion — they should not trigger the
/// recovery completed-turn path (✅ reaction, idle dispatch, etc.).
fn success_result_end_offset_after_offset(output_path: &str, start_offset: u64) -> Option<u64> {
    let Ok(bytes) = std::fs::read(output_path) else {
        return None;
    };
    let start = usize::try_from(start_offset)
        .ok()
        .map(|offset| offset.min(bytes.len()))
        .unwrap_or(bytes.len());

    let mut absolute_end = start as u64;
    for segment in bytes[start..].split_inclusive(|byte| *byte == b'\n') {
        absolute_end = absolute_end.saturating_add(segment.len() as u64);
        let line = String::from_utf8_lossy(segment);
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let is_result = value.get("type").and_then(|v| v.as_str()) == Some("result");
        let is_error = value
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if is_result && !is_error {
            return Some(absolute_end);
        }
    }

    None
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn output_has_result_after_offset(output_path: &str, start_offset: u64) -> bool {
    success_result_end_offset_after_offset(output_path, start_offset).is_some()
}

/// Extract accumulated assistant text from output JSONL after the given offset.
fn extract_response_from_output(output_path: &str, start_offset: u64) -> String {
    extract_response_from_output_pub(output_path, start_offset)
}

fn extract_turn_analytics_from_output(
    output_path: &str,
    start_offset: u64,
) -> (Option<String>, Option<TurnTokenUsage>) {
    crate::services::session_backend::extract_turn_analytics_from_output(output_path, start_offset)
}

fn recovered_turn_duration_ms(started_at: Option<&str>) -> Option<i64> {
    let started_at = started_at?.trim();
    if started_at.is_empty() {
        return None;
    }
    let parsed = chrono::NaiveDateTime::parse_from_str(started_at, "%Y-%m-%d %H:%M:%S").ok()?;
    let elapsed = chrono::Local::now().naive_local() - parsed;
    Some(elapsed.num_milliseconds().max(0))
}

async fn lookup_turn_finished_dispatch_kind(dispatch_id: Option<&str>) -> Option<String> {
    let dispatch_id = dispatch_id?;
    let body = super::internal_api::lookup_dispatch_info(dispatch_id)
        .await
        .ok()?;
    super::turn_bridge::classify_turn_finished_dispatch_kind(
        body.get("dispatch_context")
            .and_then(|value| value.as_str()),
        body.get("dispatch_type").and_then(|value| value.as_str()),
    )
    .map(str::to_string)
}

async fn persist_recovered_transcript(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    dispatch_id: Option<&str>,
    assistant_message: &str,
) -> bool {
    let assistant_message = assistant_message.trim();
    if assistant_message.is_empty() {
        return false;
    }

    let turn_id = format!("discord:{}:{}", state.channel_id, state.user_msg_id);
    let channel_id_text = state.channel_id.to_string();
    match crate::db::session_transcripts::persist_turn_db(
        db,
        pg_pool,
        crate::db::session_transcripts::PersistSessionTranscript {
            turn_id: &turn_id,
            session_key: state.session_key.as_deref(),
            channel_id: Some(channel_id_text.as_str()),
            agent_id: None,
            provider: Some(provider.as_str()),
            dispatch_id,
            user_message: &state.user_text,
            assistant_message,
            events: &[],
            duration_ms: None,
        },
    )
    .await
    {
        Ok(_) => true,
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ recovery: failed to persist session transcript: {e}");
            false
        }
    }
}

/// Public wrapper for turn_bridge fallback recovery.
///
/// Mirrors the `resolve_done_response` logic from `turn_bridge.rs`:
/// when tool_use was seen and no post-tool assistant text followed,
/// prefer the `result` record over stale pre-tool narration.
pub(super) fn extract_response_from_output_pub(output_path: &str, start_offset: u64) -> String {
    let Ok(bytes) = std::fs::read(output_path) else {
        return String::new();
    };
    let start = usize::try_from(start_offset)
        .ok()
        .map(|offset| offset.min(bytes.len()))
        .unwrap_or(bytes.len());

    let mut response = String::new();
    let mut any_tool_used = false;
    let mut has_post_tool_text = false;
    let mut result_text = String::new();

    for line in String::from_utf8_lossy(&bytes[start..]).lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let msg_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match msg_type {
            "assistant" => {
                if let Some(content) = value.get("message").and_then(|m| m.get("content")) {
                    if let Some(arr) = content.as_array() {
                        let mut block_has_tool = false;
                        let mut block_has_text = false;
                        for block in arr {
                            match block.get("type").and_then(|t| t.as_str()) {
                                Some("text") => {
                                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                        if !text.is_empty() {
                                            response.push_str(text);
                                            block_has_text = true;
                                        }
                                    }
                                }
                                Some("tool_use") => {
                                    block_has_tool = true;
                                }
                                _ => {}
                            }
                        }
                        if block_has_tool {
                            any_tool_used = true;
                            // Reset: text in a block that also has tool_use is pre-tool narration
                            has_post_tool_text = false;
                        } else if block_has_text && any_tool_used {
                            has_post_tool_text = true;
                        }
                    }
                }
            }
            "result" => {
                let subtype = value.get("subtype").and_then(|v| v.as_str()).unwrap_or("");
                if subtype == "success" {
                    if let Some(r) = value.get("result").and_then(|v| v.as_str()) {
                        result_text = r.to_string();
                    }
                }
            }
            _ => {}
        }
    }

    // Apply resolve_done_response logic: if tool was used and no post-tool
    // assistant text followed, the accumulated response is stale narration —
    // prefer the authoritative result record.
    if !result_text.is_empty() {
        if response.trim().is_empty() {
            return result_text;
        }
        if any_tool_used && !has_post_tool_text {
            return result_text;
        }
    }
    response
}

fn output_has_bytes_after_offset(output_path: &str, start_offset: u64) -> bool {
    std::fs::metadata(output_path)
        .map(|meta| meta.len() > start_offset)
        .unwrap_or(false)
}

const TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD: std::time::Duration = std::time::Duration::from_secs(2);

fn terminal_success_watcher_stop_allowed(
    confirmed_end: u64,
    tmux_tail_offset: u64,
    quiet_for: std::time::Duration,
) -> bool {
    confirmed_end >= tmux_tail_offset && quiet_for >= TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD
}

async fn terminal_success_output_drained_for_recovery(
    output_path: &str,
    confirmed_end: u64,
    tmux_session_name: Option<&str>,
) -> bool {
    let Ok(before_meta) = std::fs::metadata(output_path) else {
        return false;
    };
    let tmux_alive = tmux_session_name
        .map(crate::services::platform::tmux::has_session)
        .unwrap_or(false);

    if !tmux_alive {
        return terminal_success_watcher_stop_allowed(
            confirmed_end,
            before_meta.len(),
            TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD,
        );
    }

    if !terminal_success_watcher_stop_allowed(
        confirmed_end,
        before_meta.len(),
        TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD,
    ) {
        return false;
    }

    tokio::time::sleep(TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD).await;

    let tail_after = std::fs::metadata(output_path)
        .map(|meta| meta.len())
        .unwrap_or(confirmed_end.saturating_add(1));
    tail_after == confirmed_end
        && terminal_success_watcher_stop_allowed(
            confirmed_end,
            tail_after,
            TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD,
        )
}

fn recovery_watcher_start_offset(output_path: &str, saved_last_offset: u64) -> (u64, u64, bool) {
    let current_len = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
    if current_len >= saved_last_offset {
        (saved_last_offset, current_len, false)
    } else {
        // The output file was recreated or truncated while dcserver was down.
        // Resume from the beginning of the new file so we do not skip the
        // entire restarted session output.
        (0, current_len, true)
    }
}

pub(super) async fn restore_inflight_turns(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) {
    let states = load_inflight_states(provider);
    if states.is_empty() {
        return;
    }

    let settings_snapshot = shared.settings.read().await.clone();

    for state in states {
        // #897 round-4 High: rebind_origin inflights are synthetic
        // placeholders owned by `/api/inflight/rebind` and do NOT carry
        // a real user message, dispatch context, or placeholder Discord
        // message. Restart recovery has nothing meaningful to do with
        // them — running `replace_long_message_raw(msg_id=0)`, writing
        // `discord:<channel>:0` analytics rows, or emitting reactions
        // against `MessageId::new(0)` would all produce bogus state
        // (flagged by #897 round-4 review). The operator is expected to
        // re-invoke `/api/inflight/rebind` after dcserver comes back up
        // if the orphan tmux is still alive. Clear the stale state and
        // skip further processing for this entry.
        if state.rebind_origin {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ recovery: skipping rebind-origin inflight for channel {} — operator must re-invoke /api/inflight/rebind post-restart",
                state.channel_id
            );
            clear_inflight_state(provider, state.channel_id);
            continue;
        }

        let channel_id = ChannelId::new(state.channel_id);

        // #2235: rows written by a newer binary may carry an unknown
        // `runtime_kind` variant; `deserialize_runtime_kind_tolerant` collapsed
        // that to `None`. When a row's persisted `version` is ahead of this
        // binary's `INFLIGHT_STATE_VERSION` AND its `runtime_kind` is missing,
        // the semantics of the row are owned by code we don't understand —
        // any recovery attempt would guess at runtime/handoff shape. Silent
        // skip (debug-level log + clear) is strictly better than guessing or
        // surfacing the legacy "input fifo path missing" notice.
        if state.runtime_kind.is_none()
            && state.version > super::inflight::inflight_state_version()
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::debug!(
                "  [{ts}] ↩ inflight recovery silent-skip for channel {}: row version {} > local {} with unknown runtime_kind",
                state.channel_id,
                state.version,
                super::inflight::inflight_state_version()
            );
            finish_recovered_turn_mailbox(
                shared,
                provider,
                channel_id,
                "recovery_runtime_kind_unknown_skip",
            )
            .await;
            clear_inflight_state(provider, state.channel_id);
            continue;
        }
        let is_dm = matches!(
            channel_id.to_channel(http).await,
            Ok(serenity::model::channel::Channel::Private(_))
        );
        let restart_report_exists =
            super::restart_report::load_restart_report(provider, state.channel_id).is_some();
        crate::services::observability::emit_recovery_fired(
            provider.as_str(),
            state.channel_id,
            state.dispatch_id.as_deref(),
            state.session_key.as_deref(),
            if restart_report_exists {
                "restart_report"
            } else {
                "restore_inflight"
            },
        );
        emit_recovery_quality_event(
            provider,
            state.channel_id,
            state.dispatch_id.as_deref(),
            state.session_key.as_deref(),
            if restart_report_exists {
                "restart_report"
            } else {
                "restore_inflight"
            },
        );

        // No generation gate — adopt mode allows old-gen session recovery.
        // If a restart report exists for this channel, check whether the agent
        // has already finished before deciding to skip recovery.  When the output
        // file contains a completed result we deliver it directly and clear both
        // the inflight state and the restart report, so the flush loop won't
        // overwrite the message with a generic follow-up.
        if restart_report_exists {
            let output_path_for_check: Option<String> = state
                .output_path
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    state
                        .channel_name
                        .as_ref()
                        .map(|name| tmux_runtime_paths(&provider.build_tmux_session_name(name)).0)
                });
            let restart_tmux_name = recovery_tmux_session_name(provider, &state);
            let completed_during_downtime_end = output_path_for_check
                .as_deref()
                .and_then(|path| success_result_end_offset_after_offset(path, state.last_offset));
            let completed_during_downtime = completed_during_downtime_end.is_some();
            let completed_during_downtime_drained = match (
                output_path_for_check.as_deref(),
                completed_during_downtime_end,
            ) {
                (Some(path), Some(confirmed_end)) => {
                    terminal_success_output_drained_for_recovery(
                        path,
                        confirmed_end,
                        restart_tmux_name.as_deref(),
                    )
                    .await
                }
                (None, Some(_)) => true,
                _ => false,
            };

            if completed_during_downtime && !completed_during_downtime_drained {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ recovery: terminal success observed for channel {} but tmux output has not stayed drained; reattaching watcher",
                    state.channel_id
                );
            }

            if completed_during_downtime_drained {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ✓ recovering completed turn for channel {} (restart report exists but output has result)",
                    state.channel_id
                );
                let (recovered_session_id, recovered_usage) = output_path_for_check
                    .as_deref()
                    .map(|path| {
                        extract_turn_analytics_from_output(
                            path,
                            state.turn_start_offset.unwrap_or(state.last_offset),
                        )
                    })
                    .unwrap_or((None, None));
                let extracted = output_path_for_check
                    .as_deref()
                    .map(|p| extract_response_from_output(p, state.last_offset))
                    .unwrap_or_default();
                let assistant_response = if extracted.trim().is_empty() {
                    state.full_response.clone()
                } else {
                    extracted
                };
                let final_text = if assistant_response.trim().is_empty() {
                    "(복구됨 — 응답 텍스트 없음)".to_string()
                } else {
                    super::formatting::format_for_discord_with_provider(
                        &assistant_response,
                        provider,
                    )
                };
                let channel_id = ChannelId::new(state.channel_id);
                let current_msg_id = MessageId::new(state.current_msg_id);
                let relay_ok = super::formatting::replace_long_message_raw(
                    http,
                    channel_id,
                    current_msg_id,
                    &final_text,
                    shared,
                )
                .await
                .is_ok();
                if !should_advance_recovery_dispatch_after_relay(relay_ok) {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ recovery: Discord relay failed before downtime dispatch completion — preserving inflight for retry"
                    );
                    continue;
                }
                // Mark user message as completed only after Discord terminal
                // delivery commits; otherwise the channel shows completion
                // without the final assistant message.
                let user_msg_id = MessageId::new(state.user_msg_id);
                complete_recovery_visible_turn(
                    http,
                    shared,
                    provider,
                    &state,
                    false,
                    "completed_during_downtime",
                )
                .await;
                // Complete the dispatch if this was a work dispatch turn — the
                // normal completion path was lost when dcserver restarted.
                // #142: implementation/rework need explicit completion. Review
                // and review-decision stay pending until their API handlers run.
                // Parse saved DISPATCH evidence first; reused threads may
                // already have a newer pending dispatch on the same thread.
                let recovered_dispatch_id = parse_dispatch_id(&state.user_text).or(
                    lookup_pending_dispatch_for_thread(shared.api_port, state.channel_id).await,
                );
                let role_binding = resolve_role_binding(channel_id, state.channel_name.as_deref());
                let duration_ms =
                    recovered_turn_duration_ms(Some(state.started_at.as_str())).unwrap_or(0);
                let has_completion_evidence =
                    if None::<&crate::db::Db>.is_some() || shared.pg_pool.is_some() {
                        super::turn_bridge::persist_turn_analytics_row_with_handles(
                            None::<&crate::db::Db>,
                            shared.pg_pool.as_ref(),
                            provider,
                            channel_id,
                            user_msg_id,
                            role_binding.as_ref(),
                            recovered_dispatch_id
                                .as_deref()
                                .or(state.dispatch_id.as_deref()),
                            state.session_key.as_deref(),
                            recovered_session_id
                                .as_deref()
                                .or(state.session_id.as_deref()),
                            &state,
                            recovered_usage.unwrap_or_default(),
                            duration_ms,
                        );
                        persist_recovered_transcript(
                            None::<&crate::db::Db>,
                            shared.pg_pool.as_ref(),
                            provider,
                            &state,
                            recovered_dispatch_id
                                .as_deref()
                                .or(state.dispatch_id.as_deref()),
                            &assistant_response,
                        )
                        .await
                    } else {
                        !assistant_response.trim().is_empty()
                    };
                let completion_context = has_completion_evidence
                    .then(|| serde_json::json!({ "agent_response_present": true }));
                let fallback_result = completion_context
                    .clone()
                    .map(|mut result| {
                        if let Some(obj) = result.as_object_mut() {
                            obj.insert(
                                "completion_source".to_string(),
                                serde_json::Value::String("recovery_db_fallback".to_string()),
                            );
                            obj.insert(
                                "needs_reconcile".to_string(),
                                serde_json::Value::Bool(true),
                            );
                        }
                        result
                    })
                    .unwrap_or_else(|| {
                        serde_json::json!({
                            "completion_source": "recovery_db_fallback",
                            "needs_reconcile": true,
                        })
                    });
                let mut dispatch_completed = recovered_dispatch_id.is_none();
                if let Some(ref did) = recovered_dispatch_id {
                    if !has_completion_evidence {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ recovery: refusing to complete work dispatch {did} without assistant response"
                        );
                    } else if let Some(engine) = &shared.engine {
                        // #143: Use finalize_dispatch directly with retry.
                        for attempt in 1..=3u8 {
                            match crate::dispatch::finalize_dispatch_with_backends(
                                None::<&crate::db::Db>,
                                engine,
                                did,
                                "recovery_completed_during_downtime",
                                completion_context.as_ref(),
                            ) {
                                Ok(_) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!(
                                        "  [{ts}] ✓ recovery: completed dispatch {did} via finalize_dispatch"
                                    );
                                    let _ =
                                        super::turn_bridge::queue_dispatch_followup_with_handles(
                                            None::<&crate::db::Db>,
                                            shared.pg_pool.as_ref(),
                                            did,
                                            "recovery_completed_during_downtime",
                                        )
                                        .await;
                                    dispatch_completed = true;
                                    break;
                                }
                                Err(e) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::warn!(
                                        "  [{ts}] ⚠ recovery: finalize_dispatch failed for {did} (attempt {attempt}/3): {e}"
                                    );
                                    if attempt < 3 {
                                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                                    }
                                }
                            }
                        }
                        // All retries exhausted — use the canonical runtime-root
                        // Postgres fallback instead of mutating legacy SQLite state.
                        if !dispatch_completed {
                            dispatch_completed =
                                super::turn_bridge::runtime_db_fallback_complete_with_result(
                                    did,
                                    &fallback_result,
                                );
                            if dispatch_completed {
                                let _ = super::turn_bridge::queue_dispatch_followup_with_handles(
                                    None::<&crate::db::Db>,
                                    shared.pg_pool.as_ref(),
                                    did,
                                    "recovery_completed_during_downtime_fallback",
                                )
                                .await;
                            }
                        }
                    } else {
                        // Db/Engine not available — fall back to direct dispatch update with retry
                        let payload = crate::server::routes::dispatches::UpdateDispatchBody {
                                status: Some("completed".to_string()),
                                result: Some(completion_context.clone().map(|mut result| {
                                    if let Some(obj) = result.as_object_mut() {
                                        obj.insert(
                                            "completion_source".to_string(),
                                            serde_json::Value::String(
                                                "recovery_completed_during_downtime".to_string(),
                                            ),
                                        );
                                    }
                                    result
                                }).unwrap_or_else(|| {
                                    serde_json::json!({
                                        "completion_source": "recovery_completed_during_downtime"
                                    })
                                })),
                                allowed_from: None,
                            };
                        use super::internal_api::DispatchUpdateOutcome;
                        let mut already_terminal = false;
                        for attempt in 1..=3u8 {
                            match super::internal_api::update_dispatch(did, payload.clone()).await {
                                Ok(DispatchUpdateOutcome::Updated(_)) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!("  [{ts}] ✓ recovery: completed dispatch {did}");
                                    dispatch_completed = true;
                                    break;
                                }
                                Ok(DispatchUpdateOutcome::Conflict { body }) => {
                                    // #2194 follow-up: dispatch is already in a
                                    // terminal status. Treat as success — do NOT
                                    // run DB fallback, which would overwrite the
                                    // existing result.
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!(
                                        dispatch_id = %did,
                                        response = %body,
                                        "  [{ts}] ✓ recovery: dispatch {did} already terminal (409); leaving prior result intact"
                                    );
                                    dispatch_completed = true;
                                    already_terminal = true;
                                    break;
                                }
                                Err(err) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::warn!(
                                        "  [{ts}] ⚠ recovery: dispatch {did} completion failed (attempt {attempt}/3): {err}"
                                    );
                                }
                            }
                            if attempt < 3 {
                                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                            }
                        }
                        // API retries exhausted — runtime-root DB fallback.
                        // Skip when the dispatch was already terminal (409) so we
                        // don't clobber its preserved result.
                        if !dispatch_completed && !already_terminal {
                            dispatch_completed =
                                super::turn_bridge::runtime_db_fallback_complete_with_result(
                                    did,
                                    &fallback_result,
                                );
                        }
                    }
                }
                // Only clear recovery bookkeeping if dispatch was completed (or no dispatch).
                // Preserving state on failure allows the next recovery pass to retry.
                if dispatch_completed {
                    super::restart_report::clear_restart_report(provider, state.channel_id);
                    finish_recovered_turn_mailbox(
                        shared,
                        provider,
                        channel_id,
                        "recovery_completed_during_downtime",
                    )
                    .await;
                    clear_inflight_state(provider, state.channel_id);
                } else if let Some(ref did) = recovered_dispatch_id {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ❌ recovery: dispatch {did} completion failed — preserving state for next recovery pass"
                    );
                }
                continue;
            }

            // Agent may still be running.  If the tmux session is alive, clear
            // the restart report and fall through to normal recovery (which
            // re-attaches a watcher to pick up the remaining output).
            // If the session is dead, delegate to the flush loop for fallback.
            let tmux_name = restart_tmux_name;
            let session_alive = tmux_name
                .as_deref()
                .map_or(false, tmux_session_alive_with_retry);
            // Derive channel_name from tmux session name if not in inflight state.
            // Validate before mutating restart-report state so other same-provider
            // bots do not log/clear reports for channels they do not own.
            let effective_channel_name = state.channel_name.clone().or_else(|| {
                tmux_name.as_deref().and_then(|name| {
                    crate::services::provider::parse_provider_and_channel_from_tmux_name(name)
                        .map(|(_, ch)| ch)
                })
            });
            let (allowlist_channel_id, provider_channel_name) =
                if let Some((pid, pname)) = super::resolve_thread_parent(http, channel_id).await {
                    (pid, pname.or(effective_channel_name.clone()))
                } else {
                    (channel_id, effective_channel_name.clone())
                };
            if let Err(reason) = validate_bot_channel_routing_with_provider_channel(
                &settings_snapshot,
                provider,
                allowlist_channel_id,
                effective_channel_name.as_deref(),
                provider_channel_name.as_deref(),
                is_dm,
            ) {
                if !reason.is_expected_cross_bot_skip() {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] ⏭ inflight recovery skip for channel {} — {reason}",
                        state.channel_id,
                    );
                }
                continue;
            }

            if session_alive {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ↻ restart report exists but tmux session alive for channel {}: clearing report, spawning watcher immediately",
                    state.channel_id
                );
                super::restart_report::clear_restart_report(provider, state.channel_id);
                // Register session in-memory so handlers can find it.
                // Derive channel_name from tmux session name if not in inflight state.
                let effective_channel_name = state.channel_name.clone().or_else(|| {
                    tmux_name.as_deref().and_then(|name| {
                        crate::services::provider::parse_provider_and_channel_from_tmux_name(name)
                            .map(|(_, ch)| ch)
                    })
                });
                // Resolve thread parent so validation uses the same semantics
                // as normal message routing (router.rs).
                let (allowlist_channel_id, provider_channel_name) = if let Some((pid, pname)) =
                    super::resolve_thread_parent(http, channel_id).await
                {
                    (pid, pname.or(effective_channel_name.clone()))
                } else {
                    (channel_id, effective_channel_name.clone())
                };
                if let Err(reason) = validate_bot_channel_routing_with_provider_channel(
                    &settings_snapshot,
                    provider,
                    allowlist_channel_id,
                    effective_channel_name.as_deref(),
                    provider_channel_name.as_deref(),
                    is_dm,
                ) {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ⏭ inflight recovery skip for channel {} — {reason}",
                        state.channel_id,
                    );
                    continue;
                }
                {
                    let mut data = shared.core.lock().await;
                    let session =
                        data.sessions
                            .entry(channel_id)
                            .or_insert_with(|| DiscordSession {
                                session_id: state.session_id.clone(),
                                memento_context_loaded: false,
                                memento_reflected: false,
                                current_path: None,
                                history: Vec::new(),
                                pending_uploads: Vec::new(),
                                cleared: false,
                                remote_profile_name: None,
                                channel_id: Some(state.channel_id),
                                channel_name: effective_channel_name.clone(),
                                category_name: None,
                                last_active: tokio::time::Instant::now(),
                                worktree: None,
                                born_generation: super::runtime_store::load_generation(),
                                assistant_turns: 0,
                            });
                    session.channel_id = Some(state.channel_id);
                    session.last_active = tokio::time::Instant::now();
                    if session.channel_name.is_none() {
                        session.channel_name = effective_channel_name;
                    }
                    restore_recovered_session_worktree(session, &state);
                }

                let finish_mailbox_on_completion =
                    reregister_active_turn_from_inflight(shared, &state).await;

                // Immediately spawn a tmux watcher instead of deferring to
                // restore_tmux_watchers().  The previous "watcher will adopt"
                // approach had a race condition: the tmux session could die
                // between recovery (now) and restore_tmux_watchers (~50s later),
                // losing the in-progress response entirely.
                if let Some(ref tmux_session_name) = tmux_name {
                    let output_path =
                        crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
                    if std::fs::metadata(&output_path).is_ok() {
                        let (initial_offset, current_len, truncated) =
                            recovery_watcher_start_offset(&output_path, state.last_offset);
                        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let resume_offset = std::sync::Arc::new(std::sync::Mutex::new(None::<u64>));
                        let pause_epoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                        let turn_delivered =
                            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let last_heartbeat_ts_ms = std::sync::Arc::new(
                            std::sync::atomic::AtomicI64::new(super::tmux_watcher_now_ms()),
                        );
                        let mailbox_finalize_owed =
                            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let handle = TmuxWatcherHandle {
                            tmux_session_name: tmux_session_name.clone(),
                            paused: paused.clone(),
                            resume_offset: resume_offset.clone(),
                            cancel: cancel.clone(),
                            pause_epoch: pause_epoch.clone(),
                            turn_delivered: turn_delivered.clone(),
                            last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
                            mailbox_finalize_owed: mailbox_finalize_owed.clone(),
                        };
                        let watcher_claimed = {
                            #[cfg(unix)]
                            {
                                let claim = super::tmux::claim_or_reuse_watcher(
                                    &shared.tmux_watchers,
                                    channel_id,
                                    handle,
                                    provider,
                                    "restart_report_recovery",
                                );
                                claim.should_spawn()
                            }
                            #[cfg(not(unix))]
                            {
                                let _ = handle;
                                false
                            }
                        };
                        if watcher_claimed {
                            let ts2 = chrono::Local::now().format("%H:%M:%S");
                            if truncated {
                                tracing::info!(
                                    "  [{ts2}] ↻ recovery: output truncated for #{} (saved offset {}, file len {}), restarting watcher from 0",
                                    tmux_session_name,
                                    state.last_offset,
                                    current_len
                                );
                            }
                            tracing::info!(
                                "  [{ts2}] 👁 recovery: spawned watcher for #{} at offset {}",
                                tmux_session_name,
                                initial_offset
                            );
                            #[cfg(unix)]
                            {
                                let restored_turn =
                                    super::tmux::restored_watcher_turn_from_inflight(
                                        &state,
                                        tmux_session_name,
                                        finish_mailbox_on_completion,
                                    );
                                shared.record_tmux_watcher_reconnect(channel_id);
                                tokio::spawn(super::tmux::tmux_output_watcher_with_restore(
                                    channel_id,
                                    http.clone(),
                                    shared.clone(),
                                    output_path,
                                    tmux_session_name.clone(),
                                    initial_offset,
                                    cancel,
                                    paused,
                                    resume_offset,
                                    pause_epoch,
                                    turn_delivered,
                                    last_heartbeat_ts_ms,
                                    mailbox_finalize_owed,
                                    restored_turn,
                                ));
                            }
                        }
                    }
                }

                // Keep the inflight state until the watcher either relays the
                // final response or triggers watcher-death handoff. Clearing it
                // here breaks the handoff path if the recovered tmux session
                // dies before producing a result.
                continue;
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                if let Some(diag) = tmux_name.as_deref().and_then(|name| {
                    build_tmux_death_diagnostic(name, output_path_for_check.as_deref())
                }) {
                    tracing::info!(
                        "  [{ts}] ↻ restart report exists but tmux session is dead for channel {}: clearing report, continuing with direct fallback recovery ({diag})",
                        state.channel_id
                    );
                } else {
                    tracing::info!(
                        "  [{ts}] ↻ restart report exists but tmux session is dead for channel {}: clearing report, continuing with direct fallback recovery",
                        state.channel_id
                    );
                }
                super::restart_report::clear_restart_report(provider, state.channel_id);
            }
        }

        let current_msg_id = MessageId::new(state.current_msg_id);
        let user_msg_id = MessageId::new(state.user_msg_id);
        let channel_name = state.channel_name.clone();
        let tmux_session_name = state.tmux_session_name.clone().or_else(|| {
            channel_name
                .as_ref()
                .map(|name| provider.build_tmux_session_name(name))
        });
        let channel_name = channel_name.or_else(|| {
            tmux_session_name.as_deref().and_then(|name| {
                crate::services::provider::parse_provider_and_channel_from_tmux_name(name)
                    .map(|(_, ch)| ch)
            })
        });
        // Resolve thread parent so validation uses the same semantics
        // as normal message routing (router.rs).
        let (allowlist_channel_id, provider_channel_name) =
            if let Some((pid, pname)) = super::resolve_thread_parent(http, channel_id).await {
                (pid, pname.or(channel_name.clone()))
            } else {
                (channel_id, channel_name.clone())
            };
        if let Err(reason) = validate_bot_channel_routing_with_provider_channel(
            &settings_snapshot,
            provider,
            allowlist_channel_id,
            channel_name.as_deref(),
            provider_channel_name.as_deref(),
            is_dm,
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ inflight recovery skip for channel {} — {reason}",
                state.channel_id,
            );
            continue;
        }
        let (fallback_output, fallback_input) = tmux_session_name
            .as_deref()
            .map(tmux_runtime_paths)
            .unwrap_or_else(|| (String::new(), String::new()));
        let runtime_kind = state.runtime_kind_for_recovery();
        let output_path = state
            .output_path
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                if !fallback_output.is_empty() {
                    Some(fallback_output.clone())
                } else {
                    None
                }
            });
        let input_fifo_path = if runtime_kind.requires_input_fifo() {
            state
                .input_fifo_path
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    if !fallback_input.is_empty() {
                        Some(fallback_input.clone())
                    } else {
                        None
                    }
                })
        } else {
            state.input_fifo_path.clone().filter(|s| !s.is_empty())
        };
        // Check exit reason file for post-mortem diagnostics
        if let Some(ref op) = output_path {
            let exit_reason_path = format!("{}.exit_reason", op);
            if let Ok(reason) = std::fs::read_to_string(&exit_reason_path) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔍 exit_reason for channel {}: {}",
                    state.channel_id,
                    reason.trim()
                );
                // Clean up exit reason file after reading
                let _ = std::fs::remove_file(&exit_reason_path);
            }
        }

        let terminal_success_end = output_path
            .as_deref()
            .and_then(|path| success_result_end_offset_after_offset(path, state.last_offset));
        let output_already_completed = terminal_success_end.is_some();
        let terminal_success_drained = match (output_path.as_deref(), terminal_success_end) {
            (Some(path), Some(confirmed_end)) => {
                terminal_success_output_drained_for_recovery(
                    path,
                    confirmed_end,
                    tmux_session_name.as_deref(),
                )
                .await
            }
            (None, Some(_)) => true,
            _ => false,
        };
        if output_already_completed && !terminal_success_drained {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ recovery: terminal success observed for channel {} but tmux output has not stayed drained; reattaching watcher",
                state.channel_id
            );
        }
        let output_has_new_bytes = output_path
            .as_deref()
            .map(|path| output_has_bytes_after_offset(path, state.last_offset))
            .unwrap_or(false);

        if can_fast_path_captured_full_response(&state, terminal_success_drained) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ recovery fast-path: delivering captured full_response for channel {}",
                state.channel_id,
            );

            let assistant_response = state.full_response.clone();
            let final_text =
                super::formatting::format_for_discord_with_provider(&assistant_response, provider);
            let relay_ok = super::formatting::replace_long_message_raw(
                http,
                channel_id,
                current_msg_id,
                &final_text,
                shared,
            )
            .await
            .is_ok();

            if !should_advance_recovery_dispatch_after_relay(relay_ok) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ recovery: Discord relay failed before dispatch completion — preserving inflight for retry"
                );
                continue;
            }
            complete_recovery_visible_turn(
                http,
                shared,
                provider,
                &state,
                false,
                "captured_full_response",
            )
            .await;

            let recovered_dispatch_id = parse_dispatch_id(&state.user_text)
                .or(lookup_pending_dispatch_for_thread(shared.api_port, state.channel_id).await);
            let role_binding = resolve_role_binding(channel_id, state.channel_name.as_deref());
            let duration_ms =
                recovered_turn_duration_ms(Some(state.started_at.as_str())).unwrap_or(0);
            let has_completion_evidence =
                if None::<&crate::db::Db>.is_some() || shared.pg_pool.is_some() {
                    super::turn_bridge::persist_turn_analytics_row_with_handles(
                        None::<&crate::db::Db>,
                        shared.pg_pool.as_ref(),
                        provider,
                        channel_id,
                        user_msg_id,
                        role_binding.as_ref(),
                        recovered_dispatch_id
                            .as_deref()
                            .or(state.dispatch_id.as_deref()),
                        state.session_key.as_deref(),
                        state.session_id.as_deref(),
                        &state,
                        TurnTokenUsage::default(),
                        duration_ms,
                    );
                    persist_recovered_transcript(
                        None::<&crate::db::Db>,
                        shared.pg_pool.as_ref(),
                        provider,
                        &state,
                        recovered_dispatch_id
                            .as_deref()
                            .or(state.dispatch_id.as_deref()),
                        &assistant_response,
                    )
                    .await
                } else {
                    !assistant_response.trim().is_empty()
                };
            let completion_context = has_completion_evidence
                .then(|| serde_json::json!({ "agent_response_present": true }));
            let fallback_result = completion_context
                .clone()
                .map(|mut result| {
                    if let Some(obj) = result.as_object_mut() {
                        obj.insert(
                            "completion_source".to_string(),
                            serde_json::Value::String(
                                "recovery_captured_full_response_db_fallback".to_string(),
                            ),
                        );
                        obj.insert("needs_reconcile".to_string(), serde_json::Value::Bool(true));
                    }
                    result
                })
                .unwrap_or_else(|| {
                    serde_json::json!({
                        "completion_source": "recovery_captured_full_response_db_fallback",
                        "needs_reconcile": true,
                    })
                });
            let mut dispatch_completed = recovered_dispatch_id.is_none();
            if let Some(ref did) = recovered_dispatch_id {
                let dispatch_type = super::internal_api::lookup_dispatch_type(did)
                    .await
                    .ok()
                    .flatten();

                match dispatch_type.as_deref() {
                    Some("implementation") | Some("rework") => {
                        if !has_completion_evidence {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ recovery: refusing to complete work dispatch {did} without assistant response"
                            );
                        } else if let Some(engine) = &shared.engine {
                            for attempt in 1..=3u8 {
                                match crate::dispatch::finalize_dispatch_with_backends(
                                    None::<&crate::db::Db>,
                                    engine,
                                    did,
                                    "recovery_captured_full_response",
                                    completion_context.as_ref(),
                                ) {
                                    Ok(_) => {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        tracing::info!(
                                            "  [{ts}] ✓ recovery: completed dispatch {did} via finalize_dispatch"
                                        );
                                        let _ =
                                            super::turn_bridge::queue_dispatch_followup_with_handles(
                                                None::<&crate::db::Db>,
                                                shared.pg_pool.as_ref(),
                                                did,
                                                "recovery_captured_full_response",
                                            )
                                            .await;
                                        dispatch_completed = true;
                                        break;
                                    }
                                    Err(e) => {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        tracing::warn!(
                                            "  [{ts}] ⚠ recovery: finalize_dispatch failed for {did} (attempt {attempt}/3): {e}"
                                        );
                                        if attempt < 3 {
                                            tokio::time::sleep(std::time::Duration::from_secs(1))
                                                .await;
                                        }
                                    }
                                }
                            }
                            if !dispatch_completed {
                                dispatch_completed =
                                    super::turn_bridge::runtime_db_fallback_complete_with_result(
                                        did,
                                        &fallback_result,
                                    );
                                if dispatch_completed {
                                    let _ =
                                        super::turn_bridge::queue_dispatch_followup_with_handles(
                                            None::<&crate::db::Db>,
                                            shared.pg_pool.as_ref(),
                                            did,
                                            "recovery_captured_full_response_fallback",
                                        )
                                        .await;
                                }
                            }
                        } else {
                            dispatch_completed =
                                super::turn_bridge::runtime_db_fallback_complete_with_result(
                                    did,
                                    &fallback_result,
                                );
                            if dispatch_completed {
                                let _ = super::turn_bridge::queue_dispatch_followup_with_handles(
                                    None::<&crate::db::Db>,
                                    shared.pg_pool.as_ref(),
                                    did,
                                    "recovery_captured_full_response_runtime_fallback",
                                )
                                .await;
                            }
                        }
                        if !dispatch_completed {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ❌ recovery: dispatch {did} completion failed — preserving state for retry"
                            );
                        }
                    }
                    Some(_) => {
                        dispatch_completed = true;
                    }
                    None => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ recovery: cannot determine dispatch type for {did} — preserving state"
                        );
                    }
                }
            }

            if dispatch_completed {
                finish_recovered_turn_mailbox(
                    shared,
                    provider,
                    channel_id,
                    "recovery_captured_full_response",
                )
                .await;
                clear_inflight_state(provider, state.channel_id);
            }
            continue;
        }
        if matches!(
            recovery_phase_after_output_scan(terminal_success_drained, false),
            RecoveryPhase::Done
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ✓ recovering completed turn for channel {}: output contains result after offset {}",
                state.channel_id,
                state.last_offset
            );
            let (recovered_session_id, recovered_usage) = output_path
                .as_deref()
                .map(|path| {
                    extract_turn_analytics_from_output(
                        path,
                        state.turn_start_offset.unwrap_or(state.last_offset),
                    )
                })
                .unwrap_or((None, None));
            // Deliver the result to Discord before clearing the inflight state
            let extracted = output_path
                .as_deref()
                .map(|p| extract_response_from_output(p, state.last_offset))
                .unwrap_or_default();
            let assistant_response = if extracted.trim().is_empty() {
                state.full_response.clone()
            } else {
                extracted
            };
            let final_text = if assistant_response.trim().is_empty() {
                "(복구됨 — 응답 텍스트 없음)".to_string()
            } else {
                super::formatting::format_for_discord_with_provider(&assistant_response, provider)
            };
            // #225 P1-1: Track relay success — only clear inflight if Discord delivery succeeds
            let relay_ok = super::formatting::replace_long_message_raw(
                http,
                channel_id,
                current_msg_id,
                &final_text,
                shared,
            )
            .await
            .is_ok();

            let user_msg_id = MessageId::new(state.user_msg_id);
            if !should_advance_recovery_dispatch_after_relay(relay_ok) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ recovery: Discord relay failed before dispatch completion — preserving inflight for retry"
                );
                continue;
            }
            // Mark user message as completed only after Discord terminal delivery commits.
            complete_recovery_visible_turn(
                http,
                shared,
                provider,
                &state,
                false,
                "output_completed",
            )
            .await;

            // Complete the dispatch if this was an implementation/rework turn.
            // Review dispatches require the verdict flow (review_verdict.rs)
            // and must not be generically finalized here.
            // #225 P1-3: Use DB lookup for dispatch ID (text parsing fails in unified threads)
            let recovered_dispatch_id = parse_dispatch_id(&state.user_text)
                .or(lookup_pending_dispatch_for_thread(shared.api_port, state.channel_id).await);
            let role_binding = resolve_role_binding(channel_id, state.channel_name.as_deref());
            let duration_ms =
                recovered_turn_duration_ms(Some(state.started_at.as_str())).unwrap_or(0);
            let has_completion_evidence =
                if None::<&crate::db::Db>.is_some() || shared.pg_pool.is_some() {
                    super::turn_bridge::persist_turn_analytics_row_with_handles(
                        None::<&crate::db::Db>,
                        shared.pg_pool.as_ref(),
                        provider,
                        channel_id,
                        user_msg_id,
                        role_binding.as_ref(),
                        recovered_dispatch_id
                            .as_deref()
                            .or(state.dispatch_id.as_deref()),
                        state.session_key.as_deref(),
                        recovered_session_id
                            .as_deref()
                            .or(state.session_id.as_deref()),
                        &state,
                        recovered_usage.unwrap_or_default(),
                        duration_ms,
                    );
                    persist_recovered_transcript(
                        None::<&crate::db::Db>,
                        shared.pg_pool.as_ref(),
                        provider,
                        &state,
                        recovered_dispatch_id
                            .as_deref()
                            .or(state.dispatch_id.as_deref()),
                        &assistant_response,
                    )
                    .await
                } else {
                    !assistant_response.trim().is_empty()
                };
            let completion_context = has_completion_evidence
                .then(|| serde_json::json!({ "agent_response_present": true }));
            let fallback_result = completion_context
                .clone()
                .map(|mut result| {
                    if let Some(obj) = result.as_object_mut() {
                        obj.insert(
                            "completion_source".to_string(),
                            serde_json::Value::String("recovery_output_db_fallback".to_string()),
                        );
                        obj.insert("needs_reconcile".to_string(), serde_json::Value::Bool(true));
                    }
                    result
                })
                .unwrap_or_else(|| {
                    serde_json::json!({
                        "completion_source": "recovery_output_db_fallback",
                        "needs_reconcile": true,
                    })
                });
            let mut dispatch_completed = recovered_dispatch_id.is_none();
            if let Some(ref did) = recovered_dispatch_id {
                let dispatch_type = super::internal_api::lookup_dispatch_type(did)
                    .await
                    .ok()
                    .flatten();

                match dispatch_type.as_deref() {
                    Some("implementation") | Some("rework") => {
                        if !has_completion_evidence {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ recovery: refusing to complete work dispatch {did} without assistant response"
                            );
                        } else if let Some(engine) = &shared.engine {
                            for attempt in 1..=3u8 {
                                match crate::dispatch::finalize_dispatch_with_backends(
                                    None::<&crate::db::Db>,
                                    engine,
                                    did,
                                    "recovery_output_completed",
                                    completion_context.as_ref(),
                                ) {
                                    Ok(_) => {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        tracing::info!(
                                            "  [{ts}] ✓ recovery: completed dispatch {did} via finalize_dispatch"
                                        );
                                        let _ =
                                            super::turn_bridge::queue_dispatch_followup_with_handles(
                                                None::<&crate::db::Db>,
                                                shared.pg_pool.as_ref(),
                                                did,
                                                "recovery_output_completed",
                                            )
                                            .await;
                                        dispatch_completed = true;
                                        break;
                                    }
                                    Err(e) => {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        tracing::warn!(
                                            "  [{ts}] ⚠ recovery: finalize_dispatch failed for {did} (attempt {attempt}/3): {e}"
                                        );
                                        if attempt < 3 {
                                            tokio::time::sleep(std::time::Duration::from_secs(1))
                                                .await;
                                        }
                                    }
                                }
                            }
                            if !dispatch_completed {
                                dispatch_completed =
                                    super::turn_bridge::runtime_db_fallback_complete_with_result(
                                        did,
                                        &fallback_result,
                                    );
                                if dispatch_completed {
                                    let _ =
                                        super::turn_bridge::queue_dispatch_followup_with_handles(
                                            None::<&crate::db::Db>,
                                            shared.pg_pool.as_ref(),
                                            did,
                                            "recovery_output_completed_fallback",
                                        )
                                        .await;
                                }
                            }
                        } else {
                            dispatch_completed =
                                super::turn_bridge::runtime_db_fallback_complete_with_result(
                                    did,
                                    &fallback_result,
                                );
                            if dispatch_completed {
                                let _ = super::turn_bridge::queue_dispatch_followup_with_handles(
                                    None::<&crate::db::Db>,
                                    shared.pg_pool.as_ref(),
                                    did,
                                    "recovery_output_completed_runtime_fallback",
                                )
                                .await;
                            }
                        }
                        if !dispatch_completed {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ❌ recovery: dispatch {did} completion failed — preserving state for retry"
                            );
                        }
                    }
                    Some(_) => {
                        // Non-work dispatches (review, review-decision) need
                        // their own explicit API completion flow. Clear inflight
                        // but leave dispatch status untouched.
                        dispatch_completed = true;
                    }
                    None => {
                        // DB unavailable — cannot determine dispatch type.
                        // Preserve inflight state so the next recovery pass can retry.
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ recovery: cannot determine dispatch type for {did} — preserving state"
                        );
                    }
                }
            }

            // #225 P1-1: Only clear inflight if both dispatch completed AND relay succeeded.
            // If relay failed, preserve inflight for retry on next startup.
            if dispatch_completed {
                finish_recovered_turn_mailbox(
                    shared,
                    provider,
                    channel_id,
                    "recovery_output_completed",
                )
                .await;
                clear_inflight_state(provider, state.channel_id);
            }
            continue;
        }

        let tmux_ready_without_new_output = tmux_session_name.as_deref().map_or(false, |name| {
            !output_has_new_bytes
                && recovery_has_post_work_ready_evidence(&state)
                && crate::services::provider::tmux_session_ready_for_input(name)
        });

        if matches!(
            recovery_phase_after_output_scan(false, tmux_ready_without_new_output),
            RecoveryPhase::Done
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ✓ clearing inflight turn for channel {}: tmux is ready for input and output is idle after offset {}",
                state.channel_id,
                state.last_offset
            );
            let final_text = if state.full_response.trim().is_empty() {
                stale_inflight_message("")
            } else {
                super::formatting::format_for_discord_with_provider(&state.full_response, provider)
            };
            if relay_recovery_terminal_notice(http, shared, &state, &final_text).await {
                finish_recovered_turn_mailbox(
                    shared,
                    provider,
                    channel_id,
                    "recovery_ready_without_output",
                )
                .await;
                clear_inflight_state(provider, state.channel_id);
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ recovery: ready-without-output notice failed — preserving inflight for retry"
                );
            }
            continue;
        }

        let can_recover = tmux_session_name
            .as_deref()
            .map_or(false, |name| tmux_has_session_with_retry(name));

        if matches!(
            recovery_phase_after_tmux_probe(can_recover, None),
            RecoveryPhase::Done
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            // Even without a live tmux session, the output file may contain
            // response data. Try extracting from the full file first, then
            // fall back to saved partial response.
            let extracted_full = output_path
                .as_deref()
                .map(|p| extract_response_from_output(p, 0))
                .unwrap_or_default();
            let best_response = if !extracted_full.trim().is_empty() {
                extracted_full
            } else {
                state.full_response.clone()
            };
            let stale_text = interrupted_recovery_message(&state, &best_response);
            let death_diag = tmux_session_name
                .as_deref()
                .and_then(|name| build_tmux_death_diagnostic(name, output_path.as_deref()));
            if let Some(ref diag) = death_diag {
                tracing::info!(
                    "  [{ts}] ⚠ cannot recover inflight turn for channel {}: tmux session missing (response len: {}, {diag})",
                    state.channel_id,
                    best_response.len()
                );
            } else {
                tracing::info!(
                    "  [{ts}] ⚠ cannot recover inflight turn for channel {}: tmux session missing (response len: {})",
                    state.channel_id,
                    best_response.len()
                );
            }
            let relay_ok = relay_recovery_terminal_notice(http, shared, &state, &stale_text).await;
            if let Some(ref sk) = state.session_key {
                crate::services::termination_audit::record_termination_with_handles(
                    None::<&crate::db::Db>,
                    shared.pg_pool.as_ref(),
                    sk,
                    state.dispatch_id.as_deref(),
                    "recovery",
                    "restart_session_missing",
                    Some("tmux session missing after restart"),
                    death_diag.as_deref(),
                    Some(state.last_offset),
                    Some(false),
                );
            }
            save_missing_session_handoff(provider, &state, &best_response);
            if relay_ok {
                finish_recovered_turn_mailbox(
                    shared,
                    provider,
                    channel_id,
                    "recovery_missing_tmux",
                )
                .await;
                clear_inflight_state(provider, state.channel_id);
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ recovery: missing-tmux notice failed — preserving inflight for retry"
                );
            }
            continue;
        }

        let Some(tmux_session_name) = tmux_session_name else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ clearing inflight turn for channel {}: tmux session name missing",
                state.channel_id
            );
            let text = stale_inflight_message("tmux session name missing during recovery");
            if relay_recovery_terminal_notice(http, shared, &state, &text).await {
                finish_recovered_turn_mailbox(
                    shared,
                    provider,
                    channel_id,
                    "recovery_missing_tmux_name",
                )
                .await;
                clear_inflight_state(provider, state.channel_id);
            } else {
                tracing::warn!(
                    "  [{ts}] ⚠ recovery: missing-tmux-name notice failed — preserving inflight for retry"
                );
            }
            continue;
        };
        let Some(output_path) = output_path else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ clearing inflight turn for channel {}: output path missing",
                state.channel_id
            );
            let text = stale_inflight_message("output path missing during recovery");
            if relay_recovery_terminal_notice(http, shared, &state, &text).await {
                finish_recovered_turn_mailbox(
                    shared,
                    provider,
                    channel_id,
                    "recovery_missing_output_path",
                )
                .await;
                clear_inflight_state(provider, state.channel_id);
            } else {
                tracing::warn!(
                    "  [{ts}] ⚠ recovery: missing-output-path notice failed — preserving inflight for retry"
                );
            }
            continue;
        };
        let input_fifo_path = match recovery_input_fifo_for_runtime(runtime_kind, input_fifo_path) {
            Ok(path) => path,
            Err(reason) => {
                // #2235: when the inflight row was written without a stamped
                // `runtime_kind` (legacy pre-v8 row, hook-endpoint race, or a
                // future variant this binary doesn't recognize),
                // `runtime_kind_for_recovery` had to guess. If the guess
                // requires a FIFO that the row never carried, surfacing a
                // user-visible "input fifo path missing" notice misleads the
                // operator — the right thing is to skip recovery silently and
                // let the next turn re-establish state from scratch.
                let runtime_kind_was_inferred = state.runtime_kind.is_none();
                let ts = chrono::Local::now().format("%H:%M:%S");
                if runtime_kind_was_inferred {
                    tracing::debug!(
                        "  [{ts}] ↩ inflight recovery silent-skip for channel {}: runtime_kind unknown/missing on-disk, inferred {} requires FIFO but row carries none",
                        state.channel_id,
                        runtime_kind.as_str()
                    );
                    finish_recovered_turn_mailbox(
                        shared,
                        provider,
                        channel_id,
                        "recovery_runtime_kind_missing_skip",
                    )
                    .await;
                    clear_inflight_state(provider, state.channel_id);
                    continue;
                }
                tracing::info!(
                    "  [{ts}] ⚠ clearing inflight turn for channel {}: input fifo path missing (runtime={})",
                    state.channel_id,
                    runtime_kind.as_str()
                );
                let text = stale_inflight_message(reason);
                if relay_recovery_terminal_notice(http, shared, &state, &text).await {
                    finish_recovered_turn_mailbox(
                        shared,
                        provider,
                        channel_id,
                        "recovery_missing_input_fifo",
                    )
                    .await;
                    clear_inflight_state(provider, state.channel_id);
                } else {
                    tracing::warn!(
                        "  [{ts}] ⚠ recovery: missing-input-fifo notice failed — preserving inflight for retry"
                    );
                }
                continue;
            }
        };

        // If tmux pane is alive, skip recovery reader entirely.
        // The session is idle (waiting for input) — spawn a watcher immediately
        // instead of deferring to restore_tmux_watchers() to avoid a race
        // condition where the session could die in the gap between recovery and
        // restore_tmux_watchers (~50s), losing the response.
        let pane_alive = tmux_session_alive_with_retry(&tmux_session_name);
        if matches!(
            recovery_phase_after_tmux_probe(true, Some(pane_alive)),
            RecoveryPhase::WatcherReattach
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ inflight recovery: pane alive for channel {}, spawning watcher immediately",
                state.channel_id
            );
            // Register session in-memory so handlers can find it.
            let effective_channel_name = channel_name.clone().or_else(|| {
                crate::services::provider::parse_provider_and_channel_from_tmux_name(
                    &tmux_session_name,
                )
                .map(|(_, ch)| ch)
            });
            {
                let sqlite_settings_db = if shared.pg_pool.is_some() {
                    None
                } else {
                    None::<&crate::db::Db>
                };
                let persisted_session_path = load_last_session_path(
                    sqlite_settings_db,
                    shared.pg_pool.as_ref(),
                    &shared.token_hash,
                    channel_id.get(),
                );
                let recovery_adk_cwd = match recovery_spawn_adk_cwd(&state, persisted_session_path)
                {
                    Ok(path) => path,
                    Err(error) => {
                        let dispatch_id = state
                            .dispatch_id
                            .clone()
                            .or_else(|| parse_dispatch_id(&state.user_text));
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::error!("  [{ts}] {error}; main-workspace fallback blocked");
                        crate::services::observability::emit_recovery_fired(
                            provider.as_str(),
                            state.channel_id,
                            dispatch_id.as_deref(),
                            state.session_key.as_deref(),
                            "worktree_missing_main_fallback_blocked",
                        );
                        emit_recovery_quality_event(
                            provider,
                            state.channel_id,
                            dispatch_id.as_deref(),
                            state.session_key.as_deref(),
                            "worktree_missing_main_fallback_blocked",
                        );
                        let relay_ok = super::formatting::replace_long_message_raw(
                            http,
                            channel_id,
                            current_msg_id,
                            &format!("❌ {error}\nmain workspace fallback blocked."),
                            shared,
                        )
                        .await
                        .is_ok();
                        if should_advance_recovery_dispatch_after_relay(relay_ok) {
                            super::turn_bridge::fail_dispatch_with_retry(
                                shared.api_port,
                                dispatch_id.as_deref(),
                                &error,
                            )
                            .await;
                            super::restart_report::clear_restart_report(provider, state.channel_id);
                            clear_inflight_state(provider, state.channel_id);
                        } else {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ recovery: worktree error relay failed before dispatch failure — preserving inflight for retry"
                            );
                        }
                        continue;
                    }
                };
                let saved_remote = load_last_remote_profile(
                    sqlite_settings_db,
                    shared.pg_pool.as_ref(),
                    &shared.token_hash,
                    channel_id.get(),
                );
                let mut data = shared.core.lock().await;
                let session = data
                    .sessions
                    .entry(channel_id)
                    .or_insert_with(|| DiscordSession {
                        session_id: state.session_id.clone(),
                        memento_context_loaded: false,
                        memento_reflected: false,
                        current_path: None,
                        history: Vec::new(),
                        pending_uploads: Vec::new(),
                        cleared: false,
                        remote_profile_name: saved_remote,
                        channel_id: Some(channel_id.get()),
                        channel_name: effective_channel_name.clone(),
                        category_name: None,
                        last_active: tokio::time::Instant::now(),
                        worktree: None,
                        born_generation: super::runtime_store::load_generation(),
                        assistant_turns: 0,
                    });
                session.channel_id = Some(channel_id.get());
                session.last_active = tokio::time::Instant::now();
                if session.current_path.is_none() {
                    session.current_path = recovery_adk_cwd;
                }
                if session.channel_name.is_none() {
                    session.channel_name = effective_channel_name;
                }
                restore_recovered_session_worktree(session, &state);
            }

            let finish_mailbox_on_completion =
                reregister_active_turn_from_inflight(shared, &state).await;

            // Immediately spawn watcher to avoid race condition.
            if std::fs::metadata(&output_path).is_ok() {
                let (initial_offset, current_len, truncated) =
                    recovery_watcher_start_offset(&output_path, state.last_offset);
                let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let resume_offset = std::sync::Arc::new(std::sync::Mutex::new(None::<u64>));
                let pause_epoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                let turn_delivered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let last_heartbeat_ts_ms = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(
                    super::tmux_watcher_now_ms(),
                ));
                let mailbox_finalize_owed =
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let handle = TmuxWatcherHandle {
                    tmux_session_name: tmux_session_name.clone(),
                    paused: paused.clone(),
                    resume_offset: resume_offset.clone(),
                    cancel: cancel.clone(),
                    pause_epoch: pause_epoch.clone(),
                    turn_delivered: turn_delivered.clone(),
                    last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
                    mailbox_finalize_owed: mailbox_finalize_owed.clone(),
                };
                let watcher_claimed = {
                    #[cfg(unix)]
                    {
                        let claim = super::tmux::claim_or_reuse_watcher(
                            &shared.tmux_watchers,
                            channel_id,
                            handle,
                            provider,
                            "inflight_recovery",
                        );
                        claim.should_spawn()
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = handle;
                        false
                    }
                };
                if watcher_claimed {
                    let ts2 = chrono::Local::now().format("%H:%M:%S");
                    if truncated {
                        tracing::info!(
                            "  [{ts2}] ↻ recovery: output truncated for #{} (saved offset {}, file len {}), restarting watcher from 0",
                            tmux_session_name,
                            state.last_offset,
                            current_len
                        );
                    }
                    tracing::info!(
                        "  [{ts2}] 👁 recovery: spawned watcher for #{} at offset {}",
                        tmux_session_name,
                        initial_offset
                    );
                    #[cfg(unix)]
                    {
                        let restored_turn = super::tmux::restored_watcher_turn_from_inflight(
                            &state,
                            &tmux_session_name,
                            finish_mailbox_on_completion,
                        );
                        shared.record_tmux_watcher_reconnect(channel_id);
                        tokio::spawn(super::tmux::tmux_output_watcher_with_restore(
                            channel_id,
                            http.clone(),
                            shared.clone(),
                            output_path.clone(),
                            tmux_session_name.clone(),
                            initial_offset,
                            cancel,
                            paused,
                            resume_offset,
                            pause_epoch,
                            turn_delivered,
                            last_heartbeat_ts_ms,
                            mailbox_finalize_owed,
                            restored_turn,
                        ));
                    }
                }
            }

            // Keep the inflight state until the watcher either relays the
            // final response or triggers watcher-death handoff. Clearing it
            // here breaks the handoff path if the recovered tmux session
            // dies before producing a result.
            continue;
        }

        shared
            .recovering_channels
            .insert(channel_id, std::time::Instant::now());

        let sqlite_settings_db = if shared.pg_pool.is_some() {
            None
        } else {
            None::<&crate::db::Db>
        };
        let persisted_session_path = load_last_session_path(
            sqlite_settings_db,
            shared.pg_pool.as_ref(),
            &shared.token_hash,
            channel_id.get(),
        );
        let recovery_adk_cwd = match recovery_spawn_adk_cwd(&state, persisted_session_path) {
            Ok(path) => path,
            Err(error) => {
                let dispatch_id = state
                    .dispatch_id
                    .clone()
                    .or_else(|| parse_dispatch_id(&state.user_text));
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::error!("  [{ts}] {error}; main-workspace fallback blocked");
                crate::services::observability::emit_recovery_fired(
                    provider.as_str(),
                    state.channel_id,
                    dispatch_id.as_deref(),
                    state.session_key.as_deref(),
                    "worktree_missing_main_fallback_blocked",
                );
                emit_recovery_quality_event(
                    provider,
                    state.channel_id,
                    dispatch_id.as_deref(),
                    state.session_key.as_deref(),
                    "worktree_missing_main_fallback_blocked",
                );
                let relay_ok = super::formatting::replace_long_message_raw(
                    http,
                    channel_id,
                    current_msg_id,
                    &format!("❌ {error}\nmain workspace fallback blocked."),
                    shared,
                )
                .await
                .is_ok();
                if should_advance_recovery_dispatch_after_relay(relay_ok) {
                    super::turn_bridge::fail_dispatch_with_retry(
                        shared.api_port,
                        dispatch_id.as_deref(),
                        &error,
                    )
                    .await;
                    super::restart_report::clear_restart_report(provider, state.channel_id);
                    clear_inflight_state(provider, state.channel_id);
                } else {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ recovery: worktree error relay failed before dispatch failure — preserving inflight for retry"
                    );
                }
                continue;
            }
        };
        let saved_remote = load_last_remote_profile(
            sqlite_settings_db,
            shared.pg_pool.as_ref(),
            &shared.token_hash,
            channel_id.get(),
        );

        let cancel_token = Arc::new(CancelToken::new());
        super::turn_bridge::bind_cancel_token_tmux_runtime(
            provider,
            &cancel_token,
            &tmux_session_name,
            "recovery kickoff",
        );

        {
            let mut data = shared.core.lock().await;
            let session = data
                .sessions
                .entry(channel_id)
                .or_insert_with(|| DiscordSession {
                    session_id: state.session_id.clone(),
                    memento_context_loaded: false,
                    memento_reflected: false,
                    current_path: None,
                    history: Vec::new(),
                    pending_uploads: Vec::new(),
                    cleared: false,
                    remote_profile_name: saved_remote.clone(),
                    channel_id: Some(channel_id.get()),
                    channel_name: channel_name.clone(),
                    category_name: None,
                    last_active: tokio::time::Instant::now(),
                    worktree: None,

                    born_generation: super::runtime_store::load_generation(),
                    assistant_turns: 0,
                });
            session.channel_id = Some(channel_id.get());
            session.last_active = tokio::time::Instant::now();
            if session.current_path.is_none() {
                session.current_path = recovery_adk_cwd.clone();
            }
            if session.channel_name.is_none() {
                session.channel_name = channel_name.clone();
            }
            if session.remote_profile_name.is_none() {
                session.remote_profile_name = saved_remote;
            }
            restore_recovered_session_worktree(session, &state);
        }

        mailbox_recovery_kickoff(
            shared,
            channel_id,
            cancel_token.clone(),
            UserId::new(state.request_owner_user_id),
            MessageId::new(state.user_msg_id),
        )
        .await;

        let adk_session_key = build_adk_session_key(shared, channel_id, provider).await;
        let adk_session_name = channel_name.clone();
        let adk_session_info = derive_adk_session_info(
            Some(&state.user_text),
            channel_name.as_deref(),
            recovery_adk_cwd.as_deref(),
        );
        let role_binding = resolve_role_binding(channel_id, channel_name.as_deref());
        let adk_thread_channel_id = adk_session_name
            .as_deref()
            .and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name);
        post_adk_session_status(
            adk_session_key.as_deref(),
            adk_session_name.as_deref(),
            Some(provider.as_str()),
            "working",
            provider,
            Some(&adk_session_info),
            None,
            recovery_adk_cwd.as_deref(),
            parse_dispatch_id(&state.user_text)
                .or(lookup_pending_dispatch_for_thread(shared.api_port, channel_id.get()).await)
                .as_deref(),
            adk_thread_channel_id,
            Some(channel_id),
            role_binding
                .as_ref()
                .map(|binding| binding.role_id.as_str()),
            shared.api_port,
        )
        .await;

        let (tx, rx) = mpsc::channel();
        let cancel_for_reader = cancel_token.clone();
        let output_for_reader = output_path.clone();
        let input_for_reader = input_fifo_path.clone();
        let tmux_for_reader = tmux_session_name.clone();
        let start_offset = state.last_offset;
        let recovery_session_id = state.session_id.clone();
        let runtime_kind_for_reader = runtime_kind;
        let retry_channel_id = channel_id.get();
        std::thread::spawn(move || {
            match crate::services::session_backend::read_output_file_until_result(
                &output_for_reader,
                start_offset,
                tx.clone(),
                Some(cancel_for_reader),
                crate::services::provider::SessionProbe::tmux(tmux_for_reader.clone()),
            ) {
                Ok(ReadOutputResult::Completed { offset })
                | Ok(ReadOutputResult::Cancelled { offset }) => {
                    let _ = tx.send(StreamMessage::RuntimeReady {
                        handoff: runtime_handoff_for_recovery(
                            runtime_kind_for_reader,
                            output_for_reader,
                            input_for_reader,
                            tmux_for_reader,
                            recovery_session_id,
                            offset,
                        ),
                    });
                }
                Ok(ReadOutputResult::SessionDied { offset }) => {
                    // Check if tmux pane is actually alive — dcserver restart
                    // may cause SessionDied because no new output arrived, but
                    // the Claude CLI process could still be idle (waiting for input).
                    let pane_alive = tmux_session_alive_with_retry(&tmux_for_reader);
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    if pane_alive {
                        // Session is alive but idle — hand off to watcher instead of retrying
                        tracing::warn!(
                            "  [{ts}] ↻ Recovery: session idle but pane alive — handing off to watcher (channel {})",
                            retry_channel_id
                        );
                        let _ = tx.send(StreamMessage::RuntimeReady {
                            handoff: runtime_handoff_for_recovery(
                                runtime_kind_for_reader,
                                output_for_reader,
                                input_for_reader,
                                tmux_for_reader,
                                recovery_session_id,
                                offset,
                            ),
                        });
                    } else {
                        // Session truly died during restart recovery. Fall back
                        // to the generic auto-retry path so restart handling
                        // does not get a special handoff-only branch.
                        tracing::warn!(
                            "  [{ts}] ↻ Recovery: session died, signaling generic auto-retry (channel {})",
                            retry_channel_id
                        );
                        let _ = tx.send(StreamMessage::Done {
                            result: "__session_died_retry__".to_string(),
                            session_id: recovery_session_id,
                        });
                    }
                }
                Err(e) => {
                    let _ = tx.send(StreamMessage::Error {
                        message: e,
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: None,
                    });
                }
            }
        });

        let recovery_dispatch_id = parse_dispatch_id(&state.user_text)
            .or(lookup_pending_dispatch_for_thread(shared.api_port, channel_id.get()).await);
        let recovery_dispatch_kind =
            lookup_turn_finished_dispatch_kind(recovery_dispatch_id.as_deref()).await;
        // Backfill session_key/dispatch_id on inflight state for long-turn detection ([L]).
        let mut state = state;
        state.session_key = state.session_key.or_else(|| adk_session_key.clone());
        state.dispatch_id = state.dispatch_id.or_else(|| recovery_dispatch_id.clone());
        spawn_turn_bridge(
            shared.clone(),
            cancel_token,
            rx,
            TurnBridgeContext {
                provider: provider.clone(),
                gateway: Arc::new(DiscordGateway::new(
                    http.clone(),
                    shared.clone(),
                    provider.clone(),
                    None,
                )),
                channel_id,
                user_msg_id,
                user_text_owned: state.user_text.clone(),
                request_owner_name: String::new(),
                role_binding,
                adk_session_key,
                adk_session_name,
                adk_session_info: Some(adk_session_info),
                adk_cwd: recovery_adk_cwd.clone(),
                dispatch_id: recovery_dispatch_id,
                dispatch_kind: recovery_dispatch_kind,
                memory_recall_usage: crate::services::memory::TokenUsage::default(),
                context_window_tokens: provider.default_context_window(),
                context_compact_percent: super::adk_session::ContextThresholds::default()
                    .compact_pct_for(&provider),
                current_msg_id,
                response_sent_offset: state.response_sent_offset,
                full_response: state.full_response.clone(),
                tmux_last_offset: Some(state.last_offset),
                new_session_id: state.session_id.clone(),
                defer_watcher_resume: false,
                reuse_status_panel_message: true,
                completion_tx: None,
                inflight_state: state,
            },
        );
    }
}

/// #896: Outcome of a successful [`rebind_inflight_for_channel`] call.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RebindOutcome {
    pub tmux_session: String,
    pub channel_id: u64,
    pub initial_offset: u64,
    /// `true` when a tmux watcher was spawned by this call. On unix this is
    /// always true on success. On non-unix builds watcher spawning is a
    /// no-op, so this reads `false` even though the inflight file was
    /// written.
    pub watcher_spawned: bool,
    /// #897 P2 #2 — `true` when a pre-existing watcher handle was present
    /// for this channel and has been cancelled + replaced by the freshly
    /// spawned one. Operators use this to distinguish a clean vacant claim
    /// from a zombie-slot recovery, which is the common case where an old
    /// watcher kept its DashMap entry after its tmux exited.
    pub watcher_replaced: bool,
}

/// #896: Errors from [`rebind_inflight_for_channel`]. Map 1:1 to HTTP status
/// codes in the `/api/inflight/rebind` handler.
#[derive(Debug)]
pub enum RebindError {
    /// Target tmux session is not alive — nothing to rebind to. 404.
    TmuxNotAlive { tmux_session: String },
    /// An inflight state already exists for this channel. Caller must clear
    /// it (force-kill or natural completion) before rebinding. 409.
    InflightAlreadyExists,
    /// The tmux pane is still writing to a deleted or replaced output fd, so
    /// rebinding the pathname would silently follow the wrong file. 409.
    StaleOutputPath {
        tmux_session: String,
        output_path: String,
        live_fd: String,
        live_inode: Option<u64>,
        live_path: String,
    },
    /// Channel is not bound to the requested provider in the role-map. 400.
    ChannelNotBound,
    /// `tmux_session` not provided and no in-memory session supplies a
    /// channel_name — cannot derive the canonical tmux session name. 400.
    ChannelNameMissing,
    /// Unrecoverable internal error (inflight write, lock poisoning, etc.). 500.
    Internal(String),
}

impl std::fmt::Display for RebindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TmuxNotAlive { tmux_session } => {
                write!(f, "tmux session not alive: {tmux_session}")
            }
            Self::InflightAlreadyExists => {
                write!(f, "inflight state already exists for this channel")
            }
            Self::StaleOutputPath {
                tmux_session,
                output_path,
                live_fd,
                live_inode,
                live_path,
            } => {
                write!(
                    f,
                    "StaleOutputPath: tmux session {tmux_session} still writes to fd {live_fd}"
                )?;
                if let Some(inode) = live_inode {
                    write!(f, " (inode {inode})")?;
                }
                write!(
                    f,
                    " via {live_path}; refusing to rebind pathname {output_path}"
                )
            }
            Self::ChannelNotBound => write!(f, "channel is not bound for this provider"),
            Self::ChannelNameMissing => write!(
                f,
                "channel name missing — pass tmux_session or pre-register the channel"
            ),
            Self::Internal(msg) => write!(f, "internal: {msg}"),
        }
    }
}

/// #896: Rebind a live tmux session to a freshly-created inflight state and
/// (re)spawn the output watcher. Used to recover from orphan states where
/// the tmux session is alive but the inflight JSON was cleared by a prior
/// turn's cleanup, leaving subsequent output with no relay path.
///
/// Preconditions (enforced — caller gets a typed error on violation):
/// * Tmux session must be alive. Absent session ⇒ nothing to rebind; the
///   caller should force-kill + restart instead.
/// * No existing inflight must exist for the channel. Caller clears first
///   to avoid racing with a live turn's state.
/// * Channel must be bound to the requested provider in the role-map.
///
/// Side effects on success:
/// * Writes `~/.adk/release/runtime/discord_inflight/{provider}/{channel}.json`
///   with `last_offset` set to the tmux output file's current size, so the
///   watcher only picks up output produced *after* this call. Retroactive
///   emission of already-dropped output is intentionally out of scope.
/// * Registers / refreshes the `DiscordSession` entry in `shared.core.sessions`.
/// * Spawns a `tmux_output_watcher` via the single-watcher claim policy. If
///   a live watcher already owns the tmux session, the existing owner is
///   reused and `watcher_spawned=false` is returned — the inflight we just
///   created will still be picked up by the existing watcher, so this is not
///   an error.
pub(crate) async fn rebind_inflight_for_channel(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session_override: Option<String>,
) -> Result<RebindOutcome, RebindError> {
    let discord_channel_id = ChannelId::new(channel_id);

    // Preflight existence check — fast 409 before we walk the validation /
    // tmux-liveness path when the caller obviously shouldn't be rebinding.
    // This is advisory only; the AUTHORITATIVE guard is the atomic
    // `save_inflight_state_create_new` below which uses `O_CREAT | O_EXCL`
    // so a live turn that wins the race between here and the write cannot
    // be clobbered by the synthetic rebind state.
    let existing_inflight = match super::inflight::load_inflight_state(provider, channel_id) {
        Some(existing) => match recovery_phase_for_existing_inflight_rebind(&existing) {
            RecoveryPhase::WatcherReattach => {
                super::inflight::clear_inflight_state(provider, channel_id);
                None
            }
            RecoveryPhase::InflightRestore => Some(existing),
            RecoveryPhase::Pending | RecoveryPhase::Done => {
                return Err(RebindError::InflightAlreadyExists);
            }
        },
        None => None,
    };
    let resuming_existing_inflight = existing_inflight.is_some();

    if resuming_existing_inflight {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ♻ rebind resuming existing inflight turn for channel {} without overwriting canonical state",
            channel_id
        );
    }

    let existing_session_id = existing_inflight
        .as_ref()
        .and_then(|state| state.session_id.clone());
    let existing_saved_output_path = existing_inflight
        .as_ref()
        .and_then(|state| state.output_path.clone());

    // Resolve tmux session name + channel name from the request, falling back
    // to the in-memory session map when no override is provided.
    let (tmux_session_name, channel_name) = match tmux_session_override {
        Some(name) => {
            let ch_name =
                crate::services::provider::parse_provider_and_channel_from_tmux_name(&name)
                    .map(|(_, ch)| ch);
            (name, ch_name)
        }
        None => {
            let ch_name = {
                let data = shared.core.lock().await;
                data.sessions
                    .get(&discord_channel_id)
                    .and_then(|s| s.channel_name.clone())
            };
            let ch_name = match ch_name {
                Some(n) => n,
                None => return Err(RebindError::ChannelNameMissing),
            };
            let tmux = provider.build_tmux_session_name(&ch_name);
            (tmux, Some(ch_name))
        }
    };

    if !tmux_session_alive_with_retry(&tmux_session_name) {
        return Err(RebindError::TmuxNotAlive {
            tmux_session: tmux_session_name,
        });
    }

    // Validate provider↔channel binding against the settings snapshot,
    // mirroring what `restore_inflight_turns` requires for watcher revival.
    let settings_snapshot = shared.settings.read().await.clone();
    let is_dm = matches!(
        discord_channel_id.to_channel(http).await,
        Ok(serenity::model::channel::Channel::Private(_))
    );
    let (allowlist_channel_id, provider_channel_name) =
        if let Some((pid, pname)) = super::resolve_thread_parent(http, discord_channel_id).await {
            (pid, pname.or(channel_name.clone()))
        } else {
            (discord_channel_id, channel_name.clone())
        };
    if validate_bot_channel_routing_with_provider_channel(
        &settings_snapshot,
        provider,
        allowlist_channel_id,
        channel_name.as_deref(),
        provider_channel_name.as_deref(),
        is_dm,
    )
    .is_err()
    {
        return Err(RebindError::ChannelNotBound);
    }

    let (default_output_path, input_fifo) = tmux_runtime_paths(&tmux_session_name);
    let (output_path, synthetic_initial_offset) = {
        #[cfg(unix)]
        {
            match detect_live_tmux_output_path(&tmux_session_name, &default_output_path) {
                Ok(Some(detected)) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ♻ rebind adopted live tmux output path for {}: {} -> {} (offset {})",
                        tmux_session_name,
                        default_output_path,
                        detected.path,
                        detected.initial_offset
                    );
                    (detected.path, detected.initial_offset)
                }
                Ok(None) => {
                    let synthetic_initial_offset = std::fs::metadata(&default_output_path)
                        .map(|m| m.len())
                        .unwrap_or(0);
                    (default_output_path.clone(), synthetic_initial_offset)
                }
                Err(stale) => {
                    return Err(RebindError::StaleOutputPath {
                        tmux_session: tmux_session_name.clone(),
                        output_path: default_output_path.clone(),
                        live_fd: stale.fd,
                        live_inode: stale.inode,
                        live_path: stale.raw_path,
                    });
                }
            }
        }
        #[cfg(not(unix))]
        {
            let synthetic_initial_offset = std::fs::metadata(&default_output_path)
                .map(|m| m.len())
                .unwrap_or(0);
            (default_output_path.clone(), synthetic_initial_offset)
        }
    };

    let initial_offset = if let Some(existing) = existing_inflight.as_ref() {
        let (resume_offset, current_len, truncated) =
            recovery_watcher_start_offset(&output_path, existing.last_offset);
        if truncated {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ rebind restarting existing inflight watcher from 0 for {} (saved offset {}, file len {})",
                tmux_session_name,
                existing.last_offset,
                current_len
            );
        }
        if existing_saved_output_path.as_deref() != Some(output_path.as_str()) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ♻ rebind watcher adopted live output path for existing inflight {}: {:?} -> {}",
                tmux_session_name,
                existing_saved_output_path,
                output_path
            );
        }
        resume_offset
    } else {
        synthetic_initial_offset
    };

    let recovered_state_for_session = if let Some(existing) = existing_inflight.clone() {
        existing
    } else {
        // Build and persist the new inflight state. No request_owner / msg_ids
        // apply because this recovery has no originating Discord message.
        //
        // #897 counter-model re-review (round 2): flag this as `rebind_origin`
        // so routing / persistence code that keys off "is there a live
        // foreground turn" treats it as absent. This synthetic state exists only
        // to expose a recovered tmux session through inflight APIs; it must not
        // masquerade as a user-authored Discord turn.
        let mut state = super::inflight::InflightTurnState::new(
            provider.clone(),
            channel_id,
            channel_name.clone(),
            0, // request_owner_user_id — no originating Discord user
            0, // user_msg_id
            0, // current_msg_id (placeholder)
            String::from("/api/inflight/rebind"),
            None, // session_id
            Some(tmux_session_name.clone()),
            Some(output_path.clone()),
            Some(input_fifo.clone()),
            initial_offset,
        );
        state.rebind_origin = true;

        // Atomic create-or-fail: if a legitimate turn created its inflight file
        // between the preflight check above and this point, the write fails
        // with `AlreadyExists` and we return 409. Without this guard the
        // synthetic rebind state (user_msg_id=0, placeholder ids zeroed) would
        // overwrite the real turn's canonical state and break its completion
        // path — the exact race the #897 P2 #1 review flagged.
        match super::inflight::save_inflight_state_create_new(&state) {
            Ok(()) => {}
            Err(super::inflight::CreateNewInflightError::AlreadyExists) => {
                return Err(RebindError::InflightAlreadyExists);
            }
            Err(super::inflight::CreateNewInflightError::Internal(msg)) => {
                return Err(RebindError::Internal(msg));
            }
        }
        state
    };

    // Register / refresh the in-memory session so downstream handlers can
    // locate this channel after the rebind.
    {
        let mut data = shared.core.lock().await;
        let session = data
            .sessions
            .entry(discord_channel_id)
            .or_insert_with(|| DiscordSession {
                session_id: existing_session_id.clone(),
                memento_context_loaded: false,
                memento_reflected: false,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                remote_profile_name: None,
                channel_id: Some(channel_id),
                channel_name: channel_name.clone(),
                category_name: None,
                last_active: tokio::time::Instant::now(),
                worktree: None,
                born_generation: super::runtime_store::load_generation(),
                assistant_turns: 0,
            });
        session.channel_id = Some(channel_id);
        session.last_active = tokio::time::Instant::now();
        if session.channel_name.is_none() {
            session.channel_name = channel_name.clone();
        }
        restore_recovered_session_worktree(session, &recovered_state_for_session);
    }

    // #1135: claim with the single-watcher policy. A live watcher for this
    // same tmux session is reused; a cancelled same-session handle or a
    // different-session channel incumbent is replaced so recovery is not
    // blocked by stale registry state.
    let (watcher_spawned, watcher_replaced) = {
        #[cfg(unix)]
        {
            let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let resume_offset = std::sync::Arc::new(std::sync::Mutex::new(None::<u64>));
            let pause_epoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
            let turn_delivered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let last_heartbeat_ts_ms = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(
                super::tmux_watcher_now_ms(),
            ));
            let mailbox_finalize_owed =
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let handle = TmuxWatcherHandle {
                tmux_session_name: tmux_session_name.clone(),
                paused: paused.clone(),
                resume_offset: resume_offset.clone(),
                cancel: cancel.clone(),
                pause_epoch: pause_epoch.clone(),
                turn_delivered: turn_delivered.clone(),
                last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
                mailbox_finalize_owed: mailbox_finalize_owed.clone(),
            };
            // `claim_or_reuse_watcher` reuses a live watcher for the same
            // tmux session and only spawns when it claimed or replaced a
            // stale/different-session slot.
            let claim = super::tmux::claim_or_reuse_watcher(
                &shared.tmux_watchers,
                discord_channel_id,
                handle,
                provider,
                "recovery_restore_inflight",
            );
            if claim.should_spawn() {
                shared.record_tmux_watcher_reconnect(discord_channel_id);
                tokio::spawn(super::tmux::tmux_output_watcher(
                    discord_channel_id,
                    http.clone(),
                    shared.clone(),
                    output_path.clone(),
                    tmux_session_name.clone(),
                    initial_offset,
                    cancel,
                    paused,
                    resume_offset,
                    pause_epoch,
                    turn_delivered,
                    last_heartbeat_ts_ms,
                    mailbox_finalize_owed,
                ));
            }
            (claim.should_spawn(), claim.replaced_existing())
        }
        #[cfg(not(unix))]
        {
            (false, false)
        }
    };

    Ok(RebindOutcome {
        tmux_session: tmux_session_name,
        channel_id,
        initial_offset,
        watcher_spawned,
        watcher_replaced,
    })
}

#[cfg(test)]
mod post_work_evidence_tests {
    use super::*;
    use crate::services::provider::ProviderKind;

    #[test]
    fn recovery_input_fifo_requirement_is_runtime_specific() {
        assert_eq!(
            recovery_input_fifo_for_runtime(RuntimeHandoffKind::ClaudeTui, None).unwrap(),
            None
        );
        assert_eq!(
            recovery_input_fifo_for_runtime(RuntimeHandoffKind::CodexTui, None).unwrap(),
            None
        );
        assert!(
            recovery_input_fifo_for_runtime(RuntimeHandoffKind::LegacyTmuxWrapper, None).is_err()
        );
        assert_eq!(
            recovery_input_fifo_for_runtime(
                RuntimeHandoffKind::LegacyTmuxWrapper,
                Some("/tmp/session.input".to_string())
            )
            .unwrap(),
            Some("/tmp/session.input".to_string())
        );
    }

    #[test]
    fn recovery_handoff_preserves_runtime_kind() {
        let handoff = runtime_handoff_for_recovery(
            RuntimeHandoffKind::ClaudeTui,
            "/tmp/claude-transcript.jsonl".to_string(),
            None,
            "AgentDesk-claude-adk".to_string(),
            Some("session-1".to_string()),
            42,
        );

        match handoff {
            RuntimeHandoff::ClaudeTui {
                transcript_path,
                tmux_session_name,
                last_offset,
            } => {
                assert_eq!(transcript_path, "/tmp/claude-transcript.jsonl");
                assert_eq!(tmux_session_name, "AgentDesk-claude-adk");
                assert_eq!(last_offset, 42);
            }
            other => panic!("expected ClaudeTui handoff, got {other:?}"),
        }
    }

    #[test]
    fn tmux_ready_completion_requires_current_turn_work_evidence() {
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "background notification".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.input".to_string()),
            64,
        );
        state.task_notification_kind =
            Some(crate::services::agent_protocol::TaskNotificationKind::Background);

        assert!(
            !recovery_has_post_work_ready_evidence(&state),
            "task-notification-only inflight must not trust a stale tmux Ready for input footer"
        );

        state.full_response = "completed".to_string();
        assert!(recovery_has_post_work_ready_evidence(&state));

        state.full_response.clear();
        state.any_tool_used = true;
        assert!(recovery_has_post_work_ready_evidence(&state));

        state.any_tool_used = false;
        state.last_tool_summary = Some("Bash completed".to_string());
        assert!(recovery_has_post_work_ready_evidence(&state));
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::services::provider::ProviderKind;
    use axum::extract::Query;
    use axum::extract::{Path as AxumPath, State as AxumState};
    use axum::http::Uri;
    use axum::response::IntoResponse;
    use axum::{Json, Router};
    use poise::serenity_prelude as serenity;
    use sqlite_test::OptionalExtension;
    use std::ffi::OsString;
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    // #964 DoD item #2: integration-level fixture — when terminal success
    // has been observed but the tmux output file still has bytes trailing
    // beyond the confirmed relay end, `terminal_success_output_drained_for_recovery`
    // MUST return false so the caller reattaches the watcher instead of
    // declaring the turn complete and silently detaching.
    #[tokio::test]
    async fn terminal_success_drain_refuses_to_stop_when_output_file_has_pending_bytes() {
        let tmp = tempfile::tempdir().expect("tempdir for drain test");
        let output_path = tmp.path().join("pending-output.jsonl");
        // File length = 200. Confirmed end = 120. 80 bytes of pending tmux
        // output remain after the last relay. tmux_session=None skips the
        // live-session path and uses the pure metadata guard.
        std::fs::write(&output_path, vec![b'x'; 200]).expect("seed pending output file");

        let drained = super::terminal_success_output_drained_for_recovery(
            output_path.to_str().expect("utf8 path"),
            120,
            None,
        )
        .await;

        assert!(
            !drained,
            "drain guard must refuse stop when tmux_tail_offset > confirmed_end",
        );
    }

    #[tokio::test]
    async fn terminal_success_drain_allows_stop_when_confirmed_end_reaches_tail() {
        let tmp = tempfile::tempdir().expect("tempdir for drain test");
        let output_path = tmp.path().join("drained-output.jsonl");
        std::fs::write(&output_path, vec![b'x'; 200]).expect("seed drained output file");

        // Confirmed end == tail, and with tmux_session=None the function
        // takes the fast path (no live session → just check the invariant
        // + quiet-period gate). The quiet-period value passed is the module
        // constant, which terminal_success_watcher_stop_allowed accepts.
        let drained = super::terminal_success_output_drained_for_recovery(
            output_path.to_str().expect("utf8 path"),
            200,
            None,
        )
        .await;
        assert!(drained, "drain guard must permit stop when fully drained");
    }

    // #964: watcher-tmux lifecycle invariant — the terminal-success drain
    // guard must refuse to stop the watcher whenever the confirmed relay end
    // trails the current tmux output tail, or when the drain window has not
    // held long enough. Both are required; either alone is insufficient.
    #[test]
    fn terminal_success_drain_guard_requires_confirmed_end_and_quiet_period() {
        let quiet = TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD;

        // Happy path: confirmed_end == tmux_tail AND quiet_for has elapsed.
        assert!(terminal_success_watcher_stop_allowed(100, 100, quiet));
        assert!(terminal_success_watcher_stop_allowed(
            100,
            100,
            quiet + std::time::Duration::from_secs(1),
        ));

        // Confirmed end trails tail: watcher must NOT stop even after quiet
        // period — tmux still produced output the watcher has not relayed.
        assert!(!terminal_success_watcher_stop_allowed(99, 100, quiet));
        assert!(!terminal_success_watcher_stop_allowed(0, 100, quiet * 4,));

        // Drain window too short: the file may still be producing bytes.
        assert!(!terminal_success_watcher_stop_allowed(
            100,
            100,
            quiet - std::time::Duration::from_millis(1),
        ));

        // Confirmed end surpassing tmux_tail is still a stop-allowed condition
        // (this covers the bridge-delivered case where response_sent_offset is
        // recorded beyond the observed file metadata length).
        assert!(terminal_success_watcher_stop_allowed(200, 100, quiet));
    }

    #[test]
    fn recovery_phase_round_trips_through_str() {
        let variants = [
            RecoveryPhase::Pending,
            RecoveryPhase::WatcherReattach,
            RecoveryPhase::InflightRestore,
            RecoveryPhase::Done,
        ];

        for phase in variants {
            assert_eq!(RecoveryPhase::from_str(phase.as_str()), Some(phase));
        }
        assert_eq!(
            format!("{:?}", RecoveryPhase::InflightRestore),
            "InflightRestore"
        );
    }

    #[test]
    fn recovery_phase_from_optional_str_handles_none() {
        assert_eq!(RecoveryPhase::from_optional_str(None), None);
    }

    #[test]
    fn recovery_phase_from_str_rejects_unknown_value() {
        assert_eq!(RecoveryPhase::from_str("unknown"), None);
        assert_eq!(RecoveryPhase::from_str(""), None);
    }

    #[test]
    fn recovery_phase_sqlite_boundary_round_trips_rebind_call_site() {
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "real user input".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.input".to_string()),
            64,
        );

        let conn = sqlite_test::Connection::open_in_memory()
            .expect("in-memory sqlite should open for recovery phase test");
        conn.execute_batch(
            "CREATE TABLE recovery_phase_roundtrip (
                id TEXT PRIMARY KEY,
                state TEXT
            );",
        )
        .expect("test table should be created");

        let initial_phase = recovery_phase_for_existing_inflight_rebind(&state);
        assert_eq!(initial_phase, RecoveryPhase::InflightRestore);
        conn.execute(
            "INSERT INTO recovery_phase_roundtrip (id, state) VALUES (?1, ?2)",
            sqlite_test::params!["phase-1", initial_phase.as_str()],
        )
        .expect("initial recovery phase should be inserted");

        let persisted: Option<String> = conn
            .query_row(
                "SELECT state FROM recovery_phase_roundtrip WHERE id = ?1",
                ["phase-1"],
                |row| row.get(0),
            )
            .optional()
            .expect("test phase row should be readable");
        let phase = RecoveryPhase::from_optional_str(persisted.as_deref())
            .unwrap_or(RecoveryPhase::Pending);
        assert_eq!(phase, RecoveryPhase::InflightRestore);

        match phase {
            RecoveryPhase::InflightRestore => state.last_watcher_relayed_offset = Some(128),
            RecoveryPhase::Pending | RecoveryPhase::WatcherReattach | RecoveryPhase::Done => {}
        };
        let next_phase = recovery_phase_for_existing_inflight_rebind(&state);
        assert_eq!(next_phase, RecoveryPhase::Pending);
        conn.execute(
            "UPDATE recovery_phase_roundtrip SET state = ?1 WHERE id = ?2",
            sqlite_test::params![next_phase.as_str(), "phase-1"],
        )
        .expect("updated recovery phase should be written");

        let updated: Option<String> = conn
            .query_row(
                "SELECT state FROM recovery_phase_roundtrip WHERE id = ?1",
                ["phase-1"],
                |row| row.get(0),
            )
            .optional()
            .expect("updated phase row should be readable");
        assert_eq!(
            RecoveryPhase::from_optional_str(updated.as_deref()),
            Some(RecoveryPhase::Pending)
        );
    }

    #[derive(Default)]
    struct MockRecoveryDiscordState {
        edited_messages: Vec<(String, String, String)>,
        posted_messages: Vec<(String, String)>,
        reaction_paths: Vec<String>,
    }

    #[derive(Clone)]
    struct MockRecoveryInternalApiState {
        pending_dispatch_id: String,
        dispatch_types: std::collections::HashMap<String, String>,
    }

    struct EnvVarReset {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarReset {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarReset {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn mock_discord_message(
        channel_id: &str,
        message_id: &str,
        content: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "id": message_id,
            "channel_id": channel_id,
            "author": {
                "id": "999999999999999999",
                "username": "mock-bot",
                "bot": true
            },
            "content": content,
            "timestamp": "2026-04-23T00:00:00.000000+00:00",
            "edited_timestamp": null,
            "tts": false,
            "mention_everyone": false,
            "mentions": [],
            "mention_roles": [],
            "attachments": [],
            "embeds": [],
            "pinned": false,
            "webhook_id": null,
            "type": 0
        })
    }

    async fn spawn_mock_recovery_discord_server() -> (
        String,
        Arc<Mutex<MockRecoveryDiscordState>>,
        tokio::task::JoinHandle<()>,
    ) {
        async fn edit_message(
            AxumState(state): AxumState<Arc<Mutex<MockRecoveryDiscordState>>>,
            AxumPath((channel_id, message_id)): AxumPath<(String, String)>,
            Json(body): Json<serde_json::Value>,
        ) -> impl IntoResponse {
            let content = body
                .get("content")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            state.lock().unwrap().edited_messages.push((
                channel_id.clone(),
                message_id.clone(),
                content.clone(),
            ));
            (
                axum::http::StatusCode::OK,
                Json(mock_discord_message(&channel_id, &message_id, &content)),
            )
        }

        async fn post_message(
            AxumState(state): AxumState<Arc<Mutex<MockRecoveryDiscordState>>>,
            AxumPath(channel_id): AxumPath<String>,
            Json(body): Json<serde_json::Value>,
        ) -> impl IntoResponse {
            let content = body
                .get("content")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            state
                .lock()
                .unwrap()
                .posted_messages
                .push((channel_id.clone(), content.clone()));
            (
                axum::http::StatusCode::OK,
                Json(mock_discord_message(
                    &channel_id,
                    "message-fallback",
                    &content,
                )),
            )
        }

        async fn add_reaction(
            AxumState(state): AxumState<Arc<Mutex<MockRecoveryDiscordState>>>,
            uri: Uri,
        ) -> impl IntoResponse {
            state
                .lock()
                .unwrap()
                .reaction_paths
                .push(format!("PUT {}", uri.path()));
            axum::http::StatusCode::NO_CONTENT
        }

        async fn remove_reaction(
            AxumState(state): AxumState<Arc<Mutex<MockRecoveryDiscordState>>>,
            uri: Uri,
        ) -> impl IntoResponse {
            state
                .lock()
                .unwrap()
                .reaction_paths
                .push(format!("DELETE {}", uri.path()));
            axum::http::StatusCode::NO_CONTENT
        }

        let state = Arc::new(Mutex::new(MockRecoveryDiscordState::default()));
        let app = Router::new()
            .route(
                "/api/v10/channels/{channel_id}/messages/{message_id}",
                axum::routing::patch(edit_message),
            )
            .route(
                "/api/v10/channels/{channel_id}/messages",
                axum::routing::post(post_message),
            )
            .route(
                "/api/v10/channels/{channel_id}/messages/{message_id}/reactions/{emoji}/@me",
                axum::routing::put(add_reaction).delete(remove_reaction),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), state, handle)
    }

    async fn spawn_mock_recovery_internal_api_server(
        state: MockRecoveryInternalApiState,
    ) -> tokio::task::JoinHandle<()> {
        async fn pending_dispatch(
            AxumState(state): AxumState<MockRecoveryInternalApiState>,
        ) -> impl IntoResponse {
            (
                axum::http::StatusCode::OK,
                Json(serde_json::json!({ "dispatch_id": state.pending_dispatch_id })),
            )
        }

        async fn dispatch_info(
            AxumState(state): AxumState<MockRecoveryInternalApiState>,
            Query(params): Query<std::collections::HashMap<String, String>>,
        ) -> impl IntoResponse {
            let dispatch_id = params.get("dispatch_id").cloned().unwrap_or_default();
            match state.dispatch_types.get(&dispatch_id) {
                Some(dispatch_type) => (
                    axum::http::StatusCode::OK,
                    Json(serde_json::json!({ "dispatch_type": dispatch_type })),
                ),
                None => (
                    axum::http::StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": "dispatch not found" })),
                ),
            }
        }

        let app = Router::new()
            .route(
                "/api/internal/pending-dispatch-for-thread",
                axum::routing::get(pending_dispatch),
            )
            .route(
                "/api/internal/card-thread",
                axum::routing::get(dispatch_info),
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        super::super::internal_api::init(addr.port(), None);
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        })
    }

    #[test]
    fn detects_result_after_offset_only_in_remaining_slice() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"before\"}}]}}}}"
        )
        .unwrap();
        let offset = file.as_file().metadata().unwrap().len();
        writeln!(
            file,
            "{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}}"
        )
        .unwrap();

        assert!(output_has_result_after_offset(
            file.path().to_str().unwrap(),
            offset
        ));
    }

    #[test]
    fn success_result_end_offset_stops_at_result_line_before_followup_output() {
        let file = write_jsonl(&[
            r#"{"type":"result","subtype":"success","result":"done"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"next"}]}}"#,
        ]);
        let full_len = file.as_file().metadata().unwrap().len();
        let result_end =
            success_result_end_offset_after_offset(file.path().to_str().unwrap(), 0).unwrap();

        assert!(
            result_end < full_len,
            "confirmed end must not jump past follow-up output"
        );
        assert!(!terminal_success_watcher_stop_allowed(
            result_end,
            full_len,
            TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD
        ));
    }

    #[test]
    fn ignores_result_before_offset() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            "{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}}"
        )
        .unwrap();
        let offset = file.as_file().metadata().unwrap().len();
        writeln!(
            file,
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"after\"}}]}}}}"
        )
        .unwrap();

        assert!(!output_has_result_after_offset(
            file.path().to_str().unwrap(),
            offset
        ));
    }

    #[test]
    fn detects_new_bytes_after_offset() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "before").unwrap();
        let offset = file.as_file().metadata().unwrap().len();
        writeln!(file, "after").unwrap();

        assert!(output_has_bytes_after_offset(
            file.path().to_str().unwrap(),
            offset
        ));
    }

    #[test]
    fn ignores_missing_new_bytes_after_offset() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "before").unwrap();
        let offset = file.as_file().metadata().unwrap().len();

        assert!(!output_has_bytes_after_offset(
            file.path().to_str().unwrap(),
            offset
        ));
    }

    #[test]
    fn terminal_success_stop_requires_drained_tail_and_quiet_period() {
        assert!(terminal_success_watcher_stop_allowed(
            256,
            256,
            TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD
        ));
        assert!(!terminal_success_watcher_stop_allowed(
            128,
            256,
            TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD
        ));
        assert!(!terminal_success_watcher_stop_allowed(
            256,
            256,
            TERMINAL_SUCCESS_DRAIN_QUIET_PERIOD - std::time::Duration::from_millis(1)
        ));
    }

    fn write_jsonl(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(file, "{}", line).unwrap();
        }
        file.flush().unwrap();
        file
    }

    #[test]
    fn extract_turn_analytics_from_output_reads_session_and_cache_tokens() {
        // #1918: input/cache_read/cache_create reflect the LAST assistant
        // message's per-call usage (current context occupancy) rather than the
        // turn-cumulative `result.usage` block. output_tokens stays cumulative
        // across assistant messages.
        let file = write_jsonl(&[
            r#"{"type":"system","subtype":"init","session_id":"session-init"}"#,
            r#"{"type":"assistant","message":{"model":"claude-sonnet","usage":{"input_tokens":10,"cache_creation_input_tokens":3,"cache_read_input_tokens":4,"output_tokens":2},"content":[{"type":"text","text":"partial"}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"session-final","usage":{"input_tokens":100,"cache_creation_input_tokens":20,"cache_read_input_tokens":30,"output_tokens":40},"result":"done"}"#,
        ]);

        let (session_id, usage) =
            extract_turn_analytics_from_output(file.path().to_str().unwrap(), 0);

        assert_eq!(session_id.as_deref(), Some("session-final"));
        assert_eq!(
            usage,
            Some(crate::db::turns::TurnTokenUsage {
                input_tokens: 10,
                cache_create_tokens: 3,
                cache_read_tokens: 4,
                output_tokens: 2,
            })
        );
    }

    #[test]
    fn extract_turn_analytics_from_output_falls_back_to_result_usage_when_no_per_message() {
        // #1918: Qwen-style streams emit token counts only on the terminal
        // result event. The fallback path must honour result.usage when no
        // assistant message reported per-call usage.
        let file = write_jsonl(&[
            r#"{"type":"system","subtype":"init","session_id":"session-init"}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"partial"}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"session-final","usage":{"input_tokens":100,"cache_creation_input_tokens":20,"cache_read_input_tokens":30,"output_tokens":40},"result":"done"}"#,
        ]);

        let (session_id, usage) =
            extract_turn_analytics_from_output(file.path().to_str().unwrap(), 0);

        assert_eq!(session_id.as_deref(), Some("session-final"));
        assert_eq!(
            usage,
            Some(crate::db::turns::TurnTokenUsage {
                input_tokens: 100,
                cache_create_tokens: 20,
                cache_read_tokens: 30,
                output_tokens: 40,
            })
        );
    }

    #[test]
    fn extract_turn_analytics_keeps_last_call_input_and_cumulative_output() {
        // #1918: across multi-call turns, input/cache_* should converge on
        // the most recent assistant call's prompt (not summed) while
        // output_tokens accumulates.
        let file = write_jsonl(&[
            r#"{"type":"system","subtype":"init","session_id":"session-init"}"#,
            r#"{"type":"assistant","message":{"model":"claude-sonnet","usage":{"input_tokens":10,"cache_creation_input_tokens":3,"cache_read_input_tokens":4,"output_tokens":2},"content":[{"type":"text","text":"first"}]}}"#,
            r#"{"type":"assistant","message":{"model":"claude-sonnet","usage":{"input_tokens":50,"cache_creation_input_tokens":7,"cache_read_input_tokens":80,"output_tokens":5},"content":[{"type":"text","text":"second"}]}}"#,
            r#"{"type":"result","subtype":"success","session_id":"session-final","usage":{"input_tokens":60,"cache_creation_input_tokens":10,"cache_read_input_tokens":84,"output_tokens":7},"result":"done"}"#,
        ]);

        let (_session_id, usage) =
            extract_turn_analytics_from_output(file.path().to_str().unwrap(), 0);
        assert_eq!(
            usage,
            Some(crate::db::turns::TurnTokenUsage {
                input_tokens: 50,
                cache_create_tokens: 7,
                cache_read_tokens: 80,
                output_tokens: 7,
            })
        );
    }

    #[test]
    fn recovery_text_then_tool_then_result_prefers_result() {
        // Text -> ToolUse -> Done(result): pre-tool narration should be replaced
        let file = write_jsonl(&[
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"이슈를 생성합니다."}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"echo hi"}}]}}"#,
            r#"{"type":"result","subtype":"success","result":"이슈 #42를 생성했습니다."}"#,
        ]);
        let resp = extract_response_from_output_pub(file.path().to_str().unwrap(), 0);
        assert_eq!(resp, "이슈 #42를 생성했습니다.");
    }

    #[test]
    fn recovery_text_only_returns_text() {
        let file = write_jsonl(&[
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"안녕하세요"}]}}"#,
            r#"{"type":"result","subtype":"success","result":"done"}"#,
        ]);
        let resp = extract_response_from_output_pub(file.path().to_str().unwrap(), 0);
        assert_eq!(resp, "안녕하세요");
    }

    #[test]
    fn recovery_mixed_text_tool_in_single_block_prefers_result() {
        // Single assistant message with [text, tool_use] — text is pre-tool narration
        let file = write_jsonl(&[
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"작업 시작"},{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"result","subtype":"success","result":"완료했습니다."}"#,
        ]);
        let resp = extract_response_from_output_pub(file.path().to_str().unwrap(), 0);
        assert_eq!(resp, "완료했습니다.");
    }

    #[test]
    fn recovery_tool_then_post_tool_text_keeps_text() {
        // Text -> ToolUse -> post-tool Text: should keep accumulated text
        let file = write_jsonl(&[
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"시작합니다."}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"결과를 확인했습니다."}]}}"#,
            r#"{"type":"result","subtype":"success","result":"done"}"#,
        ]);
        let resp = extract_response_from_output_pub(file.path().to_str().unwrap(), 0);
        assert_eq!(resp, "시작합니다.결과를 확인했습니다.");
    }

    #[test]
    fn recovery_empty_response_uses_result() {
        let file =
            write_jsonl(&[r#"{"type":"result","subtype":"success","result":"결과만 있음"}"#]);
        let resp = extract_response_from_output_pub(file.path().to_str().unwrap(), 0);
        assert_eq!(resp, "결과만 있음");
    }

    #[test]
    fn recovery_error_result_not_used() {
        // Error results should not override text
        let file = write_jsonl(&[
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"진행 중"}]}}"#,
            r#"{"type":"result","subtype":"error","is_error":true,"result":"crash"}"#,
        ]);
        let resp = extract_response_from_output_pub(file.path().to_str().unwrap(), 0);
        assert_eq!(resp, "진행 중");
    }

    #[test]
    fn recovery_respects_start_offset() {
        // Only data after offset should be considered
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let line1 =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"이전 턴"}]}}"#;
        writeln!(file, "{}", line1).unwrap();
        let offset = file.as_file().metadata().unwrap().len();
        let line2 =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"새 턴"}]}}"#;
        writeln!(file, "{}", line2).unwrap();
        file.flush().unwrap();

        let resp = extract_response_from_output_pub(file.path().to_str().unwrap(), offset);
        assert_eq!(resp, "새 턴");
    }

    // ========== output_has_result_after_offset: error result tests ==========

    #[test]
    fn error_result_not_treated_as_completion() {
        let file = write_jsonl(&[
            r#"{"type":"result","subtype":"error","is_error":true,"errors":["crash"]}"#,
        ]);
        assert!(!output_has_result_after_offset(
            file.path().to_str().unwrap(),
            0
        ));
    }

    #[test]
    fn success_result_treated_as_completion() {
        let file = write_jsonl(&[r#"{"type":"result","subtype":"success","result":"done"}"#]);
        assert!(output_has_result_after_offset(
            file.path().to_str().unwrap(),
            0
        ));
    }

    #[test]
    fn error_result_before_success_still_completes() {
        // Error followed by success — the success should be detected
        let file = write_jsonl(&[
            r#"{"type":"result","subtype":"error","is_error":true,"errors":["retry"]}"#,
            r#"{"type":"result","subtype":"success","result":"ok"}"#,
        ]);
        assert!(output_has_result_after_offset(
            file.path().to_str().unwrap(),
            0
        ));
    }

    #[test]
    fn stale_synthetic_rebind_inflight_is_replaceable() {
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            0,
            0,
            0,
            "/api/inflight/rebind".to_string(),
            None,
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.input".to_string()),
            0,
        );
        state.rebind_origin = true;

        assert!(can_replace_stale_rebind_inflight(&state));

        state.full_response = "partial".to_string();
        assert!(!can_replace_stale_rebind_inflight(&state));

        state.full_response.clear();
        state.last_watcher_relayed_offset = Some(10);
        assert!(!can_replace_stale_rebind_inflight(&state));
    }

    #[test]
    fn rebind_resumes_existing_real_inflight_when_no_output_was_relayed() {
        let state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "real user input".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.input".to_string()),
            64,
        );

        assert!(can_resume_existing_rebind_inflight(&state));
        assert_eq!(
            recovery_phase_for_existing_inflight_rebind(&state),
            RecoveryPhase::InflightRestore
        );
    }

    #[test]
    fn rebind_keeps_conflict_for_existing_real_inflight_after_output_started() {
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            123,
            456,
            789,
            "real user input".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.input".to_string()),
            64,
        );
        state.full_response = "partial".to_string();

        assert!(!can_resume_existing_rebind_inflight(&state));
        assert_eq!(
            recovery_phase_for_existing_inflight_rebind(&state),
            RecoveryPhase::Pending
        );

        state.full_response.clear();
        state.last_watcher_relayed_offset = Some(128);

        assert!(!can_resume_existing_rebind_inflight(&state));
        assert_eq!(
            recovery_phase_for_existing_inflight_rebind(&state),
            RecoveryPhase::Pending
        );
    }

    #[tokio::test]
    async fn reregister_active_turn_from_inflight_is_idempotent() {
        let shared = super::super::make_shared_data_for_tests();
        let state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            4242,
            Some("adk-cdx-t4242".to_string()),
            7,
            9001,
            9002,
            "continue streaming".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-codex-adk-cdx-t4242".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.input".to_string()),
            64,
        );
        let channel_id = ChannelId::new(state.channel_id);

        assert!(reregister_active_turn_from_inflight(&shared, &state).await);
        assert!(super::super::mailbox_has_active_turn(&shared, channel_id).await);

        assert!(reregister_active_turn_from_inflight(&shared, &state).await);
        let snapshot = super::super::mailbox_snapshot(&shared, channel_id).await;
        assert_eq!(
            snapshot.active_user_message_id,
            Some(MessageId::new(state.user_msg_id))
        );
        let token = snapshot
            .cancel_token
            .expect("reregistered active turn must expose a cancel token");
        assert_eq!(
            token.tmux_session.lock().unwrap().as_deref(),
            state.tmux_session_name.as_deref(),
            "inflight reregister must not leave a naked cancel token"
        );
    }

    #[cfg(unix)]
    #[test]
    fn rebind_adopts_detected_output_path_when_inode_differs() {
        let dir = tempfile::tempdir().unwrap();
        let fallback = dir.path().join("agentdesk-session.jsonl");
        let rotated = dir.path().join("agentdesk-session.jsonl.stale");
        std::fs::write(&fallback, b"fresh\n").unwrap();
        std::fs::write(&rotated, b"stale-file-content\n").unwrap();
        let rotated_inode = std::fs::metadata(&rotated).unwrap().ino();

        let detected = detect_rebind_output_path_from_candidates(
            fallback.to_str().unwrap(),
            vec![LsofOutputCandidate {
                fd: "3w".to_string(),
                raw_path: rotated.to_string_lossy().to_string(),
                inode: Some(rotated_inode),
            }],
        )
        .expect("candidate resolution should succeed")
        .expect("mismatched inode candidate must be adopted");

        assert_eq!(detected.path, rotated.to_string_lossy());
        assert_eq!(
            detected.initial_offset,
            std::fs::metadata(&rotated).unwrap().len()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rebind_rejects_deleted_fd_even_if_pathname_now_points_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let fallback = dir.path().join("agentdesk-session.jsonl");
        std::fs::write(&fallback, b"replacement-file\n").unwrap();

        let stale = detect_rebind_output_path_from_candidates(
            fallback.to_str().unwrap(),
            vec![LsofOutputCandidate {
                fd: "5w".to_string(),
                raw_path: format!("{} (deleted)", fallback.display()),
                inode: Some(99_001),
            }],
        )
        .expect_err("deleted live fd must not silently rebind the replacement pathname");

        assert_eq!(
            stale,
            StaleOutputCandidate {
                fd: "5w".to_string(),
                raw_path: format!("{} (deleted)", fallback.display()),
                inode: Some(99_001),
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn rebind_rejects_candidate_when_lsof_inode_disagrees_with_current_path_inode() {
        let dir = tempfile::tempdir().unwrap();
        let fallback = dir.path().join("agentdesk-session.jsonl");
        std::fs::write(&fallback, b"replacement-file\n").unwrap();
        let actual_inode = std::fs::metadata(&fallback).unwrap().ino();

        let stale = detect_rebind_output_path_from_candidates(
            fallback.to_str().unwrap(),
            vec![LsofOutputCandidate {
                fd: "7w".to_string(),
                raw_path: fallback.to_string_lossy().to_string(),
                inode: Some(actual_inode + 1),
            }],
        )
        .expect_err("inode mismatch means lsof points at a different live fd than the pathname");

        assert_eq!(stale.fd, "7w");
        assert_eq!(stale.raw_path, fallback.to_string_lossy());
        assert_eq!(stale.inode, Some(actual_inode + 1));
    }

    #[cfg(unix)]
    #[test]
    fn rebind_prefers_live_repair_candidate_over_deleted_fd() {
        let dir = tempfile::tempdir().unwrap();
        let fallback = dir.path().join("agentdesk-session.jsonl");
        let rotated = dir.path().join("agentdesk-session.jsonl.1");
        std::fs::write(&fallback, b"replacement-file\n").unwrap();
        std::fs::write(&rotated, b"rotated-live-file\n").unwrap();
        let rotated_inode = std::fs::metadata(&rotated).unwrap().ino();

        let detected = detect_rebind_output_path_from_candidates(
            fallback.to_str().unwrap(),
            vec![
                LsofOutputCandidate {
                    fd: "5w".to_string(),
                    raw_path: format!("{} (deleted)", fallback.display()),
                    inode: Some(90_001),
                },
                LsofOutputCandidate {
                    fd: "7w".to_string(),
                    raw_path: rotated.to_string_lossy().to_string(),
                    inode: Some(rotated_inode),
                },
            ],
        )
        .expect("repairable live candidate should win over stale deleted fd")
        .expect("rebind should adopt the true live output file");

        assert_eq!(detected.path, rotated.to_string_lossy());
        assert_eq!(
            detected.initial_offset,
            std::fs::metadata(&rotated).unwrap().len()
        );
    }

    #[cfg(unix)]
    #[test]
    fn rebind_keeps_fallback_when_live_fd_matches_current_path_identity() {
        let dir = tempfile::tempdir().unwrap();
        let fallback = dir.path().join("agentdesk-session.jsonl");
        std::fs::write(&fallback, b"current-file\n").unwrap();
        let current_inode = std::fs::metadata(&fallback).unwrap().ino();

        let detected = detect_rebind_output_path_from_candidates(
            fallback.to_str().unwrap(),
            vec![LsofOutputCandidate {
                fd: "9w".to_string(),
                raw_path: fallback.to_string_lossy().to_string(),
                inode: Some(current_inode),
            }],
        )
        .expect("matching live fd should not fail");

        assert!(
            detected.is_none(),
            "when lsof fd/inode still matches the canonical path, recovery should keep the fallback"
        );
    }

    #[tokio::test]
    async fn persist_recovered_transcript_stores_dispatch_evidence() {
        let db = crate::db::test_db();
        let state = inflight::InflightTurnState {
            version: 1,
            provider: "codex".to_string(),
            channel_id: 1486333430516945008,
            channel_name: Some("adk-cdx-t1486333430516945008".to_string()),
            logical_channel_id: Some(1479671301387059200),
            thread_id: Some(1486333430516945008),
            thread_title: Some("[AgentDesk] #558 token audit".to_string()),
            request_owner_user_id: 343742347365974026,
            user_msg_id: 1487795113240559788,
            status_message_id: None,
            current_msg_id: 1487799916758827138,
            current_msg_len: 0,
            user_text: "릴리즈하다가 응답이 끊겼어. 이어서 설명해줘.".to_string(),
            source: crate::dispatch::Source::Text,
            session_id: Some("session-1".to_string()),
            tmux_session_name: Some("AgentDesk-codex-adk-cdx-t1486333430516945008".to_string()),
            output_path: Some("/tmp/agentdesk-test.jsonl".to_string()),
            input_fifo_path: Some("/tmp/agentdesk-test.input".to_string()),
            runtime_kind: None,
            worktree_path: None,
            worktree_branch: None,
            base_commit: None,
            last_offset: 123,
            turn_start_offset: Some(123),
            full_response: "중간까지 정리했습니다.".to_string(),
            response_sent_offset: 0,
            current_tool_line: None,
            last_tool_name: None,
            last_tool_summary: None,
            prev_tool_status: None,
            task_notification_kind: None,
            started_at: "2026-03-29 22:00:34".to_string(),
            updated_at: "2026-03-29 22:03:53".to_string(),
            born_generation: 7,
            any_tool_used: true,
            has_post_tool_text: false,
            session_key: Some("host:tmux-1".to_string()),
            delivery_bot: None,
            silent_turn: false,
            dispatch_id: Some("dispatch-from-state".to_string()),
            last_watcher_relayed_offset: None,
            last_watcher_relayed_generation_mtime_ns: None,
            restart_mode: None,
            restart_generation: None,
            rebind_origin: false,
            long_running_placeholder_active: false,
            watcher_owns_live_relay: false,
        };

        assert!(
            persist_recovered_transcript(
                Some(&db),
                None,
                &ProviderKind::Codex,
                &state,
                Some("dispatch-from-recovery"),
                "  이미 확인한 내용은 여기까지입니다.  "
            )
            .await
        );

        let turn_id = format!("discord:{}:{}", state.channel_id, state.user_msg_id);
        let conn = db.read_conn().unwrap();
        let (
            session_key,
            channel_id,
            provider,
            dispatch_id,
            user_message,
            assistant_message,
        ): (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
            String,
            String,
        ) = conn
            .query_row(
                "SELECT session_key, channel_id, provider, dispatch_id, user_message, assistant_message \
                 FROM session_transcripts WHERE turn_id = ?1",
                [turn_id.as_str()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();

        assert_eq!(session_key.as_deref(), Some("host:tmux-1"));
        assert_eq!(channel_id.as_deref(), Some("1486333430516945008"));
        assert_eq!(provider.as_deref(), Some("codex"));
        assert_eq!(
            dispatch_id.as_deref(),
            Some("dispatch-from-recovery"),
            "explicit recovery dispatch evidence must win over stale inflight state"
        );
        assert_eq!(user_message, "릴리즈하다가 응답이 끊겼어. 이어서 설명해줘.");
        assert_eq!(assistant_message, "이미 확인한 내용은 여기까지입니다.");
    }

    #[test]
    fn missing_session_recovery_does_not_save_handoff_for_followup_turn() {
        let _lock = super::super::runtime_store::lock_test_env();
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("agentdesk-root");
        std::fs::create_dir_all(root.join("runtime")).unwrap();

        struct EnvReset;
        impl Drop for EnvReset {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
            }
        }

        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", &root) };
        let _reset = EnvReset;

        let state = crate::services::discord::inflight::InflightTurnState {
            version: 1,
            provider: "codex".to_string(),
            channel_id: 1486333430516945008,
            channel_name: Some("adk-cdx-t1486333430516945008".to_string()),
            logical_channel_id: Some(1479671301387059200),
            thread_id: Some(1486333430516945008),
            thread_title: Some("[AgentDesk] #558 token audit".to_string()),
            request_owner_user_id: 343742347365974026,
            user_msg_id: 1487795113240559788,
            status_message_id: None,
            current_msg_id: 1487799916758827138,
            current_msg_len: 0,
            user_text: "릴리즈하다가 응답이 끊겼어. 이어서 설명해줘.".to_string(),
            source: crate::dispatch::Source::Text,
            session_id: Some("session-1".to_string()),
            tmux_session_name: Some("AgentDesk-codex-adk-cdx-t1486333430516945008".to_string()),
            output_path: Some("/tmp/agentdesk-test.jsonl".to_string()),
            input_fifo_path: Some("/tmp/agentdesk-test.input".to_string()),
            runtime_kind: None,
            worktree_path: None,
            worktree_branch: None,
            base_commit: None,
            last_offset: 123,
            turn_start_offset: Some(123),
            full_response: "중간까지 정리했습니다.".to_string(),
            response_sent_offset: 0,
            current_tool_line: None,
            last_tool_name: None,
            last_tool_summary: None,
            prev_tool_status: None,
            task_notification_kind: None,
            started_at: "2026-03-29 22:00:34".to_string(),
            updated_at: "2026-03-29 22:03:53".to_string(),
            born_generation: 7,
            any_tool_used: true,
            has_post_tool_text: false,
            session_key: None,
            delivery_bot: None,
            silent_turn: false,
            dispatch_id: None,
            last_watcher_relayed_offset: None,
            last_watcher_relayed_generation_mtime_ns: None,
            restart_mode: None,
            restart_generation: None,
            rebind_origin: false,
            long_running_placeholder_active: false,
            watcher_owns_live_relay: false,
        };

        save_missing_session_handoff(
            &ProviderKind::Codex,
            &state,
            "이미 확인한 내용은 여기까지입니다. 이어서 원인과 대응을 설명하겠습니다.",
        );

        let handoffs = crate::services::discord::handoff::load_handoffs(&ProviderKind::Codex);
        assert!(
            handoffs.is_empty(),
            "automatic post-restart handoff files must no longer be created"
        );
    }

    #[test]
    fn planned_restart_missing_session_uses_restart_specific_message() {
        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            ProviderKind::Codex,
            1486333430516945008,
            Some("adk-cdx-t1486333430516945008".to_string()),
            343742347365974026,
            1487795113240559788,
            1487799916758827138,
            "릴리즈하다가 응답이 끊겼어. 이어서 설명해줘.".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-codex-adk-cdx-t1486333430516945008".to_string()),
            Some("/tmp/agentdesk-test.jsonl".to_string()),
            Some("/tmp/agentdesk-test.input".to_string()),
            123,
        );
        state.restart_mode = Some(crate::services::discord::InflightRestartMode::DrainRestart);
        state.restart_generation = Some(7);

        let text = interrupted_recovery_message(&state, "");
        assert!(text.contains("dcserver 재시작"));
        assert!(!text.contains("이어붙이지 못했습니다"));
    }

    #[test]
    fn recovery_watcher_resume_uses_saved_offset_when_file_grew() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "before").unwrap();
        let saved_offset = file.as_file().metadata().unwrap().len();
        writeln!(file, "after").unwrap();
        file.flush().unwrap();

        let (offset, current_len, truncated) =
            recovery_watcher_start_offset(file.path().to_str().unwrap(), saved_offset);
        assert_eq!(offset, saved_offset);
        assert!(current_len >= saved_offset);
        assert!(!truncated);
    }

    #[test]
    fn recovery_watcher_resume_rewinds_when_file_was_truncated() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "new session output").unwrap();
        file.flush().unwrap();

        let saved_offset = file.as_file().metadata().unwrap().len() + 100;
        let (offset, current_len, truncated) =
            recovery_watcher_start_offset(file.path().to_str().unwrap(), saved_offset);
        assert_eq!(offset, 0);
        assert!(current_len < saved_offset);
        assert!(truncated);
    }

    #[test]
    fn recovery_spawn_adk_cwd_prefers_inflight_worktree() {
        let temp = tempfile::tempdir().unwrap();
        let worktree_path = temp.path().join("wt");
        std::fs::create_dir_all(&worktree_path).unwrap();

        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx-t42".to_string()),
            1,
            2,
            3,
            "resume".to_string(),
            Some("session-42".to_string()),
            Some("AgentDesk-codex-adk-cdx-t42".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        state.worktree_path = Some(worktree_path.to_string_lossy().to_string());

        let resolved = recovery_spawn_adk_cwd(&state, Some("/tmp/main-workspace".to_string()))
            .expect("existing inflight worktree must win over persisted main workspace cwd");
        assert_eq!(
            resolved.as_deref(),
            Some(worktree_path.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn recovery_spawn_adk_cwd_uses_legacy_session_path_without_inflight_worktree() {
        let state = crate::services::discord::inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx-t42".to_string()),
            1,
            2,
            3,
            "resume".to_string(),
            Some("session-42".to_string()),
            Some("AgentDesk-codex-adk-cdx-t42".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );

        let resolved = recovery_spawn_adk_cwd(&state, Some("/tmp/main-workspace".to_string()))
            .expect("legacy recovery should preserve the persisted session cwd");
        assert_eq!(resolved.as_deref(), Some("/tmp/main-workspace"));
    }

    #[test]
    fn recovery_spawn_adk_cwd_blocks_missing_inflight_worktree() {
        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx-t42".to_string()),
            1,
            2,
            3,
            "resume".to_string(),
            Some("session-42".to_string()),
            Some("AgentDesk-codex-adk-cdx-t42".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        state.worktree_path = Some("/tmp/missing-worktree".to_string());

        let error = recovery_spawn_adk_cwd(&state, Some("/tmp/main-workspace".to_string()))
            .expect_err("missing inflight worktree must block main-workspace fallback");
        assert!(error.contains("missing-worktree"));
        assert!(error.contains("recovery blocked"));
    }

    #[test]
    fn recovery_spawn_adk_cwd_allows_dispatch_fallback_without_worktree_metadata() {
        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx-t42".to_string()),
            1,
            2,
            3,
            "DISPATCH:dispatch-42 resume".to_string(),
            Some("session-42".to_string()),
            Some("AgentDesk-codex-adk-cdx-t42".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        state.dispatch_id = Some("dispatch-42".to_string());

        let resolved = recovery_spawn_adk_cwd(&state, Some("/tmp/dispatch-target".to_string()))
            .expect("dispatch fallback cwd must remain valid when no worktree was tracked");
        assert_eq!(resolved.as_deref(), Some("/tmp/dispatch-target"));
    }

    #[test]
    fn recovery_spawn_adk_cwd_blocks_missing_required_worktree_metadata_for_dispatch_turn() {
        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx-t42".to_string()),
            1,
            2,
            3,
            "DISPATCH:dispatch-42 resume".to_string(),
            Some("session-42".to_string()),
            Some("AgentDesk-codex-adk-cdx-t42".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        state.dispatch_id = Some("dispatch-42".to_string());
        state.worktree_branch = Some("agentdesk/codex/adk-cdx-t42".to_string());

        let error = recovery_spawn_adk_cwd(&state, Some("/tmp/main-workspace".to_string()))
            .expect_err("dispatch recovery without recorded worktree path must not fall back");
        assert!(error.contains("dispatch-42"));
        assert!(error.contains("worktree state missing"));
    }

    #[test]
    fn recovery_restores_worktree_identity_into_session_state() {
        fn run_git(repo_path: &std::path::Path, args: &[&str]) -> String {
            let output = GitCommand::new()
                .repo(repo_path)
                .args(args)
                .run_output()
                .unwrap_or_else(|error| panic!("git {:?} failed to spawn: {error}", args));
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }

        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "-b", "main"]);
        run_git(repo.path(), &["config", "user.name", "AgentDesk"]);
        run_git(
            repo.path(),
            &["config", "user.email", "agentdesk@example.com"],
        );
        std::fs::write(repo.path().join("README.md"), "seed\n").unwrap();
        run_git(repo.path(), &["add", "README.md"]);
        run_git(repo.path(), &["commit", "-m", "seed"]);

        let worktree_path = repo.path().join("wt-recovery");
        let branch = "wt/recovery-936";
        run_git(
            repo.path(),
            &[
                "worktree",
                "add",
                "-b",
                branch,
                worktree_path.to_str().unwrap(),
                "main",
            ],
        );

        let mut state = crate::services::discord::inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx-t42".to_string()),
            1,
            2,
            3,
            "resume".to_string(),
            Some("session-42".to_string()),
            Some("AgentDesk-codex-adk-cdx-t42".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        state.worktree_path = Some(worktree_path.to_string_lossy().to_string());
        state.worktree_branch = Some(branch.to_string());

        let mut session = DiscordSession {
            session_id: None,
            memento_context_loaded: false,
            memento_reflected: false,
            current_path: Some(worktree_path.to_string_lossy().to_string()),
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            remote_profile_name: None,
            channel_id: Some(42),
            channel_name: Some("adk-cdx-t42".to_string()),
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: 0,
            assistant_turns: 0,
        };

        restore_recovered_session_worktree(&mut session, &state);

        let restored = session
            .worktree
            .as_ref()
            .expect("recovery should rebuild worktree identity from inflight metadata");
        let canonical_repo_path = std::fs::canonicalize(repo.path()).unwrap();
        assert_eq!(restored.worktree_path, worktree_path.to_string_lossy());
        assert_eq!(restored.branch_name, branch);
        assert_eq!(
            restored.original_path,
            canonical_repo_path.to_string_lossy()
        );
    }
    #[test]
    fn fast_path_captured_full_response_requires_unsent_response() {
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            111,
            222,
            333,
            "user input".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        state.full_response = "captured response".to_string();

        assert!(
            !can_fast_path_captured_full_response(&state, false),
            "captured output alone is not authoritative completion evidence"
        );
        assert!(can_fast_path_captured_full_response(&state, true));

        state.response_sent_offset = 1;
        assert!(
            !can_fast_path_captured_full_response(&state, true),
            "partially-sent responses must stay on the existing path"
        );
    }

    #[test]
    fn fast_path_captured_full_response_skips_watcher_relayed_output() {
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            111,
            222,
            333,
            "user input".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        state.full_response = "captured response".to_string();
        state.last_watcher_relayed_offset = Some(128);

        assert!(
            !can_fast_path_captured_full_response(&state, true),
            "watcher-relayed inflights must not duplicate-send on restart"
        );
    }

    #[test]
    fn fast_path_captured_full_response_skips_rebind_origin() {
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            0,
            0,
            0,
            "/api/inflight/rebind".to_string(),
            None,
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        state.rebind_origin = true;
        state.full_response = "captured response".to_string();

        assert!(!can_fast_path_captured_full_response(&state, true));
    }

    #[test]
    fn fast_path_captured_full_response_skips_empty_response() {
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx".to_string()),
            111,
            222,
            333,
            "user input".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );
        state.full_response = "   \n\t".to_string();

        assert!(!can_fast_path_captured_full_response(&state, true));
    }

    #[tokio::test]
    async fn restore_inflight_turns_fast_path_relays_captured_response_and_clears_inflight() {
        let _lock = super::super::runtime_store::lock_test_env();
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("agentdesk-root");
        std::fs::create_dir_all(root.join("runtime")).unwrap();
        let _root_reset = EnvVarReset::set("AGENTDESK_ROOT_DIR", &root);
        let output_path = temp.path().join("agentdesk-test.jsonl");
        std::fs::write(
            &output_path,
            concat!(
                "{\"type\":\"assistant\",\"message\":\"preface\"}\n",
                "{\"type\":\"result\",\"result\":\"done\"}\n"
            ),
        )
        .unwrap();

        let db = crate::db::test_db();
        let shared = super::super::make_shared_data_for_tests_with_storage(Some(db.clone()), None);
        let (base_url, discord_state, server_handle) = spawn_mock_recovery_discord_server().await;
        let http = Arc::new(
            serenity::http::HttpBuilder::new("test-token")
                .proxy(base_url)
                .ratelimiter_disabled(true)
                .build(),
        );

        let channel_id = 1486333430516945008u64;
        let user_msg_id = 1487795113240559788u64;
        let current_msg_id = 1487799916758827138u64;
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("adk-cdx".to_string()),
            343742347365974026,
            user_msg_id,
            current_msg_id,
            "릴리즈하다가 응답이 끊겼어. 이어서 설명해줘.".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some(output_path.to_string_lossy().into_owned()),
            Some("/tmp/agentdesk-test.input".to_string()),
            0,
        );
        state.full_response =
            "이미 확인한 내용은 여기까지입니다. 이어서 원인과 대응을 설명하겠습니다.".to_string();
        state.response_sent_offset = 0;
        super::super::inflight::save_inflight_state(&state).unwrap();

        restore_inflight_turns(&http, &shared, &ProviderKind::Codex).await;
        server_handle.abort();

        assert!(
            super::super::inflight::load_inflight_state(&ProviderKind::Codex, channel_id).is_none(),
            "fast-path completion must clear the saved inflight state"
        );

        let discord_state = discord_state.lock().unwrap();
        assert_eq!(discord_state.edited_messages.len(), 1);
        assert!(discord_state.posted_messages.is_empty());
        assert_eq!(discord_state.edited_messages[0].0, channel_id.to_string());
        assert_eq!(
            discord_state.edited_messages[0].1,
            current_msg_id.to_string()
        );
        assert_eq!(discord_state.edited_messages[0].2, state.full_response);
        assert_eq!(discord_state.reaction_paths.len(), 2);
        drop(discord_state);

        let turn_id = format!("discord:{channel_id}:{user_msg_id}");
        let conn = db.read_conn().unwrap();
        let assistant_message: Option<String> = conn
            .query_row(
                "SELECT assistant_message FROM session_transcripts WHERE turn_id = ?1",
                [turn_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(
            assistant_message.as_deref(),
            Some("이미 확인한 내용은 여기까지입니다. 이어서 원인과 대응을 설명하겠습니다.")
        );
    }

    #[tokio::test]
    async fn restore_inflight_turns_prefers_saved_dispatch_id_over_reused_thread_lookup() {
        let _lock = super::super::runtime_store::lock_test_env();
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("agentdesk-root");
        std::fs::create_dir_all(root.join("runtime")).unwrap();
        let _root_reset = EnvVarReset::set("AGENTDESK_ROOT_DIR", &root);

        let db = crate::db::test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, latest_dispatch_id, active_thread_id, created_at, updated_at
                 ) VALUES (
                    'card-reuse', 'Reused thread recovery', 'in_progress',
                    'dispatch-new', '1486333430516945008', datetime('now'), datetime('now')
                 )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, dispatch_type, status, title, thread_id, created_at, updated_at
                 ) VALUES (
                    'dispatch-old', 'card-reuse', 'review', 'dispatched',
                    '[Review R1] card-reuse', '1486333430516945008', datetime('now', '-2 minute'), datetime('now', '-2 minute')
                 )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, dispatch_type, status, title, thread_id, created_at, updated_at
                 ) VALUES (
                    'dispatch-new', 'card-reuse', 'review-decision', 'pending',
                    '[리뷰 검토] card-reuse', '1486333430516945008', datetime('now', '-1 minute'), datetime('now', '-1 minute')
                 )",
                [],
            )
            .unwrap();
        }

        let shared = super::super::make_shared_data_for_tests_with_storage(Some(db.clone()), None);
        let (base_url, discord_state, server_handle) = spawn_mock_recovery_discord_server().await;
        let api_handle = spawn_mock_recovery_internal_api_server(MockRecoveryInternalApiState {
            pending_dispatch_id: "dispatch-new".to_string(),
            dispatch_types: std::collections::HashMap::from([
                ("dispatch-old".to_string(), "review".to_string()),
                ("dispatch-new".to_string(), "review-decision".to_string()),
            ]),
        })
        .await;
        let http = Arc::new(
            serenity::http::HttpBuilder::new("test-token")
                .proxy(base_url)
                .ratelimiter_disabled(true)
                .build(),
        );

        let output =
            write_jsonl(&[r#"{"type":"result","subtype":"success","result":"복구 완료"}"#]);
        let channel_id = 1486333430516945008u64;
        let user_msg_id = 1487795113240559788u64;
        let current_msg_id = 1487799916758827138u64;
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("adk-cdx".to_string()),
            343742347365974026,
            user_msg_id,
            current_msg_id,
            "DISPATCH:dispatch-old - original review request".to_string(),
            Some("session-1".to_string()),
            Some("AgentDesk-codex-adk-cdx".to_string()),
            Some(output.path().to_string_lossy().to_string()),
            Some("/tmp/agentdesk-test.input".to_string()),
            0,
        );
        state.full_response = "복구 완료".to_string();
        state.response_sent_offset = 0;
        super::super::inflight::save_inflight_state(&state).unwrap();

        restore_inflight_turns(&http, &shared, &ProviderKind::Codex).await;
        server_handle.abort();
        api_handle.abort();

        assert!(
            super::super::inflight::load_inflight_state(&ProviderKind::Codex, channel_id).is_none(),
            "completed restart recovery should clear the saved inflight state"
        );

        let discord_state = discord_state.lock().unwrap();
        assert_eq!(discord_state.edited_messages.len(), 1);
        assert_eq!(discord_state.edited_messages[0].2, "복구 완료");
        drop(discord_state);

        let turn_id = format!("discord:{channel_id}:{user_msg_id}");
        let conn = db.read_conn().unwrap();
        let dispatch_id: Option<String> = conn
            .query_row(
                "SELECT dispatch_id FROM session_transcripts WHERE turn_id = ?1",
                [turn_id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert_eq!(
            dispatch_id.as_deref(),
            Some("dispatch-old"),
            "saved DISPATCH evidence must win over a newer pending dispatch on the reused thread"
        );
    }
}
