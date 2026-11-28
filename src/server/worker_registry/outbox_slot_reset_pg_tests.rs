use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::{Value, json};
use sqlx::PgPool;

use super::{SupervisedWorkerRegistry, WORKER_SPECS, WorkerHandle};
use crate::config::session_hosts::{ForcedSessionHosts, force_for_test};
use crate::config::{Config, TestEnvVarGuard, TestRuntimeRootGuard};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::engine::PolicyEngine;
use crate::services::auto_queue::runtime::slot_reset_host_pg_tests::host::{
    exec, slot, slot_map, thread_row, thread_rows,
};
use crate::services::cluster::node_registry::ClusterRuntime;
use crate::services::discord::SharedData;
use crate::services::discord::health::slot_row_host_refusal;
use crate::services::discord::host_teardown_gate::test_support::Stored;
use crate::services::platform::tmux::SessionPresence;
use crate::services::session_host::host_decision::{
    force_presence_for_test, forced_probes, host_decision_for_rows,
};
use crate::services::session_host::surface_policy::{
    ForcedRelation, Relation, force_relation_for_test,
};

const UNKNOWN_PROVIDER: &str = "unrecognized-provider";
const OUTBOX_PARENT_BASE: u64 = 1_480_000_000_008_000;
const MANUAL_PARENT_BASE: u64 = 1_480_000_000_009_000;
const CASES: [(&str, Stored, &str); 3] = [
    ("legacy", Stored::Legacy, "claude"),
    ("hosted", Stored::Hosted, "claude"),
    ("unknown", Stored::Legacy, UNKNOWN_PROVIDER),
];
static ROW_GUARD_CALLS: Mutex<Vec<u64>> = Mutex::new(Vec::new());

fn observe_row_guard<'a>(
    pool: &'a PgPool,
    provider: Option<&'a str>,
    thread: Option<&'a str>,
    key: &'a str,
    name: &'a str,
    caller: &'a str,
) -> futures::future::BoxFuture<'a, Option<String>> {
    assert_eq!(caller, "auto_queue_slot_reset");
    ROW_GUARD_CALLS
        .lock()
        .unwrap()
        .push(thread.unwrap().parse().unwrap());
    slot_row_host_refusal(pool, provider, thread, key, name, caller)
}

#[derive(Default)]
struct RestThread {
    messages: u64,
    age_minutes: i64,
    reads: usize,
    archived: bool,
}

type RestThreads = Arc<Mutex<BTreeMap<u64, RestThread>>>;

async fn read_thread(
    State(threads): State<RestThreads>,
    Path(id): Path<u64>,
) -> Result<Json<Value>, StatusCode> {
    let mut threads = threads.lock().unwrap();
    let thread = threads.get_mut(&id).ok_or(StatusCode::NOT_FOUND)?;
    thread.reads += 1;
    let created = chrono::Utc::now() - chrono::Duration::minutes(thread.age_minutes);
    Ok(Json(json!({
        "total_message_sent": thread.messages,
        "thread_metadata": {"archived": thread.archived, "create_timestamp": created.to_rfc3339()},
    })))
}

async fn archive_thread(
    State(threads): State<RestThreads>,
    Path(id): Path<u64>,
    Json(body): Json<Value>,
) -> Result<StatusCode, StatusCode> {
    if body != json!({"archived": true}) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let mut threads = threads.lock().unwrap();
    let thread = threads.get_mut(&id).ok_or(StatusCode::NOT_FOUND)?;
    thread.archived = true;
    Ok(StatusCode::NO_CONTENT)
}

async fn spawn_rest(threads: RestThreads) -> (TestEnvVarGuard, tokio::task::JoinHandle<()>) {
    let app =
        Router::new()
            .route("/channels/{thread}", get(read_thread).patch(archive_thread))
            .route(
                "/channels/{parent}/threads",
                post(|Path(parent): Path<u64>| async move {
                    Json(json!({"id": (parent + 2).to_string()}))
                }),
            )
            .route(
                "/channels/{thread}/messages",
                post(|Path(thread): Path<u64>| async move {
                    Json(json!({"id": (thread + 1).to_string()}))
                }),
            )
            .route(
                "/channels/{thread}/messages/{id}/reactions/{emoji}/@me",
                put(|| async { StatusCode::NO_CONTENT }),
            )
            .route(
                "/channels/{thread}/thread-members/{user}",
                put(|| async { StatusCode::NO_CONTENT }),
            )
            .with_state(threads);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let env = TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_DISCORD_API_BASE_URL",
        OsStr::new(&base),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (env, server)
}

