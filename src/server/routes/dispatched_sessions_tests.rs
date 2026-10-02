use std::{future::Future, io::Write, os::unix::fs::PermissionsExt, sync::Arc, time::Duration};

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};

use super::{AppState, reconcile_stale_turn};
use crate::db::auto_queue::test_support::TestPostgresDb;

const WITNESS_DEADLINE: Duration = Duration::from_secs(10);

async fn witness_step<T>(label: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(WITNESS_DEADLINE, future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
}

fn install_missing_tmux_probe() -> (tempfile::TempDir, crate::config::TestEnvVarGuard) {
    install_tmux_probe("echo 'no server running on test socket' >&2\nexit 1")
}

fn install_tmux_probe(body: &str) -> (tempfile::TempDir, crate::config::TestEnvVarGuard) {
    let temp = tempfile::TempDir::new().expect("tmux probe dir");
    let binary = temp.path().join("tmux");
    let mut file = std::fs::File::create(&binary).expect("fake tmux");
    writeln!(file, "#!/bin/sh\n{body}").expect("fake tmux body");
    let mut permissions = std::fs::metadata(&binary)
        .expect("tmux metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&binary, permissions).expect("chmod tmux probe");
    let mut paths = vec![temp.path().to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(paths).expect("join tmux probe PATH");
    let guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "PATH",
        std::path::Path::new(&path),
    );
    (temp, guard)
}

fn test_state(pool: sqlx::PgPool) -> AppState {
    let config = crate::config::Config::default();
    let engine = crate::engine::PolicyEngine::new(&config).expect("construct policy engine");
    let broadcast_tx = crate::eventbus::new_broadcast();
    let batch_buffer = crate::eventbus::spawn_batch_flusher(broadcast_tx.clone());
    AppState {
        pg_pool: Some(pool),
        engine,
        config: Arc::new(config),
        broadcast_tx,
        batch_buffer,
        health_registry: None,
        cluster_instance_id: None,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn precondition_changed_handler_contract_is_conflict_and_retryable_pg() {
    // Lock order E -> P: take the environment lock before the PostgreSQL test
    // lifecycle lock. PATH is restored by the guard, which drops first.
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_tmux_probe, _path_guard) = install_missing_tmux_probe();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
    let session_key = format!(
        "{}:AgentDesk-claude-5464003",
        crate::services::platform::hostname_short()
    );
    sqlx::query(
        "INSERT INTO sessions (
             channel_id, session_key, provider, status, active_dispatch_id,
             last_heartbeat, session_info
         ) VALUES ($1, $2, 'claude', 'turn_active', NULL,
                   NOW() - INTERVAL '1 hour', 'original')",
    )
    .bind("route-contract-channel")
    .bind(&session_key)
    .execute(&pool)
    .await
    .unwrap();

    let mut lock = pool.begin().await.expect("begin row-lock transaction");
    let locked: String =
        sqlx::query_scalar("SELECT session_key FROM sessions WHERE session_key = $1 FOR UPDATE")
            .bind(&session_key)
            .fetch_one(&mut *lock)
            .await
            .expect("lock route-contract session");
    assert_eq!(locked, session_key);

    let state = test_state(pool.clone());
    let task_session_key = session_key.clone();
    let task = tokio::spawn(async move {
        reconcile_stale_turn(State(state), Path(task_session_key))
            .await
            .unwrap()
    });
    witness_step("handler apply lock wait", async {
        loop {
            let blocked = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (
                     SELECT 1
                       FROM pg_stat_activity
                      WHERE datname = current_database()
                        AND state = 'active'
                        AND wait_event_type = 'Lock'
                        AND query LIKE 'UPDATE sessions%reconciled stale%'
                 )",
            )
            .fetch_one(&pool)
            .await
            .expect("inspect handler apply lock wait");
            if blocked {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;

    sqlx::query("UPDATE sessions SET provider = 'codex' WHERE session_key = $1")
        .bind(&session_key)
        .execute(&mut *lock)
        .await
        .expect("move provider while handler apply is blocked");
    lock.commit().await.expect("release handler apply");

    let (status, Json(body)) = witness_step("reconcile handler completion", task)
        .await
        .expect("reconcile handler task");
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["reason"], "precondition_changed");
    assert_eq!(body["retry"], true);
    assert!(body["message"].as_str().unwrap().contains("retry"));
    assert_eq!(body["diagnostic_at"], "after_failed_update");

    pool.close().await;
    pg_db.drop().await;
}

/// Idle cleanup through the real kill-tmux route: a session whose transcript is
/// unresolved keeps its tmux and raises one deduplicated alert, while a provider
/// proven idle by its native transcript and pane is killed.
#[tokio::test(flavor = "current_thread")]
async fn idle_kill_route_preserves_unobservable_session_and_kills_proven_idle_pg() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (tmux_probe, _path_guard) = install_tmux_probe(concat!(
        "[ \"$1\" = -u ] && shift\n",
        "dir=$(dirname \"$0\")\n",
        "case \"$1\" in\n",
        "has-session) [ -e \"$dir/alive\" ] && exit 0; echo \"can't find session\" >&2; exit 1 ;;\n",
        "capture-pane) printf 'Ready for input (type message + Enter)\\n> \\n' ;;\n",
        "kill-session) echo \"$3\" >> \"$dir/killed\"; rm -f \"$dir/alive\" ;;\n",
        "esac",
    ));
    let runtime_root = tempfile::TempDir::new().expect("runtime root");
    let _root_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        runtime_root.path(),
    );
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
    let channel = format!("idle-pin-{}", uuid::Uuid::new_v4().simple());
    let tmux_name = format!("AgentDesk-claude-{channel}");
    let session_key = format!(
        "{}:{tmux_name}",
        crate::services::platform::hostname_short()
    );
    sqlx::query(
        "INSERT INTO sessions (session_key, provider, status, last_heartbeat)
         VALUES ($1, 'claude', 'idle', NOW() - INTERVAL '7 hours')",
    )
    .bind(&session_key)
    .execute(&pool)
    .await
    .unwrap();
    std::fs::write(tmux_probe.path().join("alive"), "").unwrap();
    let killed_log = tmux_probe.path().join("killed");
    let state = test_state(pool.clone());
    let kill = || {
        super::kill_tmux_session(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            Path(session_key.clone()),
            Json(crate::services::dispatched_sessions::KillTmuxOptions {
                reason: Some("idle 7시간 초과 — 자동 정리".to_string()),
                minimum_idle_minutes: Some(360),
            }),
        )
    };
    // #5993: the preservation is an `idle_cleanup_preserved` event plus a WARN
    // line, never an operator-channel message.
    let preserved = || crate::services::observability::idle_cleanup_preserved_count(&session_key);
    let outbox_rows = || async {
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*)::bigint FROM message_outbox")
            .fetch_one(&pool)
            .await
            .unwrap()
    };

    // Unobservable: live tmux, idle-looking pane, but no resolvable transcript.
    for _ in 0..2 {
        let (status, Json(body)) = kill().await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["tmux_killed"], false, "{body}");
        assert_eq!(body["skipped_provider_activity_guard"], true, "{body}");
        assert_eq!(body["preserved_reason"], "transcript_unresolved", "{body}");
        assert!(
            !killed_log.exists(),
            "unobservable session must not be killed"
        );
    }
    assert_eq!(preserved(), 2, "every skip is recorded");
    assert_eq!(outbox_rows().await, 0, "no operator-channel message");

    // Observable idle: the bound native transcript and the pane agree.
    let transcript = runtime_root.path().join("native.jsonl");
    std::fs::write(
        &transcript,
        "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
    )
    .unwrap();
    let old = std::time::SystemTime::now() - Duration::from_secs(24 * 60 * 60);
    filetime::set_file_mtime(&transcript, filetime::FileTime::from_system_time(old)).unwrap();
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        &tmux_name,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            output_path: transcript.to_string_lossy().into_owned(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let (status, Json(body)) = kill().await;
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&tmux_name);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tmux_killed"], true, "{body}");
    assert_eq!(
        std::fs::read_to_string(&killed_log).unwrap().trim(),
        format!("={tmux_name}:")
    );
    assert_eq!(preserved(), 2, "a proven-idle kill records no preservation");
    assert_eq!(outbox_rows().await, 0);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn kill_tmux_route_checks_the_host_before_any_probe_or_write_pg() {
    use crate::db::dispatched_sessions::hosted_execution::HostedState;
    use crate::db::dispatched_sessions::hosted_execution::tests::{
        TOKEN, future_schema, owner, record, wire,
    };
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (tmux_probe, _path_guard) = install_tmux_probe(concat!(
        "[ \"$1\" = -u ] && shift\n",
        "echo \"$*\" >> \"$(dirname \"$0\")/calls\"\n",
        "case \"$3\" in\n",
        "*probefail*) echo 'permission denied' >&2; exit 1 ;;\n",
        "esac\n",
        "echo \"can't find session: $3\" >&2; exit 1",
    ));
    let runtime_root = tempfile::TempDir::new().expect("runtime root");
    let _root_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        runtime_root.path(),
    );
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
    let state = test_state(pool.clone());
    let host = crate::services::platform::hostname_short();
    let tmux = |n: &str| format!("AgentDesk-claude-guard-{n}");
    let key = |n: &str| format!("{host}:{}", tmux(n));
    let names = [
        "legacy",
        "pending",
        "bound",
        "retired",
        "unknown",
        "json-null",
        "traced",
        "probefail",
    ];
    for (index, n) in names.into_iter().enumerate() {
        let channel = format!("440{index}");
        let owner = owner(&channel);
        let raw = match n {
            "pending" => Some(wire(&record(&owner, "n1", HostedState::Pending))),
            "bound" => Some(wire(&record(&owner, "n1", HostedState::Bound))),
            "retired" => Some(wire(&record(&owner, "n1", HostedState::Retired))),
            "unknown" => Some(future_schema(&owner)),
            "json-null" => Some(serde_json::Value::Null),
            _ => None,
        };
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, last_heartbeat, identity_kind,
                                   discord_token_hash, channel_id, hosted_execution)
             VALUES ($1, 'claude', 'idle', NOW() - INTERVAL '7 hours', 'discord_channel', $2, $3, $4)",
        )
        .bind(key(n))
        .bind(TOKEN)
        .bind(&channel)
        .bind(raw)
        .execute(&pool)
        .await
        .unwrap();
    }
    let marker = crate::services::tmux_common::session_temp_path(&tmux("traced"), "host_kind");
    std::fs::write(marker, "herdr").unwrap();
    let row = |n: &str| {
        let pool = pool.clone();
        let key = key(n);
        async move {
            sqlx::query_scalar::<_, serde_json::Value>(
                "SELECT to_jsonb(s) FROM sessions s WHERE session_key = $1",
            )
            .bind(key)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    let kill = |n: &str, reason: &str| {
        super::kill_tmux_session(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            Path(key(n)),
            Json(crate::services::dispatched_sessions::KillTmuxOptions {
                reason: Some(reason.to_string()),
                minimum_idle_minutes: Some(360),
            }),
        )
    };

    for n in names.into_iter().filter(|n| *n != "legacy") {
        let before = row(n).await;
        // A failed probe is preserved whatever the reason, forced ones included.
        let reason = if n == "probefail" {
            "operator cleanup"
        } else {
            "idle 7시간 초과 — 자동 정리"
        };
        let (status, Json(body)) = kill(n, reason).await;
        assert_eq!(status, StatusCode::OK, "{n}: {body}");
        assert_eq!(body["tmux_killed"], false, "{n}: {body}");
        assert_eq!(
            body["tmux_was_alive"],
            serde_json::Value::Null,
            "{n}: {body}"
        );
        assert_eq!(body["skipped_provider_activity_guard"], true, "{n}: {body}");
        let expected = if n == "probefail" {
            "tmux_probe_failed"
        } else {
            "host_not_legacy_tmux"
        };
        assert_eq!(body["preserved_reason"], expected, "{n}: {body}");
        assert_eq!(row(n).await, before, "{n}: no column may change");
    }
    let calls = std::fs::read_to_string(tmux_probe.path().join("calls")).unwrap_or_default();
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        [format!("has-session -t ={}:", tmux("probefail"))],
        "only a legacy row reaches tmux"
    );

    // Positive control: a legacy row whose tmux is gone is still reconciled.
    let (status, Json(body)) = kill("legacy", "idle 7시간 초과 — 자동 정리").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["tmux_was_alive"], false, "{body}");
    assert_eq!(body["session_row_disconnected"], true, "{body}");
    assert_eq!(row("legacy").await["status"], "disconnected");

    pool.close().await;
    pg_db.drop().await;
}

