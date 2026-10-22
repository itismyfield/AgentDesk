//! The forced turn cancel through the production router on the fake Herdr rig and a throwaway PG:
//! its dispatch, session and queue effects happen only once the scoped run retired and settled.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use futures::FutureExt;
use serde_json::{Value, json};

use super::super::{
    OperatorTerminate, TerminationResult, probe_after_capture, probe_after_close,
    probe_after_fence, probe_before_postlude, test_queue, test_turn,
};
use super::terminate_for_http;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::{ChannelId, SharedData, inflight};
use crate::services::provider::ProviderKind;
use crate::services::session_host::herdr_termination_rig as herdr;
use crate::services::turn_orchestrator::{Intervention, InterventionMode};

const CHANNEL: u64 = 6_772_101;
const OTHER: u64 = 6_772_109;
const AUTH: &str = "e07-herdr-force";
const DISPATCH: &str = "dispatch-e07";
const SUCCESSOR: &str = "dispatch-e07-successor";
const JSON: (&str, &str) = ("content-type", "application/json");

/// A postlude with nothing to release.
impl super::Released for () {
    fn released(self) -> futures::future::BoxFuture<'static, Self> {
        async {}.boxed()
    }
}

/// The rig for test `name` in its own child, on a current-thread runtime so every probe runs.
fn rig_child(name: &str) -> Option<(herdr::P11TerminationRig, tokio::runtime::Runtime)> {
    rig_child_with(name, false)
}

fn rig_child_with(
    name: &str,
    provider_alive: bool,
) -> Option<(herdr::P11TerminationRig, tokio::runtime::Runtime)> {
    if !herdr::in_child(&format!("{}::{name}", module_path!()), CHANNEL) {
        return None;
    }
    let rig = herdr::P11TerminationRig::boot_with(CHANNEL, provider_alive);
    rig.thread_installer()();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .on_thread_start(rig.thread_installer())
        .build()
        .unwrap();
    Some((rig, runtime))
}

fn channel() -> ChannelId {
    ChannelId::new(CHANNEL)
}

/// A probe that records it ran; a test asserts the flag so a probe never passes unrun.
fn ran() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

/// The bound execution's row on a throwaway PG, its runtime registered and the switch on.
struct Node {
    db: TestPostgresDb,
    pool: sqlx::PgPool,
    shared: Arc<SharedData>,
    registry: Arc<HealthRegistry>,
    key: String,
    _switches: herdr::LiveSwitches,
}

impl Node {
    /// The execution in `state`, its row the channel's active turn on `dispatch`.
    async fn bind(
        rig: &herdr::P11TerminationRig,
        state: herdr::HostedState,
        dispatch: Option<&str>,
    ) -> Self {
        // A bind earlier in this child wrote the same launch context; the rig writes it anew.
        let root = crate::config::runtime_root().unwrap();
        let context = format!("runtime/binding_contexts/claude/{}.json", herdr::EXECUTION);
        let _ = std::fs::remove_file(root.join(context));
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let (shared, registry) =
            super::super::super::host_teardown_gate::test_support::runtime(&pool).await;
        let key = format!(
            "claude/{}/{}:AgentDesk-claude-e07",
            shared.token_hash,
            herdr::NODE
        );
        let _switches = rig.bind(&pool, &shared.token_hash, &key, state).await;
        let node = Self {
            db,
            pool,
            shared,
            registry,
            key,
            _switches,
        };
        node.sql("UPDATE sessions SET status = 'turn_active', active_dispatch_id = $2, last_heartbeat = NOW() WHERE session_key = $1", dispatch).await;
        for id in [DISPATCH, SUCCESSOR] {
            sqlx::query("INSERT INTO task_dispatches (id, status) VALUES ($1, 'dispatched')")
                .bind(id)
                .execute(&node.pool)
                .await
                .unwrap();
        }
        node
    }

    async fn sql(&self, statement: &'static str, value: Option<&str>) {
        sqlx::query(statement)
            .bind(&self.key)
            .bind(value)
            .execute(&self.pool)
            .await
            .unwrap();
    }

    fn app(&self) -> axum::Router {
        let mut config = crate::config::Config::default();
        config.server.auth_token = Some(AUTH.into());
        let (pool, registry) = (Some(self.pool.clone()), Some(self.registry.clone()));
        crate::server::routes::home_stop_test_app(config, pool, registry, herdr::NODE)
    }

    async fn post(&self, uri: &str, body: Value) -> (StatusCode, Value) {
        let bearer = format!("Bearer {AUTH}");
        let headers = [("authorization", bearer.as_str()), JSON];
        let post = crate::server::routes::home_stop_test_post;
        post(&self.app(), uri, &headers, &body.to_string()).await
    }

    async fn cancel_on(&self, channel: u64, force: bool) -> (StatusCode, Value) {
        let uri = format!("/turns/{channel}/cancel");
        self.post(&uri, json!({"force": force})).await
    }

    async fn cancel(&self) -> (StatusCode, Value) {
        self.cancel_on(CHANNEL, true).await
    }

    /// The forced cancel as its own task, so the test can drop or outlive its request.
    fn spawn_cancel(&self) -> tokio::task::JoinHandle<(StatusCode, Value)> {
        let app = self.app();
        tokio::spawn(async move {
            let bearer = format!("Bearer {AUTH}");
            let headers = [("authorization", bearer.as_str()), JSON];
            let uri = format!("/turns/{CHANNEL}/cancel");
            let body = json!({"force": true}).to_string();
            crate::server::routes::home_stop_test_post(&app, &uri, &headers, &body).await
        })
    }

    /// The turn `nonce` runs on message `id` for `dispatch`, as a dispatched turn's inflight row
    /// records it.
    async fn start(&self, id: u64, nonce: &str, dispatch: Option<&str>) {
        test_turn::start(&self.shared, channel(), id, nonce).await;
        let mut row = inflight::load_inflight_state(&ProviderKind::Claude, CHANNEL).unwrap();
        row.dispatch_id = dispatch.map(str::to_string);
        inflight::save_inflight_state(&row).unwrap();
    }

