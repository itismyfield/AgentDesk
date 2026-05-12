//! Shared helpers, fixtures, mocks and types for split routes_tests modules.
//! Extracted verbatim from the original routes_tests.rs (tests removed).
//!
//! These helpers are accessible to sibling test modules via `use super::common::*;`.
//! All items are exposed as `pub(super)` so they remain crate-private.

#![allow(dead_code, unused_imports)]

// In the original single-file module `use super::*` reached into the parent
// `routes` module. After the split, this file lives at
// `routes::routes_tests::common`, so the same glob requires `super::super`.
use super::super::*;
use axum::body::{Body, HttpBody as _};
use axum::http::{Request, StatusCode};
use serde_json::json;
use sqlx::Row;
use std::ffi::OsString;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::pin::Pin;
use std::process::Command;
use std::sync::Arc;
use std::sync::MutexGuard;
use tower::ServiceExt;

pub(super) fn test_db() -> Db {
    crate::db::test_db()
}

/// Seed test agents for dispatch-related tests (#245 agent-exists guard).
pub(super) fn seed_test_agents(db: &Db) {
    let c = db.separate_conn().unwrap();
    c.execute_batch(
        "INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('ch-td', 'TD', '111', '222');
         INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('ag1', 'Agent1', '333', '444');
         INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '555', '666');"
    ).unwrap();
}

pub(super) fn test_engine(db: &Db) -> PolicyEngine {
    // Disable FSEvents-backed policy hot reload in tests. Each PolicyEngine
    // construction registers a new macOS FSEvents watcher on ./policies, and
    // the test harness reuses one process across thousands of tests. The
    // watcher handles accumulate (notify v6 cannot reliably free the FSEvents
    // stream on drop), and once macOS fseventsd's f2d_register_rpc starts
    // throttling, the next watch() call blocks indefinitely — surfacing as a
    // `cargo test --bin agentdesk` hang with no progress.
    let mut config = crate::config::Config::default();
    config.policies.hot_reload = false;
    PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap()
}

pub(super) fn test_engine_with_pg(_db: &Db, pg_pool: sqlx::PgPool) -> PolicyEngine {
    let mut config = crate::config::Config::default();
    config.policies.hot_reload = false;
    PolicyEngine::new_with_pg(&config, Some(pg_pool)).unwrap()
}

pub(super) fn test_api_router(
    db: Db,
    engine: PolicyEngine,
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
) -> axum::Router {
    test_api_router_with_config(
        db,
        engine,
        crate::config::Config::default(),
        health_registry,
    )
}

pub(super) fn test_api_router_with_config(
    db: Db,
    engine: PolicyEngine,
    config: crate::config::Config,
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
) -> axum::Router {
    let tx = crate::server::ws::new_broadcast();
    let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
    api_router(db, engine, config, tx, buf, health_registry)
}

pub(super) fn test_api_router_with_pg(
    db: Db,
    engine: PolicyEngine,
    config: crate::config::Config,
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
    pg_pool: sqlx::PgPool,
) -> axum::Router {
    let tx = crate::server::ws::new_broadcast();
    let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
    api_router_with_pg_for_tests(db, engine, config, tx, buf, health_registry, Some(pg_pool))
}

pub(super) async fn read_sse_body_until(body: &mut Body, needles: &[&str]) -> String {
    let mut output = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);

    while !needles.iter().all(|needle| output.contains(needle)) {
        let frame = tokio::time::timeout_at(
            deadline,
            futures::future::poll_fn(|cx| Pin::new(&mut *body).poll_frame(cx)),
        )
        .await
        .expect("timed out waiting for SSE frame")
        .expect("stream should still be open")
        .expect("stream frame should be readable");

        if let Ok(data) = frame.into_data() {
            output.push_str(&String::from_utf8_lossy(&data));
        }
    }

    output
}

pub(super) struct TestPostgresDb {
    pub(super) _lock: crate::db::postgres::PostgresTestLifecycleGuard,
    pub(super) admin_url: String,
    pub(super) database_name: String,
    pub(super) database_url: String,
    pub(super) cleanup_armed: bool,
}

impl TestPostgresDb {
    pub(super) async fn create() -> Self {
        let lock = crate::db::postgres::lock_test_lifecycle();
        let admin_url = postgres_admin_database_url();
        let database_name = format!("agentdesk_routes_{}", uuid::Uuid::new_v4().simple());
        let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
        crate::db::postgres::create_test_database(&admin_url, &database_name, "routes tests")
            .await
            .unwrap();

        Self {
            _lock: lock,
            admin_url,
            database_name,
            database_url,
            cleanup_armed: true,
        }
    }

    pub(super) async fn connect_and_migrate(&self) -> sqlx::PgPool {
        crate::db::postgres::connect_test_pool_and_migrate(&self.database_url, "routes tests")
            .await
            .unwrap()
    }

    pub(super) async fn drop(mut self) {
        let drop_result = crate::db::postgres::drop_test_database(
            &self.admin_url,
            &self.database_name,
            "routes tests",
        )
        .await;
        if drop_result.is_ok() {
            self.cleanup_armed = false;
        }
        drop_result.expect("drop postgres test db");
    }
}

impl Drop for TestPostgresDb {
    fn drop(&mut self) {
        if !self.cleanup_armed {
            return;
        }

        cleanup_test_postgres_db_from_drop(self.admin_url.clone(), self.database_name.clone());
    }
}

pub(super) fn cleanup_test_postgres_db_from_drop(admin_url: String, database_name: String) {
    let cleanup_database_name = database_name.clone();
    let thread_name = format!("routes tests cleanup {cleanup_database_name}");
    let spawn_result = std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("routes tests cleanup runtime failed for {database_name}: {error}");
                    return;
                }
            };

            if let Err(error) = runtime.block_on(crate::db::postgres::drop_test_database(
                &admin_url,
                &database_name,
                "routes tests",
            )) {
                eprintln!("routes tests cleanup failed for {database_name}: {error}");
            }
        });

    match spawn_result {
        Ok(handle) => {
            if handle.join().is_err() {
                eprintln!("routes tests cleanup thread panicked for {cleanup_database_name}");
            }
        }
        Err(error) => {
            eprintln!(
                "routes tests cleanup thread spawn failed for {cleanup_database_name}: {error}"
            );
        }
    }
}

