use super::*;

fn test_delivery_arguments<'a>(
    shared: &'a Arc<SharedData>,
    state: &'a InflightTurnState,
    provider: &'a ProviderKind,
    content: &'a str,
    cancel_token: Option<&'a CancelToken>,
) -> HeadlessDeliveryArguments<'a> {
    assemble_headless_delivery_arguments(
        state,
        HeadlessDeliveryInputs {
            shared,
            channel_id: ChannelId::new(5191),
            owning_user_msg_id: Some(MessageId::new(8)),
            session_key: Some("headless-seam-test"),
            provider,
            content,
            cancel_token,
        },
    )
}

fn test_inflight_state() -> InflightTurnState {
    InflightTurnState::new(
        ProviderKind::Claude,
        5191,
        None,
        7,
        8,
        9,
        "prompt".to_string(),
        None,
        None,
        None,
        None,
        0,
    )
}

/// The direct (non-outbox) fallback keeps its notify-http preference only
/// for a caller-supplied identity, so routine fallback behaviour is
/// unchanged while a user turn answer falls through to this runtime's own
/// provider http.
#[test]
fn direct_fallback_notify_http_preference_is_caller_supplied_only() {
    assert!(headless_direct_fallback_prefers_notify_http(Some("notify")));
    assert!(headless_direct_fallback_prefers_notify_http(Some("dm")));
    assert!(!headless_direct_fallback_prefers_notify_http(None));
    assert!(!headless_direct_fallback_prefers_notify_http(Some("   ")));
}

#[tokio::test]
async fn outbox_enqueue_error_reaches_cancel_check_then_direct_fallback() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(150))
        .connect_lazy("postgresql://postgres@127.0.0.1:1/agentdesk_headless_pg_error")
        .expect("construct unreachable PostgreSQL pool");
    let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool));
    let state = test_inflight_state();
    let provider = ProviderKind::Claude;

    let outcome = enqueue_headless_delivery(test_delivery_arguments(
        &shared, &state, &provider, "answer", None,
    ))
    .await;

    assert_eq!(
        outcome,
        HeadlessDeliveryOutcome::Ambiguous {
            surfaced_error: Some(
                "headless delivery unavailable for channel 5191: no outbox storage or discord http"
                    .to_string(),
            ),
        },
        "a PostgreSQL enqueue error must fall through the production cancel check into the direct fallback"
    );
}

#[tokio::test]
async fn absent_outbox_pool_reaches_cancel_check_and_suppresses_direct_fallback() {
    let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
    assert!(
        shared.pg_pool.is_none(),
        "fixture must exercise the absent-pool arm"
    );
    let state = test_inflight_state();
    let provider = ProviderKind::Claude;
    let cancel_token = CancelToken::new();
    cancel_token
        .cancelled
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let outcome = enqueue_headless_delivery(test_delivery_arguments(
        &shared,
        &state,
        &provider,
        "answer",
        Some(&cancel_token),
    ))
    .await;

    assert_eq!(outcome, HeadlessDeliveryOutcome::Cancelled);
}
