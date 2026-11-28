use std::sync::{Arc, LazyLock, Mutex};

use poise::serenity_prelude::ChannelId;

use super::{InflightEpisodePin, InflightTurnState, RelayOwnerKind};
use crate::db::dispatched_session_canonical_identity::HookSessionActorPin;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::tui_prompt_dedupe::{ExternalInputRelayLease, ExternalInputRelayOwner};

#[derive(Clone)]
pub(in crate::services::discord) struct Witness {
    pub(in crate::services::discord) episode: InflightEpisodePin,
    pub(in crate::services::discord) actor: std::sync::Weak<CancelToken>,
    pub(in crate::services::discord) pg_pin: Option<HookSessionActorPin>,
}

pub(in crate::services::discord) static CLAIMS: LazyLock<
    Mutex<std::collections::HashMap<(String, u64), Witness>>,
> = LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

/// Advance the matching live witness while the caller holds the row and source guards.
pub(in crate::services::discord) fn preserve_admitted_source(
    before: &InflightEpisodePin,
    admitted: &InflightTurnState,
    actor: &Arc<CancelToken>,
) {
    let mut claims = CLAIMS.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(witness) = claims
        .get_mut(&(admitted.provider.clone(), admitted.channel_id))
        .filter(|witness| {
            witness.episode == *before
                && witness
                    .actor
                    .upgrade()
                    .is_some_and(|saved| Arc::ptr_eq(&saved, actor))
        })
    {
        witness.episode = InflightEpisodePin::from_state(admitted);
    }
}

/// Carry a live witness across a write to the same episode while the bridge still owns it.
pub(in crate::services::discord) fn preserve_stamped_source(
    before: &InflightEpisodePin,
    stamped: &InflightTurnState,
) {
    if stamped.effective_relay_owner_kind() != RelayOwnerKind::None {
        return;
    }
    let advanced = InflightEpisodePin::from_state(stamped);
    if !before.is_same_episode_as(&advanced) {
        return;
    }
    let mut claims = CLAIMS.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(witness) = claims
        .get_mut(&(stamped.provider.clone(), stamped.channel_id))
        .filter(|witness| witness.episode == *before && witness.actor.upgrade().is_some())
    {
        witness.episode = advanced;
    }
}

pub(in crate::services::discord) struct TuiDirectExternalInputLeaseGuard {
    provider: ProviderKind,
    tmux_session_name: String,
    channel_id: ChannelId,
    lease: ExternalInputRelayLease,
    active: bool,
}

impl TuiDirectExternalInputLeaseGuard {
    pub(in crate::services::discord) fn new(
        provider: ProviderKind,
        tmux_session_name: &str,
        channel_id: ChannelId,
        lease: &ExternalInputRelayLease,
    ) -> Self {
        Self {
            provider,
            tmux_session_name: tmux_session_name.to_string(),
            channel_id,
            lease: lease.clone(),
            active: true,
        }
    }

    pub(in crate::services::discord) fn disarm(&mut self) {
        self.active = false;
    }

    fn clear_if_current(&self) -> bool {
        clear_external_input_bridge_lease_if_current(
            &self.provider,
            &self.tmux_session_name,
            self.channel_id,
            &self.lease,
        )
    }
}

impl Drop for TuiDirectExternalInputLeaseGuard {
    fn drop(&mut self) {
        // Match the exact lease so a slow timeout cannot clear a newer direct-input turn
        // that reused the same provider/session/channel after this tail started.
        if self.active {
            self.clear_if_current();
        }
    }
}

pub(in crate::services::discord) fn clear_external_input_bridge_lease_if_current(
    provider: &ProviderKind,
    tmux_session_name: &str,
    channel_id: ChannelId,
    lease: &ExternalInputRelayLease,
) -> bool {
    if !bridge_adapter_owns_external_turn(lease.relay_owner) {
        return false;
    }
    crate::services::tui_prompt_dedupe::clear_external_input_relay_lease_if_matches(
        provider.as_str(),
        tmux_session_name,
        channel_id.get(),
        lease,
    )
}

pub(in crate::services::discord) fn bridge_adapter_owns_external_turn(
    owner: ExternalInputRelayOwner,
) -> bool {
    owner == ExternalInputRelayOwner::BridgeAdapter
}

#[cfg(unix)]
pub(in crate::services::discord) struct DormantSyntheticClaim {
    pub(in crate::services::discord) row: InflightTurnState,
    pub(in crate::services::discord) actor: Arc<CancelToken>,
    pub(in crate::services::discord) lease: ExternalInputRelayLease,
    pub(in crate::services::discord) _lease: Option<TuiDirectExternalInputLeaseGuard>,
    pub(in crate::services::discord) _serial: tokio::sync::OwnedMutexGuard<()>,
}