/// Where `tmux` resolves: a live isolated server holding a same-name session, a missing
/// binary (exit 127), or a real binary with no server socket.
#[derive(Clone, Copy, Debug, PartialEq)]
enum TmuxCondition {
    LiveServer,
    MissingBinary,
    NoServerSocket,
}
use TmuxCondition::{LiveServer, MissingBinary, NoServerSocket};

/// Every stored host state other than a confirmed legacy tmux row.
const NON_LEGACY: [&str; 8] = [
    "pending",
    "bound",
    "retired",
    "unknown",
    "json-null",
    "herdr",
    "marker-dir",
    "zellij",
];

/// A recording `tmux` on PATH and a private `TMUX_TMPDIR`, so no test touches a real server.
struct TmuxEnv {
    live: bool,
    real: std::path::PathBuf,
    sockets: tempfile::TempDir,
    probe: tempfile::TempDir,
    _guards: Vec<crate::config::TestEnvVarGuard>,
}

impl TmuxEnv {
    fn install(condition: TmuxCondition) -> Self {
        let real = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join("tmux"))
            .find(|path| path.is_file())
            .expect("the live and socketless conditions need a real tmux binary");
        let sockets = tempfile::TempDir::new().expect("tmux socket dir");
        let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
        let attached = crate::config::TestEnvVarGuard::capture_after_shared_test_env_lock("TMUX");
        let mut guards = vec![set("TMUX_TMPDIR", sockets.path()), attached];
        unsafe { std::env::remove_var("TMUX") };
        let tail = match condition {
            MissingBinary => "exit 127".to_string(),
            _ => format!("exec '{}' \"$@\"", real.display()),
        };
        let record = "[ \"$1\" = -u ] && shift\necho \"$*\" >> \"$(dirname \"$0\")/calls\"";
        let (probe, path) = install_tmux_probe(&format!("{record}\n{tail}"));
        guards.push(path);
        let live = condition == LiveServer;
        Self {
            live,
            real,
            sockets,
            probe,
            _guards: guards,
        }
    }

    fn real_tmux(&self, args: &[&str]) -> bool {
        let mut command = std::process::Command::new(&self.real);
        command
            .args(args)
            .env("TMUX_TMPDIR", self.sockets.path())
            .env_remove("TMUX");
        let status = command.stderr(std::process::Stdio::null()).status();
        status.is_ok_and(|status| status.success())
    }

    fn start(&self, name: &str) {
        let args = ["new-session", "-d", "-s", name, "sleep 600"];
        assert!(
            !self.live || self.real_tmux(&args),
            "start live tmux {name}"
        );
    }

    fn alive(&self, name: &str) -> bool {
        self.real_tmux(&["has-session", "-t", &format!("={name}:")])
    }

    fn take_calls(&self) -> Vec<String> {
        let path = self.probe.path().join("calls");
        let calls = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::remove_file(path);
        calls.lines().map(str::to_string).collect()
    }
}

