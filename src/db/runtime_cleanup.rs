#![allow(dead_code)] // Dormant Task #25 substrate; no production consumer exists.

//! PostgreSQL-only runtime cleanup substrate.
//!
//! The database can serialize identities, epochs, capabilities, and receipts. It
//! cannot prove that an external destination fenced a side effect already sent by
//! a stale actor. Destination high-watermark enforcement is a future slice.
//!
//! Lock order is global and mandatory: request UUID advisory lock, canonical
//! identity advisory lock, sorted locator locks, target row, operation row,
//! intent row, then capability/request/receipt row.

mod model;

#[cfg(test)]
mod postgres_tests;

use chrono::{DateTime, Duration, Utc};
pub(crate) use model::*;
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Row, Transaction};
use uuid::Uuid;

const LOCK_NAMESPACE: i32 = 4926;
const MAX_CLAIM_DURATION_SECONDS: i64 = 300;
const MAX_CAPABILITY_DURATION_SECONDS: i64 = 300;
const PLAN: [(&str, i16); 6] = [
    ("block_runtime_admission", 1),
    ("clear_queued_input", 2),
    ("expire_runtime_lease", 3),
    ("cancel_active_runtime", 4),
    ("clear_persisted_session", 5),
    ("release_runtime_slot", 6),
];

pub(crate) async fn converge_target(
    pool: &PgPool,
    identity: CanonicalCleanupIdentity<'_>,
    preferred_target_id: Uuid,
) -> Result<CleanupTarget, sqlx::Error> {
    let mut tx = pool.begin().await?;
    lock_identity(&mut tx, identity).await?;
    sqlx::query(
        "INSERT INTO runtime_cleanup_targets
         (target_id, identity_kind, provider, discord_token_hash, channel_id)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (identity_kind, provider, discord_token_hash, channel_id) DO NOTHING",
    )
    .bind(preferred_target_id)
    .bind(identity.kind.as_str())
    .bind(identity.provider)
    .bind(identity.discord_token_hash)
    .bind(identity.channel_id)
    .execute(&mut *tx)
    .await?;
    let row = sqlx::query(
        "SELECT target_id, operation_high_watermark, retired_at IS NOT NULL AS retired
         FROM runtime_cleanup_targets
         WHERE identity_kind = $1 AND provider = $2
           AND discord_token_hash = $3 AND channel_id = $4
         FOR UPDATE",
    )
    .bind(identity.kind.as_str())
    .bind(identity.provider)
    .bind(identity.discord_token_hash)
    .bind(identity.channel_id)
    .fetch_one(&mut *tx)
    .await?;
    let target = CleanupTarget {
        target_id: row.try_get("target_id")?,
        operation_high_watermark: row.try_get("operation_high_watermark")?,
        retired: row.try_get("retired")?,
    };
    tx.commit().await?;
    Ok(target)
}

pub(crate) async fn retire_target(pool: &PgPool, target_id: Uuid) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    lock_target(&mut tx, target_id).await?;
    let changed = sqlx::query(
        "UPDATE runtime_cleanup_targets SET retired_at = clock_timestamp()
         WHERE target_id = $1 AND retired_at IS NULL",
    )
    .bind(target_id)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    tx.commit().await?;
    Ok(changed)
}

pub(crate) async fn bind_session(
    pool: &PgPool,
    target_id: Uuid,
    session_id: i64,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    lock_target(&mut tx, target_id).await?;
    ensure_active_target(&mut tx, target_id).await?;
    sqlx::query(
        "INSERT INTO runtime_cleanup_target_session_bindings (session_id, target_id)
         VALUES ($1, $2)
         ON CONFLICT (session_id) DO UPDATE SET target_id = EXCLUDED.target_id,
             bound_at = clock_timestamp()",
    )
    .bind(session_id)
    .bind(target_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn reserve_locator(
    pool: &PgPool,
    locator: &str,
    target_id: Uuid,
) -> Result<LocatorClaim, sqlx::Error> {
    let mut tx = pool.begin().await?;
    lock_locators(&mut tx, &[locator]).await?;
    lock_target(&mut tx, target_id).await?;
    ensure_active_target(&mut tx, target_id).await?;
    if let Some(row) = sqlx::query(
        "SELECT target_id, generation FROM runtime_cleanup_locator_claims
         WHERE locator = $1 AND active FOR UPDATE",
    )
    .bind(locator)
    .fetch_optional(&mut *tx)
    .await?
    {
        let claim = LocatorClaim {
            target_id: row.try_get("target_id")?,
            generation: row.try_get("generation")?,
        };
        tx.commit().await?;
        return Ok(claim);
    }
    let generation: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(generation), 0) + 1
         FROM runtime_cleanup_locator_claims WHERE locator = $1",
    )
    .bind(locator)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO runtime_cleanup_locator_claims
         (locator, generation, target_id) VALUES ($1, $2, $3)",
    )
    .bind(locator)
    .bind(generation)
    .bind(target_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(LocatorClaim {
        target_id,
        generation,
    })
}

