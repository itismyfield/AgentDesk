//! The session force-kill through the production router on the fake Herdr rig and a throwaway PG:
//! its disconnect and dispatch failure or retry happen only once the scoped run retired and settled.

use axum::Json;

use super::super::super::super::session_transition::SessionTransitionLease;
use super::super::super::{SettlementWindowProbe, probe_before_released, probe_before_selection};
use super::*;

const AGENT: &str = "e08-agent";
const RUN: &str = "e08-run";
const SELECTOR: &str = "claude-e08";

/// The rig for test `name` in its own child, on a current-thread runtime so every probe runs.
fn rig_e08(
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

/// Everything a force-kill may change: the session row with its provider session id, the two
/// dispatches, every retry made of them, the auto-queue run, the queue, the hosted row, the
/// input hold and every write Herdr received.
#[derive(Debug, Clone, PartialEq)]
struct Killed {
    session: (String, Option<String>, Option<String>),
    dispatches: Vec<(String, String)>,
    retries: Vec<(String, i64)>,
    run: String,
    queued: Vec<String>,
    row: Option<(herdr::HostedState, String)>,
    held: bool,
    writes: Vec<(String, String)>,
}

impl Killed {
    /// Herdr received closes of the rig's pane only: never a provider input.
    fn closes(&self) -> usize {
        let close =
            |(method, pane): &(String, String)| method.ends_with(".close") && pane == "w1-1";
        assert!(self.writes.iter().all(close), "{:?}", self.writes);
        self.writes.len()
    }

    /// `self` with only Herdr's writes taken from `other`.
    fn but_writes(&self, other: &Self) -> Self {
        Self {
            writes: other.writes.clone(),
            ..self.clone()
        }
    }
}

fn kept(status: &str, dispatch: Option<&str>) -> (String, Option<String>, Option<String>) {
    (
        status.into(),
        dispatch.map(str::to_string),
        Some(SELECTOR.into()),
    )
}

fn path(key: &str) -> String {
    let key = key.replace('/', "%2F").replace(':', "%3A");
    format!("/sessions/{key}/force-kill")
}

impl Node {
    /// The bound execution as the agent's session on its dispatched card, and an active auto-queue
    /// run of the agent whose only entry already failed, so an activation completes the run.
    async fn bind_e08(rig: &herdr::P11TerminationRig, state: herdr::HostedState) -> Self {
        let node = Self::bind(rig, state, Some(DISPATCH)).await;
        let channel = CHANNEL.to_string();
        sqlx::query("INSERT INTO agents (id, name, provider, discord_channel_id) VALUES ($1, 'E08', 'claude', $2)")
            .bind(AGENT).bind(&channel).execute(&node.pool).await.unwrap();
        for statement in [
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id) VALUES ('e08-card', 'e08', 'in_progress', 'e08-agent'), ('e08-aq-card', 'aq', 'in_progress', 'e08-agent')",
            "UPDATE task_dispatches SET kanban_card_id = 'e08-card', to_agent_id = 'e08-agent', dispatch_type = 'implementation', title = id, context = '{}', retry_count = 0",
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) VALUES ('e08-run', 'repo', 'e08-agent', 'active')",
            "INSERT INTO auto_queue_slots (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map) VALUES ('e08-agent', 0, 'e08-run', 0, CAST('{}' AS jsonb))",
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status) VALUES ('e08-entry', 'e08-run', 'e08-aq-card', 'e08-agent', 'failed')",
        ] {
            sqlx::query(statement).execute(&node.pool).await.unwrap();
        }
        node.sql("UPDATE sessions SET agent_id = 'e08-agent', claude_session_id = $2 WHERE session_key = $1", Some(SELECTOR)).await;
        node
    }

    async fn kill(&self, retry: bool) -> (StatusCode, Value) {
        self.post(
            &path(&self.key),
            json!({"retry": retry, "reason": "e08 test"}),
        )
        .await
    }

    /// The force-kill as its own task, so the test can drop or outlive its request.
    fn spawn_kill(&self, retry: bool) -> tokio::task::JoinHandle<(StatusCode, Value)> {
        let (app, uri) = (self.app(), path(&self.key));
        tokio::spawn(async move {
            let bearer = format!("Bearer {AUTH}");
            let headers = [("authorization", bearer.as_str()), JSON];
            let body = json!({"retry": retry, "reason": "e08 test"}).to_string();
            crate::server::routes::home_stop_test_post(&app, &uri, &headers, &body).await
        })
    }

    async fn killed(&self, rig: &herdr::P11TerminationRig) -> Killed {
        let session = sqlx::query_as(
            "SELECT status, active_dispatch_id, claude_session_id FROM sessions WHERE session_key = $1",
        )
        .bind(&self.key);
        let dispatches =
            sqlx::query_as("SELECT id, status FROM task_dispatches WHERE id = ANY($1) ORDER BY id")
                .bind(vec![DISPATCH, SUCCESSOR]);
        let retries = sqlx::query_as(
            "SELECT status, retry_count::BIGINT FROM task_dispatches WHERE id <> ALL($1) ORDER BY created_at",
        )
        .bind(vec![DISPATCH, SUCCESSOR]);
        let run = sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = $1").bind(RUN);
        let queued = match self.shared.mailbox_peek(channel()) {
            Some(_) => test_queue::queued_texts(&self.shared, channel()).await,
            None => Vec::new(),
        };
        Killed {
            session: session.fetch_one(&self.pool).await.unwrap(),
            dispatches: dispatches.fetch_all(&self.pool).await.unwrap(),
            retries: retries.fetch_all(&self.pool).await.unwrap(),
            run: run.fetch_one(&self.pool).await.unwrap(),
            queued,
            row: herdr::row(&self.pool, &self.key).await,
            held: herdr::held(),
            writes: rig.writes(),
        }
    }

    /// The production router as the node `holder` serves it.
    fn app_on(&self, holder: &str) -> axum::Router {
        let mut config = crate::config::Config::default();
        config.server.auth_token = Some(AUTH.into());
        let (pool, registry) = (Some(self.pool.clone()), Some(self.registry.clone()));
        crate::server::routes::home_stop_test_app(config, pool, registry, holder)
    }

    /// The state a direct CLI force-kill on node `holder` runs with.
    fn state(&self, holder: &str) -> crate::app_state::AppState {
        let config = crate::config::Config::default();
        let broadcast_tx = crate::eventbus::new_broadcast();
        crate::app_state::AppState {
            pg_pool: Some(self.pool.clone()),
            engine: crate::engine::PolicyEngine::new(&config).expect("policy engine"),
            config: Arc::new(config),
            batch_buffer: crate::eventbus::spawn_batch_flusher(broadcast_tx.clone()),
            broadcast_tx,
            health_registry: Some(self.registry.clone()),
            cluster_instance_id: Some(holder.into()),
        }
    }

    /// A fresh database and runtime for the next case, the previous one dropped.
    async fn again_e08(previous: Self, rig: &herdr::P11TerminationRig) -> Self {
        previous.finish().await;
        Self::bind_e08(rig, herdr::HostedState::Bound).await
    }
}

