//! PostgreSQL authority for channel-scoped circuit alert activation (#4615 S3a).
//!
//! This module is deliberately isolated from relay circuit integration. S3a is
//! dormant: live producer wiring is forbidden until S3b adds the worker delivery
//! fence. These transaction-linearized primitives only establish the schema and
//! lifecycle contract that the later slice will consume.

use sqlx::{PgPool, Postgres, Transaction};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CircuitCoordinate<'a> {
    pub provider: &'a str,
    pub channel_id: &'a str,
    pub owner_instance_id: &'a str,
    pub owner_generation: i64,
    pub episode_key: &'a str,
    pub baseline_relay_offset: i64,
    pub open_generation: i64,
    /// Channel-global monotonic CAS coordinate. Unlike `open_generation`, this
    /// never resets when `episode_key` changes.
    pub authority_epoch: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CircuitActivation {
    Activated,
    Stale,
    NotOwner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FreshVouchRevoke {
    Revoked,
    Stale,
    NotOwner,
}

fn normalized_provider(provider: &str) -> String {
    provider.trim().to_lowercase()
}

fn normalized_channel(channel_id: &str) -> String {
    channel_id.trim().to_string()
}

async fn lock_channel(
    tx: &mut Transaction<'_, Postgres>,
    provider: &str,
    channel_id: &str,
) -> Result<(), sqlx::Error> {
    let identity = crate::services::cluster::intake_router_hook::owner_record::OwnerIdentity::new(
        provider,
        channel_id,
    );
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(identity.advisory_key())
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Stage a worker-invisible circuit row with the complete fenced coordinate.
/// This is dormant scaffolding for S3b; callers must not wire it into live relay
/// production before the worker-side final delivery fence exists.
#[allow(dead_code)]
pub(crate) async fn stage_held(
    pool: &PgPool,
    message: crate::services::message_outbox::OutboxMessage<'_>,
    coordinate: &CircuitCoordinate<'_>,
    dedupe_ttl_secs: i64,
) -> Result<i64, crate::services::message_outbox::OutboxEnqueueError> {
    crate::services::message_outbox::validate_outbox_source(message.source)?;
    if coordinate.owner_generation < 0
        || coordinate.baseline_relay_offset < 0
        || coordinate.open_generation < 0
        || coordinate.authority_epoch <= 0
    {
        return Err(crate::services::message_outbox::OutboxEnqueueError::Database(
            sqlx::Error::Protocol("invalid circuit coordinate".to_string()),
        ));
    }
    let provider = normalized_provider(coordinate.provider);
    let channel_id = normalized_channel(coordinate.channel_id);
    let reason_code = crate::services::message_outbox::normalized_reason_code(message.reason_code);
    let session_key = crate::services::message_outbox::normalized_session_key(
        message.target,
        message.session_key,
    );
    let dedupe_key = crate::services::message_outbox::dedupe_key_for_message(
        message.target,
        message.content,
        reason_code,
        session_key.as_deref(),
    )
    .ok_or_else(|| {
        crate::services::message_outbox::OutboxEnqueueError::Database(sqlx::Error::Protocol(
            "circuit staging requires a dedupe identity".to_string(),
        ))
    })?;
    let mut tx = pool.begin().await?;
    let id = sqlx::query_scalar(
        "INSERT INTO message_outbox
             (target,content,bot,source,status,reason_code,session_key,dedupe_key,
              dedupe_expires_at,circuit_provider,circuit_channel_id,circuit_episode_key,
              circuit_baseline_relay_offset,circuit_open_generation,circuit_authority_epoch,
              circuit_owner_instance_id,circuit_owner_generation)
         VALUES ($1,$2,$3,$4,'held',$5,$6,$7,
                 NOW()+($8::BIGINT*INTERVAL '1 second'),$9,$10,$11,$12,$13,$14,$15,$16)
         ON CONFLICT (dedupe_key)
             WHERE dedupe_key IS NOT NULL AND status NOT IN ('failed','cancelled')
         DO UPDATE SET dedupe_expires_at=message_outbox.dedupe_expires_at
         RETURNING id",
    )
    .bind(message.target)
    .bind(message.content)
    .bind(message.bot)
    .bind(message.source)
    .bind(reason_code)
    .bind(session_key.as_deref())
    .bind(&dedupe_key)
    .bind(dedupe_ttl_secs.max(1))
    .bind(&provider)
    .bind(&channel_id)
    .bind(coordinate.episode_key)
    .bind(coordinate.baseline_relay_offset)
    .bind(coordinate.open_generation)
    .bind(coordinate.authority_epoch)
    .bind(coordinate.owner_instance_id)
    .bind(coordinate.owner_generation)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(id)
}

async fn owner_is_current(
    tx: &mut Transaction<'_, Postgres>,
    provider: &str,
    channel_id: &str,
    owner_instance_id: &str,
    owner_generation: i64,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS(
             SELECT 1 FROM intake_session_owners
              WHERE provider=$1 AND raw_channel_id=$2 AND status='active'
                AND owner_instance_id=$3 AND generation=$4)",
    )
    .bind(provider)
    .bind(channel_id)
    .bind(owner_instance_id)
    .bind(owner_generation)
    .fetch_one(&mut **tx)
    .await
}

/// Activate a held circuit alert only while its stamped coordinate is still the
/// channel authority and the intake owner epoch is still current.
#[allow(dead_code)]
pub(crate) async fn activate_fenced(
    pool: &PgPool,
    outbox_id: i64,
    coordinate: &CircuitCoordinate<'_>,
) -> Result<CircuitActivation, sqlx::Error> {
    let provider = normalized_provider(coordinate.provider);
    let channel_id = normalized_channel(coordinate.channel_id);
    let mut tx = pool.begin().await?;
    lock_channel(&mut tx, &provider, &channel_id).await?;
    if !owner_is_current(
        &mut tx,
        &provider,
        &channel_id,
        coordinate.owner_instance_id,
        coordinate.owner_generation,
    )
    .await?
    {
        tx.rollback().await?;
        return Ok(CircuitActivation::NotOwner);
    }

    let authority_changed = sqlx::query(
        "INSERT INTO message_outbox_circuit_authority
             (provider,channel_id,owner_instance_id,owner_generation,episode_key,
              baseline_relay_offset,open_generation,authority_epoch)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)
         ON CONFLICT (provider,channel_id) DO UPDATE SET
             owner_instance_id=EXCLUDED.owner_instance_id,
             owner_generation=EXCLUDED.owner_generation,
             episode_key=EXCLUDED.episode_key,
             baseline_relay_offset=EXCLUDED.baseline_relay_offset,
             open_generation=EXCLUDED.open_generation,
             authority_epoch=EXCLUDED.authority_epoch,
             revoked_at=NULL,
             updated_at=NOW()
         WHERE message_outbox_circuit_authority.authority_epoch < EXCLUDED.authority_epoch
            OR (message_outbox_circuit_authority.authority_epoch = EXCLUDED.authority_epoch
                AND message_outbox_circuit_authority.revoked_at IS NULL
                AND message_outbox_circuit_authority.episode_key = EXCLUDED.episode_key
                AND message_outbox_circuit_authority.baseline_relay_offset = EXCLUDED.baseline_relay_offset
                AND message_outbox_circuit_authority.open_generation = EXCLUDED.open_generation
                AND message_outbox_circuit_authority.owner_instance_id = EXCLUDED.owner_instance_id
                AND message_outbox_circuit_authority.owner_generation = EXCLUDED.owner_generation)",
    )
    .bind(&provider)
    .bind(&channel_id)
    .bind(coordinate.owner_instance_id)
    .bind(coordinate.owner_generation)
    .bind(coordinate.episode_key)
    .bind(coordinate.baseline_relay_offset)
    .bind(coordinate.open_generation)
    .bind(coordinate.authority_epoch)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if authority_changed != 1 {
        tx.rollback().await?;
        return Ok(CircuitActivation::Stale);
    }

    let activated = sqlx::query(
        "UPDATE message_outbox SET status='pending'
          WHERE id=$1 AND status='held'
            AND circuit_provider=$2 AND circuit_channel_id=$3
            AND circuit_episode_key=$4 AND circuit_baseline_relay_offset=$5
            AND circuit_open_generation=$6 AND circuit_authority_epoch=$7
            AND circuit_owner_instance_id=$8 AND circuit_owner_generation=$9",
    )
    .bind(outbox_id)
    .bind(&provider)
    .bind(&channel_id)
    .bind(coordinate.episode_key)
    .bind(coordinate.baseline_relay_offset)
    .bind(coordinate.open_generation)
    .bind(coordinate.authority_epoch)
    .bind(coordinate.owner_instance_id)
    .bind(coordinate.owner_generation)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if activated != 1 {
        tx.rollback().await?;
        return Ok(CircuitActivation::Stale);
    }
    tx.commit().await?;
    Ok(CircuitActivation::Activated)
}