    /// Queues user message `id` behind whatever the channel holds.
    async fn enqueue(shared: &Arc<SharedData>, id: u64, text: &str) {
        let persistence = super::super::super::queue_persistence_context(
            shared,
            &ProviderKind::Claude,
            channel(),
        );
        let queued = shared
            .mailbox(channel())
            .enqueue(intervention(id, text), persistence)
            .await;
        assert!(queued.enqueued, "{text} queued");
    }

    async fn effects(&self, rig: &herdr::P11TerminationRig) -> Effects {
        let session = sqlx::query_as(
            "SELECT status, active_dispatch_id FROM sessions WHERE session_key = $1",
        )
        .bind(&self.key)
        .fetch_one(&self.pool);
        let dispatches = sqlx::query_as("SELECT id, status FROM task_dispatches ORDER BY id");
        let dead = sqlx::query_scalar(
            "SELECT message_id FROM relay_dead_letter WHERE channel_id = $1 ORDER BY message_id",
        )
        .bind(CHANNEL.to_string());
        let queued = match self.shared.mailbox_peek(channel()) {
            Some(mailbox) => {
                let state = mailbox.snapshot().await;
                state
                    .intervention_queue
                    .iter()
                    .map(|i| i.text.clone())
                    .collect()
            }
            None => Vec::new(),
        };
        Effects {
            session: session.await.unwrap(),
            dispatches: dispatches.fetch_all(&self.pool).await.unwrap(),
            dead_lettered: dead.fetch_all(&self.pool).await.unwrap(),
            queued,
            row: herdr::row(&self.pool, &self.key).await,
            held: herdr::held(),
            writes: rig.writes(),
        }
    }

    async fn finish(self) {
        self.pool.close().await;
        self.db.drop().await;
    }
}

/// Everything a forced cancel may change: the session row, every dispatch, the dead letters, the
/// channel queue, the hosted row, the input hold and every write Herdr received.
#[derive(Debug, Clone, PartialEq)]
struct Effects {
    session: (String, Option<String>),
    dispatches: Vec<(String, String)>,
    dead_lettered: Vec<Option<String>>,
    queued: Vec<String>,
    row: Option<(herdr::HostedState, String)>,
    held: bool,
    writes: Vec<(String, String)>,
}

impl Effects {
    /// Herdr received closes of the rig's pane only: never a provider input.
    fn closes(&self) -> usize {
        let close =
            |(method, pane): &(String, String)| method.ends_with(".close") && pane == "w1-1";
        assert!(self.writes.iter().all(close), "{:?}", self.writes);
        self.writes.len()
    }
}

fn dispatches(first: &str, second: &str) -> Vec<(String, String)> {
    vec![
        (DISPATCH.into(), first.into()),
        (SUCCESSOR.into(), second.into()),
    ]
}

fn dead(ids: &[u64]) -> Vec<Option<String>> {
    ids.iter().map(|id| Some(id.to_string())).collect()
}

fn bound() -> Option<(herdr::HostedState, String)> {
    Some((herdr::HostedState::Bound, herdr::EXECUTION.to_string()))
}

fn retired() -> Option<(herdr::HostedState, String)> {
    Some((herdr::HostedState::Retired, herdr::EXECUTION.to_string()))
}

fn active(dispatch: Option<&str>) -> (String, Option<String>) {
    ("turn_active".into(), dispatch.map(str::to_string))
}

fn disconnected() -> (String, Option<String>) {
    ("disconnected".into(), None)
}

