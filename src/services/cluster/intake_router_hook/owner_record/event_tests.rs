use super::*;
use crate::services::cluster::intake_router_hook::owner_record::{self, snapshot::tests::Fixture};
use crate::services::discord::outbound::source_registry::{
    SendCallerClass, validate_send_source_for,
};

fn description() -> Description<'static> {
    Description {
        home: "mac-a",
        home_label: "device A",
        target_label: "device B",
        new_session: false,
        home_changed: false,
        bot: "notify",
    }
}
async fn acquire(f: &Fixture, id: &OwnerIdentity, target: &str) {
    let mut tx = f.pool.begin().await.unwrap();
    custody::lock(&mut tx, id).await.unwrap();
    assert!(matches!(
        owner_record::acquire_owner_in_tx(&mut tx, id, target, |_| true)
            .await
            .unwrap(),
        owner_record::AcquireOutcome::AcquiredLocal { generation: 0 }
    ));
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn routing_event_real_outbox_history_and_failed_pointer_reuse_pg() {
    let f = Fixture::new("73369301").await;
    let id = OwnerIdentity::new("claude", &f.channel);
    acquire(&f, &id, "mac-a").await;
    assert!(validate_send_source_for(SOURCE, SendCallerClass::LoopbackInternal).is_ok());
    assert!(validate_send_source_for(SOURCE, SendCallerClass::Cli).is_err());
    assert!(validate_send_source_for(SOURCE, SendCallerClass::Dashboard).is_err());
    let mut tx = f.pool.begin().await.unwrap();
    assert!(
        reserve_on_tx(&mut tx, &id, 0, description())
            .await
            .unwrap()
            .is_none()
    );
    tx.commit().await.unwrap();
    let mut events = Vec::new();
    for (generation, from, to) in [
        (0, "mac-a", "mac-b"),
        (1, "mac-b", "mac-a"),
        (2, "mac-a", "mac-b"),
    ] {
        let mut tx = f.pool.begin().await.unwrap();
        custody::lock(&mut tx, &id).await.unwrap();
        assert_eq!(
            owner_record::transfer_owner_in_tx(&mut tx, &id, from, generation, to)
                .await
                .unwrap(),
            owner_record::TransferOutcome::Transferred {
                new_generation: generation + 1
            }
        );
        let event = reserve_on_tx(&mut tx, &id, generation + 1, description())
            .await
            .unwrap()
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(event.state, DeliveryState::Pending);
        assert!(event.post_started_at.is_none() && event.message_id.is_none());
        assert_eq!(event.event.from, from);
        assert_eq!(event.event.to, to);
        events.push(event);
    }
    assert_eq!(events[0].event.content, events[2].event.content);
    assert_ne!(
        events[0].outbox_id, events[2].outbox_id,
        "distinct generations need distinct actual outbox keys"
    );
    let ids: std::collections::BTreeSet<_> = events.iter().map(|r| r.outbox_id).collect();
    let nonces: std::collections::BTreeSet<_> =
        events.iter().map(|r| r.event.nonce.clone()).collect();
    assert_eq!((ids.len(), nonces.len()), (3, 3));
    for status in ["failed", "cancelled", "sent"] {
        sqlx::query("UPDATE message_outbox SET status=$2,cancelled_at=CASE WHEN $2='cancelled' THEN NOW() ELSE NULL END WHERE id=$1")
            .bind(events[2].outbox_id)
            .bind(status)
            .execute(&f.pool)
            .await
            .unwrap();
        let mut tx = f.pool.begin().await.unwrap();
        let recovered = reserve_on_tx(&mut tx, &id, 3, description())
            .await
            .unwrap()
            .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            recovered, events[2],
            "failed/cancelled history must reuse its original pointer and nonce"
        );
    }
    let mut tx = f.pool.begin().await.unwrap();
    let mut changed = description();
    changed.home = "mac-b";
    changed.home_label = "changed after commit";
    changed.bot = "other";
    let historical = reserve_on_tx(&mut tx, &id, 1, changed)
        .await
        .unwrap()
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        historical, events[0],
        "superseded generations retain their original notice"
    );
    assert!(sqlx::query("UPDATE intake_session_owners SET routing_notice_delivery_state='sent' WHERE provider='claude' AND generation=1").execute(&f.pool).await.is_err(), "a historical event cannot claim sent without its start and message receipt");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 3);
    sqlx::query("UPDATE message_outbox SET content='tampered' WHERE id=$1")
        .bind(events[0].outbox_id)
        .execute(&f.pool)
        .await
        .unwrap();
    let mut tx = f.pool.begin().await.unwrap();
    assert!(reserve_on_tx(&mut tx, &id, 1, description()).await.is_err());
    tx.rollback().await.unwrap();
    f.finish().await;
}

