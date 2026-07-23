use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;
use sqlx::{PgPool, Row as SqlxRow};
use std::{
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use crate::engine::PolicyEngine;

/// Hard cutoff for "stale inflight" detection in the periodic reconcile.
/// Anything older than this with no live tmux pane is considered abandoned.
/// #1076 (905-7): zombie resource sweep cadence.
const STALE_INFLIGHT_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Hard cutoff for orphan `discord_uploads/<channel>/*` files. 7 days matches
/// the default retention hint used by `settings/content.rs::cleanup_old_uploads`
/// for manually-aged attachments, so the periodic sweep is a strict superset.
const STALE_UPLOAD_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const COMPLETED_QUEUE_REVIEW_DRIFT_GRACE: Duration = Duration::from_secs(5 * 60);
const COMPLETED_QUEUE_REVIEW_DRIFT_BATCH_LIMIT: i64 = 50;
const AUTO_QUEUE_PENDING_DELIVERY_ORPHAN_GRACE: Duration = Duration::from_secs(2 * 60);
const AUTO_QUEUE_PENDING_DELIVERY_ORPHAN_STALE_CLAIM: Duration = Duration::from_secs(5 * 60);
const AUTO_QUEUE_PENDING_DELIVERY_ORPHAN_BATCH_LIMIT: i64 = 50;
const BOOT_RECONCILE_OUTBOX_LEASE_TIMEOUT_SECS: i64 = 300;
const DISPATCH_DELIVERY_EVENT_RECONCILE_BATCH_LIMIT: i64 = 500;

const DISPATCH_DELIVERY_MISMATCH_MISSING_TYPED: &str = "missing_typed";
const DISPATCH_DELIVERY_MISMATCH_NOTIFIED_STATUS: &str = "notified_status_mismatch";
const DISPATCH_DELIVERY_MISMATCH_MISSING_KV_META: &str = "missing_kv_meta";

const DISPATCH_DELIVERY_RECOVERY_EXPIRED_RESERVING: &str = "expired_reserving";
const DISPATCH_DELIVERY_RECOVERY_ORPHAN_NOTIFIED: &str = "orphan_notified";
const DISPATCH_DELIVERY_RECOVERY_ORPHAN_TYPED: &str = "orphan_typed";

/// Typed delivery-event statuses that represent a *completed* channel send.
/// `finalize_dispatch_delivery_guard` writes the `dispatch_notified:*` guard for
/// every one of these success paths (not just `sent`), so the reconcile
/// classifier and recovery must both treat the whole set as "delivered" — else
/// a fallback/duplicate/skipped delivery would be flagged as a mismatch forever.
fn is_completed_delivery_status(status: &str) -> bool {
    matches!(status, "sent" | "fallback" | "duplicate" | "skipped")
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct DispatchDeliveryEventReconcileStats {
    pub kv_reserving_checked: usize,
    pub kv_notified_checked: usize,
    pub typed_events_checked: usize,
    pub mismatch_count: usize,
    pub missing_typed: usize,
    pub notified_status_mismatch: usize,
    pub missing_kv_meta: usize,
    /// Expired `dispatch_reserving:*` guard keys deleted this pass (provably
    /// past their `expires_at`, i.e. a finalize never ran). Recovery, not a
    /// mismatch report.
    pub recovered_expired_reserving: usize,
    /// Orphaned `dispatch_notified:*` guard keys reconciled this pass (typed
    /// ledger backfilled/upgraded to a completed send, or the guard pruned when
    /// its dispatch no longer exists). Recovery, not a report.
    pub recovered_orphan_notified: usize,
    /// Typed delivery events with no guard key reconciled this pass (a `sent`
    /// row whose `dispatch_notified:*` guard was lost gets the guard rebuilt; an
    /// expired `reserved` row gets finalized `failed`). Recovery, not a report.
    pub recovered_orphan_typed: usize,
}

impl DispatchDeliveryEventReconcileStats {
    pub(crate) fn touched(&self) -> bool {
        self.mismatch_count > 0 || self.recovered() > 0
    }

    pub(crate) fn recovered(&self) -> usize {
        self.recovered_expired_reserving
            + self.recovered_orphan_notified
            + self.recovered_orphan_typed
    }

    fn record_mismatch(&mut self, kind: &str) {
        self.mismatch_count += 1;
        match kind {
            DISPATCH_DELIVERY_MISMATCH_MISSING_TYPED => self.missing_typed += 1,
            DISPATCH_DELIVERY_MISMATCH_NOTIFIED_STATUS => self.notified_status_mismatch += 1,
            DISPATCH_DELIVERY_MISMATCH_MISSING_KV_META => self.missing_kv_meta += 1,
            _ => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DispatchDeliveryEventMismatch {
    pub dispatch_id: String,
    pub kind: String,
    pub expected_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_status: Option<String>,
}

impl DispatchDeliveryEventMismatch {
    fn missing_typed(dispatch_id: String, expected_status: &'static str) -> Self {
        Self {
            dispatch_id,
            kind: DISPATCH_DELIVERY_MISMATCH_MISSING_TYPED.to_string(),
            expected_status: expected_status.to_string(),
            actual_status: None,
        }
    }

    fn notified_status(dispatch_id: String, actual_status: Option<String>) -> Self {
        Self {
            dispatch_id,
            kind: DISPATCH_DELIVERY_MISMATCH_NOTIFIED_STATUS.to_string(),
            expected_status: "sent".to_string(),
            actual_status,
        }
    }

    fn missing_kv_meta(dispatch_id: String, expected_status: &'static str) -> Self {
        Self {
            dispatch_id,
            kind: DISPATCH_DELIVERY_MISMATCH_MISSING_KV_META.to_string(),
            expected_status: expected_status.to_string(),
            actual_status: Some(expected_status.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DispatchDeliveryEventReconcileReport {
    pub stats: DispatchDeliveryEventReconcileStats,
    pub mismatches: Vec<DispatchDeliveryEventMismatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct DispatchDeliveryEventMismatchMetric {
    pub name: &'static str,
    pub kind: String,
    pub value: u64,
}

#[derive(Debug, sqlx::FromRow)]
struct DeliveryKvGuardRow {
    dispatch_id: String,
    reserving_count: i64,
    notified_count: i64,
    typed_status: Option<String>,
    typed_reserved_until: Option<DateTime<Utc>>,
}

#[derive(Debug, sqlx::FromRow)]
struct DeliveryTypedGuardRow {
    dispatch_id: String,
    typed_status: String,
    reserved_until: Option<DateTime<Utc>>,
    has_reserving: bool,
    has_notified: bool,
}

static DISPATCH_DELIVERY_MISMATCH_COUNTERS: OnceLock<dashmap::DashMap<String, Arc<AtomicU64>>> =
    OnceLock::new();

static DISPATCH_DELIVERY_RECOVERY_COUNTERS: OnceLock<dashmap::DashMap<String, Arc<AtomicU64>>> =
    OnceLock::new();

fn dispatch_delivery_mismatch_counter(kind: &str) -> Arc<AtomicU64> {
    DISPATCH_DELIVERY_MISMATCH_COUNTERS
        .get_or_init(dashmap::DashMap::new)
        .entry(kind.to_string())
        .or_insert_with(|| Arc::new(AtomicU64::new(0)))
        .clone()
}

fn record_dispatch_delivery_mismatch_metric(kind: &str) {
    dispatch_delivery_mismatch_counter(kind).fetch_add(1, Ordering::Relaxed);
}

fn dispatch_delivery_recovery_counter(kind: &str) -> Arc<AtomicU64> {
    DISPATCH_DELIVERY_RECOVERY_COUNTERS
        .get_or_init(dashmap::DashMap::new)
        .entry(kind.to_string())
        .or_insert_with(|| Arc::new(AtomicU64::new(0)))
        .clone()
}

fn record_dispatch_delivery_recovery_metric(kind: &str, count: u64) {
    if count == 0 {
        return;
    }
    dispatch_delivery_recovery_counter(kind).fetch_add(count, Ordering::Relaxed);
}

pub(crate) fn dispatch_delivery_event_recovery_metrics_snapshot()
-> Vec<DispatchDeliveryEventMismatchMetric> {
    let Some(counters) = DISPATCH_DELIVERY_RECOVERY_COUNTERS.get() else {
        return Vec::new();
    };
    let mut rows: Vec<DispatchDeliveryEventMismatchMetric> = counters
        .iter()
        .map(|entry| DispatchDeliveryEventMismatchMetric {
            name: "agentdesk_dispatch_delivery_event_recovered_total",
            kind: entry.key().clone(),
            value: entry.value().load(Ordering::Relaxed),
        })
        .collect();
    rows.sort_by(|a, b| a.kind.cmp(&b.kind));
    rows
}

pub(crate) fn dispatch_delivery_event_mismatch_metrics_snapshot()
-> Vec<DispatchDeliveryEventMismatchMetric> {
    let Some(counters) = DISPATCH_DELIVERY_MISMATCH_COUNTERS.get() else {
        return Vec::new();
    };
    let mut rows: Vec<DispatchDeliveryEventMismatchMetric> = counters
        .iter()
        .map(|entry| DispatchDeliveryEventMismatchMetric {
            name: "agentdesk_dispatch_delivery_event_mismatch_total",
            kind: entry.key().clone(),
            value: entry.value().load(Ordering::Relaxed),
        })
        .collect();
    rows.sort_by(|a, b| a.kind.cmp(&b.kind));
    rows
}

#[cfg(test)]
pub(crate) fn reset_dispatch_delivery_event_mismatch_metrics_for_tests() {
    if let Some(counters) = DISPATCH_DELIVERY_MISMATCH_COUNTERS.get() {
        counters.clear();
    }
    if let Some(counters) = DISPATCH_DELIVERY_RECOVERY_COUNTERS.get() {
        counters.clear();
    }
}

/// Serialize tests that reset and assert the process-global mismatch/recovery
/// metric counters. These counters are shared across the whole test binary, so
/// without a shared lock one test's `reset_..._for_tests()` can clear a counter
/// another test (in this module or in route tests) is concurrently asserting.
/// Acquire this guard at the very top of any such test. Returns a guard that
/// must be held for the duration of the test.
#[cfg(test)]
pub(crate) fn lock_dispatch_delivery_metric_tests() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AutoQueuePendingDeliveryOrphanStats {
    pub candidates: usize,
    pub requeued_notify: usize,
    pub skipped: usize,
}

impl AutoQueuePendingDeliveryOrphanStats {
    pub(crate) fn touched(&self) -> bool {
        self.requeued_notify > 0
    }
}

#[derive(Debug, Clone)]
struct AutoQueuePendingDeliveryOrphanCandidate {
    dispatch_id: String,
    entry_id: String,
    run_id: String,
    outbox_status: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BootReconcileStats {
    pub stale_processing_outbox_reset: usize,
    pub stale_dispatch_reservations_cleared: usize,
    pub missing_notify_outbox_backfilled: usize,
    pub broken_auto_queue_entries_reset: usize,
    pub dispatch_delivery_event_mismatches: usize,
    pub stale_channel_thread_map_entries_cleared: usize,
    pub missing_review_dispatches_refired: usize,
    pub completed_queue_review_drift_recovered: usize,
    pub stale_busy_sessions_reconciled: usize,
}

impl BootReconcileStats {
    pub(crate) fn touched(&self) -> bool {
        self.stale_processing_outbox_reset > 0
            || self.stale_dispatch_reservations_cleared > 0
            || self.missing_notify_outbox_backfilled > 0
            || self.broken_auto_queue_entries_reset > 0
            || self.dispatch_delivery_event_mismatches > 0
            || self.stale_channel_thread_map_entries_cleared > 0
            || self.missing_review_dispatches_refired > 0
            || self.completed_queue_review_drift_recovered > 0
            || self.stale_busy_sessions_reconciled > 0
    }
}

pub(crate) async fn reconcile_boot_db_pg(
    pool: &PgPool,
    current_instance_id: &str,
) -> Result<BootReconcileStats> {
    // Touch next_attempt_at so oldest_pending_age reflects "re-queued at boot",
    // not the original created_at. Without this, rows that were stuck in
    // 'processing' across a restart show up as multi-minute-aged pending rows
    // and the promote health gate fails even though the outbox worker picks
    // them up on the next tick.
    let stale_processing_outbox_reset = sqlx::query(
        "UPDATE dispatch_outbox
            SET status = 'pending',
                claimed_at = NULL,
                claim_owner = NULL,
                next_attempt_at = NOW()
          WHERE status = 'processing'
            AND (
                claimed_at IS NULL
                OR claimed_at < NOW() - ($2::BIGINT * INTERVAL '1 second')
                OR claim_owner = $1
            )",
    )
    .bind(current_instance_id)
    .bind(BOOT_RECONCILE_OUTBOX_LEASE_TIMEOUT_SECS)
    .execute(pool)
    .await
    .map(|r| r.rows_affected() as usize)
    .unwrap_or(0);

    let dispatch_delivery_event_mismatches = reconcile_dispatch_delivery_events_pg(pool)
        .await
        .map(|stats| stats.mismatch_count)
        .unwrap_or_else(|error| {
            tracing::warn!(
                target: "reconcile",
                %error,
                "[dispatch-delivery-reconcile] boot mismatch reconcile failed"
            );
            0
        });

    let stale_dispatch_reservations_cleared =
        sqlx::query("DELETE FROM kv_meta WHERE key LIKE 'dispatch_reserving:%'")
            .execute(pool)
            .await
            .map(|r| r.rows_affected() as usize)
            .unwrap_or(0);

    let missing_notify_outbox_backfilled = backfill_missing_notify_outbox_pg(pool).await?;
    let broken_auto_queue_entries_reset = reset_broken_auto_queue_entries_pg(pool).await?;
    let stale_busy_sessions_reconciled =
        crate::services::stale_turn_reconciler::reconcile_stale_turns_pg(pool).await?;

    Ok(BootReconcileStats {
        stale_processing_outbox_reset,
        stale_dispatch_reservations_cleared,
        missing_notify_outbox_backfilled,
        broken_auto_queue_entries_reset,
        dispatch_delivery_event_mismatches,
        stale_channel_thread_map_entries_cleared: 0,
        missing_review_dispatches_refired: 0,
        completed_queue_review_drift_recovered: 0,
        stale_busy_sessions_reconciled,
    })
}

pub(crate) async fn reconcile_boot_runtime(
    engine: &PolicyEngine,
    pg_pool: Option<&PgPool>,
    current_instance_id: &str,
) -> Result<BootReconcileStats> {
    let mut stats = if let Some(pool) = pg_pool {
        reconcile_boot_db_pg(pool, current_instance_id).await?
    } else {
        {
            return Err(anyhow!("Postgres pool required for boot reconcile"));
        }
    };

    stats.missing_review_dispatches_refired = if let Some(pool) = pg_pool {
        refire_missing_review_dispatches_pg(pool, engine).await?
    } else {
        0
    };
    stats.completed_queue_review_drift_recovered = if let Some(pool) = pg_pool {
        reconcile_completed_queue_review_drift_pg(pool, engine).await?
    } else {
        0
    };

    if stats.touched() {
        tracing::info!(
            "[boot-reconcile] reset_processing={} cleared_reservations={} missing_notify={} broken_auto_queue={} dispatch_delivery_mismatches={} cleared_thread_map={} refired_review={} recovered_review_drift={} reconciled_stale_busy_sessions={}",
            stats.stale_processing_outbox_reset,
            stats.stale_dispatch_reservations_cleared,
            stats.missing_notify_outbox_backfilled,
            stats.broken_auto_queue_entries_reset,
            stats.dispatch_delivery_event_mismatches,
            stats.stale_channel_thread_map_entries_cleared,
            stats.missing_review_dispatches_refired,
            stats.completed_queue_review_drift_recovered,
            stats.stale_busy_sessions_reconciled
        );
    }

    Ok(stats)
}

pub(crate) async fn reconcile_completed_queue_review_drift_pg(
    pool: &PgPool,
    engine: &PolicyEngine,
) -> Result<usize> {
    let drift_candidates = completed_queue_review_drift_candidates_pg(pool).await?;
    recover_completed_queue_review_drift_pg(pool, engine, drift_candidates).await
}

pub(crate) async fn reconcile_dispatch_delivery_events_pg(
    pool: &PgPool,
) -> Result<DispatchDeliveryEventReconcileStats> {
    let report = dispatch_delivery_event_reconcile_report_pg(pool).await?;
    for mismatch in &report.mismatches {
        record_dispatch_delivery_mismatch_metric(&mismatch.kind);
        tracing::warn!(
            target: "reconcile",
            dispatch_id = %mismatch.dispatch_id,
            kind = %mismatch.kind,
            expected_status = %mismatch.expected_status,
            actual_status = mismatch.actual_status.as_deref().unwrap_or("missing"),
            "[dispatch-delivery-reconcile] delivery guard mismatch"
        );
    }

    let mut stats = report.stats;

    // Recovery: bring the guard keys and the typed delivery ledger back into
    // agreement so the same mismatch is not re-counted on every tick (#3008).
    // Both steps are idempotent and only touch deliveries that are no longer in
    // flight, so they are safe to run unconditionally each pass.
    stats.recovered_expired_reserving = recover_expired_dispatch_reserving_pg(pool).await?;
    stats.recovered_orphan_notified = recover_orphan_dispatch_notified_pg(pool).await?;
    stats.recovered_orphan_typed = recover_orphan_typed_delivery_events_pg(pool).await?;

    record_dispatch_delivery_recovery_metric(
        DISPATCH_DELIVERY_RECOVERY_EXPIRED_RESERVING,
        stats.recovered_expired_reserving as u64,
    );
    record_dispatch_delivery_recovery_metric(
        DISPATCH_DELIVERY_RECOVERY_ORPHAN_NOTIFIED,
        stats.recovered_orphan_notified as u64,
    );
    record_dispatch_delivery_recovery_metric(
        DISPATCH_DELIVERY_RECOVERY_ORPHAN_TYPED,
        stats.recovered_orphan_typed as u64,
    );

    if stats.touched() {
        tracing::warn!(
            target: "reconcile",
            kv_reserving_checked = stats.kv_reserving_checked,
            kv_notified_checked = stats.kv_notified_checked,
            typed_events_checked = stats.typed_events_checked,
            mismatch_count = stats.mismatch_count,
            missing_typed = stats.missing_typed,
            notified_status_mismatch = stats.notified_status_mismatch,
            missing_kv_meta = stats.missing_kv_meta,
            recovered_expired_reserving = stats.recovered_expired_reserving,
            recovered_orphan_notified = stats.recovered_orphan_notified,
            recovered_orphan_typed = stats.recovered_orphan_typed,
            "[dispatch-delivery-reconcile] mismatch scan + recovery completed"
        );
    }

    Ok(stats)
}

/// Reclaim `dispatch_reserving:*` guard keys whose reservation provably expired
/// (`expires_at <= NOW()`), i.e. their owning delivery never reached finalize.
///
/// For each expired key we mirror the delivery guard's own expiry handling: the
/// still-`reserved` typed delivery row is flipped to `failed` (matching
/// `recover_expired_dispatch_delivery_reservation_pg`) and then the kv guard key
/// is deleted. Doing both halves in one shot is what makes the recovery durable
/// — deleting only the kv key would leave the expired typed `reserved` row
/// behind, which the typed scan would then report as `missing_kv_meta` forever.
///
/// An in-flight reservation always has `expires_at > NOW()` and is never
/// touched. Keys with a NULL `expires_at` are anomalous and left alone — they
/// are not provably orphaned.
///
/// Returns the number of guard keys reclaimed.
async fn recover_expired_dispatch_reserving_pg(pool: &PgPool) -> Result<usize> {
    // Expire the typed reservation first so the typed ledger is consistent even
    // if the kv delete below were to fail mid-pass; on the next tick the kv key
    // would simply be reclaimed again.
    sqlx::query(
        "UPDATE dispatch_delivery_events e
            SET status = 'failed',
                error = COALESCE(e.error, 'delivery reservation expired before finalize'),
                result_json = CASE
                    WHEN e.result_json = '{}'::jsonb THEN jsonb_build_object(
                        'status', 'failed',
                        'dispatch_id', e.dispatch_id,
                        'action', 'notify',
                        'detail', 'delivery reservation expired before finalize'
                    )
                    ELSE e.result_json
                END,
                reserved_until = NULL,
                updated_at = NOW()
          WHERE e.operation = 'send'
            AND e.target_kind = 'channel'
            AND e.status = 'reserved'
            AND e.reserved_until IS NOT NULL
            AND e.reserved_until <= NOW()
            AND EXISTS (
                SELECT 1
                  FROM kv_meta m
                 WHERE m.key = 'dispatch_reserving:' || e.dispatch_id
                   AND m.expires_at IS NOT NULL
                   AND m.expires_at <= NOW()
            )",
    )
    .execute(pool)
    .await
    .map_err(|error| anyhow!("expire typed reservations for orphaned reserving guards: {error}"))?;

    // Delete the kv guard only when no *still-active* typed reservation exists
    // for the dispatch. The guard writes `kv_meta.expires_at` and the typed
    // `reserved_until` in two separate statements (both NOW()+5min), so the kv
    // side can expire a hair earlier; deleting purely on `expires_at <= NOW()`
    // could drop the guard while the typed row is still active, after which the
    // typed scan would report `missing_kv_meta` permanently. Anchoring the
    // delete to "typed reservation is also done" closes that window.
    sqlx::query(
        "DELETE FROM kv_meta m
          WHERE m.key LIKE 'dispatch\\_reserving:%' ESCAPE '\\'
            AND m.expires_at IS NOT NULL
            AND m.expires_at <= NOW()
            AND NOT EXISTS (
                SELECT 1
                  FROM dispatch_delivery_events e
                 WHERE e.dispatch_id = SUBSTRING(m.key FROM LENGTH('dispatch_reserving:') + 1)
                   AND e.operation = 'send'
                   AND e.target_kind = 'channel'
                   AND e.status = 'reserved'
                   AND (e.reserved_until IS NULL OR e.reserved_until > NOW())
            )",
    )
    .execute(pool)
    .await
    .map(|result| result.rows_affected() as usize)
    .map_err(|error| anyhow!("delete expired dispatch reserving guard keys: {error}"))
}

/// Reconcile `dispatch_notified:*` guard keys whose typed delivery ledger does
/// not yet record a completed channel send.
///
/// A `dispatch_notified:*` key is written by the delivery guard ONLY after the
/// transport send returned success, so the key itself is the durable proof that
/// the channel send happened — it is the dedupe guard that stops a retry from
/// re-sending the same dispatch. The mismatch backlog comes from a crash in the
/// narrow window between writing the notified key and finalizing the typed event
/// (the typed row is then left `reserved`, or later flipped to `failed` by the
/// expiry sweep, or never written at all).
///
/// The safe recovery is therefore to BACKFILL the typed event to `sent` to match
/// the proof the notified key already carries — never to delete the key, which
/// would drop the only idempotency guard and allow a duplicate send. We only
/// touch keys with no completed-send typed row AND no still-active reservation
/// (`reserved`, `reserved_until > NOW()`), so an in-flight delivery is never
/// disturbed.
///
/// Returns the number of guard keys whose typed ledger was reconciled.
async fn recover_orphan_dispatch_notified_pg(pool: &PgPool) -> Result<usize> {
    // 1. Upgrade a stale, settled typed row to `sent` when the notified guard
    //    proves the send actually landed but the typed finalize did not record a
    //    completed status. Two crash windows produce this:
    //      * the expiry sweep already flipped the orphaned reservation to
    //        `failed` (latest status = `failed`), or
    //      * the typed finalize never ran at all and the reservation simply
    //        aged out (latest status = `reserved` with `reserved_until` past).
    //
    //    Guards that keep this race-free against a still-running delivery:
    //      * We only act when the latest row is settled — `failed`, or `reserved`
    //        whose `reserved_until` has passed. A live finalizer for a send that
    //        outlived its 5-minute window still owns a NON-expired reservation
    //        (or is mid-INSERT of one); the `NOT EXISTS active reservation` guard
    //        below excludes exactly that case.
    //      * We skip when a completed delivery row already exists (the real
    //        finalizer landed); a placeholder would only add a contradictory row.
    let upgraded = sqlx::query(
        "WITH targets AS (
            SELECT SUBSTRING(m.key FROM LENGTH('dispatch_notified:') + 1) AS dispatch_id
              FROM kv_meta m
             WHERE m.key LIKE 'dispatch\\_notified:%' ESCAPE '\\'
        ),
        latest AS (
            SELECT DISTINCT ON (e.dispatch_id) e.id, e.dispatch_id, e.status, e.reserved_until
              FROM dispatch_delivery_events e
              JOIN targets ON targets.dispatch_id = e.dispatch_id
             WHERE e.operation = 'send'
               AND e.target_kind = 'channel'
             ORDER BY e.dispatch_id, e.updated_at DESC, e.id DESC
        )
        UPDATE dispatch_delivery_events e
            SET status = 'sent',
                error = NULL,
                reserved_until = NULL,
                -- Overwrite result_json unconditionally: the prior payload may
                -- still report `status: failed`/`reserved`, so preserving it
                -- would leave a `sent` event whose JSON contradicts its status.
                result_json = jsonb_build_object(
                    'status', 'sent',
                    'dispatch_id', e.dispatch_id,
                    'action', 'notify',
                    'detail', 'reconciled from dispatch_notified delivery guard'
                ),
                updated_at = NOW()
          FROM latest
         WHERE e.id = latest.id
           AND (
               latest.status = 'failed'
               OR (
                   latest.status = 'reserved'
                   AND latest.reserved_until IS NOT NULL
                   AND latest.reserved_until <= NOW()
               )
           )
           -- Skip when a completed delivery row already exists (the real
           -- finalizer landed): no upgrade needed, and a placeholder would only
           -- add a contradictory row.
           AND NOT EXISTS (
               SELECT 1 FROM dispatch_delivery_events c
                WHERE c.dispatch_id = e.dispatch_id
                  AND c.operation = 'send'
                  AND c.target_kind = 'channel'
                  AND c.status IN ('sent', 'fallback', 'duplicate', 'skipped')
           )
           -- Skip when an ACTIVE reservation is present: a live finalizer is
           -- mid-flight for this dispatch (its send outlived the 5-minute
           -- window), so the typed ledger is not settled yet.
           AND NOT EXISTS (
               SELECT 1 FROM dispatch_delivery_events r
                WHERE r.dispatch_id = e.dispatch_id
                  AND r.operation = 'send'
                  AND r.target_kind = 'channel'
                  AND r.status = 'reserved'
                  AND (r.reserved_until IS NULL OR r.reserved_until > NOW())
           )",
    )
    .execute(pool)
    .await
    .map(|result| result.rows_affected() as usize)
    .map_err(|error| anyhow!("upgrade stale typed events for notified guards: {error}"))?;

    // 2. Backfill a typed `sent` row for notified keys that have no typed
    //    delivery event at all (the typed write never landed). The INSERT is
    //    scoped to dispatches that still exist in `task_dispatches`: the typed
    //    table FKs to it `ON DELETE CASCADE`, so backfilling a row for a deleted
    //    dispatch would raise an FK error and abort the whole reconcile pass.
    let inserted = sqlx::query(
        "INSERT INTO dispatch_delivery_events (
            dispatch_id,
            correlation_id,
            semantic_event_id,
            operation,
            target_kind,
            status,
            attempt,
            result_json
         )
         SELECT dispatch_id,
                'dispatch:' || dispatch_id,
                'dispatch:' || dispatch_id || ':notify',
                'send',
                'channel',
                'sent',
                1,
                jsonb_build_object(
                    'status', 'sent',
                    'dispatch_id', dispatch_id,
                    'action', 'notify',
                    'detail', 'reconciled from dispatch_notified delivery guard'
                )
           FROM (
               SELECT SUBSTRING(m.key FROM LENGTH('dispatch_notified:') + 1) AS dispatch_id
                 FROM kv_meta m
                WHERE m.key LIKE 'dispatch\\_notified:%' ESCAPE '\\'
           ) targets
          WHERE EXISTS (
              SELECT 1 FROM task_dispatches td WHERE td.id = targets.dispatch_id
          )
            AND NOT EXISTS (
              SELECT 1
                FROM dispatch_delivery_events e
               WHERE e.dispatch_id = targets.dispatch_id
                 AND e.operation = 'send'
                 AND e.target_kind = 'channel'
          )
         ON CONFLICT DO NOTHING",
    )
    .execute(pool)
    .await
    .map(|result| result.rows_affected() as usize)
    .map_err(|error| anyhow!("backfill typed sent events for notified guards: {error}"))?;

    // 3. A `dispatch_notified:*` key whose `task_dispatches` row is gone is truly
    //    orphaned: the dispatch (and its CASCADE-deleted delivery events) no
    //    longer exist, so the dedupe guard protects nothing and there is nothing
    //    to re-send. Reclaim those keys so they stop pinning the mismatch scan.
    let pruned = sqlx::query(
        "DELETE FROM kv_meta m
          WHERE m.key LIKE 'dispatch\\_notified:%' ESCAPE '\\'
            AND NOT EXISTS (
                SELECT 1
                  FROM task_dispatches td
                 WHERE td.id = SUBSTRING(m.key FROM LENGTH('dispatch_notified:') + 1)
            )",
    )
    .execute(pool)
    .await
    .map(|result| result.rows_affected() as usize)
    .map_err(|error| anyhow!("prune notified guards for deleted dispatches: {error}"))?;

    Ok(upgraded + inserted + pruned)
}

/// Reconcile typed delivery events that have NO guard key in `kv_meta` — the
/// `missing_kv_meta` mismatch class.
///
/// Two sub-cases, matching `classify_delivery_typed_guard_mismatches`:
///
///   * Latest typed status is `sent` (a completed send) but neither
///     `dispatch_reserving:*` nor `dispatch_notified:*` exists — the notified
///     dedupe guard was lost (kv GC, or the guard write never landed). The typed
///     `sent` row is the source of truth that the send happened, so we rebuild
///     the missing `dispatch_notified:*` guard. Re-creating the guard is safe:
///     it only ever prevents re-sending something already delivered.
///   * Latest typed status is `reserved` and the reservation has expired
///     (`reserved_until <= NOW()`) but no guard key exists — an abandoned
///     reservation whose kv key already vanished. We finalize it to `failed`,
///     mirroring the delivery guard's own expiry handling, after which it is no
///     longer reported.
///
/// In-flight reservations (`reserved` with `reserved_until > NOW()`) are never
/// touched. Returns the number of typed rows reconciled.
async fn recover_orphan_typed_delivery_events_pg(pool: &PgPool) -> Result<usize> {
    // Latest typed event per dispatch with no guard key of either kind.
    let latest_without_guard = "WITH latest AS (
            SELECT DISTINCT ON (e.dispatch_id)
                   e.id, e.dispatch_id, e.status, e.reserved_until
              FROM dispatch_delivery_events e
             WHERE e.operation = 'send'
               AND e.target_kind = 'channel'
             ORDER BY e.dispatch_id, e.updated_at DESC, e.id DESC
        )
        SELECT latest.id, latest.dispatch_id, latest.status, latest.reserved_until
          FROM latest
         WHERE NOT EXISTS (
             SELECT 1 FROM kv_meta m
              WHERE m.key = 'dispatch_reserving:' || latest.dispatch_id
                 OR m.key = 'dispatch_notified:' || latest.dispatch_id
         )";

    // 1. Rebuild the lost `dispatch_notified:*` guard for completed sends. This
    //    must cover every completed-delivery status the guard itself writes a
    //    notified key for (sent/fallback/duplicate/skipped), or those rows would
    //    keep reporting `missing_kv_meta`.
    let notified_rebuilt = sqlx::query(&format!(
        "INSERT INTO kv_meta (key, value)
         SELECT 'dispatch_notified:' || src.dispatch_id, src.dispatch_id
           FROM ({latest_without_guard}) src
          WHERE src.status IN ('sent', 'fallback', 'duplicate', 'skipped')
         ON CONFLICT (key) DO NOTHING"
    ))
    .execute(pool)
    .await
    .map(|result| result.rows_affected() as usize)
    .map_err(|error| anyhow!("rebuild notified guard for guard-less completed events: {error}"))?;

    // 2. Finalize abandoned expired reservations whose guard key is already gone.
    let reservations_failed = sqlx::query(&format!(
        "UPDATE dispatch_delivery_events e
            SET status = 'failed',
                error = COALESCE(e.error, 'delivery reservation expired before finalize'),
                result_json = CASE
                    WHEN e.result_json = '{{}}'::jsonb THEN jsonb_build_object(
                        'status', 'failed',
                        'dispatch_id', e.dispatch_id,
                        'action', 'notify',
                        'detail', 'delivery reservation expired before finalize'
                    )
                    ELSE e.result_json
                END,
                reserved_until = NULL,
                updated_at = NOW()
           FROM ({latest_without_guard}) src
          WHERE e.id = src.id
            AND src.status = 'reserved'
            AND src.reserved_until IS NOT NULL
            AND src.reserved_until <= NOW()"
    ))
    .execute(pool)
    .await
    .map(|result| result.rows_affected() as usize)
    .map_err(|error| anyhow!("finalize guard-less expired reservations: {error}"))?;

    Ok(notified_rebuilt + reservations_failed)
}

pub(crate) async fn dispatch_delivery_event_reconcile_report_pg(
    pool: &PgPool,
) -> Result<DispatchDeliveryEventReconcileReport> {
    let mut stats = DispatchDeliveryEventReconcileStats::default();
    let mut mismatches = Vec::new();

    let mut cursor = String::new();
    loop {
        let rows = fetch_delivery_kv_guard_batch_pg(
            pool,
            &cursor,
            DISPATCH_DELIVERY_EVENT_RECONCILE_BATCH_LIMIT,
        )
        .await?;
        if rows.is_empty() {
            break;
        }
        if let Some(last_row) = rows.last() {
            cursor.clone_from(&last_row.dispatch_id);
        } else {
            cursor.clear();
        }
        for row in rows {
            stats.kv_reserving_checked += row.reserving_count.max(0) as usize;
            stats.kv_notified_checked += row.notified_count.max(0) as usize;
            classify_delivery_kv_guard_mismatches(&mut stats, &mut mismatches, row);
        }
    }

    cursor.clear();
    loop {
        let rows = fetch_delivery_typed_guard_batch_pg(
            pool,
            &cursor,
            DISPATCH_DELIVERY_EVENT_RECONCILE_BATCH_LIMIT,
        )
        .await?;
        if rows.is_empty() {
            break;
        }
        if let Some(last_row) = rows.last() {
            cursor.clone_from(&last_row.dispatch_id);
        } else {
            cursor.clear();
        }
        for row in rows {
            stats.typed_events_checked += 1;
            classify_delivery_typed_guard_mismatches(&mut stats, &mut mismatches, row);
        }
    }

    Ok(DispatchDeliveryEventReconcileReport { stats, mismatches })
}

fn classify_delivery_kv_guard_mismatches(
    stats: &mut DispatchDeliveryEventReconcileStats,
    mismatches: &mut Vec<DispatchDeliveryEventMismatch>,
    row: DeliveryKvGuardRow,
) {
    let has_notified = row.notified_count > 0;
    let typed_status = row.typed_status.as_deref();
    if typed_status.is_none() {
        let expected = if has_notified { "sent" } else { "reserved" };
        let mismatch = DispatchDeliveryEventMismatch::missing_typed(row.dispatch_id, expected);
        stats.record_mismatch(&mismatch.kind);
        mismatches.push(mismatch);
        return;
    }

    let typed_is_completed = typed_status.is_some_and(is_completed_delivery_status);
    if has_notified && !typed_is_completed {
        if typed_status == Some("reserved")
            && row
                .typed_reserved_until
                .as_ref()
                .is_some_and(|reserved_until| *reserved_until > Utc::now())
        {
            return;
        }
        let mismatch =
            DispatchDeliveryEventMismatch::notified_status(row.dispatch_id, row.typed_status);
        stats.record_mismatch(&mismatch.kind);
        mismatches.push(mismatch);
        return;
    }
}

fn classify_delivery_typed_guard_mismatches(
    stats: &mut DispatchDeliveryEventReconcileStats,
    mismatches: &mut Vec<DispatchDeliveryEventMismatch>,
    row: DeliveryTypedGuardRow,
) {
    if row.has_reserving || row.has_notified {
        return;
    }
    let status = row.typed_status.as_str();
    if status == "reserved" {
        if row
            .reserved_until
            .as_ref()
            .is_some_and(|reserved_until| *reserved_until > Utc::now())
        {
            return;
        }
        let mismatch = DispatchDeliveryEventMismatch::missing_kv_meta(row.dispatch_id, "reserved");
        stats.record_mismatch(&mismatch.kind);
        mismatches.push(mismatch);
    } else if is_completed_delivery_status(status) {
        // A completed delivery (sent/fallback/duplicate/skipped) with no guard
        // key lost its `dispatch_notified:*` dedupe guard; recovery rebuilds it.
        let mismatch = DispatchDeliveryEventMismatch::missing_kv_meta(row.dispatch_id, "sent");
        stats.record_mismatch(&mismatch.kind);
        mismatches.push(mismatch);
    }
}

async fn fetch_delivery_kv_guard_batch_pg(
    pool: &PgPool,
    cursor: &str,
    limit: i64,
) -> Result<Vec<DeliveryKvGuardRow>> {
    sqlx::query_as::<_, DeliveryKvGuardRow>(
        "WITH kv_guards AS (
            SELECT SUBSTRING(key FROM LENGTH('dispatch_reserving:') + 1) AS dispatch_id,
                   'reserving' AS guard_kind
              FROM kv_meta
             WHERE key LIKE 'dispatch\\_reserving:%' ESCAPE '\\'
               AND SUBSTRING(key FROM LENGTH('dispatch_reserving:') + 1) > $1
            UNION ALL
            SELECT SUBSTRING(key FROM LENGTH('dispatch_notified:') + 1) AS dispatch_id,
                   'notified' AS guard_kind
              FROM kv_meta
             WHERE key LIKE 'dispatch\\_notified:%' ESCAPE '\\'
               AND SUBSTRING(key FROM LENGTH('dispatch_notified:') + 1) > $1
        ),
        grouped AS (
            SELECT dispatch_id,
                   SUM(CASE WHEN guard_kind = 'reserving' THEN 1 ELSE 0 END)::BIGINT AS reserving_count,
                   SUM(CASE WHEN guard_kind = 'notified' THEN 1 ELSE 0 END)::BIGINT AS notified_count
              FROM kv_guards
             GROUP BY dispatch_id
             ORDER BY dispatch_id
             LIMIT $2
        )
        SELECT grouped.dispatch_id,
               grouped.reserving_count,
               grouped.notified_count,
               latest.status AS typed_status,
               latest.reserved_until AS typed_reserved_until
          FROM grouped
          LEFT JOIN LATERAL (
              SELECT status, reserved_until
                FROM dispatch_delivery_events
               WHERE dispatch_id = grouped.dispatch_id
                 AND operation = 'send'
                 AND target_kind = 'channel'
               ORDER BY updated_at DESC, id DESC
               LIMIT 1
          ) latest ON TRUE
         ORDER BY grouped.dispatch_id",
    )
    .bind(cursor)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("query dispatch delivery kv guard reconcile batch: {error}"))
}

async fn fetch_delivery_typed_guard_batch_pg(
    pool: &PgPool,
    cursor: &str,
    limit: i64,
) -> Result<Vec<DeliveryTypedGuardRow>> {
    sqlx::query_as::<_, DeliveryTypedGuardRow>(
        "WITH candidate_ids AS (
            SELECT DISTINCT dispatch_id
              FROM dispatch_delivery_events
             WHERE dispatch_id > $1
               AND operation = 'send'
               AND target_kind = 'channel'
             ORDER BY dispatch_id
             LIMIT $2
        ),
        latest AS (
            SELECT DISTINCT ON (events.dispatch_id)
                   events.dispatch_id,
                   events.status,
                   events.reserved_until
              FROM dispatch_delivery_events events
              JOIN candidate_ids ON candidate_ids.dispatch_id = events.dispatch_id
             WHERE events.operation = 'send'
               AND events.target_kind = 'channel'
             ORDER BY events.dispatch_id, events.updated_at DESC, events.id DESC
        )
        SELECT latest.dispatch_id,
               latest.status AS typed_status,
               latest.reserved_until,
               EXISTS (
                   SELECT 1
                     FROM kv_meta
                    WHERE key = 'dispatch_reserving:' || latest.dispatch_id
               ) AS has_reserving,
               EXISTS (
                   SELECT 1
                     FROM kv_meta
                    WHERE key = 'dispatch_notified:' || latest.dispatch_id
               ) AS has_notified
          FROM latest
         ORDER BY latest.dispatch_id",
    )
    .bind(cursor)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("query dispatch delivery typed reconcile batch: {error}"))
}

pub(crate) async fn reconcile_auto_queue_pending_delivery_orphans_pg(
    pool: &PgPool,
) -> Result<AutoQueuePendingDeliveryOrphanStats> {
    let candidates = auto_queue_pending_delivery_orphan_candidates_pg(pool).await?;
    let mut stats = AutoQueuePendingDeliveryOrphanStats {
        candidates: candidates.len(),
        ..AutoQueuePendingDeliveryOrphanStats::default()
    };

    for candidate in candidates {
        match requeue_auto_queue_pending_delivery_orphan_notify_pg(pool, &candidate.dispatch_id)
            .await
        {
            Ok(true) => {
                stats.requeued_notify += 1;
                tracing::info!(
                    target: "reconcile",
                    run_id = %candidate.run_id,
                    entry_id = %candidate.entry_id,
                    dispatch_id = %candidate.dispatch_id,
                    outbox_status = candidate.outbox_status.as_deref().unwrap_or("missing"),
                    "[auto-queue-reconcile] requeued orphan pending delivery notify"
                );
            }
            Ok(false) => {
                stats.skipped += 1;
            }
            Err(error) => {
                stats.skipped += 1;
                tracing::warn!(
                    target: "reconcile",
                    run_id = %candidate.run_id,
                    entry_id = %candidate.entry_id,
                    dispatch_id = %candidate.dispatch_id,
                    %error,
                    "[auto-queue-reconcile] failed to requeue orphan pending delivery notify"
                );
            }
        }
    }

    if stats.touched() {
        tracing::info!(
            target: "reconcile",
            candidates = stats.candidates,
            requeued_notify = stats.requeued_notify,
            skipped = stats.skipped,
            "[auto-queue-reconcile] pending delivery orphan reconcile completed"
        );
    }

    Ok(stats)
}

async fn backfill_missing_notify_outbox_pg(pool: &PgPool) -> Result<usize> {
    sqlx::query(
        "INSERT INTO dispatch_outbox (
            dispatch_id, action, agent_id, card_id, title, status, required_capabilities
         )
         SELECT
            td.id,
            'notify',
            td.to_agent_id,
            td.kanban_card_id,
            td.title,
            'pending',
            td.required_capabilities
         FROM task_dispatches td
         WHERE td.status IN ('pending', 'dispatched')
           AND NOT EXISTS (
             SELECT 1 FROM dispatch_outbox o
             WHERE o.dispatch_id = td.id AND o.action = 'notify'
           )
         ON CONFLICT (dispatch_id, action) WHERE action IN ('notify', 'followup')
         DO NOTHING",
    )
    .execute(pool)
    .await
    .map(|result| result.rows_affected() as usize)
    .map_err(anyhow::Error::from)
}

async fn reset_broken_auto_queue_entries_pg(pool: &PgPool) -> Result<usize> {
    sqlx::query(
        "UPDATE auto_queue_entries e
         SET status = 'pending',
             dispatch_id = NULL,
             slot_index = NULL,
             dispatched_at = NULL,
             completed_at = NULL
         WHERE e.status = 'dispatched'
           AND (
             e.dispatch_id IS NULL
             OR TRIM(e.dispatch_id) = ''
             OR NOT EXISTS (
               SELECT 1
               FROM task_dispatches td
               WHERE td.id = e.dispatch_id
                 AND td.status NOT IN ('cancelled', 'failed', 'completed')
             )
           )",
    )
    .execute(pool)
    .await
    .map(|result| result.rows_affected() as usize)
    .map_err(anyhow::Error::from)
}

async fn auto_queue_pending_delivery_orphan_candidates_pg(
    pool: &PgPool,
) -> Result<Vec<AutoQueuePendingDeliveryOrphanCandidate>> {
    let rows = sqlx::query(
        "WITH latest_notify AS (
            SELECT DISTINCT ON (dispatch_id)
                   id,
                   dispatch_id,
                   status,
                   claimed_at,
                   claim_owner
              FROM dispatch_outbox
             WHERE action = 'notify'
             ORDER BY dispatch_id, id DESC
         )
         SELECT td.id AS dispatch_id,
                e.id AS entry_id,
                e.run_id AS run_id,
                o.status AS outbox_status
           FROM task_dispatches td
           JOIN auto_queue_entries e
             ON e.dispatch_id = td.id
            AND e.status = 'dispatched'
           JOIN auto_queue_runs r
             ON r.id = e.run_id
            AND r.status = 'active'
           LEFT JOIN latest_notify o
             ON o.dispatch_id = td.id
          WHERE td.status = 'pending'
            AND (COALESCE(NULLIF(td.context, ''), '{}')::jsonb)->>'auto_queue' = 'true'
            AND COALESCE(e.dispatched_at, td.created_at, NOW())
                <= NOW() - ($1::BIGINT * INTERVAL '1 second')
            AND NOT EXISTS (
                SELECT 1
                  FROM sessions s
                 WHERE s.active_dispatch_id = td.id
                   AND COALESCE(s.status, '') IN ('turn_active', 'working')
            )
            AND (
                o.id IS NULL
                OR o.status = 'failed'
                OR (
                    o.status = 'processing'
                    AND (
                        o.claimed_at IS NULL
                        OR o.claimed_at <= NOW() - ($2::BIGINT * INTERVAL '1 second')
                    )
                )
                OR (
                    o.status = 'pending'
                    AND o.claim_owner IS NOT NULL
                    AND (
                        o.claimed_at IS NULL
                        OR o.claimed_at <= NOW() - ($2::BIGINT * INTERVAL '1 second')
                    )
                )
            )
          ORDER BY COALESCE(e.dispatched_at, td.created_at) ASC, td.id ASC
          LIMIT $3",
    )
    .bind(AUTO_QUEUE_PENDING_DELIVERY_ORPHAN_GRACE.as_secs() as i64)
    .bind(AUTO_QUEUE_PENDING_DELIVERY_ORPHAN_STALE_CLAIM.as_secs() as i64)
    .bind(AUTO_QUEUE_PENDING_DELIVERY_ORPHAN_BATCH_LIMIT)
    .fetch_all(pool)
    .await?;

    rows.into_iter()
        .map(|row| {
            Ok(AutoQueuePendingDeliveryOrphanCandidate {
                dispatch_id: row.try_get("dispatch_id")?,
                entry_id: row.try_get("entry_id")?,
                run_id: row.try_get("run_id")?,
                outbox_status: row.try_get("outbox_status")?,
            })
        })
        .collect::<std::result::Result<Vec<_>, sqlx::Error>>()
        .map_err(anyhow::Error::from)
}

async fn requeue_auto_queue_pending_delivery_orphan_notify_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<bool> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|error| anyhow!("begin orphan pending delivery requeue tx: {error}"))?;

    let dispatch = sqlx::query(
        "SELECT td.to_agent_id,
                td.kanban_card_id,
                td.title,
                td.required_capabilities
           FROM task_dispatches td
           JOIN auto_queue_entries e
             ON e.dispatch_id = td.id
            AND e.status = 'dispatched'
           JOIN auto_queue_runs r
             ON r.id = e.run_id
            AND r.status = 'active'
          WHERE td.id = $1
            AND td.status = 'pending'
            AND (COALESCE(NULLIF(td.context, ''), '{}')::jsonb)->>'auto_queue' = 'true'
            AND COALESCE(e.dispatched_at, td.created_at, NOW())
                <= NOW() - ($2::BIGINT * INTERVAL '1 second')
            AND NOT EXISTS (
                SELECT 1
                  FROM sessions s
                 WHERE s.active_dispatch_id = td.id
                   AND COALESCE(s.status, '') IN ('turn_active', 'working')
            )
          FOR UPDATE OF td, e",
    )
    .bind(dispatch_id)
    .bind(AUTO_QUEUE_PENDING_DELIVERY_ORPHAN_GRACE.as_secs() as i64)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| anyhow!("lock orphan pending delivery dispatch {dispatch_id}: {error}"))?;

    let Some(dispatch) = dispatch else {
        tx.rollback()
            .await
            .map_err(|error| anyhow!("rollback skipped orphan requeue {dispatch_id}: {error}"))?;
        return Ok(false);
    };

    let agent_id = dispatch
        .try_get::<Option<String>, _>("to_agent_id")?
        .ok_or_else(|| anyhow!("postgres dispatch {dispatch_id} missing to_agent_id"))?;
    let card_id = dispatch
        .try_get::<Option<String>, _>("kanban_card_id")?
        .ok_or_else(|| anyhow!("postgres dispatch {dispatch_id} missing kanban_card_id"))?;
    let title = dispatch
        .try_get::<Option<String>, _>("title")?
        .ok_or_else(|| anyhow!("postgres dispatch {dispatch_id} missing title"))?;
    let required_capabilities: Option<serde_json::Value> =
        dispatch.try_get("required_capabilities")?;

    let outbox = sqlx::query(
        "SELECT id,
                (
                    status = 'failed'
                    OR (
                        status = 'processing'
                        AND (
                            claimed_at IS NULL
                            OR claimed_at <= NOW() - ($2::BIGINT * INTERVAL '1 second')
                        )
                    )
                    OR (
                        status = 'pending'
                        AND claim_owner IS NOT NULL
                        AND (
                            claimed_at IS NULL
                            OR claimed_at <= NOW() - ($2::BIGINT * INTERVAL '1 second')
                        )
                    )
                ) AS needs_requeue
           FROM dispatch_outbox
          WHERE dispatch_id = $1
            AND action = 'notify'
          ORDER BY id DESC
          LIMIT 1
          FOR UPDATE",
    )
    .bind(dispatch_id)
    .bind(AUTO_QUEUE_PENDING_DELIVERY_ORPHAN_STALE_CLAIM.as_secs() as i64)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| anyhow!("lock notify outbox for orphan dispatch {dispatch_id}: {error}"))?;

    let changed = if let Some(outbox) = outbox {
        let needs_requeue: bool = outbox.try_get("needs_requeue")?;
        if !needs_requeue {
            false
        } else {
            let outbox_id: i64 = outbox.try_get("id")?;
            sqlx::query(
                "UPDATE dispatch_outbox
                    SET agent_id = $2,
                        card_id = $3,
                        title = $4,
                        required_capabilities = $5,
                        status = 'pending',
                        retry_count = 0,
                        next_attempt_at = NULL,
                        processed_at = NULL,
                        error = NULL,
                        delivery_status = NULL,
                        delivery_result = NULL,
                        claimed_at = NULL,
                        claim_owner = NULL
                  WHERE id = $1",
            )
            .bind(outbox_id)
            .bind(&agent_id)
            .bind(&card_id)
            .bind(&title)
            .bind(required_capabilities.as_ref())
            .execute(&mut *tx)
            .await
            .map_err(|error| {
                anyhow!("reset notify outbox for orphan dispatch {dispatch_id}: {error}")
            })?
            .rows_affected()
                > 0
        }
    } else {
        sqlx::query(
            "INSERT INTO dispatch_outbox (
                dispatch_id,
                action,
                agent_id,
                card_id,
                title,
                status,
                retry_count,
                required_capabilities
             ) VALUES (
                $1, 'notify', $2, $3, $4, 'pending', 0, $5
             )
             ON CONFLICT DO NOTHING",
        )
        .bind(dispatch_id)
        .bind(&agent_id)
        .bind(&card_id)
        .bind(&title)
        .bind(required_capabilities.as_ref())
        .execute(&mut *tx)
        .await
        .map_err(|error| {
            anyhow!("insert notify outbox for orphan dispatch {dispatch_id}: {error}")
        })?
        .rows_affected()
            > 0
    };

    if changed {
        tx.commit().await.map_err(|error| {
            anyhow!("commit orphan pending delivery requeue {dispatch_id}: {error}")
        })?;
    } else {
        tx.rollback()
            .await
            .map_err(|error| anyhow!("rollback unchanged orphan requeue {dispatch_id}: {error}"))?;
    }

    Ok(changed)
}

