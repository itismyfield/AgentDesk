//! Discord delivery for one input channel, including reactions on its original messages.

use std::sync::Arc;
use std::time::Duration;

use poise::serenity_prelude::{ChannelId, Http, MessageId, ReactionType};

use crate::services::tui_o::writer::input_facts::reactions::ReactionPort;

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
        matches!(
            tokio::time::timeout(
                NOTICE_TIMEOUT,
                ChannelId::new(self.channel).say(&*self.http, text),
            )
            .await,
            Ok(Ok(_))
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
        let channel = ChannelId::new(channel);
        let message = MessageId::new(message);
        let reaction = ReactionType::Unicode(emoji.to_string());
        let send = async {
            if present {
                channel.create_reaction(&self.http, message, reaction).await
            } else {
                channel
                    .delete_reaction(&self.http, message, None, reaction)
                    .await
            }
        };
        tokio::time::timeout(NOTICE_TIMEOUT, send)
            .await
            .map_err(|_| "input reaction timed out".to_owned())?
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
#[path = "notice_tests.rs"]
mod tests;
