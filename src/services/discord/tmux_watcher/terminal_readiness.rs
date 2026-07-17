//! #3479 Phase-1 rank-2 extraction: the tmux watcher's terminal-readiness +
//! inflight-classification PREDICATES and the pure buffer/message-id reconcilers
//! — the panel/lease eligibility checks, the JSONL `ready_for_input` sentinel
//! probes, the direct-terminal idle-commit predicate, and the suppressed-turn
//! buffer discard. PURE MOVE from `tmux_watcher.rs` (zero logic change) to shrink
//! the frozen root file below its maintainability baseline.
//!
//! These are all synchronous, side-effect-free (or local-`std::fs`-only) helpers
//! with ZERO coupling to `shared`/`http`. The async, `shared`-touching
//! `commit_watcher_direct_terminal_session_idle` sibling that sits BETWEEN these
//! two clusters in the root deliberately STAYS in `tmux_watcher.rs`. Items are
//! `pub(super)` so the parent watcher loop (and the sibling `panel_decisions`
//! module, which calls `watcher_inflight_is_panel_eligible` via the parent's
//! re-export glob) keep calling them by their original names. `InflightTurnState`
//! and the rank-1 `SessionBoundRelayAckTarget` resolve through `use super::*`.

use super::super::RestoredWatcherTurn;
use super::*;
use crate::services::discord::inflight::opt_message_id;
use crate::services::discord::replace_outcome_policy::{
    WatcherSendFailureClass, WatcherTerminalRelayPlan, watcher_partial_continuation_retry_plan,
    watcher_send_failure_retry_plan,
};

pub(super) fn adopt_watcher_terminal_message_ids_from_inflight(
    placeholder_msg_id: &mut Option<serenity::MessageId>,
    placeholder_from_restored_inflight: &mut bool,
    status_panel_msg_id: &mut Option<serenity::MessageId>,
    inflight: &InflightTurnState,
    tmux_session_name: &str,
) {
    if inflight.rebind_origin {
        return;
    }
    let matches_current_watcher_session = inflight
        .tmux_session_name
        .as_deref()
        .map(str::trim)
        .is_some_and(|name| !name.is_empty() && name == tmux_session_name);
    if !matches_current_watcher_session {
        return;
    }
    let placeholderless_discord_turn = inflight.user_msg_id != 0
        && inflight.current_msg_id != 0
        && inflight.current_msg_id == inflight.user_msg_id;
    if placeholderless_discord_turn {
        return;
    }
    if placeholder_msg_id.is_none()
        && let Some(message_id) = opt_message_id(inflight.current_msg_id)
    {
        *placeholder_msg_id = Some(message_id);
        *placeholder_from_restored_inflight = true;
    }
    if status_panel_msg_id.is_none() {
        *status_panel_msg_id =
            crate::services::discord::turn_bridge::normalize_status_panel_message_id(
                inflight.status_message_id.and_then(opt_message_id),
            );
    }
}

pub(super) fn merge_persisted_rollover_frozen_msg_ids(
    local: &mut Vec<serenity::MessageId>,
    inflight: Option<&InflightTurnState>,
    tmux_session_name: &str,
) {
    let Some(inflight) = inflight.filter(|state| {
        state.tmux_session_name.as_deref() == Some(tmux_session_name) && !state.rebind_origin
    }) else {
        return;
    };
    for msg_id in inflight
        .streaming_rollover_frozen_msg_ids
        .iter()
        .copied()
        .filter_map(opt_message_id)
    {
        if !local.contains(&msg_id) {
            local.push(msg_id);
        }
    }
}

pub(super) struct WatcherTerminalRewindSeedInput<'a> {
    pub(super) placeholder_msg_id: Option<serenity::MessageId>,
    pub(super) status_panel_msg_id: Option<serenity::MessageId>,
    pub(super) response_sent_offset: usize,
    pub(super) last_edit_text: &'a str,
    pub(super) task_notification_kind:
        Option<crate::services::agent_protocol::TaskNotificationKind>,
    pub(super) finish_mailbox_on_completion: bool,
    pub(super) injected_prompt_message_id: Option<u64>,
    pub(super) streaming_rollover_frozen_msg_ids: &'a [serenity::MessageId],
}