fn intervention(id: u64, text: &str) -> Intervention {
    use serenity::model::id::{MessageId, UserId};
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: super::super::super::runtime_store::process_generation(),
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: text.into(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

/// The termination a refused or failed forced cancel reports, and its status.
fn termination(status: StatusCode, reply: &Value) -> (StatusCode, Value) {
    (status, reply["context"]["termination"].clone())
}

fn result(name: &str, detail: &str) -> Value {
    json!({"result": name, "detail": detail})
}

fn cancelled_events() -> usize {
    let events = crate::services::observability::events::recent(100_000).into_iter();
    let ours = events
        .filter(|event| event.event_type == "turn_cancelled" && event.channel_id == Some(CHANNEL));
    ours.count()
}

async fn wait_for(what: &str, mut done: impl AsyncFnMut() -> bool) {
    for _ in 0..300 {
        if done().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

// The positive control: a forced cancel of a dispatched turn on a row the turn-start writer marked
// active retires it, then cancels, disconnects, purges and records once, with the existing keys.
#[test]
fn e07_force_retires_then_applies_effects_once_pg() {
    let Some((rig, runtime)) = rig_child("e07_force_retires_then_applies_effects_once_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        // The turn start's origin marker, as the bridge posts it, is what marks the row active.
        node.sql("UPDATE sessions SET status = 'idle', last_heartbeat = NULL WHERE session_key = $1 AND $2::TEXT IS NULL", None).await;
        let marker = json!({"session_key": node.key, "status": "turn_active", "provider": "claude",
            "channel_id": CHANNEL.to_string(), "turn_start_nonce": "turn-a", "dispatched_origin": true});
        let (status, reply) = node.post("/dispatched-sessions/webhook", marker).await;
        assert!(status.is_success(), "{status}: {reply}");
        assert_eq!(node.effects(&rig).await.session, active(Some(DISPATCH)));
        node.start(51, "turn-a", Some(DISPATCH)).await;
        test_queue::queue_original(&node.shared, channel()).await;
        let before = cancelled_events();

        let (status, reply) = node.cancel().await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        let keys = [
            "ok",
            "tmux_killed",
            "lifecycle_path",
            "queued_remaining",
            "queue_purged",
            "queue_dead_lettered_message_ids",
            "queue_unpreserved_message_ids",
            "dispatch_cancelled",
            "turn_status",
            "session_key",
            "termination",
        ];
        let claims: Value = keys.iter().map(|k| (k.to_string(), reply[k].clone())).collect();
        assert_eq!(
            claims,
            json!({"ok": true, "tmux_killed": false, "lifecycle_path": "herdr-terminate",
                "queued_remaining": 0, "queue_purged": 1, "queue_dead_lettered_message_ids": [61],
                "queue_unpreserved_message_ids": [], "dispatch_cancelled": DISPATCH,
                "turn_status": "cancelled", "session_key": node.key,
                "termination": result("retired", "retired")})
        );
        let after = node.effects(&rig).await;
        assert_eq!(after.closes(), 1);
        assert_eq!(after.session, disconnected());
        assert_eq!(after.dispatches, dispatches("cancelled", "dispatched"));
        assert_eq!(after.dead_lettered, dead(&[61]));
        assert!(after.queued.is_empty());
        assert_eq!((after.row, after.held), (retired(), false));
        assert!(test_turn::active(&node.shared, channel()).await.is_none());
        assert_eq!(cancelled_events(), before + 1, "one cancel observation");
        node.finish().await;
    });
}

// The real handler held between its close and the fresh end check: nothing of the session,
// dispatch or queue has moved, and the provider got no input; only the retire releases them.
#[test]
fn e07_force_waits_for_terminated_handler_pg() {
    let Some((rig, runtime)) = rig_child("e07_force_waits_for_terminated_handler_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        test_queue::queue_original(&node.shared, channel()).await;
        let untouched = node.effects(&rig).await;
        let token = test_turn::active(&node.shared, channel()).await.unwrap();
        let mut pause = rig.pause_end_check();
        let request = node.spawn_cancel();
        tokio::time::timeout(Duration::from_secs(10), &mut pause.arrived)
            .await
            .expect("the run reached its end check after the close")
            .unwrap();
        let held = node.effects(&rig).await;
        assert_eq!(held.closes(), 1, "the close was sent");
        assert_eq!(
            Effects {
                writes: untouched.writes.clone(),
                ..held
            },
            untouched
        );
        let current = test_turn::active(&node.shared, channel()).await;
        assert!(
            current.is_some_and(|t| Arc::ptr_eq(&t, &token)),
            "the turn is still its own"
        );
        assert_eq!(test_queue::take_queued(&node.shared, channel()).await, None);
        pause.release.send(()).unwrap();
        let (status, reply) = request.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{reply}");
        let after = node.effects(&rig).await;
        assert_eq!((after.closes(), after.session.clone()), (1, disconnected()));
        assert_eq!(after.dispatches, dispatches("cancelled", "dispatched"));
        assert_eq!(after.dead_lettered, dead(&[61]));
        node.finish().await;
    });
}

// The postlude runs while the run still holds the channel's transition: input arriving then is
// queued, recorded and purged with the rest, never prompted, and no other runtime is held.
#[test]
fn force_postlude_runs_before_transition_release_pg() {
    let Some((rig, runtime)) = rig_child("force_postlude_runs_before_transition_release_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        test_queue::queue_original(&node.shared, channel()).await;
        let (flag, shared) = (ran(), node.shared.clone());
        let started = Arc::new(Mutex::new(None));
        let (probe_flag, probe_started) = (flag.clone(), started.clone());
        probe_before_postlude(Box::new(move || {
            Box::pin(async move {
                let lease =
                    super::super::super::session_transition::SessionTransitionLease::try_acquire(
                        &shared,
                        channel(),
                    );
                assert!(lease.is_err(), "the channel transition is still held");
                let other = super::super::super::make_shared_data_for_tests();
                assert!(
                    other
                        .session_transition_lock(channel())
                        .try_lock_owned()
                        .is_ok(),
                    "another runtime's transition on the channel is free"
                );
                Node::enqueue(&shared, 62, "successor input").await;
                *probe_started.lock().unwrap() = Some(Instant::now());
                probe_flag.store(true, Ordering::SeqCst);
            })
        }));
        let (status, reply) = node.cancel().await;
        assert!(
            flag.load(Ordering::SeqCst),
            "the postlude window was observed"
        );
        let postlude = started.lock().unwrap().unwrap().elapsed();
        assert_eq!(status, StatusCode::OK, "{reply}");
        assert!(
            postlude < Duration::from_millis(2_900),
            "the postlude waits on nothing: {postlude:?}"
        );
        let after = node.effects(&rig).await;
        assert_eq!(
            after.closes(),
            1,
            "the successor input never reached the pane"
        );
        assert_eq!(after.dead_lettered, dead(&[61, 62]));
        assert!(after.queued.is_empty());
        node.finish().await;
    });
}

// Input sent while the close and the end check run is part of what the purge removes, so it is
// recorded as removed, never silently lost.
#[test]
fn e07_purge_records_input_during_termination_pg() {
    let Some((rig, runtime)) = rig_child("e07_purge_records_input_during_termination_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        test_queue::queue_original(&node.shared, channel()).await;
        let (flag, shared) = (ran(), node.shared.clone());
        let probe_flag = flag.clone();
        probe_after_close(Box::new(move || {
            Box::pin(async move {
                Node::enqueue(&shared, 62, "sent during the close").await;
                probe_flag.store(true, Ordering::SeqCst);
            })
        }));
        let (status, reply) = node.cancel().await;
        assert!(flag.load(Ordering::SeqCst));
        assert_eq!(status, StatusCode::OK, "{reply}");
        assert_eq!(reply["queue_dead_lettered_message_ids"], json!([61, 62]));
        assert_eq!(reply["queue_unpreserved_message_ids"], json!([]));
        assert_eq!(node.effects(&rig).await.dead_lettered, dead(&[61, 62]));
        node.finish().await;
    });
}

