//! Replay receipt transitions; every acknowledgement is returned only after its commit succeeded.

use super::receipt::{self, ReceiptSnapshot};
use crate::services::discord::replay_policy::live::{NoEffectEvidence, TerminalEvidence};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

/// Why a transition produced no acknowledgement.
#[derive(Debug)]
pub(crate) enum WriteError {
    /// The receipt is not in the state this exact attempt expects; nothing changed.
    CasMiss,
    /// A replay fence refused the change; nothing changed.
    Fenced(String),
    /// The write failed before commit; nothing changed.
    Storage(sqlx::Error),
    /// Commit was sent but not acknowledged; the change may be durable.
    AckUnknown(sqlx::Error),
    /// The receipt stored under this request key names a different request.
    IdentityConflict(i64),
}

fn refused(error: sqlx::Error) -> WriteError {
    match error.as_database_error().and_then(|db| db.constraint()) {
        Some("replay_disposition_fence") => WriteError::Fenced(error.to_string()),
        _ => WriteError::Storage(error),
    }
}

/// The original request: stable key, scope, every absorbed source, original text and side inputs.
pub(crate) struct CanonicalInput {
    pub(crate) request_key: String,
    pub(crate) provider: String,
    pub(crate) channel: String,
    pub(crate) sources: Vec<String>,
    pub(crate) original_text: String,
    pub(crate) owner_id: String,
    pub(crate) agent_id: String,
    pub(crate) attachments: serde_json::Value,
    pub(crate) reply_context: Option<String>,
    /// `new_input` for a fresh request, `reentry` for a stored operation coming back.
    pub(crate) provenance: &'static str,
}

impl CanonicalInput {
    /// Hash of the original text, independent of any history added to the executed prompt.
    pub(crate) fn original_hash(&self) -> String {
        hex::encode(Sha256::digest(self.original_text.as_bytes()))
    }
}

/// Registers the request once under its key, or returns the receipt already stored under it.
pub(crate) async fn register_or_reuse(
    pool: &PgPool,
    input: &CanonicalInput,
    instance: &str,
) -> Result<ReceiptSnapshot, WriteError> {
    let hash = input.original_hash();
    let stored = match stored_under_key(pool, input).await? {
        Some(stored) => stored,
        None => {
            insert_registered(pool, input, instance, &hash).await?;
            stored_under_key(pool, input)
                .await?
                .ok_or(WriteError::CasMiss)?
        }
    };
    if stored.sources != input.sources || stored.request_hash.as_deref() != Some(hash.as_str()) {
        return Err(WriteError::IdentityConflict(stored.id));
    }
    Ok(stored)
}

async fn stored_under_key(
    pool: &PgPool,
    input: &CanonicalInput,
) -> Result<Option<ReceiptSnapshot>, WriteError> {
    let key = Some(input.request_key.as_str());
    let receipts =
        receipt::receipts_for_request(pool, &input.provider, &input.channel, &input.sources, key)
            .await
            .map_err(WriteError::Storage)?;
    Ok(receipts
        .into_iter()
        .find(|receipt| receipt.request_key.as_deref() == key))
}

async fn insert_registered(
    pool: &PgPool,
    input: &CanonicalInput,
    instance: &str,
    hash: &str,
) -> Result<(), WriteError> {
    sqlx::query(
        "INSERT INTO intake_outbox (
            target_instance_id, forwarded_by_instance_id, channel_id, user_msg_id,
            request_owner_id, user_text, reply_context, attachment_refs, turn_kind, agent_id,
            provider, status, replay_only, replay_disposition, replay_source_message_ids,
            replay_episode_nonce, replay_request_hash, replay_request_key, replay_preserved)
         VALUES ($1, $1, $2, ($3::TEXT[])[cardinality($3::TEXT[])], $4, $5, $6, $7, 'foreground',
                 $8, $9, 'unknown', TRUE, 'registered_not_started', $3, $10, $11, $12,
                 jsonb_build_object('provenance', $13::TEXT))
         ON CONFLICT (replay_request_key) WHERE replay_request_key IS NOT NULL DO NOTHING",
    )
    .bind(instance)
    .bind(&input.channel)
    .bind(&input.sources)
    .bind(&input.owner_id)
    .bind(&input.original_text)
    .bind(&input.reply_context)
    .bind(&input.attachments)
    .bind(&input.agent_id)
    .bind(&input.provider)
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(hash)
    .bind(&input.request_key)
    .bind(input.provenance)
    .execute(pool)
    .await
    .map_err(refused)?;
    Ok(())
}

