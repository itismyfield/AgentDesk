//! Replay receipt transitions; every acknowledgement is returned only after its commit succeeded.

#[cfg(test)]
mod hold_notice;
mod settle;
mod start;
#[cfg(test)]
pub(crate) use hold_notice::withhold_and_notify;
pub(crate) use start::{
    BeginAck, BeginFrom, CanonicalInput, EffectProjection, begin, bind_session,
    ensure_same_request, register_or_reuse,
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