/// Revoke the current generation after a fresh liveness vouch. The authority
/// CAS and outbox cancellation share one transaction, so vouch-before-activation
/// and activation-before-vouch both leave no deliverable row.
#[allow(dead_code)]
pub(crate) async fn revoke_on_fresh_vouch(
    pool: &PgPool,
    coordinate: &CircuitCoordinate<'_>,
    reason: &str,
) -> Result<FreshVouchRevoke, sqlx::Error> {
    let provider = normalized_provider(coordinate.provider);
    let channel_id = normalized_channel(coordinate.channel_id);
    let mut tx = pool.begin().await?;
    lock_channel(&mut tx, &provider, &channel_id).await?;
    if !owner_is_current(
        &mut tx,
        &provider,
        &channel_id,
        coordinate.owner_instance_id,
        coordinate.owner_generation,
    )
    .await?
    {
        tx.rollback().await?;
        return Ok(FreshVouchRevoke::NotOwner);
    }

    let revoked = sqlx::query(
        "INSERT INTO message_outbox_circuit_authority
             (provider,channel_id,owner_instance_id,owner_generation,episode_key,
              baseline_relay_offset,open_generation,authority_epoch,revoked_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,NOW())
         ON CONFLICT (provider,channel_id) DO UPDATE SET revoked_at=NOW(),updated_at=NOW()
         WHERE message_outbox_circuit_authority.owner_instance_id=$3
           AND message_outbox_circuit_authority.owner_generation=$4
           AND message_outbox_circuit_authority.episode_key=$5
           AND message_outbox_circuit_authority.baseline_relay_offset=$6
           AND message_outbox_circuit_authority.open_generation=$7
           AND message_outbox_circuit_authority.authority_epoch=$8
           AND message_outbox_circuit_authority.revoked_at IS NULL",
    )
    .bind(&provider)
    .bind(&channel_id)
    .bind(coordinate.owner_instance_id)
    .bind(coordinate.owner_generation)
    .bind(coordinate.episode_key)
    .bind(coordinate.baseline_relay_offset)
    .bind(coordinate.open_generation)
    .bind(coordinate.authority_epoch)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    let cancelled = sqlx::query(
        "UPDATE message_outbox
            SET status='cancelled', cancelled_at=NOW(), cancel_reason=$9,
                dedupe_key=NULL, dedupe_expires_at=NULL,
                claimed_at=NULL, claim_owner=NULL, next_attempt_at=NULL
          WHERE status IN ('held','pending')
            AND circuit_provider=$1 AND circuit_channel_id=$2
            AND circuit_owner_instance_id=$3 AND circuit_owner_generation=$4
            AND circuit_episode_key=$5 AND circuit_baseline_relay_offset=$6
            AND circuit_open_generation=$7 AND circuit_authority_epoch=$8",
    )
    .bind(&provider)
    .bind(&channel_id)
    .bind(coordinate.owner_instance_id)
    .bind(coordinate.owner_generation)
    .bind(coordinate.episode_key)
    .bind(coordinate.baseline_relay_offset)
    .bind(coordinate.open_generation)
    .bind(coordinate.authority_epoch)
    .bind(reason)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    if revoked == 0 && cancelled == 0 {
        tx.rollback().await?;
        return Ok(FreshVouchRevoke::Stale);
    }
    tx.commit().await?;
    Ok(FreshVouchRevoke::Revoked)
}
