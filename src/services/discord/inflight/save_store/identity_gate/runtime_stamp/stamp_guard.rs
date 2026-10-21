//! Identity and authority refusals a runtime-handoff stamp applies to the lock-held row.

use super::*;

/// Why a stamp must not touch `on_disk`: an unnameable snapshot, moved durable authority or a
/// changed established runtime session; `None` lets the stamp proceed.
pub(super) fn stamp_identity_refusal(
    provider: &ProviderKind,
    requested: &InflightTurnState,
    expected: &InflightTurnIdentity,
    on_disk: &InflightTurnState,
    caller: &'static str,
) -> Option<GuardedSaveOutcome> {
    let durable = InflightTurnIdentity::from_state(on_disk);
    if expected.user_msg_id == 0 && expected.turn_start_offset.is_none() {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = requested.channel_id,
            caller,
            snapshot_identity = ?expected,
            durable_identity = ?durable,
            "runtime-handoff stamp skipped because offsetless id-0 snapshot cannot safely match a durable row"
        );
        return Some(GuardedSaveOutcome::Unnameable);
    }
    if on_disk.restart_mode.is_some() || on_disk.rebind_origin || !expected.matches_state(on_disk) {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = requested.channel_id,
            caller,
            snapshot_identity = ?expected,
            durable_identity = ?durable,
            durable_restart_mode = ?on_disk.restart_mode,
            durable_rebind_origin = on_disk.rebind_origin,
            "runtime-handoff stamp skipped because durable row authority changed"
        );
        return Some(GuardedSaveOutcome::from_durable_authority(on_disk));
    }
    if expected.tmux_session_name.is_some()
        && requested.tmux_session_name != expected.tmux_session_name
    {
        tracing::info!(
            provider = %provider.as_str(),
            channel_id = requested.channel_id,
            caller,
            snapshot_tmux_session_name = ?expected.tmux_session_name,
            requested_tmux_session_name = ?requested.tmux_session_name,
            "runtime-handoff stamp skipped because an established runtime session changed"
        );
        return Some(GuardedSaveOutcome::AuthorityPinned);
    }
    None
}
