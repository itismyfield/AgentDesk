//! Worker writes serialize stamped rows with the durable channel owner.
use super::IntakeOutboxRow;
use crate::services::cluster::intake_router_hook::owner_record::OwnerIdentity;
use sqlx::{PgPool, Postgres, QueryBuilder, Transaction};

#[derive(sqlx::FromRow)]
struct WorkerSnapshot {
    id: i64,
    provider: String,
    channel_id: String,
    target_instance_id: String,
    owner_instance_id: Option<String>,
    owner_generation: Option<i64>,
    claim_owner: Option<String>,
}

enum Stamp {
    Legacy,
    Owned(OwnerIdentity),
    Invalid,
}

impl WorkerSnapshot {
    fn stamp(&self) -> Stamp {
        match (&self.owner_instance_id, self.owner_generation) {
            (None, None) => Stamp::Legacy,
            (Some(owner), Some(generation)) if !owner.trim().is_empty() && generation >= 0 => {
                Stamp::Owned(OwnerIdentity::new(&self.provider, &self.channel_id))
            }
            _ => Stamp::Invalid,
        }
    }
}

async fn begin(pool: &PgPool) -> Result<Transaction<'_, Postgres>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

async fn lock_owner(
    tx: &mut Transaction<'_, Postgres>,
    id: &OwnerIdentity,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(id.advisory_key())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

// Bind Rust's normalized identity; SQL btrim/lower do not have identical Unicode semantics.
fn push_fence(sql: &mut QueryBuilder<Postgres>, row: &WorkerSnapshot, stamp: &Stamp) {
    sql.push(" AND io.provider = ")
        .push_bind(row.provider.clone())
        .push(" AND io.channel_id = ")
        .push_bind(row.channel_id.clone())
        .push(" AND io.target_instance_id = ")
        .push_bind(row.target_instance_id.clone());
    match stamp {
        Stamp::Legacy => {
            sql.push(" AND io.owner_instance_id IS NULL AND io.owner_generation IS NULL");
        }
        Stamp::Owned(identity) => {
            sql.push(" AND io.owner_instance_id = ")
                .push_bind(row.owner_instance_id.clone())
                .push(" AND io.owner_generation = ")
                .push_bind(row.owner_generation)
                .push(" AND EXISTS (SELECT 1 FROM intake_session_owners o WHERE o.provider = ")
                .push_bind(identity.provider().to_string())
                .push(" AND o.raw_channel_id = ")
                .push_bind(identity.raw_channel_id().to_string())
                .push(" AND o.status = 'active' AND o.owner_instance_id = ")
                .push_bind(row.owner_instance_id.clone())
                .push(" AND o.generation = ")
                .push_bind(row.owner_generation)
                .push(")");
        }
        Stamp::Invalid => {
            sql.push(" AND FALSE");
        }
    }
}

// Historical delegated inputs stay held even when their old home still matches.
const HOME_CLAIM_FENCE: &str = "io.home_epoch IS NULL";
const REPLAY_CLAIM_FENCE: &str = "NOT replay_sources_blocked(io.provider, io.channel_id, COALESCE(io.replay_source_message_ids, ARRAY[io.user_msg_id]))";

/// Claim the oldest eligible row without waiting on row or owner locks.
pub(crate) async fn claim_pending_for_target_except(
    pool: &PgPool,
    target_instance_id: &str,
    provider: &str,
    claim_owner: &str,
    held_channels: &[String],
) -> Result<Option<IntakeOutboxRow>, sqlx::Error> {
    let mut rejected = Vec::<i64>::new();
    loop {
        let mut tx = begin(pool).await?;
        let candidate: Option<WorkerSnapshot> = {
            let mut sql =
                QueryBuilder::new("SELECT io.* FROM intake_outbox io WHERE io.target_instance_id=");
            sql.push_bind(target_instance_id)
                .push(" AND io.provider=")
                .push_bind(provider)
                .push(" AND io.status='pending'");
            if !held_channels.is_empty() {
                sql.push(" AND NOT (io.channel_id = ANY(")
                    .push_bind(held_channels)
                    .push("::TEXT[]))");
            }
            if !rejected.is_empty() {
                sql.push(" AND NOT (io.id = ANY(")
                    .push_bind(&rejected)
                    .push("::BIGINT[]))");
            }
            sql.push(" AND ")
                .push(HOME_CLAIM_FENCE)
                .push(" AND ")
                .push(REPLAY_CLAIM_FENCE)
                .push(" ORDER BY io.created_at ASC LIMIT 1 FOR UPDATE OF io SKIP LOCKED");
            sql.build_query_as().fetch_optional(&mut *tx).await?
        };
        let Some(row) = candidate else {
            tx.commit().await?;
            return Ok(None);
        };
        #[cfg(test)]
        crate::services::cluster::intake_worker::test_executor::claim_selected().await;
        let stamp = row.stamp();
        if let Some(claimed) = confirm_selected(&mut tx, &row, claim_owner).await? {
            tx.commit().await?;
            return Ok(Some(claimed));
        }
        tx.rollback().await?;
        if matches!(stamp, Stamp::Legacy) {
            return Ok(None);
        }
        rejected.push(row.id);
    }
}

async fn confirm_selected(
    tx: &mut Transaction<'_, Postgres>,
    row: &WorkerSnapshot,
    claim_owner: &str,
) -> Result<Option<IntakeOutboxRow>, sqlx::Error> {
    let stamp = row.stamp();
    let available = match &stamp {
        Stamp::Legacy => true,
        Stamp::Invalid => false,
        // Retirement takes owner then row: never wait for the owner while holding this row.
        Stamp::Owned(identity) => {
            sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_xact_lock($1)")
                .bind(identity.advisory_key())
                .fetch_one(&mut **tx)
                .await?
        }
    };
    if available {
        let mut sql = QueryBuilder::new(
            "UPDATE intake_outbox io SET status='claimed', claimed_at=NOW(), claim_owner=",
        );
        sql.push_bind(claim_owner)
            .push(" WHERE io.id=")
            .push_bind(row.id)
            .push(" AND io.status='pending' AND ")
            .push(HOME_CLAIM_FENCE)
            .push(" AND ")
            .push(REPLAY_CLAIM_FENCE);
        push_fence(&mut sql, row, &stamp);
        if matches!(stamp, Stamp::Owned(_)) {
            sql.push(
                " AND (io.admission_kind='forwarded' OR (io.admission_kind='local' AND io.routing_receipt_id IS NOT NULL AND io.routing_state='admitted')) AND io.target_instance_id=io.owner_instance_id",
            );
        }
        sql.push(" RETURNING io.*");
        let claimed = sql.build_query_as().fetch_optional(&mut **tx).await?;
        return Ok(claimed);
    }
    Ok(None)
}

// Compatibility coverage exercises the confirming UPDATE independently of candidate selection.
#[cfg(test)]
pub(super) async fn confirm_claim(
    tx: &mut Transaction<'_, Postgres>,
    id: i64,
    claim_owner: &str,
) -> Result<Option<IntakeOutboxRow>, sqlx::Error> {
    let row: Option<WorkerSnapshot> =
        sqlx::query_as("SELECT * FROM intake_outbox WHERE id=$1 AND status='pending'")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await?;
    match row {
        Some(row) => confirm_selected(tx, &row, claim_owner).await,
        None => Ok(None),
    }
}

#[cfg(test)]
pub(crate) async fn claim_pending_for_target(
    pool: &PgPool,
    target_instance_id: &str,
    provider: &str,
    claim_owner: &str,
) -> Result<Option<IntakeOutboxRow>, sqlx::Error> {
    claim_pending_for_target_except(pool, target_instance_id, provider, claim_owner, &[]).await
}

#[derive(Clone, Copy)]
enum Change {
    Rearm,
    Accepted,
    Spawned,
    Done,
    FailedPre,
    FailedPost,
}
impl Change {
    fn from(self) -> &'static [&'static str] {
        match self {
            Self::Rearm | Self::Accepted | Self::FailedPre => &["claimed"],
            Self::Spawned => &["accepted"],
            Self::Done => &["spawned"],
            Self::FailedPost => &["accepted", "spawned"],
        }
    }
    fn set(self) -> &'static str {
        match self {
            Self::Rearm => "status='pending', claim_owner=NULL, claimed_at=NULL",
            Self::Accepted => "status='accepted', accepted_at=NOW()",
            Self::Spawned => "status='spawned', spawned_at=NOW()",
            Self::Done => "status='done', completed_at=NOW()",
            Self::FailedPre => {
                "status='failed_pre_accept', completed_at=NOW(), retry_count=retry_count+1"
            }
            Self::FailedPost => "status='failed_post_accept', completed_at=NOW()",
        }
    }
}

