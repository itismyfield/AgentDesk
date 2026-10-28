//! Replay receipt transitions; every acknowledgement is returned only after its commit succeeded.

mod hold_notice;
mod settle;
mod start;
pub(crate) use hold_notice::recover_hold_notice;
pub(crate) use hold_notice::withhold_and_notify;
pub(crate) use start::{
    BeginAck, BeginFrom, CanonicalInput, EffectProjection, begin, bind_session,
    ensure_same_request, ensure_same_request_on_connection, register_or_reuse,
};

use crate::services::discord::replay_policy::live::{NoEffectEvidence, TerminalEvidence};
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

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CasMiss => f.write_str("receipt is not at this exact attempt"),
            Self::Fenced(message) => write!(f, "replay fence refused: {message}"),
            Self::Storage(error) => write!(f, "write failed before commit: {error}"),
            Self::AckUnknown(error) => write!(f, "commit not acknowledged: {error}"),
            Self::IdentityConflict(id) => write!(f, "receipt {id} names a different request"),
        }
    }
}

pub(super) fn refused(error: sqlx::Error) -> WriteError {
    match error.as_database_error().and_then(|db| db.constraint()) {
        Some("replay_disposition_fence") => WriteError::Fenced(error.to_string()),
        _ => WriteError::Storage(error),
    }
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

async fn commit(tx: sqlx::Transaction<'_, sqlx::Postgres>) -> Result<(), WriteError> {
    tx.commit().await.map_err(WriteError::AckUnknown)
}

async fn settle(
    pool: &PgPool,
    attempt: &ExactAttempt,
    disposition: &str,
    hold: Option<(&str, &serde_json::Value)>,
) -> Result<(), WriteError> {
    let mut tx = pool.begin().await.map_err(WriteError::Storage)?;
    settle::on_tx(&mut tx, attempt, disposition, hold).await?;
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
pub(crate) struct NoEffectAck {
    receipt_id: i64,
    previous_nonce: String,
    proof: HoldProof,
    pool: PgPool,
}

impl NoEffectAck {
    pub(crate) fn into_parts(self) -> (i64, String, HoldProof, PgPool) {
        (self.receipt_id, self.previous_nonce, self.proof, self.pool)
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
        previous_nonce: attempt.nonce.clone(),
        proof: attempt.hold_proof(),
        pool: pool.clone(),
    })
}

/// Holds the attempt with its reason and preserved result; the receipt keeps blocking either way.
#[cfg(test)]
pub(crate) async fn withhold(
    pool: &PgPool,
    attempt: &ExactAttempt,
    reason: &str,
    preserved: &serde_json::Value,
) -> Result<(), WriteError> {
    settle(pool, attempt, "withheld", Some((reason, preserved))).await
}

/// A copy of custody for settlement only; it cannot construct a start permit.
#[derive(Debug, Clone)]
pub(crate) struct HoldProof {
    id: i64,
    nonce: String,
    owner: String,
    retry: Option<String>,
}
impl ExactAttempt {
    pub(crate) fn hold_proof(&self) -> HoldProof {
        HoldProof {
            id: self.receipt_id,
            nonce: self.nonce.clone(),
            owner: self.incarnation.clone(),
            retry: self.retry_of.clone(),
        }
    }
}
impl HoldProof {
    pub(crate) fn successor(&self, nonce: &str) -> Self {
        Self {
            id: self.id,
            nonce: nonce.into(),
            owner: self.owner.clone(),
            retry: Some(self.nonce.clone()),
        }
    }
    pub(crate) async fn abandon(&self, pool: &PgPool) -> Result<(), WriteError> {
        let attempt = ExactAttempt {
            receipt_id: self.id,
            nonce: self.nonce.clone(),
            incarnation: self.owner.clone(),
            retry_of: self.retry.clone(),
        };
        let reason = "start or exact handoff did not execute";
        let session:Option<String>=sqlx::query_scalar("SELECT session_key FROM sessions WHERE current_replay_receipt_id=$1 AND replay_episode_nonce=$2 LIMIT 1")
            .bind(self.id).bind(&self.nonce).fetch_optional(pool).await.map_err(WriteError::Storage)?;
        withhold_and_notify(
            pool,
            &attempt,
            reason,
            &serde_json::json!({"hold":{"nonce":self.nonce,"reason":reason}}),
            crate::db::replay_disposition::hold_notice::NoticeRouting {
                bot: "notify",
                session_key: session.as_deref(),
            },
        )
        .await
        .map(|_| ())
    }
}

impl std::fmt::Debug for NoEffectAck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoEffectAck")
            .field("receipt_id", &self.receipt_id)
            .field("previous_nonce", &self.previous_nonce)
            .finish_non_exhaustive()
    }
}
