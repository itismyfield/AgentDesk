use super::super::formatting::ReplaceLongMessageOutcome;
use super::*;

fn record_turn_bridge_terminal_replace_cleanup(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
    tmux_session_name: Option<&str>,
    outcome: super::super::placeholder_cleanup::PlaceholderCleanupOutcome,
    source: &'static str,
) {
    if let super::super::placeholder_cleanup::PlaceholderCleanupOutcome::Failed { class, detail } =
        &outcome
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ placeholder cleanup {} failed ({}) for channel {} msg {}: {}",
            super::super::placeholder_cleanup::PlaceholderCleanupOperation::EditTerminal.as_str(),
            class.as_str(),
            channel_id.get(),
            message_id.get(),
            detail
        );
    }
    shared.placeholder_cleanup.record(
        super::super::placeholder_cleanup::PlaceholderCleanupRecord {
            provider: provider.clone(),
            channel_id,
            message_id,
            tmux_session_name: tmux_session_name.map(str::to_string),
            operation: super::super::placeholder_cleanup::PlaceholderCleanupOperation::EditTerminal,
            outcome,
            source,
        },
    );
}

fn replace_outcome_commits_terminal_delivery(outcome: &ReplaceLongMessageOutcome) -> bool {
    matches!(outcome, ReplaceLongMessageOutcome::EditedOriginal)
}

pub(super) fn terminal_delivery_should_send_new_chunks(
    can_chain_locally: bool,
    formatted_response: &str,
) -> bool {
    can_chain_locally && formatted_response.len() > super::super::DISCORD_MSG_LIMIT
}

pub(super) async fn send_ordered_long_terminal_response(
    shared: &SharedData,
    gateway: &dyn TurnGateway,
    provider: &ProviderKind,
    channel_id: ChannelId,
    placeholder_msg_id: MessageId,
    tmux_session_name: Option<&str>,
    response: &str,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
) -> Result<MessageId, String> {
    let (first_msg_id, delete_result) =
        send_ordered_long_terminal_chunks(gateway, channel_id, placeholder_msg_id, response)
            .await?;
    let cleanup_outcome = match delete_result {
        Ok(()) => super::super::placeholder_cleanup::PlaceholderCleanupOutcome::Succeeded,
        Err(error) => super::super::placeholder_cleanup::classify_delete_error(&error),
    };
    shared.placeholder_cleanup.record(
        super::super::placeholder_cleanup::PlaceholderCleanupRecord {
            provider: provider.clone(),
            channel_id,
            message_id: placeholder_msg_id,
            tmux_session_name: tmux_session_name.map(str::to_string),
            operation:
                super::super::placeholder_cleanup::PlaceholderCleanupOperation::DeleteTerminal,
            outcome: cleanup_outcome,
            source: "turn_bridge_terminal_long_send_cleanup",
        },
    );
    crate::services::observability::emit_relay_delivery(
        provider.as_str(),
        channel_id.get(),
        dispatch_id,
        session_key,
        turn_id,
        Some(first_msg_id.get()),
        "turn_bridge",
        "post",
        None,
        None,
        true,
        Some("terminal long response sent as ordered chunks"),
    );
    Ok(first_msg_id)
}

async fn send_ordered_long_terminal_chunks(
    gateway: &dyn TurnGateway,
    channel_id: ChannelId,
    placeholder_msg_id: MessageId,
    response: &str,
) -> Result<(MessageId, Result<(), String>), String> {
    let message_ids = gateway
        .send_long_message_with_rollback(channel_id, placeholder_msg_id, response)
        .await?;
    let first_msg_id = message_ids
        .first()
        .copied()
        .ok_or_else(|| "long terminal response produced no Discord chunks".to_string())?;
    let delete_result = gateway.delete_message(channel_id, placeholder_msg_id).await;
    Ok((first_msg_id, delete_result))
}

