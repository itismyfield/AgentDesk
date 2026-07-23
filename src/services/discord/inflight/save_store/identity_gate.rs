use super::*;
#[path = "identity_gate/claude_e_stamp.rs"]
mod claude_e_stamp;
#[path = "identity_gate/heartbeat.rs"]
mod heartbeat;
#[path = "identity_gate/runtime_stamp.rs"]
mod runtime_stamp;
#[path = "identity_gate/stream_loop_patch.rs"]
mod stream_loop_patch;
pub(in crate::services::discord) use claude_e_stamp::stamp_claude_e_process_if_matches_identity;
pub(in crate::services::discord) use heartbeat::touch_inflight_state_if_matches_identity;
pub(in crate::services::discord) use runtime_stamp::stamp_runtime_handoff_if_matches_identity;
pub(in crate::services::discord) use stream_loop_patch::{
    clear_long_running_placeholder_if_matches_identity, patch_restart_mode_if_matches_identity,
};

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
    save_inflight_state_identity_gated_in_root(
        root,
        state,
        &InflightTurnIdentity::from_state(state),
        caller,
        false,
    )
}

/// #4259: identity-guarded save for runtime-handoff restamps. It pins the
/// loaded turn identity (`user_msg_id`, `started_at`, `tmux_session_name`, and
/// `turn_start_offset`) plus the restart/rebind authority and id-0 offsetless
/// fail-closed gates of [`save_inflight_state_if_identity_unchanged`], while
/// allowing the written state to advance `output_path` and `turn_start_offset`.
///
/// Runtime-handoff stamps legitimately re-point the row at the output the live
/// runtime actually writes: a warm follow-up reuses a resolved legacy `/tmp`
/// session path that differs from the intake seed
/// (`resolve_session_temp_path`; claude.rs/codex.rs/qwen.rs follow-up arms), so
/// the strict `output_path` equality of the `_if_identity_unchanged` variant
/// would refuse a legitimate same-turn stamp. Use THIS variant for handoffs
/// whose session identity was already established before the mutation; it still
/// refuses rows re-owned by another turn. Handoffs that first-populate
/// `tmux_session_name` use the field-scoped `runtime_stamp` RMW instead. The
/// expected identity is captured before the caller mutates its restamp cursor,
/// so the durable row must still have the loaded birth offset; the written state
/// may then advance that cursor.
pub(in crate::services::discord) fn save_inflight_state_if_identity_matches_allow_output_restamp(
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    caller: &'static str,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    save_inflight_state_if_identity_matches_allow_output_restamp_in_root(
        &root, state, expected, caller,
    )
}

pub(in crate::services::discord::inflight) fn save_inflight_state_if_identity_matches_allow_output_restamp_in_root(
    root: &Path,
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    caller: &'static str,
) -> GuardedSaveOutcome {
    save_inflight_state_identity_gated_in_root(root, state, expected, caller, true)
}

