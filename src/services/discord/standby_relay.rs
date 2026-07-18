//! Phase 5.3 of intake-node-routing (issue #2011): standalone JSONL → Discord
//! relay task for cluster-standby nodes.
//!
//! On the leader, the tmux watcher (`tmux_watcher.rs`) handles streaming
//! agent output to Discord. The watcher's relay path has many gateway-coupled
//! assumptions (cached cache, inflight reconciliation, monitor-auto-turn
//! claims, recent_stop suppression, paused/pause_epoch coordination, etc.)
//! that don't hold on cluster-standby nodes. Phase 5.2 made the watcher
//! *start* on standby via `serenity_http_or_token_fallback()`, but the watcher's
//! relay step still doesn't fire on standby in production (verified
//! 2026-05-10 with channel `1475086789696946196` outbox_id=2: response sat in
//! tmux indefinitely while the placeholder froze at "응답 처리 중").
//!
//! Phase 5.3 takes the simpler, more robust path: when on standby, skip the
//! watcher entirely and run this self-contained relay loop instead. It
//! polls the agent's JSONL output file for the `{"type":"result"}` event,
//! extracts the assistant response, and posts it to Discord via REST
//! (replacing the bridge-allocated placeholder when one is known, otherwise
//! sending a new channel message). No reliance on cached_serenity_ctx,
//! inflight reconciliation, or any of the watcher's leader-only state
//! machinery.
//!
//! Leader path is unchanged.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use serenity::model::id::{ChannelId, MessageId};

use super::SharedData;
use super::formatting::{self, ReplaceLongMessageOutcome};
use super::inflight::{
    GuardedClearOutcome, InflightSignal, InflightTurnIdentity, InflightTurnState, RelayOwnerKind,
};
use super::outbound::turn_output_controller as toc;
use super::placeholder_controller::{PlaceholderKey, PlaceholderLifecycle};
use crate::services::provider::ProviderKind;

/// #3089 A3/#3998 S1-f2: pure short-replace cut-over decision.
/// Routes the standby short-replace branch onto the unified controller when the
/// post-format body is non-empty. The `!formatted.is_empty()` half is
/// LOAD-BEARING and single-sourced here: legacy
/// `replace_long_message_raw_with_outcome` treats a zero-chunk (empty) body as
/// `EditedOriginal` → committed → **true** (no network), whereas the controller
/// short-circuits an empty body to `Skipped` → **false**. Dropping the empty-body
/// exclusion would wrongly flip empty bodies true→false, so it is pinned by
/// `standby_short_replace_should_cutover_pins_both_conditions`. Mirrors A2b's
/// `sink_guard_lease_range` extraction.
fn standby_short_replace_should_cutover(formatted: &str) -> bool {
    !formatted.is_empty()
}

const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// #2448 graduation: the 900s (15min) cap was the heuristic stop signal —
/// "after this long the primary turn is presumed dead". Now that
/// `CompletionGuard` broadcasts `InflightSignal::Completed` explicitly,
/// the wall-clock deadline is demoted to a pure safety backstop: it only
/// fires when neither the broadcast (same-node) nor the on-disk inflight
/// poll (cross-node) ever observe completion. 30 min comfortably covers
/// any sane long-running turn.
// #3034: canonical backstop value retained as documentation; current callers
// pass an explicit `timeout`, so no live read of this default yet.
#[allow(dead_code)]
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(1800);
const MAX_FILE_BYTES_PER_TICK: u64 = 1_048_576; // 1 MiB safety cap
const INFLIGHT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
const COMPLETED_SIGNAL_DRAIN_GRACE: Duration = Duration::from_secs(5);

#[derive(Clone, Debug)]
pub(in crate::services::discord) struct StandbyRelayTurnBinding {
    identity: InflightTurnIdentity,
    dispatch_id: Option<String>,
    session_key: Option<String>,
    turn_start_offset: Option<u64>,
}

impl StandbyRelayTurnBinding {
    pub(in crate::services::discord) fn from_state(state: &InflightTurnState) -> Self {
        Self {
            identity: InflightTurnIdentity::from_state(state),
            dispatch_id: state.dispatch_id.clone(),
            session_key: state.session_key.clone(),
            turn_start_offset: state.turn_start_offset,
        }
    }

    fn turn_id(&self, channel_id: ChannelId) -> Option<String> {
        if self.identity.user_msg_id == 0 {
            return None;
        }
        Some(format!(
            "discord:{}:{}",
            channel_id.get(),
            self.identity.user_msg_id
        ))
    }
}

