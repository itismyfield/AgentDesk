#![cfg(unix)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio::sync::Notify;

use super::tests::{deliver, router};
use super::{ADMISSION_ENABLED_FOR_TEST, DELIVERY_PROBE_FOR_TEST, DeliveryProbe};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::SharedData;
use crate::services::discord::health::{
    HealthRegistry, InjectPane, inflight_rows_for_tests, queue_texts, register_bot_auth_for_tests,
    register_inject_runtime, seed_external_turn_row_for_tests,
};
use crate::services::provider::ProviderKind;

const AGENT: &str = "legacy-origin-agent";

struct Fixture {
    db: TestPostgresDb,
    pool: PgPool,
    registry: Arc<HealthRegistry>,
    shared: Arc<SharedData>,
    app: Router,
    channel: u64,
    calls: Arc<AtomicUsize>,
    _root: crate::config::TestRuntimeRootGuard,
}

impl Fixture {
    async fn create(channel: u64) -> Self {
        let root = crate::config::TestRuntimeRootGuard::new();
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        crate::db::agents::insert_agent_channels_for_tests(
            &pool,
            AGENT,
            Some(&channel.to_string()),
            None,
        )
        .await;
        let registry = Arc::new(HealthRegistry::new());
        let shared =
            register_inject_runtime(&registry, &[channel, channel + 100], Some(pool.clone())).await;
        let app = router(Some(pool.clone()), Some(registry.clone()));
        Self {
            db,
            pool,
            registry,
            shared,
            app,
            channel,
            calls: Arc::new(AtomicUsize::new(0)),
            _root: root,
        }
    }

    fn probe(&self) -> DeliveryProbe {
        DeliveryProbe {
            calls: self.calls.clone(),
            ..Default::default()
        }
    }

    async fn call(&self, body: &Value) -> (StatusCode, Value) {
        call(&self.app, AGENT, self.probe(), body).await
    }