impl Drop for TmuxEnv {
    fn drop(&mut self) {
        let _ = self.real_tmux(&["kill-server"]);
    }
}

/// Runs one statement, binding every argument as nullable text.
async fn exec(pool: &sqlx::PgPool, sql: &str, args: &[Option<&str>]) {
    let query = args
        .iter()
        .fold(sqlx::query(sql), |query, arg| query.bind(*arg));
    query.execute(pool).await.unwrap();
}

/// Every column of the named rows, so "unchanged" means no write of any kind.
async fn snapshot(pool: &sqlx::PgPool, table: &str, column: &str, ids: &[&str]) -> String {
    let query = format!("SELECT jsonb_agg(to_jsonb(t) ORDER BY t.{column})::text FROM {table} t");
    let query = format!("{query} WHERE t.{column} = ANY($1)");
    let row = sqlx::query_scalar::<_, Option<String>>(&query).bind(ids);
    row.fetch_one(pool).await.unwrap().unwrap_or_default()
}

/// One column of one row, read through [`snapshot`].
async fn field(pool: &sqlx::PgPool, table: &str, id: &str, column: &str) -> serde_json::Value {
    let id_column = if table == "sessions" {
        "session_key"
    } else {
        "id"
    };
    let rows = snapshot(pool, table, id_column, &[id]).await;
    serde_json::from_str::<serde_json::Value>(&rows).unwrap()[0][column].clone()
}