/// Spawned per-turn on cluster-standby nodes. Returns when:
/// - `cancel` or `shared.restart.shutting_down` flips to true,
/// - the JSONL emits a `{"type":"result"}` event and we deliver the response,
/// - or `timeout` elapses.
pub(super) async fn run_standby_relay(
    http: Arc<serenity::http::Http>,
    channel_id: ChannelId,
    placeholder_msg_id: Option<MessageId>,
    output_path: String,
    turn_binding: StandbyRelayTurnBinding,
    start_offset: u64,
    cancel: Arc<AtomicBool>,
    shared: Arc<SharedData>,
    provider: ProviderKind,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    let mut current_offset = start_offset;
    let mut last_inflight_heartbeat = Instant::now();
    // Buffer raw bytes for incomplete trailing lines across reads. Decoding
    // only complete JSONL lines avoids replacing split UTF-8 scalars.
    let mut tail_buf: Vec<u8> = Vec::new();
    let mut tail_start_offset = start_offset;
    let mut pending_result_text: Option<String> = None;
    let mut pending_result_retry_offset: Option<u64> = None;
    let mut completed_signal_drain_until: Option<Instant> = None;
    // #2448: subscribe BEFORE the first poll tick so a `Completed` broadcast
    // emitted while we are setting up is queued instead of lost. Lag is
    // expected on heavy load — `RecvError::Lagged` is treated as "you may
    // have missed an exit-eligible signal" and triggers a force-poll +
    // state re-fetch on the next tick (matches the issue pitfalls section).
    let mut inflight_signals = shared.inflight_signals.subscribe();
    let ts_start = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts_start}] 👁 standby_relay started for channel {} from offset {} (placeholder={:?})",
        channel_id.get(),
        start_offset,
        placeholder_msg_id.map(|m| m.get())
    );

    loop {
        if cancel.load(Ordering::Relaxed) || shared.restart.shutting_down.load(Ordering::Relaxed) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 standby_relay cancelled for channel {} (offset={})",
                channel_id.get(),
                current_offset
            );
            return;
        }
        if Instant::now() > deadline {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ standby_relay deadline reached for channel {} (offset={}, no completion signal or result event observed in {}s — safety backstop)",
                channel_id.get(),
                current_offset,
                timeout.as_secs()
            );
            return;
        }
        if standby_completed_drain_expired(
            pending_result_text.as_deref(),
            completed_signal_drain_until,
            Instant::now(),
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 standby_relay exit after Completed drain grace for channel {} (offset={})",
                channel_id.get(),
                current_offset
            );
            return;
        }
        // #2448: drain the broadcast queue NON-blocking before each poll
        // tick. If we observe `Completed { channel_id: self }` before this
        // task has parsed a result, keep polling for a short grace period.
        // The result line may already be on disk but not yet consumed by this
        // relay; exiting immediately leaves the placeholder without a final
        // response.
        loop {
            use tokio::sync::broadcast::error::TryRecvError;
            match inflight_signals.try_recv() {
                Ok(InflightSignal::Completed { channel_id: c, .. })
                    if c == channel_id.get()
                        && standby_completed_signal_starts_drain(
                            pending_result_text.as_deref(),
                        ) =>
                {
                    if completed_signal_drain_until.is_none() {
                        completed_signal_drain_until =
                            Some(Instant::now() + COMPLETED_SIGNAL_DRAIN_GRACE);
                    }
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 👁 standby_relay observed InflightSignal::Completed for channel {}; draining JSONL for {:?} before exit (offset={})",
                        channel_id.get(),
                        COMPLETED_SIGNAL_DRAIN_GRACE,
                        current_offset
                    );
                    break;
                }
                Ok(InflightSignal::Completed { channel_id: c, .. }) if c == channel_id.get() => {
                    continue;
                }
                Ok(_) => continue, // other channels — ignore
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Lagged(_)) => {
                    // Codex review HIGH: a bursty publisher can saturate the
                    // 256-slot broadcast and Lag us before we observe our
                    // own `Completed`. The previous `break` here meant we
                    // silently fell through to the 1800s backstop — the
                    // exact regression #2448 was meant to close. Recheck
                    // the on-disk inflight authoritatively: if terminal
                    // (file gone or pointing at a different output), the
                    // turn already completed and we should exit now.
                    if pending_result_text.is_none()
                        && super::inflight::load_inflight_state(&provider, channel_id.get())
                            .map(|state| {
                                !standby_inflight_matches(&state, &output_path, placeholder_msg_id)
                            })
                            .unwrap_or(true)
                    {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 👁 standby_relay exit on broadcast Lag + terminal inflight for channel {} (offset={})",
                            channel_id.get(),
                            current_offset
                        );
                        return;
                    }
                    break;
                }
                Err(TryRecvError::Closed) => break, // sender dropped — keep polling
            }
        }
        if last_inflight_heartbeat.elapsed() >= INFLIGHT_HEARTBEAT_INTERVAL {
            refresh_standby_inflight_heartbeat(
                &provider,
                channel_id,
                &output_path,
                placeholder_msg_id,
                &turn_binding,
                standby_heartbeat_offset(
                    current_offset,
                    pending_result_retry_offset,
                    (!tail_buf.is_empty()).then_some(tail_start_offset),
                ),
            );
            last_inflight_heartbeat = Instant::now();
        }

        if let Some(result_text) = pending_result_text.as_deref() {
            let delivered = deliver_response(
                &http,
                channel_id,
                placeholder_msg_id,
                &shared,
                &provider,
                &turn_binding,
                result_text,
            )
            .await;
            if delivered {
                complete_standby_inflight_state(
                    &provider,
                    channel_id,
                    &output_path,
                    placeholder_msg_id,
                    &turn_binding,
                    result_text,
                    current_offset,
                );
                return;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }

        let file_size = match std::fs::metadata(&output_path) {
            Ok(meta) => meta.len(),
            Err(_) => {
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };
        if file_size <= current_offset {
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }

        let read_from = current_offset;
        let read_to = (read_from + MAX_FILE_BYTES_PER_TICK).min(file_size);
        let new_chunk = match read_file_range(&output_path, read_from, read_to) {
            Ok(s) => s,
            Err(_) => {
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };
        current_offset = read_to;

        let mut found_result_text = None;
        let decoded_lines = standby_complete_lines_from_chunk(
            &mut tail_buf,
            &mut tail_start_offset,
            read_from,
            new_chunk,
        );
        for (line_start, line) in decoded_lines.lines {
            if line.trim().is_empty() {
                continue;
            }
            if let Some(result_text) = extract_result_text(&line) {
                pending_result_retry_offset = Some(
                    decoded_lines
                        .stitched_start_offset
                        .saturating_add(line_start as u64),
                );
                found_result_text = Some(result_text);
                break;
            }
        }
        if let Some(result_text) = found_result_text {
            pending_result_text = Some(result_text);
            completed_signal_drain_until = None;
            continue;
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn read_file_range(path: &str, start: u64, end: u64) -> std::io::Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let len = end.saturating_sub(start) as usize;
    let mut buf = vec![0u8; len];
    let read = file.read(&mut buf)?;
    buf.truncate(read);
    Ok(buf)
}

#[derive(Debug)]
struct StandbyDecodedLines {
    stitched_start_offset: u64,
    lines: Vec<(usize, String)>,
}

fn standby_complete_lines_from_chunk(
    tail_buf: &mut Vec<u8>,
    tail_start_offset: &mut u64,
    read_from: u64,
    new_chunk: Vec<u8>,
) -> StandbyDecodedLines {
    let stitched_start_offset = if tail_buf.is_empty() {
        read_from
    } else {
        *tail_start_offset
    };
    let stitched = if tail_buf.is_empty() {
        new_chunk
    } else {
        let mut s = std::mem::take(tail_buf);
        s.extend_from_slice(&new_chunk);
        s
    };

    let mut lines = Vec::new();
    let mut last_complete_end = 0usize;
    for (idx, b) in stitched.iter().enumerate() {
        if *b == b'\n' {
            let line_start = last_complete_end;
            let line_bytes = &stitched[line_start..idx];
            let line = match std::str::from_utf8(line_bytes) {
                Ok(line) => line.to_string(),
                Err(_) => String::from_utf8_lossy(line_bytes).into_owned(),
            };
            lines.push((line_start, line));
            last_complete_end = idx + 1;
        }
    }
    if last_complete_end < stitched.len() {
        tail_buf.extend_from_slice(&stitched[last_complete_end..]);
        *tail_start_offset = stitched_start_offset.saturating_add(last_complete_end as u64);
    } else {
        tail_buf.clear();
        *tail_start_offset = stitched_start_offset.saturating_add(last_complete_end as u64);
    }

    StandbyDecodedLines {
        stitched_start_offset,
        lines,
    }
}

fn extract_result_text(line: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(line.trim()).ok()?;
    if parsed.get("type").and_then(Value::as_str) != Some("result") {
        return None;
    }
    let result_text = parsed.get("result").and_then(Value::as_str)?;
    let cleaned = super::response_sanitizer::strip_leading_tui_response_chrome(result_text);
    if cleaned.trim().is_empty() {
        return None;
    }
    Some(cleaned)
}

fn standby_inflight_matches(
    state: &InflightTurnState,
    output_path: &str,
    placeholder_msg_id: Option<MessageId>,
) -> bool {
    if state.output_path.as_deref() != Some(output_path) {
        return false;
    }
    if let Some(msg_id) = placeholder_msg_id {
        state.current_msg_id == msg_id.get()
    } else {
        true
    }
}

fn standby_completed_signal_starts_drain(pending_result_text: Option<&str>) -> bool {
    pending_result_text.is_none()
}

fn standby_completed_drain_expired(
    pending_result_text: Option<&str>,
    drain_until: Option<Instant>,
    now: Instant,
) -> bool {
    pending_result_text.is_none() && drain_until.is_some_and(|until| now >= until)
}

fn standby_should_send_new_chunks_for_placeholder(response_text: &str) -> bool {
    response_text.len() > super::DISCORD_MSG_LIMIT
}

fn standby_heartbeat_offset(
    current_offset: u64,
    pending_result_retry_offset: Option<u64>,
    incomplete_tail_start_offset: Option<u64>,
) -> u64 {
    pending_result_retry_offset
        .or(incomplete_tail_start_offset)
        .unwrap_or(current_offset)
}

fn refresh_standby_inflight_heartbeat(
    provider: &ProviderKind,
    channel_id: ChannelId,
    output_path: &str,
    placeholder_msg_id: Option<MessageId>,
    turn_binding: &StandbyRelayTurnBinding,
    current_offset: u64,
) {
    let expected_current_msg_id = placeholder_msg_id.map(|msg| msg.get());
    let _ = super::inflight::refresh_inflight_last_offset_if_matches_identity(
        provider,
        channel_id.get(),
        &turn_binding.identity,
        turn_binding.turn_start_offset,
        output_path,
        expected_current_msg_id,
        current_offset,
        super::inflight::RelayOwnerKind::StandbyRelay,
    );
}

fn clear_outcome_label(outcome: GuardedClearOutcome) -> &'static str {
    match outcome {
        GuardedClearOutcome::Cleared => "cleared",
        GuardedClearOutcome::UserMsgMismatch => "user_msg_mismatch",
        GuardedClearOutcome::PlannedRestartSkipped => "planned_restart_skipped",
        GuardedClearOutcome::RebindOriginSkipped => "rebind_origin_skipped",
        GuardedClearOutcome::Missing => "missing",
        GuardedClearOutcome::IoError => "io_error",
    }
}

fn emit_standby_completion_event(
    provider: &ProviderKind,
    channel_id: ChannelId,
    output_path: &str,
    placeholder_msg_id: Option<MessageId>,
    turn_binding: &StandbyRelayTurnBinding,
    outcome_label: &str,
    response_text: &str,
    current_offset: u64,
    mirrored_response: bool,
) {
    let turn_id = turn_binding.turn_id(channel_id);
    crate::services::observability::emit_inflight_lifecycle_event(
        provider.as_str(),
        channel_id.get(),
        turn_binding.dispatch_id.as_deref(),
        turn_binding.session_key.as_deref(),
        turn_id.as_deref(),
        "cleared_by_standby_relay",
        serde_json::json!({
            "outcome": outcome_label,
            "expected_user_msg_id": turn_binding.identity.user_msg_id,
            "expected_started_at": turn_binding.identity.started_at.as_str(),
            "expected_tmux_session_name": turn_binding.identity.tmux_session_name.as_deref(),
            "expected_turn_start_offset": turn_binding.turn_start_offset,
            "placeholder_msg_id": placeholder_msg_id.map(|msg| msg.get()),
            "output_path": output_path,
            "current_offset": current_offset,
            "response_len": response_text.len(),
            "mirrored_response_before_clear": mirrored_response,
        }),
    );
}

fn complete_standby_inflight_state(
    provider: &ProviderKind,
    channel_id: ChannelId,
    output_path: &str,
    placeholder_msg_id: Option<MessageId>,
    turn_binding: &StandbyRelayTurnBinding,
    response_text: &str,
    current_offset: u64,
) -> GuardedClearOutcome {
    let Some(state) = super::inflight::load_inflight_state(provider, channel_id.get()) else {
        emit_standby_completion_event(
            provider,
            channel_id,
            output_path,
            placeholder_msg_id,
            turn_binding,
            clear_outcome_label(GuardedClearOutcome::Missing),
            response_text,
            current_offset,
            false,
        );
        return GuardedClearOutcome::Missing;
    };
    if !standby_inflight_matches(&state, output_path, placeholder_msg_id) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            output_path = output_path,
            placeholder_msg_id = placeholder_msg_id.map(|msg| msg.get()),
            "[{ts}] ⚠ standby_relay skipped inflight cleanup because the on-disk row no longer matches this relay"
        );
        emit_standby_completion_event(
            provider,
            channel_id,
            output_path,
            placeholder_msg_id,
            turn_binding,
            "precheck_mismatch",
            response_text,
            current_offset,
            false,
        );
        return GuardedClearOutcome::UserMsgMismatch;
    }

    let user_msg_id = turn_binding.identity.user_msg_id;
    let (outcome, mirrored_response) =
        super::inflight::clear_inflight_state_if_matches_identity_after_delivery(
            provider,
            channel_id.get(),
            &turn_binding.identity,
            turn_binding.turn_start_offset,
            response_text,
            response_text.len(),
            current_offset,
        );
    let outcome_label = clear_outcome_label(outcome);
    emit_standby_completion_event(
        provider,
        channel_id,
        output_path,
        placeholder_msg_id,
        turn_binding,
        outcome_label,
        response_text,
        current_offset,
        mirrored_response,
    );

    match outcome {
        GuardedClearOutcome::Cleared | GuardedClearOutcome::Missing => {
            tracing::debug!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                outcome = outcome_label,
                "standby_relay completed delegated inflight cleanup"
            );
        }
        GuardedClearOutcome::UserMsgMismatch => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                user_msg_id = user_msg_id,
                "[{ts}] ⚠ standby_relay did not clear inflight because the guarded identity no longer matches"
            );
        }
        GuardedClearOutcome::PlannedRestartSkipped | GuardedClearOutcome::RebindOriginSkipped => {
            tracing::debug!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                outcome = outcome_label,
                "standby_relay preserved inflight row after delegated completion"
            );
        }
        GuardedClearOutcome::IoError => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                "standby_relay failed to clear inflight after delegated completion; sweeper will see mirrored response state"
            );
        }
    }
    outcome
}

