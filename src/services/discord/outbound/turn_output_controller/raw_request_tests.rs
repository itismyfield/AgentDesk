use super::*;
use crate::services::cluster::intake_router_hook::owner_record::output;
use crate::services::discord::gateway::DiscordGateway;
use crate::services::provider::ProviderKind;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn output_origin_real_gateway_seventy_paced_chunks_commit_pg() {
    let (_db, pool, _origin) = output::tests::fixture("7336950").await;
    let posts = Arc::new(AtomicUsize::new(0));
    let recorded = posts.clone();
    let bytes = Arc::new(AtomicUsize::new(0));
    let received = bytes.clone();
    let checking_pool = pool.clone();
    let app = axum::Router::new().route("/api/v10/{*path}", axum::routing::any(move |method: axum::http::Method, uri: axum::http::Uri, body: axum::body::Bytes| {
        let checking_pool = checking_pool.clone();
        let received = received.clone();
        let n = if method == axum::http::Method::POST { recorded.fetch_add(1, Ordering::SeqCst) + 1 } else { 0 };
        async move {
            if method == axum::http::Method::GET {
                eprintln!("ancillary read {}", uri.path());
                let response = if uri.path().ends_with("/messages") { serde_json::json!([]) }
                    else { serde_json::json!({"id":"7336950", "type":0, "name":"fixture", "username":"bot", "discriminator":"0001", "avatar":null, "bot":true}) };
                return (axum::http::StatusCode::OK, axum::Json(response));
            }
            if method == axum::http::Method::DELETE {
                return (axum::http::StatusCode::NO_CONTENT, axum::Json(serde_json::Value::Null));
            }
            assert_eq!(method, axum::http::Method::POST);
            let mut tx = checking_pool.begin().await.unwrap();
            let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
                .bind(crate::services::cluster::intake_router_hook::owner_record::OwnerIdentity::new("claude", "7336950").advisory_key())
                .fetch_one(&mut *tx).await.unwrap();
            assert!(!free, "the actual raw HTTP future must own the PG guard");
            tx.rollback().await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            received.fetch_add(body["content"].as_str().unwrap().len(), Ordering::SeqCst);
            (axum::http::StatusCode::OK, axum::Json(serde_json::json!({
                "id": (1000+n).to_string(), "channel_id":"7336950", "content":body["content"],
                "author":{"id":"1","username":"bot","discriminator":"0001","avatar":null},
                "timestamp":"2026-10-28T00:00:00+00:00","edited_timestamp":null,
                "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],
                "attachments":[],"embeds":[],"pinned":false,"type":0
            })))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        poise::serenity_prelude::HttpBuilder::new("test-token")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build(),
    );
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let gateway: Arc<dyn TurnGateway> = Arc::new(DiscordGateway::new(
        http.clone(),
        shared.clone(),
        ProviderKind::Claude,
        None,
    ));
    let bridge =
        crate::services::discord::tui_prompt_relay::bridge_gateway::terminal_notice_test_gateway(
            http, shared,
        );
    let channel = ChannelId::new(7336950);
    for (kind, gateway) in [("discord", gateway), ("bridge", bridge)] {
        for chunks in [1, 70] {
            let body = if chunks == 70 {
                "x".repeat(139300)
            } else {
                "one chunk".into()
            };
            assert_eq!(
                crate::services::discord::formatting::split_message(&body).len(),
                chunks
            );
            let lease = DeliveryLeaseCell::new(channel);
            let controller = PlaceholderController::default();
            let advanced = AtomicUsize::new(0);
            let advance = |_| {
                advanced.fetch_add(1, Ordering::SeqCst);
                true
            };
            let started = tokio::time::Instant::now();
            let turn = TurnKey::new(channel, 90001, chunks as u64);
            let outcome = deliver_turn_output(
                gateway.as_ref(),
                TurnOutputCtx {
                    turn,
                    lease_key: Some(DeliveryLeaseKey::from_turn_key(turn)),
                    owner: RelayOwnerKind::Watcher,
                    holder: LeaseHolder::Watcher { instance_id: 1 },
                    lease: &lease,
                    channel_id: channel,
                    placeholder_controller: &controller,
                    placeholder: PlaceholderSlot::None,
                    body: &body,
                    send_range: (0, body.len() as u64),
                    plan: OutputPlan::SendNewChunks {
                        chunk_count: chunks,
                        delete_anchor: false,
                    },
                    edit_fail_policy: EditFailPlaceholderPolicy::DeleteIfProvenStale,
                    fallback_commit_policy: FallbackCommitPolicy::CommitOnFallback,
                    acquire_failure_mode: AcquireFailureMode::Transient,
                    advance: Some(&advance),
                    heartbeat: None,
                    body_claim: None,
                },
            )
            .await;
            eprintln!(
                "paced case {kind} chunks={chunks} posts={} elapsed={:?} delivered={}",
                posts.load(Ordering::SeqCst),
                started.elapsed(),
                matches!(outcome, DeliveryOutcome::Delivered { .. })
            );
            assert!(
                matches!(outcome, DeliveryOutcome::Delivered { committed_to, .. } if committed_to == body.len() as u64),
                "all raw chunks must reach terminal commit"
            );
            assert_eq!(
                advanced.load(Ordering::SeqCst),
                1,
                "terminal commit must advance once"
            );
            if chunks == 70 {
                assert!(
                    started.elapsed() >= std::time::Duration::from_secs(69),
                    "production pacing must run"
                );
            }
        }
    }
    assert_eq!(posts.load(Ordering::SeqCst), 142);
    assert_eq!(
        bytes.load(Ordering::SeqCst),
        2 * (139300 + "one chunk".len())
    );
    task.abort();
}

#[tokio::test]
async fn output_origin_real_gateway_rechecks_after_pacing_and_blocks_rollback_pg() {
    let (_db, pool, origin) = output::tests::fixture("7336951").await;
    let methods = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = methods.clone();
    let (sent, first_post) = tokio::sync::oneshot::channel();
    let sent = Arc::new(std::sync::Mutex::new(Some(sent)));
    let app = axum::Router::new().route(
        "/api/v10/{*path}",
        axum::routing::any(move |method: axum::http::Method, body: axum::body::Bytes| {
            recorded.lock().unwrap().push(method.to_string());
            if let Some(sent) = sent.lock().unwrap().take() {
                sent.send(()).unwrap();
            }
            async move {
                if method == axum::http::Method::DELETE {
                    return (
                        axum::http::StatusCode::NO_CONTENT,
                        axum::Json(serde_json::Value::Null),
                    );
                }
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                (
                    axum::http::StatusCode::OK,
                    axum::Json(serde_json::json!({
                        "id":"2001", "channel_id":"7336951", "content":body["content"],
                        "author":{"id":"1","username":"bot","discriminator":"0001","avatar":null},
                        "timestamp":"2026-10-28T00:00:00+00:00","edited_timestamp":null,
                        "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],
                        "attachments":[],"embeds":[],"pinned":false,"type":0
                    })),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        poise::serenity_prelude::HttpBuilder::new("test-token")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build(),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let gateway = DiscordGateway::new(http, shared, ProviderKind::Claude, None);
    let channel = ChannelId::new(7336951);
    let body = "x".repeat(5000);
    let artifact = origin.seal(&body);
    let revoked_pool = pool.clone();
    let revocation = tokio::spawn(async move {
        first_post.await.unwrap();
        output::tests::revoke(&revoked_pool, &origin).await;
    });
    let guarded = super::transport::OriginGateway {
        inner: &gateway,
        scope: Some(output::Scope {
            pool: pool.clone(),
            provider: "claude".into(),
        }),
        origin: Some(&artifact),
        body: &body,
    };
    assert!(
        guarded
            .send_long_message_with_rollback(channel, MessageId::new(1), &body)
            .await
            .is_err()
    );
    revocation.await.unwrap();
    assert_eq!(
        *methods.lock().unwrap(),
        ["POST"],
        "next POST and rollback DELETE must recheck generation"
    );
    assert!(
        guarded
            .edit_message(channel, MessageId::new(2001), "answer")
            .await
            .is_err()
    );
    assert!(
        guarded
            .delete_message(channel, MessageId::new(2001))
            .await
            .is_err()
    );
    assert_eq!(*methods.lock().unwrap(), ["POST"]);
    server.abort();
}

#[tokio::test]
async fn output_origin_real_gateway_fallback_rollback_and_edit_raw_guards_pg() {
    let (_db, pool, origin) = output::tests::fixture("7336954").await;
    let methods = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = methods.clone();
    let posts = Arc::new(AtomicUsize::new(0));
    let checking_pool = pool.clone();
    let app = axum::Router::new().route("/api/v10/{*path}", axum::routing::any(move |method: axum::http::Method, uri: axum::http::Uri, body: axum::body::Bytes| {
        recorded.lock().unwrap().push(method.to_string());
        let checking_pool = checking_pool.clone();
        let posts = posts.clone();
        async move {
            let mut tx = checking_pool.begin().await.unwrap();
            let free: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
                .bind(crate::services::cluster::intake_router_hook::owner_record::OwnerIdentity::new("claude", "7336954").advisory_key())
                .fetch_one(&mut *tx).await.unwrap();
            assert!(!free, "fallback, rollback and retry must hold the owner key during HTTP");
            tx.rollback().await.unwrap();
            if method == axum::http::Method::DELETE {
                return (axum::http::StatusCode::NO_CONTENT, axum::Json(serde_json::Value::Null));
            }
            let n = if method == axum::http::Method::POST { posts.fetch_add(1, Ordering::SeqCst) + 1 } else { 0 };
            if (method == axum::http::Method::PATCH && uri.path().ends_with("/1")) || n == 3 {
                return (axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(serde_json::json!({"code":50035,"message":"fixture request rejected"})));
            }
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let id = if method == axum::http::Method::PATCH { uri.path().rsplit('/').next().unwrap().to_owned() } else { (3000+n).to_string() };
            (axum::http::StatusCode::OK, axum::Json(serde_json::json!({
                "id": id, "channel_id":"7336954", "content":body["content"],
                "author":{"id":"1","username":"bot","discriminator":"0001","avatar":null},
                "timestamp":"2026-10-28T00:00:00+00:00","edited_timestamp":null,
                "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],
                "attachments":[],"embeds":[],"pinned":false,"type":0
            })))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let http = Arc::new(
        poise::serenity_prelude::HttpBuilder::new("test-token")
            .proxy(format!("http://{}", listener.local_addr().unwrap()))
            .ratelimiter_disabled(true)
            .build(),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let gateway = DiscordGateway::new(http, shared, ProviderKind::Claude, None);
    let channel = ChannelId::new(7336954);
    let artifact = origin.seal("answer");
    let scope = output::Scope {
        pool,
        provider: "claude".into(),
    };
    let guarded = super::transport::OriginGateway {
        inner: &gateway,
        scope: Some(scope.clone()),
        origin: Some(&artifact),
        body: "answer",
    };
    assert!(matches!(guarded.replace_message_with_outcome(channel, MessageId::new(1), "answer").await.unwrap(),
        crate::services::discord::formatting::ReplaceLongMessageOutcome::SentFallbackAfterEditFailure { .. }));
    guarded
        .delete_message(channel, MessageId::new(3001))
        .await
        .unwrap();
    let body = "x".repeat(3980);
    let artifact = origin.seal(&body);
    let guarded = super::transport::OriginGateway {
        inner: &gateway,
        scope: Some(scope),
        origin: Some(&artifact),
        body: &body,
    };
    assert!(
        guarded
            .send_long_message_with_rollback(channel, MessageId::new(9), &body)
            .await
            .is_err()
    );
    guarded
        .edit_message(channel, MessageId::new(3002), "edited")
        .await
        .unwrap();
    assert_eq!(
        *methods.lock().unwrap(),
        ["PATCH", "POST", "DELETE", "POST", "POST", "DELETE", "PATCH"]
    );
    server.abort();
}