/// Seeds one row attached to `dispatch`. `legacy-null` is a legacy row with no stored
/// provider or identity; every other state carries the identity its hosted record names.
async fn seed_host_row(
    pool: &sqlx::PgPool,
    host: &str,
    name: &str,
    ch: &str,
    dispatch: &str,
) -> String {
    use crate::db::dispatched_sessions::hosted_execution::HostedState::*;
    use crate::db::dispatched_sessions::hosted_execution::tests::*;
    let owner = owner(ch);
    let raw = match host {
        "pending" => Some(wire(&record(&owner, "n1", Pending))),
        "bound" => Some(wire(&record(&owner, "n1", Bound))),
        "retired" => Some(wire(&record(&owner, "n1", Retired))),
        "unknown" => Some(future_schema(&owner)),
        "json-null" => Some(serde_json::Value::Null),
        _ => None,
    };
    let marker = crate::services::tmux_common::session_temp_path(name, "host_kind");
    match host {
        "herdr" | "zellij" => std::fs::write(marker, host).unwrap(),
        "marker-dir" => std::fs::create_dir_all(marker).unwrap(),
        _ => {}
    }
    let keyed = (host != "legacy-null").then_some(());
    let key = format!("{}:{name}", crate::services::platform::hostname_short());
    let raw = raw.map(|raw| raw.to_string());
    let sql = "INSERT INTO sessions (session_key, provider, status, last_heartbeat, identity_kind,
                                     discord_token_hash, channel_id, thread_channel_id,
                                     active_dispatch_id, claude_session_id, hosted_execution)
               VALUES ($1, $2, 'turn_active', NOW(), $3, $4, $5, $6, $7, 'selector', $8::jsonb)";
    let identity = [keyed.map(|_| "claude"), keyed.map(|_| "discord_channel")];
    let args = [
        Some(key.as_str()),
        identity[0],
        identity[1],
        keyed.map(|_| TOKEN),
    ];
    let args = [
        &args[..],
        &[keyed.map(|_| ch), Some(ch), Some(dispatch), raw.as_deref()],
    ];
    exec(pool, sql, &args.concat()).await;
    key
}

async fn seed_dispatch(pool: &sqlx::PgPool, dispatch: &str, card: Option<&str>) {
    let sql = "INSERT INTO task_dispatches (id, kanban_card_id, dispatch_type, status, title)
               VALUES ($1, $2, 'implementation', 'dispatched', 'host guard')";
    exec(pool, sql, &[Some(dispatch), card]).await;
}

/// Runs one force-kill entry the way its operator surface does: the force-kill route or the
/// queue's forced turn cancel, both with no runtime registry.
async fn force_kill_entry(
    state: &AppState,
    entry: &str,
    key: &str,
    ch: &str,
) -> (StatusCode, String) {
    if entry == "route" {
        let reason = "operator cleanup";
        let kill = super::force_kill_session_impl_with_reason(state, key, false, reason);
        let (status, Json(body)) = kill.await;
        return (status, body.to_string());
    }
    let forward = crate::services::session_forwarding::ForwardCallerContext::from(state);
    let headers = axum::http::HeaderMap::new();
    let cancel = state.queue_service();
    match cancel.cancel_turn(None, ch, true, &headers, &forward).await {
        Ok(body) => (StatusCode::OK, body.to_string()),
        Err(error) => (error.status(), error.message().to_string()),
    }
}

/// Force-kill with no runtime registry: only a confirmed legacy row reaches tmux or changes
/// the session and its dispatch; every other host is a conflict with nothing touched.
#[tokio::test(flavor = "current_thread")]
async fn force_kill_without_runtime_keys_refuses_unconfirmed_hosts_pg() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let runtime_root = tempfile::TempDir::new().expect("runtime root");
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let _root_guard = set("AGENTDESK_ROOT_DIR", runtime_root.path());
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
    let state = test_state(pool.clone());
    let conditions = [LiveServer, MissingBinary, NoServerSocket];
    for (round, condition) in conditions.into_iter().enumerate() {
        let tmux = TmuxEnv::install(condition);
        for (entry, settled) in [("route", "failed"), ("queue", "cancelled")] {
            let at = |n: &str| format!("{entry}-{round}-{n}");
            let name = |n: &str| format!("AgentDesk-claude-{}", at(n));
            let offset = 6_510_000 + round * 1_000 + if entry == "route" { 0 } else { 500 };
            let channel = |index: usize| (offset + index).to_string();
            // Positive control: a legacy row with no stored provider is still killed and settled.
            let (legacy, ch, dispatch) = (name("legacy"), channel(99), at("legacy"));
            seed_dispatch(&pool, &dispatch, None).await;
            tmux.start(&legacy);
            let key = seed_host_row(&pool, "legacy-null", &legacy, &ch, &dispatch).await;
            let (status, body) = force_kill_entry(&state, entry, &key, &ch).await;
            assert_eq!(status, StatusCode::OK, "{condition:?} {entry}: {body}");
            let session = field(&pool, "sessions", &key, "status").await;
            let dispatch = field(&pool, "task_dispatches", &dispatch, "status").await;
            assert_eq!(
                [session, dispatch],
                ["disconnected", settled],
                "{condition:?} {entry}"
            );
            assert!(
                !tmux.alive(&legacy),
                "{condition:?} {entry}: legacy tmux is killed"
            );
            let _ = tmux.take_calls();

            for (index, host) in NON_LEGACY.into_iter().enumerate() {
                let (ch, dispatch) = (channel(index), at(host));
                seed_dispatch(&pool, &dispatch, None).await;
                tmux.start(&name(host));
                let key = seed_host_row(&pool, host, &name(host), &ch, &dispatch).await;
                let rows = || async {
                    let session = snapshot(&pool, "sessions", "session_key", &[&key]).await;
                    (
                        session,
                        snapshot(&pool, "task_dispatches", "id", &[&dispatch]).await,
                    )
                };
                let before = rows().await;
                let (status, body) = force_kill_entry(&state, entry, &key, &ch).await;
                let refused = status == StatusCode::CONFLICT
                    && body.contains("session host is not legacy tmux");
                assert!(refused, "{condition:?} {entry} {host}: {status} {body}");
                assert_eq!(
                    rows().await,
                    before,
                    "{condition:?} {entry} {host}: nothing changes"
                );
                assert!(
                    !tmux.live || tmux.alive(&name(host)),
                    "{entry} {host}: tmux survives"
                );
            }
            assert_eq!(
                tmux.take_calls(),
                [""; 0],
                "{condition:?} {entry}: no tmux call"
            );
        }

        // A row-keyed kill whose row is gone is not legacy evidence either.
        let missing = format!("AgentDesk-claude-missing-{round}");
        tmux.start(&missing);
        let key = format!("{}:{missing}", crate::services::platform::hostname_short());
        let target = crate::services::turn_lifecycle::TurnLifecycleTarget {
            provider: Some(crate::services::provider::ProviderKind::Claude),
            channel_id: None,
            tmux_name: missing.clone(),
        };
        let (pool, session_key, stored_provider) = (&pool, key.as_str(), Some("claude"));
        let row = crate::services::turn_lifecycle::ForceKillRow {
            pool,
            session_key,
            stored_provider,
        };
        let kill = crate::services::turn_lifecycle::force_kill_turn_for_row;
        let lifecycle = kill(None, &target, row, "operator cleanup", "force_kill_api").await;
        assert!(
            lifecycle.host_guard_kept(),
            "{condition:?}: a missing row is kept"
        );
        assert!(
            !tmux.live || tmux.alive(&missing),
            "the session of a missing row survives"
        );
        assert_eq!(tmux.take_calls(), [""; 0], "{condition:?}: no tmux call");
    }
    pool.close().await;
    pg_db.drop().await;
}