struct Fixture {
    pool: PgPool,
    config: Config,
    shared: Arc<SharedData>,
    threads: RestThreads,
    server: tokio::task::JoinHandle<()>,
    _rest_env: TestEnvVarGuard,
    _hosts: ForcedSessionHosts,
    _unknown_relation: ForcedRelation,
}

impl Fixture {
    async fn new(root: &TestRuntimeRootGuard, db: &TestPostgresDb, parent_base: u64) -> Self {
        crate::services::discord::install_engine_probes();
        crate::services::discord::outbound::install_engine_ports();
        ROW_GUARD_CALLS.lock().unwrap().clear();
        let token = crate::runtime_layout::credential_token_path(
            root.path(),
            crate::domain::bot_role::UtilityBotRole::Announce.alias(),
        );
        std::fs::create_dir_all(token.parent().unwrap()).unwrap();
        std::fs::write(token, "fixture-announce-token").unwrap();
        let pool = db.connect_and_migrate_with_max_connections(8).await;
        let mut config = Config::default();
        config.policies.dir = root.path().join("policies");
        config.policies.hot_reload = false;
        config.data.dir = root.path().join("data");
        std::fs::create_dir_all(&config.policies.dir).unwrap();
        let threads = RestThreads::default();
        let (rest_env, server) = spawn_rest(threads.clone()).await;
        let (hosted_thread, unknown_parent) = (parent_base + 11, parent_base + 20);
        Self {
            pool,
            config,
            shared: crate::services::discord::make_shared_data_for_tests(),
            threads,
            server,
            _rest_env: rest_env,
            _hosts: force_for_test(
                Some("mac-mini"),
                &[(hosted_thread, "mac-mini"), (unknown_parent, "mac-mini")],
            ),
            _unknown_relation: force_relation_for_test(
                unknown_parent + 1,
                Relation::Child {
                    parent: unknown_parent,
                },
            ),
        }
    }

    fn engine(&self) -> PolicyEngine {
        PolicyEngine::new_with_pg(&self.config, Some(self.pool.clone()), observe_row_guard).unwrap()
    }

    async fn seed_slot(
        &self,
        case: (&str, Stored, &str),
        parent: u64,
        messages: u64,
        age_minutes: i64,
    ) {
        let (agent, stored, provider) = case;
        let thread = parent + 1;
        let name = format!("AgentDesk-claude-{agent}-{thread}");
        slot(&self.pool, agent, &[]).await;
        let key = thread_row(
            &self.pool,
            &self.shared,
            (thread, &name, stored),
            "idle",
            Some(provider),
        )
        .await;
        exec(
            &self.pool,
            "UPDATE sessions SET claude_session_id = 'prior-session', tokens = 19 WHERE session_key = $1",
            &[Some(&key)],
        )
        .await;
        exec(
            &self.pool,
            "UPDATE agents SET discord_channel_id = $2 WHERE id = $1",
            &[Some(agent), Some(&parent.to_string())],
        )
        .await;
        // A second channel binding records the reset after delivery rebinds the selected channel.
        for channel in [parent, parent + 4] {
            crate::db::dispatches::upsert_slot_thread_id_pg(
                &self.pool,
                agent,
                0,
                channel,
                &thread.to_string(),
            )
            .await
            .unwrap();
        }
        self.threads.lock().unwrap().insert(
            thread,
            RestThread {
                messages,
                age_minutes,
                ..RestThread::default()
            },
        );
        if provider == UNKNOWN_PROVIDER {
            force_presence_for_test(&name, SessionPresence::Missing);
            assert!(
                host_decision_for_rows(&self.pool, thread)
                    .await
                    .tmux_refusal()
                    .is_none()
            );
            assert_eq!(forced_probes(&name), 1, "the key identified the provider");
            let refused = slot_row_host_refusal(
                &self.pool,
                Some(provider),
                Some(&thread.to_string()),
                &key,
                &name,
                "auto_queue_slot_reset",
            )
            .await;
            assert!(refused.is_some_and(|reason| reason.contains("provider unknown")));
        } else if stored == Stored::Hosted {
            assert!(host_decision_for_rows(&self.pool, thread).await.is_herdr());
            assert!(
                slot_row_host_refusal(
                    &self.pool,
                    Some(provider),
                    Some(&thread.to_string()),
                    &key,
                    &name,
                    "auto_queue_slot_reset"
                )
                .await
                .is_some()
            );
        }
    }

