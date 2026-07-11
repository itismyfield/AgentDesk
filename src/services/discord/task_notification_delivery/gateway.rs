//! Task-card transport over the canonical Discord outbound v3 path (#4055).

use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use super::response_chunks::{
    DiscordResponseChunkTransport, ResponseChunkHistoryError, ResponseChunkTransport,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum CardPostReconcile {
    Found(u64),
    Ambiguous(String),
    Transient(String),
    Permanent(String),
}

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

    async fn reconcile_card_post(
        &self,
        _bot: &CardBot,
        _channel_id: u64,
        _nonce: &str,
        _content_hash: &str,
        _post_started_at: chrono::DateTime<chrono::Utc>,
    ) -> CardPostReconcile {
        CardPostReconcile::Ambiguous(
            "task-card transport cannot authoritatively reconcile an old POST".to_string(),
        )
    }
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

    async fn reconcile_card_post(
        &self,
        bot: &CardBot,
        channel_id: u64,
        nonce: &str,
        expected_content_hash: &str,
        post_started_at: chrono::DateTime<chrono::Utc>,
    ) -> CardPostReconcile {
        const PAGE_SIZE: usize = 100;
        const MAX_PAGES: usize = 10;
        let history = DiscordResponseChunkTransport::new(bot.http.as_ref(), &self.shared);
        let bot_user_id = match history.bot_user_id().await {
            Ok(bot_user_id) => bot_user_id,
            Err(error) => return CardPostReconcile::Transient(error),
        };
        let mut before = None;
        let mut previous_oldest = None;
        for page_index in 0..MAX_PAGES {
            let page = match history.history_page(channel_id, before, PAGE_SIZE).await {
                Ok(page) => page,
                Err(ResponseChunkHistoryError::Transient(error)) => {
                    return CardPostReconcile::Transient(error);
                }
                Err(ResponseChunkHistoryError::Permanent(error)) => {
                    return CardPostReconcile::Permanent(error);
                }
            };
            if page.is_empty() {
                return CardPostReconcile::Ambiguous(
                    "empty Discord history cannot disprove task-card POST-then-delete".into(),
                );
            }
            for pair in page.windows(2) {
                if pair[0].created_at < pair[1].created_at
                    || pair[0].message_id <= pair[1].message_id
                {
                    return CardPostReconcile::Ambiguous(
                        "task-card history was not contiguous newest-first".into(),
                    );
                }
            }
            if let Some(previous_id) = previous_oldest
                && page[0].message_id >= previous_id
            {
                return CardPostReconcile::Ambiguous(
                    "task-card history pagination overlapped or went forward".into(),
                );
            }
            for message in &page {
                if message.channel_id != channel_id {
                    return CardPostReconcile::Ambiguous(
                        "task-card history crossed the persisted channel".into(),
                    );
                }
                if message.author_id != bot_user_id {
                    continue;
                }
                if message.nonce.as_deref() == Some(nonce) {
                    return if message.content_hash == expected_content_hash
                        && message.referenced_message_id.is_none()
                    {
                        CardPostReconcile::Found(message.message_id)
                    } else {
                        CardPostReconcile::Ambiguous(
                            "task-card nonce was observed with different content/reference".into(),
                        )
                    };
                }
            }
            let oldest = page.last().expect("nonempty page checked");
            if oldest.created_at <= post_started_at || page.len() < PAGE_SIZE {
                return CardPostReconcile::Ambiguous(
                    "task-card history reached the attempt boundary without deletion proof".into(),
                );
            }
            previous_oldest = Some(oldest.message_id);
            before = Some(oldest.message_id);
            if page_index + 1 == MAX_PAGES {
                return CardPostReconcile::Ambiguous(
                    "task-card history reconciliation hit its page cap".into(),
                );
            }
        }
        CardPostReconcile::Ambiguous(
            "task-card history reconciliation exhausted unexpectedly".into(),
        )
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
