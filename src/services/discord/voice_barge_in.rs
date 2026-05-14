use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, GuildId, MessageId};
use tokio::sync::{Mutex, RwLock, broadcast};
use tokio_util::sync::CancellationToken;

use crate::services::provider::ProviderKind;
use crate::voice::barge_in::{
    BargeInPlayerStop, BargeInSensitivity, BargeInSensitivityState, DeferredBargeInBuffer,
    LiveBargeInCut, LiveBargeInMonitor, ProcessingBargeInDecision, run_sensitivity_ttl_reset,
};
use crate::voice::commands::{
    DEFAULT_WAKE_WORD, VoiceActiveAgentContext, VoiceCommand, VoiceLobbyRouteDecision,
    WakeWordCommand, WakeWordDecision, parse_voice_command, resolve_voice_lobby_route,
    wake_word_decision,
};
use crate::voice::config::DEFAULT_STT_LANGUAGE;
use crate::voice::progress;
use crate::voice::sanitizer::{foreground_spoken_only_with_limit, spoken_result_only_with_limit};
use crate::voice::stt::SttRuntime;
use crate::voice::tts::{
    TtsRuntime, TtsSynthesisKind,
    playback::{DEFAULT_TTS_CHUNK_MAX_CHARS, play_chunked_with_prefetch},
};
use crate::voice::{CompletedUtterance, VoiceConfig, VoiceReceiveHook};

use super::SharedData;
use super::voice_background_driver::{
    VoiceBackgroundStartRequest, VoiceBackgroundTurnDriver, select_voice_background_driver,
};
pub(in crate::services::discord) const INTERNAL_VOICE_MESSAGE_ID_START: u64 =
    9_000_000_000_000_000_000;

/// `true` iff `msg_id` is a synthetic voice-originated id (≥
/// `INTERNAL_VOICE_MESSAGE_ID_START`). Real Discord snowflakes encode
/// timestamps and worker/process/sequence fields and stay well below 2^63
/// for the foreseeable future, so the 9e18 prefix is safely above them.
/// Used by the message intake to skip ⏳/📬 reactions, placeholder POSTs,
/// and `message_reference` lookups that would fail with "Unknown message"
/// for a non-existent Discord message id.
pub(in crate::services::discord) fn is_synthetic_voice_message_id(
    msg_id: poise::serenity_prelude::MessageId,
) -> bool {
    msg_id.get() >= INTERNAL_VOICE_MESSAGE_ID_START
}
// F4 (#2046): progress/ack 재생 owner id 시작점. spoken_result owner 공간(1..)과
// 분리하기 위해 high range 사용.
const PROGRESS_PLAYBACK_OWNER_START: u64 = 1u64 << 63;
// F6 (#2046): voice config 핫캐시 TTL. 5초 안 utterance 는 캐시 재사용.
const VOICE_CONFIG_CACHE_TTL: Duration = Duration::from_secs(5);
const STT_TRANSCRIPT_POLL_TIMEOUT: Duration = Duration::from_secs(5);
const STT_TRANSCRIPT_POLL_INTERVAL: Duration = Duration::from_millis(200);
const PROCESSING_CHIME_FILE_NAME: &str = "agentdesk-voice-processing-chime.wav";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum VoiceBargeInTranscriptOutcome {
    Disabled,
    BargeInDisabled,
    EmptyTranscript,
    SensitivityChanged(BargeInSensitivity),
    VerboseProgressChanged {
        enabled: bool,
    },
    LanguageChanged(String),
    TtsVoiceChanged(String),
    VoiceCloneRequested {
        reference: Option<String>,
    },
    WakeWordsChanged {
        required: bool,
        wake_words: Vec<String>,
    },
    WakeWordRequired,
    AgentRoutingRequired,
    NoActiveTurn,
    Deferred(String),
    ExplicitStop {
        cancelled: bool,
        already_stopping: bool,
    },
    IgnoredNoise,
    TranscriptUnavailable,
    VoiceTurnStarted {
        turn_id: String,
    },
    VoiceTurnStartFailed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct VoiceProgressEvent {
    pub channel_id: u64,
    pub label: String,
}

#[derive(Clone)]
struct LivePlaybackSession {
    player: Arc<dyn BargeInPlayerStop>,
    cancellation: CancellationToken,
    owner: Option<u64>,
}

struct SpokenResultPlaybackSession {
    id: u64,
    cancellation: CancellationToken,
}

#[derive(Debug, Clone)]
struct ActiveVoiceRoute {
    agent_id: String,
    channel_id: ChannelId,
    updated_at: Instant,
}

#[derive(Clone, Debug)]
struct EffectiveVoiceForegroundConfig {
    provider: String,
    model: String,
    max_chars: usize,
    timeout_ms: u64,
}

enum VoiceTurnTargetResolution {
    Target {
        channel_id: ChannelId,
        transcript: String,
    },
    NeedsAgent,
    Ignored,
}

fn voice_lobby_accepts_source_channel(config: &VoiceConfig, channel_id: ChannelId) -> bool {
    match config.lobby_channel_id_u64() {
        Some(lobby_channel_id) => lobby_channel_id == channel_id.get(),
        None => true,
    }
}

fn normalized_foreground_max_chars(value: usize) -> usize {
    if value == 0 {
        crate::voice::config::DEFAULT_FOREGROUND_MAX_CHARS
    } else {
        value
    }
}

fn normalized_foreground_timeout_ms(value: u64) -> u64 {
    if value == 0 {
        crate::voice::config::DEFAULT_FOREGROUND_TIMEOUT_MS
    } else {
        value
    }
}

fn agent_voice_matches_channel(agent: &crate::config::AgentDef, channel_id: ChannelId) -> bool {
    agent
        .voice
        .channel_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<u64>().ok())
        == Some(channel_id.get())
}

