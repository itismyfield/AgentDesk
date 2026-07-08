use super::*;

impl VoiceBargeInRuntime {
    pub(in crate::services::discord) fn subscribe_progress(
        &self,
    ) -> broadcast::Receiver<VoiceProgressEvent> {
        self.progress_tx.subscribe()
    }

    pub(in crate::services::discord) fn publish_progress(
        &self,
        channel_id: ChannelId,
        label: impl Into<String>,
    ) {
        self.publish_progress_for_playback(channel_id, None, label);
    }

    pub(in crate::services::discord) fn publish_progress_for_playback(
        &self,
        channel_id: ChannelId,
        playback_channel_id: Option<ChannelId>,
        label: impl Into<String>,
    ) {
        let label = label.into();
        if label.trim().is_empty() {
            return;
        }
        let _ = self.progress_tx.send(VoiceProgressEvent {
            channel_id: channel_id.get(),
            playback_channel_id: playback_channel_id.map(|id| id.get()),
            label,
        });
    }

    pub(in crate::services::discord) fn spawn_progress_worker(
        self: &Arc<Self>,
        shared: Arc<SharedData>,
        shutdown_flag: Arc<AtomicBool>,
    ) {
        if !self.enabled {
            return;
        }

        let runtime = self.clone();
        let mut rx = self.subscribe_progress();
        tokio::spawn(async move {
            let mut states: HashMap<u64, VoiceProgressChannelState> = HashMap::new();
            let mut tick = tokio::time::interval(Duration::from_secs(1));

            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        if shutdown_flag.load(Ordering::Relaxed) {
                            break;
                        }
                        runtime.flush_due_progress_summaries(&shared, &mut states).await;
                        runtime.emit_due_idle_notices(&shared, &mut states).await;
                    }
                    event = rx.recv() => {
                        match event {
                            Ok(event) => {
                                runtime.handle_progress_event(&shared, &mut states, event).await;
                            }
                            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                                tracing::warn!(
                                    skipped,
                                    "voice progress worker lagged behind broadcast events"
                                );
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
        });
    }

    async fn handle_progress_event(
        self: &Arc<Self>,
        shared: &Arc<SharedData>,
        states: &mut HashMap<u64, VoiceProgressChannelState>,
        event: VoiceProgressEvent,
    ) {
        let label = event.label.trim().to_string();
        if label.is_empty() {
            return;
        }

        let channel_id = ChannelId::new(event.channel_id);
        let playback_channel_id = event
            .playback_channel_id
            .map(ChannelId::new)
            .unwrap_or(channel_id);
        if progress::is_turn_done_event(&label) {
            if let Some(state) = states.get_mut(&event.channel_id) {
                state.mark_done();
            }
            // #3906 (P4): the turn-DONE branch plays the distinct DESCENDING done
            // chime so it is audibly different from the rising processing/intake
            // chime emitted at turn start. Previously both played the SAME
            // PROCESSING_CHIME_FILE_NAME, making start and done indistinguishable.
            self.play_done_chime(
                shared,
                ChannelId::new(progress_feedback_channel_id(
                    event.channel_id,
                    event.playback_channel_id,
                )),
            )
            .await;
            return;
        }

        let now = Instant::now();
        states
            .entry(event.channel_id)
            .or_insert_with(|| VoiceProgressChannelState::new(now))
            .mark_active(now);
        if let Some(state) = states.get_mut(&event.channel_id) {
            state.set_playback_channel_id(event.playback_channel_id);
        }

        if !self.verbose_progress_enabled() {
            return;
        }

        self.mirror_progress_line(shared, channel_id, &label).await;

        let summary_events = if let Some(state) = states.get_mut(&event.channel_id) {
            state.pending_events.push(label);
            if state.pending_events.len() >= progress::PROGRESS_BATCH_MAX_EVENTS {
                let events = std::mem::take(&mut state.pending_events);
                state.next_summary_at = None;
                Some(events)
            } else {
                if state.next_summary_at.is_none() {
                    state.next_summary_at = Some(now + Duration::from_millis(1200));
                }
                None
            }
        } else {
            None
        };
        if let Some(events) = summary_events {
            self.speak_progress_summary(shared, playback_channel_id, events)
                .await;
        }
    }

    async fn flush_due_progress_summaries(
        self: &Arc<Self>,
        shared: &Arc<SharedData>,
        states: &mut HashMap<u64, VoiceProgressChannelState>,
    ) {
        if !self.verbose_progress_enabled() {
            return;
        }

        let now = Instant::now();
        let due_channels = states
            .iter()
            .filter_map(|(channel_id, state)| {
                state
                    .next_summary_at
                    .filter(|deadline| *deadline <= now && !state.pending_events.is_empty())
                    .map(|_| *channel_id)
            })
            .collect::<Vec<_>>();

        for raw_channel_id in due_channels {
            let events = if let Some(state) = states.get_mut(&raw_channel_id) {
                state.next_summary_at = None;
                std::mem::take(&mut state.pending_events)
            } else {
                Vec::new()
            };
            if !events.is_empty() {
                self.speak_progress_summary(shared, ChannelId::new(raw_channel_id), events)
                    .await;
            }
        }
    }

    async fn emit_due_idle_notices(
        self: &Arc<Self>,
        shared: &Arc<SharedData>,
        states: &mut HashMap<u64, VoiceProgressChannelState>,
    ) {
        let now = Instant::now();
        let due_channels = states
            .iter()
            .filter(|(_, state)| {
                state.active && now.duration_since(state.last_activity_at) >= state.next_idle_delay
            })
            .map(|(channel_id, _)| *channel_id)
            .collect::<Vec<_>>();

        for raw_channel_id in due_channels {
            let (channel_id, playback_channel_id) = if let Some(state) = states.get(&raw_channel_id)
            {
                (
                    ChannelId::new(raw_channel_id),
                    state
                        .playback_channel_id
                        .map(ChannelId::new)
                        .unwrap_or_else(|| ChannelId::new(raw_channel_id)),
                )
            } else {
                (
                    ChannelId::new(raw_channel_id),
                    ChannelId::new(raw_channel_id),
                )
            };
            if !super::mailbox_has_active_turn(shared, channel_id).await {
                if let Some(state) = states.get_mut(&raw_channel_id) {
                    state.mark_done();
                }
                continue;
            }

            let language = self.spoken_result_language().await;
            self.speak_progress_text(
                shared,
                playback_channel_id,
                progress::idle_notice(&language),
                "voice progress idle notice",
            )
            .await;

            if let Some(state) = states.get_mut(&raw_channel_id) {
                state.last_activity_at = Instant::now();
                state.next_idle_delay = progress::next_idle_notice_delay(state.next_idle_delay);
            }
        }

        states.retain(|_, state| state.active || !state.pending_events.is_empty());
    }

    async fn mirror_progress_line(
        &self,
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
        label: &str,
    ) {
        let Some(http) = shared.serenity_http_or_token_fallback() else {
            tracing::warn!(
                channel_id = channel_id.get(),
                "voice progress text mirror skipped: no Discord HTTP client"
            );
            return;
        };
        let language = self.spoken_result_language().await;
        let content = progress::format_progress_message(label, &language);
        if content.trim().is_empty() {
            return;
        }

        super::rate_limit_wait(shared, channel_id).await;
        if let Err(error) = super::http::send_channel_message(&http, channel_id, &content).await {
            tracing::warn!(
                error = %error,
                channel_id = channel_id.get(),
                "voice progress text mirror failed"
            );
        }
    }

    async fn speak_progress_summary(
        self: &Arc<Self>,
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
        events: Vec<String>,
    ) {
        let language = self.spoken_result_language().await;
        let summary = progress::summarize_progress_events(&events, &language);
        self.speak_progress_text(shared, channel_id, &summary, "voice progress summary")
            .await;
    }

    pub(super) async fn speak_progress_text(
        self: &Arc<Self>,
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
        text: &str,
        context: &'static str,
    ) {
        let Some(path) = self
            .synthesize_progress_tts(text, channel_id, context)
            .await
        else {
            return;
        };
        self.play_progress_audio(shared, channel_id, path, context)
            .await;
    }

    pub(super) async fn play_processing_chime(
        self: &Arc<Self>,
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
    ) {
        let Some(path) = self.processing_chime_path().await else {
            return;
        };
        self.play_progress_audio(shared, channel_id, path, "voice processing chime")
            .await;
    }

    async fn processing_chime_path(&self) -> Option<PathBuf> {
        let config = self.cached_config().await;
        let path = crate::voice::utils::expand_tilde(&config.voice.audio.temp_dir)
            .join(PROCESSING_CHIME_FILE_NAME);
        let path_for_task = path.clone();
        match tokio::task::spawn_blocking(move || {
            ensure_processing_chime_file(&path_for_task).map(|_| path_for_task)
        })
        .await
        {
            Ok(Ok(path)) => Some(path),
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "voice processing chime generation failed");
                None
            }
            Err(error) => {
                tracing::warn!(error = %error, "voice processing chime generation task failed");
                None
            }
        }
    }

    // #3906 (P4): mirror of `play_processing_chime` for the turn-DONE signal so the
    // descending done chime plays through the same songbird path as every other
    // progress playback.
    pub(super) async fn play_done_chime(
        self: &Arc<Self>,
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
    ) {
        let Some(path) = self.done_chime_path().await else {
            return;
        };
        self.play_progress_audio(shared, channel_id, path, "voice done chime")
            .await;
    }

    pub(super) async fn done_chime_path(&self) -> Option<PathBuf> {
        let config = self.cached_config().await;
        let path = crate::voice::utils::expand_tilde(&config.voice.audio.temp_dir)
            .join(DONE_CHIME_FILE_NAME);
        let path_for_task = path.clone();
        match tokio::task::spawn_blocking(move || {
            ensure_done_chime_file(&path_for_task).map(|_| path_for_task)
        })
        .await
        {
            Ok(Ok(path)) => Some(path),
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "voice done chime generation failed");
                None
            }
            Err(error) => {
                tracing::warn!(error = %error, "voice done chime generation task failed");
                None
            }
        }
    }

    pub(super) async fn play_acknowledgement(
        self: &Arc<Self>,
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
        path: PathBuf,
    ) {
        self.play_progress_audio(shared, channel_id, path, "voice barge-in acknowledgement")
            .await;
    }

    async fn play_progress_audio(
        self: &Arc<Self>,
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
        path: PathBuf,
        context: &'static str,
    ) {
        #[cfg(test)]
        self.test_state
            .play_requests
            .lock()
            .expect("voice test play requests lock")
            .push((channel_id.get(), context));

        let Some(guild_id) = self
            .voice_guilds
            .get(&channel_id.get())
            .map(|entry| *entry.value())
        else {
            tracing::debug!(
                channel_id = channel_id.get(),
                path = %path.display(),
                context,
                "voice progress playback skipped: no registered voice guild"
            );
            return;
        };
        let Some(ctx) = shared.http.cached_serenity_ctx.get() else {
            tracing::debug!(
                channel_id = channel_id.get(),
                guild_id = guild_id.get(),
                path = %path.display(),
                context,
                "voice progress playback skipped: no serenity context"
            );
            return;
        };
        let Some(manager) = songbird::get(ctx).await else {
            tracing::warn!(
                channel_id = channel_id.get(),
                guild_id = guild_id.get(),
                context,
                "voice progress playback skipped: songbird manager missing"
            );
            return;
        };
        // #4236: gate on a *connected* driver, not just a present call handle,
        // so a zombie Call cannot silently swallow a progress/chime track.
        let Some(call_lock) = crate::services::discord::voice_lifecycle::connected_voice_call(
            &manager, guild_id, channel_id, "progress",
        )
        .await
        else {
            return;
        };

        let input = songbird::input::File::new(path.clone()).into();
        let track = {
            let mut call = call_lock.lock().await;
            call.play_input(input)
        };
        // F4 (#2046): owner id 발급 + reset_after_playback_start_with_owner 로 등록.
        // 30s 만료 타이머는 `clear_playback_if_owner` 로 동일 owner 일 때만 정리.
        // 후속 progress/spoken_result playback 이 entry 를 덮어쓰면 mismatch 로
        // no-op → 후속 playback 의 barge-in 이 깨지지 않는다.
        // #3908: skip the handle registration entirely while a final-result
        // playback owns the channel so a queued progress/chime flush cannot
        // steal the final answer's barge-in handle.
        let playback_id = self.id_sequences.next_progress_playback_id();
        if self.register_progress_barge_in_handle(channel_id, Arc::new(track), playback_id) {
            let runtime = self.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                runtime.clear_playback_if_owner(channel_id, playback_id);
            });
        }
        tracing::info!(
            channel_id = channel_id.get(),
            guild_id = guild_id.get(),
            path = %path.display(),
            context,
            playback_id,
            "voice progress playback started"
        );
    }
}

