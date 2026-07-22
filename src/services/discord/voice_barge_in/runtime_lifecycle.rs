use super::*;

impl VoiceBargeInRuntime {
    pub(in crate::services::discord) fn from_voice_config(config: &VoiceConfig) -> Self {
        let default_sensitivity = config.barge_in.sensitivity;
        let conservative_ttl = Duration::from_secs(config.barge_in.conservative_ttl_secs.max(1));
        let stt = if config.enabled {
            Some(VoiceSttRuntime::from_voice_config(config))
        } else {
            None
        };
        let tts = if config.enabled {
            TtsRuntime::from_voice_config(config).ok()
        } else {
            None
        };
        let (progress_tx, _) = broadcast::channel(128);

        Self {
            enabled: config.enabled,
            barge_in_enabled: config.enabled && config.barge_in.enabled,
            sensitivity: SensitivityState::new(default_sensitivity, conservative_ttl),
            acknowledgement: AcknowledgementConfig::from_voice_config(config),
            transcript_dirs: transcript_dirs_from_config(config),
            voice_config_state: RwLock::new(config.clone()),
            spoken_result_language: RwLock::new(config.stt.language.clone()),
            verbose_progress: AtomicBool::new(config.verbose_progress),
            streaming_stt_enabled: AtomicBool::new(
                stt.as_ref().is_some_and(VoiceSttRuntime::is_streaming),
            ),
            stt: RwLock::new(stt),
            streaming_stt: StreamingSttSessions::new(),
            tts: RwLock::new(tts),
            progress_tx,
            channels: VoiceChannelStateMachines::new(),
            id_sequences: VoiceIdSequences::new(),
            config_cache: ConfigSnapshotCache::new(),
            alias_collision_signature: std::sync::Mutex::new(None),
            #[cfg(test)]
            test_state: Arc::new(VoiceBargeInTestState::default()),
        }
    }

    #[allow(dead_code)] // #3034: test-only runtime constructor; no production caller
    pub(in crate::services::discord) fn disabled() -> Self {
        let (progress_tx, _) = broadcast::channel(128);
        Self {
            enabled: false,
            barge_in_enabled: false,
            sensitivity: SensitivityState::disabled(),
            acknowledgement: AcknowledgementConfig::disabled(),
            transcript_dirs: Vec::new(),
            voice_config_state: RwLock::new(VoiceConfig::default()),
            spoken_result_language: RwLock::new(DEFAULT_STT_LANGUAGE.to_string()),
            verbose_progress: AtomicBool::new(false),
            stt: RwLock::new(None),
            streaming_stt: StreamingSttSessions::new(),
            streaming_stt_enabled: AtomicBool::new(false),
            tts: RwLock::new(None),
            progress_tx,
            channels: VoiceChannelStateMachines::new(),
            id_sequences: VoiceIdSequences::new(),
            config_cache: ConfigSnapshotCache::new(),
            alias_collision_signature: std::sync::Mutex::new(None),
            #[cfg(test)]
            test_state: Arc::new(VoiceBargeInTestState::default()),
        }
    }

    pub(super) async fn generate_foreground_ack_text_for_runtime(
        &self,
        transcript: &str,
        language: &str,
        foreground: &EffectiveVoiceForegroundConfig,
        cancel_token: Arc<crate::services::provider::CancelToken>,
    ) -> Option<VoiceForegroundDecision> {
        #[cfg(test)]
        if let Some(decision) = self
            .test_state
            .foreground_decisions
            .lock()
            .expect("voice test foreground decisions lock")
            .pop_front()
        {
            return Some(decision);
        }

        generate_foreground_ack_text(transcript, language, foreground, cancel_token).await
    }

    pub(super) async fn generate_voice_background_result_summary_for_runtime(
        &self,
        background_result: &str,
        language: &str,
        foreground: &EffectiveVoiceForegroundConfig,
        cancel_token: Arc<crate::services::provider::CancelToken>,
    ) -> Option<String> {
        #[cfg(test)]
        if let Some(summary) = self
            .test_state
            .background_result_summaries
            .lock()
            .expect("voice test background summaries lock")
            .pop_front()
        {
            return summary;
        }

        generate_voice_background_result_summary(
            background_result,
            language,
            foreground,
            cancel_token,
        )
        .await
    }