pub(crate) async fn resolve_locator(
    pool: &PgPool,
    locator: &str,
) -> Result<Option<LocatorClaim>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    lock_locators(&mut tx, &[locator]).await?;
    let row = sqlx::query(
        "SELECT target_id, generation FROM runtime_cleanup_locator_claims
         WHERE locator = $1 AND active FOR UPDATE",
    )
    .bind(locator)
    .fetch_optional(&mut *tx)
    .await?;
    let claim = row
        .map(|row| -> Result<LocatorClaim, sqlx::Error> {
            Ok(LocatorClaim {
                target_id: row.try_get("target_id")?,
                generation: row.try_get("generation")?,
            })
        })
        .transpose()?;
    tx.commit().await?;
    Ok(claim)
}

pub(crate) async fn retire_locator(
    pool: &PgPool,
    locator: &str,
    expected_generation: i64,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    lock_locators(&mut tx, &[locator]).await?;
    let changed = sqlx::query(
        "UPDATE runtime_cleanup_locator_claims
         SET active = FALSE, retired_at = clock_timestamp()
         WHERE locator = $1 AND generation = $2 AND active",
    )
    .bind(locator)
    .bind(expected_generation)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    tx.commit().await?;
    Ok(changed)
}

pub(crate) async fn create_operation(
    pool: &PgPool,
    target_id: Uuid,
    operation_id: Uuid,
) -> Result<CreatedOperation, sqlx::Error> {
    let mut tx = pool.begin().await?;
    lock_target(&mut tx, target_id).await?;
    let operation_epoch: i64 = sqlx::query_scalar(
        "UPDATE runtime_cleanup_targets
         SET operation_high_watermark = operation_high_watermark + 1
         WHERE target_id = $1 AND retired_at IS NULL
         RETURNING operation_high_watermark",
    )
    .bind(target_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO runtime_cleanup_operations
         (operation_id, target_id, operation_epoch, operation_kind)
         VALUES ($1, $2, $3, 'clear_for_resume')",
    )
    .bind(operation_id)
    .bind(target_id)
    .bind(operation_epoch)
    .execute(&mut *tx)
    .await?;
    for (intent_kind, ordinal) in PLAN {
        sqlx::query(
            "INSERT INTO runtime_cleanup_intents
             (operation_id, intent_id, ordinal, intent_kind, target_id, idempotency_identity)
             VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(operation_id)
        .bind(Uuid::new_v4())
        .bind(ordinal)
        .bind(intent_kind)
        .bind(target_id)
        .bind(Uuid::new_v4())
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(CreatedOperation {
        operation_id,
        operation_epoch,
    })
}

pub(crate) async fn claim_operation(
    pool: &PgPool,
    operation_id: Uuid,
    owner: &str,
    duration: Duration,
) -> Result<ClaimResult, sqlx::Error> {
    claim_or_renew(pool, operation_id, owner, None, duration).await
}

pub(crate) async fn renew_operation(
    pool: &PgPool,
    operation_id: Uuid,
    owner: &str,
    expected_attempt_epoch: i64,
    duration: Duration,
) -> Result<ClaimResult, sqlx::Error> {
    claim_or_renew(
        pool,
        operation_id,
        owner,
        Some(expected_attempt_epoch),
        duration,
    )
    .await
}

async fn claim_or_renew(
    pool: &PgPool,
    operation_id: Uuid,
    owner: &str,
    renew_epoch: Option<i64>,
    duration: Duration,
) -> Result<ClaimResult, sqlx::Error> {
    validate_duration(duration, MAX_CLAIM_DURATION_SECONDS)?;
    let mut tx = pool.begin().await?;
    let database_now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    let Some(row) = sqlx::query(
        "SELECT target_id, state, claim_owner, attempt_epoch, claim_expires_at
         FROM runtime_cleanup_operations WHERE operation_id = $1 FOR UPDATE",
    )
    .bind(operation_id)
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.commit().await?;
        return Ok(ClaimResult::NotFound);
    };
    let state: String = row.try_get("state")?;
    if !matches!(state.as_str(), "open" | "committed") {
        tx.commit().await?;
        return Ok(ClaimResult::Stale);
    }
    let current_owner: Option<String> = row.try_get("claim_owner")?;
    let current_epoch: i64 = row.try_get("attempt_epoch")?;
    let current_expiry: Option<DateTime<Utc>> = row.try_get("claim_expires_at")?;
    if let Some(expected) = renew_epoch {
        if current_owner.as_deref() != Some(owner)
            || current_epoch != expected
            || current_expiry.is_none_or(|expires_at| expires_at <= database_now)
        {
            tx.commit().await?;
            return Ok(ClaimResult::Stale);
        }
        let expires_at = database_now + duration;
        sqlx::query(
            "UPDATE runtime_cleanup_operations SET claim_expires_at = $2,
             updated_at = clock_timestamp() WHERE operation_id = $1",
        )
        .bind(operation_id)
        .bind(expires_at)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(ClaimResult::Renewed {
            attempt_epoch: current_epoch,
            expires_at,
        });
    }
    if let (Some(held_owner), Some(expires_at)) = (current_owner, current_expiry)
        && expires_at > database_now
    {
        tx.commit().await?;
        return Ok(ClaimResult::Held {
            owner: held_owner,
            attempt_epoch: current_epoch,
            expires_at,
        });
    }
    let attempt_epoch = current_epoch + 1;
    let expires_at = database_now + duration;
    sqlx::query(
        "UPDATE runtime_cleanup_operations SET claim_owner = $2, attempt_epoch = $3,
         claim_expires_at = $4, updated_at = clock_timestamp() WHERE operation_id = $1",
    )
    .bind(operation_id)
    .bind(owner)
    .bind(attempt_epoch)
    .bind(expires_at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(ClaimResult::Claimed {
        attempt_epoch,
        expires_at,
    })
}

pub(crate) async fn transition_operation(
    pool: &PgPool,
    operation_id: Uuid,
    owner: &str,
    expected_attempt_epoch: i64,
    next: OperationState,
) -> Result<bool, sqlx::Error> {
    let timestamp_column = match next {
        OperationState::Committed => "committed_at",
        OperationState::Completed => "completed_at",
        OperationState::Aborted => "aborted_at",
        OperationState::Open => return Ok(false),
    };
    let query = format!(
        "UPDATE runtime_cleanup_operations SET state = $4, {timestamp_column} = clock_timestamp(),
         updated_at = clock_timestamp() WHERE operation_id = $1 AND claim_owner = $2
         AND attempt_epoch = $3 AND claim_expires_at > clock_timestamp()"
    );
    Ok(sqlx::query(&query)
        .bind(operation_id)
        .bind(owner)
        .bind(expected_attempt_epoch)
        .bind(next.as_str())
        .execute(pool)
        .await?
        .rows_affected()
        == 1)
}

pub(crate) async fn issue_capability(
    pool: &PgPool,
    binding: CapabilityBinding<'_>,
) -> Result<Vec<u8>, sqlx::Error> {
    let database_now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(pool)
        .await?;
    if binding.expires_at <= database_now
        || binding.expires_at > database_now + Duration::seconds(MAX_CAPABILITY_DURATION_SECONDS)
    {
        return Err(sqlx::Error::Protocol(
            "capability duration is outside database-clock bounds".into(),
        ));
    }
    let mut secret = vec![0_u8; 32];
    rand::thread_rng().fill_bytes(&mut secret);
    let hash = Sha256::digest(&secret).to_vec();
    sqlx::query(
        "INSERT INTO runtime_cleanup_capabilities
         (capability_id, capability_hash, target_id, operation_id, intent_id,
          attempt_epoch, audience, expires_at, idempotency_identity)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)",
    )
    .bind(binding.capability_id)
    .bind(hash)
    .bind(binding.target_id)
    .bind(binding.operation_id)
    .bind(binding.intent_id)
    .bind(binding.attempt_epoch)
    .bind(binding.audience)
    .bind(binding.expires_at)
    .bind(binding.idempotency_identity)
    .execute(pool)
    .await?;
    Ok(secret)
}

pub(crate) async fn begin_capability_request(
    pool: &PgPool,
    secret: &[u8],
    binding: CapabilityBinding<'_>,
    request_fingerprint: [u8; 32],
) -> Result<CapabilityUse, sqlx::Error> {
    begin_or_replay_capability_request(
        pool,
        secret,
        binding,
        Uuid::new_v4(),
        request_fingerprint,
        false,
    )
    .await
}

pub(crate) async fn replay_capability_request(
    pool: &PgPool,
    secret: &[u8],
    binding: CapabilityBinding<'_>,
    request_id: Uuid,
    request_fingerprint: [u8; 32],
) -> Result<CapabilityUse, sqlx::Error> {
    begin_or_replay_capability_request(pool, secret, binding, request_id, request_fingerprint, true)
        .await
}

async fn begin_or_replay_capability_request(
    pool: &PgPool,
    secret: &[u8],
    binding: CapabilityBinding<'_>,
    request_id: Uuid,
    request_fingerprint: [u8; 32],
    replay_only: bool,
) -> Result<CapabilityUse, sqlx::Error> {
    let mut tx = pool.begin().await?;
    lock_request(&mut tx, request_id).await?;
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    let capability_hash = Sha256::digest(secret).to_vec();
    let Some(row) = sqlx::query(
        "SELECT c.target_id, c.operation_id, c.intent_id, c.attempt_epoch,
                c.audience, c.expires_at, c.idempotency_identity,
                o.attempt_epoch AS operation_attempt_epoch
         FROM runtime_cleanup_capabilities c
         JOIN runtime_cleanup_operations o USING (operation_id)
         WHERE c.capability_hash = $1 FOR UPDATE OF c, o",
    )
    .bind(capability_hash)
    .fetch_optional(&mut *tx)
    .await?
    else {
        tx.commit().await?;
        return Ok(CapabilityUse::NotFound);
    };
    if row.try_get::<Uuid, _>("target_id")? != binding.target_id
        || row.try_get::<Uuid, _>("operation_id")? != binding.operation_id
        || row.try_get::<Uuid, _>("intent_id")? != binding.intent_id
        || row.try_get::<i64, _>("attempt_epoch")? != binding.attempt_epoch
        || row.try_get::<String, _>("audience")? != binding.audience
        || row.try_get::<Uuid, _>("idempotency_identity")? != binding.idempotency_identity
    {
        tx.commit().await?;
        return Ok(CapabilityUse::BindingMismatch);
    }
    if row.try_get::<DateTime<Utc>, _>("expires_at")? <= now {
        tx.commit().await?;
        return Ok(CapabilityUse::Expired);
    }
    if row.try_get::<i64, _>("operation_attempt_epoch")? != binding.attempt_epoch {
        tx.commit().await?;
        return Ok(CapabilityUse::StaleAttempt);
    }
    if let Some(existing) = sqlx::query(
        "SELECT request_fingerprint FROM runtime_cleanup_request_identities WHERE request_id = $1",
    )
    .bind(request_id)
    .fetch_optional(&mut *tx)
    .await?
    {
        let prior: Vec<u8> = existing.try_get("request_fingerprint")?;
        if prior.as_slice() != request_fingerprint {
            tx.commit().await?;
            return Ok(CapabilityUse::FingerprintConflict);
        }
        let receipt = sqlx::query_scalar::<_, String>(
            "SELECT receipt_state FROM runtime_cleanup_receipts WHERE request_id = $1",
        )
        .bind(request_id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(CapabilityUse::Replay {
            request_id,
            state: receipt
                .as_deref()
                .map(parse_receipt)
                .unwrap_or(ReceiptState::Unknown),
        });
    }
    if replay_only {
        tx.commit().await?;
        return Ok(CapabilityUse::NotFound);
    }
    if let Some(existing) = sqlx::query(
        "SELECT request_id, request_fingerprint
         FROM runtime_cleanup_request_identities
         WHERE operation_id = $1 AND intent_id = $2 AND idempotency_identity = $3
         FOR UPDATE",
    )
    .bind(binding.operation_id)
    .bind(binding.intent_id)
    .bind(binding.idempotency_identity)
    .fetch_optional(&mut *tx)
    .await?
    {
        let existing_request_id: Uuid = existing.try_get("request_id")?;
        let prior: Vec<u8> = existing.try_get("request_fingerprint")?;
        if prior.as_slice() != request_fingerprint {
            tx.commit().await?;
            return Ok(CapabilityUse::FingerprintConflict);
        }
        let receipt = sqlx::query_scalar::<_, String>(
            "SELECT receipt_state FROM runtime_cleanup_receipts WHERE request_id = $1",
        )
        .bind(existing_request_id)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(CapabilityUse::Replay {
            request_id: existing_request_id,
            state: receipt
                .as_deref()
                .map(parse_receipt)
                .unwrap_or(ReceiptState::Unknown),
        });
    }
    sqlx::query(
        "INSERT INTO runtime_cleanup_request_identities
         (request_id, operation_id, intent_id, idempotency_identity, request_fingerprint)
         VALUES ($1,$2,$3,$4,$5)",
    )
    .bind(request_id)
    .bind(binding.operation_id)
    .bind(binding.intent_id)
    .bind(binding.idempotency_identity)
    .bind(request_fingerprint.to_vec())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(CapabilityUse::Accepted { request_id })
}

pub(crate) async fn record_receipt(
    pool: &PgPool,
    request_id: Uuid,
    state: ReceiptState,
    result_fingerprint: Option<[u8; 32]>,
) -> Result<bool, sqlx::Error> {
    Ok(sqlx::query(
        "INSERT INTO runtime_cleanup_receipts
         (request_id, receipt_state, result_fingerprint, terminal_at, retain_until)
         VALUES ($1,$2,$3,clock_timestamp(),clock_timestamp() + INTERVAL '30 days')
         ON CONFLICT (request_id) DO NOTHING",
    )
    .bind(request_id)
    .bind(state.as_str())
    .bind(result_fingerprint.map(|value| value.to_vec()))
    .execute(pool)
    .await?
    .rows_affected()
        == 1)
}

pub(crate) async fn gc_terminal_receipts(
    pool: &PgPool,
    batch_size: i32,
) -> Result<i32, sqlx::Error> {
    sqlx::query_scalar("SELECT agentdesk_gc_runtime_cleanup_receipts($1)")
        .bind(batch_size)
        .fetch_one(pool)
        .await
}

fn parse_receipt(value: &str) -> ReceiptState {
    match value {
        "applied" => ReceiptState::Applied,
        "not_applied" => ReceiptState::NotApplied,
        _ => ReceiptState::Unknown,
    }
}

fn validate_duration(duration: Duration, maximum_seconds: i64) -> Result<(), sqlx::Error> {
    if duration <= Duration::zero() || duration > Duration::seconds(maximum_seconds) {
        return Err(sqlx::Error::Protocol(
            "duration is outside database-clock bounds".into(),
        ));
    }
    Ok(())
}

async fn lock_request(
    tx: &mut Transaction<'_, Postgres>,
    request_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
        .bind(LOCK_NAMESPACE)
        .bind(format!("request:{request_id}"))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn lock_identity(
    tx: &mut Transaction<'_, Postgres>,
    identity: CanonicalCleanupIdentity<'_>,
) -> Result<(), sqlx::Error> {
    let key = format!(
        "identity:{}:{}:{}:{}",
        identity.kind.as_str(),
        identity.provider,
        identity.discord_token_hash,
        identity.channel_id
    );
    sqlx::query("SELECT pg_advisory_xact_lock($1, hashtext($2))")
        .bind(LOCK_NAMESPACE)
        .bind(key)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn lock_locators(
    tx: &mut Transaction<'_, Postgres>,
    locators: &[&str],
) -> Result<(), sqlx::Error> {
    let mut ordered = locators.to_vec();
    ordered.sort_unstable();
    ordered.dedup();
    for locator in ordered {
        sqlx::query("SELECT agentdesk_lock_session_locator($1)")
            .bind(locator)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn lock_target(
    tx: &mut Transaction<'_, Postgres>,
    target_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT target_id FROM runtime_cleanup_targets WHERE target_id = $1 FOR UPDATE")
        .bind(target_id)
        .fetch_one(&mut **tx)
        .await?;
    Ok(())
}

async fn ensure_active_target(
    tx: &mut Transaction<'_, Postgres>,
    target_id: Uuid,
) -> Result<(), sqlx::Error> {
    let active: bool = sqlx::query_scalar(
        "SELECT retired_at IS NULL FROM runtime_cleanup_targets WHERE target_id = $1",
    )
    .bind(target_id)
    .fetch_one(&mut **tx)
    .await?;
    if !active {
        return Err(sqlx::Error::Protocol("cleanup target is retired".into()));
    }
    Ok(())
}