async fn refire_missing_review_dispatches_pg(
    pool: &PgPool,
    engine: &PolicyEngine,
) -> Result<usize> {
    crate::pipeline::ensure_loaded();

    let cards: Vec<(String, String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT id, status, repo_id, assigned_agent_id
         FROM kanban_cards
         WHERE status NOT IN ('done', 'backlog', 'ready')",
    )
    .fetch_all(pool)
    .await?;

    let mut candidates = Vec::new();
    for (card_id, status, repo_id, agent_id) in cards {
        let effective =
            crate::pipeline::resolve_for_card_pg(pool, repo_id.as_deref(), agent_id.as_deref())
                .await;
        let is_review_state = effective.hooks_for_state(&status).map_or(false, |hooks| {
            hooks.on_enter.iter().any(|name| name == "OnReviewEnter")
        });
        if !is_review_state {
            continue;
        }

        let has_review_dispatch = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(
                SELECT 1 FROM task_dispatches
                WHERE kanban_card_id = $1
                  AND dispatch_type IN ('review', 'review-decision')
                  AND status IN ('pending', 'dispatched')
            )",
        )
        .bind(&card_id)
        .fetch_one(pool)
        .await
        .unwrap_or(false);
        if !has_review_dispatch {
            candidates.push(card_id);
        }
    }

    let mut refired = 0usize;
    for card_id in candidates {
        if let Err(e) =
            engine.fire_hook_by_name_blocking("OnReviewEnter", json!({ "card_id": card_id }))
        {
            tracing::warn!(
                "[boot-reconcile] failed to re-fire OnReviewEnter for card {}: {e}",
                card_id
            );
            continue;
        }
        crate::kanban::drain_hook_side_effects_with_backends(engine);

        let has_review_dispatch = active_review_dispatch_exists_pg(pool, &card_id).await?;
        if has_review_dispatch {
            refired += 1;
        } else {
            tracing::warn!(
                "[boot-reconcile] OnReviewEnter re-fired for card {} but no active review dispatch was created",
                card_id
            );
        }
    }

    Ok(refired)
}

