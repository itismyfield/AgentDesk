//! Restartable cleanup belongs to the original owner history row, never to its successor.
use super::{OwnerIdentity, custody};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Work {
    pub operation: Uuid,
    #[serde(default = "control_seconds")]
    pub control_seconds: i32,
    pub target: String,
    pub home_revision: i64,
    pub pending: i64,
    pub native: Option<Value>,
}
#[derive(Clone, Debug, sqlx::FromRow)]
pub(crate) struct Record {
    pub id: i64,
    pub provider: String,
    pub raw_channel_id: String,
    pub owner_instance_id: String,
    pub generation: i64,
    pub routing_cleanup: sqlx::types::Json<Work>,
    pub routing_cleanup_state: String,
    pub routing_cleanup_deadline: DateTime<Utc>,
    pub routing_cleanup_seen: Option<DateTime<Utc>>,
    pub routing_cleanup_next: DateTime<Utc>,
    pub routing_cleanup_attempts: i32,
    pub routing_cleanup_claim: Option<Uuid>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Response {
    Busy,
    Retry,
    Ended,
    NeedsOperator,
    IdentityChanged,
}
impl Response {
    fn state(self) -> &'static str {
        match self {
            Self::Busy => "busy",
            Self::Retry => "retry",
            Self::Ended => "ended",
            Self::NeedsOperator => "needs_operator",
            Self::IdentityChanged => "identity_changed",
        }
    }
}

