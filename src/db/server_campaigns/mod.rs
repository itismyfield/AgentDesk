use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Row, Transaction, types::Json};

use crate::services::campaign_execution::{
    control,
    definition::{self, DefinitionInput},
    model::*,
};

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
    pub outcome: CommandOutcome,
    pub replayed: bool,
}

#[derive(Clone, Copy)]
enum Request<'a> {
    Create(&'a DefinitionInput),
    Command(&'a Command),
}

/// B2 supplies a stable campaign ID and maps this saved outcome to its HTTP response.
pub async fn create(
    pool: &PgPool,
    id: &str,
    input: &DefinitionInput,
    key: &str,
    observed_at: DateTime<Utc>,
) -> StoreResult<CommandReply> {
    transact(pool, id, key, observed_at, Request::Create(input)).await
}

/// Internal domain commands are not an HTTP request DTO or an execution toggle.
pub async fn execute(
    pool: &PgPool,
    id: &str,
    envelope: &CommandEnvelope,
) -> StoreResult<CommandReply> {
    transact(
        pool,
        id,
        &envelope.idempotency_key,
        envelope.observed_at,
        Request::Command(&envelope.command),
    )
    .await
}

async fn transact(
    pool: &PgPool,
    id: &str,
    key: &str,
    observed_at: DateTime<Utc>,
    request: Request<'_>,
) -> StoreResult<CommandReply> {
    let mut tx = pool.begin().await?;
    let reply = apply(&mut tx, id, key, observed_at, request).await?;
    tx.commit().await?;
    Ok(reply)
}

async fn apply(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    key: &str,
    observed_at: DateTime<Utc>,
    request: Request<'_>,
) -> StoreResult<CommandReply> {
    if !definition::valid_id(id) || key.trim().is_empty() {
        return Err(StoreError::InvalidIdentity);
    }
    let fingerprint = match request {
        Request::Create(input) => format!(
            "create:{}",
            blake3::hash(&serde_json::to_vec(&(id, input))?).to_hex()
        ),
        Request::Command(command) => format!("command:{}", control::command_fingerprint(command)?),
    };
    sqlx::query(
        "INSERT INTO server_campaign_keys (campaign_id) VALUES ($1) ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .execute(&mut **tx)
    .await?;
    sqlx::query("SELECT campaign_id FROM server_campaign_keys WHERE campaign_id = $1 FOR UPDATE")
        .bind(id)
        .fetch_one(&mut **tx)
        .await?;
    // Both creates and run commands hold the campaign lock; an existing run is locked too.
    sqlx::query(
        "SELECT r.run_id FROM server_campaign_runs r JOIN server_campaign_definitions d \
         ON d.campaign_id = r.campaign_id AND d.run_id = r.run_id WHERE d.campaign_id = $1 FOR UPDATE OF r",
    ).bind(id).fetch_optional(&mut **tx).await?;
    let saved = sqlx::query(
        "SELECT fingerprint, outcome FROM server_campaign_receipts WHERE campaign_id = $1 AND idempotency_key = $2",
    ).bind(id).bind(key).fetch_optional(&mut **tx).await?;
    if let Some(saved) = saved {
        if saved.try_get::<String, _>("fingerprint")? != fingerprint {
            return Err(StoreError::IdempotencyConflict);
        }
        return Ok(CommandReply {
            outcome: saved.try_get::<Json<CommandOutcome>, _>("outcome")?.0,
            replayed: true,
        });
    }
    let previous = read::get_in(tx, id).await?;
    let decision = match (&request, &previous) {
        (Request::Create(input), None) => Some(control::create_definition(
            id.to_owned(),
            (*input).clone(),
            key,
            observed_at,
        )),
        (Request::Create(_), Some(_)) => Some(Err(Rejection::new(RejectionCode::AlreadyExists))),
        (Request::Command(command), Some(state)) => Some(control::decide_with_receipt(
            state,
            &CommandEnvelope {
                idempotency_key: key.to_owned(),
                observed_at,
                command: (*command).clone(),
            },
            None,
        )),
        (Request::Command(_), None) => None,
    };
    let outcome = match decision {
        Some(Ok(decision)) => {
            write::persist(tx, previous.as_ref(), &decision).await?;
            CommandOutcome::Decided(Ok(decision.result))
        }
        Some(Err(rejection)) => CommandOutcome::Decided(Err(rejection)),
        None => CommandOutcome::NotFound,
    };
    sqlx::query(
        "INSERT INTO server_campaign_receipts (campaign_id, idempotency_key, fingerprint, outcome, observed_at) \
         VALUES ($1, $2, $3, $4, $5)",
    ).bind(id).bind(key).bind(fingerprint).bind(Json(&outcome)).bind(observed_at)
        .execute(&mut **tx).await?;
    Ok(CommandReply {
        outcome,
        replayed: false,
    })
}

#[cfg(test)]
mod recovery_tests;
#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;
