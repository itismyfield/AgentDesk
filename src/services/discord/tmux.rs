use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Mutex};

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};

use crate::db::session_transcripts::{SessionTranscriptEvent, SessionTranscriptEventKind};
use crate::db::turns::TurnTokenUsage;
use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::message_outbox::{
    OutboxMessage, enqueue_lifecycle_notification_best_effort, enqueue_outbox_best_effort,
};
use crate::services::provider::{ProviderKind, parse_provider_and_channel_from_tmux_name};
use crate::services::session_backend::{
    StreamLineState, classify_task_notification_kind, observe_stream_context,
};
use crate::services::tmux_diagnostics::{
    build_tmux_death_diagnostic, read_tmux_exit_reason, record_tmux_exit_reason,
    tmux_exit_reason_is_normal_completion, tmux_session_exists, tmux_session_has_live_pane,
};

use super::formatting::{
    ReplaceLongMessageOutcome, build_streaming_placeholder_text, format_tool_input,
    plan_streaming_rollover, replace_long_message_raw_with_outcome, send_long_message_raw,
    truncate_str,
};
use super::placeholder_cleanup::{
    PlaceholderCleanupOperation, PlaceholderCleanupOutcome, PlaceholderCleanupRecord,
    classify_delete_error,
};
use super::settings::{
    channel_supports_provider, load_last_remote_profile, load_last_session_path,
    resolve_role_binding, validate_bot_channel_routing_with_provider_channel,
};
use super::tmux_error_detect::{
    detect_provider_overload_message, is_auth_error_message, is_prompt_too_long_message,
};
use super::tmux_overload_retry::{
    PROVIDER_OVERLOAD_MAX_RETRIES, ProviderOverloadDecision, clear_provider_overload_retry_state,
    record_provider_overload_retry, schedule_provider_overload_retry,
};
use super::tmux_restart_handoff::{
    resolve_dispatched_thread_dispatch_from_db, resume_aborted_restart_turn,
};
use super::{
    SharedData, TmuxWatcherHandle, TmuxWatcherRegistry, lock_tmux_watcher_registry, rate_limit_wait,
};
// Keep the extracted lifecycle code as a tmux child module until the remaining
// watcher helpers it calls are split out of this file.
#[path = "watchers/lifecycle.rs"]
mod watcher_lifecycle;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(in crate::services::discord) use self::watcher_lifecycle::try_claim_watcher;
use self::watcher_lifecycle::*;
#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) use self::watcher_lifecycle::{
    WATCHER_POST_TERMINAL_IDLE_WINDOW, WatcherClaimOutcome, WatcherStopDecision, WatcherStopInput,
    watcher_stop_decision_after_terminal_success,
};
pub(in crate::services::discord) use self::watcher_lifecycle::{
    claim_or_reuse_watcher, clear_recovery_handled_channels,
    fail_dispatch_for_ready_for_input_stall, refresh_session_heartbeat_from_tmux_output,
    restore_tmux_watchers, session_belongs_to_current_runtime, store_recovery_handled_channels,
};
const READY_FOR_INPUT_IDLE_PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);
pub(super) const WATCHER_ACTIVITY_HEARTBEAT_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(30);
const READY_FOR_INPUT_STUCK_LABEL: &str = "stuck_at_ready";
const READY_FOR_INPUT_STUCK_REASON: &str = "agent ended at Ready for input without commit/push";
const SUPPRESSED_INTERNAL_LABEL: &str = "(자동으로 처리된 내부 작업이라 여기서 멈췄어요)";
const SUPPRESSED_RESTART_LABEL: &str =
    "(서버가 재시작되면서 답변이 중간에 멈췄어요 — 필요하시면 다시 질문해 주세요)";
const MISSING_INFLIGHT_REATTACH_GRACE_ATTEMPTS: usize = 3;
const MISSING_INFLIGHT_REATTACH_GRACE_DELAY: tokio::time::Duration =
    tokio::time::Duration::from_millis(200);
const RECENT_WATCHER_REATTACH_OFFSET_CAPACITY: usize = 32;
const RECENT_WATCHER_REATTACH_OFFSET_TTL: std::time::Duration =
    std::time::Duration::from_secs(15 * 60);
const RECENT_TURN_STOP_CAPACITY: usize = 128;
const RECENT_TURN_STOP_TTL: std::time::Duration = std::time::Duration::from_secs(10 * 60);
const RECENT_TURN_STOP_METADATA_FALLBACK_TTL: std::time::Duration =
    std::time::Duration::from_secs(60);
/// Slack between the cancel boundary recorded at stop time and the wrapper's
/// post-cancel teardown bytes that flush into the same jsonl before the
/// session actually dies. Anything beyond this boundary is treated as
/// follow-up turn output and disqualifies the death from
/// `cancel_induced_watcher_death`. Empirically the wrapper writes <2 KB of
/// teardown lines (final stream item, "[stderr] killed", etc.) so 16 KB is
/// generous yet far below the multi-KB output of even a tiny new turn.
const CANCEL_TEARDOWN_GRACE_BYTES: u64 = 16 * 1024;
const MONITOR_AUTO_TURN_REASON_CODE: &str = "lifecycle.monitor_auto_turn";
const MONITOR_AUTO_TURN_DEFERRED_REASON_CODE: &str = "lifecycle.monitor_auto_turn.deferred";
const TMUX_LIVENESS_PROBE_INTERVAL: tokio::time::Duration = tokio::time::Duration::from_secs(2);

#[derive(Debug, Clone)]
struct RecentWatcherReattachOffset {
    channel_id: ChannelId,
    tmux_session_name: String,
    offset: u64,
    recorded_at: std::time::Instant,
}

static RECENT_WATCHER_REATTACH_OFFSETS: LazyLock<Mutex<VecDeque<RecentWatcherReattachOffset>>> =
    LazyLock::new(|| {
        Mutex::new(VecDeque::with_capacity(
            RECENT_WATCHER_REATTACH_OFFSET_CAPACITY,
        ))
    });

#[derive(Debug, Clone)]
struct RecentTurnStop {
    /// #1309 codex round-3/4 fix: the same UUID is also stamped on the
    /// PG `cancel_tombstones.client_id` row that mirrors this entry.
    /// `cancel_induced_watcher_death` registers drained UUIDs with
    /// `crate::db::cancel_tombstones::register_drained_ids` so a
    /// late-landing PG row carrying the same UUID can be DELETEd without
    /// false-suppressing an unrelated future watcher death.
    id: uuid::Uuid,
    channel_id: ChannelId,
    tmux_session_name: Option<String>,
    stop_output_offset: Option<u64>,
    reason: String,
    recorded_at: std::time::Instant,
}

static RECENT_TURN_STOPS: LazyLock<Mutex<VecDeque<RecentTurnStop>>> =
    LazyLock::new(|| Mutex::new(VecDeque::with_capacity(RECENT_TURN_STOP_CAPACITY)));

fn recent_watcher_reattach_offsets()
-> std::sync::MutexGuard<'static, VecDeque<RecentWatcherReattachOffset>> {
    match RECENT_WATCHER_REATTACH_OFFSETS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn prune_recent_watcher_reattach_offsets(
    offsets: &mut VecDeque<RecentWatcherReattachOffset>,
    now: std::time::Instant,
) {
    offsets.retain(|entry| {
        now.saturating_duration_since(entry.recorded_at) <= RECENT_WATCHER_REATTACH_OFFSET_TTL
    });
}

fn record_recent_watcher_reattach_offset(
    channel_id: ChannelId,
    tmux_session_name: &str,
    offset: u64,
) {
    let now = std::time::Instant::now();
    let mut offsets = recent_watcher_reattach_offsets();
    prune_recent_watcher_reattach_offsets(&mut offsets, now);
    while offsets.len() >= RECENT_WATCHER_REATTACH_OFFSET_CAPACITY {
        offsets.pop_front();
    }
    offsets.push_back(RecentWatcherReattachOffset {
        channel_id,
        tmux_session_name: tmux_session_name.to_string(),
        offset,
        recorded_at: now,
    });
}

fn matching_recent_watcher_reattach_offset(
    channel_id: ChannelId,
    tmux_session_name: &str,
    data_start_offset: u64,
) -> Option<RecentWatcherReattachOffset> {
    let now = std::time::Instant::now();
    let mut offsets = recent_watcher_reattach_offsets();
    prune_recent_watcher_reattach_offsets(&mut offsets, now);
    offsets
        .iter()
        .rev()
        .find(|entry| {
            entry.channel_id == channel_id
                && entry.tmux_session_name == tmux_session_name
                && entry.offset == data_start_offset
        })
        .cloned()
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn clear_recent_watcher_reattach_offsets_for_tests() {
    recent_watcher_reattach_offsets().clear();
}

fn recent_turn_stops() -> std::sync::MutexGuard<'static, VecDeque<RecentTurnStop>> {
    match RECENT_TURN_STOPS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn prune_recent_turn_stops(stops: &mut VecDeque<RecentTurnStop>, now: std::time::Instant) {
    stops.retain(|entry| now.saturating_duration_since(entry.recorded_at) <= RECENT_TURN_STOP_TTL);
}

fn tmux_output_offset(tmux_session_name: &str) -> Option<u64> {
    let (output_path, _) = super::turn_bridge::tmux_runtime_paths(tmux_session_name);
    std::fs::metadata(output_path).ok().map(|meta| meta.len())
}

pub(super) async fn record_recent_turn_stop(
    channel_id: ChannelId,
    tmux_session_name: Option<&str>,
    reason: &str,
) {
    let stop_output_offset = tmux_session_name.and_then(tmux_output_offset);
    // #1309: in-memory publish is synchronous + immediate so an in-process
    // watcher can suppress the very next death without waiting on PG.
    // The PG insert is awaited (with a 500 ms cap) so a quick dcserver
    // restart immediately after the cancel cannot lose the durable copy.
    // Cross-restart correctness AND in-process race safety are layered:
    //   - in-memory: instant suppression for live watchers
    //   - PG: durable across restart
    //   - shared `client_id` + drained-id registry: skip + delete late
    //     PG rows whose UUID was already drained in-memory
    record_recent_turn_stop_with_offset(
        channel_id,
        tmux_session_name,
        stop_output_offset,
        reason,
        crate::db::cancel_tombstones::global_pool(),
    )
    .await;
}

/// Bounded foreground budget for the durable PG mirror. Normal inserts
/// finish in well under 10 ms; if a saturated pool exceeds this we fall
/// back to in-memory only and warn — the cancel signal must not stall
/// behind PG since `turn_bridge` polls `cancel_token` and could kill the
/// wrapper before the C-c path runs (codex round-3 P2 on PR #1310).
const CANCEL_TOMBSTONE_PERSIST_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

async fn record_recent_turn_stop_with_offset(
    channel_id: ChannelId,
    tmux_session_name: Option<&str>,
    stop_output_offset: Option<u64>,
    reason: &str,
    pg_pool: Option<&sqlx::PgPool>,
) {
    let client_id = uuid::Uuid::new_v4();

    // Phase 1 — publish the in-memory entry synchronously. An in-process
    // watcher firing right after `cancel_active_turn` returns will see
    // the tombstone with zero PG dependency.
    let now = std::time::Instant::now();
    {
        let mut stops = recent_turn_stops();
        prune_recent_turn_stops(&mut stops, now);
        while stops.len() >= RECENT_TURN_STOP_CAPACITY {
            stops.pop_front();
        }
        stops.push_back(RecentTurnStop {
            id: client_id,
            channel_id,
            tmux_session_name: tmux_session_name.map(str::to_string),
            stop_output_offset,
            reason: reason.to_string(),
            recorded_at: now,
        });
    }

    // Phase 2 — durable PG mirror with a bounded foreground budget. The
    // await guarantees the row is committed before the cancel path
    // returns, so a dcserver restart immediately after the cancel can
    // still see the tombstone (codex round-2/5 P1/P2 on PR #1310). The
    // 500 ms timeout caps worst-case foreground latency under PG
    // saturation.
    if let Some(pool) = pg_pool {
        let channel_id_i64 = channel_id.get() as i64;
        let stop_output_offset_i64 = stop_output_offset.map(|v| v as i64);
        let persist = crate::db::cancel_tombstones::insert_cancel_tombstone(
            pool,
            client_id,
            channel_id_i64,
            tmux_session_name,
            stop_output_offset_i64,
            reason,
        );
        match tokio::time::timeout(CANCEL_TOMBSTONE_PERSIST_TIMEOUT, persist).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(
                    "[cancel-tombstone] PG persist failed for channel {}: {}",
                    channel_id_i64,
                    error
                );
            }
            Err(_) => {
                tracing::warn!(
                    "[cancel-tombstone] PG persist for channel {} exceeded {:?}; \
                     falling back to in-memory only",
                    channel_id_i64,
                    CANCEL_TOMBSTONE_PERSIST_TIMEOUT
                );
            }
        }
    }
}

fn recent_turn_stop_for_channel(channel_id: ChannelId) -> Option<RecentTurnStop> {
    let now = std::time::Instant::now();
    let mut stops = recent_turn_stops();
    prune_recent_turn_stops(&mut stops, now);
    stops
        .iter()
        .rev()
        .find(|entry| entry.channel_id == channel_id)
        .cloned()
}

/// Returns true if a watcher death for `(channel_id, tmux_session_name)` was
/// preceded by an explicit user-initiated turn-stop (cancel) within
/// `RECENT_TURN_STOP_METADATA_FALLBACK_TTL`. The watcher cleanup path that
/// follows a cancel writes
/// `record_tmux_exit_reason("watcher cleanup: dead session after turn")`
/// and tears the session down — surfacing that as a 🔴 lifecycle notification
/// or as the "대화를 이어붙이지 못했습니다" handoff is misleading because the
/// death IS the cancel, not a crash.
///
/// IMPORTANT: this consumes ALL matching in-window tombstones on a true
/// return so the suppression is one-shot per cancel (codex P1/P2 on #1277).
/// A single user cancel commonly records two tombstones —
/// `mailbox_cancel_active_turn` records one, and
/// `turn_lifecycle::stop_provider_turn_with_outcome` records another via
/// `record_turn_stop_tombstone` — so draining only the newest leaves the
/// duplicate alive to suppress a follow-up turn's real failure that
/// reuses the same `(channel_id, tmux_session_name)` pair within the 60s
/// metadata-fallback TTL.
///
/// `current_output_offset` is the jsonl size at the moment the watcher
/// observed the death. When the tombstone was recorded with a known
/// `stop_output_offset`, this lets us bound the suppression to the
/// canceled turn's data range (codex P2 round 3 on #1277): for
/// preserve-session stops the tmux session is reused, the wrapper keeps
/// writing past the cancel EOF, and a real crash on the follow-up turn
/// would otherwise be silently swallowed. We allow a small
/// `CANCEL_TEARDOWN_GRACE_BYTES` to accommodate the wrapper's normal
/// post-cancel teardown bytes that flush before the session actually dies.
fn cancel_induced_watcher_death(
    channel_id: ChannelId,
    tmux_session_name: &str,
    current_output_offset: Option<u64>,
) -> bool {
    let now = std::time::Instant::now();
    let mut drained_ids: Vec<uuid::Uuid> = Vec::new();
    {
        let mut stops = recent_turn_stops();
        prune_recent_turn_stops(&mut stops, now);
        stops.retain(|entry| {
            if entry.channel_id != channel_id {
                return true;
            }
            if now.saturating_duration_since(entry.recorded_at)
                > RECENT_TURN_STOP_METADATA_FALLBACK_TTL
            {
                return true;
            }
            let session_matches = match entry.tmux_session_name.as_deref() {
                Some(entry_tmux) => entry_tmux == tmux_session_name,
                None => true,
            };
            if !session_matches {
                return true;
            }
            // codex P2 round 3: when both offsets are known, only consume
            // the tombstone if the watcher has not moved past the cancel
            // boundary (with a small grace for the wrapper's teardown
            // bytes between cancel record and session kill). Past that
            // boundary means a follow-up turn produced new output, so the
            // death is unrelated to the cancel and must surface its own
            // lifecycle/handoff signal.
            if let (Some(stop_offset), Some(current_offset)) =
                (entry.stop_output_offset, current_output_offset)
            {
                if current_offset > stop_offset.saturating_add(CANCEL_TEARDOWN_GRACE_BYTES) {
                    return true;
                }
            }
            drained_ids.push(entry.id);
            false
        });
    }
    if !drained_ids.is_empty() {
        // codex round-3/4 fix on PR #1310: register the drained UUIDs so a
        // late-landing PG row carrying any of them is skipped + deleted by
        // `consume_cancel_tombstone` instead of false-suppressing an
        // unrelated future watcher death within the 60 s fallback window.
        crate::db::cancel_tombstones::register_drained_ids(&drained_ids);
        true
    } else {
        false
    }
}

/// PG-aware async wrapper around `cancel_induced_watcher_death` (#1309).
///
/// In-memory hit is the fast path. On miss, fall back to the durable
/// `cancel_tombstones` table so a dcserver restart between cancel and
/// watcher-death observation can still suppress the misleading 🔴 lifecycle
/// notice. The PG row is consumed (DELETEd) in the same tx so suppression
/// remains one-shot per cancel.
async fn cancel_induced_watcher_death_async(
    channel_id: ChannelId,
    tmux_session_name: &str,
    current_output_offset: Option<u64>,
    pg_pool: Option<&sqlx::PgPool>,
) -> bool {
    let in_memory_hit =
        cancel_induced_watcher_death(channel_id, tmux_session_name, current_output_offset);

    let Some(pool) = pg_pool else {
        return in_memory_hit;
    };

    let channel_id_i64 = channel_id.get() as i64;
    let current_offset_i64 = current_output_offset.and_then(|v| i64::try_from(v).ok());

    // codex round-1 P2 on PR #1310: even when the in-memory store hits, the
    // PG mirror needs to be consumed so a follow-up watcher death within the
    // 60s fallback window cannot inherit the stale row and silently swallow
    // a real lifecycle/restart signal. The fire-and-forget insert from the
    // record path may even land after the in-memory consume, so we always
    // try to consume both layers and treat either hit as cancel-induced.
    let pg_hit = match crate::db::cancel_tombstones::consume_cancel_tombstone(
        pool,
        channel_id_i64,
        tmux_session_name,
        current_offset_i64,
    )
    .await
    {
        Ok(consumed) => consumed,
        Err(error) => {
            tracing::warn!(
                "[cancel-tombstone] PG consume failed for channel {} session {}: {}",
                channel_id_i64,
                tmux_session_name,
                error
            );
            false
        }
    };

    in_memory_hit || pg_hit
}

fn recent_turn_stop_for_watcher_range(
    channel_id: ChannelId,
    tmux_session_name: &str,
    data_start_offset: u64,
) -> Option<RecentTurnStop> {
    let now = std::time::Instant::now();
    let mut stops = recent_turn_stops();
    prune_recent_turn_stops(&mut stops, now);
    stops
        .iter()
        .rev()
        .find(|entry| {
            recent_turn_stop_matches_watcher_range(
                entry,
                channel_id,
                tmux_session_name,
                data_start_offset,
                now,
            )
        })
        .cloned()
}

fn recent_turn_stop_matches_watcher_range(
    entry: &RecentTurnStop,
    channel_id: ChannelId,
    tmux_session_name: &str,
    data_start_offset: u64,
    now: std::time::Instant,
) -> bool {
    if entry.channel_id != channel_id {
        return false;
    }

    if let (Some(entry_tmux), Some(stop_offset)) =
        (entry.tmux_session_name.as_deref(), entry.stop_output_offset)
    {
        // Exact EOF equality means the next watcher range starts after a clean
        // cancel boundary. Only ranges that began before the stop EOF belong to
        // the canceled turn.
        return entry_tmux == tmux_session_name && data_start_offset < stop_offset;
    }

    let session_matches = entry
        .tmux_session_name
        .as_deref()
        .map_or(true, |entry_tmux| entry_tmux == tmux_session_name);
    session_matches
        && now.saturating_duration_since(entry.recorded_at)
            <= RECENT_TURN_STOP_METADATA_FALLBACK_TTL
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn clear_recent_turn_stops_for_tests() {
    recent_turn_stops().clear();
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn record_recent_turn_stop_with_offset_for_tests(
    channel_id: ChannelId,
    tmux_session_name: &str,
    stop_output_offset: u64,
    reason: &str,
) {
    // Tests target the in-memory fast path; bypass the async PG mirror so
    // the helper stays sync and existing `#[test]` cases don't need to be
    // rewritten as `#[tokio::test]`.
    let now = std::time::Instant::now();
    let mut stops = recent_turn_stops();
    prune_recent_turn_stops(&mut stops, now);
    while stops.len() >= RECENT_TURN_STOP_CAPACITY {
        stops.pop_front();
    }
    stops.push_back(RecentTurnStop {
        id: uuid::Uuid::new_v4(),
        channel_id,
        tmux_session_name: Some(tmux_session_name.to_string()),
        stop_output_offset: Some(stop_output_offset),
        reason: reason.to_string(),
        recorded_at: now,
    });
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn record_recent_turn_stop_for_tests(
    channel_id: ChannelId,
    tmux_session_name: Option<&str>,
    stop_output_offset: Option<u64>,
    reason: &str,
    recorded_at: std::time::Instant,
) {
    let mut stops = recent_turn_stops();
    stops.push_back(RecentTurnStop {
        id: uuid::Uuid::new_v4(),
        channel_id,
        tmux_session_name: tmux_session_name.map(str::to_string),
        stop_output_offset,
        reason: reason.to_string(),
        recorded_at,
    });
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct WatcherLineOutcome {
    pub found_result: bool,
    pub is_prompt_too_long: bool,
    pub is_auth_error: bool,
    pub auth_error_message: Option<String>,
    pub is_provider_overloaded: bool,
    pub provider_overload_message: Option<String>,
    pub stale_resume_detected: bool,
    pub auto_compacted: bool,
    pub task_notification_kind: Option<TaskNotificationKind>,
}

#[derive(Debug, Clone)]
pub(super) struct RestoredWatcherTurn {
    current_msg_id: MessageId,
    response_sent_offset: usize,
    full_response: String,
    last_edit_text: String,
    task_notification_kind: Option<TaskNotificationKind>,
    finish_mailbox_on_completion: bool,
}

#[derive(Debug)]
struct WatcherStreamSeed {
    placeholder_msg_id: Option<MessageId>,
    response_sent_offset: usize,
    full_response: String,
    last_edit_text: String,
    task_notification_kind: Option<TaskNotificationKind>,
    finish_mailbox_on_completion: bool,
}

fn normalize_response_sent_offset(full_response: &str, response_sent_offset: usize) -> usize {
    let mut offset = response_sent_offset.min(full_response.len());
    while offset > 0 && !full_response.is_char_boundary(offset) {
        offset -= 1;
    }
    offset
}

fn record_watcher_invariant(
    condition: bool,
    provider: Option<&ProviderKind>,
    channel_id: ChannelId,
    invariant: &'static str,
    code_location: &'static str,
    message: &'static str,
    details: serde_json::Value,
) -> bool {
    crate::services::observability::record_invariant_check(
        condition,
        crate::services::observability::InvariantViolation {
            provider: provider.map(ProviderKind::as_str),
            channel_id: Some(channel_id.get()),
            dispatch_id: None,
            session_key: None,
            turn_id: None,
            invariant,
            code_location,
            message,
            details,
        },
    )
}

pub(super) fn restored_watcher_turn_from_inflight(
    state: &super::inflight::InflightTurnState,
    tmux_session_name: &str,
    finish_mailbox_on_completion: bool,
) -> Option<RestoredWatcherTurn> {
    if state.rebind_origin
        || state.current_msg_id == 0
        || state
            .tmux_session_name
            .as_deref()
            .is_some_and(|name| name != tmux_session_name)
    {
        return None;
    }

    let response_sent_offset =
        normalize_response_sent_offset(&state.full_response, state.response_sent_offset);
    Some(RestoredWatcherTurn {
        current_msg_id: MessageId::new(state.current_msg_id),
        response_sent_offset,
        full_response: state.full_response.clone(),
        last_edit_text: reconstructed_inflight_placeholder_body(state),
        task_notification_kind: state.task_notification_kind,
        finish_mailbox_on_completion,
    })
}

fn watcher_stream_seed(restored_turn: Option<RestoredWatcherTurn>) -> WatcherStreamSeed {
    match restored_turn {
        Some(restored) => WatcherStreamSeed {
            placeholder_msg_id: Some(restored.current_msg_id),
            response_sent_offset: restored.response_sent_offset,
            full_response: restored.full_response,
            last_edit_text: restored.last_edit_text,
            task_notification_kind: restored.task_notification_kind,
            finish_mailbox_on_completion: restored.finish_mailbox_on_completion,
        },
        None => WatcherStreamSeed {
            placeholder_msg_id: None,
            response_sent_offset: 0,
            full_response: String::new(),
            last_edit_text: String::new(),
            task_notification_kind: None,
            finish_mailbox_on_completion: false,
        },
    }
}

fn lifecycle_reason_code_for_tmux_exit(reason: &str) -> &'static str {
    let lower = reason.to_ascii_lowercase();
    if tmux_exit_reason_is_normal_completion(reason) {
        "lifecycle.normal_completion"
    } else if lower.contains("force-kill")
        || lower.contains("deadlock")
        || lower.contains("prompt too long")
        || lower.contains("auth")
    {
        "lifecycle.force_kill"
    } else if lower.contains("idle") || lower.contains("turn cap") || lower.contains("cleanup") {
        "lifecycle.auto_cleanup"
    } else {
        "lifecycle.tmux_terminated"
    }
}

fn tmux_death_lifecycle_notification_reason(reason: Option<&str>) -> Option<&str> {
    let reason = reason?.trim();
    if reason.is_empty() {
        return None;
    }

    let reason = reason
        .strip_prefix('[')
        .and_then(|s| s.find("] ").map(|i| &s[i + 2..]))
        .unwrap_or(reason)
        .trim();
    if reason.is_empty() || reason.eq_ignore_ascii_case("unknown") {
        return None;
    }

    let lower = reason.to_ascii_lowercase();
    if tmux_exit_reason_is_normal_completion(reason) || lower.contains("force-kill") {
        return None;
    }

    Some(reason)
}

fn tmux_death_is_normal_completion(reason: Option<&str>, _diagnostic: Option<&str>) -> bool {
    reason.is_some_and(tmux_exit_reason_is_normal_completion)
}

fn stream_line_state_token_usage(state: &StreamLineState) -> Option<TurnTokenUsage> {
    let usage = TurnTokenUsage {
        input_tokens: state.accum_input_tokens,
        cache_create_tokens: state.accum_cache_create_tokens,
        cache_read_tokens: state.accum_cache_read_tokens,
        output_tokens: state.accum_output_tokens,
    };
    (usage.input_tokens > 0
        || usage.cache_create_tokens > 0
        || usage.cache_read_tokens > 0
        || usage.output_tokens > 0)
        .then_some(usage)
}

fn watcher_ready_for_input_turn_completed(
    tracker: &mut crate::services::provider::ReadyForInputIdleTracker,
    data_start_offset: u64,
    current_offset: u64,
    ready_for_input: bool,
    post_work_observed: bool,
    now: std::time::Instant,
) -> crate::services::provider::ReadyForInputIdleState {
    tracker.observe_idle_state(
        current_offset > data_start_offset,
        ready_for_input,
        post_work_observed,
        now,
    )
}

fn merge_task_notification_kind(
    current: Option<TaskNotificationKind>,
    new_kind: TaskNotificationKind,
) -> Option<TaskNotificationKind> {
    let priority = |kind: TaskNotificationKind| match kind {
        TaskNotificationKind::Subagent => 0,
        TaskNotificationKind::Background => 1,
        TaskNotificationKind::MonitorAutoTurn => 2,
    };

    match current {
        Some(existing) if priority(existing) >= priority(new_kind) => Some(existing),
        _ => Some(new_kind),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TerminalRelayDecision {
    should_direct_send: bool,
    should_tag_monitor_origin: bool,
    should_enqueue_notify_outbox: bool,
    suppressed: bool,
}

fn terminal_relay_decision(
    has_assistant_response: bool,
    task_notification_kind: Option<TaskNotificationKind>,
) -> TerminalRelayDecision {
    match task_notification_kind {
        Some(TaskNotificationKind::MonitorAutoTurn) => TerminalRelayDecision {
            should_direct_send: has_assistant_response,
            should_tag_monitor_origin: has_assistant_response,
            should_enqueue_notify_outbox: false,
            suppressed: !has_assistant_response,
        },
        Some(TaskNotificationKind::Background) => TerminalRelayDecision {
            // Background task_notification marks that a background event (Monitor
            // completion, task_complete, etc.) fired during the turn. The response
            // after that event is user-facing content and must reach Discord.
            // Historical behavior suppressed the whole terminal relay, which
            // caused #1044 A→C: user messages streamed after the tag were lost.
            should_direct_send: has_assistant_response,
            should_tag_monitor_origin: false,
            should_enqueue_notify_outbox: false,
            suppressed: !has_assistant_response,
        },
        Some(TaskNotificationKind::Subagent) => TerminalRelayDecision {
            // Subagent turn = internal sub-agent reporting to parent. Not routed
            // to the user-facing channel.
            should_direct_send: false,
            should_tag_monitor_origin: false,
            should_enqueue_notify_outbox: false,
            suppressed: true,
        },
        None => TerminalRelayDecision {
            should_direct_send: has_assistant_response,
            should_tag_monitor_origin: false,
            should_enqueue_notify_outbox: false,
            suppressed: false,
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SuppressedPlaceholderAction {
    None,
    Delete,
    Edit(String),
}

fn is_spinner_prefix_char(ch: char) -> bool {
    matches!(
        ch,
        '⠏' | '⠋' | '⠙' | '⠹' | '⠸' | '⠼' | '⠴' | '⠦' | '⠧' | '⠇'
    )
}

fn is_inprogress_indicator_line(line: &str) -> bool {
    line.trim_start()
        .chars()
        .next()
        .is_some_and(is_spinner_prefix_char)
}

fn strip_inprogress_indicators(body: &str) -> String {
    let mut lines: Vec<&str> = body
        .lines()
        .filter(|line| !is_inprogress_indicator_line(line))
        .collect();
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

fn rewrite_placeholder_as_terminal_suppressed(text: &str, label: &str) -> String {
    let cleaned = strip_inprogress_indicators(text);
    let trimmed = cleaned.trim_end();
    if trimmed.ends_with(label) {
        return trimmed.to_string();
    }
    if trimmed.is_empty() {
        // #1009: label itself may exceed DISCORD_MSG_LIMIT when monitor entries
        // balloon — guard here too (the with-body branch below already guards).
        let limit = super::DISCORD_MSG_LIMIT;
        if label.len() > limit {
            return truncate_str(label, limit);
        }
        return label.to_string();
    }

    let suffix = format!("\n\n{label}");
    let max_base_len = super::DISCORD_MSG_LIMIT.saturating_sub(suffix.len());
    let base = if trimmed.len() > max_base_len {
        truncate_str(trimmed, max_base_len)
    } else {
        trimmed.to_string()
    };
    let composed = format!("{base}{suffix}");
    // Final belt-and-suspenders guard (rare: suffix.len() ≥ DISCORD_MSG_LIMIT).
    if composed.len() > super::DISCORD_MSG_LIMIT {
        truncate_str(&composed, super::DISCORD_MSG_LIMIT)
    } else {
        composed
    }
}

fn reconstructed_inflight_placeholder_body(state: &super::inflight::InflightTurnState) -> String {
    let current_portion = state
        .full_response
        .get(state.response_sent_offset..)
        .unwrap_or("");
    let status_block = super::formatting::build_placeholder_status_block(
        "⠼",
        state.prev_tool_status.as_deref(),
        state.current_tool_line.as_deref(),
        &state.full_response,
    );
    build_streaming_placeholder_text(current_portion, &status_block)
}

fn orphan_suppressed_placeholder_action(
    state: &super::inflight::InflightTurnState,
    has_active_turn: bool,
    tmux_session_name: &str,
) -> SuppressedPlaceholderAction {
    if has_active_turn
        || state.rebind_origin
        || state.response_sent_offset == 0
        || state.current_msg_id == 0
        || state.tmux_session_name.as_deref() != Some(tmux_session_name)
    {
        return SuppressedPlaceholderAction::None;
    }

    let body = reconstructed_inflight_placeholder_body(state);
    SuppressedPlaceholderAction::Edit(rewrite_placeholder_as_terminal_suppressed(
        &body,
        SUPPRESSED_RESTART_LABEL,
    ))
}

/// Unified entry point for every placeholder-suppression decision.
///
/// Three production sites produced identical edit/delete/log scaffolding before
/// #1055 (bridge-guard duplicate relay at `tmux_output_watcher_with_restore`,
/// task-notification terminal suppress at the same function, and
/// `reconcile_orphan_suppressed_placeholder_for_restored_watcher`). The
/// `decide_placeholder_suppression` + `apply_placeholder_suppression` pair
/// replaces those copies so a future placeholder-suppression regression can be
/// fixed in exactly one location. See also `Shared Agent Rules` — DRY 강제.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaceholderSuppressOrigin {
    OrphanRestartHandoff,
    ActiveBridgeTurnGuard,
    TaskNotificationTerminal,
}

impl PlaceholderSuppressOrigin {
    fn log_scope(self) -> &'static str {
        match self {
            Self::OrphanRestartHandoff => "orphan suppressed placeholder reconcile",
            Self::ActiveBridgeTurnGuard => "active bridge suppressed placeholder",
            Self::TaskNotificationTerminal => "suppressed placeholder",
        }
    }
}

struct PlaceholderSuppressContext<'a> {
    origin: PlaceholderSuppressOrigin,
    placeholder_msg_id: Option<serenity::MessageId>,
    response_sent_offset: usize,
    last_edit_text: &'a str,
    inflight_state: Option<&'a super::inflight::InflightTurnState>,
    has_active_turn: bool,
    tmux_session_name: &'a str,
    task_notification_kind: Option<TaskNotificationKind>,
    reattach_offset_match: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PlaceholderSuppressDecision {
    None,
    /// Keep the user-facing body already streamed to the placeholder; strip
    /// the in-progress spinner/status-block suffix so the message is not
    /// frozen mid-stream. `reason` feeds the observability log only.
    /// `cleaned_body` is the stripped body that should be written back.
    Preserve {
        reason: &'static str,
        cleaned_body: String,
    },
    Edit(String),
    Delete,
}

fn strip_placeholder_indicators_for_preserve(text: &str) -> String {
    strip_inprogress_indicators(text).trim_end().to_string()
}

fn decide_placeholder_suppression(
    ctx: &PlaceholderSuppressContext<'_>,
) -> PlaceholderSuppressDecision {
    match ctx.origin {
        PlaceholderSuppressOrigin::OrphanRestartHandoff => {
            let Some(state) = ctx.inflight_state else {
                return PlaceholderSuppressDecision::None;
            };
            match orphan_suppressed_placeholder_action(
                state,
                ctx.has_active_turn,
                ctx.tmux_session_name,
            ) {
                SuppressedPlaceholderAction::None => PlaceholderSuppressDecision::None,
                SuppressedPlaceholderAction::Delete => PlaceholderSuppressDecision::Delete,
                SuppressedPlaceholderAction::Edit(content) => {
                    PlaceholderSuppressDecision::Edit(content)
                }
            }
        }
        PlaceholderSuppressOrigin::ActiveBridgeTurnGuard => {
            if ctx.reattach_offset_match {
                return PlaceholderSuppressDecision::Preserve {
                    reason: "reattach-offset-match",
                    cleaned_body: strip_placeholder_indicators_for_preserve(ctx.last_edit_text),
                };
            }
            match suppressed_placeholder_action(
                ctx.placeholder_msg_id.is_some(),
                ctx.response_sent_offset,
                ctx.last_edit_text,
            ) {
                SuppressedPlaceholderAction::None => PlaceholderSuppressDecision::None,
                SuppressedPlaceholderAction::Delete => PlaceholderSuppressDecision::Delete,
                SuppressedPlaceholderAction::Edit(content) => {
                    PlaceholderSuppressDecision::Edit(content)
                }
            }
        }
        PlaceholderSuppressOrigin::TaskNotificationTerminal => {
            let preserves_body = matches!(
                ctx.task_notification_kind,
                Some(TaskNotificationKind::Background | TaskNotificationKind::Subagent)
            );
            match suppressed_placeholder_action(
                ctx.placeholder_msg_id.is_some(),
                ctx.response_sent_offset,
                ctx.last_edit_text,
            ) {
                SuppressedPlaceholderAction::None => PlaceholderSuppressDecision::None,
                SuppressedPlaceholderAction::Delete => PlaceholderSuppressDecision::Delete,
                SuppressedPlaceholderAction::Edit(_) if preserves_body => {
                    PlaceholderSuppressDecision::Preserve {
                        reason: "background-or-subagent-kind",
                        cleaned_body: strip_placeholder_indicators_for_preserve(ctx.last_edit_text),
                    }
                }
                SuppressedPlaceholderAction::Edit(content) => {
                    PlaceholderSuppressDecision::Edit(content)
                }
            }
        }
    }
}

async fn apply_placeholder_suppression(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    placeholder_msg_id: Option<serenity::MessageId>,
    origin: PlaceholderSuppressOrigin,
    decision: PlaceholderSuppressDecision,
    detail: Option<&str>,
) {
    match decision {
        PlaceholderSuppressDecision::None => {}
        PlaceholderSuppressDecision::Preserve {
            reason,
            cleaned_body,
        } => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            let detail_suffix = detail.map(|d| format!(" — {d}")).unwrap_or_default();
            tracing::info!(
                "  [{ts}] 👁 {} preserved placeholder ({reason}){detail_suffix}",
                origin.log_scope()
            );
            if let Some(msg_id) = placeholder_msg_id {
                if cleaned_body.is_empty() {
                    delete_nonterminal_placeholder(
                        http,
                        channel_id,
                        shared,
                        provider,
                        tmux_session_name,
                        msg_id,
                        origin.log_scope(),
                    )
                    .await;
                } else {
                    edit_preserve_placeholder(
                        http,
                        channel_id,
                        shared,
                        provider,
                        tmux_session_name,
                        msg_id,
                        &cleaned_body,
                        origin.log_scope(),
                    )
                    .await;
                }
            }
        }
        PlaceholderSuppressDecision::Delete => {
            if let Some(msg_id) = placeholder_msg_id {
                delete_terminal_placeholder(
                    http,
                    channel_id,
                    shared,
                    provider,
                    tmux_session_name,
                    msg_id,
                    origin.log_scope(),
                )
                .await;
            }
        }
        PlaceholderSuppressDecision::Edit(content) => {
            if let Some(msg_id) = placeholder_msg_id {
                edit_terminal_placeholder(
                    http,
                    channel_id,
                    shared,
                    provider,
                    tmux_session_name,
                    msg_id,
                    &content,
                    origin.log_scope(),
                )
                .await;
            }
        }
    }
}

fn record_placeholder_cleanup(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
    tmux_session_name: &str,
    operation: PlaceholderCleanupOperation,
    outcome: PlaceholderCleanupOutcome,
    source: &'static str,
) {
    if let PlaceholderCleanupOutcome::Failed { class, detail } = &outcome {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ placeholder cleanup {} failed ({}) for channel {} msg {}: {}",
            operation.as_str(),
            class.as_str(),
            channel_id.get(),
            message_id.get(),
            detail
        );
    }
    shared.placeholder_cleanup.record(PlaceholderCleanupRecord {
        provider: provider.clone(),
        channel_id,
        message_id,
        tmux_session_name: Some(tmux_session_name.to_string()),
        operation,
        outcome,
        source,
    });
}

async fn delete_terminal_placeholder(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    delete_placeholder_with_operation(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        message_id,
        PlaceholderCleanupOperation::DeleteTerminal,
        source,
    )
    .await
}

async fn delete_nonterminal_placeholder(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    delete_placeholder_with_operation(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        message_id,
        PlaceholderCleanupOperation::DeleteNonterminal,
        source,
    )
    .await
}

async fn delete_placeholder_with_operation(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    operation: PlaceholderCleanupOperation,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    let outcome = match channel_id.delete_message(http, message_id).await {
        Ok(_) => PlaceholderCleanupOutcome::Succeeded,
        Err(error) => classify_delete_error(&error.to_string()),
    };
    record_placeholder_cleanup(
        shared,
        provider,
        channel_id,
        message_id,
        tmux_session_name,
        operation,
        outcome.clone(),
        source,
    );
    outcome
}

async fn edit_terminal_placeholder(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    content: &str,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    edit_placeholder_with_operation(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        message_id,
        content,
        PlaceholderCleanupOperation::EditTerminal,
        source,
    )
    .await
}

async fn edit_preserve_placeholder(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    content: &str,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    edit_placeholder_with_operation(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        message_id,
        content,
        PlaceholderCleanupOperation::EditPreserve,
        source,
    )
    .await
}

async fn edit_placeholder_with_operation(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    content: &str,
    operation: PlaceholderCleanupOperation,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    rate_limit_wait(shared, channel_id).await;
    let outcome = match channel_id
        .edit_message(
            http,
            message_id,
            serenity::EditMessage::new().content(content),
        )
        .await
    {
        Ok(_) => PlaceholderCleanupOutcome::Succeeded,
        Err(error) => PlaceholderCleanupOutcome::failed(error.to_string()),
    };
    record_placeholder_cleanup(
        shared,
        provider,
        channel_id,
        message_id,
        tmux_session_name,
        operation,
        outcome.clone(),
        source,
    );
    outcome
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FallbackPlaceholderCleanupDecision {
    RelayCommitted,
    PreserveInflightForCleanupRetry,
}

fn fallback_placeholder_cleanup_decision(
    cleanup: &PlaceholderCleanupOutcome,
) -> FallbackPlaceholderCleanupDecision {
    if cleanup.is_committed() {
        FallbackPlaceholderCleanupDecision::RelayCommitted
    } else {
        FallbackPlaceholderCleanupDecision::PreserveInflightForCleanupRetry
    }
}

fn suppressed_placeholder_action(
    has_placeholder: bool,
    response_sent_offset: usize,
    last_edit_text: &str,
) -> SuppressedPlaceholderAction {
    if !has_placeholder {
        return SuppressedPlaceholderAction::None;
    }

    let placeholder_was_exposed = response_sent_offset > 0 || !last_edit_text.trim().is_empty();
    if placeholder_was_exposed {
        SuppressedPlaceholderAction::Edit(rewrite_placeholder_as_terminal_suppressed(
            last_edit_text,
            SUPPRESSED_INTERNAL_LABEL,
        ))
    } else {
        SuppressedPlaceholderAction::Delete
    }
}

fn monitor_auto_turn_label(tmux_session_name: &str) -> String {
    parse_provider_and_channel_from_tmux_name(tmux_session_name)
        .map(|(_, channel_name)| channel_name)
        .filter(|channel_name| !channel_name.trim().is_empty())
        .unwrap_or_else(|| tmux_session_name.to_string())
}

fn monitor_auto_turn_session_key(channel_id: ChannelId, data_start_offset: u64) -> String {
    format!(
        "monitor_auto_turn:ch:{}:off:{}",
        channel_id.get(),
        data_start_offset
    )
}

/// #1009: Lifecycle-notice variant of the shared monitor summary line. Calls
/// `format_monitor_suppressed_label` for the trailing summary so the
/// suppressed-placeholder edit body and the lifecycle notify-outbox row use
/// identical copy (DRY enforcement). The `label` (channel/tmux session name)
/// stays as the human-readable scope prefix; `entry_keys` come from the
/// channel's `MonitoringStore` snapshot.
fn monitor_auto_turn_completion_notice(
    label: &str,
    event_count: usize,
    entry_keys: &[String],
) -> String {
    let summary = format_monitor_suppressed_label(event_count, entry_keys);
    format!("{summary} · 대상: {label}")
}

/// #1009: Shared formatter for the monitor auto-turn suppressed-placeholder
/// summary line. Produces:
///   - `🔔 Monitor n회 처리 · 다음 모니터: {key1, key2, ...}` when entries > 0
///   - `🔔 Monitor n회 처리 · (등록된 모니터 없음)` when entries == 0
/// Entry keys are emitted in the order the store returns them. Called from
/// both `suppressed_placeholder_action` (via the monitor-aware wrapper) and
/// the lifecycle-notice path so both channels use identical copy.
pub(super) fn format_monitor_suppressed_label(event_count: usize, entry_keys: &[String]) -> String {
    if entry_keys.is_empty() {
        format!("🔔 Monitor {}회 처리 · (등록된 모니터 없음)", event_count)
    } else {
        format!(
            "🔔 Monitor {}회 처리 · 다음 모니터: {{{}}}",
            event_count,
            entry_keys.join(", ")
        )
    }
}

/// #1009: Compose the full suppressed-placeholder body for monitor auto-turn.
/// Rebuilds the existing `last_edit_text`-preserve + label-append behaviour
/// from `rewrite_placeholder_as_terminal_suppressed` but with the dynamic
/// `format_monitor_suppressed_label` text. Length-guarded against
/// `DISCORD_MSG_LIMIT` by the underlying rewrite helper.
pub(super) fn format_monitor_suppressed_body(
    last_edit_text: &str,
    event_count: usize,
    entry_keys: &[String],
) -> String {
    let label = format_monitor_suppressed_label(event_count, entry_keys);
    rewrite_placeholder_as_terminal_suppressed(last_edit_text, &label)
}

/// #1009: System-level hint injected once per monitor auto-turn entry so the
/// agent produces a 1-line summary + next action before terminating. Returned
/// verbatim at the claim sites; callers log it and record the one-shot flag.
pub(super) const MONITOR_AUTO_TURN_PREAMBLE_HINT: &str = "[system] 모니터 자동 턴 종료 전, 이번 턴에서 확인한 상태 1줄 요약과 다음 액션을 반드시 남겨주세요.";

/// #1009: One-shot guard for `MONITOR_AUTO_TURN_PREAMBLE_HINT` injection.
/// Returns `Some(hint)` on first call per turn and flips `injected` to true;
/// subsequent calls return `None` so the hint is never repeated within a
/// single monitor auto-turn frame.
pub(super) fn consume_monitor_auto_turn_preamble_once(injected: &mut bool) -> Option<&'static str> {
    if *injected {
        None
    } else {
        *injected = true;
        Some(MONITOR_AUTO_TURN_PREAMBLE_HINT)
    }
}

fn enqueue_monitor_auto_turn_suppressed_notification(
    pg_pool: Option<&sqlx::PgPool>,
    db: Option<&crate::db::Db>,
    channel_id: ChannelId,
    tmux_session_name: &str,
    data_start_offset: u64,
    event_count: usize,
    entry_keys: &[String],
) -> bool {
    let target = format!("channel:{}", channel_id.get());
    let session_key = monitor_auto_turn_session_key(channel_id, data_start_offset);
    let label = monitor_auto_turn_label(tmux_session_name);
    let content = monitor_auto_turn_completion_notice(&label, event_count, entry_keys);
    enqueue_lifecycle_notification_best_effort(
        db,
        pg_pool,
        target.as_str(),
        Some(session_key.as_str()),
        MONITOR_AUTO_TURN_REASON_CODE,
        content.as_str(),
    )
}

fn enqueue_monitor_auto_turn_deferred_notification(
    pg_pool: Option<&sqlx::PgPool>,
    db: Option<&crate::db::Db>,
    channel_id: ChannelId,
    data_start_offset: u64,
) -> bool {
    let target = format!("channel:{}", channel_id.get());
    let session_key = format!(
        "monitor_auto_turn_deferred:ch:{}:off:{}",
        channel_id.get(),
        data_start_offset
    );
    enqueue_lifecycle_notification_best_effort(
        db,
        pg_pool,
        target.as_str(),
        Some(session_key.as_str()),
        MONITOR_AUTO_TURN_DEFERRED_REASON_CODE,
        "🔔 Monitor 트리거 유예 (유저 턴 종료 후 처리)",
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MonitorAutoTurnStart {
    acquired: bool,
    deferred: bool,
}

async fn start_monitor_auto_turn_when_available(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    data_start_offset: u64,
    cancel: &std::sync::atomic::AtomicBool,
) -> MonitorAutoTurnStart {
    let mut deferred = false;
    let synthetic_message_id = MessageId::new(data_start_offset.max(1));

    loop {
        if cancel.load(Ordering::Relaxed) || shared.shutting_down.load(Ordering::Relaxed) {
            return MonitorAutoTurnStart {
                acquired: false,
                deferred,
            };
        }

        let token = Arc::new(crate::services::provider::CancelToken::new());
        let started = super::mailbox_try_start_turn(
            shared,
            channel_id,
            token,
            UserId::new(1),
            synthetic_message_id,
        )
        .await;
        if started {
            shared
                .global_active
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            shared
                .turn_start_times
                .insert(channel_id, std::time::Instant::now());
            return MonitorAutoTurnStart {
                acquired: true,
                deferred,
            };
        }

        if !deferred {
            deferred = true;
            let _ = enqueue_monitor_auto_turn_deferred_notification(
                shared.pg_pool.as_ref(),
                sqlite_runtime_db(shared),
                channel_id,
                data_start_offset,
            );
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🔔 Monitor auto-turn deferred until active user turn completes (channel {}, provider={})",
                channel_id.get(),
                provider.as_str()
            );
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }
}

async fn finish_monitor_auto_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
) {
    let finish = super::mailbox_finish_turn(shared, provider, channel_id).await;
    if let Some(token) = finish.removed_token {
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = shared.global_active.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |current| current.checked_sub(1),
        );
    }
    shared.turn_start_times.remove(&channel_id);
    if let Ok(mut last) = shared.last_turn_at.lock() {
        *last = Some(chrono::Local::now().to_rfc3339());
    }
    if finish.has_pending {
        super::schedule_deferred_idle_queue_kickoff(
            shared.clone(),
            provider.clone(),
            channel_id,
            "monitor auto-turn completed with queued backlog",
        );
    }
}

async fn finish_monitor_auto_turn_if_claimed(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    claimed: &mut bool,
    finished: &mut bool,
) {
    if *claimed {
        finish_monitor_auto_turn(shared, provider, channel_id).await;
        *claimed = false;
        *finished = true;
    }
}

fn ensure_monitor_auto_turn_inflight(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    output_path: &str,
    input_fifo_path: &str,
    session_id: Option<&str>,
    turn_start_offset: u64,
    last_offset: u64,
) {
    if super::inflight::load_inflight_state(provider, channel_id.get()).is_some() {
        return;
    }

    let channel_name = parse_provider_and_channel_from_tmux_name(tmux_session_name)
        .map(|(_, channel_name)| channel_name);
    let mut synthetic = super::inflight::InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        channel_name,
        0,
        0,
        0,
        "Monitor auto-turn".to_string(),
        session_id.map(str::to_string),
        Some(tmux_session_name.to_string()),
        Some(output_path.to_string()),
        Some(input_fifo_path.to_string()),
        last_offset,
    );
    synthetic.turn_start_offset = Some(turn_start_offset);
    synthetic.rebind_origin = true;

    match super::inflight::save_inflight_state_create_new(&synthetic) {
        Ok(()) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Registered synthetic inflight for monitor auto-turn in channel {}",
                channel_id.get()
            );
        }
        Err(super::inflight::CreateNewInflightError::AlreadyExists) => {}
        Err(super::inflight::CreateNewInflightError::Internal(error)) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ Failed to register synthetic monitor inflight for channel {}: {}",
                channel_id.get(),
                error
            );
        }
    }
}