pub(super) fn postgres_base_database_url() -> String {
    if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
        let trimmed = base.trim();
        if !trimmed.is_empty() {
            return trimmed.trim_end_matches('/').to_string();
        }
    }

    let user = std::env::var("PGUSER")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("USER")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "postgres".to_string());
    let password = std::env::var("PGPASSWORD")
        .ok()
        .filter(|value| !value.trim().is_empty());
    let host = std::env::var("PGHOST")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "localhost".to_string());
    let port = std::env::var("PGPORT")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "5432".to_string());

    match password {
        Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
        None => format!("postgresql://{user}@{host}:{port}"),
    }
}

pub(super) fn postgres_admin_database_url() -> String {
    let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "postgres".to_string());
    format!("{}/{}", postgres_base_database_url(), admin_db)
}

pub(super) fn env_lock() -> MutexGuard<'static, ()> {
    crate::services::discord::runtime_store::lock_test_env()
}

pub(super) struct EnvVarGuard {
    pub(super) key: &'static str,
    pub(super) previous: Option<OsString>,
}

impl EnvVarGuard {
    pub(super) fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }

    pub(super) fn set_path(key: &'static str, value: &std::path::Path) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

pub(super) fn seed_startup_doctor_artifact(
    runtime_root: &std::path::Path,
    artifact: serde_json::Value,
) -> std::path::PathBuf {
    let runtime_dir = runtime_root.join("runtime");
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::write(runtime_dir.join("dcserver.pid"), "4242\n").unwrap();
    let boot_id = crate::cli::doctor::startup::current_boot_id().unwrap();
    let artifact_dir = runtime_dir.join("doctor").join("startup");
    fs::create_dir_all(&artifact_dir).unwrap();
    let artifact_path = artifact_dir.join(format!("{boot_id}.json"));
    fs::write(
        &artifact_path,
        serde_json::to_string_pretty(&artifact).unwrap(),
    )
    .unwrap();
    artifact_path
}

pub(super) fn sample_startup_doctor_artifact() -> serde_json::Value {
    json!({
        "schema_version": 1,
        "ok": false,
        "boot_id": "4242-test",
        "started_at": "2026-04-26T14:49:14+09:00",
        "completed_at": "2026-04-26T14:49:17+09:00",
        "run_context": "startup_once",
        "non_fatal": true,
        "summary": {"passed": 2, "warned": 1, "failed": 1, "total": 4},
        "checks": [
            {"id": "server", "status": "pass", "ok": true},
            {"id": "disk_usage", "status": "warn", "ok": true},
            {"id": "dispatch_outbox", "status": "fail", "ok": false},
            {"id": "credentials", "status": "pass", "ok": true}
        ]
    })
}

pub(super) fn local_get_request(uri: &str) -> Request<Body> {
    let mut request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.1:8791".parse::<std::net::SocketAddr>().unwrap(),
    ));
    request
}

pub(super) fn write_test_skill(
    runtime_root: &std::path::Path,
    skill_name: &str,
    description: &str,
) {
    let skill_dir = runtime_root.join("skills").join(skill_name);
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        format!("# {skill_name}\n\n{description}\n"),
    )
    .unwrap();
}

pub(super) fn write_announce_token(runtime_root: &std::path::Path) {
    let credential_dir = crate::runtime_layout::credential_dir(runtime_root);
    fs::create_dir_all(&credential_dir).unwrap();
    fs::write(
        crate::runtime_layout::credential_token_path(runtime_root, "announce"),
        "announce-token\n",
    )
    .unwrap();
    // #1448 follow-up: issue announcements moved to notify-bot. Tests
    // that exercise the announcement creation path must also seed the
    // notify token, or `create_issue_announcement_pg` short-circuits
    // with `no notify bot token configured`.
    fs::write(
        crate::runtime_layout::credential_token_path(runtime_root, "notify"),
        "notify-token\n",
    )
    .unwrap();
}

#[derive(Default)]
pub(super) struct MockDiscordDispatchState {
    pub(super) calls: Vec<String>,
    pub(super) thread_parents: std::collections::HashMap<String, String>,
}

#[derive(Default)]
pub(super) struct MockIssueAnnouncementDiscordState {
    pub(super) posts: Vec<(String, String)>,
    pub(super) edits: Vec<(String, String, String)>,
}

pub(super) async fn spawn_mock_issue_announcement_discord_server() -> (
    String,
    Arc<std::sync::Mutex<MockIssueAnnouncementDiscordState>>,
    tokio::task::JoinHandle<()>,
) {
    use axum::{
        Json, Router,
        extract::{Path, State},
        response::IntoResponse,
        routing::{patch, post},
    };

    async fn create_message(
        State(state): State<Arc<std::sync::Mutex<MockIssueAnnouncementDiscordState>>>,
        Path(channel_id): Path<String>,
        Json(body): Json<serde_json::Value>,
    ) -> impl IntoResponse {
        let content = body
            .get("content")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        state
            .lock()
            .unwrap()
            .posts
            .push((channel_id.clone(), content));

        (
            StatusCode::OK,
            Json(serde_json::json!({
                "id": format!("issue-announcement-{channel_id}")
            })),
        )
    }

    async fn edit_message(
        State(state): State<Arc<std::sync::Mutex<MockIssueAnnouncementDiscordState>>>,
        Path((channel_id, message_id)): Path<(String, String)>,
        Json(body): Json<serde_json::Value>,
    ) -> impl IntoResponse {
        let content = body
            .get("content")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string();
        state
            .lock()
            .unwrap()
            .edits
            .push((channel_id.clone(), message_id.clone(), content));

        (
            StatusCode::OK,
            Json(serde_json::json!({
                "id": message_id
            })),
        )
    }

    let state = Arc::new(std::sync::Mutex::new(
        MockIssueAnnouncementDiscordState::default(),
    ));
    let app = Router::new()
        .route("/channels/{channel_id}/messages", post(create_message))
        .route(
            "/channels/{channel_id}/messages/{message_id}",
            patch(edit_message),
        )
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (format!("http://{addr}"), state, handle)
}