async fn recover_completed_queue_review_drift_pg(
    pool: &PgPool,
    engine: &PolicyEngine,
    drift_candidates: Vec<String>,
) -> Result<usize> {
    let mut recovered = 0usize;

    for card_id in drift_candidates {
        if !completed_queue_review_drift_candidate_exists_pg(pool, &card_id).await? {
            continue;
        }

        match crate::kanban::transition_status_with_opts_pg(
            pool,
            engine,
            &card_id,
            "review",
            "review_drift_reconcile",
            crate::engine::transition::ForceIntent::SystemRecovery,
        )
        .await
        {
            Ok(_) => {
                crate::kanban::drain_hook_side_effects_with_backends(engine);
            }
            Err(error) => {
                tracing::warn!(
                    "[review-drift-reconcile] failed to transition completed queue card {} to review: {error}",
                    card_id
                );
                continue;
            }
        }

        let has_review_dispatch = active_review_dispatch_exists_pg(pool, &card_id).await?;
        if has_review_dispatch {
            recovered += 1;
        } else {
            tracing::warn!(
                "[review-drift-reconcile] transitioned completed queue card {} to review but no active review dispatch was created",
                card_id
            );
        }
    }

    Ok(recovered)
}

async fn completed_queue_review_drift_candidates_pg(pool: &PgPool) -> Result<Vec<String>> {
    completed_queue_review_drift_candidates_with_limit_pg(
        pool,
        COMPLETED_QUEUE_REVIEW_DRIFT_GRACE.as_secs() as i64,
        COMPLETED_QUEUE_REVIEW_DRIFT_BATCH_LIMIT,
    )
    .await
}