/// Seeds a card with one live dispatch, its auto-queue entry, and one session per host
/// state attached to that dispatch. Returns the session keys and tmux names.
async fn seed_card(
    pool: &sqlx::PgPool,
    tmux: &TmuxEnv,
    card: &str,
    hosts: &[&str],
    channel: usize,
) -> (Vec<String>, Vec<String>) {
    let dispatch = format!("{card}-d");
    let sql = "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id)
               VALUES ($1, 'host guard revert', 'in_progress', $1 || '-d')";
    exec(pool, sql, &[Some(card)]).await;
    seed_dispatch(pool, &dispatch, Some(card)).await;
    let sql =
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id)
               VALUES ($1 || '-e', 'k1-run', $1, 'k1-agent', 'dispatched', $1 || '-d')";
    exec(pool, sql, &[Some(card)]).await;
    let (mut keys, mut names) = (Vec::new(), Vec::new());
    for (index, host) in hosts.iter().enumerate() {
        let name = format!("AgentDesk-claude-{card}-{index}");
        tmux.start(&name);
        let ch = (channel + index).to_string();
        keys.push(seed_host_row(pool, host, &name, &ch, &dispatch).await);
        names.push(name);
    }
    (keys, names)
}

/// The backlog revert idles and detaches every live session of a card inside its transition,
/// so one session whose host is not confirmed legacy tmux refuses the whole revert first.
#[tokio::test(flavor = "current_thread")]
async fn backlog_revert_refuses_the_card_when_any_session_host_is_unconfirmed_pg() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let runtime_root = tempfile::TempDir::new().expect("runtime root");
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let _root_guard = set("AGENTDESK_ROOT_DIR", runtime_root.path());
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
    let sql = "INSERT INTO agents (id, name, provider, discord_channel_id)
               VALUES ('k1-agent', 'K1', 'claude', '6539999')";
    exec(&pool, sql, &[]).await;
    let sql = "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
               VALUES ('k1-run', 'repo', 'k1-agent', 'active')";
    exec(&pool, sql, &[]).await;
    let mut config = crate::config::Config::default();
    config.policies.dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
    config.policies.hot_reload = false;
    let mut state = test_state(pool.clone());
    state.engine = crate::engine::PolicyEngine::new_with_pg(&config, Some(pool.clone())).unwrap();
    state.config = Arc::new(config);
    let revert = |card: String| {
        let (state, source) = (state.clone(), "test:host-guard");
        async move {
            let revert = crate::server::routes::kanban::transition_card_to_backlog_with_cleanup;
            revert(&state, &card, source).await
        }
    };
    let card_rows = |card: String, keys: Vec<String>| {
        let pool = pool.clone();
        async move {
            let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
            let (dispatch, entry) = (format!("{card}-d"), format!("{card}-e"));
            [
                snapshot(&pool, "kanban_cards", "id", &[&card]).await,
                snapshot(&pool, "task_dispatches", "id", &[&dispatch]).await,
                snapshot(&pool, "auto_queue_entries", "id", &[&entry]).await,
                snapshot(&pool, "sessions", "session_key", &keys).await,
            ]
        }
    };
    let conditions = [LiveServer, MissingBinary, NoServerSocket];
    for (round, condition) in conditions.into_iter().enumerate() {
        let tmux = TmuxEnv::install(condition);
        // Positive control: a card whose only session is legacy with no stored provider reverts.
        let card = format!("k1-{round}-legacy");
        let channel = 6_539_000 + round;
        let (keys, names) = seed_card(&pool, &tmux, &card, &["legacy-null"], channel).await;
        revert(card.clone())
            .await
            .expect("a legacy-only card reverts");
        let settled = [
            field(&pool, "sessions", &keys[0], "status").await,
            field(&pool, "sessions", &keys[0], "active_dispatch_id").await,
            field(&pool, "task_dispatches", &format!("{card}-d"), "status").await,
            field(&pool, "auto_queue_entries", &format!("{card}-e"), "status").await,
        ];
        let skipped = crate::db::auto_queue::ENTRY_STATUS_SKIPPED;
        let expected = [Some("disconnected"), None, Some("cancelled"), Some(skipped)];
        assert_eq!(
            settled,
            expected.map(serde_json::Value::from),
            "{condition:?}"
        );
        assert!(
            !tmux.alive(&names[0]),
            "{condition:?}: legacy tmux is killed"
        );
        let _ = tmux.take_calls();

        let mut cases: Vec<Vec<&str>> = NON_LEGACY.iter().map(|h| vec!["legacy-null", h]).collect();
        cases.push(vec!["bound"]);
        for (index, hosts) in cases.iter().enumerate() {
            let card = format!("k1-{round}-{index}");
            let channel = 6_530_000 + round * 1_000 + index * 10;
            let (keys, names) = seed_card(&pool, &tmux, &card, hosts, channel).await;
            let before = card_rows(card.clone(), keys.clone()).await;
            let error = revert(card.clone())
                .await
                .expect_err("an unconfirmed host refuses it");
            let error = format!("{error:#}");
            assert!(
                error.contains("backlog revert refused"),
                "{condition:?} {hosts:?}: {error}"
            );
            let after = card_rows(card, keys).await;
            assert_eq!(
                after, before,
                "{condition:?} {hosts:?}: nothing of the card changes"
            );
            let alive = names.iter().all(|name| tmux.alive(name));
            assert!(!tmux.live || alive, "{hosts:?}: every tmux survives");
        }
        assert_eq!(
            tmux.take_calls(),
            [""; 0],
            "{condition:?}: a refusal reaches no tmux"
        );
    }
    pool.close().await;
    pg_db.drop().await;
}

