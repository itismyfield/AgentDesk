//! The operator route through the API router on the fake Herdr rig and a throwaway PG, and the
//! helpers the route's own tests share; a test changing the live config runs in a child process.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tracing_subscriber::layer::SubscriberExt;

use super::{probe_after_capture, probe_after_close, probe_after_fence};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::health::HealthRegistry;
use crate::services::session_host::herdr_termination_rig as herdr;

const CHILD_ENV: &str = "ADK_HERDR_TERMINATE_ROUTE_CHILD";
pub(crate) const LOOPBACK: &str = "127.0.0.1:40000";
pub(crate) const JSON: (&str, &str) = ("content-type", "application/json");
const CHANNEL: u64 = 5_340_211;
/// The rig's pane, the only one a close may name.
const PANE: &str = "w1-1";

pub(crate) fn body(key: &str, nonce: &str) -> String {
    json!({"session_key": key, "execution_nonce": nonce, "reason": "operator test"}).to_string()
}

/// Runs test `path` (its module path and name) alone in a child process that owns the live
/// config; true in that child.
pub(crate) fn in_child(path: &str) -> bool {
    let name = path.split_once("::").unwrap().1;
    if let Some(child) = std::env::var_os(CHILD_ENV) {
        assert_eq!(child, name);
        return true;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, name)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success()
            && stdout.lines().any(|line| line
                .starts_with("test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; ")),
        "{name}: {}\n{stdout}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    eprintln!("isolated child verified: {name}; selected=1");
    false
}

pub(crate) fn switch(on: bool) {
    let live = crate::config_live_reload::current();
    let mut config = live.map(|c| (*c).clone()).unwrap_or_default();
    config.runtime.herdr_terminate_enabled = on.then_some(true);
    crate::config_live_reload::install(config);
}

pub(crate) fn current_thread() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// The API router with its auth middleware, `token` as the server's configured token.
pub(crate) fn app(
    token: Option<&str>,
    registry: Option<Arc<HealthRegistry>>,
    pool: Option<sqlx::PgPool>,
) -> Router {
    let mut engine_config = crate::config::Config::default();
    engine_config.policies.hot_reload = false;
    let engine = crate::engine::PolicyEngine::new(&engine_config).unwrap();
    let tx = crate::server::ws::new_broadcast();
    let buffer = crate::server::ws::spawn_batch_flusher(tx.clone());
    let mut config = crate::config::Config::default();
    config.server.auth_token = token.map(str::to_string);
    crate::server::routes::api_router_with_pg(engine, config, tx, buffer, registry, pool)
}

pub(crate) async fn post(
    app: &Router,
    peer: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method("POST").uri("/herdr/terminate");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let mut request = request.body(Body::from(body.to_string())).unwrap();
    let peer: SocketAddr = peer.parse().unwrap();
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(peer));
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

pub(crate) async fn closed_pool() -> sqlx::PgPool {
    let pool = unreachable_pool();
    pool.close().await;
    pool
}

pub(crate) fn unreachable_pool() -> sqlx::PgPool {
    sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://postgres@127.0.0.1:1/absent")
        .unwrap()
}

pub(crate) fn refusal(status: StatusCode, result: &str, detail: &str) -> (StatusCode, Value) {
    (
        status,
        json!({"ok": false, "result": result, "detail": detail}),
    )
}

/// Result records the terminate target wrote on this thread, field by field.
#[derive(Clone, Default)]
pub(crate) struct Results(Arc<Mutex<Vec<BTreeMap<String, String>>>>);

impl Results {
    pub(crate) fn take(&self) -> Vec<BTreeMap<String, String>> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

struct Fields(BTreeMap<String, String>);

impl tracing::field::Visit for Fields {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}"));
    }
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Results {
    fn on_event(&self, event: &tracing::Event<'_>, _: tracing_subscriber::layer::Context<'_, S>) {
        if event.metadata().target() == "agentdesk::herdr_terminate" {
            let mut fields = Fields(BTreeMap::new());
            event.record(&mut fields);
            self.0.lock().unwrap().push(fields.0);
        }
    }
}

pub(crate) fn capture() -> (Results, tracing::subscriber::DefaultGuard) {
    crate::logging::test_capture::pin_callsite_interest();
    let results = Results::default();
    let subscriber = tracing_subscriber::registry().with(results.clone());
    (results, tracing::subscriber::set_default(subscriber))
}