pub(super) async fn spawn_mock_dispatch_delivery_server() -> (
    String,
    Arc<std::sync::Mutex<MockDiscordDispatchState>>,
    tokio::task::JoinHandle<()>,
) {
    use axum::{
        Json, Router,
        extract::{Path, State},
        response::IntoResponse,
        routing::{get, post},
    };

    async fn get_channel(
        State(state): State<Arc<std::sync::Mutex<MockDiscordDispatchState>>>,
        Path(channel_id): Path<String>,
    ) -> impl IntoResponse {
        let (parent_id, total_message_sent) = {
            let mut state = state.lock().unwrap();
            state.calls.push(format!("GET /channels/{channel_id}"));
            let parent_id = state
                .thread_parents
                .get(&channel_id)
                .cloned()
                .unwrap_or_else(|| channel_id.clone());
            let total_message_sent = if channel_id.starts_with("thread-") {
                1
            } else {
                0
            };
            (parent_id, total_message_sent)
        };

        (
            StatusCode::OK,
            Json(serde_json::json!({
                "id": channel_id,
                "name": format!("mock-{channel_id}"),
                "parent_id": parent_id,
                "total_message_sent": total_message_sent,
                "thread_metadata": {
                    "archived": false,
                    "locked": false
                }
            })),
        )
    }

    async fn create_thread(
        State(state): State<Arc<std::sync::Mutex<MockDiscordDispatchState>>>,
        Path(channel_id): Path<String>,
        Json(_body): Json<serde_json::Value>,
    ) -> impl IntoResponse {
        let thread_id = format!("thread-{channel_id}");
        {
            let mut state = state.lock().unwrap();
            state
                .calls
                .push(format!("POST /channels/{channel_id}/threads"));
            state
                .thread_parents
                .insert(thread_id.clone(), channel_id.clone());
        }

        (
            StatusCode::CREATED,
            Json(serde_json::json!({
                "id": thread_id,
                "name": format!("dispatch-{channel_id}"),
                "parent_id": channel_id,
                "thread_metadata": {
                    "archived": false,
                    "locked": false
                }
            })),
        )
    }

    async fn create_message(
        State(state): State<Arc<std::sync::Mutex<MockDiscordDispatchState>>>,
        Path(channel_id): Path<String>,
        Json(_body): Json<serde_json::Value>,
    ) -> impl IntoResponse {
        {
            let mut state = state.lock().unwrap();
            state
                .calls
                .push(format!("POST /channels/{channel_id}/messages"));
        }

        (
            StatusCode::OK,
            Json(serde_json::json!({
                "id": format!("message-{channel_id}")
            })),
        )
    }

    let state = Arc::new(std::sync::Mutex::new(MockDiscordDispatchState::default()));
    let app = Router::new()
        .route("/channels/{channel_id}", get(get_channel))
        .route("/channels/{channel_id}/threads", post(create_thread))
        .route("/channels/{channel_id}/messages", post(create_message))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    (format!("http://{addr}"), state, handle)
}

pub(super) struct MockGhOverride {
    pub(super) _dir: tempfile::TempDir,
    pub(super) _env: EnvVarGuard,
}

impl MockGhOverride {
    pub(super) fn path(&self) -> &std::path::Path {
        self._dir.path()
    }
}

#[cfg(unix)]
pub(super) fn install_mock_gh_pr_tracking(
    repo: &str,
    branch: &str,
    pr_number: i64,
    head_sha: &str,
) -> MockGhOverride {
    let dir = tempfile::tempdir().unwrap();
    let gh_path = dir.path().join("gh");
    let script = format!(
        "#!/bin/sh\nset -eu\nstate_file=\"$(dirname \"$0\")/created.flag\"\nif [ \"${{1-}}\" = \"--version\" ]; then\n  echo 'gh mock 1.0'\n  exit 0\nfi\nkey=\"${{1-}}:${{2-}}\"\nargs=\"$*\"\nif [ \"$key\" = 'pr:list' ] && printf '%s\\n' \"$args\" | grep -F -q -- '--repo {repo}' && printf '%s\\n' \"$args\" | grep -F -q -- '--head {branch}'; then\n  if [ -f \"$state_file\" ]; then\n    cat <<'JSON'\n[{{\"number\":{pr_number},\"headRefName\":\"{branch}\",\"headRefOid\":\"{head_sha}\"}}]\nJSON\n  else\n    echo '[]'\n  fi\n  exit 0\nfi\nif [ \"$key\" = 'pr:create' ] && printf '%s\\n' \"$args\" | grep -F -q -- '--repo {repo}' && printf '%s\\n' \"$args\" | grep -F -q -- '--head {branch}'; then\n  : > \"$state_file\"\n  echo 'https://github.com/{repo}/pull/{pr_number}'\n  exit 0\nfi\nif [ \"$key\" = 'pr:view' ] && [ \"${{3-}}\" = '{pr_number}' ] && printf '%s\\n' \"$args\" | grep -F -q -- '--repo {repo}' && printf '%s\\n' \"$args\" | grep -F -q -- '--json headRefOid' && printf '%s\\n' \"$args\" | grep -F -q -- '--jq .headRefOid'; then\n  echo '{head_sha}'\n  exit 0\nfi\necho 'gh mock: unexpected args: $*' >&2\nexit 1\n"
    );
    fs::write(&gh_path, script).unwrap();
    let mut perms = fs::metadata(&gh_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&gh_path, perms).unwrap();
    let env = EnvVarGuard::set_path("AGENTDESK_GH_PATH", &gh_path);
    MockGhOverride {
        _dir: dir,
        _env: env,
    }
}

#[cfg(unix)]
pub(super) fn install_mock_gh_issue_view_closed(issue_number: i64, repo: &str) -> MockGhOverride {
    let dir = tempfile::tempdir().unwrap();
    let gh_path = dir.path().join("gh");
    let script = format!(
        "#!/bin/sh\nset -eu\nif [ \"${{1-}}\" = \"--version\" ]; then\n  echo 'gh mock 1.0'\n  exit 0\nfi\nif [ \"${{1-}}\" = \"issue\" ] && [ \"${{2-}}\" = \"view\" ] && [ \"${{3-}}\" = \"{issue_number}\" ]; then\n  shift 3\n  args=\"$*\"\n  if printf '%s\\n' \"$args\" | grep -F -q -- '--repo {repo}' && printf '%s\\n' \"$args\" | grep -F -q -- '--json state' && printf '%s\\n' \"$args\" | grep -F -q -- '--jq .state'; then\n    echo 'CLOSED'\n    exit 0\n  fi\nfi\necho 'gh mock: unexpected args: $*' >&2\nexit 1\n"
    );
    fs::write(&gh_path, script).unwrap();
    let mut perms = fs::metadata(&gh_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&gh_path, perms).unwrap();
    let env = EnvVarGuard::set_path("AGENTDESK_GH_PATH", &gh_path);
    MockGhOverride {
        _dir: dir,
        _env: env,
    }
}

