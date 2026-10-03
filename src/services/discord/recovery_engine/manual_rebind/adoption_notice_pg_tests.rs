use super::*;
use crate::services::provider::ProviderKind;
use std::sync::{Arc, Mutex};

const NOTIFY_TOKEN: &str = "notify-6304-token";

/// Requests the mock Discord saw: `(method, path, authorization, body)`.
type Seen = Arc<Mutex<Vec<(String, String, String, serde_json::Value)>>>;

/// A fence-forward takes custody and announces it: the operator notice must land as one outbox
/// row and the worker's real drain must post it through the notify bot and settle it `sent`.
#[tokio::test]
async fn fence_forward_notice_is_enqueued_and_delivered_pg() {
    let _env_lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("root");
    let _root_env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "agentdesk_adopt_fence_notice",
        "adopt fence-forward notice outbox",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    let channel_id = 6_304_000_001_u64;
    let tmp = tempfile::tempdir().expect("tempdir");
    let transcript = tmp
        .path()
        .join("61590000-0000-4000-8000-000000000000.jsonl");
    let mut bytes = vec![b'x'; 4_095];
    bytes.push(b'\n');
    bytes.extend_from_slice(b"{\"type\":\"assistant\",\"partial\":\"UNREAD_6304\"}");
    std::fs::write(&transcript, &bytes).expect("write transcript");
    let path = transcript.to_str().expect("utf8 path");
    let tmux = "AgentDesk-claude-6304-cc";
    let mut row = inflight::InflightTurnState::new(
        ProviderKind::Claude,
        channel_id,
        None,
        crate::services::discord::tui_prompt_relay::TUI_DIRECT_SYNTHETIC_OWNER_USER_ID,
        6_304_000_002,
        6_304_000_003,
        "tui prompt".to_string(),
        None,
        Some(tmux.to_string()),
        Some(path.to_string()),
        None,
        4_096,
    );
    row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    row.turn_source = inflight::TurnSource::ExternalInput;
    row.turn_start_offset = Some(4_096);
    let facts = AdoptFenceForward {
        cause: AdoptFenceForwardCause::TurnIdentityUnknown,
        existing: &row,
        tmux_session_name: tmux,
        output_path: path,
        initial_offset: bytes.len() as u64,
        latest_lease_turn_id: None,
    };
    let custody = take_adopt_fence_forward_custody(Some(&pool), channel_id, &facts)
        .await
        .expect("custody");
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));

    announce_adopt_fence_forward(&shared, &ProviderKind::Claude, channel_id, tmux, custody);

    // The enqueue runs on a spawned task; wait for it instead of racing it.
    let target = format!("channel:{channel_id}");
    let mut rows = Vec::new();
    for _ in 0..100 {
        rows = sqlx::query_as::<_, (String, String, Option<String>, String)>(
            "SELECT source, bot, reason_code, content FROM message_outbox WHERE target = $1",
        )
        .bind(&target)
        .fetch_all(&pool)
        .await
        .expect("read outbox");
        if !rows.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(rows.len(), 1, "fence-forward notice must reach the outbox");
    let (source, bot, reason_code, content) = rows.remove(0);
    assert_eq!(source, "adopt_fence_forward_notice");
    assert_eq!(
        bot,
        crate::services::discord::bot_role::UtilityBotRole::Notify.alias()
    );
    assert_eq!(reason_code.as_deref(), Some("adopt.fence_forward"));
    assert!(content.contains("응답 이어받기 실패"), "{content}");

    let (http, seen, server) = mock_discord(channel_id).await;
    let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
    registry
        .set_utility_bot_http_for_tests(
            crate::services::discord::bot_role::UtilityBotRole::Notify,
            http,
        )
        .await;
    let pg = Arc::new(pool.clone());
    let drained =
        crate::server::outbox_worker::drain_message_outbox_once(&pg, &registry, "adopt-6304-test")
            .await;
    assert_eq!(drained, 1, "the worker claims the notice");

    let seen = seen
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .clone();
    let posts: Vec<_> = seen
        .into_iter()
        .filter(|(method, ..)| method == "POST")
        .collect();
    assert_eq!(posts.len(), 1, "one Discord post: {posts:?}");
    let (_, post_path, authorization, body) = &posts[0];
    assert_eq!(
        post_path,
        &format!("/api/v10/channels/{channel_id}/messages")
    );
    assert_eq!(
        authorization,
        &format!("Bot {NOTIFY_TOKEN}"),
        "posted by the notify bot"
    );
    let posted = body["content"].as_str().unwrap_or_default();
    assert!(posted.contains("응답 이어받기 실패"), "{posted}");
    let settled = sqlx::query_as::<_, (String, String)>(
        "SELECT source, status FROM message_outbox WHERE target = $1",
    )
    .bind(&target)
    .fetch_all(&pool)
    .await
    .expect("read settled outbox");
    assert_eq!(
        settled,
        vec![("adopt_fence_forward_notice".to_string(), "sent".to_string())]
    );
    server.abort();
    pool.close().await;
    db.drop().await;
}

/// A Discord stand-in: the channel lookup the send gate authorizes with, and message creation.
async fn mock_discord(
    channel_id: u64,
) -> (
    Arc<poise::serenity_prelude::Http>,
    Seen,
    tokio::task::JoinHandle<()>,
) {
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use axum::response::IntoResponse;
    let seen: Seen = Arc::default();
    let record = seen.clone();
    let app = axum::Router::new().fallback(move |request: Request<Body>| {
        let record = record.clone();
        async move {
            let method = request.method().clone();
            let path = request.uri().path().to_string();
            let authorization = (request.headers().get("authorization"))
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default()
                .to_string();
            let body = axum::body::to_bytes(request.into_body(), 1024 * 1024)
                .await
                .unwrap_or_default();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let content = body["content"].as_str().unwrap_or_default().to_string();
            record
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push((method.to_string(), path.clone(), authorization, body));
            let channel = format!("/api/v10/channels/{channel_id}");
            if method == Method::GET && path == channel {
                axum::Json(serde_json::json!({
                    "id": channel_id.to_string(), "type": 0, "name": "ops-6304",
                    "guild_id": "6304000000", "position": 0, "permission_overwrites": [],
                    "nsfw": false, "parent_id": null
                }))
                .into_response()
            } else if method == Method::POST && path == format!("{channel}/messages") {
                axum::Json(message_json(channel_id, &content)).into_response()
            } else {
                StatusCode::NOT_FOUND.into_response()
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock Discord");
    let proxy = format!("http://{}", listener.local_addr().expect("mock address"));
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve mock Discord");
    });
    let http = poise::serenity_prelude::HttpBuilder::new(NOTIFY_TOKEN)
        .proxy(proxy)
        .ratelimiter_disabled(true)
        .build();
    (Arc::new(http), seen, server)
}

fn message_json(channel_id: u64, content: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "6304000099", "channel_id": channel_id.to_string(),
        "author": {
            "id": "6304000098", "username": "notify-6304", "discriminator": "0001",
            "global_name": null, "avatar": null, "bot": true, "system": false,
            "public_flags": 0
        },
        "content": content, "timestamp": "2026-10-04T00:00:00.000000+00:00",
        "edited_timestamp": null, "tts": false, "mention_everyone": false, "mentions": [],
        "mention_roles": [], "attachments": [], "embeds": [], "nonce": null, "pinned": false,
        "type": 0, "flags": 0
    })
}
