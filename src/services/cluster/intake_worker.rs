//! Worker-side intake polling loop. Phase 3 of intake-node-routing
//! (docs/design/intake-node-routing.md).
//!
//! Polls `intake_outbox` for rows where `target_instance_id == self`,
//! atomically claims them, transitions through the 5-state machine
//! (`pending → claimed → accepted → spawned → done | failed_pre_accept |
//! failed_post_accept`), and invokes
//! `services::discord::execute_intake_turn_core` to actually run the
//! Discord turn.
//!
//! Phase 4 wires `start_intake_worker` into the worker-node bootstrap
//! flow. Until then the loop is callable but never started in production.
//!
//! Critical invariants enforced here:
//!   - Workers MUST abort the turn before spawning when
//!     `mark_accepted` returns `Ok(false)` — that means the leader's
//!     stale-claim sweep already reset the row, and double-execution
//!     is the only way to double-emit a Discord turn.
//!   - Once a row reaches `accepted`, auto-retry is forbidden
//!     (round-2 P0 #2). Post-accept failures call `mark_failed_post_accept`
//!     and the operator alert path takes over.

use crate::db::intake_outbox::{
    IntakeOutboxRow, claim_pending_for_target, mark_accepted, mark_done, mark_failed_post_accept,
    mark_failed_pre_accept, mark_spawned, return_claimed_to_pending,
};
use crate::services::discord::{IntakeRequest, SharedData, TurnKind, execute_intake_turn_core};
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};
use sqlx::PgPool;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Provider-local worker activity used by the restart marker poller. The
/// process-global shutdown counters remain separate; each provider waits only
/// for its own accepted intake tick before consuming its barrier slot.
pub(crate) struct IntakeWorkerLifecycle {
    admission_fenced: AtomicBool,
    active_ticks: std::sync::atomic::AtomicUsize,
    drained: tokio::sync::Notify,
}

impl Default for IntakeWorkerLifecycle {
    fn default() -> Self {
        Self {
            admission_fenced: AtomicBool::new(false),
            active_ticks: std::sync::atomic::AtomicUsize::new(0),
            drained: tokio::sync::Notify::new(),
        }
    }
}

impl IntakeWorkerLifecycle {
    /// Acquire admission for one complete worker tick. The post-increment fence
    /// check closes the load-before-fence race: either the tick is counted before
    /// the poller drains, or it relinquishes admission without touching the DB.
    pub(crate) fn try_begin_tick(&self) -> Option<IntakeWorkerTickGuard<'_>> {
        if self.admission_fenced.load(Ordering::SeqCst) {
            return None;
        }
        self.finish_admission_after_open_check()
    }

    fn finish_admission_after_open_check(&self) -> Option<IntakeWorkerTickGuard<'_>> {
        self.active_ticks.fetch_add(1, Ordering::SeqCst);
        if self.admission_fenced.load(Ordering::SeqCst) {
            self.finish_tick();
            return None;
        }
        Some(IntakeWorkerTickGuard { lifecycle: self })
    }

    pub(crate) fn fence_admission(&self) {
        self.admission_fenced.store(true, Ordering::SeqCst);
    }

    pub(crate) fn admission_is_fenced(&self) -> bool {
        self.admission_fenced.load(Ordering::SeqCst)
    }

    fn finish_tick(&self) {
        if self.active_ticks.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.drained.notify_one();
        }
    }

    /// Wait for the active tick, if any, to finish. `notify_one` retains a
    /// permit when the drop races between the atomic load and `notified()`, so
    /// the single provider poller cannot miss the drained edge.
    pub(crate) async fn wait_until_drained(&self) {
        while self.active_ticks.load(Ordering::SeqCst) != 0 {
            self.drained.notified().await;
        }
    }
}

pub(crate) struct IntakeWorkerTickGuard<'a> {
    lifecycle: &'a IntakeWorkerLifecycle,
}