#[cfg(unix)]
pub(super) fn install_mock_gh_issue_list(
    repo: &str,
    primary_json: &str,
    recent_closed_json: &str,
) -> MockGhOverride {
    let dir = tempfile::tempdir().unwrap();
    let gh_path = dir.path().join("gh");
    let script = format!(
        "#!/bin/sh\nset -eu\nif [ \"${{1-}}\" = \"--version\" ]; then\n  echo 'gh mock 1.0'\n  exit 0\nfi\nif [ \"${{1-}}\" = \"issue\" ] && [ \"${{2-}}\" = \"list\" ]; then\n  args=\"$*\"\n  if printf '%s\\n' \"$args\" | grep -F -q -- '--repo {repo}' && printf '%s\\n' \"$args\" | grep -F -q -- '--state all'; then\n    cat <<'JSON'\n{primary_json}\nJSON\n    exit 0\n  fi\n  if printf '%s\\n' \"$args\" | grep -F -q -- '--repo {repo}' && printf '%s\\n' \"$args\" | grep -F -q -- '--state closed'; then\n    cat <<'JSON'\n{recent_closed_json}\nJSON\n    exit 0\n  fi\nfi\necho 'gh mock: unexpected args: $*' >&2\nexit 1\n"
    );
    fs::write(&gh_path, script).unwrap();
    let mut perms = fs::metadata(&gh_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&gh_path, perms).unwrap();
    let env = EnvVarGuard::set_path("AGENTDESK_GH_PATH", &gh_path);
    MockGhOverride {
        _dir: dir,
        _env: env,
    }
}

#[cfg(unix)]
pub(super) fn install_mock_gh_issue_create(repo: &str, issue_number: i64) -> MockGhOverride {
    let dir = tempfile::tempdir().unwrap();
    let gh_path = dir.path().join("gh");
    let script = format!(
        "#!/bin/sh\nset -eu\ncapture_dir=\"$(dirname \"$0\")\"\nif [ \"${{1-}}\" = \"--version\" ]; then\n  echo 'gh mock 1.0'\n  exit 0\nfi\nif [ \"${{1-}}\" = \"issue\" ] && [ \"${{2-}}\" = \"create\" ]; then\n  printf '%s\\n' \"$@\" > \"$capture_dir/issue-create-args.txt\"\n  body_file=''\n  prev=''\n  for arg in \"$@\"; do\n    if [ \"$prev\" = '--body-file' ]; then\n      body_file=\"$arg\"\n      break\n    fi\n    prev=\"$arg\"\n  done\n  if [ -n \"$body_file\" ]; then\n    cp \"$body_file\" \"$capture_dir/issue-create-body.md\"\n  fi\n  echo 'https://github.com/{repo}/issues/{issue_number}'\n  exit 0\nfi\nif [ \"${{1-}}\" = \"issue\" ] && [ \"${{2-}}\" = \"view\" ] && [ \"${{3-}}\" = \"{issue_number}\" ]; then\n  shift 3\n  args=\"$*\"\n  if printf '%s\\n' \"$args\" | grep -F -q -- '--repo {repo}' && printf '%s\\n' \"$args\" | grep -F -q -- '--json state' && printf '%s\\n' \"$args\" | grep -F -q -- '--jq .state'; then\n    echo 'OPEN'\n    exit 0\n  fi\nfi\necho 'gh mock: unexpected args: $*' >&2\nexit 1\n"
    );
    fs::write(&gh_path, script).unwrap();
    let mut perms = fs::metadata(&gh_path).unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&gh_path, perms).unwrap();
    let env = EnvVarGuard::set_path("AGENTDESK_GH_PATH", &gh_path);
    MockGhOverride {
        _dir: dir,
        _env: env,
    }
}

#[cfg(windows)]
pub(super) fn install_mock_gh_pr_tracking(
    repo: &str,
    branch: &str,
    pr_number: i64,
    head_sha: &str,
) -> MockGhOverride {
    let dir = tempfile::tempdir().unwrap();
    let gh_path = dir.path().join("gh.ps1");
    let script = format!(
        "$stateFile = Join-Path $PSScriptRoot 'created.flag'\n$joined = $args -join ' '\nif ($args.Count -gt 0 -and $args[0] -eq '--version') {{\n  Write-Output 'gh mock 1.0'\n  exit 0\n}}\nif ($args.Count -ge 2 -and $args[0] -eq 'pr' -and $args[1] -eq 'list' -and $joined.Contains('--repo {repo}') -and $joined.Contains('--head {branch}')) {{\n  if (Test-Path $stateFile) {{\n@'\n[{{\"number\":{pr_number},\"headRefName\":\"{branch}\",\"headRefOid\":\"{head_sha}\"}}]\n'@ | Write-Output\n  }} else {{\n    '[]' | Write-Output\n  }}\n  exit 0\n}}\nif ($args.Count -ge 2 -and $args[0] -eq 'pr' -and $args[1] -eq 'create' -and $joined.Contains('--repo {repo}') -and $joined.Contains('--head {branch}')) {{\n  New-Item -ItemType File -Path $stateFile -Force | Out-Null\n  'https://github.com/{repo}/pull/{pr_number}' | Write-Output\n  exit 0\n}}\nif ($args.Count -ge 3 -and $args[0] -eq 'pr' -and $args[1] -eq 'view' -and $args[2] -eq '{pr_number}' -and $joined.Contains('--repo {repo}') -and $joined.Contains('--json headRefOid') -and $joined.Contains('--jq .headRefOid')) {{\n  '{head_sha}' | Write-Output\n  exit 0\n}}\nWrite-Error \"gh mock: unexpected args: $joined\"\nexit 1\n"
    );
    fs::write(&gh_path, script).unwrap();
    let env = EnvVarGuard::set_path("AGENTDESK_GH_PATH", &gh_path);
    MockGhOverride {
        _dir: dir,
        _env: env,
    }
}

