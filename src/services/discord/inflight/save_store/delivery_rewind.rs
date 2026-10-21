//! Identity- and owner-guarded delivery-rewind save operations.

use super::*;

#[cfg(test)]
#[path = "delivery_rewind/tests.rs"]
mod tests;

/// Saves a rewind only onto the episode `captured_nonce` names; the bridge passes the nonce it
/// captured once for the whole rewind so the reloads that follow compare against the same value.
pub(in crate::services::discord) fn save_inflight_delivery_rewind_if_matches_identity(
    state: &InflightTurnState,
    captured_nonce: Option<&str>,
    expected_owner: StreamRelayAuthority,
    reason: InflightDeliveryRewindReason,
) -> Result<bool, String> {
    let Some(root) = inflight_runtime_root() else {
        return Err("Home directory not found".to_string());
    };
    if !state.owned_by_turn_nonce(captured_nonce) {
        return Ok(false);
    }
    save_inflight_delivery_rewind_if_matches_identity_in_root(&root, state, expected_owner, reason)
}

pub(in crate::services::discord::inflight) fn save_inflight_delivery_rewind_if_matches_identity_in_root(
    root: &Path,
    state: &InflightTurnState,
    expected_owner: StreamRelayAuthority,
    reason: InflightDeliveryRewindReason,
) -> Result<bool, String> {
    let Some(provider) = state.provider_kind() else {
        return Err(format!("Unknown provider '{}'", state.provider));
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let _lock = lock_inflight_state_path(&path)?;
    let Some(on_disk) = load_inflight_state_unlocked(&path) else {
        return Ok(false);
    };
    let expected = InflightTurnIdentity::from_state(state);
    if !expected.matches_state(&on_disk) || on_disk.turn_nonce != state.turn_nonce {
        return Ok(false);
    }
    if on_disk.terminal_delivery_committed {
        return Ok(false);
    }
    if StreamRelayAuthority::from_state(&on_disk) != expected_owner {
        return Ok(false);
    }
    let mut updated = on_disk;
    updated.full_response = state.full_response.clone();
    updated.response_sent_offset = state.response_sent_offset;
    updated.terminal_delivery_committed = state.terminal_delivery_committed;
    updated.last_offset = updated.last_offset.max(state.last_offset);
    updated.set_relay_owner_kind(state.effective_relay_owner_kind());
    updated.ensure_finalizer_turn_id();
    if !validate_inflight_state_for_save_with_delivery_rewind_reason(
        root,
        &path,
        &updated,
        "src/services/discord/inflight.rs:save_inflight_delivery_rewind_if_matches_identity_in_root",
        Some(reason),
    ) {
        return Ok(false);
    }
    updated.updated_at = now_string();
    bump_save_generation_for_write(&path, &mut updated)?;
    let json = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
    atomic_write(&path, &json)?;
    Ok(true)
}