pub(super) fn turn_bridge_replace_outcome_committed(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
    tmux_session_name: Option<&str>,
    replace_result: Result<ReplaceLongMessageOutcome, String>,
    dispatch_id: Option<&str>,
    session_key: Option<&str>,
    turn_id: Option<&str>,
    source: &'static str,
) -> bool {
    let committed = match replace_result {
        Ok(ReplaceLongMessageOutcome::EditedOriginal) => {
            record_turn_bridge_terminal_replace_cleanup(
                shared,
                provider,
                channel_id,
                message_id,
                tmux_session_name,
                super::super::placeholder_cleanup::PlaceholderCleanupOutcome::Succeeded,
                source,
            );
            true
        }
        Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure { edit_error }) => {
            record_turn_bridge_terminal_replace_cleanup(
                shared,
                provider,
                channel_id,
                message_id,
                tmux_session_name,
                super::super::placeholder_cleanup::PlaceholderCleanupOutcome::failed(edit_error),
                source,
            );
            false
        }
        Ok(ReplaceLongMessageOutcome::PartialContinuationFailure {
            sent_chunks,
            total_chunks,
            failed_chunk_index,
            sent_continuation_message_ids,
            cleanup_errors,
            error,
        }) => {
            record_turn_bridge_terminal_replace_cleanup(
                shared,
                provider,
                channel_id,
                message_id,
                tmux_session_name,
                super::super::placeholder_cleanup::PlaceholderCleanupOutcome::failed(format!(
                    "partial continuation failure: sent_chunks={sent_chunks}, total_chunks={total_chunks}, failed_chunk_index={failed_chunk_index}, cleaned_continuations={}, cleanup_errors={}, error={error}",
                    sent_continuation_message_ids.len(),
                    cleanup_errors.len()
                )),
                source,
            );
            false
        }
        Err(error) => {
            record_turn_bridge_terminal_replace_cleanup(
                shared,
                provider,
                channel_id,
                message_id,
                tmux_session_name,
                super::super::placeholder_cleanup::PlaceholderCleanupOutcome::failed(error),
                source,
            );
            false
        }
    };
    // #2838 (relay-stability P0-1): emit a structured event for the bridge-side
    // terminal delivery decision. The watcher path already has the
    // `relay_flight_recorder` tracing, but bridge-owned replace deliveries were
    // unobserved; this makes them PG-queryable and attributable so the
    // duplicate/uncommitted vectors can be measured before the delivery-lease
    // consolidation lands.
    crate::services::observability::emit_relay_delivery(
        provider.as_str(),
        channel_id.get(),
        dispatch_id,
        session_key,
        turn_id,
        Some(message_id.get()),
        "turn_bridge",
        "edit",
        None,
        None,
        committed,
        Some(source),
    );
    committed
}

pub(super) fn should_complete_work_dispatch_after_terminal_delivery(
    completion_candidate: bool,
    terminal_delivery_committed: bool,
    preserve_inflight_for_cleanup_retry: bool,
    resume_failure_detected: bool,
    recovery_retry: bool,
    full_response: &str,
) -> bool {
    completion_candidate
        && terminal_delivery_committed
        && !preserve_inflight_for_cleanup_retry
        && !resume_failure_detected
        && !recovery_retry
        && !full_response.trim().is_empty()
}

pub(super) fn should_fail_dispatch_after_terminal_delivery(
    fail_candidate: bool,
    terminal_delivery_committed: bool,
    preserve_inflight_for_cleanup_retry: bool,
) -> bool {
    fail_candidate && terminal_delivery_committed && !preserve_inflight_for_cleanup_retry
}

#[cfg(test)]
mod tests {
    use super::{
        replace_outcome_commits_terminal_delivery, send_ordered_long_terminal_chunks,
        should_complete_work_dispatch_after_terminal_delivery,
        should_fail_dispatch_after_terminal_delivery, terminal_delivery_should_send_new_chunks,
    };
    use crate::services::discord::formatting;
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    use crate::services::discord::gateway::{GatewayFuture, TurnGateway};
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::{ChannelId, MessageId};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct FakeOrderedChunkGateway {
        sent_chunks: Arc<Mutex<Vec<String>>>,
        deleted_messages: Arc<Mutex<Vec<MessageId>>>,
        fail_after_sent_chunks: Option<usize>,
    }

    impl TurnGateway for FakeOrderedChunkGateway {
        fn send_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<MessageId, String>> {
            Box::pin(async { Err("single-message send must not be used".to_string()) })
        }