async fn update_status(
    tx: &mut Transaction<'_, Postgres>,
    row: &WorkerSnapshot,
    change: Change,
    error: Option<&str>,
    stale_after_secs: Option<i64>,
) -> Result<bool, sqlx::Error> {
    let stamp = row.stamp();
    match &stamp {
        Stamp::Invalid => return Ok(false),
        Stamp::Owned(identity) => lock_owner(tx, identity).await?,
        Stamp::Legacy => {}
    }
    // This statement gets its snapshot after the owner lock and keeps that lock through commit.
    let mut sql = QueryBuilder::new("UPDATE intake_outbox io SET ");
    sql.push(change.set());
    if let Some(error) = error {
        sql.push(", last_error=").push_bind(error);
    }
    sql.push(" WHERE io.id=")
        .push_bind(row.id)
        .push(" AND io.status=ANY(")
        .push_bind(change.from())
        .push("::TEXT[])")
        .push(" AND io.claim_owner IS NOT DISTINCT FROM ")
        .push_bind(row.claim_owner.clone());
    push_fence(&mut sql, row, &stamp);
    if matches!(change, Change::Accepted) {
        sql.push(" AND ").push(HOME_CLAIM_FENCE);
    }
    if matches!(change, Change::Accepted | Change::Rearm) {
        sql.push(" AND ").push(REPLAY_CLAIM_FENCE);
    }
    if let Some(age) = stale_after_secs {
        sql.push(" AND io.claimed_at < NOW() - (")
            .push_bind(age)
            .push("::BIGINT * INTERVAL '1 second')");
    }
    Ok(sql.build().execute(&mut **tx).await?.rows_affected() == 1)
}