impl Drop for IntakeWorkerTickGuard<'_> {
    fn drop(&mut self) {
        self.lifecycle.finish_tick();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AdmissionCheckpoint {
    BeforeClaim,
    AfterClaim,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AdmissionAction {
    Proceed,
    StopBeforeClaim,
    ReleaseClaim,
}

fn admission_action(
    cancel: &AtomicBool,
    lifecycle: &IntakeWorkerLifecycle,
    checkpoint: AdmissionCheckpoint,
) -> AdmissionAction {
    if !cancel.load(Ordering::Acquire) && !lifecycle.admission_is_fenced() {
        return AdmissionAction::Proceed;
    }
    match checkpoint {
        AdmissionCheckpoint::BeforeClaim => AdmissionAction::StopBeforeClaim,
        AdmissionCheckpoint::AfterClaim => AdmissionAction::ReleaseClaim,
    }
}

/// Poll-loop tunables. Defaults reflect the design doc's adaptive
/// polling: tight cadence right after a successful claim so a burst
/// of forwarded intakes drains fast, slower idle cadence so the
/// worker does not pin its DB pool.
#[derive(Clone, Copy, Debug)]
pub(crate) struct IntakeWorkerConfig {
    /// Sleep between poll attempts when the queue had no rows to claim.
    pub idle_poll_interval: Duration,
    /// Sleep between poll attempts when the previous tick claimed a row.
    pub busy_poll_interval: Duration,
}

impl Default for IntakeWorkerConfig {
    fn default() -> Self {
        Self {
            idle_poll_interval: Duration::from_secs(2),
            busy_poll_interval: Duration::from_millis(250),
        }
    }
}

/// Outcome of running a single claimed row through the executor. Used
/// by the loop to decide the next sleep interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TickOutcome {
    /// No row was eligible for claim. Loop should sleep longer.
    QueueEmpty,
    /// Loop claimed a row and the executor ran (whether it succeeded
    /// or failed terminally). Loop should sleep shorter to drain.
    Processed,
    /// Loop claimed a row but lost the claim before accept. The leader's
    /// stale-claim sweep got there first; this is operationally
    /// distinguishable from a normal Processed for metrics.
    LostClaimBeforeAccept,
    /// The restart admission fence stopped the tick before claim, or returned
    /// its owned claim to pending before accept.
    Cancelled,
}

/// Run a single poll cycle: claim one row, run it through accept →
/// spawn → execute → done/failed transitions, return the outcome.
///
/// This is the unit a poll loop schedules — extracted so tests can
/// drive single-tick scenarios without spawning a long-running task.
pub(crate) async fn run_intake_worker_tick(
    pool: &PgPool,
    http: &Arc<serenity::http::Http>,
    shared: &Arc<SharedData>,
    token: &str,
    target_instance_id: &str,
    provider: &str,
    claim_owner: &str,
    cancel: &AtomicBool,
) -> Result<TickOutcome, sqlx::Error> {
    let Some(_active_tick) = shared.restart.intake_worker_lifecycle.try_begin_tick() else {
        return Ok(TickOutcome::Cancelled);
    };

    // The lifecycle gate is the atomic admission boundary. The cancel check is
    // retained as a belt-and-suspenders fence for signal-driven shutdown.
    if admission_action(
        cancel,
        &shared.restart.intake_worker_lifecycle,
        AdmissionCheckpoint::BeforeClaim,
    ) == AdmissionAction::StopBeforeClaim
    {
        return Ok(TickOutcome::Cancelled);
    }

    let claimed = claim_pending_for_target(pool, target_instance_id, provider, claim_owner).await?;
    let Some(row) = claimed else {
        return Ok(TickOutcome::QueueEmpty);
    };

    // A marker can land while the claim transaction is in flight. Return only
    // this worker's row to pending and stop before payload work or spawning.
    if admission_action(
        cancel,
        &shared.restart.intake_worker_lifecycle,
        AdmissionCheckpoint::AfterClaim,
    ) == AdmissionAction::ReleaseClaim
    {
        release_cancelled_claim(pool, &row, claim_owner).await?;
        return Ok(TickOutcome::Cancelled);
    }

    // Round-2 P0 #2: pre-accept failure (cwd validation, payload
    // conversion) is retryable; post-accept failure is operator-only.
    let request = match intake_request_from_row(&row) {
        Ok(req) => req,
        Err(error) => {
            let msg = format!("payload conversion: {error}");
            tracing::warn!(
                row_id = row.id,
                channel_id = row.channel_id,
                user_msg_id = row.user_msg_id,
                "[intake_worker] pre-accept payload conversion failed: {msg}"
            );
            let _ = mark_failed_pre_accept(pool, row.id, claim_owner, &msg).await?;
            return Ok(TickOutcome::Processed);
        }
    };

    // Payload conversion can race the marker too. Recheck at the final
    // pre-accept boundary; after acceptance the lifecycle guard makes marker
    // acknowledgement wait for execute/final DB transition to drain.
    if admission_action(
        cancel,
        &shared.restart.intake_worker_lifecycle,
        AdmissionCheckpoint::AfterClaim,
    ) == AdmissionAction::ReleaseClaim
    {
        release_cancelled_claim(pool, &row, claim_owner).await?;
        return Ok(TickOutcome::Cancelled);
    }

    // Transition: claimed → accepted. If the sweep beat us to it,
    // ABORT (do not spawn) — Ok(false) means we lost ownership.
    let advanced = mark_accepted(pool, row.id, claim_owner).await?;
    if !advanced {
        tracing::warn!(
            row_id = row.id,
            channel_id = row.channel_id,
            user_msg_id = row.user_msg_id,
            "[intake_worker] lost claim before accept (stale-claim sweep won the race) — aborting before spawn"
        );
        return Ok(TickOutcome::LostClaimBeforeAccept);
    }

    // Transition: accepted → spawned. From here on, a failure is
    // post-accept and is NOT auto-retried.
    let spawned_advanced = mark_spawned(pool, row.id, claim_owner).await?;
    if !spawned_advanced {
        // accepted but couldn't reach spawned. This usually means the
        // operator already force-failed the row via transition 12. Log
        // and bail without invoking the executor.
        tracing::warn!(
            row_id = row.id,
            "[intake_worker] failed to advance accepted → spawned (operator force-fail?) — aborting"
        );
        return Ok(TickOutcome::Processed);
    }

    let result = execute_intake_turn_core(http, shared, token, request).await;

    match result {
        Ok(()) => {
            // `Ok(false)` here means the row left `spawned` while the
            // executor ran (operator force-fail via transition 12, or
            // an external state divergence). Log it so the operator
            // sees the divergence between executor success and DB
            // state — but do NOT escalate; the row's terminal state
            // is whatever the operator put it in.
            let advanced = mark_done(pool, row.id, claim_owner).await?;
            if !advanced {
                tracing::warn!(
                    row_id = row.id,
                    channel_id = row.channel_id,
                    user_msg_id = row.user_msg_id,
                    "[intake_worker] mark_done = false (row no longer in 'spawned'; operator force-fail or DB divergence)"
                );
            }
            Ok(TickOutcome::Processed)
        }
        Err(error) => {
            let msg = format!("turn execution: {error}");
            tracing::warn!(
                row_id = row.id,
                channel_id = row.channel_id,
                user_msg_id = row.user_msg_id,
                "[intake_worker] post-accept turn failed: {msg}"
            );
            let advanced = mark_failed_post_accept(pool, row.id, claim_owner, &msg).await?;
            if !advanced {
                tracing::warn!(
                    row_id = row.id,
                    channel_id = row.channel_id,
                    user_msg_id = row.user_msg_id,
                    "[intake_worker] mark_failed_post_accept = false (row no longer in 'accepted'/'spawned'; operator force-fail or DB divergence)"
                );
            }
            Ok(TickOutcome::Processed)
        }
    }
}

async fn release_cancelled_claim(
    pool: &PgPool,
    row: &IntakeOutboxRow,
    claim_owner: &str,
) -> Result<(), sqlx::Error> {
    let released = return_claimed_to_pending(pool, row.id, claim_owner).await?;
    if !released {
        tracing::warn!(
            row_id = row.id,
            channel_id = row.channel_id,
            user_msg_id = row.user_msg_id,
            "[intake_worker] restart fence could not release claim (ownership/state changed) — aborting before accept"
        );
    }
    Ok(())
}

/// Run the poll loop forever. Returns when `cancel.load(Acquire)` is true.
/// Each tick claims at most one row; backoff between ticks adapts to
/// whether the previous tick had work.
///
/// Cancellation semantics (codex Phase 3 review):
/// - Between ticks (during the adaptive sleep), the loop polls
///   `cancel` in slices of `max(busy_poll_interval, 50ms)` so a
///   flag flip unblocks within ~250ms by default.
/// - Before acceptance, the tick rechecks at the claim boundary and returns an
///   owned claim to `pending` when cancellation races the claim transaction.
/// - After `accepted`, cancellation does not interrupt execution. The active
///   tick guard makes restart acknowledgement wait for the executor and final
///   DB transition to drain. Operators with a stuck turn should use Phase 5's
///   force-fail CLI rather than relying on cancel-mid-execute.
///
/// What flips the cancel flag (codex Phase 5 P1 #3): the bootstrap passes
/// `SharedData.restart.shutting_down`. Both the signal path and the shared
/// gateway/standby restart-marker poller set it before restart acknowledgement,
/// fencing new poll ticks while an active tick drains to completion.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_intake_worker_loop(
    pool: PgPool,
    http: Arc<serenity::http::Http>,
    shared: Arc<SharedData>,
    token: String,
    target_instance_id: String,
    provider: String,
    claim_owner: String,
    config: IntakeWorkerConfig,
    cancel: Arc<AtomicBool>,
) {
    tracing::info!(
        target_instance_id,
        provider,
        claim_owner,
        idle_ms = config.idle_poll_interval.as_millis() as u64,
        busy_ms = config.busy_poll_interval.as_millis() as u64,
        "[intake_worker] poll loop started"
    );
    loop {
        if cancel.load(Ordering::Acquire) {
            tracing::info!(target_instance_id, "[intake_worker] cancelled — exiting");
            return;
        }

        let tick = run_intake_worker_tick(
            &pool,
            &http,
            &shared,
            &token,
            &target_instance_id,
            &provider,
            &claim_owner,
            cancel.as_ref(),
        )
        .await;

        let sleep_for = match tick {
            Ok(TickOutcome::QueueEmpty) => config.idle_poll_interval,
            Ok(TickOutcome::Processed) | Ok(TickOutcome::LostClaimBeforeAccept) => {
                config.busy_poll_interval
            }
            Ok(TickOutcome::Cancelled) => {
                tracing::info!(target_instance_id, "[intake_worker] cancelled — exiting");
                return;
            }
            Err(error) => {
                tracing::warn!("[intake_worker] tick error (pool/sqlx): {error} — backing off");
                config.idle_poll_interval
            }
        };

        // Sleep in small slices so a cancel flag flip unblocks within
        // ~busy_poll_interval rather than waiting out the full idle
        // interval. Codex Phase 3 #1: the slice is clamped to a
        // 50ms minimum so a misconfigured `busy_poll_interval = 0`
        // can never produce a zero-step infinite loop.
        const MIN_SLICE: Duration = Duration::from_millis(50);
        let slice = config.busy_poll_interval.max(MIN_SLICE).min(sleep_for);
        let mut remaining = sleep_for;
        while remaining > Duration::ZERO {
            if cancel.load(Ordering::Acquire) {
                return;
            }
            let step = remaining.min(slice).max(MIN_SLICE);
            tokio::time::sleep(step).await;
            remaining = remaining.saturating_sub(step);
        }
    }
}