        fn send_long_message_with_rollback<'a>(
            &'a self,
            _channel_id: ChannelId,
            _rollback_anchor_msg_id: MessageId,
            content: &'a str,
        ) -> GatewayFuture<'a, Result<Vec<MessageId>, String>> {
            let sent_chunks = self.sent_chunks.clone();
            let fail_after_sent_chunks = self.fail_after_sent_chunks;
            Box::pin(async move {
                let chunks = formatting::split_message(content);
                let mut message_ids = Vec::new();
                for (index, chunk) in chunks.iter().enumerate() {
                    sent_chunks
                        .lock()
                        .expect("sent chunks lock")
                        .push(chunk.clone());
                    message_ids.push(MessageId::new(9000 + index as u64));
                    if fail_after_sent_chunks == Some(index + 1) {
                        sent_chunks.lock().expect("sent chunks lock").clear();
                        return Err("simulated chunk failure after rollback".to_string());
                    }
                }
                Ok(message_ids)
            })
        }

        fn edit_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn delete_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            message_id: MessageId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            let deleted_messages = self.deleted_messages.clone();
            Box::pin(async move {
                deleted_messages
                    .lock()
                    .expect("deleted messages lock")
                    .push(message_id);
                Ok(())
            })
        }

        fn replace_message_with_outcome<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
            Box::pin(async { Ok(ReplaceLongMessageOutcome::EditedOriginal) })
        }

        fn add_reaction<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _emoji: char,
        ) -> GatewayFuture<'a, ()> {
            Box::pin(async {})
        }

        fn remove_reaction<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _emoji: char,
        ) -> GatewayFuture<'a, ()> {
            Box::pin(async {})
        }

        fn schedule_retry_with_history<'a>(
            &'a self,
            _channel_id: ChannelId,
            _user_message_id: MessageId,
            _user_text: &'a str,
        ) -> GatewayFuture<'a, ()> {
            Box::pin(async {})
        }

        fn dispatch_queued_turn<'a>(
            &'a self,
            _channel_id: ChannelId,
            _intervention: &'a crate::services::discord::Intervention,
            _request_owner_name: &'a str,
            _has_more_queued_turns: bool,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn validate_live_routing<'a>(
            &'a self,
            _channel_id: ChannelId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn requester_mention(&self) -> Option<String> {
            None
        }

        fn can_chain_locally(&self) -> bool {
            true
        }

        fn bot_owner_provider(&self) -> Option<ProviderKind> {
            Some(ProviderKind::Codex)
        }
    }

    #[test]
    fn work_dispatch_completion_requires_terminal_delivery_commit() {
        assert!(should_complete_work_dispatch_after_terminal_delivery(
            true,
            true,
            false,
            false,
            false,
            "visible final response",
        ));

        assert!(!should_complete_work_dispatch_after_terminal_delivery(
            true,
            false,
            false,
            false,
            false,
            "visible final response",
        ));
        assert!(!should_complete_work_dispatch_after_terminal_delivery(
            true,
            true,
            true,
            false,
            false,
            "visible final response",
        ));
        assert!(!should_complete_work_dispatch_after_terminal_delivery(
            true,
            true,
            false,
            true,
            false,
            "visible final response",
        ));
        assert!(!should_complete_work_dispatch_after_terminal_delivery(
            true,
            true,
            false,
            false,
            true,
            "visible final response",
        ));
        assert!(!should_complete_work_dispatch_after_terminal_delivery(
            true, true, false, false, false, "   ",
        ));
    }

    #[test]
    fn final_completion_delivery_stays_blocked_until_terminal_message_commits() {
        assert!(!should_complete_work_dispatch_after_terminal_delivery(
            true,
            false,
            false,
            false,
            false,
            "final response waiting for Discord delivery",
        ));
        assert!(should_complete_work_dispatch_after_terminal_delivery(
            true,
            true,
            false,
            false,
            false,
            "final response delivered",
        ));
    }

    #[test]
    fn partial_continuation_failure_does_not_commit_terminal_delivery() {
        let outcome = ReplaceLongMessageOutcome::PartialContinuationFailure {
            sent_chunks: 1,
            total_chunks: 3,
            failed_chunk_index: 1,
            sent_continuation_message_ids: Vec::new(),
            cleanup_errors: Vec::new(),
            error: "HTTP 500".to_string(),
        };

        assert!(!replace_outcome_commits_terminal_delivery(&outcome));
        assert!(!should_complete_work_dispatch_after_terminal_delivery(
            true,
            replace_outcome_commits_terminal_delivery(&outcome),
            false,
            false,
            false,
            "final response with missing continuation",
        ));
        assert!(!should_fail_dispatch_after_terminal_delivery(
            true,
            replace_outcome_commits_terminal_delivery(&outcome),
            false,
        ));
    }

    #[test]
    fn long_terminal_response_uses_new_chunk_messages() {
        let body = format!(
            "[E2E:E15:BEGIN]\n{}\n[E2E:E15:MID]\n{}\n[E2E:E15:END]",
            "E15-LINE-010\n".repeat(90),
            "E15-LINE-150\n".repeat(90)
        );

        assert!(body.len() > crate::services::discord::DISCORD_MSG_LIMIT);
        assert!(terminal_delivery_should_send_new_chunks(true, &body));
        assert!(!terminal_delivery_should_send_new_chunks(
            true,
            "[E2E:E15:BEGIN]\nE15-LINE-150\n[E2E:E15:END]"
        ));
        assert!(!terminal_delivery_should_send_new_chunks(false, &body));
    }

    #[tokio::test]
    async fn ordered_long_terminal_delivery_sends_all_chunks_and_deletes_placeholder() {
        let body = format!(
            "[E2E:E15:BEGIN]{}[E2E:E15:MID]{}[E2E:E15:END]",
            "A".repeat(2500),
            "B".repeat(2500)
        );
        let gateway = FakeOrderedChunkGateway::default();
        let placeholder_msg_id = MessageId::new(42);

        let (first_msg_id, delete_result) = send_ordered_long_terminal_chunks(
            &gateway,
            ChannelId::new(7),
            placeholder_msg_id,
            &body,
        )
        .await
        .expect("ordered long terminal send");

        assert_eq!(first_msg_id, MessageId::new(9000));
        assert!(delete_result.is_ok());
        let chunks = gateway
            .sent_chunks
            .lock()
            .expect("sent chunks lock")
            .clone();
        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.len() <= crate::services::discord::DISCORD_MSG_LIMIT)
        );
        assert_eq!(chunks.concat(), body);
        assert_eq!(
            gateway
                .deleted_messages
                .lock()
                .expect("deleted messages lock")
                .as_slice(),
            &[placeholder_msg_id]
        );
    }

    #[tokio::test]
    async fn ordered_long_terminal_delivery_rolls_back_partial_chunks_before_retry() {
        let body = format!(
            "[E2E:E15:BEGIN]{}[E2E:E15:MID]{}[E2E:E15:END]",
            "A".repeat(2500),
            "B".repeat(2500)
        );
        let gateway = FakeOrderedChunkGateway {
            fail_after_sent_chunks: Some(1),
            ..FakeOrderedChunkGateway::default()
        };

        let result = send_ordered_long_terminal_chunks(
            &gateway,
            ChannelId::new(7),
            MessageId::new(42),
            &body,
        )
        .await;

        assert!(result.is_err());
        assert!(
            gateway
                .sent_chunks
                .lock()
                .expect("sent chunks lock")
                .is_empty(),
            "rollback-aware sender must not leave partial chunks that a retry would duplicate"
        );
        assert!(
            gateway
                .deleted_messages
                .lock()
                .expect("deleted messages lock")
                .is_empty(),
            "placeholder cleanup must wait until all chunks commit"
        );
    }

    #[test]
    fn transport_error_dispatch_failure_requires_terminal_delivery_commit() {
        assert!(should_fail_dispatch_after_terminal_delivery(
            true, true, false,
        ));
        assert!(!should_fail_dispatch_after_terminal_delivery(
            true, false, false,
        ));
        assert!(!should_fail_dispatch_after_terminal_delivery(
            true, true, true,
        ));
        assert!(!should_fail_dispatch_after_terminal_delivery(
            false, true, false,
        ));
    }
}