#[cfg(windows)]
pub(super) fn install_mock_gh_issue_view_closed(issue_number: i64, repo: &str) -> MockGhOverride {
    let dir = tempfile::tempdir().unwrap();
    let gh_path = dir.path().join("gh.ps1");
    let script = format!(
        "$joined = $args -join ' '\nif ($args.Count -gt 0 -and $args[0] -eq '--version') {{\n  Write-Output 'gh mock 1.0'\n  exit 0\n}}\nif ($args.Count -ge 3 -and $args[0] -eq 'issue' -and $args[1] -eq 'view' -and $args[2] -eq '{issue_number}' -and $joined.Contains('--repo {repo}') -and $joined.Contains('--json state') -and $joined.Contains('--jq .state')) {{\n  'CLOSED' | Write-Output\n  exit 0\n}}\nWrite-Error \"gh mock: unexpected args: $joined\"\nexit 1\n"
    );
    fs::write(&gh_path, script).unwrap();
    let env = EnvVarGuard::set_path("AGENTDESK_GH_PATH", &gh_path);
    MockGhOverride {
        _dir: dir,
        _env: env,
    }
}

#[cfg(windows)]
pub(super) fn install_mock_gh_issue_list(
    repo: &str,
    primary_json: &str,
    recent_closed_json: &str,
) -> MockGhOverride {
    let dir = tempfile::tempdir().unwrap();
    let gh_path = dir.path().join("gh.ps1");
    let script = format!(
        "$joined = $args -join ' '\nif ($args.Count -gt 0 -and $args[0] -eq '--version') {{\n  Write-Output 'gh mock 1.0'\n  exit 0\n}}\nif ($args.Count -ge 2 -and $args[0] -eq 'issue' -and $args[1] -eq 'list' -and $joined.Contains('--repo {repo}')) {{\n  if ($joined.Contains('--state all')) {{\n@'\n{primary_json}\n'@ | Write-Output\n    exit 0\n  }}\n  if ($joined.Contains('--state closed')) {{\n@'\n{recent_closed_json}\n'@ | Write-Output\n    exit 0\n  }}\n}}\nWrite-Error \"gh mock: unexpected args: $joined\"\nexit 1\n"
    );
    fs::write(&gh_path, script).unwrap();
    let env = EnvVarGuard::set_path("AGENTDESK_GH_PATH", &gh_path);
    MockGhOverride {
        _dir: dir,
        _env: env,
    }
}

#[cfg(windows)]
pub(super) fn install_mock_gh_issue_create(repo: &str, issue_number: i64) -> MockGhOverride {
    let dir = tempfile::tempdir().unwrap();
    let gh_path = dir.path().join("gh.ps1");
    let script = format!(
        "$captureDir = $PSScriptRoot\n$joined = $args -join ' '\nif ($args.Count -gt 0 -and $args[0] -eq '--version') {{\n  Write-Output 'gh mock 1.0'\n  exit 0\n}}\nif ($args.Count -ge 2 -and $args[0] -eq 'issue' -and $args[1] -eq 'create') {{\n  $args | Set-Content -Path (Join-Path $captureDir 'issue-create-args.txt')\n  for ($i = 0; $i -lt $args.Count - 1; $i++) {{\n    if ($args[$i] -eq '--body-file') {{\n      Copy-Item -LiteralPath $args[$i + 1] -Destination (Join-Path $captureDir 'issue-create-body.md') -Force\n      break\n    }}\n  }}\n  'https://github.com/{repo}/issues/{issue_number}' | Write-Output\n  exit 0\n}}\nif ($args.Count -ge 3 -and $args[0] -eq 'issue' -and $args[1] -eq 'view' -and $args[2] -eq '{issue_number}' -and $joined.Contains('--repo {repo}') -and $joined.Contains('--json state') -and $joined.Contains('--jq .state')) {{\n  'OPEN' | Write-Output\n  exit 0\n}}\nWrite-Error \"gh mock: unexpected args: $joined\"\nexit 1\n"
    );
    fs::write(&gh_path, script).unwrap();
    let env = EnvVarGuard::set_path("AGENTDESK_GH_PATH", &gh_path);
    MockGhOverride {
        _dir: dir,
        _env: env,
    }
}

pub(super) fn run_git(repo_dir: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(super) fn run_git_output(repo_dir: &std::path::Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

pub(super) struct RepoDirOverride {
    pub(super) _lock: MutexGuard<'static, ()>,
    pub(super) _env: EnvVarGuard,
    pub(super) _config_dir: tempfile::TempDir,
    pub(super) _config: EnvVarGuard,
}

pub(super) fn setup_test_repo() -> (tempfile::TempDir, RepoDirOverride) {
    let lock = env_lock();
    let repo = tempfile::tempdir().unwrap();
    run_git(repo.path(), &["init", "-b", "main"]);
    run_git(repo.path(), &["config", "user.email", "test@test.com"]);
    run_git(repo.path(), &["config", "user.name", "Test"]);
    run_git(repo.path(), &["commit", "--allow-empty", "-m", "initial"]);
    let env = EnvVarGuard::set_path("AGENTDESK_REPO_DIR", repo.path());
    let config_dir = write_repo_mapping_config(&[("test-repo", repo.path())]);
    let config_path = config_dir.path().join("agentdesk.yaml");
    let config = EnvVarGuard::set_path("AGENTDESK_CONFIG", &config_path);
    (
        repo,
        RepoDirOverride {
            _lock: lock,
            _env: env,
            _config_dir: config_dir,
            _config: config,
        },
    )
}

pub(super) fn write_repo_mapping_config(entries: &[(&str, &std::path::Path)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    for (repo_id, repo_dir) in entries {
        config.github.repo_dirs.insert(
            (*repo_id).to_string(),
            repo_dir.to_string_lossy().to_string(),
        );
    }
    crate::config::save_to_path(&dir.path().join("agentdesk.yaml"), &config).unwrap();
    dir
}

pub(super) fn git_commit(repo_dir: &std::path::Path, message: &str) -> String {
    let filename = format!(
        "commit-{}.txt",
        message
            .chars()
            .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
            .collect::<String>()
    );
    std::fs::write(repo_dir.join(filename), format!("{message}\n")).unwrap();
    run_git(repo_dir, &["add", "."]);
    run_git(repo_dir, &["commit", "-m", message]);
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo_dir)
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

pub(super) async fn seed_setup_agent_for_management_test_pg(
    app: axum::Router,
    runtime_root: &std::path::Path,
    agent_id: &str,
    channel_id: &str,
) -> serde_json::Value {
    let config_path = crate::runtime_layout::config_file_path(runtime_root);
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    crate::config::save_to_path(&config_path, &crate::config::Config::default()).unwrap();
    let prompt_template = crate::runtime_layout::shared_prompt_path(runtime_root);
    fs::create_dir_all(prompt_template.parent().unwrap()).unwrap();
    fs::write(&prompt_template, "source prompt\n").unwrap();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/setup")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "agent_id": agent_id,
                        "channel_id": channel_id,
                        "provider": "codex",
                        "prompt_template_path": "config/agents/_shared.prompt.md"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

pub(super) fn seed_repo(db: &Db, repo_id: &str) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO github_repos (id, display_name) VALUES (?1, ?1)",
        [repo_id],
    )
    .unwrap();
}

pub(super) fn seed_agent(db: &Db, agent_id: &str) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES (?1, ?1, '111', '222')",
        [agent_id],
    )
    .unwrap();
}

