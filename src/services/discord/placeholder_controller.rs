//! Placeholder lifecycle controller (#1255).
//!
//! Centralises the live-turn placeholder card lifecycle so every entry point
//! that wants to drive the live placeholder -> terminal transition flow
//! goes through the same FSM and the same edit-coalescer.
//!
//! Lifecycle FSM:
//!   `NotCreated` → `Queued | Active` → `Completed | TimedOut | Aborted`
//!   `Queued` → `Active` (when a mailbox-queued turn is dispatched)
//!
//! Operating rules:
//!   - One `PlaceholderKey` (provider + channel + message) maps to exactly one
//!     Discord message id (1:1).
//!   - Repeated `ensure_active` / `ensure_queued` calls with identical content
//!     collapse into a single Discord PATCH (coalescing).
//!   - Terminal transitions (`Completed` / `TimedOut` / `Aborted`) are
//!     idempotent — the second call only emits a trace, never a PATCH.
//!   - Once a key has reached a terminal state any further `ensure_active` /
//!     `ensure_queued` calls are no-ops; `placeholder_sweeper` is responsible
//!     for cleaning up stale `Active` entries via its own TTL path.

use poise::serenity_prelude::{ChannelId, MessageId};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use crate::services::provider::ProviderKind;

use super::formatting::{
    MonitorHandoffReason, MonitorHandoffStatus, build_monitor_handoff_placeholder_with_context,
    build_monitor_handoff_placeholder_with_live_events,
};
use super::gateway::TurnGateway;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct PlaceholderKey {
    pub(super) provider: ProviderKind,
    pub(super) channel_id: ChannelId,
    pub(super) message_id: MessageId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlaceholderLifecycle {
    NotCreated,
    /// Mailbox-queued turn waiting for the active turn to finish (#1332).
    /// Visually distinct from `Active` so the user is not misled into thinking
    /// processing has already begun.
    Queued,
    Active,
    Completed,
    // #3034: documented terminal state (NotCreated → … → Completed|TimedOut|
    // Aborted) matched throughout the sweeper but not yet transitioned INTO by
    // any live path. Kept for state-machine completeness.
    #[allow(dead_code)]
    TimedOut,
    Aborted,
}

#[derive(Debug, Clone)]
pub(super) struct PlaceholderActiveInput {
    pub(super) reason: MonitorHandoffReason,
    pub(super) started_at_unix: i64,
    pub(super) tool_summary: Option<String>,
    pub(super) command_summary: Option<String>,
    pub(super) reason_detail: Option<String>,
    pub(super) context_line: Option<String>,
    pub(super) request_line: Option<String>,
    pub(super) progress_line: Option<String>,
}

#[derive(Debug, Clone)]
struct PlaceholderEntry {
    state: PlaceholderLifecycle,
    /// Last-rendered placeholder text — used to coalesce duplicate `ensure_active`
    /// calls so we don't burn Discord rate-limit budget on no-op edits.
    last_rendered: Option<String>,
    /// Snapshot of the last `Active` input. Terminal transitions reuse the
    /// snapshot to render the consistent header/footer pair.
    active_snapshot: Option<PlaceholderActiveInput>,
    last_live_events_block: Option<String>,
    last_live_events_edit_at: Option<Instant>,
}

impl Default for PlaceholderEntry {
    fn default() -> Self {
        Self {
            state: PlaceholderLifecycle::NotCreated,
            last_rendered: None,
            active_snapshot: None,
            last_live_events_block: None,
            last_live_events_edit_at: None,
        }
    }
}

/// Cap on retained `PlaceholderController` entries. Each Discord message id is
/// unique, so an unbounded map would leak entries across the lifetime of a
/// long-running dcserver — codex round-3 #1308 P2. Sized for ~hours of heavy
/// background-tool activity; eviction prefers terminal entries over Active
/// ones so we never drop a live card mid-flight.
const PLACEHOLDER_ENTRIES_MAX: usize = 4096;
const PLACEHOLDER_LIVE_EVENTS_MIN_EDIT_INTERVAL: Duration = Duration::from_secs(3);

/// PR #5 — bounded retry budget for placeholder edits. Discord routinely
/// hands back rate-limit (429) and transient gateway (5xx) errors that the
/// previous one-shot path treated as terminal `EditFailed`. Three attempts
/// covers the realistic spike profile (5/5s edit bucket + occasional 502)
/// without bloating the worst-case latency — the longest delay we ever sit
/// through is `EDIT_RETRY_BASE_DELAY * 2^(MAX_ATTEMPTS-2)` ≈ 1 s.
const EDIT_RETRY_MAX_ATTEMPTS: u32 = 3;
const EDIT_RETRY_BASE_DELAY: Duration = Duration::from_millis(500);
const EDIT_RETRY_MAX_DELAY: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditErrorCategory {
    /// 429 / "rate limit" body. Treated the same as `Transient` for sleep
    /// purposes — serenity's internal ratelimiter already honoured the
    /// `Retry-After` header before this surfaced, so we only need a small
    /// bounded backoff to ride out the residual spike. Kept as a distinct
    /// variant so future work can surface structured retry_after data and
    /// upgrade the wait policy without changing call-site logic.
    RateLimited,
    /// 5xx / network-ish hiccup or any error string we don't recognise.
    /// Exponential backoff is enough.
    Transient,
    /// Known-permanent Discord error (message gone, missing permissions,
    /// body too long, …). Retrying changes nothing; surface immediately.
    Permanent,
}

fn classify_edit_error(err: &str) -> EditErrorCategory {
    let lower = err.to_ascii_lowercase();
    // Serenity's `HttpError::UnsuccessfulRequest` Display only emits
    // Discord's JSON `error.message` (no status code), and
    // `HttpError::Request` flattens to "Error while sending HTTP request."
    // — so we cannot rely on `500`/`502`/etc. substrings being present.
    //
    // Classification strategy:
    //   1. Match the explicit rate-limit phrases Discord uses in the JSON
    //      body so we can honour `retry_after` when serenity surfaces a
    //      429 after its own internal retry budget ran out.
    //   2. Match the small set of Discord error messages that are known
    //      permanent (message gone, bad permissions, body too long, …)
    //      and short-circuit those.
    //   3. Default everything else (generic 5xx, "Error while sending HTTP
    //      request.", parse errors, opaque transport blips) to Transient —
    //      a few wasted retries on a 4xx we didn't list is cheap; failing
    //      to retry a genuine 5xx leaves the placeholder card stuck on
    //      "Processing…" until placeholder_sweeper expires it, which is
    //      exactly the bug this PR exists to close.
    if lower.contains("ratelimit")
        || lower.contains("rate limit")
        || lower.contains("rate-limit")
        || lower.contains("rate limited")
        || lower.contains("429")
        || lower.contains("too many requests")
    {
        return EditErrorCategory::RateLimited;
    }
    const PERMANENT_PATTERNS: &[&str] = &[
        "unknown message",
        "unknown channel",
        "missing permissions",
        "missing access",
        "cannot edit a message authored by another user",
        "invalid form body",
        "base_type_max_length",
        "2000 or fewer in length",
        "invalid webhook",
        "you are not allowed",
    ];
    if PERMANENT_PATTERNS.iter().any(|p| lower.contains(p)) {
        return EditErrorCategory::Permanent;
    }
    EditErrorCategory::Transient
}

fn backoff_delay(attempt: u32) -> Duration {
    let factor = 1u32 << attempt.min(8);
    let raw = EDIT_RETRY_BASE_DELAY.saturating_mul(factor);
    raw.min(EDIT_RETRY_MAX_DELAY)
}

/// Retry wrapper around `gateway.edit_message` for placeholder cards.
///
/// Sleep policy: exponential backoff on every retried attempt regardless of
/// category. We do **not** try to parse Discord's `retry_after` out of the
/// surfaced error string — serenity's `HttpError::UnsuccessfulRequest`
/// Display only emits Discord's JSON `error.message` (no status, no
/// retry_after), so any structured field is lost by the time we see a
/// `String`. Serenity's own ratelimiter already honours `Retry-After` and
/// sleeps before letting a 429 leak out to this layer, so the bounded
/// backoff here is the right shape for the residual 429 surface.
///
/// Returns the underlying error string on terminal failure so the caller's
/// `tracing::warn!` payload remains accurate.
async fn edit_message_with_retry<G: TurnGateway + ?Sized>(
    gateway: &G,
    channel_id: ChannelId,
    message_id: MessageId,
    content: &str,
) -> Result<(), String> {
    let mut last_err = String::new();
    for attempt in 0..EDIT_RETRY_MAX_ATTEMPTS {
        match gateway.edit_message(channel_id, message_id, content).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                let category = classify_edit_error(&err);
                last_err = err;
                if matches!(category, EditErrorCategory::Permanent) {
                    return Err(last_err);
                }
                // No sleep after the final attempt — burning the backoff on
                // the exit path serves no purpose and just delays the
                // `EditFailed` outcome the caller already handles.
                if attempt + 1 == EDIT_RETRY_MAX_ATTEMPTS {
                    return Err(last_err);
                }
                tokio::time::sleep(backoff_delay(attempt)).await;
            }
        }
    }
    Err(last_err)
}