/// A nameless force-kill whose inflight row names a session the guard refuses: the name
/// lookup must not save that row (its finalizer backfill) before the guard keeps the turn.
#[tokio::test(flavor = "current_thread")]
async fn nameless_force_kill_refused_by_its_inflight_name_writes_nothing_pg() {
    use crate::services::discord::host_teardown_gate::test_support as host;
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let runtime_root = tempfile::TempDir::new().expect("runtime root");
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let _root_guard = set("AGENTDESK_ROOT_DIR", runtime_root.path());
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
    let (shared, registry) = host::runtime(&pool).await;
    for (round, condition) in [LiveServer, MissingBinary, NoServerSocket]
        .into_iter()
        .enumerate()
    {
        let tmux = TmuxEnv::install(condition);
        let channel =
            poise::serenity_prelude::ChannelId::new(1_479_671_301_387_065_100 + round as u64);
        let name = format!("AgentDesk-claude-backfill-{round}");
        let key = host::channel_key(&shared, &name);
        host::seed(&pool, &key, &name, channel.get(), host::Stored::Hosted).await;
        tmux.start(&name);
        let token = host::busy_turn(&shared, channel, &name).await;
        let path = host::inflight_needing_backfill(channel);
        let before = (
            std::fs::read(&path).unwrap(),
            snapshot(&pool, "sessions", "session_key", &[&key]).await,
        );
        let target = crate::services::turn_lifecycle::TurnLifecycleTarget {
            provider: Some(crate::services::provider::ProviderKind::Claude),
            channel_id: Some(channel),
            tmux_name: String::new(),
        };
        let kill = crate::services::turn_lifecycle::force_kill_turn;
        let lifecycle = kill(
            Some(&registry),
            &target,
            "operator cleanup",
            "force_kill_api",
        )
        .await;
        assert!(lifecycle.host_guard_kept(), "{condition:?}");
        let after = (
            std::fs::read(&path).unwrap(),
            snapshot(&pool, "sessions", "session_key", &[&key]).await,
        );
        assert!(
            after == before,
            "{condition:?}: the inflight row and session row are untouched"
        );
        assert!(
            host::turn_kept(&shared, channel, &token).await,
            "{condition:?}: the turn is kept"
        );
        assert!(!host::stop_recorded(channel), "{condition:?}: no tombstone");
        assert!(
            !tmux.live || tmux.alive(&name),
            "{condition:?}: tmux survives"
        );
        assert_eq!(tmux.take_calls(), [""; 0], "{condition:?}: no tmux call");
    }
    pool.close().await;
    pg_db.drop().await;
}

/// A nameless process-backend turn on an unkeyed idle channel is still cancelled: the
/// nameless gate admits it and the force-kill stops the turn as before.
#[tokio::test(flavor = "current_thread")]
async fn nameless_process_turn_force_kill_still_cancels_pg() {
    use crate::services::discord::host_teardown_gate::test_support as host;
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let runtime_root = tempfile::TempDir::new().expect("runtime root");
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let _root_guard = set("AGENTDESK_ROOT_DIR", runtime_root.path());
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
    let (shared, registry) = host::runtime(&pool).await;
    for (round, condition) in [LiveServer, MissingBinary, NoServerSocket]
        .into_iter()
        .enumerate()
    {
        let tmux = TmuxEnv::install(condition);
        let channel =
            poise::serenity_prelude::ChannelId::new(1_479_671_301_387_065_200 + round as u64);
        let token = host::nameless_turn(&shared, channel).await;
        let target = crate::services::turn_lifecycle::TurnLifecycleTarget {
            provider: Some(crate::services::provider::ProviderKind::Claude),
            channel_id: Some(channel),
            tmux_name: String::new(),
        };
        let kill = crate::services::turn_lifecycle::force_kill_turn;
        let lifecycle = kill(
            Some(&registry),
            &target,
            "operator cleanup",
            "force_kill_api",
        )
        .await;
        assert!(
            !lifecycle.host_guard_kept(),
            "{condition:?}: {}",
            lifecycle.lifecycle_path
        );
        let cancelled = token.cancelled.load(std::sync::atomic::Ordering::SeqCst);
        assert!(cancelled, "{condition:?}: the turn's token is cancelled");
        assert!(
            !host::mailbox_turn_active(&shared, channel).await,
            "{condition:?}: mailbox freed"
        );
        assert_eq!(
            tmux.take_calls(),
            [""; 0],
            "{condition:?}: a nameless turn reaches no tmux"
        );
    }
    pool.close().await;
    pg_db.drop().await;
}