async fn completed_queue_review_drift_candidate_exists_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<bool> {
    let candidates = completed_queue_review_drift_candidates_base_pg(
        pool,
        Some(card_id),
        COMPLETED_QUEUE_REVIEW_DRIFT_GRACE.as_secs() as i64,
        1,
    )
    .await?;
    Ok(!candidates.is_empty())
}

async fn completed_queue_review_drift_candidates_with_limit_pg(
    pool: &PgPool,
    grace_seconds: i64,
    limit: i64,
) -> Result<Vec<String>> {
    completed_queue_review_drift_candidates_base_pg(pool, None, grace_seconds, limit).await
}

async fn completed_queue_review_drift_candidates_base_pg(
    pool: &PgPool,
    card_id: Option<&str>,
    grace_seconds: i64,
    limit: i64,
) -> Result<Vec<String>> {
    sqlx::query_scalar(
        "SELECT DISTINCT c.id
         FROM kanban_cards c
         JOIN auto_queue_entries e ON e.kanban_card_id = c.id
         WHERE c.status = 'in_progress'
           AND e.status = 'done'
           AND e.completed_at <= NOW() - ($1::BIGINT * INTERVAL '1 second')
           AND ($2::TEXT IS NULL OR c.id = $2)
           AND NOT EXISTS (
             SELECT 1
             FROM task_dispatches td
             WHERE td.kanban_card_id = c.id
               AND td.status IN ('pending', 'dispatched')
           )
           AND NOT EXISTS (
             SELECT 1
             FROM auto_queue_entries active_e
             WHERE active_e.kanban_card_id = c.id
               AND active_e.status IN ('pending', 'dispatched')
           )
         ORDER BY c.id
         LIMIT $3",
    )
    .bind(grace_seconds)
    .bind(card_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(anyhow::Error::from)
}

async fn active_review_dispatch_exists_pg(pool: &PgPool, card_id: &str) -> Result<bool> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
            SELECT 1 FROM task_dispatches
            WHERE kanban_card_id = $1
              AND dispatch_type IN ('review', 'review-decision')
              AND status IN ('pending', 'dispatched')
        )",
    )
    .bind(card_id)
    .fetch_one(pool)
    .await
    .map_err(anyhow::Error::from)
}

