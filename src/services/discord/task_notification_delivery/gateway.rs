//! Task-card transport over the canonical Discord outbound v3 path (#4055).

use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

#[derive(Clone)]
pub(in crate::services::discord) struct CardBot {
    pub(in crate::services::discord) key: String,
    pub(in crate::services::discord) http: Arc<serenity::Http>,
}

impl CardBot {
    pub(in crate::services::discord) fn new(
        key: impl Into<String>,
        http: Arc<serenity::Http>,
    ) -> Self {
        Self {
            key: key.into(),
            http,
        }
    }
}

#[derive(Clone, Default)]
pub(in crate::services::discord) struct CardDeliveryClients {
    bots: Vec<CardBot>,
}

impl CardDeliveryClients {
    pub(in crate::services::discord) fn new(bots: impl IntoIterator<Item = CardBot>) -> Self {
        let mut unique = Vec::<CardBot>::new();
        for bot in bots {
            if !unique.iter().any(|existing| existing.key == bot.key) {
                unique.push(bot);
            }
        }
        Self { bots: unique }
    }

    pub(in crate::services::discord) fn preferred(&self) -> Option<&CardBot> {
        self.bots.first()
    }

    pub(in crate::services::discord) fn by_key(&self, key: &str) -> Option<&CardBot> {
        self.bots.iter().find(|bot| bot.key == key)
    }
}

#[derive(Debug, thiserror::Error)]
pub(in crate::services::discord) enum TaskCardTransportError {
    #[error("{0}")]
    Transient(String),
    #[error("{0}")]
    ConfirmedMissing(String),
    #[error("{0}")]
    Permanent(String),
}

#[allow(async_fn_in_trait)]
pub(in crate::services::discord) trait TaskCardTransport:
    Send + Sync
{
    async fn post_card(
        &self,
        bot: &CardBot,
        channel_id: u64,
        content: &str,
        nonce: &str,
    ) -> Result<u64, TaskCardTransportError>;

    async fn edit_card(
        &self,
        bot: &CardBot,
        channel_id: u64,
        message_id: u64,
        content: &str,
    ) -> Result<(), TaskCardTransportError>;
}

#[derive(Clone)]
pub(in crate::services::discord) struct DiscordTaskCardTransport {
    shared: Arc<super::super::SharedData>,
}

impl DiscordTaskCardTransport {
    pub(in crate::services::discord) fn new(shared: Arc<super::super::SharedData>) -> Self {
        Self { shared }
    }
}

fn map_card_post_error(
    error: super::super::gateway::ClassifiedOutboundPostError,
) -> TaskCardTransportError {
    match error {
        super::super::gateway::ClassifiedOutboundPostError::Transient(error) => {
            TaskCardTransportError::Transient(error)
        }
        super::super::gateway::ClassifiedOutboundPostError::Permanent(error) => {
            TaskCardTransportError::Permanent(error)
        }
    }
}

impl TaskCardTransport for DiscordTaskCardTransport {
    async fn post_card(
        &self,
        bot: &CardBot,
        channel_id: u64,
        content: &str,
        nonce: &str,
    ) -> Result<u64, TaskCardTransportError> {
        super::super::gateway::send_outbound_message_with_nonce_classified(
            bot.http.clone(),
            self.shared.clone(),
            ChannelId::new(channel_id),
            content,
            nonce,
        )
        .await
        .map(|message_id| message_id.get())
        .map_err(map_card_post_error)
    }

    async fn edit_card(
        &self,
        bot: &CardBot,
        channel_id: u64,
        message_id: u64,
        content: &str,
    ) -> Result<(), TaskCardTransportError> {
        super::super::gateway::edit_outbound_message_classified(
            bot.http.clone(),
            self.shared.clone(),
            ChannelId::new(channel_id),
            MessageId::new(message_id),
            content,
        )
        .await
        .map_err(|error| match error {
            super::super::gateway::ClassifiedOutboundEditError::ConfirmedMissing(error) => {
                TaskCardTransportError::ConfirmedMissing(error)
            }
            super::super::gateway::ClassifiedOutboundEditError::Other(error) => {
                TaskCardTransportError::Transient(error)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authoritative_card_post_4xx_maps_to_permanent_transport_failure() {
        let error = map_card_post_error(
            super::super::super::gateway::ClassifiedOutboundPostError::Permanent(
                "Discord rejected task card POST with 403".to_string(),
            ),
        );
        assert!(matches!(error, TaskCardTransportError::Permanent(_)));
    }
}