/// The run's status, read after waiting long enough for a spawned activation to have run.
async fn run_after_activation(node: &Node) -> String {
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = $1")
        .bind(RUN)
        .fetch_one(&node.pool)
        .await
        .unwrap()
}

// The positive control, retry off then on: on a row the turn-start writer marked active, the
// force-kill retires the execution, then under the exclusion disconnects it (provider session
// kept) and fails its dispatch or makes one retry; the kept queue drains and the agent's
// auto-queue wakes only once the exclusion is gone, and the reply keeps the legacy keys.
#[test]
fn e08_force_kill_retires_then_applies_effects_once_pg() {
    let Some((rig, runtime)) =
        rig_e08("e08_force_kill_retires_then_applies_effects_once_pg", false)
    else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind_e08(&rig, herdr::HostedState::Bound).await;
        node.sql("UPDATE sessions SET status = 'idle', active_dispatch_id = NULL, last_heartbeat = NULL WHERE session_key = $1 AND $2::TEXT IS NULL", None).await;
        let marker = json!({"session_key": node.key, "status": "turn_active", "provider": "claude",
            "channel_id": CHANNEL.to_string(), "dispatch_id": DISPATCH, "agent_id": AGENT,
            "turn_start_nonce": "turn-a", "dispatched_origin": true});
        let (status, reply) = node.post("/dispatched-sessions/webhook", marker).await;
        assert!(status.is_success(), "{status}: {reply}");
        assert_eq!(node.killed(&rig).await.session, kept("turn_active", Some(DISPATCH)));
        node.start(51, "turn-a", Some(DISPATCH)).await;
        test_queue::queue_original(&node.shared, channel()).await;
        let before = cancelled_events();
        let (held_flag, free_flag) = (ran(), ran());
        let (shared, probe_flag) = (node.shared.clone(), held_flag.clone());
        probe_before_postlude(Box::new(move || {
            Box::pin(async move {
                let lease = SessionTransitionLease::try_acquire(&shared, channel());
                assert!(lease.is_err(), "the effects run under the transition");
                probe_flag.store(true, Ordering::SeqCst);
            })
        }));
        let (shared, pool, key, probe_flag) = (node.shared.clone(), node.pool.clone(), node.key.clone(), free_flag.clone());
        probe_before_released(Box::new(move || {
            Box::pin(async move {
                let lease = SessionTransitionLease::try_acquire(&shared, channel());
                assert!(lease.is_ok(), "the drain and the wake run once the transition is free");
                let status: String = sqlx::query_scalar("SELECT status FROM sessions WHERE session_key = $1")
                    .bind(&key).fetch_one(&pool).await.unwrap();
                assert_eq!(status, "disconnected", "the exclusion's effects came first");
                probe_flag.store(true, Ordering::SeqCst);
            })
        }));

        let (status, reply) = node.kill(false).await;
        assert!(held_flag.load(Ordering::SeqCst) && free_flag.load(Ordering::SeqCst));
        assert_eq!(status, StatusCode::OK, "{reply}");
        assert_eq!(
            reply,
            json!({"ok": true, "tmux_killed": false, "inflight_cleared": true,
                "lifecycle_path": "herdr-terminate", "queued_remaining": 1, "queue_preserved": true,
                "dispatch_failed": DISPATCH, "retry_dispatch_id": null, "retry_limit": 5,
                "retry_skipped_reason": null, "queue_activation_requested": true,
                "termination": result("retired", "retired")})
        );
        let after = node.killed(&rig).await;
        assert_eq!(after.closes(), 1);
        assert_eq!(after.session, kept("disconnected", None));
        assert_eq!(after.dispatches, dispatches("failed", "dispatched"));
        assert!(after.retries.is_empty());
        assert_eq!(after.queued, [test_queue::ORIGINAL], "the queue is kept");
        assert_eq!((after.row, after.held), (retired(), false));
        assert_eq!(cancelled_events(), before + 1, "one cancel observation");
        assert_eq!(run_after_activation(&node).await, "completed", "the agent's auto-queue woke");

        // With retry on, one retry dispatch is made through the origin's single writer, no wake.
        let node = Node::again_e08(node, &rig).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let (status, reply) = node.kill(true).await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        assert!(reply["retry_dispatch_id"].is_string(), "{reply}");
        assert_eq!(reply["queue_activation_requested"], false);
        let after = node.killed(&rig).await;
        assert_eq!((after.closes(), after.session.clone()), (1, kept("disconnected", None)));
        assert_eq!(after.dispatches, dispatches("failed", "dispatched"));
        assert_eq!(after.retries, [("pending".to_string(), 1)]);
        assert_eq!(run_after_activation(&node).await, "active", "a retry wakes nothing");
        node.finish().await;
    });
}

