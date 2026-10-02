//! Serializable voice control commands and the voice config snapshot.

use serde::{Deserialize, Serialize};

use crate::voice::barge_in::BargeInSensitivity;
use crate::voice::commands::{VoiceCommand, WakeWordCommand};
use crate::voice::config::VoiceConfig;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum VoiceRuntimeControlCommand {
    Sensitivity {
        sensitivity: BargeInSensitivity,
    },
    VerboseProgress {
        enabled: bool,
    },
    Language {
        language: String,
    },
    TtsVoice {
        voice: String,
    },
    VoiceClone {
        reference: Option<String>,
    },
    WakeWords {
        command: VoiceRuntimeWakeWordCommand,
    },
}

impl From<VoiceCommand> for VoiceRuntimeControlCommand {
    fn from(command: VoiceCommand) -> Self {
        match command {
            VoiceCommand::Sensitivity(sensitivity) => Self::Sensitivity { sensitivity },
            VoiceCommand::VerboseProgress(enabled) => Self::VerboseProgress { enabled },
            VoiceCommand::Language(language) => Self::Language { language },
            VoiceCommand::TtsVoice(voice) => Self::TtsVoice { voice },
            VoiceCommand::VoiceClone { reference } => Self::VoiceClone { reference },
            VoiceCommand::WakeWords(command) => Self::WakeWords {
                command: command.into(),
            },
        }
    }
}

impl From<VoiceRuntimeControlCommand> for VoiceCommand {
    fn from(command: VoiceRuntimeControlCommand) -> Self {
        match command {
            VoiceRuntimeControlCommand::Sensitivity { sensitivity } => {
                Self::Sensitivity(sensitivity)
            }
            VoiceRuntimeControlCommand::VerboseProgress { enabled } => {
                Self::VerboseProgress(enabled)
            }
            VoiceRuntimeControlCommand::Language { language } => Self::Language(language),
            VoiceRuntimeControlCommand::TtsVoice { voice } => Self::TtsVoice(voice),
            VoiceRuntimeControlCommand::VoiceClone { reference } => Self::VoiceClone { reference },
            VoiceRuntimeControlCommand::WakeWords { command } => Self::WakeWords(command.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum VoiceRuntimeWakeWordCommand {
    EnableDefault,
    Disable,
    Set { wake_words: Vec<String> },
}

impl From<WakeWordCommand> for VoiceRuntimeWakeWordCommand {
    fn from(command: WakeWordCommand) -> Self {
        match command {
            WakeWordCommand::EnableDefault => Self::EnableDefault,
            WakeWordCommand::Disable => Self::Disable,
            WakeWordCommand::Set(wake_words) => Self::Set { wake_words },
        }
    }
}

impl From<VoiceRuntimeWakeWordCommand> for WakeWordCommand {
    fn from(command: VoiceRuntimeWakeWordCommand) -> Self {
        match command {
            VoiceRuntimeWakeWordCommand::EnableDefault => Self::EnableDefault,
            VoiceRuntimeWakeWordCommand::Disable => Self::Disable,
            VoiceRuntimeWakeWordCommand::Set { wake_words } => Self::Set(wake_words),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct VoiceRuntimeConfigSnapshot {
    pub enabled: bool,
    pub verbose_progress: bool,
    pub barge_in_enabled: bool,
    pub default_sensitivity_mode: BargeInSensitivity,
    pub stt_language: String,
    pub tts_voice: String,
    pub wake_words: Vec<String>,
    pub wake_word_required: bool,
    pub lobby_channel_id: Option<String>,
    pub auto_join_channel_ids: Vec<String>,
    pub keep_recordings: bool,
}

impl From<&VoiceConfig> for VoiceRuntimeConfigSnapshot {
    fn from(config: &VoiceConfig) -> Self {
        Self {
            enabled: config.enabled,
            verbose_progress: config.verbose_progress,
            barge_in_enabled: config.enabled && config.barge_in.enabled,
            default_sensitivity_mode: config.default_sensitivity_mode,
            stt_language: config.stt.language.clone(),
            tts_voice: config.tts.edge.voice.clone(),
            wake_words: config.wake_words.clone(),
            wake_word_required: config.wake_word_required(),
            lobby_channel_id: config.lobby_channel_id.clone(),
            auto_join_channel_ids: config.auto_join_channel_ids_with_lobby(),
            keep_recordings: config.keep_voice_recordings(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::config::DEFAULT_EDGE_TTS_VOICE;

    #[test]
    fn config_snapshot_is_minimal_and_serializable() {
        let mut config = VoiceConfig {
            enabled: true,
            verbose_progress: true,
            lobby_channel_id: Some("100".to_string()),
            auto_join_channel_ids: vec!["200".to_string(), "100".to_string()],
            ..VoiceConfig::default()
        };
        config.stt.language = "en".to_string();

        let snapshot = VoiceRuntimeConfigSnapshot::from(&config);

        assert!(snapshot.enabled);
        assert!(snapshot.barge_in_enabled);
        assert_eq!(snapshot.stt_language, "en");
        assert_eq!(snapshot.tts_voice, DEFAULT_EDGE_TTS_VOICE);
        assert_eq!(snapshot.auto_join_channel_ids, vec!["100", "200"]);
        serde_json::to_string(&snapshot).unwrap();
    }

    #[test]
    fn control_command_round_trips_existing_voice_command() {
        let command = VoiceCommand::WakeWords(WakeWordCommand::Set(vec![
            "agentdesk".to_string(),
            "deskbot".to_string(),
        ]));

        let boundary = VoiceRuntimeControlCommand::from(command.clone());
        let restored = VoiceCommand::from(boundary);

        assert_eq!(restored, command);
    }
}
