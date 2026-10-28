use super::*;

fn utterance_source_config(channel_id: ChannelId) -> (crate::config::Config, ChannelId) {
    let config = crate::config::runtime_config();
    let source_channel_id = effective_voice_source_channel(&config, channel_id);
    (config, source_channel_id)
}

#[cfg(test)]
#[path = "utterance_snapshot_tests.rs"]
mod snapshot_reader_tests;

impl VoiceBargeInRuntime {
    /// Reuse the voice cache for five seconds, then refresh from the runtime config.
    /// Blocking standalone fallback and join-failure defaults retain their existing behavior.
    pub(super) async fn cached_config(&self) -> Arc<crate::config::Config> {
        let now = Instant::now();
        if let Some(cached) = self.config_cache.lookup_within_ttl(now) {
            return cached;
        }
        let fresh = tokio::task::spawn_blocking(crate::config::runtime_config)
            .await
            .unwrap_or_else(|_| crate::config::Config::default());
        let arc = Arc::new(fresh);
        self.config_cache.store(Instant::now(), arc.clone());
        arc
    }

    pub(in crate::services::discord) async fn process_completed_utterance(
        self: &Arc<Self>,
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel_id: ChannelId,
        utterance: &CompletedUtterance,
    ) -> VoiceBargeInTranscriptOutcome {
        if !self.enabled {
            return VoiceBargeInTranscriptOutcome::Disabled;
        }

        let transcribed = match self
            .transcribe_completed_utterance(channel_id, utterance)
            .await
        {
            Some(transcript) => transcript,
            None => return VoiceBargeInTranscriptOutcome::TranscriptUnavailable,
        };

        let transcript = transcribed.text.trim();
        let (config_snapshot, source_channel_id) = utterance_source_config(channel_id);
        let flight_context = VoiceFlightUtteranceContext::from_utterance(
            source_channel_id,
            utterance,
            transcript,
            &transcribed,
        );
        if transcript.is_empty() {
            let mut event = flight_context.event(VoiceFlightRoute::IgnoredNoise);
            event.reason = Some("empty_transcript".to_string());
            record_voice_flight_event(event);
            return VoiceBargeInTranscriptOutcome::EmptyTranscript;
        }

        let source_is_agent_voice = config_snapshot
            .agents
            .iter()
            .any(|agent| agent_voice_matches_channel(agent, source_channel_id));
        let source_is_lobby = super::settings::resolve_role_binding(source_channel_id, None)
            .is_none()
            && voice_lobby_accepts_source_channel(&config_snapshot.voice, source_channel_id);
        let transcript = if source_is_agent_voice || source_is_lobby {
            transcript.to_string()
        } else {
            match self.runtime_wake_word_decision(transcript).await {
                WakeWordDecision::NotRequired(transcript) => transcript,
                WakeWordDecision::Matched(matched) => matched.remaining,
                WakeWordDecision::Missing => {
                    let mut event = flight_context.event(VoiceFlightRoute::IgnoredNoise);
                    event.reason = Some("wake_word_required".to_string());
                    record_voice_flight_event(event);
                    return VoiceBargeInTranscriptOutcome::WakeWordRequired;
                }
            }
        };
        let transcript = transcript.trim();
        if transcript.is_empty() {
            let mut event = flight_context.event(VoiceFlightRoute::IgnoredNoise);
            event.reason = Some("empty_after_wake_word".to_string());
            record_voice_flight_event(event);
            return VoiceBargeInTranscriptOutcome::EmptyTranscript;
        }

        // #2250: also treat in-flight foreground Codex/Claude calls as
        // active work for barge-in purposes. Otherwise a barge-in arriving
        // while we are still generating the ack / channel-text / summary
        // would bypass `handle_processing_transcript` and never cancel the
        // spawned child.
        let has_inflight_foreground = self
            .channels
            .inflight_foreground_cancels
            .get(&source_channel_id.get())
            .is_some_and(|entry| !entry.value().is_empty());
        if self
            .active_barge_in_mailbox_channel(shared, source_channel_id)
            .await
            .is_some()
            || has_inflight_foreground
        {
            let outcome = self
                .handle_processing_transcript(shared, provider, source_channel_id, transcript)
                .await;
            let mut event = match &outcome {
                VoiceBargeInTranscriptOutcome::ExplicitStop {
                    cancelled,
                    already_stopping,
                    cancel_channel_id,
                } => {
                    let mut event = flight_context.event(VoiceFlightRoute::ExplicitStop);
                    event.cancel_channel_id = Some(*cancel_channel_id);
                    event.barge_in = Some(true);
                    event.cancel_source = Some("voice_barge_in_explicit_stop".to_string());
                    event.cancelled = Some(*cancelled);
                    event.already_stopping = Some(*already_stopping);
                    event
                }
                VoiceBargeInTranscriptOutcome::Deferred(_) => {
                    let mut event = flight_context.event(VoiceFlightRoute::Deferred);
                    event.barge_in = Some(true);
                    event.reason = Some("processing_barge_in_defer".to_string());
                    event
                }
                VoiceBargeInTranscriptOutcome::IgnoredNoise => {
                    let mut event = flight_context.event(VoiceFlightRoute::IgnoredNoise);
                    event.barge_in = Some(true);
                    event.reason = Some("processing_barge_in_noise".to_string());
                    event
                }
                _ => {
                    let mut event = flight_context.event(VoiceFlightRoute::IgnoredNoise);
                    event.reason = Some("processing_barge_in_no_route".to_string());
                    event
                }
            };
            if let Some(route) = self
                .channels
                .active_voice_routes
                .get(&source_channel_id.get())
            {
                event.agent_id = Some(route.agent_id.clone());
                event.background_channel_id = Some(route.channel_id.get());
            }
            record_voice_flight_event(event);
            return outcome;
        }

        if let Some(outcome) = self
            .apply_dispatcher_command(source_channel_id, transcript)
            .await
        {
            return outcome;
        }

        let (target_channel_id, transcript) = match self
            .resolve_voice_turn_target(shared, source_channel_id, transcript)
            .await
        {
            VoiceTurnTargetResolution::Target {
                channel_id,
                transcript,
            } => (channel_id, transcript),
            VoiceTurnTargetResolution::NeedsAgent => {
                let mut event = flight_context.event(VoiceFlightRoute::Deferred);
                event.reason = Some("agent_routing_required".to_string());
                record_voice_flight_event(event);
                self.ask_for_agent(shared, channel_id).await;
                return VoiceBargeInTranscriptOutcome::AgentRoutingRequired;
            }
            VoiceTurnTargetResolution::Ignored => {
                let mut event = flight_context.event(VoiceFlightRoute::IgnoredNoise);
                event.reason = Some("no_active_voice_route".to_string());
                record_voice_flight_event(event);
                return VoiceBargeInTranscriptOutcome::NoActiveTurn;
            }
        };

        // #3906 (P1): deterministic intake ack — Phase-1 chime BEFORE
        // start_voice_turn, so it survives every VoiceTurnStartFailed/#3905 drop.
        self.play_processing_chime(shared, source_channel_id).await;
        self.start_voice_turn(
            shared,
            source_channel_id,
            target_channel_id,
            utterance,
            &transcript,
            &flight_context,
        )
        .await
    }

