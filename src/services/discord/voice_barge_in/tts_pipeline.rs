use super::*;

impl VoiceBargeInRuntime {
    pub(super) async fn set_runtime_tts_voice(&self, voice: String) {
        let config = {
            let mut config = self.voice_config_state.write().await;
            config.tts.edge.voice = voice.clone();
            config.clone()
        };
        if self.enabled {
            // F10 (#2046): `.ok()` 가 Err 를 사일런트로 삼켜 TTS 가 통째로 꺼지던
            // 회귀를 방지. 실패 시 경고 로그만 남기고 기존 TTS 인스턴스를 보존.
            match TtsRuntime::from_voice_config(&config) {
                Ok(rt) => {
                    *self.tts.write().await = Some(rt);
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        voice = %voice,
                        "voice change ignored: TtsRuntime build failed, keeping previous TTS"
                    );
                }
            }
        }
    }

    pub(super) async fn synthesize_acknowledgement(
        &self,
        text: &str,
        channel_id: ChannelId,
    ) -> Option<PathBuf> {
        self.synthesize_progress_tts(text, channel_id, "voice barge-in acknowledgement")
            .await
    }

    pub(super) async fn synthesize_progress_tts(
        &self,
        text: &str,
        channel_id: ChannelId,
        context: &'static str,
    ) -> Option<PathBuf> {
        #[cfg(test)]
        {
            let request_index = {
                let mut requests = self
                    .test_state
                    .synth_requests
                    .lock()
                    .expect("voice test synth requests lock");
                requests.push((channel_id.get(), text.to_string(), context));
                requests.len()
            };
            if self.test_state.force_synth_success.load(Ordering::Relaxed) {
                return Some(
                    std::env::temp_dir()
                        .join(format!("agentdesk-test-voice-progress-{request_index}.wav")),
                );
            }
        }
        let Some(tts) = self.tts.read().await.clone() else {
            return None;
        };
        match tts.synthesize(text, TtsSynthesisKind::Progress).await {
            Ok(output) => {
                tracing::info!(
                    channel_id = channel_id.get(),
                    path = %output.path.display(),
                    cache_status = ?output.cache_status,
                    context,
                    "voice progress TTS synthesized"
                );
                Some(output.path)
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    channel_id = channel_id.get(),
                    context,
                    "voice progress TTS synthesis failed"
                );
                None
            }
        }
    }
}
