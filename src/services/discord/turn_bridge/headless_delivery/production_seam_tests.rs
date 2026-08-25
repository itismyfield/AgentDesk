use super::*;
use crate::services::message_outbox::{OutboxEnqueueOutcome, OutboxMessage};

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

#[tokio::test]
async fn durable_exact_path_observes_cancellation_before_database_work() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgresql://127.0.0.1:1/agentdesk_durable_cancel_must_not_connect")
        .expect("lazy postgres pool");
    let cancel_token = CancelToken::new();
    cancel_token.publish_cancel("issue-5191-durable-test");
    let outcome = durable_outbox::enqueue_headless_outbox_with_rollout(
        &pool,
        OutboxMessage {
            target: "channel:5191",
            content: "answer",
            bot: "claude",
            source: "headless_turn",
            reason_code: Some("headless.delivery"),
            session_key: Some("session"),
        },
        Some(MessageId::new(8)),
        &ProviderKind::Claude,
        7,
        Some(&cancel_token),
        true,
    )
    .await
    .expect("pre-cancelled durable enqueue must not touch postgres");
    assert_eq!(outcome, OutboxEnqueueOutcome::Cancelled);
}

async fn durable_test_pool(
    name: &str,
) -> Option<(
    crate::dispatch::test_support::DispatchPostgresTestDb,
    sqlx::PgPool,
)> {
    let pg_db = crate::dispatch::test_support::DispatchPostgresTestDb::try_create(
        name,
        "durable headless outbox production seam tests",
    )
    .await?;
    let pool = pg_db.connect_and_migrate().await;
    Some((pg_db, pool))
}

async fn durable_enqueue(
    pool: &sqlx::PgPool,
    content: &str,
    owner: Option<MessageId>,
    session_key: Option<&str>,
) -> OutboxEnqueueOutcome {
    durable_enqueue_as(pool, content, owner, session_key, &ProviderKind::Claude, 7).await
}

async fn durable_enqueue_as(
    pool: &sqlx::PgPool,
    content: &str,
    owner: Option<MessageId>,
    session_key: Option<&str>,
    provider: &ProviderKind,
    born_generation: u64,
) -> OutboxEnqueueOutcome {
    durable_outbox::enqueue_headless_outbox_with_rollout(
        pool,
        OutboxMessage {
            target: "channel:5191",
            content,
            bot: "claude",
            source: "headless_turn",
            reason_code: Some("headless.delivery"),
            session_key,
        },
        owner,
        provider,
        born_generation,
        None,
        true,
    )
    .await
    .unwrap()
}

async fn durable_enqueue_with_rollout(
    pool: &sqlx::PgPool,
    owner: Option<MessageId>,
    session_key: Option<&str>,
    provider: &ProviderKind,
    generation: u64,
    durable_enabled: bool,
) -> OutboxEnqueueOutcome {
    durable_outbox::enqueue_headless_outbox_with_rollout(
        pool,
        OutboxMessage {
            target: "channel:5191",
            content: "same",
            bot: "claude",
            source: "headless_turn",
            reason_code: Some("headless.delivery"),
            session_key,
        },
        owner,
        provider,
        generation,
        None,
        durable_enabled,
    )
    .await
    .unwrap()
}

fn enqueued_id(outcome: OutboxEnqueueOutcome) -> i64 {
    let OutboxEnqueueOutcome::Enqueued { id } = outcome else {
        panic!("expected enqueued outcome, got {outcome:?}")
    };
    id
}