// ============================================================================
// #1076 (905-7): zombie resource sweep — periodic reconcile
// ============================================================================
//
// Four zombie classes are covered by `reconcile_zombie_resources()`:
//
//   1. Orphan tmux sessions (AgentDesk-* with no owning channel entry AND no
//      live pane) — already handled on boot by `cleanup_orphan_tmux_sessions`;
//      the periodic path re-checks hourly so long-running processes do not
//      accumulate leaks between restarts. The periodic path is a no-op unless
//      a live `SharedData` was registered via `register_shared_runtime_handle`.
//   2. Stale inflight state files (> `STALE_INFLIGHT_MAX_AGE` and restart_mode
//      is None, i.e. never planned for resume) — deletes the JSON file so the
//      next boot does not try to resume an abandoned turn.
//   3. Zombie DashMap entries (intake dedupe / api_timestamps / tmux_relay_coords
//      growing unboundedly when channels disappear). The sweep trims any entry
//      whose key channel no longer has a matching live tmux session + no
//      active inflight.
//   4. Unrelocated `discord_uploads/<channel>/*` files older than
//      `STALE_UPLOAD_MAX_AGE` — the Discord upload content-addressed mirror
//      migrated off disk-per-channel but legacy files can linger when a
//      migration step aborts.
//
// Each helper returns a count, aggregated into `ZombieReconcileStats` for
// log emission. The callers must tolerate Postgres / SQLite / tmux being
// unavailable — all helpers degrade gracefully (zero count + warn log).

/// Aggregate stats from one run of [`reconcile_zombie_resources`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ZombieReconcileStats {
    pub orphan_tmux_killed: usize,
    pub stale_inflight_removed: usize,
    pub zombie_dashmap_trimmed: usize,
    pub stale_uploads_removed: usize,
}

impl ZombieReconcileStats {
    pub(crate) fn total(&self) -> usize {
        self.orphan_tmux_killed
            + self.stale_inflight_removed
            + self.zombie_dashmap_trimmed
            + self.stale_uploads_removed
    }
}

/// Remove inflight state JSON files older than [`STALE_INFLIGHT_MAX_AGE`] that
/// have no `restart_mode` assignment (i.e. were never scheduled for resume).
/// Returns the number of files deleted. Safe to call without a Postgres pool.
pub(crate) fn sweep_stale_inflight_files() -> usize {
    let Some(root) =
        crate::config::runtime_root().map(|p| p.join("runtime").join("discord_inflight"))
    else {
        return 0;
    };
    sweep_stale_inflight_files_at(&root, STALE_INFLIGHT_MAX_AGE)
}

fn inflight_remove_log_channel_id(path: &std::path::Path) -> u64 {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.parse::<u64>().ok())
        .unwrap_or(0)
}

fn inflight_remove_log_user_msg_id(path: &std::path::Path) -> u64 {
    #[derive(serde::Deserialize)]
    struct InflightRemoveLogFields {
        #[serde(default)]
        user_msg_id: u64,
    }

    std::fs::read_to_string(path)
        .ok()
        .and_then(|body| serde_json::from_str::<InflightRemoveLogFields>(&body).ok())
        .map(|fields| fields.user_msg_id)
        .unwrap_or(0)
}

fn log_stale_inflight_remove(path: &std::path::Path) {
    let provider = path
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("unknown");
    tracing::warn!(
        target: "agentdesk::inflight_remove",
        provider = %provider,
        channel_id = inflight_remove_log_channel_id(path),
        user_msg_id = inflight_remove_log_user_msg_id(path),
        reason = "sweep_stale_inflight_files",
        path = %path.display(),
        "discord inflight state row removal"
    );
}