#[derive(Debug, Default)]
struct PlaceholderSlotState {
    revoked: bool,
    entry: PlaceholderEntry,
}

impl std::ops::Deref for PlaceholderSlotState {
    type Target = PlaceholderEntry;

    fn deref(&self) -> &Self::Target {
        &self.entry
    }
}

impl std::ops::DerefMut for PlaceholderSlotState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.entry
    }
}

#[derive(Debug, Default)]
struct PlaceholderEntrySlot {
    state: Mutex<PlaceholderSlotState>,
}

/// Exact process-local authority for one controller entry incarnation.
///
/// The inner `Arc` is intentionally opaque: capability operations use it
/// directly and never recover authority by looking up (or creating) a key.
#[derive(Debug, Clone)]
pub(super) struct PlaceholderIncarnation {
    key: PlaceholderKey,
    slot: Arc<PlaceholderEntrySlot>,
}

#[derive(Debug, Default)]
pub(super) struct PlaceholderController {
    entries: dashmap::DashMap<PlaceholderKey, Arc<PlaceholderEntrySlot>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlaceholderControllerOutcome {
    /// Discord PATCH was issued and committed.
    Edited,
    /// Lifecycle transition succeeded but coalesced — no PATCH was issued.
    /// The caller should treat this as success.
    Coalesced,
    /// Terminal transition called twice — idempotent no-op.
    AlreadyTerminal,
    /// PATCH attempted but failed; caller decides whether to retry.
    EditFailed,
    /// Transition was rejected because the FSM forbids it (e.g. `Active` after
    /// terminal state).  Treated as a no-op with a trace.
    Rejected,
}

impl PlaceholderController {
    fn entry(&self, key: &PlaceholderKey) -> Arc<PlaceholderEntrySlot> {
        if let Some(existing) = self.entries.get(key) {
            return existing.clone();
        }
        if self.entries.len() >= PLACEHOLDER_ENTRIES_MAX {
            self.evict_terminal_entries();
        }
        self.entries
            .entry(key.clone())
            .or_insert_with(|| Arc::new(PlaceholderEntrySlot::default()))
            .clone()
    }