/// The same old generation keeps its first operation, native identity and deadline.
pub(crate) async fn request(
    pool: &PgPool,
    id: &OwnerIdentity,
    old: (&str, i64),
    pending: i64,
    target: &str,
    timeout: i32,
) -> Result<bool, sqlx::Error> {
    if timeout <= 0 {
        return Err(sqlx::Error::Protocol("invalid control deadline".into()));
    }
    let mut tx = pool.begin().await?;
    custody::lock(&mut tx, id).await?;
    let held:Option<i64>=sqlx::query_scalar("SELECT routing_home_revision FROM intake_outbox WHERE id=$1 AND provider=$2 AND channel_id=$3 AND routing_state='held' AND routing_wait_owner_generation=$4 AND target_instance_id=$5")
        .bind(pending).bind(id.provider()).bind(id.raw_channel_id()).bind(old.1).bind(target).fetch_optional(&mut *tx).await?;
    let Some(home_revision) = held else {
        return Ok(false);
    };
    let native:Option<Value>=sqlx::query_scalar("SELECT r.replay_preserved #> '{projection,binding}' FROM intake_outbox d JOIN intake_outbox r ON r.id=d.routing_receipt_id WHERE d.provider=$1 AND d.channel_id=$2 AND d.owner_instance_id=$3 AND d.owner_generation=$4 AND r.replay_preserved #> '{projection,binding}' IS NOT NULL ORDER BY d.id DESC LIMIT 1")
        .bind(id.provider()).bind(id.raw_channel_id()).bind(old.0).bind(old.1).fetch_optional(&mut *tx).await?;
    let work = Work {
        control_seconds: timeout,
        operation: Uuid::new_v4(),
        target: target.into(),
        home_revision,
        pending,
        native,
    };
    let changed=sqlx::query("UPDATE intake_session_owners SET routing_cleanup=$5,routing_cleanup_state='pending',routing_cleanup_deadline=NOW()+make_interval(secs=>$6::INT::DOUBLE PRECISION),routing_cleanup_next=NOW() WHERE provider=$1 AND raw_channel_id=$2 AND owner_instance_id=$3 AND generation=$4 AND routing_cleanup IS NULL")
        .bind(id.provider()).bind(id.raw_channel_id()).bind(old.0).bind(old.1).bind(sqlx::types::Json(work)).bind(timeout).execute(&mut *tx).await?.rows_affected();
    #[cfg(test)]
    if changed == 0 && tests::mutant("deadline") {
        sqlx::query("UPDATE intake_session_owners SET routing_cleanup_deadline=NOW()+INTERVAL '1 hour' WHERE provider=$1 AND raw_channel_id=$2 AND generation=$3").bind(id.provider()).bind(id.raw_channel_id()).bind(old.1).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(changed == 1)
}

pub(crate) async fn due(
    pool: &PgPool,
    provider: &str,
    local: &str,
) -> Result<Vec<Record>, sqlx::Error> {
    sqlx::query_as("SELECT * FROM intake_session_owners WHERE provider=$1 AND owner_instance_id=$2 AND routing_cleanup IS NOT NULL AND ((routing_cleanup_state IN ('pending','busy','running','retry','deferred') AND routing_cleanup_attempts<3) OR (routing_cleanup_state IN ('ended','deferred') AND EXISTS(SELECT 1 FROM intake_outbox d WHERE d.id::TEXT=routing_cleanup->>'pending' AND d.routing_state='held'))) AND routing_cleanup_next<=NOW() ORDER BY routing_cleanup_next,id LIMIT 32")
        .bind(provider).bind(local).fetch_all(pool).await
}
async fn locked<'a>(
    pool: &'a PgPool,
    row: &Record,
) -> Result<Transaction<'a, Postgres>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    custody::lock(
        &mut tx,
        &OwnerIdentity::new(&row.provider, &row.raw_channel_id),
    )
    .await?;
    Ok(tx)
}
/// Claim leases bound retries after crashes; callbacks must match the exact operation and lease.
pub(crate) async fn claim(pool: &PgPool, row: &Record) -> Result<Option<Record>, sqlx::Error> {
    let mut tx = locked(pool, row).await?;
    let lease = Uuid::new_v4();
    let picked=sqlx::query_as("UPDATE intake_session_owners SET routing_cleanup_state='running',routing_cleanup_claim=$3,routing_cleanup_next=NOW()+make_interval(secs=>COALESCE((routing_cleanup->>'control_seconds')::INT,12)::DOUBLE PRECISION) WHERE id=$1 AND routing_cleanup=$2 AND routing_cleanup_state IN ('pending','busy','running','retry','deferred') AND routing_cleanup_attempts<3 AND routing_cleanup_next<=NOW() RETURNING *")
        .bind(row.id).bind(&row.routing_cleanup).bind(lease).fetch_optional(&mut *tx).await?;
    tx.commit().await?;
    Ok(picked)
}
/// The attempt is durable before the existing end path may produce an effect.
pub(crate) async fn begin_end(pool: &PgPool, row: &Record) -> Result<bool, sqlx::Error> {
    #[cfg(test)]
    if tests::mutant("budget") {
        return Ok(true);
    }

    let changed=sqlx::query("UPDATE intake_session_owners SET routing_cleanup_attempts=routing_cleanup_attempts+1 WHERE id=$1 AND routing_cleanup=$2 AND routing_cleanup_claim=$3 AND routing_cleanup_state='running' AND routing_cleanup_attempts=$4 AND routing_cleanup_attempts<3")
        .bind(row.id).bind(&row.routing_cleanup).bind(row.routing_cleanup_claim).bind(row.routing_cleanup_attempts).execute(pool).await?.rows_affected();
    Ok(changed == 1)
}
pub(crate) async fn respond(
    pool: &PgPool,
    row: &Record,
    response: Response,
) -> Result<bool, sqlx::Error> {
    let mut tx = locked(pool, row).await?;
    #[cfg(test)]
    let late = tests::mutant("late");
    #[cfg(not(test))]
    let late = false;
    let changed=sqlx::query("UPDATE intake_session_owners SET routing_cleanup_state=CASE WHEN $4='retry' AND (routing_cleanup_attempts>=3 OR routing_cleanup_deadline<=NOW()) THEN 'deferred' ELSE $4 END,routing_cleanup_seen=NOW(),routing_cleanup_next=NOW()+INTERVAL '2 seconds',routing_cleanup_deferred=($4 IN ('retry','needs_operator','identity_changed')) WHERE id=$1 AND routing_cleanup=$2 AND (routing_cleanup_claim=$3 OR $5) AND routing_cleanup_state='running'")
        .bind(row.id).bind(&row.routing_cleanup).bind(row.routing_cleanup_claim).bind(response.state()).bind(late).execute(&mut *tx).await?.rows_affected();
    tx.commit().await?;
    Ok(changed == 1)
}

