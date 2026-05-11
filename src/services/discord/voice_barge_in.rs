use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, GuildId, MessageId};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

use crate::services::provider::ProviderKind;
use crate::voice::barge_in::{
    BargeInPlayerStop, BargeInSensitivity, BargeInSensitivityState, DeferredBargeInBuffer,
    LiveBargeInCut, LiveBargeInMonitor, ProcessingBargeInDecision, run_sensitivity_ttl_reset,
};
use crate::voice::stt::SttRuntime;
use crate::voice::tts::{TtsRuntime, TtsSynthesisKind};
use crate::voice::{CompletedUtterance, VoiceConfig, VoiceReceiveHook};

use super::SharedData;

const INTERNAL_VOICE_MESSAGE_ID_START: u64 = 9_000_000_000_000_000_000;
const STT_TRANSCRIPT_POLL_TIMEOUT: Duration = Duration::from_secs(5);
const STT_TRANSCRIPT_POLL_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum VoiceBargeInTranscriptOutcome {
    Disabled,
    EmptyTranscript,
    SensitivityChanged(BargeInSensitivity),
    NoActiveTurn,
    Deferred(String),
    ExplicitStop {
        cancelled: bool,
        already_stopping: bool,
    },
    IgnoredNoise,
    TranscriptUnavailable,
}

#[derive(Clone)]
struct LivePlaybackSession {
    player: Arc<dyn BargeInPlayerStop>,
    cancellation: CancellationToken,
}

struct DeferredBargeInDrain {
    acknowledgement: Option<String>,
    prompt: String,
}

pub(in crate::services::discord) struct VoiceBargeInRuntime {
    enabled: bool,
    default_sensitivity: BargeInSensitivity,
    sensitivity_state: Arc<RwLock<BargeInSensitivityState>>,
    acknowledgement_enabled: bool,
    acknowledgement_text: String,
    transcript_dirs: Vec<PathBuf>,
    stt: Option<SttRuntime>,
    tts: Option<TtsRuntime>,
    monitors: dashmap::DashMap<u64, Arc<std::sync::Mutex<LiveBargeInMonitor>>>,
    playbacks: dashmap::DashMap<u64, Arc<LivePlaybackSession>>,
    voice_guilds: dashmap::DashMap<u64, GuildId>,
    deferred_buffers: dashmap::DashMap<u64, Arc<Mutex<DeferredBargeInBuffer>>>,
    next_internal_message_id: AtomicU64,
}

impl VoiceBargeInRuntime {
    pub(in crate::services::discord) fn from_voice_config(config: &VoiceConfig) -> Self {
        let default_sensitivity = config.barge_in.sensitivity;
        let conservative_ttl = Duration::from_secs(config.barge_in.conservative_ttl_secs.max(1));
        let stt = if config.enabled {
            Some(SttRuntime::from_voice_config(config))
        } else {
            None
        };
        let tts = if config.enabled && config.barge_in.acknowledgement_enabled {
            TtsRuntime::from_voice_config(config).ok()
        } else {
            None
        };

        Self {
            enabled: config.enabled && config.barge_in.enabled,
            default_sensitivity,
            sensitivity_state: Arc::new(RwLock::new(BargeInSensitivityState::new(
                default_sensitivity,
                conservative_ttl,
            ))),
            acknowledgement_enabled: config.barge_in.acknowledgement_enabled,
            acknowledgement_text: config.barge_in.acknowledgement_text.clone(),
            transcript_dirs: transcript_dirs_from_config(config),
            stt,
            tts,
            monitors: dashmap::DashMap::new(),
            playbacks: dashmap::DashMap::new(),
            voice_guilds: dashmap::DashMap::new(),
            deferred_buffers: dashmap::DashMap::new(),
            next_internal_message_id: AtomicU64::new(INTERNAL_VOICE_MESSAGE_ID_START),
        }
    }