/// One started attempt: its receipt, episode nonce, owning incarnation and the nonce it retried.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ExactAttempt {
    receipt_id: i64,
    nonce: String,
    incarnation: String,
    retry_of: Option<String>,
}

impl ExactAttempt {
    pub(crate) fn receipt_id(&self) -> i64 {
        self.receipt_id
    }

    pub(crate) fn nonce(&self) -> &str {
        &self.nonce
    }

    pub(crate) fn retry_of(&self) -> Option<&str> {
        self.retry_of.as_deref()
    }
}

/// Which attempt a begin starts: the registered nonce, or the next nonce after a no-effect one.
pub(crate) enum BeginFrom<'a> {
    Registered {
        nonce: &'a str,
    },
    NoEffect {
        previous: &'a str,
        next: &'a str,
        session_key: &'a str,
    },
}

/// The effect binding stored with the start; the permit can only be spent on it.
pub(crate) struct EffectProjection<'a> {
    pub(crate) effect_target: &'a str,
    pub(crate) input_hash: &'a str,
    pub(crate) binding: &'a serde_json::Value,
}

/// A committed start; the permit layer seals it into the one `StartPermit` for this attempt.
#[derive(Debug)]
pub(crate) struct BeginAck {
    attempt: ExactAttempt,
    effect_target: String,
    input_hash: String,
}

impl BeginAck {
    pub(crate) fn into_parts(self) -> (ExactAttempt, String, String) {
        (self.attempt, self.effect_target, self.input_hash)
    }
}

async fn commit(tx: sqlx::Transaction<'_, sqlx::Postgres>) -> Result<(), WriteError> {
    tx.commit().await.map_err(WriteError::AckUnknown)
}

/// Starts one exact attempt with its projection; a retry also moves its session off the stale
/// resume in the same commit. A start exists only when this returns `Ok`.
pub(crate) async fn begin(
    pool: &PgPool,
    receipt_id: i64,
    from: BeginFrom<'_>,
    effect: &EffectProjection<'_>,
    incarnation: &str,
) -> Result<BeginAck, WriteError> {
    let (previous, nonce, session_key) = match from {
        BeginFrom::Registered { nonce } => (None, nonce, None),
        BeginFrom::NoEffect {
            previous,
            next,
            session_key,
        } => (Some(previous), next, Some(session_key)),
    };
    let mut tx = pool.begin().await.map_err(WriteError::Storage)?;
    let started = sqlx::query(
        "UPDATE intake_outbox
            SET replay_disposition = 'started_unclassified', replay_episode_nonce = $2,
                replay_retry_of_nonce = COALESCE($3, replay_retry_of_nonce),
                replay_owner_incarnation = $4,
                replay_preserved = COALESCE(replay_preserved, '{}'::jsonb) || jsonb_build_object(
                    'projection', jsonb_build_object('nonce', $2::TEXT, 'effect_target', $5::TEXT,
                                                     'input_hash', $6::TEXT, 'binding', $7::JSONB))
          WHERE id = $1 AND replay_episode_nonce = COALESCE($3, $2)
            AND replay_disposition = CASE WHEN $3::TEXT IS NULL THEN 'registered_not_started'
                                          ELSE 'startup_failed_no_effect' END",
    )
    .bind(receipt_id)
    .bind(nonce)
    .bind(previous)
    .bind(incarnation)
    .bind(effect.effect_target)
    .bind(effect.input_hash)
    .bind(effect.binding)
    .execute(&mut *tx)
    .await
    .map_err(refused)?;
    if started.rows_affected() != 1 {
        return Err(WriteError::CasMiss);
    }
    if let Some(session_key) = session_key {
        let moved = sqlx::query(
            "UPDATE sessions SET claude_session_id = NULL, raw_provider_session_id = NULL,
                                 replay_episode_nonce = $3
              WHERE session_key = $1 AND current_replay_receipt_id = $2
                AND replay_episode_nonce = $4",
        )
        .bind(session_key)
        .bind(receipt_id)
        .bind(nonce)
        .bind(previous)
        .execute(&mut *tx)
        .await
        .map_err(refused)?;
        if moved.rows_affected() != 1 {
            return Err(WriteError::CasMiss);
        }
    }
    commit(tx).await?;
    Ok(BeginAck {
        attempt: ExactAttempt {
            receipt_id,
            nonce: nonce.to_string(),
            incarnation: incarnation.to_string(),
            retry_of: previous.map(str::to_string),
        },
        effect_target: effect.effect_target.to_string(),
        input_hash: effect.input_hash.to_string(),
    })
}