    pub(super) fn incarnation(&self, key: &PlaceholderKey) -> PlaceholderIncarnation {
        PlaceholderIncarnation {
            key: key.clone(),
            slot: self.entry(key),
        }
    }

    pub(super) async fn revoke_incarnation(&self, incarnation: &PlaceholderIncarnation) {
        let mut guarded = incarnation.slot.state.lock().await;
        guarded.revoked = true;
        drop(guarded);
        self.entries.remove_if(&incarnation.key, |_, current| {
            Arc::ptr_eq(current, &incarnation.slot)
        });
    }

    /// Sweep entries whose state is terminal (Completed/TimedOut/Aborted) or
    /// `NotCreated` (initial edit failed; nobody committed) so the map cannot
    /// grow without bound on a long-running process. `Active` entries are
    /// left in place — they own a live Discord card. Uses `try_lock` to avoid
    /// blocking on entries the caller is mid-edit on.
    fn evict_terminal_entries(&self) {
        let mut to_remove: Vec<PlaceholderKey> = Vec::new();
        for kv in self.entries.iter() {
            if let Ok(guard) = kv.value().state.try_lock() {
                if matches!(
                    guard.state,
                    PlaceholderLifecycle::NotCreated
                        | PlaceholderLifecycle::Completed
                        | PlaceholderLifecycle::TimedOut
                        | PlaceholderLifecycle::Aborted
                ) {
                    to_remove.push(kv.key().clone());
                }
            }
        }
        for key in to_remove {
            self.entries.remove(&key);
        }
    }

    /// Drive the placeholder into `Active`. Issues a Discord PATCH only if the
    /// rendered content actually changed since the last call. Safe to call on
    /// every assistant token / tool event tick. Allowed source states:
    /// `NotCreated`, `Queued` (mailbox dequeue path, #1332), `Active` (re-render).
    pub(super) async fn ensure_active<G: TurnGateway + ?Sized>(
        &self,
        gateway: &G,
        key: PlaceholderKey,
        input: PlaceholderActiveInput,
    ) -> PlaceholderControllerOutcome {
        self.ensure_active_inner(gateway, key, input, None).await
    }

    pub(super) async fn ensure_active_with_live_events<G: TurnGateway + ?Sized>(
        &self,
        gateway: &G,
        key: PlaceholderKey,
        input: PlaceholderActiveInput,
        live_events_block: String,
    ) -> PlaceholderControllerOutcome {
        self.ensure_active_inner(gateway, key, input, Some(live_events_block))
            .await
    }

    async fn ensure_active_inner<G: TurnGateway + ?Sized>(
        &self,
        gateway: &G,
        key: PlaceholderKey,
        input: PlaceholderActiveInput,
        live_events_block: Option<String>,
    ) -> PlaceholderControllerOutcome {
        let incarnation = self.incarnation(&key);
        self.ensure_active_incarnation_inner(gateway, &incarnation, input, live_events_block)
            .await
    }

    pub(super) async fn ensure_active_incarnation<G: TurnGateway + ?Sized>(
        &self,
        gateway: &G,
        incarnation: &PlaceholderIncarnation,
        input: PlaceholderActiveInput,
    ) -> PlaceholderControllerOutcome {
        self.ensure_active_incarnation_inner(gateway, incarnation, input, None)
            .await
    }

    async fn ensure_active_incarnation_inner<G: TurnGateway + ?Sized>(
        &self,
        gateway: &G,
        incarnation: &PlaceholderIncarnation,
        input: PlaceholderActiveInput,
        live_events_block: Option<String>,
    ) -> PlaceholderControllerOutcome {
        let key = &incarnation.key;
        let mut guarded = incarnation.slot.state.lock().await;
        if guarded.revoked {
            return PlaceholderControllerOutcome::Rejected;
        }

        // Forbid re-activating after a terminal transition — placeholder_sweeper
        // owns stale-Active recovery and we never want to drag a closed card
        // back into the Active state.
        if matches!(
            guarded.state,
            PlaceholderLifecycle::Completed
                | PlaceholderLifecycle::TimedOut
                | PlaceholderLifecycle::Aborted
        ) {
            return PlaceholderControllerOutcome::Rejected;
        }

        let rendered = build_monitor_handoff_placeholder_with_live_events(
            MonitorHandoffStatus::Active,
            input.reason,
            input.started_at_unix,
            input.tool_summary.as_deref(),
            input.command_summary.as_deref(),
            input.reason_detail.as_deref(),
            input.context_line.as_deref(),
            input.request_line.as_deref(),
            input.progress_line.as_deref(),
            live_events_block.as_deref(),
        );

        // Coalesce identical re-renders into a single PATCH.  Tool-stream
        // ticks can fire several times per second and Discord rate-limits
        // PATCHes <100ms apart. `Queued` content always differs from `Active`
        // (different header), so the Queued → Active transition correctly
        // forces a PATCH even when last_rendered is set.
        if guarded.last_rendered.as_deref() == Some(rendered.as_str())
            && matches!(guarded.state, PlaceholderLifecycle::Active)
        {
            return PlaceholderControllerOutcome::Coalesced;
        }

        if let Some(block) = live_events_block.as_deref()
            && matches!(guarded.state, PlaceholderLifecycle::Active)
            && guarded.last_live_events_block.as_deref() != Some(block)
            && guarded
                .last_live_events_edit_at
                .is_some_and(|last| last.elapsed() < PLACEHOLDER_LIVE_EVENTS_MIN_EDIT_INTERVAL)
        {
            return PlaceholderControllerOutcome::Coalesced;
        }

        let edit_result =
            edit_message_with_retry(gateway, key.channel_id, key.message_id, &rendered).await;

        match edit_result {
            Ok(_) => {
                guarded.state = PlaceholderLifecycle::Active;
                guarded.last_rendered = Some(rendered);
                guarded.active_snapshot = Some(input);
                if live_events_block.is_some() {
                    guarded.last_live_events_edit_at = Some(Instant::now());
                }
                guarded.last_live_events_block = live_events_block;
                PlaceholderControllerOutcome::Edited
            }
            Err(err) => {
                tracing::warn!(
                    key = ?key,
                    error = %err,
                    site = "ensure_active",
                    "placeholder edit failed",
                );
                // codex round-9 P2 on PR #1308: leave the `NotCreated` row
                // in the map. Concurrent `ensure_active` callers may already
                // hold a clone of the same `Arc` and could commit on a
                // detached row, so future `transition` / `lifecycle` lookups
                // would create a fresh `NotCreated` and the visible card
                // would never reach a terminal state.
                // `evict_terminal_entries` now sweeps `NotCreated` rows
                // alongside terminal states, so failed initial edits still
                // bound the map under `PLACEHOLDER_ENTRIES_MAX`.
                PlaceholderControllerOutcome::EditFailed
            }
        }
    }