fn agent_voice_background_channel(agent: &crate::config::AgentDef) -> Option<ChannelId> {
    let preferred_provider = agent.provider.trim();
    if !preferred_provider.is_empty()
        && let Some((_, Some(channel))) = agent
            .channels
            .iter()
            .into_iter()
            .find(|(provider, channel)| *provider == preferred_provider && channel.is_some())
        && let Some(channel_id) = channel
            .channel_id()
            .and_then(|value| value.parse::<u64>().ok())
    {
        return Some(ChannelId::new(channel_id));
    }

    agent
        .channels
        .iter()
        .into_iter()
        .filter_map(|(_, channel)| channel)
        .find_map(|channel| {
            channel
                .channel_id()
                .and_then(|value| value.parse::<u64>().ok())
                .map(ChannelId::new)
        })
}

fn agent_text_channel_matches(agent: &crate::config::AgentDef, channel_id: ChannelId) -> bool {
    let channel_id = channel_id.get().to_string();
    agent
        .channels
        .iter()
        // AgentChannels::iter returns a fixed array, so into_iter is required.
        .into_iter()
        .filter_map(|(_, channel)| channel)
        .any(|channel| channel.channel_id().as_deref() == Some(channel_id.as_str()))
}

fn foreground_ack_text(transcript: &str, language: &str) -> String {
    let english = language.trim().to_ascii_lowercase().starts_with("en");
    let looks_like_work = looks_like_background_work_request(transcript);
    match (english, looks_like_work) {
        (true, true) => {
            "Got it. I will start that in the channel and come back briefly.".to_string()
        }
        (true, false) => "Got it. I am checking that now.".to_string(),
        (false, true) => "알겠어요. 채널에서 바로 진행하고 짧게 다시 알려드릴게요.".to_string(),
        (false, false) => "알겠어요. 바로 확인할게요.".to_string(),
    }
}

async fn generate_foreground_ack_text(
    transcript: &str,
    language: &str,
    foreground: &EffectiveVoiceForegroundConfig,
) -> Option<String> {
    let _fallback_for_tests_and_docs = foreground_ack_text(transcript, language);
    let prompt =
        crate::voice::prompt::voice_foreground_prompt(transcript, language, foreground.max_chars);
    let provider = foreground.provider.clone();
    let model = foreground.model.clone();
    let max_chars = foreground.max_chars;
    let timeout = Duration::from_millis(foreground.timeout_ms);
    let result = tokio::time::timeout(
        timeout + Duration::from_millis(250),
        tokio::task::spawn_blocking(move || {
            let provider_kind = ProviderKind::from_str_or_unsupported(&provider);
            match provider_kind {
                ProviderKind::Claude => crate::services::claude::execute_command_simple_with_model(
                    &prompt,
                    Some(&model),
                ),
                ProviderKind::Codex => {
                    crate::services::codex::execute_command_simple_with_model(&prompt, Some(&model))
                }
                ProviderKind::Gemini | ProviderKind::OpenCode | ProviderKind::Qwen => Err(format!(
                    "foreground provider {} does not support model-scoped instant call yet",
                    provider_kind.as_str()
                )),
                ProviderKind::Unsupported(value) => {
                    Err(format!("unsupported foreground provider: {value}"))
                }
            }
        }),
    )
    .await;

    let text = match result {
        Ok(Ok(Ok(text))) => text,
        Ok(Ok(Err(error))) => {
            tracing::warn!(
                error = %error,
                foreground_provider = %foreground.provider,
                foreground_model = %foreground.model,
                "voice foreground model call failed; skipping spoken fallback"
            );
            return None;
        }
        Ok(Err(error)) => {
            tracing::warn!(
                error = %error,
                foreground_provider = %foreground.provider,
                foreground_model = %foreground.model,
                "voice foreground model task failed; skipping spoken fallback"
            );
            return None;
        }
        Err(_) => {
            tracing::warn!(
                timeout_ms = foreground.timeout_ms,
                foreground_provider = %foreground.provider,
                foreground_model = %foreground.model,
                "voice foreground model timed out; skipping spoken fallback"
            );
            return None;
        }
    };

    if text.trim().eq_ignore_ascii_case("ADK_VOICE_SILENCE") {
        return None;
    }
    let spoken = foreground_spoken_only_with_limit(&text, language, max_chars);
    if spoken.trim().is_empty() {
        None
    } else {
        Some(spoken)
    }
}

fn ensure_processing_chime_file(path: &Path) -> Result<(), String> {
    if path.metadata().map(|meta| meta.len() > 0).unwrap_or(false) {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!("create processing chime dir {}: {error}", parent.display())
        })?;
    }

    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|error| format!("create processing chime {}: {error}", path.display()))?;
    let sample_rate = spec.sample_rate as f32;
    let total_samples = (sample_rate * 0.18) as usize;
    for i in 0..total_samples {
        let t = i as f32 / sample_rate;
        let progress = i as f32 / total_samples.max(1) as f32;
        let fade_in = (progress / 0.12).clamp(0.0, 1.0);
        let fade_out = ((1.0 - progress) / 0.22).clamp(0.0, 1.0);
        let envelope = fade_in.min(fade_out);
        let tone = (2.0 * std::f32::consts::PI * 880.0 * t).sin() * 0.55
            + (2.0 * std::f32::consts::PI * 1320.0 * t).sin() * 0.25;
        let sample = (tone * envelope * i16::MAX as f32 * 0.28) as i16;
        writer
            .write_sample(sample)
            .map_err(|error| format!("write processing chime {}: {error}", path.display()))?;
    }
    writer
        .finalize()
        .map_err(|error| format!("finalize processing chime {}: {error}", path.display()))
}

fn looks_like_background_work_request(transcript: &str) -> bool {
    let text = transcript.to_ascii_lowercase();
    [
        "구현",
        "수정",
        "확인",
        "검토",
        "테스트",
        "배포",
        "이슈",
        "로그",
        "파일",
        "검색",
        "만들",
        "고쳐",
        "implement",
        "fix",
        "test",
        "deploy",
        "issue",
        "log",
        "file",
        "search",
        "review",
    ]
    .iter()
    .any(|needle| {
        if needle.is_ascii() {
            text.split(|ch: char| !ch.is_ascii_alphanumeric())
                .any(|word| word == *needle)
        } else {
            text.contains(needle)
        }
    })
}

