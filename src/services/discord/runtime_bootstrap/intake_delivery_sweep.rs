//! Periodic recovery of stale open intake-delivery debt.

use super::intake_delivery_capability::SettlementCapabilities;
use crate::db::intake_outbox_delivery_proof::{
    DURABLE_INFLIGHT_SESSION_SCOPE_SQL, durable_inflight_liveness_case_sql, list_stale_dispatched,
    list_stale_spawned, open_stamp_debt_exists, settle_dispatched_unknown, settle_spawned_unknown,
};
use crate::services::discord::SharedData;
use chrono::{DateTime, Duration, Utc};
use futures::FutureExt;
use sqlx::PgPool;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const HEARTBEAT_FRESH_SECS: i64 = 30;
const SWEEP_INTERVAL_SECS: u64 = 60;
#[cfg(not(test))]
const SWEEP_INITIAL_DELAY_SECS: u64 = crate::config::INTAKE_DELIVERY_SWEEP_INITIAL_DELAY_SECS;
#[cfg(test)]
const SWEEP_INITIAL_DELAY_SECS: u64 = 0;
static SWEEP_ACTIVE: AtomicBool = AtomicBool::new(false);

pub(super) async fn begin_settlement_transaction(
    pool: &PgPool,
) -> Result<sqlx::Transaction<'_, sqlx::Postgres>, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT set_config('lock_timeout', $1, true)")
        .bind(format!(
            "{}s",
            crate::config::INTAKE_DELIVERY_SWEEP_LOCK_TIMEOUT_SECS
        ))
        .execute(&mut *transaction)
        .await?;
    Ok(transaction)
}

fn is_lock_timeout(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Database(error) if error.code().as_deref() == Some("55P03"))
}

#[derive(Clone, Copy)]
pub(super) struct SweepCutoffs {
    dispatched: DateTime<Utc>,
    spawned: DateTime<Utc>,
    heartbeat_fresh: DateTime<Utc>,
}