// The real handler held between its close and the fresh end check, with retry off and on: the
// session, both dispatches, every retry, the run and the queue are as they were.
#[test]
fn e08_retry_waits_for_terminated_handler_pg() {
    let Some((rig, runtime)) = rig_e08("e08_retry_waits_for_terminated_handler_pg", false) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let mut node = Node::bind_e08(&rig, herdr::HostedState::Bound).await;
        for retry in [false, true] {
            node.start(51, "turn-a", Some(DISPATCH)).await;
            test_queue::queue_original(&node.shared, channel()).await;
            let untouched = node.killed(&rig).await;
            let token = test_turn::active(&node.shared, channel()).await.unwrap();
            let mut pause = rig.pause_end_check();
            let request = node.spawn_kill(retry);
            tokio::time::timeout(Duration::from_secs(10), &mut pause.arrived)
                .await
                .expect("the run reached its end check after the close")
                .unwrap();
            let held = node.killed(&rig).await;
            // The rig's pane stays ended after the first run, so only that one sends a close.
            let sent = usize::from(!retry);
            assert_eq!(
                held.closes(),
                untouched.closes() + sent,
                "the close was sent once"
            );
            assert_eq!(held.but_writes(&untouched), untouched, "retry={retry}");
            let current = test_turn::active(&node.shared, channel()).await;
            assert!(
                current.is_some_and(|t| Arc::ptr_eq(&t, &token)),
                "the turn is still its own"
            );
            tokio::time::sleep(Duration::from_millis(2_500)).await;
            assert_eq!(
                node.killed(&rig).await.run,
                "active",
                "nothing woke the auto-queue"
            );
            pause.release.send(()).unwrap();
            let (status, reply) = request.await.unwrap();
            assert_eq!(status, StatusCode::OK, "{reply}");
            let after = node.killed(&rig).await;
            assert_eq!(after.session, kept("disconnected", None));
            assert_eq!(after.retries.len(), usize::from(retry));
            node = Node::again_e08(node, &rig).await;
        }
        node.finish().await;
    });
}

