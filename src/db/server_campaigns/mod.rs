use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction, types::Json};

use crate::domain::campaign::model::{
    CampaignControl, CampaignState, CommandResult, Decision, Event, EventKind, EvidenceWrite,
    NodeRuntime, Rejection, Run,
};

#[cfg(test)]
use crate::domain::campaign::{definition::DefinitionInput, model::*};

pub(crate) mod command_transaction;
pub mod evidence_store;
mod read;
mod write;
pub use read::*;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("invalid campaign ID or empty idempotency key")]
    InvalidIdentity,
    #[error("idempotency key already used for a different request")]
    IdempotencyConflict,
    #[error("stored campaign identity or binding is inconsistent")]
    InconsistentState,
}

pub type StoreResult<T> = Result<T, StoreError>;

// The saved outcome is independent of later state, including rejection and absence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommandOutcome {
    Decided(Result<CommandResult, Rejection>),
    NotFound,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandReply {
    #[cfg(test)]
    pub outcome: CommandOutcome,
    pub response: ReceiptResponse,
    #[cfg(test)]
    pub replayed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiptResponse {
    pub status: u16,
    pub body: serde_json::Value,
}

fn evidence_error(error: evidence_store::EvidenceStoreError) -> StoreError {
    match error {
        evidence_store::EvidenceStoreError::Sql(e) => StoreError::Sql(e),
        evidence_store::EvidenceStoreError::Json(e) => StoreError::Json(e),
        _ => StoreError::InconsistentState,
    }
}

// Writes one evidence row of a decision; an ID that already holds something else fails the transaction.
async fn write_evidence(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    run_id: &str,
    written: &EvidenceWrite,
) -> StoreResult<()> {
    use evidence_store::StoredEvidence;
    match written {
        EvidenceWrite::Seal(seal) => {
            let seal = StoredEvidence::Seal { seal: seal.clone() };
            evidence_store::record(tx, id, run_id, &seal)
                .await
                .map_err(evidence_error)?;
        }
        EvidenceWrite::Admitted { envelope, decision } => {
            let evidence = StoredEvidence::recorded((**envelope).clone());
            evidence_store::record(tx, id, run_id, &evidence)
                .await
                .map_err(evidence_error)?;
            evidence_store::decide(tx, id, run_id, &envelope.evidence_id, decision)
                .await
                .map_err(evidence_error)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod recovery_tests;
#[cfg(test)]
mod stage_store_tests;
#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
#[cfg(test)]
mod verification_store_tests;
