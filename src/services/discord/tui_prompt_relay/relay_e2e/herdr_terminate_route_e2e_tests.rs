//! A terminate request dropped while its close is in flight: the detached run still retires and
//! settles the captured turn, and the message queued meanwhile starts once on Herdr.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::body::Body;
use axum::http::Request;
use poise::serenity_prelude::MessageId;
use tower::ServiceExt;

use super::discord_mock::user_message;
use super::{CHANNEL_ID, RelayE2eHarness, serenity, wait_until};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::herdr_terminate::test_turn;
use crate::services::discord::queue_io::cancel_backstop_test_support as observed;
use crate::services::provider::ProviderKind;
use crate::services::session_host::herdr_termination_rig as herdr;

const QUEUED: &str = "input queued behind a dropped terminate request";
/// The event backstop that takes over a listener's no-start waits 60s; the start follows it.
const START_WAIT: Duration = Duration::from_secs(120);

fn recent_message_id(low: u64) -> MessageId {
    const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
    let recent_ms = chrono::Utc::now().timestamp_millis() - 30_000 - DISCORD_EPOCH_MS;
    MessageId::new((u64::try_from(recent_ms).unwrap() << 22) | low)
}

#[test]
fn act_route_disconnect_after_close_still_settles_pg() {
    let path = concat!(
        module_path!(),
        "::act_route_disconnect_after_close_still_settles_pg"
    );
    if !herdr::in_child(path, CHANNEL_ID) {
        return;
    }
    let rig = herdr::P11TerminationRig::boot(CHANNEL_ID);
    let install = rig.thread_installer();
    install();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .on_thread_start(install)
        .build()
        .unwrap()
        .block_on(scenario(rig));
}