// Every refusal or failure leaves session, dispatches, retries, run and queue as they were, a 500
// past the close; only a retired, settled run reaches the effects.
#[test]
fn e08_force_kill_failure_matrix_pg() {
    let Some((rig, runtime)) = rig_e08("e08_force_kill_failure_matrix_pg", false) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let refused_case = async |node: &Node, expected: (StatusCode, Value), closes: usize| {
            let untouched = node.killed(&rig).await;
            let (status, reply) = node.kill(true).await;
            assert_eq!((status, reply["termination"].clone()), expected, "{reply}");
            let after = node.killed(&rig).await;
            assert_eq!(after.closes(), untouched.closes() + closes);
            assert_eq!(after.but_writes(&untouched), untouched);
        };
        let conflict = |name: &str, detail: &str| (StatusCode::CONFLICT, result(name, detail));
        let failed = |detail: &str| (StatusCode::INTERNAL_SERVER_ERROR, result("indeterminate", detail));

        // A stored Retired record is refused: no close and no replay of any effect.
        let node = Node::bind_e08(&rig, herdr::HostedState::Retired).await;
        test_queue::queue_original(&node.shared, channel()).await;
        refused_case(&node, conflict("refused", "execution identity differs"), 0).await;

        // A transition held by someone else past the wait is a refusal.
        let node = Node::again_e08(node, &rig).await;
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

        // A successor execution taking the row before the retire leaves it the successor's.
        let closed = node.killed(&rig).await.closes();
        let node = Node::again_e08(node, &rig).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let (flag, pool, key) = (ran(), node.pool.clone(), node.key.clone());
        let probe_flag = flag.clone();
        probe_after_close(Box::new(move || {
            Box::pin(async move {
                sqlx::query("UPDATE sessions SET hosted_execution = jsonb_set(jsonb_set(hosted_execution, '{execution_nonce}', '\"fedcba9876543210fedcba9876543210\"'), '{source_ref,execution_nonce}', '\"fedcba9876543210fedcba9876543210\"') WHERE session_key = $1")
                    .bind(&key).execute(&pool).await.unwrap();
                probe_flag.store(true, Ordering::SeqCst);
            })
        }));
        let (status, reply) = node.kill(true).await;
        assert!(flag.load(Ordering::SeqCst));
        assert_eq!((status, reply["termination"].clone()), failed("retire CAS stale"));
        let after = node.killed(&rig).await;
        assert_eq!((after.closes(), after.session.clone()), (closed, kept("turn_active", Some(DISPATCH))));
        assert_eq!((after.dispatches, after.retries), (dispatches("dispatched", "dispatched"), vec![]));

        // The retire landed but the turn's settlement failed: past the retire, still no effect.
        let node = Node::again_e08(node, &rig).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        test_turn::arm_settlement_panic();
        let (status, reply) = node.kill(true).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{reply}");
        let detail = reply["termination"]["detail"].as_str().unwrap();
        assert!(detail.starts_with("retired, hold released; "), "{detail}");
        let after = node.killed(&rig).await;
        assert_eq!((after.row.clone(), after.session.clone()), (retired(), kept("turn_active", Some(DISPATCH))));
        assert_eq!((after.dispatches, after.retries), (dispatches("dispatched", "dispatched"), vec![]));

        // The switch turned off between the pick and the adapter: Disabled, nothing run or wired back.
        let node = Node::again_e08(node, &rig).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let flag = ran();
        let probe_flag = flag.clone();
        probe_before_selection(Box::new(move || {
            Box::pin(async move {
                let mut config = (*crate::config_live_reload::current().unwrap()).clone();
                config.runtime.herdr_terminate_enabled = None;
                crate::config_live_reload::install(config);
                probe_flag.store(true, Ordering::SeqCst);
            })
        }));
        refused_case(&node, conflict("disabled", "terminate disabled"), 0).await;
        assert!(flag.load(Ordering::SeqCst));
        node._switches.set_terminate(Some(true));

        // A successor execution took the row before the effects: its row is never disconnected
        // and no dispatch outcome follows.
        let node = Node::again_e08(node, &rig).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let (flag, pool, key) = (ran(), node.pool.clone(), node.key.clone());
        let probe_flag = flag.clone();
        probe_before_postlude(Box::new(move || {
            Box::pin(async move {
                sqlx::query("UPDATE sessions SET hosted_execution = jsonb_set(jsonb_set(jsonb_set(hosted_execution, '{state}', '\"bound\"'), '{execution_nonce}', '\"fedcba9876543210fedcba9876543210\"'), '{source_ref,execution_nonce}', '\"fedcba9876543210fedcba9876543210\"') WHERE session_key = $1")
                    .bind(&key).execute(&pool).await.unwrap();
                probe_flag.store(true, Ordering::SeqCst);
            })
        }));
        let (status, reply) = node.kill(true).await;
        assert!(flag.load(Ordering::SeqCst));
        assert_eq!((status, reply["termination"].clone()), (StatusCode::INTERNAL_SERVER_ERROR, result("retired", "retired")));
        assert!(reply["error"].as_str().unwrap().contains("session row moved on"), "{reply}");
        let after = node.killed(&rig).await;
        assert_eq!(after.session, kept("turn_active", Some(DISPATCH)), "never disconnected");
        assert_eq!((after.dispatches, after.retries), (dispatches("dispatched", "dispatched"), vec![]));
        assert_eq!(run_after_activation(&node).await, "active", "nothing woke");

        // The effects meet a dispatch another writer already finished: the terminal status stays.
        let node = Node::again_e08(node, &rig).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        sqlx::query("UPDATE task_dispatches SET status = 'completed' WHERE id = $1").bind(DISPATCH).execute(&node.pool).await.unwrap();
        let (status, reply) = node.kill(true).await;
        assert_eq!(status, StatusCode::OK, "{reply}");
        assert_eq!(reply["retry_dispatch_id"], Value::Null);
        let after = node.killed(&rig).await;
        assert_eq!(after.dispatches, dispatches("completed", "dispatched"), "never rewritten to failed");
        assert!(after.retries.is_empty());
        node.finish().await;
    });
}

