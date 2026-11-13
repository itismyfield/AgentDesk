use chrono::{DateTime, Utc};
use sqlx::{Postgres, Row, Transaction, types::Json};

use crate::domain::campaign::{
    candidate_seal::CandidateSeal,
    definition::{self, DefinitionInput},
    model::{CampaignState, Command, Decision, Rejection, command_fingerprint},
};

use super::{
    CommandOutcome, CommandReply, ReceiptResponse, StoreError, StoreResult, evidence_error,
    evidence_store, read, write, write_evidence,
};

pub(crate) enum StoreRequest {
    Create(DefinitionInput),
    Command(Command),
}

pub(crate) enum Preparation<'a> {
    Replay {
        tx: Transaction<'a, Postgres>,
        reply: CommandReply,
    },
    Fresh(Box<PreparedCommand<'a>>),
}

pub(crate) struct PreparedCommand<'a> {
    tx: Transaction<'a, Postgres>,
    id: String,
    key: String,
    observed_at: DateTime<Utc>,
    fingerprint: String,
    request: StoreRequest,
    previous: Option<CampaignState>,
    stored_seal: Option<CandidateSeal>,
}

pub(crate) struct StagedCommand<'a> {
    tx: Transaction<'a, Postgres>,
    id: String,
    key: String,
    observed_at: DateTime<Utc>,
    fingerprint: String,
    outcome: CommandOutcome,
}

pub(crate) async fn prepare<'a>(
    mut tx: Transaction<'a, Postgres>,
    id: &str,
    key: &str,
    observed_at: DateTime<Utc>,
    request: StoreRequest,
) -> StoreResult<Preparation<'a>> {
    if !definition::valid_id(id) || key.trim().is_empty() {
        return Err(StoreError::InvalidIdentity);
    }
    let fingerprint = match &request {
        StoreRequest::Create(input) => format!(
            "create:{}",
            blake3::hash(&serde_json::to_vec(&(id, input))?).to_hex()
        ),
        StoreRequest::Command(command) => format!("command:{}", command_fingerprint(command)?),
    };
    sqlx::query(
        "INSERT INTO server_campaign_keys (campaign_id) VALUES ($1) ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("SELECT campaign_id FROM server_campaign_keys WHERE campaign_id = $1 FOR UPDATE")
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
    // Both creates and run commands hold the campaign lock; an existing run is locked too.
    sqlx::query(
        "SELECT r.run_id FROM server_campaign_runs r JOIN server_campaign_definitions d \
         ON d.campaign_id = r.campaign_id AND d.run_id = r.run_id WHERE d.campaign_id = $1 FOR UPDATE OF r",
    ).bind(id).fetch_optional(&mut *tx).await?;
    let saved = sqlx::query(
        "SELECT fingerprint, outcome, response_status, response_body FROM server_campaign_receipts \
         WHERE campaign_id = $1 AND idempotency_key = $2",
    )
    .bind(id)
    .bind(key)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(saved) = saved {
        if saved.try_get::<String, _>("fingerprint")? != fingerprint {
            return Err(StoreError::IdempotencyConflict);
        }
        return Ok(Preparation::Replay {
            tx,
            reply: CommandReply {
                #[cfg(test)]
                outcome: saved.try_get::<Json<CommandOutcome>, _>("outcome")?.0,
                response: ReceiptResponse {
                    status: u16::try_from(saved.try_get::<i32, _>("response_status")?)
                        .map_err(|_| StoreError::InconsistentState)?,
                    body: saved
                        .try_get::<Json<serde_json::Value>, _>("response_body")?
                        .0,
                },
                #[cfg(test)]
                replayed: true,
            },
        });
    }
    let previous = read::get_in(&mut tx, id).await?;
    let stored_seal = if let (StoreRequest::Command(Command::BeginReview { seal, .. }), Some(run)) =
        (&request, previous.as_ref().and_then(|s| s.run.as_ref()))
    {
        evidence_store::sealed(&mut tx, id, &run.id, &seal.seal_id)
            .await
            .map_err(evidence_error)?
    } else {
        None
    };
    Ok(Preparation::Fresh(Box::new(PreparedCommand {
        tx,
        id: id.to_owned(),
        key: key.to_owned(),
        observed_at,
        fingerprint,
        request,
        previous,
        stored_seal,
    })))
}

impl<'a> PreparedCommand<'a> {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }

    pub(crate) fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }

    pub(crate) fn request(&self) -> &StoreRequest {
        &self.request
    }

    pub(crate) fn previous(&self) -> Option<&CampaignState> {
        self.previous.as_ref()
    }

    pub(crate) fn stored_seal(&self) -> Option<&CandidateSeal> {
        self.stored_seal.as_ref()
    }

    pub(crate) async fn stage(
        self: Box<Self>,
        decision: Option<Result<Decision, Rejection>>,
    ) -> StoreResult<StagedCommand<'a>> {
        let Self {
            mut tx,
            id,
            key,
            observed_at,
            fingerprint,
            previous,
            ..
        } = *self;
        let outcome = match decision {
            Some(Ok(decision)) => {
                write::persist(&mut tx, previous.as_ref(), &decision).await?;
                // Evidence rows commit or roll back together with the state they belong to.
                if let Some(run) = &decision.state.run {
                    for record in &decision.records {
                        write_evidence(&mut tx, &id, &run.id, record).await?;
                    }
                }
                CommandOutcome::Decided(Ok(decision.result))
            }
            Some(Err(rejection)) => CommandOutcome::Decided(Err(rejection)),
            None => CommandOutcome::NotFound,
        };
        Ok(StagedCommand {
            tx,
            id,
            key,
            observed_at,
            fingerprint,
            outcome,
        })
    }
}

impl<'a> StagedCommand<'a> {
    pub(crate) fn outcome(&self) -> &CommandOutcome {
        &self.outcome
    }

    pub(crate) async fn record_receipt(
        self,
        response: ReceiptResponse,
    ) -> StoreResult<(Transaction<'a, Postgres>, CommandReply)> {
        let Self {
            mut tx,
            id,
            key,
            observed_at,
            fingerprint,
            outcome,
        } = self;
        sqlx::query(
            "INSERT INTO server_campaign_receipts \
             (campaign_id, idempotency_key, fingerprint, outcome, observed_at, response_status, response_body) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        ).bind(id).bind(key).bind(fingerprint).bind(Json(&outcome)).bind(observed_at)
            .bind(i32::from(response.status)).bind(Json(&response.body))
            .execute(&mut *tx).await?;
        Ok((
            tx,
            CommandReply {
                #[cfg(test)]
                outcome,
                response,
                #[cfg(test)]
                replayed: false,
            },
        ))
    }
}