pub(super) fn seed_card_with_status(db: &Db, card_id: &str, status: &str) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO kanban_cards (id, title, status, priority, created_at, updated_at) \
             VALUES (?1, 'test', ?2, 'medium', datetime('now'), datetime('now'))",
        sqlite_params![card_id, status],
    )
    .unwrap();
}

pub(super) async fn seed_card_with_status_pg(pool: &sqlx::PgPool, card_id: &str, status: &str) {
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at)
         VALUES ($1, 'test', $2, 'medium', NOW(), NOW())
         ON CONFLICT (id) DO UPDATE
         SET status = EXCLUDED.status,
             updated_at = NOW()",
    )
    .bind(card_id)
    .bind(status)
    .execute(pool)
    .await
    .unwrap();
}

pub(super) fn set_pmd_channel(db: &Db, channel_id: &str) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('kanban_manager_channel_id', ?1)",
        [channel_id],
    )
    .unwrap();
}

pub(super) fn ensure_auto_queue_tables(db: &Db) {
    let conn = db.lock().unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS auto_queue_runs (
            id          TEXT PRIMARY KEY,
            repo        TEXT,
            agent_id    TEXT,
            review_mode TEXT NOT NULL DEFAULT 'enabled',
            status      TEXT DEFAULT 'active',
            ai_model    TEXT,
            ai_rationale TEXT,
            timeout_minutes INTEGER DEFAULT 120,
            unified_thread  INTEGER DEFAULT 0,
            unified_thread_id TEXT,
            unified_thread_channel_id TEXT,
            created_at  DATETIME DEFAULT CURRENT_TIMESTAMP,
            completed_at DATETIME,
            max_concurrent_threads INTEGER DEFAULT 1,
            thread_group_count INTEGER DEFAULT 1
        );
        CREATE TABLE IF NOT EXISTS auto_queue_entries (
            id              TEXT PRIMARY KEY,
            run_id          TEXT REFERENCES auto_queue_runs(id),
            kanban_card_id  TEXT REFERENCES kanban_cards(id),
            agent_id        TEXT,
            priority_rank   INTEGER DEFAULT 0,
            reason          TEXT,
            status          TEXT DEFAULT 'pending',
            dispatch_id     TEXT,
            slot_index      INTEGER,
            created_at      DATETIME DEFAULT CURRENT_TIMESTAMP,
            dispatched_at   DATETIME,
            completed_at    DATETIME,
            thread_group    INTEGER DEFAULT 0,
            batch_phase     INTEGER DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS auto_queue_entry_dispatch_history (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            entry_id        TEXT NOT NULL,
            dispatch_id     TEXT NOT NULL,
            trigger_source  TEXT,
            created_at      DATETIME DEFAULT CURRENT_TIMESTAMP,
            UNIQUE(entry_id, dispatch_id)
        );
        CREATE TABLE IF NOT EXISTS auto_queue_slots (
            agent_id              TEXT NOT NULL,
            slot_index            INTEGER NOT NULL,
            assigned_run_id       TEXT,
            assigned_thread_group INTEGER,
            thread_id_map         TEXT,
            created_at            DATETIME DEFAULT CURRENT_TIMESTAMP,
            updated_at            DATETIME DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (agent_id, slot_index)
        );
        CREATE TABLE IF NOT EXISTS auto_queue_phase_gates (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id          TEXT NOT NULL REFERENCES auto_queue_runs(id) ON DELETE CASCADE,
            phase           INTEGER NOT NULL,
            status          TEXT NOT NULL DEFAULT 'pending',
            verdict         TEXT,
            dispatch_id     TEXT REFERENCES task_dispatches(id) ON DELETE CASCADE
                                CHECK(dispatch_id IS NULL OR TRIM(dispatch_id) <> ''),
            pass_verdict    TEXT NOT NULL DEFAULT 'phase_gate_passed',
            next_phase      INTEGER,
            final_phase     INTEGER NOT NULL DEFAULT 0,
            anchor_card_id  TEXT REFERENCES kanban_cards(id) ON DELETE SET NULL,
            failure_reason  TEXT,
            created_at      DATETIME DEFAULT CURRENT_TIMESTAMP,
            updated_at      DATETIME DEFAULT CURRENT_TIMESTAMP
        );
        CREATE UNIQUE INDEX IF NOT EXISTS uq_aq_phase_gates_run_phase_dispatch_key
            ON auto_queue_phase_gates(run_id, phase, COALESCE(dispatch_id, ''));
        CREATE UNIQUE INDEX IF NOT EXISTS uq_aq_phase_gates_dispatch_id
            ON auto_queue_phase_gates(dispatch_id);",
    )
    .unwrap();
}

pub(super) fn seed_auto_queue_card(
    db: &Db,
    card_id: &str,
    issue_number: i64,
    status: &str,
    agent_id: &str,
) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            github_issue_number, created_at, updated_at
        ) VALUES (
            ?1, ?2, ?3, 'medium', ?4, 'test-repo', ?5, datetime('now'), datetime('now')
        )",
        sqlite_params![
            card_id,
            format!("Issue #{issue_number}"),
            status,
            agent_id,
            issue_number
        ],
    )
    .unwrap();
}