// A provider that outlives the close is a refusal that applies nothing and closes nothing more.
#[test]
fn e08_force_kill_failure_matrix_provider_alive_pg() {
    let Some((rig, runtime)) = rig_e08("e08_force_kill_failure_matrix_provider_alive_pg", true)
    else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind_e08(&rig, herdr::HostedState::Bound).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let untouched = node.killed(&rig).await;
        let (status, reply) = node.kill(true).await;
        let survived = result("provider_survived_close", "the provider outlived the close");
        assert_eq!(
            (status, reply["termination"].clone()),
            (StatusCode::CONFLICT, survived)
        );
        let after = node.killed(&rig).await;
        assert_eq!(after.closes(), 1);
        assert_eq!(after.but_writes(&untouched), untouched);
        node.finish().await;
    });
}

// A dropped request's run still retires and applies its effects, one retry. A request repeated
// after the retire replays nothing, and two at once retire once with one retry.
#[test]
fn e08_force_kill_duplicate_abort_restart_pg() {
    let Some((rig, runtime)) = rig_e08("e08_force_kill_duplicate_abort_restart_pg", false) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind_e08(&rig, herdr::HostedState::Bound).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let mut hold = rig.hold_next_write();
        let request = node.spawn_kill(true);
        tokio::time::timeout(Duration::from_secs(10), &mut hold.reached)
            .await
            .expect("the close reached Herdr")
            .unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        hold.release.send(()).unwrap();
        wait_for("the dropped run's effects", async || {
            node.killed(&rig).await.retries.len() == 1
        })
        .await;
        let after = node.killed(&rig).await;
        assert_eq!((after.session.clone(), after.row.clone()), (kept("disconnected", None), retired()));
        assert_eq!(after.dispatches, dispatches("failed", "dispatched"));

        // The row reads active again on the successor: a repeat neither closes nor retries.
        node.sql("UPDATE sessions SET status = 'turn_active', active_dispatch_id = $2 WHERE session_key = $1", Some(SUCCESSOR)).await;
        let before = node.killed(&rig).await;
        let (status, reply) = node.kill(true).await;
        let refused = result("refused", "execution identity differs");
        assert_eq!((status, reply["termination"].clone()), (StatusCode::CONFLICT, refused));
        assert_eq!(node.killed(&rig).await, before);
        drop(hold);

        // Two requests at once on a live row whose pane already ended: one retire, one retry.
        let node = Node::again_e08(node, &rig).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let (first, second) = tokio::join!(node.kill(true), node.kill(true));
        let mut statuses = [first.0, second.0];
        statuses.sort();
        assert_eq!(statuses, [StatusCode::OK, StatusCode::CONFLICT], "{first:?} {second:?}");
        let after = node.killed(&rig).await;
        assert_eq!(after.retries, [("pending".to_string(), 1)], "one retry");
        assert_eq!(after.dispatches, dispatches("failed", "dispatched"));
        node.finish().await;
    });
}