    #[cfg(test)]
    pub(super) fn take_test_background_handoff_outcome(
        &self,
        driver_kind: VoiceBackgroundDriverKind,
        source_channel_id: ChannelId,
        target_channel_id: ChannelId,
        announcement: &crate::voice::prompt::VoiceTranscriptAnnouncement,
        summary: &str,
        message_content: &str,
    ) -> Option<Result<VoiceBackgroundStartOutcome, String>> {
        let outcome = self
            .test_state
            .background_handoff_outcomes
            .lock()
            .expect("voice test background handoff outcomes lock")
            .pop_front()?;
        self.test_state
            .background_starts
            .lock()
            .expect("voice test background starts lock")
            .push(TestVoiceBackgroundStart {
                driver_kind,
                source_channel_id,
                target_channel_id,
                utterance_id: announcement.utterance_id.clone(),
                summary: summary.to_string(),
                message_content: message_content.to_string(),
            });
        Some(outcome)
    }

    #[cfg(test)]
    pub(super) fn take_test_turn_start_outcome(
        &self,
        driver_kind: VoiceBackgroundDriverKind,
        source_channel_id: ChannelId,
        target_channel_id: ChannelId,
        utterance_id: &str,
        message_content: &str,
    ) -> Option<Result<VoiceBackgroundStartOutcome, String>> {
        let outcome = self
            .test_state
            .turn_start_outcomes
            .lock()
            .expect("voice test turn start outcomes lock")
            .pop_front()?;
        self.test_state
            .turn_starts
            .lock()
            .expect("voice test turn starts lock")
            .push(TestVoiceBackgroundStart {
                driver_kind,
                source_channel_id,
                target_channel_id,
                utterance_id: utterance_id.to_string(),
                summary: String::new(),
                message_content: message_content.to_string(),
            });
        Some(outcome)
    }

    pub(in crate::services::discord) fn enabled(&self) -> bool {
        self.enabled
    }

    #[allow(dead_code)] // #3034: test-only mutable-state inspector; no production caller
    pub(in crate::services::discord) async fn runtime_config_snapshot(
        &self,
    ) -> VoiceRuntimeConfigSnapshot {
        let config = self.voice_config_state.read().await;
        let mut snapshot = VoiceRuntimeConfigSnapshot::from(&*config);
        snapshot.verbose_progress = self.verbose_progress_enabled();
        snapshot
    }

    /// #2250: register a CancelToken for an in-flight foreground/voice Codex
    /// or Claude call so explicit-stop barge-in, supersession by a new
    /// utterance, or runtime cleanup can terminate the spawned child.
    pub(super) fn register_inflight_foreground_cancel(
        &self,
        channel_id: ChannelId,
        token: Arc<crate::services::provider::CancelToken>,
    ) {
        self.channels
            .inflight_foreground_cancels
            .entry(channel_id.get())
            .or_default()
            .push(token);
    }

    /// #2250: remove a previously registered CancelToken once the
    /// foreground call has returned (cancelled or completed normally).
    pub(super) fn unregister_inflight_foreground_cancel(
        &self,
        channel_id: ChannelId,
        token: &Arc<crate::services::provider::CancelToken>,
    ) {
        if let Some(mut entry) = self
            .channels
            .inflight_foreground_cancels
            .get_mut(&channel_id.get())
        {
            entry.retain(|existing| !Arc::ptr_eq(existing, token));
        }
        self.channels
            .inflight_foreground_cancels
            .remove_if(&channel_id.get(), |_, value| value.is_empty());
    }

    /// #3910: whether the live STT runtime is in streaming mode. Read
    /// synchronously from an atomic mirror so the per-PCM-tick hook can gate
    /// streaming work without awaiting the `stt` lock. Kept in sync wherever
    /// `stt` is (re)built.
    pub(in crate::services::discord) fn streaming_stt_enabled(&self) -> bool {
        self.streaming_stt_enabled.load(Ordering::Relaxed)
    }

