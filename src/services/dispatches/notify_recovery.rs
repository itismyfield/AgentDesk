use anyhow::{Result, anyhow};
use sqlx::{PgPool, Row as SqlxRow};

use crate::reconcile::{
    AUTO_QUEUE_PENDING_DELIVERY_ORPHAN_GRACE, AutoQueuePendingDeliveryOrphanStats,
    DISPATCH_DELIVERY_RECOVERY_EXPIRED_RESERVING, DISPATCH_DELIVERY_RECOVERY_ORPHAN_NOTIFIED,
    DISPATCH_DELIVERY_RECOVERY_ORPHAN_TYPED, DispatchDeliveryEventReconcileStats,
    dispatch_delivery_event_reconcile_report_pg, record_dispatch_delivery_mismatch_metric,
    record_dispatch_delivery_recovery_metric,
};

const BOOT_RECONCILE_OUTBOX_LEASE_TIMEOUT_SECS: i64 = 300;
const AUTO_QUEUE_PENDING_DELIVERY_ORPHAN_STALE_CLAIM: std::time::Duration =
    std::time::Duration::from_secs(5 * 60);
const AUTO_QUEUE_PENDING_DELIVERY_ORPHAN_BATCH_LIMIT: i64 = 50;

pub(crate) struct BootNotifyRecoveryStats {
    pub(crate) stale_processing_outbox_reset: usize,
    pub(crate) stale_dispatch_reservations_cleared: usize,
    pub(crate) missing_notify_outbox_backfilled: usize,
    pub(crate) dispatch_delivery_event_mismatches: usize,
}

pub(crate) async fn reconcile_boot_notify_pg(
    pool: &PgPool,
    current_instance_id: &str,
) -> Result<BootNotifyRecoveryStats> {
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
    Ok(BootNotifyRecoveryStats {
        stale_processing_outbox_reset,
        stale_dispatch_reservations_cleared,
        missing_notify_outbox_backfilled,
        dispatch_delivery_event_mismatches,
    })
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
pub(crate) async fn recover_expired_dispatch_reserving_pg(pool: &PgPool) -> Result<usize> {
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
pub(crate) async fn recover_orphan_dispatch_notified_pg(pool: &PgPool) -> Result<usize> {
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
pub(crate) async fn recover_orphan_typed_delivery_events_pg(pool: &PgPool) -> Result<usize> {
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
    let notified_rebuilt = sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO kv_meta (key, value)
         SELECT 'dispatch_notified:' || src.dispatch_id, src.dispatch_id
           FROM ({latest_without_guard}) src
          WHERE src.status IN ('sent', 'fallback', 'duplicate', 'skipped')
         ON CONFLICT (key) DO NOTHING"
    )))
    .execute(pool)
    .await
    .map(|result| result.rows_affected() as usize)
    .map_err(|error| anyhow!("rebuild notified guard for guard-less completed events: {error}"))?;

    // 2. Finalize abandoned expired reservations whose guard key is already gone.
    let reservations_failed = sqlx::query(sqlx::AssertSqlSafe(format!(
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
    )))
    .execute(pool)
    .await
    .map(|result| result.rows_affected() as usize)
    .map_err(|error| anyhow!("finalize guard-less expired reservations: {error}"))?;

    Ok(notified_rebuilt + reservations_failed)
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

#[derive(sqlx::FromRow)]
struct UnsentNotifyCandidate {
    id: String,
    to_agent_id: Option<String>,
    kanban_card_id: String,
    title: String,
}

// Boot fills every missing notify; policy maintenance retains its narrower candidate contract.
pub(crate) async fn maintain_unsent_notifications_pg(pool: &PgPool) -> Result<()> {
    let candidates = sqlx::query_as::<_, UnsentNotifyCandidate>(
        "SELECT td.id, td.dispatch_type, td.to_agent_id, kc.title, kc.github_issue_url, kc.github_issue_number, td.kanban_card_id
         FROM task_dispatches td
         JOIN kanban_cards kc ON td.kanban_card_id = kc.id
         WHERE td.status = 'pending'
           AND td.created_at < datetime('now', '-2 minutes')
           AND NOT EXISTS (SELECT 1 FROM kv_meta WHERE key = 'dispatch_notified:' || td.id)
           AND NOT EXISTS (SELECT 1 FROM kv_meta WHERE key = 'dispatch_reserving:' || td.id)
           AND NOT EXISTS (SELECT 1 FROM dispatch_outbox WHERE dispatch_id = td.id AND status IN ('pending', 'processing', 'failed'))",
    )
    .fetch_all(pool)
    .await?;

    // Preserve separate autocommits: an error stops this pass without undoing prior inserts.
    for UnsentNotifyCandidate {
        id,
        to_agent_id,
        kanban_card_id,
        title,
    } in candidates
    {
        sqlx::query(
            "INSERT INTO dispatch_outbox (dispatch_id, action, agent_id, card_id, title, status)
             VALUES ($1, 'notify', $2, $3, $4, 'pending') ON CONFLICT DO NOTHING",
        )
        .bind(&id)
        .bind(to_agent_id)
        .bind(kanban_card_id)
        .bind(title)
        .execute(pool)
        .await?;
        tracing::info!(
            target: "policy",
            message = %format!("[notify-recovery] Dispatch {id} re-enqueued to dispatch_outbox"),
            "policy log"
        );
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct AutoQueuePendingDeliveryOrphanCandidate {
    dispatch_id: String,
    entry_id: String,
    run_id: String,
    outbox_status: Option<String>,
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

async fn auto_queue_pending_delivery_orphan_candidates_pg(
    pool: &PgPool,
) -> Result<Vec<AutoQueuePendingDeliveryOrphanCandidate>> {
    let rows = sqlx::query(
        "SELECT td.id AS dispatch_id,
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
           LEFT JOIN LATERAL (
               SELECT id, status, claimed_at, claim_owner
                 FROM dispatch_outbox
                WHERE dispatch_id = td.id
                  AND action = 'notify'
                ORDER BY id DESC
                LIMIT 1
           ) o ON true
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
