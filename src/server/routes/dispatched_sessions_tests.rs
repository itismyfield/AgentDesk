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
    sqlx::query("INSERT INTO kv_meta (key, value) VALUES ('kanban_human_alert_channel_id', '42')")
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
    let alerts = || async {
        sqlx::query_scalar::<_, String>(
            "SELECT content FROM message_outbox
             WHERE reason_code = 'relay_signal.idle_cleanup_preserved'",
        )
        .fetch_all(&pool)
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
    let sent = alerts().await;
    assert_eq!(sent.len(), 1, "repeated skips must dedupe: {sent:?}");
    assert!(sent[0].contains(&channel), "{}", sent[0]);
    assert!(sent[0].contains("transcript_unresolved"), "{}", sent[0]);
    assert!(sent[0].contains("7시간"), "{}", sent[0]);

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
    assert_eq!(
        alerts().await.len(),
        1,
        "a proven-idle kill raises no alert"
    );

    pool.close().await;
    pg_db.drop().await;
}