/// #3089 A3: standby short-replace via the turn-output controller, behaviourally
/// equal to legacy `replace_long_message_raw_with_outcome`. Standby is
/// TRANSPORT-ONLY — it never held a `DeliveryLeaseCell`, never advanced an
/// offset, and ran no heartbeat — so this uses [`toc::NoLease`] (acquire always
/// fails → `ProceedMarkerless`: no lease held, `heartbeat = None`) with
/// `advance = None` (no offset authority → `commit_and_finalize` treats a
/// confirmed transport as `advanced = true` → `Delivered`). #2757 is preserved
/// via `PreserveAlways` (the original placeholder is NEVER deleted on fallback —
/// the legacy short-replace path never deletes either; only the deferred
/// long-chunk branch does). `CommitOnFallback` mirrors legacy returning true on
/// `SentFallbackAfterEditFailure`. `Replace { Active }` keeps `post_send_finalize`
/// a no-op (the replace IS the edit). `gateway` is a seam: the live path builds
/// the real `DiscordGateway`; tests inject a fake driving the REAL controller.
///
/// Legacy bool mapping (must reproduce legacy EXACTLY):
/// - `EditedOriginal` → `Delivered` → `true`.
/// - `SentFallbackAfterEditFailure` (CommitOnFallback) → `Delivered` → `true`.
/// - `PartialContinuationFailure` → `Unknown` (no advance, I2) → `false`.
/// - transport `Err` → `Unknown` (`transient_or_unknown`) → `false`.
///
/// `NotDelivered` cannot arise here (no advance callback ⇒ `advanced` always
/// `true`), but is mapped to `false` for completeness; `Transient`/`Skipped`
/// likewise map to `false`.
async fn deliver_short_replace_via_controller<G: super::gateway::TurnGateway + ?Sized>(
    gateway: &G,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    msg_id: MessageId,
    formatted: &str,
) -> bool {
    // Standby has no turn binding here; the `TurnKey` is COSMETIC on the
    // markerless + `NoLease` path (acquire always fails ⇒ the lease/turn identity
    // never gates anything). `user_msg_id = 0` is a defensible degenerate value.
    let turn =
        super::turn_finalizer::TurnKey::new(channel_id, 0, shared.restart.current_generation);
    let no_lease = toc::NoLease;
    let outcome = toc::deliver_turn_output(
        gateway,
        toc::TurnOutputCtx {
            turn,
            lease_key: None,
            owner: RelayOwnerKind::StandbyRelay,
            // Reuse `LeaseHolder::Sink` (its doc reads "The standby / output sink
            // relay") — cosmetic on the markerless path; no `Standby` variant
            // exists and `mod.rs` is a frozen baseline, so adding one is avoided.
            holder: super::LeaseHolder::Sink,
            lease: &no_lease,
            channel_id,
            placeholder_controller: &shared.ui.placeholder_controller,
            placeholder: toc::PlaceholderSlot::Active {
                message_id: msg_id,
                key: PlaceholderKey {
                    provider: provider.clone(),
                    channel_id,
                    message_id: msg_id,
                },
            },
            body: formatted,
            // Standby has no offsets and never commits (NoLease) — the range is
            // inert. A degenerate `(0, 0)` is defensible: nothing reads it.
            send_range: (0, 0),
            // `Replace { Active }` → non-terminal → `post_send_finalize` no-ops,
            // matching the legacy edit-in-place.
            plan: toc::OutputPlan::Replace {
                lifecycle: PlaceholderLifecycle::Active,
            },
            // #2757: never delete the original on edit-fail fallback.
            edit_fail_policy: toc::EditFailPlaceholderPolicy::PreserveAlways,
            // Standby returns true on fallback (`standby_relay.rs` legacy).
            fallback_commit_policy: toc::FallbackCommitPolicy::CommitOnFallback,
            // Transport-only: a (failed) acquire still POSTs, markerless.
            acquire_failure_mode: toc::AcquireFailureMode::ProceedMarkerless,
            // No offset authority → unconditional advance (A1 semantics).
            advance: None,
            // No heartbeat (no lease to renew).
            heartbeat: None,
        },
    )
    .await;

    let ts = chrono::Local::now().format("%H:%M:%S");
    match outcome {
        // Confirmed POST (EditedOriginal OR #2757 CommitOnFallback): the controller
        // ran the (no-op markerless) commit; legacy returned true for both.
        toc::DeliveryOutcome::Delivered { .. } => {
            tracing::info!(
                "  [{ts}] 👁 standby_relay ✓ delivered terminal response via controller channel {} msg {} (#3089 A3)",
                channel_id.get(),
                msg_id.get()
            );
            true
        }
        // PartialContinuationFailure → Unknown (I2, no advance); transport Err →
        // Unknown; NotDelivered/Transient/Skipped → not confirmed. Legacy → false.
        _ => {
            tracing::warn!(
                "  [{ts}] ⚠ standby_relay controller delivery not confirmed for channel {} msg {} (#3089 A3)",
                channel_id.get(),
                msg_id.get()
            );
            false
        }
    }
}