// The disconnect matches the retired execution field by field: an unrelated field added to the
// stored record still disconnects its row, while a successor execution's row is left alone.
#[test]
fn e07_retired_postlude_field_comparison_pg() {
    let Some((rig, runtime)) = rig_child("e07_retired_postlude_field_comparison_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let (flag, pool, key) = (ran(), node.pool.clone(), node.key.clone());
        let probe_flag = flag.clone();
        probe_before_postlude(Box::new(move || {
            Box::pin(async move {
                sqlx::query("UPDATE sessions SET hosted_execution = jsonb_set(hosted_execution, '{unrelated}', '1') WHERE session_key = $1")
                    .bind(&key).execute(&pool).await.unwrap();
                probe_flag.store(true, Ordering::SeqCst);
            })
        }));
        let (status, reply) = node.cancel().await;
        assert!(flag.load(Ordering::SeqCst));
        assert_eq!(status, StatusCode::OK, "{reply}");
        assert_eq!(node.effects(&rig).await.session, disconnected());
        node.finish().await;

        // A successor execution took the row before the postlude: it keeps its row and queue.
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        test_queue::queue_original(&node.shared, channel()).await;
        let (flag, pool, key) = (ran(), node.pool.clone(), node.key.clone());
        let probe_flag = flag.clone();
        probe_before_postlude(Box::new(move || {
            Box::pin(async move {
                sqlx::query("UPDATE sessions SET hosted_execution = jsonb_set(jsonb_set(jsonb_set(hosted_execution, '{state}', '\"bound\"'), '{execution_nonce}', '\"fedcba9876543210fedcba9876543210\"'), '{source_ref,execution_nonce}', '\"fedcba9876543210fedcba9876543210\"') WHERE session_key = $1")
                    .bind(&key).execute(&pool).await.unwrap();
                probe_flag.store(true, Ordering::SeqCst);
            })
        }));
        let (status, reply) = node.cancel().await;
        assert!(flag.load(Ordering::SeqCst));
        assert_eq!(
            termination(status, &reply),
            (StatusCode::INTERNAL_SERVER_ERROR, result("retired", "retired"))
        );
        assert!(reply["error"].as_str().unwrap().contains("session row moved on"), "{reply}");
        let after = node.effects(&rig).await;
        // The bound dispatch's own cancel ran first; the disconnect left the successor's row.
        assert_eq!(after.session, ("idle".into(), None), "never disconnected");
        assert_eq!(after.dispatches, dispatches("cancelled", "dispatched"));
        assert_eq!(after.queued, [test_queue::ORIGINAL], "nothing after the refusal ran");
        assert!(after.dead_lettered.is_empty());
        node.finish().await;
    });
}

// The selected dispatch binds the run: another dispatch's captured turn, a row whose dispatch moved
// under the fence, or a turn that gave way is refused before any close, the running turn kept.
#[test]
fn e07_lookup_capture_dispatch_change_refuses_pg() {
    let Some((rig, runtime)) = rig_child("e07_lookup_capture_dispatch_change_refuses_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        // The turn running when the run captures it is already the successor's dispatch.
        node.start(61, "turn-b", Some(SUCCESSOR)).await;
        let untouched = node.effects(&rig).await;
        let (status, reply) = node.cancel().await;
        let refused = result("refused", "captured turn runs another dispatch");
        assert_eq!(termination(status, &reply), (StatusCode::CONFLICT, refused));
        assert_eq!(node.effects(&rig).await, untouched);
        let b = test_turn::active(&node.shared, channel())
            .await
            .expect("B runs");
        assert_eq!(b.turn_nonce(), Some("turn-b"));
        assert!(!b.cancelled.load(Ordering::SeqCst));

        // The captured turn is the selected dispatch's, but the row moved on under the fence.
        let node = Node::bind_again(node, &rig).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let untouched = node.effects(&rig).await;
        let (flag, pool, key) = (ran(), node.pool.clone(), node.key.clone());
        let probe_flag = flag.clone();
        probe_after_fence(Box::new(move || {
            Box::pin(async move {
                sqlx::query("UPDATE sessions SET active_dispatch_id = $2 WHERE session_key = $1")
                    .bind(&key)
                    .bind(SUCCESSOR)
                    .execute(&pool)
                    .await
                    .unwrap();
                probe_flag.store(true, Ordering::SeqCst);
            })
        }));
        let (status, reply) = node.cancel().await;
        assert!(flag.load(Ordering::SeqCst));
        let refused = result("refused", "row changed under fence");
        assert_eq!(termination(status, &reply), (StatusCode::CONFLICT, refused));
        let after = node.effects(&rig).await;
        assert_eq!(
            after,
            Effects {
                session: active(Some(SUCCESSOR)),
                ..untouched
            }
        );

        // The captured turn ended naturally and the successor's turn took the channel.
        let node = Node::bind_again(node, &rig).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let a = test_turn::active(&node.shared, channel()).await.unwrap();
        let (flag, shared) = (ran(), node.shared.clone());
        let probe_flag = flag.clone();
        probe_after_capture(Box::new(move || {
            Box::pin(async move {
                test_turn::finish_naturally(&shared, channel(), 51, "turn-a", &a).await;
                test_turn::start(&shared, channel(), 61, "turn-b").await;
                probe_flag.store(true, Ordering::SeqCst);
            })
        }));
        let (status, reply) = node.cancel().await;
        assert!(flag.load(Ordering::SeqCst));
        let refused = result("refused", "turn changed under transition");
        assert_eq!(termination(status, &reply), (StatusCode::CONFLICT, refused));
        let after = node.effects(&rig).await;
        assert_eq!((after.closes(), after.row.clone()), (0, bound()));
        assert_eq!(after.dispatches, dispatches("dispatched", "dispatched"));
        let b = test_turn::active(&node.shared, channel())
            .await
            .expect("B runs");
        assert_eq!(b.turn_nonce(), Some("turn-b"));
        node.finish().await;
    });
}

impl Node {
    /// A fresh database and runtime for the next case, the previous one dropped.
    async fn bind_again(previous: Self, rig: &herdr::P11TerminationRig) -> Self {
        previous.finish().await;
        Self::bind(rig, herdr::HostedState::Bound, Some(DISPATCH)).await
    }
}

