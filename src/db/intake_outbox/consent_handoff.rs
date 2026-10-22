#![cfg(unix)]

//! A consent release may rearm only the original row carrying a before-effects custody marker.
use super::IntakeOutboxRow;
use crate::services::cluster::intake_router_hook::owner_record::OwnerIdentity;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

const PREFIX: &str = "herdr-consent-custody:v1:";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    // A generation nonce is a release receipt, never authority for the held row.
    nonce: String,
    consent: String,
    row: i64,
    payload: String,
    claim: Option<String>,
    owner: Option<String>,
    generation: Option<i64>,
    released: bool,
}

#[derive(sqlx::FromRow)]
struct Snapshot {
    #[sqlx(flatten)]
    row: IntakeOutboxRow,
    last_error: Option<String>,
}

fn payload(row: &IntakeOutboxRow) -> Result<String, String> {
    let fields = serde_json::json!([
        row.id,
        row.target_instance_id,
        row.forwarded_by_instance_id,
        row.required_labels,
        row.execution_requirements,
        row.attachment_refs,
        row.channel_id,
        row.user_msg_id,
        row.replay_source_message_ids,
        row.request_owner_id,
        row.request_owner_name,
        row.user_text,
        row.reply_context,
        row.has_reply_boundary,
        row.dm_hint,
        row.turn_kind,
        row.merge_consecutive,
        row.reply_to_user_message,
        row.defer_watcher_resume,
        row.wait_for_completion,
        row.preserve_on_cancel,
        row.agent_id,
        row.provider,
        row.attempt_no,
        row.parent_outbox_id,
        row.admission_kind,
        row.home_epoch
    ]);
    Ok(hex::encode(Sha256::digest(
        serde_json::to_vec(&fields).map_err(|e| e.to_string())?,
    )))
}

fn marker(error: Option<&str>) -> Result<Marker, String> {
    let encoded = error.and_then(|error| error.strip_prefix(PREFIX)).ok_or(
        "outbox has no durable before-effects consent custody marker; preserve its original row",
    )?;
    serde_json::from_str(encoded).map_err(|e| format!("invalid consent custody marker: {e}"))
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

pub(crate) async fn hold_before_effects(
    pool: &PgPool,
    proof: crate::services::cluster::intake_worker::BeforeEffects,
    store: &crate::services::session_host::policy::consent::ConsentStore,
    expected: &crate::services::session_host::policy::consent::Consent,
    intent: &crate::services::session_host::policy::consent::IntentRef,
) -> Result<(), String> {
    use crate::services::session_host::policy::durable;
    let row = proof.row();
    let current = store
        .load(&expected.key)?
        .ok_or("consent custody missing")?;
    if current != *expected
        || !current.unresolved_intents().contains(intent)
        || intent.store != "intake_outbox"
        || intent.id != row.id.to_string()
        || intent.provider != row.provider
        || intent.channel.to_string() != row.channel_id
        || intent.source_message.to_string() != row.user_msg_id
        || intent.fifo != row.attempt_no as u64
        || row.status != crate::db::intake_outbox_status::IntakeOutboxStatus::Spawned
        || row.claim_owner.is_none()
    {
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
    let current_row: IntakeOutboxRow =
        sqlx::query_as("SELECT * FROM intake_outbox WHERE id=$1 FOR UPDATE")
            .bind(row.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    if payload(&current_row)? != payload(row)? {
        return Err("worker payload changed before custody handoff".into());
    }
    let marker = Marker {
        nonce: String::new(),
        consent: durable::key(&current.key)?,
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
    let changed = sqlx::query("UPDATE intake_outbox SET last_error=$2 WHERE id=$1 AND status='spawned' AND claim_owner=$3 AND owner_instance_id IS NOT DISTINCT FROM $4 AND owner_generation IS NOT DISTINCT FROM $5 AND last_error IS NULL")
        .bind(row.id).bind(encoded).bind(&row.claim_owner).bind(&row.owner_instance_id).bind(row.owner_generation)
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
