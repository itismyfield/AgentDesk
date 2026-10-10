//! The actual REST reconciliation handler leaves a held cancellation under its slow owner.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use poise::serenity_prelude::{MessageId, UserId};

use super::{PROVIDER_INPUTS_FILE, RelayE2eHarness, wait_until};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::server::routes::AppState;
use crate::server::routes::dispatched_sessions::reconcile_stale_turn;
use crate::services::discord::queue_io::cancel_backstop_test_support as observed;
use crate::services::discord::zombie_foreground_release::tests::fixtures::missing_tmux_fixture;
use crate::services::discord::{self, health::HealthRegistry, inflight};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::{Intervention, InterventionMode};

const WAIT: Duration = Duration::from_secs(15);
const QUEUED_TEXT: &str = "preserved REST reconciliation question [6016-route-q]";

fn app_state(pool: sqlx::PgPool, registry: Arc<HealthRegistry>) -> AppState {
    let config = crate::config::Config::default();
    let engine = crate::engine::PolicyEngine::new(&config).expect("construct test policy engine");
    let broadcast_tx = crate::eventbus::new_broadcast();
    let batch_buffer = crate::eventbus::spawn_batch_flusher(broadcast_tx.clone());
    AppState {
        pg_pool: Some(pool),
        engine,
        config: Arc::new(config),
        broadcast_tx,
        batch_buffer,
        health_registry: Some(registry),
        cluster_instance_id: None,
    }
}