pub(crate) fn sweep_stale_inflight_files_at(root: &std::path::Path, max_age: Duration) -> usize {
    use std::fs;
    use std::time::SystemTime;

    if !root.exists() {
        return 0;
    }

    let Ok(provider_dirs) = fs::read_dir(root) else {
        return 0;
    };

    let now = SystemTime::now();
    let mut removed = 0usize;

    for provider in provider_dirs.filter_map(|e| e.ok()) {
        let pdir = provider.path();
        if !pdir.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&pdir) else {
            continue;
        };
        for entry in files.filter_map(|e| e.ok()) {
            let fpath = entry.path();
            if !fpath.is_file() {
                continue;
            }
            // Only consider .json state files — skip anything unexpected.
            if fpath.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let age_ok = fs::metadata(&fpath)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|mtime| now.duration_since(mtime).ok())
                .map(|age| age >= max_age)
                .unwrap_or(false);
            if !age_ok {
                continue;
            }
            // Only remove when restart_mode is absent and the file is not an
            // externally adopted rebind-origin placeholder. Planned restart
            // rows are owned by drain/hot-swap lifecycle; externally adopted
            // rebind-origin rows are managed by the placeholder sweeper.
            #[derive(serde::Deserialize)]
            struct LifecycleExt {
                restart_mode: Option<serde_json::Value>,
                #[serde(default)]
                rebind_origin: bool,
                turn_source: Option<String>,
            }
            let is_managed_lifecycle = fs::read_to_string(&fpath)
                .ok()
                .and_then(|body| {
                    let v = serde_json::from_str::<LifecycleExt>(&body).ok()?;
                    let planned_restart = v.restart_mode.is_some_and(|rm| !rm.is_null());
                    if planned_restart {
                        return Some(true);
                    }
                    if !v.rebind_origin {
                        return Some(false);
                    }
                    match v.turn_source.as_deref() {
                        Some("external_adopted") => Some(true),
                        None => Some(backfill_legacy_rebind_origin_turn_source(&fpath)),
                        _ => Some(false),
                    }
                })
                .unwrap_or(false);
            if is_managed_lifecycle {
                continue;
            }
            log_stale_inflight_remove(&fpath);
            if fs::remove_file(&fpath).is_ok() {
                removed += 1;
                tracing::info!(
                    target: "reconcile",
                    path = %fpath.display(),
                    "[zombie-reconcile] removed stale inflight state file"
                );
            }
        }
    }
    removed
}

fn backfill_legacy_rebind_origin_turn_source(path: &std::path::Path) -> bool {
    let Ok(_lock) = crate::services::discord::lock_inflight_state_path(path) else {
        return false;
    };
    let Ok(body) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&body) else {
        return false;
    };
    let Some(object) = value.as_object_mut() else {
        return false;
    };
    if object.get("rebind_origin") != Some(&serde_json::Value::Bool(true)) {
        return false;
    }
    match object
        .get("turn_source")
        .and_then(serde_json::Value::as_str)
    {
        Some("external_adopted") => return true,
        Some(_) => return false,
        None => {}
    }
    object.insert(
        "turn_source".to_string(),
        serde_json::Value::String("external_adopted".to_string()),
    );
    let Ok(updated) = serde_json::to_string_pretty(&value) else {
        return false;
    };
    crate::services::discord::runtime_store::atomic_write(path, &format!("{updated}\n")).is_ok()
}

#[cfg(test)]
mod stale_inflight_sweep_tests {
    use super::sweep_stale_inflight_files_at;
    use filetime::{FileTime, set_file_mtime};
    use std::{
        fs,
        path::PathBuf,
        time::{Duration, SystemTime},
    };

    fn write_stale_inflight(root: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let provider_dir = root.join("claude");
        fs::create_dir_all(&provider_dir).expect("provider dir");
        let path = provider_dir.join(format!("{name}.json"));
        fs::write(&path, body).expect("write inflight");
        let old_mtime = FileTime::from_system_time(SystemTime::now() - Duration::from_secs(3600));
        set_file_mtime(&path, old_mtime).expect("set mtime");
        path
    }

    #[test]
    fn stale_sweep_preserves_external_rebind_origin_only() {
        let root = tempfile::tempdir().expect("temp root");
        let external = write_stale_inflight(
            root.path(),
            "external",
            r#"{"rebind_origin":true,"turn_source":"external_adopted"}"#,
        );
        let monitor = write_stale_inflight(
            root.path(),
            "monitor",
            r#"{"rebind_origin":true,"turn_source":"monitor_triggered"}"#,
        );

        let removed = sweep_stale_inflight_files_at(root.path(), Duration::from_secs(60));

        assert_eq!(removed, 1);
        assert!(
            external.exists(),
            "external adopted rebind-origin is placeholder-managed"
        );
        assert!(
            !monitor.exists(),
            "monitor-triggered stale rebind-origin remains sweepable"
        );
    }

    #[test]
    fn stale_sweep_preserves_legacy_rebind_origin_without_turn_source() {
        let root = tempfile::tempdir().expect("temp root");
        let legacy = write_stale_inflight(root.path(), "legacy", r#"{"rebind_origin":true}"#);

        let removed = sweep_stale_inflight_files_at(root.path(), Duration::from_secs(60));

        assert_eq!(removed, 0);
        assert!(
            legacy.exists(),
            "legacy rebind-origin rows predate turn_source and remain placeholder-managed"
        );
        let updated: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&legacy).expect("legacy body"))
                .expect("updated legacy json");
        assert_eq!(updated["turn_source"], "external_adopted");
    }

    #[test]
    fn stale_sweep_preserves_planned_restart_rows() {
        let root = tempfile::tempdir().expect("temp root");
        let planned = write_stale_inflight(
            root.path(),
            "planned",
            r#"{"restart_mode":"hot_swap","rebind_origin":false}"#,
        );

        let removed = sweep_stale_inflight_files_at(root.path(), Duration::from_secs(60));

        assert_eq!(removed, 0);
        assert!(planned.exists());
    }
}

/// Remove `discord_uploads/<channel>/*` files older than
/// [`STALE_UPLOAD_MAX_AGE`]. Returns the number of files removed.
pub(crate) fn sweep_stale_discord_uploads() -> usize {
    let Some(root) =
        crate::config::runtime_root().map(|p| p.join("runtime").join("discord_uploads"))
    else {
        return 0;
    };
    sweep_stale_discord_uploads_at(&root, STALE_UPLOAD_MAX_AGE)
}

pub(crate) fn sweep_stale_discord_uploads_at(root: &std::path::Path, max_age: Duration) -> usize {
    use std::fs;
    use std::time::SystemTime;

    if !root.exists() {
        return 0;
    }
    let Ok(channels) = fs::read_dir(root) else {
        return 0;
    };

    let now = SystemTime::now();
    let mut removed = 0usize;

    for ch in channels.filter_map(|e| e.ok()) {
        let ch_path = ch.path();
        if !ch_path.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&ch_path) else {
            continue;
        };
        for entry in files.filter_map(|e| e.ok()) {
            let fpath = entry.path();
            if !fpath.is_file() {
                continue;
            }
            let stale = fs::metadata(&fpath)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|mtime| now.duration_since(mtime).ok())
                .map(|age| age >= max_age)
                .unwrap_or(false);
            if stale && fs::remove_file(&fpath).is_ok() {
                removed += 1;
            }
        }
        // Drop the channel dir if now empty.
        if fs::read_dir(&ch_path)
            .ok()
            .map(|mut it| it.next().is_none())
            .unwrap_or(false)
        {
            let _ = fs::remove_dir(&ch_path);
        }
    }
    removed
}

/// Run the full zombie sweep (stale inflight + stale uploads).
///
/// The tmux orphan cleanup + DashMap trim require a live `Arc<SharedData>`
/// handle which is owned by the Discord bot runtime; those two counters are
/// filled in by the Discord-side runtime loop in
/// `services/discord/mod.rs::run_discord_zombie_sweep_tick`. The periodic
/// maintenance job records whatever the file-system and PG layers can do
/// without the Discord runtime handle, which means it is safe to run before
/// (and independently of) the bot coming up.
pub(crate) async fn reconcile_zombie_resources() -> ZombieReconcileStats {
    let stale_inflight_removed = tokio::task::spawn_blocking(sweep_stale_inflight_files)
        .await
        .unwrap_or(0);
    let stale_uploads_removed = tokio::task::spawn_blocking(sweep_stale_discord_uploads)
        .await
        .unwrap_or(0);

    let stats = ZombieReconcileStats {
        orphan_tmux_killed: 0,
        stale_inflight_removed,
        zombie_dashmap_trimmed: 0,
        stale_uploads_removed,
    };

    if stats.total() > 0 {
        tracing::info!(
            target: "reconcile",
            orphan_tmux = stats.orphan_tmux_killed,
            stale_inflight = stats.stale_inflight_removed,
            zombie_dashmap = stats.zombie_dashmap_trimmed,
            stale_uploads = stats.stale_uploads_removed,
            "[zombie-reconcile] sweep completed"
        );
    } else {
        tracing::debug!(
            target: "reconcile",
            "[zombie-reconcile] sweep completed (no zombies found)"
        );
    }

    stats
}