// Every refusal or failure leaves session, dispatches and queue as they were, a 500 past the close;
// only a retired, settled run reaches the postlude, and with no actor no queue proof is claimed.
#[test]
fn force_terminal_failure_matrix_pg() {
    let Some((rig, runtime)) = rig_child("force_terminal_failure_matrix_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let refused_case = async |node: &Node, expected: (StatusCode, Value), closes: usize| {
            let untouched = node.effects(&rig).await;
            let (status, reply) = node.cancel().await;
            assert_eq!(termination(status, &reply), expected, "{reply}");
            let after = node.effects(&rig).await;
            assert_eq!(after.closes(), untouched.closes() + closes);
            assert_eq!(Effects { writes: untouched.writes.clone(), ..after }, untouched);
        };
        let conflict = |name: &str, detail: &str| (StatusCode::CONFLICT, result(name, detail));
        let failed = |detail: &str| (StatusCode::INTERNAL_SERVER_ERROR, result("indeterminate", detail));

        // A stored Retired record is refused: no close and no replay of anything.
        let node = Node::bind(&rig, herdr::HostedState::Retired, Some(DISPATCH)).await;
        test_queue::queue_original(&node.shared, channel()).await;
        refused_case(&node, conflict("refused", "execution identity differs"), 0).await;
        node.finish().await;

        // An execution another node owns is never closed from here.
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        node.sql("UPDATE sessions SET hosted_execution = jsonb_set(jsonb_set(hosted_execution, '{owner,owner_node}', '\"other-node\"'), '{location,execution_node}', '\"other-node\"') WHERE session_key = $1 AND $2::TEXT IS NULL", None).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        test_queue::queue_original(&node.shared, channel()).await;
        refused_case(&node, conflict("refused", "execution identity differs"), 0).await;

        // A transition held by someone else past the wait is a refusal.
        let node = Node::bind_again(node, &rig).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        test_queue::queue_original(&node.shared, channel()).await;
        let held = node.shared.session_transition_lock(channel()).lock_owned().await;
        refused_case(&node, conflict("refused", "transition busy"), 0).await;
        drop(held);

        // Herdr asking for a confirmation is a typed refusal after one close; nothing is resent.
        rig.require_confirmation(true);
        let detail = "herdr asks for a confirmation the service never gives";
        refused_case(&node, conflict("confirmation_required", detail), 1).await;
        rig.require_confirmation(false);

        // A close whose endpoint then vanished is a 500 that applies nothing.
        let flag = ran();
        let probe_flag = flag.clone();
        probe_after_close(Box::new(move || {
            Box::pin(async move {
                herdr::clear_endpoints_here();
                probe_flag.store(true, Ordering::SeqCst);
            })
        }));
        refused_case(&node, failed("no local endpoint after the close"), 1).await;
        assert!(flag.load(Ordering::SeqCst));
        rig.thread_installer()();

        // The pane is gone from here on: a live row over it retires with no close at all.
        // A successor taking the row before the retire leaves it the successor's.
        let closed = node.effects(&rig).await.closes();
        let node = Node::bind_again(node, &rig).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        test_queue::queue_original(&node.shared, channel()).await;
        let (flag, pool, key) = (ran(), node.pool.clone(), node.key.clone());
        let probe_flag = flag.clone();
        probe_after_close(Box::new(move || {
            Box::pin(async move {
                sqlx::query("UPDATE sessions SET hosted_execution = jsonb_set(jsonb_set(hosted_execution, '{execution_nonce}', '\"fedcba9876543210fedcba9876543210\"'), '{source_ref,execution_nonce}', '\"fedcba9876543210fedcba9876543210\"') WHERE session_key = $1")
                    .bind(&key).execute(&pool).await.unwrap();
                probe_flag.store(true, Ordering::SeqCst);
            })
        }));
        let (status, reply) = node.cancel().await;
        assert!(flag.load(Ordering::SeqCst));
        assert_eq!(termination(status, &reply), failed("retire CAS stale"));
        let after = node.effects(&rig).await;
        assert_eq!((after.closes(), after.session.clone()), (closed, active(Some(DISPATCH))));
        assert_eq!(after.dispatches, dispatches("dispatched", "dispatched"));
        assert_eq!((after.queued, after.held), (vec![test_queue::ORIGINAL.to_string()], true));

        // The retire landed but the turn's settlement failed: past the retire, still no postlude.
        let node = Node::bind_again(node, &rig).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        test_queue::queue_original(&node.shared, channel()).await;
        test_turn::arm_settlement_panic();
        let (status, reply) = node.cancel().await;
        let (status, termination) = termination(status, &reply);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{reply}");
        assert_eq!(termination["result"], "indeterminate");
        let detail = termination["detail"].as_str().unwrap();
        assert!(detail.starts_with("retired, hold released; "), "{detail}");
        let after = node.effects(&rig).await;
        assert_eq!((after.row.clone(), after.held), (retired(), false));
        assert_eq!(after.session, active(Some(DISPATCH)));
        assert_eq!(after.dispatches, dispatches("dispatched", "dispatched"));
        assert_eq!(after.queued, [test_queue::ORIGINAL]);
        assert!(after.dead_lettered.is_empty());

        // An idle execution with one queued input: its actor's queue is purged and recorded.
        node.finish().await;
        let node = Node::bind(&rig, herdr::HostedState::Bound, None).await;
        test_queue::queue_original(&node.shared, channel()).await;
        let (status, reply) = node.cancel().await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        assert_eq!((reply["ok"].clone(), reply["queue_purged"].clone()), (json!(true), json!(1)));
        assert_eq!(reply["dispatch_cancelled"], Value::Null);
        let after = node.effects(&rig).await;
        assert_eq!((after.closes(), after.session.clone()), (closed, disconnected()));
        assert_eq!(after.dead_lettered, dead(&[61]));
        node.finish().await;

        // No mailbox at all: the retire stands, with no queue proof claimed.
        let node = Node::bind(&rig, herdr::HostedState::Bound, None).await;
        let (status, reply) = node.cancel().await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        let claims = (&reply["ok"], &reply["queue_purged"], &reply["queue_loss_recorded"]);
        assert_eq!(claims, (&json!(true), &Value::Null, &json!(false)));
        assert!(node.shared.mailbox_peek(channel()).is_none(), "no actor was created");
        assert_eq!(node.effects(&rig).await.session, disconnected());
        node.finish().await;

        // The switch read once more at the adapter: off there answers Disabled and runs nothing.
        let node = Node::bind(&rig, herdr::HostedState::Bound, None).await;
        node._switches.set_terminate(None);
        let request = OperatorTerminate { session_key: node.key.clone(), execution_nonce: herdr::EXECUTION.into() };
        let calls = Arc::new(AtomicUsize::new(0));
        let postlude_calls = calls.clone();
        let (result, applied) = terminate_for_http(&node.registry, node.pool.clone(), (request, e08::identity::unbound(&rig, &node)), None, "test", move |_| {
            postlude_calls.fetch_add(1, Ordering::SeqCst);
            async {}.boxed()
        }).await;
        assert_eq!((result, applied, calls.load(Ordering::SeqCst)), (TerminationResult::Disabled, None, 0));
        node.finish().await;
    });
}

