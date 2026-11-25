//! Who may end an inflight row: the one durable predicate every generic end consults.

use uuid::Uuid;

use super::{AgyCustodyField, AgyCustodyRoute, InflightTurnState};
use crate::services::provider::ProviderKind;
use crate::services::tui_o::store::agy_delivery::AgyOriginal;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum EndAuthority {
    /// No custody route: the existing recovery ends the row.
    Legacy,
    /// A route naming this exact execution: only the AGY owner ends it.
    Agy,
    /// A route this binary cannot read, or one that names another execution: shown, never ended.
    Held,
}

pub(in crate::services::discord) fn end_authority(state: &InflightTurnState) -> EndAuthority {
    let route = match &state.agy_custody {
        None => return EndAuthority::Legacy,
        Some(AgyCustodyField::Unreadable(_)) => return EndAuthority::Held,
        Some(AgyCustodyField::Route(route)) => route,
    };
    let nonce = state
        .turn_nonce
        .as_deref()
        .and_then(|nonce| Uuid::parse_str(nonce).ok());
    let original = match &route.original {
        AgyOriginal::Discord { message } => *message == route.message,
        AgyOriginal::Imessage { digest } => {
            state.agy_imessage_source_sha256.as_deref() == Some(digest.as_str())
        }
    };
    let names_this_execution = state.provider_kind() == Some(ProviderKind::Antigravity)
        && route.destination == state.channel_id
        && route.message == state.user_msg_id
        && state.logical_channel_id == Some(route.logical_channel)
        && nonce == Some(route.nonce)
        && original;
    if names_this_execution {
        EndAuthority::Agy
    } else {
        EndAuthority::Held
    }
}

pub(in crate::services::discord) fn generic_may_end(state: &InflightTurnState) -> bool {
    end_authority(state) == EndAuthority::Legacy
}

/// Row bytes a generic end must keep: a row that is not Legacy, or damaged bytes naming custody.
pub(super) fn generic_end_keeps(content: &str) -> bool {
    match serde_json::from_str::<InflightTurnState>(content) {
        Ok(state) => !generic_may_end(&state),
        Err(_) => raw_names_custody(content),
    }
}

/// Damaged bytes that name a custody route may be an AGY admission, so they are kept, not hidden.
pub(super) fn raw_names_custody(content: &str) -> bool {
    match serde_json::from_str::<serde_json::Value>(content) {
        Ok(value) => value
            .as_object()
            .is_some_and(|object| object.contains_key("agy_custody")),
        Err(_) => content.contains("\"agy_custody\""),
    }
}

/// A row in AGY custody as the boot owner finds it: its own route, or a row it may only hold.
#[cfg(unix)]
pub(in crate::services::discord) enum CustodyRow {
    Owned(AgyCustodyRoute),
    Held(u64),
}

/// Every row that names a custody route, whatever the current writer selection is.
#[cfg(unix)]
pub(in crate::services::discord) fn custody_rows(
    provider: &ProviderKind,
) -> Result<Vec<CustodyRow>, String> {
    let root = super::inflight_runtime_root().ok_or("inflight root unavailable")?;
    let dir = super::store::inflight_provider_dir(&root, provider);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.to_string()),
    };
    let mut rows = Vec::new();
    for entry in entries {
        let path = entry.map_err(|error| error.to_string())?.path();
        let Some(channel) = path
            .file_stem()
            .and_then(|stem| stem.to_str()?.parse().ok())
        else {
            continue;
        };
        let content = match std::fs::read_to_string(&path) {
            Ok(content) if raw_names_custody(&content) => content,
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.to_string()),
        };
        let state = serde_json::from_str::<InflightTurnState>(&content).ok();
        let owned = state.filter(|state| end_authority(state) == EndAuthority::Agy);
        rows.push(match owned.and_then(|state| state.agy_custody) {
            Some(AgyCustodyField::Route(route)) => CustodyRow::Owned(route),
            _ => CustodyRow::Held(channel),
        });
    }
    Ok(rows)
}

/// Runs `clear` as the AGY owner when `row`'s route names this exact execution and `nonce`, so the
/// custody guard lets that one row's delete through; any other row stays kept.
pub(in crate::services::discord) fn as_route_owner<T>(
    provider: &ProviderKind,
    row: &InflightTurnState,
    nonce: Uuid,
    clear: impl FnOnce() -> T,
) -> T {
    let owned = end_authority(row) == EndAuthority::Agy
        && matches!(&row.agy_custody, Some(AgyCustodyField::Route(route)) if route.nonce == nonce);
    let path = owned
        .then(super::inflight_runtime_root)
        .flatten()
        .map(|root| super::inflight_state_path(&root, provider, row.channel_id));
    super::row_removed::with_owner(path, clear)
}
