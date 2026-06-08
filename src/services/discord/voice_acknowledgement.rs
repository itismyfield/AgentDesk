//! Barge-in acknowledgement settings extracted from `VoiceBargeInRuntime`
//! (#3038 god-object split, acknowledgement slice).
//!
//! Hosts [`AcknowledgementConfig`], the cohesive pair of immutable config
//! values that previously lived as sibling fields (`acknowledgement_enabled`,
//! `acknowledgement_text`) on `VoiceBargeInRuntime`. Moving them into this
//! sibling module both isolates the concern and physically shrinks the
//! `voice_barge_in.rs` giant rather than re-inflating it.

use crate::voice::VoiceConfig;

/// Cohesive sub-concern of `VoiceBargeInRuntime`: the barge-in acknowledgement
/// settings.
///
/// Bundles the two immutable config values that were previously sibling fields
/// on `VoiceBargeInRuntime` and were only ever consumed together when draining a
/// deferred barge-in buffer:
/// - `enabled` mirrors `config.barge_in.acknowledgement_enabled`, and
/// - `text` mirrors `config.barge_in.acknowledgement_text`.
///
/// Both are seeded once at construction (from `VoiceConfig`, or to the disabled
/// defaults `false` / empty string) and never mutated thereafter, so no locking
/// or ordering is involved. The single read site
/// (`acknowledgement_before_drain`) consumes `enabled()` and `text()` as a pair,
/// exactly as the original
/// `buffer.acknowledgement_before_drain(self.acknowledgement_enabled,
/// &self.acknowledgement_text)` call did. Behavior is unchanged.
pub(in crate::services::discord) struct AcknowledgementConfig {
    enabled: bool,
    text: String,
}

impl AcknowledgementConfig {
    /// Build from a `VoiceConfig`, mirroring the original
    /// `config.barge_in.acknowledgement_*` reads.
    pub(in crate::services::discord) fn from_voice_config(config: &VoiceConfig) -> Self {
        Self {
            enabled: config.barge_in.acknowledgement_enabled,
            text: config.barge_in.acknowledgement_text.clone(),
        }
    }

    /// Disabled defaults, matching the original `false` / `String::new()` pair
    /// used by `VoiceBargeInRuntime::disabled`.
    pub(in crate::services::discord) fn disabled() -> Self {
        Self {
            enabled: false,
            text: String::new(),
        }
    }

    /// Whether the spoken acknowledgement is enabled.
    pub(in crate::services::discord) fn enabled(&self) -> bool {
        self.enabled
    }

    /// The configured acknowledgement text.
    pub(in crate::services::discord) fn text(&self) -> &str {
        &self.text
    }
}