    async fn admission_count(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM imsg_reply_obligations")
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn close(self) {
        drop(self.app);
        drop(self.registry);
        drop(self.shared);
        self.pool.close().await;
        self.db.drop().await;
    }
}

async fn call(
    app: &Router,
    agent: &str,
    probe: DeliveryProbe,
    body: &Value,
) -> (StatusCode, Value) {
    call_enabled(app, agent, probe, body, false).await
}

async fn call_enabled(
    app: &Router,
    agent: &str,
    probe: DeliveryProbe,
    body: &Value,
    enabled: bool,
) -> (StatusCode, Value) {
    let body = body.to_string();
    ADMISSION_ENABLED_FOR_TEST
        .scope(
            enabled,
            DELIVERY_PROBE_FOR_TEST.scope(probe, deliver(app, agent, &body)),
        )
        .await
}

fn input(origin: Option<&str>) -> Value {
    json!({"text": "status?", "author_discord_user_id": "200", "source": "external", "origin_id": origin})
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_http_unready_start_preserves_origin_until_a_pane_can_deliver_pg() {
    let f = Fixture::create(9_727_408).await;
    let body = input(Some("legacy-boot-unready"));
    let unregistered = router(Some(f.pool.clone()), Some(Arc::new(HealthRegistry::new())));
    let absent = call(&unregistered, AGENT, f.probe(), &body).await;
    assert_eq!(absent.0, StatusCode::SERVICE_UNAVAILABLE, "{}", absent.1);
    assert_eq!(absent.1["error"], "runtime_unavailable");
    assert!(
        absent.1["detail"]
            .as_str()
            .unwrap()
            .contains("provider runtime not registered")
    );
    assert_eq!(f.admission_count().await, 0);
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    assert!(queue_texts(&f.shared, f.channel).await.is_empty());
    assert_eq!(
        inflight_rows_for_tests(&ProviderKind::Claude, &[f.channel]),
        0
    );
    drop(unregistered);
    let first = f.call(&body).await;
    assert_eq!(first.0, StatusCode::SERVICE_UNAVAILABLE, "{}", first.1);
    assert_eq!(first.1["error"], "runtime_unavailable");
    assert_eq!(first.1["detail"], "provider runtime is not ready");
    let first_admissions = f.admission_count().await;
    let first_calls = f.calls.load(Ordering::SeqCst);
    let first_queue = queue_texts(&f.shared, f.channel).await;
    let first_inflight = inflight_rows_for_tests(&ProviderKind::Claude, &[f.channel]);

    let pane = InjectPane::new(f.channel, "external");
    let ready = f.call(&body).await;
    eprintln!(
        "boot retry first_status={} admission_rows={first_admissions} backend_calls={first_calls} next_status={} next_body={}",
        first.0, ready.0, ready.1
    );
    assert_eq!(ready.0, StatusCode::OK, "ready retry: {}", ready.1);
    assert_eq!(ready.1["delivery"], "injected");
    assert_eq!(
        first_admissions, 0,
        "unready start must not reserve the origin"
    );
    assert_eq!(first_calls, 0, "preparation must stop before the backend");
    assert!(first_queue.is_empty());
    assert_eq!(first_inflight, 0, "unready start has no turn effect");
    assert_eq!(pane.records().len(), 1);
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.call(&body).await, ready);
    assert_eq!(pane.records().len(), 1);
    let different = f.call(&input(Some("legacy-boot-distinct"))).await;
    assert_eq!(different.0, StatusCode::OK, "{}", different.1);
    assert_eq!(pane.records().len(), 2);
    assert_eq!(f.calls.load(Ordering::SeqCst), 2);
    drop(pane);
    f.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_http_saved_replay_and_authorization_precede_new_start_readiness_pg() {
    let f = Fixture::create(9_727_409).await;
    let pane = InjectPane::new(f.channel, "external");
    let body = input(Some("legacy-cached-before-readiness"));
    let first = f.call(&body).await;
    assert_eq!(first.0, StatusCode::OK, "{}", first.1);
    assert_eq!(first.1["delivery"], "injected");
    assert_eq!(pane.records().len(), 1);
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    drop(pane);
    assert!(crate::services::discord::clear_inflight_state(
        &ProviderKind::Claude,
        f.channel,
    ));
    assert_eq!(
        inflight_rows_for_tests(&ProviderKind::Claude, &[f.channel]),
        0
    );
    let fresh_unready = f.call(&input(Some("legacy-new-while-unready"))).await;
    assert_eq!(
        fresh_unready.0,
        StatusCode::SERVICE_UNAVAILABLE,
        "{}",
        fresh_unready.1
    );
    assert_eq!(fresh_unready.1["detail"], "provider runtime is not ready");
    assert_eq!(f.admission_count().await, 1);
    assert_eq!(f.call(&body).await, first);
    let mut changed = body.clone();
    changed["text"] = json!("different body");
    let conflict = f.call(&changed).await;
    assert_eq!(conflict.0, StatusCode::CONFLICT, "{}", conflict.1);
    assert_eq!(conflict.1["error"], "origin_conflict");
    let mut unauthorized = body.clone();
    unauthorized["author_discord_user_id"] = json!("300");
    let refused = f.call(&unauthorized).await;
    assert_eq!(refused.0, StatusCode::FORBIDDEN, "{}", refused.1);
    assert_eq!(refused.1["error"], "author_not_allowed");
    for (state, error) in [
        ("admitting", "admission_pending"),
        ("unknown", "admission_unknown"),
    ] {
        sqlx::query("UPDATE imsg_reply_obligations SET admission=$2 WHERE origin_id=$1")
            .bind("legacy-cached-before-readiness")
            .bind(state)
            .execute(&f.pool)
            .await
            .unwrap();
        let pending = f.call(&body).await;
        assert_eq!(pending.0, StatusCode::ACCEPTED, "{}", pending.1);
        assert_eq!(pending.1["error"], error);
    }
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    assert!(queue_texts(&f.shared, f.channel).await.is_empty());
    assert_eq!(f.admission_count().await, 1);
    f.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_http_external_hold_can_queue_without_gateway_context_pg() {
    let f = Fixture::create(9_727_410).await;
    seed_external_turn_row_for_tests(&ProviderKind::Claude, f.channel);
    let body = input(Some("legacy-queue-without-context"));
    let first = f.call(&body).await;
    assert_eq!(first.0, StatusCode::OK, "{}", first.1);
    assert_eq!(first.1["delivery"], "queued");
    assert_eq!(first.1["reason"], "external_turn_active");
    assert_eq!(queue_texts(&f.shared, f.channel).await, ["status?"]);
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.call(&body).await, first);
    assert_eq!(queue_texts(&f.shared, f.channel).await, ["status?"]);
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    let mut distinct = input(Some("legacy-queue-distinct"));
    distinct["text"] = json!("next status?");
    let different = f.call(&distinct).await;
    assert_eq!(different.0, StatusCode::OK, "{}", different.1);
    assert_eq!(different.1["delivery"], "queued");
    assert_eq!(
        queue_texts(&f.shared, f.channel).await,
        ["status?", "next status?"]
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), 2);
    assert_eq!(f.admission_count().await, 2);
    assert_eq!(
        inflight_rows_for_tests(&ProviderKind::Claude, &[f.channel]),
        1
    );
    f.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_http_same_origin_sequential_and_distinct_origins_pg() {
    let f = Fixture::create(9_727_401).await;
    let pane = InjectPane::new(f.channel, "external");
    let body = input(Some("legacy-sequential"));
    let first = f.call(&body).await;
    assert_eq!(first.0, StatusCode::OK, "{}", first.1);
    assert_eq!(first.1["delivery"], "injected", "{}", first.1);
    let duplicate = f.call(&body).await;
    assert_eq!(duplicate, first);
    assert_eq!(pane.records().len(), 1, "same-origin pane content");
    assert_eq!(f.calls.load(Ordering::SeqCst), 1, "backend entries");
    for purpose in [
        crate::db::imsg_reply::Purpose::Final,
        crate::db::imsg_reply::Purpose::Mirror,
    ] {
        let claim = crate::db::imsg_reply::claim_action(
            &f.pool,
            &crate::db::imsg_reply::ClaimAction {
                id: uuid::Uuid::new_v4(),
                origin_id: "legacy-sequential",
                purpose,
                action_key: "ordinary-http-authority",
                reply_episode_key: None,
            },
        )
        .await
        .unwrap();
        assert!(
            claim.is_none(),
            "ordinary HTTP never acquires {purpose:?} reply authority"
        );
    }
    let different = f.call(&input(Some("legacy-distinct"))).await;
    assert_eq!(different.0, StatusCode::OK, "{}", different.1);
    assert_eq!(
        pane.records().len(),
        2,
        "different origins each inject once"
    );
    assert!(
        pane.records()
            .iter()
            .all(|text| !text.contains("imsg_reply.py"))
    );
    drop(pane);
    f.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_http_concurrent_same_origin_enters_backend_once_pg() {
    let f = Fixture::create(9_727_402).await;
    let pane = InjectPane::new(f.channel, "external");
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let first_probe = DeliveryProbe {
        calls: f.calls.clone(),
        entered: Some(entered.clone()),
        release: Some(release.clone()),
    };
    let app = f.app.clone();
    let first = tokio::spawn(async move {
        call(&app, AGENT, first_probe, &input(Some("legacy-concurrent"))).await
    });
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .expect("first delivery entered");
    let duplicate = f.call(&input(Some("legacy-concurrent"))).await;
    assert_eq!(duplicate.0, StatusCode::ACCEPTED, "{}", duplicate.1);
    assert_eq!(duplicate.1["error"], "admission_pending");
    release.notify_one();
    let first = first.await.unwrap();
    assert_eq!(first.0, StatusCode::OK, "{}", first.1);
    assert_eq!(f.call(&input(Some("legacy-concurrent"))).await, first);
    assert_eq!(pane.records().len(), 1);
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    drop(pane);
    f.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_http_payload_and_provider_conflicts_never_enter_backend_pg() {
    let f = Fixture::create(9_727_403).await;
    let pane = InjectPane::new(f.channel, "external");
    let mut body = input(Some("legacy-conflict"));
    body["channel_id"] = json!(f.channel.to_string());
    assert_eq!(f.call(&body).await.0, StatusCode::OK);
    for (key, value) in [
        ("text", "changed"),
        ("source", "webhook"),
        ("author_discord_user_id", "100"),
    ] {
        let mut changed = body.clone();
        changed[key] = json!(value);
        let answer = f.call(&changed).await;
        assert_eq!(answer.0, StatusCode::CONFLICT, "{key}: {}", answer.1);
        assert_eq!(answer.1["error"], "origin_conflict");
    }
    crate::db::agents::insert_agent_channels_for_tests(
        &f.pool,
        "legacy-other-agent",
        Some(&f.channel.to_string()),
        None,
    )
    .await;
    let answer = call(&f.app, "legacy-other-agent", f.probe(), &body).await;
    assert_eq!(answer.0, StatusCode::CONFLICT, "{}", answer.1);
    sqlx::query("UPDATE agents SET discord_channel_cc=$2, discord_channel_id=$2 WHERE id=$1")
        .bind(AGENT)
        .bind((f.channel + 100).to_string())
        .execute(&f.pool)
        .await
        .unwrap();
    let mut different_channel = body.clone();
    different_channel["channel_id"] = json!((f.channel + 100).to_string());
    let answer = f.call(&different_channel).await;
    assert_eq!(answer.0, StatusCode::CONFLICT, "channel only: {}", answer.1);
    assert_eq!(answer.1["error"], "origin_conflict");
    register_bot_auth_for_tests(&f.registry, "codex", f.channel, Some(100), vec![200], false).await;
    sqlx::query("UPDATE agents SET discord_channel_cc=$2, discord_channel_id=$2, discord_channel_cdx=$3, discord_channel_alt=$3 WHERE id=$1")
        .bind(AGENT).bind((f.channel + 100).to_string()).bind(f.channel.to_string())
        .execute(&f.pool).await.unwrap();
    let provider_changed = f.call(&body).await;
    assert_eq!(
        provider_changed.0,
        StatusCode::CONFLICT,
        "provider only: {}",
        provider_changed.1
    );
    assert_eq!(provider_changed.1["error"], "origin_conflict");
    assert_eq!(pane.records().len(), 1, "conflicting payload content=0");
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    drop(pane);
    f.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_http_without_origin_preserves_response_and_reply_authority_pg() {
    let f = Fixture::create(9_727_404).await;
    let pane = InjectPane::new(f.channel, "external");
    let body = input(None);
    let expected = (
        StatusCode::OK,
        json!({
            "ok": true, "delivery": "injected", "turn_id": null,
            "channel_id": f.channel.to_string(), "reason": null,
        }),
    );
    assert_eq!(f.call(&body).await, expected);
    assert_eq!(f.call(&body).await, expected);
    assert_eq!(
        pane.records(),
        [
            "[User: external:200 (ID: 200)] status?",
            "[User: external:200 (ID: 200)] status?",
        ]
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM imsg_reply_obligations")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    drop(pane);
    f.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_http_old_hash_replays_or_conflicts_without_delivery_pg() {
    use sha2::{Digest, Sha256};
    let f = Fixture::create(9_727_405).await;
    let pane = InjectPane::new(f.channel, "external");
    let old_hash = format!(
        "{:x}",
        Sha256::digest(
            json!(["status?", "imessage", f.channel.to_string(), "200",])
                .to_string()
                .as_bytes()
        )
    );
    let cached = json!({"ok": true, "delivery": "injected", "old_response": true});
    sqlx::query("INSERT INTO imsg_reply_obligations (origin_id, source, agent_id, channel_id, person_id, request_hash, expires_at, admission, admission_result) VALUES ($1,'imessage',$2,$3,200,$4,NOW()+INTERVAL '24 hours','admitted',$5)")
        .bind("old-http-hash").bind(AGENT).bind(f.channel.to_string()).bind(old_hash)
        .bind(json!({"status": 200, "body": cached})).execute(&f.pool).await.unwrap();
    let mut body = input(Some("old-http-hash"));
    body["source"] = json!("imessage");
    let same = call_enabled(&f.app, AGENT, f.probe(), &body, true).await;
    assert_eq!(same, (StatusCode::OK, cached));
    body["text"] = json!("different request");
    let changed = call_enabled(&f.app, AGENT, f.probe(), &body, true).await;
    assert_eq!(changed.0, StatusCode::CONFLICT, "{}", changed.1);
    assert_eq!(changed.1["error"], "origin_conflict");
    assert!(pane.records().is_empty());
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    drop(pane);
    f.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_http_true_paste_ambiguity_stays_200_and_never_retries_pg() {
    let f = Fixture::create(9_727_406).await;
    let pane = InjectPane::new(f.channel, "external");
    std::fs::copy(pane.path("tmux"), pane.path("tmux-original")).unwrap();
    let wrapper = format!(
        r#"#!/bin/sh
d='{}'
"$d/tmux-original" "$@"
rc=$?
case "$2" in
paste-buffer) paste=1 ;;
if-shell) case "$7" in paste-buffer*) paste=1 ;; esac ;;
esac
if [ "$rc" = 0 ] && [ "${{paste:-0}}" = 1 ]; then
  python3 -c 'import json,sys; print(json.dumps(open(sys.argv[1]).read()))' "$d/buffer" >> "$d/content-effects"
  exit 1
fi
exit "$rc"
"#,
        pane.path("tmux").parent().unwrap().display()
    );
    pane.set("tmux", &wrapper);
    let body = input(Some("legacy-write-ambiguity"));
    let first = f.call(&body).await;
    assert_eq!(first.0, StatusCode::OK, "{}", first.1);
    assert_eq!(first.1["delivery"], "unconfirmed", "{}", first.1);
    assert_eq!(first.1["reason"], "inject_unconfirmed");
    assert_eq!(first.1["detail"], "paste_failed");
    assert!(
        pane.path("pasted").exists(),
        "the real shim applied content before losing its answer"
    );
    let effects = pane.read("content-effects").unwrap();
    assert_eq!(effects.lines().count(), 1);
    assert_eq!(f.call(&body).await, first);
    assert_eq!(pane.read("content-effects").unwrap(), effects);
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    assert!(
        pane.records().is_empty(),
        "uncertain paste sends no second Enter"
    );
    drop(pane);
    f.close().await;
}

async fn restart_child() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let url = std::env::var("ADK_LEGACY_CHILD_DB").unwrap();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .unwrap();
    let channel = 9_727_407;
    let registry = Arc::new(HealthRegistry::new());
    register_inject_runtime(&registry, &[channel], Some(pool.clone())).await;
    let pane = InjectPane::new(channel, "external");
    let script = std::fs::read_to_string(std::env::var("ADK_LEGACY_CHILD_SHIM").unwrap()).unwrap();
    pane.set("tmux", &script);
    let app = router(Some(pool.clone()), Some(registry));
    let origin = std::env::var("ADK_LEGACY_CHILD_ORIGIN").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let answer = call(
        &app,
        AGENT,
        DeliveryProbe {
            calls: calls.clone(),
            ..Default::default()
        },
        &input(Some(&origin)),
    )
    .await;
    let expected_status: u16 = std::env::var("ADK_LEGACY_CHILD_STATUS")
        .unwrap()
        .parse()
        .unwrap();
    let expected_calls: usize = std::env::var("ADK_LEGACY_CHILD_CALLS")
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(answer.0.as_u16(), expected_status, "{}", answer.1);
    assert_eq!(calls.load(Ordering::SeqCst), expected_calls, "{}", answer.1);
    if expected_status == 200 {
        assert_eq!(answer.1["delivery"], "injected", "{}", answer.1);
    } else {
        let error = if expected_status == 503 {
            "admission_settlement_failed"
        } else {
            "admission_pending"
        };
        assert_eq!(answer.1["error"], error, "{}", answer.1);
    }
    eprintln!(
        "restart child pid={} origin={origin} status={expected_status} backend_calls={expected_calls}",
        std::process::id()
    );
    drop((pane, app));
    pool.close().await;
}

async fn run_child(f: &Fixture, pane: &InjectPane, origin: &str, status: u16, calls: usize) {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    let test = concat!(
        module_path!(),
        "::legacy_http_process_restart_preserves_saved_and_unsettled_origins_pg"
    )
    .strip_prefix("agentdesk::")
    .unwrap();
    command
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env("ADK_LEGACY_CHILD_DB", &f.db.database_url)
        .env("ADK_LEGACY_CHILD_SHIM", pane.path("tmux"))
        .env("ADK_LEGACY_CHILD_ORIGIN", origin)
        .env("ADK_LEGACY_CHILD_STATUS", status.to_string())
        .env("ADK_LEGACY_CHILD_CALLS", calls.to_string());
    let output = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    eprintln!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "restart child failed: {}",
        output.status
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"),
        "child selected exactly one actual test"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_http_process_restart_preserves_saved_and_unsettled_origins_pg() {
    if std::env::var_os("ADK_LEGACY_CHILD_DB").is_some() {
        restart_child().await;
        return;
    }
    let f = Fixture::create(9_727_407).await;
    let pane = InjectPane::new(f.channel, "external");
    run_child(&f, &pane, "legacy-restart-saved", 200, 1).await;
    assert_eq!(pane.records().len(), 1);
    run_child(&f, &pane, "legacy-restart-saved", 200, 0).await;
    assert_eq!(
        pane.records().len(),
        1,
        "fresh process replays the saved response without content"
    );
    sqlx::raw_sql("CREATE FUNCTION legacy_fail_settlement() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'response storage failed'; END $$; CREATE TRIGGER legacy_fail_settlement BEFORE UPDATE ON imsg_reply_obligations FOR EACH ROW WHEN (NEW.admission IS DISTINCT FROM OLD.admission) EXECUTE FUNCTION legacy_fail_settlement()")
        .execute(&f.pool).await.unwrap();
    run_child(&f, &pane, "legacy-restart-unsettled", 503, 1).await;
    assert_eq!(pane.records().len(), 2);
    let pending: String = sqlx::query_scalar(
        "SELECT admission FROM imsg_reply_obligations WHERE origin_id='legacy-restart-unsettled'",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(pending, "admitting");
    run_child(&f, &pane, "legacy-restart-unsettled", 202, 0).await;
    assert_eq!(
        pane.records().len(),
        2,
        "different origins each have one content after restart"
    );
    drop(pane);
    f.close().await;
}
