//! Shared home settings; execution ownership is a separate authority.

use sqlx::PgPool;
use uuid::Uuid;

use crate::services::cluster::agent_execution_node::AgentExecutionNode;
use crate::services::cluster::intake_router_hook::owner_record::OwnerIdentity;

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub(crate) struct Preference {
    pub mode: String,
    pub home_instance_id: Option<String>,
    pub revision: i64,
    pub operation_id: Uuid,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WriteOutcome {
    Committed(Preference),
    Reused(Preference),
    Conflict(Option<Preference>),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum WriteError {
    #[error("invalid home preference: {0}")]
    Invalid(&'static str),
    #[error("operation id was reused for a different home write")]
    OperationMismatch,
    #[error(transparent)]
    Storage(#[from] sqlx::Error),
}

pub(crate) async fn read(
    pool: &PgPool,
    identity: &OwnerIdentity,
) -> Result<Option<Preference>, sqlx::Error> {
    sqlx::query_as("SELECT mode, home_instance_id, revision, operation_id FROM channel_home_preferences WHERE provider=$1 AND raw_channel_id=$2")
        .bind(identity.provider()).bind(identity.raw_channel_id()).fetch_optional(pool).await
}

/// Revision zero means no row. A reset writes inherit rather than deleting the row.
pub(crate) async fn write(
    pool: &PgPool,
    identity: &OwnerIdentity,
    expected_revision: i64,
    operation_id: Uuid,
    home: Option<&str>,
) -> Result<WriteOutcome, WriteError> {
    if identity.provider().is_empty() || identity.raw_channel_id().is_empty() {
        return Err(WriteError::Invalid("empty channel identity"));
    }
    let next_revision = expected_revision
        .checked_add(1)
        .filter(|_| expected_revision >= 0)
        .ok_or(WriteError::Invalid("invalid expected revision"))?;
    AgentExecutionNode {
        default_node_id: home.map(str::to_owned),
    }
    .validate()
    .map_err(WriteError::Invalid)?;
    let mode = if home.is_some() {
        "override"
    } else {
        "inherit"
    };
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(identity.advisory_key())
        .execute(&mut *tx)
        .await?;
    let stored: Option<Preference> = sqlx::query_as("SELECT mode, home_instance_id, revision, operation_id FROM channel_home_preferences WHERE provider=$1 AND raw_channel_id=$2")
        .bind(identity.provider()).bind(identity.raw_channel_id()).fetch_optional(&mut *tx).await?;
    if let Some(previous) = &stored
        && previous.operation_id == operation_id
    {
        if previous.revision != next_revision
            || previous.home_instance_id.as_deref() != home
            || previous.mode != mode
        {
            return Err(WriteError::OperationMismatch);
        }
        tx.commit().await?;
        return Ok(WriteOutcome::Reused(previous.clone()));
    }
    if stored.as_ref().map_or(0, |row| row.revision) != expected_revision {
        tx.commit().await?;
        return Ok(WriteOutcome::Conflict(stored));
    }
    let saved = sqlx::query_as("INSERT INTO channel_home_preferences (provider,raw_channel_id,mode,home_instance_id,revision,operation_id) VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT (provider,raw_channel_id) DO UPDATE SET mode=EXCLUDED.mode,home_instance_id=EXCLUDED.home_instance_id,revision=EXCLUDED.revision,operation_id=EXCLUDED.operation_id,updated_at=NOW() WHERE channel_home_preferences.revision=$7 RETURNING mode,home_instance_id,revision,operation_id")
        .bind(identity.provider()).bind(identity.raw_channel_id()).bind(mode).bind(home)
        .bind(next_revision).bind(operation_id).bind(expected_revision).fetch_one(&mut *tx).await?;
    tx.commit().await?;
    Ok(WriteOutcome::Committed(saved))
}

#[cfg(test)]
mod pg_tests;