async fn deliver_response(
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    placeholder_msg_id: Option<MessageId>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    turn_binding: &StandbyRelayTurnBinding,
    response_text: &str,
) -> bool {
    let formatted = if shared.ui.status_panel_v2_enabled {
        formatting::format_for_discord_with_status_panel(response_text, provider)
    } else {
        formatting::format_for_discord_with_provider(response_text, provider)
    };
    let formatted = super::session_banner::with_discord_turn_session_banner_identity_prefix(
        shared,
        channel_id,
        provider,
        turn_binding.identity.user_msg_id,
        Some(&turn_binding.identity.started_at),
        turn_binding.turn_start_offset,
        true,
        formatted,
    );
    let chars = formatted.chars().count();

    match placeholder_msg_id {
        Some(msg_id) => {
            if standby_should_send_new_chunks_for_placeholder(&formatted) {
                if let Err(error) = formatting::send_long_message_raw_with_rollback(
                    http, channel_id, msg_id, &formatted, shared,
                )
                .await
                {
                    let error = error.to_string();
                    let display_error =
                        super::replace_outcome_policy::strip_watcher_send_failure_class_marker(
                            &error,
                        );
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ standby_relay long send failed for channel {}: {display_error}",
                        channel_id.get()
                    );
                    return false;
                }
                let _ = super::http::delete_channel_message(http, channel_id, msg_id).await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 👁 standby_relay ✓ delivered long terminal response as ordered new chunks channel {} msg {} ({} chars)",
                    channel_id.get(),
                    msg_id.get(),
                    chars
                );
                return true;
            }
            // #3089 A3/#3998 S1-f2: route the structurally eligible
            // short-replace branch through the unified controller. The cut-over
            // decision, including the load-bearing non-empty-body exclusion,
            // lives in the single-sourced pure fn
            // `standby_short_replace_should_cutover` (see its doc comment for the
            // empty-body true→false divergence the gate guards against).
            if standby_short_replace_should_cutover(&formatted) {
                let gateway = super::gateway::DiscordGateway::new(
                    http.clone(),
                    shared.clone(),
                    provider.clone(),
                    None,
                );
                return deliver_short_replace_via_controller(
                    &gateway, shared, provider, channel_id, msg_id, &formatted,
                )
                .await;
            }
            let outcome = formatting::replace_long_message_raw_with_outcome(
                // #3805 P1: standby relay does not append a completion footer, so
                // the last-chunk anchor is unused here.
                http, channel_id, msg_id, &formatted, shared, &mut None,
            )
            .await;
            // #3089 A0: the "delivered?" return is sourced from the shared
            // disposition policy (committed = EditedOriginal | preserved
            // fallback) instead of per-arm literals.
            let committed =
                crate::services::discord::replace_outcome_policy::relay_outcome_is_committed(
                    crate::services::discord::replace_outcome_policy::ReplaceOutcomeKind::of(
                        &outcome,
                    ),
                );
            let ts = chrono::Local::now().format("%H:%M:%S");
            match outcome {
                Ok(ReplaceLongMessageOutcome::EditedOriginal) => {
                    tracing::info!(
                        "  [{ts}] 👁 standby_relay ✓ delivered terminal response (edit) channel {} msg {} ({} chars)",
                        channel_id.get(),
                        msg_id.get(),
                        chars
                    );
                    committed
                }
                Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                    edit_error, ..
                }) => {
                    // Mirror session_relay_sink #2757: never delete the original
                    // msg_id after fallback delivery — by the time the edit fails
                    // it can already be a live response card, not a disposable
                    // placeholder. The shared disposition policy (#3089 A0) pins
                    // this preserve-and-deliver decision against a cutover.
                    debug_assert!(
                        !crate::services::discord::replace_outcome_policy::edit_fail_fallback_disposition()
                            .deletes_original(),
                        "#2757: must preserve original"
                    );
                    tracing::warn!(
                        "  [{ts}] 👁 standby_relay ✓ delivered terminal response via fallback; preserving original msg {} in channel {} ({} chars, edit_error={})",
                        msg_id.get(),
                        channel_id.get(),
                        chars,
                        edit_error
                    );
                    committed
                }
                Ok(ReplaceLongMessageOutcome::PartialContinuationFailure {
                    sent_chunks,
                    total_chunks,
                    failed_chunk_index,
                    sent_continuation_message_ids,
                    cleanup_errors,
                    error,
                }) => {
                    let display_error =
                        super::replace_outcome_policy::strip_watcher_send_failure_class_marker(
                            &error,
                        );
                    tracing::warn!(
                        "  [{ts}] ⚠ standby_relay partially delivered terminal response in channel {} msg {} (sent_chunks={}, total_chunks={}, failed_chunk_index={}, cleaned_continuations={}, cleanup_errors={}, error={})",
                        channel_id.get(),
                        msg_id.get(),
                        sent_chunks,
                        total_chunks,
                        failed_chunk_index,
                        sent_continuation_message_ids.len(),
                        cleanup_errors.len(),
                        display_error
                    );
                    committed
                }
                Err(e) => {
                    let error = e.to_string();
                    let display_error =
                        super::replace_outcome_policy::strip_watcher_send_failure_class_marker(
                            &error,
                        );
                    tracing::warn!(
                        "  [{ts}] ⚠ standby_relay edit failed for channel {} msg {}: {display_error}",
                        channel_id.get(),
                        msg_id.get()
                    );
                    committed
                }
            }
        }
        None => {
            let result =
                formatting::send_long_message_raw(http, channel_id, &formatted, shared).await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            match result {
                Ok(()) => {
                    tracing::info!(
                        "  [{ts}] 👁 standby_relay ✓ delivered terminal response (new message) channel {} ({} chars)",
                        channel_id.get(),
                        chars
                    );
                    true
                }
                Err(e) => {
                    tracing::warn!(
                        "  [{ts}] ⚠ standby_relay send failed for channel {}: {e}",
                        channel_id.get()
                    );
                    false
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_result_text_returns_none_for_non_result_lines() {
        let line = r#"{"type":"assistant","message":{"content":[{"text":"hi"}]}}"#;
        assert!(extract_result_text(line).is_none());
    }

    #[test]
    fn extract_result_text_returns_text_for_result_subtype_success() {
        let line = r#"{"type":"result","subtype":"success","result":"hello"}"#;
        assert_eq!(extract_result_text(line).as_deref(), Some("hello"));
    }

    #[test]
    fn extract_result_text_strips_tui_no_response_chrome() {
        let line = "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"No response requested.\\n\\nhello\"}";
        assert_eq!(extract_result_text(line).as_deref(), Some("hello"));
        let empty = r#"{"type":"result","subtype":"success","result":"No response requested."}"#;
        assert!(extract_result_text(empty).is_none());
    }

    #[test]
    fn extract_result_text_skips_empty_result() {
        let line = r#"{"type":"result","subtype":"success","result":"   "}"#;
        assert!(extract_result_text(line).is_none());
    }

    #[test]
    fn extract_result_text_handles_invalid_json() {
        assert!(extract_result_text("not json").is_none());
        assert!(extract_result_text("").is_none());
    }

    #[test]
    fn standby_inflight_match_requires_same_output_and_placeholder() {
        with_isolated_runtime_root(|| {
            let mut state = InflightTurnState::new(
                ProviderKind::Codex,
                1234,
                None,
                42,
                100,
                5678,
                "test".to_string(),
                None,
                Some("tmux".to_string()),
                Some("/tmp/out.jsonl".to_string()),
                None,
                0,
            );

            assert!(standby_inflight_matches(
                &state,
                "/tmp/out.jsonl",
                Some(MessageId::new(5678)),
            ));
            assert!(!standby_inflight_matches(
                &state,
                "/tmp/other.jsonl",
                Some(MessageId::new(5678)),
            ));
            assert!(!standby_inflight_matches(
                &state,
                "/tmp/out.jsonl",
                Some(MessageId::new(9999)),
            ));

            state.current_msg_id = 0;
            assert!(standby_inflight_matches(&state, "/tmp/out.jsonl", None));
        });
    }

    /// #2448 acceptance — confirm the relay-side broadcast filter:
    /// `InflightSignal::Completed` for a NON-matching channel must be
    /// ignored, while one for the OWN channel must short-circuit. We
    /// exercise the filter shape inline because the live relay loop
    /// requires a `serenity::http::Http` fixture not available in this
    /// test scope.
    #[test]
    fn inflight_signal_filter_matches_own_channel_only() {
        use super::InflightSignal;
        let own = 11_111u64;
        let other = 22_222u64;

        let matches = |sig: &InflightSignal| match sig {
            InflightSignal::Completed { channel_id, .. } => *channel_id == own,
        };

        assert!(matches(&InflightSignal::Completed {
            channel_id: own,
            turn_id: 1
        }));
        assert!(!matches(&InflightSignal::Completed {
            channel_id: other,
            turn_id: 1
        }));
    }

    #[test]
    fn completed_signal_starts_drain_before_exit_when_result_not_seen() {
        let now = Instant::now();
        assert!(standby_completed_signal_starts_drain(None));
        assert!(!standby_completed_signal_starts_drain(Some(
            "final response"
        )));
        assert!(!standby_completed_drain_expired(None, None, now));
        assert!(!standby_completed_drain_expired(
            Some("final response"),
            Some(now),
            now
        ));
        assert!(!standby_completed_drain_expired(
            None,
            Some(now + Duration::from_millis(1)),
            now
        ));
        assert!(standby_completed_drain_expired(None, Some(now), now));
    }

    #[test]
    fn standby_line_decoder_preserves_utf8_split_across_chunks() {
        let marker = "가나다😀";
        let line = format!(r#"{{"type":"result","result":"{marker}"}}"#);
        let bytes = format!("{line}\n").into_bytes();
        let split = bytes
            .windows("😀".len())
            .position(|window| window == "😀".as_bytes())
            .expect("emoji bytes present")
            + 1;
        let mut tail = Vec::new();
        let mut tail_start = 100;

        let first = standby_complete_lines_from_chunk(
            &mut tail,
            &mut tail_start,
            100,
            bytes[..split].to_vec(),
        );
        assert!(first.lines.is_empty());
        assert!(!tail.is_empty());

        let second = standby_complete_lines_from_chunk(
            &mut tail,
            &mut tail_start,
            100 + split as u64,
            bytes[split..].to_vec(),
        );
        assert_eq!(second.stitched_start_offset, 100);
        assert_eq!(second.lines.len(), 1);
        assert_eq!(
            extract_result_text(&second.lines[0].1).as_deref(),
            Some(marker)
        );
        assert!(tail.is_empty());
    }

    #[test]
    fn heartbeat_offset_rewinds_to_pending_result_until_delivery_commits() {
        assert_eq!(standby_heartbeat_offset(250, None, None), 250);
        assert_eq!(standby_heartbeat_offset(250, Some(120), None), 120);
        assert_eq!(standby_heartbeat_offset(250, None, Some(180)), 180);
        assert_eq!(standby_heartbeat_offset(250, Some(120), Some(180)), 120);
    }

    #[test]
    fn placeholder_long_terminal_delivery_uses_ordered_new_chunks() {
        let body = format!(
            "[E2E:E15:BEGIN]\n{}\n[E2E:E15:MID]\n{}\n[E2E:E15:END]",
            "E15-LINE-010\n".repeat(90),
            "E15-LINE-150\n".repeat(90)
        );

        assert!(standby_should_send_new_chunks_for_placeholder(&body));
        assert!(!standby_should_send_new_chunks_for_placeholder(
            "[E2E:E15:BEGIN]\nE15-LINE-150\n[E2E:E15:END]"
        ));
    }

    fn with_isolated_runtime_root<F: FnOnce()>(f: F) {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let tmp = tempfile::tempdir().expect("create temp runtime dir for standby relay test");
        unsafe {
            std::env::set_var(
                "AGENTDESK_ROOT_DIR",
                tmp.path().to_str().expect("temp path must be valid utf-8"),
            );
        }
        f();
        unsafe {
            std::env::remove_var("AGENTDESK_ROOT_DIR");
        }
    }

    #[test]
    fn standby_completion_clears_matching_inflight_with_identity_guard() {
        with_isolated_runtime_root(|| {
            let provider = ProviderKind::Codex;
            let channel_id = ChannelId::new(1234);
            let state = InflightTurnState::new(
                provider.clone(),
                channel_id.get(),
                None,
                42,
                100,
                5678,
                "test".to_string(),
                None,
                Some("tmux".to_string()),
                Some("/tmp/out.jsonl".to_string()),
                None,
                12,
            );
            let binding = StandbyRelayTurnBinding::from_state(&state);
            super::super::inflight::save_inflight_state(&state).expect("save inflight");

            let outcome = complete_standby_inflight_state(
                &provider,
                channel_id,
                "/tmp/out.jsonl",
                Some(MessageId::new(5678)),
                &binding,
                "done",
                88,
            );

            assert_eq!(outcome, GuardedClearOutcome::Cleared);
            assert!(
                super::super::inflight::load_inflight_state(&provider, channel_id.get()).is_none()
            );
        });
    }

    #[test]
    fn standby_completion_keeps_mismatched_placeholder_inflight() {
        with_isolated_runtime_root(|| {
            let provider = ProviderKind::Codex;
            let channel_id = ChannelId::new(1235);
            let state = InflightTurnState::new(
                provider.clone(),
                channel_id.get(),
                None,
                42,
                100,
                5678,
                "test".to_string(),
                None,
                Some("tmux".to_string()),
                Some("/tmp/out.jsonl".to_string()),
                None,
                12,
            );
            let binding = StandbyRelayTurnBinding::from_state(&state);
            super::super::inflight::save_inflight_state(&state).expect("save inflight");

            let outcome = complete_standby_inflight_state(
                &provider,
                channel_id,
                "/tmp/out.jsonl",
                Some(MessageId::new(9999)),
                &binding,
                "done",
                88,
            );

            assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch);
            let loaded = super::super::inflight::load_inflight_state(&provider, channel_id.get())
                .expect("mismatched inflight should remain");
            assert_eq!(loaded.current_msg_id, 5678);
            assert!(loaded.full_response.is_empty());
        });
    }

    #[test]
    fn standby_completion_uses_captured_identity_when_fresh_turn_reuses_output() {
        with_isolated_runtime_root(|| {
            let provider = ProviderKind::Codex;
            let channel_id = ChannelId::new(1236);
            let mut old_state = InflightTurnState::new(
                provider.clone(),
                channel_id.get(),
                None,
                42,
                100,
                0,
                "old prompt".to_string(),
                None,
                Some("tmux".to_string()),
                Some("/tmp/out.jsonl".to_string()),
                None,
                12,
            );
            old_state.started_at = "2026-05-17 10:00:00".to_string();
            let binding = StandbyRelayTurnBinding::from_state(&old_state);
            super::super::inflight::save_inflight_state(&old_state).expect("save old inflight");

            let mut fresh_state = InflightTurnState::new(
                provider.clone(),
                channel_id.get(),
                None,
                42,
                101,
                0,
                "fresh prompt".to_string(),
                None,
                Some("tmux".to_string()),
                Some("/tmp/out.jsonl".to_string()),
                None,
                20,
            );
            fresh_state.started_at = "2026-05-17 10:00:05".to_string();
            super::super::inflight::save_inflight_state(&fresh_state)
                .expect("replace with fresh inflight");

            let outcome = complete_standby_inflight_state(
                &provider,
                channel_id,
                "/tmp/out.jsonl",
                None,
                &binding,
                "stale delivered response",
                88,
            );

            assert_eq!(outcome, GuardedClearOutcome::UserMsgMismatch);
            let loaded = super::super::inflight::load_inflight_state(&provider, channel_id.get())
                .expect("fresh inflight should remain");
            assert_eq!(loaded.user_msg_id, 101);
            assert_eq!(loaded.started_at, "2026-05-17 10:00:05");
            assert!(loaded.full_response.is_empty());
            assert_eq!(loaded.response_sent_offset, 0);
        });
    }

    #[test]
    fn standby_heartbeat_uses_captured_identity_when_fresh_turn_reuses_output() {
        with_isolated_runtime_root(|| {
            let provider = ProviderKind::Codex;
            let channel_id = ChannelId::new(1237);
            let mut old_state = InflightTurnState::new(
                provider.clone(),
                channel_id.get(),
                None,
                42,
                100,
                0,
                "old prompt".to_string(),
                None,
                Some("tmux".to_string()),
                Some("/tmp/out.jsonl".to_string()),
                None,
                12,
            );
            old_state.started_at = "2026-05-17 10:00:00".to_string();
            let binding = StandbyRelayTurnBinding::from_state(&old_state);
            super::super::inflight::save_inflight_state(&old_state).expect("save old inflight");

            let mut fresh_state = InflightTurnState::new(
                provider.clone(),
                channel_id.get(),
                None,
                42,
                101,
                0,
                "fresh prompt".to_string(),
                None,
                Some("tmux".to_string()),
                Some("/tmp/out.jsonl".to_string()),
                None,
                20,
            );
            fresh_state.started_at = "2026-05-17 10:00:05".to_string();
            super::super::inflight::save_inflight_state(&fresh_state)
                .expect("replace with fresh inflight");

            refresh_standby_inflight_heartbeat(
                &provider,
                channel_id,
                "/tmp/out.jsonl",
                None,
                &binding,
                88,
            );

            let loaded = super::super::inflight::load_inflight_state(&provider, channel_id.get())
                .expect("fresh inflight should remain");
            assert_eq!(loaded.user_msg_id, 101);
            assert_eq!(loaded.last_offset, 20);
            assert_eq!(loaded.started_at, "2026-05-17 10:00:05");
        });
    }

    /// #2448 acceptance — `tokio::sync::broadcast` capacity 256 must
    /// deliver `Completed` to a subscribed receiver within one recv
    /// iteration. The relay's poll-tick observes the queued message via
    /// `try_recv` on the next iteration, so the broadcast latency is
    /// bounded by the relay's `POLL_INTERVAL` (500ms) ceiling.
    #[tokio::test]
    async fn inflight_signal_broadcast_delivers_to_subscriber() {
        use super::InflightSignal;
        let (tx, mut rx) = tokio::sync::broadcast::channel::<InflightSignal>(256);

        let send_result = tx.send(InflightSignal::Completed {
            channel_id: 42,
            turn_id: 1,
        });
        assert!(send_result.is_ok());

        let received = rx.recv().await.expect("broadcast delivered");
        match received {
            InflightSignal::Completed {
                channel_id,
                turn_id,
            } => {
                assert_eq!(channel_id, 42);
                assert_eq!(turn_id, 1);
            }
        }
    }

    // #3089 A0 — characterization of the standby should-send-new-chunks
    // predicate's EXACT 2000-byte boundary (design §5 A0 item 1). The parent
    // module already proves long-vs-short; this pins the strict-`>` cliff so the
    // four surfaces' shared `len > 2000` boundary is locked. Pinned inline in
    // this `#[cfg(test)] mod tests` block => ZERO production LoC.
    mod a0_characterization_tests {
        use super::super::standby_should_send_new_chunks_for_placeholder as should_send;
        use crate::services::discord::DISCORD_MSG_LIMIT;

        #[test]
        fn a0_standby_predicate_boundary_is_strictly_greater_than_2000() {
            assert_eq!(DISCORD_MSG_LIMIT, 2000, "the shared length limit is 2000");
            assert!(
                !should_send(&"a".repeat(DISCORD_MSG_LIMIT)),
                "exactly 2000 bytes is NOT over-limit (strict >)"
            );
            assert!(
                should_send(&"a".repeat(DISCORD_MSG_LIMIT + 1)),
                "2001 bytes is over-limit => new chunks"
            );
            assert!(!should_send("short"));
        }
    }

    // #3089 A3 — drive the flag-ON production helper
    // (`deliver_short_replace_via_controller`) with a fake `TurnGateway` through
    // the REAL controller path, asserting the legacy bool mapping is reproduced
    // EXACTLY and #2757 is preserved. Mutation-sensitive (see module docstring on
    // the helper): each test fails under a targeted controller mutation.
    mod a3_controller_cutover_tests {
        use super::super::{
            RelayOwnerKind, deliver_short_replace_via_controller,
            standby_short_replace_should_cutover,
        };
        use crate::services::discord::formatting::ReplaceLongMessageOutcome;
        use crate::services::discord::gateway::{GatewayFuture, TurnGateway};
        use crate::services::discord::make_shared_data_for_tests;
        use crate::services::provider::ProviderKind;
        use poise::serenity_prelude::{ChannelId, MessageId};
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Minimal `TurnGateway` fake for the standby short-replace controller path.
        // Only `replace_message_with_outcome` is exercised (`Replace { Active }`
        // transport); `post_send_finalize` no-ops for the non-terminal `Active`
        // lifecycle, so no edit/delete fires. Every other method `panic!`s —
        // reaching one (e.g. a `delete_message` regressing #2757) is a behaviour
        // drift the test catches.
        struct StandbyFakeGateway {
            outcome: ReplaceLongMessageOutcome,
            ok: bool,
            replace_calls: AtomicUsize,
            delete_calls: AtomicUsize,
        }

        impl StandbyFakeGateway {
            fn new(outcome: ReplaceLongMessageOutcome, ok: bool) -> Self {
                Self {
                    outcome,
                    ok,
                    replace_calls: AtomicUsize::new(0),
                    delete_calls: AtomicUsize::new(0),
                }
            }
        }

        impl TurnGateway for StandbyFakeGateway {
            fn replace_message_with_outcome<'a>(
                &'a self,
                _c: ChannelId,
                _m: MessageId,
                _content: &'a str,
            ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
                Box::pin(async move {
                    self.replace_calls.fetch_add(1, Ordering::SeqCst);
                    if self.ok {
                        Ok(self.outcome.clone())
                    } else {
                        Err("fake transport failure".to_string())
                    }
                })
            }
            fn delete_message<'a>(
                &'a self,
                _c: ChannelId,
                _m: MessageId,
            ) -> GatewayFuture<'a, Result<(), String>> {
                // #2757: the standby short-replace path must NEVER delete the
                // original. Record (and still succeed) so a fallback-delete
                // mutation is caught by the `delete_calls == 0` assertions.
                self.delete_calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async move { Ok(()) })
            }
            fn send_message<'a>(
                &'a self,
                _c: ChannelId,
                _x: &'a str,
            ) -> GatewayFuture<'a, Result<MessageId, String>> {
                panic!("standby short-replace path never sends a new message")
            }
            fn edit_message<'a>(
                &'a self,
                _c: ChannelId,
                _m: MessageId,
                _x: &'a str,
            ) -> GatewayFuture<'a, Result<(), String>> {
                panic!("Active lifecycle → post_send_finalize no-op → no edit")
            }

            fn schedule_retry_with_history<'a>(
                &'a self,
                _c: ChannelId,
                _u: MessageId,
                _t: &'a str,
            ) -> GatewayFuture<'a, ()> {
                panic!("unused TurnGateway method on the standby short-replace path")
            }
            fn dispatch_queued_turn<'a>(
                &'a self,
                _c: ChannelId,
                _i: &'a crate::services::discord::Intervention,
                _o: &'a str,
                _h: bool,
            ) -> GatewayFuture<'a, Result<(), String>> {
                panic!("unused TurnGateway method on the standby short-replace path")
            }
            fn validate_live_routing<'a>(
                &'a self,
                _c: ChannelId,
            ) -> GatewayFuture<'a, Result<(), String>> {
                panic!("unused TurnGateway method on the standby short-replace path")
            }
            fn requester_mention(&self) -> Option<String> {
                None
            }
            fn can_chain_locally(&self) -> bool {
                false
            }
            fn bot_owner_provider(&self) -> Option<ProviderKind> {
                None
            }
        }

        fn run(outcome: ReplaceLongMessageOutcome, ok: bool) -> (bool, usize, usize) {
            let shared = make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel = ChannelId::new(9_041);
            let gateway = StandbyFakeGateway::new(outcome, ok);
            let delivered = futures::executor::block_on(deliver_short_replace_via_controller(
                &gateway,
                &shared,
                &provider,
                channel,
                MessageId::new(99),
                "answer",
            ));
            (
                delivered,
                gateway.replace_calls.load(Ordering::SeqCst),
                gateway.delete_calls.load(Ordering::SeqCst),
            )
        }

        #[test]
        fn edited_original_returns_true_and_does_not_delete_original() {
            let (delivered, replace_calls, delete_calls) =
                run(ReplaceLongMessageOutcome::EditedOriginal, true);
            assert!(
                delivered,
                "EditedOriginal → Delivered → true (legacy parity)"
            );
            assert_eq!(replace_calls, 1, "exactly one transport POST");
            assert_eq!(delete_calls, 0, "the original placeholder is never deleted");
        }

        #[test]
        fn fallback_after_edit_failure_returns_true_and_preserves_original() {
            // #2757: SentFallbackAfterEditFailure + CommitOnFallback → Delivered →
            // true, and PreserveAlways must NEVER delete the original. Flipping the
            // fallback policy to NoCommitOnFallback flips this to false.
            let (delivered, replace_calls, delete_calls) = run(
                ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                    edit_error: "edit failed".to_string(),
                    replacement_anchor: None,
                },
                true,
            );
            assert!(
                delivered,
                "fallback (CommitOnFallback) → Delivered → true (legacy parity)"
            );
            assert_eq!(replace_calls, 1, "exactly one transport POST");
            assert_eq!(
                delete_calls, 0,
                "#2757: PreserveAlways must never delete the original on fallback"
            );
        }

        #[test]
        fn partial_continuation_failure_returns_false() {
            // PartialContinuationFailure → Unknown (I2: never advances) → false.
            // A mutation mapping Unknown → true flips this.
            let (delivered, replace_calls, _delete_calls) = run(
                ReplaceLongMessageOutcome::PartialContinuationFailure {
                    sent_chunks: 1,
                    total_chunks: 2,
                    failed_chunk_index: 1,
                    sent_continuation_message_ids: vec![1],
                    cleanup_errors: vec![],
                    error: "mid-stream".to_string(),
                },
                true,
            );
            assert!(
                !delivered,
                "PartialContinuationFailure → Unknown → false (I2)"
            );
            assert_eq!(replace_calls, 1, "exactly one transport POST attempt");
        }

        #[test]
        fn transport_error_returns_false() {
            // transport Err → transient_or_unknown → Unknown → false.
            let (delivered, replace_calls, _delete_calls) =
                run(ReplaceLongMessageOutcome::EditedOriginal, false);
            assert!(!delivered, "transport Err → Unknown → false");
            assert_eq!(replace_calls, 1, "the single POST was attempted and failed");
        }

        // #3089 A3 (review-fix r2): characterizes the empty-body behaviour the
        // production cut-over gate relies on: the controller diverges from legacy
        // on an empty body (controller → Skipped → false; legacy zero-chunk →
        // EditedOriginal → true). NOTE: this test calls
        // `deliver_short_replace_via_controller` DIRECTLY, so it does NOT exercise
        // the production guard at `deliver_response`; it merely proves WHY the
        // guard's `!formatted.is_empty()` half must exist. The guard logic itself
        // is single-sourced in the pure fn `standby_short_replace_should_cutover`
        // and mutation-pinned by
        // `standby_short_replace_should_cutover_pins_both_conditions` below.
        #[test]
        fn empty_body_diverges_from_legacy() {
            // Empty body → controller Skipped → false (legacy would return true),
            // proving the nonempty gate must keep empty bodies on the legacy path.
            let shared = make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel = ChannelId::new(9_042);
            // Gateway whose transport PANICS if reached — the empty-body Skip must
            // short-circuit BEFORE any POST.
            let gateway = StandbyFakeGateway::new(ReplaceLongMessageOutcome::EditedOriginal, true);
            let delivered = futures::executor::block_on(deliver_short_replace_via_controller(
                &gateway,
                &shared,
                &provider,
                channel,
                MessageId::new(99),
                "",
            ));
            assert!(
                !delivered,
                "controller Skips an empty body → false; the cut-over gate keeps empty bodies legacy"
            );
            assert_eq!(
                gateway.replace_calls.load(Ordering::SeqCst),
                0,
                "an empty body never reaches transport on the controller path"
            );
            // owner identity is StandbyRelay (cosmetic, but asserted for honesty).
            assert_eq!(RelayOwnerKind::StandbyRelay.as_str(), "standby_relay");
        }

        // #3089 A3 (review-fix r2 Medium): mutation pin for the PRODUCTION
        // cut-over gate. The guard at `deliver_response` calls the single-sourced
        // pure fn `standby_short_replace_should_cutover`, so the load-bearing
        // `!formatted.is_empty()` literal lives in EXACTLY ONE place. Dropping it
        // makes an empty body wrongly cut over → controller Skip → false instead
        // of legacy zero-chunk EditedOriginal → true.
        #[test]
        fn standby_short_replace_should_cutover_pins_both_conditions() {
            assert!(
                !standby_short_replace_should_cutover(""),
                "empty body MUST stay legacy: controller Skips → false, but legacy \
                 zero-chunk → EditedOriginal → true; cutting over would flip true→false"
            );
            assert!(
                standby_short_replace_should_cutover("x"),
                "the cut-over case is a non-empty body"
            );
        }

        #[test]
        fn none_placeholder_new_message_stays_legacy() {
            let module_src = include_str!("standby_relay.rs");
            let match_marker = format!("match {} {{", "placeholder_msg_id");
            let match_src = module_src
                .split(&match_marker)
                .nth(1)
                .expect("standby placeholder route match");
            let none_marker = format!("{} => {{", "None");
            let none_src = match_src
                .split(&none_marker)
                .nth(1)
                .expect("standby no-placeholder branch");
            let legacy_send_window = &none_src[..none_src
                .find("match result")
                .expect("standby no-placeholder result match")];

            assert!(
                standby_short_replace_should_cutover("answer"),
                "sanity: a non-empty body cuts over only inside the Some(placeholder) branch"
            );
            assert!(
                legacy_send_window.contains(&format!("formatting::{}(", "send_long_message_raw")),
                "A3 placeholder=None must keep the legacy fresh-send transport"
            );
            assert!(
                !legacy_send_window.contains("deliver_short_replace_via_controller"),
                "A3 placeholder=None must not route through the short-replace controller"
            );
            assert!(
                !legacy_send_window.contains("deliver_turn_output"),
                "A3 placeholder=None remains an anchor-less fresh-send legacy branch"
            );
        }
    }
}
