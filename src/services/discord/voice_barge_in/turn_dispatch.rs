use super::*;

pub(super) fn voice_background_handoff_ack(language: &str) -> &'static str {
    if language.trim().to_ascii_lowercase().starts_with("en") {
        "I will hand that to the background agent."
    } else {
        "백그라운드 에이전트로 넘길게요."
    }
}

impl VoiceBargeInRuntime {
    pub(in crate::services::discord) async fn try_handle_voice_transcript_announcement(
        self: &Arc<Self>,
        shared: &Arc<SharedData>,
        source_channel_id: ChannelId,
        announcement: &crate::voice::prompt::VoiceTranscriptAnnouncement,
    ) -> bool {
        if !self.enabled {
            return false;
        }
        let Some(target_channel_id) = self
            .resolve_voice_background_channel_for_source(source_channel_id)
            .await
        else {
            tracing::warn!(
                source_channel_id = source_channel_id.get(),
                utterance_id = %announcement.utterance_id,
                "voice foreground handoff skipped because no background channel is mapped"
            );
            return false;
        };

        let started = Instant::now();
        let language = announcement.language.clone();
        let foreground = self
            .resolve_effective_foreground_config(source_channel_id, target_channel_id)
            .await;
        // #3906 (P1): redundant foreground-start chime removed — superseded by the
        // deterministic Phase-1 intake chime in process_completed_utterance.
        let cancel_token = Arc::new(crate::services::provider::CancelToken::new());
        // #3911 + #2335 (c): the guard OWNS the registration. It registers the
        // token BEFORE the generate `.await` below and unregisters on drop, so
        // an abort mid-`.await` (shutdown / supervisor abort) cannot leak the
        // token in `inflight_foreground_cancels` (a leak would keep
        // `has_inflight_foreground` permanently true and misroute the next
        // fresh utterance as a barge-in). Keeping the guard alive through every
        // suppressible side effect below (synth, play, background dispatch)
        // still lets a late cancel flip this token and suppress the stale
        // ack/handoff; we re-check the cancel flag at each awaited boundary.
        let _inflight_guard =
            InflightForegroundCancelGuard::register(self, source_channel_id, cancel_token.clone());
        let decision = self
            .generate_foreground_ack_text_for_runtime(
                &announcement.transcript,
                &language,
                &foreground,
                cancel_token.clone(),
            )
            .await
            .unwrap_or(VoiceForegroundDecision::Silence);
        let foreground_latency_ms = started.elapsed().as_millis().min(u64::MAX as u128) as u64;

        let record_cancel_suppressed = |label: &'static str| {
            let mut event = voice_flight_event_from_announcement(
                VoiceFlightRoute::ExplicitStop,
                source_channel_id,
                Some(target_channel_id),
                announcement,
            );
            attach_foreground_flight_metadata(
                &mut event,
                &foreground,
                foreground_latency_ms,
                "cancelled",
            );
            event.cancel_source = cancel_token
                .cancel_source()
                .or_else(|| Some(label.to_string()));
            event.cancel_channel_id = Some(target_channel_id.get());
            event.cancelled = Some(true);
            event.reason = Some(label.to_string());
            record_voice_flight_event(event);
        };