impl SweepCutoffs {
    fn from_now(dispatched_secs: u64, spawned_secs: u64) -> Self {
        let now = Utc::now();
        let dispatched_secs = dispatched_secs.min(crate::config::MAX_INTAKE_SWEEP_CUTOFF_SECS);
        let spawned_secs = spawned_secs.min(crate::config::MAX_INTAKE_SWEEP_CUTOFF_SECS);
        Self {
            dispatched: now - Duration::seconds(dispatched_secs as i64),
            spawned: now - Duration::seconds(spawned_secs as i64),
            heartbeat_fresh: now - Duration::seconds(HEARTBEAT_FRESH_SECS),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LiveSignal {
    Absent,
    Live,
    Ambiguous,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct SweepStats {
    pub(super) settled: usize,
    pub(super) skipped_live: usize,
    pub(super) skipped_ambiguous: usize,
    pub(super) truncated_dispatched: i64,
    pub(super) truncated_spawned: i64,
}

async fn live_signal(
    pool: &PgPool,
    outbox_id: i64,
    heartbeat_fresh: DateTime<Utc>,
    absence_cutoff: DateTime<Utc>,
) -> Result<LiveSignal, sqlx::Error> {
    let liveness = durable_inflight_liveness_case_sql(2, 3);
    let session_scope = DURABLE_INFLIGHT_SESSION_SCOPE_SQL;
    let statement = format!(
        "SELECT COALESCE(MAX({liveness}), 0)::smallint
           FROM public.intake_outbox io
           JOIN public.sessions s ON {session_scope}
          WHERE io.id = $1"
    );
    let signal: i16 = sqlx::query_scalar(&statement)
        .bind(outbox_id)
        .bind(heartbeat_fresh)
        .bind(absence_cutoff)
        .fetch_one(pool)
        .await?;
    Ok(match signal {
        0 => LiveSignal::Absent,
        2 => LiveSignal::Live,
        _ => LiveSignal::Ambiguous,
    })
}

async fn stale_counts(pool: &PgPool, cutoffs: SweepCutoffs) -> Result<(i64, i64), sqlx::Error> {
    sqlx::query_as(
        "SELECT
           count(*) FILTER (WHERE status='dispatched' AND dispatched_at < $1)::bigint,
           count(*) FILTER (WHERE status='spawned' AND spawned_at < $2)::bigint
         FROM public.intake_outbox",
    )
    .bind(cutoffs.dispatched)
    .bind(cutoffs.spawned)
    .fetch_one(pool)
    .await
}

async fn settle_dispatched(
    pool: &PgPool,
    id: i64,
    cutoff: DateTime<Utc>,
    heartbeat_fresh: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    #[cfg(unix)]
    {
        match super::intake_delivery_reconciler::reconcile_row(pool, id, cutoff, heartbeat_fresh)
            .await
        {
            Ok(
                super::intake_delivery_reconciler::ReconcileOutcome::Done
                | super::intake_delivery_reconciler::ReconcileOutcome::Unknown,
            ) => return Ok(true),
            Ok(super::intake_delivery_reconciler::ReconcileOutcome::Unchanged) => return Ok(false),
            Err(error) if is_lock_timeout(&error) => return Err(error),
            Err(error) => {
                tracing::warn!(outbox_id = id, %error, "journal judgment failed; falling back to unknown settlement")
            }
        }
    }
    let mut transaction = begin_settlement_transaction(pool).await?;
    let won = settle_dispatched_unknown(&mut transaction, id, cutoff, heartbeat_fresh).await?;
    transaction.commit().await?;
    Ok(won)
}

async fn settle_spawned(
    pool: &PgPool,
    id: i64,
    cutoff: DateTime<Utc>,
    heartbeat_fresh: DateTime<Utc>,
) -> Result<bool, sqlx::Error> {
    let mut transaction = begin_settlement_transaction(pool).await?;
    let won = settle_spawned_unknown(&mut transaction, id, cutoff, heartbeat_fresh).await?;
    transaction.commit().await?;
    Ok(won)
}

fn note_skip(stats: &mut SweepStats, signal: LiveSignal, id: i64) -> bool {
    match signal {
        LiveSignal::Absent => false,
        LiveSignal::Live => {
            stats.skipped_live += 1;
            tracing::debug!(
                outbox_id = id,
                "intake delivery sweep skipped live inflight heartbeat"
            );
            true
        }
        LiveSignal::Ambiguous => {
            stats.skipped_ambiguous += 1;
            tracing::info!(
                outbox_id = id,
                "intake delivery sweep deferred ambiguous inflight heartbeat"
            );
            true
        }
    }
}

pub(super) async fn sweep_once(
    pool: &PgPool,
    caps: SettlementCapabilities,
    cutoffs: SweepCutoffs,
    limit: i64,
) -> Result<SweepStats, sqlx::Error> {
    // This is a no-debt short-circuit, NOT a stage gate, and the difference is operationally
    // visible. `open_stamp_debt_exists` asks whether any row sits in `spawned` or `dispatched`,
    // and `spawned` is the normal initial status of every locally admitted row
    // (`AdmissionKind::Local => Spawned`). So on a node carrying traffic the second disjunct is
    // true and the sweep runs at every stage, `Off` included: rows older than the cutoff are
    // settled `unknown` whether or not settlement has been "turned on".
    //
    // That is the intended design -- debt already in the table has to drain after a stage is
    // lowered or a node restarts -- and two tests pin it with `settle_and_sweep: false`:
    // `sweep_runs_when_stage_lowered_but_open_dispatched_exists_pg` and
    // `sweep_runs_after_restart_with_only_spawned_stamp_debt_pg`. What the disjunct buys is
    // skipping the per-tick queries on an idle table, not holding the sweep back until the stage
    // authorizes it. The guards that do bound the damage are the staleness cutoffs, the
    // `live_signal` deferral, and the in-transaction cutoff recheck before each terminal CAS.
    if !caps.settle_and_sweep && !open_stamp_debt_exists(pool).await? {
        return Ok(SweepStats::default());
    }
    let limit = limit.clamp(1, 500);
    let totals = stale_counts(pool, cutoffs).await?;
    let dispatched = list_stale_dispatched(pool, cutoffs.dispatched, limit).await?;
    let spawned = list_stale_spawned(pool, cutoffs.spawned, limit).await?;
    let mut stats = SweepStats {
        truncated_dispatched: (totals.0 - dispatched.len() as i64).max(0),
        truncated_spawned: (totals.1 - spawned.len() as i64).max(0),
        ..SweepStats::default()
    };
    for row in dispatched {
        let signal = live_signal(pool, row.id, cutoffs.heartbeat_fresh, cutoffs.dispatched).await;
        match signal {
            Ok(signal) if note_skip(&mut stats, signal, row.id) => continue,
            Err(error) => {
                stats.skipped_ambiguous += 1;
                tracing::warn!(outbox_id = row.id, %error, "intake delivery live signal was unreadable; deferring settlement");
                continue;
            }
            _ => {}
        }
        match settle_dispatched(pool, row.id, cutoffs.dispatched, cutoffs.heartbeat_fresh).await {
            Ok(true) => stats.settled += 1,
            Ok(false) => {}
            Err(error) if is_lock_timeout(&error) => {
                stats.skipped_ambiguous += 1;
                tracing::warn!(outbox_id = row.id, %error, "stale dispatched settlement lock timed out; deferring ambiguous row");
            }
            Err(error) => {
                tracing::error!(outbox_id = row.id, %error, "stale dispatched settlement failed")
            }
        }
    }
    for row in spawned {
        let signal = live_signal(pool, row.id, cutoffs.heartbeat_fresh, cutoffs.spawned).await;
        match signal {
            Ok(signal) if note_skip(&mut stats, signal, row.id) => continue,
            Err(error) => {
                stats.skipped_ambiguous += 1;
                tracing::warn!(outbox_id = row.id, %error, "intake delivery live signal was unreadable; deferring settlement");
                continue;
            }
            _ => {}
        }
        match settle_spawned(pool, row.id, cutoffs.spawned, cutoffs.heartbeat_fresh).await {
            Ok(true) => stats.settled += 1,
            Ok(false) => {}
            Err(error) if is_lock_timeout(&error) => {
                stats.skipped_ambiguous += 1;
                tracing::warn!(outbox_id = row.id, %error, "stale spawned settlement lock timed out; deferring ambiguous row");
            }
            Err(error) => {
                tracing::error!(outbox_id = row.id, %error, "stale spawned settlement failed")
            }
        }
    }
    if stats.truncated_dispatched > 0 || stats.truncated_spawned > 0 {
        tracing::warn!(
            remaining_dispatched = stats.truncated_dispatched,
            remaining_spawned = stats.truncated_spawned,
            "intake delivery sweep batch limit left stale rows for a later tick"
        );
    }
    Ok(stats)
}

struct ActiveTaskGuard<'a>(&'a AtomicBool);

impl Drop for ActiveTaskGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn claim_active(latch: &AtomicBool) -> Option<ActiveTaskGuard<'_>> {
    latch
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .ok()
        .map(|_| ActiveTaskGuard(latch))
}

async fn contain_tick<F: Future>(future: F) -> Result<F::Output, Box<dyn std::any::Any + Send>> {
    AssertUnwindSafe(future).catch_unwind().await
}

pub(super) fn spawn_intake_delivery_sweep(shared: Arc<SharedData>) {
    let Some(pool) = shared.pg_pool.clone() else {
        return;
    };
    let Some(active) = claim_active(&SWEEP_ACTIVE) else {
        return;
    };
    crate::services::discord::task_supervisor::spawn_observed(
        "intake_delivery_sweep",
        async move {
            let _active = active;
            tokio::time::sleep(std::time::Duration::from_secs(SWEEP_INITIAL_DELAY_SECS)).await;
            let mut interval =
                tokio::time::interval(std::time::Duration::from_secs(SWEEP_INTERVAL_SECS));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let tick = contain_tick(async {
                    let config = crate::config_live_reload::current()
                        .unwrap_or_else(|| Arc::new(crate::config::Config::default()));
                    let (dispatched, spawned, limit) =
                        config.runtime.intake_delivery_sweep_settings();
                    sweep_once(
                        &pool,
                        shared.intake_delivery_capabilities.current(),
                        SweepCutoffs::from_now(dispatched, spawned),
                        limit,
                    )
                    .await
                })
                .await;
                match tick {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => tracing::error!(%error, "intake delivery sweep tick failed"),
                    Err(_) => {
                        tracing::error!("intake delivery sweep tick panicked; loop remains active")
                    }
                }
            }
        },
    );
}

#[cfg(test)]
mod tests;