    /// Drive the placeholder into `Queued` (#1332) — used when a user message
    /// loses the `mailbox_try_start_turn` race and is enqueued for later
    /// dispatch. Visually distinct from `Active` so the user knows the turn
    /// has not started yet. Allowed only from `NotCreated` / `Queued`; rejected
    /// once the card has reached `Active` or any terminal state.
    pub(super) async fn ensure_queued<G: TurnGateway + ?Sized>(
        &self,
        gateway: &G,
        key: PlaceholderKey,
        input: PlaceholderActiveInput,
    ) -> PlaceholderControllerOutcome {
        let entry = self.entry(&key);
        let mut guarded = entry.state.lock().await;

        if matches!(
            guarded.state,
            PlaceholderLifecycle::Active
                | PlaceholderLifecycle::Completed
                | PlaceholderLifecycle::TimedOut
                | PlaceholderLifecycle::Aborted
        ) {
            return PlaceholderControllerOutcome::Rejected;
        }

        let rendered = build_monitor_handoff_placeholder_with_context(
            MonitorHandoffStatus::Queued,
            input.reason,
            input.started_at_unix,
            input.tool_summary.as_deref(),
            input.command_summary.as_deref(),
            input.reason_detail.as_deref(),
            input.context_line.as_deref(),
            input.request_line.as_deref(),
            input.progress_line.as_deref(),
        );

        if guarded.last_rendered.as_deref() == Some(rendered.as_str())
            && matches!(guarded.state, PlaceholderLifecycle::Queued)
        {
            return PlaceholderControllerOutcome::Coalesced;
        }

        let edit_result =
            edit_message_with_retry(gateway, key.channel_id, key.message_id, &rendered).await;

        match edit_result {
            Ok(_) => {
                guarded.state = PlaceholderLifecycle::Queued;
                guarded.last_rendered = Some(rendered);
                guarded.active_snapshot = Some(input);
                PlaceholderControllerOutcome::Edited
            }
            Err(err) => {
                tracing::warn!(
                    key = ?key,
                    error = %err,
                    site = "ensure_queued",
                    "placeholder edit failed",
                );
                PlaceholderControllerOutcome::EditFailed
            }
        }
    }

    /// Drive the placeholder into a terminal state. Idempotent: a second call
    /// for the same `(key, status)` pair returns `AlreadyTerminal` without
    /// emitting a PATCH.  Calling with a different terminal status after a
    /// terminal transition is also rejected (returns `AlreadyTerminal`) — once
    /// closed, the card stays closed.
    pub(super) async fn transition<G: TurnGateway + ?Sized>(
        &self,
        gateway: &G,
        key: PlaceholderKey,
        target: PlaceholderLifecycle,
    ) -> PlaceholderControllerOutcome {
        debug_assert!(
            matches!(
                target,
                PlaceholderLifecycle::Completed
                    | PlaceholderLifecycle::TimedOut
                    | PlaceholderLifecycle::Aborted
            ),
            "transition() expects a terminal target, got {:?}",
            target
        );
        let entry = self.entry(&key);
        let mut guarded = entry.state.lock().await;

        if matches!(
            guarded.state,
            PlaceholderLifecycle::Completed
                | PlaceholderLifecycle::TimedOut
                | PlaceholderLifecycle::Aborted
        ) {
            return PlaceholderControllerOutcome::AlreadyTerminal;
        }

        // Reject terminal transitions before the placeholder ever became
        // `Active` — there is nothing on Discord to update, so silently no-op.
        let snapshot = match guarded.active_snapshot.clone() {
            Some(snapshot) => snapshot,
            None => {
                // Mark terminal anyway so future Active calls remain rejected.
                guarded.state = target;
                return PlaceholderControllerOutcome::Rejected;
            }
        };

        let status = match target {
            PlaceholderLifecycle::Completed => MonitorHandoffStatus::Completed,
            PlaceholderLifecycle::TimedOut => MonitorHandoffStatus::TimedOut,
            PlaceholderLifecycle::Aborted => MonitorHandoffStatus::Aborted,
            other => unreachable!("non-terminal target {:?}", other),
        };

        let rendered = build_monitor_handoff_placeholder_with_context(
            status,
            snapshot.reason,
            snapshot.started_at_unix,
            snapshot.tool_summary.as_deref(),
            snapshot.command_summary.as_deref(),
            snapshot.reason_detail.as_deref(),
            snapshot.context_line.as_deref(),
            snapshot.request_line.as_deref(),
            snapshot.progress_line.as_deref(),
        );

        let edit_result =
            edit_message_with_retry(gateway, key.channel_id, key.message_id, &rendered).await;

        match edit_result {
            Ok(_) => {
                guarded.state = target;
                guarded.last_rendered = Some(rendered);
                PlaceholderControllerOutcome::Edited
            }
            Err(err) => {
                tracing::warn!(
                    key = ?key,
                    error = %err,
                    target = ?target,
                    site = "transition",
                    "placeholder edit failed",
                );
                PlaceholderControllerOutcome::EditFailed
            }
        }
    }

