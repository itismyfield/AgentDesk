//! Shared helpers for exercising the Herdr termination route in an isolated child.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tracing_subscriber::layer::SubscriberExt;

const CHILD_ENV: &str = "ADK_HERDR_TERMINATE_ROUTE_CHILD";
pub(crate) const LOOPBACK: &str = "127.0.0.1:40000";
pub(crate) const JSON: (&str, &str) = ("content-type", "application/json");

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
pub(crate) struct Results(
    pub(in crate::services::discord) Arc<Mutex<Vec<BTreeMap<String, String>>>>,
);

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
