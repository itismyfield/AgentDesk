use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

use crate::voice::barge_in::BargeInSensitivity;

// F17 (#2046): 상대경로(`.cache/...`)는 dcserver CWD 에 따라 위치가 달라져 launchd
// 실행 시 `/` CWD 에서 권한 거부가 발생했다. STT/Receiver 와 동일하게 `~/.adk/...`
// 절대경로(틸드 확장)로 변경.
pub(crate) const DEFAULT_PROGRESS_TTS_CACHE_DIR: &str = "~/.adk/voice/tts-cache-progress";
pub(crate) const DEFAULT_EDGE_TTS_COMMAND: &str = "edge-tts";
pub(crate) const DEFAULT_EDGE_TTS_VOICE: &str = "ko-KR-SunHiNeural";
pub(crate) const DEFAULT_EDGE_TTS_RATE: &str = "+0%";
pub(crate) const DEFAULT_STT_FFMPEG_COMMAND: &str = "ffmpeg";
pub(crate) const DEFAULT_STT_WHISPER_COMMAND: &str = "whisper-cli";
pub(crate) const DEFAULT_STT_MODEL_PATH: &str = "~/.adk/voice/models/ggml-large-v3-turbo.bin";
pub(crate) const DEFAULT_STT_LANGUAGE: &str = "ko";
pub(crate) const DEFAULT_BARGE_IN_ACKNOWLEDGEMENT: &str =
    "그동안 말씀하신 거 같이 정리해서 작업할게요.";
pub(crate) const DEFAULT_BARGE_IN_TTL_SECS: u64 = 15 * 60;
pub(crate) const DEFAULT_ACTIVE_AGENT_TTL_SECS: u64 = 180;
pub(crate) const DEFAULT_FOREGROUND_PROVIDER: &str = "claude";
pub(crate) const DEFAULT_FOREGROUND_MODEL: &str = "sonnet";
pub(crate) const DEFAULT_FOREGROUND_MAX_CHARS: usize = 220;
pub(crate) const DEFAULT_FOREGROUND_TIMEOUT_MS: u64 = 3_000;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub(crate) struct VoiceConfig {
    pub enabled: bool,
    pub verbose_progress: bool,
    pub audio: VoiceAudioDirs,
    pub stt: VoiceSttConfig,
    pub tts: VoiceTtsConfig,
    pub thresholds: VoiceDbThresholds,
    pub idle: VoiceIdleTimings,
    pub barge_in: VoiceBargeInConfig,
    pub wake_words: Vec<String>,
    pub allowed_user_ids: Vec<String>,
    pub lobby_channel_id: Option<String>,
    pub active_agent_ttl_seconds: u64,
    pub foreground: VoiceForegroundConfig,
    pub spoken_result: VoiceSpokenResultConfig,
    pub default_sensitivity_mode: BargeInSensitivity,
    pub auto_join_channel_ids: Vec<String>,
    /// `false` (기본값) 이면 utterance / segment wav 와 transcript sidecar 를
    /// STT 직후 삭제하고, 시작 시 기존 누적분도 GC 한다 (#2156). 디버그/품질
    /// 분석 목적으로 보존하려면 `true` 로 두거나 환경변수 `ADK_VOICE_KEEP_WAV=1`
    /// 을 설정하면 보존된다.
    pub keep_recordings: bool,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            verbose_progress: false,
            audio: VoiceAudioDirs::default(),
            stt: VoiceSttConfig::default(),
            tts: VoiceTtsConfig::default(),
            thresholds: VoiceDbThresholds::default(),
            idle: VoiceIdleTimings::default(),
            barge_in: VoiceBargeInConfig::default(),
            wake_words: vec!["agentdesk".to_string()],
            allowed_user_ids: Vec::new(),
            lobby_channel_id: None,
            active_agent_ttl_seconds: DEFAULT_ACTIVE_AGENT_TTL_SECS,
            foreground: VoiceForegroundConfig::default(),
            spoken_result: VoiceSpokenResultConfig::default(),
            default_sensitivity_mode: BargeInSensitivity::Normal,
            auto_join_channel_ids: Vec::new(),
            keep_recordings: false,
        }
    }
}

impl VoiceConfig {
    pub(crate) fn is_default(&self) -> bool {
        self == &Self::default()
    }

    pub(crate) fn wake_word_required(&self) -> bool {
        std::env::var("REQUIRE_WAKE_WORD")
            .ok()
            .and_then(|value| parse_bool_env(&value))
            .unwrap_or_else(|| {
                self.wake_words
                    .iter()
                    .any(|wake_word| !wake_word.trim().is_empty())
            })
    }

