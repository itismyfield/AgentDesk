//! The gate itself keeps a message under a Decision forum out of agent intake: such a message
//! leaves no dedup entry, while the same message under any other parent goes on into intake.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use poise::serenity_prelude as serenity;
use serde_json::json;

use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::decision_forums::{ForumState, store};
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::discord::{Data, router};
use crate::services::provider::ProviderKind;

const GUILD: u64 = 96_326_000_999;
const FORUM: u64 = 940_821_100_000_001;
const ELSEWHERE: u64 = 940_821_100_000_002;
const UNDER_FORUM: u64 = 940_821_100_000_003;
const UNDER_ELSEWHERE: u64 = 940_821_100_000_004;

/// Both test channels are threads that name their parent; every other channel is plain text.
async fn discord_http(request: Request<Body>) -> Response {
    let path = request.uri().path();
    let id = (path.strip_prefix("/api/v10/channels/")).and_then(|id| id.parse::<u64>().ok());
    let Some(id) = id.filter(|_| request.method() == Method::GET) else {
        let unknown = json!({"message": "unexpected fixture endpoint", "code": 0});
        return (StatusCode::NOT_FOUND, axum::Json(unknown)).into_response();
    };
    let parent = match id {
        UNDER_FORUM => Some(FORUM),
        UNDER_ELSEWHERE => Some(ELSEWHERE),
        _ => None,
    };
    let channel = match parent {
        Some(parent) => json!({"id": id.to_string(), "type": 11, "guild_id": GUILD.to_string(),
            "name": "thread", "parent_id": parent.to_string(), "owner_id": "1",
            "message_count": 0, "member_count": 1,
            "thread_metadata": {"archived": false, "locked": false,
                "auto_archive_duration": 1440, "archive_timestamp": "2026-10-12T00:00:00Z"}}),
        None => json!({"id": id.to_string(), "type": 0, "guild_id": GUILD.to_string(),
            "name": "parent", "position": 0, "permission_overwrites": [], "nsfw": false,
            "parent_id": null}),
    };
    axum::Json(channel).into_response()
}

#[tokio::test]
async fn decision_forum_message_stops_at_the_gate_before_agent_intake_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let ready = ForumState::Ready(FORUM.to_string());
    store(&pool, GUILD, 1, "decision", &ready).await.unwrap();
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let (_, gateway_url, gateway) =
        discord_mock::start(discord_mock::DiscordMockState::new()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().fallback(discord_http);
    let http = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let ctx = discord_mock::serenity_context(proxy, gateway_url).await;
    let data = Arc::new(Data {
        shared: shared.clone(),
        token: "test-token".into(),
        provider: ProviderKind::Claude,
    });
    let mut reached = Vec::new();
    for (thread, id) in [
        (UNDER_FORUM, 940_821_100_000_011_u64),
        (UNDER_ELSEWHERE, 940_821_100_000_012),
    ] {
        let mut message = discord_mock::user_message(id, "operator reply");
        message.channel_id = serenity::ChannelId::new(thread);
        message.guild_id = Some(serenity::GuildId::new(GUILD));
        let event = serenity::FullEvent::Message {
            new_message: message,
        };
        let (task_ctx, task_data) = (ctx.clone(), data.clone());
        let mut handled = tokio::spawn(async move {
            let _ = router::handle_event(&task_ctx, &event, &task_data).await;
        });
        if tokio::time::timeout(Duration::from_secs(10), &mut handled)
            .await
            .is_err()
        {
            handled.abort();
        }
        let entered = shared
            .dispatch
            .intake_dedup
            .contains_key(&format!("mid:{id}"));
        reached.push(entered);
    }
    assert_eq!(
        reached,
        [false, true],
        "under the forum: no intake; under another parent: intake"
    );
    http.abort();
    gateway.abort();
    pool.close().await;
    db.drop().await;
}