/// Only persisted control results can release a staged source; absence of an ACK is deferred.
pub(crate) async fn advance(pool: &PgPool, id: i64) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let Some(row) = sqlx::query_as::<_, Record>(
        "SELECT * FROM intake_session_owners WHERE id=$1 AND routing_cleanup IS NOT NULL",
    )
    .bind(id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        return Ok(false);
    };
    let ready=sqlx::query_scalar::<_,bool>("SELECT routing_cleanup_deadline<=NOW() AND (routing_cleanup_seen IS NULL OR routing_cleanup_seen+make_interval(secs=>COALESCE((routing_cleanup->>'control_seconds')::INT,12)::DOUBLE PRECISION)<=NOW()) FROM intake_session_owners WHERE id=$1").bind(id).fetch_one(&mut *tx).await?;
    #[cfg(test)]
    let ready = ready || tests::mutant("busy");
    let readiness = match row.routing_cleanup_state.as_str() {
        "ended" => custody::SwitchReadiness::NormalEnd,
        "needs_operator" | "identity_changed" | "deferred" => custody::SwitchReadiness::Unavailable,
        "retry" if row.routing_cleanup_attempts >= 3 => custody::SwitchReadiness::Unavailable,
        _ if ready => custody::SwitchReadiness::Unavailable,
        _ => return Ok(false),
    };
    tx.rollback().await?;
    let pending:Option<(i64,String,bool)>=sqlx::query_as("SELECT d.id,d.target_instance_id,d.routing_plan->>'target' IS NOT DISTINCT FROM d.target_instance_id FROM intake_outbox d JOIN intake_outbox r ON r.id=d.routing_receipt_id WHERE d.provider=$1 AND d.channel_id=$2 AND d.routing_state='held' AND d.routing_wait_owner_generation=$3 AND r.replay_disposition='registered_not_started' ORDER BY d.id LIMIT 1")
        .bind(&row.provider).bind(&row.raw_channel_id).bind(row.generation).fetch_optional(pool).await?;
    let Some((pending, mut target, planned)) = pending else {
        return Ok(false);
    };
    #[cfg(test)]
    let planned = planned || tests::mutant("advance-plan-skip");
    if !planned {
        let identity = OwnerIdentity::new(&row.provider, &row.raw_channel_id);
        match replan_head(pool, &identity, pending).await? {
            Replan::Legacy => {}
            Replan::Ready(plan) if plan.target != row.owner_instance_id => target = plan.target,
            Replan::Ready(_) | Replan::Wait => return Ok(false),
        }
    }

    custody::promote(
        pool,
        pending,
        &target,
        Some((&row.owner_instance_id, row.generation)),
        readiness,
    )
    .await
}

#[cfg(test)]
pub(crate) mod tests;

/// Recheck control freshness under the transition's owner lock; a fresh Busy response wins a race.
pub(crate) async fn allows_on_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: &OwnerIdentity,
    generation: i64,
    pending: i64,
    target: &str,
    readiness: custody::SwitchReadiness,
) -> Result<bool, sqlx::Error> {
    #[cfg(test)]
    if tests::mutant("no-control") {
        return Ok(true);
    }
    let found:Option<(sqlx::types::Json<Work>,String,bool,i32)>=sqlx::query_as("SELECT routing_cleanup,routing_cleanup_state,routing_cleanup_deadline<=NOW() AND (routing_cleanup_seen IS NULL OR routing_cleanup_seen+make_interval(secs=>COALESCE((routing_cleanup->>'control_seconds')::INT,12)::DOUBLE PRECISION)<=NOW()),routing_cleanup_attempts FROM intake_session_owners WHERE provider=$1 AND raw_channel_id=$2 AND generation=$3 AND routing_cleanup IS NOT NULL")
        .bind(id.provider()).bind(id.raw_channel_id()).bind(generation).fetch_optional(&mut **tx).await?;
    #[cfg(test)]
    if tests::mutant("busy") {
        return Ok(true);
    }
    let Some((work, state, expired, attempts)) = found else {
        return Ok(false);
    };
    let names_target:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM intake_outbox WHERE id=$1 AND routing_state='held' AND target_instance_id=$2)").bind(pending).bind(target).fetch_one(&mut **tx).await?;
    let _ = work;
    if !names_target {
        return Ok(false);
    }
    Ok(match readiness {
        custody::SwitchReadiness::NormalEnd => state == "ended",
        custody::SwitchReadiness::Unavailable => {
            expired
                || matches!(
                    state.as_str(),
                    "deferred" | "needs_operator" | "identity_changed"
                )
                || (state == "retry" && attempts >= 3)
        }
        custody::SwitchReadiness::Wait => false,
    })
}

