//! E2E Smoke Test — shared server + API lifecycle verification
//!
//! Splits the original full-lifecycle smoke test into focused scenarios while
//! reusing a single agentdesk process per test binary for faster, clearer
//! failures.

use reqwest::{Client, StatusCode};
use serde_json::{Value, json};
use std::io::{Read, Write};
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
    database: SmokeDatabase,
}

/// The server refuses to boot without PostgreSQL, so the shared server gets a
/// database of its own on the same fixture server the lib PG tests use.
struct SmokeDatabase {
    admin_url: String,
    name: String,
    url: String,
}

impl SmokeDatabase {
    /// `None` only without a fixture base; `AGENTDESK_REQUIRE_PG=1` makes that a failure (#5218).
    fn create() -> Option<Self> {
        let base = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE")
            .ok()
            .map(|value| value.trim().trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty());
        let Some(base) = base else {
            assert_ne!(
                std::env::var("AGENTDESK_REQUIRE_PG").as_deref(),
                Ok("1"),
                "PG required but POSTGRES_TEST_DATABASE_URL_BASE unset"
            );
            // Straight to stderr, which libtest does not capture, so the skip
            // shows in the log and as a CI annotation rather than as a quiet pass.
            let _ = writeln!(
                std::io::stderr(),
                "\n::warning title=e2e smoke skipped::POSTGRES_TEST_DATABASE_URL_BASE is unset; the smoke tests need PostgreSQL and did not start a server"
            );
            return None;
        };
        let base = explicit_fixture_base(&base)
            .unwrap_or_else(|error| panic!("POSTGRES_TEST_DATABASE_URL_BASE: {error}"));
        let admin_db =
            std::env::var("POSTGRES_TEST_ADMIN_DB").unwrap_or_else(|_| "postgres".to_string());
        let name = format!("agentdesk_e2e_smoke_{}", uuid::Uuid::new_v4().simple());
        let database = Self {
            admin_url: fixture_database_url(&base, &admin_db),
            url: fixture_database_url(&base, &name),
            name,
        };
        database
            .admin(format!("CREATE DATABASE \"{}\"", database.name))
            .expect("failed to create smoke-test database");
        Some(database)
    }

    fn drop_database(&self) {
        let _ = self.admin(format!(
            "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
            self.name
        ));
    }

    /// Runs one statement on its own thread and runtime, because callers sit
    /// inside a test's runtime or an atexit hook where blocking is not allowed.
    fn admin(&self, sql: String) -> Result<(), sqlx::Error> {
        let admin_url = self.admin_url.clone();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build admin runtime")
                .block_on(async move {
                    use sqlx::Connection;
                    let mut connection = sqlx::PgConnection::connect(&admin_url).await?;
                    sqlx::raw_sql(&sql).execute(&mut connection).await?;
                    connection.close().await
                })
        })
        .join()
        .expect("admin statement thread panicked")
    }
}

/// The base must name its server like the lib fixtures' (`fixture_target.rs`): sqlx
/// fills a missing host or port from PGHOST/PGPORT, which could be any server.
fn explicit_fixture_base(base: &str) -> Result<url::Url, String> {
    let url = url::Url::parse(base).map_err(|error| format!("not a URL: {error}"))?;
    let host = url.host_str().filter(|host| !host.is_empty());
    let Some(host) = host else {
        return Err("the URL must name a host".to_string());
    };
    if host.starts_with('/') || host.get(..3).is_some_and(|p| p.eq_ignore_ascii_case("%2f")) {
        return Err("a Unix socket is not a supported fixture host".to_string());
    }
    if url.port().is_none() {
        return Err("the URL must name a port".to_string());
    }
    Ok(url)
}

