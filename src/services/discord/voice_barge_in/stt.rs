use super::*;

impl VoiceBargeInRuntime {
    pub(in crate::services::discord) fn observe_streaming_stt_pcm_i16(
        self: &Arc<Self>,
        channel_id: ChannelId,
        user_id: u64,
        samples: &[i16],
    ) {
        if samples.is_empty() {
            return;
        }
        let pcm = discord_pcm_i16_stereo_48k_to_mono_f32_16k(samples);
        if pcm.is_empty() {
            return;
        }
        let runtime = self.clone();
        let key = StreamingSttKey {
            channel_id: channel_id.get(),
            user_id,
        };
        let task_bucket = self
            .streaming_stt
            .feed_tasks()
            .entry(key)
            .or_insert_with(|| Arc::new(StdMutex::new(Vec::new())))
            .clone();
        let handle = tokio::spawn(async move {
            runtime
                .feed_streaming_stt_pcm(channel_id, user_id, pcm)
                .await;
        });
        if let Ok(mut tasks) = task_bucket.lock() {
            tasks.push(handle);
        } else {
            handle.abort();
        }
    }

    async fn feed_streaming_stt_pcm(
        self: &Arc<Self>,
        channel_id: ChannelId,
        user_id: u64,
        pcm: Vec<f32>,
    ) {
        let Some(stt) = self.stt.read().await.clone() else {
            return;
        };
        if !stt.is_streaming() {
            return;
        }

        let key = StreamingSttKey {
            channel_id: channel_id.get(),
            user_id,
        };
        let session = match self.streaming_stt.sessions().get(&key) {
            Some(entry) => entry.value().clone(),
            None => {
                let language = self.spoken_result_language().await;
                let new_session = match stt.start_session(&language).await {
                    Ok(session) => session,
                    Err(error) => {
                        tracing::warn!(
                            error = %error,
                            channel_id = channel_id.get(),
                            user_id,
                            "voice streaming STT session start failed"
                        );
                        return;
                    }
                };
                match self.streaming_stt.sessions().entry(key) {
                    dashmap::mapref::entry::Entry::Occupied(entry) => {
                        let existing = entry.get().clone();
                        if let Err(error) = stt.finalize(new_session).await {
                            tracing::debug!(
                                error = %error,
                                channel_id = channel_id.get(),
                                user_id,
                                "discarding duplicate voice streaming STT session failed"
                            );
                        }
                        existing
                    }
                    dashmap::mapref::entry::Entry::Vacant(entry) => {
                        entry.insert(new_session.clone());
                        new_session
                    }
                }
            }
        };

        if let Err(error) = stt.feed(&session, &pcm).await {
            tracing::warn!(
                error = %error,
                channel_id = channel_id.get(),
                user_id,
                "voice streaming STT feed failed"
            );
            return;
        }

        match stt.poll_partial(&session).await {
            Ok(Some(partial)) => {
                tracing::debug!(
                    channel_id = channel_id.get(),
                    user_id,
                    sequence = partial.window.as_ref().map(|window| window.sequence),
                    chars = partial.text.chars().count(),
                    "voice streaming STT partial updated"
                );
            }
            Ok(None) => {}
            Err(error) => {
                tracing::debug!(
                    error = %error,
                    channel_id = channel_id.get(),
                    user_id,
                    "voice streaming STT partial poll failed"
                );
            }
        }
    }

    async fn drain_streaming_stt_feed_tasks(&self, key: StreamingSttKey) {
        let Some((_, task_bucket)) = self.streaming_stt.feed_tasks().remove(&key) else {
            return;
        };
        let tasks = match task_bucket.lock() {
            Ok(mut tasks) => tasks.drain(..).collect::<Vec<_>>(),
            Err(error) => error.into_inner().drain(..).collect::<Vec<_>>(),
        };
        for task in tasks {
            if let Err(error) = task.await {
                tracing::debug!(
                    ?key,
                    %error,
                    "voice streaming STT feed task finished with join error before finalization"
                );
            }
        }
    }