    pub(crate) fn lobby_channel_id_u64(&self) -> Option<u64> {
        self.lobby_channel_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .and_then(|value| value.parse::<u64>().ok())
    }

    pub(crate) fn is_lobby_channel(&self, channel_id: u64) -> bool {
        self.lobby_channel_id_u64() == Some(channel_id)
    }

    pub(crate) fn active_agent_context_ttl(&self) -> Duration {
        Duration::from_secs(match self.active_agent_ttl_seconds {
            0 => DEFAULT_ACTIVE_AGENT_TTL_SECS,
            value => value,
        })
    }

    /// `keep_recordings` 또는 환경변수 `ADK_VOICE_KEEP_WAV` 에 따라 utterance wav /
    /// segment / transcript sidecar 를 보존할지 결정한다 (#2156). 환경변수가 명시적으로
    /// 설정된 경우 config 값을 덮어쓴다.
    pub(crate) fn keep_voice_recordings(&self) -> bool {
        if let Some(env_value) = std::env::var("ADK_VOICE_KEEP_WAV")
            .ok()
            .as_deref()
            .and_then(parse_bool_env)
        {
            return env_value;
        }
        self.keep_recordings
    }

    pub(crate) fn auto_join_channel_ids_with_lobby(&self) -> Vec<String> {
        let mut channel_ids = Vec::new();
        if let Some(lobby_channel_id) = self
            .lobby_channel_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            channel_ids.push(lobby_channel_id.to_string());
        }

        for channel_id in &self.auto_join_channel_ids {
            let channel_id = channel_id.trim();
            if channel_id.is_empty() || channel_ids.iter().any(|existing| existing == channel_id) {
                continue;
            }
            channel_ids.push(channel_id.to_string());
        }

        channel_ids
    }
}