// Force-kills the Herdr arm does not take answer as with the switch off and close nothing: the
// switch off itself (today's refusal, byte for byte), Qwen, another channel's thread, a record
// its row does not own, an unreadable record, and the direct CLI with the switch on.
#[test]
fn e08_force_kill_non_target_equivalence_pg() {
    let Some((rig, runtime)) = rig_e08("e08_force_kill_non_target_equivalence_pg", false) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let compare = async |node: &Node| {
            node._switches.set_terminate(None);
            let off = node.kill(true).await;
            node._switches.set_terminate(Some(true));
            let on = node.kill(true).await;
            assert_eq!(on, off, "the switch changes nothing for this force-kill");
            assert!(on.1.get("termination").is_none(), "{on:?}");
            assert_eq!(node.killed(&rig).await.closes(), 0);
            on
        };

        // The switch off: the Herdr target keeps today's refusal, the same bytes.
        let node = Node::bind_e08(&rig, herdr::HostedState::Bound).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let untouched = node.killed(&rig).await;
        node._switches.set_terminate(None);
        let (status, reply) = node.kill(true).await;
        assert_eq!(status, StatusCode::CONFLICT);
        let main = json!({"error": "session host is not legacy tmux",
            "unsupported": "session_host_not_tmux", "session_key": node.key});
        assert_eq!(reply.to_string(), main.to_string());
        assert_eq!(node.killed(&rig).await, untouched);
        node._switches.set_terminate(Some(true));

        // A Qwen execution.
        let node = Node::again_e08(node, &rig).await;
        node.sql("UPDATE sessions SET provider = 'qwen', hosted_execution = jsonb_set(jsonb_set(jsonb_set(hosted_execution, '{owner,provider}', '\"qwen\"'), '{source_ref,provider}', '\"qwen\"'), '{expected,binding_provider}', '\"qwen\"') WHERE session_key = $1 AND $2::TEXT IS NULL", None).await;
        compare(&node).await;

        // The session's thread is another channel than the execution's.
        let node = Node::again_e08(node, &rig).await;
        node.sql("UPDATE sessions SET thread_channel_id = $2 WHERE session_key = $1", Some("677210900000001")).await;
        compare(&node).await;

        // The record's owner is not the row's identity.
        let node = Node::again_e08(node, &rig).await;
        node.sql("UPDATE sessions SET discord_token_hash = $2 WHERE session_key = $1", Some("another-token-hash")).await;
        compare(&node).await;

        // An unreadable record.
        let node = Node::again_e08(node, &rig).await;
        node.sql("UPDATE sessions SET hosted_execution = '{\"state\": \"bound\"}' WHERE session_key = $1 AND $2::TEXT IS NULL", None).await;
        compare(&node).await;

        // The direct CLI keeps the legacy path with the switch on: today's refusal, no close.
        let node = Node::again_e08(node, &rig).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let untouched = node.killed(&rig).await;
        let state = node.state(herdr::NODE);
        let direct = crate::server::routes::dispatched_sessions::force_kill_session_impl_with_reason;
        let (status, Json(reply)) = direct(&state, &node.key, true, "e08 cli").await;
        assert_eq!((status, reply.to_string()), (StatusCode::CONFLICT, json!({"error": "session host is not legacy tmux",
            "unsupported": "session_host_not_tmux", "session_key": node.key}).to_string()));
        assert_eq!(node.killed(&rig).await, untouched);
        node.finish().await;
    });
}