fn queued(message_id: MessageId) -> Intervention {
    Intervention {
        author_id: UserId::new(super::discord_mock::USER_ID),
        author_is_bot: false,
        message_id,
        queued_generation: discord::runtime_store::process_generation(),
        source_message_ids: vec![message_id],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: QUEUED_TEXT.into(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconcile_stale_turn_held_release_arms_backstop_and_runs_preserved_q_pg() {
    let name = concat!(
        module_path!(),
        "::reconcile_stale_turn_held_release_arms_backstop_and_runs_preserved_q_pg"
    );
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(name)
        || !crate::services::tui_o::cutover::test_override::in_empty_list_process(name)
    {
        return;
    }
    let mut database = None;
    let harness = RelayE2eHarness::start_bound_on(async {
        let fixture = TestPostgresDb::create().await;
        let pool = fixture.connect_and_migrate().await;
        database = Some(fixture);
        pool
    })
    .await;
    let database = database.expect("an isolated PostgreSQL database under the harness env lock");
    let pool = harness
        .shared
        .pg_pool
        .clone()
        .expect("actual PG-backed runtime");
    let (database_name, backend_pid): (String, i32) =
        sqlx::query_as("SELECT current_database(), pg_backend_pid()")
            .fetch_one(&pool)
            .await
            .expect("actual PostgreSQL witness query");
    assert!(database_name.starts_with("agentdesk_db_auto_queue_"));
    eprintln!("T3c actual PostgreSQL database={database_name} backend_pid={backend_pid}");
    let _tmux = missing_tmux_fixture(&harness.root);
    harness
        .shared
        .restart
        .reconcile_done
        .store(true, Ordering::SeqCst);
    harness.answer_placeholders_immediately();
    harness.cache_relay_transport();
    let registry = Arc::new(HealthRegistry::new());
    registry
        .register("claude".into(), harness.shared.clone())
        .await;
    let channel = harness.channel_id;
    let channel_name = "6016-rest-cancel-owner";
    let provider = ProviderKind::Claude;
    let tmux_name = provider.build_tmux_session_name(channel_name);
    let previous_name = {
        let mut core = harness.shared.core.lock().await;
        let session = core
            .sessions
            .get_mut(&channel)
            .expect("bound fixture channel");
        session.channel_name.replace(channel_name.into())
    };
    assert!(
        previous_name.is_none(),
        "the harness's existing adapter is SDK"
    );
    let session_key = format!(
        "{}:{tmux_name}",
        crate::services::platform::hostname_short()
    );
    sqlx::query(
        "INSERT INTO sessions (channel_id, session_key, provider, status, active_dispatch_id,
                               last_heartbeat, session_info)
         VALUES ($1, $2, 'claude', 'turn_active', NULL,
                 NOW() - INTERVAL '1 hour', 'held cancellation fixture')",
    )
    .bind(channel.get().to_string())
    .bind(&session_key)
    .execute(&pool)
    .await
    .expect("insert the stale session handled by the real REST route");
    const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
    let recent_ms = chrono::Utc::now().timestamp_millis() - 30_000 - DISCORD_EPOCH_MS;
    let recent = u64::try_from(recent_ms).expect("after the Discord epoch") << 22;
    let old_message = MessageId::new(recent | 601);
    let queued_message = MessageId::new(recent | 602);
    let token = Arc::new(CancelToken::new());
    token.bind_unmanaged_session_name(&tmux_name);
    assert!(
        harness
            .shared
            .mailbox(channel)
            .try_start_turn(
                token.clone(),
                UserId::new(super::discord_mock::USER_ID),
                old_message,
            )
            .await
    );
    token.cancelled.store(true, Ordering::Relaxed);
    discord::increment_global_active(&harness.shared, "rest_cancel_owner_fixture");
    let mut row = inflight::InflightTurnState::new(
        provider.clone(),
        channel.get(),
        None,
        super::discord_mock::USER_ID,
        old_message.get(),
        old_message.get(),
        "cancelled source owner".into(),
        Some(super::SESSION_UUID.into()),
        Some(tmux_name.clone()),
        None,
        None,
        0,
    );
    row.turn_nonce = token.turn_nonce().map(str::to_owned);
    inflight::save_inflight_state(&row).expect("persist the isolated source owner");
    let row_path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &provider,
        channel.get(),
    );
    let row_before = std::fs::read(&row_path).unwrap();
    let source = queued(queued_message);
    let enqueued = discord::queue_io::with_post_enqueue_idle_queue_kick_suppressed(
        discord::mailbox_enqueue_intervention(&harness.shared, &provider, channel, source),
    )
    .await;
    assert!(enqueued.enqueued);
    let before = harness.mailbox().await;
    assert_eq!(before.intervention_queue.len(), 1);
    assert_eq!(before.intervention_queue[0].text, QUEUED_TEXT);
    assert!(
        !harness
            .shared
            .restart
            .deferred_hook_channels
            .contains_key(&channel)
    );
    let mut completions = harness.subscribe_completions();

    let (status, Json(body)) = tokio::time::timeout(
        WAIT,
        reconcile_stale_turn(
            State(app_state(pool.clone(), registry)),
            Path(session_key.clone()),
        ),
    )
    .await
    .expect("the actual REST reconciliation handler completes")
    .expect("actual REST reconciliation succeeds");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["reconciled"], true);
    assert_eq!(body["qualification"], "stale_heartbeat");
    assert_eq!(body["mailbox_foreground_released"], false);
    assert_eq!(body["mailbox_release_verdict"], "hold_inflight_present");
    let status: String = sqlx::query_scalar("SELECT status FROM sessions WHERE session_key = $1")
        .bind(&session_key)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        status, "idle",
        "the real handler completed its guarded PG update"
    );
    let initial_slot = harness
        .shared
        .restart
        .deferred_hook_channels
        .get(&channel)
        .expect("a held release through the REST handler must arm its slow backstop")
        .value()
        .clone();
    assert_eq!(std::fs::read(&row_path).unwrap(), row_before);
    assert!(
        wait_until(WAIT, move || Box::pin(async move {
            observed::backstop_waiting(channel)
        }))
        .await,
        "the route-owned slow backstop must actually start waiting"
    );
    let before_fire = observed::completed_fires(channel);
    initial_slot.wake.notify_one();
    assert!(
        wait_until(WAIT, {
            let shared = harness.shared.clone();
            let initial_slot = initial_slot.clone();
            move || {
                let shared = shared.clone();
                let initial_slot = initial_slot.clone();
                Box::pin(async move {
                    observed::completed_fires(channel) > before_fire
                        && observed::backstop_waiting(channel)
                        && shared
                            .restart
                            .deferred_hook_channels
                            .get(&channel)
                            .is_some_and(|entry| Arc::ptr_eq(entry.value(), &initial_slot))
                })
            }
        })
        .await,
        "the actual held evaluation must keep the same slow owner waiting for its next cycle"
    );
    let held = harness.mailbox().await;
    assert!(Arc::ptr_eq(held.cancel_token.as_ref().unwrap(), &token));
    assert_eq!(held.active_user_message_id, Some(old_message));
    assert_eq!(held.intervention_queue[0].message_id, queued_message);
    assert_eq!(held.intervention_queue[0].text, QUEUED_TEXT);
    assert_eq!(std::fs::read(&row_path).unwrap(), row_before);
    assert_eq!(harness.provider_starts(), 0);
    assert!(!harness.root.path().join(PROVIDER_INPUTS_FILE).is_file());

    // The route requires the legacy name; the queued turn uses the harness's existing SDK adapter.
    harness
        .shared
        .core
        .lock()
        .await
        .sessions
        .get_mut(&channel)
        .unwrap()
        .channel_name = previous_name;
    std::fs::remove_file(&row_path).expect("remove only the isolated source-owner fixture row");
    let successor = harness
        .shared
        .restart
        .deferred_hook_channels
        .get(&channel)
        .expect("the held route retained its next-cycle owner")
        .value()
        .clone();
    assert!(Arc::ptr_eq(&successor, &initial_slot));
    successor.wake.notify_one();
    let drained = wait_until(WAIT, {
        let shared = harness.shared.clone();
        let messages = harness.mock.messages.clone();
        let inputs = harness.root.path().join(PROVIDER_INPUTS_FILE);
        move || {
            let shared = shared.clone();
            let messages = messages.clone();
            let inputs = inputs.clone();
            Box::pin(async move {
                let actual = shared.mailbox(channel).snapshot().await;
                let submitted = inputs.is_file()
                    && std::fs::read_to_string(&inputs)
                        .unwrap()
                        .contains(QUEUED_TEXT);
                let answered = messages
                    .lock()
                    .unwrap()
                    .values()
                    .any(|(_, text)| text == "ok");
                submitted
                    && answered
                    && actual.cancel_token.is_none()
                    && actual.intervention_queue.is_empty()
                    && actual.pending_user_dispatch.is_none()
            })
        }
    })
    .await;
    assert!(
        drained,
        "the route-owned slow successor must dequeue, claim and submit the preserved Q"
    );
    assert!(harness.root.path().join(PROVIDER_INPUTS_FILE).is_file());
    assert_eq!(
        harness
            .provider_inputs()
            .concat()
            .matches(QUEUED_TEXT)
            .count(),
        1
    );
    assert_eq!(
        harness
            .messages()
            .iter()
            .filter(|(_, text)| text == "ok")
            .count(),
        1
    );
    assert!(harness.durable_queue().is_empty());
    assert!(
        wait_until(WAIT, move || Box::pin(async move {
            observed::completed_fires(channel) >= before_fire + 2
        }))
        .await,
        "both route-owned slow cycles must complete"
    );
    assert_eq!(
        harness.shared.restart.global_active.load(Ordering::Relaxed),
        0
    );
    tokio::time::timeout(WAIT, async {
        loop {
            let event = completions
                .recv()
                .await
                .expect("actual turn completion event");
            if event.channel_id == channel
                && event.turn_id == Some(queued_message.get())
                && event.queue_is_eligible()
            {
                break;
            }
        }
    })
    .await
    .expect("the real Q claim finishes through its production finalizer");
    assert!(
        std::fs::read_to_string(harness.root.path().join("tmux.calls"))
            .unwrap()
            .contains("has-session"),
        "the cancelled A's binding had measured Missing terminal evidence"
    );
    assert!(
        harness.unhandled_requests().is_empty(),
        "{:?}",
        harness.unhandled_requests()
    );
    pool.close().await;
    database.drop().await;
}
