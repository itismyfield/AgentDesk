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
            attachment: None,
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

async fn durable_enqueue_with(
    pool: &sqlx::PgPool,
    content: &str,
    target: &str,
    owner: Option<u64>,
    session_key: Option<&str>,
    provider: &ProviderKind,
    generation: u64,
    enabled: bool,
) -> OutboxEnqueueOutcome {
    durable_outbox::enqueue_headless_outbox_with_rollout(
        pool,
        OutboxMessage {
            target,
            content,
            bot: "claude",
            source: "headless_turn",
            reason_code: Some("headless.delivery"),
            session_key,
            attachment: None,
        },
        owner.map(MessageId::new),
        provider,
        generation,
        None,
        enabled,
    )
    .await
    .unwrap()
}

async fn durable_enqueue(
    pool: &sqlx::PgPool,
    content: &str,
    owner: u64,
    session_key: Option<&str>,
) -> OutboxEnqueueOutcome {
    durable_enqueue_with(
        pool,
        content,
        "channel:5191",
        Some(owner),
        session_key,
        &ProviderKind::Claude,
        7,
        true,
    )
    .await
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
    let id = enqueued_id(durable_enqueue(&pool, "first rendering", 8, Some(" session ")).await);
    assert_eq!(
        durable_enqueue(&pool, "different rendering", 8, Some(" session ")).await,
        OutboxEnqueueOutcome::Enqueued { id },
        "rendered content must not change exact identity"
    );
    assert!(matches!(
        durable_enqueue(&pool, "first rendering", 9, Some(" session ")).await,
        OutboxEnqueueOutcome::Enqueued { id: other } if other != id
    ));
    assert_eq!(
        durable_enqueue(&pool, "first rendering", 8, Some("session")).await,
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
    let first_id = enqueued_id(durable_enqueue(&pool, "answer", 8, Some("session")).await);
    let variants = [
        ("channel:5191", ProviderKind::Claude, 8),
        ("channel:5191", ProviderKind::Codex, 7),
        ("channel:5192", ProviderKind::Claude, 7),
    ];
    for (target, provider, generation) in variants {
        assert!(matches!(
            durable_enqueue_with(
                &pool, "answer", target, Some(8), Some("session"), &provider, generation, true,
            )
            .await,
            OutboxEnqueueOutcome::Enqueued { id } if id != first_id
        ));
    }
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
        let owner = 100 + index as u64;
        let id = enqueued_id(durable_enqueue(&pool, "answer", owner, Some("session")).await);
        sqlx::query("UPDATE message_outbox SET status=$1 WHERE id=$2")
            .bind(status)
            .bind(id)
            .execute(&pool)
            .await
            .expect("set active status");
        assert_eq!(
            durable_enqueue(&pool, "changed answer", owner, Some("session")).await,
            OutboxEnqueueOutcome::Enqueued { id },
            "active {status} row must be the durable handoff"
        );
    }
    for (index, status) in ["failed", "cancelled"].into_iter().enumerate() {
        let owner = 200 + index as u64;
        let id = enqueued_id(durable_enqueue(&pool, "answer", owner, Some("session")).await);
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
            durable_enqueue(&pool, "changed answer", owner, Some("session")).await,
            OutboxEnqueueOutcome::Enqueued { id: fresh } if fresh != id
        ));
    }
}

#[tokio::test]
async fn durable_concurrent_duplicates_converge_on_one_active_row_pg() {
    let Some((_pg_db, pool)) = durable_test_pool("agentdesk_headless_durable_concurrent").await
    else {
        return;
    };
    let (left, right) = tokio::join!(
        durable_enqueue(&pool, "left rendering", 5191, Some("session")),
        durable_enqueue(&pool, "right rendering", 5191, Some("session")),
    );
    assert_eq!(left, right);
}

#[tokio::test]
async fn durable_existing_sent_row_is_immediately_visible_pg() {
    let Some((_pg_db, pool)) = durable_test_pool("agentdesk_headless_durable_sent_visible").await
    else {
        return;
    };
    let id = enqueued_id(durable_enqueue(&pool, "visible answer", 5192, Some("session")).await);
    sqlx::query("UPDATE message_outbox SET status='sent', sent_at=NOW() WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .expect("mark durable row sent");
    assert_eq!(
        durable_enqueue(&pool, "re-rendered answer", 5192, Some("session")).await,
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
        (false, Some(8), Some("session"), &ProviderKind::Claude, 7),
        (true, None, Some("session"), &ProviderKind::Claude, 7),
        (
            true,
            Some(8),
            Some(over_bound.as_str()),
            &ProviderKind::Claude,
            7,
        ),
        (true, Some(8), Some("session"), &ProviderKind::Claude, 0),
        (true, Some(8), Some("session"), &unsupported, 7),
    ] {
        let enqueue = || {
            durable_enqueue_with(
                &pool,
                "same",
                "channel:5191",
                owner,
                session,
                provider,
                generation,
                enabled,
            )
        };
        assert_ne!(
            enqueue().await,
            enqueue().await,
            "legacy TTL-zero path must not suppress duplicates"
        );
    }
    let missing_session = durable_enqueue_with(
        &pool,
        "same",
        "channel:5191",
        Some(8),
        None,
        &ProviderKind::Claude,
        7,
        true,
    )
    .await;
    assert_eq!(
        missing_session,
        durable_enqueue(&pool, "different", 8, None).await,
        "the required identity tuple remains complete without an optional routing key"
    );
}