#[allow(clippy::too_many_arguments)] // thin hot-file wiring seam; fields documented on the input struct
pub(super) fn watcher_terminal_rewind_seed_from_parts(
    placeholder_msg_id: Option<serenity::MessageId>,
    status_panel_msg_id: Option<serenity::MessageId>,
    response_sent_offset: usize,
    last_edit_text: &str,
    task_notification_kind: Option<crate::services::agent_protocol::TaskNotificationKind>,
    finish_mailbox_on_completion: bool,
    injected_prompt_message_id: Option<u64>,
    streaming_rollover_frozen_msg_ids: &[serenity::MessageId],
) -> Option<RestoredWatcherTurn> {
    watcher_terminal_rewind_seed(WatcherTerminalRewindSeedInput {
        placeholder_msg_id,
        status_panel_msg_id,
        response_sent_offset,
        last_edit_text,
        task_notification_kind,
        finish_mailbox_on_completion,
        injected_prompt_message_id,
        streaming_rollover_frozen_msg_ids,
    })
}

pub(super) fn watcher_terminal_rewind_seed(
    input: WatcherTerminalRewindSeedInput<'_>,
) -> Option<RestoredWatcherTurn> {
    input
        .placeholder_msg_id
        .map(|current_msg_id| RestoredWatcherTurn {
            current_msg_id,
            status_message_id: input.status_panel_msg_id,
            response_sent_offset: input.response_sent_offset,
            full_response: String::new(),
            last_edit_text: input.last_edit_text.to_string(),
            task_notification_kind: input.task_notification_kind,
            finish_mailbox_on_completion: input.finish_mailbox_on_completion,
            injected_prompt_message_id: input.injected_prompt_message_id,
            turn_identity: None,
            streaming_rollover_frozen_msg_ids: input.streaming_rollover_frozen_msg_ids.to_vec(),
            same_turn_rewind: true,
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FreshIdleSessionBoundRetryPlan {
    pub(super) turn_start_offset: u64,
    pub(super) retry_offset: u64,
}

pub(super) fn watcher_fresh_idle_session_bound_retry_plan(
    pinned_pre_cleanup_inflight: Option<&InflightTurnState>,
    tmux_session_name: &str,
    current_offset: u64,
    fresh_idle_effective_committed_offset: u64,
) -> Option<FreshIdleSessionBoundRetryPlan> {
    pinned_pre_cleanup_inflight.and_then(|state| {
        let turn_start_offset = state.turn_start_offset?;
        (state.tmux_session_name.as_deref() == Some(tmux_session_name)
            && turn_start_offset < current_offset
            && state.effective_relay_owner_kind()
                == crate::services::discord::inflight::RelayOwnerKind::SessionBoundRelay
            && !state.session_bound_delivered
            && fresh_idle_effective_committed_offset < current_offset)
            .then_some(FreshIdleSessionBoundRetryPlan {
                turn_start_offset,
                retry_offset: turn_start_offset.max(fresh_idle_effective_committed_offset),
            })
    })
}

pub(super) fn watcher_wait_inflight_retry_plan() -> WatcherTerminalRelayPlan {
    watcher_partial_continuation_retry_plan()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WatcherRewindAttemptKey {
    // The rewind cap state is task-local in the watcher loop, so watcher
    // instance IDs are reborn with the state and cannot discriminate production
    // equality. Per-turn separation comes from turn_data_start_offset because
    // the buffered output start offset advances when the watcher moves to the
    // next terminal turn.
    turn_data_start_offset: u64,
    user_msg_id: u64,
    started_at: Option<String>,
    inflight_turn_start_offset: Option<u64>,
}

pub(super) fn watcher_rewind_attempt_key(
    turn_data_start_offset: u64,
    identity: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
) -> WatcherRewindAttemptKey {
    WatcherRewindAttemptKey {
        turn_data_start_offset,
        user_msg_id: identity.map(|identity| identity.user_msg_id).unwrap_or(0),
        started_at: identity.map(|identity| identity.started_at.clone()),
        inflight_turn_start_offset: identity.and_then(|identity| identity.turn_start_offset),
    }
}

pub(super) fn reset_rewind_attempts(
    terminal_rewind_attempt_key: &mut Option<WatcherRewindAttemptKey>,
    terminal_rewind_attempts: &mut u8,
    key: WatcherRewindAttemptKey,
) {
    if terminal_rewind_attempt_key.as_ref() != Some(&key) {
        *terminal_rewind_attempt_key = Some(key);
        *terminal_rewind_attempts = 0;
    }
}

pub(super) fn take_pending_or_restored_rewind_seed(
    pending_terminal_rewind_seed: &mut Option<RestoredWatcherTurn>,
    restored_turn: &mut Option<RestoredWatcherTurn>,
) -> Option<RestoredWatcherTurn> {
    pending_terminal_rewind_seed
        .take()
        .or_else(|| restored_turn.take())
}

pub(super) fn restored_seed_from_rewind(seed: Option<&RestoredWatcherTurn>) -> bool {
    seed.is_some_and(|seed| seed.same_turn_rewind)
}

pub(super) fn watcher_rollback_anchor_msg_id(
    prompt_anchor_reference: Option<&(serenity::ChannelId, serenity::MessageId)>,
    watcher_lease_user_msg_id: u64,
    watcher_lease_start: u64,
) -> serenity::MessageId {
    prompt_anchor_reference
        .map(|(_, message_id)| *message_id)
        .unwrap_or_else(|| {
            serenity::MessageId::new(watcher_lease_user_msg_id.max(watcher_lease_start.max(1)))
        })
}

/// Which watcher send-failure arm is asking for a no-rewind WARN.
pub(super) enum WatcherNoRewindWarnSite {
    Partial,
    EditFull,
    PlaceholderlessFull,
}

pub(super) fn stripped_send_error(message: &str) -> &str {
    crate::services::discord::replace_outcome_policy::strip_watcher_send_failure_class_marker(
        message,
    )
}

pub(super) fn info_watcher_failed_relay(error: impl std::fmt::Display) {
    let ts = chrono::Local::now().format("%H:%M:%S");
    let error = error.to_string();
    let error = stripped_send_error(&error);
    tracing::info!("  [{ts}] 👁 Failed to relay: {error}");
}

/// Resolve the retry plan for a classified send failure and, when the class is
/// non-retryable (no rewind), emit the arm-appropriate WARN. Single call-site
/// seam so the hot watcher loop carries one call per failure arm.
pub(super) fn watcher_send_failure_plan_warned(
    failure_class: WatcherSendFailureClass,
    site: WatcherNoRewindWarnSite,
    watcher_provider: &crate::services::provider::ProviderKind,
    channel_id: serenity::ChannelId,
    tmux_session_name: &str,
    error: impl std::fmt::Display,
) -> crate::services::discord::replace_outcome_policy::WatcherTerminalRelayPlan {
    let plan = watcher_send_failure_retry_plan(failure_class);
    if !plan.retry_offset {
        match site {
            WatcherNoRewindWarnSite::Partial => warn_terminal_partial_no_rewind(
                watcher_provider,
                channel_id,
                tmux_session_name,
                failure_class,
                error,
            ),
            WatcherNoRewindWarnSite::EditFull => warn_terminal_edit_full_no_rewind(
                watcher_provider,
                channel_id,
                tmux_session_name,
                failure_class,
                error,
            ),
            WatcherNoRewindWarnSite::PlaceholderlessFull => {
                warn_terminal_placeholderless_full_no_rewind(
                    watcher_provider,
                    channel_id,
                    tmux_session_name,
                    failure_class,
                    error,
                )
            }
        }
    }
    plan
}

pub(super) fn watcher_partial_continuation_failure_class(
    error: &str,
    cleanup_errors_empty: bool,
) -> WatcherSendFailureClass {
    let failure_class =
        crate::services::discord::replace_outcome_policy::classify_watcher_send_failure_message(
            error,
        );
    if cleanup_errors_empty
        || crate::services::discord::replace_outcome_policy::watcher_send_failure_message_has_class_marker(error)
    {
        failure_class
    } else {
        WatcherSendFailureClass::RollbackIncomplete
    }
}

pub(super) fn warn_terminal_partial_no_rewind(
    watcher_provider: &crate::services::provider::ProviderKind,
    channel_id: serenity::ChannelId,
    tmux_session_name: &str,
    failure_class: WatcherSendFailureClass,
    error: impl std::fmt::Display,
) {
    let error = error.to_string();
    let error = stripped_send_error(&error);
    tracing::warn!(
        provider = %watcher_provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session = %tmux_session_name,
        failure_class = failure_class.as_str(),
        error = %error,
        "watcher: terminal partial-send failure will not rewind"
    );
}

pub(super) fn warn_terminal_edit_full_no_rewind(
    watcher_provider: &crate::services::provider::ProviderKind,
    channel_id: serenity::ChannelId,
    tmux_session_name: &str,
    failure_class: WatcherSendFailureClass,
    error: impl std::fmt::Display,
) {
    let error = error.to_string();
    let error = stripped_send_error(&error);
    tracing::warn!(
        provider = %watcher_provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session = %tmux_session_name,
        failure_class = failure_class.as_str(),
        error = %error,
        "watcher: terminal edit/fallback full-send failure will not rewind"
    );
}

pub(super) fn warn_terminal_placeholderless_full_no_rewind(
    watcher_provider: &crate::services::provider::ProviderKind,
    channel_id: serenity::ChannelId,
    tmux_session_name: &str,
    failure_class: WatcherSendFailureClass,
    error: impl std::fmt::Display,
) {
    let error = error.to_string();
    let error = stripped_send_error(&error);
    tracing::warn!(
        provider = %watcher_provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session = %tmux_session_name,
        failure_class = failure_class.as_str(),
        error = %error,
        "watcher: placeholder-less terminal full-send failure will not rewind"
    );
}

pub(super) fn warn_terminal_rewind_give_up(
    watcher_provider: &crate::services::provider::ProviderKind,
    channel_id: serenity::ChannelId,
    tmux_session_name: &str,
    turn_data_start_offset: u64,
    terminal_rewind_attempts: u8,
) {
    tracing::warn!(
        provider = %watcher_provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session = %tmux_session_name,
        turn_data_start_offset,
        attempts = terminal_rewind_attempts,
        "watcher: terminal delivery rewind cap exceeded; permanent give-up without rewind"
    );
}

pub(super) fn watcher_inflight_represents_external_input(
    inflight: Option<&InflightTurnState>,
) -> bool {
    inflight.is_some_and(|inflight| {
        matches!(
            inflight.turn_source,
            crate::services::discord::inflight::TurnSource::ExternalInput
                | crate::services::discord::inflight::TurnSource::ExternalAdopted
        )
    })
}

#[cfg(test)]
mod rewind_attempt_key_tests {
    use super::*;

    fn identity(started_at: &str) -> crate::services::discord::inflight::InflightTurnIdentity {
        crate::services::discord::inflight::InflightTurnIdentity {
            user_msg_id: 42,
            started_at: started_at.to_string(),
            tmux_session_name: Some("AgentDesk-claude-adk-4115".to_string()),
            turn_start_offset: Some(128),
        }
    }

    #[test]
    fn reset_rewind_attempts_keys_offset_with_turn_identity_4115() {
        let first = identity("2026-07-06T00:00:00Z");
        let second = identity("2026-07-06T00:00:01Z");
        let mut key = None;
        let mut attempts = 3;

        reset_rewind_attempts(
            &mut key,
            &mut attempts,
            watcher_rewind_attempt_key(128, Some(&first)),
        );
        assert_eq!(attempts, 0);

        attempts = 3;
        reset_rewind_attempts(
            &mut key,
            &mut attempts,
            watcher_rewind_attempt_key(128, Some(&first)),
        );
        assert_eq!(attempts, 3, "same offset and identity keeps the cap");

        reset_rewind_attempts(
            &mut key,
            &mut attempts,
            watcher_rewind_attempt_key(128, Some(&second)),
        );
        assert_eq!(
            attempts, 0,
            "same offset with a new turn identity resets the rewind cap"
        );
    }
}

/// status-panel-v2 eligibility for a watcher-driven inflight turn.
///
/// SEPARATE from `watcher_inflight_represents_external_input` on purpose: that
/// shared predicate backs the external-input delivery LEASE and the `⏳` anchor
/// lifecycle (#3164/#3174), and broadening it there would regress both. The
/// panel only needs to know whether the watcher should create/update/clean up a
/// live status panel for this turn, so it ALSO covers the synthetic
/// monitor/self-paced-loop turns (`TurnSource::MonitorTriggered`, created by
/// `ensure_monitor_auto_turn_inflight`) — which the lease/anchor sites must
/// keep ignoring.
pub(super) fn watcher_inflight_is_panel_eligible(inflight: Option<&InflightTurnState>) -> bool {
    inflight.is_some_and(|state| {
        watcher_inflight_represents_external_input(Some(state))
            || matches!(
                state.turn_source,
                crate::services::discord::inflight::TurnSource::MonitorTriggered
            )
    })
}

/// #3099: an external-input (TUI-direct / task-notification) inflight whose
/// `user_msg_id == 0` (or a `rebind_origin` synthetic) will be SKIPPED by the
/// `⏳ → ✅` reaction block (it targets `state.user_msg_id`, and `0` is no real
/// message). When such a turn completes, the `⏳` was added to a real notify-bot
/// message tracked by the prompt anchor, so the anchor-lifecycle cleanup must
/// run instead — otherwise the hourglass goes stale next to a `✅`.
pub(super) fn watcher_inflight_needs_anchor_lifecycle_cleanup(
    inflight: &InflightTurnState,
) -> bool {
    watcher_inflight_represents_external_input(Some(inflight))
        && (inflight.user_msg_id == 0 || inflight.rebind_origin)
        // #4002 (safety belt): a relay-ownership-only SystemContinuation row must
        // never drive ANY completion reaction path. It currently keeps
        // `user_msg_id != 0` and `!rebind_origin` so this predicate is already
        // `false` for it; the explicit guard keeps that true if either invariant
        // ever changes.
        && !inflight.relay_ownership_only
}

/// #4002: gate for the watcher completion **Path B** — the `⏳ → ✅` reaction on
/// `user_msg_id` plus the `session_transcripts` / `turn_analytics` row persistence.
/// It applies ONLY to a real user-authored turn: a live `user_msg_id`, not a
/// `rebind_origin` synthetic, and NOT a `relay_ownership_only` SystemContinuation
/// (compact-resume) row — whose neutral note must never gain a `✅` or a phantom
/// user-turn analytics/transcript row (`turn_id=discord:<channel>:note.id`). The
/// relay-ownership adoption / bridge-tail stand-down / response finalize are all
/// upstream of this gate and stay unaffected.
pub(super) fn watcher_completion_lifecycle_applies(inflight: &InflightTurnState) -> bool {
    !inflight.rebind_origin && inflight.user_msg_id != 0 && !inflight.relay_ownership_only
}

pub(super) fn watcher_direct_terminal_should_commit_session_idle(
    direct_send_delivered: bool,
    inflight_present: bool,
    _external_input_lease_consumed_by_relay: bool,
    _prompt_anchor_present_before_relay: bool,
    _external_input_lease_before_relay: bool,
    _ssh_direct_pending: bool,
) -> bool {
    direct_send_delivered && !inflight_present
}

pub(super) fn watcher_terminal_token_update_status(
    watcher_direct_terminal_idle_committed: bool,
) -> &'static str {
    if watcher_direct_terminal_idle_committed {
        crate::db::session_status::IDLE
    } else {
        crate::db::session_status::TURN_ACTIVE
    }
}

pub(super) fn watcher_forward_text_after_pre_turn_skip<'a>(
    decoded_text: &'a str,
    existing_buffer_len: usize,
    pre_turn_bytes_skipped: usize,
) -> &'a str {
    let mut skip_from_decoded = pre_turn_bytes_skipped
        .saturating_sub(existing_buffer_len)
        .min(decoded_text.len());
    while skip_from_decoded < decoded_text.len()
        && !decoded_text.is_char_boundary(skip_from_decoded)
    {
        skip_from_decoded += 1;
    }
    &decoded_text[skip_from_decoded..]
}

/// #2442 (H3) — fast-path check for the wrapper's `ready_for_input` JSONL
/// sentinel in the tail of the session jsonl. Reads only the last ~4 KiB
/// so it stays O(1) regardless of jsonl size. False negatives just fall
/// back to the existing 2s `READY_FOR_INPUT_IDLE_PROBE_INTERVAL` cadence,
/// so partial-line / rotation edge cases are harmless.
pub(super) fn jsonl_tail_contains_ready_for_input_sentinel(output_path: &str) -> bool {
    use std::io::{Read, Seek, SeekFrom};

    const TAIL_WINDOW_BYTES: u64 = 4 * 1024;

    let Ok(mut file) = std::fs::File::open(output_path) else {
        return false;
    };
    let Ok(meta) = file.metadata() else {
        return false;
    };
    let len = meta.len();
    if len == 0 {
        return false;
    }
    let start = len.saturating_sub(TAIL_WINDOW_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return false;
    }
    let mut buf = Vec::with_capacity(TAIL_WINDOW_BYTES as usize);
    if file.read_to_end(&mut buf).is_err() {
        return false;
    }
    let needle = format!(
        "\"type\":\"{}\"",
        crate::services::tmux_common::WRAPPER_READY_FOR_INPUT_EVENT
    );
    String::from_utf8_lossy(&buf).contains(&needle)
}

pub(super) fn watcher_jsonl_turn_state_ready_for_input(
    provider: &crate::services::provider::ProviderKind,
    runtime_kind: Option<crate::services::agent_protocol::RuntimeHandoffKind>,
    output_path: &str,
    current_offset: u64,
) -> Option<bool> {
    let path = std::path::Path::new(output_path);
    crate::services::tui_turn_state::jsonl_ready_for_input(
        provider,
        runtime_kind,
        path,
        Some(current_offset),
    )
    .map(crate::services::tui_turn_state::TuiReadyState::is_ready)
}

pub(super) fn watcher_session_ready_for_input(
    tmux_session_name: &str,
    provider: &crate::services::provider::ProviderKind,
    output_path: &str,
    current_offset: u64,
) -> bool {
    let runtime_kind =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux_session_name)
            .map(|binding| binding.runtime_kind)
            .or_else(|| {
                crate::services::tmux_common::resolve_tmux_runtime_kind_marker(tmux_session_name)
            });
    if let Some(ready) = watcher_jsonl_turn_state_ready_for_input(
        provider,
        runtime_kind,
        output_path,
        current_offset,
    ) {
        return ready;
    }
    crate::services::provider::tmux_session_fallback_ready_for_input(
        tmux_session_name,
        provider,
        runtime_kind,
    )
    .is_some_and(crate::services::pane_readiness::FallbackPaneReadiness::is_ready)
}

pub(super) fn discard_watcher_pending_buffer_after_suppressed_turn(
    all_data: &mut String,
    all_data_start_offset: &mut u64,
    all_data_fully_mirrored_to_session_relay: &mut bool,
    all_data_session_bound_relay_ack: &mut Option<SessionBoundRelayAckTarget>,
    all_data_first_forwarded_relay_sequence: &mut Option<u64>,
    current_offset: u64,
) {
    all_data.clear();
    *all_data_start_offset = current_offset;
    *all_data_fully_mirrored_to_session_relay = true;
    *all_data_session_bound_relay_ack = None;
    *all_data_first_forwarded_relay_sequence = None;
}

#[cfg(test)]
#[path = "terminal_readiness_tests.rs"]
mod tests;