// A provider that outlives the close is a refusal that applies nothing and closes nothing more.
#[test]
fn force_terminal_failure_matrix_provider_alive_pg() {
    let name = "force_terminal_failure_matrix_provider_alive_pg";
    let Some((rig, runtime)) = rig_child_with(name, true) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        test_queue::queue_original(&node.shared, channel()).await;
        let untouched = node.effects(&rig).await;
        let (status, reply) = node.cancel().await;
        let survived = result("provider_survived_close", "the provider outlived the close");
        assert_eq!(
            termination(status, &reply),
            (StatusCode::CONFLICT, survived)
        );
        let after = node.effects(&rig).await;
        assert_eq!(after.closes(), 1);
        assert_eq!(
            Effects {
                writes: untouched.writes.clone(),
                ..after
            },
            untouched
        );
        node.finish().await;
    });
}

// A dropped request's run still retires and applies its effects, once. A request repeated after
// the retire, as a restarted client would send it, replays nothing, and two at once retire once.
#[test]
fn force_duplicate_abort_restart_pg() {
    let Some((rig, runtime)) = rig_child("force_duplicate_abort_restart_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        test_queue::queue_original(&node.shared, channel()).await;
        let mut hold = rig.hold_next_write();
        let request = node.spawn_cancel();
        tokio::time::timeout(Duration::from_secs(10), &mut hold.reached)
            .await
            .expect("the close reached Herdr")
            .unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        hold.release.send(()).unwrap();
        wait_for("the dropped run's effects", async || {
            node.effects(&rig).await.session == disconnected()
        })
        .await;
        let after = node.effects(&rig).await;
        assert_eq!(after.dispatches, dispatches("cancelled", "dispatched"));
        assert_eq!((after.dead_lettered.clone(), after.row.clone()), (dead(&[61]), retired()));

        // The row reads active again and new input waits: a repeat neither closes nor purges.
        node.sql("UPDATE sessions SET status = 'turn_active', active_dispatch_id = $2 WHERE session_key = $1", Some(SUCCESSOR)).await;
        Node::enqueue(&node.shared, 63, "after the retire").await;
        let before = node.effects(&rig).await;
        let (status, reply) = node.cancel().await;
        let refused = result("refused", "execution identity differs");
        assert_eq!(termination(status, &reply), (StatusCode::CONFLICT, refused));
        assert_eq!(node.effects(&rig).await, before);
        drop(hold);
        node.finish().await;

        // Two requests at once on a live row whose pane already ended: one retire, one refusal.
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        test_queue::queue_original(&node.shared, channel()).await;
        let (first, second) = tokio::join!(node.cancel(), node.cancel());
        let mut statuses = [first.0, second.0];
        statuses.sort();
        assert_eq!(statuses, [StatusCode::OK, StatusCode::CONFLICT], "{first:?} {second:?}");
        let after = node.effects(&rig).await;
        assert_eq!(after.dead_lettered, dead(&[61]), "recorded once");
        assert_eq!(after.dispatches, dispatches("cancelled", "dispatched"));
        node.finish().await;
    });
}

// The home command a forced cancel was admitted under stays open while its detached run lives,
// past a dropped request, and closes once the run ends.
#[test]
fn force_home_permit_survives_http_abort_pg() {
    let Some((rig, runtime)) = rig_child("force_home_permit_survives_http_abort_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        use crate::db::o_channel_homes::HomeState;
        use crate::services::cluster::channel_home;
        let home = channel_home::register_for_test(CHANNEL, Some(HomeState::Worker));
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let mut hold = rig.hold_next_write();
        let request = node.spawn_cancel();
        tokio::time::timeout(Duration::from_secs(10), &mut hold.reached)
            .await
            .expect("the close reached Herdr")
            .unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        assert_eq!(
            home.commands_in_flight(),
            1,
            "the detached run holds the command"
        );
        hold.release.send(()).unwrap();
        wait_for("the run's end", async || home.commands_in_flight() == 0).await;
        assert_eq!(node.effects(&rig).await.session, disconnected());
        channel_home::unregister(home.channel_id());
        drop(hold);
        node.finish().await;
    });
}

// While a forced cancel waits on its end check, another channel of the same runtime takes its
// queued input and its own transition: the run waits on nothing outside its channel.
#[test]
fn force_o_and_legacy_output_continuity_pg() {
    let Some((rig, runtime)) = rig_child("force_o_and_legacy_output_continuity_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let other = ChannelId::new(OTHER);
        test_queue::queue_original(&node.shared, other).await;
        let mut pause = rig.pause_end_check();
        let request = node.spawn_cancel();
        tokio::time::timeout(Duration::from_secs(10), &mut pause.arrived)
            .await
            .expect("the run reached its end check")
            .unwrap();
        let free = node.shared.session_transition_lock(other).try_lock_owned();
        assert!(free.is_ok(), "another channel's transition is free");
        drop(free);
        let taken = test_queue::take_queued(&node.shared, other).await;
        assert_eq!(taken.as_deref(), Some(test_queue::ORIGINAL));
        pause.release.send(()).unwrap();
        let (status, reply) = request.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{reply}");
        node.finish().await;
    });
}

