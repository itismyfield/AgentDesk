//! Who may end an inflight row: the one durable predicate every generic end consults.

use uuid::Uuid;

use super::{AgyCustodyField, InflightTurnState};
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
