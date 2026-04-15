//! E2E Smoke Test — shared server + API lifecycle verification
//!
//! Splits the original full-lifecycle smoke test into focused scenarios while
//! reusing a single agentdesk process per test binary for faster, clearer
//! failures.

use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Mutex, Once, OnceLock,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

/// A shared AgentDesk server process reused across smoke tests.
struct TestServer {
    child: Mutex<Child>,
    port: u16,
    temp_dir: PathBuf,
}

impl TestServer {
    /// Start an isolated AgentDesk server on a random available port.
    fn start() -> Self {
        let port = get_free_port();
        let temp_dir = create_server_temp_dir();
        let data_dir = temp_dir.join("data");
        std::fs::create_dir_all(&data_dir).expect("failed to create data dir");

        // Resolve the policies directory relative to the project root.
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let policies_dir = std::path::Path::new(manifest_dir).join("policies");

        // Write a minimal config file.
        let config_path = temp_dir.join("agentdesk.yaml");
        let config_content = format!(
            r#"server:
  port: {port}
  host: "127.0.0.1"
discord: {{}}
agents: []
github:
  repos: []
  sync_interval_minutes: 0
policies:
  dir: "{policies}"
  hot_reload: false
data:
  dir: "{data}"
  db_name: "test.sqlite"
"#,
            port = port,
            policies = policies_dir.display(),
            data = data_dir.display(),
        );
        std::fs::write(&config_path, &config_content).expect("failed to write test config");

        let binary = env!("CARGO_BIN_EXE_agentdesk");
        let child = Command::new(binary)
            .env("AGENTDESK_CONFIG", &config_path)
            .env("AGENTDESK_ROOT_DIR", &temp_dir)
            .env("RUST_LOG", "agentdesk=warn")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to start agentdesk binary");

        Self {
            child: Mutex::new(child),
            port,
            temp_dir,
        }
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn api_url(&self, path: &str) -> String {
        format!("{}/api{}", self.base_url(), path)
    }

    /// Poll health endpoint until the server is ready (max 30 seconds).
    async fn wait_ready(&self) {
        let client = Client::new();
        let url = self.api_url("/health");

        for _ in 0..300 {
            if let Some(failure) = self.startup_failure_context() {
                panic!("{failure}");
            }
            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => return,
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        if let Some(failure) = self.startup_failure_context() {
            panic!("{failure}");
        }
        panic!(
            "server did not become ready within 30 seconds on port {}",
            self.port
        );
    }

    fn startup_failure_context(&self) -> Option<String> {
        let mut child = self
            .child
            .lock()
            .expect("failed to lock shared smoke-test child");
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = read_child_pipe(&mut child.stdout);
                let stderr = read_child_pipe(&mut child.stderr);
                Some(format!(
                    "agentdesk exited before becoming ready on port {} with status {status}\nstdout:\n{}\nstderr:\n{}",
                    self.port,
                    truncate_output(&stdout),
                    truncate_output(&stderr),
                ))
            }
            Ok(None) => None,
            Err(error) => Some(format!(
                "failed to poll shared smoke-test server on port {}: {error}",
                self.port
            )),
        }
    }
}

static SHARED_SERVER: OnceLock<TestServer> = OnceLock::new();
static SHARED_SERVER_CLEANUP: Once = Once::new();
static SHARED_SERVER_STARTS: AtomicUsize = AtomicUsize::new(0);
static RESOURCE_COUNTER: AtomicUsize = AtomicUsize::new(0);
static TEST_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

extern "C" fn cleanup_shared_server() {
    if let Some(server) = SHARED_SERVER.get() {
        if let Ok(mut child) = server.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&server.temp_dir);
    }
}

fn shared_server() -> &'static TestServer {
    SHARED_SERVER.get_or_init(|| {
        SHARED_SERVER_CLEANUP.call_once(|| unsafe {
            let _ = libc::atexit(cleanup_shared_server);
        });
        SHARED_SERVER_STARTS.fetch_add(1, Ordering::SeqCst);
        TestServer::start()
    })
}

async fn suite_lock() -> tokio::sync::MutexGuard<'static, ()> {
    TEST_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

fn create_server_temp_dir() -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "agentdesk-e2e-{}-{}",
        std::process::id(),
        RESOURCE_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    if path.exists() {
        let _ = std::fs::remove_dir_all(&path);
    }
    std::fs::create_dir_all(&path).expect("failed to create server temp dir");
    path
}

fn next_resource_name(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        RESOURCE_COUNTER.fetch_add(1, Ordering::SeqCst)
    )
}