    /// #2250: signal cancellation on every in-flight foreground call for the
    /// given channel. Called by explicit-stop barge-in and supersession
    /// paths so the spawned Codex/Claude child is killed instead of running
    /// to natural exit (ADR #2175).
    pub(in crate::services::discord) fn cancel_inflight_foreground_calls(
        &self,
        channel_id: ChannelId,
        reason: &'static str,
    ) -> usize {
        let Some((_, tokens)) = self
            .channels
            .inflight_foreground_cancels
            .remove(&channel_id.get())
        else {
            return 0;
        };
        let count = tokens.len();
        for token in tokens {
            token.publish_cancel(reason);
            token.cancel_with_tmux_cleanup();
        }
        if count > 0 {
            tracing::info!(
                channel_id = channel_id.get(),
                count,
                reason,
                "voice foreground inflight Codex/Claude calls cancelled (#2250)"
            );
        }
        count
    }

    pub(in crate::services::discord) fn verbose_progress_enabled(&self) -> bool {
        self.verbose_progress.load(Ordering::Relaxed)
    }

    pub(in crate::services::discord) fn set_verbose_progress_enabled(&self, enabled: bool) {
        self.verbose_progress.store(enabled, Ordering::Relaxed);
    }

    pub(super) async fn spoken_result_language(&self) -> String {
        self.spoken_result_language.read().await.clone()
    }

    pub(in crate::services::discord) async fn try_handle_voice_channel_text_reply(
        &self,
        http: &Arc<serenity::http::Http>,
        channel_id: ChannelId,
        text: &str,
    ) -> bool {
        let text = text.trim();
        if text.is_empty() {
            return false;
        }

        let config = self.cached_config().await;
        let Some(target_channel_id) = config.agents.iter().find_map(|agent| {
            agent_voice_matches_channel(agent, channel_id)
                .then(|| agent_voice_background_channel(agent).unwrap_or(channel_id))
        }) else {
            return false;
        };
        drop(config);

        let language = self.spoken_result_language().await;
        let foreground = self
            .resolve_effective_foreground_config(channel_id, target_channel_id)
            .await;
        let cancel_token = Arc::new(crate::services::provider::CancelToken::new());
        // #3911 + #2335 (c): the guard OWNS the registration. It registers the
        // token BEFORE the generate `.await` below and unregisters on drop, so
        // an abort mid-`.await` (shutdown / supervisor abort) still runs the
        // Drop and cannot leak the token in `inflight_foreground_cancels` (a
        // leak would keep `has_inflight_foreground` true and misclassify the
        // next fresh utterance as a barge-in). Keeping the guard alive through
        // the `channel_id.say` HTTP call below still lets a late cancel
        // suppress the now-stale reply.
        let _text_reply_guard =
            InflightForegroundCancelGuard::register(self, channel_id, cancel_token.clone());
        let reply =
            generate_voice_channel_text_reply(text, &language, &foreground, cancel_token.clone())
                .await
                .unwrap_or_else(|| {
                    "지금 보이스 빠른 답변 모델 응답을 만들지 못했어요.".to_string()
                });

        if cancel_token.cancelled.load(Ordering::Relaxed) {
            tracing::info!(
                channel_id = channel_id.get(),
                cancel_source = ?cancel_token.cancel_source(),
                cancel_source_kind = ?cancel_token.cancel_source_kind(),
                stage = "pre_post",
                "voice channel text reply suppressed because cancel won the race (#2335)"
            );
            return true;
        }

        if let Err(error) = channel_id.say(http.as_ref(), reply).await {
            tracing::warn!(
                error = %error,
                channel_id = channel_id.get(),
                foreground_provider = %foreground.provider,
                foreground_model = %foreground.model,
                "failed to send voice channel text reply"
            );
        }
        true
    }

    pub(super) async fn runtime_wake_word_decision(&self, transcript: &str) -> WakeWordDecision {
        let config = self.voice_config_state.read().await;
        wake_word_decision(transcript, &config.wake_words, config.wake_word_required())
    }