/// Each record's result, after checking it names the request and its reason.
pub(crate) fn results_of(
    records: Vec<BTreeMap<String, String>>,
    key: &str,
    nonce: &str,
) -> Vec<String> {
    records
        .into_iter()
        .map(|record| {
            assert_eq!(record["session_key"], key, "{record:?}");
            assert_eq!(record["execution_nonce"], nonce, "{record:?}");
            assert_eq!(record["reason"], "operator test", "{record:?}");
            record["result"].clone()
        })
        .collect()
}

/// A row bound to the rig's pane on a throwaway PG, its runtime registered, the switch on.
struct Node {
    db: TestPostgresDb,
    pool: sqlx::PgPool,
    shared: Arc<super::SharedData>,
    registry: Arc<HealthRegistry>,
    key: String,
    _switches: herdr::LiveSwitches,
}

impl Node {
    async fn bind(rig: &herdr::P11TerminationRig, token_hash: Option<&str>) -> Self {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let (shared, registry) =
            crate::services::discord::host_teardown_gate::test_support::runtime(&pool).await;
        let hash = token_hash.unwrap_or(&shared.token_hash).to_string();
        let key = format!("claude/{hash}/{}:AgentDesk-claude-act", herdr::NODE);
        let state = herdr::HostedState::Bound;
        let _switches = rig.bind(&pool, &hash, &key, state).await;
        Self {
            db,
            pool,
            shared,
            registry,
            key,
            _switches,
        }
    }

    fn app(&self) -> Router {
        app(None, Some(self.registry.clone()), Some(self.pool.clone()))
    }

    async fn terminate(&self, nonce: &str) -> (StatusCode, Value) {
        post(&self.app(), LOOPBACK, &[JSON], &body(&self.key, nonce)).await
    }

    async fn row(&self) -> Option<(herdr::HostedState, String)> {
        herdr::row(&self.pool, &self.key).await
    }

    fn bound(&self) -> Option<(herdr::HostedState, String)> {
        Some((herdr::HostedState::Bound, herdr::EXECUTION.to_string()))
    }

    async fn finish(self) {
        self.pool.close().await;
        self.db.drop().await;
    }
}

/// The rig for test `name` in its child, with this thread and the runtime's on its registry.
fn rig_child(name: &str) -> Option<(herdr::P11TerminationRig, tokio::runtime::Runtime)> {
    let path = format!("{}::{name}", module_path!());
    if !herdr::in_child(&path, CHANNEL) {
        return None;
    }
    let rig = herdr::P11TerminationRig::boot(CHANNEL);
    rig.thread_installer()();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .on_thread_start(rig.thread_installer())
        .build()
        .unwrap();
    Some((rig, runtime))
}

fn closes(rig: &herdr::P11TerminationRig) -> usize {
    let writes = rig.writes();
    assert!(
        writes
            .iter()
            .all(|(method, pane)| method.ends_with(".close") && pane == PANE),
        "only closes of the rig's pane: {writes:?}"
    );
    writes.len()
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

// A nonce that is not the row's, or a row that does not exist, is refused with no close and no
// change; each still leaves one result record.
#[test]
fn act_route_wrong_nonce_zero_effect_pg() {
    let Some((rig, runtime)) = rig_child("act_route_wrong_nonce_zero_effect_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, None).await;
        let (results, _logs) = capture();
        let (status, reply) = node.terminate("other-nonce").await;
        assert_eq!(
            (status, &reply["result"]),
            (StatusCode::CONFLICT, &json!("refused"))
        );
        assert_eq!(reply["detail"], "execution identity differs");
        assert_eq!(closes(&rig), 0);
        assert_eq!(node.row().await, node.bound());
        assert!(herdr::held());
        herdr::input_pin(&rig.record(&node.shared.token_hash, &node.key)).unwrap();
        let logged = results_of(results.take(), &node.key, "other-nonce");
        assert_eq!(logged, ["refused"]);

        let missing = "claude/h/mac-mini:AgentDesk-claude-missing";
        let (status, reply) = post(&node.app(), LOOPBACK, &[JSON], &body(missing, "n")).await;
        assert_eq!(
            (status, &reply["detail"]),
            (StatusCode::CONFLICT, &json!("missing row"))
        );
        assert_eq!(results_of(results.take(), missing, "n"), ["refused"]);
        node.finish().await;
    });
}