    /// Invalidate only the render coalescer for a live entry. The sweeper uses
    /// this after an external Discord edit races with owner revival so the next
    /// ordinary live render must PATCH the visible card back to Active.
    pub(super) async fn invalidate_render_cache(&self, key: &PlaceholderKey) -> bool {
        let Some(entry) = self.entries.get(key).map(|entry| entry.clone()) else {
            return false;
        };
        let mut guarded = entry.state.lock().await;
        guarded.last_rendered = None;
        guarded.last_live_events_block = None;
        guarded.last_live_events_edit_at = None;
        true
    }

    /// Revoke one current incarnation and remove it without emitting a Discord
    /// PATCH. The slot mutex serializes revocation with PATCH admission and
    /// commit, so a caller retaining this slot cannot commit after detachment.
    pub(super) async fn revoke_and_detach(&self, key: &PlaceholderKey) {
        let Some(slot) = self.entries.get(key).map(|entry| entry.clone()) else {
            return;
        };
        self.revoke_incarnation(&PlaceholderIncarnation {
            key: key.clone(),
            slot,
        })
        .await;
    }

    /// Drop a key from the controller without emitting any Discord PATCH.
    /// Used by rollover-retarget paths that do not retain a PATCH incarnation.
    pub(super) fn detach(&self, key: &PlaceholderKey) {
        self.entries.remove(key);
    }

    /// #1332: drop every entry whose `(channel_id, message_id)` matches the
    /// argument, regardless of provider. Used by the queue-exit feedback path
    /// where the provider scope was discarded with the queued-placeholder
    /// mapping; without provider-agnostic detach a cancelled queued
    /// intervention would leave a stale `Queued` row in the cap-bounded
    /// `entries` map (`Queued` is excluded from the regular eviction sweep
    /// because in-flight queued placeholders must survive until dispatch).
    pub(super) fn detach_by_message(&self, channel_id: ChannelId, message_id: MessageId) {
        self.entries
            .retain(|key, _| !(key.channel_id == channel_id && key.message_id == message_id));
    }
}

#[cfg(test)]
mod edit_retry_tests {
    use super::*;
    use crate::services::discord::gateway::{GatewayFuture, TurnGateway};
    use poise::serenity_prelude::{ChannelId, MessageId};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Programmable gateway: replays a fixed `responses` queue, one per call,
    /// so we can rehearse the retry helper's behaviour against the exact error
    /// strings serenity surfaces (429 with retry_after / 5xx / 4xx).
    struct ScriptedGateway {
        responses: tokio::sync::Mutex<Vec<Result<(), String>>>,
        calls: AtomicUsize,
        edit_started: tokio::sync::Notify,
        edit_gate: Option<tokio::sync::Semaphore>,
    }

    impl ScriptedGateway {
        fn new(responses: Vec<Result<(), String>>) -> Self {
            Self {
                responses: tokio::sync::Mutex::new(responses),
                calls: AtomicUsize::new(0),
                edit_started: tokio::sync::Notify::new(),
                edit_gate: None,
            }
        }

        fn blocked() -> Self {
            Self {
                responses: tokio::sync::Mutex::new(vec![Ok(())]),
                calls: AtomicUsize::new(0),
                edit_started: tokio::sync::Notify::new(),
                edit_gate: Some(tokio::sync::Semaphore::new(0)),
            }
        }

        fn release_edit(&self) {
            self.edit_gate.as_ref().unwrap().add_permits(1);
        }
    }

    impl TurnGateway for ScriptedGateway {
        fn send_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<MessageId, String>> {
            Box::pin(async { Ok(MessageId::new(1)) })
        }