/// Convert an `IntakeOutboxRow` into the `IntakeRequest` shape that
/// `execute_intake_turn_core` accepts. Returns a string error on
/// schema-shape problems (unparseable u64, unknown turn_kind, etc.)
/// so the caller can transition the row to `failed_pre_accept` rather
/// than panic. Phase 2-pre.3 codex note follow-up: handles nullable
/// `request_owner_name` by substituting `request_owner_id` (the only
/// other identity available) when DB had NULL.
fn intake_request_from_row(row: &IntakeOutboxRow) -> Result<IntakeRequest, String> {
    let channel_id: u64 = row
        .channel_id
        .parse()
        .map_err(|e| format!("channel_id `{}` not a valid u64: {e}", row.channel_id))?;
    let user_msg_id: u64 = row
        .user_msg_id
        .parse()
        .map_err(|e| format!("user_msg_id `{}` not a valid u64: {e}", row.user_msg_id))?;
    let request_owner: u64 = row.request_owner_id.parse().map_err(|e| {
        format!(
            "request_owner_id `{}` not a valid u64: {e}",
            row.request_owner_id
        )
    })?;
    let turn_kind = parse_turn_kind(&row.turn_kind)?;
    let owner_name = row
        .request_owner_name
        .clone()
        .unwrap_or_else(|| row.request_owner_id.clone());

    Ok(IntakeRequest {
        channel_id: ChannelId::new(channel_id),
        user_msg_id: MessageId::new(user_msg_id),
        request_owner: UserId::new(request_owner),
        request_owner_name: owner_name,
        user_text: row.user_text.clone(),
        reply_to_user_message: row.reply_to_user_message,
        defer_watcher_resume: row.defer_watcher_resume,
        wait_for_completion: row.wait_for_completion,
        merge_consecutive: row.merge_consecutive,
        reply_context: row.reply_context.clone(),
        has_reply_boundary: row.has_reply_boundary,
        dm_hint: row.dm_hint,
        turn_kind,
        // NULL is the pre-0093/older-producer shape: the durable row has no
        // author-classification proof, so fail safe to the historical
        // drop-on-cancel behavior instead of guessing human intent. New
        // leaders always persist Some(true/false).
        preserve_on_cancel: row.preserve_on_cancel.unwrap_or(false),
    })
}