#[tokio::test]
async fn routing_event_transaction_rollback_and_first_peer_notification_pg() {
    let f = Fixture::new("73369302").await;
    let id = OwnerIdentity::new("claude", &f.channel);
    acquire(&f, &id, "mac-a").await;
    let mut tx = f.pool.begin().await.unwrap();
    custody::lock(&mut tx, &id).await.unwrap();
    owner_record::transfer_owner_in_tx(&mut tx, &id, "mac-a", 0, "mac-b")
        .await
        .unwrap();
    assert!(
        reserve_on_tx(&mut tx, &id, 1, description())
            .await
            .unwrap()
            .is_some()
    );
    tx.rollback().await.unwrap();
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let owner = owner_record::read_latest_owner_in_tx(&mut f.pool.begin().await.unwrap(), &id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (owner.owner_instance_id.as_str(), owner.generation),
        ("mac-a", 0)
    );
    let first = OwnerIdentity::new("codex", "73369303");
    acquire(&f, &first, "mac-b").await;
    let mut tx = f.pool.begin().await.unwrap();
    let notice = reserve_on_tx(&mut tx, &first, 0, description())
        .await
        .unwrap()
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        (notice.event.from.as_str(), notice.event.to.as_str()),
        ("mac-a", "mac-b")
    );
    sqlx::query("UPDATE intake_session_owners SET status='released' WHERE provider='codex'")
        .execute(&f.pool)
        .await
        .unwrap();
    let mut tx = f.pool.begin().await.unwrap();
    custody::lock(&mut tx, &first).await.unwrap();
    owner_record::acquire_owner_in_tx(&mut tx, &first, "mac-b", |_| true)
        .await
        .unwrap();
    assert!(
        reserve_on_tx(&mut tx, &first, 1, description())
            .await
            .unwrap()
            .is_none(),
        "same-Mac reacquisition must not emit a routing notice"
    );
    tx.commit().await.unwrap();
    assert!(
        sqlx::query(
            "UPDATE intake_session_owners SET routing_notice_outbox_id=$1 WHERE provider='claude'"
        )
        .bind(notice.outbox_id)
        .execute(&f.pool)
        .await
        .is_err(),
        "a pointer without an event must fail closed"
    );
    let deliveries: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM intake_outbox")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        deliveries, 0,
        "notice reservation must not fabricate a delivery"
    );
    f.finish().await;
}

