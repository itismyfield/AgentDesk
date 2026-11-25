//! The gate keeps a message under a Decision forum, its parent read over REST or else from the
//! gateway cache, out of intake (no dedup entry); under any other parent it goes on into intake.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use poise::serenity_prelude as serenity;
use serde_json::json;

use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::decision_forums::{ForumState, store};
use crate::db::decisions::publication::Journal;
use crate::services::discord::decisions;
use crate::services::discord::tui_prompt_relay::relay_e2e::discord_mock;
use crate::services::discord::{Data, router};
use crate::services::provider::ProviderKind;

const GUILD: u64 = 96_326_000_999;
const FORUM: u64 = 940_821_100_000_001;
const ELSEWHERE: u64 = 940_821_100_000_002;
const UNDER_FORUM: u64 = 940_821_100_000_003;
const UNDER_ELSEWHERE: u64 = 940_821_100_000_004;
const CACHED_UNDER_FORUM: u64 = 940_821_100_000_005;
const CACHED_UNDER_ELSEWHERE: u64 = 940_821_100_000_006;
/// A forum this process never configured nor registered; only a journaled request names it.
const JOURNALED: u64 = 940_821_100_000_007;
const UNDER_JOURNALED: u64 = 940_821_100_000_008;

/// Two threads name their parent over REST, the cached ones cannot be read, the rest are text.
async fn discord_http(request: Request<Body>) -> Response {
    let path = request.uri().path();
    let id = (path.strip_prefix("/api/v10/channels/")).and_then(|id| id.parse::<u64>().ok());
    let Some(id) = id.filter(|_| request.method() == Method::GET) else {
        let unknown = json!({"message": "unexpected fixture endpoint", "code": 0});
        return (StatusCode::NOT_FOUND, axum::Json(unknown)).into_response();
    };
    if matches!(id, CACHED_UNDER_FORUM | CACHED_UNDER_ELSEWHERE) {
        let down = json!({"message": "unreadable fixture channel", "code": 0});
        return (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(down)).into_response();
    }
    let parent = match id {
        UNDER_FORUM => Some(FORUM),
        UNDER_ELSEWHERE => Some(ELSEWHERE),
        UNDER_JOURNALED => Some(JOURNALED),
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

/// The gateway cache holds the guild with the two threads REST cannot read.
fn cache_threads(ctx: &serenity::Context) {
    let mut guild = serde_json::to_value(serenity::Guild::default()).unwrap();
    guild["id"] = json!(GUILD.to_string());
    let thread = |id: u64, parent: u64| {
        json!({"id": id.to_string(), "type": 11,
        "guild_id": GUILD.to_string(), "name": "thread", "parent_id": parent.to_string()})
    };
    guild["threads"] = json!([
        thread(CACHED_UNDER_FORUM, FORUM),
        thread(CACHED_UNDER_ELSEWHERE, ELSEWHERE)
    ]);
    let mut event: serenity::GuildCreateEvent = serde_json::from_value(guild).unwrap();
    ctx.cache.update(&mut event);
}

/// Whether the message got past the guard into intake's dedup. A message the guard keeps must
/// leave the handler cleanly and in time, so a crash or a hang never reads as kept out.
async fn reaches_intake(ctx: &serenity::Context, data: &Arc<Data>, thread: u64, id: u64) -> bool {
    let mut message = discord_mock::user_message(id, "operator reply");
    message.channel_id = serenity::ChannelId::new(thread);
    message.guild_id = Some(serenity::GuildId::new(GUILD));
    let event = serenity::FullEvent::Message {
        new_message: message,
    };
    let (task_ctx, task_data) = (ctx.clone(), data.clone());
    let mut handled =
        tokio::spawn(async move { router::handle_event(&task_ctx, &event, &task_data).await });
    let finished = tokio::time::timeout(Duration::from_secs(10), &mut handled).await;
    let key = format!("mid:{id}");
    let entered = data.shared.dispatch.intake_dedup.contains_key(&key);
    if !entered {
        let joined = finished.expect("a kept message leaves the gate within 10s");
        joined
            .expect("the gate task panicked")
            .expect("the gate failed");
    } else if finished.is_err() {
        handled.abort();
    }
    entered
}

#[tokio::test]
async fn decision_forum_message_stops_at_the_gate_before_agent_intake_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let ready = ForumState::Ready(FORUM.to_string());
    store(&pool, GUILD, 1, "decision", &ready).await.unwrap();
    let body = json!({"agent_id": "a", "target": {"channel_id": "1"}, "ref": "r", "title": "t",
        "options": [{"label": "x"}, {"label": "y"}]});
    let ask: decisions::Ask = serde_json::from_value(body).unwrap();
    let (request, _) = decisions::store(&pool, &ask, (1, 1), "c").await.unwrap();
    let journal = Journal {
        provider: "claude",
        bot_user_id: 1,
        forum_id: JOURNALED,
        handles: &[],
    };
    assert!(
        crate::db::decisions::publication::journal(&pool, request, &journal)
            .await
            .unwrap()
    );
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
    cache_threads(&ctx);
    let mut reached = Vec::new();
    for (thread, id) in [
        (UNDER_FORUM, 940_821_100_000_011_u64),
        (UNDER_ELSEWHERE, 940_821_100_000_012),
        (CACHED_UNDER_FORUM, 940_821_100_000_013),
        (CACHED_UNDER_ELSEWHERE, 940_821_100_000_014),
        (UNDER_JOURNALED, 940_821_100_000_015),
    ] {
        reached.push(reaches_intake(&ctx, &data, thread, id).await);
    }
    assert_eq!(
        reached,
        [false, true, false, true, false],
        "under a Decision forum (REST or cached parent, or only journaled): no intake"
    );
    http.abort();
    gateway.abort();
    pool.close().await;
    db.drop().await;
}