    pub(in crate::services::discord) async fn drain_deferred_after_turn(
        self: &Arc<Self>,
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel_id: ChannelId,
    ) -> bool {
        if !self.deferred_drainable() {
            return false;
        }
        let Some(drain) = self.take_deferred_prompt(channel_id).await else {
            return false;
        };
        self.enqueue_deferred(shared, provider, channel_id, drain)
            .await
    }

    /// Whether a finished turn may drain its deferred prompt now.
    pub(in crate::services::discord) fn deferred_drainable(&self) -> bool {
        self.barge_in_enabled
    }

    /// Takes the channel's buffer out of the map, so a later utterance starts a fresh one.
    pub(in crate::services::discord) fn detach_deferred(
        &self,
        channel_id: ChannelId,
    ) -> Option<Arc<Mutex<DeferredBargeInBuffer>>> {
        let detached = self.channels.deferred_buffers.remove(&channel_id.get());
        detached.map(|(_, buffer)| buffer)
    }

    /// Drains a buffer already detached for this channel and enqueues its prompt once.
    pub(in crate::services::discord) async fn drain_detached(
        self: &Arc<Self>,
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel_id: ChannelId,
        buffer: Arc<Mutex<DeferredBargeInBuffer>>,
    ) -> bool {
        let Some(drain) = self.drain_buffer(&buffer).await else {
            return false;
        };
        self.enqueue_deferred(shared, provider, channel_id, drain)
            .await
    }

    async fn enqueue_deferred(
        self: &Arc<Self>,
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel_id: ChannelId,
        drain: DeferredBargeInDrain,
    ) -> bool {
        if let Some(acknowledgement) = drain.acknowledgement {
            if let Some(path) = self
                .synthesize_acknowledgement(&acknowledgement, channel_id)
                .await
            {
                self.play_acknowledgement(shared, channel_id, path).await;
            }
        }

        let message_id = MessageId::new(self.id_sequences.next_internal_message_id());
        super::enqueue_internal_followup(
            shared,
            provider,
            channel_id,
            message_id,
            drain.prompt,
            "voice barge-in deferred prompt",
        )
        .await
    }

    pub(super) async fn take_deferred_prompt(
        &self,
        channel_id: ChannelId,
    ) -> Option<DeferredBargeInDrain> {
        let buffer = self
            .channels
            .deferred_buffers
            .get(&channel_id.get())
            .map(|entry| entry.value().clone())?;
        self.drain_buffer(&buffer).await
    }

    async fn drain_buffer(
        &self,
        buffer: &Mutex<DeferredBargeInBuffer>,
    ) -> Option<DeferredBargeInDrain> {
        let mut buffer = buffer.lock().await;
        let ack = &self.acknowledgement;
        let acknowledgement = buffer
            .acknowledgement_before_drain(ack.enabled(), ack.text())
            .map(ToOwned::to_owned);
        let prompt = buffer.drain_prompt()?;
        Some(DeferredBargeInDrain {
            acknowledgement,
            prompt,
        })
    }

    /// Defers `text` for `channel` as a processing barge-in would.
    #[cfg(test)]
    pub(in crate::services::discord) async fn defer_for_test(
        &self,
        channel: ChannelId,
        text: &str,
    ) {
        let buffer = self.buffer_for_channel(channel);
        assert!(buffer.lock().await.push_transcript(text));
    }

    /// The channel's current deferred buffer, if one is in the map.
    #[cfg(test)]
    pub(in crate::services::discord) fn deferred_for_test(
        &self,
        channel: ChannelId,
    ) -> Option<Arc<Mutex<DeferredBargeInBuffer>>> {
        let buffer = self.channels.deferred_buffers.get(&channel.get());
        buffer.map(|entry| entry.value().clone())
    }

    pub(super) fn buffer_for_channel(
        &self,
        channel_id: ChannelId,
    ) -> Arc<Mutex<DeferredBargeInBuffer>> {
        self.channels
            .deferred_buffers
            .entry(channel_id.get())
            .or_insert_with(|| Arc::new(Mutex::new(DeferredBargeInBuffer::new())))
            .clone()
    }
}