fn parse_bool_env(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct VoiceForegroundConfig {
    pub provider: String,
    pub model: String,
    pub max_chars: usize,
    pub timeout_ms: u64,
}

impl Default for VoiceForegroundConfig {
    fn default() -> Self {
        Self {
            provider: DEFAULT_FOREGROUND_PROVIDER.to_string(),
            model: DEFAULT_FOREGROUND_MODEL.to_string(),
            max_chars: DEFAULT_FOREGROUND_MAX_CHARS,
            timeout_ms: DEFAULT_FOREGROUND_TIMEOUT_MS,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct VoiceSpokenResultConfig {
    pub max_chars: usize,
}

impl Default for VoiceSpokenResultConfig {
    fn default() -> Self {
        Self {
            max_chars: crate::voice::sanitizer::DEFAULT_SPOKEN_RESULT_CHAR_LIMIT,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct VoiceSttConfig {
    pub ffmpeg_command: String,
    pub whisper_command: String,
    pub model_path: PathBuf,
    pub language: String,
}

impl Default for VoiceSttConfig {
    fn default() -> Self {
        Self {
            ffmpeg_command: DEFAULT_STT_FFMPEG_COMMAND.to_string(),
            whisper_command: DEFAULT_STT_WHISPER_COMMAND.to_string(),
            model_path: PathBuf::from(DEFAULT_STT_MODEL_PATH),
            language: DEFAULT_STT_LANGUAGE.to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct VoiceTtsConfig {
    pub backend: VoiceTtsBackendKind,
    pub progress_cache_dir: PathBuf,
    pub edge: VoiceEdgeTtsConfig,
}

impl Default for VoiceTtsConfig {
    fn default() -> Self {
        Self {
            backend: VoiceTtsBackendKind::Edge,
            progress_cache_dir: PathBuf::from(DEFAULT_PROGRESS_TTS_CACHE_DIR),
            edge: VoiceEdgeTtsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum VoiceTtsBackendKind {
    #[default]
    Edge,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct VoiceEdgeTtsConfig {
    pub command: String,
    pub voice: String,
    pub rate: String,
}

impl Default for VoiceEdgeTtsConfig {
    fn default() -> Self {
        Self {
            command: DEFAULT_EDGE_TTS_COMMAND.to_string(),
            voice: DEFAULT_EDGE_TTS_VOICE.to_string(),
            rate: DEFAULT_EDGE_TTS_RATE.to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct VoiceAudioDirs {
    pub recordings_dir: PathBuf,
    pub transcripts_dir: PathBuf,
    pub tts_cache_dir: PathBuf,
    pub temp_dir: PathBuf,
}

impl Default for VoiceAudioDirs {
    fn default() -> Self {
        Self {
            recordings_dir: PathBuf::from("~/.adk/voice/recordings"),
            transcripts_dir: PathBuf::from("~/.adk/voice/transcripts"),
            tts_cache_dir: PathBuf::from("~/.adk/voice/tts-cache"),
            temp_dir: PathBuf::from("~/.adk/voice/tmp"),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct VoiceBargeInConfig {
    pub enabled: bool,
    pub sensitivity: BargeInSensitivity,
    pub conservative_ttl_secs: u64,
    pub acknowledgement_enabled: bool,
    pub acknowledgement_text: String,
}

impl Default for VoiceBargeInConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sensitivity: BargeInSensitivity::Normal,
            conservative_ttl_secs: DEFAULT_BARGE_IN_TTL_SECS,
            acknowledgement_enabled: true,
            acknowledgement_text: DEFAULT_BARGE_IN_ACKNOWLEDGEMENT.to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub(crate) struct VoiceDbThresholds {
    pub speech_start_db: f32,
    pub speech_end_db: f32,
    pub wake_word_db: f32,
}

impl Default for VoiceDbThresholds {
    fn default() -> Self {
        Self {
            speech_start_db: -45.0,
            speech_end_db: -55.0,
            wake_word_db: -50.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct VoiceIdleTimings {
    pub segment_idle_ms: u64,
    pub utterance_idle_ms: u64,
    pub channel_idle_disconnect_secs: u64,
    pub wake_listen_window_secs: u64,
}

impl Default for VoiceIdleTimings {
    fn default() -> Self {
        Self {
            segment_idle_ms: 2_200,
            utterance_idle_ms: 4_500,
            channel_idle_disconnect_secs: 300,
            wake_listen_window_secs: 8,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_config_defaults_to_disabled() {
        let config = VoiceConfig::default();

        assert!(!config.enabled);
        assert!(!config.verbose_progress);
        assert!(config.allowed_user_ids.is_empty());
        assert_eq!(config.lobby_channel_id_u64(), None);
        assert_eq!(
            config.active_agent_context_ttl(),
            Duration::from_secs(DEFAULT_ACTIVE_AGENT_TTL_SECS)
        );
        assert!(config.auto_join_channel_ids.is_empty());
        assert_eq!(config.wake_words, vec!["agentdesk"]);
        assert_eq!(config.stt.ffmpeg_command, DEFAULT_STT_FFMPEG_COMMAND);
        assert_eq!(config.stt.whisper_command, DEFAULT_STT_WHISPER_COMMAND);
        assert_eq!(config.stt.model_path, PathBuf::from(DEFAULT_STT_MODEL_PATH));
        assert_eq!(config.stt.language, DEFAULT_STT_LANGUAGE);
        assert_eq!(config.tts.backend, VoiceTtsBackendKind::Edge);
        assert_eq!(config.foreground, VoiceForegroundConfig::default());
        assert_eq!(config.spoken_result, VoiceSpokenResultConfig::default());
        assert_eq!(
            config.tts.progress_cache_dir,
            PathBuf::from(DEFAULT_PROGRESS_TTS_CACHE_DIR)
        );
        assert_eq!(config.tts.edge.voice, DEFAULT_EDGE_TTS_VOICE);
        assert!(config.barge_in.enabled);
        assert_eq!(config.barge_in.sensitivity, BargeInSensitivity::Normal);
        assert_eq!(
            config.barge_in.conservative_ttl_secs,
            DEFAULT_BARGE_IN_TTL_SECS
        );
        assert!(config.barge_in.acknowledgement_enabled);
    }

    #[test]
    fn voice_config_deserializes_partial_yaml_with_defaults() {
        let config: VoiceConfig = serde_yaml::from_str(
            r#"
enabled: true
verbose_progress: true
audio:
  recordings_dir: /tmp/voice-recordings
thresholds:
  speech_start_db: -42.5
idle:
  segment_idle_ms: 2000
  channel_idle_disconnect_secs: 120
wake_words:
  - desk
allowed_user_ids:
  - "343742347365974026"
lobby_channel_id: "1509999999999999999"
active_agent_ttl_seconds: 240
auto_join_channel_ids:
  - "1500000000000000000"
foreground:
  provider: codex
  model: gpt-5.5-instant
  max_chars: 180
  timeout_ms: 2500
spoken_result:
  max_chars: 720
"#,
        )
        .unwrap();

        assert!(config.enabled);
        assert!(config.verbose_progress);
        assert_eq!(
            config.audio.recordings_dir,
            PathBuf::from("/tmp/voice-recordings")
        );
        assert_eq!(
            config.audio.transcripts_dir,
            PathBuf::from("~/.adk/voice/transcripts")
        );
        assert_eq!(config.thresholds.speech_start_db, -42.5);
        assert_eq!(config.thresholds.speech_end_db, -55.0);
        assert_eq!(config.stt, VoiceSttConfig::default());
        assert_eq!(config.tts.backend, VoiceTtsBackendKind::Edge);
        assert_eq!(config.tts.edge.command, DEFAULT_EDGE_TTS_COMMAND);
        assert_eq!(config.tts.edge.rate, DEFAULT_EDGE_TTS_RATE);
        assert_eq!(config.idle.segment_idle_ms, 2_000);
        assert_eq!(config.idle.channel_idle_disconnect_secs, 120);
        assert_eq!(config.idle.utterance_idle_ms, 4_500);
        assert_eq!(config.barge_in, VoiceBargeInConfig::default());
        assert_eq!(config.wake_words, vec!["desk"]);
        assert_eq!(config.allowed_user_ids, vec!["343742347365974026"]);
        assert_eq!(
            config.lobby_channel_id,
            Some("1509999999999999999".to_string())
        );
        assert!(config.is_lobby_channel(1_509_999_999_999_999_999));
        assert_eq!(config.active_agent_context_ttl(), Duration::from_secs(240));
        assert_eq!(config.auto_join_channel_ids, vec!["1500000000000000000"]);
        assert_eq!(
            config.auto_join_channel_ids_with_lobby(),
            vec!["1509999999999999999", "1500000000000000000"]
        );
        assert_eq!(config.foreground.provider, "codex");
        assert_eq!(config.foreground.model, "gpt-5.5-instant");
        assert_eq!(config.foreground.max_chars, 180);
        assert_eq!(config.foreground.timeout_ms, 2_500);
        assert_eq!(config.spoken_result.max_chars, 720);
    }

    #[test]
    fn voice_config_deserializes_stt_settings() {
        let config: VoiceConfig = serde_yaml::from_str(
            r#"
stt:
  ffmpeg_command: /opt/homebrew/bin/ffmpeg
  whisper_command: /opt/homebrew/bin/whisper-cli
  model_path: /models/ggml-large-v3-turbo.bin
  language: ko
"#,
        )
        .unwrap();

        assert_eq!(config.stt.ffmpeg_command, "/opt/homebrew/bin/ffmpeg");
        assert_eq!(config.stt.whisper_command, "/opt/homebrew/bin/whisper-cli");
        assert_eq!(
            config.stt.model_path,
            PathBuf::from("/models/ggml-large-v3-turbo.bin")
        );
        assert_eq!(config.stt.language, "ko");
    }

    #[test]
    fn voice_config_deserializes_tts_settings() {
        let config: VoiceConfig = serde_yaml::from_str(
            r#"
tts:
  backend: edge
  progress_cache_dir: .cache/custom-progress
  edge:
    command: edge-tts
    voice: ko-KR-InJoonNeural
    rate: "-10%"
"#,
        )
        .unwrap();

        assert_eq!(config.tts.backend, VoiceTtsBackendKind::Edge);
        assert_eq!(
            config.tts.progress_cache_dir,
            PathBuf::from(".cache/custom-progress")
        );
        assert_eq!(config.tts.edge.command, "edge-tts");
        assert_eq!(config.tts.edge.voice, "ko-KR-InJoonNeural");
        assert_eq!(config.tts.edge.rate, "-10%");
    }

    #[test]
    fn voice_config_deserializes_barge_in_settings() {
        let config: VoiceConfig = serde_yaml::from_str(
            r#"
barge_in:
  enabled: true
  sensitivity: conservative
  conservative_ttl_secs: 30
  acknowledgement_enabled: false
  acknowledgement_text: 잠시 후 이어서 볼게요.
"#,
        )
        .unwrap();

        assert!(config.barge_in.enabled);
        assert_eq!(
            config.barge_in.sensitivity,
            BargeInSensitivity::Conservative
        );
        assert_eq!(config.barge_in.conservative_ttl_secs, 30);
        assert!(!config.barge_in.acknowledgement_enabled);
        assert_eq!(
            config.barge_in.acknowledgement_text,
            "잠시 후 이어서 볼게요."
        );
    }
}
