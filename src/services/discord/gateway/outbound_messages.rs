//! Canonical outbound-v3 message helpers extracted from the gateway root.

use super::*;

/// #3082 part B: only queued-turn notices wait behind an in-flight answer
/// flush. The bounded barrier is shared by the intake placeholder helper.
pub(super) async fn await_answer_flush_if_queued_notice(
    barrier: &Arc<super::super::answer_flush_barrier::AnswerFlushBarrier>,
    channel_id: ChannelId,
    is_queued_notice: bool,
) {
    if !is_queued_notice {
        return;
    }
    if !barrier
        .wait_for_flush(
            channel_id,
            super::super::answer_flush_barrier::ANSWER_FLUSH_WAIT_TIMEOUT,
            super::super::answer_flush_barrier::ANSWER_FLUSH_WAIT_HARD_CEILING,
        )
        .await
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⏱ INTAKE: answer-flush barrier timed out for channel {}; posting queued card anyway (no deadlock)",
            channel_id
        );
    }
}

pub(in crate::services::discord) async fn send_intake_placeholder(
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    reference: Option<(ChannelId, MessageId)>,
    // Only the queued-turn "📬" notice waits. Active placeholders pass false.
    is_queued_notice: bool,
) -> Result<MessageId, String> {
    await_answer_flush_if_queued_notice(&shared.answer_flush_barrier, channel_id, is_queued_notice)
        .await;

    let client = SerenityTurnOutboundClient { http, shared };
    let mut msg = gateway_outbound_message(channel_id, "...");
    if let Some((reference_channel, reference_message)) = reference {
        msg = msg.with_reference(OutboundReferenceContext::reply_to(
            reference_channel,
            reference_message,
        ));
    }
    outbound_delivery_error(deliver_outbound(&client, shared_outbound_deduper(), msg, None).await)?
        .ok_or_else(|| "intake placeholder delivery was skipped".to_string())
}