    async fn seed_outbox(&self, agent: &str, explicit: bool) -> String {
        let pool = &self.pool;
        let branch = if explicit { "explicit" } else { "stale" };
        let dispatch_id = format!("slot-reset-{branch}-{agent}");
        exec(
            pool,
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id)
             VALUES ($1, $1, 'in_progress', $1)",
            &[Some(agent)],
        )
        .await;
        let context = json!({
            "slot_index": 0, "entry_id": agent, "reset_slot_thread_before_reuse": explicit,
        })
        .to_string();
        exec(
            pool,
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, context)
             VALUES ($1, $2, $2, 'implementation', 'pending', $3)",
            &[Some(&dispatch_id), Some(agent), Some(&context)],
        )
        .await;
        exec(
            pool,
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, slot_index)
             VALUES ($1, 'guard-run', $1, $1, 'dispatched', 0)",
            &[Some(agent)],
        )
        .await;
        exec(
            pool,
            "INSERT INTO dispatch_outbox (dispatch_id, action, agent_id, card_id, title)
             VALUES ($1, 'notify', $2, $2, $2)",
            &[Some(&dispatch_id), Some(agent)],
        )
        .await;
        dispatch_id
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn assert_session_cleared(pool: &PgPool, thread: u64) {
    let rows = thread_rows(pool, thread).await;
    assert_eq!(rows.len(), 1);
    let row: Value = serde_json::from_str(&rows[0]).unwrap();
    assert_eq!(row["status"], "idle");
    assert_eq!(row["claude_session_id"], Value::Null);
    assert_eq!(row["active_dispatch_id"], Value::Null);
    assert_eq!(row["tokens"], 0);
    assert_eq!(row["session_info"], "Slot thread reset");
}

async fn check_outbox_reset(explicit: bool) {
    let root = TestRuntimeRootGuard::new();
    let _config = TestEnvVarGuard::remove_after_shared_test_env_lock("AGENTDESK_CONFIG");
    let db = TestPostgresDb::create().await;
    let fixture = Fixture::new(&root, &db, OUTBOX_PARENT_BASE).await;
    let pool = &fixture.pool;
    exec(pool, "INSERT INTO auto_queue_runs (id, repo, status) VALUES ('guard-run', 'guard-repo', 'active')", &[]).await;
    let mut before = Vec::new();
    let mut expected_actions = Vec::new();
    let mut cases = CASES.to_vec();
    if !explicit {
        cases.extend([
            ("aged", Stored::Legacy, "claude"),
            ("fresh", Stored::Legacy, "claude"),
        ]);
    }
    for (offset, case) in cases.into_iter().enumerate() {
        let agent = case.0;
        let parent = OUTBOX_PARENT_BASE + (offset as u64) * 10;
        let messages = if explicit || matches!(agent, "aged" | "fresh") {
            500
        } else {
            501
        };
        let age = match agent {
            "aged" => 7 * 24 * 60 + 1,
            "fresh" => 7 * 24 * 60 - 1,
            _ => 1,
        };
        fixture.seed_slot(case, parent, messages, age).await;
        let dispatch_id = fixture.seed_outbox(agent, explicit).await;
        expected_actions.push((dispatch_id.clone(), "notify".to_owned()));
        expected_actions.push((dispatch_id, "status_reaction".to_owned()));
        before.push((agent, parent, thread_rows(pool, parent + 1).await));
    }

    let mut registry = SupervisedWorkerRegistry::new(
        fixture.config.clone(),
        fixture.engine(),
        None,
        Some(Arc::new(pool.clone())),
        ClusterRuntime::for_test_with_leader(Arc::default()),
    );
    let spec = WORKER_SPECS
        .into_iter()
        .find(|spec| spec.name == "dispatch_outbox_loop")
        .unwrap();
    registry.start_worker(spec, None).unwrap();
    let outcomes = crate::test_support::wait::poll_within(
        async || {
            let rows =
                sqlx::query_as::<_, (String, String, String, Option<String>, Option<String>)>(
                    "SELECT dispatch_id, action, status, delivery_status, error
                     FROM dispatch_outbox ORDER BY id",
                )
                .fetch_all(pool)
                .await
                .map_err(|error| error.to_string())?;
            let has_error = rows.iter().any(|(.., error)| error.is_some());
            let all_done = rows.len() >= expected_actions.len()
                && rows.iter().all(|(_, _, status, ..)| status == "done");
            if all_done || has_error {
                Ok(rows)
            } else {
                Err(format!("{rows:?}"))
            }
        },
        Duration::from_secs(30),
        "outbox reset deliveries",
    )
    .await
    .unwrap();
    let WorkerHandle::Tokio { _handle: worker } = registry.running.pop().unwrap()._handle else {
        panic!("dispatch outbox must be a Tokio worker");
    };
    tokio::time::pause();
    drop(registry);
    // Drive the supervisor's shutdown poll and grace period on the test clock.
    tokio::time::sleep(
        Duration::from_secs(1) + crate::server::worker_recovery::WORKER_LOCAL_SHUTDOWN_GRACE,
    )
    .await;
    tokio::time::resume();
    crate::test_support::wait::await_within(worker, Duration::from_secs(5))
        .await
        .unwrap();
    let mut actual_actions = Vec::new();
    for (dispatch_id, action, status, delivery, error) in outcomes {
        assert_eq!(
            (status.as_str(), delivery.as_deref(), error.as_deref()),
            ("done", Some("success"), None),
            "{dispatch_id} {action}",
        );
        actual_actions.push((dispatch_id, action));
    }
    actual_actions.sort_unstable();
    expected_actions.sort_unstable();
    assert_eq!(actual_actions, expected_actions);

    let mut expected_guard_threads = Vec::new();
    for (agent, parent, original) in before {
        let thread = parent + 1;
        let should_clear = matches!(agent, "legacy" | "aged");
        let (archived, reads) = {
            let threads = fixture.threads.lock().unwrap();
            (threads[&thread].archived, threads[&thread].reads)
        };
        assert_eq!(archived, should_clear, "REST archive state for {agent}");
        if should_clear {
            assert_eq!(reads, usize::from(!explicit), "reset branch for {agent}");
            assert_session_cleared(pool, thread).await;
        } else {
            assert_eq!(thread_rows(pool, thread).await, original, "{agent} session");
        }
        if matches!(agent, "legacy" | "unknown" | "aged") {
            expected_guard_threads.push(thread);
        }
        let maps = slot_map(pool, agent).await;
        assert_eq!(maps.len(), 1);
        let map: Value = serde_json::from_str(&maps[0]).unwrap();
        let mut expected = json!({(parent.to_string()): (parent + 2).to_string()});
        if !should_clear {
            expected[(parent + 4).to_string()] = json!(thread.to_string());
        }
        assert_eq!(map, expected, "{agent} slot bindings");
    }
    let mut guard_threads = ROW_GUARD_CALLS.lock().unwrap().clone();
    guard_threads.sort_unstable();
    guard_threads.dedup();
    assert_eq!(guard_threads, expected_guard_threads);
    pool.close().await;
    drop(fixture);
    db.drop().await;
}