    pub(super) async fn apply_dispatcher_command(
        &self,
        channel_id: ChannelId,
        transcript: &str,
    ) -> Option<VoiceBargeInTranscriptOutcome> {
        match parse_voice_command(transcript)? {
            VoiceCommand::Sensitivity(sensitivity) => {
                self.set_sensitivity(sensitivity).await;
                tracing::info!(
                    channel_id = channel_id.get(),
                    sensitivity = ?sensitivity,
                    "voice barge-in sensitivity changed by spoken command"
                );
                Some(VoiceBargeInTranscriptOutcome::SensitivityChanged(
                    sensitivity,
                ))
            }
            VoiceCommand::VerboseProgress(enabled) => {
                self.set_verbose_progress_enabled(enabled);
                tracing::info!(
                    channel_id = channel_id.get(),
                    verbose_progress = enabled,
                    "voice verbose progress changed by spoken command"
                );
                Some(VoiceBargeInTranscriptOutcome::VerboseProgressChanged { enabled })
            }
            VoiceCommand::Language(language) => {
                self.set_runtime_language(language.clone()).await;
                Some(VoiceBargeInTranscriptOutcome::LanguageChanged(language))
            }
            VoiceCommand::TtsVoice(voice) => {
                self.set_runtime_tts_voice(voice.clone()).await;
                Some(VoiceBargeInTranscriptOutcome::TtsVoiceChanged(voice))
            }
            VoiceCommand::VoiceClone { reference } => {
                tracing::info!(
                    channel_id = channel_id.get(),
                    reference = ?reference,
                    "voice clone command accepted for downstream implementation"
                );
                Some(VoiceBargeInTranscriptOutcome::VoiceCloneRequested { reference })
            }
            VoiceCommand::WakeWords(command) => {
                let wake_words = self.apply_wake_word_command(command).await;
                let required = self.voice_config_state.read().await.wake_word_required();
                Some(VoiceBargeInTranscriptOutcome::WakeWordsChanged {
                    required,
                    wake_words,
                })
            }
        }
    }

    // F8 (#2046): 텍스트 디스패처(`!vc <subcommand>`)도 음성 디스패처와 동일하게
    // Language/TtsVoice/VoiceClone/WakeWords 명령을 모두 적용할 수 있도록 setter
    // 들을 노출한다.
    pub(in crate::services::discord) async fn set_runtime_language_external(
        &self,
        language: String,
    ) {
        self.set_runtime_language(language).await;
    }

    pub(in crate::services::discord) async fn set_runtime_tts_voice_external(&self, voice: String) {
        self.set_runtime_tts_voice(voice).await;
    }

    pub(in crate::services::discord) async fn apply_wake_word_command_external(
        &self,
        command: WakeWordCommand,
    ) -> Vec<String> {
        self.apply_wake_word_command(command).await
    }

    pub(super) async fn set_runtime_language(&self, language: String) {
        let config = {
            let mut config = self.voice_config_state.write().await;
            config.stt.language = language.clone();
            config.clone()
        };
        *self.spoken_result_language.write().await = language;
        if self.enabled {
            self.streaming_stt.clear();
            let runtime = VoiceSttRuntime::from_voice_config(&config);
            // #3910: keep the synchronous streaming mirror aligned with the
            // freshly rebuilt runtime before publishing it.
            self.streaming_stt_enabled
                .store(runtime.is_streaming(), Ordering::Relaxed);
            *self.stt.write().await = Some(runtime);
        }
    }

    pub(super) async fn apply_wake_word_command(&self, command: WakeWordCommand) -> Vec<String> {
        let mut config = self.voice_config_state.write().await;
        match command {
            WakeWordCommand::EnableDefault => {
                if config
                    .wake_words
                    .iter()
                    .all(|value| value.trim().is_empty())
                {
                    config.wake_words = vec![DEFAULT_WAKE_WORD.to_string()];
                }
            }
            WakeWordCommand::Disable => {
                config.wake_words.clear();
            }
            WakeWordCommand::Set(wake_words) => {
                config.wake_words = wake_words;
            }
        }
        config.wake_words.clone()
    }