/// Read the `.generation` marker file mtime in nanoseconds since the unix
/// epoch. Returns 0 when the marker is missing in BOTH the canonical
/// persistent location (`runtime_root()/runtime/sessions/`) and the legacy
/// `/tmp/` fallback supported by `resolve_session_temp_path` (#892
/// migration window). All of those conditions are treated by callers as
/// "fresh wrapper".
///
/// `.generation` is written exactly once per spawn by `claude.rs` after
/// `tmux::create_session` and never touched by the live wrapper, so its
/// mtime uniquely identifies the wrapper instance even when jsonl
/// rotation changes the jsonl inode (#1270).
pub(super) fn read_generation_file_mtime_ns(tmux_session_name: &str) -> i64 {
    let Some(path) =
        crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "generation")
    else {
        return 0;
    };
    let Ok(meta) = std::fs::metadata(&path) else {
        return 0;
    };
    let Ok(modified) = meta.modified() else {
        return 0;
    };
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_nanos()).ok())
        .unwrap_or(0)
}

/// Rewrite a file's contents while preserving its prior modified time. Used
/// by the adoption path to refresh the `.generation` marker payload (so the
/// generation number on disk matches the current dcserver runtime) without
/// changing the file's mtime — the mtime is the wrapper-identity signal that
/// the regression resolver uses to distinguish "same wrapper, mid-flight
/// rotation" from "fresh wrapper after cancel→respawn" (see
/// `watermark_after_output_regression`). Adoption changes the runtime that
/// owns the wrapper, but it does NOT respawn the wrapper itself, so the
/// identity signal must stay pinned.
///
/// Failures are logged and swallowed: the worst case is a redundant fresh-
/// wrapper reset on a restored offset, which is the same behaviour the
/// codebase had before #1271. Returning an error would not unblock the
/// adoption.
pub(super) fn preserve_mtime_after_write(path: &str, content: &[u8], context: &str) {
    let prior_mtime = std::fs::metadata(path).ok().and_then(|m| m.modified().ok());
    if let Err(e) = std::fs::write(path, content) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ preserve_mtime_after_write: failed to write {} (context={}, error={})",
            path,
            context,
            e
        );
        return;
    }
    let Some(prior) = prior_mtime else {
        // No prior mtime to preserve (file did not exist or metadata unavailable).
        // The post-write mtime is the only baseline we have, which is the same
        // outcome as before this helper existed.
        return;
    };
    let times = std::fs::FileTimes::new().set_modified(prior);
    let file = match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(f) => f,
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ preserve_mtime_after_write: failed to reopen {} for set_times (context={}, error={})",
                path,
                context,
                e
            );
            return;
        }
    };
    if let Err(e) = file.set_times(times) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ preserve_mtime_after_write: set_times failed for {} (context={}, error={})",
            path,
            context,
            e
        );
    }
}

/// Decide what watermark a stale-output regression (current EOF lower than
/// `confirmed`) should land on, based on whether the wrapper instance is
/// the same one that advanced the watermark in the first place.
///
/// - Same wrapper (`.generation` mtime unchanged): mid-flight rotation
///   (`truncate_jsonl_head_safe` rename). The byte stream beyond the
///   surviving content is genuinely new, so we pin to `observed_output_end`
///   to avoid re-relaying surviving content (PR #1256 intent).
/// - Different wrapper (mtime changed, mtime missing, or first observation
///   with stored mtime == 0): cancel→respawn or any fresh spawn. The
///   current file is fully new content — reset to 0 so the watcher walks
///   it from the beginning (#1270 regression fix).
fn watermark_after_output_regression(
    stored_generation_mtime_ns: i64,
    current_generation_mtime_ns: i64,
    observed_output_end: u64,
) -> u64 {
    let same_wrapper = stored_generation_mtime_ns != 0
        && stored_generation_mtime_ns == current_generation_mtime_ns;
    if same_wrapper { observed_output_end } else { 0 }
}

fn reset_stale_relay_watermark_if_output_regressed(
    shared: &SharedData,
    channel_id: ChannelId,
    tmux_session_name: &str,
    observed_output_end: u64,
    context: &str,
) -> bool {
    let relay_coord = shared.tmux_relay_coord(channel_id);
    let mut confirmed = relay_coord
        .confirmed_end_offset
        .load(std::sync::atomic::Ordering::Acquire);

    while confirmed != 0 && observed_output_end < confirmed {
        let stored_gen_mtime_ns = relay_coord
            .confirmed_end_generation_mtime_ns
            .load(std::sync::atomic::Ordering::Acquire);
        let current_gen_mtime_ns = read_generation_file_mtime_ns(tmux_session_name);
        let new_watermark = watermark_after_output_regression(
            stored_gen_mtime_ns,
            current_gen_mtime_ns,
            observed_output_end,
        );

        match relay_coord.confirmed_end_offset.compare_exchange(
            confirmed,
            new_watermark,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        ) {
            Ok(_) => {
                relay_coord
                    .last_relay_ts_ms
                    .store(0, std::sync::atomic::Ordering::Release);
                relay_coord
                    .confirmed_end_generation_mtime_ns
                    .store(current_gen_mtime_ns, std::sync::atomic::Ordering::Release);
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] 👁 Reset stale tmux relay watermark for {} (channel {}, context={}, observed_output_end={}, stale_confirmed_end={}, new_watermark={}, generation_mtime_changed={})",
                    tmux_session_name,
                    channel_id.get(),
                    context,
                    observed_output_end,
                    confirmed,
                    new_watermark,
                    stored_gen_mtime_ns != current_gen_mtime_ns
                );
                return true;
            }
            Err(observed) => confirmed = observed,
        }
    }

    false
}

fn reset_stale_local_relay_offset_if_output_regressed(
    last_relayed_offset: &mut Option<u64>,
    last_observed_generation_mtime_ns: &mut Option<i64>,
    channel_id: ChannelId,
    tmux_session_name: &str,
    observed_output_end: u64,
    context: &str,
) -> bool {
    let Some(prev_offset) = *last_relayed_offset else {
        return false;
    };
    if observed_output_end >= prev_offset {
        return false;
    }

    let stored_gen_mtime_ns = last_observed_generation_mtime_ns.unwrap_or(0);
    let current_gen_mtime_ns = read_generation_file_mtime_ns(tmux_session_name);
    let new_offset = watermark_after_output_regression(
        stored_gen_mtime_ns,
        current_gen_mtime_ns,
        observed_output_end,
    );
    let new_local = if new_offset == 0 {
        // Fresh wrapper — clear the local watermark entirely so the next
        // tick walks the file from offset 0 (matches the global reset
        // semantics for cancel→respawn).
        None
    } else {
        Some(new_offset)
    };
    *last_relayed_offset = new_local;
    *last_observed_generation_mtime_ns = Some(current_gen_mtime_ns);

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] 👁 Reset stale tmux local relay offset for {} (channel {}, context={}, observed_output_end={}, stale_last_relayed={}, new_local_offset={:?}, generation_mtime_changed={})",
        tmux_session_name,
        channel_id.get(),
        context,
        observed_output_end,
        prev_offset,
        new_local,
        stored_gen_mtime_ns != current_gen_mtime_ns
    );
    true
}

fn advance_watcher_confirmed_end(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    committed_end_offset: u64,
    context: &'static str,
) {
    let relay_coord = shared.tmux_relay_coord(channel_id);
    let mut cur = relay_coord
        .confirmed_end_offset
        .load(std::sync::atomic::Ordering::Acquire);
    // #1270 codex P2 (round 4): capture the `.generation` mtime BEFORE
    // the CAS so the stored mtime reflects what was on disk when we
    // decided to label `committed_end_offset` as delivered. Reading after
    // the CAS opens a TOCTOU window where a fresh respawn writes a new
    // `.generation` between our advance and our marker store, then the
    // new mtime ends up paired with the OLD offset and the next
    // regression check mis-classifies the next fresh respawn as
    // same-wrapper rotation.
    let mtime_at_attempt = {
        let m = read_generation_file_mtime_ns(tmux_session_name);
        if m == 0 { None } else { Some(m) }
    };
    let mut won_advance = false;
    while cur < committed_end_offset {
        match relay_coord.confirmed_end_offset.compare_exchange(
            cur,
            committed_end_offset,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        ) {
            Ok(_) => {
                won_advance = true;
                break;
            }
            Err(observed) => cur = observed,
        }
    }
    // Pair the pre-CAS mtime with the offset only on a real advance. If
    // the loop exits because the stored watermark is already at/past
    // `committed_end_offset` (the stale-high watermark from an older
    // session — exactly the regression case this PR is trying to
    // recover from), refreshing the mtime would associate the OLD offset
    // with the NEW wrapper and break the next regression check
    // (PR #1271 round 3).
    if won_advance && let Some(mtime) = mtime_at_attempt {
        relay_coord
            .confirmed_end_generation_mtime_ns
            .store(mtime, std::sync::atomic::Ordering::Release);
    }
    let confirmed_end = relay_coord
        .confirmed_end_offset
        .load(std::sync::atomic::Ordering::Acquire);
    let confirmed_reached_current = confirmed_end >= committed_end_offset;
    record_watcher_invariant(
        confirmed_reached_current,
        Some(provider),
        channel_id,
        "tmux_confirmed_end_monotonic",
        context,
        "watcher confirmed_end_offset must reach the committed tmux output end",
        serde_json::json!({
            "current_offset": committed_end_offset,
            "confirmed_end": confirmed_end,
            "tmux_session_name": tmux_session_name,
        }),
    );
    debug_assert!(
        confirmed_reached_current,
        "watcher confirmed_end_offset must reach committed output end"
    );
}

async fn drain_watcher_output_tail_to_eof(
    output_path: &str,
    mut current_offset: u64,
) -> Result<u64, String> {
    loop {
        let read_more = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking({
                let path = output_path.to_string();
                let offset = current_offset;
                move || -> Result<(Vec<u8>, u64), String> {
                    use std::io::{Read as _, Seek as _};

                    let mut file =
                        std::fs::File::open(&path).map_err(|e| format!("open: {}", e))?;
                    file.seek(std::io::SeekFrom::Start(offset))
                        .map_err(|e| format!("seek: {}", e))?;
                    let mut buf = vec![0u8; 16384];
                    let n = file.read(&mut buf).map_err(|e| format!("read: {}", e))?;
                    buf.truncate(n);
                    Ok((buf, offset + n as u64))
                }
            }),
        )
        .await;

        match read_more {
            Ok(Ok(Ok((chunk, off)))) if !chunk.is_empty() => {
                current_offset = off;
            }
            Ok(Ok(Ok((_, off)))) => return Ok(off),
            Ok(Ok(Err(error))) => return Err(error),
            Ok(Err(error)) => return Err(format!("join error: {error}")),
            Err(_) => return Err("timeout reading tmux output tail".to_string()),
        }
    }
}

async fn drain_missing_inflight_dead_tmux_tail_to_eof(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    output_path: &str,
    current_offset: u64,
) -> u64 {
    match drain_watcher_output_tail_to_eof(output_path, current_offset).await {
        Ok(drained_offset) => {
            if drained_offset > current_offset {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 👁 missing-inflight dead-tmux drain advanced {} from offset {} to EOF {} before watcher shutdown",
                    tmux_session_name,
                    current_offset,
                    drained_offset
                );
            }
            advance_watcher_confirmed_end(
                shared,
                provider,
                channel_id,
                tmux_session_name,
                drained_offset,
                "src/services/discord/tmux.rs:missing_inflight_dead_tmux_tail_drain",
            );
            drained_offset
        }
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ missing-inflight dead-tmux drain failed for {} at offset {}: {}",
                tmux_session_name,
                current_offset,
                error
            );
            current_offset
        }
    }
}

/// #826 P1 #2 (option b): Decide which of the two offset watermarks
/// (`last_relayed_offset`, `last_enqueued_offset`) a watcher tick should
/// advance after attempting to deliver a terminal response.
///
///  - `last_relayed_offset` is the canonical "Discord has durably received
///    this byte range" watermark. It must advance ONLY on confirmed
///    foreground delivery (direct send or placeholder replace succeeded), or
///    on the notify-path fallback that reached Discord.
///  - `last_enqueued_offset` is the "outbox row committed" watermark. It
///    advances when the notify-bot outbox insert succeeded — the outbox
///    worker owns delivery + retry from there. Prevents re-enqueue of the
///    same range on the next tick without conflating staging with delivery.
///
/// Both watermarks advance in lock-step on genuine delivery so a later
/// dedupe check (which takes their max) sees a single unified floor.
///
/// Pure function extracted for regression-test coverage of the offset-commit
/// gate; the runtime version lives inline in the watcher loop because it is
/// intertwined with other relay bookkeeping.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct OffsetAdvanceDecision {
    pub advance_relayed: bool,
    pub advance_enqueued: bool,
}

#[inline]
pub(super) fn notify_path_offset_advance_decision(
    has_current_response: bool,
    enqueue_succeeded: bool,
    direct_send_delivered: bool,
) -> OffsetAdvanceDecision {
    if direct_send_delivered {
        // Confirmed foreground delivery. Lift both watermarks.
        return OffsetAdvanceDecision {
            advance_relayed: true,
            advance_enqueued: true,
        };
    }
    if enqueue_succeeded {
        // Staged on the outbox — advance the enqueue watermark to dedupe the
        // next tick, but leave the canonical relayed watermark alone.
        return OffsetAdvanceDecision {
            advance_relayed: false,
            advance_enqueued: true,
        };
    }
    if !has_current_response {
        // Empty turn — advance both in lock-step (the original single-offset
        // behaviour) so the watcher doesn't spin on this range.
        return OffsetAdvanceDecision {
            advance_relayed: true,
            advance_enqueued: true,
        };
    }
    // Nothing delivered, nothing staged — leave BOTH watermarks untouched so
    // the next tick can try again.
    OffsetAdvanceDecision::default()
}

/// #826: Build the dedupe session_key for a background-trigger outbox row.
/// Includes the tmux output offset and a short content hash so distinct
/// completions land as separate rows (different offsets ⇒ different keys)
/// while a retry of the exact same range within the dedupe window (same
/// offset + identical content) collapses into one. The resulting key is
/// compact (≤~64 chars) and safe to use as a dedupe column.
///
/// Pure function so the #897 counter-model review P1 (dedupe reason_code
/// AND session_key must BOTH be present for the lifecycle dedupe to arm)
/// has a testable contract.
#[inline]
pub(super) fn build_bg_trigger_session_key(
    channel_id: u64,
    data_start_offset: u64,
    content: &str,
) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    format!(
        "bg_trigger:ch:{channel_id}:off:{data_start_offset}:h:{:016x}",
        hasher.finish()
    )
}

/// #826: Enqueue a background-trigger turn's terminal response on the
/// notify-bot outbox so it reaches the channel without going through the
/// command bot. The notify-bot is dropped at the intake gate, which is what
/// keeps the auto-trigger path from feeding back into a new turn.
///
/// **Storage backend** (#897 counter-model re-review round 2 Medium):
/// matches `turn_bridge::enqueue_headless_delivery`'s priority —
/// `pg_pool` first when available (primary production storage), falling
/// back to the SQLite `Db` when only the legacy backend is wired in.
/// Without this, a PG-backed runtime would reach the old SQLite-only
/// code path with `Db::None` and silently fall back to direct-send,
/// bypassing the new dedupe / failure-reconcile behaviour entirely.
///
/// **Dedupe** (#897 round 1 P1 #3): both `reason_code` and `session_key`
/// are set so the lifecycle-notification dedupe in
/// `message_outbox::enqueue` can arm. `session_key` encodes
/// `channel_id + data_start_offset + content hash`, so:
///   * Distinct background completions in the same channel produce distinct
///     session_keys (different offsets or different content) → each lands
///     as its own outbox row.
///   * A duplicate retry of the exact same tmux range within the dedupe TTL
///     (same offset, identical content) collapses into the single existing
///     row, which guards against the watcher re-enqueuing while the outbox
///     worker is still delivering.
///   * The dedupe lookup filters out `status='failed'` rows, so a permanently
///     failed prior attempt is NOT allowed to suppress a fresh re-stage.
///
/// The PG path currently does INSERT without a per-tick dedupe query (the
/// SQLite-only `enqueue` helper lives in `message_outbox.rs`; porting it
/// to a shared sqlx/rusqlite interface is tracked separately). Same-row
/// dedupe on the PG side is still achievable via a `UNIQUE(reason_code,
/// session_key, status) WHERE status != 'failed'` partial index, but
/// that's a schema change outside this PR's scope. Follow-up tracked in
/// #898-family.
///
/// Returns `false` only when BOTH backends are unavailable or their
/// insert fails — the caller falls back to a direct command-bot send in
/// that case so the message is never silently lost.
pub(super) async fn enqueue_background_trigger_response_to_notify_outbox(
    pg_pool: Option<&sqlx::PgPool>,
    db: Option<&crate::db::Db>,
    channel_id: ChannelId,
    content: &str,
    data_start_offset: u64,
) -> bool {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return true;
    }
    let target = format!("channel:{}", channel_id.get());
    let session_key = build_bg_trigger_session_key(channel_id.get(), data_start_offset, content);

    // #897 round-3 High: when `pg_pool` is configured, the outbox worker
    // drains PG EXCLUSIVELY. Writing a row to SQLite as a "fallback" would
    // silently black-hole the message because no worker polls it in that
    // mode. On PG insert failure we return `false` so the caller falls
    // back to a DIRECT Discord send (the only path that guarantees
    // delivery in PG mode) rather than papering over the failure with an
    // undeliverable SQLite row. Mirrors
    // `turn_bridge::enqueue_headless_delivery` which also refuses to fall
    // back to SQLite when PG is configured.
    if let Some(pool) = pg_pool {
        return match sqlx::query(
            "INSERT INTO message_outbox
             (target, content, bot, source, reason_code, session_key)
             VALUES ($1, $2, 'notify', 'system', 'bg_trigger.auto_turn', $3)",
        )
        .bind(target.as_str())
        .bind(content)
        .bind(session_key.as_str())
        .execute(pool)
        .await
        {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(
                    "background-trigger postgres outbox insert failed for channel {}: {}",
                    channel_id,
                    error
                );
                false
            }
        };
    }

    let _ = (db, session_key);
    false
}

/// #897 counter-model review P1 #2: Find permanently-failed notify-bot
/// outbox rows that originated from this watcher's background-trigger
/// enqueues, extract the tmux offsets that caused them, and delete the
/// rows so they don't accumulate. Returns the MINIMUM observed
/// `data_start_offset` encoded in `session_key`, which the caller uses to
/// roll `last_enqueued_offset` back and re-stage the same tmux range on
/// the next watcher tick.
///
/// **Storage backend** (#897 round 2 Medium): prefers `pg_pool` when
/// available, falling back to the SQLite `Db` — mirrors the enqueue
/// path's ordering so a PG-backed runtime actually reconciles its own
/// failed rows instead of silently skipping when `Db::None`.
///
/// Why this is safe to re-stage:
/// * `message_outbox::enqueue`'s lifecycle dedupe filters out rows where
///   `status='failed'`, so re-inserting at the same session_key produces a
///   fresh pending row rather than collapsing into the dead one.
/// * We delete the failed rows here so they don't pollute `SELECT *`
///   queries or eat unbounded table space.
///
/// Without this reconciliation a single transient notify-bot or Discord
/// failure permanently suppresses re-enqueue for the remainder of the
/// watcher's lifetime — the exact P1 gap the counter-model reviewer
/// flagged. See PR #897.
async fn reconcile_failed_bg_trigger_enqueues_for_channel(
    pg_pool: Option<&sqlx::PgPool>,
    db: Option<&crate::db::Db>,
    channel_id: ChannelId,
) -> Option<u64> {
    let target = format!("channel:{}", channel_id.get());

    // #897 round-3 High: when `pg_pool` is configured it is the ONLY
    // authoritative store. Consulting SQLite as a "fallback" on PG
    // failure or on an empty PG result would surface rows from a legacy
    // test/dev database that the outbox worker never produced, and worse
    // could delete rows written by a prior run. On PG error we surface
    // `None` so the next poll retries; there is no data-safe fallback.
    if let Some(pool) = pg_pool {
        let rows_res = sqlx::query_as::<_, (i64, Option<String>)>(
            "SELECT id, session_key FROM message_outbox
             WHERE target = $1
               AND bot = 'notify'
               AND source = 'system'
               AND reason_code = 'bg_trigger.auto_turn'
               AND status = 'failed'",
        )
        .bind(target.as_str())
        .fetch_all(pool)
        .await;

        return match rows_res {
            Ok(rows) if !rows.is_empty() => {
                let mut min_offset: Option<u64> = None;
                for (_, session_key) in &rows {
                    if let Some(offset) = session_key
                        .as_deref()
                        .and_then(parse_bg_trigger_offset_from_session_key)
                    {
                        min_offset = Some(min_offset.map(|m| m.min(offset)).unwrap_or(offset));
                    }
                }
                for (id, _) in &rows {
                    if let Err(error) = sqlx::query("DELETE FROM message_outbox WHERE id = $1")
                        .bind(id)
                        .execute(pool)
                        .await
                    {
                        tracing::warn!(
                            "failed to delete reconciled bg_trigger row {}: {}",
                            id,
                            error
                        );
                    }
                }
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ♻ reconciled {} failed bg_trigger outbox row(s) for channel {} (min offset: {:?}) [pg]",
                    rows.len(),
                    channel_id,
                    min_offset,
                );
                min_offset
            }
            Ok(_) => None,
            Err(error) => {
                tracing::warn!(
                    "postgres bg_trigger reconcile query failed for channel {}: {}",
                    channel_id,
                    error
                );
                None
            }
        };
    }

    let _ = db;
    None
}

/// Pure helper: extract the `data_start_offset` encoded in a
/// background-trigger `session_key`. Format produced by
/// `build_bg_trigger_session_key` is `bg_trigger:ch:{id}:off:{offset}:h:{hash16}`.
/// Returns `None` for malformed keys so the caller can safely ignore
/// outbox rows whose session_key no longer conforms to the expected shape
/// (e.g. future schema changes or hand-written operator rows).
#[inline]
pub(super) fn parse_bg_trigger_offset_from_session_key(session_key: &str) -> Option<u64> {
    let after_off = session_key.split(":off:").nth(1)?;
    let off_str = after_off.split(':').next()?;
    off_str.parse::<u64>().ok()
}

/// Pure helper for the watermark-rollback policy (#897 P1 #2). Given the
/// watcher's current `last_enqueued_offset` and the minimum offset from a
/// reconciled outbox failure, return the new watermark that allows
/// re-emission of the failed range on the next watcher tick while
/// preserving progress past other, unaffected ranges.
///
/// Rules:
/// 1. `None → None`: nothing staged, nothing to roll back.
/// 2. Current ≤ reconciled: the watermark is already at or below the
///    failed offset, so the next visit will naturally re-emit that range.
/// 3. Current > reconciled: pull back to `reconciled.saturating_sub(1)` so
///    the dedupe floor `max(relayed, enqueued)` permits
///    `data_start_offset < prev_offset` evaluation at the exact failed
///    offset. Using `saturating_sub` guards against reconciled=0.
#[inline]
pub(super) fn rollback_enqueued_offset_for_reconciled_failures(
    last_enqueued_offset: Option<u64>,
    reconciled_min_offset: u64,
) -> Option<u64> {
    match last_enqueued_offset {
        None => None,
        Some(current) if current <= reconciled_min_offset => Some(current),
        Some(_) => Some(reconciled_min_offset.saturating_sub(1)),
    }
}

fn watcher_should_yield_to_active_bridge_turn(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    data_start_offset: u64,
    current_offset: u64,
) -> bool {
    let state = super::inflight::load_inflight_state(provider, channel_id.get());
    watcher_should_yield_to_inflight_state(
        state.as_ref(),
        tmux_session_name,
        data_start_offset,
        current_offset,
    )
}

fn watcher_should_yield_to_inflight_state(
    state: Option<&super::inflight::InflightTurnState>,
    tmux_session_name: &str,
    data_start_offset: u64,
    current_offset: u64,
) -> bool {
    let Some(state) = state else {
        return false;
    };

    if state.tmux_session_name.as_deref() != Some(tmux_session_name) {
        return false;
    }
    if state.watcher_owns_live_relay {
        return false;
    }

    let turn_start_offset = state.turn_start_offset.unwrap_or(state.last_offset);
    data_start_offset <= turn_start_offset && turn_start_offset < current_offset
}

async fn reconcile_orphan_suppressed_placeholder_for_restored_watcher(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
) {
    let has_active_turn = shared.mailbox(channel_id).has_active_turn().await;
    let Some(state) = super::inflight::load_inflight_state(provider, channel_id.get()) else {
        return;
    };
    let ctx = PlaceholderSuppressContext {
        origin: PlaceholderSuppressOrigin::OrphanRestartHandoff,
        placeholder_msg_id: Some(MessageId::new(state.current_msg_id)),
        response_sent_offset: state.response_sent_offset,
        last_edit_text: "",
        inflight_state: Some(&state),
        has_active_turn,
        tmux_session_name,
        task_notification_kind: None,
        reattach_offset_match: false,
    };
    let decision = decide_placeholder_suppression(&ctx);
    let is_edit = matches!(decision, PlaceholderSuppressDecision::Edit(_));
    let msg_id = MessageId::new(state.current_msg_id);
    apply_placeholder_suppression(
        http,
        channel_id,
        shared,
        provider,
        state.tmux_session_name.as_deref().unwrap_or("unknown"),
        ctx.placeholder_msg_id,
        ctx.origin,
        decision,
        None,
    )
    .await;
    if is_edit {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ✓ reconciled orphan suppressed placeholder for channel {} msg {}",
            channel_id.get(),
            msg_id.get()
        );
    }
}

fn persist_watcher_stream_progress(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    current_msg_id: Option<MessageId>,
    full_response: &str,
    response_sent_offset: usize,
    current_tool_line: Option<&str>,
    prev_tool_status: Option<&str>,
    task_notification_kind: Option<TaskNotificationKind>,
) {
    let Some(mut inflight) = super::inflight::load_inflight_state(provider, channel_id.get())
    else {
        return;
    };
    if inflight.tmux_session_name.as_deref() != Some(tmux_session_name) {
        return;
    }

    if let Some(msg_id) = current_msg_id {
        inflight.current_msg_id = msg_id.get();
    }
    let normalized_response_sent_offset =
        normalize_response_sent_offset(full_response, response_sent_offset);
    let monotonic_offset = normalized_response_sent_offset >= inflight.response_sent_offset;
    record_watcher_invariant(
        monotonic_offset,
        Some(provider),
        channel_id,
        "response_sent_offset_monotonic",
        "src/services/discord/tmux.rs:persist_watcher_stream_progress",
        "watcher response_sent_offset must not move backwards",
        serde_json::json!({
            "previous": inflight.response_sent_offset,
            "next": normalized_response_sent_offset,
            "tmux_session_name": tmux_session_name,
        }),
    );
    debug_assert!(
        monotonic_offset,
        "watcher response_sent_offset must not move backwards"
    );
    let offset_in_bounds = normalized_response_sent_offset <= full_response.len()
        && full_response.is_char_boundary(normalized_response_sent_offset);
    record_watcher_invariant(
        offset_in_bounds,
        Some(provider),
        channel_id,
        "response_sent_offset_in_bounds",
        "src/services/discord/tmux.rs:persist_watcher_stream_progress",
        "watcher response_sent_offset must stay on a full_response boundary",
        serde_json::json!({
            "next": normalized_response_sent_offset,
            "full_response_len": full_response.len(),
            "tmux_session_name": tmux_session_name,
        }),
    );
    debug_assert!(
        offset_in_bounds,
        "watcher response_sent_offset must stay on a full_response boundary"
    );
    inflight.full_response = full_response.to_string();
    inflight.response_sent_offset = normalized_response_sent_offset;
    inflight.current_tool_line = current_tool_line.map(str::to_string);
    inflight.prev_tool_status = prev_tool_status.map(str::to_string);
    if task_notification_kind.is_some() {
        inflight.task_notification_kind = task_notification_kind;
    }
    let _ = super::inflight::save_inflight_state(&inflight);
}