struct DeferredBargeInDrain {
    acknowledgement: Option<String>,
    prompt: String,
}

struct VoiceProgressChannelState {
    active: bool,
    pending_events: Vec<String>,
    last_activity_at: Instant,
    next_idle_delay: Duration,
    next_summary_at: Option<Instant>,
}

impl VoiceProgressChannelState {
    fn new(now: Instant) -> Self {
        Self {
            active: true,
            pending_events: Vec::new(),
            last_activity_at: now,
            next_idle_delay: progress::PROGRESS_IDLE_NOTICE_INITIAL,
            next_summary_at: None,
        }
    }

    fn mark_active(&mut self, now: Instant) {
        self.active = true;
        self.last_activity_at = now;
        self.next_idle_delay = progress::PROGRESS_IDLE_NOTICE_INITIAL;
    }

    fn mark_done(&mut self) {
        self.active = false;
        self.pending_events.clear();
        self.next_summary_at = None;
    }
}

pub(in crate::services::discord) struct VoiceBargeInRuntime {
    enabled: bool,
    barge_in_enabled: bool,
    default_sensitivity: BargeInSensitivity,
    // F18 (#2046): RwLock try_read 실패 시 default 로 폴백하면 사용자가 설정한
    // Conservative 가 잠깐 Normal 로 잘못 평가될 수 있다. 최신 값을 lock-free 로
    // 읽을 수 있도록 atomic mirror 유지.
    sensitivity_atom: std::sync::atomic::AtomicU8,
    sensitivity_state: Arc<RwLock<BargeInSensitivityState>>,
    acknowledgement_enabled: bool,
    acknowledgement_text: String,
    transcript_dirs: Vec<PathBuf>,
    voice_config_state: RwLock<VoiceConfig>,
    spoken_result_language: RwLock<String>,
    verbose_progress: AtomicBool,
    stt: RwLock<Option<SttRuntime>>,
    tts: RwLock<Option<TtsRuntime>>,
    progress_tx: broadcast::Sender<VoiceProgressEvent>,
    monitors: dashmap::DashMap<u64, Arc<std::sync::Mutex<LiveBargeInMonitor>>>,
    playbacks: dashmap::DashMap<u64, Arc<LivePlaybackSession>>,
    spoken_result_playbacks: dashmap::DashMap<u64, SpokenResultPlaybackSession>,
    voice_guilds: dashmap::DashMap<u64, GuildId>,
    active_voice_routes: dashmap::DashMap<u64, ActiveVoiceRoute>,
    deferred_buffers: dashmap::DashMap<u64, Arc<Mutex<DeferredBargeInBuffer>>>,
    next_spoken_result_playback_id: AtomicU64,
    // F4 (#2046): progress/ack 재생 owner id 발급용. 30s 만료 타이머가 owner 일치
    // 시에만 playback entry 를 정리하도록 한다.
    next_progress_playback_id: AtomicU64,
    next_internal_message_id: AtomicU64,
    // F6 (#2046): `resolve_voice_turn_target` 가 매 utterance 마다 YAML 을
    // 재파싱하지 않도록 한 `Config` snapshot 핫캐시.
    config_cache: std::sync::Mutex<Option<(Instant, Arc<crate::config::Config>)>>,
    // F12 (#2046): voice alias collision 경고를 1회만 노출. utterance 마다 같은
    // collision 으로 warn 이 쏟아져 운영 로그가 묻히는 것을 막는다.
    alias_collision_signature: std::sync::Mutex<Option<String>>,
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
        let tts = if config.enabled {
            TtsRuntime::from_voice_config(config).ok()
        } else {
            None
        };
        let (progress_tx, _) = broadcast::channel(128);

