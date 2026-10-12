use super::notice::DiscordNotice;
use crate::services::discord::input_runtime::supervisor::drive::{DrivePorts, ViewKey};
use crate::services::provider::ProviderKind;
use crate::services::tui_input::actor::pane::TmuxPane;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::ShadowProvider;
use poise::serenity_prelude::Http;
use std::sync::Arc;

pub(crate) struct RuntimeDrive {
    provider: ProviderKind,
    session: String,
    ownership: Arc<OwnershipGate>,
    notice: DiscordNotice,
}

impl RuntimeDrive {
    pub(crate) fn new(
        provider: ProviderKind,
        session: String,
        http: Arc<Http>,
        channel: u64,
        ownership: Arc<OwnershipGate>,
    ) -> Self {
        Self {
            provider,
            session,
            ownership,
            notice: DiscordNotice::new(http, channel),
        }
    }
}

impl DrivePorts for RuntimeDrive {
    type Pane = TmuxPane;
    type Reactions = DiscordNotice;

    fn pane(&mut self, key: &ViewKey) -> Option<Self::Pane> {
        let provider = match self.provider {
            ProviderKind::Claude => ShadowProvider::Claude,
            ProviderKind::Codex => ShadowProvider::Codex,
            _ => return None,
        };
        if key.provider != provider
            || key.tmux_session != self.session
            || key.execution_nonce.is_empty()
        {
            return None;
        }
        if crate::services::platform::tmux::session_presence(&self.session)
            != crate::services::platform::tmux::SessionPresence::Present
        {
            return None;
        }
        Some(
            TmuxPane::for_provider(&key.tmux_session, provider)
                .with_ownership(self.ownership.clone()),
        )
    }

    fn reactions(&self) -> &Self::Reactions {
        &self.notice
    }
}

#[cfg(test)]
#[path = "drive_tests.rs"]
mod tests;
