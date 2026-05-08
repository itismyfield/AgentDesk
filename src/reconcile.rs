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

use crate::{db::Db, engine::PolicyEngine};

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
const DISPATCH_DELIVERY_EVENT_RECONCILE_BATCH_LIMIT: i64 = 500;

const DISPATCH_DELIVERY_MISMATCH_MISSING_TYPED: &str = "missing_typed";
const DISPATCH_DELIVERY_MISMATCH_NOTIFIED_STATUS: &str = "notified_status_mismatch";
const DISPATCH_DELIVERY_MISMATCH_MISSING_KV_META: &str = "missing_kv_meta";

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
pub(crate) struct DispatchDeliveryEventReconcileStats {
    pub kv_reserving_checked: usize,
    pub kv_notified_checked: usize,
    pub typed_events_checked: usize,
    pub mismatch_count: usize,
    pub missing_typed: usize,
    pub notified_status_mismatch: usize,
    pub missing_kv_meta: usize,
}

impl DispatchDeliveryEventReconcileStats {
    pub(crate) fn touched(&self) -> bool {
        self.mismatch_count > 0
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
    }
}

pub(crate) async fn reconcile_boot_db_pg(pool: &PgPool) -> Result<BootReconcileStats> {
    // Touch next_attempt_at so oldest_pending_age reflects "re-queued at boot",
    // not the original created_at. Without this, rows that were stuck in
    // 'processing' across a restart show up as multi-minute-aged pending rows
    // and the promote health gate fails even though the outbox worker picks
    // them up on the next tick.
    let stale_processing_outbox_reset = sqlx::query(
        "UPDATE dispatch_outbox
            SET status = 'pending',
                next_attempt_at = NOW()
          WHERE status = 'processing'",
    )
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

    Ok(BootReconcileStats {
        stale_processing_outbox_reset,
        stale_dispatch_reservations_cleared,
        missing_notify_outbox_backfilled,
        broken_auto_queue_entries_reset,
        dispatch_delivery_event_mismatches,
        stale_channel_thread_map_entries_cleared: 0,
        missing_review_dispatches_refired: 0,
        completed_queue_review_drift_recovered: 0,
    })
}

pub(crate) async fn reconcile_boot_runtime(
    db: Option<&Db>,
    engine: &PolicyEngine,
    pg_pool: Option<&PgPool>,
) -> Result<BootReconcileStats> {
    let mut stats = if let Some(pool) = pg_pool {
        reconcile_boot_db_pg(pool).await?
    } else {
        #[cfg(all(test, feature = "legacy-sqlite-tests"))]
        {
            reconcile_boot_db_sqlite(
                db.ok_or_else(|| anyhow!("SQLite db required for test boot reconcile"))?,
            )?
        }
        #[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
        {
            return Err(anyhow!("Postgres pool required for boot reconcile"));
        }
    };

    stats.missing_review_dispatches_refired = if let Some(pool) = pg_pool {
        refire_missing_review_dispatches_pg(pool, db, engine).await?
    } else {
        0
    };
    stats.completed_queue_review_drift_recovered = if let Some(pool) = pg_pool {
        reconcile_completed_queue_review_drift_pg(pool, db, engine).await?
    } else {
        0
    };

    if stats.touched() {
        tracing::info!(
            "[boot-reconcile] reset_processing={} cleared_reservations={} missing_notify={} broken_auto_queue={} dispatch_delivery_mismatches={} cleared_thread_map={} refired_review={} recovered_review_drift={}",
            stats.stale_processing_outbox_reset,
            stats.stale_dispatch_reservations_cleared,
            stats.missing_notify_outbox_backfilled,
            stats.broken_auto_queue_entries_reset,
            stats.dispatch_delivery_event_mismatches,
            stats.stale_channel_thread_map_entries_cleared,
            stats.missing_review_dispatches_refired,
            stats.completed_queue_review_drift_recovered
        );
    }

    Ok(stats)
}

