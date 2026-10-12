//! Discord delivery for one input channel, including reactions on its original messages.

use std::sync::Arc;
use std::time::Duration;

use poise::serenity_prelude::Http;

use crate::services::discord::outbound::o_writer_io::GatewayPort;
use crate::services::tui_o::writer::input_facts::reactions::ReactionPort;
use crate::services::tui_o::writer::{DiscordPort, PostOutcome};

const NOTICE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(crate) struct DiscordNotice {
    http: Arc<Http>,
    channel: u64,
}

impl DiscordNotice {
    pub(crate) fn new(http: Arc<Http>, channel: u64) -> Self {
        Self { http, channel }
    }

    pub(crate) async fn send(&self, text: String) -> bool {
        if self.channel == 0 {
            return false;
        }
        // Plain posting uses the cached transport without bot identity or history lookup.
        let port = GatewayPort::new(self.http.clone(), 0);
        matches!(
            tokio::time::timeout(NOTICE_TIMEOUT, port.post(self.channel, text)).await,
            Ok(PostOutcome::Created(_))
        )
    }
}

impl ReactionPort for DiscordNotice {
    async fn set(
        &self,
        channel: u64,
        message: u64,
        emoji: char,
        present: bool,
    ) -> Result<(), String> {
        if channel != self.channel || channel == 0 || message == 0 {
            return Err("reaction source does not name this input channel".into());
        }
        tokio::time::timeout(
            NOTICE_TIMEOUT,
            ReactionPort::set(self.http.as_ref(), channel, message, emoji, present),
        )
        .await
        .map_err(|_| "input reaction timed out".to_owned())?
    }
}

#[cfg(test)]
#[path = "notice_tests.rs"]
mod tests;