async fn scenario(rig: herdr::P11TerminationRig) {
    let mut database = None;
    let harness = RelayE2eHarness::start_bound_on(async {
        let fixture = TestPostgresDb::create().await;
        let pool = fixture.connect_and_migrate().await;
        database = Some(fixture);
        pool
    })
    .await;
    let database = database.expect("an isolated PostgreSQL database");
    let pool = harness.shared.pg_pool.clone().expect("PG-backed runtime");
    let (shared, channel) = (harness.shared.clone(), harness.channel_id);
    harness.answer_placeholders_immediately();
    harness.cache_relay_transport();
    shared.restart.reconcile_done.store(true, Ordering::SeqCst);
    shared
        .core
        .lock()
        .await
        .sessions
        .get_mut(&channel)
        .expect("bound fixture channel")
        .channel_name
        .replace("p11-m2-herdr".into());
    std::mem::forget(
        crate::services::claude_tui::hook_server::publish_hook_endpoint(
            "http://127.0.0.1:1".into(),
        ),
    );
    let (store, era) =
        crate::services::herdr_launch::o_store_for_test(harness.root.path(), &[CHANNEL_ID]);
    let mut seeded = store.open_channel(&era, CHANNEL_ID).unwrap().unwrap();
    seeded.set_binding_checkpoint(3).unwrap();

    let provider = ProviderKind::Claude;
    let session_key = crate::services::discord::adk_session::build_adk_session_key(
        &shared, channel, &provider, None,
    )
    .await
    .expect("a namespaced session key");
    let _switches = rig
        .bind(
            &pool,
            &shared.token_hash,
            &session_key,
            herdr::HostedState::Bound,
        )
        .await;
    let token_a = test_turn::start(&shared, channel, 51, "turn-a").await;
    crate::services::discord::queue_io::spawn_turn_completion_idle_queue_listener(
        shared.clone(),
        provider.clone(),
    );
    assert!(
        wait_until(Duration::from_secs(15), || {
            Box::pin(async move { observed::listener_completed_reconciles(channel) >= 1 })
        })
        .await,
        "the listener finished its initial reconcile"
    );
    let registry = Arc::new(HealthRegistry::new());
    registry.register("claude".into(), shared.clone()).await;
    let mut engine_config = crate::config::Config::default();
    engine_config.policies.hot_reload = false;
    let engine = crate::engine::PolicyEngine::new(&engine_config).unwrap();
    let tx = crate::server::ws::new_broadcast();
    let buffer = crate::server::ws::spawn_batch_flusher(tx.clone());
    let config = crate::config::Config::default();
    let app = crate::server::routes::api_router_with_pg(
        engine,
        config,
        tx,
        buffer,
        Some(registry),
        Some(pool.clone()),
    );

    // The request is dropped while Herdr's answer to its close is held.
    let mut hold = rig.hold_next_write();
    let payload = serde_json::json!({"session_key": session_key,
        "execution_nonce": herdr::EXECUTION, "reason": "operator test"});
    let request = tokio::spawn(async move {
        let mut request = Request::builder()
            .method("POST")
            .uri("/herdr/terminate")
            .header("content-type", "application/json")
            .body(Body::from(payload.to_string()))
            .unwrap();
        let peer: std::net::SocketAddr = "127.0.0.1:40000".parse().unwrap();
        request
            .extensions_mut()
            .insert(axum::extract::ConnectInfo(peer));
        app.oneshot(request).await
    });
    tokio::time::timeout(Duration::from_secs(10), &mut hold.reached)
        .await
        .expect("the close reached Herdr")
        .unwrap();
    let writes = rig.writes();
    assert!(
        matches!(writes.as_slice(), [(method, pane)] if method.ends_with(".close") && pane == "w1-1"),
        "only the close reached Herdr before the drop: {writes:?}"
    );
    request.abort();
    let dropped = request.await;
    assert!(dropped.is_err_and(|error| error.is_cancelled()));

    // A message arriving while the close is pending waits in the queue.
    let queued = recent_message_id(711);
    let event = serenity::FullEvent::Message {
        new_message: user_message(queued.get(), QUEUED),
    };
    let intake = super::boxed_intake(&harness.ctx, &event, &harness.clone_data()).await;
    assert!(intake.is_ok(), "{intake:?}");
    let waiting = wait_until(Duration::from_secs(15), || {
        let shared = shared.clone();
        Box::pin(async move {
            crate::services::discord::mailbox_snapshot(&shared, channel)
                .await
                .intervention_queue
                .iter()
                .any(|item| item.message_id == queued)
        })
    })
    .await;
    assert!(waiting, "intake queued the message behind the run");
    hold.release.send(()).unwrap();

    let settled = wait_until(Duration::from_secs(15), || {
        let (pool, key, shared, a) = (
            pool.clone(),
            session_key.clone(),
            shared.clone(),
            token_a.clone(),
        );
        Box::pin(async move {
            let retired = herdr::row(&pool, &key).await
                == Some((herdr::HostedState::Retired, herdr::EXECUTION.into()));
            // The queued message may already run its own turn; A's token is what must be gone.
            let active = test_turn::active(&shared, channel).await;
            let a_settled = active.is_none_or(|active| !Arc::ptr_eq(&active, &a));
            retired && !herdr::held() && a_settled
        })
    })
    .await;
    assert!(
        settled,
        "the detached run retired, released the hold and settled A"
    );
    assert!(!token_a.cancelled.load(Ordering::Relaxed));
    let released = wait_until(Duration::from_secs(15), || {
        let shared = shared.clone();
        Box::pin(async move {
            shared
                .session_transition_lock(channel)
                .try_lock_owned()
                .is_ok()
        })
    })
    .await;
    assert!(released, "the run gave the channel transition back");
    // The close is answered; the launch talks to the server directly.
    drop(hold);

    let started = wait_until(START_WAIT, || {
        let writes = rig.writes();
        Box::pin(async move {
            writes
                .iter()
                .any(|(method, pane)| method == "pane.send_input" && pane == herdr::LAUNCHED_PANE)
        })
    })
    .await;
    if !started {
        let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel).await;
        let queue: Vec<_> = snapshot
            .intervention_queue
            .iter()
            .map(|item| item.message_id)
            .collect();
        panic!(
            "no Herdr start: writes {:?} queue {queue:?} active {} backstop {} messages {:?} starts {}",
            rig.writes(),
            test_turn::active(&shared, channel).await.is_some(),
            observed::backstop_waiting(channel),
            harness.messages(),
            harness.provider_starts()
        );
    }
    tokio::time::sleep(Duration::from_secs(5)).await;
    let writes = rig.writes();
    let count = |wanted: &str| writes.iter().filter(|(method, _)| method == wanted).count();
    assert_eq!(count("workspace.create"), 1, "{writes:?}");
    assert_eq!(count("pane.send_input"), 1, "{writes:?}");
    assert_eq!(
        writes.len(),
        3,
        "close, launch and its command only: {writes:?}"
    );
    assert!(
        harness
            .messages()
            .iter()
            .any(|(reply_to, _)| *reply_to == Some(queued.get())),
        "the started turn answers the queued message: {:?}",
        harness.messages()
    );
    drop(harness);
    pool.close().await;
    database.drop().await;
}