// #3906 (P4): generate the turn-DONE chime as a DESCENDING two-tone, mirroring
// `ensure_processing_chime_file` but with the tones reversed (a higher 660Hz tone
// falling under a 440Hz floor, vs the rising 880/1320Hz processing chime) so the
// "done" signal is audibly distinct from "processing started". Generated at
// runtime so no audio asset ships. Lives in this submodule (not voice_barge_in.rs)
// to keep the frozen giant file's production LoC flat (#3036 ratchet).
fn ensure_done_chime_file(path: &Path) -> Result<(), String> {
    if path.metadata().map(|meta| meta.len() > 0).unwrap_or(false) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create done chime dir {}: {error}", parent.display()))?;
    }

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|error| format!("create done chime {}: {error}", path.display()))?;
    let sample_rate = spec.sample_rate as f32;
    let total_samples = (sample_rate * 0.18) as usize;
    for i in 0..total_samples {
        let t = i as f32 / sample_rate;
        let progress = i as f32 / total_samples.max(1) as f32;
        let fade_in = (progress / 0.12).clamp(0.0, 1.0);
        let fade_out = ((1.0 - progress) / 0.22).clamp(0.0, 1.0);
        let envelope = fade_in.min(fade_out);
        let tone = (2.0 * std::f32::consts::PI * 660.0 * t).sin() * 0.55
            + (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.25;
        let sample = (tone * envelope * i16::MAX as f32 * 0.28) as i16;
        writer
            .write_sample(sample)
            .map_err(|error| format!("write done chime {}: {error}", path.display()))?;
    }
    writer
        .finalize()
        .map_err(|error| format!("finalize done chime {}: {error}", path.display()))
}