        fn edit_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.edit_started.notify_one();
                if let Some(gate) = &self.edit_gate {
                    gate.acquire().await.unwrap().forget();
                }
                let mut queue = self.responses.lock().await;
                if queue.is_empty() {
                    Err("scripted gateway exhausted".to_string())
                } else {
                    queue.remove(0)
                }
            })
        }

        fn replace_message_with_outcome<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<
            'a,
            Result<crate::services::discord::formatting::ReplaceLongMessageOutcome, String>,
        > {
            Box::pin(async {
                Ok(crate::services::discord::formatting::ReplaceLongMessageOutcome::EditedOriginal)
            })
        }

        fn schedule_retry_with_history<'a>(
            &'a self,
            _channel_id: ChannelId,
            _user_message_id: MessageId,
            _user_text: &'a str,
        ) -> GatewayFuture<'a, ()> {
            Box::pin(async {})
        }

        fn dispatch_queued_turn<'a>(
            &'a self,
            _channel_id: ChannelId,
            _intervention: &'a crate::services::discord::Intervention,
            _request_owner_name: &'a str,
            _has_more_queued_turns: bool,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn validate_live_routing<'a>(
            &'a self,
            _channel_id: ChannelId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn requester_mention(&self) -> Option<String> {
            None
        }

        fn can_chain_locally(&self) -> bool {
            false
        }

        fn bot_owner_provider(&self) -> Option<crate::services::provider::ProviderKind> {
            Some(crate::services::provider::ProviderKind::Codex)
        }
    }

    #[test]
    fn classify_edit_error_categorises_known_signatures() {
        // Discord's 429 body — what `e.error.message` looks like after
        // serenity exhausts its internal retry budget.
        assert_eq!(
            classify_edit_error("You are being rate limited."),
            EditErrorCategory::RateLimited
        );
        assert_eq!(
            classify_edit_error("RateLimit { retry_after: 1.5 }"),
            EditErrorCategory::RateLimited
        );

        // Known-permanent Discord error bodies.
        assert_eq!(
            classify_edit_error("Unknown Message"),
            EditErrorCategory::Permanent
        );
        assert_eq!(
            classify_edit_error("Missing Permissions"),
            EditErrorCategory::Permanent
        );
        assert_eq!(
            classify_edit_error(
                "Invalid Form Body (content: BASE_TYPE_MAX_LENGTH Must be 2000 or fewer in length)"
            ),
            EditErrorCategory::Permanent
        );

        // serenity::HttpError::Request — generic transport failure, no
        // status code in the string. Must NOT fall through to Permanent.
        assert_eq!(
            classify_edit_error("Error while sending HTTP request."),
            EditErrorCategory::Transient
        );
        // serenity::HttpError::UnsuccessfulRequest for a 5xx — `error.message`
        // tends to be empty or whatever Discord's HTML/error page exposes,
        // but the status code itself is absent from Display.
        assert_eq!(
            classify_edit_error(""),
            EditErrorCategory::Transient,
            "empty error string (5xx with no parsed body) must retry"
        );
    }

    fn key() -> PlaceholderKey {
        PlaceholderKey {
            provider: ProviderKind::Codex,
            channel_id: ChannelId::new(1),
            message_id: MessageId::new(2),
        }
    }

    fn input() -> PlaceholderActiveInput {
        PlaceholderActiveInput {
            reason: MonitorHandoffReason::ExplicitCall,
            started_at_unix: 1_700_000_000,
            tool_summary: None,
            command_summary: None,
            reason_detail: None,
            context_line: None,
            request_line: None,
            progress_line: None,
        }
    }

    #[tokio::test]
    async fn incarnation_revocation_serializes_with_edits_and_cannot_rebind() {
        let controller = Arc::new(PlaceholderController::default());
        let gateway = Arc::new(ScriptedGateway::blocked());
        let key = key();
        let old = controller.incarnation(&key);

        let edit = tokio::spawn({
            let controller = controller.clone();
            let gateway = gateway.clone();
            let old = old.clone();
            async move {
                controller
                    .ensure_active_incarnation(gateway.as_ref(), &old, input())
                    .await
            }
        });
        gateway.edit_started.notified().await;
        let revoke = tokio::spawn({
            let controller = controller.clone();
            let old = old.clone();
            async move { controller.revoke_incarnation(&old).await }
        });
        tokio::task::yield_now().await;
        assert!(
            !revoke.is_finished(),
            "revoke must wait for the admitted PATCH"
        );

        assert_eq!(gateway.calls.load(Ordering::SeqCst), 1);
        assert!(Arc::ptr_eq(
            &controller.entries.get(&key).unwrap().clone(),
            &old.slot
        ));
        gateway.release_edit();
        assert_eq!(edit.await.unwrap(), PlaceholderControllerOutcome::Edited);
        revoke.await.unwrap();
        let committed_then_revoked = old.slot.state.lock().await;
        assert!(committed_then_revoked.revoked);
        assert_eq!(committed_then_revoked.state, PlaceholderLifecycle::Active);
        drop(committed_then_revoked);
        assert_eq!(gateway.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            controller
                .ensure_active_incarnation(gateway.as_ref(), &old, input())
                .await,
            PlaceholderControllerOutcome::Rejected
        );
        assert_eq!(gateway.calls.load(Ordering::SeqCst), 1);

        let replacement = controller.incarnation(&key);
        let other_key = PlaceholderKey {
            message_id: MessageId::new(99),
            ..key.clone()
        };
        controller.revoke_incarnation(&old).await;
        assert!(Arc::ptr_eq(
            &controller.entries.get(&key).unwrap().clone(),
            &replacement.slot
        ));
        assert!(!controller.entries.contains_key(&other_key));
        assert_eq!(
            controller.entries.len(),
            1,
            "stale capability must not recreate"
        );
    }

    #[tokio::test]
    async fn detach_tombstones_retained_incarnation_before_late_patch() {
        let controller = PlaceholderController::default();
        let gateway = ScriptedGateway::new(vec![Ok(())]);
        let key = key();
        let retained = controller.incarnation(&key);

        controller.revoke_and_detach(&key).await;

        assert!(!controller.entries.contains_key(&key));
        assert!(retained.slot.state.lock().await.revoked);
        assert_eq!(
            controller
                .ensure_active_incarnation(&gateway, &retained, input())
                .await,
            PlaceholderControllerOutcome::Rejected,
            "a PATCH capability retained before detach must not commit to its tombstoned slot"
        );
        assert_eq!(gateway.calls.load(Ordering::SeqCst), 0);
        assert!(!controller.entries.contains_key(&key));
    }

    #[tokio::test]
    async fn mismatched_key_and_slot_capability_cannot_remove_or_mutate_maps() {
        let controller = PlaceholderController::default();
        let gateway = ScriptedGateway::new(vec![Ok(())]);
        let key_a = key();
        let key_b = PlaceholderKey {
            message_id: MessageId::new(99),
            ..key_a.clone()
        };
        let incarnation_a = controller.incarnation(&key_a);
        let incarnation_b = controller.incarnation(&key_b);
        let mismatched = PlaceholderIncarnation {
            key: key_a.clone(),
            slot: incarnation_b.slot.clone(),
        };

        controller.revoke_incarnation(&mismatched).await;
        assert!(controller.entries.contains_key(&key_a));
        assert!(controller.entries.contains_key(&key_b));
        assert!(Arc::ptr_eq(
            &controller.entries.get(&key_a).unwrap().clone(),
            &incarnation_a.slot
        ));
        assert!(Arc::ptr_eq(
            &controller.entries.get(&key_b).unwrap().clone(),
            &incarnation_b.slot
        ));
        assert!(!incarnation_a.slot.state.lock().await.revoked);
        assert!(incarnation_b.slot.state.lock().await.revoked);

        assert_eq!(
            controller
                .ensure_active_incarnation(&gateway, &mismatched, input())
                .await,
            PlaceholderControllerOutcome::Rejected
        );
        assert_eq!(gateway.calls.load(Ordering::SeqCst), 0);
        assert_eq!(controller.entries.len(), 2);
    }

    #[tokio::test]
    async fn cancelled_edit_releases_incarnation_mutex_without_claiming_wire_result() {
        let controller = Arc::new(PlaceholderController::default());
        let gateway = Arc::new(ScriptedGateway::blocked());
        let key = key();
        let incarnation = controller.incarnation(&key);
        let edit = tokio::spawn({
            let controller = controller.clone();
            let gateway = gateway.clone();
            let incarnation = incarnation.clone();
            async move {
                controller
                    .ensure_active_incarnation(gateway.as_ref(), &incarnation, input())
                    .await
            }
        });
        gateway.edit_started.notified().await;
        edit.abort();
        assert!(edit.await.unwrap_err().is_cancelled());
        controller.revoke_incarnation(&incarnation).await;
        let guarded = incarnation.slot.state.lock().await;
        assert!(guarded.revoked);
        assert_eq!(guarded.state, PlaceholderLifecycle::NotCreated);
        assert_eq!(gateway.calls.load(Ordering::SeqCst), 1);
    }

    /// Rate-limited then success: helper must retry once and surface Ok.
    /// Uses `start_paused` so the backoff sleep is fast-forwarded by tokio.
    #[tokio::test(start_paused = true)]
    async fn retry_recovers_from_rate_limit_then_succeeds() {
        let gateway = Arc::new(ScriptedGateway::new(vec![
            Err("You are being rate limited.".to_string()),
            Ok(()),
        ]));
        let result =
            edit_message_with_retry(gateway.as_ref(), ChannelId::new(1), MessageId::new(2), "x")
                .await;
        assert!(result.is_ok());
        assert_eq!(gateway.calls.load(Ordering::SeqCst), 2);
    }

    /// Permanent errors short-circuit: the helper must surface immediately
    /// without exhausting the retry budget.
    #[tokio::test(start_paused = true)]
    async fn retry_surfaces_permanent_errors_without_extra_attempts() {
        let gateway = Arc::new(ScriptedGateway::new(vec![Err(
            "Unknown Message".to_string()
        )]));
        let result =
            edit_message_with_retry(gateway.as_ref(), ChannelId::new(1), MessageId::new(2), "x")
                .await;
        assert!(result.is_err());
        assert_eq!(gateway.calls.load(Ordering::SeqCst), 1);
    }

    /// Generic transient errors (e.g. `serenity::HttpError::Request` whose
    /// Display is "Error while sending HTTP request.") must retry — they're
    /// the everyday 5xx / transport blip case. `start_paused` fast-forwards
    /// the backoff sleeps via the tokio scheduler, so the elapsed-time guard
    /// against a regression that adds a sleep after the final attempt is
    /// deterministic instead of wall-clock-flaky on slow CI.
    #[tokio::test(start_paused = true)]
    async fn retry_exhausts_budget_on_persistent_transient_errors() {
        let started = tokio::time::Instant::now();
        let gateway = Arc::new(ScriptedGateway::new(vec![
            Err("Error while sending HTTP request.".to_string()),
            Err("Error while sending HTTP request.".to_string()),
            Err("Error while sending HTTP request.".to_string()),
        ]));
        let result =
            edit_message_with_retry(gateway.as_ref(), ChannelId::new(1), MessageId::new(2), "x")
                .await;
        assert!(result.is_err());
        assert_eq!(
            gateway.calls.load(Ordering::SeqCst),
            EDIT_RETRY_MAX_ATTEMPTS as usize
        );
        // With `start_paused` the elapsed time is tokio's virtual clock — it
        // only advances when something awaits a timer. Two backoff sleeps
        // fire on a 3-attempt budget (after attempt 0 and after attempt 1):
        // `base + 2*base = 3*base`. If a regression adds a sleep after the
        // final attempt we'd see at least `base + 2*base + 4*base = 7*base`.
        let elapsed = started.elapsed();
        let expected_two_sleeps = backoff_delay(0).saturating_add(backoff_delay(1));
        let no_final_sleep_ceiling = expected_two_sleeps.saturating_add(backoff_delay(2));
        assert_eq!(
            elapsed, expected_two_sleeps,
            "expected exactly two backoff sleeps with virtual clock"
        );
        assert!(
            elapsed < no_final_sleep_ceiling,
            "retry helper must not sleep after the final attempt (elapsed={:?}, ceiling={:?})",
            elapsed,
            no_final_sleep_ceiling
        );
    }
}

