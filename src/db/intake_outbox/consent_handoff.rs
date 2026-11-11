#![cfg(unix)]

//! A consent release may rearm only the original row carrying a before-effects custody marker.
use super::IntakeOutboxRow;
use super::consent_projection::project;
use crate::domain::intake_consent::{
    HoldInput, Marker, PREFIX, REARM_PREFIX, RearmMarker, marker, permits_custody,
};
use crate::domain::intake_identity::OwnerIdentity;
use crate::intake_consent_authority::BeforeEffects;
use sqlx::PgPool;

#[derive(sqlx::FromRow)]
struct Snapshot {
    #[sqlx(flatten)]
    row: IntakeOutboxRow,
    last_error: Option<String>,
}

fn payload(row: &IntakeOutboxRow) -> Result<String, String> {
    project(row).payload.digest()
}

/// Never creates a row, attempt or payload. A released marker stays with the original row forever.
pub(crate) async fn release(
    pool: &PgPool,
    id: i64,
    provider: &str,
    channel: u64,
    source: u64,
    fifo: u64,
    receipt: (&str, &str),
) -> Result<(), String> {
    let (consent, nonce) = receipt;
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    let identity = OwnerIdentity::new(provider, &channel.to_string());
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(identity.advisory_key())
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    let Snapshot { row, last_error } =
        sqlx::query_as("SELECT * FROM intake_outbox WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| e.to_string())?
            .ok_or("original consent outbox row is missing; preserve its consent reference")?;
    let mut held = marker(last_error.as_deref())?;
    if row.provider != provider
        || row.channel_id != channel.to_string()
        || row.user_msg_id != source.to_string()
        || row.attempt_no as u64 != fifo
        || held.row != id
        || held.payload != payload(&row)?
        || held.consent.len() != 64
        || held.consent != consent
        || nonce.is_empty()
    {
        return Err(
            "consent outbox source or payload changed; preserve the original reference".into(),
        );
    }
    if held.released {
        // The same payload cannot be rearmed a second time, even by a later consent generation.
        tx.commit().await.map_err(|e| e.to_string())?;
        return Ok(());
    }
    if held.claim != row.claim_owner
        || held.owner != row.owner_instance_id
        || held.generation != row.owner_generation
        || row.status != super::IntakeOutboxStatus::Spawned
    {
        return Err("consent outbox claim or generation changed; preserve its original row".into());
    }
    let owner_valid = match (&row.owner_instance_id, row.owner_generation) {
        (None, None) => true,
        (Some(owner), Some(generation)) => sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM intake_session_owners WHERE provider=$1 AND raw_channel_id=$2 AND status='active' AND owner_instance_id=$3 AND generation=$4)")
            .bind(identity.provider()).bind(identity.raw_channel_id()).bind(owner).bind(generation)
            .fetch_one(&mut *tx).await.map_err(|e| e.to_string())?,
        _ => false,
    };
    if !owner_valid {
        return Err("consent outbox owner retired; preserve its original row".into());
    }
    held.released = true;
    held.nonce = nonce.into();
    let encoded = format!(
        "{PREFIX}{}",
        serde_json::to_string(&held).map_err(|e| e.to_string())?
    );
    let changed = sqlx::query("UPDATE intake_outbox SET status='pending', claim_owner=NULL, claimed_at=NULL, accepted_at=NULL, spawned_at=NULL, last_error=$2 WHERE id=$1 AND status='spawned' AND last_error IS NOT DISTINCT FROM $3")
        .bind(id).bind(encoded).bind(last_error).execute(&mut *tx).await.map_err(|e| e.to_string())?.rows_affected();
    if changed != 1 {
        return Err("consent handoff changed concurrently; preserve its original row".into());
    }
    tx.commit().await.map_err(|e| e.to_string())
}

