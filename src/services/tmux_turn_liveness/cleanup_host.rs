//! Host check HTTP kill-tmux and idle cleanup run before their first probe or write.
//! Only one found sessions row with no hosted record and no non-tmux marker is legacy.

use sqlx::PgPool;

use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::{
    CleanupRow, HostedLookup, HostedLookupKey, HostedRecord, load_cleanup_row_pg,
    load_hosted_execution_pg,
};
use crate::domain::host_observation::{HostKind, HostKindMarker};
use crate::domain::session_identity::tmux_name_from_session_key;
use crate::services::tmux_common::host_marker::read_host_kind_marker;

/// Legacy rows also require every locator's marker to be absent or tmux.
pub(crate) fn cleanup_row_deletable(row: &CleanupRow) -> bool {
    row.record_deletable()
        && (!row.is_legacy()
            || row.locator_keys().all(|key| {
                tmux_name_from_session_key(key).is_none_or(|name| {
                    matches!(
                        read_host_kind_marker(&name),
                        HostKindMarker::Absent | HostKindMarker::Known(HostKind::Tmux)
                    )
                })
            }))
}

/// Check the hosted record and marker before querying session occupancy.
pub(crate) async fn cleanup_row_is_unoccupied_pg(
    pool: &PgPool,
    key: &str,
    row: &CleanupRow,
) -> bool {
    cleanup_row_deletable(row) && super::idle_cleanup_session_is_unoccupied(pool, key).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CleanupHostRefusal {
    /// No sessions row: never read as a legacy tmux session.
    RowMissing,
    LookupFailed,
    RowConflict,
    /// A hosted record of any state, an unreadable one, or a non-tmux host marker.
    NotLegacyTmux,
}

impl CleanupHostRefusal {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::RowMissing => "host_row_missing",
            Self::LookupFailed => "host_lookup_failed",
            Self::RowConflict => "host_row_conflict",
            Self::NotLegacyTmux => "host_not_legacy_tmux",
        }
    }
}

/// `Ok(None)` only when no row matches; a found row must be legacy tmux.
async fn legacy_row_if_found_pg(
    pool: &PgPool,
    key: HostedLookupKey<'_>,
) -> Result<Option<i64>, CleanupHostRefusal> {
    let session_id = match load_hosted_execution_pg(pool, key).await {
        HostedLookup::Found(found) if found.record == HostedRecord::Legacy => found.session_id(),
        HostedLookup::Found(_) => return Err(CleanupHostRefusal::NotLegacyTmux),
        HostedLookup::Missing => return Ok(None),
        HostedLookup::Unknown(_) => return Err(CleanupHostRefusal::LookupFailed),
        HostedLookup::Conflict(_) => return Err(CleanupHostRefusal::RowConflict),
    };
    // The record is read again with every locator's marker, as ordinary cleanup reads it.
    match load_cleanup_row_pg(pool, session_id).await {
        Ok(Some(row)) if row.is_legacy() && cleanup_row_deletable(&row) => Ok(Some(session_id)),
        Ok(Some(_)) => Err(CleanupHostRefusal::NotLegacyTmux),
        Ok(None) => Ok(None),
        Err(_) => Err(CleanupHostRefusal::LookupFailed),
    }
}

/// Confirms the row a full primary or alias session key names is a legacy tmux row.
pub(crate) async fn confirm_legacy_tmux_key_pg(
    pool: &PgPool,
    session_key: &str,
) -> Result<(), CleanupHostRefusal> {
    legacy_row_if_found_pg(pool, HostedLookupKey::SessionKey(session_key))
        .await?
        .map(|_| ())
        .ok_or(CleanupHostRefusal::RowMissing)
}

/// Checks the channel's canonical row first; `session_key` names the tmux session only
/// after that, and its row must be the canonical one when both exist.
pub(crate) async fn confirm_legacy_tmux_channel_pg(
    pool: &PgPool,
    provider: &str,
    token_hash: &str,
    channel_id: &str,
    session_key: impl FnOnce() -> Option<String>,
) -> Result<String, CleanupHostRefusal> {
    let identity = CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: token_hash,
        channel_id,
    };
    let canonical =
        legacy_row_if_found_pg(pool, HostedLookupKey::Canonical { provider, identity }).await?;
    let session_key = session_key().ok_or(CleanupHostRefusal::RowMissing)?;
    let keyed = legacy_row_if_found_pg(pool, HostedLookupKey::SessionKey(&session_key))
        .await?
        .ok_or(CleanupHostRefusal::RowMissing)?;
    if canonical.is_some_and(|canonical| canonical != keyed) {
        return Err(CleanupHostRefusal::RowConflict);
    }
    Ok(session_key)
}
