//! Discord and mailbox I/O performed by one catch-up sweep. Sweep tests swap
//! the whole trait to script fetch pages and enqueue outcomes.

use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{ClaimObservation, Intervention};

use super::super::{
    MailboxEnqueueOutcome, RuntimeChannelBindingStatus, SharedData, bot_role, health, queue_io,
    reaction_cleanup, resolve_runtime_channel_binding_status,
};
use super::too_old_notice::{self, CatchUpTooOldOutboxRequest};

/// Page request as the sweep builds it; the adapter turns it into the REST
/// builder, which has no public accessors for tests to read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CatchUpFetchRequest {
    pub(super) limit: u8,
    pub(super) cursor: Option<CatchUpFetchCursor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CatchUpFetchCursor {
    After(u64),
    Before(u64),
}

impl CatchUpFetchRequest {
    pub(super) fn new(limit: u8) -> Self {
        Self {
            limit,
            cursor: None,
        }
    }

    pub(super) fn after(self, message_id: u64) -> Self {
        Self {
            cursor: Some(CatchUpFetchCursor::After(message_id)),
            ..self
        }
    }

    pub(super) fn before(self, message_id: u64) -> Self {
        Self {
            cursor: Some(CatchUpFetchCursor::Before(message_id)),
            ..self
        }
    }

    fn into_get_messages(self) -> serenity::builder::GetMessages {
        let request = serenity::builder::GetMessages::new().limit(self.limit);
        match self.cursor {
            Some(CatchUpFetchCursor::After(id)) => request.after(MessageId::new(id)),
            Some(CatchUpFetchCursor::Before(id)) => request.before(MessageId::new(id)),
            None => request,
        }
    }
}

#[async_trait::async_trait]
pub(super) trait CatchUpDiscordApi: Sync {
    async fn current_user_id(&self) -> Result<Option<u64>, String>;

    async fn resolve_runtime_channel_binding_status(
        &self,
        channel_id: ChannelId,
    ) -> RuntimeChannelBindingStatus;

    async fn fetch_messages(
        &self,
        channel_id: ChannelId,
        request: CatchUpFetchRequest,
    ) -> Result<Vec<serenity::Message>, String>;

    async fn cleanup_recovered_catch_up_hourglass(
        &self,
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
        message_id: MessageId,
    );

    /// Both phases commit recoveries through this one call.
    async fn enqueue_intervention(
        &self,
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel_id: ChannelId,
        intervention: Intervention,
        observed: ClaimObservation,
    ) -> MailboxEnqueueOutcome {
        let observed = Some(observed);
        queue_io::mailbox_enqueue_observed_intervention(
            shared,
            provider,
            channel_id,
            intervention,
            observed,
        )
        .await
    }

    fn enqueue_too_old_notice(
        &self,
        pool: Option<sqlx::PgPool>,
        request: CatchUpTooOldOutboxRequest,
    ) -> Option<tokio::task::JoinHandle<()>> {
        pool.map(|pool| too_old_notice::spawn_outbox(pool, request))
    }

    fn record_too_old_dead_letter(
        &self,
        pool: Option<&sqlx::PgPool>,
        record: crate::db::relay_dead_letter::RelayDeadLetterRecord,
    ) -> Option<tokio::task::JoinHandle<()>> {
        crate::db::relay_dead_letter::record_detached(pool, record)
    }

    async fn utility_bot_user_ids(
        &self,
        shared: &SharedData,
    ) -> (
        health::UtilityBotUserIdResolution,
        health::UtilityBotUserIdResolution,
    ) {
        let Some(registry) = shared.health_registry() else {
            return (
                health::UtilityBotUserIdResolution::Unconfigured,
                health::UtilityBotUserIdResolution::Unconfigured,
            );
        };
        (
            registry
                .utility_bot_user_id_resolution(bot_role::UtilityBotRole::Announce)
                .await,
            registry
                .utility_bot_user_id_resolution(bot_role::UtilityBotRole::Notify)
                .await,
        )
    }
}

pub(super) struct SerenityCatchUpDiscordApi<'a> {
    pub(super) http: &'a Arc<serenity::Http>,
}

#[async_trait::async_trait]
impl CatchUpDiscordApi for SerenityCatchUpDiscordApi<'_> {
    async fn current_user_id(&self) -> Result<Option<u64>, String> {
        self.http
            .get_current_user()
            .await
            .map(|user| Some(user.id.get()))
            .map_err(|err| err.to_string())
    }

    async fn resolve_runtime_channel_binding_status(
        &self,
        channel_id: ChannelId,
    ) -> RuntimeChannelBindingStatus {
        resolve_runtime_channel_binding_status(self.http, channel_id).await
    }

    async fn fetch_messages(
        &self,
        channel_id: ChannelId,
        request: CatchUpFetchRequest,
    ) -> Result<Vec<serenity::Message>, String> {
        channel_id
            .messages(self.http, request.into_get_messages())
            .await
            .map_err(|err| err.to_string())
    }

    async fn cleanup_recovered_catch_up_hourglass(
        &self,
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
        message_id: MessageId,
    ) {
        reaction_cleanup::cleanup_recovered_catch_up_hourglass(
            self.http, shared, channel_id, message_id,
        )
        .await;
    }
}
