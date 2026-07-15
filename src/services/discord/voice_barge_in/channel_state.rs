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
}

impl Default for VoiceChannelState {
    fn default() -> Self {
        Self {
            phase: VoiceChannelPhase::Idle,
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
            (
                Phase::Idle | Phase::Joining | Phase::Connected | Phase::Disconnected,
                Event::JoinSucceeded,
            ) => Phase::Connected,
            (Phase::Connected | Phase::Speaking | Phase::BargedIn, Event::PlaybackStarted) => {
                Phase::Speaking
            }
            (Phase::Speaking | Phase::BargedIn, Event::PlaybackFinished) => Phase::Connected,
            (Phase::Connected | Phase::Speaking | Phase::BargedIn, Event::BargeInDetected) => {
                Phase::BargedIn
            }
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
    voice_guilds: dashmap::DashMap<u64, GuildId>,
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
            voice_guilds: dashmap::DashMap::new(),
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
        event_guild_id: Option<GuildId>,
        event: VoiceChannelEvent,
        create_missing: bool,
    ) {
        let mut state = if create_missing {
            self.states.entry(channel_id.get()).or_default()
        } else {
            let Some(state) = self.states.get_mut(&channel_id.get()) else {
                return;
            };
            state
        };
        let previous = state.phase;
        let accepted = state.apply(event);
        let current = state.phase;
        drop(state);

        if accepted && current != previous {
            tracing::info!(
                channel_id = channel_id.get(),
                guild_id = ?event_guild_id
                    .or_else(|| self.guild_id(channel_id))
                    .map(|id| id.get()),
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
    }

    pub(super) fn join_started(&self, channel_id: ChannelId, guild_id: GuildId) {
        self.transition(
            channel_id,
            Some(guild_id),
            VoiceChannelEvent::JoinStarted,
            true,
        );
    }

    pub(super) fn connected(&self, channel_id: ChannelId, guild_id: GuildId) {
        self.register_context(channel_id, guild_id);
        self.transition(
            channel_id,
            Some(guild_id),
            VoiceChannelEvent::JoinSucceeded,
            true,
        );
    }

    pub(super) fn playback_started(&self, channel_id: ChannelId) {
        self.transition(channel_id, None, VoiceChannelEvent::PlaybackStarted, false);
    }

    pub(super) fn playback_finished(&self, channel_id: ChannelId) {
        if !self.playbacks.contains_key(&channel_id.get())
            && !self.spoken_result_playbacks.contains_key(&channel_id.get())
        {
            self.transition(channel_id, None, VoiceChannelEvent::PlaybackFinished, false);
        }
    }

    pub(super) fn barged_in(&self, channel_id: ChannelId) {
        self.transition(channel_id, None, VoiceChannelEvent::BargeInDetected, false);
    }

    pub(super) fn disconnected(&self, channel_id: ChannelId) {
        self.transition(channel_id, None, VoiceChannelEvent::Disconnected, false);
    }

    /// Record the guild association used for routing and playback without
    /// implying a connection transition. Control/text channels and `/voice
    /// attach` registrations use this path even when no voice join occurs.
    pub(super) fn register_context(&self, channel_id: ChannelId, guild_id: GuildId) {
        self.voice_guilds.insert(channel_id.get(), guild_id);
    }

    pub(super) fn guild_id(&self, channel_id: ChannelId) -> Option<GuildId> {
        self.voice_guilds
            .get(&channel_id.get())
            .map(|entry| *entry.value())
    }

    pub(super) fn channel_ids_for_guild(&self, guild_id: GuildId) -> Vec<u64> {
        self.voice_guilds
            .iter()
            .filter_map(|entry| (*entry.value() == guild_id).then_some(*entry.key()))
            .collect()
    }

    pub(super) fn remove_guild_contexts(&self, guild_id: GuildId) -> Vec<u64> {
        let channel_ids = self.channel_ids_for_guild(guild_id);
        self.voice_guilds
            .retain(|_, registered_guild_id| *registered_guild_id != guild_id);
        channel_ids
    }

    pub(super) fn forget(&self, channel_id: u64) {
        self.states.remove(&channel_id);
    }

    #[cfg(test)]
    pub(super) fn phase(&self, channel_id: ChannelId) -> VoiceChannelPhase {
        self.states
            .get(&channel_id.get())
            .map(|state| state.phase)
            .unwrap_or(VoiceChannelPhase::Idle)
    }
}

impl VoiceBargeInRuntime {
    pub(in crate::services::discord) fn register_voice_context(
        &self,
        control_channel_id: ChannelId,
        guild_id: GuildId,
    ) {
        if self.enabled {
            self.channels.register_context(control_channel_id, guild_id);
        }
    }

    pub(in crate::services::discord) fn voice_join_started(
        &self,
        channel_id: ChannelId,
        guild_id: GuildId,
    ) {
        if self.enabled {
            self.channels.join_started(channel_id, guild_id);
        }
    }

    pub(in crate::services::discord) fn voice_connected(
        &self,
        channel_id: ChannelId,
        guild_id: GuildId,
    ) {
        if self.enabled {
            self.channels.connected(channel_id, guild_id);
        }
    }

    pub(in crate::services::discord) fn voice_disconnected(&self, channel_id: ChannelId) {
        if self.enabled {
            self.channels.disconnected(channel_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopPlayer;

    impl BargeInPlayerStop for NoopPlayer {
        fn stop(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

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

    #[test]
    fn guild_context_registration_does_not_imply_connection() {
        let channels = VoiceChannelStateMachines::new();
        let channel_id = ChannelId::new(42);
        let guild_id = GuildId::new(7);

        channels.register_context(channel_id, guild_id);
        channels.disconnected(channel_id);

        assert_eq!(channels.phase(channel_id), VoiceChannelPhase::Idle);
        assert_eq!(channels.guild_id(channel_id), Some(guild_id));
        assert!(!channels.states.contains_key(&channel_id.get()));
    }

    #[test]
    fn playback_finishes_only_after_every_channel_playback_is_gone() {
        let channels = VoiceChannelStateMachines::new();
        let channel_id = ChannelId::new(42);
        let guild_id = GuildId::new(7);

        channels.connected(channel_id, guild_id);
        channels.playback_started(channel_id);
        channels.spoken_result_playbacks.insert(
            channel_id.get(),
            SpokenResultPlaybackSession {
                id: 1,
                cancellation: CancellationToken::new(),
            },
        );
        channels.playbacks.insert(
            channel_id.get(),
            Arc::new(LivePlaybackSession {
                player: Arc::new(NoopPlayer),
                cancellation: CancellationToken::new(),
                owner: Some(1),
            }),
        );

        channels.playbacks.remove(&channel_id.get());
        channels.playback_finished(channel_id);
        assert_eq!(channels.phase(channel_id), VoiceChannelPhase::Speaking);

        channels.playbacks.insert(
            channel_id.get(),
            Arc::new(LivePlaybackSession {
                player: Arc::new(NoopPlayer),
                cancellation: CancellationToken::new(),
                owner: Some(2),
            }),
        );
        channels.spoken_result_playbacks.remove(&channel_id.get());
        channels.playback_finished(channel_id);
        assert_eq!(channels.phase(channel_id), VoiceChannelPhase::Speaking);

        channels.playbacks.remove(&channel_id.get());
        channels.playback_finished(channel_id);
        assert_eq!(channels.phase(channel_id), VoiceChannelPhase::Connected);
    }

    #[test]
    fn disconnected_channel_can_rejoin() {
        let channels = VoiceChannelStateMachines::new();
        let channel_id = ChannelId::new(42);
        let guild_id = GuildId::new(7);

        channels.connected(channel_id, guild_id);
        channels.disconnected(channel_id);
        channels.join_started(channel_id, guild_id);
        channels.connected(channel_id, guild_id);

        assert_eq!(channels.phase(channel_id), VoiceChannelPhase::Connected);
    }

    #[test]
    fn late_playback_finish_does_not_resurrect_forgotten_channel_state() {
        let channels = VoiceChannelStateMachines::new();
        let channel_id = ChannelId::new(42);
        let guild_id = GuildId::new(7);

        channels.connected(channel_id, guild_id);
        channels.forget(channel_id.get());
        channels.playback_finished(channel_id);

        assert!(!channels.states.contains_key(&channel_id.get()));
    }

    #[test]
    fn guild_cleanup_includes_disconnected_contexts_and_preserves_other_guilds() {
        let channels = VoiceChannelStateMachines::new();
        let first_channel = ChannelId::new(42);
        let second_channel = ChannelId::new(43);
        let other_channel = ChannelId::new(44);
        let guild_id = GuildId::new(7);
        let other_guild_id = GuildId::new(8);

        channels.register_context(first_channel, guild_id);
        channels.connected(second_channel, guild_id);
        channels.disconnected(second_channel);
        channels.register_context(other_channel, other_guild_id);

        let mut removed = channels.remove_guild_contexts(guild_id);
        removed.sort_unstable();
        assert_eq!(removed, vec![first_channel.get(), second_channel.get()]);
        assert_eq!(channels.guild_id(first_channel), None);
        assert_eq!(channels.guild_id(second_channel), None);
        assert_eq!(channels.guild_id(other_channel), Some(other_guild_id));
    }
}