/// Decode the `intake_outbox.turn_kind` text column into the typed
/// enum. The DB schema does not constrain valid values (Phase 1
/// migration uses TEXT), so we tolerate unknown values defensively
/// by erroring rather than silently coercing.
fn parse_turn_kind(raw: &str) -> Result<TurnKind, String> {
    match raw {
        "standard" | "foreground" => Ok(TurnKind::Foreground),
        "background_trigger" => Ok(TurnKind::BackgroundTrigger),
        other => Err(format!("unknown turn_kind value: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_row() -> IntakeOutboxRow {
        IntakeOutboxRow {
            id: 42,
            target_instance_id: "worker-1".to_string(),
            forwarded_by_instance_id: "leader-1".to_string(),
            provider: "claude".to_string(),
            required_labels: serde_json::json!(["unreal"]),
            channel_id: "1234567890".to_string(),
            user_msg_id: "9876543210".to_string(),
            request_owner_id: "555".to_string(),
            request_owner_name: Some("Tester".to_string()),
            user_text: "hello".to_string(),
            reply_context: None,
            has_reply_boundary: false,
            dm_hint: Some(false),
            turn_kind: "standard".to_string(),
            merge_consecutive: false,
            reply_to_user_message: false,
            defer_watcher_resume: false,
            wait_for_completion: false,
            preserve_on_cancel: None,
            agent_id: "agent-x".to_string(),
            status: "claimed".to_string(),
            claim_owner: Some("worker-1.local".to_string()),
            attempt_no: 1,
            parent_outbox_id: None,
            retry_count: 0,
        }
    }

    #[test]
    fn intake_request_from_row_round_trips_basic_fields() {
        let row = fake_row();
        let req = intake_request_from_row(&row).expect("convert ok");
        assert_eq!(req.channel_id.get(), 1234567890);
        assert_eq!(req.user_msg_id.get(), 9876543210);
        assert_eq!(req.request_owner.get(), 555);
        assert_eq!(req.request_owner_name, "Tester");
        assert_eq!(req.user_text, "hello");
        assert_eq!(req.turn_kind, TurnKind::Foreground);
    }

    #[test]
    fn intake_request_from_row_restores_preservation_and_fails_safe_for_null() {
        for (stored, expected) in [(Some(true), true), (Some(false), false), (None, false)] {
            let mut row = fake_row();
            row.preserve_on_cancel = stored;
            let request = intake_request_from_row(&row).expect("convert preservation");
            assert_eq!(request.preserve_on_cancel, expected);
        }
    }

    #[test]
    fn worker_executor_forwards_restored_preservation_instead_of_literal_false() {
        let executor_source = include_str!("../discord/router/message_handler/intake_turn.rs");
        let start = executor_source
            .find("pub(crate) async fn execute_intake_turn_core(")
            .expect("worker executor exists");
        let end = executor_source[start..]
            .find("pub(super) async fn handle_text_message(")
            .map(|offset| start + offset)
            .expect("worker executor has a bounded body");
        let executor = &executor_source[start..end];

        assert!(
            executor.contains("request.preserve_on_cancel,"),
            "worker executor must pass the preservation bit restored from the durable row"
        );
        assert!(
            !executor.contains("request.turn_kind,\n        false,"),
            "worker executor must not restore the historical hardcoded false"
        );
    }

    #[test]
    fn intake_request_from_row_handles_null_request_owner_name() {
        let mut row = fake_row();
        row.request_owner_name = None;
        let req = intake_request_from_row(&row).expect("convert ok");
        // Falls back to request_owner_id so the executor still has a
        // stable string identity to log against.
        assert_eq!(req.request_owner_name, "555");
    }

    #[test]
    fn intake_request_from_row_rejects_non_numeric_channel_id() {
        let mut row = fake_row();
        row.channel_id = "not-a-number".to_string();
        let err = intake_request_from_row(&row).expect_err("must reject");
        assert!(err.contains("channel_id"), "{err}");
    }

    #[test]
    fn intake_request_from_row_rejects_unknown_turn_kind() {
        let mut row = fake_row();
        row.turn_kind = "ghosting".to_string();
        let err = intake_request_from_row(&row).expect_err("must reject");
        assert!(err.contains("turn_kind"), "{err}");
    }

    #[test]
    fn parse_turn_kind_accepts_standard_and_foreground_aliases() {
        assert_eq!(parse_turn_kind("standard").unwrap(), TurnKind::Foreground);
        assert_eq!(parse_turn_kind("foreground").unwrap(), TurnKind::Foreground);
        assert_eq!(
            parse_turn_kind("background_trigger").unwrap(),
            TurnKind::BackgroundTrigger
        );
        assert!(parse_turn_kind("").is_err());
    }

    #[test]
    fn marker_between_loop_check_and_claim_stops_before_claim() {
        let lifecycle = IntakeWorkerLifecycle::default();
        assert!(
            !lifecycle.admission_is_fenced(),
            "the loop-level check observed an open fence"
        );

        lifecycle.fence_admission();
        assert!(
            lifecycle.try_begin_tick().is_none(),
            "the lifecycle gate must suppress a tick admitted after the marker"
        );
    }

    #[test]
    fn restart_fence_rejects_all_future_tick_admissions_after_active_tick_drains() {
        let lifecycle = IntakeWorkerLifecycle::default();
        let active = lifecycle
            .try_begin_tick()
            .expect("tick admitted before restart fence");

        lifecycle.fence_admission();
        assert!(lifecycle.try_begin_tick().is_none());
        drop(active);
        assert!(
            lifecycle.try_begin_tick().is_none(),
            "drain must not reopen restart admission"
        );
    }

    #[test]
    fn fence_between_open_check_and_active_registration_rejects_late_tick() {
        let lifecycle = IntakeWorkerLifecycle::default();
        assert!(!lifecycle.admission_is_fenced());

        lifecycle.fence_admission();
        assert!(
            lifecycle.finish_admission_after_open_check().is_none(),
            "a worker paused after its open check must not register after the fence"
        );
        assert_eq!(lifecycle.active_ticks.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn marker_between_claim_and_accept_requires_release_without_spawn() {
        let cancel = AtomicBool::new(false);
        let lifecycle = IntakeWorkerLifecycle::default();
        assert_eq!(
            admission_action(&cancel, &lifecycle, AdmissionCheckpoint::BeforeClaim),
            AdmissionAction::Proceed
        );

        let mut accepted = false;
        let mut spawned = false;
        let mut returned_pending = false;
        lifecycle.fence_admission();
        match admission_action(&cancel, &lifecycle, AdmissionCheckpoint::AfterClaim) {
            AdmissionAction::ReleaseClaim => returned_pending = true,
            AdmissionAction::Proceed => {
                accepted = true;
                spawned = true;
            }
            AdmissionAction::StopBeforeClaim => unreachable!("row is already claimed"),
        }

        assert!(returned_pending);
        assert!(!accepted, "a fenced claimed row must not be accepted");
        assert!(!spawned, "a fenced claimed row must not spawn");
    }
}

// PG-backed tick coverage is intentionally NOT in this file:
// `run_intake_worker_tick` calls `execute_intake_turn_core` →
// `handle_text_message`, which requires a fully-populated
// `Arc<SharedData>` + Discord runtime. Constructing that from outside
// `services::discord` is not supported today (the prod-shape test
// harness `TestHealthHarness` lived in the removed SQLite-only feature). The
// pre-execute branches we DO want to pin are already
// covered at the helper level:
//   - marker after loop check but before claim: extracted admission policy above
//   - marker after claim but before accept: extracted policy above plus
//     `db::intake_outbox::postgres_tests::cancelled_owned_claim_returns_exact_row_to_pending_without_failure_pollution`
//   - lost-claim race (sweep wins between claim and accept):
//     `db::intake_outbox::postgres_tests::mark_accepted_returns_false_when_sweep_already_reset_the_claim`
//   - 23505 classification, claim ordering, sweep correctness:
//     same module's other 13 tests.
// Phase 4 (leader hook integration) will re-add tick-level integration
// tests once it has access to the harness.
