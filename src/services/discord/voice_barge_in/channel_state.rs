//! Explicit per-channel lifecycle state for the Discord voice runtime.
//!
//! The resource maps in this component intentionally retain their original
//! `DashMap` value types and operations. The extraction changes ownership and
//! observability, not locking, cancellation, or barge-in behavior.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VoiceChannelPhase {
    Idle,
    Joining,
    Connected,
    Speaking,
    BargedIn,
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VoiceChannelEvent {
    JoinStarted,
    JoinSucceeded,
    PlaybackStarted,
    PlaybackFinished,
    BargeInDetected,
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VoiceChannelState {
    phase: VoiceChannelPhase,
    guild_id: Option<GuildId>,
}

impl Default for VoiceChannelState {
    fn default() -> Self {
        Self {
            phase: VoiceChannelPhase::Idle,
            guild_id: None,
        }
    }
}

impl VoiceChannelState {
    fn apply(&mut self, event: VoiceChannelEvent) -> bool {
        use VoiceChannelEvent as Event;
        use VoiceChannelPhase as Phase;

        let next = match (self.phase, event) {
            (Phase::Idle | Phase::Disconnected, Event::JoinStarted) => Phase::Joining,
            // Direct registration is required for an already-connected Songbird
            // call discovered by the idempotent auto-join path.
            (Phase::Idle | Phase::Joining | Phase::Connected, Event::JoinSucceeded) => {
                Phase::Connected
            }
            (
                Phase::Connected | Phase::Speaking | Phase::BargedIn,
                Event::PlaybackStarted,
            ) => Phase::Speaking,
            (Phase::Speaking | Phase::BargedIn, Event::PlaybackFinished) => Phase::Connected,
            (
                Phase::Connected | Phase::Speaking | Phase::BargedIn,
                Event::BargeInDetected,
            ) => Phase::BargedIn,
            (_, Event::Disconnected) => Phase::Disconnected,
            // Idempotent lifecycle notifications do not create a transition.
            (phase, Event::JoinStarted) if phase == Phase::Joining => phase,
            (phase, Event::PlaybackFinished) if phase == Phase::Connected => phase,
            _ => return false,
        };
        self.phase = next;
        true
    }
}

/// Owns every channel-keyed registry previously stored directly on
/// `VoiceBargeInRuntime`, plus the explicit lifecycle state that explains how
/// those resources are expected to relate.
pub(super) struct VoiceChannelStateMachines {
    states: dashmap::DashMap<u64, VoiceChannelState>,
    pub(super) monitors: dashmap::DashMap<u64, Arc<std::sync::Mutex<LiveBargeInMonitor>>>,
    pub(super) playbacks: dashmap::DashMap<u64, Arc<LivePlaybackSession>>,
    pub(super) spoken_result_playbacks: dashmap::DashMap<u64, SpokenResultPlaybackSession>,
    pub(super) active_voice_routes: dashmap::DashMap<u64, ActiveVoiceRoute>,
    pub(super) deferred_buffers: dashmap::DashMap<u64, Arc<Mutex<DeferredBargeInBuffer>>>,
    pub(super) inflight_foreground_cancels:
        dashmap::DashMap<u64, Vec<Arc<crate::services::provider::CancelToken>>>,
}

impl VoiceChannelStateMachines {
    pub(super) fn new() -> Self {
        Self {
            states: dashmap::DashMap::new(),
            monitors: dashmap::DashMap::new(),
            playbacks: dashmap::DashMap::new(),
            spoken_result_playbacks: dashmap::DashMap::new(),
            active_voice_routes: dashmap::DashMap::new(),
            deferred_buffers: dashmap::DashMap::new(),
            inflight_foreground_cancels: dashmap::DashMap::new(),
        }
    }

    fn transition(
        &self,
        channel_id: ChannelId,
        guild_id: Option<GuildId>,
        event: VoiceChannelEvent,
    ) -> VoiceChannelPhase {
        let mut state = self.states.entry(channel_id.get()).or_default();
        let previous = state.phase;
        if let Some(guild_id) = guild_id {
            state.guild_id = Some(guild_id);
        }
        let accepted = state.apply(event);
        let current = state.phase;
        drop(state);

        if accepted && current != previous {
            tracing::info!(
                channel_id = channel_id.get(),
                guild_id = ?guild_id.map(|id| id.get()),
                from = ?previous,
                to = ?current,
                event = ?event,
                "voice channel state transition"
            );
        } else if !accepted {
            tracing::debug!(
                channel_id = channel_id.get(),
                from = ?previous,
                event = ?event,
                "voice channel state transition ignored"
            );
        }
        current
    }

    pub(super) fn join_started(&self, channel_id: ChannelId, guild_id: GuildId) {
        self.transition(channel_id, Some(guild_id), VoiceChannelEvent::JoinStarted);
    }

    pub(super) fn connected(&self, channel_id: ChannelId, guild_id: GuildId) {
        self.transition(
            channel_id,
            Some(guild_id),
            VoiceChannelEvent::JoinSucceeded,
        );
    }

    pub(super) fn playback_started(&self, channel_id: ChannelId) {
        self.transition(channel_id, None, VoiceChannelEvent::PlaybackStarted);
    }

    pub(super) fn playback_finished(&self, channel_id: ChannelId) {
        if !self.playbacks.contains_key(&channel_id.get())
            && !self
                .spoken_result_playbacks
                .contains_key(&channel_id.get())
        {
            self.transition(channel_id, None, VoiceChannelEvent::PlaybackFinished);
        }
    }

    pub(super) fn barged_in(&self, channel_id: ChannelId) {
        self.transition(channel_id, None, VoiceChannelEvent::BargeInDetected);
    }

    pub(super) fn disconnected(&self, channel_id: ChannelId) {
        self.transition(channel_id, None, VoiceChannelEvent::Disconnected);
    }

    pub(super) fn guild_id(&self, channel_id: ChannelId) -> Option<GuildId> {
        self.states
            .get(&channel_id.get())
            .and_then(|state| state.guild_id)
    }

    pub(super) fn channel_ids_for_guild(&self, guild_id: GuildId) -> Vec<u64> {
        self.states
            .iter()
            .filter_map(|entry| {
                (entry.guild_id == Some(guild_id)
                    && entry.phase != VoiceChannelPhase::Disconnected)
                    .then_some(*entry.key())
            })
            .collect()
    }

    pub(super) fn forget(&self, channel_id: u64) {
        self.states.remove(&channel_id);
    }

    #[cfg(test)]
    fn phase(&self, channel_id: ChannelId) -> VoiceChannelPhase {
        self.states
            .get(&channel_id.get())
            .map(|state| state.phase)
            .unwrap_or(VoiceChannelPhase::Idle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_transitions_cover_join_speak_barge_in_disconnect() {
        let channels = VoiceChannelStateMachines::new();
        let channel_id = ChannelId::new(42);
        let guild_id = GuildId::new(7);

        assert_eq!(channels.phase(channel_id), VoiceChannelPhase::Idle);
        channels.join_started(channel_id, guild_id);
        assert_eq!(channels.phase(channel_id), VoiceChannelPhase::Joining);
        channels.connected(channel_id, guild_id);
        assert_eq!(channels.phase(channel_id), VoiceChannelPhase::Connected);
        channels.playback_started(channel_id);
        assert_eq!(channels.phase(channel_id), VoiceChannelPhase::Speaking);
        channels.barged_in(channel_id);
        assert_eq!(channels.phase(channel_id), VoiceChannelPhase::BargedIn);
        channels.disconnected(channel_id);
        assert_eq!(channels.phase(channel_id), VoiceChannelPhase::Disconnected);
    }

    #[test]
    fn invalid_transition_does_not_skip_connection_lifecycle() {
        let channels = VoiceChannelStateMachines::new();
        let channel_id = ChannelId::new(42);

        channels.playback_started(channel_id);

        assert_eq!(channels.phase(channel_id), VoiceChannelPhase::Idle);
    }

    #[test]
    fn connected_registration_supports_existing_songbird_call() {
        let channels = VoiceChannelStateMachines::new();
        let channel_id = ChannelId::new(42);
        let guild_id = GuildId::new(7);

        channels.connected(channel_id, guild_id);

        assert_eq!(channels.phase(channel_id), VoiceChannelPhase::Connected);
        assert_eq!(channels.guild_id(channel_id), Some(guild_id));
    }
}