pub(crate) async fn reconcile_completed_queue_review_drift_pg(
    pool: &PgPool,
    db: Option<&Db>,
    engine: &PolicyEngine,
) -> Result<usize> {
    let drift_candidates = completed_queue_review_drift_candidates_pg(pool).await?;
    recover_completed_queue_review_drift_pg(pool, db, engine, drift_candidates).await
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

    if report.stats.touched() {
        tracing::warn!(
            target: "reconcile",
            kv_reserving_checked = report.stats.kv_reserving_checked,
            kv_notified_checked = report.stats.kv_notified_checked,
            typed_events_checked = report.stats.typed_events_checked,
            mismatch_count = report.stats.mismatch_count,
            missing_typed = report.stats.missing_typed,
            notified_status_mismatch = report.stats.notified_status_mismatch,
            missing_kv_meta = report.stats.missing_kv_meta,
            "[dispatch-delivery-reconcile] mismatch scan completed"
        );
    }

    Ok(report.stats)
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
        for row in &rows {
            stats.kv_reserving_checked += row.reserving_count.max(0) as usize;
            stats.kv_notified_checked += row.notified_count.max(0) as usize;
            classify_delivery_kv_guard_mismatches(&mut stats, &mut mismatches, row);
        }
        cursor = rows
            .last()
            .map(|row| row.dispatch_id.clone())
            .unwrap_or_default();
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
        for row in &rows {
            stats.typed_events_checked += 1;
            classify_delivery_typed_guard_mismatches(&mut stats, &mut mismatches, row);
        }
        cursor = rows
            .last()
            .map(|row| row.dispatch_id.clone())
            .unwrap_or_default();
    }

    Ok(DispatchDeliveryEventReconcileReport { stats, mismatches })
}

fn classify_delivery_kv_guard_mismatches(
    stats: &mut DispatchDeliveryEventReconcileStats,
    mismatches: &mut Vec<DispatchDeliveryEventMismatch>,
    row: &DeliveryKvGuardRow,
) {
    let has_notified = row.notified_count > 0;
    let typed_status = row.typed_status.as_deref();
    if typed_status.is_none() {
        let expected = if has_notified { "sent" } else { "reserved" };
        let mismatch =
            DispatchDeliveryEventMismatch::missing_typed(row.dispatch_id.clone(), expected);
        stats.record_mismatch(&mismatch.kind);
        mismatches.push(mismatch);
        return;
    }

    if has_notified && typed_status != Some("sent") {
        if typed_status == Some("reserved")
            && row
                .typed_reserved_until
                .as_ref()
                .is_some_and(|reserved_until| *reserved_until > Utc::now())
        {
            return;
        }
        let mismatch = DispatchDeliveryEventMismatch::notified_status(
            row.dispatch_id.clone(),
            row.typed_status.clone(),
        );
        stats.record_mismatch(&mismatch.kind);
        mismatches.push(mismatch);
        return;
    }
}