struct Isolation {
    _guards: [crate::config::TestEnvVarGuard; 2],
    _live: crate::config_live_reload::TestLiveGuard,
    _root: tempfile::TempDir,
}
fn isolation() -> Isolation {
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
    Isolation {
        _guards: guards,
        _live: live,
        _root: root,
    }
}
const DRAIN_CHANNEL: &str = "72280700";
async fn switched(
    f: &Fixture,
    id: &OwnerIdentity,
    from: &str,
    generation: i64,
    to: &str,
) -> Reservation {
    let mut tx = f.pool.begin().await.unwrap();
    custody::lock(&mut tx, id).await.unwrap();
    owner_record::transfer_owner_in_tx(&mut tx, id, from, generation, to)
        .await
        .unwrap();
    let mut described = description();
    if to == "mac-a" {
        described.target_label = "device A";
    }
    let reserved = reserve_on_tx(&mut tx, id, generation + 1, described)
        .await
        .unwrap()
        .unwrap();
    tx.commit().await.unwrap();
    reserved
}
async fn general(f: &Fixture, content: &str) -> i64 {
    let mut tx = f.pool.begin().await.unwrap();
    let id = crate::services::message_outbox::enqueue_outbox_pg_on_tx(
        &mut tx,
        OutboxMessage {
            target: "channel:72280700",
            content,
            bot: "notify",
            source: "lifecycle_notifier",
            reason_code: Some("runtime_started"),
            session_key: None,
        },
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    id
}
async fn cursor(f: &Fixture, outbox: i64) -> (String, Option<String>, bool) {
    sqlx::query_as("SELECT routing_notice_delivery_state,routing_notice_message_id,routing_notice_started_at IS NOT NULL FROM intake_session_owners WHERE routing_notice_outbox_id=$1")
        .bind(outbox).fetch_one(&f.pool).await.unwrap()
}
async fn outbox_status(f: &Fixture, outbox: i64) -> String {
    sqlx::query_scalar("SELECT status FROM message_outbox WHERE id=$1")
        .bind(outbox)
        .fetch_one(&f.pool)
        .await
        .unwrap()
}
async fn drain(
    f: &Fixture,
    registry: &std::sync::Arc<crate::services::discord::health::HealthRegistry>,
) -> usize {
    crate::server::outbox_worker::drain_message_outbox_once(
        &std::sync::Arc::new(f.pool.clone()),
        registry,
        "routing-notice-drain",
    )
    .await
}

#[tokio::test]
async fn routing_notice_drain_posts_stored_nonce_and_records_sent_pg() {
    let _isolation = isolation();
    let f = Fixture::new(DRAIN_CHANNEL).await;
    let id = OwnerIdentity::new("claude", &f.channel);
    acquire(&f, &id, "mac-a").await;
    let notice = switched(&f, &id, "mac-a", 0, "mac-b").await;
    let plain = general(&f, "general lifecycle row").await;
    let discord = crate::server::outbox_worker::origin_fixture::Discord::new().await;
    let registry = discord.registry().await;
    assert_eq!(drain(&f, &registry).await, 2);
    let posts = discord.posts();
    assert_eq!(
        posts.len(),
        2,
        "the routing row and the general row each post once"
    );
    let routed: Vec<_> = posts
        .iter()
        .filter(|(_, body)| {
            body["content"]
                .as_str()
                .is_some_and(|c| c.contains(&notice.event.content))
        })
        .collect();
    assert_eq!(routed.len(), 1);
    assert_eq!(
        routed[0].1["nonce"].as_str(),
        Some(notice.event.nonce.as_str()),
        "the actual POST must carry the stored event nonce"
    );
    assert_eq!(routed[0].1["enforce_nonce"], serde_json::json!(true));
    assert!(
        crate::services::discord::dispatch_policy::has_non_turn_provenance(
            routed[0].1["content"].as_str().unwrap()
        )
    );
    assert!(
        posts
            .iter()
            .any(|(_, body)| body["content"].as_str() == Some("general lifecycle row"))
    );
    assert_eq!(
        cursor(&f, notice.outbox_id).await,
        ("sent".into(), Some("72280799".into()), true),
        "a sent cursor needs the real message receipt"
    );
    assert_eq!(outbox_status(&f, notice.outbox_id).await, "sent");
    assert_eq!(outbox_status(&f, plain).await, "sent");
    sqlx::query("UPDATE message_outbox SET status='pending',claim_owner=NULL,claimed_at=NULL,next_attempt_at=NOW() WHERE id=$1")
        .bind(notice.outbox_id).execute(&f.pool).await.unwrap();
    assert_eq!(drain(&f, &registry).await, 1);
    assert_eq!(discord.posts().len(), 2, "a sent cursor never posts again");
    assert_eq!(outbox_status(&f, notice.outbox_id).await, "sent");
    f.finish().await;
}

#[tokio::test]
async fn routing_notice_posting_resume_keeps_nonce_and_stale_attempt_is_uncertain_pg() {
    let _isolation = isolation();
    let f = Fixture::new(DRAIN_CHANNEL).await;
    let id = OwnerIdentity::new("codex", &f.channel);
    acquire(&f, &id, "mac-a").await;
    let fresh = switched(&f, &id, "mac-a", 0, "mac-b").await;
    let stale = switched(&f, &id, "mac-b", 1, "mac-a").await;
    sqlx::query("UPDATE intake_session_owners SET routing_notice_delivery_state='posting',routing_notice_started_at=NOW()-make_interval(secs=>CASE WHEN routing_notice_outbox_id=$1 THEN 5 ELSE 600 END) WHERE routing_notice_outbox_id IN ($1,$2)")
        .bind(fresh.outbox_id).bind(stale.outbox_id).execute(&f.pool).await.unwrap();
    let discord = crate::server::outbox_worker::origin_fixture::Discord::new().await;
    let registry = discord.registry().await;
    assert_eq!(drain(&f, &registry).await, 2);
    let posts = discord.posts();
    assert_eq!(
        posts.len(),
        1,
        "an attempt outside the nonce window is never reposted"
    );
    assert_eq!(
        posts[0].1["nonce"].as_str(),
        Some(fresh.event.nonce.as_str())
    );
    assert_eq!(cursor(&f, fresh.outbox_id).await.0, "sent");
    assert_eq!(
        cursor(&f, stale.outbox_id).await,
        ("uncertain".into(), None, true)
    );
    assert_eq!(outbox_status(&f, stale.outbox_id).await, "failed");
    sqlx::query("UPDATE message_outbox SET status='pending',claim_owner=NULL,claimed_at=NULL,next_attempt_at=NOW() WHERE id=$1")
        .bind(stale.outbox_id).execute(&f.pool).await.unwrap();
    assert_eq!(drain(&f, &registry).await, 1);
    assert_eq!(
        discord.posts().len(),
        1,
        "uncertain stays without a blind repost"
    );
    let mut tx = f.pool.begin().await.unwrap();
    let kept = reserve_on_tx(&mut tx, &id, 2, description())
        .await
        .unwrap()
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        (kept.outbox_id, kept.event.nonce),
        (stale.outbox_id, stale.event.nonce)
    );
    assert_eq!(kept.state, DeliveryState::Uncertain);
    f.finish().await;
}