// A force-kill reaching a node that does not own the session is never terminated there: the
// owner is resolved first and nothing of the origin moves; forwarded to the owner, it retires.
#[test]
fn e08_force_kill_non_owner_forwards_pg() {
    let Some((rig, runtime)) = rig_e08("e08_force_kill_non_owner_forwards_pg", false) else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind_e08(&rig, herdr::HostedState::Bound).await;
        node.sql(
            "UPDATE sessions SET instance_id = $2 WHERE session_key = $1",
            Some(herdr::NODE),
        )
        .await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let untouched = node.killed(&rig).await;
        let bearer = format!("Bearer {AUTH}");
        let body = json!({"retry": true, "reason": "e08 test"}).to_string();
        let origin = node.app_on("origin-node");
        let headers = [("authorization", bearer.as_str()), JSON];
        let (status, reply) =
            crate::server::routes::home_stop_test_post(&origin, &path(&node.key), &headers, &body)
                .await;
        assert!(!status.is_success(), "{status}: {reply}");
        assert!(reply.get("termination").is_none(), "{reply}");
        assert_eq!(
            node.killed(&rig).await,
            untouched,
            "nothing moved at the origin"
        );

        // The owner receiving the forwarded request takes the Herdr arm.
        let forwarded = [
            ("authorization", bearer.as_str()),
            JSON,
            ("x-agentdesk-forwarded-by", "origin-node"),
            ("x-agentdesk-session-owner", herdr::NODE),
        ];
        let owner = node.app_on(herdr::NODE);
        let (status, reply) =
            crate::server::routes::home_stop_test_post(&owner, &path(&node.key), &forwarded, &body)
                .await;
        assert_eq!(
            (status, reply["termination"].clone()),
            (StatusCode::OK, result("retired", "retired")),
            "{reply}"
        );
        let after = node.killed(&rig).await;
        assert_eq!((after.closes(), after.retries.len()), (1, 1));
        node.finish().await;
    });
}

