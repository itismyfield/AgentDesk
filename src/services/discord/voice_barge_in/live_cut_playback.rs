use super::*;

impl VoiceBargeInRuntime {
    #[allow(dead_code)] // #3034: test-only playback-reset; prod uses reset_after_playback_start_with_owner
    pub(in crate::services::discord) fn reset_after_playback_start<P>(
        &self,
        channel_id: ChannelId,
        player: Arc<P>,
        cancellation: CancellationToken,
    ) where
        P: BargeInPlayerStop + 'static,
    {
        self.reset_after_playback_start_with_owner(channel_id, player, cancellation, None);
    }

    pub(super) fn reset_after_playback_start_with_owner<P>(
        &self,
        channel_id: ChannelId,
        player: Arc<P>,
        cancellation: CancellationToken,
        owner: Option<u64>,
    ) where
        P: BargeInPlayerStop + 'static,
    {
        if !self.barge_in_enabled {
            return;
        }

        let sensitivity = self.current_sensitivity();
        let monitor = self.monitor_for_channel(channel_id, sensitivity);
        {
            let mut monitor = lock_monitor(&monitor);
            monitor.set_sensitivity(sensitivity);
            monitor.reset_after_playback_start();
        }

        let player: Arc<dyn BargeInPlayerStop> = player;
        self.playbacks.insert(
            channel_id.get(),
            Arc::new(LivePlaybackSession {
                player,
                cancellation,
                owner,
            }),
        );
    }

    #[allow(dead_code)] // #3034: test-only playback clear; prod uses clear_playback_if_owner
    pub(in crate::services::discord) fn clear_playback(&self, channel_id: ChannelId) {
        self.playbacks.remove(&channel_id.get());
    }

    pub(super) fn clear_playback_if_owner(&self, channel_id: ChannelId, owner: u64) {
        self.playbacks
            .remove_if(&channel_id.get(), |_, session| session.owner == Some(owner));
    }

    /// #3908: register a progress/chime playback as the live barge-in handle,
    /// but ONLY when no final-result playback owns the channel. A queued
    /// progress flush that lands AFTER the final answer started must not steal
    /// the final-result barge-in handle — otherwise a user barge-in would stop
    /// only this progress track and leave the (nested) final answer playing.
    /// The progress audio still mixes via songbird `play_input`; we only skip
    /// the handle bookkeeping. Returns `true` when the handle was registered
    /// (caller arms the expiry timer), `false` when skipped.
    pub(super) fn register_progress_barge_in_handle<P>(
        &self,
        channel_id: ChannelId,
        player: Arc<P>,
        playback_id: u64,
    ) -> bool
    where
        P: BargeInPlayerStop + 'static,
    {
        if self.spoken_result_playbacks.contains_key(&channel_id.get()) {
            tracing::debug!(
                channel_id = channel_id.get(),
                playback_id,
                "voice progress playback left unregistered: final-result playback owns the barge-in handle (#3908)"
            );
            return false;
        }
        self.reset_after_playback_start_with_owner(
            channel_id,
            player,
            CancellationToken::new(),
            Some(playback_id),
        );
        true
    }

    /// Returns the detected cut alongside the `owner` of the playback session
    /// that was actually cut. F22 (#2046) diagnostics log `playback_owner`, but
    /// the caller used to read it from `self.playbacks` AFTER this method had
    /// already removed the entry on a cut, so it was always `None` (#3914). The
    /// owner is therefore snapshotted here, from the very session being stopped.
    pub(in crate::services::discord) fn observe_live_pcm_i16(
        &self,
        channel_id: ChannelId,
        samples: &[i16],
    ) -> Option<(LiveBargeInCut, Option<u64>)> {
        if !self.barge_in_enabled || samples.is_empty() {
            return None;
        }

        let playback = self
            .playbacks
            .get(&channel_id.get())
            .map(|entry| entry.value().clone())?;
        let playback_owner = playback.owner;
        let sensitivity = self.current_sensitivity();
        let monitor = self.monitor_for_channel(channel_id, sensitivity);
        let mut monitor = lock_monitor(&monitor);
        monitor.set_sensitivity(sensitivity);

        let pcm = pcm_i16_to_le_bytes(samples);
        match monitor.observe_pcm(&pcm, playback.player.as_ref(), &playback.cancellation) {
            Ok(Some(cut)) => {
                self.playbacks.remove(&channel_id.get());
                Some((cut, playback_owner))
            }
            Ok(None) => None,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    channel_id = channel_id.get(),
                    "voice live barge-in stop failed"
                );
                None
            }
        }
    }

    fn monitor_for_channel(
        &self,
        channel_id: ChannelId,
        sensitivity: BargeInSensitivity,
    ) -> Arc<std::sync::Mutex<LiveBargeInMonitor>> {
        self.monitors
            .entry(channel_id.get())
            .or_insert_with(|| {
                Arc::new(std::sync::Mutex::new(LiveBargeInMonitor::new(sensitivity)))
            })
            .clone()
    }

    pub(super) fn current_sensitivity(&self) -> BargeInSensitivity {
        // F18 (#2046): try_read 실패 시 boot-time default 가 아닌 가장 최근에
        // 설정된 sensitivity 를 반환하도록 atomic mirror 로 폴백한다. TTL reset
        // 이 일어나는 짧은 윈도우라도 사용자가 설정한 Conservative 가 잠깐
        // Normal 로 평가되는 회귀를 막는다.
        self.sensitivity.current()
    }

    pub(super) fn update_existing_monitor_sensitivity(&self, sensitivity: BargeInSensitivity) {
        for monitor in &self.monitors {
            lock_monitor(monitor.value()).set_sensitivity(sensitivity);
        }
    }
}