    pub(in crate::services::discord) fn disabled() -> Self {
        Self {
            enabled: false,
            default_sensitivity: BargeInSensitivity::Normal,
            sensitivity_state: Arc::new(RwLock::new(BargeInSensitivityState::default())),
            acknowledgement_enabled: false,
            acknowledgement_text: String::new(),
            transcript_dirs: Vec::new(),
            stt: None,
            tts: None,
            monitors: dashmap::DashMap::new(),
            playbacks: dashmap::DashMap::new(),
            voice_guilds: dashmap::DashMap::new(),
            deferred_buffers: dashmap::DashMap::new(),
            next_internal_message_id: AtomicU64::new(INTERNAL_VOICE_MESSAGE_ID_START),
        }
    }

    pub(in crate::services::discord) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(in crate::services::discord) fn register_voice_context(
        &self,
        control_channel_id: ChannelId,
        guild_id: GuildId,
    ) {
        if self.enabled {
            self.voice_guilds.insert(control_channel_id.get(), guild_id);
        }
    }

    pub(in crate::services::discord) fn unregister_voice_guild(&self, guild_id: GuildId) {
        self.voice_guilds
            .retain(|_, registered_guild_id| *registered_guild_id != guild_id);
    }

    pub(in crate::services::discord) fn spawn_sensitivity_ttl_reset(
        self: &Arc<Self>,
        shutdown_flag: Arc<AtomicBool>,
    ) {
        if !self.enabled {
            return;
        }

        let state = self.sensitivity_state.clone();
        let token = CancellationToken::new();
        let reset_token = token.clone();
        tokio::spawn(run_sensitivity_ttl_reset(state, reset_token));
        tokio::spawn(async move {
            while !shutdown_flag.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            token.cancel();
        });
    }

    pub(in crate::services::discord) async fn set_sensitivity(
        &self,
        sensitivity: BargeInSensitivity,
    ) {
        self.sensitivity_state
            .write()
            .await
            .set_sensitivity(sensitivity, Instant::now());
        self.update_existing_monitor_sensitivity(sensitivity);
    }

    pub(in crate::services::discord) async fn apply_voice_command(
        &self,
        transcript: &str,
    ) -> Option<BargeInSensitivity> {
        if !self.enabled {
            return None;
        }
        let sensitivity = self
            .sensitivity_state
            .write()
            .await
            .apply_voice_command(transcript, Instant::now())?;
        self.update_existing_monitor_sensitivity(sensitivity);
        Some(sensitivity)
    }

