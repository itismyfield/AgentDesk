use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction, types::Json};

use crate::services::campaign_execution::{
    candidate_seal::CandidateSeal,
    evidence::{EvidenceDecision, EvidenceEnvelope, EvidenceStatus},
    model::AttemptBinding,
};

// Rows in the existing evidence table; documents of any other shape are preserved, never adopted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum StoredEvidence {
    Evidence {
        envelope: EvidenceEnvelope,
        status: EvidenceStatus,
        decision: Option<EvidenceDecision>,
    },
    Seal {
        seal: CandidateSeal,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum EvidenceStoreError {
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("evidence ID already holds a different record")]
    Conflict,
    #[error("evidence status cannot change this way")]
    Transition,
}

type Result<T> = std::result::Result<T, EvidenceStoreError>;

impl StoredEvidence {
    pub fn recorded(envelope: EvidenceEnvelope) -> Self {
        Self::Evidence {
            envelope,
            status: EvidenceStatus::Recorded,
            decision: None,
        }
    }

    fn key(&self) -> (Option<&AttemptBinding>, &str, &str) {
        match self {
            Self::Evidence { envelope: e, .. } => {
                (Some(&e.binding), &e.binding.attempt_id, &e.evidence_id)
            }
            Self::Seal { seal } => (None, &seal.owner_attempt, &seal.seal_id),
        }
    }

    // Same ID is a retry only for the same submitted identity; status and times may differ.
    fn same_identity(&self, other: &StoredEvidence) -> bool {
        match (self, other) {
            (Self::Evidence { envelope: a, .. }, Self::Evidence { envelope: b, .. }) => {
                (&a.payload_digest, &a.binding, &a.pin_hash, a.source)
                    == (&b.payload_digest, &b.binding, &b.pin_hash, b.source)
            }
            (Self::Seal { seal: a }, Self::Seal { seal: b }) => a.confirm_retry(b).is_ok(),
            _ => false,
        }
    }
}

async fn load(
    tx: &mut Transaction<'_, Postgres>,
    campaign_id: &str,
    run_id: &str,
    evidence_id: &str,
) -> Result<Option<serde_json::Value>> {
    Ok(sqlx::query_scalar::<_, Json<serde_json::Value>>(
        "SELECT document FROM server_campaign_evidence \
         WHERE campaign_id = $1 AND run_id = $2 AND evidence_id = $3 FOR UPDATE",
    )
    .bind(campaign_id)
    .bind(run_id)
    .bind(evidence_id)
    .fetch_optional(&mut **tx)
    .await?
    .map(|v| v.0))
}

// Insert-or-compare under the row key: concurrent or repeated submissions keep the first record.
pub async fn record(
    tx: &mut Transaction<'_, Postgres>,
    campaign_id: &str,
    run_id: &str,
    record: &StoredEvidence,
) -> Result<StoredEvidence> {
    let (binding, attempt, id) = record.key();
    if binding.is_some_and(|b| (b.campaign_id.as_str(), b.run_id.as_str()) != (campaign_id, run_id))
    {
        return Err(EvidenceStoreError::Conflict);
    }
    let inserted = sqlx::query(
        "INSERT INTO server_campaign_evidence (campaign_id, run_id, attempt_id, evidence_id, document) \
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT (campaign_id, run_id, evidence_id) DO NOTHING",
    )
    .bind(campaign_id)
    .bind(run_id)
    .bind(attempt)
    .bind(id)
    .bind(Json(record))
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if inserted == 1 {
        return Ok(record.clone());
    }
    let stored = load(tx, campaign_id, run_id, id)
        .await?
        .and_then(|v| serde_json::from_value::<StoredEvidence>(v).ok())
        .filter(|stored| stored.same_identity(record))
        .ok_or(EvidenceStoreError::Conflict)?;
    Ok(stored)
}

// Applies one admission decision; the envelope itself is never rewritten.
pub async fn decide(
    tx: &mut Transaction<'_, Postgres>,
    campaign_id: &str,
    run_id: &str,
    evidence_id: &str,
    decision: &EvidenceDecision,
) -> Result<StoredEvidence> {
    let stored = load(tx, campaign_id, run_id, evidence_id)
        .await?
        .and_then(|v| serde_json::from_value::<StoredEvidence>(v).ok());
    let Some(StoredEvidence::Evidence {
        envelope,
        status,
        decision: previous,
    }) = stored
    else {
        return Err(EvidenceStoreError::Conflict);
    };
    if previous.as_ref() == Some(decision) {
        return Ok(StoredEvidence::Evidence {
            envelope,
            status,
            decision: previous,
        });
    }
    if !status.can_become(decision.status) {
        return Err(EvidenceStoreError::Transition);
    }
    let next = StoredEvidence::Evidence {
        envelope,
        status: decision.status,
        decision: Some(decision.clone()),
    };
    sqlx::query(
        "UPDATE server_campaign_evidence SET document = $4 \
         WHERE campaign_id = $1 AND run_id = $2 AND evidence_id = $3",
    )
    .bind(campaign_id)
    .bind(run_id)
    .bind(evidence_id)
    .bind(Json(&next))
    .execute(&mut **tx)
    .await?;
    Ok(next)
}

#[cfg(test)]
#[path = "evidence_store_tests.rs"]
mod tests;