fn classify_delivery_typed_guard_mismatches(
    stats: &mut DispatchDeliveryEventReconcileStats,
    mismatches: &mut Vec<DispatchDeliveryEventMismatch>,
    row: &DeliveryTypedGuardRow,
) {
    if row.has_reserving || row.has_notified {
        return;
    }
    match row.typed_status.as_str() {
        "reserved" => {
            if row
                .reserved_until
                .as_ref()
                .is_some_and(|reserved_until| *reserved_until > Utc::now())
            {
                return;
            }
            let mismatch =
                DispatchDeliveryEventMismatch::missing_kv_meta(row.dispatch_id.clone(), "reserved");
            stats.record_mismatch(&mismatch.kind);
            mismatches.push(mismatch);
        }
        "sent" => {
            let mismatch =
                DispatchDeliveryEventMismatch::missing_kv_meta(row.dispatch_id.clone(), "sent");
            stats.record_mismatch(&mismatch.kind);
            mismatches.push(mismatch);
        }
        _ => {}
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn reconcile_boot_db_sqlite(db: &Db) -> Result<BootReconcileStats> {
    let conn = db
        .separate_conn()
        .map_err(|error| anyhow!("open sqlite boot reconcile connection: {error}"))?;
    let stale_processing_outbox_reset = conn
        .execute(
            "UPDATE dispatch_outbox SET status = 'pending' WHERE status = 'processing'",
            [],
        )
        .unwrap_or(0);
    let stale_dispatch_reservations_cleared = conn
        .execute(
            "DELETE FROM kv_meta WHERE key LIKE 'dispatch_reserving:%'",
            [],
        )
        .unwrap_or(0);
    let missing_notify_outbox_backfilled = conn
        .execute(
            "INSERT INTO dispatch_outbox (dispatch_id, action, agent_id, card_id, title, status)
             SELECT td.id, 'notify', td.to_agent_id, td.kanban_card_id, td.title, 'pending'
             FROM task_dispatches td
             WHERE td.status IN ('pending', 'dispatched')
               AND NOT EXISTS (
                 SELECT 1 FROM dispatch_outbox o
                 WHERE o.dispatch_id = td.id AND o.action = 'notify'
               )",
            [],
        )
        .map_err(|error| anyhow!("backfill sqlite missing notify outbox: {error}"))?;
    let broken_auto_queue_entries_reset = conn
        .execute(
            "UPDATE auto_queue_entries
             SET status = 'pending',
                 dispatch_id = NULL,
                 slot_index = NULL,
                 dispatched_at = NULL,
                 completed_at = NULL
             WHERE status = 'dispatched'
               AND (
                 dispatch_id IS NULL
                 OR TRIM(dispatch_id) = ''
                 OR NOT EXISTS (
                   SELECT 1
                   FROM task_dispatches td
                   WHERE td.id = auto_queue_entries.dispatch_id
                     AND td.status NOT IN ('cancelled', 'failed', 'completed')
                 )
               )",
            [],
        )
        .map_err(|error| anyhow!("reset sqlite broken auto-queue entries: {error}"))?;

    Ok(BootReconcileStats {
        stale_processing_outbox_reset,
        stale_dispatch_reservations_cleared,
        missing_notify_outbox_backfilled,
        broken_auto_queue_entries_reset,
        dispatch_delivery_event_mismatches: 0,
        stale_channel_thread_map_entries_cleared: 0,
        missing_review_dispatches_refired: 0,
        completed_queue_review_drift_recovered: 0,
    })
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
    db: Option<&Db>,
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
        crate::kanban::drain_hook_side_effects_with_backends(db, engine);

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
    db: Option<&Db>,
    engine: &PolicyEngine,
    drift_candidates: Vec<String>,
) -> Result<usize> {
    let mut recovered = 0usize;

    for card_id in drift_candidates {
        if !completed_queue_review_drift_candidate_exists_pg(pool, &card_id).await? {
            continue;
        }

        match crate::kanban::transition_status_with_opts_pg(
            db,
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
                crate::kanban::drain_hook_side_effects_with_backends(db, engine);
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
            // Only remove when restart_mode is absent. A file with a restart_mode
            // set is owned by a planned lifecycle (drain/hot-swap); the existing
            // inflight retention helpers cover those.
            let restart_mode_present = fs::read_to_string(&fpath)
                .ok()
                .and_then(|body| serde_json::from_str::<serde_json::Value>(&body).ok())
                .and_then(|v| {
                    v.get("restart_mode")
                        .filter(|rm| !rm.is_null())
                        .map(|_| true)
                })
                .unwrap_or(false);
            if restart_mode_present {
                continue;
            }
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

    #[test]
    fn dispatch_delivery_reconcile_classifies_rows_without_postgres() {
        let mut stats = DispatchDeliveryEventReconcileStats::default();
        let mut mismatches = Vec::new();

        classify_delivery_kv_guard_mismatches(
            &mut stats,
            &mut mismatches,
            &DeliveryKvGuardRow {
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
            &DeliveryKvGuardRow {
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
            &DeliveryTypedGuardRow {
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

    #[tokio::test(flavor = "current_thread")]
    async fn dispatch_delivery_reconcile_logs_dispatch_id_and_increments_metric_pg() {
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
            metric.kind == DISPATCH_DELIVERY_MISMATCH_MISSING_TYPED && metric.value == 1
        }));

        pool.close().await;
        pg_db.drop().await;
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // #1076 (905-7): zombie reconcile sweep tests
    // ------------------------------------------------------------------

    #[test]
    fn zombie_sweep_removes_old_inflight_files_without_restart_mode() {
        use std::fs;
        let tmp = tempfile::tempdir().unwrap();
        let provider_dir = tmp.path().join("claude");
        fs::create_dir_all(&provider_dir).unwrap();

        // File with restart_mode = null -> must be removed once max_age=0.
        let stale = provider_dir.join("stale.json");
        fs::write(
            &stale,
            "{\"channel_id\":1,\"restart_mode\":null,\"updated_at\":\"x\"}",
        )
        .unwrap();

        // File WITH restart_mode -> must be preserved even when max_age=0.
        let planned = provider_dir.join("planned.json");
        fs::write(
            &planned,
            "{\"channel_id\":2,\"restart_mode\":\"DrainRestart\",\"updated_at\":\"x\"}",
        )
        .unwrap();

        // Non-json file -> ignored.
        let stray = provider_dir.join("junk.tmp");
        fs::write(&stray, "nope").unwrap();

        // max_age = 0 -> every file is "stale" by age, so the restart_mode
        // branch is the only thing protecting `planned.json`.
        let removed = sweep_stale_inflight_files_at(tmp.path(), Duration::from_secs(0));
        assert_eq!(
            removed, 1,
            "only the stale unplanned file should be removed"
        );
        assert!(!stale.exists(), "stale file must be gone");
        assert!(planned.exists(), "planned-restart file must survive");
        assert!(stray.exists(), "non-json files must be ignored");
    }

    #[test]
    fn zombie_sweep_preserves_everything_when_max_age_is_far_in_future() {
        use std::fs;
        let tmp = tempfile::tempdir().unwrap();
        let provider_dir = tmp.path().join("codex");
        fs::create_dir_all(&provider_dir).unwrap();
        let a = provider_dir.join("a.json");
        fs::write(&a, "{\"restart_mode\":null}").unwrap();
        let removed =
            sweep_stale_inflight_files_at(tmp.path(), Duration::from_secs(365 * 24 * 60 * 60));
        assert_eq!(removed, 0);
        assert!(a.exists());
    }

    #[test]
    fn zombie_sweep_removes_stale_discord_uploads() {
        use std::fs;
        let tmp = tempfile::tempdir().unwrap();
        let channel_dir = tmp.path().join("999");
        fs::create_dir_all(&channel_dir).unwrap();
        let f = channel_dir.join("old.png");
        fs::write(&f, b"old").unwrap();

        // max_age = 0 -> file qualifies as stale.
        let removed = sweep_stale_discord_uploads_at(tmp.path(), Duration::from_secs(0));
        assert_eq!(removed, 1);
        assert!(!f.exists());
        // The empty channel dir is pruned.
        assert!(!channel_dir.exists());
    }

    #[test]
    fn zombie_stats_total_sums_all_buckets() {
        let stats = ZombieReconcileStats {
            orphan_tmux_killed: 1,
            stale_inflight_removed: 2,
            zombie_dashmap_trimmed: 3,
            stale_uploads_removed: 4,
        };
        assert_eq!(stats.total(), 10);
    }
}