    pub(in crate::services::discord) async fn unregister_voice_guild(&self, guild_id: GuildId) {
        // F7 (#2046): voice_guilds 만 지우면 channel_id 키로 적재된 monitors /
        // playbacks / spoken_result_playbacks / active_voice_routes /
        // deferred_buffers 가 남아 join/leave 반복 시 누수. 같은 guild 의 모든
        // control_channel_id 를 먼저 수집해 채널 단위 state 도 함께 정리한다.
        let stale_channels = self.channels.remove_guild_contexts(guild_id);
        // #3910: outer handles for the leaving channels, collected so the inner
        // WhisperStream sessions can be discarded after the sync teardown loop.
        let mut stranded_stream_sessions: Vec<SttSessionHandle> = Vec::new();
        for channel_id in stale_channels {
            self.channels.disconnected(ChannelId::new(channel_id));
            self.channels.monitors.remove(&channel_id);
            if let Some((_, session)) = self.channels.playbacks.remove(&channel_id) {
                session.cancellation.cancel();
            }
            if let Some((_, session)) = self.channels.spoken_result_playbacks.remove(&channel_id) {
                session.cancellation.cancel();
            }
            // #2250: also abort any in-flight foreground Codex/Claude call so
            // its spawned child does not outlive the guild teardown.
            self.cancel_inflight_foreground_calls(
                ChannelId::new(channel_id),
                "voice_guild_teardown",
            );
            self.channels.active_voice_routes.remove(&channel_id);
            self.channels.deferred_buffers.remove(&channel_id);
            // #3910: a speaker leaving the channel mid-utterance otherwise leaves
            // its streaming-STT session + feed-task bucket stranded in the maps
            // (they were only reaped at utterance completion). Drop the outer
            // bucket + abort pending feed tasks here, and collect the outer
            // handles so the inner stream session is reaped below too.
            stranded_stream_sessions.extend(self.streaming_stt.remove_channel(channel_id));
            self.channels.forget(channel_id);
        }
        // #3910: dropping the outer `SttSessionHandle` alone leaves the matching
        // inner `WhisperStream` session (inserted by `start_session`, removed
        // only by `finalize()`) leaked until the runtime is rebuilt. Discard
        // those inner sessions for the leaving channels (no final decode — the
        // speaker left, so partial-transcript loss is acceptable). File mode
        // keeps no inner session, so this is a no-op there.
        if !stranded_stream_sessions.is_empty() {
            // Hoist read+clone into a local so the `RwLockReadGuard` drops at
            // this statement's end — NOT held across the `discard_stream_session`
            // awaits below (an `if let` scrutinee would extend the guard over the
            // whole body, holding `self.stt` read while awaiting → deadlock risk
            // against any `self.stt` writer / re-entrant lock). A plain `let`
            // binding (not `let-else` + `return`) preserves the rest of the fn.
            let stt = self.stt.read().await.clone();
            if let Some(stt) = stt {
                for handle in stranded_stream_sessions {
                    stt.discard_stream_session(&handle).await;
                }
            }
        }
    }

    /// F2 (#2046): 특정 길드에 매핑된 control_channel_id 목록을 반환.
    /// `leave_voice_channel` 경로에서 `VoiceReceiver::flush_for_control_channel`을
    /// 길드 단위로 한정 호출하기 위해 사용한다.
    pub(in crate::services::discord) fn control_channel_ids_for_guild(
        &self,
        guild_id: GuildId,
    ) -> Vec<u64> {
        self.channels.channel_ids_for_guild(guild_id)
    }

    pub(in crate::services::discord) fn spawn_sensitivity_ttl_reset(
        self: &Arc<Self>,
        shutdown_flag: Arc<AtomicBool>,
    ) {
        if !self.barge_in_enabled {
            return;
        }

        let state = self.sensitivity.state_handle();
        let token = CancellationToken::new();
        let reset_token = token.clone();
        tokio::spawn(run_sensitivity_ttl_reset(state, reset_token));
        // F21 (#2046): shutdown_flag 폴링 주기를 1초 → 5초 로 늘려 cpu wakeup 비용을
        // 1/5 로 줄인다. shutdown 전체 latency 가 최대 5초 늘지만 sensitivity TTL
        // 자체가 분 단위 주기라 실효 영향이 거의 없다. (Full CancellationToken
        // 통합은 SharedData 차원 리팩토링이라 follow-up 으로 남긴다.)
        tokio::spawn(async move {
            while !shutdown_flag.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            token.cancel();
        });
    }

