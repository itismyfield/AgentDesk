use super::super::gateway::{GatewayFuture, TurnGateway};
use super::*;
use crate::services::discord::{
    mailbox_requeue_intervention_front, schedule_deferred_idle_queue_kickoff,
};
use crate::services::turn_orchestrator::Intervention;

pub(super) struct TuiDirectBridgeGateway {
    pub(super) http: Arc<serenity::Http>,
    pub(super) shared: Arc<SharedData>,
    pub(super) provider: ProviderKind,
}

impl TurnGateway for TuiDirectBridgeGateway {
    fn send_message<'a>(
        &'a self,
        channel_id: ChannelId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        Box::pin(async move {
            super::super::gateway::send_outbound_message(
                self.http.clone(),
                self.shared.clone(),
                channel_id,
                content,
            )
            .await
        })
    }

    fn edit_message<'a>(
        &'a self,
        channel_id: ChannelId,
        message_id: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async move {
            super::super::gateway::edit_outbound_message(
                self.http.clone(),
                self.shared.clone(),
                channel_id,
                message_id,
                content,
            )
            .await
        })
    }

    fn send_long_message_with_rollback<'a>(
        &'a self,
        channel_id: ChannelId,
        rollback_anchor_msg_id: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<Vec<MessageId>, String>> {
        Box::pin(async move {
            super::super::formatting::send_long_message_raw_with_rollback(
                &self.http,
                channel_id,
                rollback_anchor_msg_id,
                content,
                &self.shared,
            )
            .await
            .map_err(|error| {
                super::super::replace_outcome_policy::watcher_send_failure_classified_message(
                    super::super::replace_outcome_policy::classify_watcher_send_failure(
                        error.as_ref(),
                    ),
                    error,
                )
            })
        })
    }

    fn delete_message<'a>(
        &'a self,
        channel_id: ChannelId,
        message_id: MessageId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async move {
            super::super::rate_limit_wait(&self.shared, channel_id).await;
            channel_id
                .delete_message(&self.http, message_id)
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn replace_message_with_outcome<'a>(
        &'a self,
        channel_id: ChannelId,
        message_id: MessageId,
        content: &'a str,
    ) -> GatewayFuture<'a, Result<super::super::formatting::ReplaceLongMessageOutcome, String>>
    {
        Box::pin(async move {
            super::super::formatting::replace_long_message_raw_with_outcome(
                &self.http,
                channel_id,
                message_id,
                content,
                &self.shared,
                // #3805 P1: bridge gateway returns the outcome only; the last-chunk
                // footer anchor is consumed exclusively by the tmux watcher.
                &mut None,
            )
            .await
            .map_err(|error| {
                super::super::replace_outcome_policy::watcher_send_failure_classified_message(
                    super::super::replace_outcome_policy::classify_watcher_send_failure(
                        error.as_ref(),
                    ),
                    error,
                )
            })
        })
    }

    fn schedule_retry_with_history<'a>(
        &'a self,
        channel_id: ChannelId,
        user_message_id: MessageId,
        _user_text: &'a str,
    ) -> GatewayFuture<'a, ()> {
        Box::pin(async move {
            tracing::warn!(
                provider = %self.provider.as_str(),
                channel_id = channel_id.get(),
                user_message_id = user_message_id.get(),
                "TUI-direct bridge adapter suppressed retry resubmission through Discord intake"
            );
        })
    }

    fn schedule_retry_with_history_with_completion<'a>(
        &'a self,
        channel_id: ChannelId,
        user_message_id: MessageId,
        user_text: &'a str,
        completion_tx: tokio::sync::oneshot::Sender<()>,
    ) -> GatewayFuture<'a, ()> {
        Box::pin(async move {
            self.schedule_retry_with_history(channel_id, user_message_id, user_text)
                .await;
            let _ = completion_tx.send(());
        })
    }

    fn dispatch_queued_turn<'a>(
        &'a self,
        channel_id: ChannelId,
        intervention: &'a Intervention,
        _request_owner_name: &'a str,
        has_more_queued_turns: bool,
        dispatch_lease: Option<std::sync::Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let restored = super::super::mailbox_restore_dequeued_head(
                &self.shared,
                &self.provider,
                channel_id,
                intervention.clone(),
                dispatch_lease
                    .ok_or_else(|| "queued bridge dispatch is missing its lease".to_string())?,
            )
            .await;
            if !restored.enqueued {
                return Err(format!(
                    "queued bridge restore rejected: {}",
                    restored
                        .refusal_reason
                        .map(|reason| reason.as_str())
                        .unwrap_or("persistence_error")
                ));
            }
            schedule_deferred_idle_queue_kickoff(
                self.shared.clone(),
                self.provider.clone(),
                channel_id,
                "requeue-front after bridge dispatch failure",
            );
            tracing::info!(
                provider = %self.provider.as_str(),
                channel_id = channel_id.get(),
                queued_message_id = intervention.message_id.get(),
                has_more_queued_turns,
                "TUI-direct bridge adapter deferred queued turn to normal Discord intake without prompt resubmission"
            );
            Ok(())
        })
    }

    fn validate_live_routing<'a>(
        &'a self,
        _channel_id: ChannelId,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async move { Ok(()) })
    }

    fn requester_mention(&self) -> Option<String> {
        None
    }

    fn can_chain_locally(&self) -> bool {
        true
    }

    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        None
    }
}