// Herdr's confirmation answer is a typed refusal that leaves the execution and its input open;
// the next request closes, retires, releases the hold and is the only 200.
#[test]
fn act_route_retired_is_the_only_success_pg() {
    let Some((rig, runtime)) = rig_child("act_route_retired_is_the_only_success_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, None).await;
        let record = rig.record(&node.shared.token_hash, &node.key);
        let (results, _logs) = capture();
        rig.require_confirmation(true);
        let (status, reply) = node.terminate(herdr::EXECUTION).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(reply["result"], "confirmation_required");
        assert_eq!(closes(&rig), 1, "one close, no escalation");
        assert_eq!(node.row().await, node.bound());
        assert!(herdr::held());
        herdr::input_pin(&record).unwrap();

        rig.require_confirmation(false);
        let (status, reply) = node.terminate(herdr::EXECUTION).await;
        let detail = "retired";
        assert_eq!(
            (status, reply),
            (
                StatusCode::OK,
                json!({"ok": true, "result": "retired", "detail": detail,
                    "session_key": node.key, "execution_nonce": herdr::EXECUTION})
            )
        );
        assert_eq!(closes(&rig), 2);
        let retired = (herdr::HostedState::Retired, herdr::EXECUTION.to_string());
        assert_eq!(node.row().await, Some(retired));
        assert!(!herdr::held());
        let logged = results_of(results.take(), &node.key, herdr::EXECUTION);
        assert_eq!(logged, ["confirmation_required", "retired"]);
        node.finish().await;
    });
}

// A row that moved on while the close was in flight fails the retire CAS: the reply is a server
// error, never a success, and the successor row is untouched.
#[test]
fn act_route_stale_cas_is_indeterminate_pg() {
    let Some((rig, runtime)) = rig_child("act_route_stale_cas_is_indeterminate_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, None).await;
        let mut hold = rig.hold_next_write();
        let (app, payload) = (node.app(), body(&node.key, herdr::EXECUTION));
        let request = tokio::spawn(async move { post(&app, LOOPBACK, &[JSON], &payload).await });
        tokio::time::timeout(Duration::from_secs(10), &mut hold.reached)
            .await
            .expect("the close reached Herdr")
            .unwrap();
        let successor = "fedcba9876543210fedcba9876543210";
        sqlx::query("UPDATE sessions SET hosted_execution = jsonb_set(hosted_execution, '{execution_nonce}', to_jsonb($2::text)) WHERE session_key = $1")
            .bind(&node.key).bind(successor).execute(&node.pool).await.unwrap();
        hold.release.send(()).unwrap();
        let (status, reply) = request.await.unwrap();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!((&reply["result"], &reply["ok"]), (&json!("indeterminate"), &json!(false)));
        assert_eq!(reply["detail"], "retire CAS stale");
        let stored: (String, String) = sqlx::query_as("SELECT hosted_execution->>'state', hosted_execution->>'execution_nonce' FROM sessions WHERE session_key = $1")
            .bind(&node.key).fetch_one(&node.pool).await.unwrap();
        assert_eq!(stored, ("bound".into(), successor.into()), "the successor stays");
        assert_eq!(closes(&rig), 1);
        node.finish().await;
    });
}

// The only registered runtime is another bot's: its token hash differs from the row's owner.
#[test]
fn act_route_foreign_runtime_refused_pg() {
    let Some((rig, runtime)) = rig_child("act_route_foreign_runtime_refused_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, Some("discord_feedfacefeedface")).await;
        let (status, reply) = node.terminate(herdr::EXECUTION).await;
        assert_eq!(
            (status, &reply["result"]),
            (StatusCode::CONFLICT, &json!("refused"))
        );
        assert_eq!(reply["detail"], "execution identity differs");
        assert_eq!(closes(&rig), 0);
        assert_eq!(node.row().await, node.bound());
        assert!(herdr::held());
        node.finish().await;
    });
}