async fn finish_restored_watcher_active_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    finish_mailbox_on_completion: bool,
    stop_source: &'static str,
) {
    if !finish_mailbox_on_completion {
        return;
    }

    let finish = super::mailbox_finish_turn(shared, provider, channel_id).await;
    if let Some(token) = finish.removed_token {
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let _ = shared.global_active.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |current| current.checked_sub(1),
        );
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

/// Background watcher that continuously tails a tmux output file.
/// When Claude produces output from terminal input (not Discord), relay it to Discord.
pub(super) async fn tmux_output_watcher(
    channel_id: ChannelId,
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    output_path: String,
    tmux_session_name: String,
    initial_offset: u64,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    paused: Arc<std::sync::atomic::AtomicBool>,
    resume_offset: Arc<std::sync::Mutex<Option<u64>>>,
    pause_epoch: Arc<std::sync::atomic::AtomicU64>,
    turn_delivered: Arc<std::sync::atomic::AtomicBool>,
    last_heartbeat_ts_ms: Arc<std::sync::atomic::AtomicI64>,
) {
    tmux_output_watcher_with_restore(
        channel_id,
        http,
        shared,
        output_path,
        tmux_session_name,
        initial_offset,
        cancel,
        paused,
        resume_offset,
        pause_epoch,
        turn_delivered,
        last_heartbeat_ts_ms,
        None,
    )
    .await;
}

/// Background watcher variant used by restart recovery to continue editing an
/// existing streaming placeholder instead of creating a new one.
pub(super) async fn tmux_output_watcher_with_restore(
    channel_id: ChannelId,
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    output_path: String,
    tmux_session_name: String,
    initial_offset: u64,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    paused: Arc<std::sync::atomic::AtomicBool>,
    resume_offset: Arc<std::sync::Mutex<Option<u64>>>,
    pause_epoch: Arc<std::sync::atomic::AtomicU64>,
    turn_delivered: Arc<std::sync::atomic::AtomicBool>,
    last_heartbeat_ts_ms: Arc<std::sync::atomic::AtomicI64>,
    restored_turn: Option<RestoredWatcherTurn>,
) {
    use std::io::{Read, Seek, SeekFrom};

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 👁 tmux watcher started for #{tmux_session_name} at offset {initial_offset}"
    );

    // #1134: mark the attach moment so `record_first_relay` (below) can compute
    // attach→first-relay latency. Single instrumentation point covers all
    // spawn sites (recovery_engine, turn_bridge, tmux self-recovery).
    crate::services::observability::watcher_latency::record_attach(channel_id.get());

    let (watcher_provider, watcher_channel_name) =
        parse_provider_and_channel_from_tmux_name(&tmux_session_name).unwrap_or((
            crate::services::provider::ProviderKind::Claude,
            String::new(),
        ));
    let watcher_thread_channel_id =
        super::adk_session::parse_thread_channel_id_from_name(&watcher_channel_name);
    let mut current_offset = initial_offset;
    let input_fifo_path = super::turn_bridge::tmux_runtime_paths(&tmux_session_name).1;
    // #1216: leftover JSONL bytes from a buffer that contained more than one
    // turn-terminating event. `process_watcher_lines` now stops at the first
    // `result`/auth/overload event and leaves the rest in the buffer; this
    // outer-scope `all_data` carries that leftover into the next watcher loop
    // iteration so the next turn does not need to wait for fresh disk reads.
    let mut all_data = String::new();
    let mut prompt_too_long_killed = false;
    let mut turn_result_relayed = false;
    let mut last_activity_heartbeat_at: Option<std::time::Instant> = None;
    // #1137: 1-shot guard so the "post-terminal-success continuation" log
    // is emitted exactly once per dispatch. Real-world traces (codex
    // G2/G3/G4 on 2026-04-22T23:34:13Z) showed multi-second continuation
    // bursts; logging every chunk would spam the timeline.
    let mut post_terminal_continuation_logged = false;
    let mut restored_turn = restored_turn;
    // Guard against duplicate relay: track the offset from which the last relay was sent.
    // If the outer loop circles back and current_offset hasn't advanced past this point,
    // the relay is suppressed.
    // Initialize from persisted inflight state so replacement watcher instances skip
    // already-delivered output (fixes double-reply on stale watcher replacement).
    // #1270: load both the persisted offset AND its matching
    // `.generation` mtime so a replacement watcher can correctly classify
    // an output regression on restored state. When we have a persisted
    // mtime, it labels the wrapper that produced the persisted offset:
    //   - matches current `.generation` mtime → same wrapper after
    //     `truncate_jsonl_head_safe` → pin to EOF (don't re-flood
    //     surviving content; codex P2 on PR #1271).
    //   - differs from current `.generation` mtime → cancel→respawn into
    //     the same session name → reset to 0 to pick up the fresh
    //     response.
    // When the persisted state predates this field (legacy `None`), we
    // fall back to "no baseline known" semantics — the regression check
    // treats it as a first observation and resets to 0, which is the
    // safer choice for not silently dropping a fresh response.
    let restored_inflight = parse_provider_and_channel_from_tmux_name(&tmux_session_name)
        .and_then(|(pk, _)| super::inflight::load_inflight_state(&pk, channel_id.get()));
    let mut last_relayed_offset: Option<u64> = restored_inflight
        .as_ref()
        .and_then(|s| s.last_watcher_relayed_offset);
    let mut last_observed_generation_mtime_ns: Option<i64> = restored_inflight
        .as_ref()
        .and_then(|s| s.last_watcher_relayed_generation_mtime_ns);
    if let Ok(meta) = std::fs::metadata(&output_path) {
        let observed_output_end = meta.len();
        reset_stale_relay_watermark_if_output_regressed(
            &shared,
            channel_id,
            &tmux_session_name,
            observed_output_end,
            "watcher_start",
        );
        reset_stale_local_relay_offset_if_output_regressed(
            &mut last_relayed_offset,
            &mut last_observed_generation_mtime_ns,
            channel_id,
            &tmux_session_name,
            observed_output_end,
            "watcher_start",
        );
    }
    // Rolling-size-cap rotation state. The watcher loop spins predictably
    // (~500ms sleeps) so a mod-N gate on an iteration counter gives a
    // regular-ish cadence for the size check without hitting the fs every
    // spin. See issue #892.
    let mut rotation_tick: u32 = 0;
    const ROTATION_CHECK_EVERY: u32 = 60; // ~30s at 500ms base cadence

    'watcher_loop: loop {
        last_heartbeat_ts_ms.store(
            super::tmux_watcher_now_ms(),
            std::sync::atomic::Ordering::Release,
        );
        // Always consume resume_offset first — the turn bridge may have set it
        // between the previous paused check and now, so reading it here prevents
        // the watcher from using a stale current_offset after unpausing.
        if let Some(new_offset) = resume_offset.lock().ok().and_then(|mut g| g.take()) {
            current_offset = new_offset;
            // If the bridge already delivered the previous turn, treat this resume
            // point as already consumed once so the watcher doesn't re-relay the
            // same batch after unpausing.
            last_relayed_offset = if turn_delivered.load(Ordering::Relaxed) {
                Some(new_offset)
            } else {
                None
            };
            // #1275 P2 #2: snapshot the current `.generation` mtime alongside
            // the resumed offset. Without this, the local mtime baseline stays
            // at whatever the previous setter left it (often `None` for
            // restored offsets that haven't gone through a relay/rotation
            // cycle yet). A later same-wrapper jsonl rotation would then take
            // the fresh-wrapper branch in `watermark_after_output_regression`,
            // clear `last_relayed_offset`, and re-relay surviving bytes.
            // Pair the mtime with the offset only when we keep the offset (the
            // turn_delivered branch); otherwise the next loop walks from 0
            // anyway and a baseline would be misleading.
            if last_relayed_offset.is_some() {
                last_observed_generation_mtime_ns =
                    Some(read_generation_file_mtime_ns(&tmux_session_name));
            }
            // Clear turn_delivered after preserving the duplicate-relay guard so
            // future turns beyond this resume point can be relayed normally.
            turn_delivered.store(false, Ordering::Relaxed);
        }

        // Check cancel or global shutdown (both exit quietly, no "session ended" message)
        if cancel.load(Ordering::Relaxed) || shared.shutting_down.load(Ordering::Relaxed) {
            break;
        }

        // If paused (Discord handler is processing its own turn), keep the
        // liveness monitor active so a dead pane still clears watcher state.
        if paused.load(Ordering::Relaxed) {
            match tmux_liveness_decision(
                cancel.load(Ordering::Relaxed),
                shared.shutting_down.load(Ordering::Relaxed),
                probe_tmux_session_liveness(&tmux_session_name).await,
            ) {
                TmuxLivenessDecision::Continue => {
                    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                    continue;
                }
                TmuxLivenessDecision::QuietStop => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 👁 tmux session {tmux_session_name} ended during shutdown, exiting quietly"
                    );
                    break;
                }
                TmuxLivenessDecision::TmuxDied => {
                    handle_tmux_watcher_observed_death(
                        channel_id,
                        &http,
                        &shared,
                        &tmux_session_name,
                        &output_path,
                        &watcher_provider,
                        prompt_too_long_killed,
                        turn_result_relayed,
                    )
                    .await;
                    break;
                }
            }
        }

        // Periodic size-cap rotation for the session jsonl. Running this off
        // the watcher loop keeps the wrapper child process simple while
        // still enforcing a 20 MB soft cap (see issue #892).
        rotation_tick = rotation_tick.wrapping_add(1);

        if rotation_tick % ROTATION_CHECK_EVERY == 0 {
            let path = output_path.clone();
            let session = tmux_session_name.clone();
            let prev_offset = current_offset;
            let rotation = tokio::task::spawn_blocking(move || {
                crate::services::tmux_common::truncate_jsonl_head_safe(
                    &path,
                    crate::services::tmux_common::JSONL_SIZE_CAP_BYTES,
                    crate::services::tmux_common::JSONL_TARGET_KEEP_BYTES,
                )
                .map_err(|e| e.to_string())
            })
            .await
            .unwrap_or_else(|e| Err(format!("join error: {e}")));
            match rotation {
                Ok(Some(new_size)) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ✂ rotated jsonl for {} — new size {} bytes (was beyond cap)",
                        session,
                        new_size
                    );
                    // File was rewritten from the head: reset reader offset
                    // so the watcher doesn't seek past the new EOF. Also
                    // reset the duplicate-relay guard.
                    if prev_offset > new_size {
                        current_offset = new_size;
                        last_relayed_offset = Some(new_size);
                        // #1270 codex P2: snapshot the current `.generation`
                        // mtime alongside the local offset so a later regression
                        // check has a real baseline. Without this, the local
                        // mtime would still be `None` after a normal relay path
                        // and any subsequent regression would misclassify
                        // same-wrapper rotation as fresh-respawn and clear the
                        // local offset to None — re-relaying surviving content.
                        last_observed_generation_mtime_ns =
                            Some(read_generation_file_mtime_ns(&tmux_session_name));
                        reset_stale_relay_watermark_if_output_regressed(
                            &shared,
                            channel_id,
                            &tmux_session_name,
                            new_size,
                            "jsonl_rotation",
                        );
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!("  [{ts}] ⚠ jsonl rotation failed for {}: {}", session, e);
                }
            }
        }

        // Snapshot pause epoch — if this changes later, a Discord turn claimed this data
        let epoch_snapshot = pause_epoch.load(Ordering::Relaxed);

        // Try to read new data from output file
        let read_result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking({
                let path = output_path.clone();
                let offset = current_offset;
                move || -> Result<(Vec<u8>, u64), String> {
                    let mut file =
                        std::fs::File::open(&path).map_err(|e| format!("open: {}", e))?;
                    file.seek(SeekFrom::Start(offset))
                        .map_err(|e| format!("seek: {}", e))?;
                    let mut buf = vec![0u8; 16384];
                    let n = file.read(&mut buf).map_err(|e| format!("read: {}", e))?;
                    buf.truncate(n);
                    Ok((buf, offset + n as u64))
                }
            }),
        )
        .await;

        let (data, new_offset) = match read_result {
            Ok(Ok(Ok((data, off)))) => (data, off),
            _ => {
                match tmux_liveness_decision(
                    cancel.load(Ordering::Relaxed),
                    shared.shutting_down.load(Ordering::Relaxed),
                    probe_tmux_session_liveness(&tmux_session_name).await,
                ) {
                    TmuxLivenessDecision::Continue => {
                        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                        continue;
                    }
                    TmuxLivenessDecision::QuietStop => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 👁 tmux session {tmux_session_name} ended during shutdown, exiting quietly"
                        );
                        break;
                    }
                    TmuxLivenessDecision::TmuxDied => {
                        handle_tmux_watcher_observed_death(
                            channel_id,
                            &http,
                            &shared,
                            &tmux_session_name,
                            &output_path,
                            &watcher_provider,
                            prompt_too_long_killed,
                            turn_result_relayed,
                        )
                        .await;
                        break;
                    }
                }
            }
        };

        let bytes_available = data.len().saturating_add(all_data.len());
        let poll_decision = if bytes_available == 0 {
            watcher_output_poll_decision(
                bytes_available,
                Some(tmux_liveness_decision(
                    cancel.load(Ordering::Relaxed),
                    shared.shutting_down.load(Ordering::Relaxed),
                    probe_tmux_session_liveness(&tmux_session_name).await,
                )),
            )
        } else {
            watcher_output_poll_decision(bytes_available, None)
        };
        match poll_decision {
            WatcherOutputPollDecision::DrainOutput => {}
            WatcherOutputPollDecision::Continue => {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                continue;
            }
            WatcherOutputPollDecision::QuietStop => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 👁 tmux session {tmux_session_name} ended during shutdown, exiting quietly"
                );
                break;
            }
            WatcherOutputPollDecision::TmuxDied => {
                handle_tmux_watcher_observed_death(
                    channel_id,
                    &http,
                    &shared,
                    &tmux_session_name,
                    &output_path,
                    &watcher_provider,
                    prompt_too_long_killed,
                    turn_result_relayed,
                )
                .await;
                break;
            }
        }

        // We got new data while not paused — this means terminal input triggered a response
        let data_start_offset = current_offset; // offset where this read batch started
        current_offset = new_offset;
        // #1137: surface a single warning when output keeps arriving after a
        // terminal-success relay. The watcher will keep running (the legacy
        // single-event exit was the bug); this log makes the continuation
        // observable in the operational timeline.
        if turn_result_relayed && !post_terminal_continuation_logged {
            post_terminal_continuation_logged = true;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 👁 post-terminal-success continuation: new output arrived for {tmux_session_name} after terminal success (offset {data_start_offset} -> {new_offset}); watcher staying alive"
            );
        }
        maybe_refresh_watcher_activity_heartbeat(
            None::<&crate::db::Db>,
            shared.pg_pool.as_ref(),
            &shared.token_hash,
            &watcher_provider,
            &tmux_session_name,
            watcher_thread_channel_id,
            &mut last_activity_heartbeat_at,
        );

        // Collect the full turn: keep reading until we see a "result" event.
        // #1216: append to the outer-scope `all_data` so any leftover from a
        // previous iteration (multi-turn buffer split at the first `result`)
        // is processed before the new disk read.
        all_data.push_str(&String::from_utf8_lossy(&data));
        let mut state = StreamLineState::new();
        let stream_seed = watcher_stream_seed(restored_turn.take());
        let mut full_response = stream_seed.full_response;
        let mut tool_state = WatcherToolState::new();

        // Create a placeholder message for real-time status display
        const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let mut spin_idx: usize = 0;
        let mut placeholder_msg_id: Option<serenity::MessageId> = stream_seed.placeholder_msg_id;
        let mut last_edit_text = stream_seed.last_edit_text;
        let mut response_sent_offset = stream_seed.response_sent_offset;
        let finish_mailbox_on_completion = stream_seed.finish_mailbox_on_completion;
        let mut monitor_auto_turn_claimed = false;
        let mut monitor_auto_turn_deferred = false;
        let mut monitor_auto_turn_finished = false;
        // #1009: 1-shot tracker for the monitor-auto-turn preamble hint so the
        // hint text is emitted exactly once per watcher turn frame.
        let mut monitor_auto_turn_preamble_injected = false;

        // Process any complete lines we already have
        let initial_outcome = process_watcher_lines(
            &mut all_data,
            &mut state,
            &mut full_response,
            &mut tool_state,
        );
        let mut found_result = initial_outcome.found_result;
        let mut is_prompt_too_long = initial_outcome.is_prompt_too_long;
        let mut is_auth_error = initial_outcome.is_auth_error;
        let mut auth_error_message = initial_outcome.auth_error_message;
        let mut is_provider_overloaded = initial_outcome.is_provider_overloaded;
        let mut provider_overload_message = initial_outcome.provider_overload_message;
        let mut stale_resume_detected = initial_outcome.stale_resume_detected;
        let mut task_notification_kind = stream_seed.task_notification_kind;
        if let Some(kind) = initial_outcome.task_notification_kind {
            task_notification_kind = merge_task_notification_kind(task_notification_kind, kind);
        }
        let post_terminal_success_continuation_flush =
            should_flush_post_terminal_success_continuation(
                turn_result_relayed,
                found_result,
                &full_response,
            );
        if post_terminal_success_continuation_flush {
            found_result = true;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 👁 post-terminal-success continuation: flushing relayed output for {tmux_session_name} immediately (offset {data_start_offset} -> {current_offset})"
            );
        }
        if matches!(
            task_notification_kind,
            Some(TaskNotificationKind::MonitorAutoTurn)
        ) {
            let start = start_monitor_auto_turn_when_available(
                &shared,
                &watcher_provider,
                channel_id,
                data_start_offset,
                cancel.as_ref(),
            )
            .await;
            monitor_auto_turn_claimed = start.acquired;
            monitor_auto_turn_deferred = monitor_auto_turn_deferred || start.deferred;
            if !start.acquired {
                all_data.clear();
                continue;
            }
            ensure_monitor_auto_turn_inflight(
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                &output_path,
                &input_fifo_path,
                state.last_session_id.as_deref(),
                data_start_offset,
                current_offset,
            );
            if let Some(hint) =
                consume_monitor_auto_turn_preamble_once(&mut monitor_auto_turn_preamble_injected)
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔔 monitor auto-turn preamble hint injected (channel {}): {}",
                    channel_id.get(),
                    hint
                );
            }
        }

        // Keep reading until result or timeout
        // Check if a Discord turn claimed this data since our epoch snapshot
        let epoch_changed = pause_epoch.load(Ordering::Relaxed) != epoch_snapshot;
        let mut was_paused = paused.load(Ordering::Relaxed) || epoch_changed;
        if was_paused && !monitor_auto_turn_deferred {
            // A Discord turn took over — discard what we read
            all_data.clear();
            continue;
        }
        if !found_result {
            let turn_start = tokio::time::Instant::now();
            let turn_timeout = super::turn_watchdog_timeout();
            let mut last_status_update = tokio::time::Instant::now();
            let mut ready_for_input_tracker =
                crate::services::provider::ReadyForInputIdleTracker::default();
            let mut last_ready_probe_at: Option<std::time::Instant> = None;
            let mut last_liveness_probe_at = tokio::time::Instant::now();
            let mut tmux_death_observed = false;
            let mut ready_for_input_failure_notice: Option<String> = None;
            let mut streaming_suppressed_by_recent_stop = false;

            while !found_result && turn_start.elapsed() < turn_timeout {
                if cancel.load(Ordering::Relaxed) || shared.shutting_down.load(Ordering::Relaxed) {
                    break;
                }
                if paused.load(Ordering::Relaxed) {
                    was_paused = true;
                    break;
                }

                let read_more = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    tokio::task::spawn_blocking({
                        let path = output_path.clone();
                        let offset = current_offset;
                        move || -> Result<(Vec<u8>, u64), String> {
                            let mut file =
                                std::fs::File::open(&path).map_err(|e| format!("open: {}", e))?;
                            file.seek(SeekFrom::Start(offset))
                                .map_err(|e| format!("seek: {}", e))?;
                            let mut buf = vec![0u8; 16384];
                            let n = file.read(&mut buf).map_err(|e| format!("read: {}", e))?;
                            buf.truncate(n);
                            Ok((buf, offset + n as u64))
                        }
                    }),
                )
                .await;

                match read_more {
                    Ok(Ok(Ok((chunk, off)))) if !chunk.is_empty() => {
                        current_offset = off;
                        maybe_refresh_watcher_activity_heartbeat(
                            None::<&crate::db::Db>,
                            shared.pg_pool.as_ref(),
                            &shared.token_hash,
                            &watcher_provider,
                            &tmux_session_name,
                            watcher_thread_channel_id,
                            &mut last_activity_heartbeat_at,
                        );
                        ready_for_input_tracker.record_output();
                        all_data.push_str(&String::from_utf8_lossy(&chunk));
                        let outcome = process_watcher_lines(
                            &mut all_data,
                            &mut state,
                            &mut full_response,
                            &mut tool_state,
                        );
                        found_result = found_result || outcome.found_result;
                        is_prompt_too_long = is_prompt_too_long || outcome.is_prompt_too_long;
                        is_auth_error = is_auth_error || outcome.is_auth_error;
                        if auth_error_message.is_none() {
                            auth_error_message = outcome.auth_error_message;
                        }
                        is_provider_overloaded =
                            is_provider_overloaded || outcome.is_provider_overloaded;
                        stale_resume_detected =
                            stale_resume_detected || outcome.stale_resume_detected;
                        if let Some(kind) = outcome.task_notification_kind {
                            task_notification_kind =
                                merge_task_notification_kind(task_notification_kind, kind);
                        }
                        if matches!(
                            task_notification_kind,
                            Some(TaskNotificationKind::MonitorAutoTurn)
                        ) {
                            if !monitor_auto_turn_claimed {
                                let start = start_monitor_auto_turn_when_available(
                                    &shared,
                                    &watcher_provider,
                                    channel_id,
                                    data_start_offset,
                                    cancel.as_ref(),
                                )
                                .await;
                                monitor_auto_turn_claimed = start.acquired;
                                monitor_auto_turn_deferred =
                                    monitor_auto_turn_deferred || start.deferred;
                                if !start.acquired {
                                    was_paused = true;
                                    break;
                                }
                            }
                            ensure_monitor_auto_turn_inflight(
                                &watcher_provider,
                                channel_id,
                                &tmux_session_name,
                                &output_path,
                                &input_fifo_path,
                                state.last_session_id.as_deref(),
                                data_start_offset,
                                current_offset,
                            );
                            if let Some(hint) = consume_monitor_auto_turn_preamble_once(
                                &mut monitor_auto_turn_preamble_injected,
                            ) {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!(
                                    "  [{ts}] 🔔 monitor auto-turn preamble hint injected (channel {}): {}",
                                    channel_id.get(),
                                    hint
                                );
                            }
                        }
                        if provider_overload_message.is_none() {
                            provider_overload_message = outcome.provider_overload_message;
                        }
                        // Notify when auto-compaction is detected in output
                        if outcome.auto_compacted {
                            let target = format!("channel:{}", channel_id.get());
                            let _ = enqueue_outbox_best_effort(
                                shared.pg_pool.as_ref(),
                                sqlite_runtime_db(shared.as_ref()),
                                OutboxMessage {
                                    target: target.as_str(),
                                    content: "🗜️ 자동 컨텍스트 압축 감지",
                                    bot: "notify",
                                    source: "system",
                                    reason_code: None,
                                    session_key: None,
                                },
                            )
                            .await;
                        }
                    }
                    Ok(Ok(Ok((_, off)))) => {
                        current_offset = off;
                        if last_liveness_probe_at.elapsed() >= TMUX_LIVENESS_PROBE_INTERVAL {
                            last_liveness_probe_at = tokio::time::Instant::now();
                            match watcher_output_poll_decision(
                                0,
                                Some(tmux_liveness_decision(
                                    cancel.load(Ordering::Relaxed),
                                    shared.shutting_down.load(Ordering::Relaxed),
                                    probe_tmux_session_liveness(&tmux_session_name).await,
                                )),
                            ) {
                                WatcherOutputPollDecision::DrainOutput => {}
                                WatcherOutputPollDecision::Continue => {}
                                WatcherOutputPollDecision::QuietStop => break,
                                WatcherOutputPollDecision::TmuxDied => {
                                    tmux_death_observed = true;
                                    break;
                                }
                            }
                        }
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        let now = std::time::Instant::now();
                        let should_probe_ready = last_ready_probe_at
                            .map(|last| {
                                now.duration_since(last) >= READY_FOR_INPUT_IDLE_PROBE_INTERVAL
                            })
                            .unwrap_or(true);
                        if should_probe_ready {
                            last_ready_probe_at = Some(now);
                            let ready_for_input = tokio::time::timeout(
                                std::time::Duration::from_secs(5),
                                tokio::task::spawn_blocking({
                                    let name = tmux_session_name.clone();
                                    move || {
                                        crate::services::provider::tmux_session_ready_for_input(
                                            &name,
                                        )
                                    }
                                }),
                            )
                            .await
                            .unwrap_or(Ok(false))
                            .unwrap_or(false);
                            let post_work_observed = watcher_has_post_work_ready_evidence(
                                &full_response,
                                &tool_state,
                                task_notification_kind,
                            );
                            match watcher_ready_for_input_turn_completed(
                                &mut ready_for_input_tracker,
                                data_start_offset,
                                current_offset,
                                ready_for_input,
                                post_work_observed,
                                now,
                            ) {
                                crate::services::provider::ReadyForInputIdleState::None => {}
                                crate::services::provider::ReadyForInputIdleState::FreshIdle => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!(
                                        "  [{ts}] 👁 watcher observed fresh ready-for-input idle for {tmux_session_name} at offset {current_offset}; leaving session untouched"
                                    );
                                }
                                crate::services::provider::ReadyForInputIdleState::PostWorkIdleTimeout => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    let dispatch_id = resolve_dispatched_thread_dispatch_from_db(
                                        None::<&crate::db::Db>,
                                        shared.pg_pool.as_ref(),
                                        watcher_thread_channel_id.unwrap_or_else(|| channel_id.get()),
                                    )
                                    .or_else(|| {
                                        super::inflight::load_inflight_state(
                                            &watcher_provider,
                                            channel_id.get(),
                                        )
                                        .and_then(|state| state.dispatch_id)
                                    });
                                    if let Some(dispatch_id) = dispatch_id {
                                        match fail_dispatch_for_ready_for_input_stall(
                                            &shared,
                                            &dispatch_id,
                                            &tmux_session_name,
                                        )
                                        .await
                                        {
                                            Ok(result) => {
                                                tracing::warn!(
                                                    "  [{ts}] ⚠ watcher marked post-work Ready-for-input stall as failed for {} / dispatch {} (card={:?}, card_marked={}, human_alert_sent={})",
                                                    tmux_session_name,
                                                    dispatch_id,
                                                    result.card_id,
                                                    result.card_marked,
                                                    result.human_alert_sent
                                                );
                                                if let Some(state) = super::inflight::load_inflight_state(
                                                    &watcher_provider,
                                                    channel_id.get(),
                                                )
                                                .filter(|state| !state.rebind_origin)
                                                {
                                                    let user_msg_id = serenity::MessageId::new(state.user_msg_id);
                                                    super::formatting::remove_reaction_raw(
                                                        &http,
                                                        channel_id,
                                                        user_msg_id,
                                                        '⏳',
                                                    )
                                                    .await;
                                                    super::formatting::add_reaction_raw(
                                                        &http,
                                                        channel_id,
                                                        user_msg_id,
                                                        '⚠',
                                                    )
                                                    .await;
                                                }
                                                super::inflight::clear_inflight_state(
                                                    &watcher_provider,
                                                    channel_id.get(),
                                                );
                                                ready_for_input_failure_notice = Some(format!(
                                                    "⚠️ 작업 후 `Ready for input` 상태에서 멈춰 dispatch를 실패 처리했습니다.\n사유: {READY_FOR_INPUT_STUCK_REASON}"
                                                ));
                                            }
                                            Err(error) => {
                                                tracing::warn!(
                                                    "  [{ts}] ⚠ watcher failed to persist Ready-for-input stall failure for {} / dispatch {}: {}",
                                                    tmux_session_name,
                                                    dispatch_id,
                                                    error
                                                );
                                                ready_for_input_failure_notice = Some(format!(
                                                    "⚠️ 작업 후 `Ready for input` 상태에서 멈췄지만 dispatch 실패 처리를 저장하지 못했습니다.\n사유: {}",
                                                    truncate_str(&error, 300)
                                                ));
                                            }
                                        }
                                    } else {
                                        tracing::warn!(
                                            "  [{ts}] ⚠ watcher detected post-work Ready-for-input stall for {} but could not resolve a dispatched task",
                                            tmux_session_name
                                        );
                                        ready_for_input_failure_notice = Some(
                                            "⚠️ 작업 후 `Ready for input` 상태에서 멈췄지만 연결된 dispatch를 찾지 못해 자동 실패 처리하지 못했습니다.".to_string(),
                                        );
                                    }
                                    full_response.clear();
                                    found_result = true;
                                }
                            }
                        }
                    }
                    _ => {
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                    }
                }

                // Check for stale session error during streaming — abort relay immediately.
                // Only structured error/result events can trip this flag.
                if stale_resume_detected {
                    break;
                }

                // Update Discord placeholder at configurable interval
                if last_status_update.elapsed() >= super::status_update_interval() {
                    last_status_update = tokio::time::Instant::now();
                    let indicator = SPINNER[spin_idx % SPINNER.len()];
                    spin_idx += 1;

                    let has_assistant_response_for_streaming = !full_response.trim().is_empty();
                    let recent_stop_for_streaming = if has_assistant_response_for_streaming {
                        recent_turn_stop_for_watcher_range(
                            channel_id,
                            &tmux_session_name,
                            data_start_offset,
                        )
                    } else {
                        None
                    };
                    let inflight_missing_for_streaming =
                        super::inflight::load_inflight_state(&watcher_provider, channel_id.get())
                            .is_none();
                    if should_suppress_streaming_placeholder_after_recent_stop(
                        has_assistant_response_for_streaming,
                        inflight_missing_for_streaming,
                        recent_stop_for_streaming.is_some(),
                    ) {
                        if let Some(msg_id) = placeholder_msg_id {
                            let outcome = delete_nonterminal_placeholder(
                                &http,
                                channel_id,
                                &shared,
                                &watcher_provider,
                                &tmux_session_name,
                                msg_id,
                                "watcher_streaming_recent_stop_cleanup",
                            )
                            .await;
                            if outcome.is_committed() {
                                placeholder_msg_id = None;
                                last_edit_text.clear();
                            }
                        }
                        if !streaming_suppressed_by_recent_stop {
                            if let Some(stop) = recent_stop_for_streaming {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::warn!(
                                    "  [{ts}] 🛑 watcher: suppressed streaming placeholder output for channel {} after recent turn stop ({}, tmux={}, range {}..{})",
                                    channel_id.get(),
                                    stop.reason,
                                    tmux_session_name,
                                    data_start_offset,
                                    current_offset
                                );
                            }
                            streaming_suppressed_by_recent_stop = true;
                        }
                        continue;
                    }

                    loop {
                        let current_portion =
                            full_response.get(response_sent_offset..).unwrap_or("");
                        if current_portion.is_empty() {
                            break;
                        }

                        let status_block = super::formatting::build_placeholder_status_block(
                            indicator,
                            tool_state.prev_tool_status.as_deref(),
                            tool_state.current_tool_line.as_deref(),
                            &full_response,
                        );
                        let Some(msg_id) = placeholder_msg_id else {
                            break;
                        };
                        let Some(plan) = plan_streaming_rollover(current_portion, &status_block)
                        else {
                            break;
                        };

                        rate_limit_wait(&shared, channel_id).await;
                        match channel_id
                            .edit_message(
                                &http,
                                msg_id,
                                serenity::EditMessage::new().content(&plan.frozen_chunk),
                            )
                            .await
                        {
                            Ok(_) => {
                                rate_limit_wait(&shared, channel_id).await;
                                match channel_id
                                    .send_message(
                                        &http,
                                        serenity::CreateMessage::new().content(&status_block),
                                    )
                                    .await
                                {
                                    Ok(message) => {
                                        placeholder_msg_id = Some(message.id);
                                        response_sent_offset += plan.split_at;
                                        last_edit_text = status_block;
                                        persist_watcher_stream_progress(
                                            &watcher_provider,
                                            channel_id,
                                            &tmux_session_name,
                                            placeholder_msg_id,
                                            &full_response,
                                            response_sent_offset,
                                            tool_state.current_tool_line.as_deref(),
                                            tool_state.prev_tool_status.as_deref(),
                                            task_notification_kind,
                                        );
                                    }
                                    Err(error) => {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        tracing::warn!(
                                            "  [{ts}] ⚠ tmux rollover placeholder send failed in channel {}: {}",
                                            channel_id.get(),
                                            error
                                        );
                                        rate_limit_wait(&shared, channel_id).await;
                                        let _ = channel_id
                                            .edit_message(
                                                &http,
                                                msg_id,
                                                serenity::EditMessage::new()
                                                    .content(&plan.display_snapshot),
                                            )
                                            .await;
                                        last_edit_text = plan.display_snapshot;
                                        break;
                                    }
                                }
                            }
                            Err(error) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::warn!(
                                    "  [{ts}] ⚠ tmux rollover freeze failed for msg {} in channel {}: {}",
                                    msg_id.get(),
                                    channel_id.get(),
                                    error
                                );
                                break;
                            }
                        }
                    }

                    let status_block = super::formatting::build_placeholder_status_block(
                        indicator,
                        tool_state.prev_tool_status.as_deref(),
                        tool_state.current_tool_line.as_deref(),
                        &full_response,
                    );
                    let current_portion = full_response.get(response_sent_offset..).unwrap_or("");
                    let display_text =
                        build_streaming_placeholder_text(current_portion, &status_block);

                    if display_text != last_edit_text {
                        match placeholder_msg_id {
                            Some(msg_id) => {
                                // Edit existing placeholder
                                rate_limit_wait(&shared, channel_id).await;
                                let _ = channel_id
                                    .edit_message(
                                        &http,
                                        msg_id,
                                        serenity::EditMessage::new().content(&display_text),
                                    )
                                    .await;
                            }
                            None => {
                                // Create new placeholder
                                if let Ok(msg) = channel_id.say(&http, &display_text).await {
                                    placeholder_msg_id = Some(msg.id);
                                }
                            }
                        }
                        last_edit_text = display_text;
                        persist_watcher_stream_progress(
                            &watcher_provider,
                            channel_id,
                            &tmux_session_name,
                            placeholder_msg_id,
                            &full_response,
                            response_sent_offset,
                            tool_state.current_tool_line.as_deref(),
                            tool_state.prev_tool_status.as_deref(),
                            task_notification_kind,
                        );
                    }
                }
            }

            if tmux_death_observed {
                handle_tmux_watcher_observed_death(
                    channel_id,
                    &http,
                    &shared,
                    &tmux_session_name,
                    &output_path,
                    &watcher_provider,
                    prompt_too_long_killed,
                    turn_result_relayed,
                )
                .await;
                break 'watcher_loop;
            }

            if cancel.load(Ordering::Relaxed) || shared.shutting_down.load(Ordering::Relaxed) {
                break 'watcher_loop;
            }

            if let Some(notice) = ready_for_input_failure_notice {
                match placeholder_msg_id {
                    Some(msg_id) => {
                        rate_limit_wait(&shared, channel_id).await;
                        let _ = channel_id
                            .edit_message(
                                &http,
                                msg_id,
                                serenity::EditMessage::new().content(&notice),
                            )
                            .await;
                    }
                    None => {
                        let _ = channel_id.say(&http, &notice).await;
                    }
                }
                clear_provider_overload_retry_state(channel_id);
                continue;
            }
        }

        // If paused was set while we were reading (even if already unpaused), discard partial data.
        // Also check epoch: if it changed, a Discord turn claimed this data even if paused is now false.
        let paused_now = paused.load(Ordering::Relaxed);
        let epoch_changed_now = pause_epoch.load(Ordering::Relaxed) != epoch_snapshot;
        let deferred_monitor_ready =
            monitor_auto_turn_claimed && monitor_auto_turn_deferred && !paused_now;
        if (was_paused || paused_now || epoch_changed_now) && !deferred_monitor_ready {
            // Clean up placeholder if we created one
            if let Some(msg_id) = placeholder_msg_id {
                let _ = delete_nonterminal_placeholder(
                    &http,
                    channel_id,
                    &shared,
                    &watcher_provider,
                    &tmux_session_name,
                    msg_id,
                    "watcher_pause_epoch_guard_cleanup",
                )
                .await;
            }
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
            )
            .await;
            all_data.clear();
            continue;
        }

        // Handle prompt-too-long: kill session so next message creates a fresh one
        if is_prompt_too_long {
            clear_provider_overload_retry_state(channel_id);
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Prompt too long detected in watcher for {tmux_session_name}, killing session"
            );
            prompt_too_long_killed = true;

            let sess = tmux_session_name.clone();
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio::task::spawn_blocking(move || {
                    crate::services::termination_audit::record_termination_for_tmux(
                        &sess,
                        None,
                        "tmux_watcher",
                        "prompt_too_long",
                        Some("watcher cleanup: prompt too long"),
                        None,
                    );
                    record_tmux_exit_reason(&sess, "watcher cleanup: prompt too long");
                    crate::services::platform::tmux::kill_session_with_reason(
                        &sess,
                        "watcher cleanup: prompt too long",
                    );
                }),
            )
            .await;

            let notice = "⚠️ 컨텍스트 한도 초과로 세션을 초기화했습니다. 다음 메시지부터 새 세션으로 처리됩니다.";
            match placeholder_msg_id {
                Some(msg_id) => {
                    rate_limit_wait(&shared, channel_id).await;
                    let _ = channel_id
                        .edit_message(&http, msg_id, serenity::EditMessage::new().content(notice))
                        .await;
                }
                None => {
                    let _ = channel_id.say(&http, notice).await;
                }
            }
            // Don't break — let the watcher exit naturally when session-alive check fails
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
            )
            .await;
            continue;
        }

        // Handle auth error: kill session and notify user to re-authenticate
        if is_auth_error {
            clear_provider_overload_retry_state(channel_id);
            let inflight_state =
                super::inflight::load_inflight_state(&watcher_provider, channel_id.get());
            let fallback_session_id = inflight_state
                .as_ref()
                .and_then(|state| state.session_id.as_deref());
            let dispatch_id =
                resolve_watcher_dispatch_id(&shared, channel_id, inflight_state.as_ref()).await;
            let auth_detail = auth_error_message
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("authentication expired");
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Auth error detected in watcher for {tmux_session_name}: {}",
                truncate_str(auth_detail, 300)
            );
            prompt_too_long_killed = true; // reuse flag to suppress duplicate "session ended" message

            clear_provider_session_for_retry(
                &shared,
                channel_id,
                &tmux_session_name,
                fallback_session_id,
            )
            .await;

            let sess = tmux_session_name.clone();
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio::task::spawn_blocking(move || {
                    crate::services::termination_audit::record_termination_for_tmux(
                        &sess,
                        None,
                        "tmux_watcher",
                        "auth_error",
                        Some("watcher cleanup: authentication failed"),
                        None,
                    );
                    record_tmux_exit_reason(&sess, "watcher cleanup: authentication failed");
                    crate::services::platform::tmux::kill_session_with_reason(
                        &sess,
                        "watcher cleanup: authentication failed",
                    );
                }),
            )
            .await;

            let notice = format!(
                "⚠️ 인증이 만료되어 현재 dispatch를 실패 처리했습니다. 세션을 종료합니다.\n관리자가 CLI에서 재인증(`/login`)을 완료한 후 다시 디스패치해주세요.\n\n사유: {}",
                truncate_str(auth_detail, 300)
            );
            match placeholder_msg_id {
                Some(msg_id) => {
                    rate_limit_wait(&shared, channel_id).await;
                    let _ = channel_id
                        .edit_message(&http, msg_id, serenity::EditMessage::new().content(&notice))
                        .await;
                }
                None => {
                    let _ = channel_id.say(&http, &notice).await;
                }
            }
            // #897 round-3 Medium: skip reaction work for `rebind_origin`
            // inflights — their `user_msg_id=0` identifies no real Discord
            // message so issuing reactions against it just produces API
            // errors. The synthetic state was created by
            // `/api/inflight/rebind` to adopt a live tmux session.
            if let Some(state) = inflight_state.as_ref().filter(|s| !s.rebind_origin) {
                let user_msg_id = serenity::MessageId::new(state.user_msg_id);
                super::formatting::remove_reaction_raw(&http, channel_id, user_msg_id, '⏳').await;
                super::formatting::add_reaction_raw(&http, channel_id, user_msg_id, '⚠').await;
            }
            super::inflight::clear_inflight_state(&watcher_provider, channel_id.get());
            let failure_text = format!(
                "authentication expired; re-authentication required: {}",
                truncate_str(auth_detail, 300)
            );
            super::turn_bridge::fail_dispatch_with_retry(
                shared.api_port,
                dispatch_id.as_deref(),
                &failure_text,
            )
            .await;
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
            )
            .await;
            continue;
        }

        if is_provider_overloaded {
            let overload_message = provider_overload_message
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("provider overload detected");
            let inflight_state =
                super::inflight::load_inflight_state(&watcher_provider, channel_id.get());
            let retry_text = inflight_state
                .as_ref()
                .map(|state| state.user_text.clone())
                .filter(|text| !text.trim().is_empty());
            let fallback_session_id = inflight_state
                .as_ref()
                .and_then(|state| state.session_id.as_deref());
            let dispatch_id =
                resolve_watcher_dispatch_id(&shared, channel_id, inflight_state.as_ref()).await;

            let decision = retry_text
                .as_deref()
                .map(|text| record_provider_overload_retry(channel_id, text))
                .unwrap_or(ProviderOverloadDecision::Exhausted);
            let retry_notice = match &decision {
                ProviderOverloadDecision::Retry { attempt, delay, .. } => format!(
                    "⚠️ 모델 capacity 상태를 감지해 세션을 정리했습니다. {}분 후 자동 재시도합니다. ({}/{})",
                    delay.as_secs() / 60,
                    attempt,
                    PROVIDER_OVERLOAD_MAX_RETRIES
                ),
                ProviderOverloadDecision::Exhausted => format!(
                    "⚠️ 모델 capacity 상태가 계속되어 자동 재시도를 중단했습니다. 잠시 후 다시 시도해 주세요.\n\n사유: {}",
                    truncate_str(overload_message, 300)
                ),
            };

            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Provider overload detected in watcher for {}: {}",
                tmux_session_name,
                overload_message
            );
            prompt_too_long_killed = true;

            clear_provider_session_for_retry(
                &shared,
                channel_id,
                &tmux_session_name,
                fallback_session_id,
            )
            .await;

            let sess = tmux_session_name.clone();
            let termination_reason = match &decision {
                ProviderOverloadDecision::Retry { .. } => "provider_overload_retry",
                ProviderOverloadDecision::Exhausted => "provider_overload_exhausted",
            };
            let termination_detail = format!("watcher cleanup: {overload_message}");
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio::task::spawn_blocking(move || {
                    crate::services::termination_audit::record_termination_for_tmux(
                        &sess,
                        None,
                        "tmux_watcher",
                        termination_reason,
                        Some(&termination_detail),
                        None,
                    );
                    record_tmux_exit_reason(&sess, &termination_detail);
                    crate::services::platform::tmux::kill_session_with_reason(
                        &sess,
                        &termination_detail,
                    );
                }),
            )
            .await;

            match placeholder_msg_id {
                Some(msg_id) => {
                    rate_limit_wait(&shared, channel_id).await;
                    let _ = channel_id
                        .edit_message(
                            &http,
                            msg_id,
                            serenity::EditMessage::new().content(&retry_notice),
                        )
                        .await;
                }
                None => {
                    let _ = channel_id.say(&http, &retry_notice).await;
                }
            }

            // #897 round-3 Medium: skip reaction + retry scheduling for
            // `rebind_origin` inflights — they have no real user message
            // to react against and no real user text to re-prompt.
            if let Some(state) = inflight_state.as_ref().filter(|s| !s.rebind_origin) {
                let user_msg_id = serenity::MessageId::new(state.user_msg_id);
                super::formatting::remove_reaction_raw(&http, channel_id, user_msg_id, '⏳').await;
                if matches!(&decision, ProviderOverloadDecision::Exhausted) {
                    super::formatting::add_reaction_raw(&http, channel_id, user_msg_id, '⚠').await;
                }
            }
            super::inflight::clear_inflight_state(&watcher_provider, channel_id.get());

            match decision {
                ProviderOverloadDecision::Retry {
                    attempt,
                    delay,
                    fingerprint,
                } => {
                    if let Some(retry_text) = retry_text {
                        if let Some(state) = inflight_state.as_ref().filter(|s| !s.rebind_origin) {
                            schedule_provider_overload_retry(
                                shared.clone(),
                                http.clone(),
                                watcher_provider.clone(),
                                channel_id,
                                serenity::MessageId::new(state.user_msg_id),
                                retry_text,
                                attempt,
                                delay,
                                fingerprint,
                            );
                        } else {
                            clear_provider_overload_retry_state(channel_id);
                        }
                    } else {
                        clear_provider_overload_retry_state(channel_id);
                    }
                }
                ProviderOverloadDecision::Exhausted => {
                    let failure_text = format!(
                        "provider overloaded after {} auto-retries: {}",
                        PROVIDER_OVERLOAD_MAX_RETRIES,
                        truncate_str(overload_message, 300)
                    );
                    super::turn_bridge::fail_dispatch_with_retry(
                        shared.api_port,
                        dispatch_id.as_deref(),
                        &failure_text,
                    )
                    .await;
                }
            }
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
            )
            .await;
            continue;
        }

        // Final guard: re-check epoch and turn_delivered right before relay.
        // Closes the race window where a Discord turn starts between the epoch check
        // above (line 277) and this relay — the turn_bridge may have already delivered
        // the same response to its own placeholder.
        let paused_now = paused.load(Ordering::Relaxed);
        let epoch_changed_now = pause_epoch.load(Ordering::Relaxed) != epoch_snapshot;
        let turn_delivered_now = turn_delivered.load(Ordering::Relaxed);
        let deferred_monitor_ready =
            monitor_auto_turn_claimed && monitor_auto_turn_deferred && !paused_now;
        if (paused_now || epoch_changed_now || turn_delivered_now) && !deferred_monitor_ready {
            if let Some(msg_id) = placeholder_msg_id {
                let _ = delete_nonterminal_placeholder(
                    &http,
                    channel_id,
                    &shared,
                    &watcher_provider,
                    &tmux_session_name,
                    msg_id,
                    "watcher_late_epoch_guard_cleanup",
                )
                .await;
            }
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 👁 Late epoch/delivered guard: suppressed duplicate relay for {}",
                tmux_session_name
            );
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
            )
            .await;
            continue;
        }

        if watcher_should_yield_to_active_bridge_turn(
            &watcher_provider,
            channel_id,
            &tmux_session_name,
            data_start_offset,
            current_offset,
        ) {
            let matched_reattach = matching_recent_watcher_reattach_offset(
                channel_id,
                &tmux_session_name,
                data_start_offset,
            );
            let reattach_detail = matched_reattach.as_ref().map(|r| {
                format!(
                    "{} range {}..{} matches reattach at {}",
                    tmux_session_name, data_start_offset, current_offset, r.offset
                )
            });
            let ctx = PlaceholderSuppressContext {
                origin: PlaceholderSuppressOrigin::ActiveBridgeTurnGuard,
                placeholder_msg_id,
                response_sent_offset,
                last_edit_text: &last_edit_text,
                inflight_state: None,
                has_active_turn: false,
                tmux_session_name: &tmux_session_name,
                task_notification_kind: None,
                reattach_offset_match: matched_reattach.is_some(),
            };
            apply_placeholder_suppression(
                &http,
                channel_id,
                &shared,
                &watcher_provider,
                &tmux_session_name,
                placeholder_msg_id,
                ctx.origin,
                decide_placeholder_suppression(&ctx),
                reattach_detail.as_deref(),
            )
            .await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 👁 Active bridge turn guard: suppressed duplicate relay for {} (range {}..{})",
                tmux_session_name,
                data_start_offset,
                current_offset
            );
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
            )
            .await;
            continue;
        }

        // Duplicate-relay guard: if we already relayed from this same data
        // range, suppress. Use strict `<` so output starting exactly at the
        // previous boundary is treated as the next turn rather than a re-read.
        if let Ok(meta) = std::fs::metadata(&output_path) {
            let observed_output_end = meta.len();
            reset_stale_relay_watermark_if_output_regressed(
                &shared,
                channel_id,
                &tmux_session_name,
                observed_output_end,
                "pre_local_dedupe",
            );
            reset_stale_local_relay_offset_if_output_regressed(
                &mut last_relayed_offset,
                &mut last_observed_generation_mtime_ns,
                channel_id,
                &tmux_session_name,
                observed_output_end,
                "pre_local_dedupe",
            );
        }
        if let Some(prev_offset) = last_relayed_offset {
            if data_start_offset < prev_offset {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] 👁 Duplicate relay guard: suppressed re-relay for {} (data_start={}, last_relayed={:?})",
                    tmux_session_name,
                    data_start_offset,
                    last_relayed_offset,
                );
                if let Some(msg_id) = placeholder_msg_id {
                    let _ = delete_nonterminal_placeholder(
                        &http,
                        channel_id,
                        &shared,
                        &watcher_provider,
                        &tmux_session_name,
                        msg_id,
                        "watcher_duplicate_relay_guard_cleanup",
                    )
                    .await;
                }
                finish_monitor_auto_turn_if_claimed(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &mut monitor_auto_turn_claimed,
                    &mut monitor_auto_turn_finished,
                )
                .await;
                continue;
            }
        }

        // Detect stale session resume failure in watcher output
        let is_stale_resume = stale_resume_detected;
        if is_stale_resume {
            clear_provider_overload_retry_state(channel_id);
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ Watcher detected stale session resume failure (channel {}), clearing session_id",
                channel_id
            );
            let stale_sid = {
                let mut data = shared.core.lock().await;
                let old = data
                    .sessions
                    .get(&channel_id)
                    .and_then(|s| s.session_id.clone());
                if let Some(session) = data.sessions.get_mut(&channel_id) {
                    session.clear_provider_session();
                }
                old
            };
            // Clear DB session_id
            {
                let hostname = crate::services::platform::hostname_short();
                let session_key = format!("{}:{}", hostname, tmux_session_name);
                super::adk_session::clear_provider_session_id(&session_key, shared.api_port).await;
            }
            if let Some(ref sid) = stale_sid {
                let _ = super::internal_api::clear_stale_session_id(sid).await;
            }
            crate::services::termination_audit::record_termination_for_tmux(
                &tmux_session_name,
                None,
                "tmux_watcher",
                "stale_resume_retry",
                Some("stale session resume detected — forcing fresh session before auto-retry"),
                None,
            );
            record_tmux_exit_reason(
                &tmux_session_name,
                "stale session resume detected — forcing fresh session before auto-retry",
            );
            crate::services::platform::tmux::kill_session_with_reason(
                &tmux_session_name,
                "stale session resume detected — forcing fresh session before auto-retry",
            );
            // Replace placeholder with recovery notice (don't delete — avoids visual gap)
            if let Some(msg_id) = placeholder_msg_id {
                let _ = channel_id
                    .edit_message(
                        &http,
                        msg_id,
                        serenity::EditMessage::new()
                            .content("↻ 세션 복구 중... 잠시 후 자동으로 이어갑니다."),
                    )
                    .await;
            }
            // Auto-retry: persist Discord history for LLM injection, then queue the
            // original user message as an internal follow-up instead of self-routing
            // through /api/send announce.
            //
            // #897 round-4 Medium: a `rebind_origin` inflight has no real
            // user message or text to retry with (`user_msg_id=0`,
            // user_text="/api/inflight/rebind"), so auto-retry would
            // enqueue a garbage internal follow-up. Skip the retry; the
            // operator is expected to re-invoke `/api/inflight/rebind`
            // once the tmux session is healthy again.
            match super::inflight::load_inflight_state(&watcher_provider, channel_id.get()) {
                Some(state) if state.rebind_origin => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ Watcher auto-retry skipped for channel {} — rebind_origin inflight has no user message to retry",
                        channel_id
                    );
                }
                Some(state) => {
                    super::turn_bridge::auto_retry_with_history(
                        &http,
                        &shared,
                        &watcher_provider,
                        channel_id,
                        serenity::MessageId::new(state.user_msg_id),
                        &state.user_text,
                    )
                    .await;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ↻ Watcher auto-retry queued for channel {}",
                        channel_id
                    );
                }
                None => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ Watcher auto-retry skipped: inflight state missing for channel {}",
                        channel_id
                    );
                }
            }
            // Skip normal response relay
            full_response = String::new();
        }

        let has_assistant_response = !full_response.trim().is_empty();
        let current_response = full_response.get(response_sent_offset..).unwrap_or("");
        let has_current_response = !current_response.trim().is_empty();

        let recent_stop_for_output =
            recent_turn_stop_for_watcher_range(channel_id, &tmux_session_name, data_start_offset);
        let inflight_missing_before_relay =
            super::inflight::load_inflight_state(&watcher_provider, channel_id.get()).is_none();
        if should_suppress_terminal_output_after_recent_stop(
            has_assistant_response,
            inflight_missing_before_relay,
            recent_stop_for_output.is_some(),
        ) {
            let stop = recent_stop_for_output.expect("recent stop checked above");
            let cleanup_committed = if let Some(msg_id) = placeholder_msg_id {
                delete_nonterminal_placeholder(
                    &http,
                    channel_id,
                    &shared,
                    &watcher_provider,
                    &tmux_session_name,
                    msg_id,
                    "watcher_terminal_recent_stop_cleanup",
                )
                .await
                .is_committed()
            } else {
                true
            };
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 🛑 watcher: suppressed terminal output for channel {} after recent turn stop ({}, tmux={}, range {}..{})",
                channel_id.get(),
                stop.reason,
                tmux_session_name,
                data_start_offset,
                current_offset
            );
            if cleanup_committed {
                last_relayed_offset = Some(current_offset);
                // #1270 codex P2: snapshot the current `.generation` mtime so
                // the local regression check has a real baseline (see the
                // matching snapshot in the rotation path).
                last_observed_generation_mtime_ns =
                    Some(read_generation_file_mtime_ns(&tmux_session_name));
                advance_watcher_confirmed_end(
                    &shared,
                    &watcher_provider,
                    channel_id,
                    &tmux_session_name,
                    current_offset,
                    "src/services/discord/tmux.rs:cancel_tombstone_suppressed_terminal_output",
                );
            }
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
            )
            .await;
            continue;
        }

        // Relay coordination is limited to serialization plus telemetry. The
        // local `last_relayed_offset` guard handles self-duplicate relays, and
        // watcher registration enforces one live owner per tmux session. Do
        // not suppress a valid owner solely because another watcher advanced
        // the shared confirmed_end watermark.
        let relay_coord = shared.tmux_relay_coord(channel_id);
        if let Ok(meta) = std::fs::metadata(&output_path) {
            reset_stale_relay_watermark_if_output_regressed(
                &shared,
                channel_id,
                &tmux_session_name,
                meta.len(),
                "pre_relay",
            );
        }
        // CAS the emission slot. `0` = free; any non-zero value = a watcher
        // is mid-emission with that start offset. `.max(1)` guarantees the
        // stored value is non-zero even when `data_start_offset == 0`.
        let slot_claim_token = data_start_offset.max(1);
        if relay_coord
            .relay_slot
            .compare_exchange(
                0,
                slot_claim_token,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_err()
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 👁 Cross-watcher serialization: slot busy, skipped relay for {} (data_start={})",
                tmux_session_name,
                data_start_offset
            );
            if let Some(msg_id) = placeholder_msg_id {
                let _ = delete_nonterminal_placeholder(
                    &http,
                    channel_id,
                    &shared,
                    &watcher_provider,
                    &tmux_session_name,
                    msg_id,
                    "watcher_cross_watcher_slot_busy_cleanup",
                )
                .await;
            }
            finish_monitor_auto_turn_if_claimed(
                &shared,
                &watcher_provider,
                channel_id,
                &mut monitor_auto_turn_claimed,
                &mut monitor_auto_turn_finished,
            )
            .await;
            continue;
        }

        // Send the terminal response to Discord.
        let relay_decision =
            terminal_relay_decision(has_assistant_response, task_notification_kind);
        debug_assert!(
            !relay_decision.should_enqueue_notify_outbox,
            "monitor/task-notification watcher relays must not use notify-bot outbox"
        );
        let relay_ok = if relay_decision.should_direct_send {
            let formatted = super::formatting::format_for_discord_with_provider(
                current_response,
                &watcher_provider,
            );
            let relay_text = if relay_decision.should_tag_monitor_origin {
                super::prepend_monitor_auto_turn_origin(&formatted)
            } else {
                formatted
            };
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Relaying terminal response to Discord ({} chars, offset {}, task_notification_kind={})",
                relay_text.len(),
                data_start_offset,
                task_notification_kind
                    .map(TaskNotificationKind::as_str)
                    .unwrap_or("none")
            );
            let mut relay_ok = true;
            let mut direct_send_delivered = false;
            match placeholder_msg_id {
                Some(msg_id) => {
                    if has_current_response {
                        match replace_long_message_raw_with_outcome(
                            &http,
                            channel_id,
                            msg_id,
                            &relay_text,
                            &shared,
                        )
                        .await
                        {
                            Ok(ReplaceLongMessageOutcome::EditedOriginal) => {
                                direct_send_delivered = true;
                                record_placeholder_cleanup(
                                    &shared,
                                    &watcher_provider,
                                    channel_id,
                                    msg_id,
                                    &tmux_session_name,
                                    PlaceholderCleanupOperation::EditTerminal,
                                    PlaceholderCleanupOutcome::Succeeded,
                                    "watcher_terminal_relay",
                                );
                            }
                            Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                                edit_error,
                            }) => {
                                direct_send_delivered = true;
                                record_placeholder_cleanup(
                                    &shared,
                                    &watcher_provider,
                                    channel_id,
                                    msg_id,
                                    &tmux_session_name,
                                    PlaceholderCleanupOperation::EditTerminal,
                                    PlaceholderCleanupOutcome::failed(edit_error),
                                    "watcher_terminal_relay",
                                );
                                let cleanup = delete_terminal_placeholder(
                                    &http,
                                    channel_id,
                                    &shared,
                                    &watcher_provider,
                                    &tmux_session_name,
                                    msg_id,
                                    "watcher_terminal_relay_fallback_cleanup",
                                )
                                .await;
                                match fallback_placeholder_cleanup_decision(&cleanup) {
                                    FallbackPlaceholderCleanupDecision::RelayCommitted => {}
                                    FallbackPlaceholderCleanupDecision::PreserveInflightForCleanupRetry => {
                                        relay_ok = false;
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        tracing::warn!(
                                            "  [{ts}] ⚠ watcher: terminal response was delivered via fallback send, but stale placeholder cleanup did not commit for channel {} msg {}",
                                            channel_id.get(),
                                            msg_id.get()
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!("  [{ts}] 👁 Failed to relay: {e}");
                                relay_ok = false;
                            }
                        }
                    } else {
                        let outcome = delete_terminal_placeholder(
                            &http,
                            channel_id,
                            &shared,
                            &watcher_provider,
                            &tmux_session_name,
                            msg_id,
                            "watcher_empty_terminal_cleanup",
                        )
                        .await;
                        if !outcome.is_committed() {
                            relay_ok = false;
                        }
                    }
                }
                None => {
                    if has_current_response {
                        match send_long_message_raw(&http, channel_id, &relay_text, &shared).await {
                            Ok(_) => {
                                direct_send_delivered = true;
                            }
                            Err(e) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!("  [{ts}] 👁 Failed to relay: {e}");
                                relay_ok = false;
                            }
                        }
                    }
                }
            }
            if relay_ok {
                if direct_send_delivered || !has_current_response {
                    last_relayed_offset = Some(data_start_offset);
                    // #1270 codex P2: snapshot the current `.generation` mtime
                    // on every successful relay so the local regression check
                    // has a real baseline. Without this, normal relay paths
                    // (which never enter the reset helper) leave the baseline
                    // at None, and any later regression misclassifies
                    // same-wrapper rotation as fresh-respawn — clearing the
                    // local offset and re-relaying surviving bytes.
                    last_observed_generation_mtime_ns =
                        Some(read_generation_file_mtime_ns(&tmux_session_name));
                    // #1134: first successful relay for this attach. The
                    // watcher_latency module is idempotent — only the first
                    // call after `record_attach` actually observes a sample,
                    // so the unconditional call here is safe and cheap.
                    crate::services::observability::watcher_latency::record_first_relay(
                        channel_id.get(),
                    );
                    if let Some((pk, _)) =
                        parse_provider_and_channel_from_tmux_name(&tmux_session_name)
                    {
                        if let Some(mut inflight) =
                            super::inflight::load_inflight_state(&pk, channel_id.get())
                        {
                            inflight.last_watcher_relayed_offset = Some(data_start_offset);
                            // #1270: persist the matching `.generation` mtime
                            // alongside the offset so a replacement watcher
                            // (e.g. after dcserver restart) can disambiguate
                            // same-wrapper rotation (mtime unchanged → pin to
                            // EOF) from cancel→respawn (mtime changed → reset
                            // to 0) when restoring this offset.
                            inflight.last_watcher_relayed_generation_mtime_ns =
                                last_observed_generation_mtime_ns;
                            let _ = super::inflight::save_inflight_state(&inflight);
                        }
                    }
                }
                clear_provider_overload_retry_state(channel_id);
            }
            relay_ok
        } else if relay_decision.suppressed {
            let monitor_event_count = tool_state.transcript_events.len();
            // #1009: Snapshot the channel's MonitoringStore entry keys ONCE so
            // both the lifecycle notify-outbox row and the suppressed-placeholder
            // edit body share an identical summary (DRY enforcement).
            let monitor_entry_keys: Vec<String> = if matches!(
                task_notification_kind,
                Some(TaskNotificationKind::MonitorAutoTurn)
            ) {
                let store_arc = crate::server::routes::state::global_monitoring_store();
                let store = store_arc.lock().await;
                store
                    .list(channel_id.get())
                    .into_iter()
                    .map(|entry| entry.key)
                    .collect()
            } else {
                Vec::new()
            };
            if matches!(
                task_notification_kind,
                Some(TaskNotificationKind::MonitorAutoTurn)
            ) {
                let _ = enqueue_monitor_auto_turn_suppressed_notification(
                    shared.pg_pool.as_ref(),
                    sqlite_runtime_db(shared.as_ref()),
                    channel_id,
                    &tmux_session_name,
                    data_start_offset,
                    monitor_event_count,
                    &monitor_entry_keys,
                );
            }
            let task_notification_detail = format!(
                "{} kind={} offset={}",
                tmux_session_name,
                task_notification_kind
                    .map(TaskNotificationKind::as_str)
                    .unwrap_or("none"),
                data_start_offset,
            );
            let ctx = PlaceholderSuppressContext {
                origin: PlaceholderSuppressOrigin::TaskNotificationTerminal,
                placeholder_msg_id,
                response_sent_offset,
                last_edit_text: &last_edit_text,
                inflight_state: None,
                has_active_turn: false,
                tmux_session_name: &tmux_session_name,
                task_notification_kind,
                reattach_offset_match: false,
            };
            let mut decision = decide_placeholder_suppression(&ctx);
            // #1009: Monitor auto-turn gets a richer suppressed-placeholder body
            // (event count + current MonitoringStore entry keys) in place of the
            // generic internal-suppression label.
            if matches!(
                task_notification_kind,
                Some(TaskNotificationKind::MonitorAutoTurn)
            ) {
                if let PlaceholderSuppressDecision::Edit(_) = &decision {
                    let body = format_monitor_suppressed_body(
                        &last_edit_text,
                        monitor_event_count,
                        &monitor_entry_keys,
                    );
                    decision = PlaceholderSuppressDecision::Edit(body);
                }
            }
            apply_placeholder_suppression(
                &http,
                channel_id,
                &shared,
                &watcher_provider,
                &tmux_session_name,
                placeholder_msg_id,
                ctx.origin,
                decision,
                Some(&task_notification_detail),
            )
            .await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Suppressed task-notification relay for {} (kind={}, offset {})",
                tmux_session_name,
                task_notification_kind
                    .map(TaskNotificationKind::as_str)
                    .unwrap_or("none"),
                data_start_offset
            );
            clear_provider_overload_retry_state(channel_id);
            false
        } else {
            if let Some(msg_id) = placeholder_msg_id {
                // No response text but placeholder exists — clean up
                let _ = delete_terminal_placeholder(
                    &http,
                    channel_id,
                    &shared,
                    &watcher_provider,
                    &tmux_session_name,
                    msg_id,
                    "watcher_no_response_cleanup",
                )
                .await;
            }
            false
        };
        let relay_suppressed = relay_decision.suppressed;

        // Advance the shared confirmed-delivery watermark on any committed
        // direct emission or empty-turn cleanup. CAS loop ensures we only ever move the
        // watermark FORWARD, even if some other instance has raced ahead.
        if relay_ok || relay_suppressed {
            advance_watcher_confirmed_end(
                &shared,
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                current_offset,
                "src/services/discord/tmux.rs:tmux_output_watcher_confirmed_end",
            );
        }
        // Release the emission slot regardless of success. If delivery failed
        // the local `last_relayed_offset` also stayed put, so the same watcher
        // (or its replacement) can retry on the next tick without fighting
        // the slot.
        relay_coord
            .relay_slot
            .store(0, std::sync::atomic::Ordering::Release);

        finish_monitor_auto_turn_if_claimed(
            &shared,
            &watcher_provider,
            channel_id,
            &mut monitor_auto_turn_claimed,
            &mut monitor_auto_turn_finished,
        )
        .await;

        let provider_kind = watcher_provider.clone();
        let inflight_state = super::inflight::load_inflight_state(&provider_kind, channel_id.get());
        let watcher_session_id = state.last_session_id.clone();
        let result_usage = stream_line_state_token_usage(&state);
        if inflight_state.is_none() {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher: inflight state missing for channel {} — using DB dispatch fallback",
                channel_id.get()
            );
        }

        // Mark user message as completed: ⏳ → ✅ when inflight metadata is
        // available. #897 round-3 Medium: skip the reaction + transcript +
        // analytics block entirely for `rebind_origin` inflights. Their
        // `user_msg_id=0` points at no real message, and persisting a
        // transcript with `turn_id=discord:<channel>:0` poisons
        // session_transcripts / turn_analytics. The notify-bot outbox
        // enqueue above already delivered the recovered response to the
        // user; nothing else on the success path is legitimate here.
        if let Some(state) = inflight_state.as_ref().filter(|s| !s.rebind_origin) {
            let user_msg_id = serenity::MessageId::new(state.user_msg_id);
            super::formatting::remove_reaction_raw(&http, channel_id, user_msg_id, '⏳').await;
            super::formatting::add_reaction_raw(&http, channel_id, user_msg_id, '✅').await;

            if has_assistant_response
                && (None::<&crate::db::Db>.is_some() || shared.pg_pool.is_some())
            {
                let turn_id = format!("discord:{}:{}", channel_id.get(), state.user_msg_id);
                let channel_id_text = channel_id.get().to_string();
                let resolved_did = inflight_state
                    .as_ref()
                    .and_then(|s| s.dispatch_id.clone())
                    .or_else(|| super::adk_session::parse_dispatch_id(&state.user_text))
                    .or(super::adk_session::lookup_pending_dispatch_for_thread(
                        shared.api_port,
                        channel_id.get(),
                    )
                    .await)
                    .or_else(|| {
                        resolve_dispatched_thread_dispatch_from_db(
                            None::<&crate::db::Db>,
                            shared.pg_pool.as_ref(),
                            channel_id.get(),
                        )
                    });
                if let Err(e) = crate::db::session_transcripts::persist_turn_db(
                    None::<&crate::db::Db>,
                    shared.pg_pool.as_ref(),
                    crate::db::session_transcripts::PersistSessionTranscript {
                        turn_id: &turn_id,
                        session_key: state.session_key.as_deref(),
                        channel_id: Some(channel_id_text.as_str()),
                        agent_id: resolve_role_binding(channel_id, state.channel_name.as_deref())
                            .as_ref()
                            .map(|binding| binding.role_id.as_str()),
                        provider: Some(provider_kind.as_str()),
                        dispatch_id: resolved_did.as_deref().or(state.dispatch_id.as_deref()),
                        user_message: &state.user_text,
                        assistant_message: &full_response,
                        events: &tool_state.transcript_events,
                        duration_ms: inflight_duration_ms(Some(state.started_at.as_str())),
                    },
                )
                .await
                {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!("  [{ts}] ⚠ watcher: failed to persist session transcript: {e}");
                }

                super::turn_bridge::persist_turn_analytics_row_with_handles(
                    None::<&crate::db::Db>,
                    shared.pg_pool.as_ref(),
                    &provider_kind,
                    channel_id,
                    user_msg_id,
                    resolve_role_binding(channel_id, state.channel_name.as_deref()).as_ref(),
                    resolved_did.as_deref().or(state.dispatch_id.as_deref()),
                    state.session_key.as_deref(),
                    watcher_session_id
                        .as_deref()
                        .or(state.session_id.as_deref()),
                    state,
                    result_usage.unwrap_or_default(),
                    inflight_duration_ms(Some(state.started_at.as_str())).unwrap_or(0),
                );
            }
        }

        let resolved_did = inflight_state
            .as_ref()
            .and_then(|state| state.dispatch_id.clone())
            .or_else(|| {
                inflight_state
                    .as_ref()
                    .and_then(|state| super::adk_session::parse_dispatch_id(&state.user_text))
            })
            .or(super::adk_session::lookup_pending_dispatch_for_thread(
                shared.api_port,
                channel_id.get(),
            )
            .await)
            .or_else(|| {
                resolve_dispatched_thread_dispatch_from_db(
                    None::<&crate::db::Db>,
                    shared.pg_pool.as_ref(),
                    channel_id.get(),
                )
            });

        if resolved_did.is_none() && has_assistant_response {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher: no dispatch id resolved for channel {} after terminal success",
                channel_id.get()
            );
        }
        let current_worktree_path = {
            let mut data = shared.core.lock().await;
            data.sessions
                .get_mut(&channel_id)
                .and_then(|session| session.validated_path(channel_id.get()))
        };

        let dispatch_ok = if let Some(did) = resolved_did.as_deref() {
            let dispatch_type = super::internal_api::lookup_dispatch_type(did)
                .await
                .ok()
                .flatten();

            match dispatch_type.as_deref() {
                Some("implementation") | Some("rework") => {
                    if !has_assistant_response {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ watcher: refusing to complete work dispatch {did} without assistant response"
                        );
                        false
                    } else if let (Some(db), Some(engine)) =
                        (None::<&crate::db::Db>, &shared.engine)
                    {
                        let mut work_completion_context =
                            super::turn_bridge::build_work_dispatch_completion_result(
                                None::<&crate::db::Db>,
                                shared.pg_pool.as_ref(),
                                did,
                                "watcher_completed",
                                false,
                                current_worktree_path.as_deref(),
                                Some(&full_response),
                            );
                        if let Some(obj) = work_completion_context.as_object_mut() {
                            obj.insert(
                                "agent_response_present".to_string(),
                                serde_json::Value::Bool(true),
                            );
                        }
                        match crate::dispatch::finalize_dispatch(
                            db,
                            engine,
                            did,
                            "watcher_completed",
                            Some(&work_completion_context),
                        ) {
                            Ok(_) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!(
                                    "  [{ts}] ✓ watcher: completed dispatch {did} via finalize_dispatch"
                                );
                                let _ = super::turn_bridge::queue_dispatch_followup_with_handles(
                                    Some(db),
                                    shared.pg_pool.as_ref(),
                                    did,
                                    "watcher_completed",
                                )
                                .await;
                                true
                            }
                            Err(e) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::warn!(
                                    "  [{ts}] ⚠ watcher: finalize_dispatch failed for {did}: {e}"
                                );
                                let mut fallback_result =
                                    super::turn_bridge::build_work_dispatch_completion_result(
                                        None::<&crate::db::Db>,
                                        shared.pg_pool.as_ref(),
                                        did,
                                        "watcher_db_fallback",
                                        true,
                                        current_worktree_path.as_deref(),
                                        Some(&full_response),
                                    );
                                if let Some(obj) = fallback_result.as_object_mut() {
                                    obj.insert(
                                        "agent_response_present".to_string(),
                                        serde_json::Value::Bool(true),
                                    );
                                }
                                let completed =
                                    super::turn_bridge::runtime_db_fallback_complete_with_result(
                                        did,
                                        &fallback_result,
                                    );
                                if completed {
                                    let _ =
                                        super::turn_bridge::queue_dispatch_followup_with_handles(
                                            None::<&crate::db::Db>,
                                            shared.pg_pool.as_ref(),
                                            did,
                                            "watcher_completed_fallback",
                                        )
                                        .await;
                                }
                                completed
                            }
                        }
                    } else {
                        let mut fallback_result =
                            super::turn_bridge::build_work_dispatch_completion_result(
                                None::<&crate::db::Db>,
                                shared.pg_pool.as_ref(),
                                did,
                                "watcher_db_fallback",
                                true,
                                current_worktree_path.as_deref(),
                                Some(&full_response),
                            );
                        if let Some(obj) = fallback_result.as_object_mut() {
                            obj.insert(
                                "agent_response_present".to_string(),
                                serde_json::Value::Bool(true),
                            );
                        }
                        let completed =
                            super::turn_bridge::runtime_db_fallback_complete_with_result(
                                did,
                                &fallback_result,
                            );
                        if completed {
                            let _ = super::turn_bridge::queue_dispatch_followup_with_handles(
                                None::<&crate::db::Db>,
                                shared.pg_pool.as_ref(),
                                did,
                                "watcher_completed_runtime_fallback",
                            )
                            .await;
                        }
                        completed
                    }
                }
                Some(_) => {
                    // Non-work dispatches — leave for their own completion flow
                    true
                }
                None => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ watcher: cannot determine dispatch type for {did} — preserving state"
                    );
                    false
                }
            }
        } else {
            true
        };

        // #225 P1-2: Only mark relayed + clear inflight if Discord relay succeeded.
        // If relay failed, preserve retry/handoff path for next startup.
        if relay_ok {
            if has_assistant_response
                && let Some(state) = inflight_state.as_ref().filter(|state| !state.rebind_origin)
            {
                let mut data = shared.core.lock().await;
                if let Some(session) = data.sessions.get_mut(&channel_id) {
                    if !session.cleared {
                        session.history.push(crate::ui::ai_screen::HistoryItem {
                            item_type: crate::ui::ai_screen::HistoryType::User,
                            content: state.user_text.clone(),
                        });
                        session.history.push(crate::ui::ai_screen::HistoryItem {
                            item_type: crate::ui::ai_screen::HistoryType::Assistant,
                            content: full_response.clone(),
                        });
                    }
                }
                drop(data);
            }
            turn_result_relayed = true;
            if dispatch_ok {
                super::inflight::clear_inflight_state(&provider_kind, channel_id.get());
                finish_restored_watcher_active_turn(
                    &shared,
                    &provider_kind,
                    channel_id,
                    finish_mailbox_on_completion,
                    "restored watcher completed with queued backlog",
                )
                .await;
            }
            let mailbox = shared.mailbox(channel_id);
            let has_active_turn = mailbox.has_active_turn().await;
            let should_kickoff_queue =
                if finish_mailbox_on_completion || monitor_auto_turn_finished || has_active_turn {
                    false
                } else {
                    mailbox
                        .has_pending_soft_queue(super::queue_persistence_context(
                            &shared,
                            &provider_kind,
                            channel_id,
                        ))
                        .await
                        .has_pending
                };
            if dispatch_ok && should_kickoff_queue {
                super::schedule_deferred_idle_queue_kickoff(
                    shared.clone(),
                    provider_kind.clone(),
                    channel_id,
                    "watcher completed with queued backlog",
                );
            }
        } else if !relay_suppressed {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ watcher: relay failed — preserving inflight for retry");
        }

        let terminal_output_committed = relay_ok || relay_suppressed;
        let tmux_alive_for_missing_inflight =
            if inflight_state.is_none() && resolved_did.is_none() && terminal_output_committed {
                probe_tmux_session_liveness(&tmux_session_name).await
            } else {
                true
            };
        let recent_turn_stop =
            recent_turn_stop_for_watcher_range(channel_id, &tmux_session_name, data_start_offset);
        let placeholder_cleanup_committed = placeholder_msg_id.is_some_and(|msg_id| {
            shared.placeholder_cleanup.terminal_cleanup_committed(
                &provider_kind,
                channel_id,
                msg_id,
            )
        });
        let missing_inflight_plan = missing_inflight_fallback_plan(
            inflight_state.is_none(),
            resolved_did.is_some(),
            terminal_output_committed,
            recent_turn_stop.is_some(),
            placeholder_cleanup_committed,
            tmux_alive_for_missing_inflight,
        );
        if missing_inflight_plan.trigger_reattach {
            if wait_for_reacquired_turn_bridge_inflight_state(
                &provider_kind,
                channel_id,
                &tmux_session_name,
                MISSING_INFLIGHT_REATTACH_GRACE_ATTEMPTS,
                MISSING_INFLIGHT_REATTACH_GRACE_DELAY,
            )
            .await
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ↻ watcher: explicit reattach skipped for channel {} — turn bridge reacquired inflight state during grace window",
                    channel_id.get()
                );
            } else {
                // #1136: this is the silent-drop path. The legacy code merely
                // warned and walked away when the DB-side dispatch_id resolve
                // failed; we now (a) bump a counter so operators can see the
                // failure rate in `/api/analytics/observability`, and
                // (b) explicitly trigger a watcher re-attach. The synthetic
                // inflight state is tagged `rebind_origin = true` (see
                // `trigger_missing_inflight_reattach`), so the next watcher
                // generation will NOT itself re-enter this fallback path —
                // that's the loop-prevention guard.
                crate::services::observability::metrics::record_watcher_db_fallback_resolve_failed(
                    channel_id.get(),
                    provider_kind.as_str(),
                );
                let outcome = trigger_missing_inflight_reattach(
                    &http,
                    &shared,
                    &provider_kind,
                    channel_id,
                    &tmux_session_name,
                );
                log_missing_inflight_reattach_outcome(
                    channel_id,
                    &tmux_session_name,
                    outcome,
                    "missing_inflight_db_fallback",
                );
            }
        } else if missing_inflight_plan.suppressed_by_recent_stop {
            if placeholder_cleanup_committed {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ↻ watcher: explicit reattach skipped for channel {} — terminal placeholder cleanup already committed",
                    channel_id.get()
                );
            } else if let Some(stop) = recent_turn_stop {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ↻ watcher: explicit reattach skipped for channel {} — recent turn stop still active ({})",
                    channel_id.get(),
                    stop.reason
                );
            }
        } else if !tmux_alive_for_missing_inflight {
            let _drained_offset = drain_missing_inflight_dead_tmux_tail_to_eof(
                &shared,
                &watcher_provider,
                channel_id,
                &tmux_session_name,
                &output_path,
                current_offset,
            )
            .await;
            handle_tmux_watcher_observed_death(
                channel_id,
                &http,
                &shared,
                &tmux_session_name,
                &output_path,
                &watcher_provider,
                prompt_too_long_killed,
                turn_result_relayed,
            )
            .await;
            break 'watcher_loop;
        }

        // Update session tokens from result event and auto-compact if threshold exceeded
        if let Some(tokens) = result_usage.map(|usage| usage.total_input_tokens()) {
            let provider = shared.settings.read().await.provider.clone();
            let session_key =
                super::adk_session::build_adk_session_key(&shared, channel_id, &provider).await;
            let channel_name = {
                let data = shared.core.lock().await;
                data.sessions
                    .get(&channel_id)
                    .and_then(|s| s.channel_name.clone())
            };
            let thread_channel_id = channel_name
                .as_deref()
                .and_then(super::adk_session::parse_thread_channel_id_from_name);
            let agent_id = resolve_role_binding(channel_id, channel_name.as_deref())
                .map(|binding| binding.role_id);
            super::adk_session::post_adk_session_status(
                session_key.as_deref(),
                channel_name.as_deref(),
                None,
                "idle",
                &provider,
                None,
                Some(tokens),
                None,
                None,
                thread_channel_id,
                agent_id.as_deref(),
                shared.api_port,
            )
            .await;

            let ctx_cfg = super::adk_session::fetch_context_thresholds(shared.api_port).await;
            let pct = (tokens * 100) / ctx_cfg.context_window.max(1);
            // #227: Re-enabled with 5-min cooldown (matches turn_bridge path).
            // Without cooldown, the compact turn's own result could re-trigger compact.
            let cooldown_key = format!("auto_compact_cooldown:{}", channel_id.get());
            let cooldown_value = match super::internal_api::get_kv_value(&cooldown_key) {
                Ok(value) => value,
                Err(_) => {
                    if let Some(pg_pool) = shared.pg_pool.as_ref() {
                        sqlx::query_scalar::<_, Option<String>>(
                            "SELECT value
                             FROM kv_meta
                             WHERE key = $1
                               AND (expires_at IS NULL OR expires_at > NOW())
                             LIMIT 1",
                        )
                        .bind(&cooldown_key)
                        .fetch_optional(pg_pool)
                        .await
                        .ok()
                        .flatten()
                        .flatten()
                    } else {
                        None
                    }
                }
            };
            let compact_cooldown_ok =
                cooldown_value
                    .and_then(|v| v.parse::<i64>().ok())
                    .map_or(true, |ts| {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs() as i64;
                        now - ts > 300 // 5 min cooldown
                    });
            // DISABLED — token counting still unreliable
            if false && pct >= ctx_cfg.compact_pct && !is_prompt_too_long && compact_cooldown_ok {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚡ [watcher] Auto-compact: {} at {pct}% ({tokens} tokens)",
                    tmux_session_name
                );
                let name = tmux_session_name.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    crate::services::platform::tmux::send_keys(&name, &["/compact", "Enter"])
                })
                .await;
                // Set cooldown timestamp
                let cooldown_key = format!("auto_compact_cooldown:{}", channel_id.get());
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                let now_text = now.to_string();
                if super::internal_api::set_kv_value(&cooldown_key, &now_text).is_err() {
                    if let Some(pg_pool) = shared.pg_pool.as_ref() {
                        let _ = sqlx::query(
                            "INSERT INTO kv_meta (key, value, expires_at)
                             VALUES ($1, $2, NULL)
                             ON CONFLICT (key) DO UPDATE
                             SET value = EXCLUDED.value,
                                 expires_at = EXCLUDED.expires_at",
                        )
                        .bind(&cooldown_key)
                        .bind(&now_text)
                        .execute(pg_pool)
                        .await;
                    }
                }
                // Notify: auto-compact triggered
                let target = format!("channel:{}", channel_id.get());
                let content = format!("🗜️ 자동 컨텍스트 압축 (사용률: {pct}%)");
                let _ = enqueue_outbox_best_effort(
                    shared.pg_pool.as_ref(),
                    sqlite_runtime_db(shared.as_ref()),
                    OutboxMessage {
                        target: target.as_str(),
                        content: content.as_str(),
                        bot: "notify",
                        source: "system",
                        reason_code: None,
                        session_key: None,
                    },
                )
                .await;
            }
        }
    }

    // Cleanup: only remove from DashMap if we weren't cancelled/replaced.
    // #243: When a watcher is cancelled (replaced by a new watcher or shutdown),
    // the replacement already occupies the slot — removing would delete the new entry.
    if !cancel.load(Ordering::Relaxed) {
        shared.tmux_watchers.remove(&channel_id);
    }

    let api_port = shared.api_port;
    let provider = shared.settings.read().await.provider.clone();
    let session_key =
        super::adk_session::build_adk_session_key(&shared, channel_id, &provider).await;
    let channel_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|s| s.channel_name.clone())
    };
    let dispatch_protection = super::tmux_lifecycle::resolve_dispatch_tmux_protection(
        None::<&crate::db::Db>,
        shared.pg_pool.as_ref(),
        &shared.token_hash,
        &provider,
        &tmux_session_name,
        channel_name.as_deref(),
    );
    let cleanup_plan = dead_session_cleanup_plan(dispatch_protection.is_some());

    if let Some(protection) = dispatch_protection {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ♻ tmux watcher: preserving dispatch session {} — {}",
            tmux_session_name,
            protection.log_reason()
        );
    }

    if !cleanup_plan.preserve_tmux_session {
        // Kill dead tmux session to prevent accumulation (especially for thread sessions
        // which are created per-dispatch and would otherwise linger for 24h).
        // #145: skip kill for unified-thread sessions with active auto-queue runs.
        {
            let sess = tmux_session_name.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if tmux_session_exists(&sess) && !tmux_session_has_live_pane(&sess) {
                    // Check if this is a unified-thread session before killing
                    if let Some((_, ch_name)) =
                        crate::services::provider::parse_provider_and_channel_from_tmux_name(&sess)
                    {
                        if crate::dispatch::is_unified_thread_channel_name_active(&ch_name) {
                            return;
                        }
                    }
                    crate::services::termination_audit::record_termination_for_tmux(
                        &sess,
                        None,
                        "tmux_watcher",
                        "dead_after_turn",
                        Some("watcher cleanup: dead session after turn"),
                        None,
                    );
                    record_tmux_exit_reason(&sess, "watcher cleanup: dead session after turn");

                    // #1261 (Fix B): the wrapper's stderr `[stderr] ...` lines and
                    // synthetic `[fatal startup error]` markers go to the PTY, not
                    // to the structured jsonl that `recent_output_tail` reads. Dump
                    // the current pane buffer to a `death_pane_log` file BEFORE we
                    // kill the session so the wrapper-level death context is still
                    // recoverable post-mortem. Kept out of `cleanup_session_temp_files`
                    // EXTS on purpose — the file persists past the cleanup and is
                    // overwritten on the next death of the same session.
                    if let Some(pane_content) =
                        crate::services::platform::tmux::capture_pane(&sess, -1000)
                    {
                        let stamped = format!(
                            "[{}] post-mortem capture for session={}\n{}",
                            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                            sess,
                            pane_content
                        );
                        let path = crate::services::tmux_common::session_temp_path(
                            &sess,
                            "death_pane_log",
                        );
                        if let Some(parent) = std::path::Path::new(&path).parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        let _ = std::fs::write(&path, stamped);
                    }

                    // #1261 (codex P2): the `capture_pane` subprocess above
                    // widens the gap between the outer dead-pane gate and the
                    // kill. In that window a concurrent follow-up could run
                    // claude.rs::start_claude, which kills the stale session
                    // (line 1294), respawns a fresh live session with the
                    // same name (line 1379), and we'd then kill the brand-new
                    // session here. Revalidate the dead-pane condition right
                    // before the kill so we only tear down the same
                    // dead-paned session we capture-paned.
                    if tmux_session_exists(&sess) && !tmux_session_has_live_pane(&sess) {
                        crate::services::platform::tmux::kill_session_with_reason(
                            &sess,
                            "watcher cleanup: dead session after turn",
                        );
                    }
                    // NOTE: jsonl/FIFO/etc. cleanup intentionally NOT done here.
                    // `claude.rs::start_claude` calls
                    // `cleanup_session_temp_files` at spawn time
                    // (`claude.rs:1304`) before recreating the canonical paths,
                    // which already covers the "next-spawn against stale jsonl"
                    // case. Pairing a watcher-side cleanup with the kill races
                    // with that spawn-side cleanup + recreate (#1261 codex P1):
                    // if the next message lands between our `kill_session` and
                    // our cleanup, claude's spawn already laid down fresh files
                    // and our cleanup deletes them, breaking the new turn.
                    // Keep cleanup as a single-source-of-truth on the spawn
                    // path.
                }
            })
            .await;
        }
    }

    if cleanup_plan.report_idle_status {
        // Report idle status to DB so the dashboard doesn't show stale "working" state.
        // Always report idle when the watcher exits, even if dispatch protection
        // keeps the dead tmux session around for the active-dispatch safety path.
        let thread_channel_id = channel_name
            .as_deref()
            .and_then(super::adk_session::parse_thread_channel_id_from_name);
        let agent_id = resolve_role_binding(channel_id, channel_name.as_deref())
            .map(|binding| binding.role_id);
        super::adk_session::post_adk_session_status(
            session_key.as_deref(),
            channel_name.as_deref(),
            None, // model
            "idle",
            &provider,
            None, // session_info
            None, // tokens
            None, // cwd
            None, // dispatch_id
            thread_channel_id,
            agent_id.as_deref(),
            api_port,
        )
        .await;
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] 👁 tmux watcher stopped for #{tmux_session_name}");
}

