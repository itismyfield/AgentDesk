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
        // #3911: scope the foreground registration to the generate `.await` via
        // a drop guard that OWNS it. Previously the manual `unregister` ran only
        // AFTER the await returned, so if this task was aborted mid-`.await`
        // (shutdown / supervisor abort) the token leaked in
        // `inflight_foreground_cancels` — leaving `has_inflight_foreground`
        // permanently true so the next fresh utterance was misclassified as a
        // barge-in (the channel got "stuck"). The guard's Drop unregisters on
        // every exit path (normal return, panic, or abort), matching the
        // previous unregister-right-after-generate timing.
        let summary_result = {
            let _foreground_guard = super::InflightForegroundCancelGuard::register(
                self,
                voice_channel_id,
                cancel_token.clone(),
            );
            self.generate_voice_background_result_summary_for_runtime(
                background_result,
                &language,
                &foreground,
                cancel_token.clone(),
            )
            .await
        };
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
            // #4238: this path is reached only for a voice-sourced turn, so a
            // missing TTS runtime means the user spoke and gets no spoken reply
            // — make the silent early return observable.
            tracing::warn!(
                channel_id = channel_id.get(),
                reason = "no_tts_runtime",
                "voice final TTS playback skipped: TTS runtime not configured"
            );
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
            // #4238: the agent produced an answer but it sanitized to nothing
            // speakable; surface the reason instead of returning silently.
            tracing::warn!(
                channel_id = channel_id.get(),
                reason = "empty_spoken_text",
                "voice final TTS playback skipped: answer sanitized to empty spoken text"
            );
            return;
        }

        let Some(guild_id) = self
            .voice_guilds
            .get(&channel_id.get())
            .map(|entry| *entry.value())
        else {
            crate::voice::metrics::discard(channel_id.get());
            // #4238: promote from debug — a voice turn with no registered guild
            // means the spoken reply is dropped; operators need to see why.
            tracing::warn!(
                channel_id = channel_id.get(),
                reason = "no_voice_guild",
                "voice final TTS playback skipped: no registered voice guild"
            );
            return;
        };
        let Some(ctx) = shared.http.cached_serenity_ctx.get() else {
            crate::voice::metrics::discard(channel_id.get());
            // #4238: promote from debug for the same reason as above.
            tracing::warn!(
                channel_id = channel_id.get(),
                guild_id = guild_id.get(),
                reason = "no_serenity_context",
                "voice final TTS playback skipped: no serenity context"
            );
            return;
        };
        let Some(manager) = songbird::get(ctx).await else {
            crate::voice::metrics::discard(channel_id.get());
            tracing::warn!(
                channel_id = channel_id.get(),
                guild_id = guild_id.get(),
                reason = "songbird_manager_missing",
                "voice final TTS playback skipped: songbird manager missing"
            );
            return;
        };
        // #4236: gate on a *connected* driver, not just a present call handle.
        // A zombie Call (present but no UDP socket) would otherwise accept the
        // track and hang silently; the gate skips-and-logs instead.
        let Some(call_lock) = crate::services::discord::voice_lifecycle::connected_voice_call(
            &manager,
            guild_id,
            channel_id,
            "final_result",
        )
        .await
        else {
            crate::voice::metrics::discard(channel_id.get());
            // #4238: `connected_voice_call` already warns for the zombie-driver
            // case (#4236); emit a site-level warn with a uniform `reason` so
            // the "no call handle at all" case is observable too.
            tracing::warn!(
                channel_id = channel_id.get(),
                guild_id = guild_id.get(),
                reason = "no_connected_voice_call",
                "voice final TTS playback skipped: no connected voice call"
            );
            return;
        };

        let runtime = self.clone();
        let shared = shared.clone();
        let (playback_id, cancellation) = self.start_spoken_result_playback(channel_id);
        let playback_cancellation = cancellation.clone();
        let register_cancellation = cancellation.clone();
        tokio::spawn(async move {
            let runtime_for_track = runtime.clone();
            // #4238: track whether any chunk actually reached the channel so the
            // retry below never replays audio the user already heard.
            let played_any = Arc::new(AtomicBool::new(false));
            let register_track = {
                let played_any = played_any.clone();
                move |track| {
                    played_any.store(true, Ordering::Relaxed);
                    runtime_for_track.reset_after_playback_start_with_owner(
                        channel_id,
                        Arc::new(track),
                        register_cancellation.clone(),
                        Some(playback_id),
                    );
                }
            };

            let mut result = play_chunked_with_prefetch(
                call_lock.clone(),
                tts.clone(),
                spoken.clone(),
                DEFAULT_TTS_CHUNK_MAX_CHARS,
                playback_cancellation.clone(),
                register_track.clone(),
            )
            .await;

            // #4238: TTS synth/playback failures used to be swallowed with only
            // a `warn!`. Retry ONCE, but ONLY when the failure happened before
            // any audio reached the channel — retrying after a partial play
            // would double up the chunks the user already heard. Cancellation
            // (user barge-in / teardown) surfaces as `Ok`, so an `Err` here is a
            // genuine failure; the cancel-flag guard still avoids retrying into
            // a call that is being torn down.
            if let Some(first_error) = result.as_ref().err().map(ToString::to_string) {
                if !played_any.load(Ordering::Relaxed) && !playback_cancellation.is_cancelled() {
                    tracing::warn!(
                        error = %first_error,
                        channel_id = channel_id.get(),
                        guild_id = guild_id.get(),
                        "voice final TTS playback failed before any audio; retrying once (#4238)"
                    );
                    result = play_chunked_with_prefetch(
                        call_lock,
                        tts,
                        spoken,
                        DEFAULT_TTS_CHUNK_MAX_CHARS,
                        playback_cancellation.clone(),
                        register_track,
                    )
                    .await;
                }
            }

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
                    // #4238: the spoken reply is lost (retry already spent, or a
                    // partial play failed and a replay would double audio). Post
                    // a one-shot text fallback so the answer is not silently
                    // dropped. This is the terminal failure arm and runs at most
                    // once per playback, so it cannot loop. Skip on cancellation
                    // (deliberate stop / teardown — no failure to report).
                    if !playback_cancellation.is_cancelled() {
                        runtime
                            .post_voice_playback_failure_notice(&shared, channel_id)
                            .await;
                    }
                }
            }
        });
    }

    /// #4238: post a one-shot text fallback to the routing channel after a
    /// spoken-result playback fails past its retry. Reuses the exact send path
    /// the progress-text mirror uses (`serenity_http_or_token_fallback` +
    /// `rate_limit_wait` + `http::send_channel_message`) so no new send surface
    /// is introduced. Called only from the terminal `Err` arm above, so it fires
    /// at most once per failed playback.
    async fn post_voice_playback_failure_notice(
        &self,
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
    ) {
        let Some(http) = shared.serenity_http_or_token_fallback() else {
            tracing::warn!(
                channel_id = channel_id.get(),
                "voice TTS failure text fallback skipped: no Discord HTTP client"
            );
            return;
        };
        let language = self.spoken_result_language().await;
        let content = crate::voice::progress::playback_failure_notice(&language);
        super::rate_limit_wait(shared, channel_id).await;
        match super::http::send_channel_message(&http, channel_id, content).await {
            Ok(_) => {
                tracing::warn!(
                    channel_id = channel_id.get(),
                    "voice TTS playback failed; posted text fallback notice (#4238)"
                );
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    channel_id = channel_id.get(),
                    "voice TTS failure text fallback send failed"
                );
            }
        }
    }
}