pub(super) async fn seed_repo_pg(pool: &sqlx::PgPool, repo_id: &str) {
    sqlx::query(
        "INSERT INTO github_repos (id, display_name) VALUES ($1, $1)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(repo_id)
    .execute(pool)
    .await
    .unwrap();
}

pub(super) async fn seed_agent_pg(pool: &sqlx::PgPool, agent_id: &str) {
    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ($1, $1, '111', '222')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(agent_id)
    .execute(pool)
    .await
    .unwrap();
}

pub(super) async fn seed_auto_queue_card_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
    issue_number: i64,
    status: &str,
    agent_id: &str,
) {
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            github_issue_number, created_at, updated_at
        ) VALUES (
            $1, $2, $3, 'medium', $4, 'test-repo', $5, NOW(), NOW()
        )",
    )
    .bind(card_id)
    .bind(format!("Issue #{issue_number}"))
    .bind(status)
    .bind(agent_id)
    .bind(issue_number as i32)
    .execute(pool)
    .await
    .unwrap();
}

pub(super) async fn seed_parallel_test_cards_pg(pool: &sqlx::PgPool) -> Vec<String> {
    seed_repo_pg(pool, "test-repo").await;
    for i in 1..=4 {
        sqlx::query(
            "INSERT INTO agents (id, name, provider, status, discord_channel_id, discord_channel_alt)
             VALUES ($1, $2, 'claude', 'idle', $3, $4)",
        )
        .bind(format!("agent-{i}"))
        .bind(format!("Agent{i}"))
        .bind(format!("{}", 1000 + i))
        .bind(format!("{}", 2000 + i))
        .execute(pool)
        .await
        .unwrap();
    }
    let labels = ["A", "B", "C", "D", "E", "F", "G"];
    let issue_nums: [i32; 7] = [1, 2, 3, 4, 5, 6, 7];
    let agents = [
        "agent-1", "agent-2", "agent-3", "agent-4", "agent-4", "agent-4", "agent-4",
    ];
    let metadata: [Option<&str>; 7] = [
        None,
        None,
        None,
        None,
        Some(r#"{"depends_on":[4]}"#),
        Some(r#"{"depends_on":[5]}"#),
        Some(r#"{"depends_on":[5,6]}"#),
    ];
    let mut card_ids = Vec::new();
    for i in 0..7 {
        let card_id = format!("card-{}", labels[i]);
        sqlx::query(
            "INSERT INTO kanban_cards (id, repo_id, title, status, priority, assigned_agent_id, github_issue_number, metadata)
             VALUES ($1, 'test-repo', $2, 'ready', 'medium', $3, $4, CAST($5 AS jsonb))",
        )
        .bind(&card_id)
        .bind(format!("Task {}", labels[i]))
        .bind(agents[i])
        .bind(issue_nums[i])
        .bind(metadata[i].map(|s| s.to_string()))
        .execute(pool)
        .await
        .unwrap();
        card_ids.push(card_id);
    }
    card_ids
}

pub(super) async fn seed_similarity_group_cards_pg(pool: &sqlx::PgPool) -> Vec<String> {
    seed_repo_pg(pool, "test-repo").await;
    for i in 1..=3 {
        sqlx::query(
            "INSERT INTO agents (id, name, provider, status, discord_channel_id, discord_channel_alt)
             VALUES ($1, $2, 'claude', 'idle', $3, $4)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(format!("sim-agent-{i}"))
        .bind(format!("SimAgent{i}"))
        .bind(format!("{}", 3000 + i))
        .bind(format!("{}", 4000 + i))
        .execute(pool)
        .await
        .unwrap();
    }
    let rows = [
        (
            "sim-card-auth-1",
            "sim-agent-1",
            101_i32,
            "Auto-queue route generate update",
            "Touches src/server/routes/auto_queue.rs and dashboard/src/components/agent-manager/AutoQueuePanel.tsx",
        ),
        (
            "sim-card-auth-2",
            "sim-agent-1",
            102_i32,
            "Auto-queue panel reason rendering",
            "Updates src/server/routes/auto_queue.rs plus dashboard/src/api/client.ts for generated reason text",
        ),
        (
            "sim-card-billing-1",
            "sim-agent-2",
            201_i32,
            "Unified thread nested map cleanup",
            "Files: src/server/routes/dispatches/discord_delivery.rs and policies/auto-queue.js",
        ),
        (
            "sim-card-billing-2",
            "sim-agent-2",
            202_i32,
            "Auto queue follow-up dispatch policy",
            "Relevant files: policies/auto-queue.js and src/server/routes/routes_tests.rs",
        ),
        (
            "sim-card-ops-1",
            "sim-agent-3",
            301_i32,
            "Release health probe logs",
            "Only docs/operations/release-health.md changes are needed here",
        ),
    ];
    let mut ids = Vec::new();
    for (card_id, agent_id, issue_num, title, description) in rows {
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, repo_id, title, description, status, priority, assigned_agent_id, github_issue_number
             ) VALUES ($1, 'test-repo', $2, $3, 'ready', 'medium', $4, $5)",
        )
        .bind(card_id)
        .bind(title)
        .bind(description)
        .bind(agent_id)
        .bind(issue_num)
        .execute(pool)
        .await
        .unwrap();
        ids.push(card_id.to_string());
    }
    ids
}

pub(super) fn seed_in_progress_stall_case(
    db: &Db,
    card_id: &str,
    title: &str,
    agent_id: &str,
    started_offset: &str,
    updated_offset: &str,
    latest_dispatch: Option<(&str, &str)>,
) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            started_at, created_at, updated_at
        ) VALUES (
            ?1, ?2, 'in_progress', 'medium', ?3, 'test-repo',
            datetime('now', ?4), datetime('now', ?4), datetime('now', ?5)
        )",
        sqlite_params![card_id, title, agent_id, started_offset, updated_offset,],
    )
    .unwrap();

    if let Some((dispatch_id, dispatch_offset)) = latest_dispatch {
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                ?1, ?2, ?3, 'implementation', 'dispatched', ?4, datetime('now', ?5), datetime('now', ?5)
            )",
            sqlite_params![dispatch_id, card_id, agent_id, format!("{title} Dispatch"), dispatch_offset],
        )
        .unwrap();
        conn.execute(
            "UPDATE kanban_cards SET latest_dispatch_id = ?1 WHERE id = ?2",
            sqlite_params![dispatch_id, card_id],
        )
        .unwrap();
    }
}