/// Tracks tool/thinking status during watcher output processing.
pub(super) struct WatcherToolState {
    /// Current tool status line (e.g. "⚙ Bash: `ls`")
    pub current_tool_line: Option<String>,
    /// Previous distinct tool/thinking status for 2-line trail rendering.
    pub prev_tool_status: Option<String>,
    /// Whether we are currently inside a thinking block
    pub in_thinking: bool,
    /// Whether any tool_use block has been seen in this turn
    pub any_tool_used: bool,
    /// Whether a text block was streamed after the last tool_use
    pub has_post_tool_text: bool,
    /// Structured transcript events collected during watcher replay
    pub transcript_events: Vec<SessionTranscriptEvent>,
}

impl WatcherToolState {
    pub fn new() -> Self {
        Self {
            current_tool_line: None,
            prev_tool_status: None,
            in_thinking: false,
            any_tool_used: false,
            has_post_tool_text: false,
            transcript_events: Vec::new(),
        }
    }

    fn set_current_tool_line(&mut self, next_tool_line: Option<String>) {
        let current_tool_line = self.current_tool_line.clone();
        super::formatting::preserve_previous_tool_status(
            &mut self.prev_tool_status,
            current_tool_line.as_deref(),
            next_tool_line.as_deref(),
        );
        self.current_tool_line = next_tool_line;
    }

