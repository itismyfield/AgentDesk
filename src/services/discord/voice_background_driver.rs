use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, UserId};

use crate::services::provider::ProviderKind;

use super::SharedData;

/// Boundary between voice foreground interaction and long-running provider work.
///
/// Voice foreground owns STT transcript intake, short acknowledgements, TTS,
/// barge-in, cancel/resume commands, progress mirroring, and Discord chat logs.
/// A background driver owns the provider-specific long-running turn boundary.
/// This initial interface routes `start` through the driver. The capability
/// flags below make the rest of the contract explicit while cancel/resume,
/// progress observation, and terminal result delivery continue to flow through
/// the existing mailbox and turn_bridge paths.
///
/// The current production driver is the existing headless Discord turn path.
/// Claude's TUI-hosted pseudo-headless path is represented as a first-class
/// candidate so it can share the same request/stream/cancel contract instead of
/// growing a second voice-specific integration surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum VoiceBackgroundDriverKind {
    Headless,
    ClaudeTuiPseudoHeadless,
}

impl VoiceBackgroundDriverKind {
    pub(in crate::services::discord) const fn as_str(self) -> &'static str {
        match self {
            Self::Headless => "headless",
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
    const HEADLESS: Self = Self {
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
    pub ctx: &'a serenity::Context,
    pub channel_id: ChannelId,
    pub prompt: &'a str,
    pub request_owner_name: &'a str,
    pub request_owner: UserId,
    pub shared: &'a Arc<SharedData>,
    pub token: &'a str,
    pub metadata: Option<serde_json::Value>,
    pub channel_name_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) struct VoiceBackgroundStartOutcome {
    pub turn_id: String,
    pub driver_kind: VoiceBackgroundDriverKind,
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
pub(in crate::services::discord) struct HeadlessVoiceBackgroundDriver;

impl VoiceBackgroundTurnDriver for HeadlessVoiceBackgroundDriver {
    fn kind(&self) -> VoiceBackgroundDriverKind {
        VoiceBackgroundDriverKind::Headless
    }

    fn capabilities(&self) -> VoiceBackgroundDriverCapabilities {
        VoiceBackgroundDriverCapabilities::HEADLESS
    }

    fn start<'a>(
        &'a self,
        request: VoiceBackgroundStartRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<VoiceBackgroundStartOutcome, String>> + Send + 'a>>
    {
        Box::pin(async move {
            let outcome = super::router::start_voice_headless_turn(
                request.ctx,
                request.channel_id,
                request.prompt,
                request.request_owner_name,
                request.request_owner,
                request.shared,
                request.token,
                request.metadata,
                request.channel_name_hint,
            )
            .await
            .map_err(|error| error.to_string())?;
            Ok(VoiceBackgroundStartOutcome {
                turn_id: outcome.turn_id,
                driver_kind: self.kind(),
            })
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
    Headless(HeadlessVoiceBackgroundDriver),
    ClaudeTuiPseudoHeadless(ClaudeTuiPseudoHeadlessDriver),
}

impl VoiceBackgroundTurnDriver for VoiceBackgroundDriver {
    fn kind(&self) -> VoiceBackgroundDriverKind {
        match self {
            Self::Headless(driver) => driver.kind(),
            Self::ClaudeTuiPseudoHeadless(driver) => driver.kind(),
        }
    }

    fn capabilities(&self) -> VoiceBackgroundDriverCapabilities {
        match self {
            Self::Headless(driver) => driver.capabilities(),
            Self::ClaudeTuiPseudoHeadless(driver) => driver.capabilities(),
        }
    }

    fn start<'a>(
        &'a self,
        request: VoiceBackgroundStartRequest<'a>,
    ) -> Pin<Box<dyn Future<Output = Result<VoiceBackgroundStartOutcome, String>> + Send + 'a>>
    {
        match self {
            Self::Headless(driver) => driver.start(request),
            Self::ClaudeTuiPseudoHeadless(driver) => driver.start(request),
        }
    }
}

pub(in crate::services::discord) fn select_voice_background_driver(
    _provider: &ProviderKind,
) -> VoiceBackgroundDriver {
    VoiceBackgroundDriver::Headless(HeadlessVoiceBackgroundDriver)
}

pub(in crate::services::discord) fn candidate_driver_kinds_for_provider(
    provider: &ProviderKind,
) -> Vec<VoiceBackgroundDriverKind> {
    match provider {
        ProviderKind::Claude => vec![
            VoiceBackgroundDriverKind::Headless,
            VoiceBackgroundDriverKind::ClaudeTuiPseudoHeadless,
        ],
        _ => vec![VoiceBackgroundDriverKind::Headless],
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClaudeTuiPseudoHeadlessDriver, VoiceBackgroundDriverKind, VoiceBackgroundTurnDriver,
        candidate_driver_kinds_for_provider, select_voice_background_driver,
    };
    use crate::services::provider::ProviderKind;

    #[test]
    fn claude_voice_background_candidates_keep_dual_path_visible() {
        assert_eq!(
            candidate_driver_kinds_for_provider(&ProviderKind::Claude),
            vec![
                VoiceBackgroundDriverKind::Headless,
                VoiceBackgroundDriverKind::ClaudeTuiPseudoHeadless,
            ]
        );
        assert_eq!(
            select_voice_background_driver(&ProviderKind::Claude).kind(),
            VoiceBackgroundDriverKind::Headless
        );
        assert!(
            select_voice_background_driver(&ProviderKind::Claude)
                .capabilities()
                .start
        );
        assert!(!ClaudeTuiPseudoHeadlessDriver.capabilities().start);
    }

    #[test]
    fn non_claude_providers_only_advertise_headless_candidate() {
        assert_eq!(
            candidate_driver_kinds_for_provider(&ProviderKind::Codex),
            vec![VoiceBackgroundDriverKind::Headless]
        );
    }
}