#[tokio::test]
async fn outbox_explicit_reset_archives_only_allowed_slot_threads_pg() {
    check_outbox_reset(true).await;
}

#[tokio::test]
async fn outbox_stale_reset_archives_only_allowed_slot_threads_pg() {
    check_outbox_reset(false).await;
}

#[tokio::test]
async fn manual_slot_reset_uses_the_engine_row_guard_and_archives_pg() {
    let root = TestRuntimeRootGuard::new();
    let _config = TestEnvVarGuard::remove_after_shared_test_env_lock("AGENTDESK_CONFIG");
    let db = TestPostgresDb::create().await;
    let fixture = Fixture::new(&root, &db, MANUAL_PARENT_BASE).await;
    let broadcast_tx = crate::eventbus::new_broadcast();
    let state = crate::app_state::AppState {
        pg_pool: Some(fixture.pool.clone()),
        engine: fixture.engine(),
        config: Arc::new(fixture.config.clone()),
        batch_buffer: crate::eventbus::spawn_batch_flusher(broadcast_tx.clone()),
        broadcast_tx,
        health_registry: None,
        cluster_instance_id: None,
    };
    for (offset, case) in CASES.into_iter().enumerate() {
        let agent = case.0;
        let parent = MANUAL_PARENT_BASE + (offset as u64) * 10;
        fixture.seed_slot(case, parent, 0, 1).await;
        let pool = &fixture.pool;
        let thread = parent + 1;
        let original = thread_rows(pool, thread).await;
        let original_map = slot_map(pool, agent).await;
        let (status, Json(result)) = crate::server::routes::auto_queue::reset_slot_thread(
            State(state.clone()),
            Path((agent.to_string(), 0)),
        )
        .await
        .unwrap();
        let should_clear = agent == "legacy";
        assert_eq!(status, StatusCode::OK);
        let count = usize::from(should_clear);
        assert_eq!(
            result,
            json!({
                "ok": true, "agent_id": agent, "slot_index": 0, "archived_threads": count,
                "cleared_sessions": count, "cleared_bindings": count,
            })
        );
        assert_eq!(
            fixture.threads.lock().unwrap()[&thread].archived,
            should_clear
        );
        assert_eq!(
            ROW_GUARD_CALLS.lock().unwrap().contains(&thread),
            agent != "hosted"
        );
        if should_clear {
            assert_session_cleared(pool, thread).await;
            assert_eq!(slot_map(pool, agent).await, ["{}"]);
        } else {
            assert_eq!(thread_rows(pool, thread).await, original);
            assert_eq!(slot_map(pool, agent).await, original_map);
        }
    }
    fixture.pool.close().await;
    drop(fixture);
    db.drop().await;
}