    pub(in crate::services::discord) fn reset_after_playback_start<P>(
        &self,
        channel_id: ChannelId,
        player: Arc<P>,
        cancellation: CancellationToken,
    ) where
        P: BargeInPlayerStop + 'static,
    {
        if !self.enabled {
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
            }),
        );
    }

    pub(in crate::services::discord) fn clear_playback(&self, channel_id: ChannelId) {
        self.playbacks.remove(&channel_id.get());
    }

    pub(in crate::services::discord) fn observe_live_pcm_i16(
        &self,
        channel_id: ChannelId,
        samples: &[i16],
    ) -> Option<LiveBargeInCut> {
        if !self.enabled || samples.is_empty() {
            return None;
        }

        let playback = self
            .playbacks
            .get(&channel_id.get())
            .map(|entry| entry.value().clone())?;
        let sensitivity = self.current_sensitivity();
        let monitor = self.monitor_for_channel(channel_id, sensitivity);
        let mut monitor = lock_monitor(&monitor);
        monitor.set_sensitivity(sensitivity);

        let pcm = pcm_i16_to_le_bytes(samples);
        match monitor.observe_pcm(&pcm, playback.player.as_ref(), &playback.cancellation) {
            Ok(Some(cut)) => {
                self.playbacks.remove(&channel_id.get());
                Some(cut)
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

        if let Some(sensitivity) = self.apply_voice_command(transcript).await {
            tracing::info!(
                channel_id = channel_id.get(),
                sensitivity = ?sensitivity,
                "voice barge-in sensitivity changed by spoken command"
            );
            return VoiceBargeInTranscriptOutcome::SensitivityChanged(sensitivity);
        }

        if !super::mailbox_has_active_turn(shared, channel_id).await {
            return VoiceBargeInTranscriptOutcome::NoActiveTurn;
        }

        let buffer = self.buffer_for_channel(channel_id);
        let decision = buffer
            .lock()
            .await
            .verify_processing_barge_in_after_stt(transcript);
        match decision {
            ProcessingBargeInDecision::AbortAgent => {
                let result = super::mailbox_cancel_active_turn_with_reason(
                    shared,
                    channel_id,
                    "voice_barge_in_explicit_stop",
                )
                .await;
                tracing::info!(
                    channel_id = channel_id.get(),
                    cancelled = result.token.is_some(),
                    already_stopping = result.already_stopping,
                    "voice explicit-stop barge-in processed"
                );
                VoiceBargeInTranscriptOutcome::ExplicitStop {
                    cancelled: result.token.is_some(),
                    already_stopping: result.already_stopping,
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

    pub(in crate::services::discord) async fn process_completed_utterance(
        &self,
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel_id: ChannelId,
        utterance: &CompletedUtterance,
    ) -> VoiceBargeInTranscriptOutcome {
        if !self.enabled {
            return VoiceBargeInTranscriptOutcome::Disabled;
        }

        let transcript = match self
            .transcribe_completed_utterance(channel_id, utterance)
            .await
        {
            Some(transcript) => transcript,
            None => return VoiceBargeInTranscriptOutcome::TranscriptUnavailable,
        };

        self.handle_processing_transcript(shared, provider, channel_id, &transcript)
            .await
    }

    pub(in crate::services::discord) async fn drain_deferred_after_turn(
        self: &Arc<Self>,
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel_id: ChannelId,
    ) -> bool {
        if !self.enabled {
            return false;
        }

        let Some(drain) = self.take_deferred_prompt(channel_id).await else {
            return false;
        };

        if let Some(acknowledgement) = drain.acknowledgement {
            if let Some(path) = self
                .synthesize_acknowledgement(&acknowledgement, channel_id)
                .await
            {
                self.play_acknowledgement(shared, channel_id, path).await;
            }
        }

        let message_id = MessageId::new(
            self.next_internal_message_id
                .fetch_add(1, Ordering::Relaxed),
        );
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

    async fn take_deferred_prompt(&self, channel_id: ChannelId) -> Option<DeferredBargeInDrain> {
        let buffer = self
            .deferred_buffers
            .get(&channel_id.get())
            .map(|entry| entry.value().clone())?;
        let mut buffer = buffer.lock().await;
        let acknowledgement = buffer
            .acknowledgement_before_drain(self.acknowledgement_enabled, &self.acknowledgement_text)
            .map(ToOwned::to_owned);
        let prompt = buffer.drain_prompt()?;
        Some(DeferredBargeInDrain {
            acknowledgement,
            prompt,
        })
    }

    async fn synthesize_acknowledgement(
        &self,
        text: &str,
        channel_id: ChannelId,
    ) -> Option<PathBuf> {
        let Some(tts) = self.tts.clone() else {
            return None;
        };
        match tts.synthesize(text, TtsSynthesisKind::Progress).await {
            Ok(output) => {
                tracing::info!(
                    channel_id = channel_id.get(),
                    path = %output.path.display(),
                    cache_status = ?output.cache_status,
                    "voice barge-in acknowledgement TTS synthesized"
                );
                Some(output.path)
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    channel_id = channel_id.get(),
                    "voice barge-in acknowledgement TTS synthesis failed"
                );
                None
            }
        }
    }

    async fn play_acknowledgement(
        self: &Arc<Self>,
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
        path: PathBuf,
    ) {
        let Some(guild_id) = self
            .voice_guilds
            .get(&channel_id.get())
            .map(|entry| *entry.value())
        else {
            tracing::debug!(
                channel_id = channel_id.get(),
                path = %path.display(),
                "voice barge-in acknowledgement playback skipped: no registered voice guild"
            );
            return;
        };
        let Some(ctx) = shared.cached_serenity_ctx.get() else {
            tracing::debug!(
                channel_id = channel_id.get(),
                guild_id = guild_id.get(),
                path = %path.display(),
                "voice barge-in acknowledgement playback skipped: no serenity context"
            );
            return;
        };
        let Some(manager) = songbird::get(ctx).await else {
            tracing::warn!(
                channel_id = channel_id.get(),
                guild_id = guild_id.get(),
                "voice barge-in acknowledgement playback skipped: songbird manager missing"
            );
            return;
        };
        let Some(call_lock) = manager.get(guild_id) else {
            tracing::debug!(
                channel_id = channel_id.get(),
                guild_id = guild_id.get(),
                path = %path.display(),
                "voice barge-in acknowledgement playback skipped: no active songbird call"
            );
            return;
        };

        let input = songbird::input::File::new(path.clone()).into();
        let track = {
            let mut call = call_lock.lock().await;
            call.play_input(input)
        };
        self.reset_after_playback_start(channel_id, Arc::new(track), CancellationToken::new());
        let runtime = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            runtime.clear_playback(channel_id);
        });
        tracing::info!(
            channel_id = channel_id.get(),
            guild_id = guild_id.get(),
            path = %path.display(),
            "voice barge-in acknowledgement playback started"
        );
    }

    async fn transcribe_completed_utterance(
        &self,
        channel_id: ChannelId,
        utterance: &CompletedUtterance,
    ) -> Option<String> {
        if let Some(stt) = self.stt.clone() {
            match stt.transcribe(&utterance.path).await {
                Ok(transcript) => return Some(transcript),
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
        Some(transcript)
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

    fn buffer_for_channel(&self, channel_id: ChannelId) -> Arc<Mutex<DeferredBargeInBuffer>> {
        self.deferred_buffers
            .entry(channel_id.get())
            .or_insert_with(|| Arc::new(Mutex::new(DeferredBargeInBuffer::new())))
            .clone()
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

    fn current_sensitivity(&self) -> BargeInSensitivity {
        self.sensitivity_state
            .try_read()
            .map(|state| state.sensitivity())
            .unwrap_or(self.default_sensitivity)
    }

    fn update_existing_monitor_sensitivity(&self, sensitivity: BargeInSensitivity) {
        for monitor in &self.monitors {
            lock_monitor(monitor.value()).set_sensitivity(sensitivity);
        }
    }
}

pub(in crate::services::discord) struct DiscordVoiceBargeInHook {
    runtime: Arc<VoiceBargeInRuntime>,
    shared: Arc<SharedData>,
    provider: ProviderKind,
}

impl DiscordVoiceBargeInHook {
    pub(in crate::services::discord) fn new(
        runtime: Arc<VoiceBargeInRuntime>,
        shared: Arc<SharedData>,
        provider: ProviderKind,
    ) -> Self {
        Self {
            runtime,
            shared,
            provider,
        }
    }
}

impl VoiceReceiveHook for DiscordVoiceBargeInHook {
    fn observe_pcm(&self, control_channel_id: u64, _user_id: u64, samples: &[i16]) {
        let channel_id = ChannelId::new(control_channel_id);
        let Some(cut) = self.runtime.observe_live_pcm_i16(channel_id, samples) else {
            return;
        };

        let shared = self.shared.clone();
        tokio::spawn(async move {
            let result = super::mailbox_cancel_active_turn_with_reason(
                &shared,
                channel_id,
                "voice_barge_in_live_cut",
            )
            .await;
            tracing::info!(
                channel_id = channel_id.get(),
                mean_db = cut.levels.mean_db,
                max_db = cut.levels.max_db,
                sensitivity = ?cut.sensitivity,
                candidate_frames = cut.candidate_frames,
                cancelled = result.token.is_some(),
                already_stopping = result.already_stopping,
                "voice live barge-in cut processed"
            );
        });
    }

    fn utterance_completed(&self, control_channel_id: u64, utterance: &CompletedUtterance) {
        let runtime = self.runtime.clone();
        let shared = self.shared.clone();
        let provider = self.provider.clone();
        let utterance = utterance.clone();
        tokio::spawn(async move {
            let channel_id = ChannelId::new(control_channel_id);
            let outcome = runtime
                .process_completed_utterance(&shared, &provider, channel_id, &utterance)
                .await;
            tracing::debug!(
                channel_id = channel_id.get(),
                utterance_id = %utterance.utterance_id,
                outcome = ?outcome,
                "voice barge-in transcript processing finished"
            );
        });
    }
}

fn pcm_i16_to_le_bytes(samples: &[i16]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

fn transcript_dirs_from_config(config: &VoiceConfig) -> Vec<PathBuf> {
    vec![expand_tilde(&config.audio.transcripts_dir)]
}

fn expand_tilde(path: &Path) -> PathBuf {
    let raw = path.to_string_lossy();
    if raw == "~" {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    path.to_path_buf()
}

fn lock_monitor(
    monitor: &std::sync::Mutex<LiveBargeInMonitor>,
) -> std::sync::MutexGuard<'_, LiveBargeInMonitor> {
    monitor
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[derive(Default)]
    struct MockPlayer {
        stops: AtomicUsize,
    }

    impl BargeInPlayerStop for MockPlayer {
        fn stop(&self) -> anyhow::Result<()> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn enabled_runtime() -> VoiceBargeInRuntime {
        let mut config = VoiceConfig::default();
        config.enabled = true;
        config.barge_in.acknowledgement_enabled = false;
        VoiceBargeInRuntime::from_voice_config(&config)
    }

    #[tokio::test]
    async fn spoken_sensitivity_command_updates_state_and_existing_monitor() {
        let runtime = enabled_runtime();
        let channel_id = ChannelId::new(42);
        let player = Arc::new(MockPlayer::default());
        runtime.reset_after_playback_start(channel_id, player, CancellationToken::new());

        assert_eq!(
            runtime.apply_voice_command("외부 보수 모드로 바꿔").await,
            Some(BargeInSensitivity::Conservative)
        );

        let monitor = runtime.monitors.get(&42).unwrap().value().clone();
        assert_eq!(
            lock_monitor(&monitor).sensitivity(),
            BargeInSensitivity::Conservative
        );
    }

    #[test]
    fn live_pcm_observation_stops_registered_player_and_cancels_token() {
        let runtime = enabled_runtime();
        let channel_id = ChannelId::new(42);
        let player = Arc::new(MockPlayer::default());
        let cancellation = CancellationToken::new();
        runtime.reset_after_playback_start(channel_id, player.clone(), cancellation.clone());

        let loud = [16_384, -16_384, 16_384, -16_384];
        assert!(runtime.observe_live_pcm_i16(channel_id, &loud).is_none());
        let cut = runtime.observe_live_pcm_i16(channel_id, &loud).unwrap();

        assert_eq!(cut.candidate_frames, 2);
        assert_eq!(player.stops.load(Ordering::SeqCst), 1);
        assert!(cancellation.is_cancelled());
        assert!(runtime.observe_live_pcm_i16(channel_id, &loud).is_none());
    }

    #[tokio::test]
    async fn deferred_drain_merges_prompt_and_acknowledgement() {
        let mut config = VoiceConfig::default();
        config.enabled = true;
        config.barge_in.acknowledgement_enabled = true;
        config.barge_in.acknowledgement_text = "확인했어요".to_string();
        let runtime = VoiceBargeInRuntime::from_voice_config(&config);
        let channel_id = ChannelId::new(42);
        let buffer = runtime.buffer_for_channel(channel_id);
        {
            let mut buffer = buffer.lock().await;
            buffer.push_transcript("첫 번째");
            buffer.push_transcript("두 번째");
        }

        let drain = runtime.take_deferred_prompt(channel_id).await.unwrap();

        assert_eq!(drain.acknowledgement, Some("확인했어요".to_string()));
        assert_eq!(drain.prompt, "첫 번째\n\n---\n\n두 번째");
        assert!(runtime.take_deferred_prompt(channel_id).await.is_none());
    }
}
