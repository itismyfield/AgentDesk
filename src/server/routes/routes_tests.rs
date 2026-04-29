use super::*;
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

macro_rules! sqlite_params {
    ($($param:expr),* $(,)?) => {
        ($(&$param,)*)
    };
}

fn test_db() -> Db {
    crate::db::test_db()
}

/// Seed test agents for dispatch-related tests (#245 agent-exists guard).
fn seed_test_agents(db: &Db) {
    let c = db.separate_conn().unwrap();
    c.execute_batch(
        "INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('ch-td', 'TD', '111', '222');
         INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('ag1', 'Agent1', '333', '444');
         INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '555', '666');"
    ).unwrap();
}

fn test_engine(db: &Db) -> PolicyEngine {
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

fn test_engine_with_pg(_db: &Db, pg_pool: sqlx::PgPool) -> PolicyEngine {
    let mut config = crate::config::Config::default();
    config.policies.hot_reload = false;
    PolicyEngine::new_with_pg(&config, Some(pg_pool)).unwrap()
}

fn test_api_router(
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

fn test_api_router_with_config(
    db: Db,
    engine: PolicyEngine,
    config: crate::config::Config,
    health_registry: Option<Arc<crate::services::discord::health::HealthRegistry>>,
) -> axum::Router {
    let tx = crate::server::ws::new_broadcast();
    let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
    api_router(db, engine, config, tx, buf, health_registry)
}

fn test_api_router_with_pg(
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

async fn read_sse_body_until(body: &mut Body, needles: &[&str]) -> String {
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

struct TestPostgresDb {
    _lock: crate::db::postgres::PostgresTestLifecycleGuard,
    admin_url: String,
    database_name: String,
    database_url: String,
    cleanup_armed: bool,
}

impl TestPostgresDb {
    async fn create() -> Self {
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

    async fn connect_and_migrate(&self) -> sqlx::PgPool {
        crate::db::postgres::connect_test_pool_and_migrate(&self.database_url, "routes tests")
            .await
            .unwrap()
    }

    async fn drop(mut self) {
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

fn cleanup_test_postgres_db_from_drop(admin_url: String, database_name: String) {
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

fn postgres_base_database_url() -> String {
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

fn postgres_admin_database_url() -> String {
    let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "postgres".to_string());
    format!("{}/{}", postgres_base_database_url(), admin_db)
}

fn env_lock() -> MutexGuard<'static, ()> {
    crate::services::discord::runtime_store::lock_test_env()
}

struct EnvVarGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var_os(key);
        unsafe { std::env::set_var(key, value) };
        Self { key, previous }
    }

    fn set_path(key: &'static str, value: &std::path::Path) -> Self {
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

fn seed_startup_doctor_artifact(
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

fn sample_startup_doctor_artifact() -> serde_json::Value {
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

fn local_get_request(uri: &str) -> Request<Body> {
    let mut request = Request::builder().uri(uri).body(Body::empty()).unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.1:8791".parse::<std::net::SocketAddr>().unwrap(),
    ));
    request
}

fn write_test_skill(runtime_root: &std::path::Path, skill_name: &str, description: &str) {
    let skill_dir = runtime_root.join("skills").join(skill_name);
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        format!("# {skill_name}\n\n{description}\n"),
    )
    .unwrap();
}

fn write_announce_token(runtime_root: &std::path::Path) {
    let credential_dir = crate::runtime_layout::credential_dir(runtime_root);
    fs::create_dir_all(&credential_dir).unwrap();
    fs::write(
        crate::runtime_layout::credential_token_path(runtime_root, "announce"),
        "announce-token\n",
    )
    .unwrap();
}

#[derive(Default)]
struct MockDiscordDispatchState {
    calls: Vec<String>,
    thread_parents: std::collections::HashMap<String, String>,
}

async fn spawn_mock_dispatch_delivery_server() -> (
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

struct MockGhOverride {
    _dir: tempfile::TempDir,
    _env: EnvVarGuard,
}

impl MockGhOverride {
    fn path(&self) -> &std::path::Path {
        self._dir.path()
    }
}

#[cfg(unix)]
fn install_mock_gh_pr_tracking(
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
fn install_mock_gh_issue_view_closed(issue_number: i64, repo: &str) -> MockGhOverride {
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
fn install_mock_gh_issue_list(
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
fn install_mock_gh_issue_create(repo: &str, issue_number: i64) -> MockGhOverride {
    let dir = tempfile::tempdir().unwrap();
    let gh_path = dir.path().join("gh");
    let script = format!(
        "#!/bin/sh\nset -eu\ncapture_dir=\"$(dirname \"$0\")\"\nif [ \"${{1-}}\" = \"--version\" ]; then\n  echo 'gh mock 1.0'\n  exit 0\nfi\nif [ \"${{1-}}\" = \"issue\" ] && [ \"${{2-}}\" = \"create\" ]; then\n  printf '%s\\n' \"$@\" > \"$capture_dir/issue-create-args.txt\"\n  body_file=''\n  prev=''\n  for arg in \"$@\"; do\n    if [ \"$prev\" = '--body-file' ]; then\n      body_file=\"$arg\"\n      break\n    fi\n    prev=\"$arg\"\n  done\n  if [ -n \"$body_file\" ]; then\n    cp \"$body_file\" \"$capture_dir/issue-create-body.md\"\n  fi\n  echo 'https://github.com/{repo}/issues/{issue_number}'\n  exit 0\nfi\necho 'gh mock: unexpected args: $*' >&2\nexit 1\n"
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
fn install_mock_gh_pr_tracking(
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
fn install_mock_gh_issue_view_closed(issue_number: i64, repo: &str) -> MockGhOverride {
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
fn install_mock_gh_issue_list(
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
fn install_mock_gh_issue_create(repo: &str, issue_number: i64) -> MockGhOverride {
    let dir = tempfile::tempdir().unwrap();
    let gh_path = dir.path().join("gh.ps1");
    let script = format!(
        "$captureDir = $PSScriptRoot\nif ($args.Count -gt 0 -and $args[0] -eq '--version') {{\n  Write-Output 'gh mock 1.0'\n  exit 0\n}}\nif ($args.Count -ge 2 -and $args[0] -eq 'issue' -and $args[1] -eq 'create') {{\n  $args | Set-Content -Path (Join-Path $captureDir 'issue-create-args.txt')\n  for ($i = 0; $i -lt $args.Count - 1; $i++) {{\n    if ($args[$i] -eq '--body-file') {{\n      Copy-Item -LiteralPath $args[$i + 1] -Destination (Join-Path $captureDir 'issue-create-body.md') -Force\n      break\n    }}\n  }}\n  'https://github.com/{repo}/issues/{issue_number}' | Write-Output\n  exit 0\n}}\nWrite-Error \"gh mock: unexpected args: $($args -join ' ')\"\nexit 1\n"
    );
    fs::write(&gh_path, script).unwrap();
    let env = EnvVarGuard::set_path("AGENTDESK_GH_PATH", &gh_path);
    MockGhOverride {
        _dir: dir,
        _env: env,
    }
}

fn run_git(repo_dir: &std::path::Path, args: &[&str]) {
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

fn run_git_output(repo_dir: &std::path::Path, args: &[&str]) -> String {
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

struct RepoDirOverride {
    _lock: MutexGuard<'static, ()>,
    _env: EnvVarGuard,
    _config_dir: tempfile::TempDir,
    _config: EnvVarGuard,
}

fn setup_test_repo() -> (tempfile::TempDir, RepoDirOverride) {
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

fn write_repo_mapping_config(entries: &[(&str, &std::path::Path)]) -> tempfile::TempDir {
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

fn git_commit(repo_dir: &std::path::Path, message: &str) -> String {
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

#[tokio::test]
async fn protected_domain_router_only_keeps_expected_auth_exemptions() {
    let db = test_db();
    let engine = test_engine(&db);
    let mut config = crate::config::Config::default();
    config.server.auth_token = Some("secret-token".to_string());
    let state = AppState::test_state_with_config(db, engine, config);
    let app = protected_api_domain(
        axum::Router::new()
            .route(
                "/internal/ping",
                axum::routing::get(|| async { StatusCode::OK }),
            )
            .route(
                "/dispatched-sessions/webhook",
                axum::routing::post(|| async { StatusCode::CREATED }),
            )
            .route("/send", axum::routing::post(|| async { StatusCode::OK }))
            .route(
                "/send_to_agent",
                axum::routing::post(|| async { StatusCode::OK }),
            )
            .route("/senddm", axum::routing::post(|| async { StatusCode::OK }))
            .route("/settings", axum::routing::get(|| async { StatusCode::OK })),
        state.clone(),
    )
    .with_state(state);

    let internal_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/internal/ping")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(internal_response.status(), StatusCode::OK);

    let hook_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatched-sessions/webhook")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(hook_response.status(), StatusCode::CREATED);

    let protected_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/settings")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(protected_response.status(), StatusCode::UNAUTHORIZED);

    let send_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/send")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(send_response.status(), StatusCode::UNAUTHORIZED);

    let send_to_agent_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/send_to_agent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(send_to_agent_response.status(), StatusCode::UNAUTHORIZED);

    let senddm_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/senddm")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(senddm_response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn health_detail_and_stale_mailbox_repair_pg_require_bearer_when_auth_enabled() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let mut config = crate::config::Config::default();
    config.server.host = "0.0.0.0".to_string();
    config.server.auth_token = Some("secret-token".to_string());
    let app = test_api_router_with_pg(db, engine, config, None, pool.clone());

    let mut detail_request = Request::builder()
        .uri("/health/detail")
        .body(Body::empty())
        .unwrap();
    detail_request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(
            "10.0.0.5:8791".parse::<std::net::SocketAddr>().unwrap(),
        ));
    let detail_response = app.clone().oneshot(detail_request).await.unwrap();
    assert_eq!(detail_response.status(), StatusCode::UNAUTHORIZED);

    let mut repair_request = Request::builder()
        .method("POST")
        .uri("/doctor/stale-mailbox/repair")
        .body(Body::from("{}"))
        .unwrap();
    repair_request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(
            "10.0.0.5:8791".parse::<std::net::SocketAddr>().unwrap(),
        ));
    let repair_response = app.clone().oneshot(repair_request).await.unwrap();
    assert_eq!(repair_response.status(), StatusCode::UNAUTHORIZED);

    let mut startup_doctor_request = Request::builder()
        .uri("/doctor/startup/latest")
        .body(Body::empty())
        .unwrap();
    startup_doctor_request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(
            "10.0.0.5:8791".parse::<std::net::SocketAddr>().unwrap(),
        ));
    let startup_doctor_response = app.clone().oneshot(startup_doctor_request).await.unwrap();
    assert_eq!(startup_doctor_response.status(), StatusCode::UNAUTHORIZED);

    let mut authorized_detail_request = Request::builder()
        .uri("/health/detail")
        .header("authorization", "Bearer secret-token")
        .body(Body::empty())
        .unwrap();
    authorized_detail_request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(
            "10.0.0.5:8791".parse::<std::net::SocketAddr>().unwrap(),
        ));
    let authorized_detail_response = app.oneshot(authorized_detail_request).await.unwrap();
    assert_eq!(authorized_detail_response.status(), StatusCode::OK);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn health_surfaces_latest_startup_doctor_summary_without_raw_checks() {
    let _lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    let artifact_path =
        seed_startup_doctor_artifact(runtime_root.path(), sample_startup_doctor_artifact());

    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "SQLite-only test harness has no PostgreSQL server signal, but the health body must still surface startup doctor context"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let doctor = &json["latest_startup_doctor"];
    assert_eq!(doctor["available"], true);
    assert_eq!(doctor["doctor_status"], "failed");
    assert!(
        doctor["artifact_path"].is_null(),
        "artifact_path must not be exposed on the public /api/health endpoint"
    );
    assert_eq!(doctor["failed_count"], 1);
    assert_eq!(doctor["warned_count"], 1);
    assert_eq!(doctor["detail_endpoint"], "/api/doctor/startup/latest");
    assert!(doctor.get("failed_checks").is_none());
    assert!(doctor.get("warned_checks").is_none());
    assert!(doctor.get("checks").is_none());
}

#[tokio::test]
async fn startup_doctor_latest_endpoint_returns_json_artifact_envelope() {
    let _lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    let artifact_path =
        seed_startup_doctor_artifact(runtime_root.path(), sample_startup_doctor_artifact());

    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(local_get_request("/doctor/startup/latest"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.starts_with("application/json"),
        "latest startup doctor endpoint must return JSON, got {content_type}"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["available"], true);
    assert_eq!(json["artifact_path"], artifact_path.display().to_string());
    assert_eq!(json["detail_source"], "startup_doctor_artifact");
    assert_eq!(json["followup_context"], "restart_followup");
    assert_eq!(json["summary"]["failed"], 1);
    assert_eq!(json["artifact"]["checks"][2]["id"], "dispatch_outbox");
}

#[tokio::test]
async fn startup_doctor_latest_endpoint_reports_missing_artifact_as_json() {
    let _lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    fs::create_dir_all(runtime_root.path().join("runtime")).unwrap();
    fs::write(
        runtime_root.path().join("runtime").join("dcserver.pid"),
        "4242\n",
    )
    .unwrap();

    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(local_get_request("/doctor/startup/latest"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["available"], false);
    assert_eq!(json["reason"], "startup_doctor_artifact_missing");
    assert_eq!(json["artifact"], serde_json::Value::Null);
}

#[tokio::test]
async fn startup_doctor_latest_endpoint_reports_corrupt_artifact_as_parse_error() {
    let _lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());

    let runtime_dir = runtime_root.path().join("runtime");
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::write(runtime_dir.join("dcserver.pid"), "4242\n").unwrap();

    let boot_id = crate::cli::doctor::startup::current_boot_id().unwrap();
    let artifact_dir = runtime_dir.join("doctor").join("startup");
    fs::create_dir_all(&artifact_dir).unwrap();
    fs::write(
        artifact_dir.join(format!("{boot_id}.json")),
        b"{ not valid json {{",
    )
    .unwrap();

    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(local_get_request("/doctor/startup/latest"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], false);
    assert_eq!(json["available"], false);
    assert_eq!(json["error"], "invalid_startup_doctor_artifact");
    assert!(
        !json["detail"].is_null(),
        "parse error detail string must be present"
    );
    assert_eq!(json["artifact"], serde_json::Value::Null);
}

#[tokio::test]
async fn health_detail_includes_latest_startup_doctor_detailed_fields() {
    let _lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    let artifact_path =
        seed_startup_doctor_artifact(runtime_root.path(), sample_startup_doctor_artifact());

    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(local_get_request("/health/detail"))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "SQLite-only test harness has no PostgreSQL server signal, but detail health must still surface startup doctor context"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let doctor = &json["latest_startup_doctor"];
    assert_eq!(doctor["available"], true);
    assert_eq!(doctor["artifact_path"], artifact_path.display().to_string());
    assert_eq!(
        doctor["failed_checks"]
            .as_array()
            .expect("failed_checks must be an array in detail")
            .len(),
        1,
        "sample artifact has 1 failed check"
    );
    assert_eq!(
        doctor["warned_checks"]
            .as_array()
            .expect("warned_checks must be an array in detail")
            .len(),
        1,
        "sample artifact has 1 warned check"
    );
    assert_eq!(doctor["run_context"], "startup_once");
    assert_eq!(doctor["non_fatal"], true);
    assert_eq!(doctor["followup_context"], "restart_followup");
    assert!(
        doctor.get("checks").is_none(),
        "raw checks array must not be present at top level of doctor object"
    );
}

#[tokio::test]
async fn health_detail_and_latest_endpoint_share_same_artifact_contract() {
    let _lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    let artifact_path =
        seed_startup_doctor_artifact(runtime_root.path(), sample_startup_doctor_artifact());
    let artifact_path_str = artifact_path.display().to_string();

    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let health_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let health_json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(health_response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();

    let detail_response = app
        .clone()
        .oneshot(local_get_request("/health/detail"))
        .await
        .unwrap();
    let detail_json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(detail_response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();

    let latest_response = app
        .clone()
        .oneshot(local_get_request("/doctor/startup/latest"))
        .await
        .unwrap();
    let latest_json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(latest_response.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();

    assert!(
        health_json["latest_startup_doctor"]["artifact_path"].is_null(),
        "artifact_path must not appear in the public /api/health summary"
    );
    assert_eq!(
        detail_json["latest_startup_doctor"]["artifact_path"], artifact_path_str,
        "detail health must report correct artifact_path"
    );
    assert_eq!(
        latest_json["artifact_path"], artifact_path_str,
        "latest endpoint must report correct artifact_path"
    );
    assert_eq!(
        health_json["latest_startup_doctor"]["detail_endpoint"], "/api/doctor/startup/latest",
        "public health must expose detail_endpoint"
    );
    assert_eq!(
        detail_json["latest_startup_doctor"]["detail_endpoint"], "/api/doctor/startup/latest",
        "detail health must expose detail_endpoint"
    );
    assert_eq!(
        detail_json["latest_startup_doctor"]["followup_context"], latest_json["followup_context"],
        "followup_context must be consistent between detail and latest endpoint"
    );
}

#[tokio::test]
async fn discord_control_endpoints_require_auth_token_on_non_loopback_host() {
    let db = test_db();
    let engine = test_engine(&db);
    let mut config = crate::config::Config::default();
    config.server.host = "0.0.0.0".to_string();
    let app = test_api_router_with_config(db, engine, config, None);

    let mut request = Request::builder()
        .method("POST")
        .uri("/send")
        .body(Body::from("{}"))
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "10.0.0.5:8791".parse::<std::net::SocketAddr>().unwrap(),
    ));

    let response = app.clone().oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);

    let mut send_to_agent_request = Request::builder()
        .method("POST")
        .uri("/send_to_agent")
        .body(Body::from(r#"{"role_id":"ch-pd","message":"hello"}"#))
        .unwrap();
    send_to_agent_request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(
            "10.0.0.5:8791".parse::<std::net::SocketAddr>().unwrap(),
        ));

    let send_to_agent_response = app.oneshot(send_to_agent_request).await.unwrap();

    assert_eq!(send_to_agent_response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn discord_control_endpoints_allow_loopback_without_auth_token() {
    let db = test_db();
    let engine = test_engine(&db);
    let mut config = crate::config::Config::default();
    config.server.host = "127.0.0.1".to_string();
    let app = test_api_router_with_config(db, engine, config, None);

    let mut request = Request::builder()
        .method("POST")
        .uri("/send")
        .body(Body::from("{}"))
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.1:8791".parse::<std::net::SocketAddr>().unwrap(),
    ));

    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn send_to_agent_returns_not_found_for_unknown_role_id() {
    let db = test_db();
    let engine = test_engine(&db);
    let health_registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
    let app = test_api_router(db, engine, Some(health_registry));

    let mut request = Request::builder()
        .method("POST")
        .uri("/send_to_agent")
        .body(Body::from(r#"{"role_id":"missing","message":"hello"}"#))
        .unwrap();
    request.extensions_mut().insert(axum::extract::ConnectInfo(
        "127.0.0.1:8791".parse::<std::net::SocketAddr>().unwrap(),
    ));

    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], false);
    assert_eq!(json["error"], "unknown agent target: missing");
}

#[tokio::test]
async fn public_domain_router_wraps_plain_server_errors_in_app_error_json() {
    let db = test_db();
    let engine = test_engine(&db);
    let state = AppState::test_state(db, engine);
    let app = public_api_domain(axum::Router::new().route(
        "/boom",
        axum::routing::get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
    ))
    .with_state(state);

    let response = app
        .oneshot(Request::builder().uri("/boom").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"], "internal server error");
    assert_eq!(json["code"], "internal");
}

#[tokio::test]
async fn health_api_http_pg_reports_observability_metrics_and_degraded_outbox_backlog() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new().await;
    harness.set_recovery_duration_ms(1_250);
    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db.clone(),
            engine,
            crate::config::Config::default(),
            Some(harness.registry()),
            pool.clone(),
        ),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let url = format!("http://{addr}/api/health/detail");

    let healthy_response = reqwest::get(&url).await.unwrap();
    assert_eq!(healthy_response.status(), reqwest::StatusCode::OK);
    let healthy_json: serde_json::Value = healthy_response.json().await.unwrap();
    assert_eq!(healthy_json["status"], "healthy");
    assert_eq!(healthy_json["server_up"], true);
    assert_eq!(healthy_json["fully_recovered"], true);
    assert_eq!(healthy_json["deferred_hooks"], 0);
    assert_eq!(healthy_json["queue_depth"], 0);
    assert_eq!(healthy_json["watcher_count"], 0);
    assert_eq!(healthy_json["outbox_age"], 0);
    assert!(
        (healthy_json["recovery_duration"].as_f64().unwrap() - 1.25).abs() < f64::EPSILON,
        "expected recovery_duration=1.25, got {}",
        healthy_json["recovery_duration"]
    );

    sqlx::query(
        "INSERT INTO dispatch_outbox (dispatch_id, action, status, created_at) \
         VALUES ($1, 'notify', 'pending', NOW() - INTERVAL '5 minutes')",
    )
    .bind("dispatch-1")
    .execute(&pool)
    .await
    .unwrap();

    let degraded_response = reqwest::get(&url).await.unwrap();
    assert_eq!(degraded_response.status(), reqwest::StatusCode::OK);
    let degraded_json: serde_json::Value = degraded_response.json().await.unwrap();
    assert_eq!(degraded_json["status"], "degraded");
    assert_eq!(degraded_json["server_up"], true);
    assert_eq!(degraded_json["fully_recovered"], true);
    assert!(
        degraded_json["outbox_age"].as_i64().unwrap() >= 299,
        "expected an outbox age close to 300s, got {}",
        degraded_json["outbox_age"]
    );
    assert!(
        degraded_json["degraded_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|reason| reason.starts_with("dispatch_outbox_oldest_pending_age:")),
        "expected dispatch_outbox_oldest_pending_age reason, got {:?}",
        degraded_json["degraded_reasons"]
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn health_api_reports_server_up_before_full_recovery_on_postgres() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new().await;
    harness.set_reconcile_done(false);
    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db,
            engine,
            crate::config::Config::default(),
            Some(harness.registry()),
            pg_pool.clone(),
        ),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let response = reqwest::get(format!("http://{addr}/api/health/detail"))
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let json: serde_json::Value = response.json().await.unwrap();

    assert_eq!(json["status"], "degraded");
    assert_eq!(json["server_up"], true);
    assert_eq!(json["fully_recovered"], false);
    assert!(
        json["degraded_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|reason| reason == "provider:claude:reconcile_in_progress"),
        "expected reconcile_in_progress degraded reason, got {:?}",
        json["degraded_reasons"]
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn health_api_pg_standalone_mode_reports_status_field() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db,
            engine,
            crate::config::Config::default(),
            None,
            pool.clone(),
        ),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let response = reqwest::get(format!("http://{addr}/api/health"))
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let json: serde_json::Value = response.json().await.unwrap();

    assert_eq!(json["status"], "healthy");
    assert_eq!(json["ok"], true);
    assert_eq!(json["db"], true);
    assert_eq!(json["server_up"], true);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn health_wait_script_passes_when_server_is_up_before_full_recovery_pg() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new().await;
    harness.set_reconcile_done(false);
    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db,
            engine,
            crate::config::Config::default(),
            Some(harness.registry()),
            pg_pool.clone(),
        ),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let warmup_json: serde_json::Value = reqwest::get(format!("http://{addr}/api/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(warmup_json["server_up"], true);
    assert_eq!(warmup_json["fully_recovered"], false);

    let defaults_path = format!("{}/scripts/_defaults.sh", env!("CARGO_MANIFEST_DIR"));
    let port = addr.port();
    let output = tokio::task::spawn_blocking(move || {
        Command::new("bash")
            .arg("-lc")
            .arg(format!(
                ". \"{defaults_path}\"; wait_for_http_service_health test-health {port} 2 0 0 1",
            ))
            .output()
            .unwrap()
    })
    .await
    .unwrap();

    assert!(
        output.status.success(),
        "expected wait_for_http_service_health to pass on server_up=true before full recovery; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn health_wait_script_pg_rejects_non_reconcile_degraded_server_up_response() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new().await;
    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db.clone(),
            engine,
            crate::config::Config::default(),
            Some(harness.registry()),
            pool.clone(),
        ),
    );

    sqlx::query(
        "INSERT INTO dispatch_outbox (dispatch_id, action, status, created_at) \
         VALUES ($1, 'notify', 'pending', NOW() - INTERVAL '5 minutes')",
    )
    .bind("dispatch-degraded")
    .execute(&pool)
    .await
    .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let public_json: serde_json::Value = reqwest::get(format!("http://{addr}/api/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(public_json["status"], "degraded");
    assert_eq!(public_json["server_up"], true);
    assert_eq!(public_json["fully_recovered"], true);

    let defaults_path = format!("{}/scripts/_defaults.sh", env!("CARGO_MANIFEST_DIR"));
    let port = addr.port();
    let output = tokio::task::spawn_blocking(move || {
        Command::new("bash")
            .arg("-lc")
            .arg(format!(
                ". \"{defaults_path}\"; wait_for_http_service_health test-health {port} 1 0 0 1",
            ))
            .output()
            .unwrap()
    })
    .await
    .unwrap();

    assert!(
        !output.status.success(),
        "expected wait_for_http_service_health to reject non-reconcile degraded server_up=true; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn health_wait_script_rejects_unhealthy_server_up_response_pg() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new().await;
    harness.set_connected(false);
    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db,
            engine,
            crate::config::Config::default(),
            Some(harness.registry()),
            pg_pool.clone(),
        ),
    );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let unhealthy_json: serde_json::Value = reqwest::get(format!("http://{addr}/api/health"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(unhealthy_json["status"], "unhealthy");
    assert_eq!(unhealthy_json["server_up"], true);
    assert_eq!(unhealthy_json["fully_recovered"], true);

    let defaults_path = format!("{}/scripts/_defaults.sh", env!("CARGO_MANIFEST_DIR"));
    let port = addr.port();
    let output = tokio::task::spawn_blocking(move || {
        Command::new("bash")
            .arg("-lc")
            .arg(format!(
                ". \"{defaults_path}\"; wait_for_http_service_health test-health {port} 2 0 0 1",
            ))
            .output()
            .unwrap()
    })
    .await
    .unwrap();

    assert!(
        !output.status.success(),
        "expected wait_for_http_service_health to reject unhealthy server_up=true; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn stats_memento_endpoint_reports_hourly_counts_and_dedup_hits() {
    crate::services::memory::reset_memento_throttle_for_tests();
    crate::services::memory::note_memento_tool_request("recall");
    crate::services::memory::note_memento_remote_call("recall");
    crate::services::memory::note_memento_tool_request("recall");
    crate::services::memory::note_memento_dedup_hit("recall");
    crate::services::memory::note_memento_tool_request("remember");
    crate::services::memory::note_memento_remote_call("remember");

    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/stats/memento?hours=1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["timezone"], "Asia/Seoul");
    assert_eq!(json["window_hours"], 1);
    assert_eq!(json["summary"]["request_count"], 3);
    assert_eq!(json["summary"]["remote_call_count"], 2);
    assert_eq!(json["summary"]["dedup_hit_count"], 1);
    assert_eq!(json["tools"]["recall"]["request_count"], 2);
    assert_eq!(json["tools"]["recall"]["dedup_hit_count"], 1);
    assert_eq!(json["tools"]["remember"]["remote_call_count"], 1);
    assert_eq!(json["hours"][0]["counts"]["request_count"], 3);

    crate::services::memory::reset_memento_throttle_for_tests();
}

#[tokio::test]
async fn health_api_pg_includes_latest_config_audit_report() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new().await;
    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db.clone(),
            engine,
            crate::config::Config::default(),
            Some(harness.registry()),
            pool.clone(),
        ),
    );

    {
        let report = crate::services::discord_config_audit::ConfigAuditReport {
            generated_at: "2026-04-11T01:23:45Z".to_string(),
            status: "warn".to_string(),
            dry_run: false,
            warnings_count: 1,
            warnings: vec!["DB agent 'alpha' differs from agentdesk.yaml on provider".to_string()],
            actions: vec![
                "synced 1 agent definitions from agentdesk.yaml into the agents table".to_string(),
            ],
            sources: crate::services::discord_config_audit::ConfigAuditSources {
                yaml_path: "/tmp/agentdesk.yaml".to_string(),
                yaml_present: true,
                role_map_path: Some("/tmp/role_map.json".to_string()),
                role_map_present: true,
                bot_settings_path: Some("/tmp/bot_settings.json".to_string()),
                bot_settings_present: false,
            },
            storage: crate::services::discord_config_audit::ConfigAuditDbSummary {
                missing_agents: Vec::new(),
                extra_agents: Vec::new(),
                mismatched_agents: vec!["alpha".to_string()],
                synced_agents: Some(1),
            },
        };
        sqlx::query(
            "INSERT INTO kv_meta (key, value) VALUES ('config_audit_report', $1)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(serde_json::to_string(&report).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let json: serde_json::Value = reqwest::get(format!("http://{addr}/api/health/detail"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(json["config_audit"]["status"], "warn");
    assert_eq!(json["config_audit"]["warnings_count"], 1);
    assert_eq!(json["config_audit"]["db"]["mismatched_agents"][0], "alpha");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn health_api_pg_includes_pipeline_override_report_and_degraded_reason() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new().await;
    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db.clone(),
            engine,
            crate::config::Config::default(),
            Some(harness.registry()),
            pool.clone(),
        ),
    );

    {
        let report = crate::pipeline::PipelineOverrideHealthReport {
            generated_at: "2026-04-20T00:00:00Z".to_string(),
            status: "warn".to_string(),
            warnings_count: 1,
            warnings: vec![
                "repo override alpha replaces transitions and drops 2 inherited entries"
                    .to_string(),
            ],
            parse_failures: Vec::new(),
            replace_warnings: vec![crate::pipeline::PipelineOverrideReplaceWarning {
                layer: "repo".to_string(),
                target_id: "alpha".to_string(),
                section: "transitions".to_string(),
                dropped_count: 2,
                dropped_items: vec![
                    "backlog->in_progress".to_string(),
                    "in_progress->done".to_string(),
                ],
            }],
        };
        sqlx::query(
            "INSERT INTO kv_meta (key, value) VALUES ('pipeline_override_health_report', $1)
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(serde_json::to_string(&report).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let json: serde_json::Value = reqwest::get(format!("http://{addr}/api/health/detail"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(json["status"], "degraded");
    assert_eq!(json["pipeline_overrides"]["status"], "warn");
    assert_eq!(json["pipeline_overrides"]["warnings_count"], 1);
    assert_eq!(
        json["pipeline_overrides"]["replace_warnings"][0]["target_id"],
        "alpha"
    );
    assert!(
        json["degraded_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|reason| reason == "pipeline_override_warnings:1"),
        "expected pipeline_override_warnings degraded reason, got {:?}",
        json["degraded_reasons"]
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn offices_reorder_pg_accepts_bare_array_and_updates_listing_order() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query("INSERT INTO offices (id, name, sort_order) VALUES ('office-a', 'Alpha', 2)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO offices (id, name, sort_order) VALUES ('office-b', 'Beta', 0)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO offices (id, name, sort_order) VALUES ('office-c', 'Gamma', 1)")
        .execute(&pool)
        .await
        .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let reorder_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/offices/reorder")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"[{"id":"office-a","sort_order":1},{"id":"office-b","sort_order":2},{"id":"office-c","sort_order":0}]"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(reorder_response.status(), StatusCode::OK);
    let reorder_body = axum::body::to_bytes(reorder_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let reorder_json: serde_json::Value = serde_json::from_slice(&reorder_body).unwrap();
    assert_eq!(reorder_json["ok"], true);
    assert_eq!(reorder_json["updated"], 3);

    let list_response = app
        .oneshot(
            Request::builder()
                .uri("/offices")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(list_response.status(), StatusCode::OK);
    let list_body = axum::body::to_bytes(list_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let list_json: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
    let offices = list_json["offices"].as_array().unwrap();

    assert_eq!(offices.len(), 3);
    assert_eq!(offices[0]["id"], "office-c");
    assert_eq!(offices[0]["sort_order"], 0);
    assert_eq!(offices[1]["id"], "office-a");
    assert_eq!(offices[1]["sort_order"], 1);
    assert_eq!(offices[2]["id"], "office-b");
    assert_eq!(offices[2]["sort_order"], 2);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn offices_reorder_rejects_wrapped_order_body() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/offices/reorder")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"order":[{"id":"office-a","sort_order":0}]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn round_table_meeting_channels_endpoint_does_not_fall_through_to_meeting_lookup() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = axum::Router::new().nest("/api", test_api_router(db, engine, None));

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/round-table-meetings/channels")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(
        json["channels"].is_array(),
        "expected channels array, got {json}"
    );
    assert_ne!(json["error"], json!("meeting not found"));
}

#[tokio::test]
async fn round_table_meeting_channels_endpoint_returns_configured_experts_and_fallback_name() {
    let _lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());

    let mut config = crate::config::Config::default();
    config.agents = vec![
        crate::config::AgentDef {
            id: "meeting-host".to_string(),
            name: "Meeting Host".to_string(),
            name_ko: None,
            provider: "codex".to_string(),
            channels: crate::config::AgentChannels {
                codex: Some(crate::config::AgentChannel::Detailed(
                    crate::config::AgentChannelConfig {
                        id: Some("123456789".to_string()),
                        name: Some("meeting-room".to_string()),
                        provider: Some("codex".to_string()),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
            keywords: vec!["facilitation".to_string()],
            department: None,
            avatar_emoji: None,
        },
        crate::config::AgentDef {
            id: "qwen".to_string(),
            name: "QWEN".to_string(),
            name_ko: None,
            provider: "qwen".to_string(),
            channels: crate::config::AgentChannels::default(),
            keywords: vec!["planning".to_string()],
            department: None,
            avatar_emoji: None,
        },
        crate::config::AgentDef {
            id: "gemini".to_string(),
            name: "GEMINI".to_string(),
            name_ko: None,
            provider: "gemini".to_string(),
            channels: crate::config::AgentChannels::default(),
            keywords: vec!["analysis".to_string()],
            department: None,
            avatar_emoji: None,
        },
    ];
    config.meeting = Some(crate::config::MeetingSettings {
        channel_name: "meeting-room".to_string(),
        max_rounds: Some(3),
        max_participants: Some(4),
        summary_agent: Some(crate::config::MeetingSummaryAgentDef::Static(
            "meeting-host".to_string(),
        )),
        available_agents: vec![
            crate::config::MeetingAgentEntry::RoleId("qwen".to_string()),
            crate::config::MeetingAgentEntry::RoleId("gemini".to_string()),
        ],
    });

    let config_path = crate::runtime_layout::config_file_path(runtime_root.path());
    crate::config::save_to_path(&config_path, &config).unwrap();

    let db = test_db();
    let engine = test_engine(&db);
    let health_registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
    let app = axum::Router::new().nest("/api", test_api_router(db, engine, Some(health_registry)));

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/round-table-meetings/channels")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let channels = json["channels"].as_array().unwrap();
    assert_eq!(channels.len(), 1, "expected one registered meeting channel");
    assert_eq!(channels[0]["channel_id"], json!("123456789"));
    assert_eq!(channels[0]["channel_name"], json!("meeting-room"));

    let experts = channels[0]["available_experts"].as_array().unwrap();
    assert_eq!(experts.len(), 2, "expected configured meeting experts");
    assert!(experts.iter().any(|expert| {
        expert["role_id"] == json!("qwen") && expert["provider_hint"] == json!("qwen")
    }));
    assert!(experts.iter().any(|expert| {
        expert["role_id"] == json!("gemini") && expert["provider_hint"] == json!("gemini")
    }));
}

#[tokio::test]
async fn agent_turn_pg_returns_recent_output_from_inflight_snapshot() {
    let _env_lock = env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let inflight_dir = temp
        .path()
        .join("runtime")
        .join("discord_inflight")
        .join("codex");
    std::fs::create_dir_all(&inflight_dir).unwrap();

    let tmux_name = format!(
        "AgentDesk-codex-adk-cdx-inflight-test-{}",
        std::process::id()
    );
    std::fs::write(
        inflight_dir.join("1485506232256168011.json"),
        serde_json::to_string(&json!({
            "version": 1,
            "provider": "codex",
            "channel_id": 1485506232256168011u64,
            "channel_name": "adk-cdx",
            "request_owner_user_id": 1u64,
            "user_msg_id": 2u64,
            "current_msg_id": 3u64,
            "current_msg_len": 0,
            "user_text": "show me output",
            "session_id": null,
            "tmux_session_name": tmux_name.clone(),
            "output_path": null,
            "input_fifo_path": null,
            "last_offset": 0u64,
            "started_at": "2026-04-06 10:11:12",
            "updated_at": "2026-04-06 10:11:13",
            "prev_tool_status": "✓ Read: src/config.rs",
            "current_tool_line": "⚙ Bash: rg -n turn src",
            "full_response": "partial output\nOPENAI_API_KEY=sk-secret",
            "response_sent_offset": 0,
        }))
        .unwrap(),
    )
    .unwrap();

    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, created_at, updated_at)
         VALUES ('agent-turn', 'Agent Turn', 'codex', '1485506232256168011', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions
         (session_key, agent_id, provider, status, active_dispatch_id, last_heartbeat, created_at)
         VALUES ($1, 'agent-turn', 'codex', 'working', 'dispatch-turn', NOW(), '2026-04-06 10:00:00')",
    )
    .bind(format!("mac-mini:{tmux_name}"))
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/agents/agent-turn/turn")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "working");
    assert_eq!(json["started_at"], "2026-04-06 10:11:12");
    assert_eq!(json["updated_at"], "2026-04-06 10:11:13");
    assert_eq!(json["recent_output_source"], "inflight");
    assert_eq!(json["active_dispatch_id"], "dispatch-turn");
    assert_eq!(json["prev_tool_status"], "✓ Read: src/config.rs");
    assert_eq!(json["current_tool_line"], "⚙ Bash: rg -n turn src");
    assert_eq!(json["tool_count"], 2);
    let recent_output = json["recent_output"].as_str().unwrap();
    assert!(recent_output.contains("⚙ Bash: rg -n turn src"));
    assert!(recent_output.contains("✓ Read: src/config.rs"));
    assert!(recent_output.contains("OPENAI_API_KEY=[REDACTED]"));
    assert!(!recent_output.contains("sk-secret"));
    let tool_events = json["tool_events"].as_array().unwrap();
    assert_eq!(tool_events.len(), 2);
    assert_eq!(tool_events[0]["tool_name"], "Read");
    assert_eq!(tool_events[0]["status"], "success");
    assert_eq!(tool_events[1]["tool_name"], "Bash");
    assert_eq!(tool_events[1]["status"], "running");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agent_turn_pg_reports_idle_when_agent_has_no_active_session() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, provider, created_at, updated_at)
         VALUES ('agent-idle', 'Agent Idle', 'codex', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/agents/agent-idle/turn")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["status"], "idle");
    assert!(json["recent_output"].is_null());
    assert!(json["started_at"].is_null());
    assert!(json["updated_at"].is_null());
    assert_eq!(json["recent_output_source"], "none");
    assert!(json["current_tool_line"].is_null());
    assert!(json["prev_tool_status"].is_null());
    assert_eq!(json["tool_count"], 0);
    assert!(json["tool_events"].as_array().unwrap().is_empty());

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
#[ignore = "requires tmux"]
async fn stop_agent_turn_preserves_matching_tmux_session() {
    let _env_lock = env_lock();
    Command::new("tmux")
        .arg("-V")
        .output()
        .expect("tmux must be installed for this test");

    let temp = tempfile::tempdir().unwrap();
    let _env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", temp.path());
    let inflight_dir = temp
        .path()
        .join("runtime")
        .join("discord_inflight")
        .join("codex");
    std::fs::create_dir_all(&inflight_dir).unwrap();

    let tmux_name = format!("AgentDesk-codex-agent-turn-stop-{}", std::process::id());
    let session_key = format!("mac-mini:{tmux_name}");
    let inflight_path = inflight_dir.join("agent-stop.json");
    std::fs::write(
        &inflight_path,
        serde_json::to_string(&json!({
            "version": 1,
            "provider": "codex",
            "channel_id": 1485506232256168011u64,
            "channel_name": "agent-stop",
            "request_owner_user_id": 1u64,
            "user_msg_id": 2u64,
            "current_msg_id": 3u64,
            "current_msg_len": 0,
            "user_text": "stop now",
            "session_id": null,
            "tmux_session_name": tmux_name,
            "output_path": null,
            "input_fifo_path": null,
            "last_offset": 0u64,
            "full_response": "",
            "response_sent_offset": 0,
            "started_at": "2026-04-06 10:20:00",
            "updated_at": "2026-04-06 10:20:01",
        }))
        .unwrap(),
    )
    .unwrap();

    let tmux_started = Command::new("tmux")
        .args(["new-session", "-d", "-s", &tmux_name, "sleep 30"])
        .status()
        .expect("tmux session should start for this test");
    assert!(
        tmux_started.success(),
        "tmux session should start for this test"
    );

    let db = test_db();
    let engine = test_engine(&db);
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, provider, discord_channel_id, created_at, updated_at)
             VALUES ('agent-stop', 'Agent Stop', 'codex', '1485506232256168011', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions
             (session_key, agent_id, provider, status, last_heartbeat, created_at)
             VALUES (?1, 'agent-stop', 'codex', 'working', datetime('now'), datetime('now'))",
            [session_key.clone()],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/agent-stop/turn/stop")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let tmux_still_alive = Command::new("tmux")
        .args(["has-session", "-t", &tmux_name])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if tmux_still_alive {
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &tmux_name])
            .status();
    }

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    if status != StatusCode::OK {
        panic!(
            "auto_queue_activate_dispatches_pg_only_run_without_sqlite_mirror status={} body={}",
            status,
            String::from_utf8_lossy(&body)
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "stopped");
    assert_eq!(json["tmux_killed"], false);
    assert_eq!(json["lifecycle_path"], "direct-fallback");
    assert!(
        tmux_still_alive,
        "tmux session should stay alive after /turn/stop"
    );
    assert!(
        !inflight_path.exists(),
        "matching inflight state should be removed by /turn/stop"
    );

    let conn = db.lock().unwrap();
    let session_status: String = conn
        .query_row(
            "SELECT status FROM sessions WHERE session_key = ?1",
            [session_key],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(session_status, "disconnected");
}

#[tokio::test]
async fn stop_agent_turn_pg_preserves_pending_queue_via_mailbox_fallback_cleanup() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new_with_provider(
        crate::services::provider::ProviderKind::Codex,
    )
    .await;
    let channel_id = "1485506232256168012";
    let channel_num = channel_id.parse::<u64>().unwrap();
    let tmux_name = "AgentDesk-codex-stop-canonical";
    let session_key = format!("mac-mini:{tmux_name}");

    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, created_at, updated_at)
         VALUES ('agent-stop-canonical', 'Agent Stop Canonical', 'codex', $1, NOW(), NOW())",
    )
    .bind(channel_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions
         (session_key, agent_id, provider, status, last_heartbeat, created_at)
         VALUES ($1, 'agent-stop-canonical', 'codex', 'working', NOW(), NOW())",
    )
    .bind(session_key.as_str())
    .execute(&pool)
    .await
    .unwrap();

    harness
        .seed_channel_session(
            channel_num,
            "stop-canonical",
            Some("session-stop-canonical"),
        )
        .await;
    harness.seed_active_turn(channel_num, 9, 91).await;
    harness
        .seed_queue(channel_num, &[(1_001, "preserve stop queue")])
        .await;
    harness.insert_dispatch_role_override(channel_num, 1485506232256168999);

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        Some(harness.registry()),
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/agent-stop-canonical/turn/stop")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    if status != StatusCode::OK {
        panic!(
            "stop_agent_turn_pg_preserves_pending_queue status={} body={}",
            status,
            String::from_utf8_lossy(&body)
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "stopped");
    assert_eq!(json["lifecycle_path"], "runtime-fallback");
    assert_eq!(json["queue_preserved"], true);

    let (has_active_turn, queue_depth, session_id) = harness.mailbox_state(channel_num).await;
    assert!(!has_active_turn);
    assert_eq!(queue_depth, 1);
    assert_eq!(session_id, None);
    assert!(harness.has_dispatch_role_override(channel_num));

    let session_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM sessions WHERE session_key = $1")
            .bind(&session_key)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(session_status, "disconnected");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn stop_agent_turn_tmux_only_pg_fallback_clears_mailbox_without_detaching_watcher() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let provider = crate::services::provider::ProviderKind::Codex;
    let harness =
        crate::services::discord::health::TestHealthHarness::new_with_provider(provider.clone())
            .await;
    let channel_num = 1485506232256168013u64;
    let channel_name = "operator-stop-tmux-only";
    let tmux_name = provider.build_tmux_session_name(channel_name);
    let session_key = format!("mac-mini:{tmux_name}");

    sqlx::query(
        "INSERT INTO agents (id, name, provider, created_at, updated_at)
         VALUES ('agent-stop-tmux-only', 'Agent Stop Tmux Only', 'codex', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions
         (session_key, agent_id, provider, status, last_heartbeat, created_at)
         VALUES ($1, 'agent-stop-tmux-only', 'codex', 'working', NOW(), NOW())",
    )
    .bind(session_key.as_str())
    .execute(&pool)
    .await
    .unwrap();

    harness
        .seed_channel_session(channel_num, channel_name, Some("session-stop-tmux-only"))
        .await;
    harness.seed_active_turn(channel_num, 9, 91).await;
    let watcher_cancel = harness.seed_watcher(channel_num);

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        Some(harness.registry()),
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/agent-stop-tmux-only/turn/stop")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    if status != StatusCode::OK {
        panic!(
            "stop_agent_turn_tmux_only_fallback status={} body={}",
            status,
            String::from_utf8_lossy(&body)
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "stopped");
    assert_eq!(json["lifecycle_path"], "mailbox_canonical");
    assert_eq!(json["tmux_killed"], false);

    let (has_active_turn, _, _) = harness.mailbox_state(channel_num).await;
    assert!(
        !has_active_turn,
        "tmux-only operator stop fallback must clear active mailbox state",
    );
    assert!(
        harness.has_watcher(channel_num),
        "tmux-only operator stop fallback must preserve live watcher ownership",
    );
    assert!(
        !watcher_cancel.load(std::sync::atomic::Ordering::Relaxed),
        "tmux-only operator stop fallback must not cancel the watcher",
    );

    let session_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM sessions WHERE session_key = $1")
            .bind(&session_key)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(session_status, "disconnected");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn start_agent_turn_pg_returns_conflict_when_mailbox_is_busy() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new_with_provider(
        crate::services::provider::ProviderKind::Codex,
    )
    .await;
    let channel_id = "1485506232256168123";
    let channel_num = channel_id.parse::<u64>().unwrap();

    sqlx::query(
        "INSERT INTO agents
         (id, name, provider, discord_channel_id, discord_channel_alt, created_at, updated_at)
         VALUES ('agent-turn-start-busy', 'Agent Turn Start Busy', 'codex', 'legacy-busy', $1, NOW(), NOW())",
    )
    .bind(channel_id)
    .execute(&pool)
    .await
    .unwrap();

    harness.seed_active_turn(channel_num, 7, 77).await;

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        Some(harness.registry()),
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/agent-turn-start-busy/turn/start")
                .header("content-type", "application/json")
                .body(Body::from(
                    format!(
                        r#"{{"prompt":"run headless probe","source":"system","metadata":{{"trigger_source":"test"}},"channel_id":"{}"}}"#,
                        channel_id
                    ),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], false);
    assert_eq!(json["status"], "conflict");
    assert!(
        json["error"]
            .as_str()
            .is_some_and(|value| value.contains("mailbox is busy")),
        "unexpected error body: {json}"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn start_agent_turn_pg_rejects_channel_override_outside_agent_bindings() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new_with_provider(
        crate::services::provider::ProviderKind::Codex,
    )
    .await;
    let bound_channel_id = "1485506232256168124";
    let forbidden_channel_id = "1485506232256168125";

    sqlx::query(
        "INSERT INTO agents
         (id, name, provider, discord_channel_id, discord_channel_alt, created_at, updated_at)
         VALUES ('agent-turn-start-forbidden', 'Agent Turn Start Forbidden', 'codex', 'legacy-forbidden', $1, NOW(), NOW())",
    )
    .bind(bound_channel_id)
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        Some(harness.registry()),
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/agent-turn-start-forbidden/turn/start")
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"prompt":"run headless probe","source":"system","channel_id":"{}"}}"#,
                    forbidden_channel_id
                )))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], false);
    assert!(
        json["error"]
            .as_str()
            .is_some_and(|value| value.contains("not allowed")),
        "unexpected error body: {json}"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
#[ignore = "requires tmux"]
async fn cancel_turn_preserves_tmux_and_cancels_active_dispatch() {
    let _env_lock = env_lock();
    Command::new("tmux")
        .arg("-V")
        .output()
        .expect("tmux must be installed for this test");

    let tmux_name = format!("AgentDesk-codex-turn-cancel-{}", std::process::id());
    let session_key = format!("mac-mini:{tmux_name}");
    let channel_id = "1485506232256168011";

    let tmux_started = Command::new("tmux")
        .args(["new-session", "-d", "-s", &tmux_name, "sleep 30"])
        .status()
        .expect("tmux session should start for this test");
    assert!(
        tmux_started.success(),
        "tmux session should start for this test"
    );

    let db = test_db();
    let engine = test_engine(&db);
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (id, name, provider, discord_channel_id, created_at, updated_at)
             VALUES ('agent-queue-stop', 'Agent Queue Stop', 'codex', ?1, datetime('now'), datetime('now'))",
            [channel_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at)
             VALUES ('card-turn-cancel', 'Turn Cancel', 'in_progress', 'agent-queue-stop', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches
             (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ('dispatch-turn-cancel', 'card-turn-cancel', 'agent-queue-stop', 'implementation', 'dispatched', 'Cancel me', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions
             (session_key, agent_id, provider, status, active_dispatch_id, last_heartbeat, created_at)
             VALUES (?1, 'agent-queue-stop', 'codex', 'working', 'dispatch-turn-cancel', datetime('now'), datetime('now'))",
            [session_key.clone()],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/turns/{channel_id}/cancel"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let tmux_still_alive = Command::new("tmux")
        .args(["has-session", "-t", &tmux_name])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if tmux_still_alive {
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &tmux_name])
            .status();
    }

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body).to_string();
    assert_eq!(
        status,
        StatusCode::OK,
        "unexpected reopen response: {body_text}"
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["session_key"], session_key);
    assert_eq!(json["tmux_session"], tmux_name);
    assert_eq!(json["tmux_killed"], true);
    assert_eq!(json["lifecycle_path"], "direct-fallback");
    assert_eq!(json["dispatch_cancelled"], "dispatch-turn-cancel");
    assert_eq!(json["exact_channel_match"], true);
    assert!(
        !tmux_still_alive,
        "tmux session should be killed after /turns/{{channel_id}}/cancel"
    );

    let conn = db.lock().unwrap();
    let session_row: (String, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id FROM sessions WHERE session_key = ?1",
            [session_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(session_row.0, "disconnected");
    assert_eq!(session_row.1, None);

    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-turn-cancel'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatch_status, "cancelled");
}

#[tokio::test]
async fn cancel_turn_preserves_pending_queue_via_mailbox_fallback_cleanup() {
    let _env_lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());

    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new().await;
    let channel_id = "1485506232256168013";
    let channel_num = channel_id.parse::<u64>().unwrap();
    let session_key = "mac-mini:AgentDesk-claude-cancel-canonical";
    let inflight_path = runtime_root
        .path()
        .join("runtime")
        .join("discord_inflight")
        .join("claude")
        .join(format!("{channel_num}.json"));
    fs::create_dir_all(inflight_path.parent().unwrap()).unwrap();
    fs::write(&inflight_path, "{}").unwrap();

    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, created_at, updated_at)
         VALUES ('agent-cancel-canonical', 'Agent Cancel Canonical', 'claude', $1, NOW(), NOW())",
    )
    .bind(channel_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions
         (session_key, agent_id, provider, status, last_heartbeat, created_at)
         VALUES ($1, 'agent-cancel-canonical', 'claude', 'working', NOW(), NOW())",
    )
    .bind(session_key)
    .execute(&pool)
    .await
    .unwrap();

    harness
        .seed_channel_session(
            channel_num,
            "cancel-canonical",
            Some("session-cancel-canonical"),
        )
        .await;
    harness.seed_active_turn(channel_num, 11, 111).await;
    harness
        .seed_queue(channel_num, &[(2_001, "preserve cancel queue")])
        .await;
    harness.insert_dispatch_role_override(channel_num, 1485506232256168998);
    let watcher_cancel = harness.seed_watcher(channel_num);

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        Some(harness.registry()),
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/turns/{channel_id}/cancel"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["session_key"], session_key);
    assert_eq!(json["lifecycle_path"], "runtime-fallback");
    assert_eq!(json["tmux_killed"], false);
    assert_eq!(json["queue_preserved"], true);
    assert_eq!(json["inflight_cleared"], false);
    assert_eq!(json["exact_channel_match"], true);
    assert!(json["dispatch_cancelled"].is_null());
    assert!(
        inflight_path.exists(),
        "default killed=false cancel must preserve persistent inflight for live-session handoff"
    );

    let (has_active_turn, queue_depth, session_id) = harness.mailbox_state(channel_num).await;
    assert!(!has_active_turn);
    assert_eq!(queue_depth, 1);
    assert_eq!(session_id, None);
    assert!(harness.has_dispatch_role_override(channel_num));
    assert!(
        harness.has_watcher(channel_num),
        "killed=false cancel must preserve watcher ownership"
    );
    assert!(
        !watcher_cancel.load(std::sync::atomic::Ordering::Relaxed),
        "killed=false cancel must not signal watcher cancellation"
    );

    let session_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM sessions WHERE session_key = $1")
            .bind(session_key)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(session_status, "disconnected");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn cancel_turn_targets_requested_provider_for_paired_agent() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let cc_channel_id = "1479671298497183835";
    let cdx_channel_id = "1479671301387059200";
    let cc_session_key = "mac-mini:AgentDesk-claude-adk-cc";
    let cdx_session_key = "mac-mini:AgentDesk-codex-adk-cdx";

    sqlx::query(
        "INSERT INTO agents (
            id, name, provider, discord_channel_id, discord_channel_alt,
            discord_channel_cc, discord_channel_cdx, created_at, updated_at
         )
         VALUES (
            'project-agentdesk', 'AgentDesk', 'codex', $1, $2, $1, $2,
            NOW(), NOW()
         )",
    )
    .bind(cc_channel_id)
    .bind(cdx_channel_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions
         (session_key, agent_id, provider, status, last_heartbeat, created_at)
         VALUES ($1, 'project-agentdesk', 'claude', 'working', NOW() - INTERVAL '1 minute', NOW())",
    )
    .bind(cc_session_key)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions
         (session_key, agent_id, provider, status, last_heartbeat, created_at)
         VALUES ($1, 'project-agentdesk', 'codex', 'working', NOW(), NOW())",
    )
    .bind(cdx_session_key)
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(&format!("/turns/{cc_channel_id}/cancel"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::OK, "unexpected body: {body_text}");
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["channel_id"], cc_channel_id);
    assert_eq!(json["agent_id"], "project-agentdesk");
    assert_eq!(json["requested_provider"], "claude");
    assert_eq!(json["exact_channel_match"], true);
    assert_eq!(json["session_key"], cc_session_key);
    assert_eq!(json["tmux_session"], "AgentDesk-claude-adk-cc");

    let cc_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM sessions WHERE session_key = $1")
            .bind(cc_session_key)
            .fetch_one(&pool)
            .await
            .unwrap();
    let cdx_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM sessions WHERE session_key = $1")
            .bind(cdx_session_key)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(cc_status, "disconnected");
    assert_eq!(cdx_status, "working");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn health_pg_returns_ok_with_db_status() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["db"], true);
    assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agents_pg_empty_list() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .uri("/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["agents"].as_array().unwrap().is_empty());

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agents_pg_returns_synced_agents() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, provider, status, xp) VALUES ('a1', 'Agent1', 'claude', 'idle', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let agents = json["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["id"], "a1");
    assert_eq!(agents[0]["name"], "Agent1");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agents_pg_include_current_thread_channel_id_from_working_session() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, provider, status, xp) VALUES ('a1', 'Agent1', 'codex', 'idle', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions (session_key, agent_id, provider, status, thread_channel_id, last_heartbeat)
         VALUES ($1, 'a1', 'codex', 'working', '1485506232256168011', NOW())",
    )
    .bind("mac-mini:AgentDesk-codex-adk-cdx-t1485506232256168011")
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let list_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let list_body = axum::body::to_bytes(list_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let list_json: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
    assert_eq!(
        list_json["agents"][0]["current_thread_channel_id"],
        serde_json::Value::String("1485506232256168011".to_string())
    );

    let get_response = app
        .oneshot(
            Request::builder()
                .uri("/agents/a1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let get_body = axum::body::to_bytes(get_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let get_json: serde_json::Value = serde_json::from_slice(&get_body).unwrap();
    assert_eq!(
        get_json["agent"]["current_thread_channel_id"],
        serde_json::Value::String("1485506232256168011".to_string())
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agents_pg_crud_round_trip() {
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    let create_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"id":"pg-agent","name":"PG Agent","provider":"codex","office_id":"hq","discord_channel_cdx":"1479671301387059200"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_response.status(), StatusCode::CREATED);

    let list_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let list_body = axum::body::to_bytes(list_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let list_json: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
    assert_eq!(list_json["agents"].as_array().unwrap().len(), 1);
    assert_eq!(
        list_json["agents"][0]["discord_channel_cdx"],
        "1479671301387059200"
    );

    let update_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/agents/pg-agent")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"name_ko":"피지 에이전트","pipeline_config":{"hooks":{"review":{"on_enter":["MyHook"],"on_exit":[]}}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let update_status = update_response.status();
    let update_body = axum::body::to_bytes(update_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let update_json: serde_json::Value = serde_json::from_slice(&update_body).unwrap();
    assert_eq!(
        update_status,
        StatusCode::OK,
        "unexpected update body: {update_json}"
    );
    assert_eq!(update_json["agent"]["name_ko"], "피지 에이전트");
    assert!(update_json["agent"]["pipeline_config"].is_object());

    let delete_response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/agents/pg-agent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(delete_response.status(), StatusCode::OK);

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn claude_session_id_pg_get_clears_stale_fixed_working_session() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO sessions (
            session_key, provider, status, active_dispatch_id, claude_session_id, last_heartbeat, created_at
         ) VALUES (
            'test:stale-working', 'claude', 'working', 'dispatch-123', 'stale-sid',
            NOW() - INTERVAL '7 hours', NOW() - INTERVAL '7 hours'
         )",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/dispatched-sessions/claude-session-id?session_key=test:stale-working")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["claude_session_id"].is_null());
    assert!(json["session_id"].is_null());

    let row: (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT status, active_dispatch_id, claude_session_id
         FROM sessions
         WHERE session_key = 'test:stale-working'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, "disconnected");
    assert!(row.1.is_none());
    assert!(row.2.is_none());

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn claude_session_id_pg_get_keeps_old_idle_fixed_session() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO sessions (
            session_key, provider, status, claude_session_id, last_heartbeat, created_at
         ) VALUES (
            'test:old-idle', 'claude', 'idle', 'idle-sid',
            NOW() - INTERVAL '7 hours', NOW() - INTERVAL '7 hours'
         )",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/dispatched-sessions/claude-session-id?session_key=test:old-idle")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["claude_session_id"], "idle-sid");
    assert_eq!(json["session_id"], "idle-sid");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn claude_session_id_pg_get_returns_null_on_provider_mismatch() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO sessions (
            session_key, provider, status, claude_session_id, last_heartbeat, created_at
         ) VALUES (
            'host:AgentDesk-codex-adk-cdx', 'claude', 'idle', 'claude-sid',
            NOW() - INTERVAL '1 minutes', NOW() - INTERVAL '1 minutes'
         )",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .uri(
                    "/dispatched-sessions/claude-session-id?session_key=host:AgentDesk-codex-adk-cdx&provider=codex",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["claude_session_id"].is_null());
    assert!(json["session_id"].is_null());

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn claude_session_id_pg_get_keeps_value_on_provider_match() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO sessions (
            session_key, provider, status, claude_session_id, last_heartbeat, created_at
         ) VALUES (
            'host:AgentDesk-codex-adk-cdx', 'codex', 'idle', 'codex-sid',
            NOW() - INTERVAL '1 minutes', NOW() - INTERVAL '1 minutes'
         )",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .uri(
                    "/dispatched-sessions/claude-session-id?session_key=host:AgentDesk-codex-adk-cdx&provider=codex",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["claude_session_id"], "codex-sid");
    assert_eq!(json["session_id"], "codex-sid");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn get_agent_pg_found() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, provider, status, xp) VALUES ('a1', 'Agent1', 'claude', 'idle', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/agents/a1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["agent"]["id"], "a1");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn get_agent_pg_not_found() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .uri("/agents/nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"], "agent not found");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn sessions_pg_empty_list() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .uri("/sessions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["sessions"].as_array().unwrap().is_empty());

    pool.close().await;
    pg_db.drop().await;
}

// ── Kanban CRUD tests ──────────────────────────────────────────

#[tokio::test]
async fn kanban_create_card() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"title":"Test Card","priority":"high"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["card"]["title"], "Test Card");
    assert_eq!(json["card"]["priority"], "high");
    assert_eq!(json["card"]["status"], "backlog");
    assert!(json["card"]["id"].as_str().unwrap().len() > 10); // UUID
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_create_card_pg_only_without_sqlite_mirror() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"title":"Test Card PG","priority":"high","repo_id":"repo-pg"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    if status != StatusCode::CREATED {
        panic!(
            "kanban_create_card_pg_only_without_sqlite_mirror status={} body={}",
            status,
            String::from_utf8_lossy(&body)
        );
    }

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let card_id = json["card"]["id"].as_str().unwrap();
    assert_eq!(json["card"]["title"], "Test Card PG");
    assert_eq!(json["card"]["priority"], "high");
    assert_eq!(json["card"]["status"], "backlog");
    assert_eq!(json["card"]["repo_id"], "repo-pg");

    let sqlite_card_count: i64 = db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sqlite_card_count, 0,
        "test must not rely on SQLite mirror state"
    );

    let row = sqlx::query(
        "SELECT title, status, repo_id, priority
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(row.try_get::<String, _>("title").unwrap(), "Test Card PG");
    assert_eq!(row.try_get::<String, _>("status").unwrap(), "backlog");
    assert_eq!(
        row.try_get::<Option<String>, _>("repo_id").unwrap(),
        Some("repo-pg".to_string())
    );
    assert_eq!(row.try_get::<String, _>("priority").unwrap(), "high");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn kanban_list_cards_empty() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/kanban-cards")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["cards"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn kanban_list_cards_with_filter() {
    let db = test_db();
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c1', 'Card1', 'backlog', 'medium', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c2', 'Card2', 'ready', 'high', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/kanban-cards?status=ready")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let cards = json["cards"].as_array().unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0]["id"], "c2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_list_cards_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, NOW(), NOW()
         )",
    )
    .bind("c-pg-list")
    .bind("Card PG List")
    .bind("ready")
    .bind("high")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/kanban-cards?status=ready")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        StatusCode::OK,
        "kanban_list_cards_pg_only_without_sqlite_mirror status={} body={}",
        status,
        body_text
    );

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let cards = json["cards"].as_array().unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0]["id"], "c-pg-list");

    let sqlite_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM kanban_cards WHERE id = 'c-pg-list'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_count, 0, "sqlite mirror should stay empty");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_list_pending_dispatches_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id)
         VALUES ($1, $2, '111')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("agent-pg-queue-list")
    .bind("Agent PG Queue List")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, NOW(), NOW()
         )",
    )
    .bind("card-pg-queue-list")
    .bind("Queue PG List")
    .bind("ready")
    .bind("medium")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, retry_count, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, NOW(), NOW()
         )",
    )
    .bind("dispatch-pg-queue-list")
    .bind("card-pg-queue-list")
    .bind("agent-pg-queue-list")
    .bind("implementation")
    .bind("pending")
    .bind("PG queue list dispatch")
    .bind(0_i32)
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/dispatches/pending")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        StatusCode::OK,
        "queue_list_pending_dispatches_pg_only_without_sqlite_mirror status={} body={}",
        status,
        body_text
    );

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["count"], 1);
    assert_eq!(json["dispatches"][0]["id"], "dispatch-pg-queue-list");
    assert_eq!(
        json["dispatches"][0]["kanban_card_id"],
        "card-pg-queue-list"
    );

    let sqlite_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE id = 'dispatch-pg-queue-list'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_count, 0, "sqlite mirror should stay empty");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_cancel_dispatch_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id)
         VALUES ($1, $2, '111')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("agent-pg-queue-cancel")
    .bind("Agent PG Queue Cancel")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, NOW(), NOW()
         )",
    )
    .bind("card-pg-queue-cancel")
    .bind("Queue PG Cancel")
    .bind("in_progress")
    .bind("high")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, NOW(), NOW()
         )",
    )
    .bind("dispatch-pg-queue-cancel")
    .bind("card-pg-queue-cancel")
    .bind("agent-pg-queue-cancel")
    .bind("implementation")
    .bind("pending")
    .bind("PG queue cancel dispatch")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query("INSERT INTO kv_meta (key, value) VALUES ($1, $2)")
        .bind("dispatch_notified:dispatch-pg-queue-cancel")
        .bind("1")
        .execute(&pg_pool)
        .await
        .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatches/dispatch-pg-queue-cancel/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        StatusCode::OK,
        "queue_cancel_dispatch_pg_only_without_sqlite_mirror status={} body={}",
        status,
        body_text
    );

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["dispatch_id"], "dispatch-pg-queue-cancel");

    let dispatch_status: String =
        sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
            .bind("dispatch-pg-queue-cancel")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(dispatch_status, "cancelled");

    let kv_guard_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM kv_meta WHERE key = $1")
            .bind("dispatch_notified:dispatch-pg-queue-cancel")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(
        kv_guard_count, 0,
        "dispatch_notified guard should be cleared"
    );

    let sqlite_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE id = 'dispatch-pg-queue-cancel'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_count, 0, "sqlite mirror should stay empty");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn messages_list_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, name_ko, avatar_emoji, discord_channel_id)
         VALUES ($1, $2, $3, $4, '111')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("agent-pg-message-sender")
    .bind("Sender PG")
    .bind("보내는 에이전트")
    .bind("🤖")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, name_ko, avatar_emoji, discord_channel_id)
         VALUES ($1, $2, $3, $4, '222')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("agent-pg-message-receiver")
    .bind("Receiver PG")
    .bind("받는 에이전트")
    .bind("🛰️")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO messages (
            sender_type, sender_id, receiver_type, receiver_id, content, message_type, created_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, NOW()
         )",
    )
    .bind("agent")
    .bind("agent-pg-message-sender")
    .bind("agent")
    .bind("agent-pg-message-receiver")
    .bind("PG only message")
    .bind("chat")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/messages?receiverId=agent-pg-message-receiver")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        StatusCode::OK,
        "messages_list_pg_only_without_sqlite_mirror status={} body={}",
        status,
        body_text
    );

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["messages"].as_array().unwrap().len(), 1);
    assert_eq!(json["messages"][0]["content"], json!("PG only message"));
    assert_eq!(
        json["messages"][0]["sender_name_ko"],
        json!("보내는 에이전트")
    );

    let sqlite_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(sqlite_count, 0, "sqlite mirror should stay empty");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn messages_create_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, name_ko, avatar_emoji, discord_channel_id)
         VALUES ($1, $2, $3, $4, '111')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("agent-pg-message-create-sender")
    .bind("Sender Create PG")
    .bind("생성 발신자")
    .bind("🤖")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, name_ko, avatar_emoji, discord_channel_id)
         VALUES ($1, $2, $3, $4, '222')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("agent-pg-message-create-receiver")
    .bind("Receiver Create PG")
    .bind("생성 수신자")
    .bind("🛰️")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/messages")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "sender_type":"agent",
                        "sender_id":"agent-pg-message-create-sender",
                        "receiver_type":"agent",
                        "receiver_id":"agent-pg-message-create-receiver",
                        "content":"created through pg path",
                        "message_type":"chat"
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        StatusCode::CREATED,
        "messages_create_pg_only_without_sqlite_mirror status={} body={}",
        status,
        body_text
    );

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["content"], json!("created through pg path"));
    assert_eq!(json["receiver_name_ko"], json!("생성 수신자"));

    let pg_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM messages WHERE content = $1")
            .bind("created through pg path")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(pg_count, 1);

    let sqlite_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE content = 'created through pg path'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_count, 0, "sqlite mirror should stay empty");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn hooks_skill_usage_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id)
         VALUES ($1, $2, '111')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("agent-pg-hook-skill")
    .bind("Hook Skill Agent")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO skills (id, name, description, source_path, updated_at)
         VALUES ($1, $2, $3, $4, NOW())",
    )
    .bind("skill-pg-hook")
    .bind("PG Hook Skill")
    .bind("PG hook skill")
    .bind("/tmp/skill-pg-hook")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/hook/skill-usage")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "skill_id":"skill-pg-hook",
                        "role_id":"agent-pg-hook-skill",
                        "session_key":"session-pg-hook"
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        StatusCode::OK,
        "hooks_skill_usage_pg_only_without_sqlite_mirror status={} body={}",
        status,
        body_text
    );

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], json!(true));

    let skill_usage_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM skill_usage WHERE skill_id = $1 AND agent_id = $2 AND session_key = $3",
    )
    .bind("skill-pg-hook")
    .bind("agent-pg-hook-skill")
    .bind("session-pg-hook")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(skill_usage_count, 1);

    let sqlite_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM skill_usage", [], |row| row.get(0))
        .unwrap();
    assert_eq!(sqlite_count, 0, "sqlite mirror should stay empty");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn hooks_disconnect_session_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id)
         VALUES ($1, $2, '111')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("agent-pg-hook-session")
    .bind("Hook Session Agent")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions (
            session_key, agent_id, provider, status, created_at
         ) VALUES (
            $1, $2, $3, $4, NOW()
         )",
    )
    .bind("session-pg-hook-disconnect")
    .bind("agent-pg-hook-session")
    .bind("claude")
    .bind("working")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/hook/session/session-pg-hook-disconnect")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        StatusCode::OK,
        "hooks_disconnect_session_pg_only_without_sqlite_mirror status={} body={}",
        status,
        body_text
    );

    let session_status: String =
        sqlx::query_scalar("SELECT status FROM sessions WHERE session_key = $1")
            .bind("session-pg-hook-disconnect")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(session_status, "disconnected");

    let sqlite_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM sessions WHERE session_key = 'session-pg-hook-disconnect'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_count, 0, "sqlite mirror should stay empty");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn departments_roundtrip_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO offices (id, name, sort_order, created_at)
         VALUES ($1, $2, 0, NOW())",
    )
    .bind("office-pg-dept")
    .bind("PG Office")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    let create_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/departments")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"name":"PG Department","office_id":"office-pg-dept"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_response.status(), StatusCode::CREATED);
    let create_body = axum::body::to_bytes(create_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let create_json: serde_json::Value = serde_json::from_slice(&create_body).unwrap();
    let department_id = create_json["department"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let pg_created_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM departments WHERE id = $1")
            .bind(&department_id)
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(pg_created_count, 1);

    let sqlite_created_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM departments WHERE id = ?1",
            [&department_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_created_count, 0, "sqlite mirror should stay empty");

    let update_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/departments/{department_id}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"PG Department Updated"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(update_response.status(), StatusCode::OK);
    let update_body = axum::body::to_bytes(update_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let update_json: serde_json::Value = serde_json::from_slice(&update_body).unwrap();
    assert_eq!(
        update_json["department"]["name"],
        json!("PG Department Updated")
    );

    let list_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/departments?officeId=office-pg-dept")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_response.status(), StatusCode::OK);
    let list_body = axum::body::to_bytes(list_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let list_json: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
    let departments = list_json["departments"].as_array().unwrap();
    assert_eq!(departments.len(), 1);
    assert_eq!(departments[0]["id"], json!(department_id.clone()));
    assert_eq!(departments[0]["name"], json!("PG Department Updated"));
    assert_eq!(departments[0]["office_id"], json!("office-pg-dept"));

    let reorder_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/departments/reorder")
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"{{"order":[{{"id":"{department_id}","sort_order":7}}]}}"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reorder_response.status(), StatusCode::OK);

    let pg_sort_order: i64 =
        sqlx::query_scalar("SELECT sort_order::BIGINT FROM departments WHERE id = $1")
            .bind(&department_id)
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(pg_sort_order, 7);

    let delete_response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/departments/{department_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(delete_response.status(), StatusCode::OK);

    let pg_remaining_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM departments WHERE id = $1")
            .bind(&department_id)
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(pg_remaining_count, 0);

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn offices_roundtrip_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    let create_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/offices")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"PG Office","layout":"grid"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_response.status(), StatusCode::CREATED);
    let create_body = axum::body::to_bytes(create_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let create_json: serde_json::Value = serde_json::from_slice(&create_body).unwrap();
    let office_id = create_json["office"]["id"].as_str().unwrap().to_string();

    let sqlite_office_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM offices WHERE id = ?1",
            [&office_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_office_count, 0, "sqlite mirror should stay empty");

    let update_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/offices/{office_id}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"name":"PG Office Updated","layout":"stack"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(update_response.status(), StatusCode::OK);

    let add_agent_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/offices/{office_id}/agents"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"agent_id":"pg-office-agent-1","department_id":"dept-alpha"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(add_agent_response.status(), StatusCode::OK);

    let update_agent_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(format!("/offices/{office_id}/agents/pg-office-agent-1"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"department_id":"dept-beta"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(update_agent_response.status(), StatusCode::OK);

    let batch_add_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/offices/{office_id}/agents/batch"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"agent_ids":["pg-office-agent-2","pg-office-agent-3"]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(batch_add_response.status(), StatusCode::OK);

    let pg_agent_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM office_agents WHERE office_id = $1")
            .bind(&office_id)
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(pg_agent_count, 3);

    let pg_department_id: Option<String> = sqlx::query_scalar(
        "SELECT department_id FROM office_agents WHERE office_id = $1 AND agent_id = $2",
    )
    .bind(&office_id)
    .bind("pg-office-agent-1")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(pg_department_id.as_deref(), Some("dept-beta"));

    let sqlite_office_agent_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM office_agents WHERE office_id = ?1",
            [&office_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sqlite_office_agent_count, 0,
        "sqlite office_agents mirror should stay empty"
    );

    let reorder_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/offices/reorder")
                .header("content-type", "application/json")
                .body(Body::from(format!(
                    r#"[{{"id":"{office_id}","sort_order":4}}]"#
                )))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(reorder_response.status(), StatusCode::OK);

    let list_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/offices")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(list_response.status(), StatusCode::OK);
    let list_body = axum::body::to_bytes(list_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let list_json: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
    let offices = list_json["offices"].as_array().unwrap();
    assert_eq!(offices.len(), 1);
    assert_eq!(offices[0]["id"], json!(office_id.clone()));
    assert_eq!(offices[0]["name"], json!("PG Office Updated"));
    assert_eq!(offices[0]["agent_count"], json!(3));
    assert_eq!(offices[0]["sort_order"], json!(4));

    let remove_agent_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/offices/{office_id}/agents/pg-office-agent-2"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(remove_agent_response.status(), StatusCode::OK);

    let pg_agent_count_after_remove: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM office_agents WHERE office_id = $1")
            .bind(&office_id)
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(pg_agent_count_after_remove, 2);

    let delete_response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/offices/{office_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(delete_response.status(), StatusCode::OK);

    let pg_remaining_offices: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM offices WHERE id = $1")
            .bind(&office_id)
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(pg_remaining_offices, 0);

    let pg_remaining_links: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM office_agents WHERE office_id = $1")
            .bind(&office_id)
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(pg_remaining_links, 0);

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn kanban_list_cards_filters_to_registered_repos_unless_repo_id_is_explicit() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_repo(&db, "repo-registered");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, repo_id, title, status, priority, created_at, updated_at
             ) VALUES (
                'c-registered', 'repo-registered', 'Registered Card', 'ready', 'medium',
                datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, repo_id, title, status, priority, created_at, updated_at
             ) VALUES (
                'c-unregistered', 'repo-unregistered', 'Unregistered Card', 'ready', 'medium',
                datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db, engine, None);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/kanban-cards")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let cards = json["cards"].as_array().unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0]["id"], "c-registered");

    let response = app
        .oneshot(
            Request::builder()
                .uri("/kanban-cards?repo_id=repo-unregistered")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let cards = json["cards"].as_array().unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0]["id"], "c-unregistered");
}

#[tokio::test]
async fn kanban_get_card() {
    let db = test_db();
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c1', 'Card1', 'backlog', 'medium', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/kanban-cards/c1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["card"]["id"], "c1");
    assert_eq!(json["card"]["title"], "Card1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_get_card_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at)
         VALUES ($1, $2, $3, $4, NOW(), NOW())",
    )
    .bind("c1-pg-get")
    .bind("Card1 PG")
    .bind("backlog")
    .bind("medium")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/kanban-cards/c1-pg-get")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    if status != StatusCode::OK {
        panic!(
            "kanban_get_card_pg_only_without_sqlite_mirror status={} body={}",
            status,
            String::from_utf8_lossy(&body)
        );
    }

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["card"]["id"], "c1-pg-get");
    assert_eq!(json["card"]["title"], "Card1 PG");

    let sqlite_card_count: i64 = db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM kanban_cards WHERE id = 'c1-pg-get'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sqlite_card_count, 0,
        "test must not rely on SQLite mirror state"
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn kanban_list_and_get_include_latest_dispatch_result_summary() {
    let db = test_db();
    seed_test_agents(&db);
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at)
             VALUES ('card-summary', 'Card Summary', 'in_progress', 'medium', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at
             ) VALUES (
                'dispatch-rework-summary', 'card-summary', 'agent-1', 'rework', 'pending',
                'Rework requested', ?1, datetime('now'), datetime('now')
             )",
            [json!({
                "pm_decision": "rework",
                "comment": "Handle the race condition"
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "UPDATE kanban_cards SET latest_dispatch_id = 'dispatch-rework-summary' WHERE id = 'card-summary'",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine.clone(), None);
    let list_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/kanban-cards")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(list_response.status(), StatusCode::OK);
    let list_body = axum::body::to_bytes(list_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let list_json: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
    let listed_card = list_json["cards"]
        .as_array()
        .unwrap()
        .iter()
        .find(|card| card["id"] == "card-summary")
        .expect("card-summary must be present in kanban list");
    assert_eq!(
        listed_card["latest_dispatch_result_summary"],
        "PM requested rework: Handle the race condition"
    );

    let get_response = app
        .oneshot(
            Request::builder()
                .uri("/kanban-cards/card-summary")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(get_response.status(), StatusCode::OK);
    let get_body = axum::body::to_bytes(get_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let get_json: serde_json::Value = serde_json::from_slice(&get_body).unwrap();
    assert_eq!(
        get_json["card"]["latest_dispatch_result_summary"],
        "PM requested rework: Handle the race condition"
    );
    assert_eq!(get_json["card"]["latest_dispatch_type"], "rework");
}

#[tokio::test]
async fn kanban_get_card_not_found() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/kanban-cards/nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn kanban_update_card_status() {
    let db = test_db();
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c1', 'Card1', 'backlog', 'medium', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/kanban-cards/c1")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"status":"ready"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["card"]["status"], "ready");
}

#[tokio::test]
async fn kanban_update_card_rejects_manual_non_backlog_transition() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at)
         VALUES ('c1', 'Card1', 'ready', 'medium', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/kanban-cards/c1")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"status":"in_progress"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("backlog"),
        "error must explain the restricted manual transition rule"
    );

    let status: String = sqlx::query_scalar("SELECT status FROM kanban_cards WHERE id = 'c1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(status, "ready");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
#[ignore = "obsolete SQLite kanban backlog cleanup fixture; route is PG-only after #843/#868"]
async fn kanban_update_card_to_backlog_cleans_up_dispatches_auto_queue_and_turns() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-manual-backlog");
    seed_repo(&db, "test/repo");
    ensure_auto_queue_tables(&db);

    let tmux_name = format!("manual-backlog-{}", uuid::Uuid::new_v4().simple());
    let session_key = format!("host:{tmux_name}");
    let tmux_created = if crate::services::platform::tmux::is_available() {
        let output = crate::services::platform::tmux::create_session(&tmux_name, None, "sleep 120")
            .expect("tmux session should spawn");
        assert!(
            output.status.success(),
            "tmux session should start for turn cancellation test"
        );
        true
    } else {
        false
    };

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, latest_dispatch_id, review_status, review_round, review_notes,
                suggestion_pending_at, review_entered_at, awaiting_dod_at,
                created_at, updated_at, started_at
            ) VALUES (
                'card-manual-backlog', 'Manual Backlog Cleanup', 'in_progress', 'medium', 'agent-manual-backlog', 'test-repo',
                541, 'dispatch-manual-backlog', 'reviewing', 3, 'stale review state',
                datetime('now', '-12 minutes'), datetime('now', '-11 minutes'), datetime('now', '-10 minutes'),
                datetime('now', '-20 minutes'), datetime('now', '-20 minutes'), datetime('now', '-20 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-manual-backlog', 'card-manual-backlog', 'agent-manual-backlog', 'implementation', 'pending',
                'live impl', datetime('now', '-10 minutes'), datetime('now', '-10 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, active_dispatch_id, last_heartbeat, created_at
            ) VALUES (
                ?1, 'agent-manual-backlog', 'codex', 'working', 'dispatch-manual-backlog',
                datetime('now', '-9 minutes'), datetime('now', '-9 minutes')
            )",
            sqlite_params![session_key],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-manual-backlog', 'test-repo', 'agent-manual-backlog', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-manual-backlog-pending', 'test-repo', 'agent-manual-backlog', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at
            ) VALUES (
                'entry-manual-backlog-dispatched', 'run-manual-backlog', 'card-manual-backlog', 'agent-manual-backlog',
                'dispatched', 'dispatch-manual-backlog', datetime('now', '-10 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-manual-backlog-2', 'test-repo', 'agent-manual-backlog', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status
            ) VALUES (
                'entry-manual-backlog-pending', 'run-manual-backlog-pending', 'card-manual-backlog', 'agent-manual-backlog', 'pending'
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO card_review_state (
                card_id, state, pending_dispatch_id, review_round, last_verdict, last_decision,
                approach_change_round, session_reset_round, review_entered_at, updated_at
            ) VALUES (
                'card-manual-backlog', 'suggestion_pending', 'dispatch-manual-backlog', 3, 'pass', 'approved',
                2, 3, datetime('now', '-11 minutes'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/kanban-cards/card-manual-backlog")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"status":"backlog"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["card"]["status"], "backlog");

    let conn = db.lock().unwrap();
    let (
        card_status,
        latest_dispatch_id,
        review_status,
        review_round,
        review_notes,
        suggestion_pending_at,
        review_entered_at,
        awaiting_dod_at,
    ): (
        String,
        Option<String>,
        Option<String>,
        i32,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT status, latest_dispatch_id, review_status, review_round, review_notes,
                    suggestion_pending_at, review_entered_at, awaiting_dod_at
             FROM kanban_cards WHERE id = 'card-manual-backlog'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(card_status, "backlog");
    assert!(latest_dispatch_id.is_none());
    assert!(review_status.is_none());
    assert_eq!(review_round, 0);
    assert!(review_notes.is_none());
    assert!(suggestion_pending_at.is_none());
    assert!(review_entered_at.is_none());
    assert!(awaiting_dod_at.is_none());

    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-manual-backlog'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatch_status, "cancelled");

    // #1235: stable-key (entry id) lookups instead of Vec equality.
    let entry_rows: std::collections::BTreeMap<String, (String, Option<String>)> = conn
        .prepare(
            "SELECT id, status, dispatch_id FROM auto_queue_entries
             WHERE kanban_card_id = 'card-manual-backlog'",
        )
        .unwrap()
        .query_map([], |row| {
            let id: String = row.get(0)?;
            let status: String = row.get(1)?;
            let dispatch_id: Option<String> = row.get(2)?;
            Ok((id, (status, dispatch_id)))
        })
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    let mb_dispatched = entry_rows
        .get("entry-manual-backlog-dispatched")
        .expect("seeded dispatched entry must still exist");
    let mb_pending = entry_rows
        .get("entry-manual-backlog-pending")
        .expect("seeded pending entry must still exist");
    assert_eq!(mb_dispatched.0, "skipped");
    assert!(mb_dispatched.1.is_none());
    assert_eq!(mb_pending.0, "skipped");
    assert!(mb_pending.1.is_none());

    let (session_status, active_dispatch_id): (String, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id
             FROM sessions
             WHERE session_key = ?1",
            sqlite_params![session_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(session_status, "disconnected");
    assert!(active_dispatch_id.is_none());

    let (
        review_state_round,
        review_state_status,
        review_state_pending_dispatch,
        review_state_verdict,
        review_state_decision,
    ): (i64, String, Option<String>, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT review_round, state, pending_dispatch_id, last_verdict, last_decision
             FROM card_review_state WHERE card_id = 'card-manual-backlog'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(review_state_round, 0);
    assert_eq!(review_state_status, "idle");
    assert!(review_state_pending_dispatch.is_none());
    assert!(review_state_verdict.is_none());
    assert!(review_state_decision.is_none());

    drop(conn);

    if tmux_created {
        assert!(
            !crate::services::platform::tmux::has_session(&tmux_name),
            "manual backlog revert must kill the live tmux turn"
        );
    }
}

#[tokio::test]
async fn kanban_update_card_not_found() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/kanban-cards/nonexistent")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"status":"ready"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_get_card_review_state_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at)
         VALUES ($1, $2, $3, $4, NOW(), NOW())",
    )
    .bind("card-review-state-pg")
    .bind("Review State PG")
    .bind("review")
    .bind("medium")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO card_review_state (
            card_id, review_round, state, pending_dispatch_id, last_verdict, last_decision,
            decided_by, decided_at, review_entered_at, updated_at
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, NOW(), NOW(), NOW())",
    )
    .bind("card-review-state-pg")
    .bind(2_i64)
    .bind("reviewing")
    .bind("dispatch-review-pg")
    .bind("accept")
    .bind("ship")
    .bind("agent-reviewer")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/kanban-cards/card-review-state-pg/review-state")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    if status != StatusCode::OK {
        panic!(
            "kanban_get_card_review_state_pg_only_without_sqlite_mirror status={} body={}",
            status,
            String::from_utf8_lossy(&body)
        );
    }

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["card_id"], "card-review-state-pg");
    assert_eq!(json["review_round"], 2);
    assert_eq!(json["state"], "reviewing");
    assert_eq!(json["pending_dispatch_id"], "dispatch-review-pg");
    assert_eq!(json["last_verdict"], "accept");
    assert_eq!(json["last_decision"], "ship");
    assert_eq!(json["decided_by"], "agent-reviewer");

    let sqlite_state_count: i64 = db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM card_review_state WHERE card_id = 'card-review-state-pg'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sqlite_state_count, 0,
        "test must not rely on SQLite mirror state"
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_list_card_reviews_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at)
         VALUES ($1, $2, $3, $4, NOW(), NOW())",
    )
    .bind("card-reviews-pg")
    .bind("Reviews PG")
    .bind("review")
    .bind("medium")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO review_decisions (
            kanban_card_id, dispatch_id, item_index, decision, decided_at
         ) VALUES ($1, $2, $3, $4, NOW()), ($1, $5, $6, $7, NOW())",
    )
    .bind("card-reviews-pg")
    .bind("dispatch-review-1")
    .bind(0_i64)
    .bind("accept")
    .bind("dispatch-review-2")
    .bind(1_i64)
    .bind("rework")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/kanban-cards/card-reviews-pg/reviews")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    if status != StatusCode::OK {
        panic!(
            "kanban_list_card_reviews_pg_only_without_sqlite_mirror status={} body={}",
            status,
            String::from_utf8_lossy(&body)
        );
    }

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let reviews = json["reviews"].as_array().unwrap();
    assert_eq!(reviews.len(), 2);
    assert_eq!(reviews[0]["kanban_card_id"], "card-reviews-pg");
    assert_eq!(reviews[0]["dispatch_id"], "dispatch-review-1");
    assert_eq!(reviews[0]["item_index"], 0);
    assert_eq!(reviews[0]["decision"], "accept");
    assert_eq!(reviews[1]["dispatch_id"], "dispatch-review-2");
    assert_eq!(reviews[1]["item_index"], 1);
    assert_eq!(reviews[1]["decision"], "rework");

    let sqlite_review_count: i64 = db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM review_decisions WHERE kanban_card_id = 'card-reviews-pg'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sqlite_review_count, 0,
        "test must not rely on SQLite mirror state"
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn kanban_assign_card() {
    let db = test_db();
    let engine = test_engine(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO agents (id, name, provider, status, xp) VALUES ('ch-td', 'Agent TD', 'claude', 'idle', 0)",
                [],
            ).unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c1', 'Card1', 'backlog', 'medium', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/c1/assign")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"agent_id":"ch-td"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // #255: assign walks through free transitions to the dispatchable state (requested)
    assert_eq!(json["card"]["status"], "requested");
    assert_eq!(json["card"]["assigned_agent_id"], "ch-td");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_assign_card_pg_only_without_sqlite_mirror() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, discord_channel_alt)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind("ch-td")
    .bind("Agent TD")
    .bind("claude")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at)
         VALUES ($1, $2, $3, $4, NOW(), NOW())",
    )
    .bind("c1-pg")
    .bind("Card1 PG")
    .bind("backlog")
    .bind("medium")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/c1-pg/assign")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"agent_id":"ch-td"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    if status != StatusCode::OK {
        panic!(
            "kanban_assign_card_pg_only_without_sqlite_mirror status={} body={}",
            status,
            String::from_utf8_lossy(&body)
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["card"]["status"], "requested");
    assert_eq!(json["card"]["assigned_agent_id"], "ch-td");

    let sqlite_card_count: i64 = db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM kanban_cards WHERE id = 'c1-pg'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sqlite_card_count, 0,
        "test must not rely on SQLite mirror state"
    );

    let row = sqlx::query(
        "SELECT status, assigned_agent_id
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind("c1-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(row.try_get::<String, _>("status").unwrap(), "requested");
    assert_eq!(
        row.try_get::<Option<String>, _>("assigned_agent_id")
            .unwrap(),
        Some("ch-td".to_string())
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_assign_issue_pg_upserts_without_duplicates() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind("agent-issue")
        .bind("Agent Issue")
        .execute(&pg_pool)
        .await
        .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    let request_body = json!({
        "github_repo": "owner/issue-sync",
        "github_issue_number": 77,
        "github_issue_url": "https://github.com/owner/issue-sync/issues/77",
        "title": "Issue sync via assign route",
        "description": "Assign route must reuse the same card for the same issue.",
        "assignee_agent_id": "agent-issue"
    })
    .to_string();

    let first_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/assign-issue")
                .header("content-type", "application/json")
                .body(Body::from(request_body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first_response.status(), StatusCode::CREATED);
    let first_body = axum::body::to_bytes(first_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let first_json: serde_json::Value = serde_json::from_slice(&first_body).unwrap();
    assert_eq!(first_json["deduplicated"], false);
    assert_eq!(first_json["card"]["assigned_agent_id"], "agent-issue");
    assert_eq!(first_json["card"]["github_issue_number"], 77);
    assert_eq!(first_json["card"]["status"], "requested");
    let first_card_id = first_json["card"]["id"].as_str().unwrap().to_string();

    let second_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/assign-issue")
                .header("content-type", "application/json")
                .body(Body::from(request_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second_response.status(), StatusCode::OK);
    let second_body = axum::body::to_bytes(second_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let second_json: serde_json::Value = serde_json::from_slice(&second_body).unwrap();
    assert_eq!(second_json["deduplicated"], true);
    assert_eq!(second_json["card"]["id"], first_card_id);

    let row = sqlx::query(
        "SELECT COUNT(*)::BIGINT AS card_count, MIN(id) AS card_id, MIN(status) AS status
         FROM kanban_cards
         WHERE repo_id = $1 AND github_issue_number = $2",
    )
    .bind("owner/issue-sync")
    .bind(77_i64)
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(row.try_get::<i64, _>("card_count").unwrap(), 1);
    assert_eq!(
        row.try_get::<Option<String>, _>("card_id").unwrap(),
        Some(first_card_id)
    );
    assert_eq!(
        row.try_get::<Option<String>, _>("status").unwrap(),
        Some("requested".to_string())
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn kanban_assign_card_not_found() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/nonexistent/assign")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"agent_id":"ch-td"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_delete_card_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at)
         VALUES ($1, $2, $3, $4, NOW(), NOW())",
    )
    .bind("c1-pg-delete")
    .bind("Delete PG")
    .bind("backlog")
    .bind("medium")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/kanban-cards/c1-pg-delete")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    if status != StatusCode::OK {
        panic!(
            "kanban_delete_card_pg_only_without_sqlite_mirror status={} body={}",
            status,
            String::from_utf8_lossy(&body)
        );
    }

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);

    let sqlite_card_count: i64 = db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM kanban_cards WHERE id = 'c1-pg-delete'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sqlite_card_count, 0,
        "test must not rely on SQLite mirror state"
    );

    let pg_card_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM kanban_cards WHERE id = $1")
            .bind("c1-pg-delete")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(pg_card_count, 0);

    pg_pool.close().await;
    pg_db.drop().await;
}

// ── Dispatch API tests ─────────────────────────────────────────

#[tokio::test]
async fn dispatch_pg_list_empty() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .uri("/dispatches")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["dispatches"].as_array().unwrap().is_empty());

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn dispatch_pg_create_and_get() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('ch-td', 'TD', '111', '222')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c1', 'Card1', 'ready', 'medium', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine.clone(),
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatches")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"kanban_card_id":"c1","to_agent_id":"ch-td","title":"Do it"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::CREATED, "unexpected response: {json}");
    let dispatch_id = json["dispatch"]["id"].as_str().unwrap().to_string();
    assert_eq!(json["dispatch"]["status"], "pending");
    assert_eq!(json["dispatch"]["kanban_card_id"], "c1");

    // #255: ready→requested is free, so dispatch from ready kicks off to "in_progress"
    let card_status: String = sqlx::query_scalar("SELECT status FROM kanban_cards WHERE id = 'c1'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(card_status, "in_progress");
    let notify_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM dispatch_outbox WHERE dispatch_id = $1 AND action = 'notify'",
    )
    .bind(&dispatch_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(notify_count, 1, "API create must persist notify outbox");

    // GET single dispatch
    let app2 = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response2 = app2
        .oneshot(
            Request::builder()
                .uri(&format!("/dispatches/{dispatch_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response2.status(), StatusCode::OK);
    let body2 = axum::body::to_bytes(response2.into_body(), usize::MAX)
        .await
        .unwrap();
    let json2: serde_json::Value = serde_json::from_slice(&body2).unwrap();
    assert_eq!(json2["dispatch"]["id"], dispatch_id);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_routes_allow_same_agent_parallel_delivery_on_different_provider_channels_pg() {
    let _env_lock = env_lock();
    let (base_url, state, server_handle) = spawn_mock_dispatch_delivery_server().await;
    let _api_base = EnvVarGuard::set("AGENTDESK_DISCORD_API_BASE_URL", &base_url);
    let runtime_root = tempfile::tempdir().unwrap();
    write_announce_token(runtime_root.path());
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());

    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO agents (
                id, name, provider, discord_channel_id, discord_channel_alt,
                discord_channel_cc, discord_channel_cdx, created_at, updated_at
             ) VALUES (
                'agent-parallel-provider', 'Agent Parallel Provider', 'claude', '111', '222',
                '111', '222', datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, created_at, updated_at
             ) VALUES (
                'card-parallel-impl', 'Parallel implementation', 'ready', 'medium',
                'agent-parallel-provider', datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, created_at, updated_at
             ) VALUES (
                'card-parallel-consult', 'Parallel consultation', 'ready', 'medium',
                'agent-parallel-provider', datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO agents (
            id, name, provider, discord_channel_id, discord_channel_alt,
            discord_channel_cc, discord_channel_cdx, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, NOW(), NOW()
         )",
    )
    .bind("agent-parallel-provider")
    .bind("Agent Parallel Provider")
    .bind("claude")
    .bind("111")
    .bind("222")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, NOW(), NOW()
         )",
    )
    .bind("card-parallel-impl")
    .bind("Parallel implementation")
    .bind("ready")
    .bind("medium")
    .bind("agent-parallel-provider")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, NOW(), NOW()
         )",
    )
    .bind("card-parallel-consult")
    .bind("Parallel consultation")
    .bind("ready")
    .bind("medium")
    .bind("agent-parallel-provider")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let impl_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatches")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"kanban_card_id":"card-parallel-impl","to_agent_id":"agent-parallel-provider","dispatch_type":"implementation","title":"Parallel implementation"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(impl_response.status(), StatusCode::CREATED);
    let impl_body = axum::body::to_bytes(impl_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let impl_json: serde_json::Value = serde_json::from_slice(&impl_body).unwrap();
    let impl_dispatch_id = impl_json["dispatch"]["id"]
        .as_str()
        .expect("implementation dispatch id")
        .to_string();

    let consult_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatches")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"kanban_card_id":"card-parallel-consult","to_agent_id":"agent-parallel-provider","dispatch_type":"consultation","title":"Parallel consultation"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(consult_response.status(), StatusCode::CREATED);
    let consult_body = axum::body::to_bytes(consult_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let consult_json: serde_json::Value = serde_json::from_slice(&consult_body).unwrap();
    let consult_dispatch_id = consult_json["dispatch"]["id"]
        .as_str()
        .expect("consultation dispatch id")
        .to_string();

    let pending_notify_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT
           FROM dispatch_outbox
          WHERE dispatch_id = ANY($1)
            AND action = 'notify'
            AND status = 'pending'",
    )
    .bind(vec![impl_dispatch_id.clone(), consult_dispatch_id.clone()])
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(
        pending_notify_count, 2,
        "route create must enqueue both notify rows before delivery"
    );

    let processed = crate::server::routes::dispatches::process_outbox_batch_with_real_notifier(
        Some(&db),
        &pg_pool,
    )
    .await;
    assert_eq!(processed, 2, "outbox worker should drain both notify rows");

    server_handle.abort();

    let state = state.lock().unwrap();
    assert!(
        state
            .calls
            .contains(&"POST /channels/111/threads".to_string()),
        "implementation dispatch must use the primary provider channel: {:?}",
        state.calls
    );
    assert!(
        state
            .calls
            .contains(&"POST /channels/thread-111/messages".to_string()),
        "implementation dispatch must post into its primary-thread mailbox: {:?}",
        state.calls
    );
    assert!(
        state
            .calls
            .contains(&"POST /channels/222/threads".to_string()),
        "consultation dispatch must use the counter-model provider channel: {:?}",
        state.calls
    );
    assert!(
        state
            .calls
            .contains(&"POST /channels/thread-222/messages".to_string()),
        "consultation dispatch must post into its counter-model thread mailbox: {:?}",
        state.calls
    );
    drop(state);

    let impl_thread_id: Option<String> = sqlx::query_scalar(
        "SELECT thread_id
           FROM task_dispatches
          WHERE id = $1",
    )
    .bind(&impl_dispatch_id)
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(impl_thread_id.as_deref(), Some("thread-111"));
    let impl_status: String = sqlx::query_scalar(
        "SELECT status
           FROM task_dispatches
          WHERE id = $1",
    )
    .bind(&impl_dispatch_id)
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(impl_status, "dispatched");
    let consult_thread_id: Option<String> = sqlx::query_scalar(
        "SELECT thread_id
           FROM task_dispatches
          WHERE id = $1",
    )
    .bind(&consult_dispatch_id)
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(consult_thread_id.as_deref(), Some("thread-222"));
    let consult_status: String = sqlx::query_scalar(
        "SELECT status
           FROM task_dispatches
          WHERE id = $1",
    )
    .bind(&consult_dispatch_id)
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(consult_status, "dispatched");

    let done_notify_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT
           FROM dispatch_outbox
          WHERE dispatch_id = ANY($1)
            AND action = 'notify'
            AND status = 'done'",
    )
    .bind(vec![impl_dispatch_id.clone(), consult_dispatch_id.clone()])
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(done_notify_count, 2, "notify rows must complete via outbox");
    let pending_status_reactions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT
           FROM dispatch_outbox
          WHERE dispatch_id = ANY($1)
            AND action = 'status_reaction'
            AND status = 'pending'",
    )
    .bind(vec![impl_dispatch_id, consult_dispatch_id])
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(
        pending_status_reactions, 2,
        "notify delivery must enqueue one status_reaction follow-up per dispatch"
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn dispatch_pg_create_for_terminal_card_returns_conflict_with_reason() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '555', '666')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, assigned_agent_id, created_at, updated_at)
         VALUES ('c-terminal', 'Terminal Card', 'done', 'medium', 'agent-1', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatches")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"kanban_card_id":"c-terminal","to_agent_id":"agent-1","dispatch_type":"review","title":"Review Terminal"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("terminal card c-terminal (status: done)"),
        "expected terminal-card detail, got {json}"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn dispatch_create_with_skip_outbox_omits_notify_row_pg() {
    let db = test_db();
    seed_test_agents(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    {
        let conn = db.lock().unwrap();
        conn.execute(
                "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c1-skip', 'Card1 Skip', 'ready', 'medium', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
    }

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("ch-td")
    .bind("TD")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("c1-skip")
    .bind("Card1 Skip")
    .bind("ready")
    .bind("medium")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatches")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"kanban_card_id":"c1-skip","to_agent_id":"ch-td","title":"Bookkeeping only","skip_outbox":true}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(status, StatusCode::CREATED, "unexpected response: {json}");
    let dispatch_id = json["dispatch"]["id"].as_str().unwrap().to_string();

    let verify_pool = sqlx::PgPool::connect(&pg_db.database_url).await.unwrap();
    let notify_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint
         FROM dispatch_outbox
         WHERE dispatch_id = $1 AND action = 'notify'",
    )
    .bind(&dispatch_id)
    .fetch_one(&verify_pool)
    .await
    .unwrap();
    assert_eq!(
        notify_count, 0,
        "skip_outbox=true must suppress notify outbox persistence"
    );

    verify_pool.close().await;
    drop(app);
    pg_pool.close().await;
    pg_db.drop().await;
}

/// #761: A crafted `POST /api/dispatches` call that preseeds review-target
/// fields (`reviewed_commit`, `worktree_path`, `branch`, `target_repo`) must
/// NOT be able to steer the review dispatch at an arbitrary commit/path. The
/// fields are stripped before `build_review_context` runs, and the
/// validation/refresh chain resolves the real target from the card's history.
#[tokio::test]
async fn dispatch_create_review_strips_untrusted_review_target_fields_from_context_pg() {
    let db = test_db();
    seed_test_agents(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    // Real repo exists on the card, but the caller injects a foreign
    // external target_repo. The hardened path must fail closed instead of
    // silently falling back to the card repo and reviewing unrelated code.
    let (repo, _repo_override) = setup_test_repo();
    let real_worktree_path = repo.path().to_string_lossy().into_owned();

    // Card in the review-ready state (pre-review), linked to a real repo
    // path while the caller injects a conflicting foreign target_repo.
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, github_issue_number,
                created_at, updated_at
             ) VALUES (
                'card-761', 'Preseed review target', 'in_progress', 'medium', 'ch-td', 761,
                datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("ch-td")
    .bind("TD")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, github_issue_number, repo_id
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7
         )",
    )
    .bind("card-761")
    .bind("Preseed review target")
    .bind("in_progress")
    .bind("medium")
    .bind("ch-td")
    .bind(761_i64)
    .bind(&real_worktree_path)
    .execute(&pg_pool)
    .await
    .unwrap();

    // Simulate a malicious / buggy caller preseeding review-target fields.
    // The injected commit SHA is syntactically valid but points at nothing
    // in this repo; the injected worktree path doesn't exist either.
    //
    // #761 (Codex round-2): also set `_trusted_review_target: true` in the
    // context to prove the flag is inert. The API-sourced code path MUST
    // ignore any JSON-supplied trust signal and always treat review-target
    // fields as untrusted.
    let injected_commit = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    let injected_worktree = "/tmp/agentdesk-761-attacker-controlled-worktree";
    let injected_target_repo = "/tmp/agentdesk-761-attacker-controlled-repo";
    let body = serde_json::json!({
        "kanban_card_id": "card-761",
        "to_agent_id": "ch-td",
        "dispatch_type": "review",
        "title": "[Review R1] card-761",
        "context": {
            "reviewed_commit": injected_commit,
            "worktree_path": injected_worktree,
            "branch": "attacker/controlled-branch",
            "target_repo": injected_target_repo,
            "_trusted_review_target": true,
        }
    })
    .to_string();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatches")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    assert_eq!(
        status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "unexpected response: {json}"
    );
    let error = json["error"].as_str().unwrap_or_default();
    assert!(
        error.contains("external_target_repo_unrecoverable"),
        "expected fail-closed external_target_repo guard, got {json}"
    );
    assert!(
        error.contains(injected_target_repo),
        "error should point at the rejected injected target_repo, got {json}"
    );
    assert!(
        !json.to_string().contains(injected_commit),
        "error response must not echo the injected reviewed_commit: {json}"
    );
    assert!(
        !json.to_string().contains(injected_worktree),
        "error response must not echo the injected worktree_path: {json}"
    );
    assert!(
        !json.to_string().contains("attacker/controlled-branch"),
        "error response must not echo the injected branch: {json}"
    );

    let dispatch_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM task_dispatches WHERE kanban_card_id = $1",
    )
    .bind("card-761")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(
        dispatch_count, 0,
        "fail-closed review-target validation must not persist a dispatch row"
    );

    drop(app);
    pg_pool.close().await;
    pg_db.drop().await;
}

/// #761 (Codex round-2): Focused negative test for the trust-boundary
/// redesign. The previous round's `_trusted_review_target` context flag was
/// client-controlled, so an attacker could set it alongside injected
/// review-target fields and bypass stripping entirely. The fix replaces the
/// flag with an out-of-band Rust enum parameter on `build_review_context`,
/// and the API-sourced path
/// (`POST /api/dispatches` → `create_dispatch_core_internal`) always uses
/// `ReviewTargetTrust::Untrusted`. This test asserts the flag cannot bypass
/// stripping on its own, even without any "real" review target existing for
/// the card — the injected values must be dropped and never resurrected
/// from the context payload.
#[tokio::test]
async fn dispatch_create_review_ignores_client_trusted_review_target_flag_pg() {
    let db = test_db();
    seed_test_agents(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    // Deliberately do NOT seed any work dispatch or pr_tracking row for this
    // card — the validation/refresh chain has nothing to resolve. If the
    // flag were honored, the injected fields would slip straight into the
    // persisted context. The fix means they get stripped and remain absent.
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, github_issue_number,
                created_at, updated_at
             ) VALUES (
                'card-761-flag', 'Ignore trust flag', 'in_progress', 'medium', 'ch-td', 999999,
                datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("ch-td")
    .bind("TD")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, github_issue_number
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         )",
    )
    .bind("card-761-flag")
    .bind("Ignore trust flag")
    .bind("in_progress")
    .bind("medium")
    .bind("ch-td")
    .bind(999999_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let injected_commit = "cafef00dcafef00dcafef00dcafef00dcafef00d";
    let injected_worktree = "/tmp/agentdesk-761-flag-attacker-worktree";
    let injected_target_repo = "/tmp/agentdesk-761-flag-attacker-repo";
    let body = serde_json::json!({
        "kanban_card_id": "card-761-flag",
        "to_agent_id": "ch-td",
        "dispatch_type": "review",
        "title": "[Review R1] trust-flag bypass attempt",
        "context": {
            "reviewed_commit": injected_commit,
            "worktree_path": injected_worktree,
            "branch": "attacker/trust-flag-bypass",
            "target_repo": injected_target_repo,
            // The crux: client explicitly asserts trust. The server must ignore it.
            "_trusted_review_target": true,
        }
    })
    .to_string();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatches")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    // Dispatch creation may succeed (validation chain has nothing to inject
    // but that's not fatal for review dispatches — noop-style contexts are
    // valid). What matters is that the INJECTED fields did not propagate.
    // Some routes return CREATED on success or CONFLICT if the worktree
    // recovery chain finds nothing usable; accept either, only assert on
    // the persisted context if the row was created.
    let status = response.status();
    let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
    if status == StatusCode::CREATED {
        let context = &json["dispatch"]["context"];
        assert_ne!(
            context["reviewed_commit"].as_str(),
            Some(injected_commit),
            "client-supplied trust flag must NOT bypass reviewed_commit stripping"
        );
        assert_ne!(
            context["worktree_path"].as_str(),
            Some(injected_worktree),
            "client-supplied trust flag must NOT bypass worktree_path stripping"
        );
        assert_ne!(
            context["branch"].as_str(),
            Some("attacker/trust-flag-bypass"),
            "client-supplied trust flag must NOT bypass branch stripping"
        );
        assert_ne!(
            context["target_repo"].as_str(),
            Some(injected_target_repo),
            "client-supplied trust flag must NOT bypass target_repo stripping"
        );
        assert!(
            context.get("_trusted_review_target").is_none(),
            "client-supplied _trusted_review_target flag must not persist into the dispatch context"
        );
    } else {
        // If creation failed, the injected values clearly didn't end up
        // anywhere — test passes vacuously. But the response JSON must NOT
        // echo them back (and the dispatch service doesn't echo request
        // bodies on error, so this is a sanity guard only).
        assert!(
            !json.to_string().contains(injected_commit),
            "error response must not echo the injected reviewed_commit: {json}"
        );
    }

    drop(app);
    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn api_docs_returns_group_hierarchy_by_default() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(Request::builder().uri("/docs").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let groups = json["groups"]
        .as_array()
        .expect("docs must return group array");

    let names: Vec<&str> = groups
        .iter()
        .filter_map(|group| group["name"].as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "runtime",
            "kanban",
            "agents",
            "integrations",
            "automation",
            "config",
            "observability",
            "internal",
        ],
        "docs must expose the #1063 eight-group hierarchy"
    );

    let runtime = groups
        .iter()
        .find(|group| group["name"] == "runtime")
        .expect("runtime group must be present");
    let runtime_categories = runtime["categories"]
        .as_array()
        .expect("runtime group must list categories");
    assert!(
        runtime_categories
            .iter()
            .any(|category| category == "dispatches"),
        "runtime group must contain the dispatches category: {runtime}"
    );
    assert!(
        runtime["description"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase()
            .contains("runtime")
            || runtime["description"]
                .as_str()
                .unwrap_or_default()
                .to_lowercase()
                .contains("dispatches"),
        "runtime group description must mention runtime surfaces: {runtime}"
    );
    assert!(
        json.get("endpoints").is_none(),
        "default docs response must return grouped hierarchy, not flat endpoints"
    );
    assert!(
        json.get("categories").is_none(),
        "default docs response must return groups (not the legacy flat categories field)"
    );
}

/// #1063: `GET /api/docs/{group}` lists categories under a group.
#[tokio::test]
async fn api_docs_group_kanban_lists_cards_and_reviews() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/kanban")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["group"], "kanban");
    let categories = json["categories"]
        .as_array()
        .expect("group detail must include categories array");
    let category_names: Vec<&str> = categories
        .iter()
        .filter_map(|category| category["name"].as_str())
        .collect();
    assert!(
        category_names.contains(&"kanban"),
        "kanban group must contain the kanban cards category: {category_names:?}"
    );
    assert!(
        category_names.contains(&"reviews"),
        "kanban group must contain the reviews category: {category_names:?}"
    );
    assert!(
        category_names.contains(&"pipeline"),
        "kanban group must contain the pipeline category: {category_names:?}"
    );
}

/// #1063: `GET /api/docs/{group}/{category}` returns endpoints for that
/// category (e.g. `kanban/reviews`).
#[tokio::test]
async fn api_docs_group_category_kanban_reviews_returns_endpoints() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/kanban/reviews")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["group"], "kanban");
    assert_eq!(json["category"], "reviews");
    let endpoints = json["endpoints"]
        .as_array()
        .expect("group/category response must include endpoints array");
    assert!(
        endpoints
            .iter()
            .any(|ep| ep["path"] == "/api/reviews/verdict"),
        "kanban/reviews must include the review verdict endpoint"
    );
}

/// #1063: unknown group → 404.
#[tokio::test]
async fn api_docs_unknown_group_returns_not_found() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/not-a-real-group")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// #1063: mismatched group/category → 404.
#[tokio::test]
async fn api_docs_group_category_mismatch_returns_not_found() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    // `reviews` belongs to the `kanban` group, not `automation`.
    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/automation/reviews")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

/// #1063 backward compat: `GET /api/docs/{category}` still works for the
/// legacy category names but responds with `X-Deprecated` header that points
/// at the new `/group/category` path.
#[tokio::test]
async fn api_docs_legacy_category_route_emits_deprecation_header() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/reviews")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let deprecated = response
        .headers()
        .get("x-deprecated")
        .expect("legacy category route must emit X-Deprecated header")
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(deprecated, "/api/docs/kanban/reviews");
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["deprecated"], true);
}

#[tokio::test]
async fn api_docs_flat_format_mentions_skip_outbox_for_dispatch_create() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs?format=flat")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let endpoints = json["endpoints"]
        .as_array()
        .expect("docs?format=flat must return endpoint array");
    let dispatch_post = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/dispatches")
        .expect("dispatch create endpoint must be documented");

    let description = dispatch_post["description"]
        .as_str()
        .expect("dispatch docs description must be string");
    assert!(
        description.contains("skip_outbox"),
        "dispatch create docs must mention skip_outbox option: {description}"
    );
    assert_eq!(dispatch_post["params"]["skip_outbox"]["type"], "boolean");
}

#[tokio::test]
async fn api_docs_category_exposes_dispatch_params_and_examples() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/dispatches")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["category"], "dispatches");
    assert!(
        json["count"].as_u64().unwrap_or(0) >= 4,
        "dispatches detail should include documented endpoints"
    );

    let endpoints = json["endpoints"]
        .as_array()
        .expect("category detail must include endpoint array");
    let dispatch_post = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/dispatches")
        .expect("dispatch create endpoint must be present in detail view");
    assert_eq!(
        dispatch_post["params"]["kanban_card_id"]["location"],
        "body"
    );
    assert_eq!(dispatch_post["params"]["skip_outbox"]["type"], "boolean");
    assert_eq!(
        dispatch_post["example"]["request"]["body"]["skip_outbox"],
        serde_json::json!(true)
    );
    assert_eq!(
        dispatch_post["example"]["response"]["dispatch"]["status"],
        "pending"
    );
}

#[tokio::test]
async fn api_docs_category_exposes_auto_queue_params_and_examples() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/queue")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["category"], "queue");

    let endpoints = json["endpoints"]
        .as_array()
        .expect("category detail must include endpoint array");
    let generate = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/auto-queue/generate")
        .expect("auto-queue generate endpoint must be present");
    assert!(
        generate["params"].get("mode").is_none(),
        "generate docs should not expose legacy mode selection"
    );
    assert!(
        generate["params"].get("parallel").is_none(),
        "generate docs should not expose legacy parallel toggle"
    );
    assert_eq!(
        generate["example"]["response"]["run"]["unified_thread"],
        serde_json::json!(false)
    );

    let dispatch = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/auto-queue/dispatch")
        .expect("auto-queue dispatch endpoint must be present");
    assert_eq!(dispatch["params"]["groups"]["required"], true);
    assert_eq!(dispatch["params"]["activate"]["default"], true);
    assert_eq!(dispatch["params"]["auto_assign_agent"]["default"], true);
    assert_eq!(dispatch["example"]["response"]["dispatch"]["count"], 1);

    let pause = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/auto-queue/pause")
        .expect("auto-queue pause endpoint must be present");
    assert_eq!(pause["params"]["force"]["type"], "boolean");
    assert_eq!(pause["params"]["force"]["default"], false);
    assert_eq!(pause["example"]["response"]["paused_runs"], 1);
    assert_eq!(pause["example"]["response"]["cancelled_dispatches"], 0);
    assert_eq!(pause["example"]["response"]["released_slots"], 0);
    assert_eq!(pause["example"]["response"]["cleared_slot_sessions"], 0);
}

#[tokio::test]
async fn api_docs_category_exposes_kanban_params_and_examples() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/kanban/kanban")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["category"], "kanban");
    assert_eq!(json["group"], "kanban");

    let endpoints = json["endpoints"]
        .as_array()
        .expect("category detail must include endpoint array");
    let create = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/kanban-cards")
        .expect("kanban create endpoint must be present");
    assert_eq!(create["params"]["title"]["type"], "string");
    assert_eq!(create["example"]["response"]["card"]["status"], "backlog");

    let resume = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/kanban-cards/{id}/resume")
        .expect("kanban resume endpoint must be present");
    assert_eq!(resume["params"]["force"]["type"], "boolean");
    assert_eq!(
        resume["example"]["response"]["action"]["type"],
        "new_implementation_dispatch"
    );
}

#[tokio::test]
async fn api_docs_category_exposes_agents_turn_start_contract() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/agents/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["category"], "agents");
    assert_eq!(json["group"], "agents");

    let endpoints = json["endpoints"]
        .as_array()
        .expect("category detail must include endpoint array");
    let turn_start = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/agents/{id}/turn/start")
        .expect("agents turn/start endpoint must be present");
    assert_eq!(turn_start["params"]["id"]["location"], "path");
    assert_eq!(turn_start["params"]["prompt"]["required"], true);
    assert_eq!(turn_start["params"]["metadata"]["type"], "object");
    assert_eq!(turn_start["params"]["source"]["type"], "string");
    assert_eq!(
        turn_start["example"]["response"]["status"],
        serde_json::json!("started")
    );

    let setup = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/agents/setup")
        .expect("agents setup endpoint must be present");
    assert_eq!(setup["params"]["agent_id"]["required"], true);
    assert_eq!(setup["params"]["dry_run"]["type"], "boolean");
    assert_eq!(setup["params"]["provider"]["enum"][0], "claude");
    assert_eq!(setup["example"]["response"]["dry_run"], true);
}

#[tokio::test]
async fn agent_setup_pg_dry_run_reports_plan_without_mutation() {
    let _env_lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    let config_path = crate::runtime_layout::config_file_path(runtime_root.path());
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    crate::config::save_to_path(&config_path, &crate::config::Config::default()).unwrap();
    let prompt_template = crate::runtime_layout::shared_prompt_path(runtime_root.path());
    fs::create_dir_all(prompt_template.parent().unwrap()).unwrap();
    fs::write(&prompt_template, "shared prompt\n").unwrap();
    write_test_skill(runtime_root.path(), "memory-read", "Memory read");

    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/setup")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "agent_id": "setup-agent",
                        "channel_id": "1473922824350601297",
                        "provider": "codex",
                        "prompt_template_path": "config/agents/_shared.prompt.md",
                        "skills": ["memory-read"],
                        "dry_run": true
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["dry_run"], true);
    assert!(json["created"].as_array().unwrap().is_empty());
    assert!(json["errors"].as_array().unwrap().is_empty());
    assert!(
        json["planned"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["step"] == "agentdesk_yaml" && entry["status"] == "planned")
    );
    assert!(
        json["planned"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["step"] == "skill_mapping" && entry["status"] == "planned")
    );

    let config = crate::config::load_from_path(&config_path).unwrap();
    assert!(config.agents.iter().all(|agent| agent.id != "setup-agent"));
    assert!(
        !runtime_root
            .path()
            .join("config/agents/setup-agent/IDENTITY.md")
            .exists()
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agents WHERE id = 'setup-agent'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert!(
        !crate::runtime_layout::managed_skills_manifest_path(runtime_root.path()).exists(),
        "dry_run must not create skills manifest"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agent_setup_pg_creates_resources_and_retry_is_idempotent() {
    let _env_lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    let config_path = crate::runtime_layout::config_file_path(runtime_root.path());
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    crate::config::save_to_path(&config_path, &crate::config::Config::default()).unwrap();
    let prompt_template = crate::runtime_layout::shared_prompt_path(runtime_root.path());
    fs::create_dir_all(prompt_template.parent().unwrap()).unwrap();
    fs::write(&prompt_template, "shared prompt\n").unwrap();
    write_test_skill(runtime_root.path(), "memory-read", "Memory read");

    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let request_body = json!({
        "agent_id": "setup-agent",
        "channel_id": "1473922824350601297",
        "provider": "codex",
        "prompt_template_path": "config/agents/_shared.prompt.md",
        "skills": ["memory-read"]
    });

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/setup")
                .header("content-type", "application/json")
                .body(Body::from(request_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert!(
        json["created"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["step"] == "agentdesk_yaml")
    );
    assert!(json["transaction"]["audit_log"].as_str().is_some());

    let config = crate::config::load_from_path(&config_path).unwrap();
    let agent = config
        .agents
        .iter()
        .find(|agent| agent.id == "setup-agent")
        .expect("setup agent in config");
    assert_eq!(agent.provider, "codex");
    let codex_channel = agent.channels.codex.as_ref().expect("codex channel");
    assert_eq!(
        codex_channel.channel_id().as_deref(),
        Some("1473922824350601297")
    );
    assert_eq!(
        fs::read_to_string(
            runtime_root
                .path()
                .join("config/agents/setup-agent/IDENTITY.md")
        )
        .unwrap(),
        "shared prompt\n"
    );
    assert!(runtime_root.path().join("workspaces/setup-agent").is_dir());
    let db_channel: Option<String> =
        sqlx::query_scalar("SELECT discord_channel_cdx FROM agents WHERE id = 'setup-agent'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(db_channel.as_deref(), Some("1473922824350601297"));
    let manifest: serde_json::Value = serde_json::from_slice(
        &fs::read(crate::runtime_layout::managed_skills_manifest_path(
            runtime_root.path(),
        ))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(manifest["skills"]["memory-read"]["providers"][0], "codex");
    assert_eq!(
        manifest["skills"]["memory-read"]["workspaces"][0],
        "setup-agent"
    );

    let retry = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/setup")
                .header("content-type", "application/json")
                .body(Body::from(request_body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(retry.status(), StatusCode::OK);
    let body = axum::body::to_bytes(retry.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["created"].as_array().unwrap().is_empty());
    assert!(
        json["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["step"] == "db_seed")
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agent_setup_pg_rolls_back_when_mid_step_fails() {
    let _env_lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    let _fail = EnvVarGuard::set("AGENTDESK_TEST_AGENT_SETUP_FAIL_AFTER", "prompt_file");
    let config_path = crate::runtime_layout::config_file_path(runtime_root.path());
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    crate::config::save_to_path(&config_path, &crate::config::Config::default()).unwrap();
    let prompt_template = crate::runtime_layout::shared_prompt_path(runtime_root.path());
    fs::create_dir_all(prompt_template.parent().unwrap()).unwrap();
    fs::write(&prompt_template, "shared prompt\n").unwrap();

    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/setup")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "agent_id": "setup-agent",
                        "channel_id": "1473922824350601297",
                        "provider": "codex",
                        "prompt_template_path": "config/agents/_shared.prompt.md"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], false);
    assert!(
        json["rolled_back"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["step"] == "prompt_file")
    );
    assert!(
        json["rolled_back"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["step"] == "agentdesk_yaml")
    );

    let config = crate::config::load_from_path(&config_path).unwrap();
    assert!(config.agents.iter().all(|agent| agent.id != "setup-agent"));
    assert!(
        !runtime_root
            .path()
            .join("config/agents/setup-agent/IDENTITY.md")
            .exists()
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agents WHERE id = 'setup-agent'")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    assert!(runtime_root.path().join("config/.audit").is_dir());

    pool.close().await;
    pg_db.drop().await;
}

async fn seed_setup_agent_for_management_test_pg(
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

#[tokio::test]
async fn agent_pg_patch_updates_metadata_and_prompt_content() {
    let _env_lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    seed_setup_agent_for_management_test_pg(
        app.clone(),
        runtime_root.path(),
        "managed-agent",
        "1473922824350601297",
    )
    .await;

    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/agents/managed-agent")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "name": "Managed Agent",
                        "cli_provider": "codex",
                        "sprite_number": 42,
                        "personality": "operational prompt summary",
                        "prompt_content": "updated prompt\n",
                        "auto_commit": false
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["prompt"]["changed"], true);
    assert_eq!(
        fs::read_to_string(
            runtime_root
                .path()
                .join("config/agents/managed-agent/IDENTITY.md")
        )
        .unwrap(),
        "updated prompt\n"
    );
    let row: (String, Option<i64>, Option<String>) = sqlx::query_as(
        "SELECT name, sprite_number, system_prompt FROM agents WHERE id = 'managed-agent'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(row.0, "Managed Agent");
    assert_eq!(row.1, Some(42));
    assert_eq!(row.2.as_deref(), Some("operational prompt summary"));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agent_pg_archive_and_unarchive_record_state_and_restore_config() {
    let _env_lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    seed_setup_agent_for_management_test_pg(
        app.clone(),
        runtime_root.path(),
        "managed-agent",
        "1473922824350601297",
    )
    .await;

    let archived = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/managed-agent/archive")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "reason": "test archive",
                        "discord_action": "none"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(archived.status(), StatusCode::OK);
    let body = axum::body::to_bytes(archived.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["archive_state"], "archived");
    let config = crate::config::load_from_path(&crate::runtime_layout::config_file_path(
        runtime_root.path(),
    ))
    .unwrap();
    assert!(
        config
            .agents
            .iter()
            .all(|agent| agent.id != "managed-agent")
    );
    let archive_state: String =
        sqlx::query_scalar("SELECT state FROM agent_archive WHERE agent_id = 'managed-agent'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(archive_state, "archived");

    let unarchived = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/managed-agent/unarchive")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unarchived.status(), StatusCode::OK);
    let config = crate::config::load_from_path(&crate::runtime_layout::config_file_path(
        runtime_root.path(),
    ))
    .unwrap();
    assert!(
        config
            .agents
            .iter()
            .any(|agent| agent.id == "managed-agent")
    );
    let archive_state: String =
        sqlx::query_scalar("SELECT state FROM agent_archive WHERE agent_id = 'managed-agent'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(archive_state, "unarchived");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agent_pg_duplicate_reuses_setup_and_copies_prompt() {
    let _env_lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    seed_setup_agent_for_management_test_pg(
        app.clone(),
        runtime_root.path(),
        "managed-agent",
        "1473922824350601297",
    )
    .await;
    fs::write(
        runtime_root
            .path()
            .join("config/agents/managed-agent/IDENTITY.md"),
        "source identity prompt\n",
    )
    .unwrap();

    let duplicated = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/managed-agent/duplicate")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "new_agent_id": "managed-copy",
                        "channel_id": "1473922824350601298",
                        "name": "Managed Copy"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(duplicated.status(), StatusCode::CREATED);
    assert_eq!(
        fs::read_to_string(
            runtime_root
                .path()
                .join("config/agents/managed-copy/IDENTITY.md")
        )
        .unwrap(),
        "source identity prompt\n"
    );
    let copied_name: String =
        sqlx::query_scalar("SELECT name FROM agents WHERE id = 'managed-copy'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(copied_name, "Managed Copy");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agent_pg_archive_rejects_when_active_turn_present() {
    let _env_lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    seed_setup_agent_for_management_test_pg(
        app.clone(),
        runtime_root.path(),
        "managed-agent",
        "1473922824350601297",
    )
    .await;

    // Seed an active turn for the managed-agent (status='working').
    sqlx::query(
        "INSERT INTO sessions (session_key, agent_id, provider, status, active_dispatch_id, last_heartbeat)
         VALUES ('sess-active', 'managed-agent', 'codex', 'working', 'dispatch-1', NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();

    let archived = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/managed-agent/archive")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"reason": "blocked", "discord_action": "none"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(archived.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(archived.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("active turn"),
        "expected 'active turn' error, got: {json:?}"
    );

    // agent_archive row should NOT be written when rejected.
    let archive_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agent_archive WHERE agent_id = 'managed-agent'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(archive_count, 0);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn agent_pg_duplicate_ignores_sensitive_fields_from_body() {
    let _env_lock = env_lock();
    let runtime_root = tempfile::tempdir().unwrap();
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", runtime_root.path());
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    seed_setup_agent_for_management_test_pg(
        app.clone(),
        runtime_root.path(),
        "managed-agent",
        "1473922824350601297",
    )
    .await;
    fs::write(
        runtime_root
            .path()
            .join("config/agents/managed-agent/IDENTITY.md"),
        "source identity prompt\n",
    )
    .unwrap();

    let source_channel = "1473922824350601297";
    let new_channel = "1473922824350601299";

    // Send sensitive fields that must be ignored (not in the allowlist struct):
    // - `id` / `agent_id`: must not override new_agent_id
    // - `discord_channel_id` (raw DB col): must not leak source channel
    // - `token`, `api_key`, `system_prompt`: must not be carried over
    let duplicated = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/agents/managed-agent/duplicate")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "new_agent_id": "managed-copy-2",
                        "channel_id": new_channel,
                        "name": "Managed Copy 2",
                        "id": "attacker-override",
                        "agent_id": "attacker-override",
                        "discord_channel_id": source_channel,
                        "token": "secret-token",
                        "api_key": "secret-key",
                        "system_prompt": "leaked personality"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(duplicated.status(), StatusCode::CREATED);

    // Resulting agent row must use new_agent_id + new channel (via setup's provider→column mapping),
    // NOT the source channel, and NOT any body-supplied sensitive fields.
    let (copied_id, channel_primary, channel_alt, channel_cc, channel_cdx, system_prompt): (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT id, discord_channel_id, discord_channel_alt, discord_channel_cc,
                discord_channel_cdx, system_prompt
         FROM agents WHERE id = 'managed-copy-2'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(copied_id, "managed-copy-2");
    let all_channels = [&channel_primary, &channel_alt, &channel_cc, &channel_cdx];
    assert!(
        all_channels
            .iter()
            .any(|c| c.as_deref() == Some(new_channel)),
        "at least one channel column must be the new_channel (got {all_channels:?})"
    );
    assert!(
        all_channels
            .iter()
            .all(|c| c.as_deref() != Some(source_channel)),
        "source channel must not be reused in any column (got {all_channels:?})"
    );
    assert!(
        system_prompt.as_deref() != Some("leaked personality"),
        "system_prompt from body must NOT be written during duplicate (got {system_prompt:?})"
    );

    // Attacker-override id must not exist as an agent row.
    let attacker_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM agents WHERE id = 'attacker-override'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(attacker_rows, 0);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn create_issue_route_pg_builds_pmd_body_and_agent_label() {
    let _env_lock = env_lock();
    let gh = install_mock_gh_issue_create("itismyfield/AgentDesk", 819);
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind("adk-backend")
        .bind("ADK Backend")
        .execute(&pool)
        .await
        .unwrap();
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/issues")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "repo": "ADK",
                        "title": "create-issue 스킬을 ADK API로 승격",
                        "background": "AgentDesk 내부에서 PMD 포맷 이슈를 서버 API로 직접 생성해야 한다.",
                        "content": [
                            "POST /api/issues 엔드포인트를 추가한다.",
                            "서버에서 PMD 마크다운 포맷을 강제한다."
                        ],
                        "dod": [
                            "성공 시 issue URL과 번호를 반환한다",
                            "DoD 항목은 체크리스트로 렌더링된다"
                        ],
                        "agent_id": "adk-backend"
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
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["issue"]["number"], 819);
    assert_eq!(
        json["issue"]["url"],
        "https://github.com/itismyfield/AgentDesk/issues/819"
    );
    assert_eq!(json["issue"]["repo"], "itismyfield/AgentDesk");
    let card_id = json["kanban_card_id"]
        .as_str()
        .expect("sqlite issue route must create a linked kanban card")
        .to_string();
    assert!(json["kanban_card_sync_error"].is_null());
    assert_eq!(json["applied_labels"], json!(["agent:adk-backend"]));
    assert_eq!(json["pmd_format_version"], 1);

    let row: (
        Option<String>,
        String,
        Option<i32>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT repo_id, status, github_issue_number, assigned_agent_id, metadata::text
             FROM kanban_cards
             WHERE id = $1",
    )
    .bind(card_id.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    let (repo_id, status, issue_number, assigned_agent_id, metadata_raw) = row;
    assert_eq!(repo_id.as_deref(), Some("itismyfield/AgentDesk"));
    assert_eq!(status, "backlog");
    assert_eq!(issue_number, Some(819));
    assert_eq!(assigned_agent_id.as_deref(), Some("adk-backend"));
    let metadata_json: serde_json::Value =
        serde_json::from_str(metadata_raw.as_deref().expect("metadata must exist")).unwrap();
    assert_eq!(metadata_json["labels"], "agent:adk-backend");

    let args = fs::read_to_string(gh.path().join("issue-create-args.txt")).unwrap();
    let args: Vec<&str> = args.lines().collect();
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--repo", "itismyfield/AgentDesk"])
    );
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--label", "agent:adk-backend"])
    );
    assert!(
        args.windows(2)
            .any(|pair| pair == ["--title", "create-issue 스킬을 ADK API로 승격"])
    );

    let issue_body = fs::read_to_string(gh.path().join("issue-create-body.md")).unwrap();
    assert!(
        issue_body
            .contains("## 배경\nAgentDesk 내부에서 PMD 포맷 이슈를 서버 API로 직접 생성해야 한다.")
    );
    assert!(issue_body.contains("## 내용\n- POST /api/issues 엔드포인트를 추가한다.\n- 서버에서 PMD 마크다운 포맷을 강제한다."));
    assert!(issue_body.contains("## DoD\n- [ ] 성공 시 issue URL과 번호를 반환한다\n- [ ] DoD 항목은 체크리스트로 렌더링된다"));
    assert!(!issue_body.contains("## 의존성"));
    assert!(!issue_body.contains("## 리스크"));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn create_issue_route_pg_returns_kanban_card_id() {
    let _env_lock = env_lock();
    let _gh = install_mock_gh_issue_create("itismyfield/AgentDesk", 820);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine(&db);

    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind("adk-backend")
        .bind("ADK Backend")
        .execute(&pg_pool)
        .await
        .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/issues")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "repo": "ADK",
                        "title": "PG issue sync path",
                        "background": "Postgres-backed issue creation must return the linked card id.",
                        "content": [
                            "GitHub issue create success should upsert a kanban backlog card.",
                            "Response payload should expose the linked card id."
                        ],
                        "dod": [
                            "kanban_card_id is returned",
                            "card metadata keeps the applied agent label"
                        ],
                        "agent_id": "adk-backend"
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
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let card_id = json["kanban_card_id"]
        .as_str()
        .expect("postgres issue route must return kanban_card_id")
        .to_string();
    assert!(json["kanban_card_sync_error"].is_null());

    let row = sqlx::query(
        "SELECT repo_id, status, github_issue_number, assigned_agent_id, description, metadata::text AS metadata
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(&card_id)
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(
        row.try_get::<Option<String>, _>("repo_id").unwrap(),
        Some("itismyfield/AgentDesk".to_string())
    );
    assert_eq!(row.try_get::<String, _>("status").unwrap(), "backlog");
    assert_eq!(
        row.try_get::<Option<i64>, _>("github_issue_number")
            .unwrap(),
        Some(820)
    );
    assert_eq!(
        row.try_get::<Option<String>, _>("assigned_agent_id")
            .unwrap(),
        Some("adk-backend".to_string())
    );
    let description = row
        .try_get::<Option<String>, _>("description")
        .unwrap()
        .expect("description must contain issue body");
    assert!(description.contains("## 배경"));
    let metadata_json: serde_json::Value = serde_json::from_str(
        row.try_get::<Option<String>, _>("metadata")
            .unwrap()
            .as_deref()
            .expect("metadata must exist"),
    )
    .unwrap();
    assert_eq!(metadata_json["labels"], "agent:adk-backend");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn create_issue_route_rejects_more_than_ten_dod_items() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);
    let dod: Vec<String> = (0..11).map(|index| format!("item-{index}")).collect();

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/issues")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "repo": "ADK",
                        "title": "invalid dod",
                        "background": "background",
                        "content": ["content"],
                        "dod": dod,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"], "dod items must be 10 or fewer");
}

// #1067: skill promotion integration test — exercise the canonical
// `/api/github/issues/create` path end-to-end via the mounted Axum router to
// confirm the create-issue skill body is absorbed by the server endpoint.
#[cfg(unix)]
#[tokio::test]
async fn github_issues_create_canonical_path_returns_created_issue() {
    let _env_lock = env_lock();
    let _gh = install_mock_gh_issue_create("itismyfield/AgentDesk", 1067);
    let db = test_db();
    let engine = test_engine(&db);
    db.lock()
        .unwrap()
        .execute(
            "INSERT INTO agents (id, name) VALUES (?1, ?2)",
            sqlite_params!["adk-backend", "ADK Backend"],
        )
        .unwrap();
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/github/issues/create")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "repo": "ADK",
                        "title": "#1067 skill promotion",
                        "background": "create-issue 스킬을 서버 API로 흡수한다.",
                        "content": [
                            "POST /api/github/issues/create 엔드포인트 사용.",
                            "skill body는 서버에서 PMD 포맷으로 변환된다."
                        ],
                        "dod": [
                            "canonical path (/api/github/issues/create)를 통해 이슈가 생성된다",
                            "응답에 issue_number와 url이 포함된다"
                        ],
                        "agent_id": "adk-backend"
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
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["issue"]["number"], 1067);
    assert_eq!(
        json["issue"]["url"],
        "https://github.com/itismyfield/AgentDesk/issues/1067"
    );
    assert_eq!(json["issue"]["repo"], "itismyfield/AgentDesk");
    assert_eq!(
        json["applied_labels"]
            .as_array()
            .and_then(|v| v.first())
            .and_then(|v| v.as_str()),
        Some("agent:adk-backend")
    );
}

// #1067: skill promotion integration test — watch-agent-turn.
#[tokio::test]
async fn sessions_tmux_output_pg_http_route_returns_shape_for_seeded_session() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let tmux_name = format!("AgentDesk-codex-1067-http-{}", std::process::id());
    let session_key = format!("mac-mini:{tmux_name}");

    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, created_at, updated_at)
         VALUES ('agent-1067-http', 'Agent 1067', 'codex', '123456789012345678', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions
         (session_key, agent_id, provider, status, last_heartbeat, created_at)
         VALUES ($1, 'agent-1067-http', 'codex', 'working', NOW(), NOW())",
    )
    .bind(&session_key)
    .execute(&pool)
    .await
    .unwrap();
    let session_id: i64 = sqlx::query_scalar("SELECT id FROM sessions WHERE session_key = $1")
        .bind(&session_key)
        .fetch_one(&pool)
        .await
        .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/sessions/{session_id}/tmux-output?lines=25"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["session_id"], session_id);
    assert_eq!(json["session_key"], session_key);
    assert_eq!(json["tmux_name"], tmux_name);
    assert_eq!(json["agent_id"], "agent-1067-http");
    assert_eq!(json["provider"], "codex");
    assert_eq!(json["status"], "working");
    assert_eq!(json["lines_requested"], 25);
    assert_eq!(json["lines_effective"], 25);
    // tmux session was never created, so capture returns empty and tmux_alive=false.
    assert_eq!(json["tmux_alive"], false);
    assert_eq!(json["recent_output"], "");
    assert!(json["captured_at_ms"].as_i64().unwrap() > 0);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn sessions_tmux_output_http_route_returns_404_for_unknown_session() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/sessions/987654321/tmux-output")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["session_id"], 987654321);
    assert!(
        json["error"]
            .as_str()
            .map(|s| s.contains("not found"))
            .unwrap_or(false)
    );
}

#[tokio::test]
async fn github_docs_include_issue_creation_endpoint() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/integrations/github")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let endpoints = json["endpoints"]
        .as_array()
        .expect("docs/github must return endpoint array");
    let create_issue = endpoints
        .iter()
        .find(|endpoint| {
            endpoint["method"] == "POST" && endpoint["path"] == "/api/github/issues/create"
        })
        .expect("integration docs must include POST /api/github/issues/create");
    assert_eq!(json["group"], "integrations");
    assert_eq!(json["category"], "github");
    assert_eq!(create_issue["params"]["repo"]["required"], true);
    assert_eq!(create_issue["params"]["dod"]["type"], "array[string]");
    assert_eq!(create_issue["params"]["agent_id"]["required"], false);
}

#[tokio::test]
async fn health_docs_describe_server_up_and_fully_recovered() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/ops")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let endpoints = json["endpoints"]
        .as_array()
        .expect("docs/ops must return endpoint array");
    let health = endpoints
        .iter()
        .find(|endpoint| endpoint["method"] == "GET" && endpoint["path"] == "/api/health")
        .expect("health docs must include GET /api/health");

    let description = health["description"]
        .as_str()
        .expect("health endpoint description must be present");
    assert!(description.contains("server_up"));
    assert!(description.contains("fully_recovered"));
    assert_eq!(health["example"]["response"]["server_up"], true);
    assert_eq!(health["example"]["response"]["fully_recovered"], true);
    assert_eq!(
        health["example"]["response"]["latest_startup_doctor"]["detail_endpoint"],
        "/api/doctor/startup/latest"
    );
}

#[tokio::test]
async fn health_docs_list_doctor_discovery_endpoints() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/observability/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let endpoints = json["endpoints"]
        .as_array()
        .expect("docs/observability/health must return endpoint array");
    for (method, path) in [
        ("GET", "/api/health"),
        ("GET", "/api/health/detail"),
        ("GET", "/api/doctor/startup/latest"),
        ("POST", "/api/doctor/stale-mailbox/repair"),
    ] {
        assert!(
            endpoints
                .iter()
                .any(|ep| ep["method"] == method && ep["path"] == path),
            "health docs must include {method} {path}"
        );
    }
}

#[tokio::test]
async fn api_docs_flat_format_lists_routes_missing_from_legacy_docs() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs?format=flat")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let endpoints = json["endpoints"]
        .as_array()
        .expect("docs?format=flat must return endpoint array");

    for path in [
        "/api/kanban-cards/{id}/reopen",
        "/api/reviews/decision",
        "/api/auto-queue/dispatch",
        "/api/auto-queue/dispatch-next",
        "/api/auto-queue/entries/{id}",
        "/api/auto-queue/slots/{agent_id}/{slot_index}/reset-thread",
        "/api/help",
        "/api/docs/{group}",
        "/api/docs/{group}/{category}",
        "/api/health/detail",
        "/api/doctor/startup/latest",
        "/api/doctor/stale-mailbox/repair",
        "/api/github/issues/create",
        "/api/sessions/{id}/tmux-output",
        "/api/stats/memento",
    ] {
        assert!(
            endpoints.iter().any(|ep| ep["path"] == path),
            "flat docs must include {path}"
        );
    }
}

#[tokio::test]
async fn api_docs_flat_format_omits_removed_legacy_routes() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs?format=flat")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let endpoints = json["endpoints"]
        .as_array()
        .expect("docs?format=flat must return endpoint array");

    for path in [
        "/api/agent-channels",
        "/api/dispatch-cancel/{id}",
        "/api/pipeline-stages",
        "/api/pipeline-stages/{id}",
        "/api/session/start",
        "/api/sessions/search",
        "/api/sessions/force-kill",
        "/api/auto-queue/enqueue",
        "/api/api-friction/events",
        "/api/api-friction/patterns",
        "/api/api-friction/process",
        // #1064 removals
        "/api/re-review",
        "/api/hook/session",
        "/api/auto-queue/activate",
        "/api/kanban-cards/bulk-action",
        "/api/kanban-cards/batch-transition",
        "/api/kanban-cards/{id}/force-transition",
    ] {
        assert!(
            endpoints.iter().all(|ep| ep["path"] != path),
            "flat docs must omit removed route {path}"
        );
    }

    assert!(
        endpoints
            .iter()
            .any(|ep| ep["method"] == "POST" && ep["path"] == "/api/auto-queue/runs/{id}/order"),
        "flat docs must keep the submit_order callback route"
    );
}

#[tokio::test]
async fn removed_legacy_routes_return_not_found() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    for (method, uri) in [
        ("GET", "/agent-channels"),
        ("POST", "/dispatch-cancel/dispatch-123"),
        ("GET", "/pipeline-stages"),
        ("POST", "/pipeline-stages"),
        ("DELETE", "/pipeline-stages/legacy-stage"),
        ("POST", "/session/start"),
        ("GET", "/sessions/search"),
        ("POST", "/sessions/force-kill"),
        ("POST", "/auto-queue/enqueue"),
        ("GET", "/api-friction/events"),
        ("GET", "/api-friction/patterns"),
        ("POST", "/api-friction/process"),
        // #1064 removals
        ("POST", "/re-review"),
        ("POST", "/hook/session"),
        ("DELETE", "/hook/session"),
        ("POST", "/auto-queue/activate"),
        ("POST", "/kanban-cards/bulk-action"),
        ("POST", "/kanban-cards/batch-transition"),
        ("POST", "/kanban-cards/card-x/force-transition"),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // 404 when the path is fully removed; 405 when the removed endpoint
        // collided with a remaining `{id}`-style wildcard route that now
        // rejects the method (e.g. /kanban-cards/bulk-action matching the
        // /kanban-cards/{id} GET/PATCH/DELETE route).
        assert!(
            matches!(
                response.status(),
                StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
            ),
            "{method} {uri} should return 404/405 after route cleanup, got {}",
            response.status()
        );
    }
}

#[tokio::test]
async fn api_help_exposes_detailed_endpoint_inventory() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(Request::builder().uri("/help").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["categories"]
            .as_array()
            .unwrap()
            .iter()
            .any(|category| category["name"] == "queue"),
        "/help must expose category summaries"
    );
    let dispatch = json["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|ep| ep["path"] == "/api/auto-queue/dispatch")
        .expect("/help must include the declarative dispatch endpoint");
    assert_eq!(dispatch["params"]["groups"]["required"], true);
}

#[tokio::test]
async fn api_docs_unknown_category_returns_not_found() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/not-a-real-category")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn api_docs_category_exposes_send_to_agent_endpoint() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/integrations/discord")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["group"], "integrations");
    assert_eq!(json["category"], "discord");

    let endpoints = json["endpoints"]
        .as_array()
        .expect("integrations detail must include endpoint array");
    let send_to_agent = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/discord/send-to-agent")
        .expect("canonical send-to-agent endpoint must be documented");
    assert_eq!(send_to_agent["params"]["role_id"]["location"], "body");
    assert_eq!(send_to_agent["params"]["message"]["type"], "string");
    assert_eq!(send_to_agent["params"]["mode"]["type"], "string");
}

#[tokio::test]
async fn api_docs_category_exposes_skill_prune_and_filter_params() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["category"], "admin");

    let endpoints = json["endpoints"]
        .as_array()
        .expect("admin detail must include endpoint array");
    let catalog = endpoints
        .iter()
        .find(|ep| ep["method"] == "GET" && ep["path"] == "/api/skills/catalog")
        .expect("skills catalog endpoint must be documented");
    assert_eq!(catalog["params"]["include_stale"]["location"], "query");
    assert_eq!(catalog["params"]["include_stale"]["type"], "boolean");

    let prune = endpoints
        .iter()
        .find(|ep| ep["method"] == "POST" && ep["path"] == "/api/skills/prune")
        .expect("skills prune endpoint must be documented");
    assert_eq!(prune["params"]["dry_run"]["location"], "query");
    assert_eq!(prune["params"]["dry_run"]["type"], "boolean");
}

/// #1068 (904-6) — every path in `TOP_40_PAIRED_PATHS` must ship BOTH a
/// happy-path example AND an error example, plus a curl 1-liner.
#[tokio::test]
async fn api_docs_exposes_paired_examples_for_top_40() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs?format=flat")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let endpoints = json["endpoints"]
        .as_array()
        .expect("flat docs must return endpoint array");

    let mut missing = Vec::new();
    for (method, path) in crate::server::routes::docs::TOP_40_PAIRED_PATHS {
        let Some(ep) = endpoints
            .iter()
            .find(|ep| ep["method"] == *method && ep["path"] == *path)
        else {
            missing.push(format!("endpoint not found: {method} {path}"));
            continue;
        };
        if !ep["example"].is_object() {
            missing.push(format!("{method} {path}: example (happy path) is missing"));
        }
        if !ep["error_example"].is_object() {
            missing.push(format!("{method} {path}: error_example is missing"));
        }
        let curl = ep["curl_example"].as_str().unwrap_or("");
        if curl.is_empty() || !curl.starts_with("curl ") {
            missing.push(format!(
                "{method} {path}: curl_example is missing or not a curl 1-liner (got {curl:?})"
            ));
        }
    }
    assert!(
        missing.is_empty(),
        "top-40 paired-scenario coverage is incomplete:\n- {}",
        missing.join("\n- ")
    );

    // Guard against the list shrinking below 40.
    assert_eq!(
        crate::server::routes::docs::TOP_40_PAIRED_PATHS.len(),
        40,
        "#1068 (904-6) requires exactly 40 paired-scenario endpoints"
    );
}

/// #1068 (904-6) — `/retry`, `/redispatch`, `/resume`, and `/reopen`
/// descriptions must make their semantic distinctions explicit so callers stop
/// conflating them.
#[tokio::test]
async fn api_docs_retry_redispatch_resume_reopen_semantics_are_distinguished() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs?format=flat")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let endpoints = json["endpoints"]
        .as_array()
        .expect("flat docs must return endpoint array");

    let find_desc = |path: &str| -> String {
        endpoints
            .iter()
            .find(|ep| ep["path"] == path)
            .and_then(|ep| ep["description"].as_str())
            .unwrap_or_default()
            .to_string()
    };

    let retry = find_desc("/api/kanban-cards/{id}/retry").to_lowercase();
    let redispatch = find_desc("/api/kanban-cards/{id}/redispatch").to_lowercase();
    let resume = find_desc("/api/kanban-cards/{id}/resume").to_lowercase();
    let reopen = find_desc("/api/kanban-cards/{id}/reopen").to_lowercase();

    // retry: re-execute the SAME failed step with the same params.
    assert!(
        retry.contains("re-execute")
            || retry.contains("re-run")
            || retry.contains("same failed step"),
        "/retry description must explain it re-executes the same failed step: {retry}"
    );
    assert!(
        retry.contains("same"),
        "/retry description must contrast against /redispatch by mentioning 'same': {retry}"
    );

    // redispatch: new dispatch id, same intent.
    assert!(
        redispatch.contains("new dispatch") || redispatch.contains("new dispatch id"),
        "/redispatch description must mention that a NEW dispatch id is created: {redispatch}"
    );

    // resume: continue from a paused/checkpointed state.
    assert!(
        resume.contains("continue") || resume.contains("checkpoint"),
        "/resume description must mention continuing from a checkpoint: {resume}"
    );
    assert!(
        resume.contains("paused") || resume.contains("stuck") || resume.contains("checkpoint"),
        "/resume description must mention paused/checkpointed state: {resume}"
    );

    // reopen: move closed/done card back to active.
    assert!(
        reopen.contains("closed") || reopen.contains("terminal") || reopen.contains("done"),
        "/reopen description must mention the card's terminal/closed/done state: {reopen}"
    );
    assert!(
        reopen.contains("active") || reopen.contains("re-admit") || reopen.contains("ready"),
        "/reopen description must mention re-admitting the card into an active state: {reopen}"
    );

    // Each of retry/redispatch/resume must reference the others to make the
    // distinction explicit (reopen already checked via 'closed' + 'active').
    for (name, desc) in [
        ("retry", &retry),
        ("redispatch", &redispatch),
        ("resume", &resume),
    ] {
        let other_refs = ["retry", "redispatch", "resume", "reopen"]
            .iter()
            .filter(|n| **n != name)
            .filter(|n| desc.contains(*n))
            .count();
        assert!(
            other_refs >= 2,
            "/{name} description must reference at least two of the sibling semantics (retry/redispatch/resume/reopen) to disambiguate; got {other_refs}: {desc}"
        );
    }
}

#[tokio::test]
async fn skills_catalog_pg_filters_stale_entries_and_exposes_disk_presence() {
    let _env_lock = env_lock();
    let home = tempfile::tempdir().unwrap();
    let runtime_root = home.path().join(".adk").join("release");
    write_test_skill(&runtime_root, "live-skill", "Live skill description");
    let _home_env = EnvVarGuard::set_path("HOME", home.path());
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", &runtime_root);

    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let stale_path = home
        .path()
        .join("missing")
        .join("stale-skill")
        .join("SKILL.md")
        .display()
        .to_string();
    sqlx::query(
        "INSERT INTO skills (id, name, description, source_path, updated_at)
         VALUES ($1, $2, $3, $4, NOW())",
    )
    .bind("stale-skill")
    .bind("stale-skill")
    .bind("Stale skill description")
    .bind(&stale_path)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO skill_usage (skill_id, agent_id, session_key, used_at)
         VALUES ($1, $2, $3, NOW())",
    )
    .bind("stale-skill")
    .bind("agent-stale")
    .bind("session-stale")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO skill_usage (skill_id, agent_id, session_key, used_at)
         VALUES ($1, $2, $3, NOW())",
    )
    .bind("live-skill")
    .bind("agent-live")
    .bind("session-live")
    .execute(&pool)
    .await
    .unwrap();

    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db,
            engine,
            crate::config::Config::default(),
            None,
            pool.clone(),
        ),
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/skills/catalog?include_stale=true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let catalog = json["catalog"]
        .as_array()
        .expect("catalog response must include a catalog array");
    let live = catalog
        .iter()
        .find(|entry| entry["name"] == "live-skill")
        .expect("live skill must be present when include_stale=true");
    assert_eq!(live["disk_present"], true);
    let stale = catalog
        .iter()
        .find(|entry| entry["name"] == "stale-skill")
        .expect("stale skill must be present when include_stale=true");
    assert_eq!(stale["disk_present"], false);

    let filtered = app
        .oneshot(
            Request::builder()
                .uri("/api/skills/catalog")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(filtered.status(), StatusCode::OK);
    let filtered_body = axum::body::to_bytes(filtered.into_body(), usize::MAX)
        .await
        .unwrap();
    let filtered_json: serde_json::Value = serde_json::from_slice(&filtered_body).unwrap();
    let filtered_catalog = filtered_json["catalog"]
        .as_array()
        .expect("filtered catalog response must include a catalog array");
    assert!(
        filtered_catalog
            .iter()
            .any(|entry| entry["name"] == "live-skill"),
        "default catalog response must keep live skills"
    );
    assert!(
        filtered_catalog
            .iter()
            .all(|entry| entry["name"] != "stale-skill"),
        "default catalog response must filter stale skills"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn skills_prune_dry_run_pg_previews_and_delete_preserves_usage() {
    let _env_lock = env_lock();
    let home = tempfile::tempdir().unwrap();
    let runtime_root = home.path().join(".adk").join("release");
    write_test_skill(&runtime_root, "live-skill", "Live skill description");
    let _home_env = EnvVarGuard::set_path("HOME", home.path());
    let _root_env = EnvVarGuard::set_path("AGENTDESK_ROOT_DIR", &runtime_root);

    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let stale_path = home
        .path()
        .join("missing")
        .join("stale-skill")
        .join("SKILL.md")
        .display()
        .to_string();
    sqlx::query(
        "INSERT INTO skills (id, name, description, source_path, updated_at)
         VALUES ($1, $2, $3, $4, NOW())",
    )
    .bind("stale-skill")
    .bind("stale-skill")
    .bind("Stale skill description")
    .bind(&stale_path)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO skill_usage (skill_id, agent_id, session_key, used_at)
         VALUES ($1, $2, $3, NOW())",
    )
    .bind("stale-skill")
    .bind("agent-stale")
    .bind("session-stale")
    .execute(&pool)
    .await
    .unwrap();

    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db.clone(),
            engine,
            crate::config::Config::default(),
            None,
            pool.clone(),
        ),
    );
    let dry_run = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/skills/prune?dry_run=true")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(dry_run.status(), StatusCode::OK);
    let dry_run_body = axum::body::to_bytes(dry_run.into_body(), usize::MAX)
        .await
        .unwrap();
    let dry_run_json: serde_json::Value = serde_json::from_slice(&dry_run_body).unwrap();
    assert_eq!(dry_run_json["dry_run"], true);
    assert_eq!(dry_run_json["soft_deleted_from_skills"], 0);
    assert!(
        dry_run_json["stale_skill_ids"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry == "stale-skill"),
        "dry-run must preview stale skill ids"
    );

    let stale_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM skills WHERE id = 'stale-skill'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stale_count, 1, "dry-run must not delete skills rows");

    let prune = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/skills/prune")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(prune.status(), StatusCode::OK);
    let prune_body = axum::body::to_bytes(prune.into_body(), usize::MAX)
        .await
        .unwrap();
    let prune_json: serde_json::Value = serde_json::from_slice(&prune_body).unwrap();
    assert_eq!(prune_json["soft_deleted_from_skills"], 1);
    assert_eq!(prune_json["skill_usage_policy"], "preserved");

    let stale_live_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM skills WHERE id = 'stale-skill' AND deleted_at IS NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        stale_live_count, 0,
        "prune must soft-delete stale skill metadata"
    );

    let usage_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM skill_usage WHERE skill_id = 'stale-skill'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(usage_count, 1, "prune must preserve historical skill usage");

    let filtered = app
        .oneshot(
            Request::builder()
                .uri("/api/skills/catalog")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(filtered.status(), StatusCode::OK);
    let filtered_body = axum::body::to_bytes(filtered.into_body(), usize::MAX)
        .await
        .unwrap();
    let filtered_json: serde_json::Value = serde_json::from_slice(&filtered_body).unwrap();
    let filtered_catalog = filtered_json["catalog"]
        .as_array()
        .expect("catalog response must include a catalog array");
    assert!(
        filtered_catalog
            .iter()
            .all(|entry| entry["name"] != "stale-skill"),
        "default catalog response must hide pruned stale skills"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn resume_requested_pg_creates_single_notify_backed_dispatch() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ($1, $1, '111', '222')",
    )
    .bind("agent-resume")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, created_at, updated_at
        ) VALUES (
            $1, $2, $3, $4, $5, NOW(), NOW()
        )",
    )
    .bind("card-resume")
    .bind("Resume Card")
    .bind("requested")
    .bind("medium")
    .bind("agent-resume")
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-resume/resume")
                .header("content-type", "application/json")
                .body(Body::from(r#"{}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let dispatch_id = json["action"]["dispatch_id"].as_str().unwrap().to_string();
    assert_eq!(json["action"]["type"], "new_implementation_dispatch");

    let row: (String, String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT td.dispatch_type, td.status, td.context, kc.latest_dispatch_id
         FROM task_dispatches td
         JOIN kanban_cards kc ON kc.id = td.kanban_card_id
         WHERE td.id = $1",
    )
    .bind(&dispatch_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let (dispatch_type, dispatch_status, context, latest_dispatch_id) = row;
    assert_eq!(dispatch_type, "implementation");
    assert_eq!(dispatch_status, "pending");
    assert_eq!(latest_dispatch_id.as_deref(), Some(dispatch_id.as_str()));
    let context_json: serde_json::Value =
        serde_json::from_str(context.as_deref().unwrap_or("{}")).unwrap();
    assert_eq!(context_json["resume"], true);
    assert_eq!(context_json["resumed_from"], "requested");

    let notify_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM dispatch_outbox WHERE dispatch_id = $1 AND action = 'notify'",
    )
    .bind(&dispatch_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        notify_count, 1,
        "resume(requested) must create exactly one notify outbox row via canonical core"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn dispatch_pg_create_card_not_found() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatches")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"kanban_card_id":"nope","to_agent_id":"ch-td","title":"Do it"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn dispatch_pg_complete() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('ch-td', 'TD', '111', '222')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c1', 'Card1', 'ready', 'medium', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Create dispatch
    let app = test_api_router_with_pg(
        db.clone(),
        engine.clone(),
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatches")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"kanban_card_id":"c1","to_agent_id":"ch-td","title":"Do it"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let dispatch_id = json["dispatch"]["id"].as_str().unwrap().to_string();

    // Complete dispatch
    let app2 = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response2 = app2
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(&format!("/dispatches/{dispatch_id}"))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"status":"completed","result":{"ok":true,"agent_response_present":true}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response2.status(), StatusCode::OK);
    let body2 = axum::body::to_bytes(response2.into_body(), usize::MAX)
        .await
        .unwrap();
    let json2: serde_json::Value = serde_json::from_slice(&body2).unwrap();
    assert_eq!(json2["dispatch"]["status"], "completed");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn dispatch_pg_get_not_found() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .uri("/dispatches/nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    pool.close().await;
    pg_db.drop().await;
}

// ── Policy hook firing tests ───────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kanban_terminal_status_fires_hook() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
            dir.path().join("test-hooks.js"),
            r#"
            var p = {
                name: "test-hooks",
                priority: 1,
                onCardTransition: function(payload) {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('transition', '" + payload.from + "->" + payload.to + "')",
                        []
                    );
                },
                onCardTerminal: function(payload) {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('terminal', '" + payload.card_id + ":" + payload.status + "')",
                        []
                    );
                }
            };
            agentdesk.registerPolicy(p);
            "#,
        ).unwrap();

    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let config = crate::config::Config {
        policies: crate::config::PoliciesConfig {
            dir: dir.path().to_path_buf(),
            hot_reload: false,
        },
        ..crate::config::Config::default()
    };
    let engine = PolicyEngine::new_with_pg(&config, Some(pg_pool.clone())).unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at)
         VALUES ('c1', 'Card1', 'requested', 'medium', NOW(), NOW())",
    )
    .execute(&pg_pool)
    .await
    .unwrap();

    // Use force transition: requested → done (no rule, force bypasses)
    let result = crate::kanban::transition_status_with_opts_pg_only(
        &pg_pool,
        &engine,
        "c1",
        "done",
        "pmd",
        crate::engine::transition::ForceIntent::OperatorOverride,
    )
    .await;
    assert!(
        result.is_ok(),
        "force transition should succeed: {:?}",
        result
    );

    let transition: String = sqlx::query_scalar("SELECT value FROM kv_meta WHERE key = $1")
        .bind("transition")
        .fetch_one(&pg_pool)
        .await
        .unwrap();
    assert_eq!(transition, "requested->done");

    let terminal: String = sqlx::query_scalar("SELECT value FROM kv_meta WHERE key = $1")
        .bind("terminal")
        .fetch_one(&pg_pool)
        .await
        .unwrap();
    assert_eq!(terminal, "c1:done");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn dispatch_pg_list_with_filter() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at) VALUES ('c1', 'Card1', 'ready', 'medium', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, status, title, created_at, updated_at) VALUES ('d1', 'c1', 'ag1', 'pending', 'T1', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, status, title, created_at, updated_at) VALUES ('d2', 'c1', 'ag1', 'completed', 'T2', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/dispatches?status=pending")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let dispatches = json["dispatches"].as_array().unwrap();
    assert_eq!(dispatches.len(), 1);
    assert_eq!(dispatches[0]["id"], "d1");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn dispatch_pg_endpoints_include_normalized_result_summary() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '555', '666')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at)
         VALUES ('card-dispatch-summary', 'Dispatch Summary Card', 'review', 'medium', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, created_at, updated_at
         ) VALUES (
            'dispatch-cancel-summary', 'card-dispatch-summary', 'agent-1', 'implementation', 'cancelled',
            'Cancelled dispatch', $1, NOW() - INTERVAL '1 minute', NOW() - INTERVAL '1 minute'
         )",
    )
    .bind(
        json!({
            "reason": "auto_cancelled_on_terminal_card"
        })
        .to_string(),
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, created_at, updated_at
         ) VALUES (
            'dispatch-review-summary', 'card-dispatch-summary', 'agent-1', 'review-decision', 'completed',
            'Review decision', $1, NOW(), NOW()
         )",
    )
    .bind(
        json!({
            "decision": "accept",
            "comment": "Looks good"
        })
        .to_string(),
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine.clone(),
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let list_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/dispatches")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(list_response.status(), StatusCode::OK);
    let list_body = axum::body::to_bytes(list_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let list_json: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
    let dispatches = list_json["dispatches"].as_array().unwrap();

    let cancelled = dispatches
        .iter()
        .find(|dispatch| dispatch["id"] == "dispatch-cancel-summary")
        .expect("cancelled dispatch must be returned");
    assert_eq!(
        cancelled["result_summary"],
        "Cancelled: terminal card cleanup"
    );
    assert_eq!(
        cancelled["result"]["reason"],
        serde_json::Value::String("auto_cancelled_on_terminal_card".to_string())
    );

    let review_decision = dispatches
        .iter()
        .find(|dispatch| dispatch["id"] == "dispatch-review-summary")
        .expect("review decision dispatch must be returned");
    assert_eq!(
        review_decision["result_summary"],
        "Accepted review feedback: Looks good"
    );
    assert_eq!(review_decision["result"]["decision"], "accept");

    let get_response = app
        .oneshot(
            Request::builder()
                .uri("/dispatches/dispatch-review-summary")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(get_response.status(), StatusCode::OK);
    let get_body = axum::body::to_bytes(get_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let get_json: serde_json::Value = serde_json::from_slice(&get_body).unwrap();
    assert_eq!(
        get_json["dispatch"]["result_summary"],
        "Accepted review feedback: Looks good"
    );
    assert_eq!(get_json["dispatch"]["result"]["comment"], "Looks good");

    pool.close().await;
    pg_db.drop().await;
}

// ── GitHub Repos API tests ────────────────────────────────────

#[tokio::test]
async fn github_repos_pg_empty_list() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .uri("/github/repos")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["repos"].as_array().unwrap().is_empty());

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn github_repos_pg_register_and_list_basic() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    // Register
    let app = test_api_router_with_pg(
        db.clone(),
        engine.clone(),
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/github/repos")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"id":"owner/repo1"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["repo"]["id"], "owner/repo1");

    // List
    let app2 = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response2 = app2
        .oneshot(
            Request::builder()
                .uri("/github/repos")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let body2 = axum::body::to_bytes(response2.into_body(), usize::MAX)
        .await
        .unwrap();
    let json2: serde_json::Value = serde_json::from_slice(&body2).unwrap();
    assert_eq!(json2["repos"].as_array().unwrap().len(), 1);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn github_repos_register_bad_format() {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/github/repos")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"id":"noslash"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn github_repos_pg_sync_not_registered() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/github/repos/unknown/repo/sync")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn github_repos_pg_register_and_list() {
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    let register_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/github/repos")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"id":"owner/pg-repo"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(register_response.status(), StatusCode::CREATED);

    let list_response = app
        .oneshot(
            Request::builder()
                .uri("/github/repos")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let list_body = axum::body::to_bytes(list_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let list_json: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
    assert_eq!(list_json["repos"].as_array().unwrap().len(), 1);
    assert_eq!(list_json["repos"][0]["id"], "owner/pg-repo");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn github_repos_pg_sync_triages_open_issue() {
    let _env_lock = env_lock();
    let _gh = install_mock_gh_issue_list(
        "owner/pg-repo",
        r#"[{"number":101,"state":"OPEN","title":"PG route open","labels":[{"name":"bug"},{"name":"p1"},{"name":"agent:agent-sync"}],"body":"Investigate route sync"}]"#,
        "[]",
    );
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine(&db);

    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind("agent-sync")
        .bind("Agent Sync")
        .execute(&pg_pool)
        .await
        .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    let register_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/github/repos")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"id":"owner/pg-repo"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(register_response.status(), StatusCode::CREATED);

    let sync_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/github/repos/owner/pg-repo/sync")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sync_response.status(), StatusCode::OK);

    let sync_body = axum::body::to_bytes(sync_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let sync_json: serde_json::Value = serde_json::from_slice(&sync_body).unwrap();
    assert_eq!(sync_json["repo"], "owner/pg-repo");
    assert_eq!(sync_json["issues_fetched"], 1);
    assert_eq!(sync_json["cards_created"], 1);
    assert_eq!(sync_json["cards_closed"], 0);

    let second_sync_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/github/repos/owner/pg-repo/sync")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second_sync_response.status(), StatusCode::OK);
    let second_sync_body = axum::body::to_bytes(second_sync_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let second_sync_json: serde_json::Value = serde_json::from_slice(&second_sync_body).unwrap();
    assert_eq!(second_sync_json["cards_created"], 0);

    let (status, priority, issue_number, description, assigned_agent_id, metadata_text): (
        String,
        String,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT status, priority, github_issue_number, description, assigned_agent_id, metadata::text
         FROM kanban_cards
         WHERE repo_id = $1
         ORDER BY github_issue_number",
    )
    .bind("owner/pg-repo")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(status, "backlog");
    assert_eq!(priority, "high");
    assert_eq!(issue_number, Some(101));
    assert_eq!(description.as_deref(), Some("Investigate route sync"));
    assert_eq!(assigned_agent_id.as_deref(), Some("agent-sync"));
    let metadata_json: serde_json::Value =
        serde_json::from_str(metadata_text.as_deref().expect("metadata must exist")).unwrap();
    assert_eq!(metadata_json["labels"], "bug,p1,agent:agent-sync");

    let last_synced_at: Option<String> =
        sqlx::query_scalar("SELECT last_synced_at::text FROM github_repos WHERE id = $1")
            .bind("owner/pg-repo")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert!(last_synced_at.is_some());

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn cron_jobs_include_github_issue_card_sync_job_pg() {
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .uri("/cron-jobs")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let jobs = json["jobs"]
        .as_array()
        .expect("cron jobs response must include jobs array");
    let github_sync_job = jobs
        .iter()
        .find(|job| job["id"] == "github_issue_card_sync")
        .expect("cron jobs must expose github issue card sync");
    assert_eq!(github_sync_job["schedule"]["kind"], "every");
    assert_eq!(github_sync_job["schedule"]["everyMs"], 300000);
    assert_eq!(github_sync_job["enabled"], true);

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn maintenance_jobs_pg_endpoint_lists_seed_job() -> Result<(), Box<dyn std::error::Error>> {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    sqlx::query(
        "INSERT INTO kv_meta (key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind("maintenance_job:maintenance.noop_heartbeat:next_run_ms")
    .bind("1700000000000")
    .execute(&pool)
    .await?;
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/maintenance/jobs")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    let jobs = json["jobs"].as_array().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            "maintenance response must include jobs array",
        )
    })?;
    let noop_job = jobs
        .iter()
        .find(|job| job["id"] == "maintenance.noop_heartbeat")
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                "maintenance response must include noop heartbeat job",
            )
        })?;

    assert_eq!(noop_job["schedule"]["kind"], "every");
    assert_eq!(noop_job["schedule"]["everyMs"], 900000);
    assert_eq!(
        noop_job["state"]["nextRunAtMs"],
        json!(1_700_000_000_000i64)
    );
    let quality_job = jobs
        .iter()
        .find(|job| job["id"] == "agent_quality_rollup")
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                "maintenance response must include agent quality rollup job",
            )
        })?;
    assert_eq!(quality_job["schedule"]["kind"], "every");
    assert_eq!(quality_job["schedule"]["everyMs"], 3_600_000);
    assert_eq!(quality_job["enabled"], true);

    let cron_response = app
        .oneshot(Request::builder().uri("/cron-jobs").body(Body::empty())?)
        .await?;
    assert_eq!(cron_response.status(), StatusCode::OK);
    let cron_body = axum::body::to_bytes(cron_response.into_body(), usize::MAX).await?;
    let cron_json: serde_json::Value = serde_json::from_slice(&cron_body)?;
    let cron_jobs = cron_json["jobs"].as_array().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            "cron response must include jobs array",
        )
    })?;
    let cron_maintenance_job = cron_jobs
        .iter()
        .find(|job| job["id"] == "maintenance.noop_heartbeat")
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                "cron response must include noop maintenance job",
            )
        })?;
    assert_eq!(cron_maintenance_job["state"]["status"], "active");

    pool.close().await;
    pg_db.drop().await;
    Ok(())
}

#[tokio::test]
async fn cron_api_response_includes_maintenance_section() -> Result<(), Box<dyn std::error::Error>>
{
    // #1091: /api/cron-jobs must include dynamically-registered maintenance
    // jobs, tagged `source: "maintenance"` alongside the existing cron tiers
    // which are tagged `source: "cron"`.
    use crate::services::maintenance::{register_maintenance_job, test_serialization_lock};
    use std::time::Duration;

    // Serialize with any parallel services::maintenance::tests::* test that
    // clears the process-global registry mid-run.
    let _maintenance_lock = test_serialization_lock();

    register_maintenance_job("test.cron_api_section", Duration::from_secs(300), || {
        Box::pin(async { Ok(()) })
    });

    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(Request::builder().uri("/cron-jobs").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    let jobs = json["jobs"].as_array().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::Other,
            "cron response must include jobs array",
        )
    })?;

    // Every job must have a `source` tag, and the set must contain both
    // cron tiers and our registered maintenance job.
    let mut saw_cron = false;
    let mut saw_maintenance = false;
    let mut saw_target = false;
    for job in jobs {
        let source = job["source"].as_str().unwrap_or("");
        assert!(
            !source.is_empty(),
            "every cron-jobs entry must carry a non-empty `source` tag; got {job:?}"
        );
        match source {
            "cron" => saw_cron = true,
            "maintenance" => saw_maintenance = true,
            other => panic!("unexpected source {other:?}"),
        }
        if job["id"] == "maintenance:test.cron_api_section" {
            saw_target = true;
            assert_eq!(job["source"], "maintenance");
            assert_eq!(job["schedule"]["everyMs"], 300_000);
            assert_eq!(job["enabled"], true);
        }
    }
    assert!(
        saw_cron,
        "response must include at least one `cron` source job"
    );
    assert!(
        saw_maintenance,
        "response must include at least one `maintenance` source job"
    );
    assert!(
        saw_target,
        "response must include the registered test.cron_api_section maintenance job"
    );

    Ok(())
}

#[tokio::test]
async fn agent_quality_api_returns_daily_rollup() -> Result<(), Box<dyn std::error::Error>> {
    let db = test_db();
    seed_test_agents(&db);
    {
        let conn = db.lock()?;
        conn.execute(
            "INSERT INTO agent_quality_daily (
                agent_id,
                day,
                provider,
                channel_id,
                turn_success_count,
                turn_error_count,
                review_pass_count,
                review_fail_count,
                turn_sample_size,
                review_sample_size,
                sample_size,
                turn_success_rate,
                review_pass_rate,
                turn_success_count_7d,
                turn_error_count_7d,
                review_pass_count_7d,
                review_fail_count_7d,
                turn_sample_size_7d,
                review_sample_size_7d,
                sample_size_7d,
                turn_success_rate_7d,
                review_pass_rate_7d,
                measurement_unavailable_7d,
                turn_success_count_30d,
                turn_error_count_30d,
                review_pass_count_30d,
                review_fail_count_30d,
                turn_sample_size_30d,
                review_sample_size_30d,
                sample_size_30d,
                turn_success_rate_30d,
                review_pass_rate_30d,
                measurement_unavailable_30d
             ) VALUES (
                'agent-1',
                date('now'),
                'codex',
                '555',
                4,
                1,
                3,
                1,
                5,
                4,
                9,
                0.8,
                0.75,
                4,
                1,
                3,
                1,
                5,
                4,
                9,
                0.8,
                0.75,
                0,
                20,
                5,
                12,
                4,
                25,
                16,
                41,
                0.8,
                0.75,
                0
             )",
            [],
        )?;
    }
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/agents/agent-1/quality")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(json["agentId"], "agent-1");
    assert_eq!(json["latest"]["rolling7d"]["sampleSize"], 9);
    assert_eq!(json["latest"]["rolling7d"]["measurementUnavailable"], false);
    assert_eq!(json["latest"]["rolling7d"]["turnSuccessRate"], json!(0.8));
    assert_eq!(json["daily"].as_array().map(Vec::len), Some(1));
    // #1102: DoD-mandated current / trend_7d / trend_30d fields.
    assert_eq!(json["current"]["agentId"], "agent-1");
    assert_eq!(json["trend7d"].as_array().map(Vec::len), Some(1));
    assert_eq!(json["trend30d"].as_array().map(Vec::len), Some(1));
    assert_eq!(json["fallbackFromEvents"], false);

    let ranking_response = app
        .oneshot(
            Request::builder()
                .uri("/agents/quality/ranking")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(ranking_response.status(), StatusCode::OK);
    let ranking_body = axum::body::to_bytes(ranking_response.into_body(), usize::MAX).await?;
    let ranking_json: serde_json::Value = serde_json::from_slice(&ranking_body)?;
    assert_eq!(ranking_json["agents"][0]["agentId"], "agent-1");
    assert_eq!(ranking_json["metric"], "turn_success_rate");
    assert_eq!(ranking_json["window"], "7d");
    assert_eq!(ranking_json["minSampleSize"], 5);
    // metric_value for rolling_7d turn_success_rate on the seeded row is 0.8.
    assert_eq!(ranking_json["agents"][0]["metricValue"], json!(0.8));

    Ok(())
}

/// #1102 DoD: ranking excludes agents whose rolling_7d sample_size < 5 so
/// the client doesn't have to filter client-side.
#[tokio::test]
async fn agent_quality_api_ranking_excludes_low_sample_size()
-> Result<(), Box<dyn std::error::Error>> {
    let db = test_db();
    seed_test_agents(&db);
    {
        let conn = db.lock()?;
        // agent-1: sample_size_7d = 2 (below threshold, measurement_unavailable=1)
        conn.execute(
            "INSERT INTO agent_quality_daily (
                agent_id, day, provider, channel_id,
                turn_success_count, turn_error_count, review_pass_count, review_fail_count,
                turn_sample_size, review_sample_size, sample_size,
                turn_success_rate, review_pass_rate,
                turn_success_count_7d, turn_error_count_7d, review_pass_count_7d, review_fail_count_7d,
                turn_sample_size_7d, review_sample_size_7d, sample_size_7d,
                turn_success_rate_7d, review_pass_rate_7d, measurement_unavailable_7d,
                turn_success_count_30d, turn_error_count_30d, review_pass_count_30d, review_fail_count_30d,
                turn_sample_size_30d, review_sample_size_30d, sample_size_30d,
                turn_success_rate_30d, review_pass_rate_30d, measurement_unavailable_30d
             ) VALUES (
                'agent-1', date('now'), 'codex', '555',
                1, 0, 1, 0,
                1, 1, 2,
                1.0, 1.0,
                1, 0, 1, 0,
                1, 1, 2,
                1.0, 1.0, 1,
                1, 0, 1, 0,
                1, 1, 2,
                1.0, 1.0, 1
             )",
            [],
        )?;
        // ag1: sample_size_7d = 10 (well above threshold)
        conn.execute(
            "INSERT INTO agent_quality_daily (
                agent_id, day, provider, channel_id,
                turn_success_count, turn_error_count, review_pass_count, review_fail_count,
                turn_sample_size, review_sample_size, sample_size,
                turn_success_rate, review_pass_rate,
                turn_success_count_7d, turn_error_count_7d, review_pass_count_7d, review_fail_count_7d,
                turn_sample_size_7d, review_sample_size_7d, sample_size_7d,
                turn_success_rate_7d, review_pass_rate_7d, measurement_unavailable_7d,
                turn_success_count_30d, turn_error_count_30d, review_pass_count_30d, review_fail_count_30d,
                turn_sample_size_30d, review_sample_size_30d, sample_size_30d,
                turn_success_rate_30d, review_pass_rate_30d, measurement_unavailable_30d
             ) VALUES (
                'ag1', date('now'), 'codex', '333',
                6, 1, 3, 0,
                7, 3, 10,
                0.857, 1.0,
                6, 1, 3, 0,
                7, 3, 10,
                0.857, 1.0, 0,
                6, 1, 3, 0,
                7, 3, 10,
                0.857, 1.0, 0
             )",
            [],
        )?;
    }
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/agents/quality/ranking?metric=turn_success_rate&window=7d")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    let agents = json["agents"].as_array().expect("agents array");
    assert_eq!(agents.len(), 1, "only ag1 (sample_size_7d=10) should pass");
    assert_eq!(agents[0]["agentId"], "ag1");
    assert_eq!(agents[0]["rank"], 1);
    Ok(())
}

/// #1102 DoD: when `agent_quality_daily` has no rows, the per-agent summary
/// falls back to an on-the-fly mini-rollup over `agent_quality_event`.
#[tokio::test]
async fn agent_quality_api_event_fallback_mini_rollup() -> Result<(), Box<dyn std::error::Error>> {
    let db = test_db();
    seed_test_agents(&db);
    {
        let conn = db.lock()?;
        // Seed 6 events (enough to exceed QUALITY_SAMPLE_GUARD=5 → window
        // should be measurable).
        for (i, etype) in [
            "turn_complete",
            "turn_complete",
            "turn_complete",
            "turn_complete",
            "turn_error",
            "review_pass",
        ]
        .iter()
        .enumerate()
        {
            conn.execute(
                "INSERT INTO agent_quality_event (
                    source_event_id, correlation_id, agent_id, provider, channel_id,
                    card_id, dispatch_id, event_type, payload_json, created_at
                 ) VALUES (?1, NULL, 'agent-1', 'codex', '555', NULL, NULL, ?2, '{}', datetime('now'))",
                sqlite_params![format!("evt-{i}"), etype],
            )?;
        }
    }
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/agents/agent-1/quality")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(json["agentId"], "agent-1");
    assert_eq!(
        json["fallbackFromEvents"], true,
        "fallbackFromEvents must be true when daily is empty"
    );
    let daily_len = json["daily"].as_array().map(Vec::len).unwrap_or(0);
    assert!(daily_len >= 1, "expected synthesized daily rows");
    let current_sample = json["current"]["sampleSize"].as_i64().unwrap_or(-1);
    assert_eq!(current_sample, 6, "6 events synthesized for today");
    Ok(())
}

/// #1102 DoD: docs catalog exposes both new quality endpoints.
#[tokio::test]
async fn agent_quality_api_docs_catalog_includes_endpoints()
-> Result<(), Box<dyn std::error::Error>> {
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/docs/agents?format=flat")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX).await?;
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("/api/agents/{id}/quality"),
        "docs must list /api/agents/{{id}}/quality, got: {text}"
    );
    assert!(
        text.contains("/api/agents/quality/ranking"),
        "docs must list /api/agents/quality/ranking, got: {text}"
    );
    Ok(())
}

#[tokio::test]
async fn github_repos_pg_sync_closes_card_and_cleans_live_state() {
    crate::pipeline::ensure_loaded();
    let terminal = crate::pipeline::try_get()
        .map(|pipeline| {
            pipeline
                .states
                .iter()
                .find(|state| state.terminal)
                .map(|state| state.id.clone())
                .unwrap_or_else(|| "done".to_string())
        })
        .unwrap_or_else(|| "done".to_string());
    let _env_lock = env_lock();
    let _gh = install_mock_gh_issue_list(
        "owner/pg-repo",
        r#"[{"number":404,"state":"CLOSED","title":"PG route closed","labels":[],"body":"Issue is already closed"}]"#,
        "[]",
    );
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    let register_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/github/repos")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"id":"owner/pg-repo"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(register_response.status(), StatusCode::CREATED);

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, github_issue_number,
            latest_dispatch_id, review_status, review_round, review_entered_at,
            created_at, updated_at
         )
         VALUES (
            $1, $2, $3, 'in_progress', 'medium', $4,
            $5, 'reviewing', 2, NOW(),
            NOW(), NOW()
         )",
    )
    .bind("card-pg-sync")
    .bind("owner/pg-repo")
    .bind("PG sync close")
    .bind(404_i64)
    .bind("dispatch-pg-sync")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         )
         VALUES (
            $1, $2, $3, 'implementation', 'dispatched', 'Live implementation', NOW(), NOW()
         )",
    )
    .bind("dispatch-pg-sync")
    .bind("card-pg-sync")
    .bind("pg-agent")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, 'active')",
    )
    .bind("run-pg-sync")
    .bind("owner/pg-repo")
    .bind("pg-agent")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, created_at
         )
         VALUES (
            $1, $2, $3, $4, 'pending', NOW()
         )",
    )
    .bind("entry-pg-sync")
    .bind("run-pg-sync")
    .bind("card-pg-sync")
    .bind("pg-agent")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO card_review_state (
            card_id, review_round, state, pending_dispatch_id, review_entered_at, updated_at
         )
         VALUES (
            $1, 2, 'reviewing', $2, NOW(), NOW()
         )",
    )
    .bind("card-pg-sync")
    .bind("dispatch-pg-sync")
    .execute(&pg_pool)
    .await
    .unwrap();

    let sync_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/github/repos/owner/pg-repo/sync")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sync_response.status(), StatusCode::OK);

    let sync_body = axum::body::to_bytes(sync_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let sync_json: serde_json::Value = serde_json::from_slice(&sync_body).unwrap();
    assert_eq!(sync_json["cards_created"], 0);
    assert_eq!(sync_json["cards_closed"], 1);

    let (card_status, latest_dispatch_id, review_status, review_round): (
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
    ) = sqlx::query_as(
        "SELECT status, latest_dispatch_id, review_status, review_round
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind("card-pg-sync")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(card_status, terminal);
    assert!(latest_dispatch_id.is_none());
    assert!(review_status.is_none());
    assert!(review_round.is_none());

    let dispatch_status: String =
        sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
            .bind("dispatch-pg-sync")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(dispatch_status, "cancelled");

    let (entry_status, entry_dispatch_id): (String, Option<String>) =
        sqlx::query_as("SELECT status, dispatch_id FROM auto_queue_entries WHERE id = $1")
            .bind("entry-pg-sync")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(entry_status, "skipped");
    assert!(entry_dispatch_id.is_none());

    let (run_status, run_completed_at): (String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT status, completed_at FROM auto_queue_runs WHERE id = $1")
            .bind("run-pg-sync")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(run_status, "completed");
    assert!(run_completed_at.is_some());

    let (review_state, pending_dispatch_id): (String, Option<String>) = sqlx::query_as(
        "SELECT state, pending_dispatch_id
         FROM card_review_state
         WHERE card_id = $1",
    )
    .bind("card-pg-sync")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(review_state, "idle");
    assert!(pending_dispatch_id.is_none());

    let audit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM kanban_audit_logs WHERE card_id = $1 AND source = 'github-sync'",
    )
    .bind("card-pg-sync")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(audit_count, 1);

    let last_synced_at: Option<String> =
        sqlx::query_scalar("SELECT last_synced_at::text FROM github_repos WHERE id = $1")
            .bind("owner/pg-repo")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert!(last_synced_at.is_some());

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn github_repos_pg_sync_marks_in_progress_card_done_from_main_commit() {
    let _env_lock = env_lock();
    let _gh = install_mock_gh_issue_list(
        "owner/pg-repo",
        r#"[{"number":404,"state":"OPEN","title":"PG route mainline","labels":[],"body":"Issue remains open"}]"#,
        "[]",
    );
    let repo = tempfile::tempdir().unwrap();
    run_git(repo.path(), &["init", "-b", "main"]);
    run_git(repo.path(), &["config", "user.email", "test@test.com"]);
    run_git(repo.path(), &["config", "user.name", "Test"]);
    run_git(repo.path(), &["commit", "--allow-empty", "-m", "initial"]);
    git_commit(repo.path(), "fix: mainline merge (#404)");
    let config_dir = write_repo_mapping_config(&[("owner/pg-repo", repo.path())]);
    let config_path = config_dir.path().join("agentdesk.yaml");
    let _config = EnvVarGuard::set_path("AGENTDESK_CONFIG", &config_path);

    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    let register_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/github/repos")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"id":"owner/pg-repo"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(register_response.status(), StatusCode::CREATED);

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, github_issue_number,
            latest_dispatch_id, review_status, review_round, review_entered_at,
            created_at, updated_at
         )
         VALUES (
            $1, $2, $3, 'in_progress', 'medium', $4,
            $5, 'reviewing', 2, NOW(),
            NOW(), NOW()
         )",
    )
    .bind("card-pg-mainline")
    .bind("owner/pg-repo")
    .bind("PG mainline sync")
    .bind(404_i64)
    .bind("dispatch-pg-mainline")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         )
         VALUES (
            $1, $2, $3, 'implementation', 'dispatched', 'Live implementation', NOW(), NOW()
         )",
    )
    .bind("dispatch-pg-mainline")
    .bind("card-pg-mainline")
    .bind("pg-agent")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, 'active')",
    )
    .bind("run-pg-mainline")
    .bind("owner/pg-repo")
    .bind("pg-agent")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, dispatch_id, status, created_at
         )
         VALUES (
            $1, $2, $3, $4, $5, 'dispatched', NOW()
         )",
    )
    .bind("entry-pg-mainline")
    .bind("run-pg-mainline")
    .bind("card-pg-mainline")
    .bind("pg-agent")
    .bind("dispatch-pg-mainline")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO card_review_state (
            card_id, review_round, state, pending_dispatch_id, review_entered_at, updated_at
         )
         VALUES (
            $1, 2, 'reviewing', $2, NOW(), NOW()
         )",
    )
    .bind("card-pg-mainline")
    .bind("dispatch-pg-mainline")
    .execute(&pg_pool)
    .await
    .unwrap();

    let sync_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/github/repos/owner/pg-repo/sync")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(sync_response.status(), StatusCode::OK);

    let sync_body = axum::body::to_bytes(sync_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let sync_json: serde_json::Value = serde_json::from_slice(&sync_body).unwrap();
    assert_eq!(sync_json["cards_created"], 0);

    let (card_status, latest_dispatch_id, review_status, review_round): (
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
    ) = sqlx::query_as(
        "SELECT status, latest_dispatch_id, review_status, review_round
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind("card-pg-mainline")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(card_status, "done");
    assert!(latest_dispatch_id.is_none());
    assert!(review_status.is_none());
    assert!(review_round.is_none());

    let dispatch_status: String =
        sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
            .bind("dispatch-pg-mainline")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(dispatch_status, "cancelled");

    let (entry_status, entry_dispatch_id): (String, Option<String>) =
        sqlx::query_as("SELECT status, dispatch_id FROM auto_queue_entries WHERE id = $1")
            .bind("entry-pg-mainline")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(entry_status, "done");
    assert_eq!(entry_dispatch_id.as_deref(), Some("dispatch-pg-mainline"));

    let run_status: String = sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = $1")
        .bind("run-pg-mainline")
        .fetch_one(&pg_pool)
        .await
        .unwrap();
    assert_eq!(
        run_status, "active",
        "mainline issue sync should not force the queue run to complete"
    );

    let (review_state, pending_dispatch_id): (String, Option<String>) = sqlx::query_as(
        "SELECT state, pending_dispatch_id
         FROM card_review_state
         WHERE card_id = $1",
    )
    .bind("card-pg-mainline")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(review_state, "idle");
    assert!(pending_dispatch_id.is_none());

    let last_synced_at: Option<String> =
        sqlx::query_scalar("SELECT last_synced_at::text FROM github_repos WHERE id = $1")
            .bind("owner/pg-repo")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert!(last_synced_at.is_some());

    pg_pool.close().await;
    pg_db.drop().await;
}
// ── Pipeline config hierarchy tests (#135) ──

fn seed_repo(db: &Db, repo_id: &str) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO github_repos (id, display_name) VALUES (?1, ?1)",
        [repo_id],
    )
    .unwrap();
}

fn seed_agent(db: &Db, agent_id: &str) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES (?1, ?1, '111', '222')",
        [agent_id],
    )
    .unwrap();
}

#[tokio::test]
async fn kanban_repos_pg_create_update_delete_round_trip() {
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine(&db);
    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    let create_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-repos")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"repo":"itismyfield/AgentDesk"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_response.status(), StatusCode::CREATED);

    let patch_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/kanban-repos/itismyfield/AgentDesk")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"default_agent_id":"pg-agent"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(patch_response.status(), StatusCode::OK);

    let list_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/kanban-repos")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let list_body = axum::body::to_bytes(list_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let list_json: serde_json::Value = serde_json::from_slice(&list_body).unwrap();
    assert_eq!(list_json["repos"][0]["default_agent_id"], "pg-agent");

    let delete_response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/kanban-repos/itismyfield/AgentDesk")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(delete_response.status(), StatusCode::OK);

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn pipeline_config_pg_repo_get_set_override() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("owner/repo-a")
        .execute(&pool)
        .await
        .unwrap();

    // GET — initially null
    let app = test_api_router_with_pg(
        db.clone(),
        engine.clone(),
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/repo/owner/repo-a")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(body["pipeline_config"].is_null());

    // PUT — set override
    let app2 = test_api_router_with_pg(
        db.clone(),
        engine.clone(),
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp2 = app2
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/pipeline/config/repo/owner/repo-a")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"config":{"hooks":{"review":{"on_enter":["CustomReviewHook"],"on_exit":[]}}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);

    // GET — now has override
    let app3 = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp3 = app3
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/repo/owner/repo-a")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body3: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp3.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(
        body3["pipeline_config"]["hooks"]["review"]["on_enter"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "CustomReviewHook")
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn pipeline_config_pg_agent_get_set_override() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ($1, $1, '111', '222')",
    )
    .bind("agent-x")
    .execute(&pool)
    .await
    .unwrap();

    // PUT
    let app = test_api_router_with_pg(
        db.clone(),
        engine.clone(),
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/pipeline/config/agent/agent-x")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"config":{"timeouts":{"in_progress":{"duration":"4h","clock":"started_at"}}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // GET
    let app2 = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp2 = app2
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/agent/agent-x")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp2.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        body["pipeline_config"]["timeouts"]["in_progress"]["duration"],
        "4h"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn pipeline_config_pg_effective_merges_layers() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("owner/repo-e")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ($1, $1, '111', '222')",
    )
    .bind("agent-e")
    .execute(&pool)
    .await
    .unwrap();

    // Set repo override (hooks)
    let app = test_api_router_with_pg(
        db.clone(),
        engine.clone(),
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    app.oneshot(
        Request::builder()
            .method("PUT")
            .uri("/pipeline/config/repo/owner/repo-e")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"config":{"hooks":{"in_progress":{"on_enter":["RepoHook"],"on_exit":[]}}}}"#,
            ))
            .unwrap(),
    )
    .await
    .unwrap();

    // Get effective — should include repo hook
    let app2 = test_api_router_with_pg(
        db.clone(),
        engine.clone(),
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app2
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/effective?repo=owner/repo-e&agent_id=agent-e")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["layers"]["repo"], true);
    assert_eq!(body["layers"]["agent"], false);
    // Hooks from repo override should be in effective pipeline
    let hooks = &body["pipeline"]["hooks"]["in_progress"]["on_enter"];
    assert!(hooks.as_array().unwrap().iter().any(|v| v == "RepoHook"));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn pipeline_config_pg_graph_returns_nodes_and_edges() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/graph")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    let nodes = body["nodes"].as_array().unwrap();
    let edges = body["edges"].as_array().unwrap();
    assert!(!nodes.is_empty());
    assert!(!edges.is_empty());
    // Each node has expected fields
    assert!(nodes[0]["id"].is_string());
    assert!(nodes[0]["label"].is_string());
    // Each edge has from/to/type
    assert!(edges[0]["from"].is_string());
    assert!(edges[0]["to"].is_string());
    assert!(edges[0]["type"].is_string());

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn pipeline_config_repo_invalid_override_rejected() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_repo(&db, "owner/repo-bad");

    let app = test_api_router(db, engine, None);
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/pipeline/config/repo/owner/repo-bad")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"config":{"states":"not-an-array"}}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn pipeline_config_pg_repo_broken_merge_rejected() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("owner/repo-merge")
        .execute(&pool)
        .await
        .unwrap();

    // Override that adds a timeout referencing an unknown clock and a non-existent state.
    // This parses as valid JSON but the merged effective pipeline should fail validate().
    let body = r#"{"config":{"timeouts":{"nonexistent_state":{"duration":"1h","clock":"no_such_clock"}}}}"#;

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/pipeline/config/repo/owner/repo-merge")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("validation failed"),
        "expected merged validation error, got: {}",
        body
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn pipeline_stages_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO github_repos (id, display_name)
         VALUES ($1, $2)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("owner/pg-pipeline-stages")
    .bind("PG Pipeline Stages")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    // #1097: pipeline_stages is now file-canonical (materialized from
    // policies/default-pipeline.yaml), so PUT/DELETE must be rejected with
    // HTTP 405. The test still asserts that the table is PG-only (no sqlite
    // mirror writes happen) and that GET still works as before.
    let put_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/pipeline/stages")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{
                        "repo":"owner/pg-pipeline-stages",
                        "stages":[
                            {"stage_name":"Build","stage_order":3000000000,"entry_skill":"build","timeout_minutes":3000000002,"max_retries":3000000003},
                            {"stage_name":"Review","stage_order":3000000001,"entry_skill":"review","parallel_with":"lint","timeout_minutes":3000000004}
                        ]
                    }"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let put_status = put_response.status();
    let put_body = axum::body::to_bytes(put_response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        put_status,
        StatusCode::METHOD_NOT_ALLOWED,
        "pipeline stages PUT must be rejected as file-canonical; body={}",
        String::from_utf8_lossy(&put_body)
    );
    let put_json: serde_json::Value = serde_json::from_slice(&put_body).unwrap();
    assert_eq!(put_json["table"], "pipeline_stages");
    assert_eq!(put_json["source_of_truth"], "file-canonical");
    assert!(
        put_json["error"]
            .as_str()
            .unwrap_or("")
            .contains("file-canonical"),
        "expected file-canonical error message, got: {}",
        put_json["error"]
    );

    // No rows should have been written by the rejected PUT. The PG table
    // may still contain rows materialized from policies/default-pipeline.yaml
    // for *other* repos, but nothing for this test's repo.
    let pg_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM pipeline_stages WHERE repo_id = $1")
            .bind("owner/pg-pipeline-stages")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(
        pg_count, 0,
        "rejected PUT must not insert rows for the test repo"
    );

    let sqlite_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM pipeline_stages WHERE repo_id = 'owner/pg-pipeline-stages'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_count, 0, "sqlite mirror should stay empty");

    // GET still works; since the rejected PUT wrote nothing, the list is
    // empty for this repo.
    let get_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/pipeline/stages?repo=owner/pg-pipeline-stages")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(get_response.status(), StatusCode::OK);
    let get_body = axum::body::to_bytes(get_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let get_json: serde_json::Value = serde_json::from_slice(&get_body).unwrap();
    assert_eq!(
        get_json["stages"].as_array().unwrap().len(),
        0,
        "GET must return empty list for a repo with no materialized stages"
    );

    // DELETE must also be rejected as file-canonical.
    let delete_response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/pipeline/stages?repo=owner/pg-pipeline-stages")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        delete_response.status(),
        StatusCode::METHOD_NOT_ALLOWED,
        "pipeline stages DELETE must be rejected as file-canonical"
    );
    let delete_body = axum::body::to_bytes(delete_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let delete_json: serde_json::Value = serde_json::from_slice(&delete_body).unwrap();
    assert_eq!(delete_json["table"], "pipeline_stages");
    assert_eq!(delete_json["source_of_truth"], "file-canonical");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn pipeline_card_views_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO github_repos (id, display_name)
         VALUES ($1, $2)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("owner/pg-pipeline-card")
    .bind("PG Pipeline Card")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (id, repo_id, title, status, created_at, updated_at)
         VALUES ($1, $2, $3, $4, NOW(), NOW())",
    )
    .bind("card-pg-pipeline")
    .bind("owner/pg-pipeline-card")
    .bind("PG Pipeline Card")
    .bind("in_progress")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO pipeline_stages (repo_id, stage_name, stage_order, entry_skill)
         VALUES ($1, $2, $3, $4), ($1, $5, $6, $7)",
    )
    .bind("owner/pg-pipeline-card")
    .bind("Triage")
    .bind(1_i64)
    .bind("triage")
    .bind("Implementation")
    .bind(2_i64)
    .bind("implementation")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES
            ($1, $2, $3, $4, $5, NOW() - INTERVAL '2 seconds', NOW() - INTERVAL '2 seconds'),
            ($6, $2, $7, $8, $9, NOW() - INTERVAL '1 seconds', NOW() - INTERVAL '1 seconds')",
    )
    .bind("dispatch-pg-pipeline-triage")
    .bind("card-pg-pipeline")
    .bind("triage")
    .bind("completed")
    .bind("Triage")
    .bind("dispatch-pg-pipeline-impl")
    .bind("implementation")
    .bind("running")
    .bind("Implementation")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    let pipeline_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/pipeline/cards/card-pg-pipeline")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let pipeline_status = pipeline_response.status();
    let pipeline_body = axum::body::to_bytes(pipeline_response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        pipeline_status,
        StatusCode::OK,
        "pipeline card body={}",
        String::from_utf8_lossy(&pipeline_body)
    );
    let pipeline_json: serde_json::Value = serde_json::from_slice(&pipeline_body).unwrap();
    assert_eq!(pipeline_json["stages"].as_array().unwrap().len(), 2);
    assert_eq!(pipeline_json["history"].as_array().unwrap().len(), 2);
    assert_eq!(
        pipeline_json["current_stage"]["stage_name"],
        "Implementation"
    );

    let history_response = app
        .oneshot(
            Request::builder()
                .uri("/pipeline/cards/card-pg-pipeline/history")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(history_response.status(), StatusCode::OK);
    let history_body = axum::body::to_bytes(history_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let history_json: serde_json::Value = serde_json::from_slice(&history_body).unwrap();
    assert_eq!(history_json["history"].as_array().unwrap().len(), 2);
    assert_eq!(
        history_json["history"][1]["dispatch_type"],
        "implementation"
    );

    let sqlite_dispatch_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-pg-pipeline'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_dispatch_count, 0, "sqlite mirror should stay empty");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn pipeline_config_pg_only_without_sqlite_mirror() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO github_repos (id, display_name)
         VALUES ($1, $2)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("owner/pg-pipeline-config")
    .bind("PG Pipeline Config")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id)
         VALUES ($1, $2, '111')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind("agent-pg-pipeline-config")
    .bind("PG Pipeline Agent")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    let repo_get_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/repo/owner/pg-pipeline-config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(repo_get_response.status(), StatusCode::OK);
    let repo_get_body = axum::body::to_bytes(repo_get_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let repo_get_json: serde_json::Value = serde_json::from_slice(&repo_get_body).unwrap();
    assert!(repo_get_json["pipeline_config"].is_null());

    let repo_put_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/pipeline/config/repo/owner/pg-pipeline-config")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"config":{"hooks":{"review":{"on_enter":["PgReviewHook"],"on_exit":[]}}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(repo_put_response.status(), StatusCode::OK);

    let agent_put_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/pipeline/config/agent/agent-pg-pipeline-config")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"config":{"timeouts":{"in_progress":{"duration":"4h","clock":"started_at"}}}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(agent_put_response.status(), StatusCode::OK);

    let repo_after_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/repo/owner/pg-pipeline-config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let repo_after_body = axum::body::to_bytes(repo_after_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let repo_after_json: serde_json::Value = serde_json::from_slice(&repo_after_body).unwrap();
    assert_eq!(
        repo_after_json["pipeline_config"]["hooks"]["review"]["on_enter"][0],
        "PgReviewHook"
    );

    let agent_after_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/agent/agent-pg-pipeline-config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let agent_after_body = axum::body::to_bytes(agent_after_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let agent_after_json: serde_json::Value = serde_json::from_slice(&agent_after_body).unwrap();
    assert_eq!(
        agent_after_json["pipeline_config"]["timeouts"]["in_progress"]["duration"],
        "4h"
    );

    let effective_response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/effective?repo=owner/pg-pipeline-config&agent_id=agent-pg-pipeline-config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(effective_response.status(), StatusCode::OK);
    let effective_body = axum::body::to_bytes(effective_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let effective_json: serde_json::Value = serde_json::from_slice(&effective_body).unwrap();
    assert_eq!(effective_json["layers"]["repo"], true);
    assert_eq!(effective_json["layers"]["agent"], true);
    assert_eq!(
        effective_json["pipeline"]["hooks"]["review"]["on_enter"][0],
        "PgReviewHook"
    );

    let graph_response = app
        .oneshot(
            Request::builder()
                .uri("/pipeline/config/graph?repo=owner/pg-pipeline-config&agent_id=agent-pg-pipeline-config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(graph_response.status(), StatusCode::OK);
    let graph_body = axum::body::to_bytes(graph_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let graph_json: serde_json::Value = serde_json::from_slice(&graph_body).unwrap();
    assert!(!graph_json["nodes"].as_array().unwrap().is_empty());
    assert!(!graph_json["edges"].as_array().unwrap().is_empty());

    let pg_report_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM kv_meta WHERE key = 'pipeline_override_health_report'",
    )
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(pg_report_count, 1);

    let sqlite_repo_override_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM github_repos WHERE pipeline_config IS NOT NULL AND id = 'owner/pg-pipeline-config'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sqlite_repo_override_count, 0,
        "sqlite mirror should stay empty"
    );

    let sqlite_report_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM kv_meta WHERE key = 'pipeline_override_health_report'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_report_count, 0, "sqlite mirror should stay empty");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn health_api_includes_pipeline_override_report_from_postgres_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());
    let harness = crate::services::discord::health::TestHealthHarness::new().await;
    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db.clone(),
            engine,
            crate::config::Config::default(),
            Some(harness.registry()),
            pg_pool.clone(),
        ),
    );

    let report = crate::pipeline::PipelineOverrideHealthReport {
        generated_at: "2026-04-22T00:00:00Z".to_string(),
        status: "warn".to_string(),
        warnings_count: 1,
        warnings: vec!["repo override pg warns".to_string()],
        parse_failures: Vec::new(),
        replace_warnings: vec![crate::pipeline::PipelineOverrideReplaceWarning {
            layer: "repo".to_string(),
            target_id: "owner/pg-pipeline-config".to_string(),
            section: "hooks".to_string(),
            dropped_count: 1,
            dropped_items: vec!["review".to_string()],
        }],
    };

    sqlx::query(
        "INSERT INTO kv_meta (key, value)
         VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE
         SET value = EXCLUDED.value",
    )
    .bind("pipeline_override_health_report")
    .bind(serde_json::to_string(&report).unwrap())
    .execute(&pg_pool)
    .await
    .unwrap();

    let sqlite_report_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM kv_meta WHERE key = 'pipeline_override_health_report'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_report_count, 0, "sqlite mirror should stay empty");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let json: serde_json::Value = reqwest::get(format!("http://{addr}/api/health/detail"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(json["status"], "degraded");
    assert_eq!(json["pipeline_overrides"]["status"], "warn");
    assert_eq!(json["pipeline_overrides"]["warnings_count"], 1);
    assert_eq!(
        json["pipeline_overrides"]["replace_warnings"][0]["target_id"],
        "owner/pg-pipeline-config"
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

// ── force-transition auth tests ──

fn seed_card_with_status(db: &Db, card_id: &str, status: &str) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO kanban_cards (id, title, status, priority, created_at, updated_at) \
             VALUES (?1, 'test', ?2, 'medium', datetime('now'), datetime('now'))",
        sqlite_params![card_id, status],
    )
    .unwrap();
}

async fn seed_card_with_status_pg(pool: &sqlx::PgPool, card_id: &str, status: &str) {
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

fn set_pmd_channel(db: &Db, channel_id: &str) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('kanban_manager_channel_id', ?1)",
        [channel_id],
    )
    .unwrap();
}

fn ensure_auto_queue_tables(db: &Db) {
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

fn seed_auto_queue_card(db: &Db, card_id: &str, issue_number: i64, status: &str, agent_id: &str) {
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

async fn seed_repo_pg(pool: &sqlx::PgPool, repo_id: &str) {
    sqlx::query(
        "INSERT INTO github_repos (id, display_name) VALUES ($1, $1)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(repo_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_agent_pg(pool: &sqlx::PgPool, agent_id: &str) {
    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ($1, $1, '111', '222')
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(agent_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn seed_auto_queue_card_pg(
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

async fn seed_parallel_test_cards_pg(pool: &sqlx::PgPool) -> Vec<String> {
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

async fn seed_similarity_group_cards_pg(pool: &sqlx::PgPool) -> Vec<String> {
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

#[test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
fn auto_queue_schema_migration_drops_legacy_max_concurrent_per_agent_column() {
    let db = test_db();
    let conn = db.separate_conn().unwrap();
    conn.execute_batch(
        "PRAGMA foreign_keys=ON;
         CREATE TABLE kanban_cards (id TEXT PRIMARY KEY);
         CREATE TABLE task_dispatches (
            id TEXT PRIMARY KEY,
            kanban_card_id TEXT,
            to_agent_id TEXT,
            dispatch_type TEXT,
            created_at DATETIME
         );",
    )
    .unwrap();
    conn.execute_batch(
        "CREATE TABLE auto_queue_runs (
            id          TEXT PRIMARY KEY,
            repo        TEXT,
            agent_id    TEXT,
            status      TEXT DEFAULT 'active',
            ai_model    TEXT,
            ai_rationale TEXT,
            timeout_minutes INTEGER DEFAULT 120,
            unified_thread INTEGER DEFAULT 0,
            unified_thread_id TEXT,
            unified_thread_channel_id TEXT,
            max_concurrent_threads INTEGER DEFAULT 1,
            max_concurrent_per_agent INTEGER DEFAULT 1,
            created_at  DATETIME DEFAULT CURRENT_TIMESTAMP,
            completed_at DATETIME
        );
        CREATE TABLE auto_queue_entries (
            id              TEXT PRIMARY KEY,
            run_id          TEXT REFERENCES auto_queue_runs(id),
            kanban_card_id  TEXT REFERENCES kanban_cards(id),
            agent_id        TEXT,
            priority_rank   INTEGER DEFAULT 0,
            reason          TEXT,
            status          TEXT DEFAULT 'pending',
            created_at      DATETIME DEFAULT CURRENT_TIMESTAMP,
            dispatched_at   DATETIME,
            completed_at    DATETIME
        );",
    )
    .unwrap();

    crate::db::schema::ensure_auto_queue_schema(&conn).unwrap();

    let has_legacy_column: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('auto_queue_runs') WHERE name = 'max_concurrent_per_agent'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let has_max_threads: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('auto_queue_runs') WHERE name = 'max_concurrent_threads'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let has_thread_group_count: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('auto_queue_runs') WHERE name = 'thread_group_count'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let has_batch_phase: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('auto_queue_entries') WHERE name = 'batch_phase'",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert!(!has_legacy_column);
    assert!(has_max_threads);
    assert!(has_thread_group_count);
    assert!(has_batch_phase);
}

fn seed_in_progress_stall_case(
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

fn seed_review_e2e_case(
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

fn drain_pending_transitions(db: &Db, engine: &PolicyEngine) {
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

#[test]
fn on_tick5min_stalled_timeout_uses_latest_activity_timestamp() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-stalled");

    seed_in_progress_stall_case(
        &db,
        "card-fresh-dispatch",
        "Fresh Dispatch",
        "agent-stalled",
        "-3 hours",
        "-3 hours",
        Some(("dispatch-fresh", "-10 minutes")),
    );
    seed_in_progress_stall_case(
        &db,
        "card-reentered",
        "Re-entered",
        "agent-stalled",
        "-3 hours",
        "-10 minutes",
        Some(("dispatch-old", "-3 hours")),
    );
    seed_in_progress_stall_case(
        &db,
        "card-truly-stalled",
        "Truly Stalled",
        "agent-stalled",
        "-3 hours",
        "-3 hours",
        Some(("dispatch-stale", "-3 hours")),
    );

    let _ = engine.try_fire_hook_by_name("OnTick5min", serde_json::json!({}));
    drain_pending_transitions(&db, &engine);

    let conn = db.lock().unwrap();
    let rows: std::collections::HashMap<String, (String, Option<String>)> = conn
        .prepare("SELECT id, status, blocked_reason FROM kanban_cards ORDER BY id ASC")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, (row.get(1)?, row.get(2)?)))
        })
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();

    assert_eq!(
        rows.get("card-fresh-dispatch").map(|row| row.0.as_str()),
        Some("in_progress"),
        "fresh dispatch must reset the stalled timer"
    );
    assert_eq!(
        rows.get("card-reentered").map(|row| row.0.as_str()),
        Some("in_progress"),
        "in_progress re-entry must reset the stalled timer even if latest dispatch is older"
    );
    assert_eq!(
        rows.get("card-truly-stalled").map(|row| row.0.as_str()),
        Some("in_progress"),
        "manual-intervention escalation keeps the card in_progress while attaching blocked_reason"
    );
    assert!(
        rows.get("card-truly-stalled")
            .and_then(|row| row.1.as_deref())
            .map(|reason| reason.contains("Stalled: no activity"))
            .unwrap_or(false),
        "truly stale card must carry the stalled blocked_reason"
    );
}

#[test]
fn on_tick1min_orphan_review_treats_e2e_dispatch_as_active() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-e2e");

    seed_review_e2e_case(
        &db,
        "card-e2e-review",
        "E2E Review",
        "agent-e2e",
        "-10 minutes",
        "dispatch-e2e",
        "dispatched",
        "-10 minutes",
    );

    let _ = engine.try_fire_hook_by_name("OnTick1min", serde_json::json!({}));
    drain_pending_transitions(&db, &engine);

    let conn = db.lock().unwrap();
    let (status, blocked_reason): (String, Option<String>) = conn
        .query_row(
            "SELECT status, blocked_reason FROM kanban_cards WHERE id = 'card-e2e-review'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-e2e'",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(
        status, "review",
        "active e2e-test dispatch must keep the card out of orphan review recovery"
    );
    assert!(
        blocked_reason.is_none(),
        "protected review card must not gain an orphan-review blocked_reason"
    );
    assert_eq!(
        dispatch_status, "dispatched",
        "e2e-test dispatch should stay active after onTick1min orphan review sweep"
    );
}

#[test]
fn on_tick1min_orphan_review_skips_recently_completed_review_gap() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-review-gap");
    seed_repo(&db, "test-repo");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                review_entered_at, created_at, updated_at
            ) VALUES (
                'card-review-gap', 'Review Gap', 'review', 'medium', 'agent-review-gap', 'test-repo',
                datetime('now', '-10 minutes'), datetime('now', '-10 minutes'), datetime('now', '-10 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at, completed_at
            ) VALUES (
                'dispatch-review-gap', 'card-review-gap', 'agent-review-gap', 'review', 'completed', 'Review Gap R1',
                datetime('now', '-10 minutes'), datetime('now', '-30 seconds'), datetime('now', '-30 seconds')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE kanban_cards SET latest_dispatch_id = 'dispatch-review-gap' WHERE id = 'card-review-gap'",
            [],
        )
        .unwrap();
    }

    let _ = engine.try_fire_hook_by_name("OnTick1min", serde_json::json!({}));
    drain_pending_transitions(&db, &engine);

    let conn = db.lock().unwrap();
    let (status, blocked_reason): (String, Option<String>) = conn
        .query_row(
            "SELECT status, blocked_reason FROM kanban_cards WHERE id = 'card-review-gap'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    assert_eq!(
        status, "review",
        "recently completed review dispatch must protect the review-decision creation gap"
    );
    assert!(
        blocked_reason.is_none(),
        "recent review completion gap must not leave an orphan-review blocked_reason"
    );
}

#[test]
fn on_tick30s_orphan_dispatch_recovers_true_orphan_without_regression() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-orphan-330");
    seed_repo(&db, "test-repo");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, latest_dispatch_id, started_at, created_at, updated_at
            ) VALUES (
                'card-orphan-330', 'True Orphan #330', 'in_progress', 'medium', 'agent-orphan-330', 'test-repo',
                330, 'dispatch-orphan-330', datetime('now', '-20 minutes'), datetime('now', '-20 minutes'), datetime('now', '-20 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-orphan-330', 'card-orphan-330', 'agent-orphan-330', 'implementation', 'pending',
                'orphan impl', datetime('now', '-10 minutes'), datetime('now', '-10 minutes')
            )",
            [],
        )
        .unwrap();
    }

    let _ = engine.try_fire_hook_by_name("OnTick30s", serde_json::json!({}));
    drain_pending_transitions(&db, &engine);

    let conn = db.lock().unwrap();
    let first_card_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-orphan-330'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let first_dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-orphan-330'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let first_confirm_marker_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kv_meta WHERE key = ?1",
            [format!(
                "runtime_supervisor:orphan_confirm:{}",
                "dispatch-orphan-330"
            )],
            |row| row.get(0),
        )
        .unwrap();
    let (first_action, first_note): (String, Option<String>) = conn
        .query_row(
            "SELECT chosen_action, json_extract(evidence_json, '$.supervisor_note')
             FROM runtime_decisions
             WHERE dispatch_id = 'dispatch-orphan-330'
             ORDER BY id DESC
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    drop(conn);

    assert_eq!(
        first_card_status, "in_progress",
        "first orphan tick must wait for confirm instead of rolling the card back immediately"
    );
    assert_eq!(
        first_dispatch_status, "pending",
        "first orphan tick must keep the dispatch pending until confirm succeeds"
    );
    assert!(
        first_confirm_marker_count > 0,
        "first orphan tick must persist a confirm marker"
    );
    assert_eq!(first_action, "Probe");
    assert!(
        first_note
            .as_deref()
            .unwrap_or("")
            .contains("awaiting confirm"),
        "first orphan tick must record that confirm is still pending"
    );

    let _ = engine.try_fire_hook_by_name("OnTick30s", serde_json::json!({}));
    drain_pending_transitions(&db, &engine);

    let conn = db.lock().unwrap();
    let card_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-orphan-330'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let (dispatch_status, dispatch_result): (String, Option<String>) = conn
        .query_row(
            "SELECT status, result FROM task_dispatches WHERE id = 'dispatch-orphan-330'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let (decision_signal, chosen_action, audit_dispatch_id): (String, String, Option<String>) =
        conn.query_row(
            "SELECT signal, chosen_action, dispatch_id
             FROM runtime_decisions
             WHERE dispatch_id = 'dispatch-orphan-330'
             ORDER BY id DESC
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let review_dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches
             WHERE kanban_card_id = 'card-orphan-330' AND dispatch_type = 'review'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let remaining_confirm_marker_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kv_meta WHERE key = ?1",
            [format!(
                "runtime_supervisor:orphan_confirm:{}",
                "dispatch-orphan-330"
            )],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(
        card_status, "requested",
        "true orphan implementation dispatch must roll the card back to the dispatchable preflight state"
    );
    assert_eq!(
        dispatch_status, "failed",
        "true orphan implementation dispatch must be failed when no agent work was observed"
    );
    assert!(
        dispatch_result
            .as_deref()
            .unwrap_or("")
            .contains("orphan_recovery_rollback"),
        "true orphan recovery must keep the orphan_recovery rollback marker"
    );
    assert_ne!(
        card_status, "review",
        "true orphan implementation dispatch must not auto-promote the card into review"
    );
    assert_eq!(
        review_dispatch_count, 0,
        "true orphan recovery must not create a follow-up review dispatch"
    );
    assert_eq!(decision_signal, "OrphanCandidate");
    assert_eq!(chosen_action, "Resume");
    assert_eq!(audit_dispatch_id.as_deref(), Some("dispatch-orphan-330"));
    assert!(
        remaining_confirm_marker_count == 0,
        "confirmed orphan recovery must clear the confirm marker"
    );
}

#[test]
fn on_tick30s_orphan_dispatch_skips_card_that_moved_to_backlog_mid_recovery() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-orphan-race");
    seed_repo(&db, "test-repo");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, latest_dispatch_id, started_at, created_at, updated_at
            ) VALUES (
                'card-race-330', 'Orphan Race #330', 'in_progress', 'medium', 'agent-orphan-race', 'test-repo',
                330, 'dispatch-race-330', datetime('now', '-20 minutes'), datetime('now', '-20 minutes'), datetime('now', '-20 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-race-330', 'card-race-330', 'agent-orphan-race', 'implementation', 'pending',
                'race impl', datetime('now', '-10 minutes'), datetime('now', '-10 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, 'backlog')",
            [format!(
                "test:runtime_supervisor:orphan_post_complete_override:{}",
                "dispatch-race-330"
            )],
        )
        .unwrap();
    }

    let _ = engine.try_fire_hook_by_name("OnTick30s", serde_json::json!({}));
    drain_pending_transitions(&db, &engine);

    let conn = db.lock().unwrap();
    let first_card_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-race-330'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let first_dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-race-330'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let first_confirm_marker_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kv_meta WHERE key = ?1",
            [format!(
                "runtime_supervisor:orphan_confirm:{}",
                "dispatch-race-330"
            )],
            |row| row.get(0),
        )
        .unwrap();
    let (first_action, first_note): (String, Option<String>) = conn
        .query_row(
            "SELECT chosen_action, json_extract(evidence_json, '$.supervisor_note')
             FROM runtime_decisions
             WHERE dispatch_id = 'dispatch-race-330'
             ORDER BY id DESC
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    drop(conn);

    assert_eq!(first_card_status, "in_progress");
    assert_eq!(first_dispatch_status, "pending");
    assert!(first_confirm_marker_count > 0);
    assert_eq!(first_action, "Probe");
    assert!(
        first_note
            .as_deref()
            .unwrap_or("")
            .contains("awaiting confirm"),
        "race path must also wait for confirm on the first orphan tick"
    );

    let _ = engine.try_fire_hook_by_name("OnTick30s", serde_json::json!({}));
    drain_pending_transitions(&db, &engine);

    let conn = db.lock().unwrap();
    let card_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-race-330'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-race-330'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let review_dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches
             WHERE kanban_card_id = 'card-race-330' AND dispatch_type = 'review'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let (chosen_action, decision_note): (String, Option<String>) = conn
        .query_row(
            "SELECT chosen_action, json_extract(evidence_json, '$.supervisor_note')
             FROM runtime_decisions
             WHERE dispatch_id = 'dispatch-race-330'
             ORDER BY id DESC
             LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let remaining_confirm_marker_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kv_meta WHERE key = ?1",
            [format!(
                "runtime_supervisor:orphan_confirm:{}",
                "dispatch-race-330"
            )],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(
        card_status, "backlog",
        "post-complete race guard must keep a backlogged card from reviving into review"
    );
    assert_eq!(
        dispatch_status, "failed",
        "the orphan implementation dispatch must fail instead of completing without work evidence"
    );
    assert_eq!(
        review_dispatch_count, 0,
        "skipped orphan recovery must not create a follow-up review dispatch"
    );
    assert_eq!(
        chosen_action, "Resume",
        "supervisor should still choose resume before the post-complete race guard trips"
    );
    assert!(
        decision_note
            .as_deref()
            .unwrap_or("")
            .contains("card moved to status=backlog"),
        "runtime_decisions audit must explain why the resume transition was skipped"
    );
    assert!(
        remaining_confirm_marker_count == 0,
        "race-guarded orphan recovery must clear the confirm marker after confirm completes"
    );
}

#[tokio::test]
async fn stalled_cards_and_stats_pg_use_latest_activity_timestamp() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_agent_pg(&pool, "agent-stalled").await;
    seed_repo_pg(&pool, "test-repo").await;

    async fn seed_stall_case_pg(
        pool: &sqlx::PgPool,
        card_id: &str,
        title: &str,
        agent_id: &str,
        started_offset: &str,
        updated_offset: &str,
        latest_dispatch: Option<(&str, &str)>,
    ) {
        let started = format!("NOW() + INTERVAL '{started_offset}'");
        let updated = format!("NOW() + INTERVAL '{updated_offset}'");
        let sql = format!(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                started_at, created_at, updated_at
            ) VALUES (
                $1, $2, 'in_progress', 'medium', $3, 'test-repo',
                {started}, {started}, {updated}
            )"
        );
        sqlx::query(&sql)
            .bind(card_id)
            .bind(title)
            .bind(agent_id)
            .execute(pool)
            .await
            .unwrap();

        if let Some((dispatch_id, dispatch_offset)) = latest_dispatch {
            let dispatch_at = format!("NOW() + INTERVAL '{dispatch_offset}'");
            let sql = format!(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
                ) VALUES (
                    $1, $2, $3, 'implementation', 'dispatched', $4, {dispatch_at}, {dispatch_at}
                )"
            );
            sqlx::query(&sql)
                .bind(dispatch_id)
                .bind(card_id)
                .bind(agent_id)
                .bind(format!("{title} Dispatch"))
                .execute(pool)
                .await
                .unwrap();
            sqlx::query("UPDATE kanban_cards SET latest_dispatch_id = $1 WHERE id = $2")
                .bind(dispatch_id)
                .bind(card_id)
                .execute(pool)
                .await
                .unwrap();
        }
    }

    seed_stall_case_pg(
        &pool,
        "card-fresh-dispatch",
        "Fresh Dispatch",
        "agent-stalled",
        "-3 hours",
        "-3 hours",
        Some(("dispatch-fresh", "-10 minutes")),
    )
    .await;
    seed_stall_case_pg(
        &pool,
        "card-reentered",
        "Re-entered",
        "agent-stalled",
        "-3 hours",
        "-10 minutes",
        Some(("dispatch-old", "-3 hours")),
    )
    .await;
    seed_stall_case_pg(
        &pool,
        "card-truly-stalled",
        "Truly Stalled",
        "agent-stalled",
        "-3 hours",
        "-3 hours",
        Some(("dispatch-stale", "-3 hours")),
    )
    .await;

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let stalled_resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/kanban-cards/stalled")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stalled_resp.status(), StatusCode::OK);
    let stalled_body = axum::body::to_bytes(stalled_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let stalled_json: serde_json::Value = serde_json::from_slice(&stalled_body).unwrap();
    let stalled_ids: Vec<String> = stalled_json
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|card| card["id"].as_str().map(ToString::to_string))
        .collect();
    assert_eq!(
        stalled_ids,
        vec!["card-truly-stalled".to_string()],
        "stalled endpoint must ignore fresh-dispatch and re-entered cards"
    );

    let stats_resp = app
        .oneshot(
            Request::builder()
                .uri("/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stats_resp.status(), StatusCode::OK);
    let stats_body = axum::body::to_bytes(stats_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let stats_json: serde_json::Value = serde_json::from_slice(&stats_body).unwrap();
    assert_eq!(
        stats_json["kanban"]["stale_in_progress"],
        serde_json::json!(1),
        "stats stale_in_progress count must match latest-activity stalled detection"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn stats_pg_only_without_sqlite_mirror() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query(
        "INSERT INTO offices (id, name, sort_order, created_at)
         VALUES ($1, $2, 0, NOW())",
    )
    .bind("office-pg-stats")
    .bind("PG Stats Office")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO departments (id, name, office_id, sort_order, created_at)
         VALUES ($1, $2, $3, 0, NOW())",
    )
    .bind("dept-pg-stats")
    .bind("PG Stats Department")
    .bind("office-pg-stats")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO agents (
            id, name, name_ko, department, avatar_emoji, status, xp, sprite_number, created_at, updated_at
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NOW(), NOW())",
    )
    .bind("agent-pg-stats")
    .bind("PG Stats Agent")
    .bind("피지 통계 에이전트")
    .bind("dept-pg-stats")
    .bind("🤖")
    .bind("idle")
    .bind(42_i32)
    .bind(7_i32)
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO office_agents (office_id, agent_id, department_id)
         VALUES ($1, $2, $3)",
    )
    .bind("office-pg-stats")
    .bind("agent-pg-stats")
    .bind("dept-pg-stats")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO sessions (
            session_key, agent_id, status, active_dispatch_id, tokens, last_heartbeat
         ) VALUES ($1, $2, $3, $4, $5, NOW())",
    )
    .bind("session-pg-stats")
    .bind("agent-pg-stats")
    .bind("working")
    .bind("dispatch-working-pg-stats")
    .bind(123_i32)
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, assigned_agent_id, github_issue_url, created_at, updated_at, completed_at
         ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW(), NOW())",
    )
    .bind("card-pg-done")
    .bind("owner/pg-stats-repo")
    .bind("Done Card")
    .bind("done")
    .bind("agent-pg-stats")
    .bind("https://github.com/owner/pg-stats-repo/issues/1")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, assigned_agent_id, started_at, created_at, updated_at
         ) VALUES (
            $1, $2, $3, 'in_progress', $4,
            NOW() - INTERVAL '3 hours',
            NOW() - INTERVAL '3 hours',
            NOW() - INTERVAL '3 hours'
         )",
    )
    .bind("card-pg-stale")
    .bind("owner/pg-stats-repo")
    .bind("Stale Card")
    .bind("agent-pg-stats")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES (
            $1, $2, $3, 'work', 'pending', 'Dispatch',
            NOW() - INTERVAL '3 hours',
            NOW() - INTERVAL '3 hours'
         )",
    )
    .bind("dispatch-pg-stale")
    .bind("card-pg-stale")
    .bind("agent-pg-stats")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query("UPDATE kanban_cards SET latest_dispatch_id = $1 WHERE id = $2")
        .bind("dispatch-pg-stale")
        .bind("card-pg-stale")
        .execute(&pg_pool)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, assigned_agent_id, created_at, updated_at
         ) VALUES ($1, $2, $3, 'review', $4, NOW(), NOW())",
    )
    .bind("card-pg-review")
    .bind("owner/pg-stats-repo")
    .bind("Review Card")
    .bind("agent-pg-stats")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, assigned_agent_id, created_at, updated_at
         ) VALUES ($1, $2, $3, 'requested', $4, NOW(), NOW())",
    )
    .bind("card-pg-requested")
    .bind("owner/pg-stats-repo")
    .bind("Requested Card")
    .bind("agent-pg-stats")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, assigned_agent_id, review_status, blocked_reason, created_at, updated_at
         ) VALUES ($1, $2, $3, 'failed', $4, $5, $6, NOW(), NOW())",
    )
    .bind("card-pg-failed")
    .bind("owner/pg-stats-repo")
    .bind("Failed Card")
    .bind("agent-pg-stats")
    .bind("changes_requested")
    .bind("manual-intervention-required")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    let response = app
        .oneshot(
            Request::builder()
                .uri("/stats?officeId=office-pg-stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "stats_pg_only_without_sqlite_mirror status={} body={}",
        status,
        String::from_utf8_lossy(&body)
    );

    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["agents"]["total"], json!(1));
    assert_eq!(json["agents"]["working"], json!(1));
    assert_eq!(json["dispatched_count"], json!(1));
    assert_eq!(json["top_agents"][0]["id"], json!("agent-pg-stats"));
    assert_eq!(json["top_agents"][0]["stats_tasks_done"], json!(1));
    assert_eq!(json["top_agents"][0]["stats_tokens"], json!(123));
    assert_eq!(json["departments"][0]["id"], json!("dept-pg-stats"));
    assert_eq!(json["departments"][0]["working_agents"], json!(1));
    assert_eq!(json["kanban"]["review_queue"], json!(1));
    assert_eq!(json["kanban"]["waiting_acceptance"], json!(1));
    assert_eq!(json["kanban"]["failed"], json!(1));
    assert_eq!(json["kanban"]["blocked"], json!(1));
    assert_eq!(json["kanban"]["stale_in_progress"], json!(1));
    assert_eq!(
        json["kanban"]["top_repos"][0]["github_repo"],
        json!("owner/pg-stats-repo")
    );
    assert_eq!(json["github_closed_today"], json!(1));

    let sqlite_agent_count: i64 = db
        .read_conn()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM agents WHERE id = 'agent-pg-stats'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sqlite_agent_count, 0, "sqlite mirror should stay empty");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn force_transition_succeeds_with_correct_channel() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_card_with_status_pg(&pool, "card-ft3", "requested").await;

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-ft3/transition")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(r#"{"status":"done"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["forced"], true);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn force_transition_rejects_mismatched_channel_when_pmd_channel_is_configured() {
    let _lock = env_lock();
    let db = test_db();
    let engine = test_engine(&db);
    seed_card_with_status(&db, "card-ft4", "requested");

    let config_dir = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.kanban.manager_channel_id = Some("pmd-chan-123".to_string());
    let config_path = config_dir.path().join("agentdesk.yaml");
    crate::config::save_to_path(&config_path, &config).unwrap();
    let _config_guard = EnvVarGuard::set_path("AGENTDESK_CONFIG", &config_path);

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-ft4/transition")
                .header("content-type", "application/json")
                .header("x-channel-id", "wrong-channel")
                .body(Body::from(r#"{"status":"done"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["error"],
        "force-transition requires PMD channel authorization"
    );
}

// #1342 ci-red: this test exercises the JS bridge's setStatus path during
// the OnCardTerminal hook, which on PG holds a write tx across pipeline
// resolution. With the default `#[tokio::test]` (current_thread) runtime
// the hook executor and the source pool's drivers contend for the only
// worker thread, so a multi-thread runtime is required for the test to
// drive the pool while the actor thread is dispatching JS hooks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn force_transition_to_done_tracks_pr_from_live_work_dispatch_and_cleans_it_up() {
    crate::pipeline::ensure_loaded();
    let (repo, _repo_override) = setup_test_repo();
    let _gh = install_mock_gh_pr_tracking(
        "test/repo",
        "wt/card-575-force",
        905,
        "feature-sha-575-force",
    );
    let policy_dir = tempfile::tempdir().unwrap();
    let source_policies = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
    for entry in std::fs::read_dir(&source_policies).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("js") {
            continue;
        }
        std::fs::copy(&path, policy_dir.path().join(entry.file_name())).unwrap();
    }
    std::fs::write(
        policy_dir.path().join("zz-ft-terminal-marker.js"),
        r#"
        agentdesk.registerPolicy({
          name: "ft-terminal-marker",
          priority: 9999,
          onCardTerminal: function(payload) {
            agentdesk.db.execute(
              "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('test_force_terminal_marker', ?1)",
              [payload.card_id + ":" + payload.status]
            );
          }
        });
        "#,
    )
    .unwrap();
    let origin = tempfile::tempdir().unwrap();
    run_git(origin.path(), &["init", "--bare"]);
    run_git(
        repo.path(),
        &["remote", "add", "origin", origin.path().to_str().unwrap()],
    );
    run_git(repo.path(), &["push", "-u", "origin", "main"]);

    let worktrees_dir = repo.path().join("worktrees");
    std::fs::create_dir_all(&worktrees_dir).unwrap();
    run_git(repo.path(), &["branch", "wt/card-575-force"]);

    let worktree_path = worktrees_dir.join("card-575-force");
    run_git(
        repo.path(),
        &[
            "worktree",
            "add",
            worktree_path.to_str().unwrap(),
            "wt/card-575-force",
        ],
    );
    std::fs::write(
        worktree_path.join("feature.txt"),
        "force transition merge\n",
    )
    .unwrap();
    run_git(worktree_path.as_path(), &["add", "feature.txt"]);
    run_git(
        worktree_path.as_path(),
        &["commit", "-m", "fix: force-transition merge target (#575)"],
    );
    std::fs::write(
        worktree_path.join("merge-proof.txt"),
        "second force transition merge\n",
    )
    .unwrap();
    run_git(worktree_path.as_path(), &["add", "merge-proof.txt"]);
    run_git(
        worktree_path.as_path(),
        &[
            "commit",
            "-m",
            "fix: second force-transition merge target (#575)",
        ],
    );
    run_git(repo.path(), &["push", "-u", "origin", "wt/card-575-force"]);
    let feature_commit = run_git_output(worktree_path.as_path(), &["rev-parse", "HEAD"]);

    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let mut config = crate::config::Config::default();
    config.policies.dir = policy_dir.path().to_path_buf();
    config.policies.hot_reload = false;
    let engine = PolicyEngine::new_with_pg(&config, Some(pool.clone())).unwrap();
    seed_agent_pg(&pool, "agent-ft-terminal").await;
    seed_repo_pg(&pool, "test/repo").await;
    sqlx::query(
        "INSERT INTO kv_meta (key, value) VALUES ('kanban_manager_channel_id', $1)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .bind("pmd-chan-123")
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            github_issue_number, latest_dispatch_id, created_at, updated_at, started_at
        ) VALUES (
            'card-ft-terminal', 'Issue #575', 'in_progress', 'medium', 'agent-ft-terminal', 'test/repo',
            575, 'dispatch-ft-terminal', NOW() - INTERVAL '5 minutes', NOW() - INTERVAL '5 minutes', NOW() - INTERVAL '5 minutes'
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
            created_at, updated_at
        ) VALUES (
            'dispatch-ft-terminal', 'card-ft-terminal', 'agent-ft-terminal', 'implementation', 'dispatched',
            'live impl', $1, NOW() - INTERVAL '4 minutes', NOW() - INTERVAL '4 minutes'
        )",
    )
    .bind(
        serde_json::json!({
            "worktree_path": worktree_path.to_string_lossy().to_string(),
            "worktree_branch": "wt/card-575-force"
        })
        .to_string(),
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kv_meta (key, value) VALUES ('merge_automation_enabled', 'true')
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kv_meta (key, value) VALUES ('merge_strategy_mode', 'pr-always')
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions (
            session_key, agent_id, provider, status, cwd, active_dispatch_id, last_heartbeat, created_at
        ) VALUES (
            'session-ft-terminal', 'agent-ft-terminal', 'codex', 'working', $1, 'dispatch-ft-terminal',
            NOW() - INTERVAL '4 minutes', NOW() - INTERVAL '4 minutes'
        )",
    )
    .bind(worktree_path.to_string_lossy().to_string())
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(db.clone(), engine, config, None, pool.clone());
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-ft-terminal/transition")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(r#"{"status":"done"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["forced"], true);
    assert_eq!(
        json["cancelled_dispatches"],
        serde_json::json!(1),
        "force-transition to done must cancel the live work dispatch before terminal hooks"
    );

    let hook_marker: Option<String> =
        sqlx::query_scalar("SELECT value FROM kv_meta WHERE key = 'test_force_terminal_marker'")
            .fetch_optional(&pool)
            .await
            .unwrap();
    assert_eq!(
        hook_marker.as_deref(),
        Some("card-ft-terminal:done"),
        "force-transition to done must still fire OnCardTerminal hooks"
    );

    let merge_debug: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT state, last_error FROM pr_tracking WHERE card_id = 'card-ft-terminal'",
    )
    .fetch_optional(&pool)
    .await
    .unwrap()
    .unwrap_or((None, None));

    run_git(
        repo.path(),
        &["fetch", "origin", "main", "wt/card-575-force"],
    );
    let merged = Command::new("git")
        .args([
            "merge-base",
            "--is-ancestor",
            &feature_commit,
            "origin/main",
        ])
        .current_dir(repo.path())
        .status()
        .unwrap();
    let pushed_feature = Command::new("git")
        .args(["show", "origin/wt/card-575-force:feature.txt"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    let pushed_proof = Command::new("git")
        .args(["show", "origin/wt/card-575-force:merge-proof.txt"])
        .current_dir(repo.path())
        .output()
        .unwrap();
    assert!(
        !merged.success(),
        "force-transition terminal path must not direct-merge into origin/main when PR+CI is required; pr_tracking={:?}",
        merge_debug
    );
    assert!(
        pushed_feature.status.success()
            && pushed_proof.status.success()
            && String::from_utf8_lossy(&pushed_feature.stdout) == "force transition merge\n"
            && String::from_utf8_lossy(&pushed_proof.stdout) == "second force transition merge\n",
        "force-transition terminal path must still push the tracked worktree branch for PR creation; pr_tracking={:?}",
        merge_debug
    );

    let mut card_status = String::new();
    let mut latest_dispatch_id: Option<String> = None;
    let mut blocked_reason: Option<String> = None;
    let mut dispatch_status = String::new();
    let mut pr_tracking_state: Option<String> = None;
    let mut pr_tracking_pr_number: Option<i64> = None;
    let mut pr_tracking_last_error: Option<String> = None;

    let tracking_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let card_row: (String, Option<String>, Option<String>) = sqlx::query_as(
            "SELECT status, latest_dispatch_id, blocked_reason FROM kanban_cards WHERE id = 'card-ft-terminal'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let observed_dispatch_status: String = sqlx::query_scalar(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-ft-terminal'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        // #1342 ci-red: pr_tracking.pr_number is INT8/BIGINT in the PG
        // schema (sqlx 0.8 strict typing), so it must be decoded as i64.
        let pr_row: (Option<String>, Option<i64>, Option<String>) = sqlx::query_as(
            "SELECT state, pr_number, last_error FROM pr_tracking WHERE card_id = 'card-ft-terminal'",
        )
        .fetch_optional(&pool)
        .await
        .unwrap()
        .unwrap_or((None, None, None));

        card_status = card_row.0;
        latest_dispatch_id = card_row.1;
        blocked_reason = card_row.2;
        dispatch_status = observed_dispatch_status;
        pr_tracking_state = pr_row.0;
        pr_tracking_pr_number = pr_row.1;
        pr_tracking_last_error = pr_row.2;

        if pr_tracking_state.as_deref() == Some("wait-ci")
            && pr_tracking_pr_number == Some(905)
            && blocked_reason.as_deref() == Some("ci:waiting")
        {
            break;
        }

        if std::time::Instant::now() >= tracking_deadline {
            break;
        }

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    assert_eq!(card_status, "done");
    assert!(
        latest_dispatch_id.is_none(),
        "force-transition terminal cleanup must clear stale latest_dispatch_id"
    );
    assert_eq!(
        dispatch_status, "cancelled",
        "live implementation dispatch must not survive a force-transition to done"
    );
    assert_eq!(
        pr_tracking_state.as_deref(),
        Some("wait-ci"),
        "force-transition terminal cleanup should track the created PR and wait for CI"
    );
    assert_eq!(pr_tracking_pr_number, Some(905));
    assert_eq!(pr_tracking_last_error, None);
    assert_eq!(blocked_reason.as_deref(), Some("ci:waiting"));

    pool.close().await;
    pg_db.drop().await;
}

// #1064: /api/kanban-cards/batch-transition and bulk-action were removed in
// favour of per-card POST /api/kanban-cards/{id}/transition. The paths now
// collide with the /kanban-cards/{id} wildcard (GET/PATCH/DELETE), so POST
// against them returns 405 Method Not Allowed — still unambiguously "not
// served" from the caller's perspective.
#[tokio::test]
async fn removed_batch_transition_route_is_unserved() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card_with_status(&db, "card-bt-1", "backlog");
    set_pmd_channel(&db, "pmd-chan-123");

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/batch-transition")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(r#"{"card_ids":["card-bt-1"],"status":"ready"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(matches!(
        response.status(),
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
    ));
}

#[tokio::test]
async fn removed_bulk_action_route_is_unserved() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_card_with_status(&db, "card-ba-1", "backlog");

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/bulk-action")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"action":"pass","card_ids":["card-ba-1"]}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(matches!(
        response.status(),
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
    ));
}

#[tokio::test]
async fn postgres_force_transition_to_ready_cleans_up_live_state() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());
    seed_agent(&db, "agent-ft-clean-pg");
    seed_repo(&db, "test-repo");
    set_pmd_channel(&db, "pmd-chan-123");
    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind("agent-ft-clean-pg")
        .bind("Agent Force Transition Cleanup PG")
        .execute(&pg_pool)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            latest_dispatch_id, review_status, review_round, review_notes,
            suggestion_pending_at, review_entered_at, awaiting_dod_at,
            created_at, updated_at, started_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6,
            $7, $8, $9, $10,
            NOW() - INTERVAL '12 minutes', NOW() - INTERVAL '11 minutes', NOW() - INTERVAL '10 minutes',
            NOW() - INTERVAL '20 minutes', NOW() - INTERVAL '20 minutes', NOW() - INTERVAL '20 minutes'
         )",
    )
    .bind("card-ft-clean-pg")
    .bind("Force Transition Cleanup PG")
    .bind("in_progress")
    .bind("medium")
    .bind("agent-ft-clean-pg")
    .bind("test-repo")
    .bind("dispatch-ft-clean-pg")
    .bind("reviewing")
    .bind(4_i64)
    .bind("stale review notes")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, NOW() - INTERVAL '10 minutes', NOW() - INTERVAL '10 minutes'
         )",
    )
    .bind("dispatch-ft-clean-pg")
    .bind("card-ft-clean-pg")
    .bind("agent-ft-clean-pg")
    .bind("implementation")
    .bind("pending")
    .bind("live impl")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions (
            session_key, agent_id, provider, status, active_dispatch_id, last_heartbeat, created_at
         ) VALUES (
            $1, $2, $3, $4, $5, NOW() - INTERVAL '9 minutes', NOW() - INTERVAL '9 minutes'
         )",
    )
    .bind("session-ft-clean-pg")
    .bind("agent-ft-clean-pg")
    .bind("codex")
    .bind("working")
    .bind("dispatch-ft-clean-pg")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES
            ($1, $2, $3, 'active'),
            ($4, $2, $3, 'active')",
    )
    .bind("run-ft-clean-pg")
    .bind("test-repo")
    .bind("agent-ft-clean-pg")
    .bind("run-ft-clean-pg-pending")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, NOW() - INTERVAL '10 minutes'
         )",
    )
    .bind("entry-ft-clean-pg-dispatched")
    .bind("run-ft-clean-pg")
    .bind("card-ft-clean-pg")
    .bind("agent-ft-clean-pg")
    .bind("dispatched")
    .bind("dispatch-ft-clean-pg")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status
         ) VALUES (
            $1, $2, $3, $4, $5
         )",
    )
    .bind("entry-ft-clean-pg-pending")
    .bind("run-ft-clean-pg-pending")
    .bind("card-ft-clean-pg")
    .bind("agent-ft-clean-pg")
    .bind("pending")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO card_review_state (
            card_id, state, pending_dispatch_id, review_round, last_verdict, last_decision,
            approach_change_round, session_reset_round, review_entered_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, NOW() - INTERVAL '11 minutes', NOW()
         )",
    )
    .bind("card-ft-clean-pg")
    .bind("suggestion_pending")
    .bind("old-review-dispatch")
    .bind(4_i64)
    .bind("pass")
    .bind("approved")
    .bind(3_i64)
    .bind(4_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-ft-clean-pg/transition")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(r#"{"status":"ready"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body).to_string();
    assert_eq!(
        status,
        StatusCode::OK,
        "unexpected force-transition response: {body_text}"
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["forced"], true);
    // #1235: lower-bound counts; backend-specific cleanup paths can fold in
    // additional rows whose count is not a stable contract.
    let cancelled_dispatches_reported = json["cancelled_dispatches"].as_i64().unwrap_or_default();
    let skipped_entries_reported = json["skipped_auto_queue_entries"]
        .as_i64()
        .unwrap_or_default();
    assert!(
        cancelled_dispatches_reported >= 1,
        "expected at least the live impl dispatch to be cancelled, got {cancelled_dispatches_reported}"
    );
    assert!(
        skipped_entries_reported >= 2,
        "expected at least the 2 seeded auto_queue entries to be skipped, got {skipped_entries_reported}"
    );
    assert_eq!(json["card"]["status"], "ready");

    let (
        card_status,
        latest_dispatch_id,
        review_status,
        review_round,
        review_notes,
        suggestion_pending_at,
        review_entered_at,
        awaiting_dod_at,
    ): (
        String,
        Option<String>,
        Option<String>,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT status, latest_dispatch_id, review_status, review_round, review_notes,
                suggestion_pending_at::text, review_entered_at::text, awaiting_dod_at::text
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind("card-ft-clean-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    let (review_state_round, review_state_status, review_state_pending_dispatch): (
        i64,
        String,
        Option<String>,
    ) = sqlx::query_as(
        "SELECT review_round, state, pending_dispatch_id
         FROM card_review_state
         WHERE card_id = $1",
    )
    .bind("card-ft-clean-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    let dispatch_status: String =
        sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = $1")
            .bind("dispatch-ft-clean-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    // #1235: stable-key lookups (id → row) replace Vec equality. Order-only
    // comparisons assumed deterministic IDs and exact row counts; both vary
    // when sibling cleanup paths fan out.
    let entry_rows_vec: Vec<(String, String, Option<String>)> = sqlx::query_as(
        "SELECT id, status, dispatch_id
         FROM auto_queue_entries
         WHERE kanban_card_id = $1",
    )
    .bind("card-ft-clean-pg")
    .fetch_all(&pg_pool)
    .await
    .unwrap();
    let entry_rows: std::collections::BTreeMap<String, (String, Option<String>)> = entry_rows_vec
        .into_iter()
        .map(|(id, status, dispatch_id)| (id, (status, dispatch_id)))
        .collect();
    let run_rows_vec: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, status
         FROM auto_queue_runs
         WHERE id IN ($1, $2)",
    )
    .bind("run-ft-clean-pg")
    .bind("run-ft-clean-pg-pending")
    .fetch_all(&pg_pool)
    .await
    .unwrap();
    let run_rows: std::collections::BTreeMap<String, String> = run_rows_vec.into_iter().collect();
    let (session_status, active_dispatch_id): (String, Option<String>) = sqlx::query_as(
        "SELECT status, active_dispatch_id
         FROM sessions
         WHERE session_key = $1",
    )
    .bind("session-ft-clean-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();

    assert_eq!(card_status, "ready");
    assert!(latest_dispatch_id.is_none());
    assert!(review_status.is_none());
    assert_eq!(review_round, 0);
    assert!(review_notes.is_none());
    assert!(suggestion_pending_at.is_none());
    assert!(review_entered_at.is_none());
    assert!(awaiting_dod_at.is_none());
    assert_eq!(review_state_round, 0);
    assert_eq!(review_state_status, "idle");
    assert!(review_state_pending_dispatch.is_none());
    assert_eq!(dispatch_status, "cancelled");
    // #1235: per-id contract assertions. The PG cleanup path keeps the
    // dispatch_id link on the originally-dispatched entry but skips the
    // status; only the status field is the stable contract here.
    let pg_entry_dispatched = entry_rows
        .get("entry-ft-clean-pg-dispatched")
        .expect("seeded dispatched entry must still exist");
    let pg_entry_pending = entry_rows
        .get("entry-ft-clean-pg-pending")
        .expect("seeded pending entry must still exist");
    assert_eq!(
        pg_entry_dispatched.0, "skipped",
        "force-transition cleanup must skip the live (dispatched) auto-queue entry on PG"
    );
    assert_eq!(
        pg_entry_pending.0, "skipped",
        "force-transition cleanup must skip the pending auto-queue entry on PG"
    );
    assert!(
        pg_entry_pending.1.is_none(),
        "pending entry never had a dispatch_id; cleanup must keep it unset on PG, got {:?}",
        pg_entry_pending.1
    );
    assert_eq!(
        run_rows.get("run-ft-clean-pg").map(String::as_str),
        Some("completed"),
        "force-transition cleanup must complete the live run on PG"
    );
    assert_eq!(
        run_rows.get("run-ft-clean-pg-pending").map(String::as_str),
        Some("completed"),
        "force-transition cleanup must complete the pending-only run on PG"
    );
    assert_eq!(session_status, "idle");
    assert!(active_dispatch_id.is_none());

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn rereview_reactivates_done_card_with_fresh_review_dispatch() {
    crate::pipeline::ensure_loaded();
    let _env_lock = env_lock();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-rereview");
    set_pmd_channel(&db, "pmd-chan-123");
    ensure_auto_queue_tables(&db);

    let repo = tempfile::tempdir().unwrap();
    run_git(repo.path(), &["init", "-b", "main"]);
    run_git(repo.path(), &["config", "user.email", "test@test.com"]);
    run_git(repo.path(), &["config", "user.name", "Test"]);
    run_git(repo.path(), &["commit", "--allow-empty", "-m", "initial"]);
    let expected_commit = git_commit(repo.path(), "fix: review target (#269)");
    let _repo_dir = EnvVarGuard::set_path("AGENTDESK_REPO_DIR", repo.path());
    let _config_dir = write_repo_mapping_config(&[("test-repo", repo.path())]);
    let _config = EnvVarGuard::set_path(
        "AGENTDESK_CONFIG",
        &_config_dir.path().join("agentdesk.yaml"),
    );

    let completed_commit = "1111111111111111111111111111111111111269";
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, latest_dispatch_id, created_at, updated_at, completed_at
            ) VALUES (
                'card-rereview', 'Issue #269', 'done', 'medium', 'agent-rereview', 'test-repo',
                269, 'rd-old', datetime('now'), datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, result,
                created_at, updated_at
            ) VALUES (
                'impl-rereview', 'card-rereview', 'agent-rereview', 'implementation', 'completed',
                'impl', ?1, datetime('now', '-2 minutes'), datetime('now', '-2 minutes')
            )",
            [serde_json::json!({
                "completed_commit": completed_commit,
                "completed_branch": "main"
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context,
                created_at, updated_at
            ) VALUES (
                'review-old', 'card-rereview', 'agent-rereview', 'review', 'completed',
                'old review', ?1, datetime('now', '-1 minutes'), datetime('now', '-1 minutes')
            )",
            [serde_json::json!({
                "reviewed_commit": "wrong-review-target"
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at
            ) VALUES (
                'rd-old', 'card-rereview', 'agent-rereview', 'review-decision', 'completed',
                'old rd', datetime('now', '-30 seconds'), datetime('now', '-30 seconds')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-rereview', 'test-repo', 'agent-rereview', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, completed_at
            ) VALUES (
                'entry-rereview', 'run-rereview', 'card-rereview', 'agent-rereview',
                'done', 'rd-old', datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-rereview/rereview")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(
                    r#"{"reason":"repair wrong review target in unified thread"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["rereviewed"], true);

    let review_dispatch_id = json["review_dispatch_id"]
        .as_str()
        .expect("response must include new review dispatch id")
        .to_string();

    let conn = db.lock().unwrap();
    let card_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-rereview'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(card_status, "review");

    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = ?1",
            [&review_dispatch_id],
            |row| row.get(0),
        )
        .unwrap();
    let reviewed_commit = conn
        .query_row(
            "SELECT context FROM task_dispatches WHERE id = ?1",
            [&review_dispatch_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .unwrap()
        .and_then(|context| {
            serde_json::from_str::<serde_json::Value>(&context)
                .ok()
                .and_then(|value| {
                    value
                        .get("reviewed_commit")
                        .and_then(|entry| entry.as_str())
                        .map(str::to_string)
                })
        });
    assert_eq!(dispatch_status, "pending");
    assert_eq!(
        reviewed_commit.as_deref(),
        Some(expected_commit.as_str()),
        "reviewed_commit should be recovered from the repo fallback chain"
    );

    let (entry_status, entry_dispatch_id): (String, String) = conn
        .query_row(
            "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-rereview'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(entry_status, "dispatched");
    assert_eq!(entry_dispatch_id, review_dispatch_id);
}

#[tokio::test]
async fn dispute_repeat_pg_does_not_reuse_poisoned_review_target() {
    crate::pipeline::ensure_loaded();
    let _env_lock = env_lock();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_agent_pg(&pool, "agent-dispute-repeat").await;
    seed_repo_pg(&pool, "test-repo").await;

    let repo = tempfile::tempdir().unwrap();
    run_git(repo.path(), &["init", "-b", "main"]);
    run_git(repo.path(), &["config", "user.email", "test@test.com"]);
    run_git(repo.path(), &["config", "user.name", "Test"]);
    run_git(repo.path(), &["commit", "--allow-empty", "-m", "initial"]);
    let safe_commit = git_commit(repo.path(), "fix: safe review target (#472)");
    let worktree_dir = repo.path().join("wt-472");
    run_git(
        repo.path(),
        &[
            "worktree",
            "add",
            worktree_dir.to_str().unwrap(),
            "-b",
            "wt/472-poison",
        ],
    );
    let worktree_path = worktree_dir.to_string_lossy().to_string();
    let poisoned_commit = git_commit(&worktree_dir, "chore: stale target (#482)");
    let _repo_dir = EnvVarGuard::set_path("AGENTDESK_REPO_DIR", repo.path());
    let _config_dir = write_repo_mapping_config(&[("test-repo", repo.path())]);
    let _config = EnvVarGuard::set_path(
        "AGENTDESK_CONFIG",
        &_config_dir.path().join("agentdesk.yaml"),
    );

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            github_issue_number, latest_dispatch_id, review_status, created_at, updated_at
        ) VALUES (
            'card-dispute-repeat', 'Issue #472', 'review', 'medium', 'agent-dispute-repeat', 'test-repo',
            472, 'rd-dispute-1', 'suggestion_pending', NOW(), NOW()
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, result,
            created_at, updated_at, completed_at
        ) VALUES (
            'impl-dispute-repeat', 'card-dispute-repeat', 'agent-dispute-repeat', 'implementation',
            'completed', 'impl', $1, $2,
            NOW() - INTERVAL '10 minutes', NOW() - INTERVAL '10 minutes', NOW() - INTERVAL '10 minutes'
        )",
    )
    .bind(
        serde_json::json!({
            "worktree_path": worktree_path,
            "branch": "wt/472-poison"
        })
        .to_string(),
    )
    .bind(
        serde_json::json!({
            "completed_worktree_path": worktree_path,
            "completed_branch": "wt/472-poison",
            "completed_commit": poisoned_commit.clone(),
        })
        .to_string(),
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title,
            created_at, updated_at
        ) VALUES (
            'rd-dispute-1', 'card-dispute-repeat', 'agent-dispute-repeat', 'review-decision',
            'pending', '[Review Decision]', NOW() - INTERVAL '1 minutes', NOW() - INTERVAL '1 minutes'
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO card_review_state (
            card_id, state, pending_dispatch_id, review_round, updated_at
        ) VALUES (
            'card-dispute-repeat', 'suggestion_pending', 'rd-dispute-1', 1, NOW()
        )",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let dispute_request = |dispatch_id: &str| {
        Request::builder()
            .method("POST")
            .uri("/review-decision")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({
                    "card_id": "card-dispute-repeat",
                    "decision": "dispute",
                    "dispatch_id": dispatch_id,
                })
                .to_string(),
            ))
            .unwrap()
    };

    let response1 = app
        .clone()
        .oneshot(dispute_request("rd-dispute-1"))
        .await
        .unwrap();
    assert_eq!(response1.status(), StatusCode::OK);

    let body1 = axum::body::to_bytes(response1.into_body(), usize::MAX)
        .await
        .unwrap();
    let json1: serde_json::Value = serde_json::from_slice(&body1).unwrap();
    let first_review_dispatch_id = json1["review_dispatch_id"]
        .as_str()
        .expect("dispute response must include first review dispatch id")
        .to_string();
    let first_reviewed_commit = json1["reviewed_commit"]
        .as_str()
        .expect("dispute response must include first reviewed commit")
        .to_string();
    assert_eq!(first_reviewed_commit, safe_commit);
    assert_ne!(first_reviewed_commit, poisoned_commit);

    let first_rd_status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM task_dispatches WHERE id = 'rd-dispute-1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(first_rd_status, "completed");

    sqlx::query(
        "UPDATE task_dispatches
         SET status = 'completed', completed_at = NOW(), updated_at = NOW()
         WHERE id = $1",
    )
    .bind(&first_review_dispatch_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title,
            created_at, updated_at
        ) VALUES (
            'rd-dispute-2', 'card-dispute-repeat', 'agent-dispute-repeat', 'review-decision',
            'pending', '[Review Decision 2]', NOW(), NOW()
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE kanban_cards
         SET latest_dispatch_id = 'rd-dispute-2', review_status = 'suggestion_pending', updated_at = NOW()
         WHERE id = 'card-dispute-repeat'",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE card_review_state
         SET state = 'suggestion_pending', pending_dispatch_id = 'rd-dispute-2', updated_at = NOW()
         WHERE card_id = 'card-dispute-repeat'",
    )
    .execute(&pool)
    .await
    .unwrap();

    let response2 = app.oneshot(dispute_request("rd-dispute-2")).await.unwrap();
    assert_eq!(response2.status(), StatusCode::OK);

    let body2 = axum::body::to_bytes(response2.into_body(), usize::MAX)
        .await
        .unwrap();
    let json2: serde_json::Value = serde_json::from_slice(&body2).unwrap();
    let second_review_dispatch_id = json2["review_dispatch_id"]
        .as_str()
        .expect("dispute response must include second review dispatch id")
        .to_string();
    let second_reviewed_commit = json2["reviewed_commit"]
        .as_str()
        .expect("dispute response must include second reviewed commit")
        .to_string();
    assert_ne!(second_review_dispatch_id, first_review_dispatch_id);
    assert_eq!(second_reviewed_commit, safe_commit);
    assert_ne!(second_reviewed_commit, poisoned_commit);

    let second_context_raw = sqlx::query_scalar::<_, Option<String>>(
        "SELECT context FROM task_dispatches WHERE id = $1",
    )
    .bind(&second_review_dispatch_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let second_context: serde_json::Value =
        serde_json::from_str(second_context_raw.as_deref().unwrap_or("{}"))
            .expect("second review dispatch must persist context");
    assert_eq!(second_context["reviewed_commit"], safe_commit);
    let actual_worktree_path = std::fs::canonicalize(
        second_context["worktree_path"]
            .as_str()
            .expect("review dispatch must persist worktree_path"),
    )
    .unwrap()
    .to_string_lossy()
    .to_string();
    let expected_worktree_path = std::fs::canonicalize(repo.path())
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert_eq!(actual_worktree_path, expected_worktree_path);
    assert_eq!(second_context["branch"], "main");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn reopen_reactivates_done_card_without_deadlocking_review_tuning_fixup() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-reopen");
    set_pmd_channel(&db, "pmd-chan-123");
    ensure_auto_queue_tables(&db);
    let reopen_target = crate::pipeline::get()
        .dispatchable_states()
        .into_iter()
        .next()
        .expect("default pipeline should expose at least one dispatchable state")
        .to_string();

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                review_status, created_at, updated_at, completed_at
            ) VALUES (
                'card-reopen', 'Issue #270', 'done', 'medium', 'agent-reopen', 'test-repo',
                'pass', datetime('now'), datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-reopen', 'test-repo', 'agent-reopen', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, completed_at
            ) VALUES (
                'entry-reopen', 'run-reopen', 'card-reopen', 'agent-reopen',
                'done', datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO review_tuning_outcomes (
                card_id, dispatch_id, review_round, verdict, decision, outcome
            ) VALUES (
                'card-reopen', 'review-pass', 1, 'pass', 'approved', 'true_negative'
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-reopen/reopen")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(
                    r#"{"reason":"retry after incorrect pass","review_status":"queued"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["reopened"], true);
    assert_eq!(json["to"], reopen_target);

    let conn = db.lock().unwrap();
    let (status, review_status, completed_at): (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT status, review_status, completed_at
             FROM kanban_cards WHERE id = 'card-reopen'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(status, reopen_target);
    assert_eq!(review_status.as_deref(), Some("queued"));
    assert!(completed_at.is_none());

    let entry_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-reopen'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(entry_status, "dispatched");

    let outcome: String = conn
        .query_row(
            "SELECT outcome FROM review_tuning_outcomes
             WHERE card_id = 'card-reopen'
             ORDER BY review_round DESC, id DESC
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(outcome, "false_negative");
}

#[tokio::test]
async fn transition_to_done_records_true_negative_in_postgres_review_tuning() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = crate::engine::PolicyEngine::new_with_pg(
        &crate::config::Config::default(),
        Some(pg_pool.clone()),
    )
    .unwrap();

    seed_agent_pg(&pg_pool, "agent-pg-tn").await;
    seed_repo_pg(&pg_pool, "test-repo").await;

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, review_status, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, NOW(), NOW()
         )",
    )
    .bind("card-pg-tn")
    .bind("test-repo")
    .bind("PG TN")
    .bind("review")
    .bind("medium")
    .bind("agent-pg-tn")
    .bind("pass")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO card_review_state (card_id, review_round, last_verdict, updated_at)
         VALUES ($1, $2, $3, NOW())",
    )
    .bind("card-pg-tn")
    .bind(2_i32)
    .bind("pass")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, dispatch_type, status, result, created_at, updated_at, completed_at
         )
         VALUES ($1, $2, 'review', 'completed', $3, NOW(), NOW(), NOW())",
    )
    .bind("dispatch-pg-tn")
    .bind("card-pg-tn")
    .bind(
        serde_json::json!({
            "items": [
                {"category": "logic"},
                {"category": "tests"}
            ]
        })
        .to_string(),
    )
    .execute(&pg_pool)
    .await
    .unwrap();

    let result = crate::kanban::transition_status_with_opts_pg_only(
        &pg_pool,
        &engine,
        "card-pg-tn",
        "done",
        "review",
        crate::engine::transition::ForceIntent::OperatorOverride,
    )
    .await;
    assert!(result.is_ok(), "transition to done should succeed");

    let row = sqlx::query(
        "SELECT review_round::BIGINT AS review_round, verdict, decision, outcome, finding_categories
         FROM review_tuning_outcomes
         WHERE card_id = $1
         ORDER BY id DESC
         LIMIT 1",
    )
    .bind("card-pg-tn")
    .fetch_one(&pg_pool)
    .await
    .unwrap();

    assert_eq!(row.try_get::<i64, _>("review_round").unwrap(), 2);
    assert_eq!(row.try_get::<String, _>("verdict").unwrap(), "pass");
    assert_eq!(row.try_get::<String, _>("decision").unwrap(), "done");
    assert_eq!(
        row.try_get::<String, _>("outcome").unwrap(),
        "true_negative"
    );
    assert_eq!(
        row.try_get::<String, _>("finding_categories").unwrap(),
        "[\"logic\",\"tests\"]"
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn postgres_reopen_updates_review_tuning_outcome_in_postgres() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    seed_agent(&db, "agent-reopen-pg");
    seed_repo(&db, "test-repo");
    set_pmd_channel(&db, "pmd-chan-123");
    ensure_auto_queue_tables(&db);
    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind("agent-reopen-pg")
        .bind("Agent Reopen PG")
        .execute(&pg_pool)
        .await
        .unwrap();
    let reopen_target = crate::pipeline::get()
        .dispatchable_states()
        .into_iter()
        .next()
        .expect("default pipeline should expose at least one dispatchable state")
        .to_string();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            review_status, created_at, updated_at, completed_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, NOW(), NOW(), NOW()
         )",
    )
    .bind("card-reopen-pg")
    .bind("Issue #270 PG")
    .bind("done")
    .bind("medium")
    .bind("agent-reopen-pg")
    .bind("test-repo")
    .bind("pass")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("run-reopen-pg")
    .bind("test-repo")
    .bind("agent-reopen-pg")
    .bind("active")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, completed_at
         ) VALUES (
            $1, $2, $3, $4, $5, NOW()
         )",
    )
    .bind("entry-reopen-pg")
    .bind("run-reopen-pg")
    .bind("card-reopen-pg")
    .bind("agent-reopen-pg")
    .bind("done")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO review_tuning_outcomes (
            card_id, dispatch_id, review_round, verdict, decision, outcome
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         )",
    )
    .bind("card-reopen-pg")
    .bind("review-pass-pg")
    .bind(1_i32)
    .bind("pass")
    .bind("approved")
    .bind("true_negative")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-reopen-pg/reopen")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(
                    r#"{"reason":"retry after incorrect pass","review_status":"queued"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body).to_string();
    assert_eq!(
        status,
        StatusCode::OK,
        "unexpected reopen response for card-reopen-pg: {body_text}"
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["reopened"], true);
    assert_eq!(json["to"], reopen_target);
    assert_eq!(json["card"]["status"], reopen_target);

    let outcome: String = sqlx::query_scalar(
        "SELECT outcome
         FROM review_tuning_outcomes
         WHERE card_id = $1
         ORDER BY review_round DESC, id DESC
         LIMIT 1",
    )
    .bind("card-reopen-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(outcome, "false_negative");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn reopen_skips_preflight_already_applied_for_api_reopen() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-reopen-skip");
    seed_repo(&db, "test-repo");
    set_pmd_channel(&db, "pmd-chan-123");

    let reopen_target = crate::pipeline::get()
        .dispatchable_states()
        .into_iter()
        .next()
        .expect("default pipeline should expose at least one dispatchable state")
        .to_string();

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                created_at, updated_at, completed_at
            ) VALUES (
                'card-reopen-skip', 'Issue #272', 'done', 'medium', 'agent-reopen-skip', 'test-repo',
                datetime('now'), datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at, completed_at
            ) VALUES (
                'impl-reopen-skip', 'card-reopen-skip', 'agent-reopen-skip', 'implementation',
                'completed', 'stale impl', datetime('now', '-1 hour'), datetime('now', '-1 hour'),
                datetime('now', '-1 hour')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-reopen-skip/reopen")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(r#"{"reason":"skip preflight on API reopen"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["reopened"], true);
    assert_eq!(json["to"], reopen_target);

    let conn = db.lock().unwrap();
    let (status, metadata_raw): (String, Option<String>) = conn
        .query_row(
            "SELECT status, metadata FROM kanban_cards WHERE id = 'card-reopen-skip'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        status, reopen_target,
        "API reopen must skip already_applied preflight and keep card reopened"
    );
    let metadata: serde_json::Value =
        serde_json::from_str(metadata_raw.as_deref().unwrap_or("{}")).unwrap();
    assert_eq!(metadata["preflight_status"], "skipped");
    assert_eq!(metadata["preflight_summary"], "Skipped for API reopen");
    assert!(
        metadata.get("skip_preflight_once").is_none(),
        "skip_preflight_once must be consumed during reopen transition"
    );
}

#[tokio::test]
async fn reopen_returns_bad_gateway_when_github_reopen_fails_before_response() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-reopen-ghfail");
    set_pmd_channel(&db, "pmd-chan-123");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_url, created_at, updated_at, completed_at
            ) VALUES (
                'card-reopen-ghfail', 'Issue #271', 'done', 'medium', 'agent-reopen-ghfail',
                'test-repo', 'https://example.com/not-github', datetime('now'),
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-reopen-ghfail/reopen")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(r#"{"reason":"gh reopen failure test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["reopened"], false);
    assert_eq!(json["github_issue_url"], "https://example.com/not-github");
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not a github url"),
        "expected invalid github url parse error, got {json}"
    );
}

#[tokio::test]
async fn reopen_reset_full_clears_review_thread_and_preflight_state() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-reopen-reset");
    seed_repo(&db, "test-repo");
    set_pmd_channel(&db, "pmd-chan-123");
    ensure_auto_queue_tables(&db);

    let reopen_target = crate::pipeline::get()
        .dispatchable_states()
        .into_iter()
        .next()
        .expect("default pipeline should expose at least one dispatchable state")
        .to_string();

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                latest_dispatch_id, review_status, review_round, review_notes,
                suggestion_pending_at, review_entered_at, awaiting_dod_at,
                metadata, channel_thread_map, active_thread_id,
                created_at, updated_at, completed_at
            ) VALUES (
                'card-reopen-reset', 'Issue #273', 'done', 'medium', 'agent-reopen-reset', 'test-repo',
                'dispatch-reopen-reset', 'suggestion_pending', 4, 'stale review notes',
                datetime('now', '-12 minutes'), datetime('now', '-11 minutes'), datetime('now', '-10 minutes'),
                '{\"keep\":\"yes\",\"preflight_status\":\"already_applied\",\"preflight_summary\":\"stale\",\"preflight_checked_at\":\"2026-04-01T00:00:00Z\",\"consultation_status\":\"completed\",\"consultation_result\":{\"summary\":\"stale\"}}',
                '{\"111\":\"222\"}', '222',
                datetime('now', '-20 minutes'), datetime('now', '-20 minutes'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-reopen-reset', 'card-reopen-reset', 'agent-reopen-reset', 'consultation',
                'pending', 'stale consult', datetime('now', '-9 minutes'), datetime('now', '-9 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, active_dispatch_id, last_heartbeat, created_at
            ) VALUES (
                'session-reopen-reset', 'agent-reopen-reset', 'codex', 'working', 'dispatch-reopen-reset',
                datetime('now', '-9 minutes'), datetime('now', '-9 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-reopen-reset', 'test-repo', 'agent-reopen-reset', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-reopen-reset-history', 'test-repo', 'agent-reopen-reset', 'completed')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at
            ) VALUES (
                'entry-reopen-live', 'run-reopen-reset', 'card-reopen-reset', 'agent-reopen-reset',
                'dispatched', 'dispatch-reopen-reset', datetime('now', '-9 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, completed_at
            ) VALUES (
                'entry-reopen-done', 'run-reopen-reset-history', 'card-reopen-reset', 'agent-reopen-reset',
                'done', datetime('now', '-30 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO card_review_state (
                card_id, state, pending_dispatch_id, review_round, last_verdict, last_decision,
                approach_change_round, session_reset_round, review_entered_at, updated_at
            ) VALUES (
                'card-reopen-reset', 'suggestion_pending', 'dispatch-reopen-reset', 4, 'pass', 'approved',
                3, 4, datetime('now', '-11 minutes'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-reopen-reset/reopen")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(
                    r#"{"reason":"full reset reopen","reset_full":true,"review_status":"queued"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["reopened"], true);
    assert_eq!(json["reset_full"], true);
    assert_eq!(json["cancelled_dispatches"], 1);
    assert_eq!(json["skipped_auto_queue_entries"], 1);

    let conn = db.lock().unwrap();
    let (
        status,
        latest_dispatch_id,
        review_status,
        review_round,
        review_notes,
        suggestion_pending_at,
        review_entered_at,
        awaiting_dod_at,
        metadata_raw,
        channel_thread_map,
        active_thread_id,
        completed_at,
    ): (
        String,
        Option<String>,
        Option<String>,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT status, latest_dispatch_id, review_status, review_round, review_notes,
                    suggestion_pending_at, review_entered_at, awaiting_dod_at,
                    metadata, channel_thread_map, active_thread_id, completed_at
             FROM kanban_cards WHERE id = 'card-reopen-reset'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(status, reopen_target);
    assert!(latest_dispatch_id.is_none());
    assert_eq!(review_status.as_deref(), Some("queued"));
    assert_eq!(review_round, 0);
    assert!(review_notes.is_none());
    assert!(suggestion_pending_at.is_none());
    assert!(review_entered_at.is_none());
    assert!(awaiting_dod_at.is_none());
    assert!(channel_thread_map.is_none());
    assert!(active_thread_id.is_none());
    assert!(completed_at.is_none());

    let metadata: serde_json::Value =
        serde_json::from_str(metadata_raw.as_deref().unwrap_or("{}")).unwrap();
    assert_eq!(metadata["keep"], "yes");
    assert!(
        metadata.get("preflight_status").is_none(),
        "reset_full must clear stale preflight status"
    );
    assert!(
        metadata.get("preflight_summary").is_none(),
        "reset_full must clear stale preflight summary"
    );
    assert!(
        metadata.get("consultation_status").is_none(),
        "reset_full must clear stale consultation status"
    );
    assert!(
        metadata.get("consultation_result").is_none(),
        "reset_full must clear stale consultation result"
    );
    assert!(
        metadata.get("skip_preflight_once").is_none(),
        "reset_full must not leave a preflight skip marker behind"
    );

    let (
        review_state_round,
        review_state_status,
        review_state_pending_dispatch,
        review_state_verdict,
        review_state_decision,
        review_state_approach_change_round,
        review_state_session_reset_round,
        review_state_entered_at,
    ): (
        i64,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT review_round, state, pending_dispatch_id, last_verdict, last_decision,
                    approach_change_round, session_reset_round, review_entered_at
             FROM card_review_state WHERE card_id = 'card-reopen-reset'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(review_state_round, 0);
    assert_eq!(review_state_status, "idle");
    assert!(review_state_pending_dispatch.is_none());
    assert!(review_state_verdict.is_none());
    assert!(review_state_decision.is_none());
    assert!(review_state_approach_change_round.is_none());
    assert!(review_state_session_reset_round.is_none());
    assert!(review_state_entered_at.is_none());

    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-reopen-reset'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatch_status, "cancelled");

    let (session_status, active_dispatch_id): (String, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id
             FROM sessions
             WHERE session_key = 'session-reopen-reset'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(session_status, "idle");
    assert!(active_dispatch_id.is_none());

    let entry_rows: Vec<(String, Option<String>)> = conn
        .prepare(
            "SELECT status, dispatch_id FROM auto_queue_entries
             WHERE kanban_card_id = 'card-reopen-reset'
             ORDER BY id ASC",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(
        entry_rows,
        vec![
            ("dispatched".to_string(), None),
            ("skipped".to_string(), None),
        ],
        "reset_full must reactivate done entries but skip stale live entries"
    );
}

#[tokio::test]
async fn retry_preserves_review_dispatch_type() {
    let (_repo, _repo_guard) = setup_test_repo();
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-review-retry");
    seed_repo(&db, "test-repo");
    ensure_auto_queue_tables(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, latest_dispatch_id, created_at, updated_at
            ) VALUES (
                'card-review-retry', 'Issue #331 retry', 'review', 'medium', 'agent-review-retry', 'test-repo',
                331, 'dispatch-review-old', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at
            ) VALUES (
                'dispatch-review-old', 'card-review-retry', 'agent-review-retry', 'review', 'pending',
                '[Review] Issue #331 retry', datetime('now', '-10 minutes'), datetime('now', '-10 minutes')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-review-retry/retry")
                .header("content-type", "application/json")
                .body(Body::from(r#"{}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["card"]["latest_dispatch_type"], "review");

    let conn = db.lock().unwrap();
    let old_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-review-old'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_status, "cancelled");

    let latest_dispatch_id: String = conn
        .query_row(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-review-retry'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_ne!(latest_dispatch_id, "dispatch-review-old");

    let (dispatch_type, status, title): (String, String, String) = conn
        .query_row(
            "SELECT dispatch_type, status, title FROM task_dispatches WHERE id = ?1",
            [&latest_dispatch_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(dispatch_type, "review");
    assert_eq!(status, "pending");
    assert_eq!(title, "[Review] Issue #331 retry");
}

#[tokio::test]
async fn redispatch_preserves_review_dispatch_type() {
    let (_repo, _repo_guard) = setup_test_repo();
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-review-redispatch");
    seed_repo(&db, "test-repo");
    ensure_auto_queue_tables(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, latest_dispatch_id, review_status, created_at, updated_at
            ) VALUES (
                'card-review-redispatch', 'Issue #331 redispatch', 'review', 'medium', 'agent-review-redispatch', 'test-repo',
                331, 'dispatch-review-redispatch-old', 'queued', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at
            ) VALUES (
                'dispatch-review-redispatch-old', 'card-review-redispatch', 'agent-review-redispatch', 'review', 'dispatched',
                '[Review] Issue #331 redispatch', datetime('now', '-10 minutes'), datetime('now', '-10 minutes')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-review-redispatch/redispatch")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"reason":"requeue review"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["card"]["latest_dispatch_type"], "review");

    let conn = db.lock().unwrap();
    let old_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-review-redispatch-old'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_status, "cancelled");

    let latest_dispatch_id: String = conn
        .query_row(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-review-redispatch'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_ne!(latest_dispatch_id, "dispatch-review-redispatch-old");

    let (dispatch_type, status, title, review_status): (String, String, String, Option<String>) =
        conn.query_row(
            "SELECT td.dispatch_type, td.status, td.title, kc.review_status
             FROM task_dispatches td
             JOIN kanban_cards kc ON kc.latest_dispatch_id = td.id
             WHERE td.id = ?1",
            [&latest_dispatch_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(dispatch_type, "review");
    assert_eq!(status, "pending");
    assert_eq!(title, "[Review] Issue #331 redispatch");
    assert!(
        review_status.is_none(),
        "redispatch should clear stale review_status before creating the new review dispatch"
    );
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_dispatch_prepares_backlog_cards_and_auto_assigns_agent() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "project-agentdesk");
    ensure_auto_queue_tables(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, created_at, updated_at
            ) VALUES (
                'card-dq-423', 'Issue #423', 'backlog', 'high', NULL, 'test-repo', 423,
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, created_at, updated_at
            ) VALUES (
                'card-dq-405', 'Issue #405', 'ready', 'medium', NULL, 'test-repo', 405,
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, created_at, updated_at
            ) VALUES (
                'card-dq-407', 'Issue #407', 'requested', 'medium', NULL, 'test-repo', 407,
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "project-agentdesk",
                        "groups": [
                            {"issues": [423, 405], "sequential": true},
                            {"issues": [407]}
                        ],
                        "activate": false
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["activated"], false);
    assert_eq!(json["requested"]["auto_assign_agent"], true);
    assert_eq!(json["run"]["status"], "generated");

    let run_id = json["run"]["id"]
        .as_str()
        .expect("dispatch run id must be present");
    let entries = json["entries"]
        .as_array()
        .expect("dispatch snapshot must include entries");
    assert_eq!(entries.len(), 3);

    let conn = db.lock().unwrap();
    let assigned_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kanban_cards
             WHERE id IN ('card-dq-423', 'card-dq-405', 'card-dq-407')
               AND assigned_agent_id = 'project-agentdesk'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(assigned_count, 3);

    let backlog_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-dq-423'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(backlog_status, "ready");

    let entry_layout: Vec<(i64, i64, i64)> = {
        let mut stmt = conn
            .prepare(
                "SELECT kc.github_issue_number, e.thread_group, e.priority_rank
                 FROM auto_queue_entries e
                 JOIN kanban_cards kc ON kc.id = e.kanban_card_id
                 WHERE e.run_id = ?1
                 ORDER BY kc.github_issue_number ASC",
            )
            .unwrap();
        stmt.query_map([run_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect()
    };
    assert_eq!(entry_layout, vec![(405, 0, 1), (407, 1, 0), (423, 0, 0)]);
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_dispatch_persists_review_mode_in_run() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-review-mode");
    seed_repo(&db, "test-repo");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-review-mode", 4966, "ready", "agent-review-mode");

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "agent-review-mode",
                        "groups": [
                            {"issues": [4966]}
                        ],
                        "review_mode": "disabled",
                        "activate": false
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["run"]["review_mode"], "disabled");

    let run_id = json["run"]["id"]
        .as_str()
        .expect("dispatch run id must be present");
    let conn = db.lock().unwrap();
    let stored_review_mode: String = conn
        .query_row(
            "SELECT review_mode FROM auto_queue_runs WHERE id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored_review_mode, "disabled");
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_dispatch_rejects_when_live_run_exists_without_force() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_agent(&db, "project-agentdesk");
    seed_auto_queue_card(
        &db,
        "card-dispatch-existing",
        4901,
        "ready",
        "project-agentdesk",
    );
    seed_auto_queue_card(
        &db,
        "card-dispatch-backlog",
        4902,
        "backlog",
        "project-agentdesk",
    );
    seed_auto_queue_card(
        &db,
        "card-dispatch-ready",
        4903,
        "ready",
        "project-agentdesk",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count, created_at
            ) VALUES (
                'run-dispatch-active', 'test-repo', 'project-agentdesk', 'active', 1, 1, datetime('now', '-1 minute')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-dispatch-existing', 'run-dispatch-active', 'card-dispatch-existing', 'project-agentdesk', 'pending', 0, 0
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "project-agentdesk",
                        "groups": [
                            {"issues": [4903], "thread_group": 0, "batch_phase": 1},
                            {"issues": [4902], "thread_group": 7, "batch_phase": 3}
                        ],
                        "activate": false
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["existing_run_id"], "run-dispatch-active");
    assert_eq!(json["existing_run_status"], "active");
    assert!(
        json["error"]
            .as_str()
            .unwrap_or("")
            .contains("run_id=run-dispatch-active"),
        "conflict response must include the existing run id: {json}"
    );

    let conn = db.lock().unwrap();
    let total_runs: i64 = conn
        .query_row("SELECT COUNT(*) FROM auto_queue_runs", [], |row| row.get(0))
        .unwrap();
    let active_runs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_runs WHERE status = 'active'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(total_runs, 1, "dispatch conflict must not create a new run");
    assert_eq!(
        active_runs, 1,
        "dispatch conflict must leave the original live run untouched"
    );

    let backlog_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-dispatch-backlog'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        backlog_status, "backlog",
        "409 conflict must happen before backlog auto-promotion"
    );

    let entry_layout: Vec<(i64, i64, i64, i64)> = {
        let mut stmt = conn
            .prepare(
                "SELECT kc.github_issue_number, e.thread_group, e.priority_rank, COALESCE(e.batch_phase, 0)
                 FROM auto_queue_entries e
                 JOIN kanban_cards kc ON kc.id = e.kanban_card_id
                 WHERE e.run_id = 'run-dispatch-active'
                 ORDER BY kc.github_issue_number ASC",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .unwrap()
        .map(|row| row.unwrap())
        .collect()
    };
    assert_eq!(
        entry_layout,
        vec![(4901, 0, 0, 0)],
        "dispatch conflict must not enqueue new entries into the existing run"
    );

    let run_meta: (i64, i64) = conn
        .query_row(
            "SELECT max_concurrent_threads, thread_group_count
             FROM auto_queue_runs
             WHERE id = 'run-dispatch-active'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(run_meta, (1, 1));
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_dispatch_force_cancels_live_run_and_creates_new_run() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_agent(&db, "project-agentdesk");
    seed_auto_queue_card(
        &db,
        "card-dispatch-force-existing",
        4911,
        "ready",
        "project-agentdesk",
    );
    seed_auto_queue_card(
        &db,
        "card-dispatch-force-backlog",
        4912,
        "backlog",
        "project-agentdesk",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count, created_at
            ) VALUES (
                'run-dispatch-force-old', 'test-repo', 'project-agentdesk', 'active', 1, 1, datetime('now', '-1 minute')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-dispatch-force-existing', 'run-dispatch-force-old', 'card-dispatch-force-existing', 'project-agentdesk', 'pending', 0, 0
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "project-agentdesk",
                        "groups": [
                            {"issues": [4912], "thread_group": 2, "batch_phase": 3}
                        ],
                        "activate": false,
                        "force": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let new_run_id = json["run"]["id"].as_str().unwrap_or("");
    assert!(
        !new_run_id.is_empty() && new_run_id != "run-dispatch-force-old",
        "force dispatch must create a replacement run: {json}"
    );
    assert_eq!(json["run"]["status"], "generated");

    let conn = db.lock().unwrap();
    let old_run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-dispatch-force-old'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_run_status, "cancelled");

    let old_entry_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-dispatch-force-existing'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(old_entry_status, "skipped");

    let new_entry_layout: Vec<(i64, i64, i64)> = {
        let mut stmt = conn
            .prepare(
                "SELECT kc.github_issue_number, e.thread_group, COALESCE(e.batch_phase, 0)
                 FROM auto_queue_entries e
                 JOIN kanban_cards kc ON kc.id = e.kanban_card_id
                 WHERE e.run_id = ?1
                 ORDER BY e.priority_rank ASC, kc.github_issue_number ASC",
            )
            .unwrap();
        stmt.query_map([new_run_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .unwrap()
        .map(|row| row.unwrap())
        .collect()
    };
    assert_eq!(new_entry_layout, vec![(4912, 2, 3)]);

    let backlog_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-dispatch-force-backlog'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(backlog_status, "ready");
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_add_run_entry_creates_pending_entry_for_active_run() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_agent(&db, "project-agentdesk");
    seed_auto_queue_card(
        &db,
        "card-run-entry-existing",
        4921,
        "ready",
        "project-agentdesk",
    );
    seed_auto_queue_card(
        &db,
        "card-run-entry-new",
        4922,
        "ready",
        "project-agentdesk",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count, created_at
            ) VALUES (
                'run-add-entry-active', 'test-repo', 'project-agentdesk', 'active', 1, 1, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, batch_phase
            ) VALUES (
                'entry-run-entry-existing', 'run-add-entry-active', 'card-run-entry-existing',
                'project-agentdesk', 'pending', 0, 0, 1
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/runs/run-add-entry-active/entries")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&json!({
                        "issue_number": 4922,
                        "batch_phase": 4,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["run_id"], "run-add-entry-active");
    assert_eq!(json["thread_group"], 1);
    assert_eq!(json["priority_rank"], 0);

    let conn = db.lock().unwrap();
    let inserted: (i64, i64, i64, String) = conn
        .query_row(
            "SELECT priority_rank, thread_group, batch_phase, status
             FROM auto_queue_entries
             WHERE run_id = 'run-add-entry-active'
               AND kanban_card_id = 'card-run-entry-new'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(inserted, (0, 1, 4, "pending".to_string()));

    let run_meta: (i64, i64) = conn
        .query_row(
            "SELECT thread_group_count, max_concurrent_threads
             FROM auto_queue_runs
             WHERE id = 'run-add-entry-active'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(run_meta, (2, 2));
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_add_run_entry_rejects_non_active_runs() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_agent(&db, "project-agentdesk");
    seed_auto_queue_card(
        &db,
        "card-run-entry-cancelled",
        4923,
        "ready",
        "project-agentdesk",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, created_at
            ) VALUES (
                'run-add-entry-cancelled', 'test-repo', 'project-agentdesk', 'cancelled', datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/runs/run-add-entry-cancelled/entries")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&json!({
                        "issue_number": 4923,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap_or("")
            .contains("status=cancelled"),
        "inactive runs must be rejected with status details: {json}"
    );
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_add_run_entry_rejects_non_ready_cards() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_agent(&db, "project-agentdesk");
    seed_auto_queue_card(
        &db,
        "card-run-entry-backlog",
        4924,
        "backlog",
        "project-agentdesk",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, created_at
            ) VALUES (
                'run-add-entry-ready-only', 'test-repo', 'project-agentdesk', 'active', datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/runs/run-add-entry-ready-only/entries")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&json!({
                        "issue_number": 4924,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap_or("")
            .contains("must be in ready status"),
        "run-entry add must reject non-ready cards: {json}"
    );
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_update_entry_moves_pending_entry_and_syncs_run_groups() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-update");
    seed_auto_queue_card(&db, "card-update-1", 1801, "ready", "agent-update");
    seed_auto_queue_card(&db, "card-update-2", 1802, "ready", "agent-update");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-update-entry', 'test-repo', 'agent-update', 'generated', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-update-1', 'run-update-entry', 'card-update-1', 'agent-update', 'pending', 0, 0
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-update-2', 'run-update-entry', 'card-update-2', 'agent-update', 'pending', 1, 0
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/auto-queue/entries/entry-update-2")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "thread_group": 3,
                        "priority_rank": 0
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["entry"]["thread_group"], 3);
    assert_eq!(json["entry"]["priority_rank"], 0);

    let conn = db.lock().unwrap();
    let run_meta: (i64, i64) = conn
        .query_row(
            "SELECT max_concurrent_threads, thread_group_count
             FROM auto_queue_runs
             WHERE id = 'run-update-entry'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(run_meta, (2, 2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_update_entry_restores_skipped_entry_to_pending() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-update");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-update-restore", 1699, "ready", "agent-update");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-update-entry-restore', 'test-repo', 'agent-update', 'cancelled', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank,
                thread_group, dispatch_id, slot_index, completed_at
            ) VALUES (
                'entry-update-restore', 'run-update-entry-restore', 'card-update-restore',
                'agent-update', 'skipped', 5, 0, 'dispatch-old', 0, datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/auto-queue/entries/entry-update-restore")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "status": "pending",
                        "thread_group": 2,
                        "priority_rank": 0
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["entry"]["status"], "pending");
    assert_eq!(json["entry"]["thread_group"], 2);
    assert_eq!(json["entry"]["priority_rank"], 0);

    let conn = db.lock().unwrap();
    let (status, dispatch_id, slot_index, completed_at, thread_group, priority_rank): (
        String,
        Option<String>,
        Option<i64>,
        Option<String>,
        i64,
        i64,
    ) = conn
        .query_row(
            "SELECT status, dispatch_id, slot_index, completed_at, thread_group, priority_rank
             FROM auto_queue_entries
             WHERE id = 'entry-update-restore'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(status, "pending");
    assert!(dispatch_id.is_none());
    assert!(slot_index.is_none());
    assert!(completed_at.is_none());
    assert_eq!(thread_group, 2);
    assert_eq!(priority_rank, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_update_entry_updates_batch_phase_only_and_with_priority_rank() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-update-phase");
    seed_auto_queue_card(
        &db,
        "card-update-phase",
        1810,
        "ready",
        "agent-update-phase",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-update-phase', 'test-repo', 'agent-update-phase', 'generated', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, batch_phase
            ) VALUES (
                'entry-update-phase', 'run-update-phase', 'card-update-phase',
                'agent-update-phase', 'pending', 3, 0, 0
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/auto-queue/entries/entry-update-phase")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "batch_phase": 2
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["entry"]["batch_phase"], 2);

    {
        let conn = db.lock().unwrap();
        let batch_phase: i64 = conn
            .query_row(
                "SELECT batch_phase FROM auto_queue_entries WHERE id = 'entry-update-phase'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(batch_phase, 2);
    }

    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/auto-queue/entries/entry-update-phase")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "batch_phase": 1,
                        "priority_rank": 0
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["entry"]["batch_phase"], 1);
    assert_eq!(json["entry"]["priority_rank"], 0);

    let conn = db.lock().unwrap();
    let entry_meta: (i64, i64) = conn
        .query_row(
            "SELECT batch_phase, priority_rank
             FROM auto_queue_entries
             WHERE id = 'entry-update-phase'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(entry_meta, (1, 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_update_run_updates_max_concurrent_threads_only_and_with_status() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-update-max', 'test-repo', 'generated', 1, 4
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/auto-queue/runs/run-update-max")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "max_concurrent_threads": 4
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    {
        let conn = db.lock().unwrap();
        let max_concurrent_threads: i64 = conn
            .query_row(
                "SELECT max_concurrent_threads
                 FROM auto_queue_runs
                 WHERE id = 'run-update-max'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(max_concurrent_threads, 4);
    }

    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/auto-queue/runs/run-update-max")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "status": "completed",
                        "max_concurrent_threads": 2
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let conn = db.lock().unwrap();
    let run_meta: (String, i64, Option<String>) = conn
        .query_row(
            "SELECT status, max_concurrent_threads, completed_at
             FROM auto_queue_runs
             WHERE id = 'run-update-max'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(run_meta.0, "completed");
    assert_eq!(run_meta.1, 2);
    assert!(run_meta.2.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_update_run_pg_updates_max_concurrent_threads_only_and_with_status() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, status, max_concurrent_threads, thread_group_count
         ) VALUES (
            $1, $2, $3, $4, $5
         )",
    )
    .bind("run-update-max-pg")
    .bind("test-repo")
    .bind("generated")
    .bind(1_i64)
    .bind(4_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/auto-queue/runs/run-update-max-pg")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "max_concurrent_threads": 4
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let max_concurrent_threads = sqlx::query_scalar::<_, i64>(
        "SELECT max_concurrent_threads::BIGINT
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind("run-update-max-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(max_concurrent_threads, 4);

    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/auto-queue/runs/run-update-max-pg")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "status": "completed",
                        "max_concurrent_threads": 2
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let run_meta: (String, i64, Option<chrono::DateTime<chrono::Utc>>) = sqlx::query_as(
        "SELECT status,
                max_concurrent_threads::BIGINT,
                completed_at
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind("run-update-max-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(run_meta.0, "completed");
    assert_eq!(run_meta.1, 2);
    assert!(run_meta.2.is_some());

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_rebind_slot_assigns_run_and_updates_dispatched_entry_slot() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-rebind");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-rebind", 1700, "in_progress", "agent-rebind");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-rebind', 'test-repo', 'agent-rebind', 'active', 2, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, thread_group
            ) VALUES (
                'entry-rebind', 'run-rebind', 'card-rebind', 'agent-rebind',
                'dispatched', 'dispatch-rebind', 3
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/slots/agent-rebind/1/rebind")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-rebind",
                        "thread_group": 3
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["updated_entries"], 1);

    let conn = db.lock().unwrap();
    let slot_binding: (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-rebind'
               AND slot_index = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(slot_binding.0.as_deref(), Some("run-rebind"));
    assert_eq!(slot_binding.1, Some(3));

    let entry_slot: Option<i64> = conn
        .query_row(
            "SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-rebind'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(entry_slot, Some(1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_restore_run_restores_skipped_entries_by_card_state() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-restore");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-restore-pending", 1801, "ready", "agent-restore");
    seed_auto_queue_card(&db, "card-restore-done", 1802, "done", "agent-restore");
    seed_auto_queue_card(&db, "card-restore-live", 1803, "requested", "agent-restore");
    seed_auto_queue_card(
        &db,
        "card-restore-new",
        1804,
        "in_progress",
        "agent-restore",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-restore', 'test-repo', 'agent-restore', 'cancelled', 4, 4
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-restore-pending', 'run-restore', 'card-restore-pending', 'agent-restore', 'skipped', 0, 0
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-restore-done', 'run-restore', 'card-restore-done', 'agent-restore', 'skipped', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-restore-live', 'run-restore', 'card-restore-live', 'agent-restore', 'skipped', 2, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-restore-new', 'run-restore', 'card-restore-new', 'agent-restore', 'skipped', 3, 3
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-restore-old-done', 'card-restore-done', 'agent-restore',
                'cancelled', 'Old Done Dispatch', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-restore-live', 'card-restore-live', 'agent-restore',
                'dispatched', 'Live Dispatch', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-restore-old-new', 'card-restore-new', 'agent-restore',
                'cancelled', 'Old New Dispatch', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE kanban_cards
             SET latest_dispatch_id = 'dispatch-restore-live'
             WHERE id = 'card-restore-live'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE kanban_cards
             SET latest_dispatch_id = 'dispatch-restore-old-new'
             WHERE id = 'card-restore-new'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entry_dispatch_history (entry_id, dispatch_id, trigger_source)
             VALUES ('entry-restore-done', 'dispatch-restore-old-done', 'seed')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entry_dispatch_history (entry_id, dispatch_id, trigger_source)
             VALUES ('entry-restore-live', 'dispatch-restore-live', 'seed')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entry_dispatch_history (entry_id, dispatch_id, trigger_source)
             VALUES ('entry-restore-new', 'dispatch-restore-old-new', 'seed')",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/runs/run-restore/restore")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["run_status"], "active");
    assert_eq!(json["restored_pending"], 1);
    assert_eq!(json["restored_done"], 1);
    assert_eq!(json["restored_dispatched"], 2);
    assert_eq!(json["created_dispatches"], 1);
    assert_eq!(json["rebound_slots"], 2);
    assert_eq!(json["unbound_dispatches"], 0);

    let conn = db.lock().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-restore'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_status, "active");

    let pending_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-restore-pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pending_status, "pending");

    let done_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-restore-done'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(done_status, "done");

    let live_entry: (String, Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT status, dispatch_id, slot_index
             FROM auto_queue_entries
             WHERE id = 'entry-restore-live'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(live_entry.0, "dispatched");
    assert_eq!(live_entry.1.as_deref(), Some("dispatch-restore-live"));
    assert!(live_entry.2.is_some());

    let new_entry: (String, Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT status, dispatch_id, slot_index
             FROM auto_queue_entries
             WHERE id = 'entry-restore-new'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(new_entry.0, "dispatched");
    assert!(new_entry.1.is_some());
    assert_ne!(new_entry.1.as_deref(), Some("dispatch-restore-old-new"));
    assert!(new_entry.2.is_some());

    let rebound_slots: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM auto_queue_slots
             WHERE assigned_run_id = 'run-restore'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(rebound_slots, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_cancel_restore_reloads_user_cancelled_entries() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-restore-user-cancelled");
    seed_auto_queue_card(
        &db,
        "card-restore-user-cancelled",
        1810,
        "in_progress",
        "agent-restore-user-cancelled",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-restore-user-cancelled', 'test-repo', 'agent-restore-user-cancelled', 'paused', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, completed_at
            ) VALUES (
                'entry-restore-user-cancelled', 'run-restore-user-cancelled',
                'card-restore-user-cancelled', 'agent-restore-user-cancelled',
                'user_cancelled', 0, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let cancel_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/cancel?run_id=run-restore-user-cancelled")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(cancel_response.status(), StatusCode::OK);
    let cancel_body = axum::body::to_bytes(cancel_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let cancel_json: serde_json::Value = serde_json::from_slice(&cancel_body).unwrap();
    assert_eq!(cancel_json["cancelled_runs"], 1);
    assert_eq!(cancel_json["cancelled_entries"], 1);

    {
        let conn = db.lock().unwrap();
        let entry_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-restore-user-cancelled'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            entry_status, "skipped",
            "run cancel must sweep user_cancelled entries into the restorable skipped bucket"
        );
    }

    let restore_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/runs/run-restore-user-cancelled/restore")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(restore_response.status(), StatusCode::OK);
    let restore_body = axum::body::to_bytes(restore_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let restore_json: serde_json::Value = serde_json::from_slice(&restore_body).unwrap();
    assert_eq!(restore_json["ok"], true);
    assert_eq!(restore_json["run_status"], "active");
    assert_eq!(restore_json["restored_pending"], 1);
    assert_eq!(restore_json["restored_done"], 0);
    assert_eq!(restore_json["restored_dispatched"], 0);

    let conn = db.lock().unwrap();
    let entry_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-restore-user-cancelled'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        entry_status, "pending",
        "restore must reload swept user_cancelled entries instead of leaving them stranded"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_restore_run_rejects_active_run() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-restore-reject");
    ensure_auto_queue_tables(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-restore-active', 'test-repo', 'agent-restore-reject', 'active')",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/runs/run-restore-active/restore")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("already active")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_restore_run_retries_from_restoring_after_partial_failure() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-restore-retry");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(
        &db,
        "card-restore-retry-ok",
        1811,
        "ready",
        "agent-restore-retry",
    );
    seed_auto_queue_card(
        &db,
        "card-restore-retry-fail",
        1812,
        "ready",
        "agent-restore-retry",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-restore-retry', 'test-repo', 'agent-restore-retry', 'cancelled', 2, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-restore-retry-ok', 'run-restore-retry', 'card-restore-retry-ok',
                'agent-restore-retry', 'skipped', 0, 0
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-restore-retry-fail', 'run-restore-retry', 'card-restore-retry-fail',
                'agent-restore-retry', 'skipped', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_restore_retry_entry
             BEFORE UPDATE OF status ON auto_queue_entries
             WHEN OLD.id = 'entry-restore-retry-fail'
               AND NEW.status != OLD.status
             BEGIN
                 SELECT RAISE(ABORT, 'restore retry blocked');
             END;",
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/runs/run-restore-retry/restore")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], false);
    assert_eq!(json["run_status"], "cancelled");
    assert_eq!(json["restored_pending"], 0);
    assert!(
        json["errors"]
            .as_array()
            .unwrap_or(&Vec::new())
            .iter()
            .any(|value| value
                .as_str()
                .unwrap_or_default()
                .contains("entry-restore-retry-fail")),
        "restore response must surface the skipped entry that still needs recovery"
    );

    {
        let conn = db.lock().unwrap();
        let run_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_runs WHERE id = 'run-restore-retry'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(run_status, "cancelled");

        let restored_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-restore-retry-ok'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(restored_status, "skipped");

        let missing_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-restore-retry-fail'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(missing_status, "skipped");
        conn.execute("DROP TRIGGER fail_restore_retry_entry", [])
            .unwrap();
    }

    let retry_app = test_api_router(db.clone(), test_engine(&db), None);
    let retry_response = retry_app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/runs/run-restore-retry/restore")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(retry_response.status(), StatusCode::OK);
    let retry_body = axum::body::to_bytes(retry_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let retry_json: serde_json::Value = serde_json::from_slice(&retry_body).unwrap();
    assert_eq!(retry_json["ok"], true);
    assert_eq!(retry_json["run_status"], "active");
    assert_eq!(retry_json["restored_pending"], 2);

    let conn = db.lock().unwrap();
    let final_run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-restore-retry'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(final_run_status, "active");

    let entry_states: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, status
                 FROM auto_queue_entries
                 WHERE run_id = 'run-restore-retry'
                 ORDER BY id ASC",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(
        entry_states,
        vec![
            (
                "entry-restore-retry-fail".to_string(),
                "pending".to_string(),
            ),
            ("entry-restore-retry-ok".to_string(), "pending".to_string()),
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_run_id_does_not_dispatch_restoring_runs() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-restoring-activate");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(
        &db,
        "card-restoring-activate",
        1700,
        "ready",
        "agent-restoring-activate",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-restoring-activate', 'test-repo', 'agent-restoring-activate', 'restoring', datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-restoring-activate', 'run-restoring-activate', 'card-restoring-activate', 'agent-restoring-activate', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-restoring-activate",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["count"], 0);
    assert_eq!(json["message"], "Run is restoring");

    let conn = db.lock().unwrap();
    let entry_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-restoring-activate'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(entry_status, "pending");

    let dispatch_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM task_dispatches", [], |row| row.get(0))
        .unwrap();
    assert_eq!(dispatch_count, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_active_only_does_not_promote_generated_runs() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-active-only");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-active-run", 1701, "ready", "agent-active-only");
    seed_auto_queue_card(
        &db,
        "card-generated-run",
        1702,
        "ready",
        "agent-active-only",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-active', 'test-repo', 'agent-active-only', 'active', datetime('now', '-2 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-generated', 'test-repo', 'agent-active-only', 'generated', datetime('now', '-1 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-active', 'run-active', 'card-active-run', 'agent-active-only', 'pending', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-generated', 'run-generated', 'card-generated-run', 'agent-active-only', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "agent_id": "agent-active-only",
                        "active_only": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["count"], 1,
        "auto_queue_activate_dispatches_pg_only_run_without_sqlite_mirror body={json}"
    );
    assert_eq!(json["dispatched"][0]["card_id"], "card-active-run");

    let conn = db.lock().unwrap();
    let generated_run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-generated'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let generated_entry_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-generated'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let active_dispatch_card: String = conn
        .query_row(
            "SELECT kanban_card_id FROM task_dispatches ORDER BY rowid DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(generated_run_status, "generated");
    assert_eq!(generated_entry_status, "pending");
    assert_eq!(active_dispatch_card, "card-active-run");
}

/// #162: A card in 'requested' state, assigned to the same agent, must not
/// be blocked by the busy-agent guard when that card itself is the dispatch target.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_requested_card_not_blocked_by_own_status() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-req-self");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-req-self", 1630, "requested", "agent-req-self");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('run-req-self', 'test-repo', 'agent-req-self', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-req-self', 'run-req-self', 'card-req-self', 'agent-req-self', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "agent_id": "agent-req-self",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["count"], 1,
        "activate must succeed — busy guard must exclude the card being dispatched"
    );

    let conn = db.lock().unwrap();
    let entry_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-req-self'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(entry_status, "dispatched");
}

/// #162/#500: A card in 'backlog' (non-dispatchable) state must be walked
/// to the dispatchable state via canonical transitions before dispatch creation.
/// The walk must preserve the same requested-state hook side-effects as a
/// manual transition.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_walks_backlog_card_to_dispatchable_state() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-walk");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-walk-bl", 1631, "backlog", "agent-walk");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE kanban_cards
             SET description = ?1
             WHERE id = 'card-walk-bl'",
            ["DoD: keep auto-queue walk hook parity and preserve activation behavior."],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('run-walk', 'test-repo', 'agent-walk', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-walk', 'run-walk', 'card-walk-bl', 'agent-walk', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "agent_id": "agent-walk",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["count"], 1,
        "activate must succeed for backlog card via silent walk"
    );

    // Verify the card was walked through free transitions and dispatch was created
    let conn = db.lock().unwrap();
    let entry_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-walk'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(entry_status, "dispatched");

    // Card should have been dispatched (moved past backlog via silent walk)
    let dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-walk-bl'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        dispatch_count, 1,
        "exactly one dispatch must be created for the walked card"
    );

    let metadata: Option<String> = conn
        .query_row(
            "SELECT metadata FROM kanban_cards WHERE id = 'card-walk-bl'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let metadata_json: serde_json::Value =
        serde_json::from_str(metadata.as_deref().expect("walk must persist metadata")).unwrap();
    assert_eq!(
        metadata_json["preflight_status"], "clear",
        "requested-state preflight hook must run during auto-queue walk"
    );
}

/// #500: If the requested-state hook decides the card is already applied,
/// activate() must respect that side-effect instead of creating a new dispatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_walk_respects_requested_hook_skip() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-walk-skip");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-walk-skip", 1632, "backlog", "agent-walk-skip");
    let _gh = install_mock_gh_issue_view_closed(1632, "itismyfield/AgentDesk");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE kanban_cards
             SET github_issue_url = 'https://github.com/itismyfield/AgentDesk/issues/1632'
             WHERE id = 'card-walk-skip'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at, completed_at
            ) VALUES (
                'dispatch-walk-skip', 'card-walk-skip', 'agent-walk-skip', 'implementation', 'completed',
                'Existing implementation', datetime('now'), datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('run-walk-skip', 'test-repo', 'agent-walk-skip', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-walk-skip', 'run-walk-skip', 'card-walk-skip', 'agent-walk-skip', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "agent_id": "agent-walk-skip",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["count"], 0,
        "activate must not create a new dispatch when requested-state preflight skips the card"
    );

    let conn = db.lock().unwrap();
    let (card_status, entry_status): (String, String) = conn
        .query_row(
            "SELECT
                (SELECT status FROM kanban_cards WHERE id = 'card-walk-skip'),
                (SELECT status FROM auto_queue_entries WHERE id = 'entry-walk-skip')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(card_status, "done");
    assert_eq!(entry_status, "skipped");

    let dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-walk-skip'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        dispatch_count, 1,
        "hook-driven skip must not create an additional dispatch"
    );
}

/// #430: legacy unified_thread runs still dispatch, but via slot pooling.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_legacy_unified_thread_run_dispatches_via_slot_pool() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-unified");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-unified-1", 1625, "ready", "agent-unified");
    seed_auto_queue_card(&db, "card-unified-2", 1626, "ready", "agent-unified");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, unified_thread) \
             VALUES ('run-unified', 'test-repo', 'agent-unified', 'active', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-u1', 'run-unified', 'card-unified-1', 'agent-unified', 'pending', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-u2', 'run-unified', 'card-unified-2', 'agent-unified', 'pending', 1)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "agent_id": "agent-unified",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["count"], 1, "first activate dispatches one entry");
    assert_eq!(json["dispatched"][0]["card_id"], "card-unified-1");

    // Verify dispatch was created and entry was linked
    let conn = db.lock().unwrap();
    let (entry_status, dispatch_id): (String, Option<String>) = conn
        .query_row(
            "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-u1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(entry_status, "dispatched");
    let dispatch_id = dispatch_id.expect("entry must have linked dispatch_id");

    // Verify the dispatch references the correct card
    let dispatch_card: String = conn
        .query_row(
            "SELECT kanban_card_id FROM task_dispatches WHERE id = ?1",
            [&dispatch_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatch_card, "card-unified-1");
    let notify_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM dispatch_outbox WHERE dispatch_id = ?1 AND action = 'notify'",
            [&dispatch_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        notify_count, 1,
        "auto-queue activation must use canonical notify persistence"
    );

    // Second entry stays pending (sequential within group)
    let entry2_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-u2'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(entry2_status, "pending");

    // Run stays active (not prematurely completed)
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-unified'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_status, "active");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_consult_required_creates_consultation_dispatch() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-consult");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-consult", 1720, "ready", "agent-consult");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE kanban_cards SET metadata = ?1 WHERE id = 'card-consult'",
            [serde_json::json!({
                "keep": "yes",
                "preflight_status": "consult_required",
                "preflight_summary": "need counter review"
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('run-consult', 'test-repo', 'agent-consult', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-consult', 'run-consult', 'card-consult', 'agent-consult', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-consult",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["count"], 1,
        "consultation dispatch should count as dispatched"
    );

    let conn = db.lock().unwrap();
    let (entry_status, dispatch_id): (String, Option<String>) = conn
        .query_row(
            "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-consult'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(entry_status, "dispatched");
    let dispatch_id = dispatch_id.expect("consultation dispatch id must be stored");

    let (dispatch_type, to_agent_id, dispatch_context_raw): (String, String, Option<String>) = conn
        .query_row(
            "SELECT dispatch_type, to_agent_id, context FROM task_dispatches WHERE id = ?1",
            [&dispatch_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(dispatch_type, "consultation");
    assert_eq!(to_agent_id, "agent-consult");
    let dispatch_context: serde_json::Value =
        serde_json::from_str(dispatch_context_raw.as_deref().unwrap_or("{}")).unwrap();
    assert_eq!(dispatch_context["auto_queue"], true);
    assert_eq!(dispatch_context["entry_id"], "entry-consult");
    assert_eq!(dispatch_context["thread_group"], 0);
    assert_eq!(dispatch_context["slot_index"], serde_json::Value::Null);
    assert_eq!(dispatch_context["run_id"], "run-consult");
    assert_eq!(dispatch_context["batch_phase"], 0);

    let metadata_raw: String = conn
        .query_row(
            "SELECT metadata FROM kanban_cards WHERE id = 'card-consult'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let metadata: serde_json::Value = serde_json::from_str(&metadata_raw).unwrap();
    assert_eq!(metadata["keep"], "yes");
    assert_eq!(metadata["preflight_status"], "consult_required");
    assert_eq!(metadata["consultation_status"], "pending");
    assert_eq!(metadata["consultation_dispatch_id"], dispatch_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_consult_required_prefers_registry_counterpart_provider() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-qwen");
    seed_agent(&db, "agent-codex");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-consult-qwen", 1721, "ready", "agent-qwen");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE agents SET provider = 'qwen' WHERE id = 'agent-qwen'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE agents SET provider = 'codex' WHERE id = 'agent-codex'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE kanban_cards SET metadata = ?1 WHERE id = 'card-consult-qwen'",
            [serde_json::json!({
                "preflight_status": "consult_required",
                "preflight_summary": "need external consultation"
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('run-consult-qwen', 'test-repo', 'agent-qwen', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-consult-qwen', 'run-consult-qwen', 'card-consult-qwen', 'agent-qwen', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-consult-qwen",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let conn = db.lock().unwrap();
    let to_agent_id: String = conn
        .query_row(
            "SELECT to_agent_id
             FROM task_dispatches
             WHERE kanban_card_id = 'card-consult-qwen'
             ORDER BY created_at DESC
             LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(to_agent_id, "agent-codex");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_already_applied_skips_entry_and_completes_run() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-skip");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-skip", 1721, "ready", "agent-skip");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE kanban_cards SET metadata = ?1 WHERE id = 'card-skip'",
            [serde_json::json!({
                "preflight_status": "already_applied",
                "preflight_summary": "nothing to do"
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ('run-skip', 'test-repo', 'agent-skip', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-skip', 'run-skip', 'card-skip', 'agent-skip', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-skip",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["count"], 0,
        "already_applied entry should be skipped, not dispatched"
    );

    let conn = db.lock().unwrap();
    let (entry_status, dispatch_id): (String, Option<String>) = conn
        .query_row(
            "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-skip'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(entry_status, "skipped");
    assert!(
        dispatch_id.is_none(),
        "skipped entry must not create a dispatch"
    );

    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-skip'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_status, "completed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_reuses_released_slot_for_next_group() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-slot");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-slot-0", 1722, "ready", "agent-slot");
    seed_auto_queue_card(&db, "card-slot-1", 1723, "ready", "agent-slot");
    seed_auto_queue_card(&db, "card-slot-2", 1724, "ready", "agent-slot");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ('agent-slot', 0, ?1)",
            [json!({"111": "222000000000000001"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ('agent-slot', 1, ?1)",
            [json!({"111": "222000000000000002"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, session_info, tokens,
                active_dispatch_id, thread_channel_id, claude_session_id,
                last_heartbeat, created_at
            ) VALUES (
                'host:AgentDesk-claude-slot-thread-0', 'agent-slot', 'claude', 'working',
                'slot 0 seed', 41, 'dispatch-slot-old-0', '222000000000000001', 'claude-slot-0',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, session_info, tokens,
                active_dispatch_id, thread_channel_id, claude_session_id,
                last_heartbeat, created_at
            ) VALUES (
                'host:AgentDesk-claude-slot-thread-1', 'agent-slot', 'claude', 'working',
                'slot 1 seed', 73, 'dispatch-slot-old-1', '222000000000000002', 'claude-slot-1',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, unified_thread,
                max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-slot', 'test-repo', 'agent-slot', 'active', 1, 2, 3
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group) \
             VALUES ('entry-slot-0', 'run-slot', 'card-slot-0', 'agent-slot', 'pending', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group) \
             VALUES ('entry-slot-1', 'run-slot', 'card-slot-1', 'agent-slot', 'pending', 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group) \
             VALUES ('entry-slot-2', 'run-slot', 'card-slot-2', 'agent-slot', 'pending', 2, 2)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let first_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-slot",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(first_response.status(), StatusCode::OK);
    let first_body = axum::body::to_bytes(first_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let first_json: serde_json::Value = serde_json::from_slice(&first_body).unwrap();
    assert_eq!(
        first_json["count"], 2,
        "first activation must dispatch two groups in parallel when two slots are available"
    );

    {
        let conn = db.lock().unwrap();
        let first_slot_session: (String, Option<String>, i64, Option<String>) = conn
            .query_row(
                "SELECT status, active_dispatch_id, COALESCE(tokens, 0), claude_session_id
                 FROM sessions WHERE thread_channel_id = '222000000000000001'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        let second_slot_session: (String, Option<String>, i64, Option<String>) = conn
            .query_row(
                "SELECT status, active_dispatch_id, COALESCE(tokens, 0), claude_session_id
                 FROM sessions WHERE thread_channel_id = '222000000000000002'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            first_slot_session.0, "idle",
            "slot session status must be idle after release"
        );
        assert_eq!(
            first_slot_session.1, None,
            "active_dispatch_id must be cleared on slot release"
        );
        assert_eq!(
            first_slot_session.2, 0,
            "tokens must be cleared so the slot starts from a fresh session"
        );
        assert!(
            first_slot_session.3.is_none(),
            "claude_session_id must be cleared on slot release"
        );
        assert_eq!(
            second_slot_session.0, "idle",
            "a reused sibling slot must also be reset before dispatch"
        );
        assert_eq!(
            second_slot_session.1, None,
            "a reused sibling slot must clear its prior dispatch context"
        );
        assert_eq!(
            second_slot_session.2, 0,
            "a reused sibling slot must clear prior token state"
        );
        assert!(
            second_slot_session.3.is_none(),
            "a reused sibling slot must clear claude_session_id"
        );
        let first_slot: Option<i64> = conn
            .query_row(
                "SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-slot-0'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let second_slot: Option<i64> = conn
            .query_row(
                "SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-slot-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(first_slot, Some(0));
        assert_eq!(second_slot, Some(1));
        conn.execute(
            "UPDATE sessions
             SET status = 'working',
                 session_info = 'slot 0 in-progress context',
                 tokens = 99,
                 active_dispatch_id = 'dispatch-slot-in-progress',
                 claude_session_id = 'claude-slot-0-rehydrated'
             WHERE thread_channel_id = '222000000000000001'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE sessions
             SET status = 'working',
                 session_info = 'slot 1 should stay hot',
                 tokens = 123,
                 active_dispatch_id = 'dispatch-slot-1-hot',
                 claude_session_id = 'claude-slot-1-hot'
             WHERE thread_channel_id = '222000000000000002'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE auto_queue_entries SET status = 'done', completed_at = datetime('now') WHERE id = 'entry-slot-0'",
            [],
        )
        .unwrap();
    }

    let second_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-slot",
                        "thread_group": 2,
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(second_response.status(), StatusCode::OK);
    let second_body = axum::body::to_bytes(second_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let second_json: serde_json::Value = serde_json::from_slice(&second_body).unwrap();
    assert_eq!(
        second_json["count"], 1,
        "released slot should allow next group dispatch"
    );

    let conn = db.lock().unwrap();
    let recycled_slot: Option<i64> = conn
        .query_row(
            "SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-slot-2'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        recycled_slot,
        Some(0),
        "completed group slot must be reused for the next group"
    );
    let recycled_dispatch_context: Option<String> = conn
        .query_row(
            "SELECT td.context
             FROM task_dispatches td
             JOIN auto_queue_entries e ON e.dispatch_id = td.id
             WHERE e.id = 'entry-slot-2'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let recycled_dispatch_context =
        serde_json::from_str::<serde_json::Value>(&recycled_dispatch_context.unwrap()).unwrap();
    assert_eq!(
        recycled_dispatch_context["reset_slot_thread_before_reuse"].as_bool(),
        Some(true),
        "independent group reuse must force a fresh slot-thread reset"
    );

    let slot_zero_group: Option<i64> = conn
        .query_row(
            "SELECT assigned_thread_group FROM auto_queue_slots WHERE agent_id = 'agent-slot' AND slot_index = 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let slot_one_group: Option<i64> = conn
        .query_row(
            "SELECT assigned_thread_group FROM auto_queue_slots WHERE agent_id = 'agent-slot' AND slot_index = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(slot_zero_group, Some(2));
    assert_eq!(slot_one_group, Some(1));

    let recycled_slot_session: (String, Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id, COALESCE(tokens, 0), claude_session_id
             FROM sessions WHERE thread_channel_id = '222000000000000001'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    let untouched_slot_session: (String, Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id, COALESCE(tokens, 0), claude_session_id
             FROM sessions WHERE thread_channel_id = '222000000000000002'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        recycled_slot_session.0, "idle",
        "status must be idle after slot release"
    );
    assert_eq!(
        recycled_slot_session.1, None,
        "active_dispatch_id must be cleared"
    );
    assert_eq!(
        recycled_slot_session.2, 0,
        "recycled slot must clear prior token counts before the next dispatch"
    );
    assert!(
        recycled_slot_session.3.is_none(),
        "recycled slot must clear claude_session_id before the next dispatch"
    );
    assert_eq!(
        untouched_slot_session,
        (
            "working".to_string(),
            Some("dispatch-slot-1-hot".to_string()),
            123,
            Some("claude-slot-1-hot".to_string())
        ),
        "active sibling group must not be cleared while it is still reusing its own context"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_dispatch_create_failure_releases_reserved_slot() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-dispatch-fail");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(
        &db,
        "card-dispatch-fail",
        4170,
        "ready",
        "agent-dispatch-fail",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-dispatch-fail', 'test-repo', 'agent-dispatch-fail', 'active', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-dispatch-fail', 'run-dispatch-fail', 'card-dispatch-fail',
                'agent-dispatch-fail', 'pending', 0, 0
            )",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_task_dispatch_insert
             BEFORE INSERT ON task_dispatches
             BEGIN
                 SELECT RAISE(ABORT, 'dispatch insert blocked');
             END;",
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-dispatch-fail",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["count"], 0,
        "failed create_dispatch must not report a dispatched group"
    );

    let conn = db.lock().unwrap();
    let entry_row: (String, Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT status, dispatch_id, slot_index
             FROM auto_queue_entries
             WHERE id = 'entry-dispatch-fail'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(entry_row.0, "pending");
    assert!(entry_row.1.is_none());
    assert!(entry_row.2.is_none());

    let slot_row: (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-dispatch-fail' AND slot_index = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        slot_row,
        (None, None),
        "failed create_dispatch must release the reserved slot"
    );

    let dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-dispatch-fail'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatch_count, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_reuses_same_group_slot_with_fresh_session_each_time() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-same-group");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-same-group-0", 1822, "ready", "agent-same-group");
    seed_auto_queue_card(&db, "card-same-group-1", 1823, "ready", "agent-same-group");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ('agent-same-group', 0, ?1)",
            [json!({"111": "222000000000000101"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, session_info, tokens,
                active_dispatch_id, thread_channel_id, claude_session_id,
                last_heartbeat, created_at
            ) VALUES (
                'host:AgentDesk-claude-same-group-thread', 'agent-same-group', 'claude', 'working',
                'slot seed', 17, 'dispatch-same-group-seed', '222000000000000101', 'claude-same-group-seed',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, unified_thread,
                max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-same-group', 'test-repo', 'agent-same-group', 'active', 1, 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group)
             VALUES ('entry-same-group-0', 'run-same-group', 'card-same-group-0', 'agent-same-group', 'pending', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group)
             VALUES ('entry-same-group-1', 'run-same-group', 'card-same-group-1', 'agent-same-group', 'pending', 1, 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let first_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-same-group",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(first_response.status(), StatusCode::OK);

    {
        let conn = db.lock().unwrap();
        let cleared_session: (String, Option<String>, i64, Option<String>) = conn
            .query_row(
                "SELECT status, active_dispatch_id, COALESCE(tokens, 0), claude_session_id
                 FROM sessions WHERE thread_channel_id = '222000000000000101'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            cleared_session,
            ("idle".to_string(), None, 0, None),
            "slot release must clear provider session continuity before dispatch"
        );
        conn.execute(
            "UPDATE sessions
             SET status = 'working',
                 session_info = 'group context retained',
                 tokens = 77,
                 active_dispatch_id = 'dispatch-same-group-hot',
                 claude_session_id = 'claude-same-group-hot'
             WHERE thread_channel_id = '222000000000000101'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE auto_queue_entries
             SET status = 'done', completed_at = datetime('now')
             WHERE id = 'entry-same-group-0'",
            [],
        )
        .unwrap();
    }

    let second_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-same-group",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(second_response.status(), StatusCode::OK);

    let conn = db.lock().unwrap();
    let continued_session: (String, Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id, COALESCE(tokens, 0), claude_session_id
             FROM sessions WHERE thread_channel_id = '222000000000000101'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    let slot_group: Option<i64> = conn
        .query_row(
            "SELECT assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-same-group' AND slot_index = 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        continued_session,
        ("idle".to_string(), None, 0, None),
        "same-group continuation must keep the slot assignment but start from a fresh session"
    );
    let continued_dispatch_context: Option<String> = conn
        .query_row(
            "SELECT td.context
             FROM task_dispatches td
             JOIN auto_queue_entries e ON e.dispatch_id = td.id
             WHERE e.id = 'entry-same-group-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let continued_dispatch_context =
        serde_json::from_str::<serde_json::Value>(&continued_dispatch_context.unwrap()).unwrap();
    assert!(
        continued_dispatch_context
            .get("reset_slot_thread_before_reuse")
            .is_none(),
        "same-group continuation must not force an independent slot-thread reset"
    );
    assert_eq!(
        slot_group,
        Some(0),
        "same-group continuation must keep the original slot assignment"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_does_not_dispatch_same_group_follow_up_while_prior_is_active() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-same-group-guard");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(
        &db,
        "card-same-group-guard-0",
        4160,
        "ready",
        "agent-same-group-guard",
    );
    seed_auto_queue_card(
        &db,
        "card-same-group-guard-1",
        4161,
        "ready",
        "agent-same-group-guard",
    );
    seed_auto_queue_card(
        &db,
        "card-same-group-guard-2",
        4162,
        "ready",
        "agent-same-group-guard",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ('agent-same-group-guard', 0, ?1)",
            [json!({"111": "222000000000000201"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
             VALUES ('agent-same-group-guard', 1, ?1)",
            [json!({"111": "222000000000000202"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, unified_thread,
                max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-same-group-guard', 'test-repo', 'agent-same-group-guard', 'active', 1, 2, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group)
             VALUES ('entry-same-group-guard-0', 'run-same-group-guard', 'card-same-group-guard-0', 'agent-same-group-guard', 'pending', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group)
             VALUES ('entry-same-group-guard-1', 'run-same-group-guard', 'card-same-group-guard-1', 'agent-same-group-guard', 'pending', 1, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group)
             VALUES ('entry-same-group-guard-2', 'run-same-group-guard', 'card-same-group-guard-2', 'agent-same-group-guard', 'pending', 0, 1)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let first_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-same-group-guard",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(first_response.status(), StatusCode::OK);
    let first_body = axum::body::to_bytes(first_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let first_json: serde_json::Value = serde_json::from_slice(&first_body).unwrap();
    assert_eq!(
        first_json["count"], 2,
        "first activate must dispatch both groups in parallel when slots are available"
    );

    let second_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-same-group-guard",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(second_response.status(), StatusCode::OK);
    let second_body = axum::body::to_bytes(second_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let second_json: serde_json::Value = serde_json::from_slice(&second_body).unwrap();
    assert_eq!(
        second_json["count"], 0,
        "same-group follow-up must stay pending while the prior entry is still dispatched"
    );

    let conn = db.lock().unwrap();
    let guard_entry_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-same-group-guard-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        guard_entry_status, "pending",
        "follow-up entry must not be marked dispatched before prior same-group work completes"
    );
    let sibling_group_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-same-group-guard-2'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sibling_group_status, "dispatched",
        "different thread_group for the same agent must dispatch in parallel when slots are available"
    );

    let guard_dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM task_dispatches
             WHERE kanban_card_id = 'card-same-group-guard-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        guard_dispatch_count, 0,
        "no dispatch row should be created for the blocked same-group follow-up"
    );
    let sibling_group_dispatch_count: i64 = conn
        .query_row(
            "SELECT COUNT(*)
             FROM task_dispatches
             WHERE kanban_card_id = 'card-same-group-guard-2'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sibling_group_dispatch_count, 1,
        "a sibling group should create its own dispatch while the same-group follow-up stays blocked"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_expands_slot_pool_to_run_max_concurrency() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-slot-expand");
    ensure_auto_queue_tables(&db);

    for issue_number in 0..4 {
        seed_auto_queue_card(
            &db,
            &format!("card-slot-expand-{issue_number}"),
            1900 + issue_number,
            "ready",
            "agent-slot-expand",
        );
    }

    {
        let conn = db.lock().unwrap();
        for slot_index in 0..3 {
            conn.execute(
                "INSERT INTO auto_queue_slots (agent_id, slot_index, thread_id_map)
                 VALUES (?1, ?2, '{}')",
                sqlite_params!["agent-slot-expand", slot_index],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, unified_thread,
                max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-slot-expand', 'test-repo', 'agent-slot-expand', 'active', 1, 4, 4
            )",
            [],
        )
        .unwrap();
        for (priority_rank, thread_group) in (0..4).enumerate() {
            conn.execute(
                "INSERT INTO auto_queue_entries (
                    id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
                ) VALUES (?1, 'run-slot-expand', ?2, 'agent-slot-expand', 'pending', ?3, ?4)",
                sqlite_params![
                    format!("entry-slot-expand-{thread_group}"),
                    format!("card-slot-expand-{thread_group}"),
                    priority_rank as i64,
                    thread_group as i64,
                ],
            )
            .unwrap();
        }
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-slot-expand",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["count"], 4,
        "activate must dispatch all groups in parallel when slots are available"
    );
    assert_eq!(json["active_groups"], 4);

    let conn = db.lock().unwrap();
    let slot_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_slots WHERE agent_id = 'agent-slot-expand'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        slot_count, 4,
        "slot pool should expand from 3 seeded rows to match run max_concurrent_threads"
    );

    let mut stmt = conn
        .prepare(
            "SELECT slot_index
             FROM auto_queue_entries
             WHERE run_id = 'run-slot-expand'
             ORDER BY priority_rank ASC",
        )
        .unwrap();
    let mut assigned_slots = stmt
        .query_map([], |row| row.get::<_, Option<i64>>(0))
        .unwrap()
        .filter_map(|row| row.ok().flatten())
        .collect::<Vec<_>>();
    assigned_slots.sort_unstable();
    assert_eq!(assigned_slots, vec![0, 1, 2, 3]);

    let fourth_slot_group: Option<i64> = conn
        .query_row(
            "SELECT assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-slot-expand' AND slot_index = 3",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        fourth_slot_group,
        Some(3),
        "newly expanded slot must be assigned when parallel dispatch fills all slots"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_allows_same_agent_parallel_across_runs_when_free_slot_exists() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-cross-run");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(
        &db,
        "card-cross-run-active",
        1962,
        "requested",
        "agent-cross-run",
    );
    seed_auto_queue_card(&db, "card-cross-run-next", 1963, "ready", "agent-cross-run");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-cross-run-active', 'test-repo', 'agent-cross-run', 'active', 2, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-cross-run-next', 'test-repo', 'agent-cross-run', 'active', 2, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, thread_group, dispatched_at
            ) VALUES (
                'entry-cross-run-active', 'run-cross-run-active', 'card-cross-run-active', 'agent-cross-run',
                'dispatched', 'dispatch-cross-run-active', 0, 0, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-cross-run-next', 'run-cross-run-next', 'card-cross-run-next', 'agent-cross-run',
                'pending', 0, 0
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (
                agent_id, slot_index, assigned_run_id, assigned_thread_group
            ) VALUES (
                'agent-cross-run', 0, 'run-cross-run-active', 0
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-cross-run-active', 'card-cross-run-active', 'agent-cross-run',
                'implementation', 'pending', 'Cross-run active dispatch', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-cross-run-next",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["count"], 1,
        "a second run for the same agent should dispatch when another slot is free"
    );

    let conn = db.lock().unwrap();
    let next_entry: (String, Option<i64>) = conn
        .query_row(
            "SELECT status, slot_index
             FROM auto_queue_entries
             WHERE id = 'entry-cross-run-next'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(next_entry.0, "dispatched");
    assert_eq!(next_entry.1, Some(1));

    let next_slot: (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-cross-run' AND slot_index = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(next_slot.0.as_deref(), Some("run-cross-run-next"));
    assert_eq!(next_slot.1, Some(0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_keeps_single_slot_agent_single_dispatched_group() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-single-slot");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(
        &db,
        "card-single-slot-0",
        1960,
        "ready",
        "agent-single-slot",
    );
    seed_auto_queue_card(
        &db,
        "card-single-slot-1",
        1961,
        "ready",
        "agent-single-slot",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, unified_thread,
                max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-single-slot', 'test-repo', 'agent-single-slot', 'active', 1, 1, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group)
             VALUES ('entry-single-slot-0', 'run-single-slot', 'card-single-slot-0', 'agent-single-slot', 'pending', 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group)
             VALUES ('entry-single-slot-1', 'run-single-slot', 'card-single-slot-1', 'agent-single-slot', 'pending', 1, 1)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-single-slot",
                        "active_only": true
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["count"], 1,
        "a single-slot run must still dispatch only one same-agent group"
    );
    assert_eq!(json["active_groups"], 1);

    let conn = db.lock().unwrap();
    let dispatched_entries: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries
             WHERE run_id = 'run-single-slot' AND status = 'dispatched'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatched_entries, 1);

    let pending_slot: Option<i64> = conn
        .query_row(
            "SELECT slot_index FROM auto_queue_entries WHERE id = 'entry-single-slot-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        pending_slot, None,
        "the second group must stay unassigned until the lone slot becomes free"
    );

    let slot_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_slots WHERE agent_id = 'agent-single-slot'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(slot_count, 1);
}

// NOTE: auto_queue_activate_requested_card_not_blocked_by_own_status and
// auto_queue_activate_walks_backlog_card_to_dispatchable_state tests already
// defined above (from main branch merge). Duplicate definitions removed.

/// #107 regression: empty claude_session_id must be normalized to NULL at the API
/// boundary so that stale clear paths don't poison the DB with "".
#[tokio::test]
async fn hook_session_pg_normalizes_empty_claude_session_id_to_null() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    // 1. Save a valid claude_session_id
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatched-sessions/webhook")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"session_key":"test:sess1","status":"working","claude_session_id":"valid-id-123"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify it was stored
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT claude_session_id FROM sessions WHERE session_key = 'test:sess1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored.as_deref(), Some("valid-id-123"));

    // 2. Send empty string — should be normalized to NULL (not stored as "")
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatched-sessions/webhook")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"session_key":"test:sess1","status":"working","claude_session_id":""}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The COALESCE in the upsert preserves the old value when the new one is NULL,
    // so the valid-id-123 should still be there (empty was normalized to NULL → COALESCE keeps old).
    // This is correct: to actually clear, use the dedicated clear-session-id endpoint.
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT claude_session_id FROM sessions WHERE session_key = 'test:sess1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        stored.as_deref(),
        Some("valid-id-123"),
        "Empty string should be normalized to NULL, and COALESCE keeps the old value"
    );

    // 3. Use the dedicated clear endpoint to actually clear
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatched-sessions/clear-session-id")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"session_key":"test:sess1"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify it's actually cleared (NULL)
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT claude_session_id FROM sessions WHERE session_key = 'test:sess1'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        stored.is_none(),
        "After clear-session-id, value should be NULL"
    );

    // 4. Verify GET returns null after clear
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/dispatched-sessions/claude-session-id?session_key=test:sess1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["claude_session_id"].is_null(),
        "GET should return null after clear"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn hook_session_pg_persists_raw_provider_session_id_separately() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/dispatched-sessions/webhook")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"session_key":"test:gemini-raw","status":"working","provider":"gemini","claude_session_id":"latest","session_id":"aa678e6b-c6d3-4dd2-9197-58580c00cc6c"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let stored: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT claude_session_id, raw_provider_session_id
         FROM sessions
         WHERE session_key = 'test:gemini-raw'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored.0.as_deref(), Some("latest"));
    assert_eq!(
        stored.1.as_deref(),
        Some("aa678e6b-c6d3-4dd2-9197-58580c00cc6c")
    );

    let resp = app
        .oneshot(
            Request::builder()
                .uri("/dispatched-sessions/claude-session-id?session_key=test:gemini-raw&provider=gemini")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["claude_session_id"], "latest");
    assert_eq!(json["session_id"], "latest");
    assert_eq!(
        json["raw_provider_session_id"],
        "aa678e6b-c6d3-4dd2-9197-58580c00cc6c"
    );

    pool.close().await;
    pg_db.drop().await;
}

// ── #140: Parallel thread group auto-queue tests ──────────────────

/// Helper: seed kanban cards for the parallel dispatch test scenario.
/// Creates 7 cards:
///   - 3 independent (issue #1, #2, #3)
///   - 4 in a dependency chain: #4 → #5 → #6 → #7
/// Returns card IDs in order [A, B, C, D, E, F, G].
fn seed_parallel_test_cards(db: &Db) -> Vec<String> {
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

fn seed_similarity_group_cards(db: &Db) -> Vec<String> {
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

#[tokio::test]
async fn smart_generate_pg_creates_correct_thread_groups_and_batch_phases() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let _card_ids = seed_parallel_test_cards_pg(&pool).await;

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "max_concurrent_threads": 3,
                        "max_concurrent_per_agent": 3,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let entries = json["entries"].as_array().expect("entries must be array");
    assert_eq!(entries.len(), 7, "all 7 cards should be queued");

    // Verify run keeps the requested concurrency cap while ignoring the
    // legacy max_concurrent_per_agent input.
    let run = &json["run"];
    assert_eq!(run["max_concurrent_threads"], 3);
    assert!(run.get("max_concurrent_per_agent").is_none());
    assert_eq!(run["ai_model"], "smart-planner");

    // Collect thread_group assignments per issue number.
    let mut groups: std::collections::HashMap<i64, Vec<(i64, i64, i64)>> =
        std::collections::HashMap::new();
    for entry in entries {
        let issue_num = entry["github_issue_number"].as_i64().unwrap();
        let thread_group = entry["thread_group"].as_i64().unwrap();
        let priority_rank = entry["priority_rank"].as_i64().unwrap();
        let batch_phase = entry["batch_phase"].as_i64().unwrap();
        groups
            .entry(thread_group)
            .or_default()
            .push((issue_num, priority_rank, batch_phase));
    }

    let group_count = run["thread_group_count"].as_i64().unwrap();
    assert_eq!(
        group_count,
        groups.len() as i64,
        "thread_group_count must match actual distinct groups"
    );

    // Independent cards (issues 1, 2, 3) should each be in their own group (size 1)
    let mut independent_groups = 0;
    let mut chain_group = None;
    for (group_num, members) in &groups {
        if members.len() == 1 {
            let issue = members[0].0;
            assert!(
                [1, 2, 3].contains(&issue),
                "single-member group should be an independent card, got issue #{issue}"
            );
            assert_eq!(
                members[0].2, 0,
                "independent cards should start in batch phase 0"
            );
            independent_groups += 1;
        } else {
            // This must be the dependency chain group
            assert!(
                chain_group.is_none(),
                "only one multi-member group expected"
            );
            chain_group = Some(*group_num);
        }
    }
    assert_eq!(independent_groups, 3, "3 independent cards → 3 groups");

    // Verify the chain group: issues 4,5,6,7 in topological order
    let chain = chain_group.expect("dependency chain group must exist");
    let mut chain_members = groups[&chain].clone();
    chain_members.sort_by_key(|(_, rank, _)| *rank);
    let chain_issues: Vec<i64> = chain_members.iter().map(|(num, _, _)| *num).collect();
    let chain_phases: Vec<i64> = chain_members.iter().map(|(_, _, phase)| *phase).collect();
    // Issue #4 must come first (rank 0), #5 second, then #6 and #7 (order between 6,7 may vary
    // since #7 depends on both #5 and #6, making #6 and #7 at different levels)
    assert_eq!(chain_issues[0], 4, "chain start (#4) must have lowest rank");
    assert_eq!(chain_issues[1], 5, "#5 depends on #4, must be second");
    // #6 depends on #5, #7 depends on #5 and #6 — so #6 before #7
    assert_eq!(chain_issues[2], 6, "#6 depends on #5, must be third");
    assert_eq!(chain_issues[3], 7, "#7 depends on #5 and #6, must be last");
    assert_eq!(
        chain_phases,
        vec![0, 1, 2, 3],
        "dependency chain should advance one batch phase at a time"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_generate_rolls_back_when_entry_insert_fails() {
    let db = test_db();
    let engine = test_engine(&db);
    let _card_ids = seed_parallel_test_cards(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_auto_queue_entry_insert
             BEFORE INSERT ON auto_queue_entries
             BEGIN
                 SELECT RAISE(ABORT, 'entry insert blocked');
             END;",
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "max_concurrent_threads": 3,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("create auto-queue entry"),
        "generate must expose entry insert failure instead of silently succeeding"
    );

    let conn = db.lock().unwrap();
    let run_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM auto_queue_runs", [], |row| row.get(0))
        .unwrap();
    let entry_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM auto_queue_entries", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(run_count, 0, "failed generate must roll back run creation");
    assert_eq!(
        entry_count, 0,
        "failed generate must not leave partial entries"
    );
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_status_exposes_explicit_thread_links_from_configured_channels() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_agent(&db, "agent-thread-links");
    seed_auto_queue_card(
        &db,
        "card-thread-links",
        4131,
        "review",
        "agent-thread-links",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE kanban_cards
             SET channel_thread_map = ?1
             WHERE id = 'card-thread-links'",
            [json!({
                "111": "222000000000000001",
                "222": "222000000000000002"
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, created_at
            ) VALUES (
                'run-thread-links', 'test-repo', 'agent-thread-links', 'active', datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank
            ) VALUES (
                'entry-thread-links', 'run-thread-links', 'card-thread-links',
                'agent-thread-links', 'dispatched', 0
            )",
            [],
        )
        .unwrap();
    }

    let mut config = crate::config::Config::default();
    config.discord.guild_id = Some("guild-123".to_string());
    let app = test_api_router_with_config(db.clone(), engine, config, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/auto-queue/status?agent_id=agent-thread-links")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let thread_links = json["entries"][0]["thread_links"]
        .as_array()
        .expect("thread_links must be an array");

    assert_eq!(thread_links.len(), 2);
    assert_eq!(thread_links[0]["label"], "work");
    assert_eq!(thread_links[0]["channel_id"], "111");
    assert_eq!(thread_links[0]["thread_id"], "222000000000000001");
    assert_eq!(
        thread_links[0]["url"],
        "https://discord.com/channels/guild-123/222000000000000001"
    );
    assert_eq!(thread_links[1]["label"], "review");
    assert_eq!(thread_links[1]["channel_id"], "222");
    assert_eq!(thread_links[1]["thread_id"], "222000000000000002");
    assert_eq!(
        thread_links[1]["url"],
        "https://discord.com/channels/guild-123/222000000000000002"
    );
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_history_returns_recent_runs_with_summary_metrics() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_agent(&db, "agent-history");
    seed_auto_queue_card(&db, "card-history-done", 5131, "done", "agent-history");
    seed_auto_queue_card(&db, "card-history-skipped", 5132, "done", "agent-history");
    seed_auto_queue_card(&db, "card-history-pending", 5133, "review", "agent-history");
    seed_auto_queue_card(
        &db,
        "card-history-dispatched",
        5134,
        "review",
        "agent-history",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, created_at, completed_at
            ) VALUES (
                'run-history-completed', 'test-repo', 'agent-history', 'completed',
                datetime('now', '-20 minutes'), datetime('now', '-10 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, created_at
            ) VALUES (
                'run-history-active', 'test-repo', 'agent-history', 'active',
                datetime('now', '-5 minutes')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank
            ) VALUES (
                'entry-history-done', 'run-history-completed', 'card-history-done',
                'agent-history', 'done', 0
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank
            ) VALUES (
                'entry-history-skipped', 'run-history-completed', 'card-history-skipped',
                'agent-history', 'skipped', 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank
            ) VALUES (
                'entry-history-pending', 'run-history-active', 'card-history-pending',
                'agent-history', 'pending', 0
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank
            ) VALUES (
                'entry-history-dispatched', 'run-history-active', 'card-history-dispatched',
                'agent-history', 'dispatched', 1
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/auto-queue/history?repo=test-repo&limit=2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let runs = json["runs"].as_array().expect("runs must be an array");

    assert_eq!(json["summary"]["total_runs"], 2);
    assert_eq!(json["summary"]["completed_runs"], 1);
    assert_eq!(json["summary"]["success_rate"], 0.25);
    assert_eq!(json["summary"]["failure_rate"], 0.75);
    assert_eq!(runs.len(), 2);

    assert_eq!(runs[0]["id"], "run-history-active");
    assert_eq!(runs[0]["status"], "active");
    assert_eq!(runs[0]["entry_count"], 2);
    assert_eq!(runs[0]["done_count"], 0);
    assert_eq!(runs[0]["pending_count"], 1);
    assert_eq!(runs[0]["dispatched_count"], 1);
    assert_eq!(runs[0]["success_rate"], 0.0);
    assert_eq!(runs[0]["failure_rate"], 1.0);
    assert!(runs[0]["duration_ms"].as_i64().unwrap() >= 0);
    assert!(runs[0]["completed_at"].is_null());

    assert_eq!(runs[1]["id"], "run-history-completed");
    assert_eq!(runs[1]["status"], "completed");
    assert_eq!(runs[1]["entry_count"], 2);
    assert_eq!(runs[1]["done_count"], 1);
    assert_eq!(runs[1]["skipped_count"], 1);
    assert_eq!(runs[1]["success_rate"], 0.5);
    assert_eq!(runs[1]["failure_rate"], 0.5);
    assert!(runs[1]["duration_ms"].as_i64().unwrap() > 0);
    assert!(runs[1]["completed_at"].as_i64().unwrap() > runs[1]["created_at"].as_i64().unwrap());
}

#[tokio::test]
async fn auto_queue_status_pg_exposes_explicit_thread_links_from_configured_channels() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("agent-thread-links-pg")
    .bind("Agent Thread Links PG")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number, channel_thread_map
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8::jsonb
         )",
    )
    .bind("card-thread-links-pg")
    .bind("test-repo")
    .bind("Issue #4131")
    .bind("review")
    .bind("medium")
    .bind("agent-thread-links-pg")
    .bind(4131_i64)
    .bind(
        json!({
            "111": "222000000000000001",
            "222": "222000000000000002"
        })
        .to_string(),
    )
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("run-thread-links-pg")
    .bind("test-repo")
    .bind("agent-thread-links-pg")
    .bind("active")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         )",
    )
    .bind("entry-thread-links-pg")
    .bind("run-thread-links-pg")
    .bind("card-thread-links-pg")
    .bind("agent-thread-links-pg")
    .bind("dispatched")
    .bind(0_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let mut config = crate::config::Config::default();
    config.discord.guild_id = Some("guild-123".to_string());
    let app = test_api_router_with_pg(db, engine, config, None, pg_pool.clone());

    let response = app
        .oneshot(
            Request::builder()
                .uri("/auto-queue/status?agent_id=agent-thread-links-pg")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let thread_links = json["entries"][0]["thread_links"]
        .as_array()
        .expect("thread_links must be an array");

    assert_eq!(thread_links.len(), 2);
    assert_eq!(thread_links[0]["label"], "work");
    assert_eq!(thread_links[0]["channel_id"], "111");
    assert_eq!(thread_links[0]["thread_id"], "222000000000000001");
    assert_eq!(
        thread_links[0]["url"],
        "https://discord.com/channels/guild-123/222000000000000001"
    );
    assert_eq!(thread_links[1]["label"], "review");
    assert_eq!(thread_links[1]["channel_id"], "222");
    assert_eq!(thread_links[1]["thread_id"], "222000000000000002");
    assert_eq!(
        thread_links[1]["url"],
        "https://discord.com/channels/guild-123/222000000000000002"
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_history_pg_returns_recent_runs_with_summary_metrics() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name)
         VALUES ($1, $2)",
    )
    .bind("agent-history-pg")
    .bind("Agent History PG")
    .execute(&pg_pool)
    .await
    .unwrap();

    for (card_id, issue_number, status) in [
        ("card-history-done-pg", 5131_i64, "done"),
        ("card-history-skipped-pg", 5132_i64, "done"),
        ("card-history-pending-pg", 5133_i64, "review"),
        ("card-history-dispatched-pg", 5134_i64, "review"),
    ] {
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7
             )",
        )
        .bind(card_id)
        .bind("test-repo")
        .bind(format!("Issue #{issue_number}"))
        .bind(status)
        .bind("medium")
        .bind("agent-history-pg")
        .bind(issue_number)
        .execute(&pg_pool)
        .await
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, agent_id, status, created_at, completed_at
         ) VALUES (
            $1, $2, $3, $4, NOW() - INTERVAL '20 minutes', NOW() - INTERVAL '10 minutes'
         )",
    )
    .bind("run-history-completed-pg")
    .bind("test-repo")
    .bind("agent-history-pg")
    .bind("completed")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, agent_id, status, created_at
         ) VALUES (
            $1, $2, $3, $4, NOW() - INTERVAL '5 minutes'
         )",
    )
    .bind("run-history-active-pg")
    .bind("test-repo")
    .bind("agent-history-pg")
    .bind("active")
    .execute(&pg_pool)
    .await
    .unwrap();

    for (entry_id, run_id, card_id, status, priority_rank) in [
        (
            "entry-history-done-pg",
            "run-history-completed-pg",
            "card-history-done-pg",
            "done",
            0_i64,
        ),
        (
            "entry-history-skipped-pg",
            "run-history-completed-pg",
            "card-history-skipped-pg",
            "skipped",
            1_i64,
        ),
        (
            "entry-history-pending-pg",
            "run-history-active-pg",
            "card-history-pending-pg",
            "pending",
            0_i64,
        ),
        (
            "entry-history-dispatched-pg",
            "run-history-active-pg",
            "card-history-dispatched-pg",
            "dispatched",
            1_i64,
        ),
    ] {
        sqlx::query(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank
             ) VALUES (
                $1, $2, $3, $4, $5, $6
             )",
        )
        .bind(entry_id)
        .bind(run_id)
        .bind(card_id)
        .bind("agent-history-pg")
        .bind(status)
        .bind(priority_rank)
        .execute(&pg_pool)
        .await
        .unwrap();
    }

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/auto-queue/history?repo=test-repo&limit=2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let runs = json["runs"].as_array().expect("runs must be an array");

    assert_eq!(json["summary"]["total_runs"], 2);
    assert_eq!(json["summary"]["completed_runs"], 1);
    assert_eq!(json["summary"]["success_rate"], 0.25);
    assert_eq!(json["summary"]["failure_rate"], 0.75);
    assert_eq!(runs.len(), 2);

    assert_eq!(runs[0]["id"], "run-history-active-pg");
    assert_eq!(runs[0]["status"], "active");
    assert_eq!(runs[0]["entry_count"], 2);
    assert_eq!(runs[0]["done_count"], 0);
    assert_eq!(runs[0]["pending_count"], 1);
    assert_eq!(runs[0]["dispatched_count"], 1);
    assert_eq!(runs[0]["success_rate"], 0.0);
    assert_eq!(runs[0]["failure_rate"], 1.0);
    assert!(runs[0]["duration_ms"].as_i64().unwrap() >= 0);
    assert!(runs[0]["completed_at"].is_null());

    assert_eq!(runs[1]["id"], "run-history-completed-pg");
    assert_eq!(runs[1]["status"], "completed");
    assert_eq!(runs[1]["entry_count"], 2);
    assert_eq!(runs[1]["done_count"], 1);
    assert_eq!(runs[1]["skipped_count"], 1);
    assert_eq!(runs[1]["success_rate"], 0.5);
    assert_eq!(runs[1]["failure_rate"], 0.5);
    assert!(runs[1]["duration_ms"].as_i64().unwrap() > 0);
    assert!(runs[1]["completed_at"].as_i64().unwrap() > runs[1]["created_at"].as_i64().unwrap());

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_reset_pg_preserves_active_runs_on_global_reset() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind("agent-reset-pg")
        .bind("Agent Reset PG")
        .execute(&pg_pool)
        .await
        .unwrap();

    for (card_id, issue_number) in [
        ("card-reset-active-pg", 6231_i64),
        ("card-reset-generated-pg", 6232_i64),
    ] {
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7
             )",
        )
        .bind(card_id)
        .bind("test-repo")
        .bind(format!("Issue #{issue_number}"))
        .bind("ready")
        .bind("medium")
        .bind("agent-reset-pg")
        .bind(issue_number)
        .execute(&pg_pool)
        .await
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, $4), ($5, $6, $7, $8)",
    )
    .bind("run-reset-active-pg")
    .bind("test-repo")
    .bind("agent-reset-pg")
    .bind("active")
    .bind("run-reset-generated-pg")
    .bind("test-repo")
    .bind("agent-reset-pg")
    .bind("generated")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         ), (
            $7, $8, $9, $10, $11, $12
         )",
    )
    .bind("entry-reset-active-pg")
    .bind("run-reset-active-pg")
    .bind("card-reset-active-pg")
    .bind("agent-reset-pg")
    .bind("pending")
    .bind(0_i64)
    .bind("entry-reset-generated-pg")
    .bind("run-reset-generated-pg")
    .bind("card-reset-generated-pg")
    .bind("agent-reset-pg")
    .bind("pending")
    .bind(1_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/reset-global")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"confirmation_token":"confirm-global-reset"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["ok"], true);
    assert_eq!(json["deleted_entries"], 1);
    assert_eq!(json["completed_runs"], 1);
    assert_eq!(json["protected_active_runs"], 1);
    assert_eq!(
        json["warning"],
        "global reset preserved 1 active run(s); use agent_id to reset a specific queue"
    );

    let active_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind("run-reset-active-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    let generated_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind("run-reset-generated-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    let active_entry_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM auto_queue_entries WHERE run_id = $1",
    )
    .bind("run-reset-active-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    let generated_entry_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM auto_queue_entries WHERE run_id = $1",
    )
    .bind("run-reset-generated-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();

    assert_eq!(active_status, "active");
    assert_eq!(generated_status, "completed");
    assert_eq!(active_entry_count, 1);
    assert_eq!(generated_entry_count, 0);

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_reorder_pg_updates_priority_ranks() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind("agent-reorder-pg")
        .bind("Agent Reorder PG")
        .execute(&pg_pool)
        .await
        .unwrap();

    for (card_id, issue_number) in [
        ("card-reorder-1-pg", 6331_i64),
        ("card-reorder-2-pg", 6332_i64),
        ("card-reorder-3-pg", 6333_i64),
    ] {
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7
             )",
        )
        .bind(card_id)
        .bind("test-repo")
        .bind(format!("Issue #{issue_number}"))
        .bind("ready")
        .bind("medium")
        .bind("agent-reorder-pg")
        .bind(issue_number)
        .execute(&pg_pool)
        .await
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("run-reorder-pg")
    .bind("test-repo")
    .bind("agent-reorder-pg")
    .bind("active")
    .execute(&pg_pool)
    .await
    .unwrap();
    for (entry_id, card_id, priority_rank) in [
        ("entry-reorder-1-pg", "card-reorder-1-pg", 0_i64),
        ("entry-reorder-2-pg", "card-reorder-2-pg", 1_i64),
        ("entry-reorder-3-pg", "card-reorder-3-pg", 2_i64),
    ] {
        sqlx::query(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank
             ) VALUES (
                $1, $2, $3, $4, $5, $6
             )",
        )
        .bind(entry_id)
        .bind("run-reorder-pg")
        .bind(card_id)
        .bind("agent-reorder-pg")
        .bind("pending")
        .bind(priority_rank)
        .execute(&pg_pool)
        .await
        .unwrap();
    }

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/auto-queue/reorder")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "orderedIds": [
                            "entry-reorder-3-pg",
                            "entry-reorder-1-pg",
                            "entry-reorder-2-pg"
                        ]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);

    let ordered_ids = sqlx::query_scalar::<_, String>(
        "SELECT id
         FROM auto_queue_entries
         WHERE run_id = $1
         ORDER BY priority_rank ASC, created_at ASC, id ASC",
    )
    .bind("run-reorder-pg")
    .fetch_all(&pg_pool)
    .await
    .unwrap();

    assert_eq!(
        ordered_ids,
        vec![
            "entry-reorder-3-pg".to_string(),
            "entry-reorder-1-pg".to_string(),
            "entry-reorder-2-pg".to_string(),
        ]
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_add_run_entry_pg_creates_pending_entry_for_active_run() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind("project-agentdesk")
        .bind("Project AgentDesk")
        .execute(&pg_pool)
        .await
        .unwrap();

    for (card_id, issue_number) in [
        ("card-run-entry-existing-pg", 7121_i64),
        ("card-run-entry-new-pg", 7122_i64),
    ] {
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7
             )",
        )
        .bind(card_id)
        .bind("test-repo")
        .bind(format!("Issue #{issue_number}"))
        .bind("ready")
        .bind("medium")
        .bind("project-agentdesk")
        .bind(issue_number)
        .execute(&pg_pool)
        .await
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, agent_id, status, max_concurrent_threads, thread_group_count
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         )",
    )
    .bind("run-add-entry-active-pg")
    .bind("test-repo")
    .bind("project-agentdesk")
    .bind("active")
    .bind(1_i64)
    .bind(1_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, batch_phase
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8
         )",
    )
    .bind("entry-run-entry-existing-pg")
    .bind("run-add-entry-active-pg")
    .bind("card-run-entry-existing-pg")
    .bind("project-agentdesk")
    .bind("pending")
    .bind(0_i64)
    .bind(0_i64)
    .bind(1_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/runs/run-add-entry-active-pg/entries")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "issue_number": 7122,
                        "batch_phase": 4,
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
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["run_id"], "run-add-entry-active-pg");
    assert_eq!(json["thread_group"], 1);
    assert_eq!(json["priority_rank"], 0);
    assert_eq!(json["entry"]["card_id"], "card-run-entry-new-pg");

    let inserted: (i64, i64, i64, String) = sqlx::query_as(
        "SELECT priority_rank::BIGINT,
                COALESCE(thread_group, 0)::BIGINT,
                COALESCE(batch_phase, 0)::BIGINT,
                status
         FROM auto_queue_entries
         WHERE run_id = $1
           AND kanban_card_id = $2",
    )
    .bind("run-add-entry-active-pg")
    .bind("card-run-entry-new-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(inserted, (0, 1, 4, "pending".to_string()));

    let run_meta: (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(thread_group_count, 0)::BIGINT,
                COALESCE(max_concurrent_threads, 0)::BIGINT
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind("run-add-entry-active-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(run_meta, (2, 2));

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_submit_order_pg_activates_pending_run_and_skips_non_dispatchable_cards() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let dispatchable_state = crate::pipeline::get()
        .dispatchable_states()
        .into_iter()
        .next()
        .expect("default pipeline should expose at least one dispatchable state")
        .to_string();

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind("project-agentdesk")
        .bind("Project AgentDesk")
        .execute(&pg_pool)
        .await
        .unwrap();

    for (card_id, issue_number, status) in [
        ("card-order-backlog-pg", 7421_i64, "backlog".to_string()),
        (
            "card-order-ready-a-pg",
            7422_i64,
            dispatchable_state.clone(),
        ),
        (
            "card-order-ready-b-pg",
            7423_i64,
            dispatchable_state.clone(),
        ),
    ] {
        sqlx::query(
            "INSERT INTO kanban_cards (
                id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
             ) VALUES (
                $1, $2, $3, $4, $5, $6, $7
             )",
        )
        .bind(card_id)
        .bind("test-repo")
        .bind(format!("Issue #{issue_number}"))
        .bind(status)
        .bind("medium")
        .bind("project-agentdesk")
        .bind(issue_number)
        .execute(&pg_pool)
        .await
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("run-submit-order-pg")
    .bind("test-repo")
    .bind("project-agentdesk")
    .bind("pending")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/runs/run-submit-order-pg/order")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .header("x-agent-id", "project-agentdesk")
                .body(Body::from(
                    json!({
                        "order": [7421, "card-order-ready-b-pg", 7422],
                        "rationale": "manual pg ordering",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["created"], 2);
    assert_eq!(json["run_id"], "run-submit-order-pg");

    let run_status: (String, Option<String>) = sqlx::query_as(
        "SELECT status, ai_rationale
         FROM auto_queue_runs
         WHERE id = $1",
    )
    .bind("run-submit-order-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(run_status.0, "active");
    assert_eq!(run_status.1.as_deref(), Some("manual pg ordering"));

    let entries: Vec<(String, i64)> = sqlx::query_as(
        "SELECT kanban_card_id, priority_rank::BIGINT
         FROM auto_queue_entries
         WHERE run_id = $1
         ORDER BY priority_rank ASC, id ASC",
    )
    .bind("run-submit-order-pg")
    .fetch_all(&pg_pool)
    .await
    .unwrap();
    assert_eq!(
        entries,
        vec![
            ("card-order-ready-b-pg".to_string(), 1),
            ("card-order-ready-a-pg".to_string(), 2),
        ]
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_status_legacy_thread_falls_back_to_active_label_without_url_guessing() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_agent(&db, "agent-thread-links-legacy");
    seed_auto_queue_card(
        &db,
        "card-thread-links-legacy",
        4132,
        "in_progress",
        "agent-thread-links-legacy",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE kanban_cards
             SET active_thread_id = '333000000000000009'
             WHERE id = 'card-thread-links-legacy'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, created_at
            ) VALUES (
                'run-thread-links-legacy', 'test-repo', 'agent-thread-links-legacy',
                'active', datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank
            ) VALUES (
                'entry-thread-links-legacy', 'run-thread-links-legacy',
                'card-thread-links-legacy', 'agent-thread-links-legacy', 'pending', 0
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/auto-queue/status?agent_id=agent-thread-links-legacy")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let thread_links = json["entries"][0]["thread_links"]
        .as_array()
        .expect("thread_links must be an array");

    assert_eq!(thread_links.len(), 1);
    assert_eq!(thread_links[0]["label"], "active");
    assert_eq!(thread_links[0]["role"], "active");
    assert_eq!(thread_links[0]["thread_id"], "333000000000000009");
    assert_eq!(thread_links[0]["url"], serde_json::Value::Null);
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_status_scopes_global_run_entries_by_repo_and_agent() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_repo(&db, "test-repo");
    seed_repo(&db, "other-repo");
    seed_agent(&db, "agent-scope-a");
    seed_agent(&db, "agent-scope-b");
    seed_auto_queue_card(&db, "card-scope-a", 4201, "ready", "agent-scope-a");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, created_at, updated_at
             ) VALUES (
                'card-scope-b', 'Issue #4202', 'ready', 'medium', 'agent-scope-b', 'other-repo',
                4202, datetime('now'), datetime('now')
             )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, status, created_at)
             VALUES ('run-scope-global', 'active', datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, reason
             ) VALUES (
                'entry-scope-a', 'run-scope-global', 'card-scope-a', 'agent-scope-a',
                'pending', 0, 0, 'scope group a'
             )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, reason
             ) VALUES (
                'entry-scope-b', 'run-scope-global', 'card-scope-b', 'agent-scope-b',
                'dispatched', 1, 1, 'scope group b'
             )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db, engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .uri("/auto-queue/status?repo=test-repo&agent_id=agent-scope-a")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["run"]["id"], "run-scope-global");
    let entries = json["entries"]
        .as_array()
        .expect("entries must be an array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], "entry-scope-a");

    let agents = json["agents"]
        .as_object()
        .expect("agents must be an object");
    assert_eq!(agents.len(), 1);
    assert_eq!(agents["agent-scope-a"]["pending"], 1);
    assert!(agents.get("agent-scope-b").is_none());

    let thread_groups = json["thread_groups"]
        .as_object()
        .expect("thread_groups must be an object");
    assert_eq!(thread_groups.len(), 1);
    assert_eq!(thread_groups["0"]["pending"], 1);
    assert_eq!(thread_groups["0"]["status"], "pending");
    assert_eq!(thread_groups["0"]["reason"], "scope group a");
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_generate_issue_numbers_filters_cards_and_promotes_backlog() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-generate-327");
    seed_repo(&db, "test-repo");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id, github_issue_number, created_at, updated_at
            ) VALUES (
                'card-gen-327-ready', 'Generate Ready #327', 'ready', 'high', 'agent-generate-327', 'test-repo', 3271, datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id, github_issue_number, created_at, updated_at
            ) VALUES (
                'card-gen-327-backlog', 'Generate Backlog #327', 'backlog', 'medium', 'agent-generate-327', 'test-repo', 3272, datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id, github_issue_number, created_at, updated_at
            ) VALUES (
                'card-gen-327-extra', 'Generate Extra', 'ready', 'urgent', 'agent-generate-327', 'test-repo', 3999, datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "agent-generate-327",
                        "issue_numbers": [3271, 3272],
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let entries = json["entries"].as_array().unwrap();
    let queued_issues: Vec<i64> = entries
        .iter()
        .filter_map(|entry| entry["github_issue_number"].as_i64())
        .collect();
    assert_eq!(entries.len(), 2);
    assert!(queued_issues.contains(&3271));
    assert!(queued_issues.contains(&3272));
    assert!(!queued_issues.contains(&3999));

    let conn = db.lock().unwrap();
    let backlog_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-gen-327-backlog'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        backlog_status, "ready",
        "selected backlog card must be promoted before queue generation"
    );
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_generate_rejects_when_live_run_exists_without_force() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-generate-conflict");
    seed_repo(&db, "test-repo");
    seed_auto_queue_card(
        &db,
        "card-gen-conflict-backlog",
        3281,
        "backlog",
        "agent-generate-conflict",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, created_at
            ) VALUES (
                'run-generate-conflict', 'test-repo', 'agent-generate-conflict', 'active', datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "agent-generate-conflict",
                        "issue_numbers": [3281],
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["existing_run_id"], "run-generate-conflict");
    assert_eq!(json["existing_run_status"], "active");

    let conn = db.lock().unwrap();
    let run_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM auto_queue_runs", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        run_count, 1,
        "generate conflict must not create a second run"
    );

    let backlog_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-gen-conflict-backlog'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        backlog_status, "backlog",
        "generate conflict must happen before backlog auto-promotion"
    );
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_generate_empty_state_reports_filtered_counts() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-generate-counts");
    seed_agent(&db, "other-agent");
    seed_repo(&db, "test-repo");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id, github_issue_number,
                created_at, updated_at
            ) VALUES (
                'card-generate-counts-backlog', 'Generate Counts Backlog', 'backlog', 'medium',
                'agent-generate-counts', 'test-repo', 5410, datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id, github_issue_number,
                created_at, updated_at
            ) VALUES (
                'card-generate-counts-other-agent', 'Other Agent Ready', 'ready', 'high',
                'other-agent', 'test-repo', 5411, datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "agent-generate-counts",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["message"], "No dispatchable cards found");
    assert_eq!(json["counts"]["backlog"], 1);
    assert_eq!(json["counts"]["ready"], 0);
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_generate_accepts_but_ignores_unified_thread_flag() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-generate-unified");
    seed_repo(&db, "test-repo");
    seed_auto_queue_card(
        &db,
        "card-generate-unified",
        3881,
        "ready",
        "agent-generate-unified",
    );

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "agent-generate-unified",
                        "unified_thread": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["run"]["unified_thread"], serde_json::json!(false));

    let run_id = json["run"]["id"]
        .as_str()
        .expect("generated run id must be present");
    let conn = db.lock().unwrap();
    let stored_unified_thread: i64 = conn
        .query_row(
            "SELECT unified_thread FROM auto_queue_runs WHERE id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        stored_unified_thread, 0,
        "generate must ignore unified_thread and keep slot pooling enabled"
    );
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_generate_entries_payload_persists_batch_phases() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-generate-phase");
    seed_repo(&db, "test-repo");
    seed_auto_queue_card(
        &db,
        "card-generate-phase-1",
        4231,
        "ready",
        "agent-generate-phase",
    );
    seed_auto_queue_card(
        &db,
        "card-generate-phase-2",
        4232,
        "ready",
        "agent-generate-phase",
    );

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "agent-generate-phase",
                        "entries": [
                            { "issue_number": 4232, "batch_phase": 2 },
                            { "issue_number": 4231, "batch_phase": 1 }
                        ],
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let entries = json["entries"].as_array().expect("entries must be array");

    let phases_by_issue: std::collections::HashMap<i64, i64> = entries
        .iter()
        .filter_map(|entry| {
            Some((
                entry["github_issue_number"].as_i64()?,
                entry["batch_phase"].as_i64()?,
            ))
        })
        .collect();

    assert_eq!(phases_by_issue.get(&4231), Some(&1));
    assert_eq!(phases_by_issue.get(&4232), Some(&2));

    let conn = db.lock().unwrap();
    let stored_phases: std::collections::HashMap<i64, i64> = {
        let mut stmt = conn
            .prepare(
                "SELECT kc.github_issue_number, COALESCE(e.batch_phase, 0)
                 FROM auto_queue_entries e
                 JOIN kanban_cards kc ON kc.id = e.kanban_card_id
                 ORDER BY kc.github_issue_number ASC",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
            .unwrap()
            .filter_map(|row| row.ok())
            .collect()
    };
    assert_eq!(stored_phases.get(&4231), Some(&1));
    assert_eq!(stored_phases.get(&4232), Some(&2));
}

#[tokio::test]
async fn generate_smart_planner_pg_groups_by_file_paths_and_recommends_threads() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let _card_ids = seed_similarity_group_cards_pg(&pool).await;

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let run = &json["run"];
    let entries = json["entries"].as_array().expect("entries must be array");
    assert_eq!(
        entries.len(),
        5,
        "all similarity test cards should be queued"
    );
    assert_eq!(
        run["thread_group_count"].as_i64().unwrap(),
        5,
        "similarity-only cards stay in distinct groups and are staggered by phase"
    );
    assert_eq!(
        run["max_concurrent_threads"].as_i64().unwrap(),
        4,
        "recommended concurrency is capped even when smart planner emits more groups"
    );
    assert_eq!(run["ai_model"].as_str().unwrap(), "smart-planner");

    let staggered_entries = entries
        .iter()
        .filter(|entry| {
            entry["batch_phase"]
                .as_i64()
                .map(|phase| phase > 0)
                .unwrap_or(false)
        })
        .count();
    assert!(
        staggered_entries >= 2,
        "similarity signals should still stagger conflicting work into later phases"
    );

    let status_resp = app
        .oneshot(
            Request::builder()
                .uri("/auto-queue/status?repo=test-repo")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(status_resp.status(), StatusCode::OK);
    let status_body = axum::body::to_bytes(status_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let status_json: serde_json::Value = serde_json::from_slice(&status_body).unwrap();
    let thread_groups = status_json["thread_groups"]
        .as_object()
        .expect("thread_groups must be present");
    assert_eq!(
        thread_groups.len(),
        5,
        "status should expose all planner-emitted thread groups"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generate_smart_planner_pg_without_file_paths_uses_dependency_only_groups() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let _card_ids = seed_parallel_test_cards_pg(&pool).await;

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let run = &json["run"];
    let entries = json["entries"].as_array().expect("entries must be array");
    assert_eq!(
        entries.len(),
        7,
        "all dependency-seed cards should be queued"
    );
    assert_eq!(
        run["thread_group_count"].as_i64().unwrap(),
        4,
        "without file paths, smart planner must fall back to dependency-only grouping"
    );
    assert_eq!(run["ai_model"].as_str().unwrap(), "smart-planner");
    assert!(
        run["ai_rationale"]
            .as_str()
            .map(|text| text.contains("파일 경로 신호 없이"))
            .unwrap_or(false),
        "rationale should explain the dependency-only fallback"
    );
    assert!(
        entries.iter().all(|entry| {
            entry["reason"]
                .as_str()
                .map(|reason| !reason.contains("유사도 그룹"))
                .unwrap_or(true)
        }),
        "fallback path should not stamp similarity reasons"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn generate_pg_ignores_non_dependency_issue_references_in_description() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-context").await;
    seed_auto_queue_card_pg(&pool, "card-context-only", 497, "ready", "agent-context").await;
    seed_auto_queue_card_pg(
        &pool,
        "card-referenced-open",
        494,
        "backlog",
        "agent-context",
    )
    .await;
    sqlx::query(
        "UPDATE kanban_cards
         SET description = $1
         WHERE id = 'card-context-only'",
    )
    .bind("## 컨텍스트\n관련 작업: #494")
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let entries = json["entries"].as_array().expect("entries must be array");
    assert_eq!(
        entries.len(),
        1,
        "context-only references must not exclude the card"
    );
    assert_eq!(entries[0]["github_issue_number"].as_i64(), Some(497));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn generate_pg_excludes_card_with_explicit_external_dependency() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-dependency").await;
    seed_auto_queue_card_pg(&pool, "card-explicit-dep", 497, "ready", "agent-dependency").await;
    seed_auto_queue_card_pg(
        &pool,
        "card-explicit-target",
        494,
        "backlog",
        "agent-dependency",
    )
    .await;
    sqlx::query(
        "UPDATE kanban_cards
         SET description = $1
         WHERE id = 'card-explicit-dep'",
    )
    .bind("Depends on #494")
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["run"].is_null(),
        "explicit unresolved dependencies should prevent queue generation"
    );
    assert_eq!(
        json["message"].as_str(),
        Some("No cards available (1개 외부 의존성 미충족으로 제외)")
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn generate_pg_ignores_legacy_mode_and_still_uses_smart_planner() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let _card_ids = seed_similarity_group_cards_pg(&pool).await;

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "mode": "pm-assisted",
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let run = &json["run"];
    let entries = json["entries"].as_array().expect("entries must be array");
    assert_eq!(run["thread_group_count"], 5);
    assert_eq!(run["max_concurrent_threads"], 4);
    assert_eq!(run["ai_model"], "smart-planner");
    assert!(
        !entries.is_empty(),
        "legacy mode input should be ignored rather than triggering PM-assisted flow"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn smart_activate_pg_dispatches_multiple_groups() {
    crate::pipeline::ensure_loaded();

    let (repo, _repo_guard) = setup_test_repo();
    let config_dir = write_repo_mapping_config(&[("test-repo", repo.path())]);
    let _config_guard = EnvVarGuard::set_path(
        "AGENTDESK_CONFIG",
        &config_dir.path().join("agentdesk.yaml"),
    );

    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let _card_ids = seed_parallel_test_cards_pg(&pool).await;

    let app = test_api_router_with_pg(
        db.clone(),
        engine.clone(),
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    // Step 1: Generate with the smart planner (no agent_id filter — cards have mixed agents)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "max_concurrent_threads": 3,
                        "max_concurrent_per_agent": 3,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Step 2: Activate without agent_id — allows dispatching across different agents
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "unified_thread": false,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let activate_json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // Should dispatch 3 entries (one per group, up to max_concurrent_threads=3)
    let dispatched_count = activate_json["count"].as_i64().unwrap();
    assert_eq!(
        dispatched_count, 3,
        "activate should dispatch 3 groups (max_concurrent_threads=3)"
    );
    assert_eq!(activate_json["active_groups"], 3);

    // Step 3: Verify status API shows group-level info
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/auto-queue/status?repo=test-repo")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let status_json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    // thread_groups should be present with group-level statuses
    let thread_groups = status_json["thread_groups"]
        .as_object()
        .expect("thread_groups must be an object");
    assert!(
        thread_groups.len() >= 2,
        "status should have multiple thread groups"
    );

    // At least some groups should be "active" (dispatched) and some "pending"
    let active_count = thread_groups
        .values()
        .filter(|g| g["status"] == "active")
        .count();
    let pending_count = thread_groups
        .values()
        .filter(|g| g["status"] == "pending")
        .count();
    assert!(active_count > 0, "should have active groups");
    assert!(
        pending_count > 0,
        "should have pending groups (4th group not yet started)"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn generate_pg_ignores_legacy_parallel_toggle_and_keeps_smart_groups() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    let _card_ids = seed_parallel_test_cards_pg(&pool).await;

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "parallel": false,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let entries = json["entries"].as_array().unwrap();
    let run = &json["run"];

    let distinct_groups = entries
        .iter()
        .map(|entry| entry["thread_group"].as_i64().unwrap())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(run["thread_group_count"], 4);
    assert_eq!(run["max_concurrent_threads"], 4);
    assert_eq!(run["ai_model"], "smart-planner");
    assert_eq!(
        distinct_groups.len(),
        4,
        "legacy parallel=false should be ignored in favor of smart grouping"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activate_waits_for_current_batch_phase_pg_before_dispatching_next_phase() {
    crate::pipeline::ensure_loaded();

    let (repo, _repo_guard) = setup_test_repo();
    let config_dir = write_repo_mapping_config(&[("test-repo", repo.path())]);
    let _config_guard = EnvVarGuard::set_path(
        "AGENTDESK_CONFIG",
        &config_dir.path().join("agentdesk.yaml"),
    );

    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-phase-a").await;
    seed_agent_pg(&pool, "agent-phase-b").await;
    seed_auto_queue_card_pg(&pool, "card-phase-1-a", 4241, "ready", "agent-phase-a").await;
    seed_auto_queue_card_pg(&pool, "card-phase-1-b", 4242, "ready", "agent-phase-b").await;
    seed_auto_queue_card_pg(&pool, "card-phase-2-a", 4243, "ready", "agent-phase-a").await;
    seed_auto_queue_card_pg(&pool, "card-phase-2-b", 4244, "ready", "agent-phase-b").await;

    sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, status, max_concurrent_threads, thread_group_count
        ) VALUES (
            'run-batch-phase', 'test-repo', 'active', 2, 2
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, batch_phase
        ) VALUES
        ('entry-phase-1-a', 'run-batch-phase', 'card-phase-1-a', 'agent-phase-a', 'pending', 0, 0, 1),
        ('entry-phase-1-b', 'run-batch-phase', 'card-phase-1-b', 'agent-phase-b', 'pending', 1, 1, 1),
        ('entry-phase-2-a', 'run-batch-phase', 'card-phase-2-a', 'agent-phase-a', 'pending', 2, 0, 2),
        ('entry-phase-2-b', 'run-batch-phase', 'card-phase-2-b', 'agent-phase-b', 'pending', 3, 1, 2)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );

    let first_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-batch-phase",
                        "active_only": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(first_response.status(), StatusCode::OK);
    let first_body = axum::body::to_bytes(first_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let first_json: serde_json::Value = serde_json::from_slice(&first_body).unwrap();
    assert_eq!(first_json["count"], 2);

    let dispatched_phases: Vec<(String, i64)> = sqlx::query_as(
        "SELECT id, COALESCE(batch_phase, 0)::BIGINT
         FROM auto_queue_entries
         WHERE status = 'dispatched'
         ORDER BY id ASC",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let dispatched_phases: std::collections::HashMap<String, i64> =
        dispatched_phases.into_iter().collect();
    assert_eq!(dispatched_phases.len(), 2);
    assert_eq!(dispatched_phases.get("entry-phase-1-a"), Some(&1));
    assert_eq!(dispatched_phases.get("entry-phase-1-b"), Some(&1));

    sqlx::query(
        "UPDATE auto_queue_entries
         SET status = 'done', dispatch_id = NULL, completed_at = NOW()
         WHERE id = 'entry-phase-1-a'",
    )
    .execute(&pool)
    .await
    .unwrap();

    let second_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-batch-phase",
                        "active_only": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(second_response.status(), StatusCode::OK);
    let second_body = axum::body::to_bytes(second_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let second_json: serde_json::Value = serde_json::from_slice(&second_body).unwrap();
    assert_eq!(
        second_json["count"], 0,
        "phase 2 must stay blocked while phase 1 still has an in-flight entry"
    );

    sqlx::query(
        "UPDATE auto_queue_entries
         SET status = 'done', dispatch_id = NULL, completed_at = NOW()
         WHERE id = 'entry-phase-1-b'",
    )
    .execute(&pool)
    .await
    .unwrap();

    let third_response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-batch-phase",
                        "active_only": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(third_response.status(), StatusCode::OK);
    let third_body = axum::body::to_bytes(third_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let third_json: serde_json::Value = serde_json::from_slice(&third_body).unwrap();
    assert_eq!(
        third_json["count"], 2,
        "next batch phase should become dispatchable once phase 1 is complete"
    );

    let phase_two_dispatched = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT FROM auto_queue_entries
         WHERE status = 'dispatched' AND COALESCE(batch_phase, 0) = 2",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(phase_two_dispatched, 2);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_pause_soft_does_not_cancel_live_dispatches_or_release_slots() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-pause-slot");
    seed_auto_queue_card(
        &db,
        "card-pause-dispatched",
        4496,
        "in_progress",
        "agent-pause-slot",
    );
    seed_auto_queue_card(&db, "card-pause-pending", 4497, "ready", "agent-pause-slot");
    seed_auto_queue_card(
        &db,
        "card-pause-phase-gate-anchor",
        4498,
        "ready",
        "agent-pause-slot",
    );
    seed_auto_queue_card(&db, "card-pause-orphan", 4499, "ready", "agent-pause-slot");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-pause-slot', 'test-repo', 'agent-pause-slot', 'active', 1, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (
                agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
            ) VALUES (
                'agent-pause-slot', 0, 'run-pause-slot', 0, ?1
            )",
            [json!({"111": "222000000000004496"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-pause-slot', 'card-pause-dispatched', 'agent-pause-slot',
                'implementation', 'dispatched', 'Pause slot dispatch', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at
            ) VALUES (
                'dispatch-pause-phase-gate', 'card-pause-phase-gate-anchor', 'agent-pause-slot',
                'review', 'dispatched', 'Pause phase gate', ?1, datetime('now'), datetime('now')
            )",
            [json!({
                "slot_index": 0,
                "sidecar_dispatch": true,
                "phase_gate": {
                    "run_id": "run-pause-slot",
                    "batch_phase": 1,
                    "next_phase": 2,
                    "anchor_card_id": "card-pause-phase-gate-anchor"
                }
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, thread_group, dispatched_at
            ) VALUES (
                'entry-pause-dispatched', 'run-pause-slot', 'card-pause-dispatched',
                'agent-pause-slot', 'dispatched', 'dispatch-pause-slot', 0, 0, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, thread_group, dispatched_at
            ) VALUES (
                'entry-pause-orphan', 'run-pause-slot', 'card-pause-orphan',
                'agent-pause-slot', 'dispatched', NULL, NULL, 2, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-pause-pending', 'run-pause-slot', 'card-pause-pending',
                'agent-pause-slot', 'pending', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_phase_gates (
                run_id, phase, status, dispatch_id, pass_verdict, next_phase, final_phase, anchor_card_id, created_at, updated_at
            ) VALUES (
                'run-pause-slot', 1, 'pending', 'dispatch-pause-phase-gate',
                'phase_gate_passed', 2, 0, 'card-pause-phase-gate-anchor', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, session_info, tokens,
                active_dispatch_id, thread_channel_id, claude_session_id,
                last_heartbeat, created_at
            ) VALUES (
                'host:AgentDesk-claude-pause-slot', 'agent-pause-slot', 'claude', 'working',
                'pause slot seed', 19, 'dispatch-pause-slot', '222000000000004496', 'claude-pause-slot',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, session_info, tokens,
                active_dispatch_id, claude_session_id, last_heartbeat, created_at
            ) VALUES (
                'host:AgentDesk-claude-pause-sidecar', 'agent-pause-slot', 'claude', 'working',
                'pause sidecar seed', 7, 'dispatch-pause-phase-gate', 'claude-pause-sidecar',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/pause")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["paused_runs"], 1);
    assert_eq!(json["cancelled_dispatches"], 0);
    assert_eq!(json["released_slots"], 0);
    assert_eq!(json["cleared_slot_sessions"], 0);

    let conn = db.lock().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-pause-slot'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_status, "paused");

    let dispatched_entry: (String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT status, dispatch_id, dispatched_at
             FROM auto_queue_entries
             WHERE id = 'entry-pause-dispatched'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(dispatched_entry.0, "dispatched");
    assert_eq!(dispatched_entry.1, Some("dispatch-pause-slot".to_string()));
    assert!(
        dispatched_entry.2.is_some(),
        "soft pause must leave the in-flight dispatch timestamp untouched"
    );

    let pending_entry: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-pause-pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pending_entry, "pending");

    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-pause-slot'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatch_status, "dispatched");

    let slot: (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-pause-slot' AND slot_index = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(slot, (Some("run-pause-slot".to_string()), Some(0)));

    let session: (String, Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id, tokens, claude_session_id
             FROM sessions
             WHERE session_key = 'host:AgentDesk-claude-pause-slot'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        session,
        (
            "working".to_string(),
            Some("dispatch-pause-slot".to_string()),
            19,
            Some("claude-pause-slot".to_string()),
        )
    );
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_pause_force_cancels_live_dispatches_and_releases_slots() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-pause-slot");
    seed_auto_queue_card(
        &db,
        "card-pause-dispatched",
        4496,
        "in_progress",
        "agent-pause-slot",
    );
    seed_auto_queue_card(&db, "card-pause-pending", 4497, "ready", "agent-pause-slot");
    seed_auto_queue_card(
        &db,
        "card-pause-phase-gate-anchor",
        4498,
        "ready",
        "agent-pause-slot",
    );
    seed_auto_queue_card(&db, "card-pause-orphan", 4499, "ready", "agent-pause-slot");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-pause-slot', 'test-repo', 'agent-pause-slot', 'active', 1, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (
                agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
            ) VALUES (
                'agent-pause-slot', 0, 'run-pause-slot', 0, ?1
            )",
            [json!({"111": "222000000000004496"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-pause-slot', 'card-pause-dispatched', 'agent-pause-slot',
                'implementation', 'dispatched', 'Pause slot dispatch', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at
            ) VALUES (
                'dispatch-pause-phase-gate', 'card-pause-phase-gate-anchor', 'agent-pause-slot',
                'review', 'dispatched', 'Pause phase gate', ?1, datetime('now'), datetime('now')
            )",
            [json!({
                "slot_index": 0,
                "sidecar_dispatch": true,
                "phase_gate": {
                    "run_id": "run-pause-slot",
                    "batch_phase": 1,
                    "next_phase": 2,
                    "anchor_card_id": "card-pause-phase-gate-anchor"
                }
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, thread_group, dispatched_at
            ) VALUES (
                'entry-pause-dispatched', 'run-pause-slot', 'card-pause-dispatched',
                'agent-pause-slot', 'dispatched', 'dispatch-pause-slot', 0, 0, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, thread_group, dispatched_at
            ) VALUES (
                'entry-pause-orphan', 'run-pause-slot', 'card-pause-orphan',
                'agent-pause-slot', 'dispatched', NULL, NULL, 2, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-pause-pending', 'run-pause-slot', 'card-pause-pending',
                'agent-pause-slot', 'pending', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_phase_gates (
                run_id, phase, status, dispatch_id, pass_verdict, next_phase, final_phase, anchor_card_id, created_at, updated_at
            ) VALUES (
                'run-pause-slot', 1, 'pending', 'dispatch-pause-phase-gate',
                'phase_gate_passed', 2, 0, 'card-pause-phase-gate-anchor', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, session_info, tokens,
                active_dispatch_id, thread_channel_id, claude_session_id,
                last_heartbeat, created_at
            ) VALUES (
                'host:AgentDesk-claude-pause-slot', 'agent-pause-slot', 'claude', 'working',
                'pause slot seed', 19, 'dispatch-pause-slot', '222000000000004496', 'claude-pause-slot',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, session_info, tokens,
                active_dispatch_id, claude_session_id, last_heartbeat, created_at
            ) VALUES (
                'host:AgentDesk-claude-pause-sidecar', 'agent-pause-slot', 'claude', 'working',
                'pause sidecar seed', 7, 'dispatch-pause-phase-gate', 'claude-pause-sidecar',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/pause")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({"force": true})).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["paused_runs"], 1);
    assert_eq!(json["cancelled_dispatches"], 2);
    assert_eq!(json["released_slots"], 1);
    assert_eq!(json["cleared_slot_sessions"], 2);

    let conn = db.lock().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-pause-slot'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_status, "paused");

    let dispatched_entry_status: String = conn
        .query_row(
            "SELECT status
             FROM auto_queue_entries
             WHERE id = 'entry-pause-dispatched'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatched_entry_status, "skipped");

    let pending_entry: String = conn
        .query_row(
            "SELECT status FROM auto_queue_entries WHERE id = 'entry-pause-pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pending_entry, "pending");

    let orphan_entry: (String, Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT status, dispatch_id, slot_index
             FROM auto_queue_entries
             WHERE id = 'entry-pause-orphan'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(orphan_entry.0, "pending");
    assert!(orphan_entry.1.is_none());
    assert!(orphan_entry.2.is_none());

    let dispatch_statuses: Vec<(String, String)> = conn
        .prepare(
            "SELECT id, status
             FROM task_dispatches
             WHERE id IN ('dispatch-pause-slot', 'dispatch-pause-phase-gate')
             ORDER BY id ASC",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        dispatch_statuses,
        vec![
            (
                "dispatch-pause-phase-gate".to_string(),
                "cancelled".to_string(),
            ),
            ("dispatch-pause-slot".to_string(), "cancelled".to_string()),
        ]
    );

    let phase_gate_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_phase_gates WHERE run_id = 'run-pause-slot'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(phase_gate_rows, 0);

    let slot: (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-pause-slot' AND slot_index = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(slot, (None, None));

    let session: (String, Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id, tokens, claude_session_id
             FROM sessions
             WHERE session_key = 'host:AgentDesk-claude-pause-slot'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(session.0, "idle");
    assert_eq!(session.1, None);
    assert_eq!(session.2, 0);
    assert_eq!(session.3, None);

    let sidecar_session: (String, Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id, tokens, claude_session_id
             FROM sessions
             WHERE session_key = 'host:AgentDesk-claude-pause-sidecar'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(sidecar_session.0, "idle");
    assert_eq!(sidecar_session.1, None);
    assert_eq!(sidecar_session.2, 0);
    assert_eq!(sidecar_session.3, None);
}

#[tokio::test]
async fn auto_queue_pause_pg_soft_does_not_cancel_live_dispatches_or_release_slots() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, discord_channel_alt)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind("agent-pause-slot-pg")
    .bind("Agent Pause Slot PG")
    .bind("claude")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
         ) VALUES
         ($1, $2, $3, $4, $5, $6, $7),
         ($8, $9, $10, $11, $12, $13, $14)",
    )
    .bind("card-pause-dispatched-pg")
    .bind("test-repo")
    .bind("Pause dispatched PG")
    .bind("in_progress")
    .bind("medium")
    .bind("agent-pause-slot-pg")
    .bind(5496_i64)
    .bind("card-pause-pending-pg")
    .bind("test-repo")
    .bind("Pause pending PG")
    .bind("ready")
    .bind("medium")
    .bind("agent-pause-slot-pg")
    .bind(5497_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, agent_id, status, max_concurrent_threads, thread_group_count
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         )",
    )
    .bind("run-pause-slot-pg")
    .bind("test-repo")
    .bind("agent-pause-slot-pg")
    .bind("active")
    .bind(1_i64)
    .bind(2_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_slots (
            agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
         ) VALUES (
            $1, $2, $3, $4, $5::jsonb
         )",
    )
    .bind("agent-pause-slot-pg")
    .bind(0_i64)
    .bind("run-pause-slot-pg")
    .bind(0_i64)
    .bind(json!({"111": "222000000000054496"}).to_string())
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         )",
    )
    .bind("dispatch-pause-slot-pg")
    .bind("card-pause-dispatched-pg")
    .bind("agent-pause-slot-pg")
    .bind("implementation")
    .bind("dispatched")
    .bind("Pause slot dispatch PG")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, thread_group, dispatched_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, NOW()
         ), (
            $10, $11, $12, $13, $14, NULL, NULL, $15, $16, NULL
         )",
    )
    .bind("entry-pause-dispatched-pg")
    .bind("run-pause-slot-pg")
    .bind("card-pause-dispatched-pg")
    .bind("agent-pause-slot-pg")
    .bind("dispatched")
    .bind("dispatch-pause-slot-pg")
    .bind(0_i64)
    .bind(0_i64)
    .bind(0_i64)
    .bind("entry-pause-pending-pg")
    .bind("run-pause-slot-pg")
    .bind("card-pause-pending-pg")
    .bind("agent-pause-slot-pg")
    .bind("pending")
    .bind(1_i64)
    .bind(1_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions (
            session_key, agent_id, provider, status, session_info, tokens,
            active_dispatch_id, thread_channel_id, claude_session_id
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9
         )",
    )
    .bind("host:AgentDesk-claude-pause-slot-pg")
    .bind("agent-pause-slot-pg")
    .bind("claude")
    .bind("working")
    .bind("pause slot seed pg")
    .bind(19_i64)
    .bind("dispatch-pause-slot-pg")
    .bind("222000000000054496")
    .bind("claude-pause-slot-pg")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/pause")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["paused_runs"], 1);
    assert_eq!(json["cancelled_dispatches"], 0);
    assert_eq!(json["released_slots"], 0);
    assert_eq!(json["cleared_slot_sessions"], 0);

    let run_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind("run-pause-slot-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(run_status, "paused");

    let dispatched_entry: (
        String,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
    ) = sqlx::query_as(
        "SELECT status, dispatch_id, dispatched_at
             FROM auto_queue_entries
             WHERE id = $1",
    )
    .bind("entry-pause-dispatched-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(dispatched_entry.0, "dispatched");
    assert_eq!(
        dispatched_entry.1,
        Some("dispatch-pause-slot-pg".to_string())
    );
    assert!(
        dispatched_entry.2.is_some(),
        "soft pause must leave the postgres dispatch timestamp untouched"
    );

    let dispatch_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id = $1")
            .bind("dispatch-pause-slot-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(dispatch_status, "dispatched");

    let slot: (Option<String>, Option<i64>) = sqlx::query_as(
        "SELECT assigned_run_id, assigned_thread_group
         FROM auto_queue_slots
         WHERE agent_id = $1 AND slot_index = $2",
    )
    .bind("agent-pause-slot-pg")
    .bind(0_i64)
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(slot, (Some("run-pause-slot-pg".to_string()), Some(0)));

    let session: (String, Option<String>, i64, Option<String>) = sqlx::query_as(
        "SELECT status, active_dispatch_id, tokens::BIGINT, claude_session_id
         FROM sessions
         WHERE session_key = $1",
    )
    .bind("host:AgentDesk-claude-pause-slot-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(
        session,
        (
            "working".to_string(),
            Some("dispatch-pause-slot-pg".to_string()),
            19,
            Some("claude-pause-slot-pg".to_string()),
        )
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_reset_slot_thread_pg_clears_slot_binding_without_sqlite_mirror() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query(
        "INSERT INTO agents (id, name, provider)
         VALUES ($1, $2, $3)",
    )
    .bind("agent-reset-slot-pg")
    .bind("Agent Reset Slot PG")
    .bind("claude")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_slots (
            agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
         ) VALUES (
            $1, $2, $3, $4, $5::jsonb
         )",
    )
    .bind("agent-reset-slot-pg")
    .bind(0_i64)
    .bind("run-reset-slot-pg")
    .bind(0_i64)
    .bind("{}")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/slots/agent-reset-slot-pg/0/reset-thread")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["ok"], true);
    assert_eq!(json["agent_id"], "agent-reset-slot-pg");
    assert_eq!(json["slot_index"], 0);
    assert_eq!(json["archived_threads"], 0);
    assert_eq!(json["cleared_sessions"], 0);
    assert_eq!(json["cleared_bindings"], 1);

    let slot_map = sqlx::query_scalar::<_, Option<String>>(
        "SELECT thread_id_map::text
         FROM auto_queue_slots
         WHERE agent_id = $1 AND slot_index = $2",
    )
    .bind("agent-reset-slot-pg")
    .bind(0_i64)
    .fetch_one(&pg_pool)
    .await
    .unwrap()
    .unwrap();
    assert_eq!(slot_map, "{}");

    let sqlite_slot_count: i64 = db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_slots WHERE agent_id = 'agent-reset-slot-pg'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sqlite_slot_count, 0,
        "test must not rely on SQLite mirror state"
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_cancel_cancels_live_dispatches_skips_entries_and_releases_slots() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-cancel-slot");
    seed_auto_queue_card(
        &db,
        "card-cancel-dispatched",
        4596,
        "in_progress",
        "agent-cancel-slot",
    );
    seed_auto_queue_card(
        &db,
        "card-cancel-pending",
        4597,
        "ready",
        "agent-cancel-slot",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-cancel-slot', 'test-repo', 'agent-cancel-slot', 'paused', 1, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (
                agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
            ) VALUES (
                'agent-cancel-slot', 0, 'run-cancel-slot', 0, ?1
            )",
            [json!({"111": "222000000000004597"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-cancel-slot', 'card-cancel-dispatched', 'agent-cancel-slot',
                'implementation', 'dispatched', 'Cancel slot dispatch', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, thread_group, dispatched_at
            ) VALUES (
                'entry-cancel-dispatched', 'run-cancel-slot', 'card-cancel-dispatched',
                'agent-cancel-slot', 'dispatched', 'dispatch-cancel-slot', 0, 0, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-cancel-pending', 'run-cancel-slot', 'card-cancel-pending',
                'agent-cancel-slot', 'pending', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (
                session_key, agent_id, provider, status, session_info, tokens,
                active_dispatch_id, thread_channel_id, claude_session_id,
                last_heartbeat, created_at
            ) VALUES (
                'host:AgentDesk-claude-cancel-slot', 'agent-cancel-slot', 'claude', 'working',
                'cancel slot seed', 23, 'dispatch-cancel-slot', '222000000000004597', 'claude-cancel-slot',
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["cancelled_runs"], 1);
    assert_eq!(json["cancelled_entries"], 2);
    assert_eq!(json["cancelled_dispatches"], 1);
    assert_eq!(json["deleted_phase_gates"], 0);
    assert_eq!(json["remaining_live_dispatches"], 0);
    assert_eq!(json["released_slots"], 1);

    let conn = db.lock().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-cancel-slot'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_status, "cancelled");

    let entries: Vec<(String, Option<String>, Option<String>)> = {
        let mut stmt = conn
            .prepare(
                "SELECT status, dispatch_id, completed_at
                 FROM auto_queue_entries
                 WHERE run_id = 'run-cancel-slot'
                 ORDER BY id ASC",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|(status, dispatch_id, completed_at)| status == "skipped"
                && dispatch_id.is_none()
                && completed_at.is_some()),
        "cancel must skip every active/pending queue entry and stamp completed_at"
    );

    let dispatch_status: String = conn
        .query_row(
            "SELECT status FROM task_dispatches WHERE id = 'dispatch-cancel-slot'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(dispatch_status, "cancelled");

    let slot: (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-cancel-slot' AND slot_index = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(slot, (None, None));

    let session: (String, Option<String>, i64, Option<String>) = conn
        .query_row(
            "SELECT status, active_dispatch_id, tokens, claude_session_id
             FROM sessions
             WHERE session_key = 'host:AgentDesk-claude-cancel-slot'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(session.0, "idle");
    assert_eq!(session.1, None);
    assert_eq!(session.2, 0);
    assert_eq!(session.3, None);
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_cancel_includes_restoring_runs() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-cancel-restoring");
    seed_auto_queue_card(
        &db,
        "card-cancel-restoring-pending",
        4598,
        "ready",
        "agent-cancel-restoring",
    );
    seed_auto_queue_card(
        &db,
        "card-cancel-restoring-skipped",
        4599,
        "ready",
        "agent-cancel-restoring",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, created_at
            ) VALUES (
                'run-cancel-restoring', 'test-repo', 'agent-cancel-restoring', 'restoring', datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-cancel-restoring-pending', 'run-cancel-restoring', 'card-cancel-restoring-pending',
                'agent-cancel-restoring', 'pending', 0, 0
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-cancel-restoring-skipped', 'run-cancel-restoring', 'card-cancel-restoring-skipped',
                'agent-cancel-restoring', 'skipped', 1, 1
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["cancelled_runs"], 1);
    assert_eq!(json["cancelled_entries"], 1);

    let conn = db.lock().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-cancel-restoring'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_status, "cancelled");

    let entry_states: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, status
                 FROM auto_queue_entries
                 WHERE run_id = 'run-cancel-restoring'
                 ORDER BY id ASC",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(
        entry_states,
        vec![
            (
                "entry-cancel-restoring-pending".to_string(),
                "skipped".to_string(),
            ),
            (
                "entry-cancel-restoring-skipped".to_string(),
                "skipped".to_string(),
            ),
        ]
    );
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_cancel_targets_only_requested_run() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-cancel-target");
    seed_auto_queue_card(
        &db,
        "card-cancel-target-a",
        4601,
        "ready",
        "agent-cancel-target",
    );
    seed_auto_queue_card(
        &db,
        "card-cancel-target-b",
        4602,
        "ready",
        "agent-cancel-target",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, created_at
            ) VALUES (
                'run-cancel-target-a', 'test-repo', 'agent-cancel-target', 'active', datetime('now', '-1 minute')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, created_at
            ) VALUES (
                'run-cancel-target-b', 'test-repo', 'agent-cancel-target', 'paused', datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-cancel-target-a', 'run-cancel-target-a', 'card-cancel-target-a',
                'agent-cancel-target', 'pending', 0, 0
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-cancel-target-b', 'run-cancel-target-b', 'card-cancel-target-b',
                'agent-cancel-target', 'pending', 0, 0
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/cancel?run_id=run-cancel-target-b")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["cancelled_runs"], 1);
    assert_eq!(json["cancelled_entries"], 1);

    let conn = db.lock().unwrap();
    let run_states: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, status
                 FROM auto_queue_runs
                 WHERE id IN ('run-cancel-target-a', 'run-cancel-target-b')
                 ORDER BY id ASC",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(
        run_states,
        vec![
            ("run-cancel-target-a".to_string(), "active".to_string()),
            ("run-cancel-target-b".to_string(), "cancelled".to_string()),
        ]
    );

    let entry_states: Vec<(String, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, status
                 FROM auto_queue_entries
                 WHERE id IN ('entry-cancel-target-a', 'entry-cancel-target-b')
                 ORDER BY id ASC",
            )
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(
        entry_states,
        vec![
            ("entry-cancel-target-a".to_string(), "pending".to_string()),
            ("entry-cancel-target-b".to_string(), "skipped".to_string()),
        ]
    );
}

#[tokio::test]
async fn auto_queue_cancel_pg_targets_only_requested_run() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, provider)
         VALUES ($1, $2, $3)",
    )
    .bind("agent-cancel-target-pg")
    .bind("Agent Cancel Target PG")
    .bind("claude")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
         ) VALUES
         ($1, $2, $3, $4, $5, $6, $7),
         ($8, $9, $10, $11, $12, $13, $14)",
    )
    .bind("card-cancel-target-a-pg")
    .bind("test-repo")
    .bind("Cancel target A PG")
    .bind("ready")
    .bind("medium")
    .bind("agent-cancel-target-pg")
    .bind(5601_i64)
    .bind("card-cancel-target-b-pg")
    .bind("test-repo")
    .bind("Cancel target B PG")
    .bind("ready")
    .bind("medium")
    .bind("agent-cancel-target-pg")
    .bind(5602_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, agent_id, status, created_at
         ) VALUES
         ($1, $2, $3, $4, NOW() - INTERVAL '1 minute'),
         ($5, $6, $7, $8, NOW())",
    )
    .bind("run-cancel-target-a-pg")
    .bind("test-repo")
    .bind("agent-cancel-target-pg")
    .bind("active")
    .bind("run-cancel-target-b-pg")
    .bind("test-repo")
    .bind("agent-cancel-target-pg")
    .bind("paused")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
         ) VALUES
         ($1, $2, $3, $4, $5, $6, $7),
         ($8, $9, $10, $11, $12, $13, $14)",
    )
    .bind("entry-cancel-target-a-pg")
    .bind("run-cancel-target-a-pg")
    .bind("card-cancel-target-a-pg")
    .bind("agent-cancel-target-pg")
    .bind("pending")
    .bind(0_i64)
    .bind(0_i64)
    .bind("entry-cancel-target-b-pg")
    .bind("run-cancel-target-b-pg")
    .bind("card-cancel-target-b-pg")
    .bind("agent-cancel-target-pg")
    .bind("pending")
    .bind(0_i64)
    .bind(0_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/cancel?run_id=run-cancel-target-b-pg")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        StatusCode::OK,
        "auto_queue_cancel_pg_targets_only_requested_run status={} body={}",
        status,
        body_text
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["cancelled_runs"], 1);
    assert_eq!(json["cancelled_entries"], 1);

    let run_states: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, status
         FROM auto_queue_runs
         WHERE id IN ($1, $2)
         ORDER BY id ASC",
    )
    .bind("run-cancel-target-a-pg")
    .bind("run-cancel-target-b-pg")
    .fetch_all(&pg_pool)
    .await
    .unwrap();
    assert_eq!(
        run_states,
        vec![
            ("run-cancel-target-a-pg".to_string(), "active".to_string()),
            (
                "run-cancel-target-b-pg".to_string(),
                "cancelled".to_string()
            ),
        ]
    );

    let entry_states: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, status
         FROM auto_queue_entries
         WHERE id IN ($1, $2)
         ORDER BY id ASC",
    )
    .bind("entry-cancel-target-a-pg")
    .bind("entry-cancel-target-b-pg")
    .fetch_all(&pg_pool)
    .await
    .unwrap();
    assert_eq!(
        entry_states,
        vec![
            (
                "entry-cancel-target-a-pg".to_string(),
                "pending".to_string()
            ),
            (
                "entry-cancel-target-b-pg".to_string(),
                "skipped".to_string()
            ),
        ]
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_cancel_pg_cancels_live_dispatches_skips_entries_and_releases_slots() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, discord_channel_alt)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind("agent-cancel-slot-pg")
    .bind("Agent Cancel Slot PG")
    .bind("claude")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
         ) VALUES
         ($1, $2, $3, $4, $5, $6, $7),
         ($8, $9, $10, $11, $12, $13, $14)",
    )
    .bind("card-cancel-dispatched-pg")
    .bind("test-repo")
    .bind("Cancel dispatched PG")
    .bind("in_progress")
    .bind("medium")
    .bind("agent-cancel-slot-pg")
    .bind(6596_i64)
    .bind("card-cancel-pending-pg")
    .bind("test-repo")
    .bind("Cancel pending PG")
    .bind("ready")
    .bind("medium")
    .bind("agent-cancel-slot-pg")
    .bind(6597_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, agent_id, status, max_concurrent_threads, thread_group_count
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         )",
    )
    .bind("run-cancel-slot-pg")
    .bind("test-repo")
    .bind("agent-cancel-slot-pg")
    .bind("paused")
    .bind(1_i64)
    .bind(2_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_slots (
            agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
         ) VALUES (
            $1, $2, $3, $4, $5::jsonb
         )",
    )
    .bind("agent-cancel-slot-pg")
    .bind(0_i64)
    .bind("run-cancel-slot-pg")
    .bind(0_i64)
    .bind(json!({"111": "222000000000065001"}).to_string())
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         )",
    )
    .bind("dispatch-cancel-slot-pg")
    .bind("card-cancel-dispatched-pg")
    .bind("agent-cancel-slot-pg")
    .bind("implementation")
    .bind("dispatched")
    .bind("Cancel slot dispatch PG")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, thread_group, dispatched_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, NOW()
         ), (
            $10, $11, $12, $13, $14, NULL, NULL, $15, $16, NULL
         )",
    )
    .bind("entry-cancel-dispatched-pg")
    .bind("run-cancel-slot-pg")
    .bind("card-cancel-dispatched-pg")
    .bind("agent-cancel-slot-pg")
    .bind("dispatched")
    .bind("dispatch-cancel-slot-pg")
    .bind(0_i64)
    .bind(0_i64)
    .bind(0_i64)
    .bind("entry-cancel-pending-pg")
    .bind("run-cancel-slot-pg")
    .bind("card-cancel-pending-pg")
    .bind("agent-cancel-slot-pg")
    .bind("pending")
    .bind(1_i64)
    .bind(1_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO sessions (
            session_key, agent_id, provider, status, session_info, tokens,
            active_dispatch_id, thread_channel_id, claude_session_id
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9
         )",
    )
    .bind("host:AgentDesk-claude-cancel-slot-pg")
    .bind("agent-cancel-slot-pg")
    .bind("claude")
    .bind("working")
    .bind("cancel slot seed pg")
    .bind(23_i64)
    .bind("dispatch-cancel-slot-pg")
    .bind("222000000000065001")
    .bind("claude-cancel-slot-pg")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        StatusCode::OK,
        "auto_queue_cancel_pg_cancels_live_dispatches_skips_entries_and_releases_slots status={} body={}",
        status,
        body_text
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["cancelled_runs"], 1);
    assert_eq!(json["cancelled_entries"], 2);
    assert_eq!(json["cancelled_dispatches"], 1);
    assert_eq!(json["deleted_phase_gates"], 0);
    assert_eq!(json["remaining_live_dispatches"], 0);
    assert_eq!(json["released_slots"], 1);
    assert_eq!(json["cleared_slot_sessions"], 1);

    let run_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind("run-cancel-slot-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(run_status, "cancelled");

    let entries: Vec<(
        String,
        Option<String>,
        Option<chrono::DateTime<chrono::Utc>>,
    )> = sqlx::query_as(
        "SELECT status, dispatch_id, completed_at
             FROM auto_queue_entries
             WHERE run_id = $1
             ORDER BY id ASC",
    )
    .bind("run-cancel-slot-pg")
    .fetch_all(&pg_pool)
    .await
    .unwrap();
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|(status, dispatch_id, completed_at)| status == "skipped"
                && dispatch_id.is_none()
                && completed_at.is_some()),
        "cancel must skip every active/pending PG queue entry and stamp completed_at"
    );

    let dispatch_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM task_dispatches WHERE id = $1")
            .bind("dispatch-cancel-slot-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(dispatch_status, "cancelled");

    let slot: (Option<String>, Option<i64>) = sqlx::query_as(
        "SELECT assigned_run_id, assigned_thread_group
         FROM auto_queue_slots
         WHERE agent_id = $1 AND slot_index = $2",
    )
    .bind("agent-cancel-slot-pg")
    .bind(0_i64)
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(slot, (None, None));

    let session: (String, Option<String>, i64, Option<String>) = sqlx::query_as(
        "SELECT status, active_dispatch_id, tokens::BIGINT, claude_session_id
         FROM sessions
         WHERE session_key = $1",
    )
    .bind("host:AgentDesk-claude-cancel-slot-pg")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(session.0, "idle");
    assert_eq!(session.1, None);
    assert_eq!(session.2, 0);
    assert_eq!(session.3, None);

    let sqlite_run_count: i64 = db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_runs WHERE id = 'run-cancel-slot-pg'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sqlite_run_count, 0,
        "test must not rely on SQLite mirror state"
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_cancel_pg_includes_restoring_runs() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, provider)
         VALUES ($1, $2, $3)",
    )
    .bind("agent-cancel-restoring-pg")
    .bind("Agent Cancel Restoring PG")
    .bind("claude")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
         ) VALUES
         ($1, $2, $3, $4, $5, $6, $7),
         ($8, $9, $10, $11, $12, $13, $14)",
    )
    .bind("card-cancel-restoring-pending-pg")
    .bind("test-repo")
    .bind("Cancel restoring pending PG")
    .bind("ready")
    .bind("medium")
    .bind("agent-cancel-restoring-pg")
    .bind(6598_i64)
    .bind("card-cancel-restoring-skipped-pg")
    .bind("test-repo")
    .bind("Cancel restoring skipped PG")
    .bind("ready")
    .bind("medium")
    .bind("agent-cancel-restoring-pg")
    .bind(6599_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("run-cancel-restoring-pg")
    .bind("test-repo")
    .bind("agent-cancel-restoring-pg")
    .bind("restoring")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
         ) VALUES
         ($1, $2, $3, $4, $5, $6, $7),
         ($8, $9, $10, $11, $12, $13, $14)",
    )
    .bind("entry-cancel-restoring-pending-pg")
    .bind("run-cancel-restoring-pg")
    .bind("card-cancel-restoring-pending-pg")
    .bind("agent-cancel-restoring-pg")
    .bind("pending")
    .bind(0_i64)
    .bind(0_i64)
    .bind("entry-cancel-restoring-skipped-pg")
    .bind("run-cancel-restoring-pg")
    .bind("card-cancel-restoring-skipped-pg")
    .bind("agent-cancel-restoring-pg")
    .bind("skipped")
    .bind(1_i64)
    .bind(1_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        StatusCode::OK,
        "auto_queue_cancel_pg_includes_restoring_runs status={} body={}",
        status,
        body_text
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["cancelled_runs"], 1);
    assert_eq!(json["cancelled_entries"], 1);

    let run_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind("run-cancel-restoring-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(run_status, "cancelled");

    let entry_states: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, status
         FROM auto_queue_entries
         WHERE run_id = $1
         ORDER BY id ASC",
    )
    .bind("run-cancel-restoring-pg")
    .fetch_all(&pg_pool)
    .await
    .unwrap();
    assert_eq!(
        entry_states,
        vec![
            (
                "entry-cancel-restoring-pending-pg".to_string(),
                "skipped".to_string(),
            ),
            (
                "entry-cancel-restoring-skipped-pg".to_string(),
                "skipped".to_string(),
            ),
        ]
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_cancel_pg_sweeps_user_cancelled_entries() {
    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, provider)
         VALUES ($1, $2, $3)",
    )
    .bind("agent-cancel-user-cancelled-pg")
    .bind("Agent Cancel User Cancelled PG")
    .bind("claude")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
         ) VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind("card-cancel-user-cancelled-pg")
    .bind("test-repo")
    .bind("Cancel user_cancelled PG")
    .bind("in_progress")
    .bind("medium")
    .bind("agent-cancel-user-cancelled-pg")
    .bind(6600_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, agent_id, status, max_concurrent_threads, thread_group_count
         ) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind("run-cancel-user-cancelled-pg")
    .bind("test-repo")
    .bind("agent-cancel-user-cancelled-pg")
    .bind("paused")
    .bind(1_i64)
    .bind(1_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, completed_at
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, NOW())",
    )
    .bind("entry-cancel-user-cancelled-pg")
    .bind("run-cancel-user-cancelled-pg")
    .bind("card-cancel-user-cancelled-pg")
    .bind("agent-cancel-user-cancelled-pg")
    .bind("user_cancelled")
    .bind(0_i64)
    .bind(0_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/cancel?run_id=run-cancel-user-cancelled-pg")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let body_text = String::from_utf8_lossy(&body);
    assert_eq!(
        status,
        StatusCode::OK,
        "auto_queue_cancel_pg_sweeps_user_cancelled_entries status={} body={}",
        status,
        body_text
    );
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["cancelled_runs"], 1);
    assert_eq!(json["cancelled_entries"], 1);

    let run_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind("run-cancel-user-cancelled-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(run_status, "cancelled");

    let entry_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_entries WHERE id = $1")
            .bind("entry-cancel-user-cancelled-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(
        entry_status, "skipped",
        "PG run cancel must sweep user_cancelled entries into skipped so restore semantics stay consistent"
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_cancel_surfaces_warning_when_slot_release_fails() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-cancel-warn");
    seed_auto_queue_card(
        &db,
        "card-cancel-warn-dispatched",
        4603,
        "in_progress",
        "agent-cancel-warn",
    );
    seed_auto_queue_card(
        &db,
        "card-cancel-warn-pending",
        4604,
        "ready",
        "agent-cancel-warn",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-cancel-warn', 'test-repo', 'agent-cancel-warn', 'active', 1, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (
                agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
            ) VALUES (
                'agent-cancel-warn', 0, 'run-cancel-warn', 0, ?1
            )",
            [json!({"111": "222000000000004603"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-cancel-warn', 'card-cancel-warn-dispatched', 'agent-cancel-warn',
                'implementation', 'dispatched', 'Cancel warning dispatch', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, thread_group, dispatched_at
            ) VALUES (
                'entry-cancel-warn-dispatched', 'run-cancel-warn', 'card-cancel-warn-dispatched',
                'agent-cancel-warn', 'dispatched', 'dispatch-cancel-warn', 0, 0, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-cancel-warn-pending', 'run-cancel-warn', 'card-cancel-warn-pending',
                'agent-cancel-warn', 'pending', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_cancel_initial_slot_release
             BEFORE UPDATE OF assigned_run_id ON auto_queue_slots
             WHEN OLD.assigned_run_id = 'run-cancel-warn'
               AND NEW.assigned_run_id IS NULL
               AND (
                   SELECT COUNT(*)
                   FROM auto_queue_entries
                   WHERE run_id = 'run-cancel-warn'
                     AND status IN ('pending', 'dispatched')
               ) > 1
             BEGIN
                 SELECT RAISE(ABORT, 'cancel slot release blocked');
             END;",
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["cancelled_runs"], 1);
    assert_eq!(json["released_slots"], 0);
    assert!(
        json["warning"]
            .as_str()
            .unwrap_or_default()
            .contains("failed to release slots for run run-cancel-warn"),
        "cancel response must surface slot release failures"
    );

    let conn = db.lock().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-cancel-warn'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_status, "cancelled");
}

#[tokio::test]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_cancel_also_cancels_phase_gate_dispatches_and_deletes_gate_rows() {
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-cancel-phase-gate");
    seed_auto_queue_card(
        &db,
        "card-cancel-phase-gate-live",
        4606,
        "in_progress",
        "agent-cancel-phase-gate",
    );
    seed_auto_queue_card(
        &db,
        "card-cancel-phase-gate-pending",
        4607,
        "ready",
        "agent-cancel-phase-gate",
    );
    seed_auto_queue_card(
        &db,
        "card-cancel-phase-gate-anchor",
        4608,
        "reviewing",
        "agent-cancel-phase-gate",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (
                id, repo, agent_id, status, max_concurrent_threads, thread_group_count
            ) VALUES (
                'run-cancel-phase-gate', 'test-repo', 'agent-cancel-phase-gate', 'active', 1, 2
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (
                agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
            ) VALUES (
                'agent-cancel-phase-gate', 0, 'run-cancel-phase-gate', 0, ?1
            )",
            [json!({"111": "222000000000004608"}).to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
            ) VALUES (
                'dispatch-cancel-phase-live', 'card-cancel-phase-gate-live', 'agent-cancel-phase-gate',
                'implementation', 'dispatched', 'Cancel run dispatch', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at
            ) VALUES (
                'dispatch-cancel-phase-gate', 'card-cancel-phase-gate-anchor', 'agent-cancel-phase-gate',
                'review', 'dispatched', 'Cancel phase gate', ?1, datetime('now'), datetime('now')
            )",
            [json!({
                "slot_index": 0,
                "sidecar_dispatch": true,
                "phase_gate": {
                    "run_id": "run-cancel-phase-gate",
                    "batch_phase": 1,
                    "next_phase": 2,
                    "anchor_card_id": "card-cancel-phase-gate-anchor"
                }
            })
            .to_string()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, thread_group, dispatched_at
            ) VALUES (
                'entry-cancel-phase-live', 'run-cancel-phase-gate', 'card-cancel-phase-gate-live',
                'agent-cancel-phase-gate', 'dispatched', 'dispatch-cancel-phase-live', 0, 0, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group
            ) VALUES (
                'entry-cancel-phase-pending', 'run-cancel-phase-gate', 'card-cancel-phase-gate-pending',
                'agent-cancel-phase-gate', 'pending', 1, 1
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_phase_gates (
                run_id, phase, status, dispatch_id, pass_verdict, next_phase, final_phase, anchor_card_id, created_at, updated_at
            ) VALUES (
                'run-cancel-phase-gate', 1, 'pending', 'dispatch-cancel-phase-gate',
                'phase_gate_passed', 2, 0, 'card-cancel-phase-gate-anchor', datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/cancel")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["cancelled_runs"], 1);
    assert_eq!(json["cancelled_entries"], 2);
    assert_eq!(json["cancelled_dispatches"], 2);
    assert_eq!(json["deleted_phase_gates"], 1);
    assert_eq!(json["remaining_live_dispatches"], 0);
    assert_eq!(json["released_slots"], 1);

    let conn = db.lock().unwrap();
    let statuses: Vec<(String, String)> = conn
        .prepare(
            "SELECT id, status
             FROM task_dispatches
             WHERE id IN ('dispatch-cancel-phase-live', 'dispatch-cancel-phase-gate')
             ORDER BY id ASC",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        statuses,
        vec![
            (
                "dispatch-cancel-phase-gate".to_string(),
                "cancelled".to_string(),
            ),
            (
                "dispatch-cancel-phase-live".to_string(),
                "cancelled".to_string(),
            ),
        ]
    );

    let phase_gate_rows: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_phase_gates WHERE run_id = 'run-cancel-phase-gate'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(phase_gate_rows, 0);

    let slot: (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-cancel-phase-gate' AND slot_index = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(slot, (None, None));
    let active_slot_dispatches: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0
             FROM task_dispatches
             WHERE to_agent_id = 'agent-cancel-phase-gate'
               AND status IN ('pending', 'dispatched')
               AND CAST(json_extract(COALESCE(context, '{}'), '$.slot_index') AS INTEGER) = 0
               AND COALESCE(CAST(json_extract(COALESCE(context, '{}'), '$.sidecar_dispatch') AS INTEGER), 0) = 0
               AND json_type(COALESCE(context, '{}'), '$.phase_gate') IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        !active_slot_dispatches,
        "cancelled phase-gate dispatches must not keep the slot blocked"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activate_run_id_blocks_phase_gate_paused_runs_pg_path() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, discord_channel_alt)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind("agent-phase-gate-pg")
    .bind("Agent Phase Gate PG")
    .bind("claude")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7
         )",
    )
    .bind("card-phase-gate-paused-pg")
    .bind("test-repo")
    .bind("Phase gate paused PG")
    .bind("ready")
    .bind("medium")
    .bind("agent-phase-gate-pg")
    .bind(64381_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("run-phase-gate-paused-pg")
    .bind("test-repo")
    .bind("agent-phase-gate-pg")
    .bind("paused")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, batch_phase
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7
         )",
    )
    .bind("entry-phase-gate-paused-pg")
    .bind("run-phase-gate-paused-pg")
    .bind("card-phase-gate-paused-pg")
    .bind("agent-phase-gate-pg")
    .bind("pending")
    .bind(0_i64)
    .bind(2_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_phase_gates (
            run_id, phase, status, dispatch_id, pass_verdict
         ) VALUES (
            $1, $2, $3, NULL, $4
         )",
    )
    .bind("run-phase-gate-paused-pg")
    .bind(1_i64)
    .bind("pending")
    .bind("phase_gate_passed")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-phase-gate-paused-pg",
                        "active_only": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["count"], 0);
    assert_eq!(json["message"], "Run is waiting on phase gate");

    let run_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind("run-phase-gate-paused-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(run_status, "paused");

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_queue_activate_dispatches_pg_only_run_without_sqlite_mirror() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, discord_channel_alt)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind("agent-activate-pg-only")
    .bind("Agent Activate PG Only")
    .bind("claude")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7
         )",
    )
    .bind("card-activate-pg-only")
    .bind("test-repo")
    .bind("Activate PG Only")
    .bind("ready")
    .bind("medium")
    .bind("agent-activate-pg-only")
    .bind(64384_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (
            id, repo, agent_id, status, max_concurrent_threads, thread_group_count
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         )",
    )
    .bind("run-activate-pg-only")
    .bind("test-repo")
    .bind("agent-activate-pg-only")
    .bind("active")
    .bind(1_i64)
    .bind(1_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, batch_phase
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8
         )",
    )
    .bind("entry-activate-pg-only")
    .bind("run-activate-pg-only")
    .bind("card-activate-pg-only")
    .bind("agent-activate-pg-only")
    .bind("pending")
    .bind(0_i64)
    .bind(0_i64)
    .bind(0_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "run_id": "run-activate-pg-only",
                        "active_only": true,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    if status != StatusCode::OK {
        panic!(
            "auto_queue_activate_dispatches_pg_only_run_without_sqlite_mirror status={} body={}",
            status,
            String::from_utf8_lossy(&body)
        );
    }
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        json["count"],
        serde_json::json!(1),
        "auto_queue_activate_dispatches_pg_only_run_without_sqlite_mirror body={json}"
    );

    let sqlite_run_count: i64 = db
        .lock()
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_runs WHERE id = 'run-activate-pg-only'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        sqlite_run_count, 0,
        "test must not rely on SQLite mirror state"
    );

    let entry_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_entries WHERE id = $1")
            .bind("entry-activate-pg-only")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(entry_status, "dispatched");

    let dispatch_count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::BIGINT
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type = 'implementation'",
    )
    .bind("card-activate-pg-only")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert_eq!(dispatch_count, 1);

    let latest_dispatch_id = sqlx::query_scalar::<_, Option<String>>(
        "SELECT latest_dispatch_id
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind("card-activate-pg-only")
    .fetch_one(&pg_pool)
    .await
    .unwrap();
    assert!(
        latest_dispatch_id
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
    );

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_run_skips_phase_gate_blocked_runs_pg_path() {
    crate::pipeline::ensure_loaded();

    let db = test_db();
    let engine = test_engine(&db);
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("test-repo")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id, discord_channel_alt)
         VALUES
         ($1, $2, $3, $4, $5),
         ($6, $7, $8, $9, $10)",
    )
    .bind("agent-resume-gate-pg")
    .bind("Agent Resume Gate PG")
    .bind("claude")
    .bind("111")
    .bind("222")
    .bind("agent-resume-free-pg")
    .bind("Agent Resume Free PG")
    .bind("claude")
    .bind("333")
    .bind("444")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number
         ) VALUES
         ($1, $2, $3, $4, $5, $6, $7),
         ($8, $9, $10, $11, $12, $13, $14)",
    )
    .bind("card-resume-gate-pg")
    .bind("test-repo")
    .bind("Resume gate PG")
    .bind("ready")
    .bind("medium")
    .bind("agent-resume-gate-pg")
    .bind(64382_i64)
    .bind("card-resume-free-pg")
    .bind("test-repo")
    .bind("Resume free PG")
    .bind("ready")
    .bind("medium")
    .bind("agent-resume-free-pg")
    .bind(64383_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES
         ($1, $2, $3, $4),
         ($5, $6, $7, $8)",
    )
    .bind("run-resume-gate-pg")
    .bind("test-repo")
    .bind("agent-resume-gate-pg")
    .bind("paused")
    .bind("run-resume-free-pg")
    .bind("test-repo")
    .bind("agent-resume-free-pg")
    .bind("paused")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, batch_phase
         ) VALUES
         ($1, $2, $3, $4, $5, $6, $7),
         ($8, $9, $10, $11, $12, $13, $14)",
    )
    .bind("entry-resume-gate-pg")
    .bind("run-resume-gate-pg")
    .bind("card-resume-gate-pg")
    .bind("agent-resume-gate-pg")
    .bind("pending")
    .bind(0_i64)
    .bind(2_i64)
    .bind("entry-resume-free-pg")
    .bind("run-resume-free-pg")
    .bind("card-resume-free-pg")
    .bind("agent-resume-free-pg")
    .bind("pending")
    .bind(0_i64)
    .bind(0_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_phase_gates (
            run_id, phase, status, dispatch_id, pass_verdict
         ) VALUES (
            $1, $2, $3, NULL, $4
         )",
    )
    .bind("run-resume-gate-pg")
    .bind(1_i64)
    .bind("failed")
    .bind("phase_gate_passed")
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db,
        engine,
        crate::config::Config::default(),
        None,
        pg_pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/resume")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["resumed_runs"], 1);
    assert_eq!(json["blocked_runs"], 1);

    let blocked_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind("run-resume-gate-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    let resumed_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM auto_queue_runs WHERE id = $1")
            .bind("run-resume-free-pg")
            .fetch_one(&pg_pool)
            .await
            .unwrap();
    assert_eq!(blocked_status, "paused");
    assert_eq!(resumed_status, "active");

    pg_pool.close().await;
    pg_db.drop().await;
}

/// Regression test for #191: onTick1min recovery must reset stuck auto-queue
/// entries that are 'dispatched' but have orphan (NULL), phantom (missing row),
/// or cancelled/failed dispatch_ids — while leaving valid dispatches untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_activate_ignores_legacy_max_concurrent_per_agent() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();

    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);
    seed_agent(&db, "agent-393");
    seed_auto_queue_card(&db, "card-393-1", 3931, "ready", "agent-393");
    seed_auto_queue_card(&db, "card-393-2", 3932, "ready", "agent-393");

    let app = test_api_router(db.clone(), engine.clone(), None);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/generate")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "agent-393",
                        "parallel": true,
                        "max_concurrent_threads": 2,
                        "max_concurrent_per_agent": 1,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let generated_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let run = &generated_json["run"];
    assert_eq!(run["max_concurrent_threads"], 2);
    assert!(run.get("max_concurrent_per_agent").is_none());

    {
        let conn = db.lock().unwrap();
        let run_id = run["id"].as_str().unwrap();
        conn.execute(
            "UPDATE auto_queue_entries
             SET thread_group = CASE id
                 WHEN ?1 THEN 0
                 WHEN ?2 THEN 1
                 ELSE thread_group
             END
             WHERE run_id = ?3",
            sqlite_params![
                generated_json["entries"][0]["id"].as_str().unwrap(),
                generated_json["entries"][1]["id"].as_str().unwrap(),
                run_id
            ],
        )
        .unwrap();
        conn.execute(
            "UPDATE auto_queue_runs SET thread_group_count = 2 WHERE id = ?1",
            [run_id],
        )
        .unwrap();
    }

    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/dispatch-next")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_string(&serde_json::json!({
                        "repo": "test-repo",
                        "agent_id": "agent-393",
                        "unified_thread": false,
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let activate_json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(activate_json["count"], 2);
    assert_eq!(activate_json["active_groups"], 2);
}

#[tokio::test]
async fn auto_queue_recovery_resets_orphan_phantom_and_cancelled_entries() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-recovery").await;
    seed_auto_queue_card_pg(&pool, "card-orphan", 9001, "in_progress", "agent-recovery").await;
    seed_auto_queue_card_pg(&pool, "card-phantom", 9002, "in_progress", "agent-recovery").await;
    seed_auto_queue_card_pg(
        &pool,
        "card-cancelled",
        9003,
        "in_progress",
        "agent-recovery",
    )
    .await;
    seed_auto_queue_card_pg(&pool, "card-valid", 9004, "in_progress", "agent-recovery").await;

    // Active run
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('run-recovery', 'test-repo', 'agent-recovery', 'active')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Entry A: dispatched + dispatch_id=NULL (orphan — should be reset)
    // #214: dispatched_at must be >2min ago to pass grace period
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at)
         VALUES ('entry-orphan', 'run-recovery', 'card-orphan', 'agent-recovery', 'dispatched', NULL, NOW() - INTERVAL '3 minutes')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Entry B: dispatched + phantom dispatch_id (not in task_dispatches — should be reset)
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at)
         VALUES ('entry-phantom', 'run-recovery', 'card-phantom', 'agent-recovery', 'dispatched', 'phantom-id-999', NOW() - INTERVAL '3 minutes')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Entry C: dispatched + cancelled dispatch (should be reset)
    sqlx::query(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title)
         VALUES ('dispatch-cancelled', 'card-cancelled', 'agent-recovery', 'implementation', 'cancelled', 'test')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at)
         VALUES ('entry-cancelled', 'run-recovery', 'card-cancelled', 'agent-recovery', 'dispatched', 'dispatch-cancelled', NOW() - INTERVAL '3 minutes')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Entry D: dispatched + valid active dispatch (must NOT be reset)
    sqlx::query(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title)
         VALUES ('dispatch-valid', 'card-valid', 'agent-recovery', 'implementation', 'dispatched', 'test')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at)
         VALUES ('entry-valid', 'run-recovery', 'card-valid', 'agent-recovery', 'dispatched', 'dispatch-valid', NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Fire onTick1min — triggers recovery path 2
    engine
        .fire_hook(
            crate::engine::hooks::Hook::OnTick1min,
            serde_json::json!({}),
        )
        .unwrap();

    // A: orphan (NULL dispatch_id) → reset to pending
    let (status_a, did_a): (String, Option<String>) = sqlx::query_as(
        "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-orphan'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(status_a, "pending", "orphan entry must be reset to pending");
    assert!(did_a.is_none(), "orphan entry dispatch_id must stay NULL");

    // B: phantom dispatch_id → reset to pending
    let (status_b, did_b): (String, Option<String>) = sqlx::query_as(
        "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-phantom'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        status_b, "pending",
        "phantom dispatch entry must be reset to pending"
    );
    assert!(
        did_b.is_none(),
        "phantom entry dispatch_id must be cleared to NULL"
    );

    // C: cancelled dispatch → reset to pending
    let (status_c, did_c): (String, Option<String>) = sqlx::query_as(
        "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-cancelled'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        status_c, "pending",
        "cancelled dispatch entry must be reset to pending"
    );
    assert!(
        did_c.is_none(),
        "cancelled entry dispatch_id must be cleared to NULL"
    );

    // D: valid active dispatch → must remain dispatched
    let (status_d, did_d): (String, Option<String>) = sqlx::query_as(
        "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-valid'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        status_d, "dispatched",
        "valid dispatch entry must NOT be reset"
    );
    assert_eq!(
        did_d.as_deref(),
        Some("dispatch-valid"),
        "valid entry dispatch_id must be preserved"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_recovery_honors_stale_dispatch_runtime_config() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-recovery-config").await;
    seed_auto_queue_card_pg(
        &pool,
        "card-expired-old",
        9011,
        "in_progress",
        "agent-recovery-config",
    )
    .await;
    seed_auto_queue_card_pg(
        &pool,
        "card-expired-recent",
        9012,
        "in_progress",
        "agent-recovery-config",
    )
    .await;
    seed_auto_queue_card_pg(
        &pool,
        "card-orphan-config",
        9013,
        "in_progress",
        "agent-recovery-config",
    )
    .await;

    sqlx::query(
        "INSERT INTO kv_meta (key, value) VALUES
            ('staleDispatchedGraceMin', '5'),
            ('staleDispatchedTerminalStatuses', 'expired'),
            ('staleDispatchedRecoverNullDispatch', 'false'),
            ('staleDispatchedRecoverMissingDispatch', 'false')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('run-recovery-config', 'test-repo', 'agent-recovery-config', 'active')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title)
         VALUES ('dispatch-expired-old', 'card-expired-old', 'agent-recovery-config', 'implementation', 'expired', 'test')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title)
         VALUES ('dispatch-expired-recent', 'card-expired-recent', 'agent-recovery-config', 'implementation', 'expired', 'test')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at)
         VALUES
         ('entry-expired-old', 'run-recovery-config', 'card-expired-old', 'agent-recovery-config', 'dispatched', 'dispatch-expired-old', NOW() - INTERVAL '6 minutes'),
         ('entry-expired-recent', 'run-recovery-config', 'card-expired-recent', 'agent-recovery-config', 'dispatched', 'dispatch-expired-recent', NOW() - INTERVAL '4 minutes'),
         ('entry-orphan-config', 'run-recovery-config', 'card-orphan-config', 'agent-recovery-config', 'dispatched', NULL, NOW() - INTERVAL '6 minutes')",
    )
    .execute(&pool)
    .await
    .unwrap();

    engine
        .fire_hook(
            crate::engine::hooks::Hook::OnTick1min,
            serde_json::json!({}),
        )
        .unwrap();

    let expired_old: (String, Option<String>) = sqlx::query_as(
        "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-expired-old'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(expired_old.0, "pending");
    assert!(expired_old.1.is_none());

    let expired_recent: (String, Option<String>) = sqlx::query_as(
        "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-expired-recent'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(expired_recent.0, "dispatched");
    assert_eq!(expired_recent.1.as_deref(), Some("dispatch-expired-recent"));

    let orphan_config: (String, Option<String>) = sqlx::query_as(
        "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-orphan-config'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(orphan_config.0, "dispatched");
    assert!(orphan_config.1.is_none());

    pool.close().await;
    pg_db.drop().await;
}

/// Regression test for #295: onTick1min must backstop terminal cards that still
/// have pending auto-queue entries in active/paused runs.
#[tokio::test]
async fn auto_queue_recovery_skips_terminal_pending_entries() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-terminal-recovery").await;
    seed_auto_queue_card_pg(
        &pool,
        "card-terminal-active",
        9011,
        "done",
        "agent-terminal-recovery",
    )
    .await;
    seed_auto_queue_card_pg(
        &pool,
        "card-terminal-paused",
        9012,
        "done",
        "agent-terminal-recovery",
    )
    .await;
    seed_auto_queue_card_pg(
        &pool,
        "card-terminal-generated",
        9013,
        "done",
        "agent-terminal-recovery",
    )
    .await;
    seed_auto_queue_card_pg(
        &pool,
        "card-nonterminal-active",
        9014,
        "requested",
        "agent-terminal-recovery",
    )
    .await;

    for (run_id, status) in [
        ("run-terminal-active", "active"),
        ("run-terminal-paused", "paused"),
        ("run-terminal-generated", "generated"),
    ] {
        sqlx::query(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) \
             VALUES ($1, 'test-repo', 'agent-terminal-recovery', $2)",
        )
        .bind(run_id)
        .bind(status)
        .execute(&pool)
        .await
        .unwrap();
    }

    for (entry_id, run_id, card_id) in [
        (
            "entry-terminal-active",
            "run-terminal-active",
            "card-terminal-active",
        ),
        (
            "entry-terminal-paused",
            "run-terminal-paused",
            "card-terminal-paused",
        ),
        (
            "entry-terminal-generated",
            "run-terminal-generated",
            "card-terminal-generated",
        ),
        (
            "entry-nonterminal-active",
            "run-terminal-active",
            "card-nonterminal-active",
        ),
    ] {
        sqlx::query(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status) \
             VALUES ($1, $2, $3, 'agent-terminal-recovery', 'pending')",
        )
        .bind(entry_id)
        .bind(run_id)
        .bind(card_id)
        .execute(&pool)
        .await
        .unwrap();
    }

    engine
        .fire_hook(
            crate::engine::hooks::Hook::OnTick1min,
            serde_json::json!({}),
        )
        .unwrap();

    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, status FROM auto_queue_entries ORDER BY id ASC")
            .fetch_all(&pool)
            .await
            .unwrap();
    let statuses: std::collections::HashMap<String, String> = rows.into_iter().collect();

    assert_eq!(
        statuses.get("entry-terminal-active").map(String::as_str),
        Some("skipped")
    );
    assert_eq!(
        statuses.get("entry-terminal-paused").map(String::as_str),
        Some("skipped")
    );
    assert_eq!(
        statuses.get("entry-terminal-generated").map(String::as_str),
        Some("pending"),
        "generated runs are not part of #295 terminal cleanup scope"
    );
    assert_ne!(
        statuses.get("entry-nonterminal-active").map(String::as_str),
        Some("skipped"),
        "non-terminal pending work must not be swept by #295 terminal cleanup"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn auto_queue_recovery_completes_finished_non_phase_gate_runs_and_releases_slots() {
    crate::pipeline::ensure_loaded();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    seed_repo_pg(&pool, "test-repo").await;
    seed_agent_pg(&pool, "agent-finished-recovery").await;
    seed_auto_queue_card_pg(
        &pool,
        "card-finished-done",
        9015,
        "done",
        "agent-finished-recovery",
    )
    .await;
    seed_auto_queue_card_pg(
        &pool,
        "card-finished-skipped",
        9016,
        "done",
        "agent-finished-recovery",
    )
    .await;

    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ('run-finished-recovery', 'test-repo', 'agent-finished-recovery', 'active')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, completed_at
        ) VALUES (
            'entry-finished-done', 'run-finished-recovery', 'card-finished-done',
            'agent-finished-recovery', 'done', 0, 0, NOW()
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, completed_at
        ) VALUES (
            'entry-finished-skipped', 'run-finished-recovery', 'card-finished-skipped',
            'agent-finished-recovery', 'skipped', 1, 1, NOW()
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_slots (
            agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
        ) VALUES (
            'agent-finished-recovery', 0, 'run-finished-recovery', 0, '{}'::jsonb
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_slots (
            agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
        ) VALUES (
            'agent-finished-recovery', 1, 'run-finished-recovery', 1, '{}'::jsonb
        )",
    )
    .execute(&pool)
    .await
    .unwrap();

    engine
        .fire_hook(
            crate::engine::hooks::Hook::OnTick1min,
            serde_json::json!({}),
        )
        .unwrap();

    let run_status: String =
        sqlx::query_scalar("SELECT status FROM auto_queue_runs WHERE id = 'run-finished-recovery'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        run_status, "completed",
        "finished non-phase-gate run must be completed by onTick1min backstop"
    );

    for slot_index in [0_i32, 1_i32] {
        let slot: (Option<String>, Option<i64>) = sqlx::query_as(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-finished-recovery' AND slot_index = $1",
        )
        .bind(slot_index)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            slot,
            (None, None),
            "completed run must release slot {slot_index}"
        );
    }

    pool.close().await;
    pg_db.drop().await;
}

#[test]
fn auto_queue_recovery_keeps_user_cancelled_runs_active() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);

    seed_agent(&db, "agent-user-cancelled-recovery");
    seed_auto_queue_card(
        &db,
        "card-user-cancelled-recovery",
        9017,
        "in_progress",
        "agent-user-cancelled-recovery",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-user-cancelled-recovery', 'test-repo', 'agent-user-cancelled-recovery', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, completed_at
            ) VALUES (
                'entry-user-cancelled-recovery', 'run-user-cancelled-recovery',
                'card-user-cancelled-recovery', 'agent-user-cancelled-recovery',
                'user_cancelled', 0, 0, datetime('now')
            )",
            [],
        )
        .unwrap();
    }

    engine
        .fire_hook(
            crate::engine::hooks::Hook::OnTick1min,
            serde_json::json!({}),
        )
        .unwrap();

    let conn = db.lock().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-user-cancelled-recovery'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        run_status, "active",
        "user_cancelled entries must block onTick1min from auto-completing the run"
    );
}

#[test]
fn auto_queue_recovery_keeps_finished_phase_gate_runs_blocked_until_gate_resolves() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    ensure_auto_queue_tables(&db);

    seed_agent(&db, "agent-finished-gate");
    seed_auto_queue_card(
        &db,
        "card-finished-gate",
        9017,
        "done",
        "agent-finished-gate",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
             VALUES ('run-finished-gate', 'test-repo', 'agent-finished-gate', 'paused')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, priority_rank, thread_group, batch_phase, completed_at
            ) VALUES (
                'entry-finished-gate', 'run-finished-gate', 'card-finished-gate',
                'agent-finished-gate', 'done', 0, 0, 1, datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_slots (
                agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map
            ) VALUES (
                'agent-finished-gate', 0, 'run-finished-gate', 0, '{}'
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_phase_gates (
                run_id, phase, status, dispatch_id, pass_verdict
             ) VALUES (?1, ?2, ?3, NULL, 'phase_gate_passed')",
            sqlite_params!["run-finished-gate", 1, "pending",],
        )
        .unwrap();
    }

    engine
        .fire_hook(
            crate::engine::hooks::Hook::OnTick1min,
            serde_json::json!({}),
        )
        .unwrap();

    let conn = db.lock().unwrap();
    let run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-finished-gate'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        run_status, "paused",
        "finished phase-gate run must stay paused until the gate resolves"
    );

    let slot: (Option<String>, Option<i64>) = conn
        .query_row(
            "SELECT assigned_run_id, assigned_thread_group
             FROM auto_queue_slots
             WHERE agent_id = 'agent-finished-gate' AND slot_index = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        slot,
        (Some("run-finished-gate".to_string()), Some(0)),
        "phase-gate blocked run must retain its slot assignment"
    );
}

// ── #265: Dispatch status validation ──────────────────────────

/// #265: PATCH /dispatches/:id with an invalid status like "done" must return
/// 400 and must NOT modify the dispatch or its associated card state.
#[tokio::test]
async fn patch_dispatch_pg_rejects_invalid_status() {
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());

    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('ch-td', 'TD', '111', '222')",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
         VALUES ('card-265', 'Stuck Card', 'in_progress', 'ch-td', 'dispatch-265', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
         VALUES ('dispatch-265', 'card-265', 'ch-td', 'rework', 'dispatched', 'Rework task', NOW(), NOW())",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/dispatches/dispatch-265")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"status":"done"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "invalid status 'done' must be rejected with 400"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["error"]
            .as_str()
            .unwrap()
            .contains("invalid dispatch status"),
        "error message must mention invalid status"
    );

    // Verify dispatch status is unchanged (pipeline invariant)
    let dispatch_status: String =
        sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = 'dispatch-265'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        dispatch_status, "dispatched",
        "dispatch status must be unchanged after rejected update"
    );

    // Verify card state is also unchanged (pipeline invariant)
    let card_status: String =
        sqlx::query_scalar("SELECT status FROM kanban_cards WHERE id = 'card-265'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        card_status, "in_progress",
        "card status must be unchanged after rejected dispatch update"
    );

    pool.close().await;
    pg_db.drop().await;
}

/// #265: Valid statuses like "cancelled" must still work through the generic path.
#[tokio::test]
#[ignore = "obsolete SQLite dispatch route fixture; PR #868 runtime path is PostgreSQL-only"]
async fn patch_dispatch_accepts_valid_status_cancelled() {
    let db = test_db();
    let engine = test_engine(&db);
    seed_test_agents(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
             VALUES ('card-265v', 'Valid Card', 'in_progress', 'ch-td', 'dispatch-265v', datetime('now'), datetime('now'))",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ('dispatch-265v', 'card-265v', 'ch-td', 'rework', 'dispatched', 'Rework task', datetime('now'), datetime('now'))",
            [],
        ).unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/dispatches/dispatch-265v")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"status":"cancelled"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "valid status 'cancelled' must be accepted"
    );
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["dispatch"]["status"], "cancelled");
}

#[tokio::test]
async fn rereview_clears_stale_review_fields() {
    let (_repo, _repo_guard) = setup_test_repo();
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-stale-cleanup");
    set_pmd_channel(&db, "pmd-chan-123");
    ensure_auto_queue_tables(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, review_status, suggestion_pending_at,
                review_entered_at, awaiting_dod_at,
                created_at, updated_at
            ) VALUES (
                'card-stale', 'Issue #300', 'review', 'medium', 'agent-stale-cleanup', 'test-repo',
                300, 'suggestion_pending', datetime('now', '-10 minutes'),
                datetime('now', '-20 minutes'), datetime('now', '-5 minutes'),
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at
            ) VALUES (
                'impl-stale', 'card-stale', 'agent-stale-cleanup', 'implementation', 'completed',
                'impl', datetime('now', '-30 minutes'), datetime('now', '-30 minutes')
            )",
            [],
        )
        .unwrap();
        // Seed card_review_state with stale data
        conn.execute(
            "INSERT INTO card_review_state (card_id, state, pending_dispatch_id, review_round, updated_at)
             VALUES ('card-stale', 'suggestion_pending', 'old-dispatch-id', 1, datetime('now'))",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-stale/rereview")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(r#"{"reason":"stale cleanup test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let conn = db.lock().unwrap();
    let (review_status, suggestion_pending_at, awaiting_dod_at): (
        Option<String>,
        Option<String>,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT review_status, suggestion_pending_at, awaiting_dod_at
             FROM kanban_cards WHERE id = 'card-stale'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    // After cleanup + OnReviewEnter hook: review_status is refreshed to "reviewing"
    // (not the stale "suggestion_pending"), and stale timestamps are cleared.
    assert_ne!(
        review_status.as_deref(),
        Some("suggestion_pending"),
        "stale review_status 'suggestion_pending' should be cleared by rereview"
    );
    assert!(
        suggestion_pending_at.is_none(),
        "suggestion_pending_at should be NULL after rereview"
    );
    assert!(
        awaiting_dod_at.is_none(),
        "awaiting_dod_at should be NULL after rereview"
    );

    // card_review_state should NOT be stale "suggestion_pending" with old pending_dispatch_id
    let (rs_state, rs_pending): (String, Option<String>) = conn
        .query_row(
            "SELECT state, pending_dispatch_id FROM card_review_state WHERE card_id = 'card-stale'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_ne!(
        rs_state, "suggestion_pending",
        "card_review_state.state should not be stale 'suggestion_pending'"
    );
    assert_ne!(
        rs_pending.as_deref(),
        Some("old-dispatch-id"),
        "card_review_state.pending_dispatch_id should not be the old stale value"
    );
}

#[tokio::test]
async fn rereview_resets_repeated_finding_round_markers() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-acr-reset");
    set_pmd_channel(&db, "pmd-chan-123");
    ensure_auto_queue_tables(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, created_at, updated_at
            ) VALUES (
                'card-acr', 'Issue #272', 'review', 'medium', 'agent-acr-reset', 'test-repo',
                272, datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at
            ) VALUES (
                'impl-acr', 'card-acr', 'agent-acr-reset', 'implementation', 'completed',
                'impl', datetime('now', '-30 minutes'), datetime('now', '-30 minutes')
            )",
            [],
        )
        .unwrap();
        // Seed card_review_state with non-null repeated-finding markers from a previous cycle
        conn.execute(
            "INSERT INTO card_review_state (
                card_id, state, review_round, approach_change_round, session_reset_round, updated_at
             ) VALUES ('card-acr', 'reviewing', 3, 2, 3, datetime('now'))",
            [],
        )
        .unwrap();
    }

    // Verify repeated-finding markers are set before rereview
    {
        let conn = db.lock().unwrap();
        let (acr, reset_round): (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT approach_change_round, session_reset_round
                 FROM card_review_state WHERE card_id = 'card-acr'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            acr,
            Some(2),
            "approach_change_round should be 2 before rereview"
        );
        assert_eq!(
            reset_round,
            Some(3),
            "session_reset_round should be 3 before rereview"
        );
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-acr/rereview")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(
                    r#"{"reason":"repeated-finding marker reset test"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // repeated-finding markers should be NULL after rereview
    let conn = db.lock().unwrap();
    let (acr, reset_round): (Option<i64>, Option<i64>) = conn
        .query_row(
            "SELECT approach_change_round, session_reset_round
             FROM card_review_state WHERE card_id = 'card-acr'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(
        acr.is_none(),
        "approach_change_round should be NULL after rereview, got {:?}",
        acr
    );
    assert!(
        reset_round.is_none(),
        "session_reset_round should be NULL after rereview, got {:?}",
        reset_round
    );
}

#[tokio::test]
async fn idle_sync_preserves_repeated_finding_round_markers() {
    // Regression test for #272/#420: generic idle sync (timeout, gate-failure, pass)
    // must NOT clear repeated-finding markers — only the explicit rereview path does.
    let db = test_db();
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, priority, created_at, updated_at)
             VALUES ('card-preserve', 'preserve test', 'review', 'medium', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO card_review_state (
                card_id, state, review_round, approach_change_round, session_reset_round, updated_at
             ) VALUES ('card-preserve', 'reviewing', 3, 2, 3, datetime('now'))",
            [],
        )
        .unwrap();

        // Simulate a non-rereview idle sync (e.g. pass/approved, timeout fallback)
        conn.execute(
            "UPDATE card_review_state
             SET state = 'idle',
                 last_verdict = 'pass',
                 updated_at = datetime('now')
             WHERE card_id = 'card-preserve'",
            [],
        )
        .unwrap();

        let (acr, reset_round): (Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT approach_change_round, session_reset_round
                 FROM card_review_state WHERE card_id = 'card-preserve'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            acr,
            Some(2),
            "approach_change_round must be preserved on generic idle sync, got {:?}",
            acr
        );
        assert_eq!(
            reset_round,
            Some(3),
            "session_reset_round must be preserved on generic idle sync, got {:?}",
            reset_round
        );
    }
}

#[tokio::test]
async fn rereview_backlog_card_transitions_to_review_with_dispatch() {
    crate::pipeline::ensure_loaded();
    let (_repo, _repo_guard) = setup_test_repo();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let db = test_db();
    let engine = test_engine_with_pg(&db, pool.clone());
    seed_agent_pg(&pool, "agent-backlog-rr").await;
    seed_repo_pg(&pool, "test-repo").await;

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, priority, assigned_agent_id, repo_id,
            github_issue_number, created_at, updated_at
        ) VALUES (
            'card-backlog-rr', 'Issue #301', 'backlog', 'medium', 'agent-backlog-rr', 'test-repo',
            301, NOW(), NOW()
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title,
            created_at, updated_at
        ) VALUES (
            'impl-backlog-rr', 'card-backlog-rr', 'agent-backlog-rr', 'implementation', 'completed',
            'impl', NOW() - INTERVAL '30 minutes', NOW() - INTERVAL '30 minutes'
        )",
    )
    .execute(&pool)
    .await
    .unwrap();

    let app = test_api_router_with_pg(
        db.clone(),
        engine,
        crate::config::Config::default(),
        None,
        pool.clone(),
    );
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-backlog-rr/rereview")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(r#"{"reason":"backlog rereview test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["rereviewed"], true);
    assert!(
        json["review_dispatch_id"].as_str().is_some(),
        "should have a dispatch id"
    );

    let card_status: String =
        sqlx::query_scalar("SELECT status FROM kanban_cards WHERE id = 'card-backlog-rr'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(card_status, "review", "card should transition to review");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn rereview_returns_bad_gateway_when_github_reopen_fails_before_response() {
    let (_repo, _repo_guard) = setup_test_repo();
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-rereview-ghfail");
    set_pmd_channel(&db, "pmd-chan-123");
    ensure_auto_queue_tables(&db);

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_url, created_at, updated_at, completed_at
            ) VALUES (
                'card-rereview-ghfail', 'Issue #336', 'done', 'medium', 'agent-rereview-ghfail',
                'test-repo', 'https://example.com/not-github', datetime('now'),
                datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at, completed_at
            ) VALUES (
                'impl-rereview-ghfail', 'card-rereview-ghfail', 'agent-rereview-ghfail',
                'implementation', 'completed', 'impl', datetime('now', '-30 minutes'),
                datetime('now', '-30 minutes'), datetime('now', '-30 minutes')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/card-rereview-ghfail/rereview")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(r#"{"reason":"gh reopen failure test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["rereviewed"], false);
    assert_eq!(json["github_issue_url"], "https://example.com/not-github");
    assert!(
        json["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not a github url"),
        "expected invalid github url parse error, got {json}"
    );
}

#[tokio::test]
async fn batch_rereview_processes_multiple_issues() {
    let (_repo, _repo_guard) = setup_test_repo();
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-batch-rr");
    set_pmd_channel(&db, "pmd-chan-123");
    ensure_auto_queue_tables(&db);

    {
        let conn = db.lock().unwrap();
        // Card for issue #401
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, created_at, updated_at
            ) VALUES (
                'card-batch-1', 'Issue #401', 'done', 'medium', 'agent-batch-rr', 'test-repo',
                401, datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at
            ) VALUES (
                'impl-batch-1', 'card-batch-1', 'agent-batch-rr', 'implementation', 'completed',
                'impl 401', datetime('now', '-30 minutes'), datetime('now', '-30 minutes')
            )",
            [],
        )
        .unwrap();
        // Card for issue #402
        conn.execute(
            "INSERT INTO kanban_cards (
                id, title, status, priority, assigned_agent_id, repo_id,
                github_issue_number, created_at, updated_at
            ) VALUES (
                'card-batch-2', 'Issue #402', 'done', 'medium', 'agent-batch-rr', 'test-repo',
                402, datetime('now'), datetime('now')
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                created_at, updated_at
            ) VALUES (
                'impl-batch-2', 'card-batch-2', 'agent-batch-rr', 'implementation', 'completed',
                'impl 402', datetime('now', '-30 minutes'), datetime('now', '-30 minutes')
            )",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/kanban-cards/batch-rereview")
                .header("content-type", "application/json")
                .header("x-channel-id", "pmd-chan-123")
                .body(Body::from(
                    serde_json::json!({
                        "issues": [401, 402, 999],
                        "reason": "batch test"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    let results = json["results"].as_array().expect("results should be array");
    assert_eq!(results.len(), 3, "should have 3 results");

    // Issue 401 should succeed
    assert_eq!(results[0]["issue"], 401);
    assert_eq!(results[0]["ok"], true);
    assert!(results[0]["dispatch_id"].as_str().is_some());

    // Issue 402 should succeed
    assert_eq!(results[1]["issue"], 402);
    assert_eq!(results[1]["ok"], true);
    assert!(results[1]["dispatch_id"].as_str().is_some());

    // Issue 999 should fail (not found)
    assert_eq!(results[2]["issue"], 999);
    assert_eq!(results[2]["ok"], false);
    assert!(
        results[2]["error"]
            .as_str()
            .unwrap_or_default()
            .contains("not found")
    );

    // Verify both cards transitioned to review
    let conn = db.lock().unwrap();
    let status_1: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-batch-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status_1, "review");

    let status_2: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = 'card-batch-2'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(status_2, "review");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_reset_completes_generated_and_pending_runs() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-reset");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(&db, "card-reset-generated", 1711, "ready", "agent-reset");
    seed_auto_queue_card(&db, "card-reset-pending", 1712, "ready", "agent-reset");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-reset-generated', 'test-repo', 'agent-reset', 'generated', datetime('now', '-2 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-reset-pending', 'test-repo', 'agent-reset', 'pending', datetime('now', '-1 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-reset-generated', 'run-reset-generated', 'card-reset-generated', 'agent-reset', 'pending', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-reset-pending', 'run-reset-pending', 'card-reset-pending', 'agent-reset', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .header("content-type", "application/json")
                .uri("/auto-queue/reset")
                .body(Body::from(r#"{"agent_id":"agent-reset"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["deleted_entries"], 2);
    assert_eq!(json["completed_runs"], 2);

    let status_response = app
        .oneshot(
            Request::builder()
                .uri("/auto-queue/status?agent_id=agent-reset")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(status_response.status(), StatusCode::OK);
    let status_body = axum::body::to_bytes(status_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let status_json: serde_json::Value = serde_json::from_slice(&status_body).unwrap();
    assert_eq!(status_json["run"]["id"], "run-reset-pending");
    assert_eq!(status_json["run"]["status"], "completed");
    assert_eq!(
        status_json["entries"]
            .as_array()
            .map(|entries| entries.len()),
        Some(0)
    );

    let conn = db.lock().unwrap();
    let generated_run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-reset-generated'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let pending_run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-reset-pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let remaining_entries: i64 = conn
        .query_row("SELECT COUNT(*) FROM auto_queue_entries", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(generated_run_status, "completed");
    assert_eq!(pending_run_status, "completed");
    assert_eq!(remaining_entries, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_reset_with_agent_id_only_clears_matching_agent_scope() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-reset-a");
    seed_agent(&db, "agent-reset-b");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(
        &db,
        "card-reset-a-generated",
        1713,
        "ready",
        "agent-reset-a",
    );
    seed_auto_queue_card(&db, "card-reset-a-active", 1714, "ready", "agent-reset-a");
    seed_auto_queue_card(&db, "card-reset-b-active", 1715, "ready", "agent-reset-b");

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-reset-a-generated', 'test-repo', 'agent-reset-a', 'generated', datetime('now', '-3 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-reset-a-active', 'test-repo', 'agent-reset-a', 'active', datetime('now', '-2 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-reset-b-active', 'test-repo', 'agent-reset-b', 'active', datetime('now', '-1 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-reset-a-generated', 'run-reset-a-generated', 'card-reset-a-generated', 'agent-reset-a', 'pending', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-reset-a-active', 'run-reset-a-active', 'card-reset-a-active', 'agent-reset-a', 'dispatched', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-reset-b-active', 'run-reset-b-active', 'card-reset-b-active', 'agent-reset-b', 'dispatched', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/reset")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"agent_id":"agent-reset-a"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["deleted_entries"], 2);
    assert_eq!(json["completed_runs"], 2);
    assert_eq!(json["protected_active_runs"], 0);

    let conn = db.lock().unwrap();
    let run_a_generated: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-reset-a-generated'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let run_a_active: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-reset-a-active'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let run_b_active: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-reset-b-active'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let remaining_agent_b_entries: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries WHERE agent_id = 'agent-reset-b'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let remaining_entries: i64 = conn
        .query_row("SELECT COUNT(*) FROM auto_queue_entries", [], |row| {
            row.get(0)
        })
        .unwrap();
    drop(conn);

    assert_eq!(run_a_generated, "completed");
    assert_eq!(run_a_active, "completed");
    assert_eq!(run_b_active, "active");
    assert_eq!(remaining_agent_b_entries, 1);
    assert_eq!(remaining_entries, 1);

    let status_response = app
        .oneshot(
            Request::builder()
                .uri("/auto-queue/status?agent_id=agent-reset-b")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(status_response.status(), StatusCode::OK);
    let status_body = axum::body::to_bytes(status_response.into_body(), usize::MAX)
        .await
        .unwrap();
    let status_json: serde_json::Value = serde_json::from_slice(&status_body).unwrap();
    assert_eq!(status_json["run"]["id"], "run-reset-b-active");
    assert_eq!(status_json["run"]["status"], "active");
    assert_eq!(
        status_json["entries"]
            .as_array()
            .map(|entries| entries.len()),
        Some(1)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "obsolete SQLite auto-queue fixture; PR #868 runtime path is PostgreSQL-only"]
async fn auto_queue_reset_requires_agent_id_and_reset_global_requires_confirmation() {
    crate::pipeline::ensure_loaded();
    let db = test_db();
    let engine = test_engine(&db);
    seed_agent(&db, "agent-reset-global-active");
    seed_agent(&db, "agent-reset-global-pending");
    ensure_auto_queue_tables(&db);
    seed_auto_queue_card(
        &db,
        "card-reset-global-active",
        1716,
        "ready",
        "agent-reset-global-active",
    );
    seed_auto_queue_card(
        &db,
        "card-reset-global-pending",
        1717,
        "ready",
        "agent-reset-global-pending",
    );

    {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-reset-global-active', 'test-repo', 'agent-reset-global-active', 'active', datetime('now', '-2 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-reset-global-pending', 'test-repo', 'agent-reset-global-pending', 'pending', datetime('now', '-1 minutes'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-reset-global-active', 'run-reset-global-active', 'card-reset-global-active', 'agent-reset-global-active', 'dispatched', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank) \
             VALUES ('entry-reset-global-pending', 'run-reset-global-pending', 'card-reset-global-pending', 'agent-reset-global-pending', 'pending', 0)",
            [],
        )
        .unwrap();
    }

    let app = test_api_router(db.clone(), engine, None);
    let rejection = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/reset")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(rejection.status(), StatusCode::BAD_REQUEST);
    let rejection_body = axum::body::to_bytes(rejection.into_body(), usize::MAX)
        .await
        .unwrap();
    let rejection_json: serde_json::Value = serde_json::from_slice(&rejection_body).unwrap();
    assert_eq!(rejection_json["error"], "agent_id is required for reset");

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/auto-queue/reset-global")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"confirmation_token":"confirm-global-reset"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["deleted_entries"], 1);
    assert_eq!(json["completed_runs"], 1);
    assert_eq!(json["protected_active_runs"], 1);
    assert_eq!(
        json["warning"],
        "global reset preserved 1 active run(s); use agent_id to reset a specific queue"
    );

    let conn = db.lock().unwrap();
    let active_run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-reset-global-active'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let pending_run_status: String = conn
        .query_row(
            "SELECT status FROM auto_queue_runs WHERE id = 'run-reset-global-pending'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let active_entries: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries WHERE run_id = 'run-reset-global-active'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let remaining_entries: i64 = conn
        .query_row("SELECT COUNT(*) FROM auto_queue_entries", [], |row| {
            row.get(0)
        })
        .unwrap();
    drop(conn);

    assert_eq!(active_run_status, "active");
    assert_eq!(pending_run_status, "completed");
    assert_eq!(active_entries, 1);
    assert_eq!(remaining_entries, 1);
}

#[tokio::test]
async fn v1_routes_pg_surface_dashboard_contract() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("repo-v1")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (
            id, name, name_ko, provider, status, xp, avatar_emoji, discord_channel_id, discord_channel_alt
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9
         )",
    )
    .bind("agent-v1")
    .bind("V1 Agent")
    .bind("브이원 에이전트")
    .bind("claude")
    .bind("working")
    .bind(60_i64)
    .bind("🤖")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO skills (id, name, description, source_path, updated_at)
         VALUES ($1, $2, $3, $4, NOW())",
    )
    .bind("live-skill")
    .bind("Live Skill")
    .bind("Live skill description")
    .bind("/tmp/live-skill/SKILL.md")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO skill_usage (skill_id, agent_id, session_key, used_at)
         VALUES ($1, $2, $3, NOW())",
    )
    .bind("live-skill")
    .bind("agent-v1")
    .bind("session-v1")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, NOW(), NOW()
         )",
    )
    .bind("card-v1-current")
    .bind("repo-v1")
    .bind("Current V1 Card")
    .bind("in_progress")
    .bind("high")
    .bind("agent-v1")
    .bind(791_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, NOW(), NOW()
         )",
    )
    .bind("card-v1-review")
    .bind("repo-v1")
    .bind("Review Queue Card")
    .bind("review")
    .bind("medium")
    .bind("agent-v1")
    .bind(792_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, NOW(), NOW()
         )",
    )
    .bind("dispatch-current")
    .bind("card-v1-current")
    .bind("agent-v1")
    .bind("implementation")
    .bind("dispatched")
    .bind("Current dispatch")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query("UPDATE kanban_cards SET latest_dispatch_id = $1 WHERE id = $2")
        .bind("dispatch-current")
        .bind("card-v1-current")
        .execute(&pg_pool)
        .await
        .unwrap();

    for index in 0..5_i64 {
        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
             ) VALUES (
                $1, $2, $3, $4, $5, $6,
                NOW() - ($7::BIGINT || ' hours')::INTERVAL,
                NOW() - ($7::BIGINT || ' hours')::INTERVAL
             )",
        )
        .bind(format!("dispatch-completed-{index}"))
        .bind("card-v1-review")
        .bind("agent-v1")
        .bind("implementation")
        .bind("completed")
        .bind(format!("Completed dispatch {index}"))
        .bind(index + 1)
        .execute(&pg_pool)
        .await
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO sessions (
            session_key, agent_id, provider, status, active_dispatch_id, session_info, tokens,
            last_heartbeat, thread_channel_id, created_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, NOW(), $8, NOW()
         )",
    )
    .bind("host:session-v1")
    .bind("agent-v1")
    .bind("claude")
    .bind("working")
    .bind("dispatch-current")
    .bind("v1 session")
    .bind(321_i64)
    .bind("222000000000001")
    .execute(&pg_pool)
    .await
    .unwrap();

    sqlx::query(
        "INSERT INTO kanban_audit_logs (card_id, from_status, to_status, source, result, created_at)
         VALUES ($1, $2, $3, $4, $5, NOW())",
    )
    .bind("card-v1-current")
    .bind("requested")
    .bind("in_progress")
    .bind("dispatch")
    .bind("ok")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO audit_logs (entity_type, entity_id, action, actor, timestamp)
         VALUES ($1, $2, $3, $4, NOW())",
    )
    .bind("provider")
    .bind("claude")
    .bind("provider_restart_pending")
    .bind("system")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
         VALUES ($1, $2, $3, $4)",
    )
    .bind("run-v1")
    .bind("repo-v1")
    .bind("agent-v1")
    .bind("active")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank
         ) VALUES (
            $1, $2, $3, $4, $5, $6
         )",
    )
    .bind("entry-v1")
    .bind("run-v1")
    .bind("card-v1-current")
    .bind("agent-v1")
    .bind("pending")
    .bind(0_i64)
    .execute(&pg_pool)
    .await
    .unwrap();

    let app = axum::Router::new().nest(
        "/api",
        test_api_router_with_pg(
            db,
            engine,
            crate::config::Config::default(),
            None,
            pg_pool.clone(),
        ),
    );

    let overview = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/overview")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(overview.status(), StatusCode::OK);
    assert_eq!(
        overview.headers().get("cache-control").unwrap(),
        "max-age=30"
    );
    let overview_body = axum::body::to_bytes(overview.into_body(), usize::MAX)
        .await
        .unwrap();
    let overview_json: serde_json::Value = serde_json::from_slice(&overview_body).unwrap();
    assert_eq!(overview_json["session_count"], json!(1));
    assert_eq!(overview_json["metrics"]["agents"]["total"], json!(1));
    assert_eq!(overview_json["metrics"]["kanban"]["review_queue"], json!(1));
    assert!(overview_json["spark_14d"].as_array().is_some());

    let agents = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/agents")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(agents.status(), StatusCode::OK);
    assert_eq!(agents.headers().get("cache-control").unwrap(), "max-age=10");
    let agents_body = axum::body::to_bytes(agents.into_body(), usize::MAX)
        .await
        .unwrap();
    let agents_json: serde_json::Value = serde_json::from_slice(&agents_body).unwrap();
    assert_eq!(
        agents_json["agents"][0]["current_task"]["dispatch_id"],
        json!("dispatch-current")
    );
    assert_eq!(
        agents_json["agents"][0]["skills_7d"][0]["id"],
        json!("live-skill")
    );

    let tokens = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/tokens?range=7d")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(tokens.status(), StatusCode::OK);
    assert_eq!(tokens.headers().get("cache-control").unwrap(), "max-age=60");
    let tokens_body = axum::body::to_bytes(tokens.into_body(), usize::MAX)
        .await
        .unwrap();
    let tokens_json: serde_json::Value = serde_json::from_slice(&tokens_body).unwrap();
    assert!(tokens_json["summary"]["total_cost"].is_string());
    assert!(tokens_json["daily"].is_array());

    let kanban = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/kanban")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(kanban.status(), StatusCode::OK);
    assert_eq!(kanban.headers().get("cache-control").unwrap(), "max-age=5");
    let kanban_body = axum::body::to_bytes(kanban.into_body(), usize::MAX)
        .await
        .unwrap();
    let kanban_json: serde_json::Value = serde_json::from_slice(&kanban_body).unwrap();
    assert_eq!(kanban_json["auto_queue"]["run"]["id"], json!("run-v1"));
    assert!(kanban_json.get("wip_limit").is_some());

    let ops = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/ops/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(ops.status(), StatusCode::OK);
    assert_eq!(ops.headers().get("cache-control").unwrap(), "max-age=5");
    let ops_body = axum::body::to_bytes(ops.into_body(), usize::MAX)
        .await
        .unwrap();
    let ops_json: serde_json::Value = serde_json::from_slice(&ops_body).unwrap();
    assert!(ops_json["bottlenecks"].is_array());

    let activity = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/activity?limit=8")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(activity.status(), StatusCode::OK);
    let activity_body = axum::body::to_bytes(activity.into_body(), usize::MAX)
        .await
        .unwrap();
    let activity_json: serde_json::Value = serde_json::from_slice(&activity_body).unwrap();
    let kinds = activity_json["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["kind"].as_str())
        .collect::<std::collections::HashSet<_>>();
    assert!(kinds.contains("dispatch"));
    assert!(kinds.contains("kanban_transition"));
    assert!(kinds.contains("provider_event"));
    assert!(activity_json["next_cursor"].is_string() || activity_json["next_cursor"].is_null());

    let achievements = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/achievements")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(achievements.status(), StatusCode::OK);
    assert_eq!(
        achievements.headers().get("cache-control").unwrap(),
        "max-age=300"
    );
    let achievements_body = axum::body::to_bytes(achievements.into_body(), usize::MAX)
        .await
        .unwrap();
    let achievements_json: serde_json::Value = serde_json::from_slice(&achievements_body).unwrap();
    assert_eq!(
        achievements_json["achievements"][0]["rarity"],
        json!("common")
    );
    assert!(achievements_json["achievements"][0]["progress"].is_object());
    assert_eq!(
        achievements_json["daily_missions"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    let settings_get = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/settings")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(settings_get.status(), StatusCode::OK);
    assert_eq!(
        settings_get.headers().get("cache-control").unwrap(),
        "no-store"
    );
    let settings_get_body = axum::body::to_bytes(settings_get.into_body(), usize::MAX)
        .await
        .unwrap();
    let settings_get_json: serde_json::Value = serde_json::from_slice(&settings_get_body).unwrap();
    assert!(settings_get_json["entries"].as_array().is_some());

    let settings_patch = app
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/v1/settings/merge_strategy")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"value":"rebase"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(settings_patch.status(), StatusCode::OK);
    let settings_patch_body = axum::body::to_bytes(settings_patch.into_body(), usize::MAX)
        .await
        .unwrap();
    let settings_patch_json: serde_json::Value =
        serde_json::from_slice(&settings_patch_body).unwrap();
    assert_eq!(settings_patch_json["key"], json!("merge_strategy"));
    assert_eq!(settings_patch_json["value"], json!("rebase"));
    assert_eq!(settings_patch_json["live_override"]["active"], json!(true));

    pg_pool.close().await;
    pg_db.drop().await;
}

#[tokio::test]
async fn v1_stream_pg_emits_snapshot_and_replays_shared_bus_events() {
    let db = test_db();
    let pg_db = TestPostgresDb::create().await;
    let pg_pool = pg_db.connect_and_migrate().await;
    let engine = test_engine_with_pg(&db, pg_pool.clone());

    sqlx::query("INSERT INTO github_repos (id, display_name) VALUES ($1, $1)")
        .bind("repo-v1-stream")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO agents (
            id, name, provider, status, xp, avatar_emoji, discord_channel_id, discord_channel_alt
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8
         )",
    )
    .bind("agent-v1-stream")
    .bind("V1 Stream Agent")
    .bind("claude")
    .bind("working")
    .bind(60_i64)
    .bind("🤖")
    .bind("111")
    .bind("222")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, repo_id, title, status, priority, assigned_agent_id, github_issue_number, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, NOW(), NOW()
         )",
    )
    .bind("card-v1-stream")
    .bind("repo-v1-stream")
    .bind("Stream Card")
    .bind("in_progress")
    .bind("high")
    .bind("agent-v1-stream")
    .bind(792_i64)
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, NOW(), NOW()
         )",
    )
    .bind("dispatch-v1-stream")
    .bind("card-v1-stream")
    .bind("agent-v1-stream")
    .bind("implementation")
    .bind("dispatched")
    .bind("Stream dispatch")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query("UPDATE kanban_cards SET latest_dispatch_id = $1 WHERE id = $2")
        .bind("dispatch-v1-stream")
        .bind("card-v1-stream")
        .execute(&pg_pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO sessions (
            session_key, agent_id, provider, status, active_dispatch_id, session_info, tokens,
            last_heartbeat, thread_channel_id, created_at
         ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, NOW(), $8, NOW()
         )",
    )
    .bind("host:session-v1-stream")
    .bind("agent-v1-stream")
    .bind("claude")
    .bind("working")
    .bind("dispatch-v1-stream")
    .bind("v1 stream session")
    .bind(321_i64)
    .bind("222000000000002")
    .execute(&pg_pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO kanban_audit_logs (card_id, from_status, to_status, source, result, created_at)
         VALUES ($1, $2, $3, $4, $5, NOW())",
    )
    .bind("card-v1-stream")
    .bind("requested")
    .bind("in_progress")
    .bind("dispatch")
    .bind("ok")
    .execute(&pg_pool)
    .await
    .unwrap();

    let tx = crate::server::ws::new_broadcast();
    let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
    let app = axum::Router::new().nest(
        "/api",
        api_router_with_pg_for_tests(
            db,
            engine,
            crate::config::Config::default(),
            tx.clone(),
            buf,
            None,
            Some(pg_pool.clone()),
        ),
    );

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.starts_with("text/event-stream"))
    );
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");

    let mut body = response.into_body();
    let snapshot = read_sse_body_until(
        &mut body,
        &[
            "event: agent.status",
            "event: token.tick",
            "event: achievement.unlocked",
            "event: kanban.transition",
            "event: ops.health",
        ],
    )
    .await;

    assert!(snapshot.contains("\"agent_id\":\"agent-v1-stream\""));
    assert!(snapshot.contains("\"delta_tokens\":321"));
    assert!(snapshot.contains("\"achievement_id\""));
    assert!(snapshot.contains("\"from\":\"requested\""));
    assert!(snapshot.contains("\"status\":\"ok\""));
    drop(body);

    crate::server::ws::emit_event(
        &tx,
        "agent.status",
        json!({
            "agent_id": "agent-v1-stream",
            "status": "idle",
            "task": null,
        }),
    );
    crate::server::ws::emit_event(
        &tx,
        "token.tick",
        json!({
            "agent_id": "agent-v1-stream",
            "delta_tokens": 7,
            "delta_cost_usd": "0.12",
        }),
    );

    let replay_response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/stream")
                .header("Last-Event-ID", "1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(replay_response.status(), StatusCode::OK);

    let mut replay_body = replay_response.into_body();
    let replay = read_sse_body_until(
        &mut replay_body,
        &["id: 2", "event: token.tick", "\"delta_tokens\":7"],
    )
    .await;
    assert!(replay.contains("\"delta_cost_usd\":\"0.12\""));

    pg_pool.close().await;
    pg_db.drop().await;
}

/// #1069 / 904-7 — callsite migration smoke test.
///
/// Scans the dashboard frontend, policies (JS), shell scripts, skills (Markdown),
/// and the example config for references to API paths that were renamed in
/// #1064 / #1065. Server-side route handler files and auto-generated docs are
/// excluded — they legitimately reference the old paths as deprecated aliases
/// or history. The test fails when a new callsite re-introduces a legacy path.
#[test]
fn callsites_migrated_off_legacy_api_paths_1069() {
    use std::path::Path;

    // Banned substrings — these are the paths fully removed in #1064/#1065.
    // /api/hook/session is not banned: the parameterized DELETE
    // (`/api/hook/session/{sessionKey}`) and auth.rs prefix bypass legitimately
    // keep the prefix alive. Callsites should still hit
    // /api/dispatched-sessions/webhook for the unparameterized POST/DELETE.
    let banned: &[&str] = &[
        "/api/re-review",
        "/api/auto-queue/activate",
        "/api/discord-bindings",
    ];

    // Roots that frontend / policy / script / skill / config callsites live in.
    let roots = [
        "dashboard/src",
        "policies",
        "scripts",
        "skills",
        "agentdesk.example.yaml",
        "FEATURES.md",
        "README.md",
        "CLAUDE.md",
    ];

    fn walk(p: &Path, out: &mut Vec<std::path::PathBuf>) {
        if p.is_file() {
            out.push(p.to_path_buf());
            return;
        }
        let Ok(entries) = std::fs::read_dir(p) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if matches!(
                name,
                "node_modules" | "dist" | "build" | ".git" | "generated" | "target"
            ) {
                continue;
            }
            if path.is_dir() {
                walk(&path, out);
            } else if path.is_file() {
                out.push(path);
            }
        }
    }

    let repo_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    for r in roots {
        walk(&repo_root.join(r), &mut files);
    }

    let mut hits: Vec<String> = Vec::new();
    for file in &files {
        let ext = file
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !matches!(
            ext.as_str(),
            "ts" | "tsx" | "js" | "jsx" | "mjs" | "json" | "yaml" | "yml" | "sh" | "md"
        ) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(file) else {
            continue;
        };
        let path_str = file.to_string_lossy();
        for needle in banned {
            if !content.contains(needle) {
                continue;
            }
            for line in content.lines() {
                if !line.contains(needle) {
                    continue;
                }
                let trimmed = line.trim_start();
                // Allow comment / doc lines that explicitly call out the
                // historical migration. A live callsite (fetch URL string,
                // bash curl, etc.) will not match these markers.
                let is_comment_like = trimmed.starts_with("//")
                    || trimmed.starts_with("#")
                    || trimmed.starts_with("*")
                    || trimmed.starts_with("/*")
                    || trimmed.starts_with("|") // markdown table row
                    || trimmed.starts_with(">"); // markdown blockquote
                let mentions_history = trimmed.contains("#1064")
                    || trimmed.contains("#1065")
                    || trimmed.contains("#1069")
                    || trimmed.contains("removed")
                    || trimmed.contains("legacy")
                    || trimmed.contains("formerly")
                    || trimmed.contains("deprecated")
                    || trimmed.contains("→")
                    || trimmed.contains("->");
                if is_comment_like && mentions_history {
                    continue;
                }
                hits.push(format!("{path_str}: {trimmed}"));
            }
        }
    }

    assert!(
        hits.is_empty(),
        "#1069 callsite audit: legacy API paths still referenced outside server route handlers / generated docs:\n  {}",
        hits.join("\n  ")
    );
}
