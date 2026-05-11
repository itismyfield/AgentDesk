use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub(crate) const DEFAULT_PROGRESS_TTS_CACHE_DIR: &str = ".cache/voice-tts-progress";
pub(crate) const DEFAULT_EDGE_TTS_COMMAND: &str = "edge-tts";
pub(crate) const DEFAULT_EDGE_TTS_VOICE: &str = "ko-KR-SunHiNeural";
pub(crate) const DEFAULT_EDGE_TTS_RATE: &str = "+0%";

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub(crate) struct VoiceConfig {
    pub enabled: bool,
    pub audio: VoiceAudioDirs,
    pub tts: VoiceTtsConfig,
    pub thresholds: VoiceDbThresholds,
    pub idle: VoiceIdleTimings,
    pub wake_words: Vec<String>,
    pub allowed_user_ids: Vec<String>,
    pub auto_join_channel_ids: Vec<String>,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            audio: VoiceAudioDirs::default(),
            tts: VoiceTtsConfig::default(),
            thresholds: VoiceDbThresholds::default(),
            idle: VoiceIdleTimings::default(),
            wake_words: vec!["agentdesk".to_string()],
            allowed_user_ids: Vec::new(),
            auto_join_channel_ids: Vec::new(),
        }
    }
}

impl VoiceConfig {
    pub(crate) fn is_default(&self) -> bool {
        self == &Self::default()
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
        assert!(config.allowed_user_ids.is_empty());
        assert!(config.auto_join_channel_ids.is_empty());
        assert_eq!(config.wake_words, vec!["agentdesk"]);
        assert_eq!(config.tts.backend, VoiceTtsBackendKind::Edge);
        assert_eq!(
            config.tts.progress_cache_dir,
            PathBuf::from(DEFAULT_PROGRESS_TTS_CACHE_DIR)
        );
        assert_eq!(config.tts.edge.voice, DEFAULT_EDGE_TTS_VOICE);
    }

    #[test]
    fn voice_config_deserializes_partial_yaml_with_defaults() {
        let config: VoiceConfig = serde_yaml::from_str(
            r#"
enabled: true
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
auto_join_channel_ids:
  - "1500000000000000000"
"#,
        )
        .unwrap();

        assert!(config.enabled);
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
        assert_eq!(config.tts.backend, VoiceTtsBackendKind::Edge);
        assert_eq!(config.tts.edge.command, DEFAULT_EDGE_TTS_COMMAND);
        assert_eq!(config.tts.edge.rate, DEFAULT_EDGE_TTS_RATE);
        assert_eq!(config.idle.segment_idle_ms, 2_000);
        assert_eq!(config.idle.channel_idle_disconnect_secs, 120);
        assert_eq!(config.idle.utterance_idle_ms, 4_500);
        assert_eq!(config.wake_words, vec!["desk"]);
        assert_eq!(config.allowed_user_ids, vec!["343742347365974026"]);
        assert_eq!(config.auto_join_channel_ids, vec!["1500000000000000000"]);
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
}
