use super::*;

pub(in crate::services::discord) fn save_inflight_state_if_identity_unchanged(
    state: &InflightTurnState,
    caller: &'static str,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    save_inflight_state_if_identity_unchanged_in_root(&root, state, caller)
}

pub(in crate::services::discord::inflight) fn save_inflight_state_if_identity_unchanged_in_root(
    root: &Path,
    state: &InflightTurnState,
    caller: &'static str,
) -> GuardedSaveOutcome {
    let Some(provider) = state.provider_kind() else {
        return GuardedSaveOutcome::IoError;
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return GuardedSaveOutcome::IoError;
        }
    }
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    let Ok(data) = fs::read_to_string(&path) else {
        tracing::debug!(
            provider = %provider.as_str(),
            channel = state.channel_id,
            caller = caller,
            snapshot_identity = ?InflightTurnIdentity::from_state(state),
            "inflight identity-refresh save skipped because durable row is missing"
        );
        return GuardedSaveOutcome::Missing;
    };
    let Ok(on_disk) = serde_json::from_str::<InflightTurnState>(&data) else {
        tracing::debug!(
            provider = %provider.as_str(),
            channel = state.channel_id,
            caller = caller,
            snapshot_identity = ?InflightTurnIdentity::from_state(state),
            "inflight identity-refresh save skipped because durable row is malformed"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    };
    let expected = InflightTurnIdentity::from_state(state);
    let durable = InflightTurnIdentity::from_state(&on_disk);
    if state.user_msg_id == 0 && state.turn_start_offset.is_none() {
        tracing::info!(
            provider = %provider.as_str(),
            channel = state.channel_id,
            caller = caller,
            snapshot_identity = ?expected,
            durable_identity = ?durable,
            snapshot_turn_start_offset = ?state.turn_start_offset,
            durable_turn_start_offset = ?on_disk.turn_start_offset,
            "inflight identity-refresh save skipped because offsetless id-0 snapshot cannot safely match a durable row"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if on_disk.output_path != state.output_path {
        tracing::info!(
            provider = %provider.as_str(),
            channel = state.channel_id,
            caller = caller,
            snapshot_identity = ?expected,
            durable_identity = ?durable,
            snapshot_output_path = ?state.output_path.as_deref(),
            durable_output_path = ?on_disk.output_path.as_deref(),
            durable_restart_mode = ?on_disk.restart_mode,
            durable_rebind_origin = on_disk.rebind_origin,
            "inflight identity-refresh save skipped because durable row output path changed"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if on_disk.restart_mode.is_some() || on_disk.rebind_origin || !expected.matches_state(&on_disk)
    {
        tracing::info!(
            provider = %provider.as_str(),
            channel = state.channel_id,
            caller = caller,
            snapshot_identity = ?expected,
            durable_identity = ?durable,
            durable_restart_mode = ?on_disk.restart_mode,
            durable_rebind_origin = on_disk.rebind_origin,
            "inflight identity-refresh save skipped because durable row authority changed"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }

    let mut updated = state.clone();
    updated.ensure_finalizer_turn_id();
    if !validate_inflight_state_for_save(
        root,
        &path,
        &updated,
        "src/services/discord/inflight.rs:save_inflight_state_if_identity_unchanged_in_root",
    ) {
        tracing::info!(
            provider = %provider.as_str(),
            channel = state.channel_id,
            caller = caller,
            snapshot_identity = ?expected,
            durable_identity = ?durable,
            "inflight identity-refresh save skipped because validation rejected the refreshed write"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }
    updated.updated_at = now_string();
    bump_save_generation_for_write(&path, &mut updated);
    let Ok(json) = serde_json::to_string_pretty(&updated) else {
        return GuardedSaveOutcome::IoError;
    };
    match atomic_write(&path, &json) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = state.channel_id,
                caller = caller,
                snapshot_identity = ?expected,
                error = %error,
                "inflight identity-refresh save failed; leaving durable row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

/// #4185: after restart-cancel, the broad identity-refresh save intentionally
/// refuses restart-marked rows. This narrow RMW keeps that guard intact and
/// patches only the cleaned terminal `full_response` back onto the same
/// restart-preserved row.
pub(in crate::services::discord) fn patch_restart_full_response_if_identity_unchanged(
    state: &InflightTurnState,
    caller: &'static str,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    patch_restart_full_response_if_identity_unchanged_in_root(&root, state, caller)
}

pub(in crate::services::discord::inflight) fn patch_restart_full_response_if_identity_unchanged_in_root(
    root: &Path,
    state: &InflightTurnState,
    caller: &'static str,
) -> GuardedSaveOutcome {
    let Some(provider) = state.provider_kind() else {
        return GuardedSaveOutcome::IoError;
    };
    if state.restart_mode.is_none() {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return GuardedSaveOutcome::IoError;
    }
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return GuardedSaveOutcome::Missing;
    };
    let Ok(mut on_disk) = serde_json::from_str::<InflightTurnState>(&data) else {
        return GuardedSaveOutcome::IdentityMismatch;
    };
    let expected = InflightTurnIdentity::from_state(state);
    let durable = InflightTurnIdentity::from_state(&on_disk);
    if state.user_msg_id == 0 && state.turn_start_offset.is_none() {
        tracing::info!(
            provider = %provider.as_str(),
            channel = state.channel_id,
            caller = caller,
            snapshot_identity = ?expected,
            durable_identity = ?durable,
            "restart-preserved full_response patch skipped because offsetless id-0 snapshot cannot safely match a durable row"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if !expected.matches_state(&on_disk)
        || on_disk.restart_mode.is_none()
        || on_disk.restart_mode != state.restart_mode
        || on_disk.restart_generation != state.restart_generation
        || on_disk.rebind_origin
        || on_disk.output_path != state.output_path
    {
        tracing::info!(
            provider = %provider.as_str(),
            channel = state.channel_id,
            caller = caller,
            snapshot_identity = ?expected,
            durable_identity = ?durable,
            snapshot_restart_mode = ?state.restart_mode,
            durable_restart_mode = ?on_disk.restart_mode,
            snapshot_restart_generation = ?state.restart_generation,
            durable_restart_generation = ?on_disk.restart_generation,
            durable_rebind_origin = on_disk.rebind_origin,
            snapshot_output_path = ?state.output_path.as_deref(),
            durable_output_path = ?on_disk.output_path.as_deref(),
            "restart-preserved full_response patch skipped because durable row is not the same restart-preserved turn"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }

    let response_sent_offset = on_disk.response_sent_offset;
    if response_sent_offset > state.full_response.len()
        || !state.full_response.is_char_boundary(response_sent_offset)
    {
        tracing::info!(
            provider = %provider.as_str(),
            channel = state.channel_id,
            caller = caller,
            response_sent_offset = response_sent_offset,
            full_response_len = state.full_response.len(),
            "restart-preserved full_response patch skipped because the existing response offset would become invalid"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if response_sent_offset > 0
        && on_disk.full_response.as_bytes().get(..response_sent_offset)
            != Some(&state.full_response.as_bytes()[..response_sent_offset])
    {
        tracing::info!(
            provider = %provider.as_str(),
            channel = state.channel_id,
            caller = caller,
            response_sent_offset = response_sent_offset,
            raw_full_response_len = on_disk.full_response.len(),
            cleaned_full_response_len = state.full_response.len(),
            "already-relayed prefix diverges after API_FRICTION cleaning; keeping raw text to preserve resume-offset semantics"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }

    on_disk.full_response = state.full_response.clone();
    match persist_under_lock(
        root,
        &path,
        &on_disk,
        "src/services/discord/inflight.rs:patch_restart_full_response_if_identity_unchanged_in_root",
    ) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = state.channel_id,
                caller = caller,
                error = %error,
                "restart-preserved full_response patch failed; leaving durable row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

pub(in crate::services::discord) fn save_inflight_delivery_rewind_if_matches_identity(
    state: &InflightTurnState,
    reason: InflightDeliveryRewindReason,
) -> Result<bool, String> {
    let Some(root) = inflight_runtime_root() else {
        return Err("Home directory not found".to_string());
    };
    save_inflight_delivery_rewind_if_matches_identity_in_root(&root, state, reason)
}

pub(in crate::services::discord::inflight) fn save_inflight_delivery_rewind_if_matches_identity_in_root(
    root: &Path,
    state: &InflightTurnState,
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
    if !expected.matches_state(&on_disk) {
        return Ok(false);
    }
    if on_disk.terminal_delivery_committed {
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
    bump_save_generation_for_write(&path, &mut updated);
    let json = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
    atomic_write(&path, &json)?;
    Ok(true)
}

/// Outcome of [`save_inflight_state_if_matches_identity`] — the #3041 P1-2 R3
/// identity-guarded re-save used on a delivery-lease `Skip` epilogue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum GuardedSaveOutcome {
    /// On-disk row still matched the turn identity; the row was rewritten.
    Saved,
    /// No inflight row existed (the lease HOLDER already cleared it on its
    /// success path). We do NOT resurrect it — the turn is already delivered.
    Missing,
    /// A row existed but its identity did NOT match (a newer turn replaced it,
    /// or a planned-restart / rebind-origin marker now owns the row). We do
    /// NOT clobber it.
    IdentityMismatch,
    /// Filesystem / serialization error during the write.
    IoError,
}

fn identity_matches_with_offset_guard(
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    state: &InflightTurnState,
) -> bool {
    if !expected.matches_state(state) {
        return false;
    }
    // Anchor bind/reuse reads or rewrites another persisted row. For synthetic
    // id-0 rows, fail closed unless BOTH sides carry the birth offset and it
    // matches. This is stricter than the delivery-lease id-0 degenerate fallback,
    // which is transport-level dedup only and never authorizes row mutation.
    if expected.user_msg_id == 0 {
        return matches!(
            (expected_turn_start_offset, state.turn_start_offset),
            (Some(expected_offset), Some(actual_offset)) if expected_offset == actual_offset
        );
    }
    if let Some(expected_offset) = expected_turn_start_offset {
        state.turn_start_offset == Some(expected_offset)
    } else {
        true
    }
}

pub(in crate::services::discord) fn recovery_anchor_msg_id_if_matches_identity(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> Option<u64> {
    let root = inflight_runtime_root()?;
    let path = inflight_state_path(&root, provider, channel_id);
    let _lock = lock_inflight_state_path(&path).ok()?;
    let data = fs::read_to_string(&path).ok()?;
    let state = serde_json::from_str::<InflightTurnState>(&data).ok()?;
    if !identity_matches_with_offset_guard(expected, expected_turn_start_offset, &state) {
        return None;
    }
    (state.current_msg_id != 0).then_some(state.current_msg_id)
}

pub(in crate::services::discord) fn bind_recovery_anchor_if_matches_identity(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    expected_current_msg_id: u64,
    anchor_msg_id: u64,
    anchor_text_len: usize,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    let path = inflight_state_path(&root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    let data = match fs::read_to_string(&path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return GuardedSaveOutcome::Missing;
        }
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                error = %error,
                "inflight recovery anchor bind could not read on-disk row; blocking durable anchor write"
            );
            return GuardedSaveOutcome::IoError;
        }
    };
    let Ok(mut on_disk) = serde_json::from_str::<InflightTurnState>(&data) else {
        return GuardedSaveOutcome::IdentityMismatch;
    };
    if !identity_matches_with_offset_guard(expected, expected_turn_start_offset, &on_disk) {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if on_disk.current_msg_id != expected_current_msg_id {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    on_disk.current_msg_id = anchor_msg_id;
    on_disk.current_msg_len = anchor_text_len;
    on_disk.ensure_finalizer_turn_id();
    on_disk.updated_at = now_string();
    bump_save_generation_for_write(&path, &mut on_disk);
    let Ok(json) = serde_json::to_string_pretty(&on_disk) else {
        return GuardedSaveOutcome::IoError;
    };
    match atomic_write(&path, &json) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                error = %error,
                "inflight recovery anchor bind failed; leaving on-disk row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

pub(in crate::services::discord) fn persist_leak_recovery_response_offset_if_matches_identity_locked(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_current_msg_id: u64,
    delivered_offset: usize,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    persist_leak_recovery_response_offset_if_matches_identity_locked_in_root(
        &root,
        provider,
        channel_id,
        expected,
        expected_current_msg_id,
        delivered_offset,
    )
}

pub(in crate::services::discord::inflight) fn persist_leak_recovery_response_offset_if_matches_identity_locked_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    expected_current_msg_id: u64,
    delivered_offset: usize,
) -> GuardedSaveOutcome {
    let path = inflight_state_path(root, provider, channel_id);
    if let Some(parent) = path.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return GuardedSaveOutcome::IoError;
    }
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    let Some(mut on_disk) = load_inflight_state_unlocked(&path) else {
        return GuardedSaveOutcome::Missing;
    };
    if !expected.matches_state(&on_disk) || on_disk.current_msg_id != expected_current_msg_id {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if on_disk.response_sent_offset >= delivered_offset {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if delivered_offset > on_disk.full_response.len()
        || !on_disk.full_response.is_char_boundary(delivered_offset)
    {
        return GuardedSaveOutcome::IdentityMismatch;
    }

    on_disk.response_sent_offset = delivered_offset;
    match persist_under_lock(
        root,
        &path,
        &on_disk,
        "src/services/discord/inflight.rs:persist_leak_recovery_response_offset_if_matches_identity_locked_in_root",
    ) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "leak recovery offset patch failed; leaving on-disk row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

pub(in crate::services::discord) fn persist_recovery_output_path_if_matches_identity_locked(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    output_path: String,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    persist_recovery_output_path_if_matches_identity_locked_in_root(
        &root,
        provider,
        channel_id,
        expected,
        output_path,
    )
}

pub(in crate::services::discord::inflight) fn persist_recovery_output_path_if_matches_identity_locked_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    output_path: String,
) -> GuardedSaveOutcome {
    let path = inflight_state_path(root, provider, channel_id);
    if let Some(parent) = path.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return GuardedSaveOutcome::IoError;
    }
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    let Some(mut on_disk) = load_inflight_state_unlocked(&path) else {
        return GuardedSaveOutcome::Missing;
    };
    if !expected.matches_state(&on_disk) {
        return GuardedSaveOutcome::IdentityMismatch;
    }

    on_disk.output_path = Some(output_path);
    match persist_under_lock(
        root,
        &path,
        &on_disk,
        "src/services/discord/inflight.rs:persist_recovery_output_path_if_matches_identity_locked_in_root",
    ) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "recovery output-path patch failed; leaving on-disk row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

/// #3041 P1-2 (codex P1-2 R3): identity-guarded re-save for the bridge's
/// delivery-lease `Skip` epilogue. On a Skip the live HOLDER (watcher) owns the
/// turn and CLEARS the row on success, so the bridge epilogue must NOT blindly
/// `save_inflight_state`: if the holder's clear won the race, a blind re-save
/// would resurrect a STALE row for an already-delivered turn (recovery then sees
/// it delivered, never clears, leaks the row). This closes the window the same
/// way `clear_inflight_state_if_matches` (#2427 D-wire) does: under the lock,
/// write only when the row is STILL present AND its `(user_msg_id, started_at,
/// tmux_session_name)` identity (+ `turn_start_offset` when known) matches. Gone
/// (`Missing`) or replaced by a newer turn / restart-rebind marker
/// (`IdentityMismatch`) → no-op; holder FAILED + didn't clear → still present &
/// matching → refresh (`Saved`). Same flock + atomic_write primitives as the
/// rest of the module (Windows-safe).
pub(in crate::services::discord) fn save_inflight_state_if_matches_identity(
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    save_inflight_state_if_matches_identity_in_root(
        &root,
        state,
        expected,
        expected_turn_start_offset,
    )
}

pub(in crate::services::discord) fn save_existing_inflight_rebind_adoption_if_matches_identity(
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    save_existing_inflight_rebind_adoption_if_matches_identity_in_root(
        &root,
        state,
        expected,
        expected_turn_start_offset,
    )
}

pub(in crate::services::discord) fn save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity(
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    expected_last_offset: u64,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity_in_root(
        &root,
        state,
        expected,
        expected_turn_start_offset,
        expected_last_offset,
    )
}

pub(in crate::services::discord::inflight) fn save_existing_inflight_rebind_adoption_if_matches_identity_in_root(
    root: &Path,
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    save_existing_inflight_rebind_adoption_impl_in_root(
        root,
        state,
        expected,
        expected_turn_start_offset,
        None,
    )
}

pub(in crate::services::discord::inflight) fn save_existing_inflight_rebind_adoption_with_offset_rebase_if_matches_identity_in_root(
    root: &Path,
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    expected_last_offset: u64,
) -> GuardedSaveOutcome {
    save_existing_inflight_rebind_adoption_impl_in_root(
        root,
        state,
        expected,
        expected_turn_start_offset,
        Some(expected_last_offset),
    )
}

fn save_existing_inflight_rebind_adoption_impl_in_root(
    root: &Path,
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
    expected_last_offset_for_rebase: Option<u64>,
) -> GuardedSaveOutcome {
    let Some(provider) = state.provider_kind() else {
        return GuardedSaveOutcome::IoError;
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return GuardedSaveOutcome::IoError;
        }
    }
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return GuardedSaveOutcome::Missing;
    };
    let Ok(on_disk) = serde_json::from_str::<InflightTurnState>(&data) else {
        return GuardedSaveOutcome::IdentityMismatch;
    };
    if on_disk.rebind_origin {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if on_disk.restart_mode != state.restart_mode {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if expected.user_msg_id == 0 || !expected.matches_state(&on_disk) {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if let Some(expected_offset) = expected_turn_start_offset {
        if on_disk.turn_start_offset != Some(expected_offset) {
            return GuardedSaveOutcome::IdentityMismatch;
        }
    }
    if expected_last_offset_for_rebase
        .is_some_and(|expected_last| on_disk.last_offset != expected_last)
    {
        return GuardedSaveOutcome::IdentityMismatch;
    }

    let mut updated = on_disk;
    updated.tmux_session_name = state.tmux_session_name.clone();
    updated.output_path = state.output_path.clone();
    updated.input_fifo_path = state.input_fifo_path.clone();
    updated.runtime_kind = state.runtime_kind;
    updated.session_id = state.session_id.clone();
    updated.set_relay_owner_kind(state.effective_relay_owner_kind());
    if expected_last_offset_for_rebase.is_some() {
        updated.last_offset = state.last_offset;
        updated.turn_start_offset = state.turn_start_offset;
        updated.last_watcher_relayed_offset = state.last_watcher_relayed_offset;
        updated.last_watcher_relayed_generation_mtime_ns =
            state.last_watcher_relayed_generation_mtime_ns;
    }
    updated.ensure_finalizer_turn_id();
    let _ = validate_inflight_state_for_save(
        root,
        &path,
        &updated,
        "src/services/discord/inflight.rs:save_existing_inflight_rebind_adoption_if_matches_identity_in_root",
    );
    updated.updated_at = now_string();
    bump_save_generation_for_write(&path, &mut updated);
    let Ok(json) = serde_json::to_string_pretty(&updated) else {
        return GuardedSaveOutcome::IoError;
    };
    match atomic_write(&path, &json) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = state.channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "existing inflight rebind adoption save failed; leaving on-disk row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

/// Root-explicit inner form of [`save_inflight_state_if_matches_identity`] for
/// unit tests (avoids `AGENTDESK_ROOT_DIR` env-var races).
pub(in crate::services::discord::inflight) fn save_inflight_state_if_matches_identity_in_root(
    root: &Path,
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_turn_start_offset: Option<u64>,
) -> GuardedSaveOutcome {
    let Some(provider) = state.provider_kind() else {
        return GuardedSaveOutcome::IoError;
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return GuardedSaveOutcome::IoError;
        }
    }
    // Hold the sidecar flock across the read AND the write so a concurrent
    // holder `clear_inflight_state` (which takes the same lock) cannot land its
    // remove between our identity check and our write.
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    // Holder already cleared the row on its success path → do NOT resurrect.
    let Ok(data) = fs::read_to_string(&path) else {
        return GuardedSaveOutcome::Missing;
    };
    let Ok(on_disk) = serde_json::from_str::<InflightTurnState>(&data) else {
        // Malformed row: treat like a mismatch and do not clobber — the loader
        // eviction path GCs malformed payloads on the next read.
        return GuardedSaveOutcome::IdentityMismatch;
    };
    // A newer turn (different identity) or a planned-restart / rebind-origin
    // marker now owns the row — never overwrite it with this preserved turn.
    if on_disk.restart_mode.is_some() || on_disk.rebind_origin {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if expected.user_msg_id == 0 || !expected.matches_state(&on_disk) {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if let Some(expected_offset) = expected_turn_start_offset {
        if on_disk.turn_start_offset != Some(expected_offset) {
            return GuardedSaveOutcome::IdentityMismatch;
        }
    }
    if on_disk.output_path != state.output_path {
        tracing::info!(
            provider = %provider.as_str(),
            channel = state.channel_id,
            snapshot_identity = ?expected,
            durable_identity = ?InflightTurnIdentity::from_state(&on_disk),
            snapshot_output_path = ?state.output_path.as_deref(),
            durable_output_path = ?on_disk.output_path.as_deref(),
            durable_restart_mode = ?on_disk.restart_mode,
            durable_rebind_origin = on_disk.rebind_origin,
            "inflight identity-guarded save skipped because durable row output path changed"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }
    // #3089 B3: verdict observe-only here — this path already identity/offset-
    // gates above; the #3416 backward vector is the plain overwrite tails.
    let mut updated = state.clone();
    updated.ensure_finalizer_turn_id();
    let _ = validate_inflight_state_for_save(
        root,
        &path,
        &updated,
        "src/services/discord/inflight.rs:save_inflight_state_if_matches_identity_in_root",
    );
    updated.updated_at = now_string();
    bump_save_generation_for_write(&path, &mut updated);
    let Ok(json) = serde_json::to_string_pretty(&updated) else {
        return GuardedSaveOutcome::IoError;
    };
    match atomic_write(&path, &json) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel = state.channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "inflight identity-guarded save failed; leaving on-disk row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}