/// `base` pointed at `database`: replaces the path and drops `dbname`, which sqlx reads last.
fn fixture_database_url(base: &url::Url, database: &str) -> String {
    let mut url = base.clone();
    url.set_path(&format!("/{database}"));
    let query: Vec<(String, String)> = base
        .query_pairs()
        .filter(|(key, _)| key != "dbname")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    url.set_query(None);
    if !query.is_empty() {
        url.query_pairs_mut().extend_pairs(query);
    }
    let options: sqlx::postgres::PgConnectOptions = url
        .as_str()
        .parse()
        .expect("fixture database URL does not parse");
    assert_eq!(options.get_database(), Some(database));
    url.into()
}

#[test]
fn explicit_fixture_base_rejects_ambient_server_selection() {
    for base in [
        "postgresql:///postgres?sslmode=disable",
        "postgresql://postgres@127.0.0.1/postgres",
        "postgresql://postgres@%2Ftmp%2Fpg:5432/postgres",
    ] {
        assert!(explicit_fixture_base(base).is_err(), "{base} was accepted");
    }
    assert!(explicit_fixture_base("postgresql://postgres@127.0.0.1:5432/postgres").is_ok());
}

#[test]
fn fixture_database_url_targets_the_named_database() {
    for base in [
        "postgresql://postgres:postgres@127.0.0.1:5432",
        "postgresql://postgres:postgres@127.0.0.1:5432/postgres?application_name=ci",
        "postgresql://postgres:postgres@127.0.0.1:5432/?dbname=postgres&sslmode=disable",
    ] {
        let url = fixture_database_url(&url::Url::parse(base).expect("base parses"), "smoke_db");
        assert!(url.contains("/smoke_db"), "{base} -> {url}");
        assert!(!url.contains("dbname"), "{base} -> {url}");
    }
}

impl TestServer {
    /// Start an isolated AgentDesk server on a random available port.
    fn start(database: SmokeDatabase) -> Self {
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
database:
  enabled: true
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
            .env("DATABASE_URL", &database.url)
            .env("RUST_LOG", "agentdesk=warn")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to start agentdesk binary");

        Self {
            child: Mutex::new(child),
            port,
            temp_dir,
            database,
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

static SHARED_SERVER: OnceLock<Option<TestServer>> = OnceLock::new();
static SHARED_SERVER_CLEANUP: Once = Once::new();
static SHARED_SERVER_STARTS: AtomicUsize = AtomicUsize::new(0);
static RESOURCE_COUNTER: AtomicUsize = AtomicUsize::new(0);
static TEST_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

extern "C" fn cleanup_shared_server() {
    if let Some(Some(server)) = SHARED_SERVER.get() {
        if let Ok(mut child) = server.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        server.database.drop_database();
        let _ = std::fs::remove_dir_all(&server.temp_dir);
    }
}

fn shared_server() -> Option<&'static TestServer> {
    SHARED_SERVER
        .get_or_init(|| {
            let database = SmokeDatabase::create()?;
            SHARED_SERVER_CLEANUP.call_once(|| unsafe {
                let _ = libc::atexit(cleanup_shared_server);
            });
            SHARED_SERVER_STARTS.fetch_add(1, Ordering::SeqCst);
            Some(TestServer::start(database))
        })
        .as_ref()
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
    /// `None` when no PostgreSQL fixture base is configured (see [`SmokeDatabase::create`]).
    async fn new(prefix: &str) -> Option<Self> {
        let guard = suite_lock().await;
        let server = shared_server()?;
        server.wait_ready().await;
        assert_eq!(
            SHARED_SERVER_STARTS.load(Ordering::SeqCst),
            1,
            "shared smoke-test server should start exactly once"
        );

        Some(Self {
            _guard: guard,
            client: Client::new(),
            server,
            prefix: next_resource_name(prefix),
        })
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
    let Some(ctx) = TestContext::new("smoke-health-and-agents").await else {
        return;
    };

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
#[ignore = "requires PG-aware smoke server boot; create_dispatch_with_options is PG-only after R4"]
async fn smoke_cards_and_dispatches() {
    let Some(ctx) = TestContext::new("smoke-cards-and-dispatches").await else {
        return;
    };

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
    let Some(ctx) = TestContext::new("smoke-settings-and-errors").await else {
        return;
    };

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
