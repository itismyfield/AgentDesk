//! Sessions-row lookup for a caller that holds no bot token hash or no channel name: every
//! hash registered for the provider, each tried under exact keys only, never a name match.

use serde_json::Value;
use sqlx::PgPool;

use super::health::HealthRegistry;
use super::inflight::{KeyedTeardown, teardown_for_lookup};
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityConflictKind, SessionIdentityKind,
    resolve_session_row_pg,
};
use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedLookupKey, load_hosted_execution_pg,
};
use crate::services::provider::ProviderKind;
use crate::services::session_host::HostLiveness;

/// The turn channel a routine run recorded: a decimal string as the executor writes it.
pub(crate) fn recorded_channel_id(value: Option<&Value>) -> Option<u64> {
    let value = value?;
    match value.as_str() {
        Some(text) => text.parse().ok(),
        None => value.as_u64(),
    }
}

/// Reads each hash's namespaced key and its `(provider, hash, channel)` tuple, then merges.
pub(super) async fn derive_hosted_lookup(
    pool: &PgPool,
    hashes: &[String],
    provider: &ProviderKind,
    channel_id: u64,
    tmux_name: &str,
) -> HostedLookup {
    if hashes.is_empty() {
        return HostedLookup::Unknown("no registered bot hash".to_string());
    }
    if tmux_name.trim().is_empty() {
        return HostedLookup::Unknown("empty tmux name".to_string());
    }
    let channel = channel_id.to_string();
    let mut lookups = Vec::with_capacity(hashes.len() * 2);
    for hash in hashes {
        let key = super::adk_session::build_namespaced_session_key(hash, provider, tmux_name);
        lookups.push(load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(&key)).await);
        let identity = CanonicalSessionIdentity {
            kind: SessionIdentityKind::DiscordChannel,
            discord_token_hash: hash,
            channel_id: &channel,
        };
        let canonical = HostedLookupKey::Canonical {
            provider: provider.as_str(),
            identity,
        };
        lookups.push(load_hosted_execution_pg(pool, canonical).await);
    }
    merge_lookups(lookups)
}

/// A conflict, then a failed read, decides first: a failed candidate may hide another row.
/// Found rows must be one row read identically; only all-missing is Missing.
pub(super) fn merge_lookups(lookups: Vec<HostedLookup>) -> HostedLookup {
    let decisive = lookups
        .iter()
        .find(|lookup| matches!(lookup, HostedLookup::Conflict(_)))
        .or_else(|| {
            lookups
                .iter()
                .find(|lookup| matches!(lookup, HostedLookup::Unknown(_)))
        });
    if let Some(decisive) = decisive {
        return decisive.clone();
    }
    let found: Vec<_> = lookups
        .into_iter()
        .filter_map(|lookup| match lookup {
            HostedLookup::Found(observation) => Some(observation),
            _ => None,
        })
        .collect();
    let Some(first) = found.first() else {
        return HostedLookup::Missing;
    };
    if found.iter().any(|o| o.session_id() != first.session_id()) {
        return HostedLookup::Conflict(SessionIdentityConflictKind::EvidenceDivergence);
    }
    if found.iter().any(|o| o != first) {
        return HostedLookup::Unknown("record changed between candidate reads".to_string());
    }
    HostedLookup::Found(first.clone())
}

/// The tmux name the one `(provider, hash, channel)` row behind `hashes` records under its
/// own key, for a caller with no channel name. No row, a failed read or two rows answer
/// instead; no row is Missing, which a nameless caller never reads as a legacy answer.
pub(super) async fn derive_channel_session_name(
    pool: &PgPool,
    hashes: &[String],
    provider: &ProviderKind,
    channel_id: u64,
) -> Result<String, HostedLookup> {
    let hashes: Vec<&String> = hashes
        .iter()
        .filter(|hash| !hash.trim().is_empty())
        .collect();
    if hashes.is_empty() {
        return Err(HostedLookup::Unknown("no registered bot hash".to_string()));
    }
    if channel_id == 0 {
        return Err(HostedLookup::Unknown("no channel id".to_string()));
    }
    let channel = channel_id.to_string();
    let (mut rows, mut failed) = (Vec::new(), Vec::new());
    for hash in hashes {
        let identity = CanonicalSessionIdentity {
            kind: SessionIdentityKind::DiscordChannel,
            discord_token_hash: hash,
            channel_id: &channel,
        };
        match resolve_session_row_pg(pool, None, Some(provider.as_str()), Some(identity)).await {
            Ok(Some(row)) => rows.push(row),
            Ok(None) => {}
            Err(error) => failed.push(error.conflict_kind().map_or_else(
                || HostedLookup::Unknown(format!("{error:?}")),
                HostedLookup::Conflict,
            )),
        }
    }
    let failed = merge_lookups(failed);
    if failed != HostedLookup::Missing {
        return Err(failed);
    }
    let Some((id, key)) = rows.first() else {
        return Err(HostedLookup::Missing);
    };
    if rows.iter().any(|(other, _)| other != id) {
        return Err(HostedLookup::Conflict(
            SessionIdentityConflictKind::EvidenceDivergence,
        ));
    }
    super::session_identity::tmux_name_from_session_key(key)
        .ok_or_else(|| HostedLookup::Unknown(format!("row key {key} names no tmux session")))
}

/// Whether a routine probe's failure on `tmux_name` may stand: the row behind a registered
/// hash is a found legacy row and no marker or inflight row names another host.
pub(crate) async fn routine_session_failure_admitted(
    registry: Option<&HealthRegistry>,
    pool: &PgPool,
    provider: &ProviderKind,
    channel_id: Option<u64>,
    tmux_name: &str,
    observed: HostLiveness,
) -> bool {
    // The inflight row is keyed by channel, so without one its host evidence is unread.
    let Some(channel_id) = channel_id else {
        tracing::warn!(
            tmux_name,
            "routine run has no turn channel; failure deferred"
        );
        return false;
    };
    let hashes = match registry {
        Some(registry) => registry.registered_token_hashes(provider).await,
        None => Vec::new(),
    };
    let lookup = derive_hosted_lookup(pool, &hashes, provider, channel_id, tmux_name).await;
    let teardown = teardown_for_lookup(
        lookup,
        provider,
        channel_id,
        None,
        tmux_name,
        Some(observed),
        "routine_fresh_session_probe",
    );
    matches!(teardown, KeyedTeardown::Cleared(_))
}

#[cfg(test)]
#[path = "host_key_derivation_tests.rs"]
pub(crate) mod tests;