#[tokio::test]
async fn routing_notice_gc_keeps_referenced_failed_rows_pg() {
    let f = Fixture::new("73369304").await;
    let id = OwnerIdentity::new("claude", &f.channel);
    acquire(&f, &id, "mac-a").await;
    let failed = switched(&f, &id, "mac-a", 0, "mac-b").await;
    let cancelled = switched(&f, &id, "mac-b", 1, "mac-a").await;
    let unrelated = [
        general(&f, "unrelated failed").await,
        general(&f, "unrelated cancelled").await,
    ];
    for (row, status) in [
        (failed.outbox_id, "failed"),
        (cancelled.outbox_id, "cancelled"),
        (unrelated[0], "failed"),
        (unrelated[1], "cancelled"),
    ] {
        sqlx::query("UPDATE message_outbox SET status=$2,cancelled_at=CASE WHEN $2='cancelled' THEN NOW() ELSE NULL END,created_at=NOW()-INTERVAL '8 days' WHERE id=$1")
            .bind(row).bind(status).execute(&f.pool).await.unwrap();
    }
    let (_, pruned, _) = crate::services::message_outbox::gc_stale_outbox_rows(&f.pool)
        .await
        .expect("referenced routing rows must not make the whole GC fail");
    assert_eq!(pruned, 2);
    let left: Vec<i64> = sqlx::query_scalar("SELECT id FROM message_outbox ORDER BY id")
        .fetch_all(&f.pool)
        .await
        .unwrap();
    assert_eq!(left, vec![failed.outbox_id, cancelled.outbox_id]);
    f.finish().await;
}