fn next_channel_id() -> String {
    format!(
        "12345678{:010}",
        RESOURCE_COUNTER.fetch_add(1, Ordering::SeqCst)
    )
}

fn get_free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("failed to bind for port");
    listener.local_addr().unwrap().port()
}

fn read_child_pipe(pipe: &mut Option<impl Read>) -> String {
    let mut output = String::new();
    if let Some(pipe) = pipe.as_mut() {
        let _ = pipe.read_to_string(&mut output);
    }
    output
}

fn truncate_output(output: &str) -> String {
    const MAX_CHARS: usize = 2_000;
    let truncated: String = output.chars().take(MAX_CHARS).collect();
    if output.chars().count() > MAX_CHARS {
        format!("{truncated}\n...[truncated]")
    } else if truncated.is_empty() {
        "<empty>".to_string()
    } else {
        truncated
    }
}

struct TestContext {
    _guard: tokio::sync::MutexGuard<'static, ()>,
    client: Client,
    server: &'static TestServer,
    prefix: String,
}

impl TestContext {
    async fn new(prefix: &str) -> Self {
        let guard = suite_lock().await;
        let server = shared_server();
        server.wait_ready().await;
        assert_eq!(
            SHARED_SERVER_STARTS.load(Ordering::SeqCst),
            1,
            "shared smoke-test server should start exactly once"
        );

        Self {
            _guard: guard,
            client: Client::new(),
            server,
            prefix: next_resource_name(prefix),
        }
    }

    fn title(&self, suffix: &str) -> String {
        format!("{} {suffix}", self.prefix)
    }
}

async fn json_response(resp: reqwest::Response) -> (StatusCode, Value) {
    let status = resp.status();
    let body = resp.json().await.unwrap_or_else(|_| json!({}));
    (status, body)
}