#[cfg(test)]
mod dispatch_delivery_reconcile_tests {
    use super::*;
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
    };

    #[derive(Clone)]
    struct TestLogWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for TestLogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct TestPostgresDb {
        _lock: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn try_create() -> Option<Self> {
            let lock = crate::db::postgres::lock_test_lifecycle();
            let admin_url = postgres_admin_database_url();
            let database_name = format!(
                "agentdesk_delivery_reconcile_{}",
                uuid::Uuid::new_v4().simple()
            );
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            if let Err(error) = crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "dispatch delivery reconcile tests",
            )
            .await
            {
                eprintln!("skipping postgres-backed dispatch delivery reconcile test: {error}");
                drop(lock);
                return None;
            }

            Some(Self {
                _lock: lock,
                admin_url,
                database_name,
                database_url,
            })
        }

        async fn connect_and_migrate(&self) -> PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "dispatch delivery reconcile tests",
            )
            .await
            .unwrap()
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "dispatch delivery reconcile tests",
            )
            .await
            .unwrap();
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
    }

    async fn seed_dispatch(pool: &PgPool, dispatch_id: &str) {
        sqlx::query(
            "INSERT INTO task_dispatches (id, status, title)
             VALUES ($1, 'pending', 'Delivery reconcile test')",
        )
        .bind(dispatch_id)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_delivery_event(pool: &PgPool, dispatch_id: &str, status: &str) {
        sqlx::query(
            "INSERT INTO dispatch_delivery_events (
                dispatch_id,
                correlation_id,
                semantic_event_id,
                operation,
                target_kind,
                status,
                attempt,
                result_json,
                reserved_until,
                created_at,
                updated_at
             ) VALUES (
                $1,
                'dispatch:' || $1,
                'dispatch:' || $1 || ':notify',
                'send',
                'channel',
                $2,
                1,
                '{}'::jsonb,
                CASE WHEN $2 = 'reserved' THEN NOW() - INTERVAL '1 minute' ELSE NULL END,
                NOW() - INTERVAL '1 minute',
                NOW() - INTERVAL '1 minute'
             )",
        )
        .bind(dispatch_id)
        .bind(status)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn insert_kv(pool: &PgPool, key: &str, dispatch_id: &str) {
        sqlx::query("INSERT INTO kv_meta (key, value) VALUES ($1, $2)")
            .bind(key)
            .bind(dispatch_id)
            .execute(pool)
            .await
            .unwrap();
    }

    /// Insert a kv_meta row whose `expires_at` is offset from NOW by
    /// `expires_in_seconds` (negative => already expired).
    async fn insert_kv_with_expiry(
        pool: &PgPool,
        key: &str,
        dispatch_id: &str,
        expires_in_seconds: i64,
    ) {
        sqlx::query(
            "INSERT INTO kv_meta (key, value, expires_at)
             VALUES ($1, $2, NOW() + ($3::BIGINT * INTERVAL '1 second'))",
        )
        .bind(key)
        .bind(dispatch_id)
        .bind(expires_in_seconds)
        .execute(pool)
        .await
        .unwrap();
    }

    async fn kv_key_exists(pool: &PgPool, key: &str) -> bool {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM kv_meta WHERE key = $1")
            .bind(key)
            .fetch_one(pool)
            .await
            .unwrap()
            > 0
    }

    #[tokio::test]
    async fn boot_reconcile_keeps_non_expired_processing_row_owned_by_peer_pg() {
        let Some(pg_db) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO dispatch_outbox (
                dispatch_id, action, status, claimed_at, claim_owner
             ) VALUES (
                'dispatch-live-peer-lease', 'notify', 'processing', NOW(), 'peer-instance'
             )",
        )
        .execute(&pool)
        .await
        .expect("seed peer-owned processing outbox row");

        let stats = reconcile_boot_db_pg(&pool, "current-instance")
            .await
            .expect("boot reconcile succeeds");
        assert_eq!(stats.stale_processing_outbox_reset, 0);

        let row = sqlx::query(
            "SELECT status, claim_owner, claimed_at
               FROM dispatch_outbox
              WHERE dispatch_id = 'dispatch-live-peer-lease'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.try_get::<String, _>("status").unwrap(), "processing");
        assert_eq!(
            row.try_get::<String, _>("claim_owner").unwrap(),
            "peer-instance"
        );
        assert!(
            row.try_get::<Option<DateTime<Utc>>, _>("claimed_at")
                .unwrap()
                .is_some()
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[test]
    fn dispatch_delivery_reconcile_classifies_rows_without_postgres() {
        let mut stats = DispatchDeliveryEventReconcileStats::default();
        let mut mismatches = Vec::new();

        classify_delivery_kv_guard_mismatches(
            &mut stats,
            &mut mismatches,
            DeliveryKvGuardRow {
                dispatch_id: "dispatch-missing-typed".to_string(),
                reserving_count: 1,
                notified_count: 0,
                typed_status: None,
                typed_reserved_until: None,
            },
        );
        classify_delivery_kv_guard_mismatches(
            &mut stats,
            &mut mismatches,
            DeliveryKvGuardRow {
                dispatch_id: "dispatch-notified-status".to_string(),
                reserving_count: 0,
                notified_count: 1,
                typed_status: Some("failed".to_string()),
                typed_reserved_until: None,
            },
        );
        classify_delivery_typed_guard_mismatches(
            &mut stats,
            &mut mismatches,
            DeliveryTypedGuardRow {
                dispatch_id: "dispatch-typed-only".to_string(),
                typed_status: "sent".to_string(),
                reserved_until: None,
                has_reserving: false,
                has_notified: false,
            },
        );

        assert_eq!(stats.mismatch_count, 3);
        assert_eq!(stats.missing_typed, 1);
        assert_eq!(stats.notified_status_mismatch, 1);
        assert_eq!(stats.missing_kv_meta, 1);
        assert_eq!(
            mismatches
                .iter()
                .map(|mismatch| mismatch.kind.as_str())
                .collect::<Vec<_>>(),
            vec![
                DISPATCH_DELIVERY_MISMATCH_MISSING_TYPED,
                DISPATCH_DELIVERY_MISMATCH_NOTIFIED_STATUS,
                DISPATCH_DELIVERY_MISMATCH_MISSING_KV_META,
            ]
        );
    }

    #[test]
    fn dispatch_delivery_reconcile_treats_all_completed_statuses_as_delivered() {
        // The delivery guard writes `dispatch_notified:*` for every success path
        // (sent/fallback/duplicate/skipped), so a notified key paired with any of
        // those typed statuses is a legitimate completed delivery, NOT a mismatch.
        for status in ["sent", "fallback", "duplicate", "skipped"] {
            let mut stats = DispatchDeliveryEventReconcileStats::default();
            let mut mismatches = Vec::new();
            classify_delivery_kv_guard_mismatches(
                &mut stats,
                &mut mismatches,
                DeliveryKvGuardRow {
                    dispatch_id: format!("dispatch-completed-{status}"),
                    reserving_count: 0,
                    notified_count: 1,
                    typed_status: Some(status.to_string()),
                    typed_reserved_until: None,
                },
            );
            assert_eq!(
                stats.mismatch_count, 0,
                "notified + typed '{status}' must not be flagged as a mismatch"
            );
            assert!(mismatches.is_empty());

            // And the typed-side scan must classify a guard-less completed
            // delivery as missing_kv_meta (so recovery rebuilds the guard).
            let mut typed_stats = DispatchDeliveryEventReconcileStats::default();
            let mut typed_mismatches = Vec::new();
            classify_delivery_typed_guard_mismatches(
                &mut typed_stats,
                &mut typed_mismatches,
                DeliveryTypedGuardRow {
                    dispatch_id: format!("dispatch-guardless-{status}"),
                    typed_status: status.to_string(),
                    reserved_until: None,
                    has_reserving: false,
                    has_notified: false,
                },
            );
            assert_eq!(
                typed_stats.missing_kv_meta, 1,
                "guard-less completed '{status}' delivery must report missing_kv_meta"
            );
        }
    }

    #[tokio::test]
    async fn dispatch_delivery_reconcile_classifies_three_mismatch_kinds_pg() {
        let Some(pg_db) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;

        seed_dispatch(&pool, "dispatch-missing-typed").await;
        insert_kv(
            &pool,
            "dispatch_reserving:dispatch-missing-typed",
            "dispatch-missing-typed",
        )
        .await;

        seed_dispatch(&pool, "dispatch-notified-status").await;
        insert_kv(
            &pool,
            "dispatch_notified:dispatch-notified-status",
            "dispatch-notified-status",
        )
        .await;
        insert_delivery_event(&pool, "dispatch-notified-status", "failed").await;

        seed_dispatch(&pool, "dispatch-typed-only").await;
        insert_delivery_event(&pool, "dispatch-typed-only", "sent").await;

        seed_dispatch(&pool, "dispatch-failed-typed-only").await;
        insert_delivery_event(&pool, "dispatch-failed-typed-only", "failed").await;

        let report = dispatch_delivery_event_reconcile_report_pg(&pool)
            .await
            .unwrap();

        assert_eq!(report.stats.mismatch_count, 3);
        assert_eq!(report.stats.missing_typed, 1);
        assert_eq!(report.stats.notified_status_mismatch, 1);
        assert_eq!(report.stats.missing_kv_meta, 1);
        assert!(report.mismatches.iter().any(|mismatch| {
            mismatch.dispatch_id == "dispatch-missing-typed"
                && mismatch.kind == DISPATCH_DELIVERY_MISMATCH_MISSING_TYPED
                && mismatch.expected_status == "reserved"
        }));
        assert!(report.mismatches.iter().any(|mismatch| {
            mismatch.dispatch_id == "dispatch-notified-status"
                && mismatch.kind == DISPATCH_DELIVERY_MISMATCH_NOTIFIED_STATUS
                && mismatch.actual_status.as_deref() == Some("failed")
        }));
        assert!(report.mismatches.iter().any(|mismatch| {
            mismatch.dispatch_id == "dispatch-typed-only"
                && mismatch.kind == DISPATCH_DELIVERY_MISMATCH_MISSING_KV_META
                && mismatch.expected_status == "sent"
        }));

        pool.close().await;
        pg_db.drop().await;
    }

    // SAFETY (await_holding_lock): `lock_dispatch_delivery_metric_tests()` is a
    // std Mutex held across awaits to serialize tests that read/reset the
    // process-global dispatch-delivery mismatch metrics. The hold is required —
    // releasing before the awaits would let concurrent tests clobber the shared
    // metric counters. Test-only.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_delivery_reconcile_logs_dispatch_id_and_increments_metric_pg() {
        let _serial = lock_dispatch_delivery_metric_tests();
        reset_dispatch_delivery_event_mismatch_metrics_for_tests();
        let Some(pg_db) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;

        seed_dispatch(&pool, "dispatch-log-missing-typed").await;
        insert_kv(
            &pool,
            "dispatch_reserving:dispatch-log-missing-typed",
            "dispatch-log-missing-typed",
        )
        .await;

        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer_buffer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || TestLogWriter {
                buffer: writer_buffer.clone(),
            })
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let stats = reconcile_dispatch_delivery_events_pg(&pool).await.unwrap();
        drop(_guard);
        let logs = String::from_utf8(buffer.lock().unwrap().clone()).unwrap();
        let metrics = dispatch_delivery_event_mismatch_metrics_snapshot();

        assert_eq!(stats.mismatch_count, 1);
        assert!(
            logs.contains("dispatch-log-missing-typed")
                && logs.contains(DISPATCH_DELIVERY_MISMATCH_MISSING_TYPED),
            "mismatch log must include dispatch_id and kind; logs={logs}"
        );
        assert!(metrics.iter().any(|metric| {
            metric.kind == DISPATCH_DELIVERY_MISMATCH_MISSING_TYPED && metric.value >= 1
        }));

        pool.close().await;
        pg_db.drop().await;
    }

    async fn latest_typed_status(pool: &PgPool, dispatch_id: &str) -> Option<String> {
        sqlx::query_scalar::<_, String>(
            "SELECT status
               FROM dispatch_delivery_events
              WHERE dispatch_id = $1
                AND operation = 'send'
                AND target_kind = 'channel'
              ORDER BY updated_at DESC, id DESC
              LIMIT 1",
        )
        .bind(dispatch_id)
        .fetch_optional(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn dispatch_delivery_reconcile_expires_reserving_but_keeps_active_pg() {
        let Some(pg_db) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;

        // Expired reservation (finalize never ran): kv expires_at AND typed
        // reserved_until both in the past.
        seed_dispatch(&pool, "dispatch-expired-reserving").await;
        insert_kv_with_expiry(
            &pool,
            "dispatch_reserving:dispatch-expired-reserving",
            "dispatch-expired-reserving",
            -60,
        )
        .await;
        insert_delivery_event(&pool, "dispatch-expired-reserving", "reserved").await;

        // Active reservation still in flight: expires_at in the future.
        seed_dispatch(&pool, "dispatch-active-reserving").await;
        insert_kv_with_expiry(
            &pool,
            "dispatch_reserving:dispatch-active-reserving",
            "dispatch-active-reserving",
            300,
        )
        .await;

        let recovered = recover_expired_dispatch_reserving_pg(&pool).await.unwrap();

        assert_eq!(
            recovered, 1,
            "only the expired reserving key should be reclaimed"
        );
        assert!(
            !kv_key_exists(&pool, "dispatch_reserving:dispatch-expired-reserving").await,
            "expired reserving key must be deleted"
        );
        assert_eq!(
            latest_typed_status(&pool, "dispatch-expired-reserving").await,
            Some("failed".to_string()),
            "expired typed reservation must be flipped to failed so it stops re-reporting"
        );
        assert!(
            kv_key_exists(&pool, "dispatch_reserving:dispatch-active-reserving").await,
            "in-flight reservation must be preserved"
        );

        // Idempotent: a second pass reclaims nothing more.
        let recovered_again = recover_expired_dispatch_reserving_pg(&pool).await.unwrap();
        assert_eq!(recovered_again, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn dispatch_delivery_reconcile_backfills_notified_but_keeps_delivered_pg() {
        let Some(pg_db) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;

        // Orphan #1: notified key with no typed delivery event at all.
        seed_dispatch(&pool, "dispatch-notified-no-typed").await;
        insert_kv(
            &pool,
            "dispatch_notified:dispatch-notified-no-typed",
            "dispatch-notified-no-typed",
        )
        .await;

        // Orphan #2: notified key whose latest typed event is 'failed'.
        seed_dispatch(&pool, "dispatch-notified-failed").await;
        insert_kv(
            &pool,
            "dispatch_notified:dispatch-notified-failed",
            "dispatch-notified-failed",
        )
        .await;
        insert_delivery_event(&pool, "dispatch-notified-failed", "failed").await;

        // Legitimate: notified key backed by a 'sent' typed event.
        seed_dispatch(&pool, "dispatch-notified-sent").await;
        insert_kv(
            &pool,
            "dispatch_notified:dispatch-notified-sent",
            "dispatch-notified-sent",
        )
        .await;
        insert_delivery_event(&pool, "dispatch-notified-sent", "sent").await;

        // In flight: notified key alongside an active 'reserved' event (future
        // reserved_until). Anomalous but must NOT be reclaimed mid-flight.
        seed_dispatch(&pool, "dispatch-notified-reserved").await;
        insert_kv(
            &pool,
            "dispatch_notified:dispatch-notified-reserved",
            "dispatch-notified-reserved",
        )
        .await;
        sqlx::query(
            "INSERT INTO dispatch_delivery_events (
                dispatch_id, correlation_id, semantic_event_id, operation,
                target_kind, status, attempt, result_json, reserved_until,
                created_at, updated_at
             ) VALUES (
                $1, 'dispatch:' || $1, 'dispatch:' || $1 || ':notify', 'send',
                'channel', 'reserved', 1, '{}'::jsonb, NOW() + INTERVAL '5 minutes',
                NOW(), NOW()
             )",
        )
        .bind("dispatch-notified-reserved")
        .execute(&pool)
        .await
        .unwrap();

        // Orphan #3: notified key whose latest typed event is an EXPIRED
        // 'reserved' (the typed finalize never ran and the reservation aged out).
        // No active reservation / completed row => safe to upgrade to 'sent'.
        seed_dispatch(&pool, "dispatch-notified-expired-reserved").await;
        insert_kv(
            &pool,
            "dispatch_notified:dispatch-notified-expired-reserved",
            "dispatch-notified-expired-reserved",
        )
        .await;
        sqlx::query(
            "INSERT INTO dispatch_delivery_events (
                dispatch_id, correlation_id, semantic_event_id, operation,
                target_kind, status, attempt, result_json, reserved_until,
                created_at, updated_at
             ) VALUES (
                $1, 'dispatch:' || $1, 'dispatch:' || $1 || ':notify', 'send',
                'channel', 'reserved', 1, '{}'::jsonb, NOW() - INTERVAL '1 minute',
                NOW() - INTERVAL '6 minutes', NOW() - INTERVAL '6 minutes'
             )",
        )
        .bind("dispatch-notified-expired-reserved")
        .execute(&pool)
        .await
        .unwrap();

        let recovered = recover_orphan_dispatch_notified_pg(&pool).await.unwrap();

        // The notified key is itself proof of a completed send, so recovery
        // backfills/upgrades the typed event to 'sent' rather than deleting the
        // idempotency guard (which would allow a duplicate send).
        assert_eq!(
            recovered, 3,
            "no-typed, stale-failed, and expired-reserved orphans should all reconcile"
        );
        assert!(
            kv_key_exists(&pool, "dispatch_notified:dispatch-notified-no-typed").await,
            "notified guard must be preserved (it is the dedupe proof)"
        );
        assert_eq!(
            latest_typed_status(&pool, "dispatch-notified-no-typed").await,
            Some("sent".to_string()),
            "missing typed event must be backfilled as sent"
        );
        assert!(
            kv_key_exists(&pool, "dispatch_notified:dispatch-notified-failed").await,
            "notified guard for a stale failed event must be preserved"
        );
        assert_eq!(
            latest_typed_status(&pool, "dispatch-notified-failed").await,
            Some("sent".to_string()),
            "stale failed typed event must be upgraded to sent"
        );
        assert!(
            kv_key_exists(&pool, "dispatch_notified:dispatch-notified-sent").await,
            "notified guard for a completed send must be preserved"
        );
        // The in-flight reservation must remain 'reserved' — recovery must not
        // touch a delivery that is still being attempted.
        assert_eq!(
            latest_typed_status(&pool, "dispatch-notified-reserved").await,
            Some("reserved".to_string()),
            "in-flight reservation must not be reconciled mid-flight"
        );
        // The expired reservation behind a notified guard must be upgraded.
        assert_eq!(
            latest_typed_status(&pool, "dispatch-notified-expired-reserved").await,
            Some("sent".to_string()),
            "expired reservation behind a notified guard must be upgraded to sent"
        );

        // Idempotent: a second pass reconciles nothing more.
        let recovered_again = recover_orphan_dispatch_notified_pg(&pool).await.unwrap();
        assert_eq!(recovered_again, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn dispatch_delivery_reconcile_notified_upgrade_avoids_live_finalizer_race_pg() {
        let Some(pg_db) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;

        // A delivery whose send outlived its reservation: the expiry sweep flipped
        // the attempt to `failed`, but a new `reserved` attempt is in flight (the
        // real finalizer is still running). Upgrading the failed row now would
        // race that finalizer, so recovery must leave it `failed`.
        seed_dispatch(&pool, "dispatch-live-finalizer").await;
        insert_kv(
            &pool,
            "dispatch_notified:dispatch-live-finalizer",
            "dispatch-live-finalizer",
        )
        .await;
        sqlx::query(
            "INSERT INTO dispatch_delivery_events (
                dispatch_id, correlation_id, semantic_event_id, operation,
                target_kind, status, attempt, result_json, reserved_until,
                created_at, updated_at
             ) VALUES
             ($1, 'dispatch:' || $1, 'dispatch:' || $1 || ':notify', 'send',
              'channel', 'failed', 1, '{}'::jsonb, NULL,
              NOW() - INTERVAL '2 minutes', NOW() - INTERVAL '2 minutes'),
             ($1, 'dispatch:' || $1, 'dispatch:' || $1 || ':notify', 'send',
              'channel', 'reserved', 2, '{}'::jsonb, NOW() + INTERVAL '5 minutes',
              NOW(), NOW())",
        )
        .bind("dispatch-live-finalizer")
        .execute(&pool)
        .await
        .unwrap();

        // A delivery whose real finalize already landed a `sent` row alongside a
        // stale `failed` attempt: recovery must not add a contradictory
        // placeholder, it should leave the existing rows alone.
        seed_dispatch(&pool, "dispatch-already-sent").await;
        insert_kv(
            &pool,
            "dispatch_notified:dispatch-already-sent",
            "dispatch-already-sent",
        )
        .await;
        sqlx::query(
            "INSERT INTO dispatch_delivery_events (
                dispatch_id, correlation_id, semantic_event_id, operation,
                target_kind, target_channel_id, status, attempt, result_json,
                created_at, updated_at
             ) VALUES
             ($1, 'dispatch:' || $1, 'dispatch:' || $1 || ':notify', 'send',
              'channel', NULL, 'failed', 1, '{}'::jsonb,
              NOW() - INTERVAL '2 minutes', NOW() - INTERVAL '2 minutes'),
             ($1, 'dispatch:' || $1, 'dispatch:' || $1 || ':notify', 'send',
              'channel', '123', 'sent', 2, '{}'::jsonb, NOW(), NOW())",
        )
        .bind("dispatch-already-sent")
        .execute(&pool)
        .await
        .unwrap();

        let recovered = recover_orphan_dispatch_notified_pg(&pool).await.unwrap();

        assert_eq!(
            recovered, 0,
            "neither the live-finalizer nor already-sent case should be upgraded"
        );
        // The in-flight reservation's failed attempt #1 stays failed.
        let live_statuses: Vec<String> = sqlx::query_scalar(
            "SELECT status FROM dispatch_delivery_events
              WHERE dispatch_id = 'dispatch-live-finalizer'
              ORDER BY attempt",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            live_statuses,
            vec!["failed".to_string(), "reserved".to_string()],
            "live-finalizer dispatch must keep failed#1 + reserved#2 untouched"
        );
        // No placeholder sent row was created for the already-sent dispatch.
        let placeholder_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM dispatch_delivery_events
              WHERE dispatch_id = 'dispatch-already-sent'
                AND status = 'sent'
                AND target_channel_id IS NULL",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(placeholder_count, 0, "no null-channel placeholder sent row");

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn dispatch_delivery_reconcile_prunes_notified_for_deleted_dispatch_pg() {
        let Some(pg_db) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;

        // A notified guard whose owning task_dispatches row no longer exists.
        // Backfilling a typed event here would violate the FK; recovery must
        // instead prune the now-meaningless guard without erroring.
        insert_kv(
            &pool,
            "dispatch_notified:dispatch-deleted",
            "dispatch-deleted",
        )
        .await;

        let recovered = recover_orphan_dispatch_notified_pg(&pool).await.unwrap();

        assert_eq!(recovered, 1, "the orphaned notified guard should be pruned");
        assert!(
            !kv_key_exists(&pool, "dispatch_notified:dispatch-deleted").await,
            "notified guard for a deleted dispatch must be removed"
        );

        // Idempotent: a second pass does nothing.
        let recovered_again = recover_orphan_dispatch_notified_pg(&pool).await.unwrap();
        assert_eq!(recovered_again, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn dispatch_delivery_reconcile_recovers_guard_less_typed_events_pg() {
        let Some(pg_db) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;

        // missing_kv_meta #1: a completed `sent` event whose dispatch_notified
        // guard was lost. Recovery rebuilds the guard.
        seed_dispatch(&pool, "dispatch-typed-sent-no-guard").await;
        insert_delivery_event(&pool, "dispatch-typed-sent-no-guard", "sent").await;

        // missing_kv_meta #2: an expired `reserved` event with no guard key.
        // Recovery finalizes it to 'failed'. insert_delivery_event sets
        // reserved_until = NOW() - 1 minute for the 'reserved' status.
        seed_dispatch(&pool, "dispatch-typed-reserved-no-guard").await;
        insert_delivery_event(&pool, "dispatch-typed-reserved-no-guard", "reserved").await;

        // Still-active reservation (future reserved_until) with no guard key must
        // NOT be touched — it is in flight.
        seed_dispatch(&pool, "dispatch-typed-active-no-guard").await;
        sqlx::query(
            "INSERT INTO dispatch_delivery_events (
                dispatch_id, correlation_id, semantic_event_id, operation,
                target_kind, status, attempt, result_json, reserved_until,
                created_at, updated_at
             ) VALUES (
                $1, 'dispatch:' || $1, 'dispatch:' || $1 || ':notify', 'send',
                'channel', 'reserved', 1, '{}'::jsonb, NOW() + INTERVAL '5 minutes',
                NOW(), NOW()
             )",
        )
        .bind("dispatch-typed-active-no-guard")
        .execute(&pool)
        .await
        .unwrap();

        let recovered = recover_orphan_typed_delivery_events_pg(&pool)
            .await
            .unwrap();

        assert_eq!(
            recovered, 2,
            "both guard-less mismatch cases should be reconciled"
        );
        assert!(
            kv_key_exists(&pool, "dispatch_notified:dispatch-typed-sent-no-guard").await,
            "notified guard must be rebuilt for a completed sent event"
        );
        assert_eq!(
            latest_typed_status(&pool, "dispatch-typed-reserved-no-guard").await,
            Some("failed".to_string()),
            "expired guard-less reservation must be finalized as failed"
        );
        assert_eq!(
            latest_typed_status(&pool, "dispatch-typed-active-no-guard").await,
            Some("reserved".to_string()),
            "in-flight reservation must be left untouched"
        );

        // Idempotent: a second pass reconciles nothing more. (The active one is
        // still in flight, so it is correctly skipped both times.)
        let recovered_again = recover_orphan_typed_delivery_events_pg(&pool)
            .await
            .unwrap();
        assert_eq!(recovered_again, 0);

        pool.close().await;
        pg_db.drop().await;
    }

    // SAFETY (await_holding_lock): same rationale as
    // `dispatch_delivery_reconcile_logs_dispatch_id_and_increments_metric_pg` —
    // the metric-serialization Mutex must stay held across the awaits. Test-only.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_delivery_reconcile_recovers_and_clears_backlog_pg() {
        let _serial = lock_dispatch_delivery_metric_tests();
        reset_dispatch_delivery_event_mismatch_metrics_for_tests();
        let Some(pg_db) = TestPostgresDb::try_create().await else {
            return;
        };
        let pool = pg_db.connect_and_migrate().await;

        // Orphaned notified key with no typed event => missing_typed mismatch on
        // the first pass, then reclaimed so the backlog clears on the next pass.
        seed_dispatch(&pool, "dispatch-backlog-notified").await;
        insert_kv(
            &pool,
            "dispatch_notified:dispatch-backlog-notified",
            "dispatch-backlog-notified",
        )
        .await;

        // Expired reserving key => reclaimed by recovery.
        seed_dispatch(&pool, "dispatch-backlog-reserving").await;
        insert_kv_with_expiry(
            &pool,
            "dispatch_reserving:dispatch-backlog-reserving",
            "dispatch-backlog-reserving",
            -60,
        )
        .await;

        // Guard-less completed `sent` event => missing_kv_meta mismatch on the
        // first pass; recovery rebuilds the notified guard.
        seed_dispatch(&pool, "dispatch-backlog-typed-sent").await;
        insert_delivery_event(&pool, "dispatch-backlog-typed-sent", "sent").await;

        let first = reconcile_dispatch_delivery_events_pg(&pool).await.unwrap();
        assert!(
            first.mismatch_count > 0,
            "first pass should report the backlog"
        );
        assert!(
            first.missing_kv_meta > 0,
            "first pass should observe the typed-only mismatch class too"
        );
        assert_eq!(first.recovered_expired_reserving, 1);
        assert_eq!(first.recovered_orphan_notified, 1);
        assert_eq!(first.recovered_orphan_typed, 1);

        let recovery_metrics = dispatch_delivery_event_recovery_metrics_snapshot();
        assert!(
            recovery_metrics.iter().any(|m| {
                m.kind == DISPATCH_DELIVERY_RECOVERY_EXPIRED_RESERVING && m.value >= 1
            })
        );
        assert!(
            recovery_metrics
                .iter()
                .any(|m| { m.kind == DISPATCH_DELIVERY_RECOVERY_ORPHAN_NOTIFIED && m.value >= 1 })
        );
        assert!(
            recovery_metrics
                .iter()
                .any(|m| { m.kind == DISPATCH_DELIVERY_RECOVERY_ORPHAN_TYPED && m.value >= 1 })
        );

        // Root-cause proof: the same orphans no longer recur on the next tick.
        let second = reconcile_dispatch_delivery_events_pg(&pool).await.unwrap();
        assert_eq!(
            second.mismatch_count, 0,
            "backlog must clear once orphans are reclaimed"
        );
        assert_eq!(second.recovered(), 0, "nothing left to recover");

        pool.close().await;
        pg_db.drop().await;
    }
}
