use std::{future::Future, pin::Pin, sync::Arc};

use crate::services::provider::ProviderKind;
use poise::serenity_prelude::{ChannelId, MessageId};

use super::SharedData;

/// Boundary between voice foreground interaction and long-running provider work.
///
/// Voice foreground owns STT transcript intake, short acknowledgements, TTS,
/// barge-in, cancel/resume commands, progress mirroring, and Discord chat logs.
/// A background driver owns the long-running turn trigger boundary. Voice
/// must not call provider/headless execution directly: the canonical trigger is
/// an announce-bot transcript message in the routed text channel, which then
/// flows through the normal Discord intake and turn bridge.
///
/// Legacy headless and Claude TUI pseudo-headless are kept visible as candidate
/// shapes so dual-path provider work can converge here later, but production
/// voice start currently selects `AnnounceBotTranscript`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum VoiceBackgroundDriverKind {
    AnnounceBotTranscript,
    HeadlessLegacy,
    ClaudeTuiPseudoHeadless,
}

impl VoiceBackgroundDriverKind {
    pub(in crate::services::discord) const fn as_str(self) -> &'static str {
        match self {
            Self::AnnounceBotTranscript => "announce_bot_transcript",
            Self::HeadlessLegacy => "headless_legacy",
            Self::ClaudeTuiPseudoHeadless => "claude_tui_pseudo_headless",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) struct VoiceBackgroundDriverCapabilities {
    pub start: bool,
    pub follow_up: bool,
    pub cancel: bool,
    pub resume: bool,
    pub progress_observation: bool,
    pub terminal_result_delivery: bool,
}

impl VoiceBackgroundDriverCapabilities {
    const ANNOUNCE_BOT_TRANSCRIPT: Self = Self {
        start: true,
        follow_up: true,
        cancel: true,
        resume: true,
        progress_observation: true,
        terminal_result_delivery: true,
    };

    const CANDIDATE_NOT_ENABLED: Self = Self {
        start: false,
        follow_up: false,
        cancel: false,
        resume: false,
        progress_observation: false,
        terminal_result_delivery: false,
    };
}

pub(in crate::services::discord) struct VoiceBackgroundStartRequest<'a> {
    pub channel_id: ChannelId,
    pub shared: &'a Arc<SharedData>,
    pub message_content: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct VoiceBackgroundStartOutcome {
    pub turn_id: String,
    pub driver_kind: VoiceBackgroundDriverKind,
    pub message_id: Option<MessageId>,
}

pub(in crate::services::discord) trait VoiceBackgroundTurnDriver {
    fn kind(&self) -> VoiceBackgroundDriverKind;
    fn capabilities(&self) -> VoiceBackgroundDriverCapabilities;

    fn start<'a>(
        &'a self,
        request: VoiceBackgroundStartRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<VoiceBackgroundStartOutcome, String>> + Send + 'a>>;
}

#[derive(Debug, Clone, Copy)]
pub(in crate::services::discord) struct AnnounceBotTranscriptDriver;

impl VoiceBackgroundTurnDriver for AnnounceBotTranscriptDriver {
    fn kind(&self) -> VoiceBackgroundDriverKind {
        VoiceBackgroundDriverKind::AnnounceBotTranscript
    }

    fn capabilities(&self) -> VoiceBackgroundDriverCapabilities {
        VoiceBackgroundDriverCapabilities::ANNOUNCE_BOT_TRANSCRIPT
    }