fn save_inflight_state_identity_gated_in_root(
    root: &Path,
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    caller: &'static str,
    allow_output_restamp: bool,
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
            channel_id = state.channel_id,
            caller = caller,
            snapshot_identity = ?InflightTurnIdentity::from_state(state),
            "inflight identity-refresh save skipped because durable row is missing"
        );
        return GuardedSaveOutcome::Missing;
    };
    let Ok(on_disk) = serde_json::from_str::<InflightTurnState>(&data) else {
        tracing::debug!(
            provider = %provider.as_str(),
            channel_id = state.channel_id,
            caller = caller,
            snapshot_identity = ?InflightTurnIdentity::from_state(state),
            "inflight identity-refresh save skipped because durable row is malformed"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    };
    let durable = InflightTurnIdentity::from_state(&on_disk);
    if expected.user_msg_id == 0 && expected.turn_start_offset.is_none() {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = state.channel_id,
            caller = caller,
            snapshot_identity = ?expected,
            durable_identity = ?durable,
            snapshot_turn_start_offset = ?expected.turn_start_offset,
            durable_turn_start_offset = ?on_disk.turn_start_offset,
            "inflight identity-refresh save skipped because offsetless id-0 snapshot cannot safely match a durable row"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if !allow_output_restamp && on_disk.output_path != state.output_path {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = state.channel_id,
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
            channel_id = state.channel_id,
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
            channel_id = state.channel_id,
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
                channel_id = state.channel_id,
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
            channel_id = state.channel_id,
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
            channel_id = state.channel_id,
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
            channel_id = state.channel_id,
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
            channel_id = state.channel_id,
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
                channel_id = state.channel_id,
                caller = caller,
                error = %error,
                "restart-preserved full_response patch failed; leaving durable row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
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
                channel_id,
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
                channel_id,
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
                channel_id,
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
                channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "recovery output-path patch failed; leaving on-disk row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

/// #4370: stamp the `readopted_from_inflight` marker onto the persisted row for a
/// turn this process re-adopted from inflight, under the sidecar flock and pinned
/// to the re-adopted turn's identity. Returns a [`GuardedSaveOutcome`]:
///
///   - `Saved`            — the marker is now set (or was already set → still
///                          `Saved`, idempotent).
///   - `Missing`          — no row exists; it was cleared concurrently. We do NOT
///                          resurrect it (there is no live turn to protect).
///   - `IdentityMismatch` — a newer turn (or a rebind-origin placeholder) owns the
///                          row. We do NOT clobber it.
///   - `IoError`          — filesystem / serialization failure.
///
/// This is a NARROW read-modify-write (like
/// [`persist_recovery_output_path_if_matches_identity_locked`]), deliberately NOT
/// [`save_inflight_state_if_identity_unchanged`]. The broad identity-refresh save
/// refuses a DrainRestart row while it still carries `restart_mode`. Here we
/// re-read under the lock, verify the SAME turn, stamp the additive readoption
/// marker, and consume the restart marker because successful adoption transferred
/// lifecycle authority to this process. It never resurrects a concurrently-cleared
/// row (closes the load-then-blind-save TOCTOU window, #4370 F1).
pub(in crate::services::discord) fn mark_readopted_from_inflight_if_identity_unchanged(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    mark_readopted_from_inflight_if_identity_unchanged_in_root(
        &root, provider, channel_id, expected,
    )
}

pub(in crate::services::discord::inflight) fn mark_readopted_from_inflight_if_identity_unchanged_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
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
    // #4370 R3-5: match the broad `save_inflight_state_identity_gated_in_root`
    // id-0 fail-closed gate. `InflightTurnIdentity` cannot disambiguate colliding
    // `user_msg_id == 0 && turn_start_offset == None` rows (see the identity doc at
    // `model.rs`), so an offsetless id-0 snapshot must never authorize mutating a
    // durable row it cannot uniquely name. Not currently reachable (classify
    // short-circuits `owner == TUI_DIRECT_SYNTHETIC_OWNER_USER_ID` to `Synthetic`
    // before reading this marker, and real re-adopted owners carry a non-zero
    // `user_msg_id`), but kept as defense-in-depth so this narrow patch stays as
    // fail-closed as the broad save it replaces for restart adoption.
    if expected.user_msg_id == 0 && expected.turn_start_offset.is_none() {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id,
            snapshot_identity = ?expected,
            durable_identity = ?InflightTurnIdentity::from_state(&on_disk),
            "readopted-from-inflight marker skipped because offsetless id-0 snapshot cannot safely match a durable row (#4370 R3-5 fail-closed)"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if on_disk.rebind_origin || !expected.matches_state(&on_disk) {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    match persist_readopted_under_lock(
        root,
        &path,
        &mut on_disk,
        "src/services/discord/inflight.rs:mark_readopted_from_inflight_if_identity_unchanged_in_root",
    ) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id,
                error = %error,
                "readopted-from-inflight marker patch failed; leaving durable row untouched"
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
        None,
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
        None,
        expected_turn_start_offset,
        Some(expected_last_offset),
    )
}

pub(super) fn save_existing_inflight_rebind_adoption_impl_in_root(
    root: &Path,
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_episode: Option<&InflightEpisodePin>,
    expected_turn_start_offset: Option<u64>,
    expected_last_offset_for_rebase: Option<u64>,
) -> GuardedSaveOutcome {
    lock_and_save_existing_inflight_rebind_adoption_impl_in_root(
        root,
        state,
        expected,
        expected_episode,
        expected_turn_start_offset,
        expected_last_offset_for_rebase,
    )
    .map_or_else(|outcome| outcome, |_| GuardedSaveOutcome::Saved)
}

pub(in crate::services::discord::inflight) fn lock_and_save_existing_inflight_rebind_adoption_impl_in_root(
    root: &Path,
    state: &InflightTurnState,
    expected: &InflightTurnIdentity,
    expected_episode: Option<&InflightEpisodePin>,
    expected_turn_start_offset: Option<u64>,
    expected_last_offset_for_rebase: Option<u64>,
) -> Result<
    (
        super::super::store::InflightStateFileLock,
        InflightTurnState,
    ),
    GuardedSaveOutcome,
> {
    let Some(provider) = state.provider_kind() else {
        return Err(GuardedSaveOutcome::IoError);
    };
    let path = inflight_state_path(root, &provider, state.channel_id);
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return Err(GuardedSaveOutcome::IoError);
        }
    }
    let Ok(lock) = lock_inflight_state_path(&path) else {
        return Err(GuardedSaveOutcome::IoError);
    };
    let Ok(data) = fs::read_to_string(&path) else {
        return Err(GuardedSaveOutcome::Missing);
    };
    let Ok(on_disk) = serde_json::from_str::<InflightTurnState>(&data) else {
        return Err(GuardedSaveOutcome::IdentityMismatch);
    };
    if expected_episode.is_some_and(|pin| !pin.matches_state(&on_disk)) {
        return Err(GuardedSaveOutcome::IdentityMismatch);
    }
    if on_disk.rebind_origin {
        return Err(GuardedSaveOutcome::IdentityMismatch);
    }
    if on_disk.restart_mode != state.restart_mode {
        return Err(GuardedSaveOutcome::IdentityMismatch);
    }
    // #4400 (b) r2: zero-id `expected` authorizes this save ONLY for the
    // adoptable #3107 self-heal orphan carrying a birth `turn_start_offset`
    // (the id-0 fail-closed rule of `identity_matches_with_offset_guard`); all
    // other zero-id shapes keep the unconditional refusal (I2).
    if !expected.matches_state(&on_disk)
        || (expected.user_msg_id == 0
            && !(on_disk.is_adoptable_orphaned_synthetic_watcher_row()
                && on_disk.turn_start_offset.is_some()))
    {
        return Err(GuardedSaveOutcome::IdentityMismatch);
    }
    if let Some(expected_offset) = expected_turn_start_offset {
        if on_disk.turn_start_offset != Some(expected_offset) {
            return Err(GuardedSaveOutcome::IdentityMismatch);
        }
    }
    if expected_last_offset_for_rebase
        .is_some_and(|expected_last| on_disk.last_offset != expected_last)
    {
        return Err(GuardedSaveOutcome::IdentityMismatch);
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
        return Err(GuardedSaveOutcome::IoError);
    };
    match atomic_write(&path, &json) {
        Ok(()) => Ok((lock, updated)),
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = state.channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "existing inflight rebind adoption save failed; leaving on-disk row untouched"
            );
            Err(GuardedSaveOutcome::IoError)
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
            channel_id = state.channel_id,
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
                channel_id = state.channel_id,
                expected_user_msg_id = expected.user_msg_id,
                error = %error,
                "inflight identity-guarded save failed; leaving on-disk row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain_restart_seed(channel_id: u64, tmux_session_name: &str) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("adk-test".to_string()),
            343_742_347_365_974_026,
            77_010,
            18,
            "user prompt".to_string(),
            Some("session".to_string()),
            Some(tmux_session_name.to_string()),
            Some(format!("/tmp/{tmux_session_name}.jsonl")),
            None,
            512,
        )
    }

    #[test]
    fn claude_e_handoff_stamp_accepts_stale_memory_generation_but_guards_identity() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::TempDir::new().expect("runtime root");
        let mut state = drain_restart_seed(45_960, "AgentDesk-claude-4596");
        state.provider = ProviderKind::Claude.as_str().to_string();
        save_inflight_state_in_root(temp.path(), &state).expect("seed owner row");
        let path = inflight_state_path(temp.path(), &ProviderKind::Claude, state.channel_id);
        let durable: InflightTurnState =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read seeded row"))
                .expect("parse seeded row");
        let expected = InflightTurnIdentity::from_state(&state);
        assert!(
            durable.save_generation > state.save_generation,
            "test must reproduce the production stale in-memory generation"
        );

        let mut handoff = state.clone();
        handoff.tmux_session_name = None;
        handoff.runtime_kind = Some(RuntimeHandoffKind::ClaudeEAdapter);
        handoff.claude_e_pid = Some(42);
        handoff.claude_e_process_starttime = Some(9001);
        assert_eq!(
            claude_e_stamp::stamp_claude_e_process_if_matches_identity_in_root(
                temp.path(),
                &handoff,
                &expected,
            ),
            GuardedSaveOutcome::Saved,
        );
        let persisted: InflightTurnState =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read stamped row"))
                .expect("parse stamped row");
        assert_eq!(persisted.claude_e_pid, Some(42));
        assert_eq!(persisted.claude_e_process_starttime, Some(9001));

        let mut newer = persisted.clone();
        newer.user_msg_id = 99_999;
        save_inflight_state_in_root(temp.path(), &newer).expect("seed newer turn");
        assert_eq!(
            claude_e_stamp::stamp_claude_e_process_if_matches_identity_in_root(
                temp.path(),
                &handoff,
                &expected,
            ),
            GuardedSaveOutcome::IdentityMismatch,
        );
        let still_newer: InflightTurnState =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read newer row"))
                .expect("parse newer row");
        assert_eq!(still_newer.user_msg_id, 99_999);
    }

    // #4370 F1: the `readopted_from_inflight` marker is a narrow adoption patch.
    // It lands on a DrainRestart row (where the broad identity-refresh save
    // refuses `restart_mode` rows) and consumes the handoff marker; it never
    // resurrects a concurrently-cleared row (`Missing`);
    // and it refuses to clobber a different turn's row (`IdentityMismatch`).
    #[test]
    fn readopted_marker_lands_on_restart_preserved_row_and_never_resurrects() {
        // #3293: pin the runtime root to a tempdir before any state construction
        // resolves it, so an ambient `AGENTDESK_ROOT_DIR=~/.adk/release` (every
        // release workspace has one) cannot make this test touch live state.
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            temp.path(),
        );
        let provider = ProviderKind::Codex;

        // (1) Missing: no durable row → the marker patch does NOT resurrect it.
        let mut state = drain_restart_seed(44_370, "AgentDesk-codex-4370-drain");
        let expected = InflightTurnIdentity::from_state(&state);
        assert_eq!(
            mark_readopted_from_inflight_if_identity_unchanged_in_root(
                temp.path(),
                &provider,
                state.channel_id,
                &expected,
            ),
            GuardedSaveOutcome::Missing,
            "an absent row must not be resurrected by the marker patch",
        );

        // (2) Saved on a DrainRestart-preserved row — where the broad refresh
        // REFUSES (`restart_mode.is_some()`), proving why F1 needs this narrow
        // patch instead of `save_inflight_state_if_identity_unchanged`.
        state.set_restart_mode(InflightRestartMode::DrainRestart);
        save_inflight_state_in_root(temp.path(), &state).expect("seed restart-preserved row");
        let expected = InflightTurnIdentity::from_state(&state);
        assert_eq!(
            save_inflight_state_if_identity_unchanged_in_root(
                temp.path(),
                &state,
                "test::readopted_marker_broad_refresh_refuses",
            ),
            GuardedSaveOutcome::IdentityMismatch,
            "the broad identity-refresh save must keep refusing restart_mode rows",
        );
        assert_eq!(
            mark_readopted_from_inflight_if_identity_unchanged_in_root(
                temp.path(),
                &provider,
                state.channel_id,
                &expected,
            ),
            GuardedSaveOutcome::Saved,
        );

        let persisted_path = inflight_state_path(temp.path(), &provider, state.channel_id);
        let persisted: InflightTurnState = serde_json::from_str(
            &std::fs::read_to_string(&persisted_path).expect("read persisted inflight"),
        )
        .expect("parse persisted inflight");
        assert!(
            persisted.readopted_from_inflight,
            "the marker must land on the restart-preserved row"
        );
        assert_eq!(
            persisted.restart_mode, None,
            "successful replacement-process adoption must consume restart_mode"
        );
        assert_eq!(persisted.restart_generation, None);

        // (3) Idempotent: a re-mark of an already-marked row is a `Saved` no-op.
        assert_eq!(
            mark_readopted_from_inflight_if_identity_unchanged_in_root(
                temp.path(),
                &provider,
                state.channel_id,
                &expected,
            ),
            GuardedSaveOutcome::Saved,
        );

        // (4) IdentityMismatch: a different turn identity must not be clobbered.
        let mut other = state.clone();
        other.user_msg_id = 99_999;
        let mismatched = InflightTurnIdentity::from_state(&other);
        assert_eq!(
            mark_readopted_from_inflight_if_identity_unchanged_in_root(
                temp.path(),
                &provider,
                state.channel_id,
                &mismatched,
            ),
            GuardedSaveOutcome::IdentityMismatch,
        );

        // (5) #4370 R3-5: an offsetless id-0 snapshot is refused fail-closed even
        // against a byte-identical durable row. Because `InflightTurnIdentity`
        // cannot uniquely name a `user_msg_id == 0 && turn_start_offset == None`
        // row, the marker patch must never authorize mutating it (mirrors the
        // broad `save_inflight_state_identity_gated_in_root` id-0 gate). Asserting
        // `IdentityMismatch` (not `Missing`) proves BOTH that the row persisted AND
        // that the id-0 guard — not a matches_state miss — produced the refusal.
        let mut id0 = InflightTurnState::new(
            ProviderKind::Codex,
            54_370,
            Some("adk-test".to_string()),
            343_742_347_365_974_026,
            0, // user_msg_id == 0
            0,
            "id0 offsetless prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-codex-4370-id0".to_string()),
            Some("/tmp/AgentDesk-codex-4370-id0.jsonl".to_string()),
            None,
            512,
        );
        // Force the offsetless id-0 shape the R3-5 guard fails closed on (the
        // constructor seeds a `turn_start_offset` from `last_offset`).
        id0.turn_start_offset = None;
        assert_eq!(id0.user_msg_id, 0);
        assert!(id0.turn_start_offset.is_none());
        save_inflight_state_in_root(temp.path(), &id0).expect("seed offsetless id-0 row");
        let id0_expected = InflightTurnIdentity::from_state(&id0);
        assert_eq!(
            mark_readopted_from_inflight_if_identity_unchanged_in_root(
                temp.path(),
                &ProviderKind::Codex,
                id0.channel_id,
                &id0_expected,
            ),
            GuardedSaveOutcome::IdentityMismatch,
            "an offsetless id-0 snapshot must be refused fail-closed even against a byte-identical durable row (#4370 R3-5)",
        );
    }

    // #4259: a restamp compares the durable row with the identity captured
    // before its cursor mutation. The repaired write may advance the cursor,
    // but a stale snapshot must not overwrite a newer turn which shares all
    // other identity fields.
    #[test]
    fn output_restamp_advances_stale_offset_but_rejects_different_turn() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = tempfile::tempdir().expect("runtime root");
        let mut durable = drain_restart_seed(42_590, "AgentDesk-codex-4259");
        durable.turn_start_offset = Some(17);
        durable.output_path = Some("/tmp/old-rollout.jsonl".to_string());
        save_inflight_state_in_root(root.path(), &durable).expect("seed durable row");

        let expected = InflightTurnIdentity::from_state(&durable);
        let mut restamped = durable.clone();
        restamped.turn_start_offset = Some(911);
        restamped.last_offset = 911;
        restamped.output_path = Some("/tmp/rotated-rollout.jsonl".to_string());
        assert_eq!(
            save_inflight_state_if_identity_matches_allow_output_restamp_in_root(
                root.path(),
                &restamped,
                &expected,
                "test::codex_idle_rollout_stale_offset_repair",
            ),
            GuardedSaveOutcome::Saved,
            "the durable row is matched against the loaded offset, then written with the advanced offset",
        );
        let path = inflight_state_path(root.path(), &ProviderKind::Codex, durable.channel_id);
        let persisted: InflightTurnState =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read restamped row"))
                .expect("parse restamped row");
        assert_eq!(persisted.turn_start_offset, Some(911));
        assert_eq!(
            persisted.output_path.as_deref(),
            Some("/tmp/rotated-rollout.jsonl")
        );

        let expected = InflightTurnIdentity::from_state(&persisted);
        let mut synthetic_refresh = persisted.clone();
        synthetic_refresh.turn_start_offset = Some(1_337);
        synthetic_refresh.last_offset = 1_337;
        assert_eq!(
            save_inflight_state_if_identity_matches_allow_output_restamp_in_root(
                root.path(),
                &synthetic_refresh,
                &expected,
                "test::synthetic_start_advanced_offset_refresh",
            ),
            GuardedSaveOutcome::Saved,
            "a same-turn synthetic refresh must re-own after its cursor advances",
        );

        let mut different_turn = synthetic_refresh.clone();
        different_turn.user_msg_id += 1;
        different_turn.turn_start_offset = Some(2_048);
        let different_expected = InflightTurnIdentity::from_state(&different_turn);
        assert_eq!(
            save_inflight_state_if_identity_matches_allow_output_restamp_in_root(
                root.path(),
                &different_turn,
                &different_expected,
                "test::output_restamp_different_turn_rejected",
            ),
            GuardedSaveOutcome::IdentityMismatch,
            "a different user-message identity must not clobber the durable turn",
        );
        let preserved: InflightTurnState =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read sealed row"))
                .expect("parse sealed row");
        assert_eq!(preserved.user_msg_id, synthetic_refresh.user_msg_id);
        assert_eq!(preserved.turn_start_offset, Some(1_337));

        // Mutation proof: round-1's offset-dropping restamp predicate would see
        // only {0, started_at, tmux_session_name} below and return Saved, thereby
        // clobbering `newer_zero_id`. Full pre-mutation identity comparison must
        // reject it because birth offset 3_001 differs from 3_002.
        let mut older_zero_id = drain_restart_seed(42_591, "AgentDesk-codex-zero-id-4259");
        older_zero_id.user_msg_id = 0;
        older_zero_id.started_at = "2026-07-22T10:00:00Z".to_string();
        older_zero_id.turn_start_offset = Some(3_001);
        older_zero_id.last_offset = 3_001;
        let older_expected = InflightTurnIdentity::from_state(&older_zero_id);
        let mut newer_zero_id = older_zero_id.clone();
        newer_zero_id.turn_start_offset = Some(3_002);
        newer_zero_id.last_offset = 3_002;
        newer_zero_id.output_path = Some("/tmp/newer-zero-id.jsonl".to_string());
        save_inflight_state_in_root(root.path(), &newer_zero_id)
            .expect("seed newer zero-id turn with same timestamp and tmux");
        let mut stale_zero_id_restamp = older_zero_id.clone();
        stale_zero_id_restamp.turn_start_offset = Some(4_001);
        stale_zero_id_restamp.last_offset = 4_001;
        stale_zero_id_restamp.output_path = Some("/tmp/stale-zero-id.jsonl".to_string());
        assert_eq!(
            save_inflight_state_if_identity_matches_allow_output_restamp_in_root(
                root.path(),
                &stale_zero_id_restamp,
                &older_expected,
                "test::output_restamp_zero_id_collision_rejected",
            ),
            GuardedSaveOutcome::IdentityMismatch,
            "same-timestamp zero-id turns are disambiguated by their loaded birth offsets",
        );
        let zero_id_path =
            inflight_state_path(root.path(), &ProviderKind::Codex, newer_zero_id.channel_id);
        let zero_id_preserved: InflightTurnState = serde_json::from_str(
            &std::fs::read_to_string(&zero_id_path).expect("read newer zero-id row"),
        )
        .expect("parse newer zero-id row");
        assert_eq!(zero_id_preserved.turn_start_offset, Some(3_002));
        assert_eq!(
            zero_id_preserved.output_path.as_deref(),
            Some("/tmp/newer-zero-id.jsonl"),
            "the rejected stale snapshot leaves the newer durable row unchanged",
        );
    }

    /// The zero-id headless row exactly as the #3107 watcher self-heal
    /// re-mints it (the #4400 (b) adoption subject): no request owner, no user
    /// message, Watcher relay owner, live-stream anchors, birth offset stamped
    /// by the constructor.
    fn orphaned_synthetic_watcher_row(channel_id: u64) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Claude,
            channel_id,
            None,
            0,
            0,
            1_518_888_000_000_000_001,
            String::new(),
            None,
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/48fdb7f3-0000-4000-8000-000000004400.jsonl".to_string()),
            None,
            8_192,
        );
        state.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::Watcher);
        state
    }

    /// #4400 (b) review r2: the rebind adoption save must accept the adoptable
    /// zero-id orphan (pre-fix it was refused as `IdentityMismatch`, turning
    /// the classifier's 409 self-deadlock into a 500 self-deadlock — the fix
    /// was invalid on the real path), while every OTHER zero-id shape keeps
    /// the unconditional refusal. Each contrast row is a mutation kill: widen
    /// the new arm past the orphan shape and its assert fails.
    #[test]
    fn rebind_adoption_save_adopts_zero_id_orphan_but_refuses_live_synthetic_shapes() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            temp.path(),
        );
        let provider = ProviderKind::Claude;

        // (1) The adoptable orphan: Saved. The adoption mutates only the
        // watcher-binding surface (tmux/output/owner) and must keep the row's
        // zero-id identity and committed offsets untouched.
        let orphan = orphaned_synthetic_watcher_row(64_400);
        save_inflight_state_in_root(temp.path(), &orphan).expect("seed orphan row");
        let expected = InflightTurnIdentity::from_state(&orphan);
        let mut adopted = orphan.clone();
        adopted.output_path = Some("/tmp/48fdb7f3-0000-4000-8000-000000004400.jsonl".to_string());
        adopted.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::Watcher);
        assert_eq!(
            save_existing_inflight_rebind_adoption_if_matches_identity_in_root(
                temp.path(),
                &adopted,
                &expected,
                orphan.turn_start_offset,
            ),
            GuardedSaveOutcome::Saved,
            "#4400 (b): the adoption save must accept the orphaned zero-id synthetic watcher row \
             — refusing it turns the respawn 409 deadlock into an Internal-error 500 deadlock (I1)",
        );
        let persisted_path = inflight_state_path(temp.path(), &provider, orphan.channel_id);
        let persisted: InflightTurnState = serde_json::from_str(
            &std::fs::read_to_string(&persisted_path).expect("read adopted row"),
        )
        .expect("parse adopted row");
        assert_eq!(persisted.user_msg_id, 0);
        assert_eq!(persisted.request_owner_user_id, 0);
        assert_eq!(
            persisted.last_offset, 8_192,
            "the non-rebase adoption save must preserve the committed offset (I3)"
        );

        // (2) Opus-arm safety contrast: a live TUI-direct synthetic row
        // (#4018 owner `request_owner_user_id == 1`) is NOT the orphan and
        // must keep the refusal — adopting it would steal a live relay owner.
        let mut tui_direct = orphaned_synthetic_watcher_row(64_401);
        tui_direct.request_owner_user_id = 1;
        save_inflight_state_in_root(temp.path(), &tui_direct).expect("seed TUI-direct row");
        let tui_expected = InflightTurnIdentity::from_state(&tui_direct);
        assert_eq!(
            save_existing_inflight_rebind_adoption_if_matches_identity_in_root(
                temp.path(),
                &tui_direct,
                &tui_expected,
                tui_direct.turn_start_offset,
            ),
            GuardedSaveOutcome::IdentityMismatch,
            "a live TUI-direct synthetic row (owner 1) must keep the zero-id refusal (I2)",
        );

        // (3) Bridge-owned/default zero-id row: not the self-heal shape.
        let mut bridge_owned = orphaned_synthetic_watcher_row(64_402);
        bridge_owned.set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::None);
        save_inflight_state_in_root(temp.path(), &bridge_owned).expect("seed bridge-owned row");
        let bridge_expected = InflightTurnIdentity::from_state(&bridge_owned);
        assert_eq!(
            save_existing_inflight_rebind_adoption_if_matches_identity_in_root(
                temp.path(),
                &bridge_owned,
                &bridge_expected,
                bridge_owned.turn_start_offset,
            ),
            GuardedSaveOutcome::IdentityMismatch,
            "a bridge-owned zero-id row must keep the refusal",
        );

        // (4) Offsetless zero-id orphan: fail closed (mirrors the id-0
        // birth-offset rule in `identity_matches_with_offset_guard`).
        let mut offsetless = orphaned_synthetic_watcher_row(64_403);
        offsetless.turn_start_offset = None;
        save_inflight_state_in_root(temp.path(), &offsetless).expect("seed offsetless row");
        let offsetless_expected = InflightTurnIdentity::from_state(&offsetless);
        assert_eq!(
            save_existing_inflight_rebind_adoption_if_matches_identity_in_root(
                temp.path(),
                &offsetless,
                &offsetless_expected,
                None,
            ),
            GuardedSaveOutcome::IdentityMismatch,
            "an offsetless zero-id row cannot be uniquely named and must be refused fail-closed",
        );
    }
}
