//! HTTP entry tests for the host guard the stale-mailbox repair takes before its tmux
//! probe, its turn stop, its session disconnect and its inflight clear.

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use poise::serenity_prelude::ChannelId;
use tower::ServiceExt;

use crate::services::discord::host_teardown_gate::test_support::{
    Stored, busy_turn, channel_key, runtime, seed, shared_on, turn_kept,
};

async fn post_repair(
    registry: std::sync::Arc<crate::services::discord::health::HealthRegistry>,
    pool: &sqlx::PgPool,
    channel: ChannelId,
) -> (StatusCode, serde_json::Value) {
    let mut engine_config = crate::config::Config::default();
    engine_config.policies.hot_reload = false;
    let engine = crate::engine::PolicyEngine::new(&engine_config).unwrap();
    let tx = crate::server::ws::new_broadcast();
    let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
    let config = crate::config::Config::default();
    let app = crate::server::routes::api_router_with_pg(
        engine,
        config,
        tx,
        buf,
        Some(registry),
        Some(pool.clone()),
    );
    let body = serde_json::json!({"channel_id": channel.get(), "provider": "claude"});
    let mut request = Request::builder()
        .method("POST")
        .uri("/doctor/stale-mailbox/repair")
        .body(Body::from(body.to_string()))
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.1:8791".parse::<std::net::SocketAddr>().unwrap(),
    ));
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).expect("json body"))
}

async fn session_status(pool: &sqlx::PgPool, key: &str) -> Option<String> {
    sqlx::query_scalar("SELECT status FROM sessions WHERE session_key = $1")
        .bind(key)
        .fetch_optional(pool)
        .await
        .unwrap()
}

// Only a found legacy row reaches the injected tmux probe; every other stored answer is a
// 409 that leaves the turn, its inflight row and the active session row as they were.
#[tokio::test]
async fn stale_mailbox_repair_defers_before_the_tmux_probe_unless_the_row_is_legacy_pg() {
    use crate::services::session_host::test_support::InjectedPresenceGuard;
    use crate::services::session_host::{HostPresence, HostSessionRef};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (shared, registry) = runtime(&pool).await;
    let legacy = [
        HostPresence::Missing,
        HostPresence::Present,
        HostPresence::ProbeFailed,
    ]
    .map(|presence| (Stored::Legacy, presence));
    // A probe that would fail shows the host refusal comes before it.
    let refused = Stored::ALL[1..]
        .iter()
        .map(|stored| (*stored, HostPresence::ProbeFailed));
    for (n, (stored, presence)) in legacy.into_iter().chain(refused).enumerate() {
        let channel = ChannelId::new(1_479_671_301_387_059_900 + n as u64);
        let name = format!("AgentDesk-claude-p4a-repair-{n}");
        let _probe = InjectedPresenceGuard::set(HostSessionRef::tmux(&name), presence);
        let key = channel_key(&shared, &name);
        seed(&pool, &key, &name, channel.get(), stored).await;
        sqlx::query(
            "UPDATE sessions SET thread_channel_id = $2, status = 'turn_active' WHERE session_key = $1",
        )
        .bind(&key)
        .bind(channel.get().to_string())
        .execute(&pool)
        .await
        .unwrap();
        let token = busy_turn(&shared, channel, &name).await;
        let (status, json) = post_repair(registry.clone(), &pool, channel).await;
        let kept = turn_kept(&shared, channel, &token).await;
        let row = session_status(&pool, &key).await;
        let case = format!("{stored:?} {presence:?} {json}");
        let gate = match (stored, presence) {
            (Stored::Legacy, HostPresence::Missing) => {
                assert_eq!(status, StatusCode::OK, "{case}");
                assert!(!kept, "{case}");
                assert_eq!(row.as_deref(), Some("disconnected"), "{case}");
                continue;
            }
            (Stored::Legacy, HostPresence::Present) => "tmux_present",
            (Stored::Legacy, HostPresence::ProbeFailed) => "tmux_probe_failed",
            _ => "host_not_legacy_tmux",
        };
        assert_eq!(status, StatusCode::CONFLICT, "{case}");
        assert_eq!(json["safety_gate"], gate, "{case}");
        assert!(kept, "{case}");
        let unchanged = row.is_none_or(|status| status == "turn_active");
        assert!(unchanged, "{case}");
    }
    pool.close().await;
    db.drop().await;
}

/// The stale-mailbox route still answers 409 for an UNMEASURED tail, names it, and records it once;
/// the seeded legacy sessions row is what lets the host guard reach that measurement.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_mailbox_repair_names_and_records_an_unmeasured_tail_pg() {
    use crate::services::discord::relay_recovery::unread_tail_seed::{
        UnreadTailSeed, UnreadTailShape,
    };
    use crate::services::discord::relay_recovery::{
        UNREAD_TAIL_SITE_STALE_MAILBOX, stale_mailbox_idle_tail_admits,
    };
    let mut postgres = None;
    let slot = &mut postgres;
    let runtime = move || async move {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let shared = shared_on(&pool).await;
        *slot = Some((db, pool));
        shared
    };
    let shape = UnreadTailShape::RowOutputMissing;
    let Some(seed) = UnreadTailSeed::start_with_runtime(5_996_120_001, shape, runtime).await else {
        return;
    };
    let (db, pool) = postgres.take().expect("a postgres runtime");
    let (name, channel) = (&seed.tmux_session, seed.channel.get());
    let key = channel_key(&seed.shared, name);
    self::seed(&pool, &key, name, channel, Stored::Legacy).await;
    for _ in 0..2 {
        let (status, json) = post_repair(seed.registry.clone(), &pool, seed.channel).await;
        assert_eq!(status, StatusCode::CONFLICT, "{json}");
        assert_eq!(json["safety_gate"], "tmux_present", "{json}");
        assert_eq!(json["unread_tail"], "tail_not_measured", "{json}");
        assert!(seed.turn_kept(), "the idle turn must survive: {json}");
    }
    let refusals = seed.refusals();
    assert_eq!(refusals.len(), 1, "{refusals:?}");
    assert_eq!(refusals[0]["site"], UNREAD_TAIL_SITE_STALE_MAILBOX);
    assert_eq!(refusals[0]["decided_by"], "tail_not_measured");

    // A fresh episode another conjunct already refused records nothing.
    let mut fresh_episode = seed
        .registry
        .snapshot_watcher_state_for_provider(&seed.provider, seed.channel.get())
        .await
        .expect("fixture snapshot");
    fresh_episode.mailbox_active_user_msg_id = Some(1);
    assert!(!stale_mailbox_idle_tail_admits(
        &seed.provider,
        &fresh_episode,
        false
    ));
    assert_eq!(seed.refusals().len(), 1, "{:?}", seed.refusals());
    pool.close().await;
    db.drop().await;
}
