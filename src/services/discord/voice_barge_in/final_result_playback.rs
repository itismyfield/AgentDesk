use super::*;

impl VoiceBargeInRuntime {
    pub(in crate::services::discord) async fn speak_voice_background_completion_summary(
        self: &Arc<Self>,
        shared: &Arc<SharedData>,
        voice_channel_id: ChannelId,
        background_channel_id: ChannelId,
        background_result: &str,
        failed: bool,
    ) {
        if !self.enabled {
            return;
        }
        let language = self.spoken_result_language().await;
        let foreground = self
            .resolve_effective_foreground_config(voice_channel_id, background_channel_id)
            .await;
        let cancel_token = Arc::new(crate::services::provider::CancelToken::new());
        self.register_inflight_foreground_cancel(voice_channel_id, cancel_token.clone());
        let summary_result = self
            .generate_voice_background_result_summary_for_runtime(
                background_result,
                &language,
                &foreground,
                cancel_token.clone(),
            )
            .await;
        self.unregister_inflight_foreground_cancel(voice_channel_id, &cancel_token);
        // #2250: if cancel won the race (e.g. user barge-in or guild
        // teardown), suppress fallback speech and skip TTS entirely.
        // Otherwise the user would still hear the completion summary
        // after they explicitly stopped.
        if cancel_token.cancelled.load(Ordering::Relaxed) {
            tracing::info!(
                voice_channel_id = voice_channel_id.get(),
                background_channel_id = background_channel_id.get(),
                cancel_source = ?cancel_token.cancel_source(),
                "voice background completion summary suppressed because cancel won the race (#2250)"
            );
            return;
        }
        let summary = summary_result.unwrap_or_else(|| {
            fallback_voice_background_result_summary(
                background_result,
                &language,
                foreground.max_chars,
                failed,
            )
        });
        if summary.trim().is_empty() {
            return;
        }
        tracing::info!(
            voice_channel_id = voice_channel_id.get(),
            background_channel_id = background_channel_id.get(),
            foreground_provider = %foreground.provider,
            foreground_model = %foreground.model,
            failed,
            summary_chars = summary.chars().count(),
            "voice background completion summary queued"
        );
        self.speak_progress_text(
            shared,
            voice_channel_id,
            &summary,
            "voice background result summary",
        )
        .await;
    }

    pub(super) fn start_spoken_result_playback(
        &self,
        channel_id: ChannelId,
    ) -> (u64, CancellationToken) {
        let id = self.id_sequences.next_spoken_result_playback_id();
        let cancellation = CancellationToken::new();
        if let Some(previous) = self.spoken_result_playbacks.insert(
            channel_id.get(),
            SpokenResultPlaybackSession {
                id,
                cancellation: cancellation.clone(),
            },
        ) {
            previous.cancellation.cancel();
        }
        (id, cancellation)
    }

    pub(super) fn clear_spoken_result_playback_if_current(&self, channel_id: ChannelId, id: u64) {
        self.spoken_result_playbacks
            .remove_if(&channel_id.get(), |_, session| session.id == id);
    }

    pub(in crate::services::discord) async fn spawn_spoken_result_playback(
        self: &Arc<Self>,
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
        answer: &str,
    ) {
        // Voice #10: agent stage ends when the answer is ready for TTS.
        // Record even if TTS bails below — keeps the partial latency state
        // monotonic with the agent timeline.
        crate::voice::metrics::finish_agent_start(channel_id.get());

        let Some(tts) = self.tts.read().await.clone() else {
            // Voice #10: drop the partial latency record so the next turn
            // doesn't inherit stale stt/agent ms.
            crate::voice::metrics::discard(channel_id.get());
            return;
        };
        let language = self.spoken_result_language().await;
        let spoken_result_max_chars = self.cached_config().await.voice.spoken_result.max_chars;
        let spoken_result_max_chars = if spoken_result_max_chars == 0 {
            crate::voice::sanitizer::DEFAULT_SPOKEN_RESULT_CHAR_LIMIT
        } else {
            spoken_result_max_chars
        };
        let spoken = spoken_result_only_with_limit(answer, &language, spoken_result_max_chars);
        if spoken.trim().is_empty() {
            crate::voice::metrics::discard(channel_id.get());
            return;
        }

        let Some(guild_id) = self
            .voice_guilds
            .get(&channel_id.get())
            .map(|entry| *entry.value())
        else {
            crate::voice::metrics::discard(channel_id.get());
            tracing::debug!(
                channel_id = channel_id.get(),
                "voice final TTS playback skipped: no registered voice guild"
            );
            return;
        };
        let Some(ctx) = shared.cached_serenity_ctx.get() else {
            crate::voice::metrics::discard(channel_id.get());
            tracing::debug!(
                channel_id = channel_id.get(),
                guild_id = guild_id.get(),
                "voice final TTS playback skipped: no serenity context"
            );
            return;
        };
        let Some(manager) = songbird::get(ctx).await else {
            crate::voice::metrics::discard(channel_id.get());
            tracing::warn!(
                channel_id = channel_id.get(),
                guild_id = guild_id.get(),
                "voice final TTS playback skipped: songbird manager missing"
            );
            return;
        };
        let Some(call_lock) = manager.get(guild_id) else {
            crate::voice::metrics::discard(channel_id.get());
            tracing::debug!(
                channel_id = channel_id.get(),
                guild_id = guild_id.get(),
                "voice final TTS playback skipped: no active songbird call"
            );
            return;
        };

        let runtime = self.clone();
        let (playback_id, cancellation) = self.start_spoken_result_playback(channel_id);
        let playback_cancellation = cancellation.clone();
        let register_cancellation = cancellation.clone();
        tokio::spawn(async move {
            let runtime_for_track = runtime.clone();
            let register_track = move |track| {
                runtime_for_track.reset_after_playback_start_with_owner(
                    channel_id,
                    Arc::new(track),
                    register_cancellation.clone(),
                    Some(playback_id),
                );
            };

            let result = play_chunked_with_prefetch(
                call_lock,
                tts,
                spoken,
                DEFAULT_TTS_CHUNK_MAX_CHARS,
                playback_cancellation,
                register_track,
            )
            .await;

            runtime.clear_playback_if_owner(channel_id, playback_id);
            runtime.clear_spoken_result_playback_if_current(channel_id, playback_id);
            match result {
                Ok(report) => {
                    let synth_ms = report
                        .first_chunk_synthesis_ms
                        .unwrap_or(0)
                        .min(u64::MAX as u128) as u64;
                    let first_audio_out_ms = report
                        .first_audio_start_ms
                        .unwrap_or(0)
                        .min(u64::MAX as u128) as u64;
                    crate::voice::metrics::record_tts(
                        channel_id.get(),
                        synth_ms,
                        first_audio_out_ms,
                    );
                    tracing::info!(
                        channel_id = channel_id.get(),
                        guild_id = guild_id.get(),
                        chunks = report.chunk_count,
                        played_chunks = report.played_chunks,
                        first_chunk_synthesis_ms = ?report.first_chunk_synthesis_ms,
                        first_audio_out_ms,
                        first_audio_start_ms = ?report.first_audio_start_ms,
                        "voice final TTS chunked playback finished"
                    );
                }
                Err(error) => {
                    crate::voice::metrics::discard(channel_id.get());
                    tracing::warn!(
                        error = %error,
                        channel_id = channel_id.get(),
                        guild_id = guild_id.get(),
                        "voice final TTS chunked playback failed"
                    );
                }
            }
        });
    }
}