async fn retry_delay(f: &Fixture, outbox: i64) -> f64 {
    sqlx::query_scalar("SELECT EXTRACT(EPOCH FROM next_attempt_at-NOW())::FLOAT8 FROM message_outbox WHERE id=$1 AND status='pending'")
        .bind(outbox).fetch_one(&f.pool).await.unwrap()
}
/// Moves both clocks back as if `seconds` had elapsed since the interrupted attempt.
async fn elapse(f: &Fixture, outbox: i64, seconds: i32) {
    sqlx::query("UPDATE message_outbox SET next_attempt_at=LEAST(next_attempt_at,NOW()),claimed_at=claimed_at-make_interval(secs=>$2::DOUBLE PRECISION) WHERE id=$1")
        .bind(outbox).bind(seconds).execute(&f.pool).await.unwrap();
    sqlx::query("UPDATE intake_session_owners SET routing_notice_started_at=routing_notice_started_at-make_interval(secs=>$2::DOUBLE PRECISION) WHERE routing_notice_outbox_id=$1")
        .bind(outbox).bind(seconds).execute(&f.pool).await.unwrap();
}

#[tokio::test]
async fn routing_notice_failed_post_retries_inside_nonce_window_after_real_claim_pg() {
    let _isolation = isolation();
    let f = Fixture::new(DRAIN_CHANNEL).await;
    let id = OwnerIdentity::new("opencode", &f.channel);
    acquire(&f, &id, "mac-a").await;
    let notice = switched(&f, &id, "mac-a", 0, "mac-b").await;
    let unreachable = std::sync::Arc::new(crate::services::discord::health::HealthRegistry::new());
    assert_eq!(drain(&f, &unreachable).await, 1);
    assert_eq!(cursor(&f, notice.outbox_id).await.0, "posting");
    let delay = retry_delay(&f, notice.outbox_id).await;
    assert!(
        delay <= 30.0,
        "a routing notice retry must land inside its nonce window, waited {delay}s"
    );
    elapse(&f, notice.outbox_id, delay.ceil() as i32).await;
    let discord = crate::server::outbox_worker::origin_fixture::Discord::new().await;
    assert_eq!(drain(&f, &discord.registry().await).await, 1);
    let posts = discord.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].1["nonce"].as_str(),
        Some(notice.event.nonce.as_str())
    );
    assert_eq!(cursor(&f, notice.outbox_id).await.0, "sent");
    f.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routing_notice_claim_interrupted_mid_post_resumes_inside_nonce_window_pg() {
    let _isolation = isolation();
    let f = Fixture::new(DRAIN_CHANNEL).await;
    let id = OwnerIdentity::new("grok", &f.channel);
    acquire(&f, &id, "mac-a").await;
    let notice = switched(&f, &id, "mac-a", 0, "mac-b").await;
    // Accepts the POST and never answers, so the worker dies holding a real processing claim.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let held = tokio::spawn(async move {
        let mut sockets = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            sockets.push(socket);
        }
    });
    let hanging = std::sync::Arc::new(
        poise::serenity_prelude::HttpBuilder::new("hanging-notify-token")
            .proxy(format!("http://{address}"))
            .ratelimiter_disabled(true)
            .build(),
    );
    let registry = std::sync::Arc::new(crate::services::discord::health::HealthRegistry::new());
    registry
        .set_utility_bot_http_for_tests(
            crate::services::discord::bot_role::UtilityBotRole::Notify,
            hanging,
        )
        .await;
    let worker = {
        let pool = std::sync::Arc::new(f.pool.clone());
        tokio::spawn(async move {
            crate::server::outbox_worker::drain_message_outbox_once(&pool, &registry, "dies").await
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while cursor(&f, notice.outbox_id).await.0 != "posting" {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    worker.abort();
    held.abort();
    let status: String = sqlx::query_scalar("SELECT status FROM message_outbox WHERE id=$1")
        .bind(notice.outbox_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        status, "processing",
        "the interrupted worker keeps its real claim"
    );
    elapse(&f, notice.outbox_id, 60).await;
    let discord = crate::server::outbox_worker::origin_fixture::Discord::new().await;
    assert_eq!(
        drain(&f, &discord.registry().await).await,
        1,
        "a routing claim must be reclaimable inside the nonce window"
    );
    let posts = discord.posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].1["nonce"].as_str(),
        Some(notice.event.nonce.as_str())
    );
    assert_eq!(
        cursor(&f, notice.outbox_id).await,
        ("sent".into(), Some("72280799".into()), true)
    );
    f.finish().await;
}