pub(super) fn seed_review_e2e_case(
    db: &Db,
    card_id: &str,
    title: &str,
    agent_id: &str,
    review_offset: &str,
    dispatch_id: &str,
    dispatch_status: &str,
    dispatch_offset: &str,
) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            review_entered_at, created_at, updated_at
        ) VALUES (
            ?1, ?2, 'review', 'medium', ?3, 'test-repo',
            datetime('now', ?4), datetime('now', ?4), datetime('now', ?4)
        )",
        sqlite_params![card_id, title, agent_id, review_offset],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
        ) VALUES (
            ?1, ?2, ?3, 'e2e-test', ?4, ?5, datetime('now', ?6), datetime('now', ?6)
        )",
        sqlite_params![
            dispatch_id,
            card_id,
            agent_id,
            dispatch_status,
            format!("{title} E2E"),
            dispatch_offset
        ],
    )
    .unwrap();
    conn.execute(
        "UPDATE kanban_cards SET latest_dispatch_id = ?1 WHERE id = ?2",
        sqlite_params![dispatch_id, card_id],
    )
    .unwrap();
}

pub(super) fn drain_pending_transitions(db: &Db, engine: &PolicyEngine) {
    loop {
        let transitions = engine.drain_pending_transitions();
        if transitions.is_empty() {
            break;
        }
        for (card_id, old_s, new_s) in &transitions {
            crate::kanban::fire_transition_hooks(db, engine, card_id, old_s, new_s);
        }
    }
}

/// Helper: seed kanban cards for the parallel dispatch test scenario.
/// Creates 7 cards:
///   - 3 independent (issue #1, #2, #3)
///   - 4 in a dependency chain: #4 → #5 → #6 → #7
/// Returns card IDs in order [A, B, C, D, E, F, G].
pub(super) fn seed_parallel_test_cards(db: &Db) -> Vec<String> {
    let conn = db.lock().unwrap();
    // Create separate agents so busy-agent guard doesn't block parallel dispatch
    for i in 1..=4 {
        conn.execute(
            &format!(
                "INSERT INTO agents (id, name, provider, status, discord_channel_id, discord_channel_alt)
                 VALUES ('agent-{i}', 'Agent{i}', 'claude', 'idle', '{}', '{}')",
                1000 + i,
                2000 + i,
            ),
            [],
        )
        .unwrap();
    }

    let mut card_ids = Vec::new();
    let labels = ["A", "B", "C", "D", "E", "F", "G"];
    let issue_nums = [1, 2, 3, 4, 5, 6, 7];
    // Each independent card gets its own agent; chain cards share agent-4
    let agents = [
        "agent-1", // A: independent
        "agent-2", // B: independent
        "agent-3", // C: independent
        "agent-4", // D: chain start
        "agent-4", // E: depends on D
        "agent-4", // F: depends on E
        "agent-4", // G: depends on E and F
    ];
    // Structured dependency metadata: cards E(#5), F(#6), G(#7) reference their predecessor
    let metadata = [
        None,                            // A: independent
        None,                            // B: independent
        None,                            // C: independent
        None,                            // D: chain start
        Some(r#"{"depends_on":[4]}"#),   // E: depends on D
        Some(r#"{"depends_on":[5]}"#),   // F: depends on E
        Some(r#"{"depends_on":[5,6]}"#), // G: depends on E and F (still same component)
    ];

    for i in 0..7 {
        let card_id = format!("card-{}", labels[i]);
        let meta_val = metadata[i]
            .map(|m| format!("'{}'", m))
            .unwrap_or("NULL".to_string());
        conn.execute(
            &format!(
                "INSERT INTO kanban_cards (id, repo_id, title, status, priority, assigned_agent_id, github_issue_number, metadata)
                 VALUES ('{}', 'test-repo', 'Task {}', 'ready', 'medium', '{}', {}, {})",
                card_id, labels[i], agents[i], issue_nums[i], meta_val
            ),
            [],
        )
        .unwrap();
        card_ids.push(card_id);
    }

    card_ids
}

pub(super) fn seed_similarity_group_cards(db: &Db) -> Vec<String> {
    let conn = db.lock().unwrap();
    for i in 1..=3 {
        conn.execute(
            &format!(
                "INSERT OR IGNORE INTO agents (id, name, provider, status, discord_channel_id, discord_channel_alt)
                 VALUES ('sim-agent-{i}', 'SimAgent{i}', 'claude', 'idle', '{}', '{}')",
                3000 + i,
                4000 + i,
            ),
            [],
        )
        .unwrap();
    }

    let rows = [
        (
            "sim-card-auth-1",
            "sim-agent-1",
            101,
            "Auto-queue route generate update",
            "Touches src/server/routes/auto_queue.rs and dashboard/src/components/agent-manager/AutoQueuePanel.tsx",
        ),
        (
            "sim-card-auth-2",
            "sim-agent-1",
            102,
            "Auto-queue panel reason rendering",
            "Updates src/server/routes/auto_queue.rs plus dashboard/src/api/client.ts for generated reason text",
        ),
        (
            "sim-card-billing-1",
            "sim-agent-2",
            201,
            "Unified thread nested map cleanup",
            "Files: src/server/routes/dispatches/discord_delivery.rs and policies/auto-queue.js",
        ),
        (
            "sim-card-billing-2",
            "sim-agent-2",
            202,
            "Auto queue follow-up dispatch policy",
            "Relevant files: policies/auto-queue.js and src/server/routes/routes_tests.rs",
        ),
        (
            "sim-card-ops-1",
            "sim-agent-3",
            301,
            "Release health probe logs",
            "Only docs/operations/release-health.md changes are needed here",
        ),
    ];

    let mut ids = Vec::new();
    for (card_id, agent_id, issue_num, title, description) in rows {
        conn.execute(
            "INSERT INTO kanban_cards (
                id, repo_id, title, description, status, priority, assigned_agent_id, github_issue_number
             ) VALUES (?1, 'test-repo', ?2, ?3, 'ready', 'medium', ?4, ?5)",
            sqlite_params![card_id, title, description, agent_id, issue_num],
        )
        .unwrap();
        ids.push(card_id.to_string());
    }

    ids
}