/// Any healthy runtime may advance an already stored control request; physical end stays local.
pub(crate) async fn advance_pending(pool: &PgPool, provider: &str) -> Result<(), sqlx::Error> {
    #[cfg(test)]
    if tests::mutant("periodic") {
        return Ok(());
    }

    sqlx::query("UPDATE intake_outbox d SET routing_state='superseded' WHERE d.provider=$1 AND d.routing_state='held' AND EXISTS(SELECT 1 FROM intake_outbox r WHERE r.id=d.routing_receipt_id AND r.replay_disposition IN ('withheld','classified_normal'))").bind(provider).execute(pool).await?;
    let ids:Vec<i64>=sqlx::query_scalar("SELECT o.id FROM intake_session_owners o JOIN LATERAL (SELECT d.* FROM intake_outbox d WHERE d.provider=o.provider AND d.channel_id=o.raw_channel_id AND d.routing_wait_owner_generation=o.generation AND d.routing_state='held' ORDER BY d.id LIMIT 1) d ON TRUE WHERE o.provider=$1 AND d.routing_state='held' AND d.routing_home_revision=COALESCE((SELECT revision FROM channel_home_preferences h WHERE h.provider=d.provider AND h.raw_channel_id=d.channel_id),0) AND (o.routing_cleanup_state IN ('ended','needs_operator','identity_changed','deferred') OR o.routing_cleanup_attempts>=3 OR (o.routing_cleanup_deadline<=NOW() AND (o.routing_cleanup_seen IS NULL OR o.routing_cleanup_seen+make_interval(secs=>COALESCE((o.routing_cleanup->>'control_seconds')::INT,12)::DOUBLE PRECISION)<=NOW()))) ORDER BY o.routing_cleanup_deadline,o.id LIMIT 32")
        .bind(provider).fetch_all(pool).await?;
    for id in ids {
        advance(pool, id).await?;
    }
    // A /node change after staging leaves held plans on an old revision; re-plan or release them.
    let stale:Vec<(String,serde_json::Value)>=sqlx::query_as("SELECT d.channel_id,d.routing_plan->'home_proof' FROM intake_outbox d WHERE d.provider=$1 AND d.routing_state='held' AND d.routing_plan IS NOT NULL AND d.routing_home_revision<>COALESCE((SELECT revision FROM channel_home_preferences h WHERE h.provider=d.provider AND h.raw_channel_id=d.channel_id),0) AND NOT EXISTS(SELECT 1 FROM intake_outbox h WHERE h.provider=d.provider AND h.channel_id=d.channel_id AND h.routing_state='held' AND h.id<d.id) ORDER BY d.id LIMIT 32")
        .bind(provider).fetch_all(pool).await?;
    #[cfg(test)]
    let stale = if tests::mutant("stale-replan") {
        Vec::new()
    } else {
        stale
    };
    for (channel, proof) in stale {
        let id = OwnerIdentity::new(provider, &channel);
        if let Some((head, plan)) = super::plan::replan_stale(pool, &id, &proof).await? {
            resume_head(pool, &id, head, &plan, super::super::switch_timeout()).await?;
        }
    }
    let heads:Vec<(i64,String,String,i64,bool)>=sqlx::query_as("SELECT d.id,d.target_instance_id,o.owner_instance_id,o.generation,d.routing_plan->>'target' IS NOT DISTINCT FROM d.target_instance_id FROM intake_outbox d JOIN intake_session_owners o ON o.provider=d.provider AND o.raw_channel_id=d.channel_id AND o.status='active' AND o.generation=d.routing_wait_owner_generation AND o.owner_instance_id=d.target_instance_id WHERE d.provider=$1 AND d.routing_state='held' AND NOT EXISTS(SELECT 1 FROM intake_outbox h WHERE h.provider=d.provider AND h.channel_id=d.channel_id AND h.routing_state='held' AND h.id<d.id) AND EXISTS(SELECT 1 FROM intake_session_owners history WHERE history.provider=d.provider AND history.raw_channel_id=d.channel_id AND history.routing_cleanup IS NOT NULL) ORDER BY d.id LIMIT 32")
        .bind(provider).fetch_all(pool).await?;
    for (id, target, owner, generation, planned) in heads {
        #[cfg(test)]
        let planned = planned || tests::mutant("plan-less-head");
        // Custody without a matching plan is planned afresh first; legacy custody stays put.
        #[cfg(test)]
        if !planned && tests::mutant("heads-replan-skip") {
            continue;
        }
        if !planned {
            let channel =
                sqlx::query_scalar::<_, String>("SELECT channel_id FROM intake_outbox WHERE id=$1")
                    .bind(id)
                    .fetch_one(pool)
                    .await?;
            let identity = OwnerIdentity::new(provider, &channel);
            if let Replan::Ready(plan) = replan_head(pool, &identity, id).await? {
                resume_head(pool, &identity, id, &plan, super::super::switch_timeout()).await?;
            }
            continue;
        }
        custody::promote(
            pool,
            id,
            &target,
            Some((&owner, generation)),
            custody::SwitchReadiness::Wait,
        )
        .await?;
    }
    #[cfg(test)]
    if tests::mutant("plan-head-history") {
        return Ok(());
    }
    // A first switch has no cleanup history; its saved plan still owns the held head.
    let planned:Vec<(i64,String,Option<String>,Option<i64>)>=sqlx::query_as("SELECT d.id,d.target_instance_id,o.owner_instance_id,o.generation FROM intake_outbox d LEFT JOIN LATERAL (SELECT owner_instance_id,generation,status FROM intake_session_owners x WHERE x.provider=d.provider AND x.raw_channel_id=d.channel_id ORDER BY generation DESC LIMIT 1) o ON TRUE WHERE d.provider=$1 AND d.routing_state='held' AND d.routing_plan->>'target'=d.target_instance_id AND (o.status IS DISTINCT FROM 'active' OR o.owner_instance_id=d.target_instance_id) AND NOT EXISTS(SELECT 1 FROM intake_outbox h WHERE h.provider=d.provider AND h.channel_id=d.channel_id AND h.routing_state='held' AND h.id<d.id) AND NOT EXISTS(SELECT 1 FROM intake_session_owners history WHERE history.provider=d.provider AND history.raw_channel_id=d.channel_id AND history.routing_cleanup IS NOT NULL) ORDER BY d.id LIMIT 32")
        .bind(provider).fetch_all(pool).await?;
    for (id, target, owner, generation) in planned {
        custody::promote(
            pool,
            id,
            &target,
            owner.as_deref().zip(generation),
            custody::SwitchReadiness::Wait,
        )
        .await?;
    }
    Ok(())
}