    pub(in crate::services::discord) async fn set_sensitivity(
        &self,
        sensitivity: BargeInSensitivity,
    ) {
        self.sensitivity.set(sensitivity).await;
        self.update_existing_monitor_sensitivity(sensitivity);
    }

    #[allow(dead_code)] // #3034: test-only voice-command entry point; no production caller
    pub(in crate::services::discord) async fn apply_voice_command(
        &self,
        transcript: &str,
    ) -> Option<BargeInSensitivity> {
        if !self.barge_in_enabled {
            return None;
        }
        let sensitivity = self.sensitivity.apply_voice_command(transcript).await?;
        self.update_existing_monitor_sensitivity(sensitivity);
        Some(sensitivity)
    }

    pub(in crate::services::discord) async fn handle_processing_transcript(
        &self,
        shared: &Arc<SharedData>,
        _provider: &ProviderKind,
        channel_id: ChannelId,
        transcript: &str,
    ) -> VoiceBargeInTranscriptOutcome {
        if !self.enabled {
            return VoiceBargeInTranscriptOutcome::Disabled;
        }

        let transcript = transcript.trim();
        if transcript.is_empty() {
            return VoiceBargeInTranscriptOutcome::EmptyTranscript;
        }

        if !self.barge_in_enabled {
            return VoiceBargeInTranscriptOutcome::BargeInDisabled;
        }

        if let Some(outcome) = self.apply_dispatcher_command(channel_id, transcript).await {
            return outcome;
        }

        // #2250: in-flight foreground Codex/Claude calls are also
        // cancellable "active work" — do not bail with NoActiveTurn if the
        // only active work is a foreground call, otherwise barge-in cannot
        // reach the registered cancel token.
        let has_inflight_foreground = self
            .channels
            .inflight_foreground_cancels
            .get(&channel_id.get())
            .is_some_and(|entry| !entry.value().is_empty());
        let cancel_channel = self
            .active_barge_in_mailbox_channel(shared, channel_id)
            .await;
        if cancel_channel.is_none() && !has_inflight_foreground {
            return VoiceBargeInTranscriptOutcome::NoActiveTurn;
        }

        let buffer = self.buffer_for_channel(channel_id);
        let decision = buffer
            .lock()
            .await
            .verify_processing_barge_in_after_stt(transcript);
        match decision {
            ProcessingBargeInDecision::AbortAgent => {
                self.channels.barged_in(channel_id);
                // #2250: also cancel any in-flight foreground/voice Codex
                // call so its child process is killed mid-flight, not just
                // the background turn.
                let inflight_cancelled = self
                    .cancel_inflight_foreground_calls(channel_id, "voice_barge_in_explicit_stop");
                let _ = inflight_cancelled;
                let cancel_channel = cancel_channel.unwrap_or(channel_id);
                let result = super::mailbox_cancel_active_turn_with_reason(
                    shared,
                    cancel_channel,
                    "voice_barge_in_explicit_stop",
                )
                .await;
                // F22 (#2046): 사후 분석 라벨 강화. transcript 글자 수, 현재
                // sensitivity, 활성 progress playback 보유 여부.
                let sensitivity = self.current_sensitivity();
                let playback_active = self.channels.playbacks.contains_key(&channel_id.get());
                tracing::info!(
                    channel_id = channel_id.get(),
                    cancel_channel_id = cancel_channel.get(),
                    cancelled = result.token.is_some(),
                    already_stopping = result.already_stopping,
                    transcript_chars = transcript.chars().count(),
                    sensitivity = ?sensitivity,
                    playback_active,
                    "voice explicit-stop barge-in processed"
                );
                VoiceBargeInTranscriptOutcome::ExplicitStop {
                    cancelled: result.token.is_some(),
                    already_stopping: result.already_stopping,
                    cancel_channel_id: cancel_channel.get(),
                }
            }
            ProcessingBargeInDecision::DeferPrompt(prompt) => {
                tracing::info!(
                    channel_id = channel_id.get(),
                    "voice processing barge-in deferred for next turn"
                );
                VoiceBargeInTranscriptOutcome::Deferred(prompt)
            }
            ProcessingBargeInDecision::IgnoreNoise => VoiceBargeInTranscriptOutcome::IgnoredNoise,
        }
    }
}