        Self {
            enabled: config.enabled,
            barge_in_enabled: config.enabled && config.barge_in.enabled,
            default_sensitivity,
            sensitivity_atom: std::sync::atomic::AtomicU8::new(default_sensitivity.as_u8()),
            sensitivity_state: Arc::new(RwLock::new(BargeInSensitivityState::new(
                default_sensitivity,
                conservative_ttl,
            ))),
            acknowledgement_enabled: config.barge_in.acknowledgement_enabled,
            acknowledgement_text: config.barge_in.acknowledgement_text.clone(),
            transcript_dirs: transcript_dirs_from_config(config),
            voice_config_state: RwLock::new(config.clone()),
            spoken_result_language: RwLock::new(config.stt.language.clone()),
            verbose_progress: AtomicBool::new(config.verbose_progress),
            stt: RwLock::new(stt),
            tts: RwLock::new(tts),
            progress_tx,
            monitors: dashmap::DashMap::new(),
            playbacks: dashmap::DashMap::new(),
            spoken_result_playbacks: dashmap::DashMap::new(),
            voice_guilds: dashmap::DashMap::new(),
            active_voice_routes: dashmap::DashMap::new(),
            deferred_buffers: dashmap::DashMap::new(),
            next_spoken_result_playback_id: AtomicU64::new(1),
            next_progress_playback_id: AtomicU64::new(PROGRESS_PLAYBACK_OWNER_START),
            next_internal_message_id: AtomicU64::new(INTERNAL_VOICE_MESSAGE_ID_START),
            config_cache: std::sync::Mutex::new(None),
            alias_collision_signature: std::sync::Mutex::new(None),
        }
    }

    pub(in crate::services::discord) fn disabled() -> Self {
        let (progress_tx, _) = broadcast::channel(128);
        Self {
            enabled: false,
            barge_in_enabled: false,
            default_sensitivity: BargeInSensitivity::Normal,
            sensitivity_atom: std::sync::atomic::AtomicU8::new(BargeInSensitivity::Normal.as_u8()),
            sensitivity_state: Arc::new(RwLock::new(BargeInSensitivityState::default())),
            acknowledgement_enabled: false,
            acknowledgement_text: String::new(),
            transcript_dirs: Vec::new(),
            voice_config_state: RwLock::new(VoiceConfig::default()),
            spoken_result_language: RwLock::new(DEFAULT_STT_LANGUAGE.to_string()),
            verbose_progress: AtomicBool::new(false),
            stt: RwLock::new(None),
            tts: RwLock::new(None),
            progress_tx,
            monitors: dashmap::DashMap::new(),
            playbacks: dashmap::DashMap::new(),
            spoken_result_playbacks: dashmap::DashMap::new(),
            voice_guilds: dashmap::DashMap::new(),
            active_voice_routes: dashmap::DashMap::new(),
            deferred_buffers: dashmap::DashMap::new(),
            next_spoken_result_playback_id: AtomicU64::new(1),
            next_progress_playback_id: AtomicU64::new(PROGRESS_PLAYBACK_OWNER_START),
            next_internal_message_id: AtomicU64::new(INTERNAL_VOICE_MESSAGE_ID_START),
            config_cache: std::sync::Mutex::new(None),
            alias_collision_signature: std::sync::Mutex::new(None),
        }
    }

    pub(in crate::services::discord) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(in crate::services::discord) fn verbose_progress_enabled(&self) -> bool {
        self.verbose_progress.load(Ordering::Relaxed)
    }

    pub(in crate::services::discord) fn set_verbose_progress_enabled(&self, enabled: bool) {
        self.verbose_progress.store(enabled, Ordering::Relaxed);
    }

    async fn spoken_result_language(&self) -> String {
        self.spoken_result_language.read().await.clone()
    }

    async fn runtime_wake_word_decision(&self, transcript: &str) -> WakeWordDecision {
        let config = self.voice_config_state.read().await;
        wake_word_decision(transcript, &config.wake_words, config.wake_word_required())
    }

    async fn apply_dispatcher_command(
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

    async fn set_runtime_language(&self, language: String) {
        let config = {
            let mut config = self.voice_config_state.write().await;
            config.stt.language = language.clone();
            config.clone()
        };
        *self.spoken_result_language.write().await = language;
        if self.enabled {
            *self.stt.write().await = Some(SttRuntime::from_voice_config(&config));
        }
    }

    async fn set_runtime_tts_voice(&self, voice: String) {
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

    async fn apply_wake_word_command(&self, command: WakeWordCommand) -> Vec<String> {
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
        let label = label.into();
        if label.trim().is_empty() {
            return;
        }
        let _ = self.progress_tx.send(VoiceProgressEvent {
            channel_id: channel_id.get(),
            label,
        });
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
        // F7 (#2046): voice_guilds 만 지우면 channel_id 키로 적재된 monitors /
        // playbacks / spoken_result_playbacks / active_voice_routes /
        // deferred_buffers 가 남아 join/leave 반복 시 누수. 같은 guild 의 모든
        // control_channel_id 를 먼저 수집해 채널 단위 state 도 함께 정리한다.
        let stale_channels: Vec<u64> = self
            .voice_guilds
            .iter()
            .filter_map(|entry| {
                if *entry.value() == guild_id {
                    Some(*entry.key())
                } else {
                    None
                }
            })
            .collect();
        self.voice_guilds
            .retain(|_, registered_guild_id| *registered_guild_id != guild_id);
        for channel_id in stale_channels {
            self.monitors.remove(&channel_id);
            if let Some((_, session)) = self.playbacks.remove(&channel_id) {
                session.cancellation.cancel();
            }
            if let Some((_, session)) = self.spoken_result_playbacks.remove(&channel_id) {
                session.cancellation.cancel();
            }
            self.active_voice_routes.remove(&channel_id);
            self.deferred_buffers.remove(&channel_id);
        }
    }

    /// F2 (#2046): 특정 길드에 매핑된 control_channel_id 목록을 반환.
    /// `leave_voice_channel` 경로에서 `VoiceReceiver::flush_for_control_channel`을
    /// 길드 단위로 한정 호출하기 위해 사용한다.
    pub(in crate::services::discord) fn control_channel_ids_for_guild(
        &self,
        guild_id: GuildId,
    ) -> Vec<u64> {
        self.voice_guilds
            .iter()
            .filter_map(|entry| {
                if *entry.value() == guild_id {
                    Some(*entry.key())
                } else {
                    None
                }
            })
            .collect()
    }

    pub(in crate::services::discord) fn spawn_sensitivity_ttl_reset(
        self: &Arc<Self>,
        shutdown_flag: Arc<AtomicBool>,
    ) {
        if !self.barge_in_enabled {
            return;
        }

        let state = self.sensitivity_state.clone();
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
        if progress::is_turn_done_event(&label) {
            if let Some(state) = states.get_mut(&event.channel_id) {
                state.mark_done();
            }
            return;
        }

        let now = Instant::now();
        states
            .entry(event.channel_id)
            .or_insert_with(|| VoiceProgressChannelState::new(now))
            .mark_active(now);

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
            self.speak_progress_summary(shared, channel_id, events)
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
            let channel_id = ChannelId::new(raw_channel_id);
            if !super::mailbox_has_active_turn(shared, channel_id).await {
                if let Some(state) = states.get_mut(&raw_channel_id) {
                    state.mark_done();
                }
                continue;
            }

            let language = self.spoken_result_language().await;
            self.speak_progress_text(
                shared,
                channel_id,
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

    async fn speak_progress_text(
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

    async fn play_processing_chime(
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

    pub(in crate::services::discord) async fn set_sensitivity(
        &self,
        sensitivity: BargeInSensitivity,
    ) {
        // F18 (#2046): atomic mirror 를 먼저 갱신해 두면 try_read 충돌 윈도우에서도
        // current_sensitivity 가 최신 값을 본다.
        self.sensitivity_atom
            .store(sensitivity.as_u8(), Ordering::Relaxed);
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
        if !self.barge_in_enabled {
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
        self.reset_after_playback_start_with_owner(channel_id, player, cancellation, None);
    }

    fn reset_after_playback_start_with_owner<P>(
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

    pub(in crate::services::discord) fn clear_playback(&self, channel_id: ChannelId) {
        self.playbacks.remove(&channel_id.get());
    }

    fn clear_playback_if_owner(&self, channel_id: ChannelId, owner: u64) {
        self.playbacks
            .remove_if(&channel_id.get(), |_, session| session.owner == Some(owner));
    }

    fn start_spoken_result_playback(&self, channel_id: ChannelId) -> (u64, CancellationToken) {
        let id = self
            .next_spoken_result_playback_id
            .fetch_add(1, Ordering::SeqCst);
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

    fn clear_spoken_result_playback_if_current(&self, channel_id: ChannelId, id: u64) {
        self.spoken_result_playbacks
            .remove_if(&channel_id.get(), |_, session| session.id == id);
    }

    pub(in crate::services::discord) fn observe_live_pcm_i16(
        &self,
        channel_id: ChannelId,
        samples: &[i16],
    ) -> Option<LiveBargeInCut> {
        if !self.barge_in_enabled || samples.is_empty() {
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

        if !self.barge_in_enabled {
            return VoiceBargeInTranscriptOutcome::BargeInDisabled;
        }

        if let Some(outcome) = self.apply_dispatcher_command(channel_id, transcript).await {
            return outcome;
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
                // F22 (#2046): 사후 분석 라벨 강화. transcript 글자 수, 현재
                // sensitivity, 활성 progress playback 보유 여부.
                let sensitivity = self.current_sensitivity();
                let playback_active = self.playbacks.contains_key(&channel_id.get());
                tracing::info!(
                    channel_id = channel_id.get(),
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

    async fn start_voice_turn(
        self: &Arc<Self>,
        shared: &Arc<SharedData>,
        source_channel_id: ChannelId,
        target_channel_id: ChannelId,
        utterance: &CompletedUtterance,
        transcript: &str,
    ) -> VoiceBargeInTranscriptOutcome {
        let verbose_progress = self.verbose_progress_enabled();
        let language = self.spoken_result_language().await;
        let foreground = self
            .resolve_effective_foreground_config(source_channel_id, target_channel_id)
            .await;
        let background_provider = self
            .resolve_background_provider_for_target(target_channel_id)
            .await;
        let driver = select_voice_background_driver(&background_provider);
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
        match driver
            .start(VoiceBackgroundStartRequest {
                channel_id: target_channel_id,
                shared,
                message_content: &announcement,
            })
            .await
        {
            Ok(outcome) => {
                self.spawn_foreground_acknowledgement(
                    shared.clone(),
                    source_channel_id,
                    target_channel_id,
                    transcript.to_string(),
                    language.clone(),
                    foreground.clone(),
                );
                tracing::info!(
                    source_channel_id = source_channel_id.get(),
                    target_channel_id = target_channel_id.get(),
                    user_id = utterance.user_id,
                    utterance_id = %utterance.utterance_id,
                    turn_id = %outcome.turn_id,
                    background_provider = %background_provider.as_str(),
                    background_driver = %outcome.driver_kind.as_str(),
                    foreground_provider = %foreground.provider,
                    foreground_model = %foreground.model,
                    foreground_max_chars = foreground.max_chars,
                    "voice transcript announcement posted as canonical turn trigger"
                );
                return VoiceBargeInTranscriptOutcome::VoiceTurnStarted {
                    turn_id: outcome.turn_id,
                };
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    source_channel_id = source_channel_id.get(),
                    target_channel_id = target_channel_id.get(),
                    user_id = utterance.user_id,
                    utterance_id = %utterance.utterance_id,
                    "voice transcript announcement failed; refusing direct voice turn fallback"
                );
                return VoiceBargeInTranscriptOutcome::VoiceTurnStartFailed(format!(
                    "voice transcript announcement failed: {error}"
                ));
            }
        }
    }

    fn spawn_foreground_acknowledgement(
        self: &Arc<Self>,
        shared: Arc<SharedData>,
        source_channel_id: ChannelId,
        target_channel_id: ChannelId,
        transcript: String,
        language: String,
        foreground: EffectiveVoiceForegroundConfig,
    ) {
        let runtime = self.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            runtime
                .play_processing_chime(&shared, source_channel_id)
                .await;
            let prompt = crate::voice::prompt::voice_foreground_prompt(
                &transcript,
                &language,
                foreground.max_chars,
            );
            tracing::debug!(
                source_channel_id = source_channel_id.get(),
                target_channel_id = target_channel_id.get(),
                foreground_provider = %foreground.provider,
                foreground_model = %foreground.model,
                timeout_ms = foreground.timeout_ms,
                prompt_chars = prompt.chars().count(),
                "voice foreground prompt prepared for instant response"
            );
            let Some(ack) = generate_foreground_ack_text(&transcript, &language, &foreground).await
            else {
                tracing::info!(
                    source_channel_id = source_channel_id.get(),
                    target_channel_id = target_channel_id.get(),
                    elapsed_ms = started.elapsed().as_millis(),
                    foreground_provider = %foreground.provider,
                    foreground_model = %foreground.model,
                    "voice foreground skipped spoken acknowledgement after processing chime"
                );
                return;
            };
            let Some(path) = runtime
                .synthesize_acknowledgement(&ack, source_channel_id)
                .await
            else {
                return;
            };
            runtime
                .play_acknowledgement(&shared, source_channel_id, path)
                .await;
            tracing::info!(
                source_channel_id = source_channel_id.get(),
                target_channel_id = target_channel_id.get(),
                elapsed_ms = started.elapsed().as_millis(),
                foreground_provider = %foreground.provider,
                foreground_model = %foreground.model,
                "voice foreground first audio queued"
            );
        });
    }

    /// F6 (#2046): `Config` 핫캐시. TTL 안이면 캐시된 `Arc<Config>` 반환,
    /// 만료 시 spawn_blocking 으로 `load_graceful` 을 1회 호출해 갱신한다.
    /// 매 utterance 마다 동기 std::fs read + serde_yaml 파싱이 발생하던 hot path 를
    /// 5초 TTL 로 묶어 부하를 줄이고 async executor 블록도 회피한다.
    async fn cached_config(&self) -> Arc<crate::config::Config> {
        let now = Instant::now();
        if let Ok(guard) = self.config_cache.lock()
            && let Some((loaded_at, cached)) = guard.as_ref()
            && now.duration_since(*loaded_at) < VOICE_CONFIG_CACHE_TTL
        {
            return cached.clone();
        }
        let fresh = tokio::task::spawn_blocking(crate::config::load_graceful)
            .await
            .unwrap_or_else(|_| crate::config::Config::default());
        let arc = Arc::new(fresh);
        if let Ok(mut guard) = self.config_cache.lock() {
            *guard = Some((Instant::now(), arc.clone()));
        }
        arc
    }

    async fn resolve_effective_foreground_config(
        &self,
        source_channel_id: ChannelId,
        target_channel_id: ChannelId,
    ) -> EffectiveVoiceForegroundConfig {
        let config = self.cached_config().await;
        let mut provider = config.voice.foreground.provider.trim().to_string();
        if provider.is_empty() {
            provider = crate::voice::config::DEFAULT_FOREGROUND_PROVIDER.to_string();
        }
        let mut model = config.voice.foreground.model.trim().to_string();
        if model.is_empty() {
            model = crate::voice::config::DEFAULT_FOREGROUND_MODEL.to_string();
        }
        let mut max_chars = normalized_foreground_max_chars(config.voice.foreground.max_chars);
        let mut timeout_ms = normalized_foreground_timeout_ms(config.voice.foreground.timeout_ms);

        if let Some(agent) = config.agents.iter().find(|agent| {
            agent_voice_matches_channel(agent, source_channel_id)
                || agent_text_channel_matches(agent, target_channel_id)
                || agent_text_channel_matches(agent, source_channel_id)
        }) {
            if let Some(value) = agent
                .voice
                .foreground
                .provider
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                provider = value.to_string();
            }
            if let Some(value) = agent
                .voice
                .foreground
                .model
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                model = value.to_string();
            }
            if let Some(value) = agent.voice.foreground.max_chars {
                max_chars = normalized_foreground_max_chars(value);
            }
            if let Some(value) = agent.voice.foreground.timeout_ms {
                timeout_ms = normalized_foreground_timeout_ms(value);
            }
        }

        EffectiveVoiceForegroundConfig {
            provider,
            model,
            max_chars,
            timeout_ms,
        }
    }

    async fn resolve_background_provider_for_target(
        &self,
        target_channel_id: ChannelId,
    ) -> ProviderKind {
        let config = self.cached_config().await;
        config
            .agents
            .iter()
            .find(|agent| agent_text_channel_matches(agent, target_channel_id))
            .map(|agent| ProviderKind::from_str_or_unsupported(&agent.provider))
            .unwrap_or_else(|| ProviderKind::Unsupported("unknown".to_string()))
    }

    async fn resolve_voice_turn_target(
        &self,
        _shared: &Arc<SharedData>,
        source_channel_id: ChannelId,
        transcript: &str,
    ) -> VoiceTurnTargetResolution {
        let config = self.cached_config().await;
        if let Some((agent_id, target_channel_id)) = config.agents.iter().find_map(|agent| {
            if agent_voice_matches_channel(agent, source_channel_id) {
                agent_voice_background_channel(agent)
                    .map(|channel_id| (agent.id.clone(), channel_id))
            } else {
                None
            }
        }) {
            self.bind_routed_voice_context(source_channel_id, target_channel_id);
            self.active_voice_routes.insert(
                source_channel_id.get(),
                ActiveVoiceRoute {
                    agent_id,
                    channel_id: target_channel_id,
                    updated_at: Instant::now(),
                },
            );
            return VoiceTurnTargetResolution::Target {
                channel_id: target_channel_id,
                transcript: transcript.trim().to_string(),
            };
        }

        if super::settings::resolve_role_binding(source_channel_id, None).is_some() {
            return VoiceTurnTargetResolution::Target {
                channel_id: source_channel_id,
                transcript: transcript.trim().to_string(),
            };
        }

        if !voice_lobby_accepts_source_channel(&config.voice, source_channel_id) {
            tracing::debug!(
                source_channel_id = source_channel_id.get(),
                lobby_channel_id = config.voice.lobby_channel_id.as_deref(),
                "voice source channel is not role-bound or configured as voice lobby"
            );
            return VoiceTurnTargetResolution::Ignored;
        }

        let active_context = self
            .active_voice_routes
            .get(&source_channel_id.get())
            .map(|entry| VoiceActiveAgentContext {
                agent_id: entry.agent_id.clone(),
                channel_id: entry.channel_id.get(),
                updated_at: entry.updated_at,
            });
        let now = Instant::now();
        match resolve_voice_lobby_route(&config, transcript, active_context.as_ref(), now) {
            Ok(VoiceLobbyRouteDecision::Routed(route)) => {
                let remaining = route.remaining_transcript.trim();
                if remaining.is_empty() {
                    return VoiceTurnTargetResolution::NeedsAgent;
                }
                let target_channel_id = ChannelId::new(route.channel_id);
                self.bind_routed_voice_context(source_channel_id, target_channel_id);
                self.active_voice_routes.insert(
                    source_channel_id.get(),
                    ActiveVoiceRoute {
                        agent_id: route.agent_id,
                        channel_id: target_channel_id,
                        updated_at: now,
                    },
                );
                VoiceTurnTargetResolution::Target {
                    channel_id: target_channel_id,
                    transcript: remaining.to_string(),
                }
            }
            Ok(VoiceLobbyRouteDecision::ContinueActive {
                agent_id,
                channel_id,
                transcript,
            }) => {
                let target_channel_id = ChannelId::new(channel_id);
                self.bind_routed_voice_context(source_channel_id, target_channel_id);
                self.active_voice_routes.insert(
                    source_channel_id.get(),
                    ActiveVoiceRoute {
                        agent_id,
                        channel_id: target_channel_id,
                        updated_at: now,
                    },
                );
                VoiceTurnTargetResolution::Target {
                    channel_id: target_channel_id,
                    transcript,
                }
            }
            Ok(VoiceLobbyRouteDecision::NeedAgent) => VoiceTurnTargetResolution::NeedsAgent,
            Err(error) => {
                // F12 (#2046): 매 utterance 마다 같은 collision 으로 warn 이
                // 쏟아지는 것을 막기 위해 normalized signature 단위로 1회만 warn.
                let signature = error.normalized.clone();
                let first_time = if let Ok(mut guard) = self.alias_collision_signature.lock() {
                    if guard.as_deref() == Some(&signature) {
                        false
                    } else {
                        *guard = Some(signature.clone());
                        true
                    }
                } else {
                    true
                };
                if first_time {
                    tracing::warn!(
                        error = %error,
                        source_channel_id = source_channel_id.get(),
                        normalized = %signature,
                        "voice lobby routing disabled: alias collision detected (suppressed until alias changes)"
                    );
                } else {
                    tracing::debug!(
                        error = %error,
                        source_channel_id = source_channel_id.get(),
                        "voice lobby routing still blocked by previously logged alias collision"
                    );
                }
                VoiceTurnTargetResolution::NeedsAgent
            }
        }
    }

    fn bind_routed_voice_context(
        &self,
        source_channel_id: ChannelId,
        target_channel_id: ChannelId,
    ) {
        let Some(guild_id) = self
            .voice_guilds
            .get(&source_channel_id.get())
            .map(|entry| *entry.value())
        else {
            return;
        };
        self.voice_guilds.insert(target_channel_id.get(), guild_id);
    }

    async fn ask_for_agent(&self, shared: &Arc<SharedData>, channel_id: ChannelId) {
        let Some(http) = shared.serenity_http_or_token_fallback() else {
            return;
        };
        super::rate_limit_wait(shared, channel_id).await;
        if let Err(error) =
            super::http::send_channel_message(&http, channel_id, "어느 에이전트?").await
        {
            tracing::warn!(
                error = %error,
                channel_id = channel_id.get(),
                "failed to send voice lobby routing prompt"
            );
        }
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

        let transcript = match self
            .transcribe_completed_utterance(channel_id, utterance)
            .await
        {
            Some(transcript) => transcript,
            None => return VoiceBargeInTranscriptOutcome::TranscriptUnavailable,
        };

        let transcript = transcript.trim();
        if transcript.is_empty() {
            return VoiceBargeInTranscriptOutcome::EmptyTranscript;
        }

        let config_snapshot = crate::config::load_graceful();
        let source_is_lobby = super::settings::resolve_role_binding(channel_id, None).is_none()
            && voice_lobby_accepts_source_channel(&config_snapshot.voice, channel_id);
        let transcript = if source_is_lobby {
            transcript.to_string()
        } else {
            match self.runtime_wake_word_decision(transcript).await {
                WakeWordDecision::NotRequired(transcript) => transcript,
                WakeWordDecision::Matched(matched) => matched.remaining,
                WakeWordDecision::Missing => {
                    return VoiceBargeInTranscriptOutcome::WakeWordRequired;
                }
            }
        };
        let transcript = transcript.trim();
        if transcript.is_empty() {
            return VoiceBargeInTranscriptOutcome::EmptyTranscript;
        }

        if super::mailbox_has_active_turn(shared, channel_id).await {
            return self
                .handle_processing_transcript(shared, provider, channel_id, transcript)
                .await;
        }

        if let Some(outcome) = self.apply_dispatcher_command(channel_id, transcript).await {
            return outcome;
        }

        let (target_channel_id, transcript) = match self
            .resolve_voice_turn_target(shared, channel_id, transcript)
            .await
        {
            VoiceTurnTargetResolution::Target {
                channel_id,
                transcript,
            } => (channel_id, transcript),
            VoiceTurnTargetResolution::NeedsAgent => {
                self.ask_for_agent(shared, channel_id).await;
                return VoiceBargeInTranscriptOutcome::AgentRoutingRequired;
            }
            VoiceTurnTargetResolution::Ignored => {
                return VoiceBargeInTranscriptOutcome::NoActiveTurn;
            }
        };

        self.start_voice_turn(
            shared,
            channel_id,
            target_channel_id,
            utterance,
            &transcript,
        )
        .await
    }

    pub(in crate::services::discord) async fn drain_deferred_after_turn(
        self: &Arc<Self>,
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel_id: ChannelId,
    ) -> bool {
        if !self.barge_in_enabled {
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
        self.synthesize_progress_tts(text, channel_id, "voice barge-in acknowledgement")
            .await
    }

    async fn synthesize_progress_tts(
        &self,
        text: &str,
        channel_id: ChannelId,
        context: &'static str,
    ) -> Option<PathBuf> {
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

    async fn play_acknowledgement(
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
        let Some(ctx) = shared.cached_serenity_ctx.get() else {
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
        let Some(call_lock) = manager.get(guild_id) else {
            tracing::debug!(
                channel_id = channel_id.get(),
                guild_id = guild_id.get(),
                path = %path.display(),
                context,
                "voice progress playback skipped: no active songbird call"
            );
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
        let playback_id = self
            .next_progress_playback_id
            .fetch_add(1, Ordering::SeqCst);
        self.reset_after_playback_start_with_owner(
            channel_id,
            Arc::new(track),
            CancellationToken::new(),
            Some(playback_id),
        );
        let runtime = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            runtime.clear_playback_if_owner(channel_id, playback_id);
        });
        tracing::info!(
            channel_id = channel_id.get(),
            guild_id = guild_id.get(),
            path = %path.display(),
            context,
            playback_id,
            "voice progress playback started"
        );
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
                    let play_ms = report
                        .first_audio_start_ms
                        .unwrap_or(0)
                        .min(u64::MAX as u128) as u64;
                    crate::voice::metrics::record_tts(channel_id.get(), synth_ms, play_ms);
                    tracing::info!(
                        channel_id = channel_id.get(),
                        guild_id = guild_id.get(),
                        chunks = report.chunk_count,
                        played_chunks = report.played_chunks,
                        first_chunk_synthesis_ms = ?report.first_chunk_synthesis_ms,
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

    async fn transcribe_completed_utterance(
        &self,
        channel_id: ChannelId,
        utterance: &CompletedUtterance,
    ) -> Option<String> {
        let stt_started_at = std::time::Instant::now();
        if let Some(stt) = self.stt.read().await.clone() {
            match stt.transcribe(&utterance.path).await {
                Ok(transcript) => {
                    crate::voice::metrics::record_stt(
                        channel_id.get(),
                        Some(&utterance.utterance_id),
                        stt_started_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
                    );
                    return Some(transcript);
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
        crate::voice::metrics::record_stt(
            channel_id.get(),
            Some(&utterance.utterance_id),
            stt_started_at.elapsed().as_millis().min(u64::MAX as u128) as u64,
        );
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
        // F18 (#2046): try_read 실패 시 boot-time default 가 아닌 가장 최근에
        // 설정된 sensitivity 를 반환하도록 atomic mirror 로 폴백한다. TTL reset
        // 이 일어나는 짧은 윈도우라도 사용자가 설정한 Conservative 가 잠깐
        // Normal 로 평가되는 회귀를 막는다.
        self.sensitivity_state
            .try_read()
            .map(|state| state.sensitivity())
            .unwrap_or_else(|_| {
                BargeInSensitivity::from_u8(self.sensitivity_atom.load(Ordering::Relaxed))
            })
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
        // F22 (#2046): playback_owner 라벨 추가 — 어떤 progress / spoken_result
        // playback 이 cut 되었는지 사후 분석 가능.
        let playback_owner = self
            .runtime
            .playbacks
            .get(&channel_id.get())
            .and_then(|entry| entry.value().owner);
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
                playback_owner = ?playback_owner,
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

    #[test]
    fn foreground_ack_text_stays_short_and_routes_work_to_background() {
        assert_eq!(
            foreground_ack_text("이슈 구현하고 테스트해줘", "ko"),
            "알겠어요. 채널에서 바로 진행하고 짧게 다시 알려드릴게요."
        );
        assert_eq!(
            foreground_ack_text("what time is it?", "en-US"),
            "Got it. I am checking that now."
        );
    }

    fn test_agent(provider: &str) -> crate::config::AgentDef {
        crate::config::AgentDef {
            id: "project-agentdesk".to_string(),
            name: "AgentDesk".to_string(),
            name_ko: None,
            aliases: Vec::new(),
            wake_word: None,
            voice_enabled: true,
            sensitivity_mode: None,
            voice: crate::config::AgentVoiceConfig {
                channel_id: Some("300".to_string()),
                foreground: crate::config::AgentVoiceForegroundConfig::default(),
            },
            provider: provider.to_string(),
            channels: crate::config::AgentChannels {
                claude: Some(crate::config::AgentChannel::from("100")),
                codex: Some(crate::config::AgentChannel::from("200")),
                gemini: None,
                opencode: None,
                qwen: None,
            },
            keywords: Vec::new(),
            department: None,
            avatar_emoji: None,
        }
    }

    #[test]
    fn agent_voice_channel_routes_to_provider_main_channel() {
        let agent = test_agent("codex");
        assert!(agent_voice_matches_channel(&agent, ChannelId::new(300)));
        assert_eq!(
            agent_voice_background_channel(&agent),
            Some(ChannelId::new(200))
        );
    }

    #[test]
    fn agent_voice_channel_route_falls_back_to_first_text_channel() {
        let agent = test_agent("missing-provider");
        assert_eq!(
            agent_voice_background_channel(&agent),
            Some(ChannelId::new(100))
        );
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

    #[test]
    fn new_spoken_result_playback_cancels_previous_channel_playback() {
        let runtime = enabled_runtime();
        let channel_id = ChannelId::new(42);

        let (first_id, first_cancellation) = runtime.start_spoken_result_playback(channel_id);
        let (second_id, second_cancellation) = runtime.start_spoken_result_playback(channel_id);

        assert_ne!(first_id, second_id);
        assert!(first_cancellation.is_cancelled());
        assert!(!second_cancellation.is_cancelled());

        runtime.clear_spoken_result_playback_if_current(channel_id, first_id);
        assert!(runtime.spoken_result_playbacks.contains_key(&42));

        runtime.clear_spoken_result_playback_if_current(channel_id, second_id);
        assert!(!runtime.spoken_result_playbacks.contains_key(&42));
    }

    #[tokio::test]
    async fn progress_subscriber_receives_voice_turn_events() {
        let runtime = enabled_runtime();
        let mut rx = runtime.subscribe_progress();

        runtime.publish_progress(ChannelId::new(42), "tool:Bash");

        let event = rx.recv().await.unwrap();
        assert_eq!(event.channel_id, 42);
        assert_eq!(event.label, "tool:Bash");
    }

    #[test]
    fn stale_spoken_result_clear_does_not_remove_newer_live_playback() {
        let runtime = enabled_runtime();
        let channel_id = ChannelId::new(42);
        let first_player = Arc::new(MockPlayer::default());
        let second_player = Arc::new(MockPlayer::default());

        runtime.reset_after_playback_start_with_owner(
            channel_id,
            first_player,
            CancellationToken::new(),
            Some(1),
        );
        runtime.reset_after_playback_start_with_owner(
            channel_id,
            second_player,
            CancellationToken::new(),
            Some(2),
        );

        runtime.clear_playback_if_owner(channel_id, 1);

        assert_eq!(runtime.playbacks.get(&42).unwrap().owner, Some(2));
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