// The endpoint vanishing between the gate target and the pane read is a refusal, not a panic,
// and the fence it had taken opens again.
#[test]
fn act_route_view_vanishes_after_target_is_refused_pg() {
    let Some((rig, runtime)) = rig_child("act_route_view_vanishes_after_target_is_refused_pg")
    else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, None).await;
        probe_after_fence(Box::new(|| {
            Box::pin(async { herdr::clear_endpoints_here() })
        }));
        let (status, reply) = node.terminate(herdr::EXECUTION).await;
        assert_eq!(
            (status, &reply["result"]),
            (StatusCode::CONFLICT, &json!("refused"))
        );
        assert_eq!(reply["detail"], "no local endpoint");
        rig.thread_installer()();
        herdr::input_pin(&rig.record(&node.shared.token_hash, &node.key)).unwrap();
        assert_eq!(closes(&rig), 0);
        assert_eq!(node.row().await, node.bound());
        assert!(herdr::held());
        node.finish().await;
    });
}

// A panic before the close could have been sent reopens the fence; one after the close keeps
// the execution closed. Both are a logged 500, and the row and hold stay for a rerun.
#[test]
fn act_route_panic_reopens_the_fence_only_before_the_close_pg() {
    let Some((rig, runtime)) =
        rig_child("act_route_panic_reopens_the_fence_only_before_the_close_pg")
    else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, None).await;
        let record = rig.record(&node.shared.token_hash, &node.key);
        let (results, _logs) = capture();
        let panicked = (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!("terminate task panicked"),
        );
        probe_after_fence(Box::new(|| {
            Box::pin(async { panic!("injected after the fence") })
        }));
        let (status, reply) = node.terminate(herdr::EXECUTION).await;
        assert_eq!((status, reply["detail"].clone()), panicked);
        assert_eq!(closes(&rig), 0);
        herdr::input_pin(&record).expect("no close was sent, so the fence opened again");

        probe_after_close(Box::new(|| {
            Box::pin(async { panic!("injected after the close") })
        }));
        let (status, reply) = node.terminate(herdr::EXECUTION).await;
        assert_eq!((status, reply["detail"].clone()), panicked);
        assert_eq!(closes(&rig), 1);
        assert!(
            herdr::input_pin(&record).is_err(),
            "a sent close keeps the execution closed"
        );
        assert_eq!(node.row().await, node.bound());
        assert!(herdr::held());
        let logged = results_of(results.take(), &node.key, herdr::EXECUTION);
        assert_eq!(logged, ["indeterminate", "indeterminate"]);
        node.finish().await;
    });
}

// One record per run where the run ends: none for the switch off, one for a panic inside the
// service, and one for a run whose request was dropped while its close was in flight.
#[test]
fn act_route_logs_result_once_pg() {
    let Some((rig, runtime)) = rig_child("act_route_logs_result_once_pg") else {
        return;
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    runtime.block_on(async {
        let node = Node::bind(&rig, None).await;
        let (results, _logs) = capture();
        switch(false);
        let (status, reply) = node.terminate(herdr::EXECUTION).await;
        assert_eq!(
            (status, &reply["result"]),
            (StatusCode::CONFLICT, &json!("disabled"))
        );
        assert!(results.take().is_empty(), "disabled leaves no record");
        switch(true);

        probe_after_capture(Box::new(|| {
            Box::pin(async { panic!("injected after capture") })
        }));
        let (status, reply) = node.terminate(herdr::EXECUTION).await;
        assert_eq!(
            (status, &reply["result"]),
            (StatusCode::INTERNAL_SERVER_ERROR, &json!("indeterminate"))
        );
        let logged = results_of(results.take(), &node.key, herdr::EXECUTION);
        assert_eq!(logged, ["indeterminate"]);

        let mut hold = rig.hold_next_write();
        let (app, payload) = (node.app(), body(&node.key, herdr::EXECUTION));
        let request = tokio::spawn(async move { post(&app, LOOPBACK, &[JSON], &payload).await });
        tokio::time::timeout(Duration::from_secs(10), &mut hold.reached)
            .await
            .expect("the close reached Herdr")
            .unwrap();
        request.abort();
        assert!(
            request.await.unwrap_err().is_cancelled(),
            "the request was dropped"
        );
        hold.release.send(()).unwrap();
        wait_for("the dropped run's record", async || {
            !results.0.lock().unwrap().is_empty()
        })
        .await;
        let logged = results_of(results.take(), &node.key, herdr::EXECUTION);
        assert_eq!(logged, ["retired"]);
        let retired = (herdr::HostedState::Retired, herdr::EXECUTION.to_string());
        assert_eq!(node.row().await, Some(retired));
        assert!(!herdr::held());
        node.finish().await;
    });
}
