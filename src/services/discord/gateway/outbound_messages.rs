//! Canonical outbound-v3 message helpers extracted from the gateway root.

use super::*;

#[cfg(test)]
mod placeholder_tests {
    use super::*;
    use axum::{
        Json, Router,
        body::Bytes,
        http::{Method, StatusCode, Uri},
        routing::any,
    };
    use std::sync::Mutex;
    use std::time::Duration;

    async fn exercise(fail: bool) -> (Vec<String>, Vec<(String, String, String)>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let app = Router::new().route("/{*path}", any(move |method: Method, uri: Uri, body: Bytes| {
            let recorded = recorded.clone();
            async move {
                recorded.lock().unwrap_or_else(|e| e.into_inner())
                    .push((method.to_string(), uri.path().to_string(), String::from_utf8(body.to_vec()).unwrap()));
                if fail {
                    return (StatusCode::BAD_REQUEST, Json(serde_json::json!({"code":50035,"message":"injected failure after effect"})));
                }
                (StatusCode::OK, Json(serde_json::json!({
                    "id":"99","channel_id":"7","content":"...",
                    "author":{"id":"1","username":"bot","discriminator":"0001","avatar":null},
                    "timestamp":"2026-10-10T00:00:00+00:00","edited_timestamp":null,
                    "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],
                    "attachments":[],"embeds":[],"pinned":false,"type":0
                })))
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = Arc::new(
            serenity::HttpBuilder::new("test-token")
                .proxy(format!("http://{}", listener.local_addr().unwrap()))
                .ratelimiter_disabled(true)
                .build(),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let shared = crate::services::discord::make_shared_data_for_tests();
        let post = send_intake_placeholder(
            http.clone(),
            shared.clone(),
            ChannelId::new(7),
            Some((ChannelId::new(7), MessageId::new(8))),
            false,
        )
        .await;
        let patch =
            edit_intake_placeholder(http, shared, ChannelId::new(7), MessageId::new(99)).await;
        server.abort();
        let result = vec![format!("{post:?}"), format!("{}", patch.is_ok())];
        let calls = calls.lock().unwrap_or_else(|e| e.into_inner()).clone();
        (result, calls)
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn real_post_patch_preserve_http_results_and_reply_reference() {
        let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let _root = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            dir.path().as_os_str(),
        );
        for fail in [false, true] {
            let (results, calls) = runtime().block_on(exercise(fail));
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0].0, "POST");
            assert_eq!(calls[1].0, "PATCH");
            assert_eq!(results[0].starts_with("Ok("), !fail, "{results:?}");
            assert_eq!(results[1], (!fail).to_string());
            let post: serde_json::Value = serde_json::from_str(&calls[0].2).unwrap();
            assert_eq!(post["content"], "...");
            assert_eq!(post["message_reference"]["message_id"], "8");
            let patch: serde_json::Value = serde_json::from_str(&calls[1].2).unwrap();
            assert_eq!(patch["content"], "...");
        }
    }

    #[test]
    fn queued_barrier_cancellation_prevents_http() {
        runtime().block_on(async {
            let shared = crate::services::discord::make_shared_data_for_tests();
            let channel = ChannelId::new(7);
            let _flush = shared.answer_flush_barrier.begin_flush(channel);
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let recorded = calls.clone();
            let app = Router::new().route(
                "/{*path}",
                any(move || {
                    recorded.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    async { StatusCode::BAD_REQUEST }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let http = Arc::new(
                serenity::HttpBuilder::new("test-token")
                    .proxy(format!("http://{}", listener.local_addr().unwrap()))
                    .ratelimiter_disabled(true)
                    .build(),
            );
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let mut send = Box::pin(send_intake_placeholder(http, shared, channel, None, true));
            assert!(
                tokio::time::timeout(Duration::from_millis(10), &mut send)
                    .await
                    .is_err()
            );
            drop(send);
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
            server.abort();
        });
    }
}

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

pub(in crate::services::discord) async fn edit_intake_placeholder(
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    message_id: MessageId,
) -> Result<(), ClassifiedOutboundEditError> {
    edit_outbound_message_classified(http, shared, channel_id, message_id, "...").await
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
    outbound_delivery_error(deliver_outbound(&client, shared_outbound_deduper(), msg, None).await)
        .and_then(|id| id.ok_or_else(|| "intake placeholder delivery was skipped".to_string()))
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

fn classify_outbound_edit_result(
    result: DeliveryResult,
) -> Result<(), ClassifiedOutboundEditError> {
    match result {
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
    classify_outbound_edit_result(
        deliver_outbound(&client, shared_outbound_deduper(), msg, None).await,
    )
}

#[cfg(test)]
mod classified_edit_tests {
    use super::*;

    #[test]
    fn only_authoritative_missing_allows_placeholder_replacement_4888() {
        assert!(matches!(
            classify_outbound_edit_result(DeliveryResult::ConfirmedMissing {
                reason: "404 Unknown Message (10008)".to_string(),
            }),
            Err(ClassifiedOutboundEditError::ConfirmedMissing(_))
        ));
        for result in [
            DeliveryResult::TransientFailure {
                reason: "429 rate limited".to_string(),
            },
            DeliveryResult::TransientFailure {
                reason: "500 server error".to_string(),
            },
            DeliveryResult::TransientFailure {
                reason: "503 unavailable".to_string(),
            },
            DeliveryResult::TransientFailure {
                reason: "network timeout".to_string(),
            },
            DeliveryResult::PermanentFailure {
                reason: "403 forbidden".to_string(),
            },
        ] {
            assert!(matches!(
                classify_outbound_edit_result(result),
                Err(ClassifiedOutboundEditError::Other(_))
            ));
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

pub(super) fn outbound_delivery_error(result: DeliveryResult) -> Result<Option<MessageId>, String> {
    match result {
        DeliveryResult::Sent { messages, .. } => first_raw_message_id(&messages)
            .map(|message_id| parse_message_id(&message_id))
            .transpose(),
        DeliveryResult::Fallback {
            messages,
            fallback_used,
            ..
        } => {
            let message_id = first_raw_message_id(&messages).unwrap_or_default();
            tracing::info!(
                delivery_status = "fallback",
                fallback_kind = ?fallback_used,
                message_id,
                "[discord] outbound delivery used fallback"
            );
            parse_message_id(&message_id).map(Some)
        }
        DeliveryResult::Duplicate {
            existing_messages, ..
        } => {
            let message_id = first_raw_message_id(&existing_messages);
            tracing::info!(
                delivery_status = "duplicate",
                ?message_id,
                "[discord] outbound delivery deduplicated"
            );
            match message_id {
                Some(message_id) => parse_message_id(&message_id).map(Some),
                None => Ok(None),
            }
        }
        DeliveryResult::Skip { reason } => {
            tracing::info!(
                delivery_status = "skip",
                reason,
                "[discord] outbound delivery skipped"
            );
            Ok(None)
        }
        DeliveryResult::TransientFailure { reason }
        | DeliveryResult::ConfirmedMissing { reason }
        | DeliveryResult::PermanentFailure { reason } => Err(reason),
    }
}