    fn clear_current_tool_line(&mut self) {
        let current_tool_line = self.current_tool_line.clone();
        super::formatting::preserve_previous_tool_status(
            &mut self.prev_tool_status,
            current_tool_line.as_deref(),
            None,
        );
        self.current_tool_line = None;
    }

    fn mark_thinking(&mut self) {
        if self.current_tool_line.as_deref() != Some(REDACTED_THINKING_STATUS_LINE) {
            self.set_current_tool_line(Some(REDACTED_THINKING_STATUS_LINE.to_string()));
        }
    }
}

/// Process buffered lines for the tmux watcher.
/// Extracts text content, tracks tool status, and detects result events.
/// Returns true if a "result" event was found.
pub(super) fn process_watcher_lines(
    buffer: &mut String,
    state: &mut StreamLineState,
    full_response: &mut String,
    tool_state: &mut WatcherToolState,
) -> WatcherLineOutcome {
    let mut outcome = WatcherLineOutcome::default();

    while let Some(pos) = buffer.find('\n') {
        let line: String = buffer.drain(..=pos).collect();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Parse the JSON line
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            observe_stream_context(&val, state);
            let event_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match event_type {
                "assistant" => {
                    if let Some(message) = val.get("message") {
                        if let Some(model) = message.get("model").and_then(|value| value.as_str()) {
                            state.last_model = Some(model.to_string());
                        }
                        if let Some(usage) = message.get("usage") {
                            state.accum_input_tokens = state.accum_input_tokens.saturating_add(
                                usage
                                    .get("input_tokens")
                                    .and_then(|value| value.as_u64())
                                    .unwrap_or(0),
                            );
                            state.accum_cache_read_tokens =
                                state.accum_cache_read_tokens.saturating_add(
                                    usage
                                        .get("cache_read_input_tokens")
                                        .and_then(|value| value.as_u64())
                                        .unwrap_or(0),
                                );
                            state.accum_cache_create_tokens =
                                state.accum_cache_create_tokens.saturating_add(
                                    usage
                                        .get("cache_creation_input_tokens")
                                        .and_then(|value| value.as_u64())
                                        .unwrap_or(0),
                                );
                            state.accum_output_tokens = state.accum_output_tokens.saturating_add(
                                usage
                                    .get("output_tokens")
                                    .and_then(|value| value.as_u64())
                                    .unwrap_or(0),
                            );
                        }
                        // Text content from assistant message
                        if let Some(content) = message.get("content") {
                            if let Some(arr) = content.as_array() {
                                for block in arr {
                                    let block_type = block.get("type").and_then(|t| t.as_str());
                                    if block_type == Some("text") {
                                        if let Some(text) =
                                            block.get("text").and_then(|t| t.as_str())
                                        {
                                            full_response.push_str(text);
                                            push_transcript_event(
                                                &mut tool_state.transcript_events,
                                                SessionTranscriptEvent {
                                                    kind: SessionTranscriptEventKind::Assistant,
                                                    tool_name: None,
                                                    summary: None,
                                                    content: text.to_string(),
                                                    status: Some("success".to_string()),
                                                    is_error: false,
                                                },
                                            );
                                            if tool_state.any_tool_used {
                                                tool_state.has_post_tool_text = true;
                                            }
                                            tool_state.clear_current_tool_line();
                                        }
                                    } else if block_type == Some("tool_use") {
                                        tool_state.any_tool_used = true;
                                        tool_state.has_post_tool_text = false;
                                        let name = block
                                            .get("name")
                                            .and_then(|n| n.as_str())
                                            .unwrap_or("Tool");
                                        let input_str = block
                                            .get("input")
                                            .map(|i| i.to_string())
                                            .unwrap_or_default();
                                        let summary = format_tool_input(name, &input_str);
                                        let display = if summary.is_empty() {
                                            format!("⚙ {}", name)
                                        } else {
                                            let truncated: String =
                                                summary.chars().take(500).collect();
                                            format!("⚙ {}: {}", name, truncated)
                                        };
                                        tool_state.set_current_tool_line(Some(display));
                                        push_transcript_event(
                                            &mut tool_state.transcript_events,
                                            SessionTranscriptEvent {
                                                kind: SessionTranscriptEventKind::ToolUse,
                                                tool_name: Some(name.to_string()),
                                                summary: (!summary.is_empty()).then_some(summary),
                                                content: input_str,
                                                status: Some("running".to_string()),
                                                is_error: false,
                                            },
                                        );
                                    } else if block_type == Some("thinking") {
                                        tool_state.mark_thinking();
                                        push_transcript_event(
                                            &mut tool_state.transcript_events,
                                            redacted_thinking_transcript_event(),
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                "content_block_start" => {
                    if let Some(cb) = val.get("content_block") {
                        let cb_type = cb.get("type").and_then(|t| t.as_str());
                        if cb_type == Some("thinking") {
                            tool_state.in_thinking = true;
                            tool_state.mark_thinking();
                        } else if cb_type == Some("tool_use") {
                            tool_state.any_tool_used = true;
                            tool_state.has_post_tool_text = false;
                            let name = cb.get("name").and_then(|n| n.as_str()).unwrap_or("Tool");
                            tool_state.set_current_tool_line(Some(format!("⚙ {}", name)));
                        }
                    }
                }
                "content_block_delta" => {
                    if let Some(delta) = val.get("delta") {
                        if delta.get("thinking").and_then(|t| t.as_str()).is_some() {
                            tool_state.mark_thinking();
                        } else if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                            full_response.push_str(text);
                            if tool_state.any_tool_used {
                                tool_state.has_post_tool_text = true;
                            }
                            tool_state.clear_current_tool_line();
                        }
                    }
                }
                "content_block_stop" => {
                    if tool_state.in_thinking {
                        tool_state.in_thinking = false;
                        tool_state.mark_thinking();
                        push_transcript_event(
                            &mut tool_state.transcript_events,
                            redacted_thinking_transcript_event(),
                        );
                    } else if let Some(line) = tool_state.current_tool_line.clone() {
                        // Tool completed — mark with checkmark
                        if line.starts_with("⚙") {
                            tool_state.set_current_tool_line(Some(line.replacen("⚙", "✓", 1)));
                        }
                    }
                }
                "result" => {
                    outcome.stale_resume_detected = outcome.stale_resume_detected
                        || super::turn_bridge::result_event_has_stale_resume_error(&val);
                    if let Some(session_id) = val.get("session_id").and_then(|value| value.as_str())
                    {
                        state.last_session_id = Some(session_id.to_string());
                    }
                    let is_error = val
                        .get("is_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let result_str = extract_result_error_text(&val);
                    push_transcript_event(
                        &mut tool_state.transcript_events,
                        SessionTranscriptEvent {
                            kind: if is_error {
                                SessionTranscriptEventKind::Error
                            } else {
                                SessionTranscriptEventKind::Result
                            },
                            tool_name: None,
                            summary: Some(if result_str.trim().is_empty() {
                                if is_error {
                                    "error".to_string()
                                } else {
                                    "completed".to_string()
                                }
                            } else {
                                truncate_str(&result_str, 120).to_string()
                            }),
                            content: result_str.clone(),
                            status: Some(if is_error { "error" } else { "success" }.to_string()),
                            is_error,
                        },
                    );

                    if is_error {
                        if is_prompt_too_long_message(&result_str) {
                            outcome.is_prompt_too_long = true;
                        }
                        if is_auth_error_message(&result_str) {
                            outcome.is_auth_error = true;
                            outcome.auth_error_message.get_or_insert(result_str.clone());
                        }
                        if let Some(message) = detect_provider_overload_message(&result_str) {
                            outcome.is_provider_overloaded = true;
                            outcome.provider_overload_message.get_or_insert(message);
                        }
                    }

                    // Use result text when streaming didn't capture the final response:
                    // 1. full_response is empty — no text was streamed at all
                    // 2. tools were used but no text was streamed after the last tool
                    //    (accumulated text is stale pre-tool narration)
                    if !outcome.is_prompt_too_long
                        && !outcome.is_auth_error
                        && !outcome.is_provider_overloaded
                        && !result_str.is_empty()
                    {
                        if full_response.is_empty()
                            || (tool_state.any_tool_used && !tool_state.has_post_tool_text)
                        {
                            full_response.clear();
                            full_response.push_str(&result_str);
                        }
                    }
                    if let Some(usage) = val.get("usage") {
                        state.accum_input_tokens = usage
                            .get("input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        state.accum_cache_read_tokens = usage
                            .get("cache_read_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        state.accum_cache_create_tokens = usage
                            .get("cache_creation_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        state.accum_output_tokens = usage
                            .get("output_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                    }

                    state.final_result = Some(String::new());
                    outcome.found_result = true;
                    // #1216: stop after the first turn-terminating event so a
                    // buffer containing multiple completed turns (post-deploy
                    // backlog, paused watcher resume) does not merge their
                    // `assistant` text into a single `full_response`. The
                    // unprocessed tail stays in `buffer` for the next call.
                    break;
                }
                "system" => {
                    if val.get("subtype").and_then(|s| s.as_str()) == Some("init")
                        && let Some(session_id) =
                            val.get("session_id").and_then(|value| value.as_str())
                    {
                        state.last_session_id = Some(session_id.to_string());
                    }
                    // Detect auto-compaction events from Claude Code
                    if let Some(msg) = val.get("message").and_then(|m| m.as_str()) {
                        let lower = msg.to_ascii_lowercase();
                        if lower.contains("compacted")
                            || lower.contains("auto-compact")
                            || lower.contains("conversation has been compressed")
                        {
                            outcome.auto_compacted = true;
                        }
                    }
                    if let Some(subtype) = val.get("subtype").and_then(|s| s.as_str()) {
                        if subtype == "compact" || subtype == "auto_compact" {
                            outcome.auto_compacted = true;
                        }
                        // `task_notification` is the authoritative
                        // provider-normalized marker for a background-trigger
                        // turn (Claude emits it directly; Codex normalizes
                        // `background_event` into the same JSONL shape). It
                        // lets us distinguish a background-trigger turn from
                        // a normal foreground turn whose inflight file was
                        // merely cleared early by turn_bridge.
                        if subtype == "task_notification" {
                            outcome.task_notification_kind = merge_task_notification_kind(
                                outcome.task_notification_kind,
                                classify_task_notification_kind(&val, state),
                            );
                        }
                    }
                }
                _ => {}
            }
        } else if is_auth_error_message(trimmed) {
            outcome.found_result = true;
            outcome.is_auth_error = true;
            outcome
                .auth_error_message
                .get_or_insert(trimmed.to_string());
            push_transcript_event(
                &mut tool_state.transcript_events,
                SessionTranscriptEvent {
                    kind: SessionTranscriptEventKind::Error,
                    tool_name: None,
                    summary: Some("authentication error".to_string()),
                    content: trimmed.to_string(),
                    status: Some("error".to_string()),
                    is_error: true,
                },
            );
            state.final_result = Some(String::new());
            // #1216: see `result` arm — stop after a turn-terminating event.
            break;
        } else if let Some(message) = detect_provider_overload_message(trimmed) {
            outcome.found_result = true;
            outcome.is_provider_overloaded = true;
            outcome.provider_overload_message.get_or_insert(message);
            push_transcript_event(
                &mut tool_state.transcript_events,
                SessionTranscriptEvent {
                    kind: SessionTranscriptEventKind::Error,
                    tool_name: None,
                    summary: Some("provider overload".to_string()),
                    content: trimmed.to_string(),
                    status: Some("error".to_string()),
                    is_error: true,
                },
            );
            state.final_result = Some(String::new());
            // #1216: see `result` arm — stop after a turn-terminating event.
            break;
        }
    }

    outcome
}

/// Remove jsonl/input/prompt/owner/etc files in the persistent sessions
/// directory that no longer belong to a running tmux session. Conservative:
/// require an owner marker (or the jsonl) to be older than
/// `ORPHAN_MIN_AGE_SECS` and require the session to be absent from tmux
/// before deleting. Legacy `/tmp/` files are *never* swept at startup —
/// pre-migration wrappers may still be writing into them.
async fn sweep_orphan_session_files() {
    const ORPHAN_MIN_AGE_SECS: u64 = 10 * 60; // 10 minutes

    let Some(dir) = crate::services::tmux_common::persistent_sessions_dir() else {
        return;
    };
    if !dir.exists() {
        return;
    }

    // List live tmux sessions.
    let live: std::collections::HashSet<String> = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(crate::services::platform::tmux::list_session_names),
    )
    .await
    {
        Ok(Ok(Ok(names))) => names.into_iter().collect(),
        _ => return, // tmux unavailable — skip sweep rather than risk false positives
    };

    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };

    // Group files under the sessions dir by the `agentdesk-<hash>-<host>-<session>`
    // prefix. Any prefix whose session name is not in `live` *and* whose
    // oldest file mtime is older than ORPHAN_MIN_AGE_SECS is swept.
    let mut groups: std::collections::HashMap<String, (String, std::time::SystemTime)> =
        std::collections::HashMap::new();
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !name.starts_with("agentdesk-") {
            continue;
        }
        // Strip extension.
        let stem = match name.rsplit_once('.') {
            Some((s, _)) => s.to_string(),
            None => name.clone(),
        };
        // Session name is the last token after the fourth dash — but our
        // prefix format is `agentdesk-<12hex>-<host>-<session>` and host
        // may contain dashes. The simplest robust approach: split_once on
        // `agentdesk-<hash>-<host>-` is hard to reverse, so instead we use
        // the owner file's prefix as the grouping key directly — any file
        // whose stem matches some live session (ends with `-<live>`) is kept.
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or_else(|_| std::time::SystemTime::now());
        groups
            .entry(stem.clone())
            .and_modify(|slot| {
                if mtime < slot.1 {
                    *slot = (stem.clone(), mtime);
                }
            })
            .or_insert((stem, mtime));
    }

    let now = std::time::SystemTime::now();
    let mut swept = 0usize;
    for (stem, (_, oldest_mtime)) in groups {
        // Is this stem associated with any live tmux session? We check
        // whether ANY live session name appears as a suffix of the stem.
        // Since session names are distinctive (provider:channel shape), a
        // conservative suffix match keeps ambiguity low; we also require
        // that the match is preceded by a dash so we don't match e.g.
        // "claude:foo" against a stem ending with "-thisisnotclaude:foo".
        let is_live = live.iter().any(|live_name| {
            let needle = format!("-{}", live_name);
            stem.ends_with(&needle) || stem == *live_name
        });
        if is_live {
            continue;
        }
        // Conservative: require age threshold.
        let age = now
            .duration_since(oldest_mtime)
            .unwrap_or(std::time::Duration::ZERO);
        if age.as_secs() < ORPHAN_MIN_AGE_SECS {
            continue;
        }
        // Delete every file under this stem.
        let Ok(iter) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in iter.flatten() {
            if let Ok(fname) = entry.file_name().into_string() {
                if fname.starts_with(&format!("{}.", stem)) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        swept += 1;
    }
    if swept > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🧹 Swept {} orphan session file group(s) from {}",
            swept,
            dir.display()
        );
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::FallbackPlaceholderCleanupDecision;
    use super::{
        CANCEL_TEARDOWN_GRACE_BYTES, DeadSessionCleanupPlan,
        MONITOR_AUTO_TURN_DEFERRED_REASON_CODE, MONITOR_AUTO_TURN_PREAMBLE_HINT,
        MONITOR_AUTO_TURN_REASON_CODE, MissingInflightReattachOutcome, OffsetAdvanceDecision,
        PlaceholderSuppressContext, PlaceholderSuppressDecision, PlaceholderSuppressOrigin,
        READY_FOR_INPUT_STUCK_REASON, SUPPRESSED_INTERNAL_LABEL, SUPPRESSED_RESTART_LABEL,
        SuppressedPlaceholderAction, TmuxWatcherHandle, TmuxWatcherRegistry, WatcherClaimAction,
        WatcherToolState, build_bg_trigger_session_key, cancel_induced_watcher_death,
        claim_or_reuse_watcher, clear_recent_turn_stops_for_tests,
        clear_recent_watcher_reattach_offsets_for_tests, consume_monitor_auto_turn_preamble_once,
        dead_session_cleanup_plan, decide_placeholder_suppression,
        enqueue_background_trigger_response_to_notify_outbox,
        enqueue_monitor_auto_turn_suppressed_notification, fail_dispatch_for_ready_for_input_stall,
        fallback_placeholder_cleanup_decision, finish_monitor_auto_turn,
        finish_restored_watcher_active_turn, format_monitor_suppressed_body,
        format_monitor_suppressed_label, lifecycle_reason_code_for_tmux_exit,
        load_restored_provider_session_id, matching_recent_watcher_reattach_offset,
        missing_inflight_fallback_plan, notify_path_offset_advance_decision,
        orphan_suppressed_placeholder_action, parse_bg_trigger_offset_from_session_key,
        process_watcher_lines, recent_turn_stop_for_channel, recent_turn_stop_for_watcher_range,
        record_recent_turn_stop_for_tests, record_recent_turn_stop_with_offset_for_tests,
        record_recent_watcher_reattach_offset, refresh_session_heartbeat_from_tmux_output,
        reset_stale_local_relay_offset_if_output_regressed,
        reset_stale_relay_watermark_if_output_regressed, restored_watcher_turn_from_inflight,
        rollback_enqueued_offset_for_reconciled_failures,
        should_flush_post_terminal_success_continuation,
        should_suppress_streaming_placeholder_after_recent_stop,
        should_suppress_terminal_output_after_recent_stop, start_monitor_auto_turn_when_available,
        strip_inprogress_indicators, suppressed_placeholder_action, terminal_relay_decision,
        tmux_death_is_normal_completion, tmux_death_lifecycle_notification_reason,
        trigger_missing_inflight_reattach, wait_for_reacquired_turn_bridge_inflight_state,
        watcher_ready_for_input_turn_completed, watcher_should_yield_to_inflight_state,
        watcher_stream_seed,
    };
    use crate::db::session_transcripts::SessionTranscriptEventKind;
    use crate::services::agent_protocol::TaskNotificationKind;
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::discord::placeholder_cleanup::PlaceholderCleanupOutcome;
    use crate::services::discord::runtime_store::test_env_lock;
    use crate::services::provider::{CancelToken, ProviderKind, ReadyForInputIdleTracker};
    use crate::services::session_backend::StreamLineState;
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::Duration;

    fn test_watcher_handle(tmux_session_name: &str) -> TmuxWatcherHandle {
        TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            paused: Arc::new(AtomicBool::new(true)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(AtomicBool::new(false)),
            pause_epoch: Arc::new(AtomicU64::new(0)),
            turn_delivered: Arc::new(AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
                super::super::tmux_watcher_now_ms(),
            )),
        }
    }

    #[test]
    fn normal_completion_exit_reason_maps_to_dedicated_lifecycle_code() {
        assert_eq!(
            lifecycle_reason_code_for_tmux_exit("turn completed (code 0)"),
            "lifecycle.normal_completion"
        );
        assert_eq!(
            lifecycle_reason_code_for_tmux_exit("dispatch turn completed"),
            "lifecycle.normal_completion"
        );
        assert_eq!(
            lifecycle_reason_code_for_tmux_exit("unified-thread run completed"),
            "lifecycle.normal_completion"
        );
    }

    #[test]
    fn normal_completion_detection_requires_a_trusted_exit_reason() {
        assert!(tmux_death_is_normal_completion(
            Some("turn completed (code 0)"),
            Some("recent_output=completed_result_present")
        ));
        assert!(!tmux_death_is_normal_completion(
            None,
            Some("recent_output=completed_result_present")
        ));
    }

    #[test]
    fn tmux_death_lifecycle_notification_skips_missing_or_unknown_reason() {
        assert_eq!(tmux_death_lifecycle_notification_reason(None), None);
        assert_eq!(tmux_death_lifecycle_notification_reason(Some("")), None);
        assert_eq!(
            tmux_death_lifecycle_notification_reason(Some("unknown")),
            None
        );
        assert_eq!(
            tmux_death_lifecycle_notification_reason(Some("[2026-04-26 22:26:38] unknown")),
            None
        );
    }

    #[test]
    fn tmux_death_lifecycle_notification_keeps_actionable_cleanup_reason() {
        assert_eq!(
            tmux_death_lifecycle_notification_reason(Some(
                "[2026-04-26 22:26:38] idle 60분 초과 — 자동 정리"
            )),
            Some("idle 60분 초과 — 자동 정리")
        );
        assert_eq!(
            tmux_death_lifecycle_notification_reason(Some("explicit cleanup via force-kill API")),
            None
        );
    }

    #[test]
    fn restored_live_tmux_session_loads_namespaced_provider_session_id() {
        let db = crate::db::test_db();
        let provider = ProviderKind::Codex;
        let session_key = crate::services::discord::adk_session::build_namespaced_session_key(
            "tokenxyz",
            &provider,
            &provider.build_tmux_session_name("adk-cdx"),
        );
        db.lock()
            .unwrap()
            .execute(
                "INSERT INTO sessions (session_key, provider, claude_session_id) VALUES (?1, ?2, ?3)",
                [session_key.as_str(), provider.as_str(), "persisted-sid-1"],
            )
            .unwrap();

        assert_eq!(
            load_restored_provider_session_id(Some(&db), None, "tokenxyz", &provider, "adk-cdx",)
                .as_deref(),
            Some("persisted-sid-1")
        );
    }

    #[test]
    fn restored_live_tmux_session_falls_back_to_legacy_session_key() {
        let db = crate::db::test_db();
        let provider = ProviderKind::Codex;
        let session_key = crate::services::discord::adk_session::build_legacy_session_key(
            &provider.build_tmux_session_name("adk-cdx"),
        );
        db.lock()
            .unwrap()
            .execute(
                "INSERT INTO sessions (session_key, provider, claude_session_id) VALUES (?1, ?2, ?3)",
                [session_key.as_str(), provider.as_str(), "legacy-sid-1"],
            )
            .unwrap();

        assert_eq!(
            load_restored_provider_session_id(Some(&db), None, "tokenxyz", &provider, "adk-cdx",)
                .as_deref(),
            Some("legacy-sid-1")
        );
    }

    #[test]
    fn watcher_output_activity_refreshes_namespaced_session_heartbeat() {
        let db = crate::db::test_db();
        let provider = ProviderKind::Codex;
        let channel_name = "adk-cdx-t1485506232256168011";
        let tmux_name = provider.build_tmux_session_name(channel_name);
        let session_key = crate::services::discord::adk_session::build_namespaced_session_key(
            "tokenxyz", &provider, &tmux_name,
        );
        db.lock()
            .unwrap()
            .execute(
                "INSERT INTO sessions
                 (session_key, provider, status, thread_channel_id, last_heartbeat, created_at)
                 VALUES (?1, ?2, 'idle', '1485506232256168011', '2026-04-09 01:02:03', '2026-04-09 01:02:03')",
                [session_key.as_str(), provider.as_str()],
            )
            .unwrap();

        assert!(refresh_session_heartbeat_from_tmux_output(
            Some(&db),
            None,
            "tokenxyz",
            &provider,
            &tmux_name,
            Some(1485506232256168011),
        ));

        let last_heartbeat: String = db
            .lock()
            .unwrap()
            .query_row(
                "SELECT last_heartbeat FROM sessions WHERE session_key = ?1",
                [session_key.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(last_heartbeat, "2026-04-09 01:02:03");
    }

    #[test]
    fn claim_or_reuse_watcher_replaces_different_tmux_on_same_channel() {
        let watchers = TmuxWatcherRegistry::new();
        let channel_id = ChannelId::new(1485506232256168123);

        assert!(super::try_claim_watcher(
            &watchers,
            channel_id,
            test_watcher_handle("AgentDesk-codex-adk-cdx-a")
        ));
        assert_eq!(watchers.len(), 1);

        let outcome = claim_or_reuse_watcher(
            &watchers,
            channel_id,
            test_watcher_handle("AgentDesk-codex-adk-cdx-b"),
            &ProviderKind::Codex,
            "unit-test",
        );
        assert_eq!(
            outcome.action(),
            WatcherClaimAction::SpawnReplacedDifferentSession
        );
        assert_eq!(outcome.owner_channel_id(), channel_id);
        assert_eq!(watchers.len(), 1);

        let watcher = watchers.get(&channel_id).expect("watcher should exist");
        assert_eq!(watcher.tmux_session_name, "AgentDesk-codex-adk-cdx-b");
        assert!(watcher.paused.load(Ordering::Relaxed));
    }

    #[test]
    fn claim_or_reuse_watcher_reuses_live_same_tmux_session() {
        let watchers = TmuxWatcherRegistry::new();
        let channel_a = ChannelId::new(1485506232256168124);
        let channel_b = ChannelId::new(1485506232256168125);
        let tmux_name = "AgentDesk-codex-adk-cdx";

        let initial = test_watcher_handle(tmux_name);
        let initial_cancel = initial.cancel.clone();
        let initial_paused = initial.paused.clone();
        initial_paused.store(false, Ordering::Relaxed);

        assert!(super::try_claim_watcher(&watchers, channel_a, initial));
        let outcome = claim_or_reuse_watcher(
            &watchers,
            channel_b,
            test_watcher_handle(tmux_name),
            &ProviderKind::Codex,
            "unit-test-reuse",
        );
        assert_eq!(outcome.action(), WatcherClaimAction::ReuseExisting);
        assert_eq!(outcome.owner_channel_id(), channel_a);
        assert_eq!(watchers.len(), 1, "same tmux must have one owner");
        assert!(
            !initial_cancel.load(Ordering::Relaxed),
            "live incumbent must be reused, not cancelled"
        );
        assert!(watchers.contains_key(&channel_a));
        assert!(!watchers.contains_key(&channel_b));
        watchers.assert_invariants_for_tests();
    }

    #[test]
    fn cross_channel_same_tmux_reuse_targets_owner_watcher_state() {
        let watchers = TmuxWatcherRegistry::new();
        let channel_a = ChannelId::new(1485506232256168136);
        let channel_b = ChannelId::new(1485506232256168137);
        let tmux_name = "AgentDesk-codex-adk-cdx-owner";

        let initial = test_watcher_handle(tmux_name);
        let owner_paused = initial.paused.clone();
        let owner_pause_epoch = initial.pause_epoch.clone();
        let owner_resume_offset = initial.resume_offset.clone();
        let owner_turn_delivered = initial.turn_delivered.clone();
        assert!(super::try_claim_watcher(&watchers, channel_a, initial));

        let incoming = test_watcher_handle(tmux_name);
        let incoming_paused = incoming.paused.clone();
        let incoming_turn_delivered = incoming.turn_delivered.clone();
        incoming_paused.store(false, Ordering::Relaxed);
        let outcome = claim_or_reuse_watcher(
            &watchers,
            channel_b,
            incoming,
            &ProviderKind::Codex,
            "unit-test-cross-channel-owner",
        );

        assert_eq!(outcome.action(), WatcherClaimAction::ReuseExisting);
        assert_eq!(outcome.owner_channel_id(), channel_a);
        assert!(
            watchers.get(&channel_b).is_none(),
            "duplicate attach must not install a watcher under the requested channel"
        );

        let owner_channel = outcome.owner_channel_id();
        if let Some(watcher) = watchers.get(&owner_channel) {
            watcher.pause_epoch.fetch_add(1, Ordering::Relaxed);
            watcher.paused.store(true, Ordering::Relaxed);
            watcher.turn_delivered.store(true, Ordering::Relaxed);
            if let Ok(mut guard) = watcher.resume_offset.lock() {
                *guard = Some(42);
            }
            watcher.paused.store(false, Ordering::Relaxed);
        }

        assert_eq!(owner_pause_epoch.load(Ordering::Relaxed), 1);
        assert!(!owner_paused.load(Ordering::Relaxed));
        assert!(owner_turn_delivered.load(Ordering::Relaxed));
        assert_eq!(
            owner_resume_offset
                .lock()
                .expect("resume offset lock")
                .as_ref(),
            Some(&42)
        );
        assert!(!incoming_paused.load(Ordering::Relaxed));
        assert!(!incoming_turn_delivered.load(Ordering::Relaxed));
        watchers.assert_invariants_for_tests();
    }

    #[test]
    fn claim_or_reuse_watcher_replaces_stale_heartbeat_same_tmux_session() {
        let watchers = TmuxWatcherRegistry::new();
        let channel_a = ChannelId::new(1485506232256168144);
        let channel_b = ChannelId::new(1485506232256168145);
        let tmux_name = "AgentDesk-codex-adk-cdx-stale-heartbeat";

        let initial = test_watcher_handle(tmux_name);
        let initial_cancel = initial.cancel.clone();
        initial.last_heartbeat_ts_ms.store(
            super::super::tmux_watcher_now_ms() - super::super::TMUX_WATCHER_STALE_HEARTBEAT_MS - 1,
            Ordering::Release,
        );
        assert!(super::try_claim_watcher(&watchers, channel_a, initial));

        let outcome = claim_or_reuse_watcher(
            &watchers,
            channel_b,
            test_watcher_handle(tmux_name),
            &ProviderKind::Codex,
            "unit-test-stale-heartbeat",
        );

        assert_eq!(outcome.action(), WatcherClaimAction::SpawnReplacedStale);
        assert_eq!(outcome.owner_channel_id(), channel_b);
        assert!(
            initial_cancel.load(Ordering::Relaxed),
            "stale incumbent must be cancelled before replacement"
        );
        assert!(!watchers.contains_key(&channel_a));
        assert!(watchers.contains_key(&channel_b));
        watchers.assert_invariants_for_tests();
    }

    #[test]
    fn watcher_registry_rekeys_same_channel_to_new_tmux_session() {
        let watchers = TmuxWatcherRegistry::new();
        let channel_id = ChannelId::new(1485506232256168139);
        let old_tmux = "AgentDesk-codex-adk-cdx-old-owner";
        let new_tmux = "AgentDesk-codex-adk-cdx-new-owner";

        assert!(super::try_claim_watcher(
            &watchers,
            channel_id,
            test_watcher_handle(old_tmux)
        ));

        let outcome = claim_or_reuse_watcher(
            &watchers,
            channel_id,
            test_watcher_handle(new_tmux),
            &ProviderKind::Codex,
            "unit-test-direct-tmux-rekey",
        );

        assert_eq!(
            outcome.action(),
            WatcherClaimAction::SpawnReplacedDifferentSession
        );
        assert_eq!(watchers.owner_channel_for_tmux_session(old_tmux), None);
        assert_eq!(
            watchers.owner_channel_for_tmux_session(new_tmux),
            Some(channel_id)
        );
        assert_eq!(
            watchers
                .channel_binding(&channel_id)
                .expect("channel remains attached")
                .tmux_session_name,
            new_tmux
        );
        assert_eq!(watchers.len(), 1);
        watchers.assert_invariants_for_tests();
    }

    #[test]
    fn claim_or_reuse_watcher_replaces_cancelled_same_tmux_session() {
        let watchers = TmuxWatcherRegistry::new();
        let channel_a = ChannelId::new(1485506232256168126);
        let channel_b = ChannelId::new(1485506232256168127);
        let tmux_name = "AgentDesk-codex-adk-cdx";

        let initial = test_watcher_handle(tmux_name);
        initial.cancel.store(true, Ordering::Relaxed);
        assert!(super::try_claim_watcher(&watchers, channel_a, initial));

        let incoming = test_watcher_handle(tmux_name);
        let incoming_cancel = incoming.cancel.clone();
        let outcome = claim_or_reuse_watcher(
            &watchers,
            channel_b,
            incoming,
            &ProviderKind::Codex,
            "unit-test-replace-stale",
        );
        assert_eq!(outcome.action(), WatcherClaimAction::SpawnReplacedStale);
        assert_eq!(outcome.owner_channel_id(), channel_b);
        assert_eq!(watchers.len(), 1, "stale same-tmux owner is replaced");
        assert!(!watchers.contains_key(&channel_a));
        assert!(watchers.contains_key(&channel_b));
        assert!(!incoming_cancel.load(Ordering::Relaxed));
        watchers.assert_invariants_for_tests();
    }

    #[test]
    fn stale_owner_remove_after_replacement_does_not_delete_new_tmux_owner() {
        let watchers = TmuxWatcherRegistry::new();
        let old_owner = ChannelId::new(1485506232256168140);
        let new_owner = ChannelId::new(1485506232256168141);
        let tmux_name = "AgentDesk-codex-adk-cdx-remove-after-claim";

        let initial = test_watcher_handle(tmux_name);
        initial.cancel.store(true, Ordering::Relaxed);
        assert!(super::try_claim_watcher(&watchers, old_owner, initial));

        let outcome = claim_or_reuse_watcher(
            &watchers,
            new_owner,
            test_watcher_handle(tmux_name),
            &ProviderKind::Codex,
            "unit-test-remove-after-claim",
        );
        assert_eq!(outcome.action(), WatcherClaimAction::SpawnReplacedStale);
        assert_eq!(outcome.owner_channel_id(), new_owner);

        assert!(
            watchers.remove(&old_owner).is_none(),
            "old owner cleanup must not remove the newly claimed tmux watcher"
        );
        assert!(!watchers.contains_key(&old_owner));
        assert!(watchers.contains_key(&new_owner));
        assert_eq!(
            watchers.owner_channel_for_tmux_session(tmux_name),
            Some(new_owner)
        );
        assert_eq!(watchers.len(), 1);
        watchers.assert_invariants_for_tests();
    }

    #[test]
    fn stale_owner_remove_interleaving_with_same_tmux_claim_preserves_registry() {
        let watchers = Arc::new(TmuxWatcherRegistry::new());
        let old_owner = ChannelId::new(1485506232256168142);
        let new_owner = ChannelId::new(1485506232256168143);
        let tmux_name = "AgentDesk-codex-adk-cdx-remove-claim-interleave";

        let initial = test_watcher_handle(tmux_name);
        initial.cancel.store(true, Ordering::Relaxed);
        assert!(super::try_claim_watcher(&watchers, old_owner, initial));
        watchers.assert_invariants_for_tests();

        let channel_index_removed = Arc::new(std::sync::Barrier::new(2));
        let release_remove = Arc::new(std::sync::Barrier::new(2));

        let remove_join = {
            let watchers = watchers.clone();
            let channel_index_removed = channel_index_removed.clone();
            let release_remove = release_remove.clone();
            std::thread::spawn(move || {
                watchers.remove_after_channel_index_drop_for_tests(
                    &old_owner,
                    &channel_index_removed,
                    &release_remove,
                )
            })
        };

        channel_index_removed.wait();
        assert!(
            !watchers.contains_key(&old_owner),
            "test hook must pause after the old channel index is dropped"
        );

        let (claim_started_tx, claim_started_rx) = std::sync::mpsc::channel();
        let claim_join = {
            let watchers = watchers.clone();
            std::thread::spawn(move || {
                claim_started_tx
                    .send(())
                    .expect("claim start signal should send");
                claim_or_reuse_watcher(
                    &watchers,
                    new_owner,
                    test_watcher_handle(tmux_name),
                    &ProviderKind::Codex,
                    "unit-test-remove-claim-interleave",
                )
            })
        };
        claim_started_rx
            .recv()
            .expect("claim thread should reach the claim call");
        for _ in 0..100 {
            if claim_join.is_finished() {
                break;
            }
            std::thread::yield_now();
        }
        assert!(
            !claim_join.is_finished(),
            "same-tmux claim must wait while old-owner remove holds the registry mutation lock"
        );
        assert!(
            !watchers.contains_key(&new_owner),
            "new owner cannot be installed until the old-owner remove releases the lock"
        );

        release_remove.wait();
        let removed = remove_join.join().expect("remove thread should not panic");
        assert!(
            removed.is_some(),
            "old-owner remove should remove the stale watcher"
        );
        let outcome = claim_join.join().expect("claim thread should not panic");
        assert_eq!(outcome.action(), WatcherClaimAction::SpawnFresh);
        assert_eq!(outcome.owner_channel_id(), new_owner);

        assert!(!watchers.contains_key(&old_owner));
        assert!(watchers.contains_key(&new_owner));
        assert_eq!(
            watchers.owner_channel_for_tmux_session(tmux_name),
            Some(new_owner)
        );
        assert_eq!(watchers.len(), 1);
        watchers.assert_invariants_for_tests();
    }

    #[test]
    fn concurrent_same_tmux_attach_attempts_leave_one_live_watcher() {
        let watchers = Arc::new(TmuxWatcherRegistry::new());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let tmux_name = "AgentDesk-codex-adk-cdx-race";

        let mut joins = Vec::new();
        for channel in [
            ChannelId::new(1485506232256168128),
            ChannelId::new(1485506232256168129),
        ] {
            let watchers = watchers.clone();
            let barrier = barrier.clone();
            joins.push(std::thread::spawn(move || {
                let handle = test_watcher_handle(tmux_name);
                barrier.wait();
                claim_or_reuse_watcher(
                    &watchers,
                    channel,
                    handle,
                    &ProviderKind::Codex,
                    "unit-test-race",
                )
            }));
        }

        let outcomes = joins
            .into_iter()
            .map(|join| join.join().expect("race worker should not panic"))
            .collect::<Vec<_>>();

        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| outcome.should_spawn())
                .count(),
            1,
            "exactly one attach attempt should spawn a watcher"
        );
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| outcome.action() == WatcherClaimAction::ReuseExisting)
                .count(),
            1,
            "the losing attach attempt should reuse the winner"
        );
        assert_eq!(watchers.len(), 1, "same tmux must end with one slot");
        assert_eq!(
            watchers
                .iter()
                .filter(|entry| entry.tmux_session_name == tmux_name)
                .count(),
            1
        );
    }

    // Same channel but a different tmux session still cancels the incumbent so
    // a new turn is not blocked by a stale slot from an older session.
    #[test]
    fn claim_or_reuse_watcher_dedupes_to_single_winner_and_cancels_different_session() {
        let watchers = TmuxWatcherRegistry::new();
        let channel_id = ChannelId::new(1485506232256130);

        let initial = test_watcher_handle("AgentDesk-codex-adk-cdx-old");
        let initial_cancel = initial.cancel.clone();
        let initial_paused = initial.paused.clone();
        initial_paused.store(false, Ordering::Relaxed);

        assert!(super::try_claim_watcher(&watchers, channel_id, initial));
        assert!(!initial_cancel.load(Ordering::Relaxed));

        let incoming = test_watcher_handle("AgentDesk-codex-adk-cdx-new");
        let incoming_cancel = incoming.cancel.clone();
        let incoming_paused = incoming.paused.clone();
        incoming_paused.store(true, Ordering::Relaxed);

        let outcome = claim_or_reuse_watcher(
            &watchers,
            channel_id,
            incoming,
            &ProviderKind::Codex,
            "unit-test-different-session",
        );
        assert_eq!(
            outcome.action(),
            WatcherClaimAction::SpawnReplacedDifferentSession
        );
        assert_eq!(outcome.owner_channel_id(), channel_id);
        assert_eq!(watchers.len(), 1, "exactly one watcher entry survives");

        assert!(initial_cancel.load(Ordering::Relaxed));
        assert!(!incoming_cancel.load(Ordering::Relaxed));

        let surviving = watchers.get(&channel_id).expect("watcher should exist");
        assert!(
            surviving.paused.load(Ordering::Relaxed),
            "slot must hold the incoming handle (paused=true), not the stale one",
        );
    }

    #[test]
    fn missing_inflight_reattach_reuses_live_same_tmux_owner() {
        let watchers = TmuxWatcherRegistry::new();
        let channel_id = ChannelId::new(1485506232256168138);
        let tmux_name = "AgentDesk-codex-adk-cdx-reuse";

        let initial = test_watcher_handle(tmux_name);
        let initial_cancel = initial.cancel.clone();
        assert!(super::try_claim_watcher(&watchers, channel_id, initial));

        let incoming = test_watcher_handle(tmux_name);
        let incoming_cancel = incoming.cancel.clone();
        let outcome = claim_or_reuse_watcher(
            &watchers,
            channel_id,
            incoming,
            &ProviderKind::Codex,
            "unit-test-missing-inflight-reuse",
        );

        assert_eq!(outcome.action(), WatcherClaimAction::ReuseExisting);
        assert_eq!(outcome.owner_channel_id(), channel_id);
        assert!(!outcome.should_spawn());
        assert!(
            !initial_cancel.load(Ordering::Relaxed),
            "missing-inflight metadata repair must not cancel a live same-tmux watcher"
        );
        assert!(!incoming_cancel.load(Ordering::Relaxed));
        assert_eq!(watchers.len(), 1);
        assert_eq!(
            watchers
                .get(&channel_id)
                .expect("incumbent watcher")
                .tmux_session_name,
            tmux_name
        );
    }

    // #1270 unit table: pure-logic decision for the watermark-after-output-
    // regression policy. No file I/O — pins down the
    // (stored_mtime, current_mtime, observed_eof) → new_watermark mapping
    // so it can't drift without these tests catching it.
    #[test]
    fn watermark_after_output_regression_pins_to_eof_when_generation_mtime_unchanged() {
        // Same wrapper instance: jsonl was rotated by truncate_jsonl_head_safe.
        // Pinning to current EOF avoids re-relaying surviving content
        // (PR #1256 intent).
        assert_eq!(
            super::watermark_after_output_regression(123_456_789, 123_456_789, 438_675),
            438_675
        );
    }

    #[test]
    fn watermark_after_output_regression_resets_to_zero_for_fresh_generation() {
        // Cancel→respawn: claude.rs cleanup_session_temp_files deleted the
        // old `.generation`, then claude.rs wrote a fresh one with a new
        // mtime. The current jsonl is fully new content, so the watcher
        // must walk it from offset 0 (#1270).
        assert_eq!(
            super::watermark_after_output_regression(123_456_789, 999_999_999, 92_566),
            0
        );
    }

    #[test]
    fn watermark_after_output_regression_resets_to_zero_when_generation_missing() {
        // `.generation` file deleted (cancel→respawn mid-stream) — treat as
        // fresh. read_generation_file_mtime_ns returns 0 in that case.
        assert_eq!(
            super::watermark_after_output_regression(123_456_789, 0, 92_566),
            0
        );
    }

    #[test]
    fn watermark_after_output_regression_resets_to_zero_on_first_observation() {
        // First time the watermark is being adjusted (stored mtime still 0
        // from the AtomicI64 default). We have no baseline to claim
        // "rotation", so default to fresh-file semantics.
        assert_eq!(
            super::watermark_after_output_regression(0, 123_456_789, 92_566),
            0
        );
    }

    #[test]
    fn relay_watermark_does_not_reset_for_same_output_epoch() {
        let shared = super::super::make_shared_data_for_tests();
        let channel_id = ChannelId::new(1485506232256168126);
        let relay_coord = shared.tmux_relay_coord(channel_id);
        relay_coord
            .confirmed_end_offset
            .store(1_024, Ordering::Release);

        assert!(!reset_stale_relay_watermark_if_output_regressed(
            shared.as_ref(),
            channel_id,
            "AgentDesk-codex-adk-cdx",
            1_024,
            "unit-test",
        ));
        assert_eq!(
            relay_coord.confirmed_end_offset.load(Ordering::Acquire),
            1_024
        );

        assert!(!reset_stale_relay_watermark_if_output_regressed(
            shared.as_ref(),
            channel_id,
            "AgentDesk-codex-adk-cdx",
            2_048,
            "unit-test",
        ));
        assert_eq!(
            relay_coord.confirmed_end_offset.load(Ordering::Acquire),
            1_024
        );
    }

    #[test]
    fn stale_relay_watermark_resets_to_zero_when_generation_file_missing() {
        let shared = super::super::make_shared_data_for_tests();
        let channel_id = ChannelId::new(1485506232256168125);
        let relay_coord = shared.tmux_relay_coord(channel_id);
        relay_coord
            .confirmed_end_offset
            .store(1_548_758, Ordering::Release);
        relay_coord.last_relay_ts_ms.store(12345, Ordering::Release);
        // Stored mtime is non-zero, simulating a watermark previously
        // captured against a now-vanished `.generation` file (the cancel→
        // respawn timing window where claude.rs deleted the marker but has
        // not yet written the new one).
        relay_coord
            .confirmed_end_generation_mtime_ns
            .store(123_456_789, Ordering::Release);

        // No `.generation` file on disk for this synthetic session — the
        // helper returns 0 for current_gen_mtime, the stored value differs,
        // so the regression resolver picks the fresh-file branch and the
        // watermark drops to 0 instead of pinning to the observed end.
        // Otherwise the user's response (which lives below the observed end
        // in the new fresh jsonl) would be silently skipped (#1270).
        assert!(reset_stale_relay_watermark_if_output_regressed(
            shared.as_ref(),
            channel_id,
            "AgentDesk-claude-adk-cc-issue-1270-no-genfile",
            438_675,
            "unit-test",
        ));
        assert_eq!(relay_coord.confirmed_end_offset.load(Ordering::Acquire), 0);
        assert_eq!(relay_coord.last_relay_ts_ms.load(Ordering::Acquire), 0);
    }

    #[test]
    fn stale_local_relay_offset_clears_to_none_for_fresh_generation() {
        // #1270: when the wrapper is fresh (mtime changed or file missing),
        // the local last_relayed_offset must be cleared so the next loop
        // tick walks the fresh jsonl from offset 0 and relays the new
        // response. Pinning to the regressed observed_output_end (the old
        // PR #1256 behavior) drops the entire response body that's already
        // landed below the EOF.
        let channel_id = ChannelId::new(1485506232256168127);
        let mut last_relayed_offset = Some(1_548_758);
        let mut last_observed_generation_mtime_ns: Option<i64> = Some(123_456_789);

        assert!(reset_stale_local_relay_offset_if_output_regressed(
            &mut last_relayed_offset,
            &mut last_observed_generation_mtime_ns,
            channel_id,
            "AgentDesk-claude-adk-cc-issue-1270-local-no-genfile",
            438_675,
            "unit-test",
        ));
        assert_eq!(
            last_relayed_offset, None,
            "fresh wrapper must clear local offset so next tick starts at 0"
        );
    }

    // ─── #1275 P2 #1: adoption preserves `.generation` mtime ──────────────
    #[test]
    fn preserve_mtime_after_write_pins_mtime_across_content_rewrite() {
        // The adoption path rewrites the `.generation` payload from the old
        // generation number to the current one. If the rewrite bumps the
        // mtime, a restored watcher with `last_watcher_relayed_generation_mtime_ns`
        // captured before the dcserver restart will mismatch the freshly
        // touched mtime, the regression resolver classifies as fresh wrapper,
        // and a rotated jsonl re-relays surviving content.
        //
        // This test pins the helper's contract: same content rewrite, but
        // mtime stays at the prior value within filesystem resolution.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("adoption.generation");
        std::fs::write(&path, b"42").expect("seed generation");

        // Backdate the file so the post-write set_times target is far
        // enough from "now" that any drift would be detectable.
        let backdated = std::time::SystemTime::now() - std::time::Duration::from_secs(60 * 60 * 24);
        let times = std::fs::FileTimes::new().set_modified(backdated);
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("reopen for backdate");
        f.set_times(times).expect("backdate set_times");
        drop(f);
        let prior_mtime = std::fs::metadata(&path)
            .expect("metadata before")
            .modified()
            .expect("modified before");

        super::preserve_mtime_after_write(
            path.to_str().expect("utf8 path"),
            b"99",
            "unit-test-adoption",
        );

        let after_content = std::fs::read_to_string(&path).expect("read after");
        assert_eq!(after_content, "99", "content must reflect new generation");
        let after_mtime = std::fs::metadata(&path)
            .expect("metadata after")
            .modified()
            .expect("modified after");
        // Tolerate ≤1ms slop: APFS records sub-microsecond mtimes, but some
        // filesystems clamp to microseconds. We backdated by 24h, so any
        // difference within a millisecond means the helper successfully
        // restored the prior mtime instead of letting `std::fs::write` stamp
        // "now".
        let drift = after_mtime
            .duration_since(prior_mtime)
            .unwrap_or_else(|_| prior_mtime.duration_since(after_mtime).unwrap_or_default());
        assert!(
            drift < std::time::Duration::from_millis(1),
            "mtime drift {:?} after preserve_mtime_after_write — adoption \
             would mis-classify the wrapper as fresh-respawn",
            drift
        );
    }

    #[test]
    fn preserve_mtime_after_write_creates_file_when_missing() {
        // If the prior file does not exist, the helper still writes the new
        // content. Without a prior mtime to restore, the post-write mtime is
        // accepted as the new baseline — this is the same behaviour as the
        // pre-#1275 raw `std::fs::write` and is the only safe fallback.
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("missing.generation");
        super::preserve_mtime_after_write(
            path.to_str().expect("utf8 path"),
            b"7",
            "unit-test-missing",
        );
        let content = std::fs::read_to_string(&path).expect("read");
        assert_eq!(content, "7");
    }

    // ─── #1275 P2 #2: resume_offset path snapshots `.generation` mtime ────
    //
    // The race itself is in the watcher loop body, which is hundreds of
    // lines of async I/O behind a `tmux_session_alive` poll — extracting it
    // for a unit test would require a refactor on the scale of this PR
    // itself. We pin the policy by exercising the helpers the watcher loop
    // calls (which is what the bug report describes: a missing
    // `last_observed_generation_mtime_ns` snapshot lets the next regression
    // check take the fresh-wrapper branch on a same-wrapper rotation).
    #[test]
    fn same_wrapper_rotation_does_not_clear_local_offset_when_mtime_baseline_present() {
        // The fix at the resume_offset site stores the current `.generation`
        // mtime in `last_observed_generation_mtime_ns` whenever it preserves
        // `last_relayed_offset` across the resume. Once that baseline is in
        // place, a later jsonl rotation that regresses `observed_output_end`
        // below the stored offset must classify as same-wrapper (mtime
        // unchanged) and pin to the new EOF — NOT clear to None.
        //
        // We model this with a temp `.generation` file whose mtime stays
        // pinned across the regression check, mimicking same-wrapper jsonl
        // rotation right after the resume_offset consumer ran.
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let tmux_name = format!(
            "AgentDesk-claude-adk-issue-1275-resume-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let gen_path = crate::services::tmux_common::session_temp_path(&tmux_name, "generation");
        if let Some(parent) = std::path::Path::new(&gen_path).parent() {
            std::fs::create_dir_all(parent).expect("runtime dir");
        }
        std::fs::write(&gen_path, b"42").expect("seed generation");
        let baseline_mtime = super::read_generation_file_mtime_ns(&tmux_name);
        assert!(
            baseline_mtime > 0,
            "test fixture must produce a non-zero generation mtime"
        );

        // Simulate the post-resume state: offset preserved + mtime baseline
        // captured (the #1275 P2 #2 fix). The same-wrapper jsonl rotation
        // then regresses observed_output_end below the stored offset.
        let channel_id = ChannelId::new(1485506232256168129);
        let mut last_relayed_offset = Some(1_548_758_u64);
        let mut last_observed_generation_mtime_ns: Option<i64> = Some(baseline_mtime);

        let observed_after_rotation = 438_675_u64;
        assert!(reset_stale_local_relay_offset_if_output_regressed(
            &mut last_relayed_offset,
            &mut last_observed_generation_mtime_ns,
            channel_id,
            &tmux_name,
            observed_after_rotation,
            "unit-test-1275-p2-2",
        ));
        assert_eq!(
            last_relayed_offset,
            Some(observed_after_rotation),
            "same-wrapper rotation must pin the local offset to current \
             EOF, NOT clear it (would re-relay surviving content)"
        );

        unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
    }

    #[test]
    fn missing_mtime_baseline_clears_local_offset_on_regression() {
        // Reverse-direction guard: when the resume_offset path forgets to
        // snapshot the mtime baseline (the pre-#1275 bug), a same-wrapper
        // rotation falls through to the fresh-wrapper branch and clears the
        // local offset to None — re-relaying surviving content. This test
        // documents that bad behaviour so flipping the snapshot OFF would
        // immediately fail `same_wrapper_rotation_does_not_clear_local_offset_*`.
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let tmux_name = format!(
            "AgentDesk-claude-adk-issue-1275-no-baseline-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let gen_path = crate::services::tmux_common::session_temp_path(&tmux_name, "generation");
        if let Some(parent) = std::path::Path::new(&gen_path).parent() {
            std::fs::create_dir_all(parent).expect("runtime dir");
        }
        std::fs::write(&gen_path, b"42").expect("seed generation");

        let channel_id = ChannelId::new(1485506232256168130);
        let mut last_relayed_offset = Some(1_548_758_u64);
        // The bug state: no baseline captured at the resume site.
        let mut last_observed_generation_mtime_ns: Option<i64> = None;

        assert!(reset_stale_local_relay_offset_if_output_regressed(
            &mut last_relayed_offset,
            &mut last_observed_generation_mtime_ns,
            channel_id,
            &tmux_name,
            438_675,
            "unit-test-1275-no-baseline",
        ));
        assert_eq!(
            last_relayed_offset, None,
            "without an mtime baseline, the regression check must \
             conservatively classify as fresh-wrapper (this is the bug \
             P2 #2 closes — the baseline is now snapshotted at the \
             resume_offset site)"
        );

        unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
    }

    #[test]
    fn missing_inflight_fallback_warns_and_triggers_reattach_on_db_miss() {
        let plan = missing_inflight_fallback_plan(true, false, true, false, false, true);
        assert!(plan.warn);
        assert!(plan.trigger_reattach);
        assert!(!plan.suppressed_by_recent_stop);

        let resolved = missing_inflight_fallback_plan(true, true, true, false, false, true);
        assert!(resolved.warn);
        assert!(!resolved.trigger_reattach);

        let uncommitted = missing_inflight_fallback_plan(true, false, false, false, false, true);
        assert!(uncommitted.warn);
        assert!(!uncommitted.trigger_reattach);

        let stopped = missing_inflight_fallback_plan(true, false, true, true, false, true);
        assert!(stopped.warn);
        assert!(!stopped.trigger_reattach);
        assert!(stopped.suppressed_by_recent_stop);

        let cleaned = missing_inflight_fallback_plan(true, false, true, false, true, true);
        assert!(cleaned.warn);
        assert!(cleaned.trigger_reattach);
        assert!(
            !cleaned.suppressed_by_recent_stop,
            "terminal placeholder cleanup alone must not suppress live-session reattach"
        );

        let dead_tmux = missing_inflight_fallback_plan(true, false, true, false, false, false);
        assert!(dead_tmux.warn);
        assert!(!dead_tmux.trigger_reattach);
        assert!(!dead_tmux.suppressed_by_recent_stop);
    }

    #[test]
    fn missing_inflight_recent_stop_still_suppresses_placeholder_cleanup_reattach() {
        let stopped_and_cleaned =
            missing_inflight_fallback_plan(true, false, true, true, true, true);

        assert!(stopped_and_cleaned.warn);
        assert!(!stopped_and_cleaned.trigger_reattach);
        assert!(
            stopped_and_cleaned.suppressed_by_recent_stop,
            "recent cancel/stop remains the suppression authority for stale output"
        );
    }

    #[test]
    fn post_terminal_success_continuation_with_text_flushes_without_result_event() {
        assert!(should_flush_post_terminal_success_continuation(
            true,
            false,
            "PR #1333 opened. Routes batch 3 still running."
        ));
        assert!(
            !should_flush_post_terminal_success_continuation(true, true, "already terminal"),
            "a real result event should use the normal terminal relay path"
        );
        assert!(
            !should_flush_post_terminal_success_continuation(true, false, "   "),
            "tool/status-only continuation should not fabricate an empty relay"
        );
        assert!(
            !should_flush_post_terminal_success_continuation(false, false, "pre-terminal text"),
            "pre-terminal output must continue waiting for its normal result"
        );
    }

    #[test]
    fn fallback_send_requires_committed_original_placeholder_cleanup() {
        assert_eq!(
            fallback_placeholder_cleanup_decision(&PlaceholderCleanupOutcome::Succeeded),
            FallbackPlaceholderCleanupDecision::RelayCommitted
        );
        assert_eq!(
            fallback_placeholder_cleanup_decision(&PlaceholderCleanupOutcome::AlreadyGone),
            FallbackPlaceholderCleanupDecision::RelayCommitted
        );
        assert_eq!(
            fallback_placeholder_cleanup_decision(&PlaceholderCleanupOutcome::failed(
                "HTTP 403 Forbidden: Missing Permissions"
            )),
            FallbackPlaceholderCleanupDecision::PreserveInflightForCleanupRetry
        );
    }

    #[tokio::test]
    async fn missing_inflight_dead_tmux_branch_drains_post_result_tail_to_eof()
    -> Result<(), Box<dyn std::error::Error>> {
        let tmp = tempfile::tempdir()?;
        let output_path = tmp.path().join("tmux-output.jsonl");
        let result_chunk = b"{\"type\":\"result\",\"subtype\":\"success\"}\n";
        let post_result_tail = b"{\"type\":\"session_configured\",\"session_id\":\"tail\"}\n";
        let mut output = Vec::new();
        output.extend_from_slice(result_chunk);
        output.extend_from_slice(post_result_tail);
        std::fs::write(&output_path, output)?;

        let current_offset = result_chunk.len() as u64;
        let expected_eof = current_offset + post_result_tail.len() as u64;
        let shared = super::super::make_shared_data_for_tests();
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(987_1171_001);
        let tmux_name = "AgentDesk-codex-test-1171-tail-drain";
        let relay_coord = shared.tmux_relay_coord(channel_id);
        relay_coord
            .confirmed_end_offset
            .store(current_offset, Ordering::Release);

        let drained_offset = super::drain_missing_inflight_dead_tmux_tail_to_eof(
            shared.as_ref(),
            &provider,
            channel_id,
            tmux_name,
            output_path.to_str().expect("utf8 temp path"),
            current_offset,
        )
        .await;

        assert_eq!(drained_offset, expected_eof);
        assert_eq!(
            relay_coord.confirmed_end_offset.load(Ordering::Acquire),
            expected_eof,
            "missing-inflight dead-tmux shutdown must commit the readable post-result tail"
        );

        Ok(())
    }

    #[test]
    fn db_fallback_resolve_failed_counter_increments_when_reattach_fires() {
        // #1136: when the watcher hits the "inflight missing → DB dispatch
        // fallback" path AND the DB-side resolve fails, the runtime must
        // bump the `watcher_db_fallback_resolve_failed` counter and trigger
        // an explicit re-attach (instead of silently dropping the watcher).
        // This test exercises the counter wiring directly so any future
        // refactor that drops the increment fails loudly.
        crate::services::observability::metrics::reset_for_tests();

        let plan = missing_inflight_fallback_plan(true, false, true, false, false, true);
        assert!(
            plan.trigger_reattach,
            "DB fallback resolve failure on a committed terminal output should request reattach"
        );

        let channel_id = ChannelId::new(987_1136_001);
        let provider = ProviderKind::Codex;
        crate::services::observability::metrics::record_watcher_db_fallback_resolve_failed(
            channel_id.get(),
            provider.as_str(),
        );
        crate::services::observability::metrics::record_watcher_db_fallback_resolve_failed(
            channel_id.get(),
            provider.as_str(),
        );

        let snapshot = crate::services::observability::metrics::snapshot();
        let row = snapshot
            .iter()
            .find(|row| row.channel_id == channel_id.get() && row.provider == provider.as_str())
            .expect("counter row should exist after recording");
        assert_eq!(
            row.watcher_db_fallback_resolve_failed, 2,
            "each silent-drop avoidance increments the counter exactly once"
        );

        crate::services::observability::metrics::reset_for_tests();
    }

    #[tokio::test]
    async fn explicit_reattach_returns_session_dead_when_tmux_pane_missing() {
        // #1136: trigger_missing_inflight_reattach must surface its outcome
        // so the caller can log / count success-vs-failure. With no live
        // tmux pane to reattach to, the outcome is `SessionDead` — that is
        // the explicit "재부착 실패 — 추가 진단 필요" branch.
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let shared = super::super::make_shared_data_for_tests();
        let http = Arc::new(poise::serenity_prelude::Http::new("Bot test-token"));
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(987_1136_002);
        // Pick a tmux name that is guaranteed not to exist.
        let tmux_name = format!(
            "AgentDesk-codex-test-1136-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );

        let outcome =
            trigger_missing_inflight_reattach(&http, &shared, &provider, channel, &tmux_name);
        assert_eq!(outcome, MissingInflightReattachOutcome::SessionDead);

        // The synthetic inflight state must NOT have been persisted when the
        // pane is dead — that's the loop-prevention guard at work.
        assert!(
            super::super::inflight::load_inflight_state(&provider, channel.get()).is_none(),
            "no inflight state should leak when reattach is skipped"
        );

        unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
    }

    #[tokio::test]
    async fn missing_inflight_reattach_spawn_increments_reconnect_count() {
        if !crate::services::claude::is_tmux_available() {
            eprintln!("skipping live reattach counter test: tmux is unavailable");
            return;
        }

        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };
        clear_recent_watcher_reattach_offsets_for_tests();

        let tmux_name = format!(
            "AgentDesk-codex-test-964-count-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let tmux_created = std::process::Command::new("tmux")
            .args(["new-session", "-d", "-s", &tmux_name, "sleep 600"])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !tmux_created {
            unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
            panic!("failed to create tmux session for reconnect counter test");
        }

        let shared = super::super::make_shared_data_for_tests();
        let http = Arc::new(poise::serenity_prelude::Http::new("Bot test-token"));
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(987_0964_001);
        let (output_path, _) = super::super::turn_bridge::tmux_runtime_paths(&tmux_name);
        if let Some(parent) = std::path::Path::new(&output_path).parent() {
            std::fs::create_dir_all(parent).expect("runtime dir");
        }
        std::fs::write(&output_path, b"reattach bytes").expect("seed output file");

        let outcome =
            trigger_missing_inflight_reattach(&http, &shared, &provider, channel, &tmux_name);

        assert_eq!(
            outcome,
            MissingInflightReattachOutcome::Spawned {
                replaced_existing: false
            }
        );
        assert_eq!(
            shared
                .tmux_relay_coord(channel)
                .reconnect_count
                .load(Ordering::Acquire),
            1,
            "fresh missing-inflight reattach spawn must increment reconnect_count"
        );

        if let Some(watcher) = shared.tmux_watchers.get(&channel) {
            watcher.cancel.store(true, Ordering::Relaxed);
        }
        let _ = std::process::Command::new("tmux")
            .args(["kill-session", "-t", &tmux_name])
            .status();
        unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
    }

    #[tokio::test]
    async fn missing_inflight_reattach_reuses_live_self_watcher() {
        if !crate::services::claude::is_tmux_available() {
            eprintln!("skipping live reattach regression: tmux is unavailable");
            return;
        }

        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };
        clear_recent_watcher_reattach_offsets_for_tests();

        let tmux_name = format!(
            "AgentDesk-codex-test-1135-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let tmux_created = std::process::Command::new("tmux")
            .args(["new-session", "-d", "-s", &tmux_name, "sleep 30"])
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if !tmux_created {
            unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
            panic!("failed to create tmux session for live reattach regression");
        }

        let (output_path, _) = super::super::turn_bridge::tmux_runtime_paths(&tmux_name);
        if let Some(parent) = std::path::Path::new(&output_path).parent() {
            std::fs::create_dir_all(parent).expect("runtime dir");
        }
        std::fs::write(&output_path, b"already relayed bytes").expect("seed output file");
        let expected_offset = std::fs::metadata(&output_path).expect("metadata").len();

        let shared = super::super::make_shared_data_for_tests();
        let http = Arc::new(poise::serenity_prelude::Http::new("Bot test-token"));
        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(987_1135_002);
        let initial = test_watcher_handle(&tmux_name);
        let initial_cancel = initial.cancel.clone();
        assert!(super::try_claim_watcher(
            &shared.tmux_watchers,
            channel,
            initial
        ));

        let outcome =
            trigger_missing_inflight_reattach(&http, &shared, &provider, channel, &tmux_name);
        assert_eq!(
            outcome,
            MissingInflightReattachOutcome::ReusedExisting {
                owner_channel_id: channel
            }
        );
        assert!(
            !initial_cancel.load(Ordering::Relaxed),
            "reattach metadata repair must preserve the already-running self watcher"
        );
        assert!(
            matching_recent_watcher_reattach_offset(channel, &tmux_name, expected_offset).is_none(),
            "no fresh watcher generation should be recorded when the live watcher is reused"
        );

        let state = super::super::inflight::load_inflight_state(&provider, channel.get())
            .expect("synthetic reattach inflight state");
        assert!(state.rebind_origin);
        assert_eq!(state.last_offset, expected_offset);
        assert_eq!(state.tmux_session_name.as_deref(), Some(tmux_name.as_str()));

        if let Some(watcher) = shared.tmux_watchers.get(&channel) {
            watcher.cancel.store(true, Ordering::Relaxed);
        }
        let _ = std::process::Command::new("tmux")
            .args(["kill-session", "-t", &tmux_name])
            .status();
        unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
    }

    #[test]
    fn recent_turn_stop_suppresses_missing_inflight_reattach() {
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        clear_recent_turn_stops_for_tests();
        let channel = ChannelId::new(987_1044_002);
        let tmux_name = "AgentDesk-codex-recent-stop-suppress";

        assert!(recent_turn_stop_for_channel(channel).is_none());
        record_recent_turn_stop_with_offset_for_tests(channel, tmux_name, 128, "unit-test stop");

        let recent_stop = recent_turn_stop_for_channel(channel)
            .expect("recent turn stop tombstone should be visible");
        assert_eq!(recent_stop.reason, "unit-test stop");
        assert!(
            recent_turn_stop_for_watcher_range(channel, tmux_name, 127).is_some(),
            "cancelled turn range should match the stop tombstone"
        );

        let plan = missing_inflight_fallback_plan(true, false, true, true, false, true);
        assert!(!plan.trigger_reattach);
        assert!(plan.suppressed_by_recent_stop);

        clear_recent_turn_stops_for_tests();
    }

    #[test]
    fn recent_turn_stop_does_not_suppress_later_unrelated_turn_range() {
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        clear_recent_turn_stops_for_tests();
        let channel = ChannelId::new(987_1044_003);
        let tmux_name = "AgentDesk-codex-recent-stop-later-turn";

        record_recent_turn_stop_with_offset_for_tests(channel, tmux_name, 128, "unit-test stop");

        assert!(
            recent_turn_stop_for_watcher_range(channel, tmux_name, 127).is_some(),
            "late output that started before the stopped turn boundary should be suppressed"
        );
        assert!(
            recent_turn_stop_for_watcher_range(channel, tmux_name, 128).is_none(),
            "a clean later turn starting exactly at cancel EOF must not be suppressed"
        );
        assert!(
            recent_turn_stop_for_watcher_range(channel, tmux_name, 129).is_none(),
            "a later turn in the same channel/tmux must not be suppressed by the old TTL"
        );

        let stopped = missing_inflight_fallback_plan(
            true,
            false,
            true,
            recent_turn_stop_for_watcher_range(channel, tmux_name, 127).is_some(),
            false,
            true,
        );
        assert!(!stopped.trigger_reattach);
        assert!(stopped.suppressed_by_recent_stop);

        let later = missing_inflight_fallback_plan(
            true,
            false,
            true,
            recent_turn_stop_for_watcher_range(channel, tmux_name, 129).is_some(),
            false,
            true,
        );
        assert!(later.trigger_reattach);
        assert!(!later.suppressed_by_recent_stop);

        clear_recent_turn_stops_for_tests();
    }

    #[test]
    fn recent_turn_stop_equal_boundary_does_not_suppress_later_turn() {
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        clear_recent_turn_stops_for_tests();
        let channel = ChannelId::new(987_1044_014);
        let tmux_name = "AgentDesk-codex-recent-stop-equality";

        record_recent_turn_stop_with_offset_for_tests(channel, tmux_name, 2048, "unit-test stop");

        assert!(
            recent_turn_stop_for_watcher_range(channel, tmux_name, 2047).is_some(),
            "ranges that began before cancel EOF belong to the stopped turn"
        );
        assert!(
            recent_turn_stop_for_watcher_range(channel, tmux_name, 2048).is_none(),
            "ranges starting exactly at cancel EOF belong to the next turn"
        );
        let later = missing_inflight_fallback_plan(
            true,
            false,
            true,
            recent_turn_stop_for_watcher_range(channel, tmux_name, 2048).is_some(),
            false,
            true,
        );
        assert!(later.trigger_reattach);
        assert!(!later.suppressed_by_recent_stop);

        clear_recent_turn_stops_for_tests();
    }

    #[test]
    fn cancel_induced_watcher_death_matches_session_after_recent_stop() {
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        clear_recent_turn_stops_for_tests();
        let channel = ChannelId::new(987_1271_001);
        let tmux_name = "AgentDesk-claude-cancel-induced";

        assert!(
            !cancel_induced_watcher_death(channel, tmux_name, None),
            "no tombstone yet → not cancel-induced"
        );

        record_recent_turn_stop_with_offset_for_tests(
            channel,
            tmux_name,
            128,
            "mailbox_cancel_active_turn",
        );

        assert!(
            !cancel_induced_watcher_death(channel, "AgentDesk-claude-other-session", None),
            "tombstone bound to a different tmux name must NOT suppress unrelated session deaths"
        );
        assert!(
            !cancel_induced_watcher_death(ChannelId::new(987_1271_002), tmux_name, None),
            "tombstone for a different channel must NOT suppress this channel's death"
        );
        assert!(
            cancel_induced_watcher_death(channel, tmux_name, None),
            "watcher death right after a cancel for the same session must be classified as cancel-induced"
        );

        clear_recent_turn_stops_for_tests();
    }

    #[test]
    fn cancel_induced_watcher_death_consumes_tombstone_so_later_failures_surface() {
        // Codex P1 on PR #1277: without consumption a follow-up turn that
        // reuses the same channel/tmux name would inherit the cancel
        // tombstone for the full 60s TTL, silently swallowing an unrelated
        // crash. After a true return the tombstone must be gone.
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        clear_recent_turn_stops_for_tests();
        let channel = ChannelId::new(987_1276_001);
        let tmux_name = "AgentDesk-claude-cancel-consume";

        record_recent_turn_stop_with_offset_for_tests(channel, tmux_name, 256, "user-cancel");

        assert!(
            cancel_induced_watcher_death(channel, tmux_name, None),
            "first post-cancel watcher death is the legitimate consumer"
        );
        assert!(
            !cancel_induced_watcher_death(channel, tmux_name, None),
            "tombstone must be consumed so a later real failure on the reused session is NOT suppressed"
        );

        clear_recent_turn_stops_for_tests();
    }

    #[test]
    fn cancel_induced_watcher_death_drains_duplicate_tombstones_per_cancel() {
        // Codex P2 on PR #1277: a single cancel commonly records two
        // tombstones — `mailbox_cancel_active_turn` writes one, and
        // `turn_lifecycle::stop_provider_turn_with_outcome` writes another
        // via `record_turn_stop_tombstone`. If we removed only one entry,
        // the duplicate would remain and silently swallow a follow-up
        // turn's real failure on the reused (channel, tmux) pair.
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        clear_recent_turn_stops_for_tests();
        let channel = ChannelId::new(987_1276_020);
        let tmux_name = "AgentDesk-claude-duplicate-cancel-tombstones";

        record_recent_turn_stop_with_offset_for_tests(
            channel,
            tmux_name,
            128,
            "mailbox_cancel_active_turn",
        );
        record_recent_turn_stop_with_offset_for_tests(
            channel,
            tmux_name,
            128,
            "turn_lifecycle::stop",
        );

        assert!(
            cancel_induced_watcher_death(channel, tmux_name, None),
            "first cancel-induced death must consume both duplicate tombstones"
        );
        assert!(
            !cancel_induced_watcher_death(channel, tmux_name, None),
            "no in-window tombstone must remain after the first consumer drains the duplicates"
        );

        clear_recent_turn_stops_for_tests();
    }

    #[test]
    fn cancel_induced_watcher_death_does_not_suppress_real_failure_past_cancel_eof() {
        // Codex P2 round 3 on PR #1277: for preserve-session stops the same
        // tmux session is reused, the wrapper writes follow-up turn output
        // past the cancel boundary, then crashes. With only the
        // channel/session/time match the death would inherit the cancel
        // tombstone and silently swallow the lifecycle notification +
        // restart handoff. The current_output_offset boundary check must
        // surface the real failure.
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        clear_recent_turn_stops_for_tests();
        let channel = ChannelId::new(987_1276_030);
        let tmux_name = "AgentDesk-claude-cancel-eof-boundary";
        let stop_offset: u64 = 1024;

        record_recent_turn_stop_with_offset_for_tests(
            channel,
            tmux_name,
            stop_offset,
            "user-cancel",
        );

        // 1) Watcher death observed at the cancel boundary (no follow-up
        //    writes) must classify as cancel-induced.
        assert!(
            cancel_induced_watcher_death(channel, tmux_name, Some(stop_offset)),
            "death at the cancel boundary is the legitimate cleanup"
        );

        // 2) Re-record so the second case has a fresh tombstone, then a
        //    crash that observed bytes well past the cancel boundary +
        //    teardown grace must NOT be suppressed.
        record_recent_turn_stop_with_offset_for_tests(
            channel,
            tmux_name,
            stop_offset,
            "user-cancel",
        );
        let post_followup_eof = stop_offset + CANCEL_TEARDOWN_GRACE_BYTES + 4096;
        assert!(
            !cancel_induced_watcher_death(channel, tmux_name, Some(post_followup_eof)),
            "death observed past cancel EOF + teardown grace must surface its own signal"
        );

        // 3) Within the teardown grace window, still treat as cancel-induced
        //    (wrapper's normal post-cancel flush bytes).
        let teardown_eof = stop_offset + (CANCEL_TEARDOWN_GRACE_BYTES / 2);
        assert!(
            cancel_induced_watcher_death(channel, tmux_name, Some(teardown_eof)),
            "death within teardown grace window stays cancel-induced"
        );

        clear_recent_turn_stops_for_tests();
    }

    #[test]
    fn cancel_induced_watcher_death_consumes_only_matching_tombstone() {
        // When two channels independently cancel turns, consuming one
        // channel's tombstone must NOT remove the other channel's.
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        clear_recent_turn_stops_for_tests();
        let channel_a = ChannelId::new(987_1276_010);
        let channel_b = ChannelId::new(987_1276_011);
        let tmux_a = "AgentDesk-claude-channel-a";
        let tmux_b = "AgentDesk-claude-channel-b";

        record_recent_turn_stop_with_offset_for_tests(channel_a, tmux_a, 100, "user-cancel-a");
        record_recent_turn_stop_with_offset_for_tests(channel_b, tmux_b, 100, "user-cancel-b");

        assert!(cancel_induced_watcher_death(channel_a, tmux_a, None));
        assert!(
            cancel_induced_watcher_death(channel_b, tmux_b, None),
            "channel-b tombstone must remain intact after channel-a's was consumed"
        );

        clear_recent_turn_stops_for_tests();
    }

    #[test]
    fn cancel_induced_watcher_death_allows_session_unscoped_tombstone() {
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        clear_recent_turn_stops_for_tests();
        let channel = ChannelId::new(987_1271_003);
        let tmux_name = "AgentDesk-claude-no-session-tombstone";

        record_recent_turn_stop_for_tests(
            channel,
            None,
            None,
            "session-less cancel",
            std::time::Instant::now(),
        );

        assert!(
            cancel_induced_watcher_death(channel, tmux_name, None),
            "tombstones recorded without a tmux session name still cover any death on the same channel"
        );

        clear_recent_turn_stops_for_tests();
    }

    #[test]
    fn recent_turn_stop_metadata_unavailable_fallback_is_bounded() {
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        clear_recent_turn_stops_for_tests();
        let channel = ChannelId::new(987_1044_015);
        let tmux_name = "AgentDesk-codex-recent-stop-no-metadata";

        record_recent_turn_stop_for_tests(
            channel,
            None,
            None,
            "unit-test stop missing metadata",
            std::time::Instant::now(),
        );
        assert!(
            recent_turn_stop_for_watcher_range(channel, tmux_name, 4096).is_some(),
            "fresh same-channel tombstones without metadata should still suppress the stop race"
        );
        let fallback = missing_inflight_fallback_plan(
            true,
            false,
            true,
            recent_turn_stop_for_watcher_range(channel, tmux_name, 4096).is_some(),
            false,
            true,
        );
        assert!(!fallback.trigger_reattach);
        assert!(fallback.suppressed_by_recent_stop);

        clear_recent_turn_stops_for_tests();
        record_recent_turn_stop_for_tests(
            channel,
            None,
            None,
            "unit-test old missing metadata",
            std::time::Instant::now()
                - super::RECENT_TURN_STOP_METADATA_FALLBACK_TTL
                - std::time::Duration::from_secs(1),
        );
        assert!(
            recent_turn_stop_for_watcher_range(channel, tmux_name, 4097).is_none(),
            "metadata-free fallback must expire before it can suppress unrelated later turns"
        );
        let later = missing_inflight_fallback_plan(
            true,
            false,
            true,
            recent_turn_stop_for_watcher_range(channel, tmux_name, 4097).is_some(),
            false,
            true,
        );
        assert!(later.trigger_reattach);
        assert!(!later.suppressed_by_recent_stop);

        clear_recent_turn_stops_for_tests();
    }

    #[test]
    fn recent_turn_stop_suppresses_streaming_placeholder_before_send() {
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        clear_recent_turn_stops_for_tests();
        let channel = ChannelId::new(987_1044_016);
        let tmux_name = "AgentDesk-codex-recent-stop-streaming";

        record_recent_turn_stop_with_offset_for_tests(channel, tmux_name, 512, "unit-test stop");

        let stopped_range = recent_turn_stop_for_watcher_range(channel, tmux_name, 511).is_some();
        assert!(should_suppress_streaming_placeholder_after_recent_stop(
            true,
            true,
            stopped_range
        ));
        assert!(
            !should_suppress_streaming_placeholder_after_recent_stop(true, false, stopped_range),
            "active inflight streaming should keep normal placeholder behavior"
        );
        assert!(
            !should_suppress_streaming_placeholder_after_recent_stop(
                true,
                true,
                recent_turn_stop_for_watcher_range(channel, tmux_name, 512).is_some()
            ),
            "streaming at the exact cancel EOF boundary belongs to a later turn"
        );

        clear_recent_turn_stops_for_tests();
    }

    #[test]
    fn recent_turn_stop_suppresses_late_terminal_output_after_inflight_deletion() {
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        clear_recent_turn_stops_for_tests();
        let channel = ChannelId::new(987_1044_004);
        let tmux_name = "AgentDesk-codex-recent-stop-output";

        record_recent_turn_stop_with_offset_for_tests(channel, tmux_name, 512, "unit-test stop");

        let stopped_range = recent_turn_stop_for_watcher_range(channel, tmux_name, 511).is_some();
        assert!(should_suppress_terminal_output_after_recent_stop(
            true,
            true,
            stopped_range
        ));

        let later_range = recent_turn_stop_for_watcher_range(channel, tmux_name, 512).is_some();
        assert!(!should_suppress_terminal_output_after_recent_stop(
            true,
            true,
            later_range
        ));
        assert!(!should_suppress_terminal_output_after_recent_stop(
            true,
            false,
            stopped_range
        ));
        assert!(!should_suppress_terminal_output_after_recent_stop(
            false,
            true,
            stopped_range
        ));

        clear_recent_turn_stops_for_tests();
    }

    #[tokio::test]
    async fn missing_inflight_reattach_grace_preserves_same_offset_bridge_placeholder()
    -> Result<(), Box<dyn std::error::Error>> {
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let tmp = tempfile::tempdir()?;
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(987_1044_001);
        let channel_name = "adk-cdx-issue-1044";
        let tmux_name = provider.build_tmux_session_name(channel_name);
        let turn_offset = 44_096_u64;

        let terminal_success_plan =
            missing_inflight_fallback_plan(true, false, true, false, false, true);
        assert!(terminal_success_plan.trigger_reattach);
        assert!(super::super::inflight::load_inflight_state(&provider, channel.get()).is_none());

        let writer_provider = provider.clone();
        let writer_tmux_name = tmux_name.clone();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            let mut state = InflightTurnState::new(
                writer_provider,
                channel.get(),
                Some(channel_name.to_string()),
                7,
                9,
                11,
                "next turn at same offset".to_string(),
                Some("session-1044".to_string()),
                Some(writer_tmux_name),
                Some("/tmp/issue-1044.jsonl".to_string()),
                Some("/tmp/issue-1044.fifo".to_string()),
                turn_offset,
            );
            state.turn_start_offset = Some(turn_offset);
            state.full_response = "already visible bridge placeholder body".to_string();
            state.response_sent_offset = state.full_response.len();
            let _ = super::super::inflight::save_inflight_state(&state);
        });

        let bridge_reacquired = wait_for_reacquired_turn_bridge_inflight_state(
            &provider,
            channel,
            &tmux_name,
            3,
            Duration::from_millis(50),
        )
        .await;
        let _ = writer.await;

        assert!(
            bridge_reacquired,
            "next turn should reacquire inflight during the missing-inflight reattach grace window"
        );
        let next_turn_state = super::super::inflight::load_inflight_state(&provider, channel.get())
            .ok_or_else(|| std::io::Error::other("expected next turn inflight state"))?;
        assert!(watcher_should_yield_to_inflight_state(
            Some(&next_turn_state),
            &tmux_name,
            turn_offset,
            turn_offset + 128,
        ));

        let placeholder_body = "already visible bridge placeholder body";
        let final_placeholder_body = if bridge_reacquired {
            placeholder_body.to_string()
        } else {
            match suppressed_placeholder_action(true, placeholder_body.len(), placeholder_body) {
                SuppressedPlaceholderAction::Edit(content) => content,
                _ => String::new(),
            }
        };

        assert_eq!(final_placeholder_body, placeholder_body);
        assert!(!final_placeholder_body.contains(SUPPRESSED_INTERNAL_LABEL));
        assert!(!final_placeholder_body.contains(SUPPRESSED_RESTART_LABEL));

        unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
        Ok(())
    }

    #[tokio::test]
    async fn bridge_guard_preserves_placeholder_when_range_matches_recent_reattach()
    -> Result<(), Box<dyn std::error::Error>> {
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        clear_recent_watcher_reattach_offsets_for_tests();
        let tmp = tempfile::tempdir()?;
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let provider = ProviderKind::Codex;
        let channel = ChannelId::new(987_1044_002);
        let channel_name = "adk-cdx-issue-1044b";
        let tmux_name = provider.build_tmux_session_name(channel_name);
        let reattach_offset = 7_628_900_u64;
        let suppressed_end_offset = 7_636_322_u64;
        let placeholder_body = "real response body already delivered by watcher reattach";

        let test_result = async {
            super::super::inflight::clear_inflight_state(&provider, channel.get());
            let terminal_success_plan =
                missing_inflight_fallback_plan(true, false, true, false, false, true);
            assert!(terminal_success_plan.trigger_reattach);
            assert!(
                super::super::inflight::load_inflight_state(&provider, channel.get()).is_none()
            );

            let bridge_reacquired = wait_for_reacquired_turn_bridge_inflight_state(
                &provider,
                channel,
                &tmux_name,
                1,
                Duration::from_millis(1),
            )
            .await;
            assert!(
                !bridge_reacquired,
                "grace window should still see no bridge-owned inflight state"
            );

            record_recent_watcher_reattach_offset(channel, &tmux_name, reattach_offset);

            let mut state = InflightTurnState::new(
                provider.clone(),
                channel.get(),
                Some(channel_name.to_string()),
                0,
                0,
                44,
                "watcher missing-inflight reattach".to_string(),
                None,
                Some(tmux_name.clone()),
                Some("/tmp/issue-1044b.jsonl".to_string()),
                Some("/tmp/issue-1044b.fifo".to_string()),
                reattach_offset,
            );
            state.rebind_origin = true;
            state.full_response = placeholder_body.to_string();
            state.response_sent_offset = placeholder_body.len();
            super::super::inflight::save_inflight_state_create_new(&state).map_err(|error| {
                std::io::Error::other(format!("failed to save reattach inflight state: {error}"))
            })?;

            assert!(watcher_should_yield_to_inflight_state(
                Some(&state),
                &tmux_name,
                reattach_offset,
                suppressed_end_offset,
            ));
            let matched_reattach =
                matching_recent_watcher_reattach_offset(channel, &tmux_name, reattach_offset);
            assert!(
                matched_reattach.is_some(),
                "suppressed range start should match the recent watcher reattach offset"
            );

            let final_placeholder_body = if matched_reattach.is_some() {
                placeholder_body.to_string()
            } else {
                match suppressed_placeholder_action(true, placeholder_body.len(), placeholder_body)
                {
                    SuppressedPlaceholderAction::Edit(content) => content,
                    SuppressedPlaceholderAction::Delete | SuppressedPlaceholderAction::None => {
                        String::new()
                    }
                }
            };

            assert_eq!(final_placeholder_body, placeholder_body);
            assert!(!final_placeholder_body.contains(SUPPRESSED_INTERNAL_LABEL));

            let non_reattach_suppress =
                suppressed_placeholder_action(true, placeholder_body.len(), placeholder_body);
            assert!(matches!(
                non_reattach_suppress,
                SuppressedPlaceholderAction::Edit(ref content)
                    if content.contains(SUPPRESSED_INTERNAL_LABEL)
            ));

            Ok::<(), Box<dyn std::error::Error>>(())
        }
        .await;

        super::super::inflight::clear_inflight_state(&provider, channel.get());
        clear_recent_watcher_reattach_offsets_for_tests();
        unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
        test_result
    }

    #[test]
    fn watcher_yields_to_active_bridge_turn_when_batch_overlaps_turn_start() {
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("deadlock-manager".to_string()),
            7,
            9,
            11,
            "ping".to_string(),
            Some("session-1".to_string()),
            Some("#AgentDesk-codex-deadlock-manager".to_string()),
            Some("/tmp/output.jsonl".to_string()),
            Some("/tmp/input.fifo".to_string()),
            0,
        );
        state.turn_start_offset = Some(120);
        state.last_offset = 180;
        let should_yield = watcher_should_yield_to_inflight_state(
            Some(&state),
            "#AgentDesk-codex-deadlock-manager",
            100,
            180,
        );

        assert!(should_yield);
    }

    #[test]
    fn watcher_owned_live_relay_does_not_yield_to_active_bridge_guard() {
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("deadlock-manager".to_string()),
            7,
            9,
            11,
            "ping".to_string(),
            Some("session-1".to_string()),
            Some("#AgentDesk-codex-deadlock-manager".to_string()),
            Some("/tmp/output.jsonl".to_string()),
            Some("/tmp/input.fifo".to_string()),
            0,
        );
        state.turn_start_offset = Some(120);
        state.last_offset = 180;
        state.watcher_owns_live_relay = true;

        assert!(!watcher_should_yield_to_inflight_state(
            Some(&state),
            "#AgentDesk-codex-deadlock-manager",
            100,
            180,
        ));
    }

    #[tokio::test]
    async fn restored_watcher_finish_does_not_underflow_global_active() {
        let shared = super::super::make_shared_data_for_tests();
        let provider = ProviderKind::Codex;
        let channel_id = ChannelId::new(1485506232256981);
        let token = Arc::new(CancelToken::new());
        assert!(
            super::super::mailbox_try_start_turn(
                &shared,
                channel_id,
                token,
                UserId::new(343742347365974026),
                MessageId::new(1487795113240559701),
            )
            .await
        );
        assert_eq!(shared.global_active.load(Ordering::Relaxed), 0);

        finish_restored_watcher_active_turn(
            &shared,
            &provider,
            channel_id,
            true,
            "restored_watcher_finish_does_not_underflow_global_active",
        )
        .await;

        assert_eq!(shared.global_active.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn watcher_does_not_yield_for_non_overlapping_or_other_session_turns() {
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("deadlock-manager".to_string()),
            7,
            9,
            11,
            "ping".to_string(),
            Some("session-1".to_string()),
            Some("#AgentDesk-codex-deadlock-manager".to_string()),
            Some("/tmp/output.jsonl".to_string()),
            Some("/tmp/input.fifo".to_string()),
            0,
        );
        state.turn_start_offset = Some(220);
        state.last_offset = 260;
        let different_range = watcher_should_yield_to_inflight_state(
            Some(&state),
            "#AgentDesk-codex-deadlock-manager",
            100,
            180,
        );
        let different_session = watcher_should_yield_to_inflight_state(
            Some(&state),
            "#AgentDesk-codex-somewhere-else",
            200,
            280,
        );

        assert!(!different_range);
        assert!(!different_session);
    }

    #[test]
    fn watcher_output_activity_refreshes_legacy_session_heartbeat() {
        let db = crate::db::test_db();
        let provider = ProviderKind::Codex;
        let channel_name = "adk-cdx-t1485506232256168011";
        let tmux_name = provider.build_tmux_session_name(channel_name);
        let session_key =
            crate::services::discord::adk_session::build_legacy_session_key(&tmux_name);
        db.lock()
            .unwrap()
            .execute(
                "INSERT INTO sessions
                 (session_key, provider, status, thread_channel_id, last_heartbeat, created_at)
                 VALUES (?1, ?2, 'idle', '1485506232256168011', '2026-04-09 01:02:03', '2026-04-09 01:02:03')",
                [session_key.as_str(), provider.as_str()],
            )
            .unwrap();

        assert!(refresh_session_heartbeat_from_tmux_output(
            Some(&db),
            None,
            "tokenxyz",
            &provider,
            &tmux_name,
            Some(1485506232256168011),
        ));

        let last_heartbeat: String = db
            .lock()
            .unwrap()
            .query_row(
                "SELECT last_heartbeat FROM sessions WHERE session_key = ?1",
                [session_key.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(last_heartbeat, "2026-04-09 01:02:03");
    }

    #[test]
    fn watcher_ignores_assistant_text_that_mentions_stale_resume_phrase() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"The log contained No conversation found while I was debugging.\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}\n"
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(!outcome.stale_resume_detected);
        assert_eq!(
            full_response,
            "The log contained No conversation found while I was debugging."
        );
    }

    #[test]
    fn watcher_detects_structured_stale_resume_error_result() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"partial\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"errors\":[\"No conversation found\"]}\n"
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(outcome.stale_resume_detected);
        assert_eq!(full_response, "partial");
    }

    // ── #378 E2E: normal success result is not flagged ──

    #[test]
    fn normal_success_result_has_no_error_flags() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Here is the answer.\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}\n"
        ).to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(!outcome.is_prompt_too_long);
        assert!(!outcome.is_auth_error);
        assert!(!outcome.is_provider_overloaded);
        assert!(!outcome.stale_resume_detected);
        assert_eq!(full_response, "Here is the answer.");
    }

    /// #1216: when the buffer holds two completed turns back-to-back (typical
    /// post-deploy/restart drain), `process_watcher_lines` must stop after the
    /// first `result` and leave the second turn untouched in the buffer for
    /// the next call. Otherwise both turns' `assistant` text gets concatenated
    /// into one `full_response`, then sliced into multi-message Discord bursts
    /// at the 2000-char cap.
    #[test]
    fn process_watcher_lines_stops_at_first_result_in_multi_turn_buffer() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"first turn body\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done-1\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"second turn body\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done-2\"}\n",
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result, "first result must be recognised");
        assert_eq!(
            full_response, "first turn body",
            "second turn must not bleed into the first turn's body"
        );
        assert!(
            buffer.contains("second turn body"),
            "second turn must remain in the buffer for the next call"
        );
        assert!(
            buffer.contains("done-2"),
            "second turn's result line must remain in the buffer"
        );

        let mut full_response2 = String::new();
        let outcome2 = process_watcher_lines(
            &mut buffer,
            &mut state,
            &mut full_response2,
            &mut tool_state,
        );
        assert!(outcome2.found_result, "second result is consumed next");
        assert_eq!(full_response2, "second turn body");
        assert!(
            buffer.trim().is_empty(),
            "buffer should be drained after both turns are consumed"
        );
    }

    #[test]
    fn watcher_tracks_previous_tool_status_for_two_line_trail() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Read\",\"input\":{\"file_path\":\"src/config.rs\"}}]}}\n",
            "{\"type\":\"content_block_stop\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Bash\",\"input\":{\"command\":\"cargo build\"}}]}}\n"
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(!outcome.found_result);
        assert_eq!(
            tool_state.prev_tool_status.as_deref(),
            Some("✓ Read: src/config.rs")
        );
        assert_eq!(
            tool_state.current_tool_line.as_deref(),
            Some("⚙ Bash: `cargo build`")
        );
    }

    #[test]
    fn watcher_redacts_assistant_thinking_block() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"thinking\",\"thinking\":\"internal reasoning\"},{\"type\":\"text\",\"text\":\"final answer\"}]}}\n",
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(!outcome.found_result);
        assert_eq!(full_response, "final answer");
        assert!(!full_response.contains("internal reasoning"));
        assert!(!full_response.contains("Reasoning"));
        assert!(tool_state.transcript_events.iter().any(|event| matches!(
            event.kind,
            SessionTranscriptEventKind::Thinking
        ) && event.summary.is_none()
            && event.content.is_empty()));
        assert!(tool_state.transcript_events.iter().all(|event| {
            event.summary.as_deref() != Some("internal reasoning")
                && !event.content.contains("internal reasoning")
        }));
    }

    #[test]
    fn watcher_records_redacted_assistant_thinking_block_without_plaintext() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"thinking\"},{\"type\":\"text\",\"text\":\"final answer\"}]}}\n",
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(!outcome.found_result);
        assert_eq!(full_response, "final answer");
        assert_eq!(
            tool_state.prev_tool_status.as_deref(),
            Some(super::REDACTED_THINKING_STATUS_LINE)
        );
        assert!(tool_state.transcript_events.iter().any(|event| matches!(
            event.kind,
            SessionTranscriptEventKind::Thinking
        ) && event.summary.is_none()
            && event.content.is_empty()));
    }

    #[test]
    fn watcher_redacts_streaming_thinking_delta() {
        let mut buffer = concat!(
            "{\"type\":\"content_block_start\",\"content_block\":{\"type\":\"thinking\"}}\n",
            "{\"type\":\"content_block_delta\",\"delta\":{\"thinking\":\"internal reasoning\"}}\n",
            "{\"type\":\"content_block_stop\"}\n",
            "{\"type\":\"content_block_delta\",\"delta\":{\"text\":\"final answer\"}}\n",
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(!outcome.found_result);
        assert_eq!(full_response, "final answer");
        assert!(!full_response.contains("internal reasoning"));
        assert!(!full_response.contains("Reasoning"));
        assert!(tool_state.transcript_events.iter().any(|event| matches!(
            event.kind,
            SessionTranscriptEventKind::Thinking
        ) && event.summary.is_none()
            && event.content.is_empty()));
        assert!(tool_state.transcript_events.iter().all(|event| {
            event.summary.as_deref() != Some("internal reasoning")
                && !event.content.contains("internal reasoning")
        }));
    }

    #[test]
    fn dead_session_cleanup_plan_preserves_tmux_but_still_reports_idle() {
        let plan = dead_session_cleanup_plan(true);

        assert_eq!(
            plan,
            DeadSessionCleanupPlan {
                preserve_tmux_session: true,
                report_idle_status: true,
            }
        );
    }

    #[test]
    fn dead_session_cleanup_plan_kills_unprotected_sessions_and_reports_idle() {
        let plan = dead_session_cleanup_plan(false);

        assert_eq!(
            plan,
            DeadSessionCleanupPlan {
                preserve_tmux_session: false,
                report_idle_status: true,
            }
        );
    }

    // -----------------------------------------------------------------
    // #1137 watcher-stop strictness — 4 combinations of
    // (terminal_success_seen yes/no) x (tmux_alive yes/no), plus the
    // post-terminal-success continuation path that motivated the issue.
    // -----------------------------------------------------------------

    fn watcher_stop_input_default() -> super::WatcherStopInput {
        super::WatcherStopInput {
            terminal_success_seen: false,
            tmux_alive: true,
            confirmed_end: 0,
            tmux_tail_offset: 0,
            idle_duration: None,
            idle_threshold: super::WATCHER_POST_TERMINAL_IDLE_WINDOW,
        }
    }

    #[test]
    fn watcher_stop_no_terminal_success_alive_keeps_watching() {
        // (terminal=N, tmux_alive=Y) — pre-terminal-success traffic must
        // never trigger the strict-stop path; the watcher just keeps reading.
        let input = super::WatcherStopInput {
            terminal_success_seen: false,
            tmux_alive: true,
            confirmed_end: 1024,
            tmux_tail_offset: 1024,
            idle_duration: Some(std::time::Duration::from_secs(60)),
            ..watcher_stop_input_default()
        };
        assert_eq!(
            super::watcher_stop_decision_after_terminal_success(input),
            super::WatcherStopDecision::Continue
        );
    }

    #[test]
    fn watcher_stop_no_terminal_success_dead_tmux_stops() {
        // (terminal=N, tmux_alive=N) — even without a result event, a dead
        // tmux pane means the watcher has nothing left to read; stop quietly.
        let input = super::WatcherStopInput {
            terminal_success_seen: false,
            tmux_alive: false,
            confirmed_end: 0,
            tmux_tail_offset: 4096,
            idle_duration: Some(std::time::Duration::from_millis(10)),
            ..watcher_stop_input_default()
        };
        assert_eq!(
            super::watcher_stop_decision_after_terminal_success(input),
            super::WatcherStopDecision::Stop
        );
    }

    #[test]
    fn watcher_stop_terminal_success_alive_with_new_output_continues() {
        // (terminal=Y, tmux_alive=Y, confirmed_end < tail) — the codex
        // G4/G2/G3 case from the 2026-04-22 incident. New output landed
        // after the terminal-success log; watcher MUST persist and the
        // caller logs "post-terminal-success continuation".
        let input = super::WatcherStopInput {
            terminal_success_seen: true,
            tmux_alive: true,
            confirmed_end: 1024,
            tmux_tail_offset: 2048,
            idle_duration: Some(std::time::Duration::from_secs(60)),
            ..watcher_stop_input_default()
        };
        assert_eq!(
            super::watcher_stop_decision_after_terminal_success(input),
            super::WatcherStopDecision::PostTerminalSuccessContinuation
        );
    }

    #[test]
    fn watcher_stop_terminal_success_alive_idle_window_elapsed_continues_until_tmux_death() {
        // (terminal=Y, tmux_alive=Y, confirmed_end == tail, idle >= 5s) —
        // #1171: a quiet, idle post-result pane is not enough to end watcher
        // ownership. The watcher stops only after tmux liveness reports death.
        let input = super::WatcherStopInput {
            terminal_success_seen: true,
            tmux_alive: true,
            confirmed_end: 4096,
            tmux_tail_offset: 4096,
            idle_duration: Some(super::WATCHER_POST_TERMINAL_IDLE_WINDOW),
            ..watcher_stop_input_default()
        };
        assert_eq!(
            super::watcher_stop_decision_after_terminal_success(input),
            super::WatcherStopDecision::Continue
        );
    }

    #[test]
    fn watcher_stop_terminal_success_dead_tmux_stops_immediately() {
        // (terminal=Y, tmux_alive=N) — dead tmux dominates: stop without
        // waiting for the idle window to elapse, even if confirmed_end
        // hasn't caught up to the tail (no further output is possible).
        let input = super::WatcherStopInput {
            terminal_success_seen: true,
            tmux_alive: false,
            confirmed_end: 1024,
            tmux_tail_offset: 8192,
            idle_duration: None,
            ..watcher_stop_input_default()
        };
        assert_eq!(
            super::watcher_stop_decision_after_terminal_success(input),
            super::WatcherStopDecision::Stop
        );
    }

    #[test]
    fn watcher_stop_terminal_success_alive_caught_up_but_idle_too_short_continues() {
        // (terminal=Y, tmux_alive=Y, confirmed_end == tail, idle < 5s) —
        // confirmed_end caught up but the idle window hasn't elapsed; classify
        // this as a continuation wait, not a stop signal.
        let input = super::WatcherStopInput {
            terminal_success_seen: true,
            tmux_alive: true,
            confirmed_end: 4096,
            tmux_tail_offset: 4096,
            idle_duration: Some(std::time::Duration::from_secs(2)),
            ..watcher_stop_input_default()
        };
        assert_eq!(
            super::watcher_stop_decision_after_terminal_success(input),
            super::WatcherStopDecision::PostTerminalSuccessContinuation
        );
    }

    #[test]
    fn watcher_stop_terminal_success_alive_no_idle_observation_yet_continues() {
        // First poll after the relay has `idle_duration: None`. We require
        // an explicit observation before treating the alive pane as settled.
        let input = super::WatcherStopInput {
            terminal_success_seen: true,
            tmux_alive: true,
            confirmed_end: 4096,
            tmux_tail_offset: 4096,
            idle_duration: None,
            ..watcher_stop_input_default()
        };
        assert_eq!(
            super::watcher_stop_decision_after_terminal_success(input),
            super::WatcherStopDecision::PostTerminalSuccessContinuation
        );
    }

    #[test]
    fn tmux_liveness_decision_makes_dead_tmux_the_normal_stop_authority() {
        assert_eq!(
            super::tmux_liveness_decision(false, false, true),
            super::TmuxLivenessDecision::Continue
        );
        assert_eq!(
            super::tmux_liveness_decision(false, false, false),
            super::TmuxLivenessDecision::TmuxDied
        );
        assert_eq!(
            super::tmux_liveness_decision(true, false, false),
            super::TmuxLivenessDecision::QuietStop
        );
        assert_eq!(
            super::tmux_liveness_decision(false, true, false),
            super::TmuxLivenessDecision::QuietStop
        );
    }

    #[test]
    fn watcher_output_poll_drains_final_chunk_before_dead_tmux_shutdown() {
        assert_eq!(
            super::watcher_output_poll_decision(128, None),
            super::WatcherOutputPollDecision::DrainOutput
        );
        assert_eq!(
            super::watcher_output_poll_decision(
                0,
                Some(super::tmux_liveness_decision(false, false, false)),
            ),
            super::WatcherOutputPollDecision::TmuxDied
        );
    }

    #[test]
    fn paused_watcher_liveness_detects_dead_tmux_unless_operator_stopped() {
        assert_eq!(
            super::tmux_liveness_decision(false, false, false),
            super::TmuxLivenessDecision::TmuxDied
        );
        assert_eq!(
            super::tmux_liveness_decision(true, false, false),
            super::TmuxLivenessDecision::QuietStop
        );
        assert_eq!(
            super::tmux_liveness_decision(false, true, false),
            super::TmuxLivenessDecision::QuietStop
        );
    }

    #[test]
    fn watcher_ready_for_input_completion_requires_stable_idle_prompt_after_output() {
        let mut tracker = ReadyForInputIdleTracker::default();
        let start = std::time::Instant::now();

        assert_eq!(
            watcher_ready_for_input_turn_completed(&mut tracker, 100, 100, true, true, start),
            crate::services::provider::ReadyForInputIdleState::None
        );

        tracker.record_output();
        assert_eq!(
            watcher_ready_for_input_turn_completed(&mut tracker, 100, 120, true, true, start),
            crate::services::provider::ReadyForInputIdleState::None
        );
        assert_eq!(
            watcher_ready_for_input_turn_completed(
                &mut tracker,
                100,
                120,
                true,
                true,
                start + std::time::Duration::from_secs(10)
            ),
            crate::services::provider::ReadyForInputIdleState::None
        );
        assert_eq!(
            watcher_ready_for_input_turn_completed(
                &mut tracker,
                100,
                120,
                true,
                true,
                start + std::time::Duration::from_secs(16)
            ),
            crate::services::provider::ReadyForInputIdleState::PostWorkIdleTimeout
        );
    }

    #[test]
    fn watcher_ready_for_input_fresh_idle_does_not_trigger_failure_path() {
        let mut tracker = ReadyForInputIdleTracker::default();
        let start = std::time::Instant::now();

        tracker.record_output();
        assert_eq!(
            watcher_ready_for_input_turn_completed(&mut tracker, 100, 120, true, false, start),
            crate::services::provider::ReadyForInputIdleState::None
        );
        assert_eq!(
            watcher_ready_for_input_turn_completed(
                &mut tracker,
                100,
                120,
                true,
                false,
                start + std::time::Duration::from_secs(10)
            ),
            crate::services::provider::ReadyForInputIdleState::None
        );
        assert_eq!(
            watcher_ready_for_input_turn_completed(
                &mut tracker,
                100,
                120,
                true,
                false,
                start + std::time::Duration::from_secs(16)
            ),
            crate::services::provider::ReadyForInputIdleState::FreshIdle
        );
    }

    #[tokio::test]
    async fn ready_for_input_stall_marks_dispatch_failed_and_alerts_humans() {
        let db = crate::db::test_db();
        let shared = super::super::make_shared_data_for_tests_with_storage(Some(db.clone()), None);

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id)
                 VALUES ('agent-1', 'Agent 1', '444111')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
                 VALUES ('run-ready-stall', 'test/repo', 'agent-1', 'running')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority)
                 VALUES ('card-ready-stall', 'Ready stall card', 'in_progress', 'medium')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, thread_id, created_at, updated_at
                 ) VALUES (
                    'dispatch-ready-stall', 'card-ready-stall', 'agent-1', 'implementation', 'dispatched', 'Ready stall', '123456', datetime('now'), datetime('now')
                 )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (
                    id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at
                 ) VALUES (
                    'entry-ready-stall', 'run-ready-stall', 'card-ready-stall', 'agent-1', 'dispatched', 'dispatch-ready-stall', datetime('now')
                 )",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kv_meta (key, value) VALUES ('kanban_human_alert_channel_id', '555123')",
                [],
            )
            .unwrap();
        }

        let result = fail_dispatch_for_ready_for_input_stall(
            &shared,
            "dispatch-ready-stall",
            "AgentDesk-ready-stall",
        )
        .await
        .expect("ready-for-input failure helper");

        assert!(result.dispatch_failed);
        assert_eq!(result.card_id.as_deref(), Some("card-ready-stall"));
        assert!(result.card_marked);
        assert!(result.human_alert_sent);

        let conn = db.lock().unwrap();
        let dispatch_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = 'dispatch-ready-stall'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dispatch_status, "failed");

        let entry_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-ready-stall'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(entry_status, "failed");

        let (blocked_reason, metadata_raw): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT blocked_reason, metadata FROM kanban_cards WHERE id = 'card-ready-stall'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            blocked_reason.as_deref(),
            Some(READY_FOR_INPUT_STUCK_REASON)
        );
        let metadata: serde_json::Value =
            serde_json::from_str(metadata_raw.as_deref().expect("metadata after ready stall"))
                .unwrap();
        assert_eq!(metadata["labels"], "stuck_at_ready");

        let (target, reason_code, content): (String, Option<String>, String) = conn
            .query_row(
                "SELECT target, reason_code, content
                 FROM message_outbox
                 ORDER BY id DESC
                 LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(target, "channel:555123");
        assert_eq!(reason_code.as_deref(), Some("dispatch.stuck_at_ready"));
        assert!(content.contains("dispatch-ready-stall"));
        assert!(content.contains("card-ready-stall"));
    }

    // ── #826: background-task auto-trigger relay routes through notify outbox ──

    /// When a `Bash run_in_background` (or codex `--background`) task completes
    /// and Claude Code's `<task-notification>` mechanism fires the auto turn
    /// after the bridge has already cleaned up, the watcher must enqueue the
    /// terminal response on the notify-bot outbox so the user sees it. Going
    /// through the command bot would risk other agents in the channel treating
    /// the response as an actionable directive (infinite-loop hazard).
    #[tokio::test]
    async fn background_trigger_response_enqueues_notify_outbox_row() {
        let db = crate::db::test_db();
        let channel = ChannelId::new(987_654_321);
        let content = "PR #825 리뷰 4건 fix 완료";

        let enqueued = enqueue_background_trigger_response_to_notify_outbox(
            /*pg_pool*/ None,
            Some(&db),
            channel,
            content,
            /*data_start_offset*/ 4096,
        )
        .await;
        assert!(
            enqueued,
            "background-trigger enqueue must succeed when db is present"
        );

        let conn = db.lock().unwrap();
        let (target, stored_content, bot, source, reason_code, session_key): (
            String,
            String,
            String,
            String,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT target, content, bot, source, reason_code, session_key
                 FROM message_outbox ORDER BY id DESC LIMIT 1",
                [],
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
            .expect("expected one outbox row");

        assert_eq!(target, format!("channel:{}", channel.get()));
        assert_eq!(stored_content, content);
        assert_eq!(bot, "notify", "must use notify bot to avoid loop hazard");
        assert_eq!(source, "system");
        // #897 counter-model review P1 #3: both reason_code and session_key
        // must be populated so the lifecycle dedupe in message_outbox can arm.
        assert_eq!(reason_code.as_deref(), Some("bg_trigger.auto_turn"));
        let session_key = session_key.expect("session_key must be populated for dedupe");
        assert!(
            session_key.starts_with(&format!("bg_trigger:ch:{}:off:4096:h:", channel.get())),
            "session_key must encode channel + offset + content hash; got {session_key}"
        );
    }

    /// #897 P1 #3: consecutive background-task completions in the same
    /// channel must each produce their own outbox row — each event is a
    /// distinct tmux range, so the `session_key` (which includes
    /// `data_start_offset` and a content hash) must differ between them and
    /// the dedupe must NOT collapse legitimately-separate events into one.
    #[tokio::test]
    async fn background_trigger_response_does_not_dedupe_distinct_events() {
        let db = crate::db::test_db();
        let channel = ChannelId::new(555_111_222);
        assert!(
            enqueue_background_trigger_response_to_notify_outbox(
                None,
                Some(&db),
                channel,
                "first completion",
                /*data_start_offset*/ 1_000,
            )
            .await
        );
        assert!(
            enqueue_background_trigger_response_to_notify_outbox(
                None,
                Some(&db),
                channel,
                "second completion",
                /*data_start_offset*/ 2_000,
            )
            .await
        );

        let count: i64 = db
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM message_outbox WHERE target = ?1 AND bot = 'notify'",
                [format!("channel:{}", channel.get()).as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 2,
            "consecutive events with distinct offsets/content must land as separate rows"
        );
    }

    /// #897 P1 #3: a genuine retry of the SAME tmux range (same offset +
    /// identical content) within the dedupe TTL must collapse into a single
    /// outbox row, preventing the watcher from re-enqueuing while the outbox
    /// worker is still driving the same message to Discord.
    #[tokio::test]
    async fn background_trigger_response_dedupes_identical_retry() {
        let db = crate::db::test_db();
        let channel = ChannelId::new(666_222_333);
        assert!(
            enqueue_background_trigger_response_to_notify_outbox(
                None,
                Some(&db),
                channel,
                "same content",
                /*data_start_offset*/ 8_192,
            )
            .await
        );
        assert!(
            enqueue_background_trigger_response_to_notify_outbox(
                None,
                Some(&db),
                channel,
                "same content",
                /*data_start_offset*/ 8_192,
            )
            .await
        );

        let count: i64 = db
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM message_outbox WHERE target = ?1 AND bot = 'notify'",
                [format!("channel:{}", channel.get()).as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "identical retry at the same offset must dedupe to a single row"
        );
    }

    /// Empty/whitespace responses must short-circuit without writing a row —
    /// otherwise the user sees a noise notification with no content.
    #[tokio::test]
    async fn background_trigger_response_skips_empty_payload() {
        let db = crate::db::test_db();
        let channel = ChannelId::new(111_222_333);
        assert!(
            enqueue_background_trigger_response_to_notify_outbox(
                None,
                Some(&db),
                channel,
                "   \n",
                0,
            )
            .await
        );
        let count: i64 = db
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM message_outbox", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "empty content must not produce an outbox row");
    }

    /// When the database is unavailable, the helper reports failure so the
    /// caller can fall back to a direct Discord send rather than silently
    /// dropping the response (#826 root cause was a silent drop).
    #[tokio::test]
    async fn background_trigger_response_reports_failure_when_db_missing() {
        let channel = ChannelId::new(999_888_777);
        let ok = enqueue_background_trigger_response_to_notify_outbox(
            /*pg_pool*/ None,
            /*db*/ None,
            channel,
            "would-have-been-delivered",
            0,
        )
        .await;
        assert!(!ok, "missing db must surface as failure to enable fallback");
    }

    /// #897 P1 #2 guard: `parse_bg_trigger_offset_from_session_key` must
    /// round-trip the exact offset that `build_bg_trigger_session_key`
    /// embedded, across a spread of offsets. Without a stable inverse, the
    /// reconciliation poll cannot identify which tmux range to re-stage
    /// after an outbox failure.
    #[test]
    fn parse_bg_trigger_offset_roundtrips_build_key() {
        for offset in [0u64, 1, 4096, 1 << 32, 1 << 48, u64::MAX] {
            let key = build_bg_trigger_session_key(42, offset, "payload");
            let parsed = parse_bg_trigger_offset_from_session_key(&key);
            assert_eq!(
                parsed,
                Some(offset),
                "offset {} must round-trip through session_key",
                offset
            );
        }
    }

    /// #897 P1 #2: malformed / foreign session_keys must not panic or
    /// produce spurious offsets — the reconcile poll has to be robust to
    /// hand-written rows or schema drift.
    #[test]
    fn parse_bg_trigger_offset_returns_none_for_non_matching_keys() {
        assert_eq!(parse_bg_trigger_offset_from_session_key(""), None);
        assert_eq!(
            parse_bg_trigger_offset_from_session_key("random:session:key"),
            None
        );
        assert_eq!(
            parse_bg_trigger_offset_from_session_key("bg_trigger:ch:1:off:not-a-number:h:abcd"),
            None
        );
        assert_eq!(
            parse_bg_trigger_offset_from_session_key("bg_trigger:ch:1:off:"),
            None
        );
    }

    /// #897 P1 #2 policy guard: rollback must pull the watermark back
    /// below the failed offset when it has moved past, but must NOT
    /// accidentally advance the watermark when it is already behind the
    /// failure. And it must never panic on a failed offset of 0.
    #[test]
    fn rollback_enqueued_offset_pulls_back_only_when_ahead_of_failure() {
        // Nothing staged → nothing to roll back.
        assert_eq!(
            rollback_enqueued_offset_for_reconciled_failures(None, 12_000),
            None,
        );

        // Watermark already at or below the failed offset → unchanged.
        assert_eq!(
            rollback_enqueued_offset_for_reconciled_failures(Some(8_000), 12_000),
            Some(8_000),
        );
        assert_eq!(
            rollback_enqueued_offset_for_reconciled_failures(Some(12_000), 12_000),
            Some(12_000),
        );

        // Watermark ahead of the failure → pulled back to just before it.
        assert_eq!(
            rollback_enqueued_offset_for_reconciled_failures(Some(20_000), 12_000),
            Some(11_999),
        );

        // Reconciled offset 0 must saturate at 0, not wrap.
        assert_eq!(
            rollback_enqueued_offset_for_reconciled_failures(Some(5), 0),
            Some(0),
        );
    }

    /// #897 P1 #2: without a Postgres outbox, the reconciler returns
    /// `None` and leaves direct-send fallback decisions to the caller.
    #[tokio::test]
    async fn reconcile_returns_none_when_no_failed_rows() {
        let db = crate::db::test_db();
        let channel = ChannelId::new(888_555_222);
        let min =
            super::reconcile_failed_bg_trigger_enqueues_for_channel(None, Some(&db), channel).await;
        assert_eq!(min, None);
    }

    /// #897 P1 #3 guard: `build_bg_trigger_session_key` must produce the
    /// same key for identical inputs (so dedupe can arm) and differing keys
    /// when EITHER the offset OR the content changes.
    #[test]
    fn build_bg_trigger_session_key_is_stable_and_offset_sensitive() {
        let a = build_bg_trigger_session_key(100, 4096, "payload");
        let b = build_bg_trigger_session_key(100, 4096, "payload");
        assert_eq!(a, b, "identical inputs must yield identical keys");

        let different_offset = build_bg_trigger_session_key(100, 8192, "payload");
        assert_ne!(a, different_offset, "different offset must yield a new key");

        let different_content = build_bg_trigger_session_key(100, 4096, "payload2");
        assert_ne!(
            a, different_content,
            "different content must yield a new key"
        );

        let different_channel = build_bg_trigger_session_key(200, 4096, "payload");
        assert_ne!(
            a, different_channel,
            "different channel must yield a new key"
        );
    }

    #[test]
    fn terminal_relay_decision_suppresses_internal_task_notifications_without_notify_outbox() {
        assert_eq!(
            terminal_relay_decision(true, None),
            super::TerminalRelayDecision {
                should_direct_send: true,
                should_tag_monitor_origin: false,
                should_enqueue_notify_outbox: false,
                suppressed: false,
            }
        );
        assert_eq!(
            terminal_relay_decision(true, Some(TaskNotificationKind::MonitorAutoTurn)),
            super::TerminalRelayDecision {
                should_direct_send: true,
                should_tag_monitor_origin: true,
                should_enqueue_notify_outbox: false,
                suppressed: false,
            }
        );
        assert_eq!(
            terminal_relay_decision(false, Some(TaskNotificationKind::MonitorAutoTurn)),
            super::TerminalRelayDecision {
                should_direct_send: false,
                should_tag_monitor_origin: false,
                should_enqueue_notify_outbox: false,
                suppressed: true,
            }
        );
        assert_eq!(
            terminal_relay_decision(true, Some(TaskNotificationKind::Subagent)),
            super::TerminalRelayDecision {
                should_direct_send: false,
                should_tag_monitor_origin: false,
                should_enqueue_notify_outbox: false,
                suppressed: true,
            }
        );
        // Background kind with assistant response = user-facing content after a
        // mid-turn background event (e.g. Monitor completion). Must relay (#1058).
        assert_eq!(
            terminal_relay_decision(true, Some(TaskNotificationKind::Background)),
            super::TerminalRelayDecision {
                should_direct_send: true,
                should_tag_monitor_origin: false,
                should_enqueue_notify_outbox: false,
                suppressed: false,
            }
        );
        // Background kind without any assistant response = only the tag arrived,
        // nothing to show user. Suppress.
        assert_eq!(
            terminal_relay_decision(false, Some(TaskNotificationKind::Background)),
            super::TerminalRelayDecision {
                should_direct_send: false,
                should_tag_monitor_origin: false,
                should_enqueue_notify_outbox: false,
                suppressed: true,
            }
        );
    }

    #[test]
    fn strip_inprogress_indicators_removes_spinner_tool_preview_lines() {
        let input = concat!(
            "작업 요약\n",
            "  ⠼ ⚙ TodoWrite: Todo: 1 pending, 0 in progress, 5 completed\n",
            "중요한 결과\n",
            "⠋ ⚙ Bash: cargo check\n",
            "\n"
        );

        assert_eq!(strip_inprogress_indicators(input), "작업 요약\n중요한 결과");
    }

    #[test]
    fn strip_inprogress_indicators_leaves_plain_text_unchanged() {
        let input = "작업 요약\n⚙ spinner 없이 시작한 일반 텍스트\n중요한 결과";

        assert_eq!(strip_inprogress_indicators(input), input);
    }

    #[test]
    fn suppressed_placeholder_preserves_exposed_live_edit() {
        assert_eq!(
            suppressed_placeholder_action(
                true,
                32,
                "partial response\n\n⠼ ⚙ TodoWrite: Todo: 1 pending, 0 in progress, 5 completed",
            ),
            SuppressedPlaceholderAction::Edit(format!(
                "partial response\n\n{SUPPRESSED_INTERNAL_LABEL}"
            ))
        );
        assert_eq!(
            suppressed_placeholder_action(true, 0, "status only"),
            SuppressedPlaceholderAction::Edit(format!(
                "status only\n\n{SUPPRESSED_INTERNAL_LABEL}"
            ))
        );
    }

    #[test]
    fn suppressed_placeholder_deletes_only_clean_placeholder() {
        assert_eq!(
            suppressed_placeholder_action(true, 0, ""),
            SuppressedPlaceholderAction::Delete
        );
        assert_eq!(
            suppressed_placeholder_action(false, 99, "already visible"),
            SuppressedPlaceholderAction::None
        );
    }

    // ── #1009 Monitor suppressed-placeholder formatter tests ──────────────────

    #[test]
    fn format_monitor_suppressed_label_with_entries_lists_keys() {
        let keys = vec!["ci-build".to_string(), "pr-review".to_string()];
        let label = format_monitor_suppressed_label(3, &keys);
        assert_eq!(
            label,
            "🔔 Monitor 3회 처리 · 다음 모니터: {ci-build, pr-review}"
        );
    }

    #[test]
    fn format_monitor_suppressed_label_with_no_entries_shows_empty_marker() {
        let label = format_monitor_suppressed_label(2, &[]);
        assert_eq!(label, "🔔 Monitor 2회 처리 · (등록된 모니터 없음)");
    }

    #[test]
    fn format_monitor_suppressed_body_appends_label_after_existing_body() {
        let keys = vec!["ci-build".to_string()];
        let body = format_monitor_suppressed_body("부분 응답", 1, &keys);
        assert!(
            body.starts_with("부분 응답"),
            "preserves prior body: {body}"
        );
        assert!(
            body.contains("🔔 Monitor 1회 처리 · 다음 모니터: {ci-build}"),
            "appends monitor summary label: {body}"
        );
        assert!(
            !body.contains(SUPPRESSED_INTERNAL_LABEL),
            "monitor body must not carry the generic internal label: {body}"
        );
    }

    #[test]
    fn format_monitor_suppressed_body_guards_against_discord_msg_limit() {
        // Build an oversize pre-existing body AND an oversize set of entry keys
        // so the combined output would exceed DISCORD_MSG_LIMIT without a guard.
        let oversize_body = "가".repeat(super::super::DISCORD_MSG_LIMIT);
        let entry_keys: Vec<String> = (0..500).map(|i| format!("monitor-key-{i}")).collect();
        let body = format_monitor_suppressed_body(&oversize_body, 9, &entry_keys);
        assert!(
            body.len() <= super::super::DISCORD_MSG_LIMIT,
            "expected body len {} to be <= DISCORD_MSG_LIMIT {}",
            body.len(),
            super::super::DISCORD_MSG_LIMIT
        );
    }

    #[test]
    fn consume_monitor_auto_turn_preamble_once_is_one_shot_per_turn() {
        let mut flag = false;
        let first = consume_monitor_auto_turn_preamble_once(&mut flag);
        assert_eq!(first, Some(MONITOR_AUTO_TURN_PREAMBLE_HINT));
        assert!(flag, "flag must flip to true after first consumption");
        assert!(
            first
                .expect("first consumption returns the hint")
                .contains("1줄 요약"),
            "hint must mention 1-line summary requirement"
        );
        assert!(
            first
                .expect("first consumption returns the hint")
                .contains("다음 액션"),
            "hint must mention next-action requirement"
        );

        // Subsequent calls within the same turn must return None.
        assert_eq!(consume_monitor_auto_turn_preamble_once(&mut flag), None);
        assert_eq!(consume_monitor_auto_turn_preamble_once(&mut flag), None);
    }

    #[test]
    fn terminal_relay_decision_direct_sends_when_monitor_turn_has_assistant_response() {
        // #1009 DoD regression lock: when the monitor auto-turn produces an
        // assistant response text, the terminal relay MUST take the direct-send
        // path (suppressed=false) — no suppression, no placeholder rewrite.
        let decision = terminal_relay_decision(true, Some(TaskNotificationKind::MonitorAutoTurn));
        assert_eq!(
            decision,
            super::TerminalRelayDecision {
                should_direct_send: true,
                should_tag_monitor_origin: true,
                should_enqueue_notify_outbox: false,
                suppressed: false,
            }
        );
    }

    fn test_placeholder_suppress_context<'a>(
        origin: PlaceholderSuppressOrigin,
        placeholder_msg_id: Option<MessageId>,
        response_sent_offset: usize,
        last_edit_text: &'a str,
        tmux_session_name: &'a str,
        task_notification_kind: Option<TaskNotificationKind>,
        reattach_offset_match: bool,
    ) -> PlaceholderSuppressContext<'a> {
        PlaceholderSuppressContext {
            origin,
            placeholder_msg_id,
            response_sent_offset,
            last_edit_text,
            inflight_state: None,
            has_active_turn: false,
            tmux_session_name,
            task_notification_kind,
            reattach_offset_match,
        }
    }

    #[test]
    fn decide_placeholder_suppression_bridge_guard_preserves_on_reattach_match() {
        let ctx = test_placeholder_suppress_context(
            PlaceholderSuppressOrigin::ActiveBridgeTurnGuard,
            Some(MessageId::new(1)),
            42,
            "already delivered body",
            "AgentDesk-claude-adk-cc",
            None,
            true,
        );
        match decide_placeholder_suppression(&ctx) {
            PlaceholderSuppressDecision::Preserve {
                reason: "reattach-offset-match",
                cleaned_body,
            } => {
                assert_eq!(cleaned_body, "already delivered body");
            }
            other => panic!("expected Preserve reattach-offset-match, got {other:?}"),
        }
    }

    #[test]
    fn decide_placeholder_suppression_preserve_strips_inprogress_indicators() {
        let ctx = test_placeholder_suppress_context(
            PlaceholderSuppressOrigin::ActiveBridgeTurnGuard,
            Some(MessageId::new(1)),
            42,
            "real body\n\n⠼ ⚙ TodoWrite: Todo: 1 pending, 0 in progress, 5 completed\n",
            "AgentDesk-claude-adk-cc",
            None,
            true,
        );
        match decide_placeholder_suppression(&ctx) {
            PlaceholderSuppressDecision::Preserve { cleaned_body, .. } => {
                assert_eq!(cleaned_body, "real body");
                assert!(!cleaned_body.contains("⠼"));
                assert!(!cleaned_body.contains("⚙"));
            }
            other => panic!("expected Preserve with stripped body, got {other:?}"),
        }
    }

    #[test]
    fn decide_placeholder_suppression_bridge_guard_falls_through_to_edit_label_without_match() {
        let ctx = test_placeholder_suppress_context(
            PlaceholderSuppressOrigin::ActiveBridgeTurnGuard,
            Some(MessageId::new(1)),
            42,
            "visible body",
            "AgentDesk-claude-adk-cc",
            None,
            false,
        );
        match decide_placeholder_suppression(&ctx) {
            PlaceholderSuppressDecision::Edit(content) => {
                assert!(content.contains(SUPPRESSED_INTERNAL_LABEL))
            }
            other => panic!("expected Edit with label, got {other:?}"),
        }
    }

    #[test]
    fn decide_placeholder_suppression_task_notification_preserves_background_body() {
        let ctx = test_placeholder_suppress_context(
            PlaceholderSuppressOrigin::TaskNotificationTerminal,
            Some(MessageId::new(1)),
            42,
            "live user-facing content",
            "AgentDesk-claude-adk-cc",
            Some(TaskNotificationKind::Background),
            false,
        );
        match decide_placeholder_suppression(&ctx) {
            PlaceholderSuppressDecision::Preserve {
                reason: "background-or-subagent-kind",
                cleaned_body,
            } => {
                assert_eq!(cleaned_body, "live user-facing content");
            }
            other => panic!("expected Preserve background-or-subagent-kind, got {other:?}"),
        }
    }

    #[test]
    fn decide_placeholder_suppression_task_notification_preserves_subagent_body() {
        let ctx = test_placeholder_suppress_context(
            PlaceholderSuppressOrigin::TaskNotificationTerminal,
            Some(MessageId::new(1)),
            42,
            "subagent body",
            "AgentDesk-claude-adk-cc",
            Some(TaskNotificationKind::Subagent),
            false,
        );
        match decide_placeholder_suppression(&ctx) {
            PlaceholderSuppressDecision::Preserve {
                reason: "background-or-subagent-kind",
                cleaned_body,
            } => {
                assert_eq!(cleaned_body, "subagent body");
            }
            other => panic!("expected Preserve background-or-subagent-kind, got {other:?}"),
        }
    }

    #[test]
    fn decide_placeholder_suppression_task_notification_edits_for_monitor_auto_turn() {
        let ctx = test_placeholder_suppress_context(
            PlaceholderSuppressOrigin::TaskNotificationTerminal,
            Some(MessageId::new(1)),
            42,
            "monitor-auto body",
            "AgentDesk-claude-adk-cc",
            Some(TaskNotificationKind::MonitorAutoTurn),
            false,
        );
        match decide_placeholder_suppression(&ctx) {
            PlaceholderSuppressDecision::Edit(content) => {
                assert!(content.contains(SUPPRESSED_INTERNAL_LABEL))
            }
            other => panic!("expected Edit with label, got {other:?}"),
        }
    }

    #[test]
    fn decide_placeholder_suppression_task_notification_deletes_unexposed_placeholder() {
        let ctx = test_placeholder_suppress_context(
            PlaceholderSuppressOrigin::TaskNotificationTerminal,
            Some(MessageId::new(1)),
            0,
            "",
            "AgentDesk-claude-adk-cc",
            Some(TaskNotificationKind::MonitorAutoTurn),
            false,
        );
        assert_eq!(
            decide_placeholder_suppression(&ctx),
            PlaceholderSuppressDecision::Delete
        );
    }

    #[test]
    fn orphan_suppressed_placeholder_reconcile_rewrites_terminal_marker() {
        let tmux_name = ProviderKind::Codex.build_tmux_session_name("adk-cdx-t42");
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx-t42".to_string()),
            7,
            9,
            11,
            "background task".to_string(),
            Some("session-1".to_string()),
            Some(tmux_name.clone()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            128,
        );
        state.full_response = "already delivered\npending tail".to_string();
        state.response_sent_offset = "already delivered\n".len();
        state.current_tool_line =
            Some("⚙ TodoWrite: Todo: 1 pending, 0 in progress, 5 completed".to_string());

        let action = orphan_suppressed_placeholder_action(&state, false, &tmux_name);

        assert_eq!(
            action,
            SuppressedPlaceholderAction::Edit(format!(
                "pending tail\n\n{SUPPRESSED_RESTART_LABEL}"
            ))
        );
    }

    #[test]
    fn internal_suppress_and_orphan_reconcile_use_distinct_labels() {
        let tmux_name = ProviderKind::Codex.build_tmux_session_name("adk-cdx-t42");
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx-t42".to_string()),
            7,
            9,
            11,
            "background task".to_string(),
            Some("session-1".to_string()),
            Some(tmux_name.clone()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            128,
        );
        state.full_response = "already delivered\nshared tail".to_string();
        state.response_sent_offset = "already delivered\n".len();

        assert_eq!(
            suppressed_placeholder_action(true, state.response_sent_offset, "shared tail"),
            SuppressedPlaceholderAction::Edit(format!(
                "shared tail\n\n{SUPPRESSED_INTERNAL_LABEL}"
            ))
        );
        assert_eq!(
            orphan_suppressed_placeholder_action(&state, false, &tmux_name),
            SuppressedPlaceholderAction::Edit(format!("shared tail\n\n{SUPPRESSED_RESTART_LABEL}"))
        );
    }

    #[test]
    fn orphan_suppressed_placeholder_reconcile_skips_active_turns() {
        let tmux_name = ProviderKind::Codex.build_tmux_session_name("adk-cdx-t42");
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx-t42".to_string()),
            7,
            9,
            11,
            "background task".to_string(),
            Some("session-1".to_string()),
            Some(tmux_name.clone()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            128,
        );
        state.full_response = "already delivered\npending tail".to_string();
        state.response_sent_offset = "already delivered\n".len();

        assert_eq!(
            orphan_suppressed_placeholder_action(&state, true, &tmux_name),
            SuppressedPlaceholderAction::None
        );
    }

    #[test]
    fn restored_watcher_seed_uses_existing_placeholder_and_offset() {
        let tmux_name = ProviderKind::Codex.build_tmux_session_name("adk-cdx-t42");
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx-t42".to_string()),
            7,
            9,
            11,
            "continue".to_string(),
            Some("session-1".to_string()),
            Some(tmux_name.clone()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            128,
        );
        state.full_response = "already delivered\npending".to_string();
        state.response_sent_offset = "already delivered\n".len();
        state.task_notification_kind = Some(TaskNotificationKind::Background);

        let restored = restored_watcher_turn_from_inflight(&state, &tmux_name, true)
            .expect("valid inflight should seed watcher resume");
        let seed = watcher_stream_seed(Some(restored));

        assert_eq!(seed.placeholder_msg_id, Some(MessageId::new(11)));
        assert_eq!(seed.response_sent_offset, "already delivered\n".len());
        assert_eq!(seed.full_response, "already delivered\npending");
        assert_eq!(
            seed.task_notification_kind,
            Some(TaskNotificationKind::Background)
        );
        assert!(seed.finish_mailbox_on_completion);
    }

    #[test]
    fn restored_watcher_seed_rejects_mismatched_tmux_session() {
        let tmux_name = ProviderKind::Codex.build_tmux_session_name("adk-cdx-t42");
        let state = InflightTurnState::new(
            ProviderKind::Codex,
            42,
            Some("adk-cdx-t42".to_string()),
            7,
            9,
            11,
            "continue".to_string(),
            Some("session-1".to_string()),
            Some(tmux_name),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            128,
        );

        assert!(
            restored_watcher_turn_from_inflight(&state, "AgentDesk-codex-other-channel", true)
                .is_none()
        );
    }

    #[tokio::test]
    async fn monitor_auto_turn_suppress_enqueues_notify_outbox_row() {
        let db = crate::db::test_db();
        let channel = ChannelId::new(987_000_111);

        let entry_keys = vec!["ci-build".to_string(), "pr-review".to_string()];
        let enqueued = enqueue_monitor_auto_turn_suppressed_notification(
            None,
            Some(&db),
            channel,
            "monitor-session",
            14_900,
            7,
            &entry_keys,
        );
        assert!(enqueued);

        let row = {
            if let Ok(conn) = db.lock() {
                conn.query_row(
                    "SELECT target, content, bot, source, reason_code, session_key
                     FROM message_outbox ORDER BY id DESC LIMIT 1",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, Option<String>>(5)?,
                        ))
                    },
                )
                .ok()
            } else {
                None
            }
        };

        // #1009: lifecycle notice now reuses `format_monitor_suppressed_label`
        // so suppressed placeholder + notify-outbox row share identical copy.
        let expected_content = format!(
            "{} · 대상: monitor-session",
            format_monitor_suppressed_label(7, &entry_keys)
        );
        assert_eq!(
            row,
            Some((
                format!("channel:{}", channel.get()),
                expected_content,
                "notify".to_string(),
                "system".to_string(),
                Some(MONITOR_AUTO_TURN_REASON_CODE.to_string()),
                Some(format!("monitor_auto_turn:ch:{}:off:14900", channel.get())),
            ))
        );
    }

    #[test]
    fn monitor_auto_turn_completion_notice_shares_formatter_with_suppressed_body() {
        // #1009 DRY enforcement: the lifecycle notice content and the
        // suppressed-placeholder body both end with the SAME label produced
        // by `format_monitor_suppressed_label`.
        let entry_keys = vec!["ci-build".to_string(), "release".to_string()];
        let event_count = 4;
        let label = format_monitor_suppressed_label(event_count, &entry_keys);
        let notice =
            super::monitor_auto_turn_completion_notice("foo-channel", event_count, &entry_keys);
        assert!(
            notice.starts_with(&label),
            "lifecycle notice must lead with the shared formatter output: {notice}"
        );
        assert!(
            notice.contains("foo-channel"),
            "lifecycle notice must still scope by channel/tmux name: {notice}"
        );
        let body = format_monitor_suppressed_body("placeholder body", event_count, &entry_keys);
        assert!(
            body.contains(&label),
            "suppressed body must also embed the shared label: {body}"
        );
    }

    #[test]
    fn monitor_auto_turn_completion_notice_shows_empty_marker_with_no_entries() {
        let notice = super::monitor_auto_turn_completion_notice("foo-channel", 2, &[]);
        assert!(notice.starts_with("🔔 Monitor 2회 처리 · (등록된 모니터 없음)"));
        assert!(notice.ends_with("· 대상: foo-channel"));
    }

    #[tokio::test]
    async fn monitor_auto_turn_normal_relay_does_not_request_notify_outbox() {
        let db = crate::db::test_db();
        let decision = terminal_relay_decision(true, Some(TaskNotificationKind::MonitorAutoTurn));

        assert!(decision.should_direct_send);
        assert!(!decision.suppressed);

        let count = {
            if let Ok(conn) = db.lock() {
                conn.query_row("SELECT COUNT(*) FROM message_outbox", [], |row| {
                    row.get::<_, i64>(0)
                })
                .ok()
            } else {
                None
            }
        };
        assert_eq!(count, Some(0));
    }

    #[tokio::test]
    async fn monitor_auto_turn_defers_until_user_turn_finishes_and_notifies()
    -> Result<(), Box<dyn std::error::Error>> {
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let tmp = tempfile::tempdir()?;
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let db = crate::db::test_db();
        let shared = super::super::make_shared_data_for_tests_with_storage(Some(db.clone()), None);
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(987_000_222);
        let user_started = super::super::mailbox_try_start_turn(
            &shared,
            channel,
            Arc::new(CancelToken::new()),
            UserId::new(42),
            MessageId::new(100),
        )
        .await;
        assert!(user_started);

        let cancel = Arc::new(AtomicBool::new(false));
        let shared_for_task = shared.clone();
        let cancel_for_task = cancel.clone();
        let provider_for_task = provider.clone();
        let handle = tokio::spawn(async move {
            start_monitor_auto_turn_when_available(
                &shared_for_task,
                &provider_for_task,
                channel,
                24_000,
                cancel_for_task.as_ref(),
            )
            .await
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(80)).await;
        let deferred_count = {
            if let Ok(conn) = db.lock() {
                conn.query_row(
                    "SELECT COUNT(*) FROM message_outbox WHERE reason_code = ?1",
                    [MONITOR_AUTO_TURN_DEFERRED_REASON_CODE],
                    |row| row.get::<_, i64>(0),
                )
                .ok()
            } else {
                None
            }
        };
        assert_eq!(deferred_count, Some(1));

        let finish = super::super::mailbox_finish_turn(&shared, &provider, channel).await;
        assert!(finish.removed_token.is_some());

        let start = tokio::time::timeout(tokio::time::Duration::from_secs(2), handle).await??;
        assert_eq!(
            start,
            super::MonitorAutoTurnStart {
                acquired: true,
                deferred: true,
            }
        );

        let snapshot = super::super::mailbox_snapshot(&shared, channel).await;
        assert!(snapshot.cancel_token.is_some());
        assert_eq!(snapshot.active_request_owner, Some(UserId::new(1)));
        assert_eq!(
            snapshot.active_user_message_id,
            Some(MessageId::new(24_000))
        );

        finish_monitor_auto_turn(&shared, &provider, channel).await;
        assert!(
            !super::super::mailbox_has_active_turn(&shared, channel).await,
            "monitor mailbox claim must clear after the monitor turn finishes"
        );

        unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
        Ok(())
    }

    #[tokio::test]
    async fn user_message_queues_while_monitor_auto_turn_is_active()
    -> Result<(), Box<dyn std::error::Error>> {
        let _lock = match test_env_lock().lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let tmp = tempfile::tempdir()?;
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };

        let shared = super::super::make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(987_000_333);
        let cancel = AtomicBool::new(false);
        let start =
            start_monitor_auto_turn_when_available(&shared, &provider, channel, 31_000, &cancel)
                .await;
        assert_eq!(
            start,
            super::MonitorAutoTurnStart {
                acquired: true,
                deferred: false,
            }
        );
        assert!(super::super::mailbox_has_active_turn(&shared, channel).await);

        let queued = super::super::mailbox_enqueue_intervention(
            &shared,
            &provider,
            channel,
            super::super::Intervention {
                author_id: UserId::new(99),
                message_id: MessageId::new(200),
                source_message_ids: vec![MessageId::new(200)],
                text: "queued behind monitor".to_string(),
                mode: super::super::InterventionMode::Soft,
                created_at: std::time::Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
            },
        )
        .await;
        assert!(queued.enqueued);

        let snapshot = super::super::mailbox_snapshot(&shared, channel).await;
        assert!(snapshot.cancel_token.is_some());
        assert_eq!(snapshot.intervention_queue.len(), 1);

        finish_monitor_auto_turn(&shared, &provider, channel).await;

        let snapshot = super::super::mailbox_snapshot(&shared, channel).await;
        assert!(snapshot.cancel_token.is_none());
        assert_eq!(snapshot.intervention_queue.len(), 1);
        let next = super::super::mailbox_take_next_soft_intervention(&shared, &provider, channel)
            .await
            .map(|(intervention, _)| intervention.text);
        assert_eq!(next.as_deref(), Some("queued behind monitor"));

        unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
        Ok(())
    }

    #[test]
    fn process_watcher_lines_classifies_background_task_notification() {
        let mut buffer = concat!(
            "{\"type\":\"system\",\"subtype\":\"task_notification\",\"task_id\":\"bg-42\",\"status\":\"completed\",\"summary\":\"CI green\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"PR #825 리뷰 반영 완료\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}\n"
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert_eq!(
            outcome.task_notification_kind,
            Some(TaskNotificationKind::Background)
        );
        assert_eq!(full_response, "PR #825 리뷰 반영 완료");
    }

    #[test]
    fn process_watcher_lines_classifies_subagent_task_notification() {
        let mut buffer = concat!(
            "{\"type\":\"system\",\"subtype\":\"task_started\",\"task_id\":\"sub-1\",\"task_type\":\"local_agent\"}\n",
            "{\"type\":\"system\",\"subtype\":\"task_notification\",\"task_id\":\"sub-1\",\"status\":\"completed\",\"summary\":\"Subagent finished\"}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}\n"
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert_eq!(
            outcome.task_notification_kind,
            Some(TaskNotificationKind::Subagent)
        );
        assert_eq!(full_response, "done");
    }

    #[test]
    fn process_watcher_lines_classifies_monitor_auto_turn_task_notification() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_mon_1\",\"name\":\"Monitor\",\"input\":{\"command\":\"gh pr view\"}}]}}\n",
            "{\"type\":\"system\",\"subtype\":\"task_started\",\"task_id\":\"mon-1\",\"tool_use_id\":\"toolu_mon_1\",\"task_type\":\"tool\"}\n",
            "{\"type\":\"system\",\"subtype\":\"task_notification\",\"task_id\":\"mon-1\",\"status\":\"completed\",\"summary\":\"Monitor event: PR updated\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"PR #938 상태 갱신 완료\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}\n"
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert_eq!(
            outcome.task_notification_kind,
            Some(TaskNotificationKind::MonitorAutoTurn)
        );
        assert_eq!(full_response, "PR #938 상태 갱신 완료");
    }

    #[test]
    fn process_watcher_lines_leaves_task_notification_kind_empty_for_foreground_turn() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}\n"
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert_eq!(outcome.task_notification_kind, None);
        assert_eq!(full_response, "hello");
    }

    /// #826 P1 #2 regression guard: when the notify-bot outbox enqueue fails
    /// AND no direct-send fallback reaches Discord, the watcher MUST leave
    /// BOTH offset watermarks untouched so the same tmux range can be
    /// re-relayed on the next scan. Advancing the canonical relayed offset
    /// here is the bug that permanently loses a completion notification when
    /// notify-bot is unavailable.
    #[test]
    fn notify_path_does_not_advance_offset_on_enqueue_failure_without_fallback() {
        // Enqueue failed AND direct-send fallback also failed → leave both
        // watermarks alone (the content is still in flight from the watcher's
        // point of view; next tick must retry).
        assert_eq!(
            notify_path_offset_advance_decision(
                /*has_current_response*/ true, /*enqueue_succeeded*/ false,
                /*direct_send_delivered*/ false,
            ),
            OffsetAdvanceDecision {
                advance_relayed: false,
                advance_enqueued: false
            },
            "enqueue-fail + fallback-fail with content must leave both watermarks untouched"
        );

        // Enqueue SUCCEEDED but no foreground delivery confirmation yet —
        // advance ONLY the enqueue watermark so the outbox row is deduped on
        // the next tick, while the canonical relayed watermark waits for
        // actual Discord delivery. THIS is the P1 #2 fix: the original code
        // treated enqueue success as a delivery-equivalent and advanced the
        // relayed offset.
        assert_eq!(
            notify_path_offset_advance_decision(
                /*has_current_response*/ true, /*enqueue_succeeded*/ true,
                /*direct_send_delivered*/ false,
            ),
            OffsetAdvanceDecision {
                advance_relayed: false,
                advance_enqueued: true
            },
            "enqueue success without delivery confirmation must NOT advance last_relayed_offset"
        );

        // Enqueue failed but fallback direct-send reached Discord → both
        // watermarks lift together.
        assert_eq!(
            notify_path_offset_advance_decision(true, false, true),
            OffsetAdvanceDecision {
                advance_relayed: true,
                advance_enqueued: true
            }
        );

        // Both succeeded (uncommon but possible) → lock-step advance.
        assert_eq!(
            notify_path_offset_advance_decision(true, true, true),
            OffsetAdvanceDecision {
                advance_relayed: true,
                advance_enqueued: true
            }
        );

        // No content to deliver → trivially safe to advance past the empty
        // range (preserves the original single-offset behaviour so the
        // watcher doesn't spin on an empty turn).
        assert_eq!(
            notify_path_offset_advance_decision(false, false, false),
            OffsetAdvanceDecision {
                advance_relayed: true,
                advance_enqueued: true
            }
        );
    }

    /// #826 P1 #2 regression guard: the dedupe-floor in the watcher's
    /// duplicate-relay guard must be `max(last_relayed_offset,
    /// last_enqueued_offset)`. After a notify-path enqueue advances ONLY the
    /// enqueue watermark, a later tick that re-reads the same tmux range
    /// must still be suppressed — otherwise we'd double-enqueue the same
    /// response while the outbox worker was still delivering the first copy.
    #[test]
    fn enqueued_offset_gates_dedupe_even_without_relayed_advance() {
        // Mirror the max()-dedupe logic from the watcher loop (kept inline
        // there for hot-path performance — this test pins the invariant).
        fn dedupe_floor(relayed: Option<u64>, enqueued: Option<u64>) -> Option<u64> {
            match (relayed, enqueued) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (Some(a), None) | (None, Some(a)) => Some(a),
                (None, None) => None,
            }
        }

        // Enqueue advanced but relayed did not — dedupe still protects
        // against re-emit of the same start offset.
        assert_eq!(
            dedupe_floor(/*relayed*/ None, /*enqueued*/ Some(4096)),
            Some(4096),
            "enqueue-only advance must still guard the dedupe floor"
        );

        // Relayed leapfrogs a stale enqueue marker (e.g. a genuine
        // foreground delivery arrived later) — floor follows the higher
        // watermark.
        assert_eq!(dedupe_floor(Some(8192), Some(4096)), Some(8192));

        // Both absent — no floor, watcher may relay freely.
        assert_eq!(dedupe_floor(None, None), None);
    }
}