async fn list_agents(ctx: &TestContext) -> Vec<Value> {
    let (status, body) = json_response(
        ctx.client
            .get(ctx.server.api_url("/agents"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    body["agents"].as_array().unwrap().clone()
}

async fn get_agent(ctx: &TestContext, agent_id: &str) -> (StatusCode, Value) {
    json_response(
        ctx.client
            .get(ctx.server.api_url(&format!("/agents/{agent_id}")))
            .send()
            .await
            .unwrap(),
    )
    .await
}

async fn create_agent(ctx: &TestContext, label: &str) -> String {
    let agent_id = format!("{}-{label}-agent", ctx.prefix);
    let (status, body) = json_response(
        ctx.client
            .post(ctx.server.api_url("/agents"))
            .json(&json!({
                "id": agent_id,
                "name": format!("{} {label}", ctx.prefix),
                "provider": "claude",
                "discord_channel_id": next_channel_id(),
            }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(
        status.is_success(),
        "create agent should succeed: {status} {body}"
    );
    agent_id
}

async fn update_agent_name(ctx: &TestContext, agent_id: &str, name: &str) {
    let (status, body) = json_response(
        ctx.client
            .patch(ctx.server.api_url(&format!("/agents/{agent_id}")))
            .json(&json!({ "name": name }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(
        status.is_success(),
        "update agent should succeed: {status} {body}"
    );
}

async fn delete_agent(ctx: &TestContext, agent_id: &str) -> (StatusCode, Value) {
    json_response(
        ctx.client
            .delete(ctx.server.api_url(&format!("/agents/{agent_id}")))
            .send()
            .await
            .unwrap(),
    )
    .await
}

async fn list_cards(ctx: &TestContext) -> Vec<Value> {
    let (status, body) = json_response(
        ctx.client
            .get(ctx.server.api_url("/kanban-cards"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    body["cards"].as_array().unwrap().clone()
}

async fn get_card(ctx: &TestContext, card_id: &str) -> (StatusCode, Value) {
    json_response(
        ctx.client
            .get(ctx.server.api_url(&format!("/kanban-cards/{card_id}")))
            .send()
            .await
            .unwrap(),
    )
    .await
}

async fn create_card(ctx: &TestContext, title: &str, priority: &str) -> String {
    let (status, body) = json_response(
        ctx.client
            .post(ctx.server.api_url("/kanban-cards"))
            .json(&json!({
                "title": title,
                "priority": priority,
            }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(
        status.is_success(),
        "create card should succeed: {status} {body}"
    );
    body["card"]["id"]
        .as_str()
        .expect("card id should be a string")
        .to_string()
}

async fn update_card(ctx: &TestContext, card_id: &str, body: Value) {
    let (status, response_body) = json_response(
        ctx.client
            .patch(ctx.server.api_url(&format!("/kanban-cards/{card_id}")))
            .json(&body)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(
        status.is_success(),
        "update card should succeed: {status} {response_body}"
    );
}

async fn delete_card(ctx: &TestContext, card_id: &str) -> (StatusCode, Value) {
    json_response(
        ctx.client
            .delete(ctx.server.api_url(&format!("/kanban-cards/{card_id}")))
            .send()
            .await
            .unwrap(),
    )
    .await
}

async fn list_dispatches_for_card(ctx: &TestContext, card_id: &str) -> Vec<Value> {
    let (status, body) = json_response(
        ctx.client
            .get(
                ctx.server
                    .api_url(&format!("/dispatches?kanban_card_id={card_id}")),
            )
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    body["dispatches"].as_array().unwrap().clone()
}

async fn get_dispatch(ctx: &TestContext, dispatch_id: &str) -> (StatusCode, Value) {
    json_response(
        ctx.client
            .get(ctx.server.api_url(&format!("/dispatches/{dispatch_id}")))
            .send()
            .await
            .unwrap(),
    )
    .await
}

async fn create_dispatch(ctx: &TestContext, card_id: &str, agent_id: &str, title: &str) -> String {
    let (status, body) = json_response(
        ctx.client
            .post(ctx.server.api_url("/dispatches"))
            .json(&json!({
                "kanban_card_id": card_id,
                "to_agent_id": agent_id,
                "title": title,
                "dispatch_type": "implementation",
            }))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(
        status.is_success(),
        "create dispatch should succeed: {status} {body}"
    );
    body["dispatch"]["id"]
        .as_str()
        .expect("dispatch id should be a string")
        .to_string()
}

async fn update_dispatch(ctx: &TestContext, dispatch_id: &str, body: Value) {
    let (status, response_body) = json_response(
        ctx.client
            .patch(ctx.server.api_url(&format!("/dispatches/{dispatch_id}")))
            .json(&body)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(
        status.is_success(),
        "update dispatch should succeed: {status} {response_body}"
    );
}

async fn get_settings(ctx: &TestContext) -> (StatusCode, Value) {
    json_response(
        ctx.client
            .get(ctx.server.api_url("/settings"))
            .send()
            .await
            .unwrap(),
    )
    .await
}

async fn put_settings(ctx: &TestContext, body: Value) {
    let (status, response_body) = json_response(
        ctx.client
            .put(ctx.server.api_url("/settings"))
            .json(&body)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert!(
        status.is_success(),
        "put settings should succeed: {status} {response_body}"
    );
}

async fn get_stats(ctx: &TestContext) -> (StatusCode, Value) {
    json_response(
        ctx.client
            .get(ctx.server.api_url("/stats"))
            .send()
            .await
            .unwrap(),
    )
    .await
}

fn find_by_id<'a>(items: &'a [Value], id: &str) -> Option<&'a Value> {
    items
        .iter()
        .find(|item| item["id"].as_str().is_some_and(|value| value == id))
}

// ── Smoke Tests ────────────────────────────────────────────────

#[tokio::test]
#[cfg_attr(
    target_os = "windows",
    ignore = "server startup unreliable on Windows CI"
)]
async fn smoke_health_and_agents() {
    let ctx = TestContext::new("smoke-health-and-agents").await;

    let (status, body) = json_response(
        ctx.client
            .get(ctx.server.api_url("/health"))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "health check should return 200");
    assert_eq!(body["ok"], true);
    assert_eq!(body["db"], true);

    let agent_id = format!("{}-primary-agent", ctx.prefix);
    let agents = list_agents(&ctx).await;
    assert!(
        find_by_id(&agents, &agent_id).is_none(),
        "shared server should not already contain this test's agent"
    );

    let created_agent_id = create_agent(&ctx, "primary").await;
    assert_eq!(created_agent_id, agent_id);

    let agents = list_agents(&ctx).await;
    let agent = find_by_id(&agents, &agent_id).expect("created agent should be listed");
    assert_eq!(agent["name"], format!("{} primary", ctx.prefix));

    let (status, body) = get_agent(&ctx, &agent_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["agent"]["id"], agent_id);

    let updated_name = format!("{} updated", ctx.prefix);
    update_agent_name(&ctx, &agent_id, &updated_name).await;

    let (status, body) = get_agent(&ctx, &agent_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["agent"]["name"], updated_name);

    let (status, body) = get_agent(&ctx, "nonexistent-id").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.get("error").is_some(),
        "non-existent agent should have error field"
    );

    let (status, _) = get_card(&ctx, "nonexistent-id").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = delete_agent(&ctx, &agent_id).await;
    assert!(
        status.is_success(),
        "agent cleanup should succeed: {status} {body}"
    );

    let agents = list_agents(&ctx).await;
    assert!(
        find_by_id(&agents, &agent_id).is_none(),
        "agent should be removed after cleanup"
    );
}

#[tokio::test]
#[cfg_attr(
    target_os = "windows",
    ignore = "server startup unreliable on Windows CI"
)]
async fn smoke_cards_and_dispatches() {
    let ctx = TestContext::new("smoke-cards-and-dispatches").await;

    let agent_id = create_agent(&ctx, "dispatch").await;
    let card_title = ctx.title("Implement Feature X");

    let cards = list_cards(&ctx).await;
    assert!(
        cards.iter().all(|card| card["title"] != card_title),
        "shared server should not already contain this test's card"
    );

    let card_id = create_card(&ctx, &card_title, "high").await;

    let cards = list_cards(&ctx).await;
    let card = find_by_id(&cards, &card_id).expect("created card should be listed");
    assert_eq!(card["title"], card_title);
    assert_eq!(card["priority"], "high");

    let (status, body) = get_card(&ctx, &card_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["card"]["id"], card_id);

    update_card(
        &ctx,
        &card_id,
        json!({
            "assigned_agent_id": agent_id,
            "status": "ready",
        }),
    )
    .await;

    let (status, body) = get_card(&ctx, &card_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["card"]["assigned_agent_id"], agent_id);
    assert_eq!(body["card"]["status"], "ready");

    let dispatch_id = create_dispatch(&ctx, &card_id, &agent_id, &ctx.title("Dispatch")).await;

    let dispatches = list_dispatches_for_card(&ctx, &card_id).await;
    let dispatch = find_by_id(&dispatches, &dispatch_id).expect("dispatch should be listed");
    assert_eq!(dispatch["kanban_card_id"], card_id);

    let (status, body) = get_dispatch(&ctx, &dispatch_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["dispatch"]["id"], dispatch_id);
    assert_eq!(body["dispatch"]["status"], "pending");

    update_dispatch(
        &ctx,
        &dispatch_id,
        json!({
            "status": "completed",
            "result": {
                "summary": "Feature X implemented successfully",
                "agent_response_present": true
            },
        }),
    )
    .await;

    let (status, body) = get_dispatch(&ctx, &dispatch_id).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["dispatch"]["status"], "completed");

    // Dispatch delete is not exposed in the smoke-test API surface, so this
    // scenario relies on per-test prefixes rather than full DB cleanup.
    let (status, body) = delete_agent(&ctx, &agent_id).await;
    assert!(
        status == StatusCode::OK || status == StatusCode::INTERNAL_SERVER_ERROR,
        "agent cleanup should either succeed or fail gracefully with FK references: {status} {body}"
    );
    if status == StatusCode::INTERNAL_SERVER_ERROR {
        let (health_status, _) = json_response(
            ctx.client
                .get(ctx.server.api_url("/health"))
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            health_status,
            StatusCode::OK,
            "server should stay healthy after FK-constrained cleanup"
        );
    }
}

#[tokio::test]
#[cfg_attr(
    target_os = "windows",
    ignore = "server startup unreliable on Windows CI"
)]
async fn smoke_settings_and_errors() {
    let ctx = TestContext::new("smoke-settings-and-errors").await;

    let (status, original_settings) = get_settings(&ctx).await;
    assert_eq!(status, StatusCode::OK);

    let settings_body = json!({
        "theme": "dark",
        "language": "ko",
        "smoke_test_run": ctx.prefix,
    });
    put_settings(&ctx, settings_body.clone()).await;

    let (status, body) = get_settings(&ctx).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, settings_body);

    let (status, _) = get_stats(&ctx).await;
    assert_eq!(status, StatusCode::OK);

    let agent_id = create_agent(&ctx, "cleanup").await;
    let cleanup_card_title = ctx.title("Cleanup Card");
    let card_id = create_card(&ctx, &cleanup_card_title, "medium").await;

    let (status, body) = delete_card(&ctx, &card_id).await;
    assert!(
        status.is_success(),
        "card cleanup should succeed for an unreferenced smoke-test card: {status} {body}"
    );

    let (status, body) = delete_agent(&ctx, &agent_id).await;
    assert!(
        status.is_success(),
        "agent cleanup should succeed for an unreferenced smoke-test agent: {status} {body}"
    );

    let cards = list_cards(&ctx).await;
    assert!(
        find_by_id(&cards, &card_id).is_none(),
        "cleanup card should be removed"
    );

    let agents = list_agents(&ctx).await;
    assert!(
        find_by_id(&agents, &agent_id).is_none(),
        "cleanup agent should be removed"
    );

    put_settings(&ctx, original_settings).await;
}
