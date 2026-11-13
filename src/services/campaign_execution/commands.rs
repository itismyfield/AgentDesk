use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};

use crate::{
    db::server_campaigns::{
        CommandOutcome, CommandReply, ReceiptResponse, StoreResult,
        command_transaction::{self, Preparation, StoreRequest},
    },
    domain::campaign::{
        definition::DefinitionInput,
        model::{Command, CommandEnvelope, Rejection, RejectionCode},
    },
};

use super::control;

pub(crate) type RenderReceipt = fn(&CommandOutcome) -> StoreResult<ReceiptResponse>;

/// The caller supplies a stable ID and pure response renderer; replay uses the saved response.
pub(crate) async fn create(
    pool: &PgPool,
    id: &str,
    input: &DefinitionInput,
    key: &str,
    observed_at: DateTime<Utc>,
    render: RenderReceipt,
) -> StoreResult<CommandReply> {
    let tx = pool.begin().await?;
    let (tx, reply) = apply_in(
        tx,
        id,
        key,
        observed_at,
        StoreRequest::Create(input.clone()),
        render,
    )
    .await?;
    tx.commit().await?;
    Ok(reply)
}

/// Internal domain commands are not an HTTP request DTO or an execution toggle.
pub(crate) async fn execute(
    pool: &PgPool,
    id: &str,
    envelope: &CommandEnvelope,
    render: RenderReceipt,
) -> StoreResult<CommandReply> {
    let tx = pool.begin().await?;
    let (tx, reply) = apply_in(
        tx,
        id,
        &envelope.idempotency_key,
        envelope.observed_at,
        StoreRequest::Command(envelope.command.clone()),
        render,
    )
    .await?;
    tx.commit().await?;
    Ok(reply)
}

pub(crate) async fn apply_in<'a>(
    tx: Transaction<'a, Postgres>,
    id: &str,
    key: &str,
    observed_at: DateTime<Utc>,
    request: StoreRequest,
    render: RenderReceipt,
) -> StoreResult<(Transaction<'a, Postgres>, CommandReply)> {
    let prepared = match command_transaction::prepare(tx, id, key, observed_at, request).await? {
        Preparation::Replay { tx, reply } => return Ok((tx, reply)),
        Preparation::Fresh(prepared) => prepared,
    };
    let unsealed = match (
        prepared.request(),
        prepared.previous().and_then(|s| s.run.as_ref()),
    ) {
        (StoreRequest::Command(Command::BeginReview { seal, .. }), Some(_)) => prepared
            .stored_seal()
            .is_none_or(|stored| seal.confirm_retry(stored).is_err()),
        _ => false,
    };
    let decision = match (prepared.request(), prepared.previous()) {
        _ if unsealed => Some(Err(Rejection::new(RejectionCode::StaleBinding))),
        (StoreRequest::Create(input), None) => Some(control::create_definition(
            prepared.id().to_owned(),
            input.clone(),
            prepared.key(),
            prepared.observed_at(),
        )),
        (StoreRequest::Create(_), Some(_)) => {
            Some(Err(Rejection::new(RejectionCode::AlreadyExists)))
        }
        (StoreRequest::Command(command), Some(state)) => Some(control::decide_with_receipt(
            state,
            &CommandEnvelope {
                idempotency_key: prepared.key().to_owned(),
                observed_at: prepared.observed_at(),
                command: command.clone(),
            },
            None,
        )),
        (StoreRequest::Command(_), None) => None,
    };
    let staged = prepared.stage(decision).await?;
    let response = render(staged.outcome())?;
    staged.record_receipt(response).await
}

#[cfg(test)]
#[path = "commands/transaction_tests.rs"]
mod transaction_tests;