// The home command a force-kill was admitted under stays open while its detached run lives,
// past a dropped request, and closes once the run ends.
#[test]
fn e08_force_kill_home_permit_survives_http_abort_pg() {
    let Some((rig, runtime)) = rig_e08("e08_force_kill_home_permit_survives_http_abort_pg", false)
    else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        use crate::db::o_channel_homes::HomeState;
        use crate::services::cluster::channel_home;
        let home = channel_home::register_for_test(CHANNEL, Some(HomeState::Worker));
        let node = Node::bind_e08(&rig, herdr::HostedState::Bound).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let mut hold = rig.hold_next_write();
        let request = node.spawn_kill(false);
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
        assert_eq!(node.killed(&rig).await.session, kept("disconnected", None));
        channel_home::unregister(home.channel_id());
        drop(hold);
        node.finish().await;
    });
}

// The force-kill's own read binds the run: when its turn ended and the successor's dispatch took
// the channel right after that read, the run is refused before any close and the successor lives.
#[test]
fn e08_selection_change_refuses_before_close_pg() {
    let Some((rig, runtime)) = rig_e08("e08_selection_change_refuses_before_close_pg", false)
    else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind_e08(&rig, herdr::HostedState::Bound).await;
        node.start(51, "turn-a", Some(DISPATCH)).await;
        let flag = ran();
        let probe = successor_takes_over(&node, flag.clone());
        probe_before_selection(probe);
        let untouched = node.killed(&rig).await;
        let (status, reply) = node.kill(true).await;
        assert!(flag.load(Ordering::SeqCst));
        let refused = result("refused", "captured turn runs another dispatch");
        assert_eq!(
            (status, reply["termination"].clone()),
            (StatusCode::CONFLICT, refused),
            "{reply}"
        );
        let after = node.killed(&rig).await;
        assert_eq!(
            after,
            Killed {
                session: kept("turn_active", Some(SUCCESSOR)),
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

/// A probe in which turn A ends naturally and turn B starts for the successor dispatch, which
/// the row then names as its active one.
pub(super) fn successor_takes_over(node: &Node, flag: Arc<AtomicBool>) -> SettlementWindowProbe {
    let (shared, pool, key) = (node.shared.clone(), node.pool.clone(), node.key.clone());
    Box::new(move || {
        Box::pin(async move {
            let a = test_turn::active(&shared, channel()).await.unwrap();
            test_turn::finish_naturally(&shared, channel(), 51, "turn-a", &a).await;
            test_turn::start(&shared, channel(), 61, "turn-b").await;
            let mut row = inflight::load_inflight_state(&ProviderKind::Claude, CHANNEL).unwrap();
            row.dispatch_id = Some(SUCCESSOR.into());
            inflight::save_inflight_state(&row).unwrap();
            sqlx::query("UPDATE sessions SET active_dispatch_id = $2 WHERE session_key = $1")
                .bind(&key)
                .bind(SUCCESSOR)
                .execute(&pool)
                .await
                .unwrap();
            flag.store(true, Ordering::SeqCst);
        })
    })
}