#[tokio::test]
async fn durable_exact_identity_is_content_free_and_owner_sensitive_pg() {
    let Some((_pg_db, pool)) = durable_test_pool("agentdesk_headless_durable_identity").await
    else {
        return;
    };
    let first = durable_enqueue(
        &pool,
        "first rendering",
        Some(MessageId::new(8)),
        Some(" session "),
    )
    .await;
    let duplicate = durable_enqueue(
        &pool,
        "different rendering",
        Some(MessageId::new(8)),
        Some(" session "),
    )
    .await;
    let different_owner = durable_enqueue(
        &pool,
        "first rendering",
        Some(MessageId::new(9)),
        Some(" session "),
    )
    .await;
    let different_session = durable_enqueue(
        &pool,
        "first rendering",
        Some(MessageId::new(8)),
        Some("session"),
    )
    .await;

    let id = enqueued_id(first);
    assert_eq!(
        duplicate,
        OutboxEnqueueOutcome::Enqueued { id },
        "rendered content must not change exact identity"
    );
    assert!(matches!(
        different_owner,
        OutboxEnqueueOutcome::Enqueued { id: other }
            if other != id
    ));
    assert_eq!(
        different_session,
        OutboxEnqueueOutcome::Enqueued { id },
        "session routing bytes are preserved but are not a replacement for the required identity tuple"
    );
    let stored_session: String =
        sqlx::query_scalar("SELECT session_key FROM message_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .expect("read stored session routing key");
    assert_eq!(stored_session, " session ");
    let (stored_content, finite_expiry): (String, bool) = sqlx::query_as(
        "SELECT content, dedupe_expires_at IS NOT NULL FROM message_outbox WHERE id=$1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .expect("read conflict-preserved row");
    assert_eq!(stored_content, "first rendering");
    assert!(
        finite_expiry,
        "durable sent rows must remain retention-eligible"
    );
}

#[tokio::test]
async fn durable_provider_channel_and_generation_axes_are_distinct_pg() {
    let Some((_pg_db, pool)) = durable_test_pool("agentdesk_headless_durable_tuple_axes").await
    else {
        return;
    };
    let owner = Some(MessageId::new(8));
    let first = durable_enqueue_as(
        &pool,
        "answer",
        owner,
        Some("session"),
        &ProviderKind::Claude,
        7,
    )
    .await;
    let first_id = enqueued_id(first);
    let new_generation = durable_enqueue_as(
        &pool,
        "answer",
        owner,
        Some("session"),
        &ProviderKind::Claude,
        8,
    )
    .await;
    let new_provider = durable_enqueue_as(
        &pool,
        "answer",
        owner,
        Some("session"),
        &ProviderKind::Codex,
        7,
    )
    .await;
    for outcome in [new_generation, new_provider] {
        assert!(matches!(
            outcome,
            OutboxEnqueueOutcome::Enqueued { id }
                if id != first_id
        ));
    }
    let other_channel = durable_outbox::enqueue_headless_outbox_with_rollout(
        &pool,
        OutboxMessage {
            target: "channel:5192",
            content: "answer",
            bot: "claude",
            source: "headless_turn",
            reason_code: Some("headless.delivery"),
            session_key: Some("session"),
        },
        owner,
        &ProviderKind::Claude,
        7,
        None,
        true,
    )
    .await
    .expect("enqueue other-channel durable row");
    assert!(matches!(
        other_channel,
        OutboxEnqueueOutcome::Enqueued { id }
            if id != first_id
    ));
}

#[tokio::test]
async fn durable_active_statuses_return_existing_id_and_terminal_statuses_retry_pg() {
    let Some((_pg_db, pool)) = durable_test_pool("agentdesk_headless_durable_statuses").await
    else {
        return;
    };
    let predicate: String = sqlx::query_scalar("SELECT pg_get_expr(i.indpred, i.indrelid) FROM pg_index i JOIN pg_class c ON c.oid=i.indexrelid WHERE c.relname='uq_message_outbox_active_dedupe_key'").fetch_one(&pool).await.unwrap();
    assert!(
        predicate.contains("status <> ALL")
            && predicate.contains("failed")
            && predicate.contains("cancelled")
    );
    for (index, status) in ["pending", "processing", "held", "sent"]
        .into_iter()
        .enumerate()
    {
        let owner = MessageId::new(100 + index as u64);
        let first = durable_enqueue(&pool, "answer", Some(owner), Some("session")).await;
        let id = enqueued_id(first);
        sqlx::query("UPDATE message_outbox SET status=$1 WHERE id=$2")
            .bind(status)
            .bind(id)
            .execute(&pool)
            .await
            .expect("set active status");
        assert_eq!(
            durable_enqueue(&pool, "changed answer", Some(owner), Some("session")).await,
            OutboxEnqueueOutcome::Enqueued { id },
            "active {status} row must be the durable handoff"
        );
    }

    for (index, status) in ["failed", "cancelled"].into_iter().enumerate() {
        let owner = MessageId::new(200 + index as u64);
        let first = durable_enqueue(&pool, "answer", Some(owner), Some("session")).await;
        let id = enqueued_id(first);
        sqlx::query(
            "UPDATE message_outbox
             SET status=$1,
                 cancelled_at=CASE WHEN $1='cancelled' THEN NOW() ELSE cancelled_at END,
                 cancel_reason=CASE WHEN $1='cancelled' THEN 'issue-5191-test' ELSE cancel_reason END
             WHERE id=$2",
        )
        .bind(status)
        .bind(id)
        .execute(&pool)
        .await
        .expect("set terminal status");
        assert!(matches!(
            durable_enqueue(&pool, "changed answer", Some(owner), Some("session")).await,
            OutboxEnqueueOutcome::Enqueued { id: fresh }
                if fresh != id
        ));
    }
}

#[tokio::test]
async fn durable_concurrent_duplicates_converge_on_one_active_row_pg() {
    let Some((_pg_db, pool)) = durable_test_pool("agentdesk_headless_durable_concurrent").await
    else {
        return;
    };
    let left = durable_enqueue(
        &pool,
        "left rendering",
        Some(MessageId::new(5191)),
        Some("session"),
    );
    let right = durable_enqueue(
        &pool,
        "right rendering",
        Some(MessageId::new(5191)),
        Some("session"),
    );
    let (left, right) = tokio::join!(left, right);
    assert_eq!(left, right);
}

#[tokio::test]
async fn durable_existing_sent_row_is_immediately_visible_pg() {
    let Some((_pg_db, pool)) = durable_test_pool("agentdesk_headless_durable_sent_visible").await
    else {
        return;
    };
    let id = enqueued_id(
        durable_enqueue(
            &pool,
            "visible answer",
            Some(MessageId::new(5192)),
            Some("session"),
        )
        .await,
    );
    sqlx::query("UPDATE message_outbox SET status='sent', sent_at=NOW() WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .expect("mark durable row sent");
    assert_eq!(
        durable_enqueue(
            &pool,
            "re-rendered answer",
            Some(MessageId::new(5192)),
            Some("session"),
        )
        .await,
        OutboxEnqueueOutcome::Enqueued { id }
    );
    assert_eq!(
        durable_outbox::wait_for_headless_delivery_outbox_visible(
            &pool,
            id,
            std::time::Duration::ZERO,
        )
        .await,
        Ok(())
    );
}

#[tokio::test]
async fn durable_rollout_falls_back_to_ttl_zero_for_off_or_incomplete_identity_pg() {
    let Some((_pg_db, pool)) = durable_test_pool("agentdesk_headless_durable_rollout").await else {
        return;
    };
    let over_bound = "x".repeat(durable_outbox::MAX_HEADLESS_SESSION_ROUTING_KEY_BYTES + 1);
    let unsupported = ProviderKind::Unsupported("issue-5191-test".to_string());
    for (enabled, owner, session, provider, generation) in [
        (
            false,
            Some(MessageId::new(8)),
            Some("session"),
            &ProviderKind::Claude,
            7,
        ),
        (true, None, Some("session"), &ProviderKind::Claude, 7),
        (
            true,
            Some(MessageId::new(8)),
            Some(over_bound.as_str()),
            &ProviderKind::Claude,
            7,
        ),
        (
            true,
            Some(MessageId::new(8)),
            Some("session"),
            &ProviderKind::Claude,
            0,
        ),
        (
            true,
            Some(MessageId::new(8)),
            Some("session"),
            &unsupported,
            7,
        ),
    ] {
        let first =
            durable_enqueue_with_rollout(&pool, owner, session, provider, generation, enabled)
                .await;
        let second =
            durable_enqueue_with_rollout(&pool, owner, session, provider, generation, enabled)
                .await;
        assert_ne!(
            first, second,
            "legacy TTL-zero path must not suppress duplicates"
        );
    }
    let missing_session_first = durable_enqueue_with_rollout(
        &pool,
        Some(MessageId::new(8)),
        None,
        &ProviderKind::Claude,
        7,
        true,
    )
    .await;
    let missing_session_duplicate =
        durable_enqueue(&pool, "different", Some(MessageId::new(8)), None).await;
    assert_eq!(
        missing_session_first, missing_session_duplicate,
        "the required identity tuple remains complete without an optional routing key"
    );
}
