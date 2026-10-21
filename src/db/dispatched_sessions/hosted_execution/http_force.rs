//! Row reads and the one write an HTTP force cancel of a Herdr execution makes around the
//! P11 terminate service; each is bound to the row id and execution it was given.
use super::*;

/// The execution a cancel's selected row records, when it is a Herdr record its row owns.
pub(crate) fn herdr_execution(
    raw: Option<&Value>,
    identity: &[Option<String>; 4],
) -> Option<HostedExecution> {
    let HostedRecord::Known(record) = HostedRecord::decode(raw) else {
        return None;
    };
    let herdr = record
        .location
        .as_ref()
        .is_some_and(|l| l.host == HERDR_HOST);
    (herdr && owner_matches_row(&record.owner, identity)).then_some(record)
}

/// The row's active dispatch; a missing row is an error, never an idle one.
pub(crate) async fn active_dispatch_pg(
    pool: &PgPool,
    session_id: i64,
) -> Result<Option<String>, String> {
    sqlx::query_scalar("SELECT active_dispatch_id FROM sessions WHERE id = $1")
        .bind(session_id)
        .fetch_one(pool)
        .await
        .map_err(|error| format!("load active dispatch: {error}"))
}

/// Disconnects the row only while it still records `record` retired, field by field, with
/// `dispatch` or, once that dispatch's cancel cleared it, none active; successors stay untouched.
pub(crate) async fn disconnect_retired_pg(
    pool: &PgPool,
    session_id: i64,
    record: &HostedExecution,
    dispatch: Option<&str>,
) -> Result<bool, String> {
    let owner = &record.owner;
    let updated = sqlx::query(
        "UPDATE sessions
         SET status = 'disconnected', active_dispatch_id = NULL, claude_session_id = NULL
         WHERE id = $1
           AND hosted_execution->>'state' = 'retired'
           AND hosted_execution->>'execution_nonce' = $2
           AND hosted_execution->'owner'->>'provider' = $3
           AND hosted_execution->'owner'->>'discord_token_hash' = $4
           AND hosted_execution->'owner'->>'channel_id' = $5
           AND hosted_execution->'owner'->>'logical_key' = $6
           AND hosted_execution->'owner'->>'owner_node' = $7
           AND hosted_execution->'owner'->>'runtime_root' = $8
           AND (active_dispatch_id IS NULL OR active_dispatch_id = $9)",
    )
    .bind(session_id)
    .bind(&record.execution_nonce)
    .bind(&owner.provider)
    .bind(&owner.discord_token_hash)
    .bind(&owner.channel_id)
    .bind(&owner.logical_key)
    .bind(&owner.owner_node)
    .bind(&owner.runtime_root)
    .bind(dispatch)
    .execute(pool)
    .await
    .map_err(|error| format!("disconnect retired session: {error}"))?;
    Ok(updated.rows_affected() == 1)
}
