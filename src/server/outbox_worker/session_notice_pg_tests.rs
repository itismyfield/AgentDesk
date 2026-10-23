use super::drain_message_outbox_once;
use crate::dispatch::test_support::DispatchPostgresTestDb;
use crate::services::discord::bot_role::UtilityBotRole;
use crate::services::discord::health::HealthRegistry;
use crate::services::message_outbox::{self, OutboxMessage};
use poise::serenity_prelude::{Http, HttpBuilder};
use sqlx::{PgPool, Row};
use std::sync::{Arc, Mutex};

const CHANNEL: u64 = 72_280_700;
const TARGET: &str = "channel:72280700";
const SESSION: &str = "fixture-host:AgentDesk-claude-notify-inline-cc";
const ORIGINAL: &str = "♻️ 이전 대화 이어가기 실패\n사유: **new** reason\n복구: retained";
const REASON: &str = "lifecycle.session_resume_failed";
const NOTIFY: &str = "notify-inline-test-token";
const PROVIDER: &str = "provider-inline-test-token";

type Seen = Arc<Mutex<Vec<(String, String, serde_json::Value)>>>;

struct Isolation {
    _guards: [crate::config::TestEnvVarGuard; 2],
    _live: crate::config_live_reload::TestLiveGuard,
    _root: tempfile::TempDir,
}

impl Isolation {
    fn new() -> Self {
        let live = crate::config_live_reload::test_live_guard(
            crate::config::test_env_lock::acquire_shared_test_env_lock(),
        );
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.yaml");
        std::fs::write(&config, "server: {}\n").unwrap();
        let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
        let guards = [
            set("AGENTDESK_ROOT_DIR", root.path()),
            set("AGENTDESK_CONFIG", &config),
        ];
        crate::config_live_reload::install(crate::config::Config::default());
        Self {
            _guards: guards,
            _live: live,
            _root: root,
        }
    }
}

struct Discord {
    notify: Arc<Http>,
    provider: Arc<Http>,
    seen: Seen,
    task: tokio::task::AbortHandle,
}

impl Discord {
    async fn new() -> Self {
        use axum::body::Body;
        use axum::http::{Method, Request, StatusCode};
        use axum::response::IntoResponse;
        let seen = Seen::default();
        let recorded = seen.clone();
        let app = axum::Router::new().fallback(move |request: Request<Body>| {
            let recorded = recorded.clone();
            async move {
                let method = request.method().clone();
                let path = request.uri().path().to_string();
                let authorization = request.headers().get("authorization")
                    .and_then(|value| value.to_str().ok()).unwrap_or_default().to_string();
                let body = axum::body::to_bytes(request.into_body(), 1024 * 1024).await.unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
                recorded.lock().unwrap().push((method.to_string(), authorization, body.clone()));
                let channel = format!("/api/v10/channels/{CHANNEL}");
                if method == Method::GET && path == channel {
                    axum::Json(serde_json::json!({
                        "id": CHANNEL.to_string(), "type": 0, "name": "notify-inline-fixture",
                        "guild_id": "72280701", "position": 0, "permission_overwrites": [],
                        "nsfw": false, "parent_id": null
                    })).into_response()
                } else if method == Method::POST && path == format!("{channel}/messages") {
                    axum::Json(serde_json::json!({
                        "id": "72280799", "channel_id": CHANNEL.to_string(),
                        "author": { "id":"72280798", "username":"fixture", "discriminator":"0001", "avatar":null, "bot":true },
                        "content": body["content"], "timestamp":"2026-10-23T00:00:00+00:00",
                        "edited_timestamp":null, "tts":false, "mention_everyone":false,
                        "mentions":[], "mention_roles":[], "attachments":[], "embeds":[], "pinned":false, "type":0
                    })).into_response()
                } else { StatusCode::NOT_FOUND.into_response() }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        let build = |token| {
            Arc::new(
                HttpBuilder::new(token)
                    .proxy(proxy.clone())
                    .ratelimiter_disabled(true)
                    .build(),
            )
        };
        let (notify, provider) = (build(NOTIFY), build(PROVIDER));
        let task =
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }).abort_handle();
        Self {
            notify,
            provider,
            seen,
            task,
        }
    }

    fn posts(&self) -> Vec<(String, serde_json::Value)> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, ..)| method == "POST")
            .map(|(_, auth, body)| (auth.clone(), body.clone()))
            .collect()
    }

    async fn registry(&self) -> Arc<HealthRegistry> {
        let registry = Arc::new(HealthRegistry::new());
        registry
            .set_utility_bot_http_for_tests(UtilityBotRole::Notify, self.notify.clone())
            .await;
        registry
    }
}