// Cancels the force branch does not take (Qwen, another channel's execution named by its thread,
// no channel identity, not forced) answer as with the switch off and close nothing.
#[test]
fn force_non_target_equivalence_pg() {
    let Some((rig, runtime)) = rig_child("force_non_target_equivalence_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let compare = async |node: &Node, channel: u64| {
            node._switches.set_terminate(None);
            let off = node.cancel_on(channel, true).await;
            node._switches.set_terminate(Some(true));
            let on = node.cancel_on(channel, true).await;
            assert_eq!(on, off, "the switch changes nothing for this cancel");
            assert!(on.1.get("termination").is_none() && on.1["context"].get("termination").is_none());
            assert_eq!(node.effects(&rig).await.closes(), 0);
            on
        };

        // The switch off: the Herdr target keeps today's refusal.
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        node._switches.set_terminate(None);
        let (status, reply) = node.cancel().await;
        assert_eq!(status, StatusCode::CONFLICT, "{reply}");
        assert_eq!(reply["error"], "session host is not legacy tmux", "{reply}");
        assert!(reply["context"].get("termination").is_none());
        node._switches.set_terminate(Some(true));
        node.finish().await;

        // A Qwen execution.
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        node.sql("UPDATE sessions SET provider = 'qwen', hosted_execution = jsonb_set(jsonb_set(jsonb_set(hosted_execution, '{owner,provider}', '\"qwen\"'), '{source_ref,provider}', '\"qwen\"'), '{expected,binding_provider}', '\"qwen\"') WHERE session_key = $1 AND $2::TEXT IS NULL", None).await;
        compare(&node, CHANNEL).await;
        node.finish().await;

        // Named by its thread here, owned by another channel.
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        node.sql("UPDATE sessions SET thread_channel_id = $2 WHERE session_key = $1", Some(&OTHER.to_string())).await;
        compare(&node, OTHER).await;
        node.finish().await;

        // No channel on the row: the record's owner no longer matches it.
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        node.sql("UPDATE sessions SET channel_id = NULL WHERE session_key = $1 AND $2::TEXT IS NULL", None).await;
        compare(&node, CHANNEL).await;
        node.finish().await;

        // Not forced: the holder's stop answers, never the terminate.
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let (status, reply) = node.cancel_on(CHANNEL, false).await;
        assert!(reply.get("termination").is_none(), "{status}: {reply}");
        let after = node.effects(&rig).await;
        assert_eq!((after.closes(), after.row.clone()), (0, bound()));
        node.finish().await;
    });
}

// A recovery's retire before the fence, or one with hold cleanup pending, is refused with nothing
// applied; while the run holds the transition through its postlude a recovery cannot take it.
#[test]
fn e07_root_replaced_force_race_pg() {
    let Some((rig, runtime)) = rig_child("e07_root_replaced_force_race_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        // The recovery's retire lands between the run's capture and its fence.
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        test_queue::queue_original(&node.shared, channel()).await;
        let untouched = node.effects(&rig).await;
        let (flag, pool, key) = (ran(), node.pool.clone(), node.key.clone());
        let probe_flag = flag.clone();
        probe_after_capture(Box::new(move || {
            Box::pin(async move {
                sqlx::query("UPDATE sessions SET hosted_execution = jsonb_set(hosted_execution, '{state}', '\"retired\"') WHERE session_key = $1")
                    .bind(&key).execute(&pool).await.unwrap();
                probe_flag.store(true, Ordering::SeqCst);
            })
        }));
        let (status, reply) = node.cancel().await;
        assert!(flag.load(Ordering::SeqCst));
        let refused = result("refused", "row changed under fence");
        assert_eq!(termination(status, &reply), (StatusCode::CONFLICT, refused));
        let after = node.effects(&rig).await;
        assert_eq!(after, Effects { row: retired(), ..untouched });

        // Retired with its hold cleanup still pending: refused, and the hold is not the run's.
        let node = Node::bind_again(node, &rig).await;
        node.sql("UPDATE sessions SET hosted_execution = jsonb_set(hosted_execution, '{state}', '\"retired\"') WHERE session_key = $1 AND $2::TEXT IS NULL", None).await;
        let untouched = node.effects(&rig).await;
        assert!(untouched.held);
        let (status, reply) = node.cancel().await;
        let refused = result("refused", "execution identity differs");
        assert_eq!(termination(status, &reply), (StatusCode::CONFLICT, refused));
        assert_eq!(node.effects(&rig).await, untouched);

        // The run holds the transition through its postlude, so a recovery's lease is refused.
        let node = Node::bind_again(node, &rig).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let (flag, shared) = (ran(), node.shared.clone());
        let probe_flag = flag.clone();
        probe_before_postlude(Box::new(move || {
            Box::pin(async move {
                let lease = super::super::super::session_transition::SessionTransitionLease::try_acquire(
                    &shared,
                    channel(),
                );
                assert!(lease.is_err(), "a recovery finds the transition held");
                probe_flag.store(true, Ordering::SeqCst);
            })
        }));
        let (status, reply) = node.cancel().await;
        assert!(flag.load(Ordering::SeqCst));
        assert_eq!(status, StatusCode::OK, "{reply}");
        node.finish().await;
    });
}

