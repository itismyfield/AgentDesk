//! Relay-owner projections bridge entry and tick reconciliation derive from the durable row.

use super::*;

pub(super) struct LiveWatcherRelayObservation {
    pub(super) incarnation: WatcherClaimIncarnation,
}

pub(super) fn live_watcher_relay_observation(
    shared: &SharedData,
    owner_channel_id: ChannelId,
) -> Option<LiveWatcherRelayObservation> {
    let watcher = shared.tmux_watchers.get(&owner_channel_id)?;
    if watcher.cancel.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    Some(LiveWatcherRelayObservation {
        incarnation: WatcherClaimIncarnation::from_handle(owner_channel_id, &watcher),
    })
}

pub(super) fn relay_owner_flags(
    owner_kind: crate::services::discord::inflight::RelayOwnerKind,
    watcher_registered: bool,
) -> (bool, bool, bool) {
    use crate::services::discord::inflight::RelayOwnerKind;

    let watcher_owns_assistant_relay = matches!(owner_kind, RelayOwnerKind::Watcher);
    let watcher_relay_available_for_turn = watcher_owns_assistant_relay && watcher_registered;
    let standby_relay_owns_output = matches!(
        owner_kind,
        RelayOwnerKind::StandbyRelay | RelayOwnerKind::SessionBoundRelay | RelayOwnerKind::Unknown
    );
    (
        watcher_owns_assistant_relay,
        watcher_relay_available_for_turn,
        standby_relay_owns_output,
    )
}

pub(super) fn reconciled_watcher_owner_channel_id(
    durable_owner_channel_id: Option<u64>,
    delivery_channel_id: u64,
) -> ChannelId {
    durable_owner_channel_id
        .and_then(crate::services::discord::inflight::opt_channel_id)
        .unwrap_or_else(|| ChannelId::new(delivery_channel_id))
}
