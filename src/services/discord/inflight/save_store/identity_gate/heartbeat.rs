use super::*;

/// Bump only the durable row heartbeat while the caller still owns the turn.
///
/// Unlike a whole-row save, this locked patch never copies the caller's stale
/// snapshot over fields that another component may have advanced. The durable
/// row must retain the explicit identity captured before the stream tick began,
/// and restart/rebind authority changes fail closed.
pub(in crate::services::discord) fn touch_inflight_state_if_matches_identity(
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    caller: &'static str,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    touch_inflight_state_if_matches_identity_in_root(&root, provider, channel_id, expected, caller)
}

fn touch_inflight_state_if_matches_identity_in_root(
    root: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    caller: &'static str,
) -> GuardedSaveOutcome {
    let path = inflight_state_path(root, provider, channel_id);
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    let Some(on_disk) = load_inflight_state_unlocked(&path) else {
        return GuardedSaveOutcome::Missing;
    };
    if expected.user_msg_id == 0 && expected.turn_start_offset.is_none() {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id,
            caller,
            snapshot_identity = ?expected,
            "inflight heartbeat skipped because offsetless id-0 identity cannot safely own a durable row"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if on_disk.restart_mode.is_some() || on_disk.rebind_origin || !expected.matches_state(&on_disk)
    {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id,
            caller,
            snapshot_identity = ?expected,
            durable_identity = ?InflightTurnIdentity::from_state(&on_disk),
            durable_restart_mode = ?on_disk.restart_mode,
            durable_rebind_origin = on_disk.rebind_origin,
            "inflight heartbeat skipped because durable row authority changed"
        );
        return GuardedSaveOutcome::IdentityMismatch;
    }

    match persist_under_lock(
        root,
        &path,
        &on_disk,
        "src/services/discord/inflight/save_store/identity_gate/heartbeat.rs:touch_inflight_state_if_matches_identity_in_root",
    ) {
        Ok(()) => GuardedSaveOutcome::Saved,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id,
                caller,
                error = %error,
                "inflight heartbeat failed; leaving durable row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}