#[cfg(test)]
mod live_events_tests {
    use super::*;
    use crate::services::discord::gateway::{GatewayFuture, TurnGateway};
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::{ChannelId, MessageId};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingGateway {
        edits: AtomicUsize,
        last_edit: tokio::sync::Mutex<Option<String>>,
    }

    impl CountingGateway {
        fn new() -> Self {
            Self {
                edits: AtomicUsize::new(0),
                last_edit: tokio::sync::Mutex::new(None),
            }
        }
    }

    impl TurnGateway for CountingGateway {
        fn send_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<MessageId, String>> {
            Box::pin(async { Ok(MessageId::new(1)) })
        }

        fn edit_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            content: &'a str,
        ) -> GatewayFuture<'a, Result<(), String>> {
            let content = content.to_string();
            Box::pin(async move {
                self.edits.fetch_add(1, Ordering::SeqCst);
                *self.last_edit.lock().await = Some(content);
                Ok(())
            })
        }

        fn replace_message_with_outcome<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<
            'a,
            Result<crate::services::discord::formatting::ReplaceLongMessageOutcome, String>,
        > {
            Box::pin(async {
                Ok(crate::services::discord::formatting::ReplaceLongMessageOutcome::EditedOriginal)
            })
        }

        fn schedule_retry_with_history<'a>(
            &'a self,
            _channel_id: ChannelId,
            _user_message_id: MessageId,
            _user_text: &'a str,
        ) -> GatewayFuture<'a, ()> {
            Box::pin(async {})
        }

        fn dispatch_queued_turn<'a>(
            &'a self,
            _channel_id: ChannelId,
            _intervention: &'a crate::services::discord::Intervention,
            _request_owner_name: &'a str,
            _has_more_queued_turns: bool,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn validate_live_routing<'a>(
            &'a self,
            _channel_id: ChannelId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn requester_mention(&self) -> Option<String> {
            None
        }

        fn can_chain_locally(&self) -> bool {
            false
        }

        fn bot_owner_provider(&self) -> Option<ProviderKind> {
            Some(ProviderKind::Codex)
        }
    }

    fn key() -> PlaceholderKey {
        PlaceholderKey {
            provider: ProviderKind::Codex,
            channel_id: ChannelId::new(1),
            message_id: MessageId::new(2),
        }
    }

    fn input() -> PlaceholderActiveInput {
        PlaceholderActiveInput {
            reason: MonitorHandoffReason::ExplicitCall,
            started_at_unix: 1_700_000_000,
            tool_summary: Some("Monitor".to_string()),
            command_summary: Some("session=foo".to_string()),
            reason_detail: None,
            context_line: None,
            request_line: None,
            progress_line: None,
        }
    }

    #[tokio::test]
    async fn ensure_active_with_live_events_appends_block_and_throttles_changes() {
        let gateway = Arc::new(CountingGateway::new());
        let controller = PlaceholderController::default();
        let outcome = controller
            .ensure_active_with_live_events(
                gateway.as_ref(),
                key(),
                input(),
                "```text\n[Bash] echo 1\n```".to_string(),
            )
            .await;
        assert_eq!(outcome, PlaceholderControllerOutcome::Edited);
        let last = gateway.last_edit.lock().await.clone().unwrap();
        assert!(last.contains("[Bash] echo 1"));

        let outcome = controller
            .ensure_active_with_live_events(
                gateway.as_ref(),
                key(),
                input(),
                "```text\n[Bash] echo 2\n```".to_string(),
            )
            .await;
        assert_eq!(outcome, PlaceholderControllerOutcome::Coalesced);
        assert_eq!(gateway.edits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn render_cache_invalidation_forces_identical_live_rerender() {
        let gateway = Arc::new(CountingGateway::new());
        let controller = PlaceholderController::default();
        let first = controller
            .ensure_active(gateway.as_ref(), key(), input())
            .await;
        let coalesced = controller
            .ensure_active(gateway.as_ref(), key(), input())
            .await;

        assert_eq!(first, PlaceholderControllerOutcome::Edited);
        assert_eq!(coalesced, PlaceholderControllerOutcome::Coalesced);
        assert_eq!(gateway.edits.load(Ordering::SeqCst), 1);
        assert!(controller.invalidate_render_cache(&key()).await);

        let repaired = controller
            .ensure_active(gateway.as_ref(), key(), input())
            .await;
        assert_eq!(repaired, PlaceholderControllerOutcome::Edited);
        assert_eq!(gateway.edits.load(Ordering::SeqCst), 2);
    }
}
