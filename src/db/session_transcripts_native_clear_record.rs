//! The native clear correlation as stored, whatever its state, so a cutoff is never forgotten
//! unread.

use super::{NativeClearGeneration, NativeClearStateRow, Result};
use sqlx::PgPool;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NativeClearRecord {
    pub(crate) generation: NativeClearGeneration,
    pub(crate) ticket: Option<serde_json::Value>,
    pub(crate) resolved: bool,
    /// A later boundary write replaced this generation.
    pub(crate) superseded: bool,
    pub(crate) after_frontier: bool,
}

pub(crate) async fn native_channel_clear_record(
    pool: &PgPool,
    channel_id: &str,
) -> Result<Option<NativeClearRecord>> {
    Ok(from_row(super::native_clear_row(pool, channel_id).await?))
}

pub(super) fn from_row(row: Option<NativeClearStateRow>) -> Option<NativeClearRecord> {
    let (clear_generation, native_generation, ticket, resolved, after_frontier) = row?;
    let native_generation = native_generation?;
    Some(NativeClearRecord {
        generation: NativeClearGeneration(native_generation),
        ticket,
        resolved,
        superseded: clear_generation != native_generation,
        after_frontier,
    })
}
