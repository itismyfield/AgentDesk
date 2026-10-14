//! Offline conversion of one exact retired row to the existing legacy representation.
use super::*;
use crate::services::session_host::legacy_return::RetiredEnd;

/// The caller must keep dcserver stopped and publish the tmux marker only after this commits.
/// The archive is one slot for this row's lifetime; a later incarnation does not overwrite it.
pub(crate) async fn return_legacy_pg(
    pool: &PgPool,
    observed: &HostedObservation,
    owner: &HostedOwner,
    old_nonce: &str,
    ended: &RetiredEnd,
) -> Result<HostedCasOutcome, HostedTransitionError> {
    let known = match &observed.record {
        HostedRecord::Known(known) if known.state == HostedState::Retired => known,
        HostedRecord::Known(known) => {
            return Err(HostedTransitionError::NotAllowed(Some(known.state)));
        }
        HostedRecord::Legacy => return Err(HostedTransitionError::NotAllowed(None)),
        HostedRecord::Unknown(_) => return Err(HostedTransitionError::UnknownRecord),
    };
    if known.owner != *owner {
        return Err(HostedTransitionError::OwnerMismatch);
    }
    if known.execution_nonce != old_nonce {
        return Err(HostedTransitionError::NonceMismatch);
    }
    if !ended.matches(known) || HostedRecord::decode(observed.raw.as_ref()) != observed.record {
        return Err(HostedTransitionError::ExpectedOverwrite);
    }
    let mut tx = pool.begin().await.map_err(database_error)?;
    let result = sqlx::query(
        "UPDATE sessions
         SET returned_hosted_execution = hosted_execution,
             hosted_execution = NULL, status = 'idle', last_heartbeat = NOW()
         WHERE id = $1
           AND hosted_execution::TEXT IS NOT DISTINCT FROM $2::JSONB::TEXT
           AND hosted_execution->>'state' = 'retired'
           AND hosted_execution->>'execution_nonce' = $3
           AND hosted_execution->'owner' = $4::JSONB
           AND hosted_execution->'owner'->>'logical_key' = $5
           AND identity_kind = 'discord_channel'
           AND provider = $6 AND discord_token_hash = $7 AND channel_id = $8
           AND returned_hosted_execution IS NULL
           AND active_dispatch_id IS NULL AND COALESCE(active_children, 0) = 0
           AND NOT EXISTS (SELECT 1 FROM sessions child
                           WHERE child.parent_session_id = sessions.id AND child.closed_at IS NULL)",
    )
    .bind(observed.session_id)
    .bind(observed.raw.clone())
    .bind(old_nonce)
    .bind(serde_json::to_value(owner).map_err(|_| HostedTransitionError::Incomplete)?)
    .bind(&owner.logical_key)
    .bind(&owner.provider)
    .bind(&owner.discord_token_hash)
    .bind(&owner.channel_id)
    .execute(&mut *tx)
    .await
    .map_err(database_error)?;
    tx.commit().await.map_err(database_error)?;
    Ok(if result.rows_affected() == 1 {
        HostedCasOutcome::Written
    } else {
        HostedCasOutcome::Stale
    })
}

fn database_error(error: sqlx::Error) -> HostedTransitionError {
    HostedTransitionError::Database(format!("return legacy: {error}"))
}

#[cfg(test)]
#[path = "legacy_return_tests.rs"]
mod tests;
