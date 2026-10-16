//! The native clear boundary write an earlier build's `/clear` made; tests seed with it the
//! unresolved clears admission still settles.

use anyhow::{Result, anyhow};
use sqlx::{Postgres, Transaction};

use super::{NativeClearGeneration, lock_channel_transcript_clear_fence};

// The `finish_channel_clear_boundary_tx` row update plus the correlation, in one statement so the
// generation and its ticket commit or vanish together.
const WRITE_NATIVE_CLEAR_BOUNDARY_SQL: &str = "INSERT INTO channel_session_clear_boundaries (
         channel_id, cleared_at, cleared_through_id, clear_generation,
         native_clear_generation, native_clear_ticket
     )
     SELECT $1, NOW(), COALESCE(MAX(id), 0), 1, 1, $2
       FROM session_transcripts
      WHERE channel_id = $1
     ON CONFLICT (channel_id) DO UPDATE SET
         cleared_at = GREATEST(
             channel_session_clear_boundaries.cleared_at,
             EXCLUDED.cleared_at
         ),
         cleared_through_id = GREATEST(
             channel_session_clear_boundaries.cleared_through_id,
             EXCLUDED.cleared_through_id
         ),
         clear_generation = channel_session_clear_boundaries.clear_generation + 1,
         native_clear_generation = channel_session_clear_boundaries.clear_generation + 1,
         native_clear_ticket = EXCLUDED.native_clear_ticket,
         native_clear_resolved_at = NULL
     RETURNING native_clear_generation";

/// Native variant of [`super::finish_channel_clear_boundary_tx`]: same lock and markers, plus the ticket
/// the clear waits on, unresolved, under the generation this write produces.
pub(crate) async fn finish_native_channel_clear_boundary_tx(
    mut tx: Transaction<'_, Postgres>,
    channel_id: &str,
    ticket: &serde_json::Value,
) -> Result<NativeClearGeneration> {
    let generation = write_native_channel_clear_boundary(&mut tx, channel_id, ticket).await?;
    tx.commit()
        .await
        .map_err(|error| anyhow!("commit native channel clear boundary failed: {error}"))?;
    Ok(generation)
}

pub(crate) async fn write_native_channel_clear_boundary(
    tx: &mut Transaction<'_, Postgres>,
    channel_id: &str,
    ticket: &serde_json::Value,
) -> Result<NativeClearGeneration> {
    let channel_id = channel_id.trim();
    if channel_id.is_empty() || !ticket.is_object() {
        return Err(anyhow!(
            "native channel clear boundary requires a channel_id and an object ticket"
        ));
    }
    lock_channel_transcript_clear_fence(tx, channel_id)
        .await
        .map_err(|error| anyhow!("lock native channel clear boundary failed: {error}"))?;
    let generation = sqlx::query_scalar::<_, i64>(WRITE_NATIVE_CLEAR_BOUNDARY_SQL)
        .bind(channel_id)
        .bind(ticket)
        .fetch_one(&mut **tx)
        .await
        .map_err(|error| anyhow!("record native channel clear boundary failed: {error}"))?;
    Ok(NativeClearGeneration(generation))
}