async fn transition(
    pool: &PgPool,
    id: i64,
    claim_owner: &str,
    change: Change,
    error: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let mut tx = begin(pool).await?;
    let row: Option<WorkerSnapshot> = sqlx::query_as(
        "SELECT * FROM intake_outbox
        WHERE id=$1 AND claim_owner=$2 AND status=ANY($3::TEXT[])",
    )
    .bind(id)
    .bind(claim_owner)
    .bind(change.from())
    .fetch_optional(&mut *tx)
    .await?;
    let changed = match row {
        Some(row) => update_status(&mut tx, &row, change, error, None).await?,
        None => false,
    };
    tx.commit().await?;
    Ok(changed)
}

/// Release only this pre-accept claim, preserving retry/error bookkeeping.
pub(crate) async fn return_claimed_to_pending(
    pool: &PgPool,
    id: i64,
    claim_owner: &str,
) -> Result<bool, sqlx::Error> {
    transition(pool, id, claim_owner, Change::Rearm, None).await
}

/// A refused accept must stop the worker before it invokes the executor.
pub(crate) async fn mark_accepted(
    pool: &PgPool,
    id: i64,
    claim_owner: &str,
) -> Result<bool, sqlx::Error> {
    transition(pool, id, claim_owner, Change::Accepted, None).await
}

pub(crate) async fn mark_spawned(
    pool: &PgPool,
    id: i64,
    claim_owner: &str,
) -> Result<bool, sqlx::Error> {
    transition(pool, id, claim_owner, Change::Spawned, None).await
}

/// A miss may also mean relay dispatch or terminal settlement already won.
pub(crate) async fn mark_done(
    pool: &PgPool,
    id: i64,
    claim_owner: &str,
) -> Result<bool, sqlx::Error> {
    transition(pool, id, claim_owner, Change::Done, None).await
}

pub(crate) async fn mark_failed_pre_accept(
    pool: &PgPool,
    id: i64,
    claim_owner: &str,
    error_message: &str,
) -> Result<bool, sqlx::Error> {
    transition(
        pool,
        id,
        claim_owner,
        Change::FailedPre,
        Some(error_message),
    )
    .await
}

pub(crate) async fn mark_failed_post_accept(
    pool: &PgPool,
    id: i64,
    claim_owner: &str,
    error_message: &str,
) -> Result<bool, sqlx::Error> {
    transition(
        pool,
        id,
        claim_owner,
        Change::FailedPost,
        Some(error_message),
    )
    .await
}

/// Existing maintenance API; no production sweep is started here. Errors may follow partial commits.
#[cfg(test)]
pub(crate) async fn sweep_stale_pre_accept_claims(
    pool: &PgPool,
    stale_after_secs: i64,
) -> Result<u64, sqlx::Error> {
    let result = sqlx::query("UPDATE intake_outbox io SET status='pending', claim_owner=NULL, claimed_at=NULL
        WHERE io.status='claimed' AND io.owner_instance_id IS NULL AND io.owner_generation IS NULL
          AND io.claimed_at < NOW() - ($1::BIGINT * INTERVAL '1 second')
          AND NOT replay_sources_blocked(io.provider, io.channel_id, COALESCE(io.replay_source_message_ids, ARRAY[io.user_msg_id]))")
        .bind(stale_after_secs.max(1)).execute(pool).await?;
    Ok(result.rows_affected() + sweep_stamped_claims(pool, stale_after_secs).await?)
}

/// Stamped-only maintenance. Each row commits independently; an error can leave earlier rows rearmed.
pub(crate) async fn sweep_stamped_claims(
    pool: &PgPool,
    stale_after_secs: i64,
) -> Result<u64, sqlx::Error> {
    let age = stale_after_secs.max(1);
    let candidates: Vec<WorkerSnapshot> = sqlx::query_as(
        "SELECT * FROM intake_outbox
        WHERE status='claimed' AND (owner_instance_id IS NOT NULL OR owner_generation IS NOT NULL)
          AND claimed_at < NOW() - ($1::BIGINT * INTERVAL '1 second') ORDER BY id",
    )
    .bind(age)
    .fetch_all(pool)
    .await?;
    let mut count = 0;
    for row in candidates {
        let mut tx = begin(pool).await?;
        let changed = update_status(&mut tx, &row, Change::Rearm, None, Some(age)).await?;
        tx.commit().await?;
        count += u64::from(changed);
    }
    Ok(count)
}