pub(in crate::services::discord) async fn send_outbound_message(
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    content: &str,
) -> Result<MessageId, String> {
    let client = SerenityTurnOutboundClient { http, shared };
    let msg = gateway_outbound_message(channel_id, content);
    outbound_delivery_error(deliver_outbound(&client, shared_outbound_deduper(), msg, None).await)?
        .ok_or_else(|| "message delivery was skipped".to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum ClassifiedOutboundPostError {
    Transient(String),
    Permanent(String),
}

fn classify_terminal_post_result(result: &DeliveryResult) -> Option<ClassifiedOutboundPostError> {
    match result {
        DeliveryResult::TransientFailure { reason } => {
            Some(ClassifiedOutboundPostError::Transient(reason.clone()))
        }
        DeliveryResult::PermanentFailure { reason }
        | DeliveryResult::ConfirmedMissing { reason } => {
            Some(ClassifiedOutboundPostError::Permanent(reason.clone()))
        }
        DeliveryResult::Skip { reason } => {
            Some(ClassifiedOutboundPostError::Transient(reason.clone()))
        }
        _ => None,
    }
}

pub(in crate::services::discord) async fn send_outbound_message_with_nonce_classified(
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    content: &str,
    nonce: &str,
) -> Result<MessageId, ClassifiedOutboundPostError> {
    let client = SerenityTurnOutboundClient { http, shared };
    let msg = task_card_outbound_message(channel_id, content, nonce);
    let result = deliver_outbound(&client, shared_outbound_deduper(), msg, None).await;
    if let Some(error) = classify_terminal_post_result(&result) {
        return Err(error);
    }
    match result {
        committed => outbound_delivery_error(committed)
            .map_err(ClassifiedOutboundPostError::Transient)?
            .ok_or_else(|| {
                ClassifiedOutboundPostError::Transient(
                    "message delivery was skipped without an authoritative rejection".to_string(),
                )
            }),
    }
}

#[cfg(test)]
mod classified_post_tests {
    use super::*;
    use crate::services::discord::outbound::OutboundDeduper;
    use crate::services::dispatches::discord_delivery::{
        DispatchMessagePostError, DispatchMessagePostErrorKind,
    };

    struct FailingPostClient {
        status: Option<reqwest::StatusCode>,
    }

    impl DiscordOutboundClient for FailingPostClient {
        async fn post_message(
            &self,
            _target_channel: &str,
            _content: &str,
        ) -> Result<String, DispatchMessagePostError> {
            Err(match self.status {
                Some(status) => DispatchMessagePostError::http(
                    DispatchMessagePostErrorKind::Other,
                    status,
                    None,
                    format!("mock Discord POST {status}"),
                ),
                None => DispatchMessagePostError::new(
                    DispatchMessagePostErrorKind::Other,
                    "mock Discord transport failure".to_string(),
                ),
            })
        }
    }

    async fn production_card_post_class(
        status: Option<reqwest::StatusCode>,
    ) -> ClassifiedOutboundPostError {
        let result = deliver_outbound(
            &FailingPostClient { status },
            &OutboundDeduper::new(),
            task_card_outbound_message(ChannelId::new(4055), "task card", "adktest4055"),
            None,
        )
        .await;
        classify_terminal_post_result(&result).expect("failed POST must remain classified")
    }

    #[tokio::test]
    async fn production_card_post_preserves_transient_500_503_and_transport_vs_permanent_403() {
        for status in [
            Some(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
            Some(reqwest::StatusCode::SERVICE_UNAVAILABLE),
            None,
        ] {
            assert!(matches!(
                production_card_post_class(status).await,
                ClassifiedOutboundPostError::Transient(_)
            ));
        }
        assert!(matches!(
            production_card_post_class(Some(reqwest::StatusCode::FORBIDDEN)).await,
            ClassifiedOutboundPostError::Permanent(_)
        ));
    }

    #[test]
    fn authoritative_card_post_rejection_stays_permanent() {
        let result = DeliveryResult::PermanentFailure {
            reason: "Discord rejected task card POST with 403".to_string(),
        };
        assert_eq!(
            classify_terminal_post_result(&result),
            Some(ClassifiedOutboundPostError::Permanent(
                "Discord rejected task card POST with 403".to_string()
            ))
        );
        assert_eq!(
            classify_terminal_post_result(&DeliveryResult::Skip {
                reason: "in flight".to_string(),
            }),
            Some(ClassifiedOutboundPostError::Transient(
                "in flight".to_string()
            ))
        );
    }
}

#[derive(Debug)]
pub(in crate::services::discord) enum ClassifiedOutboundEditError {
    ConfirmedMissing(String),
    Other(String),
}

impl std::fmt::Display for ClassifiedOutboundEditError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConfirmedMissing(error) | Self::Other(error) => formatter.write_str(error),
        }
    }
}

pub(in crate::services::discord) async fn edit_outbound_message_classified(
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    content: &str,
) -> Result<(), ClassifiedOutboundEditError> {
    let client = SerenityTurnOutboundClient { http, shared };
    let msg = gateway_outbound_message(channel_id, content)
        .with_operation(OutboundOperation::Edit { message_id });
    match deliver_outbound(&client, shared_outbound_deduper(), msg, None).await {
        DeliveryResult::Sent { .. }
        | DeliveryResult::Fallback { .. }
        | DeliveryResult::Duplicate { .. } => Ok(()),
        DeliveryResult::ConfirmedMissing { reason } => {
            Err(ClassifiedOutboundEditError::ConfirmedMissing(reason))
        }
        DeliveryResult::Skip { reason }
        | DeliveryResult::TransientFailure { reason }
        | DeliveryResult::PermanentFailure { reason } => {
            Err(ClassifiedOutboundEditError::Other(reason))
        }
    }
}

pub(in crate::services::discord) async fn edit_outbound_message(
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
    content: &str,
) -> Result<(), String> {
    let client = SerenityTurnOutboundClient { http, shared };
    let msg = gateway_outbound_message(channel_id, content)
        .with_operation(OutboundOperation::Edit { message_id });
    outbound_delivery_error(deliver_outbound(&client, shared_outbound_deduper(), msg, None).await)
        .map(|_| ())
}