    fn start<'a>(
        &'a self,
        request: VoiceBackgroundStartRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<VoiceBackgroundStartOutcome, String>> + Send + 'a>>
    {
        Box::pin(async move {
            let registry = request
                .shared
                .health_registry()
                .ok_or_else(|| "health registry unavailable".to_string())?;
            let target = format!("channel:{}", request.channel_id.get());
            let (status, body) = super::health::send_message_with_backends(
                &registry,
                None::<&crate::db::Db>,
                request.shared.pg_pool.as_ref(),
                &target,
                request.message_content,
                "voice",
                "announce",
                Some("voice transcript"),
            )
            .await;
            if !status.starts_with("200") {
                return Err(format!("announce send returned {status}: {body}"));
            }
            let value = serde_json::from_str::<serde_json::Value>(&body)
                .map_err(|error| format!("parse announce send response: {error}"))?;
            let message_id = value
                .get("message_id")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .and_then(|value| value.parse::<u64>().ok())
                .map(MessageId::new)
                .ok_or_else(|| "announce send response missing message_id".to_string())?;
            Ok(VoiceBackgroundStartOutcome {
                turn_id: format!("voice-announce:{}", message_id.get()),
                driver_kind: self.kind(),
                message_id: Some(message_id),
            })
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub(in crate::services::discord) struct HeadlessLegacyVoiceBackgroundDriver;

impl VoiceBackgroundTurnDriver for HeadlessLegacyVoiceBackgroundDriver {
    fn kind(&self) -> VoiceBackgroundDriverKind {
        VoiceBackgroundDriverKind::HeadlessLegacy
    }

    fn capabilities(&self) -> VoiceBackgroundDriverCapabilities {
        VoiceBackgroundDriverCapabilities::CANDIDATE_NOT_ENABLED
    }

    fn start<'a>(
        &'a self,
        _request: VoiceBackgroundStartRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<VoiceBackgroundStartOutcome, String>> + Send + 'a>>
    {
        Box::pin(async move {
            Err(
                "legacy direct headless voice trigger is disabled; use announce-bot transcript"
                    .to_string(),
            )
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub(in crate::services::discord) struct ClaudeTuiPseudoHeadlessDriver;

impl VoiceBackgroundTurnDriver for ClaudeTuiPseudoHeadlessDriver {
    fn kind(&self) -> VoiceBackgroundDriverKind {
        VoiceBackgroundDriverKind::ClaudeTuiPseudoHeadless
    }

    fn capabilities(&self) -> VoiceBackgroundDriverCapabilities {
        VoiceBackgroundDriverCapabilities::CANDIDATE_NOT_ENABLED
    }

    fn start<'a>(
        &'a self,
        _request: VoiceBackgroundStartRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<VoiceBackgroundStartOutcome, String>> + Send + 'a>>
    {
        Box::pin(async move {
            Err(
                "claude_tui_pseudo_headless driver is not enabled yet; use headless fallback"
                    .to_string(),
            )
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub(in crate::services::discord) enum VoiceBackgroundDriver {
    AnnounceBotTranscript(AnnounceBotTranscriptDriver),
    HeadlessLegacy(HeadlessLegacyVoiceBackgroundDriver),
    ClaudeTuiPseudoHeadless(ClaudeTuiPseudoHeadlessDriver),
}

impl VoiceBackgroundTurnDriver for VoiceBackgroundDriver {
    fn kind(&self) -> VoiceBackgroundDriverKind {
        match self {
            Self::AnnounceBotTranscript(driver) => driver.kind(),
            Self::HeadlessLegacy(driver) => driver.kind(),
            Self::ClaudeTuiPseudoHeadless(driver) => driver.kind(),
        }
    }

    fn capabilities(&self) -> VoiceBackgroundDriverCapabilities {
        match self {
            Self::AnnounceBotTranscript(driver) => driver.capabilities(),
            Self::HeadlessLegacy(driver) => driver.capabilities(),
            Self::ClaudeTuiPseudoHeadless(driver) => driver.capabilities(),
        }
    }

    fn start<'a>(
        &'a self,
        request: VoiceBackgroundStartRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<VoiceBackgroundStartOutcome, String>> + Send + 'a>>
    {
        match self {
            Self::AnnounceBotTranscript(driver) => driver.start(request),
            Self::HeadlessLegacy(driver) => driver.start(request),
            Self::ClaudeTuiPseudoHeadless(driver) => driver.start(request),
        }
    }
}

pub(in crate::services::discord) fn select_voice_background_driver(
    _provider: &ProviderKind,
) -> VoiceBackgroundDriver {
    VoiceBackgroundDriver::AnnounceBotTranscript(AnnounceBotTranscriptDriver)
}

pub(in crate::services::discord) fn candidate_driver_kinds_for_provider(
    provider: &ProviderKind,
) -> Vec<VoiceBackgroundDriverKind> {
    match provider {
        ProviderKind::Claude => vec![
            VoiceBackgroundDriverKind::AnnounceBotTranscript,
            VoiceBackgroundDriverKind::HeadlessLegacy,
            VoiceBackgroundDriverKind::ClaudeTuiPseudoHeadless,
        ],
        _ => vec![VoiceBackgroundDriverKind::AnnounceBotTranscript],
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClaudeTuiPseudoHeadlessDriver, HeadlessLegacyVoiceBackgroundDriver,
        VoiceBackgroundDriverKind, VoiceBackgroundTurnDriver, candidate_driver_kinds_for_provider,
        select_voice_background_driver,
    };
    use crate::services::provider::ProviderKind;

    #[test]
    fn claude_voice_background_candidates_keep_dual_path_visible() {
        assert_eq!(
            candidate_driver_kinds_for_provider(&ProviderKind::Claude),
            vec![
                VoiceBackgroundDriverKind::AnnounceBotTranscript,
                VoiceBackgroundDriverKind::HeadlessLegacy,
                VoiceBackgroundDriverKind::ClaudeTuiPseudoHeadless,
            ]
        );
        assert_eq!(
            select_voice_background_driver(&ProviderKind::Claude).kind(),
            VoiceBackgroundDriverKind::AnnounceBotTranscript
        );
        assert!(
            select_voice_background_driver(&ProviderKind::Claude)
                .capabilities()
                .start
        );
        assert!(!ClaudeTuiPseudoHeadlessDriver.capabilities().start);
        assert!(!HeadlessLegacyVoiceBackgroundDriver.capabilities().start);
    }

    #[test]
    fn non_claude_providers_only_advertise_announce_candidate() {
        assert_eq!(
            candidate_driver_kinds_for_provider(&ProviderKind::Codex),
            vec![VoiceBackgroundDriverKind::AnnounceBotTranscript]
        );
    }
}
