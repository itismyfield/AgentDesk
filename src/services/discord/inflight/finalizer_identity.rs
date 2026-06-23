use std::fs;
use std::path::Path;

use super::{InflightTurnState, lock_inflight_state_path, persist_under_lock};
use crate::services::provider::ProviderKind;

pub(super) fn parse_inflight_state_content(content: &str) -> serde_json::Result<InflightTurnState> {
    parse_inflight_state_content_with_finalizer_backfill(content).map(|(state, _)| state)
}

pub(super) fn parse_inflight_state_content_with_finalizer_backfill(
    content: &str,
) -> serde_json::Result<(InflightTurnState, bool)> {
    let mut state = serde_json::from_str::<InflightTurnState>(content)?;
    if state.runtime_kind.is_none()
        && let Ok(raw_value) = serde_json::from_str::<serde_json::Value>(content)
        && let Some(raw_runtime) = raw_value.get("runtime_kind")
        && let Some(raw_str) = raw_runtime.as_str()
        && !raw_str.is_empty()
        && !matches!(
            raw_str,
            "legacy_tmux_wrapper" | "claude_tui" | "codex_tui" | "process_backend"
        )
    {
        state.runtime_kind_unknown_on_disk = true;
    }
    let finalizer_backfilled = state.ensure_finalizer_turn_id();
    Ok((state, finalizer_backfilled))
}

pub(super) fn read_inflight_state_content(path: &Path) -> Option<InflightTurnState> {
    let content = fs::read_to_string(path).ok()?;
    parse_inflight_state_content(&content).ok()
}

pub(super) fn backfill_finalizer_turn_id_under_lock(
    root: &Path,
    path: &Path,
    provider: &ProviderKind,
) -> Option<InflightTurnState> {
    let Ok(_lock) = lock_inflight_state_path(path) else {
        return None;
    };
    let content = fs::read_to_string(path).ok()?;
    let (state, backfilled) =
        parse_inflight_state_content_with_finalizer_backfill(&content).ok()?;
    if backfilled && state.provider_kind().as_ref() == Some(provider) {
        let _ = persist_under_lock(
            root,
            path,
            &state,
            "src/services/discord/inflight/finalizer_identity.rs:backfill_finalizer_turn_id_under_lock",
        );
    }
    Some(state)
}