    pub(super) async fn transcribe_completed_utterance(
        &self,
        channel_id: ChannelId,
        utterance: &CompletedUtterance,
    ) -> Option<TranscribedVoiceUtterance> {
        let stt_started_at = std::time::Instant::now();
        if let Some(stt) = self.stt.read().await.clone() {
            if stt.is_streaming() {
                let key = StreamingSttKey {
                    channel_id: channel_id.get(),
                    user_id: utterance.user_id,
                };
                self.drain_streaming_stt_feed_tasks(key).await;
                if let Some((_, session)) = self.streaming_stt.sessions().remove(&key) {
                    match stt.finalize(session).await {
                        Ok(transcript) if !transcript.text.trim().is_empty() => {
                            let stt_latency_ms =
                                stt_started_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
                            crate::voice::metrics::record_stt(
                                channel_id.get(),
                                Some(&utterance.utterance_id),
                                stt_latency_ms,
                            );
                            return Some(TranscribedVoiceUtterance {
                                text: transcript.text,
                                stt_mode: "stream",
                                stt_latency_ms,
                            });
                        }
                        Ok(_) => {
                            tracing::debug!(
                                channel_id = channel_id.get(),
                                utterance_id = %utterance.utterance_id,
                                "voice streaming STT finalized empty transcript; falling back to file STT"
                            );
                        }
                        Err(error) => {
                            tracing::warn!(
                                error = %error,
                                channel_id = channel_id.get(),
                                utterance_id = %utterance.utterance_id,
                                "voice streaming STT finalize failed; falling back to file STT"
                            );
                        }
                    }
                } else {
                    tracing::debug!(
                        channel_id = channel_id.get(),
                        utterance_id = %utterance.utterance_id,
                        "voice streaming STT had no live session for utterance; falling back to file STT"
                    );
                }
            }

            match stt.transcribe_file(&utterance.path).await {
                Ok(transcript) => {
                    let stt_latency_ms =
                        stt_started_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
                    crate::voice::metrics::record_stt(
                        channel_id.get(),
                        Some(&utterance.utterance_id),
                        stt_latency_ms,
                    );
                    return Some(TranscribedVoiceUtterance {
                        text: transcript,
                        stt_mode: if stt.is_streaming() {
                            "file_fallback"
                        } else {
                            "file"
                        },
                        stt_latency_ms,
                    });
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        channel_id = channel_id.get(),
                        utterance_id = %utterance.utterance_id,
                        path = %utterance.path.display(),
                        "voice STT transcription failed; falling back to transcript sidecar"
                    );
                }
            }
        }

        let Some(transcript) = self.wait_for_stt_transcript(utterance).await else {
            tracing::debug!(
                channel_id = channel_id.get(),
                utterance_id = %utterance.utterance_id,
                path = %utterance.path.display(),
                "voice barge-in skipped utterance because no STT transcript sidecar appeared"
            );
            return None;
        };
        let stt_latency_ms = stt_started_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
        crate::voice::metrics::record_stt(
            channel_id.get(),
            Some(&utterance.utterance_id),
            stt_latency_ms,
        );
        Some(TranscribedVoiceUtterance {
            text: transcript,
            stt_mode: "sidecar",
            stt_latency_ms,
        })
    }

    async fn wait_for_stt_transcript(&self, utterance: &CompletedUtterance) -> Option<String> {
        let deadline = tokio::time::Instant::now() + STT_TRANSCRIPT_POLL_TIMEOUT;
        let candidates = self.transcript_path_candidates(utterance);
        loop {
            for path in &candidates {
                match tokio::fs::read_to_string(path).await {
                    Ok(text) if !text.trim().is_empty() => return Some(text),
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        tracing::warn!(
                            error = %error,
                            path = %path.display(),
                            utterance_id = %utterance.utterance_id,
                            "failed to read voice STT transcript sidecar"
                        );
                    }
                }
            }

            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(STT_TRANSCRIPT_POLL_INTERVAL).await;
        }
    }

    fn transcript_path_candidates(&self, utterance: &CompletedUtterance) -> Vec<PathBuf> {
        let mut candidates = Vec::new();
        candidates.push(utterance.path.with_extension("txt"));
        for dir in &self.transcript_dirs {
            candidates.push(
                dir.join(format!("user_{}", utterance.user_id))
                    .join(format!("{}.txt", utterance.utterance_id)),
            );
            candidates.push(dir.join(format!("{}.txt", utterance.utterance_id)));
        }
        candidates
    }

    /// #2156: process_completed_utterance 가 끝나면 utterance wav / segment wav /
    /// transcript sidecar 를 삭제한다. config `voice.keep_recordings` 가 true 거나
    /// 환경변수 `ADK_VOICE_KEEP_WAV=1` 이면 보존한다.
    ///
    /// Race 노트: 외부 STT subprocess 가 sidecar `.txt` 를 비동기로 쓰는 경로에서,
    /// `wait_for_stt_transcript` 의 polling 이 timeout 으로 끝난 직후 cleanup 이
    /// 돌면 sidecar 가 늦게 도착해 즉시 삭제될 수 있다. 이미 polling 단계에서
    /// 충분히 기다린 뒤이므로 손실은 운영자 관점에서 "이 utterance 는 STT 가
    /// 끝내 실패한 것" 과 동치다. 보존이 필요하면 `keep_recordings=true` 로 두면
    /// sidecar 가 그대로 남는다.
    pub(super) async fn cleanup_utterance_artifacts(&self, utterance: &CompletedUtterance) {
        if self.voice_config_state.read().await.keep_voice_recordings() {
            return;
        }
        remove_file_quietly(&utterance.path).await;
        for segment in &utterance.segment_paths {
            remove_file_quietly(segment).await;
        }
        for candidate in self.transcript_path_candidates(utterance) {
            remove_file_quietly_silent(&candidate).await;
        }
    }
}