/// A legacy card whose kill the runtime keeps (no runtime takes the channel) is refused whole
/// before anything changes; once a runtime takes the channel the same card reverts.
#[tokio::test(flavor = "current_thread")]
async fn backlog_revert_refuses_a_card_whose_kill_the_runtime_keeps_pg() {
    use crate::services::discord::host_teardown_gate::test_support as host;
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let runtime_root = tempfile::TempDir::new().expect("runtime root");
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let _root_guard = set("AGENTDESK_ROOT_DIR", runtime_root.path());
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
    let sql = "INSERT INTO agents (id, name, provider, discord_channel_id)
               VALUES ('k1-agent', 'K1', 'claude', '6549999')";
    exec(&pool, sql, &[]).await;
    let sql = "INSERT INTO auto_queue_runs (id, repo, agent_id, status)
               VALUES ('k1-run', 'repo', 'k1-agent', 'active')";
    exec(&pool, sql, &[]).await;
    let mut config = crate::config::Config::default();
    config.policies.dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
    config.policies.hot_reload = false;
    let mut state = test_state(pool.clone());
    state.engine = crate::engine::PolicyEngine::new_with_pg(&config, Some(pool.clone())).unwrap();
    state.config = Arc::new(config);
    let (shared, registry) = host::runtime(&pool).await;
    state.health_registry = Some(registry);
    for (round, condition) in [LiveServer, MissingBinary, NoServerSocket]
        .into_iter()
        .enumerate()
    {
        let tmux = TmuxEnv::install(condition);
        for taken in [false, true] {
            host::allow_channels(&shared, if taken { &[] } else { &[1] }).await;
            let card = format!("k2-{round}-{taken}");
            let channel = 6_549_000 + round * 10 + usize::from(taken) * 5;
            let hosts = ["legacy", "legacy"];
            let (keys, names) = seed_card(&pool, &tmux, &card, &hosts, channel).await;
            let rows = || async {
                let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
                let (dispatch, entry) = (format!("{card}-d"), format!("{card}-e"));
                [
                    snapshot(&pool, "kanban_cards", "id", &[&card]).await,
                    snapshot(&pool, "task_dispatches", "id", &[&dispatch]).await,
                    snapshot(&pool, "auto_queue_entries", "id", &[&entry]).await,
                    snapshot(&pool, "sessions", "session_key", &keys).await,
                ]
            };
            let before = rows().await;
            let revert = crate::server::routes::kanban::transition_card_to_backlog_with_cleanup;
            let reverted = revert(&state, &card, "test:shared-verdict").await;
            if taken {
                reverted.expect("a card whose kill the runtime admits reverts");
                let dispatch = format!("{card}-d");
                let status = field(&pool, "task_dispatches", &dispatch, "status").await;
                assert_eq!(status, "cancelled", "{condition:?}");
                for key in &keys {
                    let status = field(&pool, "sessions", key, "status").await;
                    assert_eq!(status, "disconnected", "{condition:?}");
                }
                let dead = names.iter().all(|name| !tmux.alive(name));
                assert!(dead, "{condition:?}: the admitted kills reach tmux");
                let _ = tmux.take_calls();
                continue;
            }
            let error = format!("{:#}", reverted.expect_err("the kept kill refuses it"));
            let refused = error.contains("is kept by the force-kill host guard");
            assert!(refused, "{condition:?}: {error}");
            assert_eq!(
                rows().await,
                before,
                "{condition:?}: nothing of the card changes"
            );
            let alive = names.iter().all(|name| tmux.alive(name));
            assert!(!tmux.live || alive, "{condition:?}: every tmux survives");
            assert_eq!(tmux.take_calls(), [""; 0], "{condition:?}: no tmux call");
        }
    }
    pool.close().await;
    pg_db.drop().await;
}

/// `/resume` of a legacy row whose teardown the runtime keeps refuses before the durable
/// rebind; once a runtime takes the channel the same row resumes.
#[tokio::test(flavor = "current_thread")]
async fn resume_refuses_a_session_whose_teardown_the_runtime_keeps_pg() {
    use crate::services::discord::host_teardown_gate::test_support as host;
    use crate::services::session_resume::{
        ResumePreviousOptions, ResumeRebindError, perform_resume_rebind,
    };
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let runtime_root = tempfile::TempDir::new().expect("runtime root");
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let _root_guard = set("AGENTDESK_ROOT_DIR", runtime_root.path());
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
    let (shared, registry) = host::runtime(&pool).await;
    let (old_cwd, target_cwd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let target_sid = "99999999-9999-9999-9999-999999999999";
    let opts = ResumePreviousOptions {
        session_id: Some(target_sid.to_string()),
        cwd: Some(target_cwd.path().to_str().unwrap().to_string()),
    };
    for (round, condition) in [LiveServer, MissingBinary, NoServerSocket]
        .into_iter()
        .enumerate()
    {
        let tmux = TmuxEnv::install(condition);
        for taken in [false, true] {
            host::allow_channels(&shared, if taken { &[] } else { &[1] }).await;
            let name = format!("AgentDesk-claude-resume-verdict-{round}-{taken}");
            let channel = 1_479_671_301_387_067_000 + round as u64 * 10 + u64::from(taken);
            let channel = poise::serenity_prelude::ChannelId::new(channel);
            let key = host::channel_key(&shared, &name);
            let sql = "INSERT INTO sessions (session_key, provider, status, cwd, claude_session_id,
                                             raw_provider_session_id, last_heartbeat)
                       VALUES ($1, 'claude', 'idle', $2, 'old-sid', 'old-sid', NOW())";
            let cwd = old_cwd.path().to_str();
            exec(&pool, sql, &[Some(key.as_str()), cwd]).await;
            tmux.start(&name);
            let before = snapshot(&pool, "sessions", "session_key", &[&key]).await;
            let claude = Some(crate::services::provider::ProviderKind::Claude);
            let resume = perform_resume_rebind(
                &pool,
                Some(&registry),
                &key,
                claude,
                Some(channel),
                &name,
                &opts,
            );
            let resumed = resume.await;
            if taken {
                resumed.expect("a teardown the runtime admits resumes");
                let sid = field(&pool, "sessions", &key, "claude_session_id").await;
                assert_eq!(sid, target_sid, "{condition:?}: the row is rebound");
                assert!(
                    !tmux.alive(&name),
                    "{condition:?}: the old tmux is torn down"
                );
                let _ = tmux.take_calls();
                continue;
            }
            let refused = matches!(&resumed, Err(ResumeRebindError::HostUnsupported(reason))
                if reason.contains("teardown is kept"));
            assert!(refused, "{condition:?}: {resumed:?}");
            let after = snapshot(&pool, "sessions", "session_key", &[&key]).await;
            assert_eq!(after, before, "{condition:?}: no durable rebind");
            assert!(
                !tmux.live || tmux.alive(&name),
                "{condition:?}: tmux survives"
            );
            assert_eq!(tmux.take_calls(), [""; 0], "{condition:?}: no tmux call");
        }
    }
    pool.close().await;
    pg_db.drop().await;
}