/// Rearms the exact pre-effects row once; its marker survives later claims without a TTL reset.
pub(crate) async fn return_before_record(
    pool: &PgPool,
    proof: BeforeEffects,
) -> Result<bool, String> {
    let expected = proof.row();
    if expected.status != super::IntakeOutboxStatus::Spawned || expected.claim_owner.is_none() {
        return Err("consent rollback requires this worker's before-effects spawned row".into());
    }
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    let identity = OwnerIdentity::new(&expected.provider, &expected.channel_id);
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(identity.advisory_key())
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    let current: Option<Snapshot> =
        sqlx::query_as("SELECT * FROM intake_outbox WHERE id=$1 FOR UPDATE")
            .bind(expected.id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    let Some(Snapshot { row, last_error }) = current else {
        return Ok(false);
    };
    if row.status != super::IntakeOutboxStatus::Spawned
        || last_error.is_some()
        || row.claim_owner != expected.claim_owner
        || row.owner_instance_id != expected.owner_instance_id
        || row.owner_generation != expected.owner_generation
        || payload(&row)? != payload(expected)?
    {
        return Ok(false);
    }
    let owner_valid = match (&row.owner_instance_id, row.owner_generation) {
        (None, None) => true,
        (Some(owner), Some(generation)) => sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM intake_session_owners WHERE provider=$1 AND raw_channel_id=$2 AND status='active' AND owner_instance_id=$3 AND generation=$4)")
            .bind(identity.provider()).bind(identity.raw_channel_id()).bind(owner).bind(generation)
            .fetch_one(&mut *tx).await.map_err(|e| e.to_string())?,
        _ => false,
    };
    if !owner_valid {
        return Ok(false);
    }
    let marker = RearmMarker {
        row: row.id,
        payload: payload(&row)?,
        owner: row.owner_instance_id.clone(),
        generation: row.owner_generation,
        count: 1,
        rearmed_at: chrono::Utc::now().timestamp_millis(),
    };
    let encoded = format!(
        "{REARM_PREFIX}{}",
        serde_json::to_string(&marker).map_err(|e| e.to_string())?
    );
    let changed = sqlx::query("UPDATE intake_outbox SET status='pending', claim_owner=NULL, claimed_at=NULL, accepted_at=NULL, spawned_at=NULL, last_error=$3 WHERE id=$1 AND status='spawned' AND claim_owner=$2 AND last_error IS NOT DISTINCT FROM $4")
        .bind(row.id).bind(&row.claim_owner).bind(encoded).bind(last_error).execute(&mut *tx).await.map_err(|e| e.to_string())?.rows_affected();
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(changed == 1)
}

pub(crate) async fn hold_before_effects(
    pool: &PgPool,
    proof: BeforeEffects,
    input: HoldInput,
) -> Result<(), String> {
    let row = proof.row();
    if !input.matches(&project(row))? {
        return Err(
            "before-effects consent custody does not identify this exact worker payload".into(),
        );
    }
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    let identity = OwnerIdentity::new(&row.provider, &row.channel_id);
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(identity.advisory_key())
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    let Snapshot {
        row: current_row,
        last_error,
    } = sqlx::query_as("SELECT * FROM intake_outbox WHERE id=$1 FOR UPDATE")
        .bind(row.id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    if payload(&current_row)? != payload(row)? {
        return Err("worker payload changed before custody handoff".into());
    }
    if !permits_custody(last_error.as_deref(), &project(&current_row))? {
        return Err("worker error marker changed before consent custody handoff".into());
    }
    let marker = Marker {
        nonce: String::new(),
        consent: input.consent().to_owned(),
        row: row.id,
        payload: payload(row)?,
        claim: row.claim_owner.clone(),
        owner: row.owner_instance_id.clone(),
        generation: row.owner_generation,
        released: false,
    };
    let encoded = format!(
        "{PREFIX}{}",
        serde_json::to_string(&marker).map_err(|e| e.to_string())?
    );
    let changed = sqlx::query("UPDATE intake_outbox SET last_error=$2 WHERE id=$1 AND status='spawned' AND claim_owner=$3 AND owner_instance_id IS NOT DISTINCT FROM $4 AND owner_generation IS NOT DISTINCT FROM $5 AND last_error IS NOT DISTINCT FROM $6")
        .bind(row.id).bind(encoded).bind(&row.claim_owner).bind(&row.owner_instance_id).bind(row.owner_generation).bind(last_error)
        .execute(&mut *tx).await.map_err(|e| e.to_string())?.rows_affected();
    if changed == 1 {
        tx.commit().await.map_err(|e| e.to_string())
    } else {
        Err("worker claim changed before consent custody handoff".into())
    }
}

#[cfg(test)]
#[path = "consent_handoff_tests.rs"]
pub(crate) mod tests;