        let log_cancel_suppressed = |label: &'static str| {
            tracing::info!(
                source_channel_id = source_channel_id.get(),
                target_channel_id = target_channel_id.get(),
                utterance_id = %announcement.utterance_id,
                cancel_source = ?cancel_token.cancel_source(),
                cancel_source_kind = ?cancel_token.cancel_source_kind(),
                stage = label,
                "voice foreground side effect suppressed because cancel won the race (#2335)"
            );
        };

        if foreground_decision::ack_cancel_suppresses_fallback(&cancel_token) {
            record_cancel_suppressed("post_generation");
            log_cancel_suppressed("post_generation");
            return true;
        }

        match decision {
            VoiceForegroundDecision::Silence => {
                let mut event = voice_flight_event_from_announcement(
                    VoiceFlightRoute::ForegroundSilence,
                    source_channel_id,
                    Some(target_channel_id),
                    announcement,
                );
                attach_foreground_flight_metadata(
                    &mut event,
                    &foreground,
                    foreground_latency_ms,
                    "silence",
                );
                record_voice_flight_event(event);
                tracing::info!(
                    source_channel_id = source_channel_id.get(),
                    target_channel_id = target_channel_id.get(),
                    utterance_id = %announcement.utterance_id,
                    elapsed_ms = started.elapsed().as_millis(),
                    foreground_provider = %foreground.provider,
                    foreground_model = %foreground.model,
                    "voice foreground chose silence"
                );
            }
            VoiceForegroundDecision::Speak(spoken) => {
                if foreground_decision::ack_cancel_suppresses_fallback(&cancel_token) {
                    record_cancel_suppressed("pre_speak_synth");
                    log_cancel_suppressed("pre_speak_synth");
                    return true;
                }
                if let Some(path) = self
                    .synthesize_acknowledgement(&spoken, source_channel_id)
                    .await
                {
                    if foreground_decision::ack_cancel_suppresses_fallback(&cancel_token) {
                        record_cancel_suppressed("post_speak_synth");
                        log_cancel_suppressed("post_speak_synth");
                        return true;
                    }
                    self.play_acknowledgement(shared, source_channel_id, path)
                        .await;
                }
                let mut event = voice_flight_event_from_announcement(
                    VoiceFlightRoute::ForegroundSpeak,
                    source_channel_id,
                    Some(target_channel_id),
                    announcement,
                );
                attach_foreground_flight_metadata(
                    &mut event,
                    &foreground,
                    foreground_latency_ms,
                    "speak",
                );
                event.tts_chars = Some(spoken.chars().count());
                record_voice_flight_event(event);
                tracing::info!(
                    source_channel_id = source_channel_id.get(),
                    target_channel_id = target_channel_id.get(),
                    utterance_id = %announcement.utterance_id,
                    elapsed_ms = started.elapsed().as_millis(),
                    foreground_provider = %foreground.provider,
                    foreground_model = %foreground.model,
                    "voice foreground spoken response queued"
                );
            }
            VoiceForegroundDecision::HandoffBackground(summary) => {
                if cancel_token.cancelled.load(Ordering::Relaxed) {
                    record_cancel_suppressed("pre_background_handoff");
                    log_cancel_suppressed("pre_background_handoff");
                    return true;
                }
                match self
                    .dispatch_voice_background_handoff(
                        shared,
                        source_channel_id,
                        target_channel_id,
                        announcement,
                        &summary,
                    )
                    .await
                {
                    Ok(handoff_outcome) => {
                        let VoiceBackgroundHandoffOutcome {
                            turn_id,
                            handoff_message_id,
                            correlation_id,
                        } = handoff_outcome;
                        let tombstone = handoff_message_id.and_then(|id| {
                            crate::voice::cancel_tombstone::global_store().lookup(id)
                        });
                        if let Some(observation) =
                            observe_voice_handoff_cancel(&cancel_token, tombstone)
                        {
                            record_and_cancel_voice_handoff_if_observed(
                                shared,
                                source_channel_id,
                                target_channel_id,
                                &turn_id,
                                handoff_message_id,
                                observation.clone(),
                            )
                            .await;
                            let mut event = voice_flight_event_from_announcement(
                                VoiceFlightRoute::ExplicitStop,
                                source_channel_id,
                                Some(target_channel_id),
                                announcement,
                            );
                            attach_foreground_flight_metadata(
                                &mut event,
                                &foreground,
                                foreground_latency_ms,
                                "handoff_cancelled",
                            );
                            event.handoff_correlation_id = Some(correlation_id.clone());
                            event.handoff_message_id = handoff_message_id.map(|id| id.get());
                            event.background_turn_id = Some(turn_id.clone());
                            event.cancel_source = Some(observation.cancel_reason);
                            event.cancel_channel_id = Some(target_channel_id.get());
                            event.cancelled = Some(true);
                            event.reason = Some("post_background_handoff_started".to_string());
                            record_voice_flight_event(event);
                            log_cancel_suppressed("post_background_handoff_started");
                            return true;
                        }
                        let ack = voice_background_handoff_ack(&language);
                        let ack_path = self
                            .synthesize_acknowledgement(ack, source_channel_id)
                            .await;
                        // #2403: re-use the same cancel/tombstone handling
                        // after synthesis. A stop arriving during TTS must
                        // suppress the spoken ack AND cancel the just-started
                        // target turn, not merely return before playback.
                        let tombstone_after_synth = handoff_message_id.and_then(|id| {
                            crate::voice::cancel_tombstone::global_store().lookup(id)
                        });
                        if let Some(observation) =
                            observe_voice_handoff_cancel(&cancel_token, tombstone_after_synth)
                        {
                            record_and_cancel_voice_handoff_if_observed(
                                shared,
                                source_channel_id,
                                target_channel_id,
                                &turn_id,
                                handoff_message_id,
                                observation.clone(),
                            )
                            .await;
                            let mut event = voice_flight_event_from_announcement(
                                VoiceFlightRoute::ExplicitStop,
                                source_channel_id,
                                Some(target_channel_id),
                                announcement,
                            );
                            attach_foreground_flight_metadata(
                                &mut event,
                                &foreground,
                                foreground_latency_ms,
                                "handoff_cancelled",
                            );
                            event.handoff_correlation_id = Some(correlation_id.clone());
                            event.handoff_message_id = handoff_message_id.map(|id| id.get());
                            event.background_turn_id = Some(turn_id.clone());
                            event.cancel_source = Some(observation.cancel_reason);
                            event.cancel_channel_id = Some(target_channel_id.get());
                            event.cancelled = Some(true);
                            event.reason = Some("post_background_handoff_play".to_string());
                            record_voice_flight_event(event);
                            log_cancel_suppressed("post_background_handoff_play");
                            return true;
                        }
                        if let Some(path) = ack_path {
                            self.play_acknowledgement(shared, source_channel_id, path)
                                .await;
                        }
                        let mut event = voice_flight_event_from_announcement(
                            VoiceFlightRoute::BackgroundHandoff,
                            source_channel_id,
                            Some(target_channel_id),
                            announcement,
                        );
                        attach_foreground_flight_metadata(
                            &mut event,
                            &foreground,
                            foreground_latency_ms,
                            "handoff_background",
                        );
                        event.handoff_correlation_id = Some(correlation_id.clone());
                        event.handoff_message_id = handoff_message_id.map(|id| id.get());
                        event.background_turn_id = Some(turn_id.clone());
                        event.tts_chars = Some(ack.chars().count());
                        record_voice_flight_event(event);
                        tracing::info!(
                            source_channel_id = source_channel_id.get(),
                            target_channel_id = target_channel_id.get(),
                            utterance_id = %announcement.utterance_id,
                            turn_id = %turn_id,
                            elapsed_ms = started.elapsed().as_millis(),
                            foreground_provider = %foreground.provider,
                            foreground_model = %foreground.model,
                            "voice foreground handed request to background"
                        );
                    }
                    Err(error) => {
                        let mut event = voice_flight_event_from_announcement(
                            VoiceFlightRoute::BackgroundHandoff,
                            source_channel_id,
                            Some(target_channel_id),
                            announcement,
                        );
                        attach_foreground_flight_metadata(
                            &mut event,
                            &foreground,
                            foreground_latency_ms,
                            "handoff_background",
                        );
                        event.reason = Some(format!("handoff_failed:{error}"));
                        record_voice_flight_event(event);
                        tracing::warn!(
                            error = %error,
                            source_channel_id = source_channel_id.get(),
                            target_channel_id = target_channel_id.get(),
                            utterance_id = %announcement.utterance_id,
                            "voice foreground background handoff failed"
                        );
                    }
                }
            }
        }
        true
    }

    pub(super) async fn dispatch_voice_background_handoff(
        &self,
        shared: &Arc<SharedData>,
        source_channel_id: ChannelId,
        target_channel_id: ChannelId,
        announcement: &crate::voice::prompt::VoiceTranscriptAnnouncement,
        summary: &str,
    ) -> Result<VoiceBackgroundHandoffOutcome, String> {
        let driver = select_voice_background_driver();
        let guild_id = self.voice_turn_guild_id(source_channel_id, target_channel_id);
        let prompt = build_voice_background_handoff_prompt(
            &announcement.transcript,
            summary,
            &announcement.language,
        );
        let correlation_id = crate::voice::prompt::new_voice_background_handoff_correlation_id();
        let prompt =
            crate::voice::prompt::append_voice_background_handoff_marker(&prompt, &correlation_id);
        let generation = default_voice_announce_generation() + 1;
        let agent_id = self
            .channels
            .active_voice_routes
            .get(&source_channel_id.get())
            .map(|entry| entry.agent_id.clone());
        let meta = crate::voice::announce_meta::VoiceBackgroundHandoffMeta {
            voice_channel_id: source_channel_id.get(),
            background_channel_id: target_channel_id.get(),
            agent_id,
            local_only_fallback: false,
        };
        let store = crate::voice::announce_meta::global_store();
        store.reserve_handoff(&correlation_id, meta.clone());
        let mut durable_reserved = false;
        if let Some(pool) = shared.pg_pool.as_ref() {
            match crate::voice::announce_meta::persist_handoff_reservation_durable(
                pool,
                &correlation_id,
                &meta,
            )
            .await
            {
                Ok(()) => {
                    durable_reserved = true;
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        correlation_id = %correlation_id,
                        source_channel_id = source_channel_id.get(),
                        target_channel_id = target_channel_id.get(),
                        utterance_id = %announcement.utterance_id,
                        "voice background handoff durable reservation failed before publish; refusing to publish"
                    );
                    store.cancel_handoff_reservation(&correlation_id);
                    return Err(format!(
                        "voice background handoff durable reservation failed before publish: {error}"
                    ));
                }
            }
        } else {
            tracing::debug!(
                correlation_id = %correlation_id,
                "voice background handoff durable reservation skipped — postgres pool unavailable"
            );
        }

        let link_generation = generation.min(i32::MAX as u64) as i32;
        let mut voice_turn_link_inserted = false;
        if let (Some(pool), Some(guild_id)) = (shared.pg_pool.as_ref(), guild_id) {
            let link = crate::voice::turn_link::VoiceTurnLinkInsert {
                guild_id: guild_id.get(),
                voice_channel_id: source_channel_id.get(),
                background_channel_id: target_channel_id.get(),
                utterance_id: announcement.utterance_id.clone(),
                generation: link_generation,
                announce_message_id: None,
                dispatch_id: None,
                turn_id: None,
            };
            match crate::voice::turn_link::upsert_active_voice_turn_link_pg(pool, &link).await {
                Ok(Some(_)) => {
                    voice_turn_link_inserted = true;
                }
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        source_channel_id = source_channel_id.get(),
                        target_channel_id = target_channel_id.get(),
                        utterance_id = %announcement.utterance_id,
                        "voice background handoff voice_turn_link pre-publish insert failed; terminal TTS will fall back to announce metadata"
                    );
                }
            }
        }

        let start_result = {
            #[cfg(test)]
            {
                if let Some(result) = self.take_test_background_handoff_outcome(
                    driver.kind(),
                    source_channel_id,
                    target_channel_id,
                    announcement,
                    summary,
                    &prompt,
                ) {
                    result
                } else {
                    driver
                        .start(VoiceBackgroundStartRequest {
                            guild_id,
                            voice_channel_id: source_channel_id,
                            channel_id: target_channel_id,
                            shared,
                            utterance_id: &announcement.utterance_id,
                            generation,
                            message_content: &prompt,
                        })
                        .await
                }
            }
            #[cfg(not(test))]
            {
                driver
                    .start(VoiceBackgroundStartRequest {
                        guild_id,
                        voice_channel_id: source_channel_id,
                        channel_id: target_channel_id,
                        shared,
                        utterance_id: &announcement.utterance_id,
                        generation,
                        message_content: &prompt,
                    })
                    .await
            }
        };
        let outcome = match start_result {
            Ok(outcome) => outcome,
            Err(error) => {
                store.cancel_handoff_reservation(&correlation_id);
                if voice_turn_link_inserted {
                    if let (Some(pool), Some(guild_id)) = (shared.pg_pool.as_ref(), guild_id) {
                        if let Err(mark_error) =
                            crate::voice::turn_link::mark_terminal_voice_turn_link_pg(
                                pool,
                                guild_id.get(),
                                source_channel_id.get(),
                                &announcement.utterance_id,
                                link_generation,
                            )
                            .await
                        {
                            tracing::warn!(
                                error = %mark_error,
                                correlation_id = %correlation_id,
                                "voice_turn_link cleanup failed after background handoff publish error"
                            );
                        }
                    }
                }
                if durable_reserved {
                    if let Some(pool) = shared.pg_pool.as_ref() {
                        if let Err(cancel_error) =
                            crate::voice::announce_meta::cancel_handoff_reservation_durable(
                                pool,
                                &correlation_id,
                            )
                            .await
                        {
                            tracing::warn!(
                                error = %cancel_error,
                                correlation_id = %correlation_id,
                                "voice background handoff durable reservation cleanup failed after publish error"
                            );
                        }
                    }
                }
                return Err(error);
            }
        };

        if let (Some(pool), Some(guild_id)) = (shared.pg_pool.as_ref(), guild_id) {
            let patch = crate::voice::turn_link::VoiceTurnLinkIdentityPatch {
                guild_id: guild_id.get(),
                voice_channel_id: source_channel_id.get(),
                utterance_id: announcement.utterance_id.clone(),
                generation: link_generation,
                announce_message_id: outcome.message_id.map(|id| id.get()),
                dispatch_id: None,
                turn_id: Some(outcome.turn_id.clone()),
            };
            if let Err(error) =
                crate::voice::turn_link::attach_voice_turn_link_ids_pg(pool, &patch).await
            {
                tracing::warn!(
                    error = %error,
                    source_channel_id = source_channel_id.get(),
                    target_channel_id = target_channel_id.get(),
                    utterance_id = %announcement.utterance_id,
                    announce_message_id = outcome.message_id.map(|id| id.get()),
                    turn_id = %outcome.turn_id,
                    "voice background handoff voice_turn_link identity attach failed; terminal TTS will fall back to announce metadata"
                );
            }
        }

        // #2236: stamp a typed marker keyed by the posted message id so the
        // turn bridge can route the background turn's spoken summary back to
        // the foreground voice channel WITHOUT relying on user-authored prompt
        // text. #2392 moves the stamp to a pre-publish reservation: if an
        // immediate completion beats this bind, terminal delivery claims the
        // reservation by the correlation marker and this late bind becomes a
        // no-op instead of resurrecting the handoff.
        if let Some(message_id) = outcome.message_id {
            if !store.bind_handoff_message_id(&correlation_id, message_id) {
                tracing::info!(
                    correlation_id = %correlation_id,
                    message_id = message_id.get(),
                    source_channel_id = source_channel_id.get(),
                    target_channel_id = target_channel_id.get(),
                    utterance_id = %announcement.utterance_id,
                    "voice background handoff local reservation was already consumed before message-id bind"
                );
            }
            if durable_reserved {
                if let Some(pool) = shared.pg_pool.as_ref() {
                    match crate::voice::announce_meta::bind_handoff_durable_message_id(
                        pool,
                        &correlation_id,
                        message_id,
                    )
                    .await
                    {
                        Ok(true) => {}
                        Ok(false) => {
                            tracing::info!(
                                correlation_id = %correlation_id,
                                message_id = message_id.get(),
                                source_channel_id = source_channel_id.get(),
                                target_channel_id = target_channel_id.get(),
                                utterance_id = %announcement.utterance_id,
                                "voice background handoff durable reservation was already consumed before message-id bind"
                            );
                        }
                        Err(error) => {
                            tracing::warn!(
                                error = %error,
                                correlation_id = %correlation_id,
                                message_id = message_id.get(),
                                source_channel_id = source_channel_id.get(),
                                target_channel_id = target_channel_id.get(),
                                utterance_id = %announcement.utterance_id,
                                "voice background handoff durable reservation bind failed"
                            );
                        }
                    }
                }
            }
        } else {
            store.cancel_handoff_reservation(&correlation_id);
            if durable_reserved {
                if let Some(pool) = shared.pg_pool.as_ref() {
                    if let Err(error) =
                        crate::voice::announce_meta::cancel_handoff_reservation_durable(
                            pool,
                            &correlation_id,
                        )
                        .await
                    {
                        tracing::warn!(
                            error = %error,
                            correlation_id = %correlation_id,
                            "voice background handoff durable reservation cleanup failed after missing message id"
                        );
                    }
                }
            }
            tracing::warn!(
                source_channel_id = source_channel_id.get(),
                target_channel_id = target_channel_id.get(),
                utterance_id = %announcement.utterance_id,
                "voice background handoff dispatch returned no message_id; spoken summary routing will fall back to legacy prefix detection"
            );
        }
        Ok(VoiceBackgroundHandoffOutcome {
            turn_id: outcome.turn_id,
            handoff_message_id: outcome.message_id,
            correlation_id,
        })
    }

    pub(super) async fn start_voice_turn(
        self: &Arc<Self>,
        shared: &Arc<SharedData>,
        source_channel_id: ChannelId,
        target_channel_id: ChannelId,
        utterance: &CompletedUtterance,
        transcript: &str,
        flight_context: &VoiceFlightUtteranceContext,
    ) -> VoiceBargeInTranscriptOutcome {
        let verbose_progress = self.verbose_progress_enabled();
        let language = self.spoken_result_language().await;
        let foreground = self
            .resolve_effective_foreground_config(source_channel_id, target_channel_id)
            .await;
        let driver = select_voice_background_driver();
        let guild_id = self.voice_turn_guild_id(source_channel_id, target_channel_id);
        if guild_id.is_none() {
            tracing::warn!(
                source_channel_id = source_channel_id.get(),
                target_channel_id = target_channel_id.get(),
                utterance_id = %utterance.utterance_id,
                "voice transcript announcement has no registered voice guild; sending without delivery id"
            );
        }
        let announcement = crate::voice::prompt::build_voice_transcript_announcement(
            transcript,
            utterance.user_id,
            &utterance.utterance_id,
            &language,
            verbose_progress,
            &utterance.started_at,
            &utterance.completed_at,
            utterance.samples_written,
        );
        let announcement_meta = crate::voice::prompt::voice_transcript_announcement_meta(
            transcript,
            utterance.user_id,
            &utterance.utterance_id,
            &language,
            verbose_progress,
            &utterance.started_at,
            &utterance.completed_at,
            utterance.samples_written,
            Some(
                utterance
                    .control_channel_id
                    .unwrap_or(source_channel_id.get()),
            ),
            Some(flight_context.stt_mode.as_str()),
            Some(flight_context.stt_latency_ms),
        );
        let generation = default_voice_announce_generation();
        let voice_delivery_id = guild_id.map(|guild_id| {
            voice_announce_delivery_id(
                guild_id,
                source_channel_id,
                &utterance.utterance_id,
                generation,
            )
        });
        let durable_pending_key = voice_delivery_id
            .as_ref()
            .map(|delivery_id| {
                crate::voice::announce_meta::durable_voice_announcement_pending_key(
                    &delivery_id.correlation_id,
                    &delivery_id.semantic_event_id,
                )
            })
            .unwrap_or_else(|| {
                crate::voice::announce_meta::durable_voice_announcement_pending_key(
                    &format!(
                        "voice:no-guild:{}:{}:{}",
                        source_channel_id.get(),
                        target_channel_id.get(),
                        utterance.utterance_id
                    ),
                    &format!("announce:generation:{generation}"),
                )
            });
        let announcement = crate::voice::prompt::append_voice_transcript_announcement_ref(
            &announcement,
            &durable_pending_key,
        );
        let mut durable_reserved = false;
        if let Some(pool) = shared.pg_pool.as_ref() {
            match crate::voice::announce_meta::persist_voice_announcement_reservation_durable(
                pool,
                &durable_pending_key,
                source_channel_id,
                &announcement,
                &announcement_meta,
            )
            .await
            {
                Ok(true) => {
                    durable_reserved = true;
                }
                Ok(false) => {
                    tracing::warn!(
                        source_channel_id = source_channel_id.get(),
                        target_channel_id = target_channel_id.get(),
                        utterance_id = %utterance.utterance_id,
                        "voice transcript announcement durable reservation was already consumed; refusing to resurrect voice metadata"
                    );
                    return VoiceBargeInTranscriptOutcome::VoiceTurnStartFailed(
                        "voice transcript announcement was already consumed".to_string(),
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        source_channel_id = source_channel_id.get(),
                        target_channel_id = target_channel_id.get(),
                        utterance_id = %utterance.utterance_id,
                        "voice transcript announcement durable reservation failed before publish"
                    );
                    return VoiceBargeInTranscriptOutcome::VoiceTurnStartFailed(format!(
                        "voice transcript announcement durable reservation failed: {error}"
                    ));
                }
            }
        }
        let start_result = {
            #[cfg(test)]
            {
                if let Some(outcome) = self.take_test_turn_start_outcome(
                    driver.kind(),
                    source_channel_id,
                    target_channel_id,
                    &utterance.utterance_id,
                    &announcement,
                ) {
                    outcome
                } else {
                    driver
                        .start(VoiceBackgroundStartRequest {
                            guild_id,
                            voice_channel_id: source_channel_id,
                            channel_id: source_channel_id,
                            shared,
                            utterance_id: &utterance.utterance_id,
                            generation,
                            message_content: &announcement,
                        })
                        .await
                }
            }
            #[cfg(not(test))]
            {
                driver
                    .start(VoiceBackgroundStartRequest {
                        guild_id,
                        voice_channel_id: source_channel_id,
                        channel_id: source_channel_id,
                        shared,
                        utterance_id: &utterance.utterance_id,
                        generation,
                        message_content: &announcement,
                    })
                    .await
            }
        };
        match start_result {
            Ok(outcome) => {
                if let Some(message_id) = outcome.message_id {
                    let mut cache_local_metadata = !durable_reserved;
                    if durable_reserved {
                        if let Some(pool) = shared.pg_pool.as_ref() {
                            match crate::voice::announce_meta::bind_voice_announcement_durable_message_id(
                                pool,
                                &durable_pending_key,
                                message_id,
                            )
                            .await
                            {
                                Ok(true) => {
                                    cache_local_metadata = true;
                                }
                                Ok(false) => {
                                    tracing::info!(
                                        message_id = message_id.get(),
                                        source_channel_id = source_channel_id.get(),
                                        target_channel_id = target_channel_id.get(),
                                        utterance_id = %utterance.utterance_id,
                                        "voice transcript announcement durable reservation was already consumed or bound elsewhere; skipping local metadata cache"
                                    );
                                }
                                Err(error) => {
                                    tracing::warn!(
                                        error = %error,
                                        message_id = message_id.get(),
                                        source_channel_id = source_channel_id.get(),
                                        target_channel_id = target_channel_id.get(),
                                        utterance_id = %utterance.utterance_id,
                                        "voice transcript announcement durable reservation bind failed; skipping local metadata cache so workers must use the pending ref"
                                    );
                                }
                            }
                        }
                    }
                    if cache_local_metadata {
                        crate::voice::announce_meta::global_store()
                            .insert(message_id, announcement_meta);
                    }
                }
                tracing::info!(
                    source_channel_id = source_channel_id.get(),
                    target_channel_id = target_channel_id.get(),
                    user_id = utterance.user_id,
                    utterance_id = %utterance.utterance_id,
                    turn_id = %outcome.turn_id,
                    background_driver = %outcome.driver_kind.as_str(),
                    foreground_provider = %foreground.provider,
                    foreground_model = %foreground.model,
                    foreground_max_chars = foreground.max_chars,
                    "voice transcript announcement posted to voice channel as canonical foreground trigger"
                );
                let mut event = flight_context.event(VoiceFlightRoute::Queued);
                event.background_channel_id = Some(target_channel_id.get());
                event.turn_id = Some(outcome.turn_id.clone());
                event.foreground_provider = Some(foreground.provider.clone());
                event.foreground_model = Some(foreground.model.clone());
                event.foreground_decision = Some("queued_foreground_trigger".to_string());
                if let Some(route) = self
                    .channels
                    .active_voice_routes
                    .get(&source_channel_id.get())
                {
                    event.agent_id = Some(route.agent_id.clone());
                }
                record_voice_flight_event(event);
                return VoiceBargeInTranscriptOutcome::VoiceTurnStarted {
                    turn_id: outcome.turn_id,
                };
            }
            Err(error) => {
                if durable_reserved {
                    if let Some(pool) = shared.pg_pool.as_ref() {
                        if let Err(cancel_error) =
                            crate::voice::announce_meta::cancel_voice_announcement_reservation_durable(
                                pool,
                                &durable_pending_key,
                            )
                            .await
                        {
                            tracing::warn!(
                                error = %cancel_error,
                                source_channel_id = source_channel_id.get(),
                                target_channel_id = target_channel_id.get(),
                                utterance_id = %utterance.utterance_id,
                                "voice transcript announcement durable reservation cleanup failed after publish error"
                            );
                        }
                    }
                }
                tracing::warn!(
                    error = %error,
                    source_channel_id = source_channel_id.get(),
                    target_channel_id = target_channel_id.get(),
                    user_id = utterance.user_id,
                    utterance_id = %utterance.utterance_id,
                    "voice transcript announcement failed; refusing direct voice turn fallback"
                );
                let mut event = flight_context.event(VoiceFlightRoute::Queued);
                event.background_channel_id = Some(target_channel_id.get());
                event.foreground_provider = Some(foreground.provider.clone());
                event.foreground_model = Some(foreground.model.clone());
                event.foreground_decision = Some("queue_failed".to_string());
                event.reason = Some(format!("voice_turn_start_failed:{error}"));
                record_voice_flight_event(event);
                return VoiceBargeInTranscriptOutcome::VoiceTurnStartFailed(format!(
                    "voice transcript announcement failed: {error}"
                ));
            }
        }
    }
}