// A record a `!clear` retired by RootReplaced recovery is no forced cancel's success: refused, with
// the dispatch, row and input queued after the clear left as the clear left them.
#[test]
fn e07_root_replaced_retired_is_not_http_success_pg() {
    use super::super::super::commands::control::{
        SoftClearNotifyMode, clear_channel_session_state,
    };
    let path = format!(
        "{}::e07_root_replaced_retired_is_not_http_success_pg",
        module_path!()
    );
    if !herdr::in_child(&path, CHANNEL) {
        return;
    }
    let rig = herdr::P11TerminationRig::boot(CHANNEL);
    let install = rig.thread_installer();
    install();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .on_thread_start(install)
        .build()
        .unwrap();
    runtime.block_on(async {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let (shared, registry) =
            super::super::super::host_teardown_gate::test_support::runtime(&pool).await;
        let provider = ProviderKind::Claude;
        super::super::super::host_defer_gate::tests::map_channel(&shared, channel(), "e07-clear")
            .await;
        let key = super::super::super::adk_session::build_adk_session_key;
        let key = key(&shared, channel(), &provider, None).await.unwrap();
        let switches = rig.bind(&pool, &shared.token_hash, &key, herdr::HostedState::Bound).await;
        let root = crate::config::runtime_root().unwrap().display().to_string();
        sqlx::query("UPDATE sessions SET hosted_execution = jsonb_set(jsonb_set(hosted_execution, '{owner,runtime_root}', to_jsonb($2::text)), '{source_ref,runtime_root}', to_jsonb($2::text)) WHERE session_key = $1")
            .bind(&key).bind(root).execute(&pool).await.unwrap();
        for id in [DISPATCH, SUCCESSOR] {
            sqlx::query("INSERT INTO task_dispatches (id, status) VALUES ($1, 'dispatched')")
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
        }
        rig.restore_shell();
        let http = Arc::new(poise::serenity_prelude::Http::new(""));
        let notify = SoftClearNotifyMode::Suppress;
        let clear = clear_channel_session_state(&http, &shared, &provider, channel(), "/clear", notify);
        clear.await.expect("a clear always commits");
        assert_eq!(herdr::row(&pool, &key).await, retired(), "the recovery retired it");
        assert!(!herdr::held());

        // The row reads as the channel's active dispatched turn again, with input waiting.
        sqlx::query("UPDATE sessions SET status = 'turn_active', active_dispatch_id = $2, last_heartbeat = NOW() WHERE session_key = $1")
            .bind(&key).bind(DISPATCH).execute(&pool).await.unwrap();
        Node::enqueue(&shared, 63, "after the clear").await;
        let node = Node {
            db,
            pool,
            shared,
            registry,
            key,
            _switches: switches,
        };
        let untouched = node.effects(&rig).await;
        let (status, reply) = node.cancel().await;
        let refused = result("refused", "execution identity differs");
        assert_eq!(termination(status, &reply), (StatusCode::CONFLICT, refused));
        let after = node.effects(&rig).await;
        assert_eq!(after, untouched);
        assert_eq!(after.closes(), 0, "the restored shell is never closed");
        assert_eq!(after.session, active(Some(DISPATCH)));
        assert_eq!(after.dispatches, dispatches("dispatched", "dispatched"));
        assert_eq!(after.queued, ["after the clear"]);
        node.finish().await;
    });
}

// The cancel's own read binds the run: when its turn ended and the successor's dispatch took the
// channel right after that read, the run is refused before any close and the successor lives.
#[test]
fn e07_selection_change_refuses_before_close_pg() {
    let Some((rig, runtime)) = rig_child("e07_selection_change_refuses_before_close_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let flag = ran();
        super::super::probe_before_selection(e08::successor_takes_over(&node, flag.clone()));
        let untouched = node.effects(&rig).await;
        let (status, reply) = node.cancel().await;
        assert!(flag.load(Ordering::SeqCst));
        let refused = result("refused", "captured turn runs another dispatch");
        assert_eq!(
            termination(status, &reply),
            (StatusCode::CONFLICT, refused),
            "{reply}"
        );
        let after = node.effects(&rig).await;
        assert_eq!(
            after,
            Effects {
                session: active(Some(SUCCESSOR)),
                ..untouched
            }
        );
        let b = test_turn::active(&node.shared, channel())
            .await
            .expect("B runs");
        assert_eq!(b.turn_nonce(), Some("turn-b"));
        assert!(!b.cancelled.load(Ordering::SeqCst));
        node.finish().await;
    });
}

// Another registry publishing its actor while the purge waits changes nothing: record and depth
// are the exclusion actor's, and the other actor's queue is left as it was.
#[test]
fn e07_purge_records_on_the_exclusion_actor_pg() {
    let Some((rig, runtime)) = rig_child("e07_purge_records_on_the_exclusion_actor_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        use crate::services::turn_orchestrator::{ChannelMailboxRegistry, QueuePersistenceContext};
        let node = Node::bind(&rig, herdr::HostedState::Bound, Some(DISPATCH)).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        test_queue::queue_original(&node.shared, channel()).await;
        let (flag, other) = (ran(), Arc::new(Mutex::new(None)));
        let (probe_flag, probe_other) = (flag.clone(), other.clone());
        crate::services::queue::set_pre_purge_probe_for_test(
            channel(),
            Box::new(move || {
                async move {
                    let registry = ChannelMailboxRegistry::default();
                    let handle = registry.handle(channel());
                    let persistence =
                        QueuePersistenceContext::new(&ProviderKind::Claude, "other-registry", None);
                    handle
                        .replace_queue(
                            vec![intervention(99, "the other actor's input")],
                            persistence,
                        )
                        .await;
                    *probe_other.lock().unwrap() = Some((registry, handle));
                    probe_flag.store(true, Ordering::SeqCst);
                }
                .boxed()
            }),
        );
        let (status, reply) = node.cancel().await;
        assert!(flag.load(Ordering::SeqCst));
        assert_eq!(status, StatusCode::OK, "{reply}");
        let claims = (
            &reply["queue_dead_lettered_message_ids"],
            &reply["queued_remaining"],
        );
        assert_eq!(claims, (&json!([61]), &json!(0)), "{reply}");
        let (_registry, handle) = other.lock().unwrap().take().unwrap();
        let state = handle.snapshot().await;
        let texts: Vec<_> = state
            .intervention_queue
            .iter()
            .map(|i| i.text.as_str())
            .collect();
        assert_eq!(
            texts,
            ["the other actor's input"],
            "the other actor keeps its queue"
        );
        assert_eq!(node.effects(&rig).await.dead_lettered, dead(&[61]));
        node.finish().await;
    });
}

#[path = "http_force_e08_tests.rs"]
mod e08;
