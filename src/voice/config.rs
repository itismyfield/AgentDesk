use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(default)]
pub(crate) struct VoiceConfig {
    pub enabled: bool,
    pub audio: VoiceAudioDirs,
    pub thresholds: VoiceDbThresholds,
    pub idle: VoiceIdleTimings,
    pub wake_words: Vec<String>,
    pub allowed_user_ids: Vec<String>,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            audio: VoiceAudioDirs::default(),
            thresholds: VoiceDbThresholds::default(),
            idle: VoiceIdleTimings::default(),
            wake_words: vec!["agentdesk".to_string()],
            allowed_user_ids: Vec::new(),
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
    pub utterance_idle_ms: u64,
    pub channel_idle_disconnect_secs: u64,
    pub wake_listen_window_secs: u64,
}

impl Default for VoiceIdleTimings {
    fn default() -> Self {
        Self {
            utterance_idle_ms: 1_200,
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
        assert_eq!(config.wake_words, vec!["agentdesk"]);
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
  channel_idle_disconnect_secs: 120
wake_words:
  - desk
allowed_user_ids:
  - "343742347365974026"
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
        assert_eq!(config.idle.channel_idle_disconnect_secs, 120);
        assert_eq!(config.idle.utterance_idle_ms, 1_200);
        assert_eq!(config.wake_words, vec!["desk"]);
        assert_eq!(config.allowed_user_ids, vec!["343742347365974026"]);
    }
}
