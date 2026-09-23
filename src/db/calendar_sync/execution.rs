use super::*;

// Bound ownership across provider I/O; retry backoff applies only before dispatch.
const OPERATION_LEASE_SECONDS: i64 = 90;
const RETRY_BACKOFF_BASE_SECONDS: i64 = 15;
const RETRY_BACKOFF_MAX_SECONDS: i64 = 15 * 60;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecoveryResolution {
    Retry,
    Adopt,
    ConfirmNotApplied,
}

#[derive(sqlx::FromRow)]
pub(crate) struct Claim {
    pub id: Uuid,
    pub target_id: Uuid,
    pub event_id: Uuid,
    pub claim_token: Uuid,
    pub revision: i64,
    pub action: String,
    pub snapshot: Value,
    pub account_id: String,
    pub app_id: i64,
    pub user_id: i64,
    pub remote_id: Option<String>,
    pub attempts: i32,
}

pub(crate) async fn claim(
    pool: &PgPool,
    accounts: &[String],
) -> Result<Option<Claim>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE kakao_calendar_operations SET status='needs_reconcile',error_code='dispatch_interrupted',updated_at=NOW()
        WHERE status='dispatching' AND lease_expires_at <= NOW()") .execute(&mut *tx).await?;
    sqlx::query("UPDATE kakao_calendar_operations SET status='queued',claim_token=NULL,lease_expires_at=NULL,updated_at=NOW()
        WHERE status='preparing' AND lease_expires_at <= NOW() AND dispatched_at IS NULL").execute(&mut *tx).await?;
    sqlx::query("UPDATE kakao_calendar_operations o SET status='superseded',updated_at=NOW()
        FROM kakao_calendar_targets t JOIN kakao_calendar_events e ON e.id=t.event_id
        WHERE o.target_id=t.id AND o.revision < e.revision AND o.status='queued' AND o.dispatched_at IS NULL").execute(&mut *tx).await?;
    let row = sqlx::query_as::<_, Claim>("WITH candidate AS (
        SELECT o.id FROM kakao_calendar_operations o JOIN kakao_calendar_targets t ON t.id=o.target_id
        JOIN kakao_calendar_bindings b USING(binding_id)
        WHERE o.status='queued' AND o.next_attempt_at <= NOW() AND b.account_id=ANY($1)
        AND NOT EXISTS(SELECT 1 FROM kakao_calendar_operations prior WHERE prior.target_id=o.target_id
            AND prior.revision < o.revision AND prior.status NOT IN ('applied','superseded'))
        ORDER BY o.created_at,o.id FOR UPDATE OF o SKIP LOCKED LIMIT 1
    ), claimed AS (
        UPDATE kakao_calendar_operations o SET status='preparing',claim_token=$2,lease_expires_at=NOW()+($3::BIGINT*INTERVAL '1 second'),attempts=attempts+1,updated_at=NOW()
        FROM candidate c WHERE o.id=c.id RETURNING o.*
    ) SELECT c.id,c.target_id,t.event_id,c.claim_token,c.revision,c.action,c.snapshot,c.attempts,
        b.account_id,b.app_id,b.user_id,t.remote_id FROM claimed c
        JOIN kakao_calendar_targets t ON t.id=c.target_id JOIN kakao_calendar_bindings b USING(binding_id)")
        .bind(accounts).bind(Uuid::new_v4()).bind(OPERATION_LEASE_SECONDS).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(row)
}

pub(crate) async fn dispatch(pool: &PgPool, claim: &Claim) -> Result<bool, sqlx::Error> {
    // Lock the parent in the same order as mutation. Revision/tombstone cannot change during the fence.
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM kakao_calendar_events WHERE id=$1 FOR UPDATE")
        .bind(claim.event_id)
        .execute(&mut *tx)
        .await?;
    let result = sqlx::query("UPDATE kakao_calendar_operations o SET status='dispatching',dispatched_at=NOW(),lease_expires_at=NOW()+($5::BIGINT*INTERVAL '1 second'),updated_at=NOW()
        FROM kakao_calendar_targets t JOIN kakao_calendar_events e ON e.id=t.event_id JOIN kakao_calendar_bindings b USING(binding_id)
        WHERE o.id=$1 AND o.claim_token=$2 AND o.target_id=t.id AND o.status='preparing'
        AND o.dispatched_at IS NULL AND o.lease_expires_at > NOW() AND e.revision=o.revision
        AND e.deleted=(o.action='delete') AND b.app_id=$3 AND b.user_id=$4
        AND NOT EXISTS(SELECT 1 FROM kakao_calendar_operations prior WHERE prior.target_id=o.target_id
            AND prior.revision < o.revision AND prior.status NOT IN ('applied','superseded'))")
        .bind(claim.id).bind(claim.claim_token).bind(claim.app_id).bind(claim.user_id).bind(OPERATION_LEASE_SECONDS).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(result.rows_affected() == 1)
}

pub(crate) async fn complete(
    pool: &PgPool,
    claim: &Claim,
    remote: Option<&str>,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM kakao_calendar_events WHERE id=$1 FOR UPDATE")
        .bind(claim.event_id)
        .execute(&mut *tx)
        .await?;
    // Expired lease is not a reason to throw away a late confirmed response. Matching token remains required.
    let changed = sqlx::query(
        "UPDATE kakao_calendar_operations SET status='applied',error_code=NULL,updated_at=NOW()
        WHERE id=$1 AND claim_token=$2 AND status IN ('dispatching','needs_reconcile')",
    )
    .bind(claim.id)
    .bind(claim.claim_token)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if changed {
        sqlx::query(
            "UPDATE kakao_calendar_targets SET remote_id=$2,applied_revision=$3 WHERE id=$1",
        )
        .bind(claim.target_id)
        .bind(remote)
        .bind(claim.revision)
        .execute(&mut *tx)
        .await?;
        if claim.action == "delete" {
            // Retain durable IDs/fingerprints/tombstone, but scrub personal details after every target is deleted.
            let scrubbed = sqlx::query("UPDATE kakao_calendar_events e SET content='{}'::jsonb WHERE e.id=$1 AND e.deleted
                AND NOT EXISTS(SELECT 1 FROM kakao_calendar_targets t WHERE t.event_id=e.id AND (t.remote_id IS NOT NULL OR t.applied_revision <> e.revision))")
                .bind(claim.event_id).execute(&mut *tx).await?.rows_affected() == 1;
            if scrubbed {
                sqlx::query("UPDATE kakao_calendar_operations o SET snapshot='{}'::jsonb,recovery_note=NULL FROM kakao_calendar_targets t WHERE o.target_id=t.id AND t.event_id=$1")
                    .bind(claim.event_id).execute(&mut *tx).await?;
            }
        }
    }
    tx.commit().await?;
    Ok(changed)
}

pub(crate) async fn fail(
    pool: &PgPool,
    claim: &Claim,
    status: &str,
    code: &str,
) -> Result<(), sqlx::Error> {
    // Only pre-dispatch retry may return to queued. Unknown calls retain their original token as late evidence.
    sqlx::query(
        "UPDATE kakao_calendar_operations SET status=$3,error_code=$4,
        next_attempt_at=NOW()+(LEAST(attempts::BIGINT*attempts*$5::BIGINT,$6::BIGINT)*INTERVAL '1 second'),updated_at=NOW()
        WHERE id=$1 AND claim_token=$2 AND status IN ('preparing','dispatching')
        AND ($3 <> 'queued' OR dispatched_at IS NULL)",
    )
    .bind(claim.id)
    .bind(claim.claim_token)
    .bind(status)
    .bind(code)
    .bind(RETRY_BACKOFF_BASE_SECONDS)
    .bind(RETRY_BACKOFF_MAX_SECONDS)
    .execute(pool)
    .await?;
    Ok(())
}

pub(crate) async fn recovery_target(
    pool: &PgPool,
    event: Uuid,
    operation: Uuid,
) -> Result<Claim, CalendarDbError> {
    sqlx::query_as::<_,Claim>("SELECT o.id,o.target_id,t.event_id,COALESCE(o.claim_token,gen_random_uuid()) AS claim_token,
        o.revision,o.action,o.snapshot,o.attempts,b.account_id,b.app_id,b.user_id,t.remote_id
        FROM kakao_calendar_operations o JOIN kakao_calendar_targets t ON t.id=o.target_id JOIN kakao_calendar_bindings b USING(binding_id)
        WHERE o.id=$1 AND t.event_id=$2 AND o.status IN ('blocked','rejected','needs_reconcile')")
        .bind(operation).bind(event).fetch_optional(pool).await?.ok_or(CalendarDbError::Conflict)
}

pub(crate) async fn recover(
    pool: &PgPool,
    claim: &Claim,
    resolution: RecoveryResolution,
    remote: Option<&str>,
    note: &str,
) -> Result<(), CalendarDbError> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM kakao_calendar_events WHERE id=$1 FOR UPDATE")
        .bind(claim.event_id)
        .execute(&mut *tx)
        .await?;
    let row = sqlx::query(
        "SELECT status,lease_expires_at > NOW() AS lease_active,claim_token FROM kakao_calendar_operations WHERE id=$1 FOR UPDATE",
    )
    .bind(claim.id)
    .fetch_one(&mut *tx)
    .await?;
    let status: String = row.get("status");
    if row.get::<Option<Uuid>, _>("claim_token") != Some(claim.claim_token) {
        return Err(CalendarDbError::Conflict);
    }
    if !["blocked", "rejected", "needs_reconcile"].contains(&status.as_str()) {
        return Err(CalendarDbError::Conflict);
    }
    if status == "needs_reconcile" && row.get::<Option<bool>, _>("lease_active").unwrap_or(false) {
        return Err(CalendarDbError::Conflict);
    }
    match resolution {
        RecoveryResolution::Retry if status != "needs_reconcile" => {
            sqlx::query("UPDATE kakao_calendar_operations SET status='queued',claim_token=NULL,dispatched_at=NULL,lease_expires_at=NULL,error_code=NULL,recovery_note=$2,next_attempt_at=NOW(),updated_at=NOW() WHERE id=$1")
                .bind(claim.id).bind(note).execute(&mut *tx).await?;
        }
        RecoveryResolution::Adopt
            if status == "needs_reconcile" && remote.is_some() && claim.action != "delete" =>
        {
            sqlx::query(
                "UPDATE kakao_calendar_targets SET remote_id=$2,applied_revision=$3 WHERE id=$1",
            )
            .bind(claim.target_id)
            .bind(remote)
            .bind(claim.revision)
            .execute(&mut *tx)
            .await?;
            sqlx::query("UPDATE kakao_calendar_operations SET status='applied',claim_token=NULL,error_code=NULL,recovery_note=$2,updated_at=NOW() WHERE id=$1")
                .bind(claim.id).bind(note).execute(&mut *tx).await?;
        }
        // Explicit operator judgment after stopping the old credential owner. Never an automatic replay.
        RecoveryResolution::ConfirmNotApplied if status == "needs_reconcile" => {
            sqlx::query("UPDATE kakao_calendar_operations SET status='queued',claim_token=NULL,dispatched_at=NULL,lease_expires_at=NULL,error_code=NULL,recovery_note=$2,next_attempt_at=NOW(),updated_at=NOW() WHERE id=$1")
                .bind(claim.id).bind(note).execute(&mut *tx).await?;
        }
        _ => return Err(CalendarDbError::Conflict),
    }
    tx.commit().await?;
    Ok(())
}