impl Drop for Discord {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn database(label: &str) -> (DispatchPostgresTestDb, Arc<PgPool>) {
    let db = DispatchPostgresTestDb::try_create("adk_outbox_inline", label)
        .await
        .expect("session notice contracts require real isolated PostgreSQL; never skip");
    let pool = Arc::new(db.connect_and_migrate().await);
    (db, pool)
}

async fn designation(pool: &PgPool, id: i64) -> Option<serde_json::Value> {
    sqlx::query_scalar("SELECT session_notice FROM message_outbox WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn assert_inline(content: &str) {
    assert!(
        content.starts_with("-# "),
        "designated session notice must use provider subtext, not the old header: {content:?}"
    );
    assert_eq!(
        content.lines().count(),
        1,
        "raw multiline must never escape the session notice display contract: {content:?}"
    );
    assert!(
        !content.chars().any(char::is_control),
        "session notice must strip control characters: {content:?}"
    );
    assert!(
        !content.contains(['\u{2028}', '\u{2029}']),
        "session notice must strip Unicode line separators"
    );
    assert!(
        !content.contains("**"),
        "session notice must escape header markdown: {content:?}"
    );
    assert!(content.encode_utf16().count() <= 2_000);
}

#[tokio::test]
async fn eligible_lifecycle_notice_pins_provider_display_and_deduplicates_after_restart_pg() {
    let _isolation = Isolation::new();
    let (db, pool) = database("enqueue-time provider display and restart sentinel").await;
    let discord = Discord::new().await;
    let registry = discord.registry().await;
    let _provider_rest = crate::services::discord::session_notice_delivery::register_test_provider(
        &registry,
        "claude",
        CHANNEL,
        discord.provider.clone(),
        pool.as_ref().clone(),
    )
    .await;
    let resolved = crate::services::discord::session_notice_delivery::channel_provider(
        &registry,
        poise::serenity_prelude::ChannelId::new(CHANNEL),
        None,
    )
    .await
    .expect("unhinted lookup must resolve the actual channel owner");
    assert_eq!(resolved.as_str(), "claude");
    let channel_http = crate::services::discord::session_notice_delivery::channel_http(
        &registry,
        poise::serenity_prelude::ChannelId::new(CHANNEL),
    )
    .await
    .expect("direct notice HTTP lookup must reuse the channel owner contract");
    assert!(Arc::ptr_eq(&channel_http, &discord.provider));
    assert!(
        crate::services::discord::session_notice_delivery::channel_provider(
            &registry,
            poise::serenity_prelude::ChannelId::new(CHANNEL + 1),
            None,
        )
        .await
        .is_err(),
        "one registered provider must not claim an unrelated unbound channel"
    );
    {
        let ambiguous = discord.registry().await;
        let _claude_rest =
            crate::services::discord::session_notice_delivery::register_test_provider(
                &ambiguous,
                "claude",
                CHANNEL,
                discord.provider.clone(),
                pool.as_ref().clone(),
            )
            .await;
        let _codex_rest =
            crate::services::discord::session_notice_delivery::register_test_provider(
                &ambiguous,
                "codex",
                CHANNEL,
                discord.provider.clone(),
                pool.as_ref().clone(),
            )
            .await;
        assert!(
            crate::services::discord::session_notice_delivery::channel_http(
                &ambiguous,
                poise::serenity_prelude::ChannelId::new(CHANNEL),
            )
            .await
            .is_err(),
            "direct notice lookup must defer when two providers own the channel"
        );
    }
    assert!(
        message_outbox::enqueue_lifecycle_notification_pg(
            &pool,
            TARGET,
            Some(SESSION),
            REASON,
            ORIGINAL
        )
        .await
        .unwrap()
    );
    assert!(
        !message_outbox::enqueue_lifecycle_notification_pg(
            &pool,
            TARGET,
            Some(SESSION),
            REASON,
            ORIGINAL
        )
        .await
        .unwrap()
    );
    let row = sqlx::query("SELECT id,bot,content,dedupe_key FROM message_outbox")
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    let id: i64 = row.get("id");
    assert_eq!(
        row.get::<String, _>("bot"),
        "notify",
        "display metadata must not rewrite row bot/dedupe identity"
    );
    assert_eq!(row.get::<String, _>("content"), ORIGINAL);
    let dedupe: Option<String> = row.get("dedupe_key");
    assert!(dedupe.is_some());
    let display = designation(&pool, id)
        .await
        .expect("eligible producer must freeze display metadata when its row is created");
    assert_eq!(display["version"], 1);
    assert_eq!(display["provider_hint"], "claude");
    assert_inline(display["content"].as_str().unwrap());
    assert!(display["content"].as_str().unwrap().contains("(+2줄)"));
    assert_eq!(
        drain_message_outbox_once(&pool, &registry, "inline-first-worker").await,
        1
    );
    let posts = discord.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].0,
        format!("Bot {PROVIDER}"),
        "designated notice must post with the owning provider, never notify"
    );
    assert_eq!(posts[0].1["content"], display["content"]);
    assert_inline(posts[0].1["content"].as_str().unwrap());
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM message_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(pool.as_ref())
            .await
            .unwrap(),
        "sent"
    );
    let reopened = Arc::new(db.connect_and_migrate().await);
    assert!(
        !message_outbox::enqueue_lifecycle_notification_pg(
            &reopened,
            TARGET,
            Some(SESSION),
            REASON,
            ORIGINAL
        )
        .await
        .unwrap()
    );
    assert_eq!(
        drain_message_outbox_once(&reopened, &registry, "inline-restarted-worker").await,
        0
    );
    assert_eq!(
        discord.posts().len(),
        1,
        "a sent row survives normal pool/worker restart without another post"
    );
    assert_eq!(designation(&reopened, id).await, Some(display));
    let stall = message_outbox::enqueue_outbox_pg_returning_id(
        &pool,
        OutboxMessage {
            target: TARGET,
            content: "⚠️ 스톨 의심: 요청자 <@343742347365974026>",
            bot: "notify",
            source: "stall_watchdog",
            reason_code: Some("stall_watchdog_suspected_stall"),
            session_key: Some(SESSION),
        },
    )
    .await
    .unwrap()
    .expect("stall fixture must enqueue");
    assert_eq!(
        drain_message_outbox_once(&pool, &registry, "inline-stall-worker").await,
        1
    );
    let posts = discord.posts();
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[1].0, format!("Bot {PROVIDER}"));
    let content = posts[1].1["content"].as_str().unwrap();
    assert_inline(content);
    assert!(
        content.contains("<@343742347365974026>"),
        "stall provider POST must preserve the requester user mention"
    );
    assert!(
        !content.contains("‹@"),
        "stall provider POST must not escape the requester user mention"
    );
    assert_eq!(
        posts[1].1["content"],
        designation(&pool, stall).await.unwrap()["content"]
    );
    reopened.close().await;
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn unmarked_legacy_pending_and_stale_processing_keep_notify_and_original_payload_pg() {
    let _isolation = Isolation::new();
    let (db, pool) = database("unmarked legacy rows retain frozen old display").await;
    let discord = Discord::new().await;
    let registry = discord.registry().await;
    let _provider_rest = crate::services::discord::session_notice_delivery::register_test_provider(
        &registry,
        "claude",
        CHANNEL,
        discord.provider.clone(),
        pool.as_ref().clone(),
    )
    .await;
    for (state, session) in [
        ("pending", "legacy-pending"),
        ("processing", "legacy-stale"),
    ] {
        let mut tx = pool.begin().await.unwrap();
        let id = message_outbox::enqueue_outbox_pg_on_tx(
            &mut tx,
            OutboxMessage {
                target: TARGET,
                content: ORIGINAL,
                bot: "notify",
                source: "lifecycle_notifier",
                reason_code: Some(REASON),
                session_key: Some(session),
            },
        )
        .await
        .unwrap();
        sqlx::query(
            "UPDATE message_outbox SET session_notice=NULL,status=$2,
             claim_owner='dead-worker',claimed_at=NOW()-INTERVAL '1 day' WHERE id=$1",
        )
        .bind(id)
        .bind(state)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert!(
            designation(&pool, id).await.is_none(),
            "legacy fixture must have SQL NULL display metadata"
        );
    }
    assert_eq!(
        drain_message_outbox_once(&pool, &registry, "legacy-restarted-worker").await,
        2
    );
    let posts = discord.posts();
    assert_eq!(posts.len(), 2);
    for (authorization, body) in posts {
        assert_eq!(
            authorization,
            format!("Bot {NOTIFY}"),
            "legacy must retain notify and original payload even when a provider owner is available"
        );
        assert_eq!(
            body["content"], ORIGINAL,
            "legacy must retain notify and original payload; no delivery-time reformatting"
        );
    }
    let sent: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM message_outbox WHERE status='sent' AND session_notice IS NULL",
    )
    .fetch_one(pool.as_ref())
    .await
    .unwrap();
    assert_eq!(sent, 2);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn designated_unknown_owner_retries_without_notify_fallback_pg() {
    let _isolation = Isolation::new();
    let (db, pool) = database("unknown provider display owner retries without notify").await;
    let discord = Discord::new().await;
    let registry = discord.registry().await;
    assert!(
        message_outbox::enqueue_lifecycle_notification_pg(
            &pool,
            TARGET,
            Some("unknown-owner-session"),
            REASON,
            ORIGINAL
        )
        .await
        .unwrap()
    );
    let id: i64 = sqlx::query_scalar("SELECT id FROM message_outbox")
        .fetch_one(pool.as_ref())
        .await
        .unwrap();
    let display = designation(&pool, id)
        .await
        .expect("eligible unknown-owner row must still retain its provider display designation");
    assert!(display["provider_hint"].is_null());
    assert_eq!(
        drain_message_outbox_once(&pool, &registry, "unknown-owner-worker").await,
        1
    );
    assert!(
        discord.posts().is_empty(),
        "unknown owner must not notify: the existing outbox retry owns delivery"
    );
    let row =
        sqlx::query("SELECT status,retry_count,sent_at,error FROM message_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(pool.as_ref())
            .await
            .unwrap();
    assert_eq!(row.get::<String, _>("status"), "pending");
    assert_eq!(row.get::<i64, _>("retry_count"), 1);
    assert!(
        row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("sent_at")
            .is_none()
    );
    assert!(row.get::<Option<String>, _>("error").is_some());
    assert_eq!(designation(&pool, id).await, Some(display.clone()));
    let _provider_rest = crate::services::discord::session_notice_delivery::register_test_provider(
        &registry,
        "claude",
        CHANNEL,
        discord.provider.clone(),
        pool.as_ref().clone(),
    )
    .await;
    sqlx::query("UPDATE message_outbox SET next_attempt_at=NOW() WHERE id=$1")
        .bind(id)
        .execute(pool.as_ref())
        .await
        .unwrap();
    assert_eq!(
        drain_message_outbox_once(&pool, &registry, "late-owner-worker").await,
        1,
        "an unknown-owner notice must remain deliverable by the existing retry path"
    );
    let posts = discord.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].0,
        format!("Bot {PROVIDER}"),
        "a null hint must resolve the actual owner at retry, never the notify bot"
    );
    assert_inline(posts[0].1["content"].as_str().unwrap());
    assert_eq!(
        designation(&pool, id).await,
        Some(display),
        "runtime owner resolution must not rewrite the enqueue-time designation"
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM message_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(pool.as_ref())
            .await
            .unwrap(),
        "sent"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn user_content_ops_and_unlisted_sources_remain_sql_null_display_pg() {
    let _isolation = Isolation::new();
    let (db, pool) = database("user content and unrelated source boundaries").await;
    for (source, reason) in [
        ("system", None),
        ("scheduled_message", Some("scheduled_message.deliver")),
        ("routine-runtime", Some("routine.result")),
        ("github_sync", Some("github_sync.terminal_open_issue")),
        ("lifecycle_notifier", Some("lifecycle.not-a-session-status")),
        ("lifecycle_notifier", Some("lifecycle.soft_clear")),
    ] {
        let id = message_outbox::enqueue_outbox_pg_returning_id(
            &pool,
            OutboxMessage {
                target: TARGET,
                content: ORIGINAL,
                bot: "notify",
                source,
                reason_code: reason,
                session_key: Some(SESSION),
            },
        )
        .await
        .unwrap()
        .expect("source-boundary fixture must enqueue a row");
        assert!(
            designation(&pool, id).await.is_none(),
            "user content/ops/unlisted source must stay SQL NULL: {source}/{reason:?}"
        );
    }
    for (index, (source, reason)) in [
        ("lifecycle_notifier", "lifecycle.monitor_auto_turn"),
        ("lifecycle_notifier", "lifecycle.monitor_auto_turn.deferred"),
        ("lifecycle_notifier", "lifecycle.session_resume_failed"),
        ("lifecycle_notifier", "lifecycle.context_compacted"),
        ("lifecycle_notifier", "lifecycle.mcp_credential_change"),
        ("lifecycle_notifier", "codex_goal_clear_active_turn"),
        ("lifecycle_notifier", "codex_goal_clear"),
        ("lifecycle_notifier", "codex_goal_pause_ignored"),
        ("lifecycle_notifier", "codex_goal_resume_ignored"),
        ("lifecycle_notifier", "codex_local_control"),
        ("lifecycle_notifier", "codex_unsupported_control"),
        ("queue_overflow_notice", "queue_overflow.evict"),
        ("adopt_fence_forward_notice", "adopt.fence_forward"),
        ("boot_custody_notice", "boot_custody.notice"),
        ("stall_watchdog", "stall_watchdog_suspected_stall"),
    ]
    .into_iter()
    .enumerate()
    {
        let session = format!("notice-matrix-{index}");
        let id = message_outbox::enqueue_outbox_pg_returning_id(
            &pool,
            OutboxMessage {
                target: TARGET,
                content: "⚠️ requester <@343742347365974026> · <@!343742347365974026> · <@&343742347365974026> · <@123>\nmore",
                bot: "notify",
                source,
                reason_code: Some(reason),
                session_key: Some(&session),
            },
        )
        .await
        .unwrap()
        .expect("whitelist matrix must enqueue a row");
        let display = designation(&pool, id).await.unwrap_or_else(|| {
            panic!("every automatic status source must freeze its provider display at enqueue: {source}/{reason}")
        });
        assert_eq!(display["version"], 1);
        assert!(
            display["provider_hint"].is_null(),
            "an unknown eligible owner still opts in: {source}/{reason}"
        );
        let content = display["content"].as_str().unwrap();
        assert_inline(content);
        assert!(
            content.contains("(+1줄)"),
            "matrix preview preserves omitted-line count: {source}/{reason}"
        );
        assert_eq!(
            content.contains("<@343742347365974026>"),
            source == "stall_watchdog",
            "only stall source may preserve strict requester mentions"
        );
        assert!(
            !content.contains("<@!") && !content.contains("<@&") && !content.contains("<@123>"),
            "non-user and malformed mentions must stay escaped"
        );
    }
    sqlx::query(
        "INSERT INTO agents(id,name,provider,discord_channel_id,discord_channel_cc,discord_channel_cdx)
         VALUES('inline-binding-claude','inline binding','codex','72280702',$1,'72280701')",
    ).bind(CHANNEL.to_string()).execute(pool.as_ref()).await.unwrap();
    let bound = message_outbox::enqueue_outbox_pg_returning_id(
        &pool,
        OutboxMessage {
            target: TARGET,
            content: ORIGINAL,
            bot: "notify",
            source: "lifecycle_notifier",
            reason_code: Some(REASON),
            session_key: Some("AgentDesk-codex-binding-precedence"),
        },
    )
    .await
    .unwrap()
    .expect("binding precedence fixture must enqueue");
    let display = designation(&pool, bound)
        .await
        .expect("binding precedence notice must opt in");
    assert_eq!(
        display["provider_hint"], "claude",
        "explicit channel binding wins over configured provider and session locator"
    );
    sqlx::query(
        "INSERT INTO agents(id,name,provider,discord_channel_id)
         VALUES('inline-binding-codex','conflicting inline binding','codex',$1)",
    )
    .bind(CHANNEL.to_string())
    .execute(pool.as_ref())
    .await
    .unwrap();
    let ambiguous = message_outbox::enqueue_outbox_pg_returning_id(
        &pool,
        OutboxMessage {
            target: TARGET,
            content: ORIGINAL,
            bot: "notify",
            source: "lifecycle_notifier",
            reason_code: Some(REASON),
            session_key: Some("AgentDesk-claude-binding-ambiguity"),
        },
    )
    .await
    .unwrap()
    .expect("binding ambiguity fixture must enqueue");
    let display = designation(&pool, ambiguous)
        .await
        .expect("ambiguous binding still freezes provider display");
    assert!(
        display["provider_hint"].is_null(),
        "conflicting channel bindings must not choose a provider from the session locator"
    );
    let mut tx = pool.begin().await.unwrap();
    let attachment = message_outbox::enqueue_outbox_pg_on_tx_with_attachment(
        &mut tx,
        OutboxMessage {
            target: TARGET,
            content: ORIGINAL,
            bot: "notify",
            source: "lifecycle_notifier",
            reason_code: Some(REASON),
            session_key: Some("attachment-boundary"),
        },
        Some(message_outbox::OutboxAttachment {
            filename: "fixture.txt",
            content_type: "text/plain",
            data: b"user content",
        }),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(
        designation(&pool, attachment).await.is_none(),
        "user attachment content must retain its original delivery surface"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn explicit_autoqueue_notice_does_not_convert_arbitrary_system_content_pg() {
    let _isolation = Isolation::new();
    let (db, pool) = database("explicit autoqueue display factory boundary").await;
    let mut tx = pool.begin().await.unwrap();
    let message = OutboxMessage {
        target: TARGET,
        content: "자동큐 완료: agent run\nsecond line",
        bot: "notify",
        source: "system",
        reason_code: None,
        session_key: Some(SESSION),
    };
    let ordinary = message_outbox::enqueue_outbox_pg_on_tx(&mut tx, message)
        .await
        .unwrap();
    let automatic = message_outbox::enqueue_session_notice_pg_on_tx(&mut tx, message)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        designation(&pool, ordinary).await.is_none(),
        "arbitrary system content must not inherit the autoqueue display contract"
    );
    let display = designation(&pool, automatic)
        .await
        .expect("the explicit autoqueue factory must freeze its provider display");
    assert_eq!(display["version"], 1);
    assert_eq!(display["provider_hint"], "claude");
    let content = display["content"].as_str().unwrap();
    assert_inline(content);
    assert!(
        content.starts_with("-# ✅ "),
        "autoqueue completion uses the existing success status icon: {content:?}"
    );
    assert!(content.contains("(+1줄)"));
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT bot,content FROM message_outbox ORDER BY id")
            .fetch_all(pool.as_ref())
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![
            ("notify".into(), message.content.into()),
            ("notify".into(), message.content.into())
        ]
    );
    pool.close().await;
    db.drop().await;
}
