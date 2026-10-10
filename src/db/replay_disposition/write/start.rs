//! Registration and start of a receipt; the request start path connects these.

use super::super::receipt::ReceiptSnapshot;
use super::{ExactAttempt, WriteError, commit, refused};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

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
/// Same-key registrations are serialised, so a later one reuses the first instead of being fenced.
pub(crate) async fn register_or_reuse(
    pool: &PgPool,
    input: &CanonicalInput,
    instance: &str,
) -> Result<ReceiptSnapshot, WriteError> {
    let mut tx = pool.begin().await.map_err(WriteError::Storage)?;
    sqlx::query("SELECT pg_advisory_xact_lock(6008, hashtext($1))")
        .bind(&input.request_key)
        .execute(&mut *tx)
        .await
        .map_err(WriteError::Storage)?;
    let stored = match stored_request(&mut tx, StoredBy::Key(&input.request_key)).await? {
        Some(stored) => stored,
        None => {
            insert_registered(&mut tx, input, instance).await?;
            stored_request(&mut tx, StoredBy::Key(&input.request_key))
                .await?
                .ok_or(WriteError::CasMiss)?
        }
    };
    if !stored.names(input) {
        return Err(WriteError::IdentityConflict(stored.receipt.id));
    }
    commit(tx).await?;
    Ok(stored.receipt)
}

/// Refuses an exact retry whose input is not the request stored under the receipt; writes nothing.
pub(crate) async fn ensure_same_request(
    pool: &PgPool,
    receipt_id: i64,
    input: &CanonicalInput,
) -> Result<(), WriteError> {
    let mut conn = pool.acquire().await.map_err(WriteError::Storage)?;
    match stored_request(&mut conn, StoredBy::Id(receipt_id)).await? {
        Some(stored) if stored.names(input) => Ok(()),
        Some(_) => Err(WriteError::IdentityConflict(receipt_id)),
        None => Err(WriteError::CasMiss),
    }
}

/// A stored receipt with the request identity it was registered under.
#[derive(sqlx::FromRow)]
struct StoredRequest {
    #[sqlx(flatten)]
    receipt: ReceiptSnapshot,
    provider: String,
    channel_id: String,
    request_owner_id: String,
    agent_id: String,
    attachment_refs: serde_json::Value,
    reply_context: Option<String>,
}

impl StoredRequest {
    /// Same key, scope, sources, owner, agent, side inputs and original text; provenance may differ.
    fn names(&self, input: &CanonicalInput) -> bool {
        let receipt = &self.receipt;
        receipt.request_key.as_deref() == Some(input.request_key.as_str())
            && self.provider.trim().eq_ignore_ascii_case(input.provider.trim())
            && self.channel_id == input.channel
            && receipt.sources == input.sources
            && self.request_owner_id == input.owner_id
            && self.agent_id == input.agent_id
            && self.attachment_refs == input.attachments
            && self.reply_context == input.reply_context
            && receipt.request_hash.as_deref() == Some(input.original_hash().as_str())
    }
}

enum StoredBy<'a> {
    Key(&'a str),
    Id(i64),
}

async fn stored_request(
    conn: &mut sqlx::PgConnection,
    by: StoredBy<'_>,
) -> Result<Option<StoredRequest>, WriteError> {
    let (key, id) = match by {
        StoredBy::Key(key) => (Some(key), None),
        StoredBy::Id(id) => (None, Some(id)),
    };
    sqlx::query_as(
        "SELECT id, replay_source_message_ids AS sources, replay_disposition AS disposition,
                replay_episode_nonce AS episode_nonce, replay_request_key AS request_key,
                replay_request_hash AS request_hash, user_text AS original_text,
                replay_preserved AS preserved, provider, channel_id, request_owner_id, agent_id,
                attachment_refs, reply_context
           FROM intake_outbox
          WHERE replay_disposition IS NOT NULL
            AND (replay_request_key = $1 OR id = $2)",
    )
    .bind(key)
    .bind(id)
    .fetch_optional(conn)
    .await
    .map_err(WriteError::Storage)
}

async fn insert_registered(
    conn: &mut sqlx::PgConnection,
    input: &CanonicalInput,
    instance: &str,
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
    .bind(input.original_hash())
    .bind(&input.request_key)
    .bind(input.provenance)
    .execute(conn)
    .await
    .map_err(refused)?;
    Ok(())
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