async fn settle(
    pool: &PgPool,
    attempt: &ExactAttempt,
    disposition: &str,
    hold: Option<(&str, &serde_json::Value)>,
) -> Result<(), WriteError> {
    let mut tx = pool.begin().await.map_err(WriteError::Storage)?;
    let settled = sqlx::query(
        "UPDATE intake_outbox
            SET replay_disposition = $4, replay_hold_reason = COALESCE($5, replay_hold_reason),
                replay_preserved = COALESCE(replay_preserved, '{}'::jsonb) || COALESCE($6, '{}'::jsonb)
          WHERE id = $1 AND replay_episode_nonce = $2 AND replay_owner_incarnation = $3
            AND (replay_disposition = 'started_unclassified'
                 OR (replay_disposition = 'startup_failed_no_effect' AND $4 = 'withheld'))",
    )
    .bind(attempt.receipt_id)
    .bind(&attempt.nonce)
    .bind(&attempt.incarnation)
    .bind(disposition)
    .bind(hold.map(|(reason, _)| reason))
    .bind(hold.map(|(_, preserved)| preserved))
    .execute(&mut *tx)
    .await
    .map_err(refused)?;
    if settled.rows_affected() != 1 {
        return Err(WriteError::CasMiss);
    }
    commit(tx).await
}

/// Classifies the attempt as an ordinary terminal; only then may ordinary completion or retry run.
pub(crate) async fn classify_normal(
    pool: &PgPool,
    attempt: &ExactAttempt,
    terminal: &TerminalEvidence,
) -> Result<(), WriteError> {
    if !terminal.names(attempt) {
        return Err(WriteError::CasMiss);
    }
    settle(pool, attempt, "classified_normal", None).await
}

/// The committed no-effect classification of one nonce; the permit layer turns it into a `RetryPermit`.
#[derive(Debug)]
pub(crate) struct NoEffectAck {
    receipt_id: i64,
    previous_nonce: String,
}

impl NoEffectAck {
    pub(crate) fn into_parts(self) -> (i64, String) {
        (self.receipt_id, self.previous_nonce)
    }
}

pub(crate) async fn classify_no_effect(
    pool: &PgPool,
    attempt: ExactAttempt,
    evidence: &NoEffectEvidence,
) -> Result<NoEffectAck, WriteError> {
    if !evidence.names(&attempt) {
        return Err(WriteError::CasMiss);
    }
    settle(pool, &attempt, "startup_failed_no_effect", None).await?;
    Ok(NoEffectAck {
        receipt_id: attempt.receipt_id,
        previous_nonce: attempt.nonce,
    })
}

/// Holds the attempt with its reason and preserved result; the receipt keeps blocking either way.
pub(crate) async fn withhold(
    pool: &PgPool,
    attempt: &ExactAttempt,
    reason: &str,
    preserved: &serde_json::Value,
) -> Result<(), WriteError> {
    settle(pool, attempt, "withheld", Some((reason, preserved))).await
}