/// The planned head either asks the active other owner to close or is admitted to its target.
pub(crate) async fn resume_head(
    pool: &PgPool,
    id: &OwnerIdentity,
    head: i64,
    plan: &super::plan::Plan,
    timeout: i32,
) -> Result<(), sqlx::Error> {
    match plan.expected.as_ref() {
        Some((owner, generation, status)) if status == "active" && *owner != plan.target => {
            request(pool, id, (owner, *generation), head, &plan.target, timeout).await?;
        }
        expected => {
            // False keeps the head held; advance_pending resumes the stored plan.
            custody::promote(
                pool,
                head,
                &plan.target,
                expected.map(|(owner, generation, _)| (owner.as_str(), *generation)),
                custody::SwitchReadiness::Wait,
            )
            .await?;
        }
    }
    Ok(())
}

enum Replan {
    /// Fallback never planned this channel; shared custody keeps its own promotion rules.
    Legacy,
    Ready(super::plan::Plan),
    Wait,
}

/// A fallback head without a matching plan is re-planned (or released) before any promotion.
async fn replan_head(pool: &PgPool, id: &OwnerIdentity, head: i64) -> Result<Replan, sqlx::Error> {
    let Some(proof) = super::plan::channel_proof(pool, id).await? else {
        return Ok(Replan::Legacy);
    };
    Ok(match super::plan::replan_stale(pool, id, &proof).await? {
        Some((planned, plan)) if planned == head => Replan::Ready(plan),
        _ => Replan::Wait,
    })
}

fn control_seconds() -> i32 {
    12
}