/// A routine kill or fresh teardown the host guard keeps disconnects nothing: the session row
/// stays as it was. Once a runtime takes the channel the same row is disconnected.
#[tokio::test(flavor = "current_thread")]
async fn routine_teardown_kept_by_the_host_guard_disconnects_nothing_pg() {
    use crate::services::discord::host_teardown_gate::test_support as host;
    use crate::services::routines::{RoutineSessionCommand, RoutineSessionController};
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let runtime_root = tempfile::TempDir::new().expect("runtime root");
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let _root_guard = set("AGENTDESK_ROOT_DIR", runtime_root.path());
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
    let sql = "INSERT INTO agents (id, name, provider, discord_channel_cc)
               VALUES ('r-agent', 'routine guard', 'claude', '6551000')";
    exec(&pool, sql, &[]).await;
    let (shared, registry) = host::runtime(&pool).await;
    let controller = RoutineSessionController::new(Arc::new(pool.clone()), Some(registry));
    let host_name = crate::services::platform::hostname_short();
    for (round, condition) in [LiveServer, MissingBinary, NoServerSocket]
        .into_iter()
        .enumerate()
    {
        let tmux = TmuxEnv::install(condition);
        for taken in [false, true] {
            host::allow_channels(&shared, if taken { &[] } else { &[1] }).await;
            for (index, entry) in ["kill", "fresh", "owned"].into_iter().enumerate() {
                let thread = 1_479_671_301_387_068_000 + (round * 100 + index) as u64;
                let thread = (thread + u64::from(taken) * 10).to_string();
                let name = format!("AgentDesk-claude-routine-{round}-{taken}-{entry}");
                let key = format!("{host_name}:{name}");
                let sql = "INSERT INTO sessions (session_key, agent_id, provider, status,
                                                 thread_channel_id, claude_session_id, last_heartbeat)
                           VALUES ($1, 'r-agent', 'claude', 'turn_active', $2, 'sid', NOW())";
                exec(&pool, sql, &[Some(key.as_str()), Some(thread.as_str())]).await;
                tmux.start(&name);
                let strategy = if entry == "kill" {
                    "persistent"
                } else {
                    "fresh"
                };
                let routine = crate::services::routines::store::RoutineRecord {
                    id: format!("routine-{round}-{taken}-{entry}"),
                    agent_id: Some("r-agent".to_string()),
                    fallback_agent_id: None,
                    max_retries: 0,
                    script_ref: "script".to_string(),
                    name: "Routine".to_string(),
                    status: "enabled".to_string(),
                    execution_strategy: strategy.to_string(),
                    schedule: None,
                    next_due_at: None,
                    last_run_at: None,
                    last_result: None,
                    checkpoint: None,
                    discord_thread_id: Some(thread),
                    timeout_secs: None,
                    in_flight_run_id: None,
                    pause_reason: None,
                    created_at: chrono::Utc::now(),
                    updated_at: chrono::Utc::now(),
                };
                let before = snapshot(&pool, "sessions", "session_key", &[&key]).await;
                let result = match entry {
                    "kill" => {
                        let kill = RoutineSessionCommand::Kill;
                        let control = controller.control_persistent_session(&routine, kill, "test");
                        control.await
                    }
                    "fresh" => {
                        controller
                            .teardown_fresh_session(&routine, None, "test")
                            .await
                    }
                    _ => {
                        let teardown =
                            controller.teardown_fresh_session_by_name(&routine, &key, "test");
                        teardown.await
                    }
                };
                let result = result.expect("the routine teardown runs");
                let case = format!("{condition:?} {entry} taken={taken}");
                if taken {
                    assert_eq!(result.disconnected_sessions, 1, "{case}");
                    let status = field(&pool, "sessions", &key, "status").await;
                    assert_eq!(status, "disconnected", "{case}");
                    assert!(!tmux.alive(&name), "{case}: the admitted kill reaches tmux");
                    let _ = tmux.take_calls();
                    continue;
                }
                assert_eq!(result.lifecycle_path, "host-guard-kept", "{case}");
                assert_eq!(result.disconnected_sessions, 0, "{case}");
                let after = snapshot(&pool, "sessions", "session_key", &[&key]).await;
                assert_eq!(after, before, "{case}: the session row is untouched");
                assert!(!tmux.live || tmux.alive(&name), "{case}: tmux survives");
                assert_eq!(tmux.take_calls(), [""; 0], "{case}: no tmux call");
            }
        }
    }
    pool.close().await;
    pg_db.drop().await;
}
