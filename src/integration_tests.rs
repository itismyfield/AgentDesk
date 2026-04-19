//! #124: Pipeline integration test harness — 6 mandatory scenarios
//!
//! These tests verify pipeline correctness end-to-end before #106 data-driven transition.

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::{Arc, Mutex, Once, OnceLock};

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use crate::db;
    use crate::dispatch;
    use crate::engine::PolicyEngine;
    use crate::kanban;
    use crate::server::routes::AppState;
    use serde_json::json;

    mod high_risk_recovery;

    fn test_db() -> db::Db {
        let conn = libsql_rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        db::schema::migrate(&conn).unwrap();
        db::wrap_conn(conn)
    }

    fn test_engine(db: &db::Db) -> PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
        config.policies.hot_reload = false;
        PolicyEngine::new(&config, db.clone()).unwrap()
    }

    fn test_engine_with_dir(db: &db::Db, dir: &std::path::Path) -> PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = dir.to_path_buf();
        config.policies.hot_reload = false;
        PolicyEngine::new(&config, db.clone()).unwrap()
    }

    struct WorktreeCommitOverrideGuard;

    impl WorktreeCommitOverrideGuard {
        fn set(commit: &str) -> Self {
            crate::server::routes::review_verdict::set_test_worktree_commit_override(Some(
                commit.to_string(),
            ));
            Self
        }
    }

    impl Drop for WorktreeCommitOverrideGuard {
        fn drop(&mut self) {
            crate::server::routes::review_verdict::clear_test_worktree_commit_override();
        }
    }

    fn repo_dir_env_lock() -> &'static Mutex<()> {
        crate::config::shared_test_env_lock()
    }

    fn lock_repo_dir_env() -> std::sync::MutexGuard<'static, ()> {
        repo_dir_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    struct RepoDirOverride {
        _guard: std::sync::MutexGuard<'static, ()>,
        previous: Option<OsString>,
    }

    impl RepoDirOverride {
        fn new(path: &std::path::Path) -> Self {
            let guard = lock_repo_dir_env();
            let previous = std::env::var_os("AGENTDESK_REPO_DIR");
            unsafe { std::env::set_var("AGENTDESK_REPO_DIR", path) };
            Self {
                _guard: guard,
                previous,
            }
        }
    }

    impl Drop for RepoDirOverride {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_REPO_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_REPO_DIR") },
            }
        }
    }

    struct RuntimeRootOverride {
        _guard: std::sync::MutexGuard<'static, ()>,
        previous: Option<OsString>,
    }

    impl RuntimeRootOverride {
        fn new(path: &std::path::Path) -> Self {
            let guard = lock_repo_dir_env();
            let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
            Self {
                _guard: guard,
                previous,
            }
        }
    }

    struct GeminiPathOverride {
        _guard: std::sync::MutexGuard<'static, ()>,
        previous: Option<OsString>,
    }

    impl GeminiPathOverride {
        fn new(path: &std::path::Path) -> Self {
            let guard = crate::services::discord::runtime_store::lock_test_env();
            let previous = std::env::var_os("AGENTDESK_GEMINI_PATH");
            unsafe { std::env::set_var("AGENTDESK_GEMINI_PATH", path) };
            Self {
                _guard: guard,
                previous,
            }
        }
    }

    impl Drop for GeminiPathOverride {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_GEMINI_PATH", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_GEMINI_PATH") },
            }
        }
    }

    struct GeminiStreamingEnvOverride {
        _guard: std::sync::MutexGuard<'static, ()>,
        previous_gemini_path: Option<OsString>,
        previous_home: Option<OsString>,
    }

    impl GeminiStreamingEnvOverride {
        fn new(gemini_path: &std::path::Path, trusted_dir: &std::path::Path) -> Self {
            let guard = crate::services::discord::runtime_store::lock_test_env();
            let previous_gemini_path = std::env::var_os("AGENTDESK_GEMINI_PATH");
            let previous_home = std::env::var_os("HOME");
            let gemini_config_dir = trusted_dir.join(".gemini");
            fs::create_dir_all(&gemini_config_dir).unwrap();
            let trusted_folders = serde_json::json!({
                trusted_dir.display().to_string(): "TRUST_FOLDER"
            });
            fs::write(
                gemini_config_dir.join("trustedFolders.json"),
                serde_json::to_vec(&trusted_folders).unwrap(),
            )
            .unwrap();
            unsafe {
                std::env::set_var("AGENTDESK_GEMINI_PATH", gemini_path);
                std::env::set_var("HOME", trusted_dir);
            }
            Self {
                _guard: guard,
                previous_gemini_path,
                previous_home,
            }
        }
    }

    impl Drop for GeminiStreamingEnvOverride {
        fn drop(&mut self) {
            match self.previous_gemini_path.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_GEMINI_PATH", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_GEMINI_PATH") },
            }
            match self.previous_home.take() {
                Some(value) => unsafe { std::env::set_var("HOME", value) },
                None => unsafe { std::env::remove_var("HOME") },
            }
        }
    }

    impl Drop for RuntimeRootOverride {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    struct RepoAndRuntimeOverride {
        _guard: std::sync::MutexGuard<'static, ()>,
        previous_repo: Option<OsString>,
        previous_root: Option<OsString>,
    }

    impl RepoAndRuntimeOverride {
        fn new(repo_path: &std::path::Path, runtime_root: &std::path::Path) -> Self {
            let guard = lock_repo_dir_env();
            let previous_repo = std::env::var_os("AGENTDESK_REPO_DIR");
            let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
            unsafe {
                std::env::set_var("AGENTDESK_REPO_DIR", repo_path);
                std::env::set_var("AGENTDESK_ROOT_DIR", runtime_root);
            }
            Self {
                _guard: guard,
                previous_repo,
                previous_root,
            }
        }
    }

    impl Drop for RepoAndRuntimeOverride {
        fn drop(&mut self) {
            match self.previous_repo.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_REPO_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_REPO_DIR") },
            }
            match self.previous_root.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
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

    fn setup_test_repo() -> (tempfile::TempDir, RepoDirOverride) {
        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "-b", "main"]);
        run_git(repo.path(), &["config", "user.email", "test@test.com"]);
        run_git(repo.path(), &["config", "user.name", "Test"]);
        run_git(repo.path(), &["commit", "--allow-empty", "-m", "initial"]);
        let override_guard = RepoDirOverride::new(repo.path());
        (repo, override_guard)
    }

    fn setup_test_repo_with_origin() -> (tempfile::TempDir, tempfile::TempDir, RepoDirOverride) {
        let remote = tempfile::tempdir().unwrap();
        run_git(remote.path(), &["init", "--bare", "--initial-branch=main"]);

        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "-b", "main"]);
        run_git(repo.path(), &["config", "user.email", "test@test.com"]);
        run_git(repo.path(), &["config", "user.name", "Test"]);
        run_git(
            repo.path(),
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        run_git(repo.path(), &["commit", "--allow-empty", "-m", "initial"]);
        run_git(repo.path(), &["push", "-u", "origin", "main"]);

        let override_guard = RepoDirOverride::new(repo.path());
        (repo, remote, override_guard)
    }

    fn setup_test_repo_with_mock_gh(
        replies: &[MockGhReply],
    ) -> (tempfile::TempDir, RepoAndMockGhEnv) {
        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "-b", "main"]);
        run_git(repo.path(), &["config", "user.email", "test@test.com"]);
        run_git(repo.path(), &["config", "user.name", "Test"]);
        run_git(repo.path(), &["commit", "--allow-empty", "-m", "initial"]);

        let lock = lock_repo_dir_env();
        let previous_repo_dir = std::env::var_os("AGENTDESK_REPO_DIR");
        unsafe { std::env::set_var("AGENTDESK_REPO_DIR", repo.path()) };

        let gh = install_mock_gh_with_lock(lock, replies);
        (
            repo,
            RepoAndMockGhEnv {
                _gh: gh,
                previous_repo_dir,
            },
        )
    }

    fn setup_test_repo_with_origin_and_mock_gh(
        replies: &[MockGhReply],
    ) -> (tempfile::TempDir, tempfile::TempDir, RepoAndMockGhEnv) {
        let remote = tempfile::tempdir().unwrap();
        run_git(remote.path(), &["init", "--bare", "--initial-branch=main"]);

        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "-b", "main"]);
        run_git(repo.path(), &["config", "user.email", "test@test.com"]);
        run_git(repo.path(), &["config", "user.name", "Test"]);
        run_git(
            repo.path(),
            &["remote", "add", "origin", remote.path().to_str().unwrap()],
        );
        run_git(repo.path(), &["commit", "--allow-empty", "-m", "initial"]);
        run_git(repo.path(), &["push", "-u", "origin", "main"]);

        let lock = lock_repo_dir_env();
        let previous_repo_dir = std::env::var_os("AGENTDESK_REPO_DIR");
        unsafe { std::env::set_var("AGENTDESK_REPO_DIR", repo.path()) };

        let gh = install_mock_gh_with_lock(lock, replies);
        (
            repo,
            remote,
            RepoAndMockGhEnv {
                _gh: gh,
                previous_repo_dir,
            },
        )
    }

    fn setup_test_repo_with_runtime_root()
    -> (tempfile::TempDir, tempfile::TempDir, RepoAndRuntimeOverride) {
        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "-b", "main"]);
        run_git(repo.path(), &["config", "user.email", "test@test.com"]);
        run_git(repo.path(), &["config", "user.name", "Test"]);
        run_git(repo.path(), &["commit", "--allow-empty", "-m", "initial"]);

        let runtime_root = tempfile::tempdir().unwrap();
        fs::create_dir_all(runtime_root.path().join("runtime")).unwrap();

        let override_guard = RepoAndRuntimeOverride::new(repo.path(), runtime_root.path());
        (repo, runtime_root, override_guard)
    }

    fn seed_agent(db: &db::Db) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) \
             VALUES ('agent-1', 'Test Agent', '111', '222')",
            [],
        )
        .unwrap();
    }

    fn seed_card(db: &db::Db, card_id: &str, status: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at) \
             VALUES (?1, 'Test Card', ?2, 'agent-1', datetime('now'), datetime('now'))",
            libsql_rusqlite::params![card_id, status],
        )
        .unwrap();
    }

    fn set_config_key(db: &db::Db, key: &str, value: serde_json::Value) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
            libsql_rusqlite::params![key, value.to_string()],
        )
        .unwrap();
    }

    fn seed_dispatch(db: &db::Db, dispatch_id: &str, card_id: &str, dtype: &str, status: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
             VALUES (?1, ?2, 'agent-1', ?3, ?4, 'Test Dispatch', datetime('now'), datetime('now'))",
            libsql_rusqlite::params![dispatch_id, card_id, dtype, status],
        )
        .unwrap();
        conn.execute(
            "UPDATE kanban_cards SET latest_dispatch_id = ?1 WHERE id = ?2",
            libsql_rusqlite::params![dispatch_id, card_id],
        )
        .unwrap();
    }

    fn seed_assistant_response_for_dispatch(db: &db::Db, dispatch_id: &str, message: &str) {
        crate::db::session_transcripts::persist_turn(
            db,
            crate::db::session_transcripts::PersistSessionTranscript {
                turn_id: &format!("integration-test:{dispatch_id}"),
                session_key: Some("integration-test-session"),
                channel_id: Some("111"),
                agent_id: Some("agent-1"),
                provider: Some("codex"),
                dispatch_id: Some(dispatch_id),
                user_message: "Implement the task",
                assistant_message: message,
                events: &[],
                duration_ms: None,
            },
        )
        .unwrap();
    }

    fn seed_completed_work_dispatch_for_review(
        db: &db::Db,
        dispatch_id: &str,
        card_id: &str,
        dispatch_type: &str,
    ) {
        let repo_dir = crate::services::platform::resolve_repo_dir()
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|path| path.display().to_string())
            })
            .unwrap();
        let reviewed_commit = crate::services::platform::git_head_commit(&repo_dir)
            .unwrap_or_else(|| "ci-detached-head".to_string());
        let completed_branch = crate::services::platform::shell::git_branch_name(&repo_dir);

        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                result, created_at, updated_at, completed_at
            ) VALUES (
                ?1, ?2, 'agent-1', ?3, 'completed', 'Completed work',
                ?4, datetime('now', '-5 minutes'), datetime('now', '-5 minutes'), datetime('now', '-5 minutes')
            )",
            libsql_rusqlite::params![
                dispatch_id,
                card_id,
                dispatch_type,
                serde_json::json!({
                    "completed_worktree_path": repo_dir,
                    "completed_branch": completed_branch,
                    "completed_commit": reviewed_commit,
                })
                .to_string(),
            ],
        )
        .unwrap();
    }

    fn seed_completed_work_dispatch_target(
        db: &db::Db,
        dispatch_id: &str,
        card_id: &str,
        dispatch_type: &str,
        worktree_path: &str,
        branch: &str,
        commit: &str,
    ) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                result, created_at, updated_at, completed_at
            ) VALUES (
                ?1, ?2, 'agent-1', ?3, 'completed', 'Completed work',
                ?4, datetime('now', '-5 minutes'), datetime('now', '-5 minutes'), datetime('now', '-5 minutes')
            )",
            libsql_rusqlite::params![
                dispatch_id,
                card_id,
                dispatch_type,
                serde_json::json!({
                    "completed_worktree_path": worktree_path,
                    "completed_branch": branch,
                    "completed_commit": commit,
                })
                .to_string(),
            ],
        )
        .unwrap();
    }

    fn seed_completed_review_dispatch(
        db: &db::Db,
        dispatch_id: &str,
        card_id: &str,
        verdict: &str,
    ) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                result, created_at, updated_at, completed_at
            ) VALUES (
                ?1, ?2, 'agent-1', 'review', 'completed', 'Completed review',
                ?3, datetime('now', '-1 minutes'), datetime('now', '-1 minutes'), datetime('now', '-1 minutes')
            )",
            libsql_rusqlite::params![
                dispatch_id,
                card_id,
                serde_json::json!({
                    "verdict": verdict,
                })
                .to_string(),
            ],
        )
        .unwrap();
    }

    fn seed_repo(db: &db::Db, repo_id: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO github_repos (id, display_name, sync_enabled) VALUES (?1, ?1, 1)",
            [repo_id],
        )
        .unwrap();
    }

    fn seed_card_with_repo(
        db: &db::Db,
        card_id: &str,
        status: &str,
        repo_id: &str,
        issue_number: i64,
        active_thread_id: Option<&str>,
    ) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO kanban_cards \
             (id, title, status, assigned_agent_id, repo_id, github_issue_number, github_issue_url, active_thread_id, created_at, updated_at) \
             VALUES (?1, 'Codex Card', ?2, 'agent-1', ?3, ?4, ?5, ?6, datetime('now'), datetime('now'))",
            libsql_rusqlite::params![
                card_id,
                status,
                repo_id,
                issue_number,
                format!("https://github.com/{repo_id}/issues/{issue_number}"),
                active_thread_id
            ],
        )
        .unwrap();
    }

    fn seed_thread_session(db: &db::Db, session_key: &str, thread_channel_id: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (session_key, agent_id, provider, status, thread_channel_id, last_heartbeat) \
             VALUES (?1, 'agent-1', 'codex', 'idle', ?2, datetime('now'))",
            libsql_rusqlite::params![session_key, thread_channel_id],
        )
        .unwrap();
    }

    fn seed_worktree_session(db: &db::Db, session_key: &str, cwd: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO sessions (session_key, agent_id, provider, status, cwd, last_heartbeat) \
             VALUES (?1, 'agent-1', 'codex', 'working', ?2, datetime('now'))",
            libsql_rusqlite::params![session_key, cwd],
        )
        .unwrap();
    }

    fn set_kv(db: &db::Db, key: &str, value: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
            libsql_rusqlite::params![key, value],
        )
        .unwrap();
    }

    fn kv_value(db: &db::Db, key: &str) -> Option<String> {
        let conn = db.lock().unwrap();
        conn.query_row("SELECT value FROM kv_meta WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .ok()
    }

    #[derive(Clone)]
    struct TestLogWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    fn test_log_buffer() -> &'static Arc<Mutex<Vec<u8>>> {
        static BUFFER: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
        BUFFER.get_or_init(|| Arc::new(Mutex::new(Vec::new())))
    }

    fn ensure_test_log_capture() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::INFO)
                .with_ansi(false)
                .without_time()
                .with_writer(|| TestLogWriter {
                    buffer: test_log_buffer().clone(),
                })
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("global tracing subscriber should initialize once");
        });
    }

    fn test_log_capture_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    impl Write for TestLogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture_policy_logs<T>(run: impl FnOnce() -> T) -> (T, String) {
        ensure_test_log_capture();
        let _guard = test_log_capture_lock().lock().unwrap();
        let buffer = test_log_buffer().clone();
        buffer.lock().unwrap().clear();
        let result = run();
        let captured = buffer.lock().unwrap().clone();
        (result, String::from_utf8_lossy(&captured).to_string())
    }

    fn set_phase_gate_state(
        db: &db::Db,
        run_id: &str,
        phase: i64,
        status: &str,
        dispatch_ids: &[&str],
        next_phase: Option<i64>,
        final_phase: bool,
        anchor_card_id: Option<&str>,
        verdict: Option<&str>,
        failure_reason: Option<&str>,
    ) {
        let conn = db.lock().unwrap();
        let final_phase = if final_phase { 1 } else { 0 };
        if dispatch_ids.is_empty() {
            conn.execute(
                "INSERT INTO auto_queue_phase_gates (
                    run_id, phase, status, verdict, dispatch_id, pass_verdict,
                    next_phase, final_phase, anchor_card_id, failure_reason
                ) VALUES (?1, ?2, ?3, ?4, NULL, 'phase_gate_passed', ?5, ?6, ?7, ?8)",
                libsql_rusqlite::params![
                    run_id,
                    phase,
                    status,
                    verdict,
                    next_phase,
                    final_phase,
                    anchor_card_id,
                    failure_reason,
                ],
            )
            .unwrap();
            return;
        }

        for dispatch_id in dispatch_ids {
            conn.execute(
                "INSERT INTO auto_queue_phase_gates (
                    run_id, phase, status, verdict, dispatch_id, pass_verdict,
                    next_phase, final_phase, anchor_card_id, failure_reason
                ) VALUES (?1, ?2, ?3, ?4, ?5, 'phase_gate_passed', ?6, ?7, ?8, ?9)",
                libsql_rusqlite::params![
                    run_id,
                    phase,
                    status,
                    verdict,
                    dispatch_id,
                    next_phase,
                    final_phase,
                    anchor_card_id,
                    failure_reason,
                ],
            )
            .unwrap();
        }
    }

    fn phase_gate_state(db: &db::Db, run_id: &str, phase: i64) -> Option<serde_json::Value> {
        let conn = db.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT dispatch_id, status, verdict, next_phase, final_phase, anchor_card_id, failure_reason
                 FROM auto_queue_phase_gates
                 WHERE run_id = ?1 AND phase = ?2
                 ORDER BY dispatch_id ASC",
            )
            .unwrap();
        let rows = stmt
            .query_map(libsql_rusqlite::params![run_id, phase], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        drop(stmt);
        drop(conn);

        if rows.is_empty() {
            return None;
        }

        let failed = rows.iter().find(|row| row.1 == "failed");
        let dispatch_ids = rows
            .iter()
            .filter_map(|row| row.0.clone())
            .collect::<Vec<_>>();
        let status = if failed.is_some() {
            "failed"
        } else if !dispatch_ids.is_empty() && rows.iter().all(|row| row.1 == "passed") {
            "passed"
        } else {
            rows[0].1.as_str()
        };

        let mut value = serde_json::json!({
            "run_id": run_id,
            "batch_phase": phase,
            "next_phase": rows[0].3,
            "final_phase": rows[0].4 != 0,
            "anchor_card_id": rows[0].5,
            "status": status,
            "dispatch_ids": dispatch_ids,
        });
        if let Some(failed_row) = failed {
            value["failed_dispatch_id"] = serde_json::json!(failed_row.0);
            value["failed_verdict"] = serde_json::json!(failed_row.2);
            value["failed_reason"] = serde_json::json!(failed_row.6);
        }
        Some(value)
    }

    fn escalation_pending_reasons(db: &db::Db, card_id: &str) -> Vec<String> {
        kv_value(db, &format!("pm_pending:{card_id}"))
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|value| {
                value["reasons"].as_array().map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                })
            })
            .unwrap_or_default()
    }

    fn escalation_last_request(db: &db::Db) -> serde_json::Value {
        let raw = kv_value(db, "test_http_last").expect("test_http_last must exist");
        serde_json::from_str(&raw).expect("test_http_last must be valid JSON")
    }

    fn relative_local_time(minutes_ago: i64) -> String {
        (chrono::Local::now() - chrono::Duration::minutes(minutes_ago))
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    fn setup_timeouts_policy_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
        fs::copy(
            source_dir.join("00-escalation.js"),
            dir.path().join("00-escalation.js"),
        )
        .unwrap();
        fs::copy(
            source_dir.join("timeouts.js"),
            dir.path().join("timeouts.js"),
        )
        .unwrap();
        fs::write(
            dir.path().join("zz-timeouts-test-overrides.js"),
            r#"
            agentdesk.http.post = function(url, body) {
                var countRows = agentdesk.db.query(
                    "SELECT value FROM kv_meta WHERE key = ?1",
                    ["test_http_count"]
                );
                var nextCount = countRows.length > 0
                    ? (parseInt(countRows[0].value, 10) || 0) + 1
                    : 1;
                agentdesk.db.execute(
                    "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                    ["test_http_count", "" + nextCount]
                );
                agentdesk.db.execute(
                    "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                    ["test_http_last", JSON.stringify({ url: url, body: body })]
                );
                if (url.indexOf("/force-kill") !== -1) {
                    var tmuxKilledRows = agentdesk.db.query(
                        "SELECT value FROM kv_meta WHERE key = ?1",
                        ["test_force_kill_tmux_killed"]
                    );
                    var tmuxKilled = true;
                    if (tmuxKilledRows.length > 0) {
                        var raw = (tmuxKilledRows[0].value || "").toLowerCase();
                        tmuxKilled = !(raw === "0" || raw === "false" || raw === "no");
                    }
                    return {
                        ok: true,
                        tmux_killed: tmuxKilled
                    };
                }
                return {
                    ok: true,
                    new_deadline_ms: Date.now() + (((body && body.extend_secs) || 0) * 1000)
                };
            };
            var rawExec = agentdesk.exec;
            agentdesk.exec = function(cmd, args) {
                if (cmd === "tmux" && args && args[0] === "list-panes") {
                    return "0";
                }
                return rawExec(cmd, args);
            };
            agentdesk.registerPolicy({
                name: "timeouts-test-overrides",
                priority: 9999
            });
            "#,
        )
        .unwrap();
        dir
    }

    fn setup_triage_policy_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("policies")
            .join("triage-rules.js");
        fs::copy(&source, dir.path().join("triage-rules.js")).unwrap();
        dir
    }

    fn setup_escalation_policy_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("policies")
            .join("00-escalation.js");
        fs::copy(&source, dir.path().join("00-escalation.js")).unwrap();
        fs::write(
            dir.path().join("zz-escalation-test-overrides.js"),
            r#"
            agentdesk.http.post = function(url, body) {
                var countRows = agentdesk.db.query(
                    "SELECT value FROM kv_meta WHERE key = ?1",
                    ["test_http_count"]
                );
                var nextCount = countRows.length > 0
                    ? (parseInt(countRows[0].value, 10) || 0) + 1
                    : 1;
                agentdesk.db.execute(
                    "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                    ["test_http_count", "" + nextCount]
                );
                agentdesk.db.execute(
                    "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                    ["test_http_last", JSON.stringify({ url: url, body: body })]
                );
                return { ok: true };
            };
            agentdesk.registerPolicy({
                name: "escalation-test-overrides",
                priority: 9999
            });
            "#,
        )
        .unwrap();
        dir
    }

    fn setup_auto_queue_activate_spy_policy_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");

        for entry in fs::read_dir(&source_dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("js") {
                continue;
            }
            fs::copy(&path, dir.path().join(entry.file_name())).unwrap();
        }

        fs::write(
            dir.path().join("zz-auto-queue-activate-spy.js"),
            r#"
            var rawActivate = agentdesk.autoQueue.activate;
            agentdesk.autoQueue.activate = function(runIdOrBody, threadGroup) {
                var body;
                if (runIdOrBody && typeof runIdOrBody === "object" && !Array.isArray(runIdOrBody)) {
                    body = Object.assign({}, runIdOrBody);
                } else {
                    body = {
                        run_id: runIdOrBody || null,
                        active_only: true
                    };
                    if (threadGroup !== null && threadGroup !== undefined) {
                        body.thread_group = threadGroup;
                    }
                }
                if (body.active_only === undefined) {
                    body.active_only = true;
                }
                var countRows = agentdesk.db.query(
                    "SELECT value FROM kv_meta WHERE key = ?1",
                    ["test_auto_queue_activate_count"]
                );
                var nextCount = countRows.length > 0
                    ? (parseInt(countRows[0].value, 10) || 0) + 1
                    : 1;
                agentdesk.db.execute(
                    "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                    ["test_auto_queue_activate_count", "" + nextCount]
                );
                agentdesk.db.execute(
                    "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                    ["test_auto_queue_activate_last", JSON.stringify(body)]
                );
                return rawActivate(body);
            };
            agentdesk.registerPolicy({
                name: "auto-queue-activate-spy",
                priority: 9999
            });
            "#,
        )
        .unwrap();
        dir
    }

    fn setup_merge_policy_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let source_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
        fs::copy(
            source_dir.join("00-pr-tracking.js"),
            dir.path().join("00-pr-tracking.js"),
        )
        .unwrap();
        fs::copy(
            source_dir.join("merge-automation.js"),
            dir.path().join("merge-automation.js"),
        )
        .unwrap();
        dir
    }

    fn write_codex_inflight(
        runtime_root: &std::path::Path,
        channel_id: &str,
        started_at: &str,
        updated_at: &str,
        session_key: &str,
        tmux_name: &str,
        dispatch_id: Option<&str>,
    ) {
        let inflight_dir = runtime_root.join("runtime/discord_inflight/codex");
        fs::create_dir_all(&inflight_dir).unwrap();
        fs::write(
            inflight_dir.join(format!("{channel_id}.json")),
            serde_json::to_string(&json!({
                "provider": "codex",
                "channel_id": channel_id,
                "channel_name": "Test Channel",
                "tmux_session_name": tmux_name,
                "started_at": started_at,
                "updated_at": updated_at,
                "session_key": session_key,
                "dispatch_id": dispatch_id,
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn seed_pr_tracking(
        db: &db::Db,
        card_id: &str,
        repo_id: &str,
        worktree_path: Option<&str>,
        branch: &str,
        pr_number: Option<i64>,
        head_sha: Option<&str>,
        state: &str,
    ) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO pr_tracking \
             (card_id, repo_id, worktree_path, branch, pr_number, head_sha, state, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, datetime('now'), datetime('now'))",
            libsql_rusqlite::params![card_id, repo_id, worktree_path, branch, pr_number, head_sha, state],
        )
        .unwrap();
    }

    /// #743: Seed a create-pr dispatch + pr_tracking row with matching
    /// dispatch_generation stamps so success-path stale guard in
    /// onDispatchCompleted accepts the completion.
    fn seed_stamped_create_pr_state(
        db: &db::Db,
        dispatch_id: &str,
        card_id: &str,
        repo_id: &str,
        worktree_path: Option<&str>,
        branch: &str,
        pr_number: Option<i64>,
        head_sha: Option<&str>,
        state: &str,
        status: &str,
    ) -> String {
        let generation = uuid::Uuid::new_v4().to_string();
        let context = serde_json::json!({
            "dispatch_generation": generation,
            "review_round_at_dispatch": 0,
            "sidecar_dispatch": true,
            "worktree_path": worktree_path,
            "worktree_branch": branch,
            "branch": branch,
        })
        .to_string();
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches \
             (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at) \
             VALUES (?1, ?2, 'agent-1', 'create-pr', ?3, 'Test Create PR', ?4, datetime('now'), datetime('now'))",
            libsql_rusqlite::params![dispatch_id, card_id, status, context],
        )
        .unwrap();
        conn.execute(
            "UPDATE kanban_cards SET latest_dispatch_id = ?1 WHERE id = ?2",
            libsql_rusqlite::params![dispatch_id, card_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO pr_tracking \
             (card_id, repo_id, worktree_path, branch, pr_number, head_sha, state, \
              dispatch_generation, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, datetime('now'), datetime('now'))",
            libsql_rusqlite::params![
                card_id,
                repo_id,
                worktree_path,
                branch,
                pr_number,
                head_sha,
                state,
                generation,
            ],
        )
        .unwrap();
        generation
    }

    fn pr_tracking_state(db: &db::Db, card_id: &str) -> Option<String> {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT state FROM pr_tracking WHERE card_id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .ok()
    }

    fn pr_tracking_branch(db: &db::Db, card_id: &str) -> Option<String> {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT branch FROM pr_tracking WHERE card_id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .ok()
    }

    fn pr_tracking_pr_number(db: &db::Db, card_id: &str) -> Option<i64> {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT pr_number FROM pr_tracking WHERE card_id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .ok()
    }

    fn pr_tracking_last_error(db: &db::Db, card_id: &str) -> Option<String> {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT last_error FROM pr_tracking WHERE card_id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .ok()
    }

    fn count_dispatches_by_type(db: &db::Db, card_id: &str, dispatch_type: &str) -> i64 {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = ?1 AND dispatch_type = ?2",
            libsql_rusqlite::params![card_id, dispatch_type],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn count_active_dispatches_by_type(db: &db::Db, card_id: &str, dispatch_type: &str) -> i64 {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM task_dispatches \
             WHERE kanban_card_id = ?1 AND dispatch_type = ?2 \
             AND status IN ('pending', 'dispatched')",
            libsql_rusqlite::params![card_id, dispatch_type],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn latest_dispatch_title(db: &db::Db, card_id: &str, dispatch_type: &str) -> Option<String> {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT title FROM task_dispatches WHERE kanban_card_id = ?1 AND dispatch_type = ?2 ORDER BY created_at DESC, id DESC LIMIT 1",
            libsql_rusqlite::params![card_id, dispatch_type],
            |row| row.get(0),
        )
        .ok()
    }

    fn review_state_value(db: &db::Db, card_id: &str) -> Option<String> {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT state FROM card_review_state WHERE card_id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .ok()
    }

    fn message_outbox_rows(db: &db::Db) -> Vec<(String, String)> {
        let conn = db.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT target, content FROM message_outbox ORDER BY id ASC")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    struct MockGhEnv {
        _lock: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
        old_path: Option<OsString>,
        old_gh_path: Option<OsString>,
        log_path: PathBuf,
    }

    struct RepoAndMockGhEnv {
        _gh: MockGhEnv,
        previous_repo_dir: Option<OsString>,
    }

    struct MockGhReply {
        key: &'static str,
        contains: Option<&'static str>,
        stdout: &'static str,
    }

    impl Drop for RepoAndMockGhEnv {
        fn drop(&mut self) {
            match self.previous_repo_dir.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_REPO_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_REPO_DIR") },
            }
        }
    }

    impl Drop for MockGhEnv {
        fn drop(&mut self) {
            if let Some(old_path) = &self.old_path {
                unsafe {
                    std::env::set_var("PATH", old_path);
                }
            } else {
                unsafe {
                    std::env::remove_var("PATH");
                }
            }
            if let Some(old_gh_path) = &self.old_gh_path {
                unsafe {
                    std::env::set_var("AGENTDESK_GH_PATH", old_gh_path);
                }
            } else {
                unsafe {
                    std::env::remove_var("AGENTDESK_GH_PATH");
                }
            }
        }
    }

    #[cfg(unix)]
    fn build_mock_gh_script(replies: &[MockGhReply]) -> String {
        let mut script = String::from(
            "#!/bin/sh\nset -eu\nlog_file=\"$(dirname \"$0\")/gh.log\"\nprintf '%s\\n' \"$*\" >> \"$log_file\"\nif [ \"${1-}\" = \"--version\" ]; then\n  echo 'gh mock 1.0'\n  exit 0\nfi\nkey=\"${1-}:${2-}\"\nargs=\"$*\"\n",
        );

        for reply in replies {
            script.push_str(&format!("if [ \"$key\" = '{}' ]", reply.key));
            if let Some(token) = reply.contains {
                script.push_str(&format!(
                    " && printf '%s\\n' \"$args\" | grep -F -q -- '{}'",
                    token
                ));
            }
            script.push_str("; then\n");
            script.push_str("cat <<'JSON'\n");
            script.push_str(reply.stdout);
            script.push_str("\nJSON\nexit 0\nfi\n");
        }

        script.push_str("echo '[]'\n");
        script
    }

    #[cfg(unix)]
    fn write_executable_script(path: &std::path::Path, contents: &str) {
        fs::write(path, contents).unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    #[cfg(windows)]
    fn build_mock_gh_script(replies: &[MockGhReply]) -> (String, String) {
        let wrapper =
            "@echo off\r\npwsh -NoProfile -ExecutionPolicy Bypass -File \"%~dp0gh.ps1\" %*\r\n"
                .to_string();

        let mut script = String::from(
            "$LogFile = Join-Path $PSScriptRoot 'gh.log'\nAdd-Content -Path $LogFile -Value ($args -join ' ')\nif ($args.Count -gt 0 -and $args[0] -eq '--version') {\n  Write-Output 'gh mock 1.0'\n  exit 0\n}\n$key = if ($args.Count -ge 2) { \"$($args[0]):$($args[1])\" } elseif ($args.Count -eq 1) { \"$($args[0]):\" } else { ':' }\n$joined = $args -join ' '\n",
        );

        for reply in replies {
            script.push_str(&format!("if ($key -eq '{}'", reply.key.replace('\'', "''")));
            if let Some(token) = reply.contains {
                script.push_str(&format!(
                    " -and $joined.Contains('{}')",
                    token.replace('\'', "''")
                ));
            }
            script.push_str(") {\n");
            script.push_str("@'\n");
            script.push_str(reply.stdout);
            script.push_str("\n'@ | Write-Output\nexit 0\n}\n");
        }

        script.push_str("'[]' | Write-Output\n");
        (wrapper, script)
    }

    fn install_mock_gh_with_lock(
        lock: std::sync::MutexGuard<'static, ()>,
        replies: &[MockGhReply],
    ) -> MockGhEnv {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join("gh.log");
        #[cfg(unix)]
        {
            let gh_path = dir.path().join("gh");
            let script = build_mock_gh_script(replies);
            fs::write(&gh_path, script).unwrap();
            let mut perms = fs::metadata(&gh_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&gh_path, perms).unwrap();
            let old_path = std::env::var_os("PATH");
            let old_gh_path = std::env::var_os("AGENTDESK_GH_PATH");
            let joined = match &old_path {
                Some(existing) => std::env::join_paths(
                    std::iter::once(dir.path().to_path_buf())
                        .chain(std::env::split_paths(existing)),
                )
                .unwrap(),
                None => std::env::join_paths([dir.path()]).unwrap(),
            };
            unsafe {
                std::env::set_var("PATH", joined);
                std::env::set_var("AGENTDESK_GH_PATH", &gh_path);
            }

            return MockGhEnv {
                _lock: lock,
                _dir: dir,
                old_path,
                old_gh_path,
                log_path,
            };
        }

        #[cfg(windows)]
        {
            let gh_ps1_path = dir.path().join("gh.ps1");
            let (_wrapper, script) = build_mock_gh_script(replies);
            fs::write(&gh_ps1_path, script).unwrap();

            let old_path = std::env::var_os("PATH");
            let old_gh_path = std::env::var_os("AGENTDESK_GH_PATH");
            let joined = match &old_path {
                Some(existing) => std::env::join_paths(
                    std::iter::once(dir.path().to_path_buf())
                        .chain(std::env::split_paths(existing)),
                )
                .unwrap(),
                None => std::env::join_paths([dir.path()]).unwrap(),
            };
            unsafe {
                std::env::set_var("PATH", joined);
                std::env::set_var("AGENTDESK_GH_PATH", &gh_ps1_path);
            }

            return MockGhEnv {
                _lock: lock,
                _dir: dir,
                old_path,
                old_gh_path,
                log_path,
            };
        }
    }

    fn install_mock_gh(replies: &[MockGhReply]) -> MockGhEnv {
        let lock = crate::services::discord::runtime_store::lock_test_env();
        install_mock_gh_with_lock(lock, replies)
    }

    fn gh_log(env: &MockGhEnv) -> String {
        fs::read_to_string(&env.log_path).unwrap_or_default()
    }

    fn ensure_auto_queue_tables(db: &db::Db) {
        let conn = db.lock().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS auto_queue_runs (
                id          TEXT PRIMARY KEY,
                repo        TEXT,
                agent_id    TEXT,
                status      TEXT DEFAULT 'active',
                ai_model    TEXT,
                ai_rationale TEXT,
                timeout_minutes INTEGER DEFAULT 120,
                unified_thread  INTEGER DEFAULT 0,
                unified_thread_id TEXT,
                unified_thread_channel_id TEXT,
                max_concurrent_threads INTEGER DEFAULT 1,
                thread_group_count INTEGER DEFAULT 1,
                created_at  DATETIME DEFAULT CURRENT_TIMESTAMP,
                completed_at DATETIME
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
                thread_group    INTEGER DEFAULT 0,
                created_at      DATETIME DEFAULT CURRENT_TIMESTAMP,
                dispatched_at   DATETIME,
                completed_at    DATETIME
            );
            CREATE TABLE IF NOT EXISTS auto_queue_entry_dispatch_history (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                entry_id        TEXT NOT NULL,
                dispatch_id     TEXT NOT NULL,
                trigger_source  TEXT,
                created_at      DATETIME DEFAULT CURRENT_TIMESTAMP,
                UNIQUE(entry_id, dispatch_id)
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
                ON auto_queue_phase_gates(dispatch_id);
            ",
        )
        .unwrap();
    }

    fn get_card_status(db: &db::Db, card_id: &str) -> String {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT status FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn get_dispatch_status(db: &db::Db, dispatch_id: &str) -> String {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT status FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .unwrap()
    }

    // ── Scenario 1: Implementation idle does not complete (#115) ────

    #[tokio::test]
    async fn scenario_1_implementation_idle_does_not_complete() {
        let db = test_db();
        seed_agent(&db);
        seed_card(&db, "card-s1", "in_progress");
        seed_dispatch(&db, "d-s1", "card-s1", "implementation", "pending");

        let state = AppState {
            db: db.clone(),
            pg_pool: None,
            engine: test_engine(&db),
            config: std::sync::Arc::new(crate::config::Config::default()),
            broadcast_tx: crate::server::ws::new_broadcast(),
            batch_buffer: crate::server::ws::spawn_batch_flusher(crate::server::ws::new_broadcast()),
            health_registry: None,
        };

        let (status, _) = crate::server::routes::dispatched_sessions::hook_session(
            axum::extract::State(state),
            axum::Json(
                crate::server::routes::dispatched_sessions::HookSessionBody {
                    session_key: "test-session".to_string(),
                    agent_id: None,
                    status: Some("idle".to_string()),
                    provider: Some("claude".to_string()),
                    session_info: None,
                    name: None,
                    model: None,
                    tokens: None,
                    cwd: None,
                    dispatch_id: Some("d-s1".to_string()),
                    claude_session_id: None,
                    thread_channel_id: None,
                    session_id: None,
                },
            ),
        )
        .await;

        assert_eq!(status, axum::http::StatusCode::OK);

        // Implementation dispatch must NOT be auto-completed by idle
        let d_status = get_dispatch_status(&db, "d-s1");
        assert_eq!(
            d_status, "pending",
            "implementation dispatch must NOT be completed by idle heartbeat"
        );
    }

    // ── Scenario 2: Single active review-decision per card (#116) ───

    #[test]
    fn scenario_2_single_active_review_decision_per_card() {
        let db = test_db();
        seed_agent(&db);
        seed_card(&db, "card-s2", "review");

        let r1 = dispatch::create_dispatch_core(
            &db,
            "card-s2",
            "agent-1",
            "review-decision",
            "[RD1]",
            &serde_json::json!({"verdict": "improve"}),
        );
        assert!(r1.is_ok(), "first review-decision should succeed");

        let r2 = dispatch::create_dispatch_core(
            &db,
            "card-s2",
            "agent-1",
            "review-decision",
            "[RD2]",
            &serde_json::json!({"verdict": "rework"}),
        );
        assert!(r2.is_ok(), "second review-decision should succeed");

        let conn = db.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches \
                 WHERE kanban_card_id = 'card-s2' AND dispatch_type = 'review-decision' \
                 AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "exactly 1 active review-decision per card");

        let r1_id = r1.unwrap().0;
        let r1_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = ?1",
                [&r1_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            r1_status, "cancelled",
            "first review-decision should be cancelled"
        );
    }

    // ── Scenario 4: Card status full cycle ──────────────────────────

    #[test]
    fn scenario_4_card_status_full_cycle() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-s4", "backlog");

        // backlog → ready
        assert!(kanban::transition_status(&db, &engine, "card-s4", "ready").is_ok());
        assert_eq!(get_card_status(&db, "card-s4"), "ready");

        // ready → requested (free transition, no dispatch needed — #255 preflight state)
        assert!(kanban::transition_status(&db, &engine, "card-s4", "requested").is_ok());
        assert_eq!(get_card_status(&db, "card-s4"), "requested");

        // requested → in_progress (needs dispatch — gated transition)
        seed_dispatch(&db, "d-s4-impl", "card-s4", "implementation", "pending");
        assert!(kanban::transition_status(&db, &engine, "card-s4", "in_progress").is_ok());
        assert_eq!(get_card_status(&db, "card-s4"), "in_progress");

        // Verify started_at
        {
            let conn = db.lock().unwrap();
            let started_at: Option<String> = conn
                .query_row(
                    "SELECT started_at FROM kanban_cards WHERE id = 'card-s4'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(started_at.is_some(), "started_at must be set");
        }

        // in_progress → review
        assert!(kanban::transition_status(&db, &engine, "card-s4", "review").is_ok());
        assert_eq!(get_card_status(&db, "card-s4"), "review");

        // review → done (force)
        assert!(
            kanban::transition_status_with_opts(&db, &engine, "card-s4", "done", "test", true)
                .is_ok()
        );
        assert_eq!(get_card_status(&db, "card-s4"), "done");

        // Verify done cleanup
        {
            let conn = db.lock().unwrap();
            let review_status: Option<String> = conn
                .query_row(
                    "SELECT review_status FROM kanban_cards WHERE id = 'card-s4'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(review_status, None, "review_status cleared on done");

            let completed_at: Option<String> = conn
                .query_row(
                    "SELECT completed_at FROM kanban_cards WHERE id = 'card-s4'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(completed_at.is_some(), "completed_at set on done");
        }
    }

    #[test]
    fn terminal_transition_records_card_retrospective_from_latest_completed_dispatch() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-retro-e2e", "review", "test/repo", 418, None);

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards
                 SET review_round = 2,
                     review_notes = 'canonical thread_links only'
                 WHERE id = 'card-retro-e2e'",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO card_review_state (card_id, state, review_round) \
                 VALUES ('card-retro-e2e', 'reviewing', 2)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title, result,
                    created_at, updated_at, completed_at
                 ) VALUES (
                    'dispatch-retro-e2e', 'card-retro-e2e', 'agent-1', 'implementation', 'completed', 'Done', ?1,
                    datetime('now', '-37 minutes'), datetime('now'), datetime('now')
                 )",
                [json!({
                    "summary": "Discord 링크 생성은 canonical thread_links만 사용하도록 정리"
                })
                .to_string()],
            )
            .unwrap();
        }

        assert!(
            kanban::transition_status_with_opts(
                &db,
                &engine,
                "card-retro-e2e",
                "done",
                "test",
                true,
            )
            .is_ok()
        );
        assert_eq!(get_card_status(&db, "card-retro-e2e"), "done");

        let conn = db.lock().unwrap();
        let stored: (String, String, i64, String, String) = conn
            .query_row(
                "SELECT topic, content, review_round, terminal_status, sync_status
                 FROM card_retrospectives
                 WHERE card_id = 'card-retro-e2e'",
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
        assert_eq!(stored.0, "issue-418");
        assert!(stored.1.contains("AgentDesk 이슈 #418"));
        assert!(stored.1.contains("canonical thread_links"));
        assert!(stored.1.contains("review 2라운드"));
        assert_eq!(stored.2, 2);
        assert_eq!(stored.3, "done");
        assert!(
            stored.4 == "skipped_backend"
                || stored.4 == "skipped_no_runtime"
                || stored.4 == "queued",
            "unexpected sync status: {}",
            stored.4
        );
    }

    // ── Scenario 5: Timeout recovery ────────────────────────────────

    #[test]
    fn scenario_5_timeout_recovery_requested_to_manual_intervention() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);

        // Card stuck in requested for 50 min with exhausted retries
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, requested_at, created_at, updated_at) \
                 VALUES ('card-s5', 'Stale', 'requested', 'agent-1', datetime('now', '-50 minutes'), datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, retry_count, created_at, updated_at) \
                 VALUES ('d-s5', 'card-s5', 'agent-1', 'implementation', 'pending', 'Test', 10, datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE kanban_cards SET latest_dispatch_id = 'd-s5' WHERE id = 'card-s5'",
                [],
            )
            .unwrap();
        }

        // Fire onTick1min — [A] requested timeout lives in 1min tier (#127)
        let _ = engine.try_fire_hook_by_name("OnTick1min", serde_json::json!({}));

        // Drain transitions
        loop {
            let transitions = engine.drain_pending_transitions();
            if transitions.is_empty() {
                break;
            }
            for (card_id, old_s, new_s) in &transitions {
                kanban::fire_transition_hooks(&db, &engine, card_id, old_s, new_s);
            }
        }

        let conn = db.lock().unwrap();
        let (status, blocked_reason): (String, Option<String>) = conn
            .query_row(
                "SELECT status, blocked_reason FROM kanban_cards WHERE id = 'card-s5'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            status, "requested",
            "stale requested card with exhausted retries must stay in requested"
        );
        assert_eq!(
            blocked_reason.as_deref(),
            Some("Timed out waiting for agent (10 retries exhausted)"),
            "stale requested card must carry a manual-intervention blocked_reason"
        );
    }

    #[test]
    fn requested_preflight_cards_skip_timeout_without_dispatch() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, requested_at, created_at, updated_at) \
                 VALUES ('card-s5-preflight', 'Preflight', 'requested', 'agent-1', datetime('now', '-50 minutes'), datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        }

        let _ = engine.try_fire_hook_by_name("OnTick1min", serde_json::json!({}));

        loop {
            let transitions = engine.drain_pending_transitions();
            if transitions.is_empty() {
                break;
            }
            for (card_id, old_s, new_s) in &transitions {
                kanban::fire_transition_hooks(&db, &engine, card_id, old_s, new_s);
            }
        }

        let conn = db.lock().unwrap();
        let (status, latest_dispatch_id, blocked_reason): (
            String,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT status, latest_dispatch_id, blocked_reason FROM kanban_cards WHERE id = 'card-s5-preflight'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let dispatch_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-s5-preflight'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(
            status, "requested",
            "requested preflight cards without a dispatch must remain requested"
        );
        assert!(
            latest_dispatch_id.is_none(),
            "preflight timeout skip must not attach a dispatch"
        );
        assert!(
            blocked_reason.is_none(),
            "preflight timeout skip must not leave a blocked reason"
        );
        assert_eq!(
            dispatch_count, 0,
            "preflight timeout skip must not create a side dispatch"
        );
    }

    #[test]
    fn escalation_flush_bundles_suppressed_reasons_and_renotifies_after_cooldown() {
        let db = test_db();
        let policy_dir = setup_escalation_policy_dir();
        let engine = test_engine_with_dir(&db, policy_dir.path());
        seed_agent(&db);
        seed_card(&db, "card-escalation", "review");
        db.lock()
            .unwrap()
            .execute(
                "UPDATE kanban_cards SET review_status = 'dilemma_pending' WHERE id = 'card-escalation'",
                [],
            )
            .unwrap();
        set_config_key(&db, "server_port", json!(8791));

        engine
            .eval_js::<String>(
                r#"(() => { escalate("card-escalation", "reason-a"); flushEscalations(); return "ok"; })()"#,
            )
            .unwrap();
        assert_eq!(kv_value(&db, "test_http_count").as_deref(), Some("1"));
        assert_eq!(
            escalation_pending_reasons(&db, "card-escalation"),
            Vec::<String>::new()
        );

        engine
            .eval_js::<String>(
                r#"(() => { escalate("card-escalation", "reason-a"); flushEscalations(); return "ok"; })()"#,
            )
            .unwrap();
        assert_eq!(
            kv_value(&db, "test_http_count").as_deref(),
            Some("1"),
            "same-card alert must be suppressed during cooldown"
        );
        assert_eq!(
            escalation_pending_reasons(&db, "card-escalation"),
            vec!["reason-a".to_string()]
        );

        engine
            .eval_js::<String>(
                r#"(() => { escalate("card-escalation", "reason-b"); flushEscalations(); return "ok"; })()"#,
            )
            .unwrap();
        assert_eq!(kv_value(&db, "test_http_count").as_deref(), Some("1"));
        assert_eq!(
            escalation_pending_reasons(&db, "card-escalation"),
            vec!["reason-a".to_string(), "reason-b".to_string()]
        );

        let stale_sent_at = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64)
            - 601;
        set_kv(
            &db,
            "pm_decision_sent:card-escalation",
            &json!({
                "sent_at": stale_sent_at,
                "status": "review:dilemma_pending"
            })
            .to_string(),
        );

        engine
            .eval_js::<String>(r#"(() => { flushEscalations(); return "ok"; })()"#)
            .unwrap();
        assert_eq!(kv_value(&db, "test_http_count").as_deref(), Some("2"));
        assert!(
            kv_value(&db, "pm_pending:card-escalation").is_none(),
            "successful resend must clear the pending bundle"
        );

        let last_request = escalation_last_request(&db);
        let reasons = last_request["body"]["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item.as_str())
            .collect::<Vec<_>>();
        assert_eq!(reasons, vec!["reason-a", "reason-b"]);
    }

    #[test]
    fn on_tick5min_stale_in_progress_skips_cards_already_blocked() {
        let db = test_db();
        let policy_dir = setup_timeouts_policy_dir();
        let engine = test_engine_with_dir(&db, policy_dir.path());
        seed_agent(&db);
        seed_card(&db, "card-stale-blocked", "in_progress");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards
                 SET blocked_reason = 'Stalled: no activity for 120+ min',
                     started_at = datetime('now', '-3 hours'),
                     updated_at = datetime('now', '-3 hours')
                 WHERE id = 'card-stale-blocked'",
                [],
            )
            .unwrap();
        }

        engine
            .try_fire_hook_by_name("OnTick5min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        let blocked_reason: Option<String> = db
            .lock()
            .unwrap()
            .query_row(
                "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-stale-blocked'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(
            blocked_reason.as_deref(),
            Some("Stalled: no activity for 120+ min")
        );
        assert!(
            kv_value(&db, "test_http_count").is_none(),
            "#653: stale in_progress cards with blocked_reason must not re-escalate"
        );
    }

    #[test]
    fn active_manual_intervention_preserves_pending_escalation_bundle() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-force-enter", "requested");
        db.lock()
            .unwrap()
            .execute(
                "UPDATE kanban_cards SET blocked_reason = 'Needs PM review' WHERE id = 'card-force-enter'",
                [],
            )
            .unwrap();
        set_kv(
            &db,
            "pm_pending:card-force-enter",
            r#"{"title":"Test Card","reasons":["manual intervention"]}"#,
        );
        set_kv(
            &db,
            "pm_decision_sent:card-force-enter",
            r#"{"sent_at":123,"status":"blocked:Needs PM review"}"#,
        );

        kanban::transition_status_with_opts(
            &db,
            &engine,
            "card-force-enter",
            "backlog",
            "test",
            true,
        )
        .unwrap();

        assert!(kv_value(&db, "pm_pending:card-force-enter").is_some());
        assert!(kv_value(&db, "pm_decision_sent:card-force-enter").is_some());
    }

    #[test]
    fn resolving_manual_intervention_clears_escalation_cooldown_keys() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-force-leave", "requested");
        db.lock()
            .unwrap()
            .execute(
                "UPDATE kanban_cards SET blocked_reason = 'Needs PM review' WHERE id = 'card-force-leave'",
                [],
            )
            .unwrap();
        set_kv(
            &db,
            "pm_pending:card-force-leave",
            r#"{"title":"Test Card","reasons":["manual intervention"]}"#,
        );
        set_kv(
            &db,
            "pm_decision_sent:card-force-leave",
            r#"{"sent_at":123,"status":"blocked:Needs PM review"}"#,
        );

        kanban::transition_status_with_opts_and_on_conn(
            &db,
            &engine,
            "card-force-leave",
            "backlog",
            "test",
            true,
            |conn| {
                conn.execute(
                    "UPDATE kanban_cards SET blocked_reason = NULL WHERE id = 'card-force-leave'",
                    [],
                )?;
                Ok(())
            },
        )
        .unwrap();

        assert!(kv_value(&db, "pm_pending:card-force-leave").is_none());
        assert!(kv_value(&db, "pm_decision_sent:card-force-leave").is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn auto_queue_on_tick_dispatches_ready_card_via_requested_preflight() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        ensure_auto_queue_tables(&db);

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, repo_id, created_at, updated_at) \
                 VALUES ('card-aq-ready', 'AQ Ready', 'ready', 'agent-1', 'repo-1', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
                 VALUES ('run-aq-ready', 'repo-1', 'agent-1', 'active', datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, created_at) \
                 VALUES ('entry-aq-ready', 'run-aq-ready', 'card-aq-ready', 'agent-1', 'pending', 0, datetime('now'))",
                [],
            )
            .unwrap();
        }

        // onTick1min is now a thin localhost API trigger. In the unit harness there is
        // no Axum server, so exercise the authoritative activate route directly.
        let state = AppState {
            db: db.clone(),
            pg_pool: None,
            engine: engine.clone(),
            config: std::sync::Arc::new(crate::config::Config::default()),
            broadcast_tx: crate::server::ws::new_broadcast(),
            batch_buffer: crate::server::ws::spawn_batch_flusher(crate::server::ws::new_broadcast()),
            health_registry: None,
        };
        let (status, body) = crate::server::routes::auto_queue::activate(
            axum::extract::State(state),
            axum::Json(crate::server::routes::auto_queue::ActivateBody {
                run_id: Some("run-aq-ready".to_string()),
                repo: None,
                agent_id: None,
                thread_group: None,
                unified_thread: None,
                active_only: Some(true),
            }),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(body.0["count"].as_u64(), Some(1));
        kanban::drain_hook_side_effects(&db, &engine);

        let conn = db.lock().unwrap();
        let entry_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-aq-ready'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            entry_status, "dispatched",
            "ready card must be dispatched by auto-queue tick"
        );

        let dispatch_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-aq-ready'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dispatch_count, 1, "exactly one dispatch must be created");

        let card_status: String = conn
            .query_row(
                "SELECT status FROM kanban_cards WHERE id = 'card-aq-ready'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            card_status, "in_progress",
            "ready card must advance through requested preflight to in_progress"
        );
    }

    #[test]
    fn auto_queue_activate_concurrent_calls_dispatch_once() {
        // `create_dispatch()` may resolve the default repo/worktree from
        // AGENTDESK_REPO_DIR. Hold the shared env lock with a real git repo so
        // this concurrency test does not race unrelated env-mutating tests.
        let (_repo, _repo_guard) = setup_test_repo();
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        ensure_auto_queue_tables(&db);

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, repo_id, created_at, updated_at) \
                 VALUES ('card-aq-concurrent', 'AQ Concurrent', 'ready', 'agent-1', 'repo-1', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
                 VALUES ('run-aq-concurrent', 'repo-1', 'agent-1', 'active', datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, created_at) \
                 VALUES ('entry-aq-concurrent', 'run-aq-concurrent', 'card-aq-concurrent', 'agent-1', 'pending', 0, datetime('now'))",
                [],
            )
            .unwrap();
        }

        let deps = crate::server::routes::auto_queue::AutoQueueActivateDeps::for_bridge(
            db.clone(),
            engine.clone(),
        );
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let make_worker = || {
            let deps = deps.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                crate::server::routes::auto_queue::activate_with_deps(
                    &deps,
                    crate::server::routes::auto_queue::ActivateBody {
                        run_id: Some("run-aq-concurrent".to_string()),
                        repo: None,
                        agent_id: None,
                        thread_group: None,
                        unified_thread: None,
                        active_only: Some(true),
                    },
                )
            })
        };

        let first_handle = make_worker();
        let second_handle = make_worker();
        let first = first_handle.join().unwrap();
        let second = second_handle.join().unwrap();
        assert_eq!(first.0, axum::http::StatusCode::OK);
        assert_eq!(second.0, axum::http::StatusCode::OK);

        // The two concurrent activate calls must collectively dispatch exactly once.
        // Check via DB rather than response counts — under heavy contention a thread
        // may observe the reservation without its count being reflected in the JSON
        // response (the entry was already claimed by the other thread). On Windows
        // the surviving dispatch can become visible a little later than the hook
        // drain call, so poll briefly for the stabilized row set.
        let mut dispatch_count = 0;
        let mut entry_status = String::new();
        let mut card_status = String::new();
        let mut latest_dispatch_id: Option<String> = None;
        let mut dispatch_status: Option<String> = None;
        let mut entry_dispatch_id: Option<String> = None;

        for attempt in 0..80 {
            kanban::drain_hook_side_effects(&db, &engine);

            let (
                observed_dispatch_count,
                observed_entry_status,
                observed_card_status,
                observed_latest_dispatch_id,
                observed_dispatch_status,
                observed_entry_dispatch_id,
            ) = {
                let conn = db.lock().unwrap();
                let observed_dispatch_count: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-aq-concurrent'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                let observed_entry_status: String = conn
                    .query_row(
                        "SELECT status FROM auto_queue_entries WHERE id = 'entry-aq-concurrent'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                let (observed_card_status, observed_latest_dispatch_id): (String, Option<String>) =
                    conn.query_row(
                        "SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = 'card-aq-concurrent'",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .unwrap();
                let observed_dispatch_status =
                    observed_latest_dispatch_id
                        .as_deref()
                        .and_then(|dispatch_id| {
                            conn.query_row(
                                "SELECT status FROM task_dispatches WHERE id = ?1",
                                [dispatch_id],
                                |row| row.get(0),
                            )
                            .ok()
                        });
                let observed_entry_dispatch_id: Option<String> = conn
                    .query_row(
                        "SELECT dispatch_id FROM auto_queue_entries WHERE id = 'entry-aq-concurrent'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                (
                    observed_dispatch_count,
                    observed_entry_status,
                    observed_card_status,
                    observed_latest_dispatch_id,
                    observed_dispatch_status,
                    observed_entry_dispatch_id,
                )
            };

            dispatch_count = observed_dispatch_count;
            entry_status = observed_entry_status;
            card_status = observed_card_status;
            latest_dispatch_id = observed_latest_dispatch_id;
            dispatch_status = observed_dispatch_status;
            entry_dispatch_id = observed_entry_dispatch_id;

            if dispatch_count == 1
                && card_status == "in_progress"
                && dispatch_status.as_deref() == Some("pending")
                && entry_status == "dispatched"
                && entry_dispatch_id == latest_dispatch_id
            {
                break;
            }

            if attempt < 79 {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }

        assert_eq!(dispatch_count, 1, "only one dispatch row must be created");
        assert_eq!(
            card_status, "in_progress",
            "concurrent activate must leave the card on the active implementation path"
        );
        assert_eq!(
            dispatch_status.as_deref(),
            Some("pending"),
            "the surviving dispatch must remain pending after concurrent activation"
        );
        assert_eq!(
            entry_status, "dispatched",
            "the shared entry must remain dispatched after the winning activate call"
        );
        assert_eq!(
            entry_dispatch_id, latest_dispatch_id,
            "recovered concurrent activate must keep the entry attached to the surviving dispatch"
        );
    }

    #[test]
    fn auto_queue_activate_agent_scope_uses_free_slot_for_additional_group() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        ensure_auto_queue_tables(&db);

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, repo_id, created_at, updated_at) \
                 VALUES ('card-aq-live', 'AQ Live', 'in_progress', 'agent-1', 'repo-1', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, repo_id, created_at, updated_at) \
                 VALUES ('card-aq-next', 'AQ Next', 'ready', 'agent-1', 'repo-1', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status, max_concurrent_threads, thread_group_count, created_at) \
                 VALUES ('run-aq-slot-scale', 'repo-1', 'agent-1', 'active', 2, 2, datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, thread_group, priority_rank, dispatched_at, created_at) \
                 VALUES ('entry-aq-live', 'run-aq-slot-scale', 'card-aq-live', 'agent-1', 'dispatched', 'dispatch-aq-live', 0, 0, 0, datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, thread_group, priority_rank, created_at) \
                 VALUES ('entry-aq-next', 'run-aq-slot-scale', 'card-aq-next', 'agent-1', 'pending', 1, 0, datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_slots (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map, created_at, updated_at) \
                 VALUES ('agent-1', 0, 'run-aq-slot-scale', 0, '{}', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_slots (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map, created_at, updated_at) \
                 VALUES ('agent-1', 1, NULL, NULL, '{}', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        }
        seed_dispatch(
            &db,
            "dispatch-aq-live",
            "card-aq-live",
            "implementation",
            "dispatched",
        );

        let deps = crate::server::routes::auto_queue::AutoQueueActivateDeps::for_bridge(
            db.clone(),
            engine.clone(),
        );
        let (status, body) = crate::server::routes::auto_queue::activate_with_deps(
            &deps,
            crate::server::routes::auto_queue::ActivateBody {
                run_id: Some("run-aq-slot-scale".to_string()),
                repo: Some("repo-1".to_string()),
                agent_id: Some("agent-1".to_string()),
                thread_group: None,
                unified_thread: None,
                active_only: Some(true),
            },
        );

        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(
            body.0["count"].as_u64(),
            Some(1),
            "agent-scoped activate should dispatch another group when a free slot exists"
        );

        kanban::drain_hook_side_effects(&db, &engine);

        let conn = db.lock().unwrap();
        let (entry_status, slot_index, dispatch_id): (String, Option<i64>, Option<String>) = conn
            .query_row(
                "SELECT status, slot_index, dispatch_id FROM auto_queue_entries WHERE id = 'entry-aq-next'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(entry_status, "dispatched");
        assert_eq!(slot_index, Some(1));
        assert!(
            dispatch_id.is_some(),
            "newly dispatched group must persist dispatch_id"
        );
    }

    #[test]
    fn auto_queue_on_tick_recovery_counts_failures_and_fails_at_retry_limit() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        ensure_auto_queue_tables(&db);
        seed_card(&db, "card-aq-orphan", "in_progress");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
                 VALUES ('run-aq-orphan', 'repo-1', 'agent-1', 'active', datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kv_meta (key, value) VALUES ('kanban_human_alert_channel_id', 'human-alert')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, dispatched_at, created_at) \
                 VALUES ('entry-aq-orphan', 'run-aq-orphan', 'card-aq-orphan', 'agent-1', 'dispatched', 0, datetime('now', '-5 minutes'), datetime('now', '-5 minutes'))",
                [],
            )
            .unwrap();
        }

        for expected_retry_count in 1..=3 {
            engine
                .try_fire_hook_by_name("OnTick1min", json!({}))
                .unwrap();

            let conn = db.lock().unwrap();
            let (status, retry_count, dispatch_id): (String, i64, Option<String>) = conn
                .query_row(
                    "SELECT status, retry_count, dispatch_id
                     FROM auto_queue_entries
                     WHERE id = 'entry-aq-orphan'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            let transition_source: String = conn
                .query_row(
                    "SELECT trigger_source FROM auto_queue_entry_transitions \
                     WHERE entry_id = 'entry-aq-orphan' ORDER BY id DESC LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            drop(conn);

            let expected_status = if expected_retry_count >= 3 {
                "failed"
            } else {
                "pending"
            };
            assert_eq!(status, expected_status);
            assert_eq!(retry_count, expected_retry_count);
            assert!(
                dispatch_id.is_none(),
                "orphan recovery must not invent a replacement dispatch id"
            );
            assert_eq!(
                transition_source, "tick_recovery",
                "orphan recovery should record the periodic recovery source"
            );

            if expected_retry_count < 3 {
                let conn = db.lock().unwrap();
                crate::db::auto_queue::update_entry_status_on_conn(
                    &conn,
                    "entry-aq-orphan",
                    crate::db::auto_queue::ENTRY_STATUS_DISPATCHED,
                    "test_rearm_orphan_dispatch",
                    &crate::db::auto_queue::EntryStatusUpdateOptions::default(),
                )
                .unwrap();
                conn.execute(
                    "UPDATE auto_queue_entries
                     SET dispatched_at = datetime('now', '-5 minutes')
                     WHERE id = 'entry-aq-orphan'",
                    [],
                )
                .unwrap();
            }
        }

        let messages = message_outbox_rows(&db);
        let auto_queue_alerts: Vec<_> = messages
            .into_iter()
            .filter(|(target, content)| {
                target == "channel:human-alert" && content.contains("[Auto Queue]")
            })
            .collect();
        assert_eq!(
            auto_queue_alerts.len(),
            1,
            "terminal tick recovery should emit exactly one auto-queue human alert"
        );
    }

    // ── Scenario 6: Dispatch roundtrip — create → complete_dispatch → PM gate → review ──
    //
    // Tests the full dispatch lifecycle using the canonical completion path:
    // 1. dispatch::create_dispatch_core creates a pending dispatch
    // 2. dispatch::complete_dispatch completes via the same path as PATCH /api/dispatches/:id
    //    (DB update → OnDispatchCompleted → drain transitions → fire_transition_hooks)
    // 3. PM gate passes (no DoD, no duration check) → card transitions to review
    // 4. OnReviewEnter fires → review dispatch is created

    #[test]
    fn scenario_6_dispatch_roundtrip() {
        let (_repo, _repo_guard) = setup_test_repo();
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-s6", "in_progress");

        // Step 1: Create implementation dispatch via canonical path
        let (dispatch_id, _, _) = dispatch::create_dispatch_core(
            &db,
            "card-s6",
            "agent-1",
            "implementation",
            "[Impl]",
            &serde_json::json!({}),
        )
        .unwrap();
        assert_eq!(get_dispatch_status(&db, &dispatch_id), "pending");

        // Verify latest_dispatch_id was updated
        {
            let conn = db.lock().unwrap();
            let latest: String = conn
                .query_row(
                    "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-s6'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                latest, dispatch_id,
                "latest_dispatch_id must point to new dispatch"
            );
        }

        // Step 2: Complete via dispatch::complete_dispatch — the canonical path
        // used by PATCH /api/dispatches/:id and turn_bridge.
        // This handles: DB update → OnDispatchCompleted → drain transitions → fire_transition_hooks
        seed_assistant_response_for_dispatch(&db, &dispatch_id, "implemented card-s6");
        let result = dispatch::complete_dispatch(
            &db,
            &engine,
            &dispatch_id,
            &serde_json::json!({"completion_source": "test_harness"}),
        );
        assert!(
            result.is_ok(),
            "complete_dispatch should succeed: {:?}",
            result.err()
        );
        assert_eq!(get_dispatch_status(&db, &dispatch_id), "completed");

        // Step 3: PM gate passes (no DoD items, no duration constraint) → card must be in review
        let final_status = get_card_status(&db, "card-s6");
        assert_eq!(
            final_status, "review",
            "PM gate with empty DoD should pass → card must be in review"
        );

        // Step 4: Verify review state was properly initialized
        {
            let conn = db.lock().unwrap();

            // review_entered_at must be set
            let review_entered: Option<String> = conn
                .query_row(
                    "SELECT review_entered_at FROM kanban_cards WHERE id = 'card-s6'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(review_entered.is_some(), "review_entered_at must be set");

            // OnReviewEnter should have created a review dispatch
            let review_dispatch_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM task_dispatches \
                     WHERE kanban_card_id = 'card-s6' AND dispatch_type = 'review' \
                     AND status IN ('pending', 'dispatched')",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                review_dispatch_count, 1,
                "OnReviewEnter should create exactly 1 review dispatch"
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scenario_6b_review_verdict_pass_completes_roundtrip_to_done() {
        let (_repo, runtime_root, _env_guard) = setup_test_repo_with_runtime_root();
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-s6b", "in_progress");

        let (dispatch_id, _, _) = dispatch::create_dispatch_core(
            &db,
            "card-s6b",
            "agent-1",
            "implementation",
            "[Impl]",
            &json!({}),
        )
        .unwrap();
        seed_assistant_response_for_dispatch(&db, &dispatch_id, "implemented card-s6b");
        dispatch::complete_dispatch(
            &db,
            &engine,
            &dispatch_id,
            &json!({"completion_source": "test_harness"}),
        )
        .unwrap();

        assert_eq!(
            get_card_status(&db, "card-s6b"),
            "review",
            "implementation completion must advance the card into review first"
        );

        let (review_dispatch_id, reviewed_commit, review_provider) = {
            let conn = db.lock().unwrap();
            let (dispatch_id, context): (String, Option<String>) = conn
                .query_row(
                    "SELECT id, context FROM task_dispatches \
                     WHERE kanban_card_id = 'card-s6b' AND dispatch_type = 'review' \
                     AND status IN ('pending', 'dispatched') \
                     ORDER BY created_at DESC, id DESC LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            let review_context = context
                .as_deref()
                .and_then(|value| serde_json::from_str::<serde_json::Value>(value).ok())
                .expect("review dispatch must carry JSON context");
            let reviewed_commit = review_context
                .get("reviewed_commit")
                .and_then(|field| field.as_str())
                .map(str::to_string)
                .expect("review dispatch must carry reviewed_commit context");
            let review_provider = review_context
                .get("target_provider")
                .and_then(|field| field.as_str())
                .map(str::to_string)
                .expect("review dispatch must carry target_provider context");
            (dispatch_id, reviewed_commit, review_provider)
        };

        let state = AppState::test_state(db.clone(), engine);
        let (status, body) = crate::server::routes::review_verdict::submit_verdict(
            axum::extract::State(state),
            axum::Json(crate::server::routes::review_verdict::SubmitVerdictBody {
                dispatch_id: review_dispatch_id.clone(),
                overall: "pass".to_string(),
                items: None,
                notes: Some("characterization pass".to_string()),
                feedback: None,
                commit: Some(reviewed_commit.clone()),
                provider: Some(review_provider),
            }),
        )
        .await;

        assert_eq!(
            status,
            axum::http::StatusCode::OK,
            "pass verdict should succeed: {body:?}"
        );
        assert_eq!(body.0["ok"], true);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let conn = db.lock().unwrap();
        let (card_status, review_status, latest_dispatch_id): (
            String,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT status, review_status, latest_dispatch_id FROM kanban_cards WHERE id = 'card-s6b'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let review_dispatch_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = ?1",
                [&review_dispatch_id],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);

        assert_eq!(
            review_dispatch_status, "completed",
            "review verdict must complete the pending review dispatch"
        );
        assert_eq!(
            card_status, "done",
            "pass verdict must finish the full implementation -> review -> done lifecycle"
        );
        assert_eq!(review_status, None, "done card must clear review_status");
        assert_eq!(
            latest_dispatch_id.as_deref(),
            Some(review_dispatch_id.as_str()),
            "latest_dispatch_id should remain anchored to the completing review dispatch"
        );
        assert_eq!(
            count_active_dispatches_by_type(&db, "card-s6b", "review"),
            0,
            "no active review dispatch may remain after a passing verdict"
        );
        assert!(
            runtime_root
                .path()
                .join("runtime")
                .join("review_passed")
                .join(&reviewed_commit)
                .exists(),
            "pass verdict must stamp the reviewed commit marker in the runtime root"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scenario_6c_review_verdict_pass_closes_issue_and_creates_phase_gate_for_single_phase_run()
     {
        let gh = install_mock_gh(&[MockGhReply {
            key: "issue:close",
            contains: Some("--repo test/repo"),
            stdout: "",
        }]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-s6c", "review", "test/repo", 483, None);
        seed_dispatch(&db, "review-s6c", "card-s6c", "review", "pending");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET review_status = 'reviewing' WHERE id = 'card-s6c'",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
                 VALUES ('run-s6c', 'test/repo', 'agent-1', 'active', datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, created_at, dispatched_at) \
                 VALUES ('entry-s6c', 'run-s6c', 'card-s6c', 'agent-1', 'dispatched', 1, datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        }

        let state = AppState::test_state(db.clone(), engine);
        let (status, body) = crate::server::routes::review_verdict::submit_verdict(
            axum::extract::State(state),
            axum::Json(crate::server::routes::review_verdict::SubmitVerdictBody {
                dispatch_id: "review-s6c".to_string(),
                overall: "pass".to_string(),
                items: None,
                notes: Some("characterization pass".to_string()),
                feedback: None,
                commit: None,
                provider: None,
            }),
        )
        .await;

        assert_eq!(
            status,
            axum::http::StatusCode::OK,
            "pass verdict should succeed: {body:?}"
        );

        let conn = db.lock().unwrap();
        let card_status: String = conn
            .query_row(
                "SELECT status FROM kanban_cards WHERE id = 'card-s6c'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let entry_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-s6c'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let run_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_runs WHERE id = 'run-s6c'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let phase_gate_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches \
                 WHERE kanban_card_id = 'card-s6c' \
                   AND dispatch_type = 'phase-gate' \
                   AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);

        assert_eq!(card_status, "done");
        assert_eq!(
            entry_status, "done",
            "pass verdict terminal transition must close the active auto-queue entry"
        );
        let phase_gate_json =
            phase_gate_state(&db, "run-s6c", 0).expect("single-phase run must persist gate state");
        assert_eq!(
            run_status, "paused",
            "pass verdict terminal transition must pause for a single-phase gate"
        );
        assert_eq!(
            phase_gate_count, 1,
            "pass verdict terminal transition must create a single-phase gate dispatch"
        );
        assert_eq!(phase_gate_json["status"], "pending");
        assert_eq!(phase_gate_json["batch_phase"], 0);
        assert_eq!(phase_gate_json["next_phase"], serde_json::Value::Null);
        assert_eq!(phase_gate_json["final_phase"], true);

        let log = gh_log(&gh);
        assert!(
            log.contains("issue close 483 --repo test/repo"),
            "pass verdict path must close the linked GitHub issue"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scenario_6d_review_verdict_pass_uses_current_review_state_gate_targets() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/multi-review");

        let multi_review_override = serde_json::json!({
            "states": [
                {"id": "backlog", "label": "Backlog"},
                {"id": "ready", "label": "Ready"},
                {"id": "requested", "label": "Requested"},
                {"id": "in_progress", "label": "In Progress"},
                {"id": "review_stage_one", "label": "Review Stage One"},
                {"id": "qa_test", "label": "QA Test"},
                {"id": "review_stage_two", "label": "Review Stage Two"},
                {"id": "done", "label": "Done", "terminal": true}
            ],
            "transitions": [
                {"from": "backlog", "to": "ready", "type": "free"},
                {"from": "ready", "to": "requested", "type": "free"},
                {"from": "requested", "to": "in_progress", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "in_progress", "to": "review_stage_one", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "review_stage_one", "to": "qa_test", "type": "gated", "gates": ["review_passed"]},
                {"from": "review_stage_one", "to": "in_progress", "type": "gated", "gates": ["review_rework"]},
                {"from": "qa_test", "to": "review_stage_two", "type": "free"},
                {"from": "review_stage_two", "to": "done", "type": "gated", "gates": ["review_passed"]},
                {"from": "review_stage_two", "to": "qa_test", "type": "gated", "gates": ["review_rework"]}
            ],
            "gates": {
                "active_dispatch": {"type": "builtin", "check": "has_active_dispatch"},
                "review_passed": {"type": "builtin", "check": "review_verdict_pass"},
                "review_rework": {"type": "builtin", "check": "review_verdict_rework"}
            },
            "hooks": {},
            "clocks": {},
            "events": {},
            "timeouts": {}
        });

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE github_repos SET pipeline_config = ?1 WHERE id = 'test/multi-review'",
                [multi_review_override.to_string()],
            )
            .unwrap();
        }

        seed_card_with_repo(
            &db,
            "card-s6d",
            "review_stage_two",
            "test/multi-review",
            697,
            None,
        );
        seed_dispatch(&db, "review-s6d", "card-s6d", "review", "pending");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET review_status = 'reviewing' WHERE id = 'card-s6d'",
                [],
            )
            .unwrap();
        }

        let state = AppState::test_state(db.clone(), engine);
        let (status, body) = crate::server::routes::review_verdict::submit_verdict(
            axum::extract::State(state),
            axum::Json(crate::server::routes::review_verdict::SubmitVerdictBody {
                dispatch_id: "review-s6d".to_string(),
                overall: "pass".to_string(),
                items: None,
                notes: Some("multi-review branch pass".to_string()),
                feedback: None,
                commit: None,
                provider: None,
            }),
        )
        .await;

        assert_eq!(
            status,
            axum::http::StatusCode::OK,
            "pass verdict should succeed on later review branch: {body:?}"
        );

        let conn = db.lock().unwrap();
        let card_status: String = conn
            .query_row(
                "SELECT status FROM kanban_cards WHERE id = 'card-s6d'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let review_dispatch_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = 'review-s6d'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);

        assert_eq!(
            review_dispatch_status, "completed",
            "review dispatch must complete after the verdict"
        );
        assert_eq!(
            card_status, "done",
            "pass verdict must follow the current review state's review_passed gate"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scenario_655_noop_review_pass_skips_create_pr_and_creates_phase_gate() {
        let gh = install_mock_gh(&[MockGhReply {
            key: "issue:close",
            contains: Some("--repo test/repo"),
            stdout: "",
        }]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-655-pass", "review", "test/repo", 655, None);

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET review_status = 'reviewing', latest_dispatch_id = 'review-655-pass' WHERE id = 'card-655-pass'",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
                 VALUES ('run-655-pass', 'test/repo', 'agent-1', 'active', datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, priority_rank, created_at, dispatched_at) \
                 VALUES ('entry-655-pass', 'run-655-pass', 'card-655-pass', 'agent-1', 'dispatched', 'impl-655-pass', 1, datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, context, completed_at, created_at, updated_at) \
                 VALUES ('impl-655-pass', 'card-655-pass', 'agent-1', 'implementation', 'completed', '[Impl noop]', ?1, ?2, datetime('now', '-2 minutes'), datetime('now', '-5 minutes'), datetime('now', '-2 minutes'))",
                libsql_rusqlite::params![
                    serde_json::json!({
                        "work_outcome": "noop",
                        "completed_without_changes": true,
                        "completed_worktree_path": "/tmp/wt-655-pass",
                        "completed_branch": "wt/655-noop",
                        "completed_commit": "abc12345deadbeef",
                        "notes": "already implemented"
                    }).to_string(),
                    serde_json::json!({
                        "worktree_path": "/tmp/wt-655-pass",
                        "worktree_branch": "wt/655-noop",
                        "target_repo": "test/repo"
                    }).to_string()
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, completed_at, created_at, updated_at) \
                 VALUES ('review-655-pass-stale', 'card-655-pass', 'agent-1', 'review', 'completed', '[Review stale]', ?1, datetime('now', '+1 minute'), datetime('now', '-10 minutes'), datetime('now', '+1 minute'))",
                libsql_rusqlite::params![serde_json::json!({
                    "review_mode": "regular_review",
                    "parent_dispatch_id": "impl-655-pass"
                })
                .to_string()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at) \
                 VALUES ('review-655-pass', 'card-655-pass', 'agent-1', 'review', 'pending', '[Review noop]', ?1, datetime('now'), datetime('now'))",
                libsql_rusqlite::params![
                    serde_json::json!({
                        "review_mode": "noop_verification",
                        "noop_reason": "already implemented",
                        "parent_dispatch_id": "impl-655-pass"
                    }).to_string()
                ],
            )
            .unwrap();
        }

        let state = AppState::test_state(db.clone(), engine);
        let (status, body) = crate::server::routes::review_verdict::submit_verdict(
            axum::extract::State(state),
            axum::Json(crate::server::routes::review_verdict::SubmitVerdictBody {
                dispatch_id: "review-655-pass".to_string(),
                overall: "pass".to_string(),
                items: None,
                notes: Some("noop verification passed".to_string()),
                feedback: None,
                commit: None,
                provider: None,
            }),
        )
        .await;

        assert_eq!(
            status,
            axum::http::StatusCode::OK,
            "noop verification pass should succeed: {body:?}"
        );

        let conn = db.lock().unwrap();
        let card_status: String = conn
            .query_row(
                "SELECT status FROM kanban_cards WHERE id = 'card-655-pass'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let create_pr_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-655-pass' AND dispatch_type = 'create-pr'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let entry_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-655-pass'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let run_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_runs WHERE id = 'run-655-pass'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let phase_gate_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches \
                 WHERE kanban_card_id = 'card-655-pass' \
                   AND dispatch_type = 'phase-gate' \
                   AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);

        assert_eq!(card_status, "done");
        assert_eq!(
            create_pr_count, 0,
            "#655: noop verification pass must not create a create-pr dispatch"
        );
        assert_eq!(
            entry_status, "done",
            "#655: noop verification pass must still close the active auto-queue entry"
        );
        let phase_gate_json = phase_gate_state(&db, "run-655-pass", 0)
            .expect("#655: single-phase noop pass must persist gate state");
        assert_eq!(
            run_status, "paused",
            "#655: noop verification pass must pause the run for the single-phase gate"
        );
        assert_eq!(
            phase_gate_count, 1,
            "#655: noop verification pass must create a phase-gate dispatch after review passes"
        );
        assert_eq!(phase_gate_json["status"], "pending");
        assert_eq!(phase_gate_json["batch_phase"], 0);
        assert_eq!(phase_gate_json["next_phase"], serde_json::Value::Null);
        assert_eq!(phase_gate_json["final_phase"], true);

        let log = gh_log(&gh);
        assert!(
            log.contains("issue close 655 --repo test/repo"),
            "#655: noop verification pass must close the linked GitHub issue"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn scenario_655_noop_review_reject_creates_rework_dispatch() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-655-reject", "review");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET assigned_agent_id = 'agent-1', review_status = 'reviewing', latest_dispatch_id = 'review-655-reject', title = 'Noop Reject Card', github_issue_number = 655 WHERE id = 'card-655-reject'",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at) \
                 VALUES ('review-655-reject', 'card-655-reject', 'agent-1', 'review', 'pending', '[Review noop]', ?1, datetime('now'), datetime('now'))",
                libsql_rusqlite::params![
                    serde_json::json!({
                        "review_mode": "noop_verification",
                        "noop_reason": "already implemented elsewhere"
                    }).to_string()
                ],
            )
            .unwrap();
        }

        let state = AppState::test_state(db.clone(), engine);
        let (status, body) = crate::server::routes::review_verdict::submit_verdict(
            axum::extract::State(state),
            axum::Json(crate::server::routes::review_verdict::SubmitVerdictBody {
                dispatch_id: "review-655-reject".to_string(),
                overall: "reject".to_string(),
                items: None,
                notes: Some("required behavior is still missing".to_string()),
                feedback: None,
                commit: None,
                provider: None,
            }),
        )
        .await;

        assert_eq!(
            status,
            axum::http::StatusCode::OK,
            "noop verification reject should succeed: {body:?}"
        );

        let conn = db.lock().unwrap();
        let card_status: String = conn
            .query_row(
                "SELECT status FROM kanban_cards WHERE id = 'card-655-reject'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let review_status: Option<String> = conn
            .query_row(
                "SELECT review_status FROM kanban_cards WHERE id = 'card-655-reject'",
                [],
                |row| row.get(0),
            )
            .ok()
            .flatten();
        let rework_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-655-reject' AND dispatch_type = 'rework' AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let review_decision_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-655-reject' AND dispatch_type = 'review-decision' AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);

        assert_eq!(card_status, "in_progress");
        assert_eq!(review_status.as_deref(), Some("rework_pending"));
        assert_eq!(
            rework_count, 1,
            "#655: noop verification reject must create a rework dispatch"
        );
        assert_eq!(
            review_decision_count, 0,
            "#655: noop verification reject must not create a review-decision dispatch on the synchronous verdict path"
        );
    }

    // ── Scenario 7: dispatch uses card's effective pipeline, not global default (#134/#136) ──

    #[test]
    fn scenario_7_dispatch_uses_card_effective_pipeline() {
        let db = test_db();
        seed_agent(&db);
        crate::pipeline::ensure_loaded();

        // Simple pipeline override: ready→in_progress (gated), in_progress→done (gated)
        // No "requested" state at all — kickoff should be "in_progress"
        let simple_override = serde_json::json!({
            "states": [
                {"id": "backlog", "label": "Backlog"},
                {"id": "ready", "label": "Ready"},
                {"id": "in_progress", "label": "In Progress"},
                {"id": "done", "label": "Done", "terminal": true}
            ],
            "transitions": [
                {"from": "backlog", "to": "ready", "type": "free"},
                {"from": "ready", "to": "in_progress", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "in_progress", "to": "done", "type": "gated", "gates": ["active_dispatch"]}
            ],
            "gates": {
                "active_dispatch": {"type": "builtin", "check": "has_active_dispatch"}
            },
            "hooks": {
                "in_progress": {"on_enter": ["OnCardTransition"], "on_exit": []},
                "done": {"on_enter": ["OnCardTransition", "OnCardTerminal"], "on_exit": []}
            },
            "clocks": {
                "in_progress": {"set": "started_at"},
                "done": {"set": "completed_at"}
            },
            "events": {
                "on_dispatch_completed": ["OnDispatchCompleted"]
            },
            "timeouts": {
                "in_progress": {"duration": "4h", "clock": "started_at", "on_exhaust": "done"}
            }
        });

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO github_repos (id, display_name, pipeline_config) VALUES ('repo-s7', 'test/s7', ?1)",
                [simple_override.to_string()],
            ).unwrap();
            // Card with repo_id pointing to override — in "ready" (dispatchable in simple pipeline)
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, repo_id, assigned_agent_id, created_at, updated_at) \
                 VALUES ('card-s7', 'S7 Card', 'ready', 'repo-s7', 'agent-1', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
            // Need a completed dispatch so the pending-dispatch guard doesn't block
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
                 VALUES ('d-s7-old', 'card-s7', 'agent-1', 'implementation', 'completed', 'old', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        }

        // #255: Default pipeline kickoff is "in_progress" (requested is now a free preflight state,
        // so the dispatchable state is "requested" with gated target "in_progress").
        let default_kickoff = crate::pipeline::get()
            .transitions
            .iter()
            .find(|t| {
                t.transition_type == crate::pipeline::TransitionType::Gated
                    && crate::pipeline::get()
                        .dispatchable_states()
                        .contains(&t.from.as_str())
            })
            .map(|t| t.to.as_str())
            .unwrap();
        assert_eq!(
            default_kickoff, "in_progress",
            "default pipeline kickoff must be 'in_progress' (#255: requested is preflight)"
        );

        // Create dispatch via create_dispatch_core_with_id — should use card's effective pipeline
        let result = dispatch::create_dispatch_core_with_id(
            &db,
            "d-s7-new",
            "card-s7",
            "agent-1",
            "implementation",
            "[S7 test]",
            &serde_json::json!({}),
        );
        assert!(
            result.is_ok(),
            "dispatch creation should succeed: {:?}",
            result.err()
        );

        // Card status must be "in_progress" (both override and default kickoff target the same)
        let status = get_card_status(&db, "card-s7");
        assert_eq!(
            status, "in_progress",
            "dispatch must use card's effective pipeline kickoff"
        );

        // Also test create_dispatch_core (the non-ID path)
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, repo_id, assigned_agent_id, created_at, updated_at) \
                 VALUES ('card-s7b', 'S7b Card', 'ready', 'repo-s7', 'agent-1', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
                 VALUES ('d-s7b-old', 'card-s7b', 'agent-1', 'implementation', 'completed', 'old', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        }
        let result2 = dispatch::create_dispatch_core(
            &db,
            "card-s7b",
            "agent-1",
            "implementation",
            "[S7b test]",
            &serde_json::json!({}),
        );
        assert!(
            result2.is_ok(),
            "create_dispatch_core should succeed: {:?}",
            result2.err()
        );
        assert_eq!(
            get_card_status(&db, "card-s7b"),
            "in_progress",
            "create_dispatch_core must also use card's effective pipeline kickoff"
        );
    }

    // ── Scenario 8: Custom pipeline override — resolve and validate (#135/#136) ──

    #[test]
    fn scenario_8_custom_pipeline_override_resolve_and_validate() {
        let db = test_db();
        seed_agent(&db);
        crate::pipeline::ensure_loaded();

        // Insert a repo with a simple pipeline override (no review state)
        let simple_override = serde_json::json!({
            "states": [
                {"id": "backlog", "label": "Backlog"},
                {"id": "ready", "label": "Ready"},
                {"id": "in_progress", "label": "In Progress"},
                {"id": "done", "label": "Done", "terminal": true}
            ],
            "transitions": [
                {"from": "backlog", "to": "ready", "type": "free"},
                {"from": "ready", "to": "in_progress", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "in_progress", "to": "done", "type": "gated", "gates": ["active_dispatch"]}
            ],
            "gates": {
                "active_dispatch": {"type": "builtin", "check": "has_active_dispatch"}
            },
            "hooks": {
                "in_progress": {"on_enter": ["OnCardTransition"], "on_exit": []},
                "done": {"on_enter": ["OnCardTransition", "OnCardTerminal"], "on_exit": []}
            },
            "clocks": {
                "in_progress": {"set": "started_at"},
                "done": {"set": "completed_at"}
            },
            "events": {
                "on_dispatch_completed": ["OnDispatchCompleted"]
            },
            "timeouts": {
                "in_progress": {"duration": "4h", "clock": "started_at", "on_exhaust": "done"}
            }
        });

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO github_repos (id, display_name, pipeline_config) VALUES ('repo-simple', 'test/simple', ?1)",
                [simple_override.to_string()],
            )
            .unwrap();
        }

        // Resolve effective pipeline for this repo
        let conn = db.lock().unwrap();
        let effective = crate::pipeline::resolve_for_card(&conn, Some("repo-simple"), None);
        drop(conn);

        // Validate the effective pipeline
        assert!(
            effective.validate().is_ok(),
            "simple pipeline override must be valid"
        );

        // Verify states: no "review" or "requested" state
        let state_ids: Vec<&str> = effective.states.iter().map(|s| s.id.as_str()).collect();
        assert!(
            !state_ids.contains(&"review"),
            "simple pipeline has no review state"
        );
        assert!(
            !state_ids.contains(&"requested"),
            "simple pipeline has no requested state"
        );
        assert!(
            state_ids.contains(&"in_progress"),
            "simple pipeline has in_progress"
        );
        assert!(state_ids.contains(&"done"), "simple pipeline has done");

        // Verify terminal state
        assert!(effective.is_terminal("done"), "done is terminal");
        assert!(
            !effective.is_terminal("in_progress"),
            "in_progress is not terminal"
        );

        // Verify dispatchable state
        let dispatchable = effective.dispatchable_states();
        assert_eq!(
            dispatchable,
            vec!["ready"],
            "ready is the only dispatchable state"
        );

        // Verify transitions work: card can go ready → in_progress (gated)
        assert!(
            effective.find_transition("ready", "in_progress").is_some(),
            "ready → in_progress transition must exist"
        );
        assert!(
            effective.find_transition("in_progress", "done").is_some(),
            "in_progress → done transition must exist"
        );
        // No review transition
        assert!(
            effective.find_transition("in_progress", "review").is_none(),
            "in_progress → review must NOT exist in simple pipeline"
        );
    }

    // ── Scenario 9: QA pipeline override with custom qa_test state (#136) ──

    #[test]
    fn scenario_9_qa_pipeline_override_transitions() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        crate::pipeline::ensure_loaded();

        // Store QA pipeline as repo override
        let qa_override = serde_json::json!({
            "states": [
                {"id": "backlog", "label": "Backlog"},
                {"id": "ready", "label": "Ready"},
                {"id": "requested", "label": "Requested"},
                {"id": "in_progress", "label": "In Progress"},
                {"id": "review", "label": "Review"},
                {"id": "qa_test", "label": "QA Test"},
                {"id": "done", "label": "Done", "terminal": true}
            ],
            "transitions": [
                {"from": "backlog", "to": "ready", "type": "free"},
                {"from": "ready", "to": "requested", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "requested", "to": "in_progress", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "in_progress", "to": "review", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "review", "to": "qa_test", "type": "gated", "gates": ["review_passed"]},
                {"from": "review", "to": "in_progress", "type": "gated", "gates": ["review_rework"]},
                {"from": "qa_test", "to": "done", "type": "gated", "gates": ["active_dispatch"]}
            ],
            "gates": {
                "active_dispatch": {"type": "builtin", "check": "has_active_dispatch"},
                "review_passed": {"type": "builtin", "check": "review_verdict_pass"},
                "review_rework": {"type": "builtin", "check": "review_verdict_rework"}
            },
            "hooks": {
                "in_progress": {"on_enter": ["OnCardTransition"], "on_exit": []},
                "review": {"on_enter": ["OnCardTransition", "OnReviewEnter"], "on_exit": []},
                "qa_test": {"on_enter": ["OnCardTransition"], "on_exit": []},
                "done": {"on_enter": ["OnCardTransition", "OnCardTerminal"], "on_exit": []}
            },
            "clocks": {
                "requested": {"set": "requested_at"},
                "in_progress": {"set": "started_at", "mode": "coalesce"},
                "review": {"set": "review_entered_at"},
                "done": {"set": "completed_at"}
            }
        });

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO github_repos (id, display_name, pipeline_config) VALUES ('repo-qa', 'test/qa', ?1)",
                [qa_override.to_string()],
            )
            .unwrap();
        }

        // Resolve and validate
        let conn = db.lock().unwrap();
        let effective = crate::pipeline::resolve_for_card(&conn, Some("repo-qa"), None);
        drop(conn);
        assert!(effective.validate().is_ok(), "QA pipeline must be valid");

        // Key assertion: review → qa_test transition exists (not review → done)
        let review_pass = effective.find_transition("review", "qa_test");
        assert!(
            review_pass.is_some(),
            "review → qa_test must exist in QA pipeline"
        );
        let review_done = effective.find_transition("review", "done");
        assert!(
            review_done.is_none(),
            "review → done must NOT exist in QA pipeline"
        );

        // qa_test → done transition
        let qa_done = effective.find_transition("qa_test", "done");
        assert!(qa_done.is_some(), "qa_test → done must exist");

        // qa_test → in_progress has no explicit rule (force bypass only)
        let qa_rework = effective.find_transition("qa_test", "in_progress");
        assert!(
            qa_rework.is_none(),
            "qa_test → in_progress should not have explicit rule"
        );

        // Verify custom state has hooks
        let qa_hooks = effective.hooks_for_state("qa_test");
        assert!(qa_hooks.is_some(), "qa_test must have hook bindings");
        assert!(
            qa_hooks
                .unwrap()
                .on_enter
                .contains(&"OnCardTransition".to_string()),
            "qa_test on_enter must include OnCardTransition"
        );

        // Test actual card transition through qa_test
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, repo_id, assigned_agent_id, created_at, updated_at) \
                 VALUES ('card-qa', 'QA Card', 'qa_test', 'repo-qa', 'agent-1', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
                 VALUES ('d-qa', 'card-qa', 'agent-1', 'implementation', 'dispatched', 'QA test', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE kanban_cards SET latest_dispatch_id = 'd-qa' WHERE id = 'card-qa'",
                [],
            )
            .unwrap();
        }

        // Force transition qa_test → in_progress (simulating QA failure)
        let result = kanban::transition_status_with_opts(
            &db,
            &engine,
            "card-qa",
            "in_progress",
            "qa-fail",
            true,
        );
        assert!(
            result.is_ok(),
            "qa_test → in_progress force transition must work"
        );
        assert_eq!(get_card_status(&db, "card-qa"), "in_progress");
    }

    // ── Scenario 10: Multi-dispatchable pipeline — kickoff resolves from card's current state ──

    #[test]
    fn scenario_10_multi_dispatchable_kickoff_uses_current_state() {
        let db = test_db();
        seed_agent(&db);
        crate::pipeline::ensure_loaded();

        // Pipeline with TWO dispatchable states, each with a DIFFERENT gated target:
        //   ready      → (gated) → in_progress
        //   qa_ready   → (gated) → qa_test
        // If kickoff resolution ignores old_status, it picks the first one arbitrarily.
        let multi_disp_override = serde_json::json!({
            "states": [
                {"id": "backlog", "label": "Backlog"},
                {"id": "ready", "label": "Ready"},
                {"id": "in_progress", "label": "In Progress"},
                {"id": "review", "label": "Review"},
                {"id": "qa_ready", "label": "QA Ready"},
                {"id": "qa_test", "label": "QA Test"},
                {"id": "done", "label": "Done", "terminal": true}
            ],
            "transitions": [
                {"from": "backlog", "to": "ready", "type": "free"},
                {"from": "ready", "to": "in_progress", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "in_progress", "to": "review", "type": "free"},
                {"from": "review", "to": "qa_ready", "type": "free"},
                {"from": "qa_ready", "to": "qa_test", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "qa_test", "to": "done", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "review", "to": "in_progress", "type": "gated", "gates": ["review_rework"]}
            ],
            "gates": {
                "active_dispatch": {"type": "builtin", "check": "has_active_dispatch"},
                "review_rework": {"type": "builtin", "check": "review_verdict_rework"}
            },
            "hooks": {},
            "clocks": {},
            "events": {},
            "timeouts": {}
        });

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO github_repos (id, display_name, pipeline_config) VALUES ('repo-multi', 'test/multi', ?1)",
                [multi_disp_override.to_string()],
            ).unwrap();
        }

        // Card A: in "ready" — dispatch should kick off to "in_progress"
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, repo_id, assigned_agent_id, created_at, updated_at) \
                 VALUES ('card-multi-a', 'Multi A', 'ready', 'repo-multi', 'agent-1', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
                 VALUES ('d-multi-a-old', 'card-multi-a', 'agent-1', 'implementation', 'completed', 'old', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        }

        let result_a = dispatch::create_dispatch_core_with_id(
            &db,
            "d-multi-a",
            "card-multi-a",
            "agent-1",
            "implementation",
            "[Multi A]",
            &serde_json::json!({}),
        );
        assert!(
            result_a.is_ok(),
            "dispatch for card-multi-a should succeed: {:?}",
            result_a.err()
        );
        assert_eq!(
            get_card_status(&db, "card-multi-a"),
            "in_progress",
            "card in 'ready' must kick off to 'in_progress', not 'qa_test'"
        );

        // Card B: in "qa_ready" — dispatch should kick off to "qa_test"
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, repo_id, assigned_agent_id, created_at, updated_at) \
                 VALUES ('card-multi-b', 'Multi B', 'qa_ready', 'repo-multi', 'agent-1', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
                 VALUES ('d-multi-b-old', 'card-multi-b', 'agent-1', 'implementation', 'completed', 'old', datetime('now'), datetime('now'))",
                [],
            ).unwrap();
        }

        let result_b = dispatch::create_dispatch_core_with_id(
            &db,
            "d-multi-b",
            "card-multi-b",
            "agent-1",
            "implementation",
            "[Multi B]",
            &serde_json::json!({}),
        );
        assert!(
            result_b.is_ok(),
            "dispatch for card-multi-b should succeed: {:?}",
            result_b.err()
        );
        assert_eq!(
            get_card_status(&db, "card-multi-b"),
            "qa_test",
            "card in 'qa_ready' must kick off to 'qa_test', not 'in_progress'"
        );
    }

    // ── #158: card_review_state write centralisation tests ──────────

    /// Helper: query card_review_state for a card.
    fn get_review_state(
        db: &db::Db,
        card_id: &str,
    ) -> Option<(String, Option<String>, Option<String>)> {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT state, last_verdict, last_decision FROM card_review_state WHERE card_id = ?1",
            [card_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .ok()
    }

    /// #158: Typed bridge (review_state_sync) writes card_review_state correctly.
    /// Tests the Rust entrypoint that backs the JS agentdesk.reviewState.sync bridge.
    #[test]
    fn scenario_158a_typed_bridge_writes_review_state() {
        let db = test_db();
        seed_agent(&db);
        seed_card(&db, "card-158a", "review");

        // Step 1: Set reviewing state with round via JSON wrapper (same path as JS bridge)
        let result = crate::engine::ops::review_state_sync(
            &db,
            r#"{"card_id":"card-158a","state":"reviewing","review_round":1}"#,
        );
        assert!(
            result.contains("\"ok\":true"),
            "sync to reviewing must succeed: {result}"
        );

        let (state, _, _) =
            get_review_state(&db, "card-158a").expect("card_review_state row must exist");
        assert_eq!(state, "reviewing", "bridge must create reviewing state");

        // Step 2: Update with verdict
        let result2 = crate::engine::ops::review_state_sync(
            &db,
            r#"{"card_id":"card-158a","state":"suggestion_pending","last_verdict":"improve"}"#,
        );
        assert!(
            result2.contains("\"ok\":true"),
            "sync to suggestion_pending must succeed: {result2}"
        );

        let (state2, verdict, _) = get_review_state(&db, "card-158a").unwrap();
        assert_eq!(state2, "suggestion_pending");
        assert_eq!(verdict.as_deref(), Some("improve"));

        // Step 3: Set to idle — must clear pending_dispatch_id
        let result3 =
            crate::engine::ops::review_state_sync(&db, r#"{"card_id":"card-158a","state":"idle"}"#);
        assert!(
            result3.contains("\"ok\":true"),
            "sync to idle must succeed: {result3}"
        );

        let (state3, _, _) = get_review_state(&db, "card-158a").unwrap();
        assert_eq!(state3, "idle", "bridge must allow idle transition");

        // Step 4: Verify JS bridge is registered and callable (smoke test)
        let engine = test_engine(&db);
        let js_check: String = engine
            .eval_js(r#"typeof agentdesk.reviewState.sync === "function" ? "ok" : "missing""#)
            .unwrap();
        assert_eq!(
            js_check, "ok",
            "agentdesk.reviewState.sync must be registered as a function"
        );
    }

    /// #158: ExecuteSQL intent rejects direct card_review_state mutations.
    #[test]
    fn scenario_158b_execute_sql_intent_rejects_review_state_write() {
        let db = test_db();
        seed_agent(&db);
        seed_card(&db, "card-158b", "review");

        // Attempt INSERT via ExecuteSQL intent — must fail
        let insert_intent = crate::engine::intent::Intent::ExecuteSQL {
            sql: "INSERT INTO card_review_state (card_id, state, updated_at) VALUES ('card-158b', 'idle', datetime('now'))".to_string(),
            params: vec![],
        };
        let result = crate::engine::intent::execute_intents(&db, None, vec![insert_intent]);
        assert_eq!(
            result.errors, 1,
            "INSERT into card_review_state via ExecuteSQL must be rejected"
        );

        // Attempt INSERT OR REPLACE via ExecuteSQL intent — must also fail
        let replace_intent = crate::engine::intent::Intent::ExecuteSQL {
            sql: "INSERT OR REPLACE INTO card_review_state (card_id, state, updated_at) VALUES ('card-158b', 'idle', datetime('now'))".to_string(),
            params: vec![],
        };
        let result_replace =
            crate::engine::intent::execute_intents(&db, None, vec![replace_intent]);
        assert_eq!(
            result_replace.errors, 1,
            "INSERT OR REPLACE into card_review_state via ExecuteSQL must be rejected"
        );

        // Attempt REPLACE INTO via ExecuteSQL intent — must also fail
        let replace_into_intent = crate::engine::intent::Intent::ExecuteSQL {
            sql: "REPLACE INTO card_review_state (card_id, state, updated_at) VALUES ('card-158b', 'idle', datetime('now'))".to_string(),
            params: vec![],
        };
        let result_replace_into =
            crate::engine::intent::execute_intents(&db, None, vec![replace_into_intent]);
        assert_eq!(
            result_replace_into.errors, 1,
            "REPLACE INTO card_review_state via ExecuteSQL must be rejected"
        );

        // Attempt UPDATE via ExecuteSQL intent — must also fail
        let update_intent = crate::engine::intent::Intent::ExecuteSQL {
            sql: "UPDATE card_review_state SET state = 'idle' WHERE card_id = 'card-158b'"
                .to_string(),
            params: vec![],
        };
        let result2 = crate::engine::intent::execute_intents(&db, None, vec![update_intent]);
        assert_eq!(
            result2.errors, 1,
            "UPDATE card_review_state via ExecuteSQL must be rejected"
        );

        // Attempt DELETE via ExecuteSQL intent — must also fail
        let delete_intent = crate::engine::intent::Intent::ExecuteSQL {
            sql: "DELETE FROM card_review_state WHERE card_id = 'card-158b'".to_string(),
            params: vec![],
        };
        let result3 = crate::engine::intent::execute_intents(&db, None, vec![delete_intent]);
        assert_eq!(
            result3.errors, 1,
            "DELETE from card_review_state via ExecuteSQL must be rejected"
        );

        // Verify no row was created
        assert!(
            get_review_state(&db, "card-158b").is_none(),
            "no card_review_state row should exist after blocked intents"
        );
    }

    /// #158: JS db.execute() blocks direct card_review_state SQL writes.
    #[test]
    fn scenario_158c_js_db_execute_blocks_review_state_direct_sql() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-158c", "review");

        // Try INSERT via agentdesk.db.execute — must throw
        let insert_result: String = engine
            .eval_js(r#"
                try {
                    agentdesk.db.execute(
                        "INSERT INTO card_review_state (card_id, state, updated_at) VALUES ('card-158c', 'idle', datetime('now'))"
                    );
                    "unexpected_success"
                } catch(e) {
                    e.message.indexOf("card_review_state") >= 0 ? "blocked" : "wrong_error: " + e.message
                }
            "#)
            .unwrap();
        assert_eq!(
            insert_result, "blocked",
            "JS db.execute INSERT into card_review_state must be blocked"
        );

        // Try INSERT OR REPLACE via agentdesk.db.execute — must throw
        let replace_result: String = engine
            .eval_js(r#"
                try {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO card_review_state (card_id, state, updated_at) VALUES ('card-158c', 'idle', datetime('now'))"
                    );
                    "unexpected_success"
                } catch(e) {
                    e.message.indexOf("card_review_state") >= 0 ? "blocked" : "wrong_error: " + e.message
                }
            "#)
            .unwrap();
        assert_eq!(
            replace_result, "blocked",
            "JS db.execute INSERT OR REPLACE into card_review_state must be blocked"
        );

        // Try REPLACE INTO via agentdesk.db.execute — must throw
        let replace_into_result: String = engine
            .eval_js(r#"
                try {
                    agentdesk.db.execute(
                        "REPLACE INTO card_review_state (card_id, state, updated_at) VALUES ('card-158c', 'idle', datetime('now'))"
                    );
                    "unexpected_success"
                } catch(e) {
                    e.message.indexOf("card_review_state") >= 0 ? "blocked" : "wrong_error: " + e.message
                }
            "#)
            .unwrap();
        assert_eq!(
            replace_into_result, "blocked",
            "JS db.execute REPLACE INTO card_review_state must be blocked"
        );

        // Try UPDATE via agentdesk.db.execute — must throw
        let update_result: String = engine
            .eval_js(r#"
                try {
                    agentdesk.db.execute(
                        "UPDATE card_review_state SET state = 'idle' WHERE card_id = 'card-158c'"
                    );
                    "unexpected_success"
                } catch(e) {
                    e.message.indexOf("card_review_state") >= 0 ? "blocked" : "wrong_error: " + e.message
                }
            "#)
            .unwrap();
        assert_eq!(
            update_result, "blocked",
            "JS db.execute UPDATE on card_review_state must be blocked"
        );

        // Try DELETE via agentdesk.db.execute — must throw
        let delete_result: String = engine
            .eval_js(r#"
                try {
                    agentdesk.db.execute(
                        "DELETE FROM card_review_state WHERE card_id = 'card-158c'"
                    );
                    "unexpected_success"
                } catch(e) {
                    e.message.indexOf("card_review_state") >= 0 ? "blocked" : "wrong_error: " + e.message
                }
            "#)
            .unwrap();
        assert_eq!(
            delete_result, "blocked",
            "JS db.execute DELETE on card_review_state must be blocked"
        );

        // Verify no row was created by blocked operations
        assert!(
            get_review_state(&db, "card-158c").is_none(),
            "no card_review_state row should exist after blocked JS db.execute"
        );
    }

    /// #158: Full review cycle — card transitions sync card_review_state via single entrypoint.
    #[test]
    fn scenario_158d_review_cycle_syncs_canonical_state() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-158d", "in_progress");

        // Create implementation dispatch and complete it to trigger review transition
        seed_dispatch(&db, "d-158d", "card-158d", "implementation", "pending");
        seed_assistant_response_for_dispatch(&db, "d-158d", "implemented review target");

        let result = dispatch::complete_dispatch(
            &db,
            &engine,
            "d-158d",
            &serde_json::json!({"completion_source": "test_harness"}),
        );
        assert!(
            result.is_ok(),
            "complete_dispatch should succeed: {:?}",
            result.err()
        );

        // Card should be in review
        assert_eq!(get_card_status(&db, "card-158d"), "review");

        // card_review_state must be "reviewing" (synced via single entrypoint during transition)
        let (state, _, _) = get_review_state(&db, "card-158d")
            .expect("card_review_state must exist after review transition");
        assert_eq!(
            state, "reviewing",
            "canonical review state must be 'reviewing' after entering review"
        );

        // Force card to done — review state must reset to idle
        assert!(
            kanban::transition_status_with_opts(&db, &engine, "card-158d", "done", "test", true)
                .is_ok()
        );
        assert_eq!(get_card_status(&db, "card-158d"), "done");

        let (state2, _, _) = get_review_state(&db, "card-158d").unwrap();
        assert_eq!(
            state2, "idle",
            "canonical review state must be 'idle' after terminal transition"
        );
    }

    /// #158: review-automation.js OnReviewEnter hook uses reviewState.sync bridge.
    #[test]
    fn scenario_158e_on_review_enter_js_hook_syncs_canonical_state() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-158e", "review");
        seed_completed_work_dispatch_for_review(&db, "impl-158e", "card-158e", "implementation");
        db.lock()
            .unwrap()
            .execute(
                "UPDATE kanban_cards SET blocked_reason = 'orphan review — dispatch 없음' WHERE id = 'card-158e'",
                [],
            )
            .unwrap();

        kanban::fire_enter_hooks(&db, &engine, "card-158e", "review");

        let (state, _, _) = get_review_state(&db, "card-158e")
            .expect("card_review_state must exist after OnReviewEnter hook");
        assert_eq!(
            state, "reviewing",
            "OnReviewEnter policy hook must sync canonical review state via bridge"
        );

        let conn = db.lock().unwrap();
        let review_round: i64 = conn
            .query_row(
                "SELECT review_round FROM kanban_cards WHERE id = 'card-158e'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(review_round, 1, "OnReviewEnter must increment review_round");

        let review_dispatch_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches \
                 WHERE kanban_card_id = 'card-158e' AND dispatch_type = 'review' \
                 AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            review_dispatch_count, 1,
            "OnReviewEnter must create exactly one pending review dispatch"
        );

        let blocked_reason: Option<String> = conn
            .query_row(
                "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-158e'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            blocked_reason.is_none(),
            "OnReviewEnter must clear stale blocked_reason from prior review rounds"
        );
    }

    #[test]
    fn scenario_615_on_review_enter_skips_terminal_card() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-615-terminal", "done");
        seed_completed_work_dispatch_for_review(
            &db,
            "impl-615-terminal",
            "card-615-terminal",
            "implementation",
        );
        db.lock()
            .unwrap()
            .execute(
                "UPDATE kanban_cards SET review_status = 'reviewing', blocked_reason = 'stale review dispatch' WHERE id = 'card-615-terminal'",
                [],
            )
            .unwrap();

        kanban::fire_enter_hooks(&db, &engine, "card-615-terminal", "review");

        let conn = db.lock().unwrap();
        let review_dispatch_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches \
                 WHERE kanban_card_id = 'card-615-terminal' AND dispatch_type = 'review' \
                 AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            review_dispatch_count, 0,
            "#615: terminal cards must not spawn new review dispatches on stale OnReviewEnter"
        );

        let blocked_reason: Option<String> = conn
            .query_row(
                "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-615-terminal'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            blocked_reason.is_none(),
            "#615: terminal OnReviewEnter must clear stale blocked_reason"
        );
        drop(conn);

        assert_eq!(get_card_status(&db, "card-615-terminal"), "done");
        assert_eq!(
            count_active_dispatches_by_type(&db, "card-615-terminal", "review"),
            0,
            "#615: terminal card must not create a stale review dispatch"
        );
        assert_eq!(
            review_state_value(&db, "card-615-terminal").as_deref(),
            Some("idle"),
            "#615: terminal OnReviewEnter must keep canonical review state idle"
        );
    }

    #[test]
    fn scenario_335_on_review_enter_reuses_round_without_new_completed_work() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-335-reopen", "review");
        seed_completed_work_dispatch_for_review(
            &db,
            "impl-335-reopen",
            "card-335-reopen",
            "implementation",
        );
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET review_round = 1 WHERE id = 'card-335-reopen'",
                [],
            )
            .unwrap();
        }

        kanban::fire_enter_hooks(&db, &engine, "card-335-reopen", "review");

        let conn = db.lock().unwrap();
        let review_round: i64 = conn
            .query_row(
                "SELECT review_round FROM kanban_cards WHERE id = 'card-335-reopen'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            review_round, 1,
            "#335: reopen without fresh implementation/rework must reuse the current review_round"
        );
        drop(conn);

        assert_eq!(
            latest_dispatch_title(&db, "card-335-reopen", "review").as_deref(),
            Some("[Review R1] card-335-reopen")
        );
        assert_eq!(
            count_active_dispatches_by_type(&db, "card-335-reopen", "review"),
            1
        );
    }

    #[test]
    fn scenario_335_on_review_enter_advances_round_after_completed_rework() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-335-rereview", "review");
        seed_completed_work_dispatch_for_review(
            &db,
            "impl-335-rereview",
            "card-335-rereview",
            "implementation",
        );
        seed_completed_work_dispatch_for_review(
            &db,
            "rework-335-rereview",
            "card-335-rereview",
            "rework",
        );
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET review_round = 1 WHERE id = 'card-335-rereview'",
                [],
            )
            .unwrap();
        }

        kanban::fire_enter_hooks(&db, &engine, "card-335-rereview", "review");

        let conn = db.lock().unwrap();
        let review_round: i64 = conn
            .query_row(
                "SELECT review_round FROM kanban_cards WHERE id = 'card-335-rereview'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            review_round, 2,
            "#335: completed rework must advance review_round for the next review cycle"
        );
        drop(conn);

        assert_eq!(
            latest_dispatch_title(&db, "card-335-rereview", "review").as_deref(),
            Some("[Review R2] card-335-rereview")
        );
        assert_eq!(
            count_active_dispatches_by_type(&db, "card-335-rereview", "review"),
            1
        );
    }

    #[test]
    fn scenario_review_disabled_on_review_enter_completes_card() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-review-disabled", "review");
        set_config_key(&db, "review_enabled", json!(false));

        kanban::fire_enter_hooks(&db, &engine, "card-review-disabled", "review");

        assert_eq!(get_card_status(&db, "card-review-disabled"), "done");

        {
            let conn = db.lock().unwrap();
            let completed_at: Option<String> = conn
                .query_row(
                    "SELECT completed_at FROM kanban_cards WHERE id = 'card-review-disabled'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(
                completed_at.is_some(),
                "review-disabled completion must still fire OnCardTerminal side-effects"
            );

            let review_round: i64 = conn
                .query_row(
                    "SELECT COALESCE(review_round, 0) FROM kanban_cards WHERE id = 'card-review-disabled'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                review_round, 0,
                "review-disabled path must not increment review_round"
            );

            let review_dispatch_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM task_dispatches \
                     WHERE kanban_card_id = 'card-review-disabled' AND dispatch_type = 'review'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                review_dispatch_count, 0,
                "review-disabled path must not create review dispatch"
            );

            let blocked_reason: Option<String> = conn
                .query_row(
                    "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-review-disabled'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(
                blocked_reason.is_none(),
                "review-disabled completion must not leave blocked_reason"
            );
        }

        let (state, _, _) = get_review_state(&db, "card-review-disabled")
            .expect("terminal transition must sync canonical review state");
        assert_eq!(state, "idle");
    }

    #[test]
    fn scenario_review_disabled_on_review_enter_closes_issue_and_creates_phase_gate_for_single_phase_run()
     {
        let gh = install_mock_gh(&[MockGhReply {
            key: "issue:close",
            contains: Some("--repo test/repo"),
            stdout: "",
        }]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(
            &db,
            "card-review-disabled-gh",
            "review",
            "test/repo",
            482,
            None,
        );
        ensure_auto_queue_tables(&db);
        set_config_key(&db, "review_enabled", json!(false));

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
                 VALUES ('run-review-disabled-gh', 'test/repo', 'agent-1', 'active', datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, dispatch_id, dispatched_at, created_at) \
                 VALUES ('entry-review-disabled-gh', 'run-review-disabled-gh', 'card-review-disabled-gh', 'agent-1', 'dispatched', 1, 'review-disabled-gh-dispatch', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        }

        kanban::fire_enter_hooks(&db, &engine, "card-review-disabled-gh", "review");

        assert_eq!(get_card_status(&db, "card-review-disabled-gh"), "done");

        let conn = db.lock().unwrap();
        let run_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_runs WHERE id = 'run-review-disabled-gh'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let phase_gate_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches \
                 WHERE kanban_card_id = 'card-review-disabled-gh' \
                   AND dispatch_type = 'phase-gate' \
                   AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let entry_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-review-disabled-gh'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);

        assert_eq!(
            entry_status, "done",
            "terminal review-disabled transition must close the active auto-queue entry"
        );
        let phase_gate_json = phase_gate_state(&db, "run-review-disabled-gh", 0)
            .expect("single-phase review-disabled run must persist gate state");
        assert_eq!(
            run_status, "paused",
            "review-disabled JS terminal path must pause the run for single-phase gate"
        );
        assert_eq!(
            phase_gate_count, 1,
            "review-disabled single-phase terminal transition must create a phase-gate dispatch"
        );
        assert_eq!(phase_gate_json["status"], "pending");
        assert_eq!(phase_gate_json["batch_phase"], 0);
        assert_eq!(phase_gate_json["next_phase"], serde_json::Value::Null);
        assert_eq!(phase_gate_json["final_phase"], true);

        let log = gh_log(&gh);
        assert!(
            log.contains("issue close 482 --repo test/repo"),
            "review-disabled JS terminal path must close the linked GitHub issue"
        );
    }

    #[test]
    fn scenario_review_disabled_on_review_enter_creates_phase_gate_for_multi_phase_run() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-review-disabled-phase", "review");
        seed_card(&db, "card-next-phase", "ready");
        set_config_key(&db, "review_enabled", json!(false));

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
                 VALUES ('run-review-disabled-phase', 'test/repo', 'agent-1', 'active', datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, batch_phase, created_at, dispatched_at) \
                 VALUES ('entry-review-disabled-phase', 'run-review-disabled-phase', 'card-review-disabled-phase', 'agent-1', 'dispatched', 0, 1, datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, batch_phase, created_at) \
                 VALUES ('entry-next-phase', 'run-review-disabled-phase', 'card-next-phase', 'agent-1', 'pending', 1, 2, datetime('now'))",
                [],
            )
            .unwrap();
        }

        kanban::fire_enter_hooks(&db, &engine, "card-review-disabled-phase", "review");

        assert_eq!(get_card_status(&db, "card-review-disabled-phase"), "done");

        let conn = db.lock().unwrap();
        let run_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_runs WHERE id = 'run-review-disabled-phase'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let phase_gate_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches \
                 WHERE kanban_card_id = 'card-review-disabled-phase' \
                   AND dispatch_type = 'phase-gate' \
                   AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);

        let phase_gate_json = phase_gate_state(&db, "run-review-disabled-phase", 1)
            .expect("phase gate state must exist");
        assert_eq!(
            run_status, "paused",
            "multi-phase review-disabled terminal transition must pause the run for phase gate"
        );
        assert_eq!(
            phase_gate_count, 1,
            "multi-phase review-disabled terminal transition must create a phase-gate dispatch"
        );
        assert_eq!(phase_gate_json["status"], "pending");
        assert_eq!(phase_gate_json["batch_phase"], 1);
        assert_eq!(phase_gate_json["next_phase"], 2);
    }

    #[test]
    fn continue_run_after_entry_creates_phase_gate_for_single_phase_run() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        ensure_auto_queue_tables(&db);
        seed_card(&db, "card-single-phase-gate", "done");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
                 VALUES ('run-single-phase-gate', 'test/repo', 'agent-1', 'active', datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, batch_phase, created_at, completed_at) \
                 VALUES ('entry-single-phase-gate', 'run-single-phase-gate', 'card-single-phase-gate', 'agent-1', 'done', 0, 0, datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        }

        engine
            .eval_js::<String>(
                r#"(() => {
                    continueRunAfterEntry("run-single-phase-gate", "agent-1", 0, 0, "card-single-phase-gate");
                    return "ok";
                })()"#,
            )
            .expect("single-phase continueRunAfterEntry should evaluate");

        let conn = db.lock().unwrap();
        let run_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_runs WHERE id = 'run-single-phase-gate'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let phase_gate_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches \
                 WHERE kanban_card_id = 'card-single-phase-gate' \
                   AND dispatch_type = 'phase-gate' \
                   AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);

        let phase_gate_json = phase_gate_state(&db, "run-single-phase-gate", 0)
            .expect("phase gate state must exist for single-phase runs");
        assert_eq!(
            run_status, "paused",
            "single-phase completion must pause the run for phase gate"
        );
        assert_eq!(
            phase_gate_count, 1,
            "single-phase completion must create a phase-gate dispatch"
        );
        assert_eq!(phase_gate_json["status"], "pending");
        assert_eq!(phase_gate_json["batch_phase"], 0);
        assert_eq!(phase_gate_json["next_phase"], serde_json::Value::Null);
        assert_eq!(phase_gate_json["final_phase"], true);
    }

    #[test]
    fn deploy_gate_creation_skips_when_phase_still_has_live_entries() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        ensure_auto_queue_tables(&db);
        seed_card(&db, "card-deploy-anchor", "done");
        seed_card(&db, "card-deploy-live", "ready");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status, deploy_phases, created_at) \
                 VALUES ('run-deploy-live-phase', 'test/repo', 'agent-1', 'active', '[1]', datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, batch_phase, priority_rank, created_at) \
                 VALUES ('entry-deploy-live', 'run-deploy-live-phase', 'card-deploy-live', 'agent-1', 'pending', 1, 0, datetime('now'))",
                [],
            )
            .unwrap();
        }

        engine
            .eval_js::<String>(
                r#"(() => {
                    _createDeployGateDispatch("run-deploy-live-phase", 1, 2, false, "card-deploy-anchor");
                    return "ok";
                })()"#,
            )
            .expect("deploy gate helper should evaluate");

        let run_status: String = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT status FROM auto_queue_runs WHERE id = 'run-deploy-live-phase'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };

        assert_eq!(run_status, "active");
        assert!(
            phase_gate_state(&db, "run-deploy-live-phase", 1).is_none(),
            "deploy gate must not persist while the phase still has live entries"
        );
    }

    #[tokio::test]
    async fn auto_queue_phase_gate_blocks_resume_then_completes_final_run_on_pass() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        ensure_auto_queue_tables(&db);
        seed_card(&db, "card-phase-final", "done");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
                 VALUES ('run-phase-final', 'test/repo', 'agent-1', 'paused', datetime('now'))",
                [],
            )
            .unwrap();
        }

        let phase_gate_dispatch = dispatch::create_dispatch(
            &db,
            &engine,
            "card-phase-final",
            "agent-1",
            "phase-gate",
            "[phase-gate P2] Final",
            &json!({
                "auto_queue": true,
                "sidecar_dispatch": true,
                "phase_gate": {
                    "run_id": "run-phase-final",
                    "batch_phase": 2,
                    "next_phase": serde_json::Value::Null,
                    "final_phase": true,
                    "pass_verdict": "phase_gate_passed",
                    "expected_gate_count": 1
                }
            }),
        )
        .expect("phase gate dispatch should be created");
        let phase_gate_dispatch_id = phase_gate_dispatch["id"].as_str().unwrap().to_string();
        set_phase_gate_state(
            &db,
            "run-phase-final",
            2,
            "pending",
            &[phase_gate_dispatch_id.as_str()],
            None,
            true,
            Some("card-phase-final"),
            None,
            None,
        );

        let state = AppState::test_state(db.clone(), engine.clone());
        let (resume_status, resume_body) =
            crate::server::routes::auto_queue::resume_run(axum::extract::State(state)).await;
        assert_eq!(resume_status, axum::http::StatusCode::OK);
        assert_eq!(resume_body.0["resumed_runs"].as_u64(), Some(0));
        assert_eq!(resume_body.0["blocked_runs"].as_u64(), Some(1));
        assert_eq!(
            phase_gate_state(&db, "run-phase-final", 2).is_some(),
            true,
            "resume must leave the blocking phase gate untouched"
        );

        let completed = dispatch::complete_dispatch(
            &db,
            &engine,
            &phase_gate_dispatch_id,
            &json!({
                "verdict": "phase_gate_passed",
                "summary": "phase gate approved"
            }),
        )
        .expect("phase gate completion should succeed");
        assert_eq!(completed["status"], "completed");

        let run_status: String = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT status FROM auto_queue_runs WHERE id = 'run-phase-final'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            run_status, "completed",
            "final phase-gate pass should resume and complete the paused run"
        );
        assert!(
            phase_gate_state(&db, "run-phase-final", 2).is_none(),
            "successful gate completion must clear the persisted phase gate state"
        );
    }

    // #698: phase 0 is the default starting phase per default-pipeline.yaml.
    // A falsy guard on `gate.batch_phase` (the pre-fix behavior) would ignore
    // phase-0 gate completions, stranding the run as `paused` forever.
    #[tokio::test]
    async fn auto_queue_phase_gate_completes_for_batch_phase_zero() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        ensure_auto_queue_tables(&db);
        seed_card(&db, "card-phase-zero", "done");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
                 VALUES ('run-phase-zero', 'test/repo', 'agent-1', 'paused', datetime('now'))",
                [],
            )
            .unwrap();
        }

        let phase_gate_dispatch = dispatch::create_dispatch(
            &db,
            &engine,
            "card-phase-zero",
            "agent-1",
            "phase-gate",
            "[phase-gate P0] Default start",
            &json!({
                "auto_queue": true,
                "sidecar_dispatch": true,
                "phase_gate": {
                    "run_id": "run-phase-zero",
                    "batch_phase": 0,
                    "next_phase": 1,
                    "final_phase": false,
                    "pass_verdict": "phase_gate_passed",
                    "expected_gate_count": 1
                }
            }),
        )
        .expect("phase 0 gate dispatch should be created");
        let phase_gate_dispatch_id = phase_gate_dispatch["id"].as_str().unwrap().to_string();
        set_phase_gate_state(
            &db,
            "run-phase-zero",
            0,
            "pending",
            &[phase_gate_dispatch_id.as_str()],
            Some(1),
            false,
            Some("card-phase-zero"),
            None,
            None,
        );

        assert!(
            phase_gate_state(&db, "run-phase-zero", 0).is_some(),
            "seeded phase 0 gate state must exist before completion"
        );

        let completed = dispatch::complete_dispatch(
            &db,
            &engine,
            &phase_gate_dispatch_id,
            &json!({
                "verdict": "phase_gate_passed",
                "summary": "phase 0 gate approved"
            }),
        )
        .expect("phase 0 gate completion should succeed");
        assert_eq!(completed["status"], "completed");

        let run_status: String = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT status FROM auto_queue_runs WHERE id = 'run-phase-zero'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        assert_ne!(
            run_status, "paused",
            "phase 0 gate pass must not leave the run paused (#698)"
        );
        assert!(
            phase_gate_state(&db, "run-phase-zero", 0).is_none(),
            "phase 0 gate completion must clear the persisted phase gate state (#698)"
        );
    }

    #[test]
    fn auto_queue_cancel_releases_slots_and_clears_linked_sessions() {
        let db = test_db();
        seed_agent(&db);
        ensure_auto_queue_tables(&db);
        seed_card(&db, "card-cancel-live", "in_progress");
        seed_card(&db, "card-cancel-pending", "ready");
        seed_dispatch(
            &db,
            "dispatch-cancel-live",
            "card-cancel-live",
            "implementation",
            "dispatched",
        );

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
                 VALUES ('run-cancel-cleanup', 'repo-1', 'agent-1', 'active', datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, priority_rank, dispatched_at, created_at) \
                 VALUES ('entry-cancel-live', 'run-cancel-cleanup', 'card-cancel-live', 'agent-1', 'dispatched', 'dispatch-cancel-live', 0, 0, datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, slot_index, priority_rank, created_at) \
                 VALUES ('entry-cancel-pending', 'run-cancel-cleanup', 'card-cancel-pending', 'agent-1', 'pending', 0, 1, datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_slots (agent_id, slot_index, assigned_run_id, assigned_thread_group, thread_id_map) \
                 VALUES ('agent-1', 0, 'run-cancel-cleanup', 0, '{\"main\":\"9001\"}')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO sessions (session_key, agent_id, provider, status, thread_channel_id, active_dispatch_id, last_heartbeat) \
                 VALUES ('test-slot-session', 'agent-1', 'codex', 'working', '9001', 'dispatch-cancel-live', datetime('now'))",
                [],
            )
            .unwrap();
        }

        let conn = db.separate_conn().expect("separate conn");
        let body = crate::server::routes::auto_queue::cancel_with_conn(None, &conn);
        assert_eq!(body["cancelled_runs"].as_u64(), Some(1));
        assert_eq!(body["cancelled_dispatches"].as_u64(), Some(1));
        assert_eq!(body["cancelled_entries"].as_u64(), Some(2));
        assert_eq!(body["rolled_back_cards"].as_u64(), Some(1));
        assert_eq!(body["released_slots"].as_u64(), Some(1));
        assert_eq!(body["cleared_slot_sessions"].as_u64(), Some(1));
        drop(conn);

        let conn = db.lock().unwrap();
        let run_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_runs WHERE id = 'run-cancel-cleanup'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let (card_status, latest_dispatch_id): (String, Option<String>) = conn
            .query_row(
                "SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = 'card-cancel-live'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let entry_statuses: Vec<(String, String)> = conn
            .prepare(
                "SELECT id, status FROM auto_queue_entries WHERE run_id = 'run-cancel-cleanup' ORDER BY id ASC",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        let (assigned_run_id, assigned_thread_group): (Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT assigned_run_id, assigned_thread_group FROM auto_queue_slots \
                 WHERE agent_id = 'agent-1' AND slot_index = 0",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let (session_status, active_dispatch_id, session_info): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT status, active_dispatch_id, session_info FROM sessions WHERE session_key = 'test-slot-session'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        let dispatch_status: String = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = 'dispatch-cancel-live'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(run_status, "cancelled");
        assert_eq!(
            entry_statuses,
            vec![
                ("entry-cancel-live".to_string(), "skipped".to_string()),
                ("entry-cancel-pending".to_string(), "skipped".to_string()),
            ],
            "cancel should skip both in-flight and pending entries after cleanup"
        );
        assert_eq!(dispatch_status, "cancelled");
        assert_eq!(
            card_status, "ready",
            "cancelled run should roll back an in-progress card to ready"
        );
        assert!(
            latest_dispatch_id.is_none(),
            "cancelled run rollback should clear latest_dispatch_id"
        );
        assert!(assigned_run_id.is_none());
        assert!(assigned_thread_group.is_none());
        assert_eq!(session_status, "idle");
        assert!(active_dispatch_id.is_none());
        assert_eq!(session_info.as_deref(), Some("Slot thread reset"));
    }

    #[test]
    fn auto_queue_cancel_rolls_back_requested_and_in_progress_cards_but_not_review() {
        let db = test_db();
        seed_agent(&db);
        ensure_auto_queue_tables(&db);
        seed_card(&db, "card-cancel-requested", "requested");
        seed_card(&db, "card-cancel-progress", "in_progress");
        seed_card(&db, "card-cancel-review", "review");
        seed_dispatch(
            &db,
            "dispatch-cancel-requested",
            "card-cancel-requested",
            "implementation",
            "dispatched",
        );
        seed_dispatch(
            &db,
            "dispatch-cancel-progress",
            "card-cancel-progress",
            "implementation",
            "dispatched",
        );
        seed_dispatch(
            &db,
            "dispatch-cancel-review",
            "card-cancel-review",
            "review",
            "dispatched",
        );

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards
                 SET review_status = 'pending', blocked_reason = 'manual:waiting'
                 WHERE id IN ('card-cancel-requested', 'card-cancel-progress')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
                 VALUES ('run-cancel-rollback', 'repo-1', 'agent-1', 'active', datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, priority_rank, dispatched_at, created_at) \
                 VALUES ('entry-cancel-requested', 'run-cancel-rollback', 'card-cancel-requested', 'agent-1', 'dispatched', 'dispatch-cancel-requested', 0, datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, priority_rank, dispatched_at, created_at) \
                 VALUES ('entry-cancel-progress', 'run-cancel-rollback', 'card-cancel-progress', 'agent-1', 'dispatched', 'dispatch-cancel-progress', 1, datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, priority_rank, dispatched_at, created_at) \
                 VALUES ('entry-cancel-review', 'run-cancel-rollback', 'card-cancel-review', 'agent-1', 'dispatched', 'dispatch-cancel-review', 2, datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        }

        let conn = db.separate_conn().expect("separate conn");
        let body = crate::server::routes::auto_queue::cancel_with_conn(None, &conn);
        assert_eq!(body["cancelled_runs"].as_u64(), Some(1));
        assert_eq!(body["cancelled_dispatches"].as_u64(), Some(3));
        assert_eq!(body["cancelled_entries"].as_u64(), Some(3));
        assert_eq!(body["rolled_back_cards"].as_u64(), Some(2));
        drop(conn);

        let conn = db.lock().unwrap();
        let requested_card: (String, Option<String>, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT status, review_status, blocked_reason, latest_dispatch_id
                 FROM kanban_cards WHERE id = 'card-cancel-requested'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        let in_progress_card: (String, Option<String>, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT status, review_status, blocked_reason, latest_dispatch_id
                 FROM kanban_cards WHERE id = 'card-cancel-progress'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        let review_card: (String, Option<String>, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT status, review_status, blocked_reason, latest_dispatch_id
                 FROM kanban_cards WHERE id = 'card-cancel-review'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        let requested_live_dispatches: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches
                 WHERE kanban_card_id = 'card-cancel-requested' AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let in_progress_live_dispatches: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches
                 WHERE kanban_card_id = 'card-cancel-progress' AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(
            requested_card.0, "ready",
            "cancelled requested card should roll back to ready"
        );
        assert_eq!(
            in_progress_card.0, "ready",
            "cancelled in-progress card should roll back to ready"
        );
        assert_eq!(
            requested_card.1, None,
            "rollback should clear requested-card review_status"
        );
        assert_eq!(
            in_progress_card.1, None,
            "rollback should clear in-progress-card review_status"
        );
        assert_eq!(
            requested_card.2, None,
            "rollback should clear requested-card blocked_reason"
        );
        assert_eq!(
            in_progress_card.2, None,
            "rollback should clear in-progress-card blocked_reason"
        );
        assert!(
            requested_card.3.is_none(),
            "rollback should clear requested-card latest_dispatch_id"
        );
        assert!(
            in_progress_card.3.is_none(),
            "rollback should clear in-progress-card latest_dispatch_id"
        );
        assert_eq!(
            requested_live_dispatches, 0,
            "requested card should not keep a live dispatch after run cancel"
        );
        assert_eq!(
            in_progress_live_dispatches, 0,
            "in-progress card should not keep a live dispatch after run cancel"
        );
        assert_eq!(
            review_card.0, "review",
            "review card must not be rolled back by run cancel"
        );
        assert!(
            review_card.3.is_some(),
            "review card should keep its latest_dispatch_id because rollback is skipped"
        );
    }

    #[test]
    fn scenario_single_provider_review_auto_approves_without_round() {
        let db = test_db();
        let engine = test_engine(&db);
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT OR IGNORE INTO agents (id, name, provider, discord_channel_id, discord_channel_cc) \
                 VALUES ('agent-1', 'Single Provider Agent', 'claude', '111', '111')",
                [],
            )
            .unwrap();
        }
        seed_card(&db, "card-single-provider", "review");

        kanban::fire_enter_hooks(&db, &engine, "card-single-provider", "review");

        assert_eq!(get_card_status(&db, "card-single-provider"), "done");

        {
            let conn = db.lock().unwrap();
            let review_round: i64 = conn
                .query_row(
                    "SELECT COALESCE(review_round, 0) FROM kanban_cards WHERE id = 'card-single-provider'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                review_round, 0,
                "single-provider auto-approval must not increment review_round without a real review"
            );

            let review_dispatch_count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM task_dispatches \
                     WHERE kanban_card_id = 'card-single-provider' AND dispatch_type = 'review'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                review_dispatch_count, 0,
                "single-provider auto-approval must not create review dispatch"
            );

            let blocked_reason: Option<String> = conn
                .query_row(
                    "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-single-provider'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(
                blocked_reason.is_none(),
                "single-provider auto-approval must not leave blocked_reason"
            );
        }

        let (state, _, _) = get_review_state(&db, "card-single-provider")
            .expect("terminal transition must sync canonical review state");
        assert_eq!(state, "idle");
    }

    #[test]
    fn scenario_245_review_dispatch_uses_canonical_assigned_agent_id() {
        let (_repo, _repo_guard) = setup_test_repo();
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-245", "review");

        kanban::fire_enter_hooks(&db, &engine, "card-245", "review");

        let conn = db.lock().unwrap();
        let to_agent_id: String = conn
            .query_row(
                "SELECT to_agent_id FROM task_dispatches \
                 WHERE kanban_card_id = 'card-245' AND dispatch_type = 'review' \
                 ORDER BY rowid DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            to_agent_id, "agent-1",
            "review dispatch must target canonical assigned_agent_id, not a channel alias"
        );
    }

    // ── #195: review-decision accept creates rework dispatch ──────────
    //
    // Verifies that when an agent accepts review feedback via POST /api/review-decision,
    // a rework dispatch is automatically created and the card transitions to the
    // rework target state (in_progress), NOT directly to review.
    // This prevents the pipeline from getting stuck when the accept decision
    // was the only active dispatch for the card.

    #[tokio::test]
    async fn scenario_195_accept_creates_rework_dispatch() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-195", "review");

        // Set up a pending review-decision dispatch (simulates the state after
        // counter-model review found suggestions and agent received decision prompt)
        seed_dispatch(&db, "rd-195", "card-195", "review-decision", "pending");

        // Set up card_review_state with pending_dispatch_id pointing to the review-decision
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO card_review_state (card_id, state, pending_dispatch_id) \
                 VALUES ('card-195', 'suggestion_pending', 'rd-195')",
                [],
            )
            .unwrap();
        }

        let state = AppState {
            db: db.clone(),
            pg_pool: None,
            engine,
            config: std::sync::Arc::new(crate::config::Config::default()),
            broadcast_tx: crate::server::ws::new_broadcast(),
            batch_buffer: crate::server::ws::spawn_batch_flusher(crate::server::ws::new_broadcast()),
            health_registry: None,
        };

        // Call the review-decision handler with accept
        let (status, json) = crate::server::routes::review_verdict::submit_review_decision(
            axum::extract::State(state),
            axum::Json(crate::server::routes::review_verdict::ReviewDecisionBody {
                card_id: "card-195".to_string(),
                decision: "accept".to_string(),
                comment: None,
                dispatch_id: Some("rd-195".to_string()),
            }),
        )
        .await;

        assert_eq!(
            status,
            axum::http::StatusCode::OK,
            "accept should succeed: {json:?}"
        );
        assert_eq!(
            json.0["rework_dispatch_created"], true,
            "rework_dispatch_created must be true in response"
        );

        // Review-decision dispatch must be completed
        assert_eq!(
            get_dispatch_status(&db, "rd-195"),
            "completed",
            "review-decision dispatch must be completed after accept"
        );

        // Card must be in rework target state (in_progress), NOT review
        let card_status = get_card_status(&db, "card-195");
        assert_eq!(
            card_status, "in_progress",
            "#195: accept must transition card to rework target (in_progress), not review"
        );

        // A rework dispatch must exist for this card
        let conn = db.lock().unwrap();
        let rework_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches \
                 WHERE kanban_card_id = 'card-195' AND dispatch_type = 'rework' \
                 AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            rework_count, 1,
            "#195: accept must create exactly 1 rework dispatch"
        );

        // Verify canonical review state is rework_pending
        let review_state: Option<String> = conn
            .query_row(
                "SELECT state FROM card_review_state WHERE card_id = 'card-195'",
                [],
                |row| row.get(0),
            )
            .ok()
            .flatten();
        assert_eq!(
            review_state.as_deref(),
            Some("rework_pending"),
            "#195: canonical review state must be 'rework_pending' after accept"
        );
    }

    #[tokio::test]
    async fn scenario_339_accept_skip_rework_auto_approves_without_alternate_reviewer() {
        let _worktree_override = WorktreeCommitOverrideGuard::set("bbb2222");
        let db = test_db();
        let engine = test_engine(&db);
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) \
                 VALUES ('agent-nocm', 'Agent No Counter', '123', '')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, \
                 review_status, suggestion_pending_at, github_issue_number, created_at, updated_at) \
                 VALUES ('card-339-skip', 'Skip Rework Fallback', 'review', 'agent-nocm', \
                 'rd-339-skip', 'suggestion_pending', datetime('now', '-10 minutes'), 246, \
                 datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, \
                 title, context, completed_at, created_at, updated_at) \
                 VALUES ('review-339-skip', 'card-339-skip', 'agent-nocm', 'review', \
                 'completed', '[Review R1]', '{\"reviewed_commit\":\"aaa1111\"}', \
                 datetime('now', '-5 minutes'), datetime('now', '-10 minutes'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, \
                 title, created_at, updated_at) \
                 VALUES ('rd-339-skip', 'card-339-skip', 'agent-nocm', 'review-decision', \
                 'pending', '[Decision] card-339-skip', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO card_review_state (card_id, state, pending_dispatch_id) \
                 VALUES ('card-339-skip', 'suggestion_pending', 'rd-339-skip')",
                [],
            )
            .unwrap();
        }

        let state = AppState {
            db: db.clone(),
            pg_pool: None,
            engine,
            config: std::sync::Arc::new(crate::config::Config::default()),
            broadcast_tx: crate::server::ws::new_broadcast(),
            batch_buffer: crate::server::ws::spawn_batch_flusher(crate::server::ws::new_broadcast()),
            health_registry: None,
        };

        let (status, json) = crate::server::routes::review_verdict::submit_review_decision(
            axum::extract::State(state),
            axum::Json(crate::server::routes::review_verdict::ReviewDecisionBody {
                card_id: "card-339-skip".to_string(),
                decision: "accept".to_string(),
                comment: None,
                dispatch_id: Some("rd-339-skip".to_string()),
            }),
        )
        .await;

        assert_eq!(
            status,
            axum::http::StatusCode::OK,
            "single-provider skip_rework accept should auto-approve: {json:?}"
        );
        assert_eq!(json.0["direct_review_created"], false);
        assert_eq!(json.0["rework_dispatch_created"], false);
        assert_eq!(json.0["review_auto_approved"], true);
        assert_eq!(get_dispatch_status(&db, "rd-339-skip"), "cancelled");
        assert_eq!(get_card_status(&db, "card-339-skip"), "done");
        assert_eq!(
            count_active_dispatches_by_type(&db, "card-339-skip", "review"),
            0,
            "single-provider auto-approve must not leave an active review dispatch behind"
        );
        assert_eq!(
            count_active_dispatches_by_type(&db, "card-339-skip", "rework"),
            0,
            "single-provider auto-approve must not create a rework dispatch"
        );
        assert_eq!(
            count_active_dispatches_by_type(&db, "card-339-skip", "review-decision"),
            0,
            "single-provider auto-approve must consume the pending review-decision"
        );
    }

    #[tokio::test]
    async fn scenario_339_accept_rework_failure_keeps_review_decision_recoverable() {
        let db = test_db();
        let engine = test_engine(&db);
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, latest_dispatch_id, review_status, \
                 created_at, updated_at) \
                 VALUES ('card-339-no-agent', 'No Agent Rework Failure', 'review', 'rd-339-no-agent', \
                 'suggestion_pending', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, \
                 title, created_at, updated_at) \
                 VALUES ('rd-339-no-agent', 'card-339-no-agent', 'ghost-agent', 'review-decision', \
                 'pending', '[Decision] card-339-no-agent', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO card_review_state (card_id, state, pending_dispatch_id) \
                 VALUES ('card-339-no-agent', 'suggestion_pending', 'rd-339-no-agent')",
                [],
            )
            .unwrap();
        }

        let state = AppState {
            db: db.clone(),
            pg_pool: None,
            engine,
            config: std::sync::Arc::new(crate::config::Config::default()),
            broadcast_tx: crate::server::ws::new_broadcast(),
            batch_buffer: crate::server::ws::spawn_batch_flusher(crate::server::ws::new_broadcast()),
            health_registry: None,
        };

        let (status, json) = crate::server::routes::review_verdict::submit_review_decision(
            axum::extract::State(state),
            axum::Json(crate::server::routes::review_verdict::ReviewDecisionBody {
                card_id: "card-339-no-agent".to_string(),
                decision: "accept".to_string(),
                comment: None,
                dispatch_id: Some("rd-339-no-agent".to_string()),
            }),
        )
        .await;

        assert_eq!(
            status,
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "accept must fail closed when no follow-up dispatch can be created: {json:?}"
        );
        assert!(
            json.0["error"]
                .as_str()
                .unwrap_or_default()
                .contains("no follow-up dispatch created")
        );
        assert_eq!(json.0["pending_dispatch_id"], "rd-339-no-agent");

        let actual_card_status = get_card_status(&db, "card-339-no-agent");
        assert_eq!(
            json.0["card_status_after"].as_str(),
            Some(actual_card_status.as_str()),
            "error payload must report the real post-failure card status"
        );
        assert_eq!(
            get_dispatch_status(&db, "rd-339-no-agent"),
            "pending",
            "fail-closed accept must keep the review-decision dispatch live for retry"
        );
        assert_eq!(
            count_active_dispatches_by_type(&db, "card-339-no-agent", "review"),
            0
        );
        assert_eq!(
            count_active_dispatches_by_type(&db, "card-339-no-agent", "rework"),
            0
        );
        assert_eq!(
            count_active_dispatches_by_type(&db, "card-339-no-agent", "review-decision"),
            1,
            "recovery path must retain exactly one live review-decision dispatch"
        );
    }

    // ── #195: rework dispatch completion triggers re-review cycle ──────
    //
    // Verifies the full accept → rework → re-review cycle:
    // After rework dispatch completes, OnDispatchCompleted (kanban-rules.js)
    // transitions the card to review, and OnReviewEnter creates a new review dispatch.

    #[test]
    fn scenario_195_rework_completion_triggers_review() {
        let (_repo, _repo_guard) = setup_test_repo();
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-195b", "in_progress");

        // Create and complete a rework dispatch — simulates the rework turn finishing
        seed_dispatch(&db, "rw-195b", "card-195b", "rework", "pending");
        seed_assistant_response_for_dispatch(&db, "rw-195b", "reworked after review");

        let result = dispatch::complete_dispatch(
            &db,
            &engine,
            "rw-195b",
            &serde_json::json!({"completion_source": "test_harness"}),
        );
        assert!(
            result.is_ok(),
            "complete_dispatch should succeed: {:?}",
            result.err()
        );

        // Rework completion → card must transition to review (via kanban-rules.js)
        let status = get_card_status(&db, "card-195b");
        assert_eq!(
            status, "review",
            "#195: rework completion must transition card to review"
        );

        // OnReviewEnter must create a review dispatch for re-review
        let conn = db.lock().unwrap();
        let review_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches \
                 WHERE kanban_card_id = 'card-195b' AND dispatch_type = 'review' \
                 AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            review_count, 1,
            "#195: rework completion must trigger OnReviewEnter → review dispatch"
        );
    }

    #[test]
    fn scenario_655_rework_noop_completion_uses_noop_verification_review_context() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-655-rework-noop", "in_progress");
        seed_dispatch(
            &db,
            "rw-655-noop",
            "card-655-rework-noop",
            "rework",
            "pending",
        );

        let result = dispatch::complete_dispatch(
            &db,
            &engine,
            "rw-655-noop",
            &serde_json::json!({
                "completion_source": "test_harness",
                "work_outcome": "noop",
                "completed_without_changes": true,
                "notes": "rework turned out already implemented"
            }),
        );
        assert!(
            result.is_ok(),
            "complete_dispatch should succeed: {:?}",
            result.err()
        );

        assert_eq!(get_card_status(&db, "card-655-rework-noop"), "review");

        let conn = db.lock().unwrap();
        let latest_dispatch_id: Option<String> = conn
            .query_row(
                "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-655-rework-noop'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let latest_dispatch_id = latest_dispatch_id
            .expect("#655: latest_dispatch_id must point at the follow-up review dispatch");
        let latest_dispatch_context: serde_json::Value = conn
            .query_row(
                "SELECT context FROM task_dispatches WHERE id = ?1",
                [&latest_dispatch_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .unwrap()
            .as_deref()
            .map(|raw| serde_json::from_str(raw).unwrap())
            .unwrap_or_else(|| serde_json::json!({}));
        drop(conn);

        assert_eq!(latest_dispatch_context["review_mode"], "noop_verification");
        assert_eq!(latest_dispatch_context["parent_dispatch_id"], "rw-655-noop");
        assert_eq!(
            latest_dispatch_context["noop_reason"],
            "rework turned out already implemented"
        );
    }

    #[test]
    fn scenario_332_implementation_noop_completion_routes_to_review_with_noop_context() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-332", "in_progress");
        seed_dispatch(&db, "impl-332", "card-332", "implementation", "pending");
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) VALUES ('run-332', 'repo', 'agent-1', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at) \
                 VALUES ('entry-332', 'run-332', 'card-332', 'agent-1', 'dispatched', 'impl-332', datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE kanban_cards SET metadata = ?1 WHERE id = 'card-332'",
            libsql_rusqlite::params![
                serde_json::json!({
                    "preflight_status": "consult_required",
                    "preflight_summary": "need clarification",
                    "preflight_checked_at": "2026-04-15T01:02:03Z",
                    "consultation_status": "completed",
                    "consultation_result": {"summary": "stale"},
                    "keep": "yes"
                })
                .to_string()
            ],
        )
        .unwrap();
        drop(conn);

        let result = dispatch::complete_dispatch(
            &db,
            &engine,
            "impl-332",
            &serde_json::json!({
                "completion_source": "test_harness",
                "work_outcome": "noop",
                "completed_without_changes": true,
                "card_status_target": "ready",
                "notes": "spec already satisfied"
            }),
        );
        assert!(
            result.is_ok(),
            "complete_dispatch should succeed: {:?}",
            result.err()
        );

        assert_eq!(
            get_card_status(&db, "card-332"),
            "review",
            "#655: explicit noop outcome must route implementation card into review"
        );

        let conn = db.lock().unwrap();
        let (review_count, latest_dispatch_id): (i64, Option<String>) = conn
            .query_row(
                "SELECT \
                    (SELECT COUNT(*) FROM task_dispatches \
                     WHERE kanban_card_id = 'card-332' AND dispatch_type = 'review' \
                     AND status IN ('pending', 'dispatched')), \
                    latest_dispatch_id \
                 FROM kanban_cards WHERE id = 'card-332'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            review_count, 1,
            "#655: noop completion must create a follow-up review dispatch"
        );
        let latest_dispatch_id =
            latest_dispatch_id.expect("#655: latest_dispatch_id must point at the review dispatch");
        let (latest_dispatch_type, latest_dispatch_context): (String, Option<String>) = conn
            .query_row(
                "SELECT dispatch_type, context FROM task_dispatches WHERE id = ?1",
                [&latest_dispatch_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let auto_queue_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-332'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            latest_dispatch_type, "review",
            "#655: latest_dispatch_id must move to the pending review dispatch"
        );
        let latest_dispatch_context: serde_json::Value = serde_json::from_str(
            latest_dispatch_context
                .as_deref()
                .expect("review dispatch must carry JSON context"),
        )
        .unwrap();
        assert_eq!(latest_dispatch_context["review_mode"], "noop_verification");
        assert_eq!(
            latest_dispatch_context["noop_reason"],
            "spec already satisfied"
        );
        assert_eq!(latest_dispatch_context["parent_dispatch_id"], "impl-332");
        assert_eq!(
            auto_queue_status, "dispatched",
            "#655: auto-queue entry must remain live until noop review reaches terminal state"
        );

        let metadata_json: String = conn
            .query_row(
                "SELECT metadata FROM kanban_cards WHERE id = 'card-332'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);

        let metadata: serde_json::Value = serde_json::from_str(&metadata_json).unwrap();
        assert_eq!(metadata["work_resolution_status"], "noop");
        assert_eq!(metadata["work_resolution_result"]["work_outcome"], "noop");
        assert_eq!(
            metadata["work_resolution_result"]["completed_without_changes"],
            true
        );
        assert_eq!(
            metadata["work_resolution_result"]["card_status_target"],
            "ready"
        );
        assert!(metadata["preflight_status"].is_null());
        assert!(metadata["preflight_summary"].is_null());
        assert!(metadata["preflight_checked_at"].is_null());
        assert!(metadata["consultation_status"].is_null());
        assert!(metadata["consultation_result"].is_null());
        assert_eq!(metadata["keep"], "yes");
    }

    #[test]
    fn scenario_615_completed_without_changes_routes_to_review_even_without_explicit_noop_marker() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-615-noop", "in_progress");
        seed_dispatch(
            &db,
            "impl-615-noop",
            "card-615-noop",
            "implementation",
            "pending",
        );
        seed_assistant_response_for_dispatch(
            &db,
            "impl-615-noop",
            "verified existing implementation without additional edits",
        );

        let result = dispatch::complete_dispatch(
            &db,
            &engine,
            "impl-615-noop",
            &serde_json::json!({
                "completion_source": "test_harness",
                "completed_without_changes": true,
                "card_status_target": "done",
                "notes": "already applied without code change"
            }),
        );
        assert!(
            result.is_ok(),
            "complete_dispatch should succeed: {:?}",
            result.err()
        );

        assert_eq!(
            get_card_status(&db, "card-615-noop"),
            "review",
            "#655: completed_without_changes must route through review instead of bypassing it"
        );
        assert_eq!(
            count_active_dispatches_by_type(&db, "card-615-noop", "review"),
            1,
            "#655: completed_without_changes must enqueue a review dispatch"
        );
        assert_eq!(
            review_state_value(&db, "card-615-noop").as_deref(),
            Some("reviewing"),
            "#655: noop completion must leave canonical review state in reviewing"
        );

        let metadata_json: String = db
            .lock()
            .unwrap()
            .query_row(
                "SELECT metadata FROM kanban_cards WHERE id = 'card-615-noop'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let metadata: serde_json::Value = serde_json::from_str(&metadata_json).unwrap();
        assert_eq!(metadata["work_resolution_status"], "noop");
        assert_eq!(
            metadata["work_resolution_result"]["completed_without_changes"],
            true
        );
        assert_eq!(
            metadata["work_resolution_result"]["card_status_target"],
            "done"
        );
    }

    #[test]
    fn scenario_547_implementation_noop_completion_waits_for_review_before_auto_queue_activate() {
        let policies_dir = setup_auto_queue_activate_spy_policy_dir();
        let db = test_db();
        let engine = test_engine_with_dir(&db, policies_dir.path());
        seed_agent(&db);
        set_kv(&db, "server_port", "8791");
        seed_card(&db, "card-547-noop", "in_progress");
        seed_card(&db, "card-547-next", "ready");
        seed_dispatch(
            &db,
            "impl-547-noop",
            "card-547-noop",
            "implementation",
            "pending",
        );

        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status) VALUES ('run-547', 'repo', 'agent-1', 'active')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at, thread_group, batch_phase) \
             VALUES ('entry-547-noop', 'run-547', 'card-547-noop', 'agent-1', 'dispatched', 'impl-547-noop', datetime('now'), 0, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, thread_group, batch_phase) \
             VALUES ('entry-547-next', 'run-547', 'card-547-next', 'agent-1', 'pending', 0, 0)",
            [],
        )
        .unwrap();
        drop(conn);

        let result = dispatch::complete_dispatch(
            &db,
            &engine,
            "impl-547-noop",
            &serde_json::json!({
                "completion_source": "test_harness",
                "work_outcome": "noop",
                "completed_without_changes": true,
                "card_status_target": "ready",
                "notes": "already implemented"
            }),
        );
        assert!(
            result.is_ok(),
            "complete_dispatch should succeed: {:?}",
            result.err()
        );

        assert_eq!(get_card_status(&db, "card-547-noop"), "review");

        let conn = db.lock().unwrap();
        let entry_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-547-noop'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        assert_eq!(
            entry_status, "dispatched",
            "#655: noop completion must keep the active auto-queue entry live until review finishes"
        );

        assert_eq!(
            kv_value(&db, "test_auto_queue_activate_count").as_deref(),
            None,
            "#655: noop completion must not trigger auto-queue activate before review verdict"
        );

        let next_entry_status: String = db
            .lock()
            .unwrap()
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-547-next'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            next_entry_status, "pending",
            "#655: follow-up auto-queue entry must stay pending until noop review reaches terminal state"
        );
    }

    #[test]
    fn scenario_547_implementation_noop_completion_defers_phase_gate_until_review_passes() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-547-phase-1", "in_progress");
        seed_card(&db, "card-547-phase-2", "ready");
        seed_dispatch(
            &db,
            "impl-547-phase-1",
            "card-547-phase-1",
            "implementation",
            "pending",
        );

        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-547-phase', 'test/repo', 'agent-1', 'active', datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at, batch_phase, created_at) \
             VALUES ('entry-547-phase-1', 'run-547-phase', 'card-547-phase-1', 'agent-1', 'dispatched', 'impl-547-phase-1', datetime('now'), 1, datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, batch_phase, created_at) \
             VALUES ('entry-547-phase-2', 'run-547-phase', 'card-547-phase-2', 'agent-1', 'pending', 2, datetime('now'))",
            [],
        )
        .unwrap();
        drop(conn);

        let result = dispatch::complete_dispatch(
            &db,
            &engine,
            "impl-547-phase-1",
            &serde_json::json!({
                "completion_source": "test_harness",
                "work_outcome": "noop",
                "completed_without_changes": true,
                "card_status_target": "ready",
                "notes": "already implemented"
            }),
        );
        assert!(
            result.is_ok(),
            "complete_dispatch should succeed: {:?}",
            result.err()
        );

        assert_eq!(get_card_status(&db, "card-547-phase-1"), "review");

        let conn = db.lock().unwrap();
        let entry_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-547-phase-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let run_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_runs WHERE id = 'run-547-phase'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let phase_gate_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches \
                 WHERE kanban_card_id = 'card-547-phase-1' \
                   AND dispatch_type = 'phase-gate' \
                   AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);

        assert_eq!(entry_status, "dispatched");
        assert_eq!(
            run_status, "active",
            "#655: multi-phase auto-queue run must stay active until noop review passes"
        );
        assert_eq!(
            phase_gate_count, 0,
            "#655: noop completion must not create a phase-gate dispatch before review finishes"
        );
        assert!(
            phase_gate_state(&db, "run-547-phase", 1).is_none(),
            "#655: phase-gate state must remain empty until the noop review reaches terminal state"
        );
    }

    #[test]
    fn scenario_494_implementation_noop_completion_final_entry_waits_for_review_before_notify() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-494-noop-final", "in_progress");
        seed_dispatch(
            &db,
            "impl-494-noop-final",
            "card-494-noop-final",
            "implementation",
            "pending",
        );

        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
             VALUES ('run-494-noop-final', 'test/repo', 'agent-1', 'active', datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, dispatch_id, dispatched_at, batch_phase, created_at) \
             VALUES ('entry-494-noop-final', 'run-494-noop-final', 'card-494-noop-final', 'agent-1', 'dispatched', 'impl-494-noop-final', datetime('now'), 0, datetime('now'))",
            [],
        )
        .unwrap();
        drop(conn);

        let result = dispatch::complete_dispatch(
            &db,
            &engine,
            "impl-494-noop-final",
            &serde_json::json!({
                "completion_source": "test_harness",
                "work_outcome": "noop",
                "completed_without_changes": true,
                "card_status_target": "ready",
                "notes": "already implemented"
            }),
        );
        assert!(
            result.is_ok(),
            "complete_dispatch should succeed: {:?}",
            result.err()
        );

        let conn = db.lock().unwrap();
        let run_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_runs WHERE id = 'run-494-noop-final'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);

        let messages = message_outbox_rows(&db);
        assert_eq!(
            run_status, "active",
            "#655: noop completion on the final entry must keep the run active until review passes"
        );
        assert_eq!(
            messages.len(),
            0,
            "#655: noop completion on the final entry must not queue completion notify before review passes"
        );
    }

    #[test]
    fn scenario_211_review_pass_seeds_pr_tracking_and_create_pr_dispatch() {
        let (repo, _repo_guard) = setup_test_repo();
        run_git(repo.path(), &["checkout", "-b", "wt/card-211-review"]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(
            &db,
            "card-211-review",
            "review",
            "test/repo",
            211,
            Some("123456789012345678"),
        );
        seed_completed_work_dispatch_for_review(
            &db,
            "impl-211-review",
            "card-211-review",
            "implementation",
        );
        seed_completed_review_dispatch(&db, "review-211-pass", "card-211-review", "pass");

        engine
            .try_fire_hook_by_name(
                "OnReviewVerdict",
                serde_json::json!({"card_id": "card-211-review", "verdict": "pass"}),
            )
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(
            count_active_dispatches_by_type(&db, "card-211-review", "create-pr"),
            1
        );
        assert_eq!(
            pr_tracking_state(&db, "card-211-review").as_deref(),
            Some("create-pr")
        );
        assert_eq!(
            pr_tracking_branch(&db, "card-211-review").as_deref(),
            Some("wt/card-211-review")
        );
    }

    /// #701 non-skip path: review pass with an active non-skip pipeline stage
    /// (e.g. `dev-deploy`) keeps PR creation deferred so `ci-recovery` cannot
    /// race `deploy-pipeline` for ownership. The card still enters the stage
    /// (pipeline_stage_id set, status=in_progress, blocked_reason=deploy:waiting)
    /// but no create-pr dispatch is seeded here — the follow-up on completion
    /// path is tracked separately (scope deliberately limited in this PR).
    #[test]
    fn scenario_701_review_pass_with_pipeline_stage_enters_stage_without_early_pr() {
        let (repo, _repo_guard) = setup_test_repo();
        run_git(repo.path(), &["checkout", "-b", "wt/card-701-pipeline"]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");

        // Seed a dev-deploy-like pipeline stage (trigger_after=review_pass,
        // provider=self, skip_condition=no_rs_changes). Without a mock gh,
        // `hasRsChanges` falls back to `true`, so the stage is entered (not
        // skipped) — this exercises the non-skip path.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO pipeline_stages (repo_id, stage_name, stage_order, trigger_after, provider, skip_condition) \
                 VALUES ('test/repo', 'dev-deploy', 100, 'review_pass', 'self', 'no_rs_changes')",
                [],
            )
            .unwrap();
        }

        seed_card_with_repo(
            &db,
            "card-701-pipeline",
            "review",
            "test/repo",
            701,
            Some("123456789012345679"),
        );
        seed_completed_work_dispatch_for_review(
            &db,
            "impl-701-pipeline",
            "card-701-pipeline",
            "implementation",
        );
        seed_completed_review_dispatch(&db, "review-701-pass", "card-701-pipeline", "pass");

        engine
            .try_fire_hook_by_name(
                "OnReviewVerdict",
                serde_json::json!({"card_id": "card-701-pipeline", "verdict": "pass"}),
            )
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        // Non-skip path: the stage owns the card. No create-pr dispatch is
        // seeded here — that would race `ci-recovery` with `deploy-pipeline`.
        assert_eq!(
            count_active_dispatches_by_type(&db, "card-701-pipeline", "create-pr"),
            0,
            "#701: non-skip path must NOT seed create-pr — pipeline stage owns the card"
        );
        assert!(
            pr_tracking_state(&db, "card-701-pipeline").is_none(),
            "#701: non-skip path must NOT seed pr_tracking"
        );
        // The card is bound to the pipeline stage (TEXT column per schema).
        let conn = db.lock().unwrap();
        let (stage_id, blocked, status): (Option<String>, Option<String>, String) = conn
            .query_row(
                "SELECT pipeline_stage_id, blocked_reason, status FROM kanban_cards WHERE id = 'card-701-pipeline'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert!(
            stage_id.is_some(),
            "#701: non-skip path should bind the card to the pipeline stage"
        );
        assert_eq!(blocked.as_deref(), Some("deploy:waiting"));
        assert_eq!(status, "in_progress");
    }

    /// #701 regression (skip path): when pipeline stage's `skip_condition`
    /// matches (e.g. no_rs_changes with a PR that touches no .rs files),
    /// the card must still get a create-pr dispatch AND `pipeline_stage_id`
    /// must be cleared to NULL per DoD.
    #[cfg(unix)]
    #[test]
    fn scenario_701_review_pass_with_skipped_pipeline_stage_still_dispatches_create_pr() {
        // `hasRsChanges` short-circuits to false when pr:list returns a PR AND
        // the subsequent `repos/.../pulls/N/files` reply contains no .rs paths.
        // `setup_test_repo_with_mock_gh` holds a single env-lock guard for both
        // the repo override and the mock gh binary — using `install_mock_gh`
        // and `setup_test_repo` separately deadlocks because each tries to
        // re-acquire the same static env lock.
        let (repo, _env) = setup_test_repo_with_mock_gh(&[
            MockGhReply {
                key: "pr:list",
                contains: None,
                stdout: "[{\"number\":902}]",
            },
            MockGhReply {
                key: "api:repos/test/repo/pulls/902/files",
                contains: None,
                stdout: "README.md\ndashboard/src/App.tsx",
            },
        ]);
        run_git(repo.path(), &["checkout", "-b", "wt/card-701-skip"]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO pipeline_stages (repo_id, stage_name, stage_order, trigger_after, provider, skip_condition) \
                 VALUES ('test/repo', 'dev-deploy', 100, 'review_pass', 'self', 'no_rs_changes')",
                [],
            )
            .unwrap();
        }

        seed_card_with_repo(
            &db,
            "card-701-skip",
            "review",
            "test/repo",
            702,
            Some("123456789012345680"),
        );
        seed_completed_work_dispatch_for_review(
            &db,
            "impl-701-skip",
            "card-701-skip",
            "implementation",
        );
        seed_completed_review_dispatch(&db, "review-701-skip", "card-701-skip", "pass");

        engine
            .try_fire_hook_by_name(
                "OnReviewVerdict",
                serde_json::json!({"card_id": "card-701-skip", "verdict": "pass"}),
            )
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        // DoD: create-pr dispatch exists even when the pipeline stage was skipped.
        assert_eq!(
            count_active_dispatches_by_type(&db, "card-701-skip", "create-pr"),
            1,
            "#701: skip path must still create a create-pr dispatch"
        );
        assert_eq!(
            pr_tracking_state(&db, "card-701-skip").as_deref(),
            Some("create-pr")
        );
        // DoD: pipeline_stage_id must be cleared on skip.
        let conn = db.lock().unwrap();
        let stage_id: Option<i64> = conn
            .query_row(
                "SELECT pipeline_stage_id FROM kanban_cards WHERE id = 'card-701-skip'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            stage_id.is_none(),
            "#701 DoD: pipeline_stage_id must be NULL after skip_condition match"
        );
    }

    /// #701 regression (counter-stage DoD gate): `advancePipelineStage`'s
    /// counter-provider skip gate reads `card.description` to decide whether
    /// the DoD requires E2E. The initial card SELECT must include
    /// `description` — omitting it caused `dodText` to collapse to "" and
    /// silently bypass every E2E stage, so a card with "E2E test coverage"
    /// in its DoD could reach terminal (and, after the #701 handoff, PR/CI)
    /// without ever running E2E.
    #[cfg(unix)]
    #[test]
    fn scenario_701_advance_to_counter_e2e_respects_description_dod_gate() {
        let (_repo, _repo_guard) = setup_test_repo();

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");

        // Two stages: a pre-e2e stage (order 50) so the card has a valid
        // current pipeline_stage_id, and a counter-provider e2e-test stage
        // at order 100 that advancePipelineStage will advance into.
        let pre_stage_id: i64;
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO pipeline_stages (repo_id, stage_name, stage_order, trigger_after, provider) \
                 VALUES ('test/repo', 'pre-e2e-gate', 50, 'review_pass', 'self')",
                [],
            )
            .unwrap();
            pre_stage_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO pipeline_stages (repo_id, stage_name, stage_order, provider) \
                 VALUES ('test/repo', 'e2e-test', 100, 'counter')",
                [],
            )
            .unwrap();
        }

        seed_card_with_repo(
            &db,
            "card-701-e2e-required",
            "in_progress",
            "test/repo",
            703,
            Some("123456789012345681"),
        );
        // DoD explicitly lists E2E — the counter skip gate MUST NOT skip.
        // Also bind the card to pre_stage_id so advancePipelineStage finds
        // the counter e2e-test stage as the next stage.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET description = ?1, pipeline_stage_id = ?2, updated_at = datetime('now') \
                 WHERE id = 'card-701-e2e-required'",
                libsql_rusqlite::params!["- [ ] E2E test coverage", pre_stage_id.to_string()],
            )
            .unwrap();
        }

        // Seed a completed e2e-test dispatch with a "pass" verdict directly
        // in the DB — create_dispatch_core requires a resolvable worktree,
        // which we don't need here. Firing OnDispatchCompleted manually
        // triggers deploy-pipeline.onDispatchCompleted, which calls
        // advancePipelineStage and hits the counter-stage skip gate.
        let dispatch_id = "e2e-701-required-bootstrap";
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (
                    id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                    result, created_at, updated_at, completed_at
                ) VALUES (
                    ?1, 'card-701-e2e-required', 'agent-1', 'e2e-test', 'completed',
                    '[E2E Test bootstrap]',
                    '{\"verdict\":\"pass\"}',
                    datetime('now', '-1 minute'), datetime('now', '-1 minute'), datetime('now', '-1 minute')
                )",
                libsql_rusqlite::params![dispatch_id],
            )
            .unwrap();
        }
        engine
            .try_fire_hook_by_name(
                "OnDispatchCompleted",
                serde_json::json!({"dispatch_id": dispatch_id}),
            )
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        // The DoD explicitly lists E2E, so the counter-stage skip gate must
        // NOT skip — the card must advance INTO the counter e2e-test stage,
        // not past it to terminal/PR. We assert on pipeline_stage_id because
        // the e2e-test dispatch creation itself requires worktree resolution
        // that this minimal test harness doesn't provide; the gate's
        // observable effect (stage advancement vs stage clear) is what
        // actually distinguishes the bug.
        //
        // Before the fix: the SELECT omitted description, dodText was "",
        //   indexOf("e2e") === -1 was true → skip taken → pipeline_stage_id
        //   cleared to NULL and card handed to attemptCreatePr (which could
        //   silently ship code without E2E).
        // After the fix: description is loaded, dodText contains "e2e" →
        //   skip NOT taken → pipeline_stage_id advanced to counter stage
        //   (non-NULL, pointing at the e2e-test stage).
        let conn = db.lock().unwrap();
        let stage_id_after: Option<String> = conn
            .query_row(
                "SELECT pipeline_stage_id FROM kanban_cards WHERE id = 'card-701-e2e-required'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        assert!(
            stage_id_after.is_some(),
            "#701: counter-stage dodText gate must honor description — E2E in DoD means NO skip (pipeline_stage_id must advance, not clear)"
        );
        // Pipeline is still running — no PR handoff may have happened.
        assert_eq!(
            count_active_dispatches_by_type(&db, "card-701-e2e-required", "create-pr"),
            0,
            "#701: counter-stage gate did not skip — no create-pr may leak through"
        );
    }

    /// #701 regression (noop_verification + pipeline): when review passes
    /// with review_mode='noop_verification' (the agent verified there are
    /// no changes to ship), the card must go straight to terminal and skip
    /// pipeline entry. Without this short-circuit, a noop card would enter
    /// a non-skip pipeline, and the post-pipeline
    /// `agentdesk.reviewAutomation.attemptCreatePr()` call from
    /// deploy-pipeline.js would dispatch `create-pr` for noop work —
    /// resulting in an empty PR, wasted CI, and possible auto-merge of
    /// "no changes".
    #[tokio::test(flavor = "current_thread")]
    async fn scenario_701_noop_verification_pass_skips_pipeline_entry() {
        let _gh = install_mock_gh(&[MockGhReply {
            key: "issue:close",
            contains: Some("--repo test/repo"),
            stdout: "",
        }]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-701-noop", "review", "test/repo", 711, None);

        // Seed a dev-deploy pipeline stage — without the short-circuit this
        // would be entered by the review-pass handler.
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO pipeline_stages (repo_id, stage_name, stage_order, trigger_after, provider) \
                 VALUES ('test/repo', 'dev-deploy', 100, 'review_pass', 'self')",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE kanban_cards SET review_status = 'reviewing', latest_dispatch_id = 'review-701-noop' WHERE id = 'card-701-noop'",
                [],
            )
            .unwrap();
            // Implementation dispatch completed with work_outcome='noop'.
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, completed_at, created_at, updated_at) \
                 VALUES ('impl-701-noop', 'card-701-noop', 'agent-1', 'implementation', 'completed', '[Impl noop]', ?1, datetime('now', '-2 minutes'), datetime('now', '-5 minutes'), datetime('now', '-2 minutes'))",
                libsql_rusqlite::params![serde_json::json!({
                    "work_outcome": "noop",
                    "completed_without_changes": true,
                    "notes": "already implemented"
                }).to_string()],
            )
            .unwrap();
            // The pending review dispatch uses review_mode='noop_verification'.
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at) \
                 VALUES ('review-701-noop', 'card-701-noop', 'agent-1', 'review', 'pending', '[Review noop]', ?1, datetime('now'), datetime('now'))",
                libsql_rusqlite::params![serde_json::json!({
                    "review_mode": "noop_verification",
                    "parent_dispatch_id": "impl-701-noop"
                }).to_string()],
            )
            .unwrap();
        }

        let state = AppState::test_state(db.clone(), engine);
        let (status, _body) = crate::server::routes::review_verdict::submit_verdict(
            axum::extract::State(state),
            axum::Json(crate::server::routes::review_verdict::SubmitVerdictBody {
                dispatch_id: "review-701-noop".to_string(),
                overall: "pass".to_string(),
                items: None,
                notes: Some("noop verification passed".to_string()),
                feedback: None,
                commit: None,
                provider: None,
            }),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);

        let conn = db.lock().unwrap();
        let (card_status, pipeline_stage_id): (String, Option<String>) = conn
            .query_row(
                "SELECT status, pipeline_stage_id FROM kanban_cards WHERE id = 'card-701-noop'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let create_pr_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = 'card-701-noop' AND dispatch_type = 'create-pr'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);

        assert_eq!(
            card_status, "done",
            "#701: noop_verification pass must short-circuit to terminal even when a pipeline stage is configured"
        );
        assert!(
            pipeline_stage_id.is_none(),
            "#701: noop_verification pass must NOT bind the card to any pipeline stage (found {:?})",
            pipeline_stage_id
        );
        assert_eq!(
            create_pr_count, 0,
            "#701: noop_verification pass must NOT create a create-pr dispatch (pipeline must be skipped, not just its PR handoff)"
        );
    }

    /// #701 regression (markPrCreateFailed ordering): kanban terminal
    /// transitions clear blocked_reason as part of their cleanup, so
    /// writing the failure marker BEFORE setStatus wipes it immediately.
    /// The helper must setStatus first and stamp blocked_reason afterward
    /// so the pr:create_failed marker survives the transition.
    #[cfg(unix)]
    #[test]
    fn scenario_701_mark_pr_create_failed_marker_survives_terminal_transition() {
        let (_repo, _repo_guard) = setup_test_repo();

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        // Seed a card with review_status set so the terminal transition
        // has cleanup to do — this is the path that historically cleared
        // blocked_reason.
        seed_card_with_repo(
            &db,
            "card-701-mark-failed",
            "review",
            "test/repo",
            712,
            None,
        );
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET review_status = 'reviewing' WHERE id = 'card-701-mark-failed'",
                [],
            )
            .unwrap();
        }

        // Invoke the review-automation helper via the JS engine. It is not
        // exported on `agentdesk.reviewAutomation` by name for arbitrary
        // reasons (only the curated PR helpers are), but it's reachable
        // via `agentdesk.reviewAutomation.markPrCreateFailed(...)`.
        engine
            .eval_js::<String>(
                r#"(() => { agentdesk.reviewAutomation.markPrCreateFailed("card-701-mark-failed", "test_reason"); return "ok"; })()"#,
            )
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        let conn = db.lock().unwrap();
        let (card_status, blocked_reason): (String, Option<String>) = conn
            .query_row(
                "SELECT status, blocked_reason FROM kanban_cards WHERE id = 'card-701-mark-failed'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        drop(conn);

        assert_eq!(
            card_status, "done",
            "#701: markPrCreateFailed must move the card to the configured terminal state so merge-automation retry can see it"
        );
        assert_eq!(
            blocked_reason.as_deref(),
            Some("pr:create_failed:test_reason"),
            "#701: markPrCreateFailed must persist the pr:create_failed marker AFTER setStatus (terminal transitions clear blocked_reason, so the ordering matters)"
        );
    }

    /// #743 regression: escalateToManualIntervention can overwrite the
    /// machine-readable create-pr escalation marker with a human-readable
    /// message. markPrCreateFailed must restore the machine marker afterward so
    /// merge automation keeps the card in the create-pr failure lane.
    #[cfg(unix)]
    #[test]
    fn scenario_743_mark_pr_create_failed_escalation_preserves_machine_marker() {
        let (_repo, _repo_guard) = setup_test_repo();

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(
            &db,
            "card-743-mark-escalated",
            "review",
            "test/repo",
            1000,
            None,
        );
        seed_pr_tracking(
            &db,
            "card-743-mark-escalated",
            "test/repo",
            Some("wt/card-743-mark-escalated"),
            "feature/card-743-mark-escalated",
            None,
            Some("sha-743-mark-escalated"),
            "create-pr",
        );
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE pr_tracking SET retry_count = 2, last_error = 'previous_failure' \
                 WHERE card_id = 'card-743-mark-escalated'",
                [],
            )
            .unwrap();
        }

        engine
            .eval_js::<String>(
                r#"(() => {
                    agentdesk.reviewAutomation.markPrCreateFailed(
                        "card-743-mark-escalated",
                        "handoff_crashed"
                    );
                    return "ok";
                })()"#,
            )
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        let conn = db.lock().unwrap();
        let (card_status, blocked_reason, tracking_state, retry_count): (
            String,
            Option<String>,
            String,
            i64,
        ) = conn
            .query_row(
                "SELECT kc.status, kc.blocked_reason, pt.state, pt.retry_count \
                 FROM kanban_cards kc \
                 JOIN pr_tracking pt ON pt.card_id = kc.id \
                 WHERE kc.id = 'card-743-mark-escalated'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        drop(conn);

        assert_eq!(card_status, "done");
        assert_eq!(tracking_state, "escalated");
        assert_eq!(retry_count, 3);
        assert_eq!(
            blocked_reason.as_deref(),
            Some("pr:create_failed_escalated:max_retries"),
            "#743: escalation must preserve the machine marker after human notification overwrites blocked_reason"
        );
    }

    /// #743: pr:create_failed card with NO pr_tracking row → escalate to
    /// manual intervention via blocked_reason='pr:create_failed_escalated:no_tracking'
    /// rather than silently deferring to processTrackedMergeQueue (which has
    /// nothing to retry against).
    #[cfg(unix)]
    #[test]
    fn scenario_743_terminal_no_tracking_with_create_failed_escalates() {
        let (_repo, _repo_guard) = setup_test_repo();

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-743-notrack", "done", "test/repo", 1001, None);
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET blocked_reason = 'pr:create_failed:handoff_crashed' WHERE id = 'card-743-notrack'",
                [],
            )
            .unwrap();
        }
        set_kv(&db, "merge_automation_enabled", "true");

        engine
            .try_fire_hook_by_name(
                "OnCardTerminal",
                serde_json::json!({"card_id": "card-743-notrack"}),
            )
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        let conn = db.lock().unwrap();
        let blocked_reason: Option<String> = conn
            .query_row(
                "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-743-notrack'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            blocked_reason.as_deref(),
            Some("pr:create_failed_escalated:no_tracking"),
            "#743: missing pr_tracking at terminal must escalate, not silently defer"
        );
    }

    /// #743: pr:create_failed_escalated:* marker is a terminal-state
    /// noop — don't re-escalate on subsequent OnCardTerminal fires.
    #[cfg(unix)]
    #[test]
    fn scenario_743_terminal_already_escalated_is_noop() {
        let (_repo, _repo_guard) = setup_test_repo();

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-743-esc", "done", "test/repo", 1002, None);
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET blocked_reason = 'pr:create_failed_escalated:max_retries' WHERE id = 'card-743-esc'",
                [],
            )
            .unwrap();
        }
        set_kv(&db, "merge_automation_enabled", "true");

        engine
            .try_fire_hook_by_name(
                "OnCardTerminal",
                serde_json::json!({"card_id": "card-743-esc"}),
            )
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        let conn = db.lock().unwrap();
        let blocked_reason: Option<String> = conn
            .query_row(
                "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-743-esc'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            blocked_reason.as_deref(),
            Some("pr:create_failed_escalated:max_retries"),
            "#743: already-escalated marker must be preserved (no re-escalation loop)"
        );
    }

    /// #743: pr_tracking with state='create-pr' whose dispatch_generation
    /// differs from the currently-active create-pr dispatch's stamped
    /// generation is stale — reseed so the retry loop targets the current
    /// lifecycle rather than a superseded one.
    #[cfg(unix)]
    #[test]
    fn scenario_743_terminal_stale_generation_reseeds_tracking() {
        let (_repo, _repo_guard) = setup_test_repo();

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-743-stale-gen", "done", "test/repo", 1003, None);
        set_kv(&db, "merge_automation_enabled", "true");

        // Seed a create-pr dispatch with generation=G_ACTIVE. Use the helper
        // that stamps both dispatch and pr_tracking with matching gen, then
        // overwrite the pr_tracking gen with a different (stale) value.
        seed_stamped_create_pr_state(
            &db,
            "disp-743-stale",
            "card-743-stale-gen",
            "test/repo",
            None,
            "wt/card-743-stale",
            None,
            Some("sha-current"),
            "create-pr",
            "pending",
        );
        let stale_generation = "00000000-0000-0000-0000-stale0stale01".to_string();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE pr_tracking SET dispatch_generation = ?1 WHERE card_id = 'card-743-stale-gen'",
                [&stale_generation],
            )
            .unwrap();
        }

        engine
            .try_fire_hook_by_name(
                "OnCardTerminal",
                serde_json::json!({"card_id": "card-743-stale-gen"}),
            )
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        let conn = db.lock().unwrap();
        let (new_gen, retry_count, last_error): (Option<String>, i64, Option<String>) = conn
            .query_row(
                "SELECT dispatch_generation, retry_count, last_error FROM pr_tracking WHERE card_id = 'card-743-stale-gen'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_ne!(
            new_gen.as_deref(),
            Some(stale_generation.as_str()),
            "#743: stale generation must be replaced by reseedPrTracking"
        );
        assert_eq!(retry_count, 0);
        assert_eq!(last_error, None);

        let active_dispatch: Option<String> = conn
            .query_row(
                "SELECT status FROM task_dispatches WHERE id = 'disp-743-stale'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            active_dispatch.as_deref(),
            Some("cancelled"),
            "#743: reseedPrTracking must cancel the active stale dispatch so the partial unique index does not block the next handoff"
        );
    }

    /// #743: if handoffCreatePr reuses an already-active create-pr dispatch, it
    /// must refresh pr_tracking back to the active dispatch's generation so
    /// stale loser generations do not leak into retry logic.
    #[cfg(unix)]
    #[test]
    fn scenario_743_handoff_reuse_refreshes_tracking_generation_from_active_dispatch() {
        let (_repo, _repo_guard) = setup_test_repo();

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(
            &db,
            "card-743-handoff-reuse",
            "review",
            "test/repo",
            1005,
            None,
        );

        let active_generation = seed_stamped_create_pr_state(
            &db,
            "disp-743-existing",
            "card-743-handoff-reuse",
            "test/repo",
            Some("wt/card-743-handoff-reuse"),
            "feature/card-743-handoff-reuse",
            None,
            Some("sha-743-handoff-reuse"),
            "create-pr",
            "pending",
        );
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE pr_tracking SET dispatch_generation = '00000000-0000-0000-0000-stale0reuse01' \
                 WHERE card_id = 'card-743-handoff-reuse'",
                [],
            )
            .unwrap();
            conn.execute(
                "UPDATE kanban_cards SET blocked_reason = NULL WHERE id = 'card-743-handoff-reuse'",
                [],
            )
            .unwrap();
        }

        let raw = engine
            .eval_js::<String>(
                r#"(() => JSON.stringify(agentdesk.reviewAutomation.handoffCreatePr(
                    "card-743-handoff-reuse",
                    {
                        repo_id: "test/repo",
                        worktree_path: "wt/card-743-handoff-reuse",
                        branch: "feature/card-743-handoff-reuse",
                        head_sha: "sha-743-handoff-reuse",
                        agent_id: "agent-1",
                        title: "Test Create PR Reuse"
                    }
                )))()"#,
            )
            .unwrap();
        let result: serde_json::Value = serde_json::from_str(&raw).unwrap();

        let conn = db.lock().unwrap();
        let (dispatch_generation, blocked_reason, active_count): (
            Option<String>,
            Option<String>,
            i64,
        ) = conn
            .query_row(
                "SELECT pt.dispatch_generation, kc.blocked_reason, \
                        (SELECT COUNT(*) FROM task_dispatches \
                         WHERE kanban_card_id = 'card-743-handoff-reuse' \
                           AND dispatch_type = 'create-pr' \
                           AND status IN ('pending', 'dispatched')) \
                 FROM pr_tracking pt \
                 JOIN kanban_cards kc ON kc.id = pt.card_id \
                 WHERE pt.card_id = 'card-743-handoff-reuse'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        drop(conn);

        assert_eq!(result["ok"], true);
        assert_eq!(result["reused"], true);
        assert_eq!(result["dispatch_id"], "disp-743-existing");
        assert_eq!(result["generation"], active_generation);
        assert_eq!(
            dispatch_generation.as_deref(),
            Some(active_generation.as_str()),
            "#743: handoffCreatePr reuse must refresh pr_tracking to the active dispatch generation"
        );
        assert_eq!(blocked_reason.as_deref(), Some("pr:creating"));
        assert_eq!(active_count, 1);
    }

    /// #743: pr_tracking.head_sha divergence from the latest completed work
    /// dispatch's head_sha → reseed (the candidate commit moved).
    #[cfg(unix)]
    #[test]
    fn scenario_743_terminal_divergent_head_sha_reseeds_tracking() {
        let (_repo, _repo_guard) = setup_test_repo();

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-743-headdiv", "done", "test/repo", 1004, None);
        set_kv(&db, "merge_automation_enabled", "true");

        // Latest work dispatch with head_sha=new_head_sha.
        seed_completed_work_dispatch_target(
            &db,
            "impl-743-headdiv",
            "card-743-headdiv",
            "implementation",
            "/tmp/wt/card-743-headdiv",
            "wt/card-743-headdiv",
            "new_head_sha",
        );

        // Seed tracking with head_sha=old_head_sha but matching generation
        // on the active dispatch so only the head_sha path is exercised.
        seed_stamped_create_pr_state(
            &db,
            "disp-743-headdiv",
            "card-743-headdiv",
            "test/repo",
            None,
            "wt/card-743-headdiv",
            None,
            Some("old_head_sha"),
            "create-pr",
            "pending",
        );

        engine
            .try_fire_hook_by_name(
                "OnCardTerminal",
                serde_json::json!({"card_id": "card-743-headdiv"}),
            )
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        let conn = db.lock().unwrap();
        let head_sha: Option<String> = conn
            .query_row(
                "SELECT head_sha FROM pr_tracking WHERE card_id = 'card-743-headdiv'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            head_sha.as_deref(),
            Some("new_head_sha"),
            "#743: divergent head_sha must be reseeded to the latest completed work head_sha"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scenario_211_create_pr_completion_advances_tracking_to_wait_ci() {
        let _gh = install_mock_gh(&[MockGhReply {
            key: "pr:list",
            contains: Some("--head wt/card-211-create"),
            stdout: "[{\"number\":411,\"headRefName\":\"wt/card-211-create\",\"headRefOid\":\"abc111\"}]",
        }]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-211-create", "review", "test/repo", 212, None);
        seed_stamped_create_pr_state(
            &db,
            "create-pr-211",
            "card-211-create",
            "test/repo",
            None,
            "wt/card-211-create",
            None,
            Some("oldsha"),
            "create-pr",
            "pending",
        );

        let result = dispatch::complete_dispatch(
            &db,
            &engine,
            "create-pr-211",
            &serde_json::json!({"completion_source": "test_harness"}),
        );
        assert!(
            result.is_ok(),
            "create-pr completion should succeed: {:?}",
            result.err()
        );

        assert_eq!(
            pr_tracking_state(&db, "card-211-create").as_deref(),
            Some("wait-ci")
        );
        assert_eq!(pr_tracking_pr_number(&db, "card-211-create"), Some(411));
        assert_eq!(get_card_status(&db, "card-211-create"), "done");

        let conn = db.lock().unwrap();
        let blocked_reason: Option<String> = conn
            .query_row(
                "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-211-create'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(blocked_reason.as_deref(), Some("ci:waiting"));
    }

    #[cfg(unix)]
    #[test]
    fn scenario_211_create_pr_completion_respects_custom_review_pass_target() {
        let _gh = install_mock_gh(&[MockGhReply {
            key: "pr:list",
            contains: Some("--head wt/card-211-qa-create"),
            stdout: "[{\"number\":412,\"headRefName\":\"wt/card-211-qa-create\",\"headRefOid\":\"abc222\"}]",
        }]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/qa");
        let qa_override = serde_json::json!({
            "states": [
                {"id": "backlog", "label": "Backlog"},
                {"id": "ready", "label": "Ready"},
                {"id": "requested", "label": "Requested"},
                {"id": "in_progress", "label": "In Progress"},
                {"id": "review", "label": "Review"},
                {"id": "qa_test", "label": "QA Test"},
                {"id": "done", "label": "Done", "terminal": true}
            ],
            "transitions": [
                {"from": "backlog", "to": "ready", "type": "free"},
                {"from": "ready", "to": "requested", "type": "free"},
                {"from": "requested", "to": "in_progress", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "in_progress", "to": "review", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "review", "to": "qa_test", "type": "gated", "gates": ["review_passed"]},
                {"from": "review", "to": "in_progress", "type": "gated", "gates": ["review_rework"]},
                {"from": "qa_test", "to": "done", "type": "gated", "gates": ["active_dispatch"]}
            ],
            "gates": {
                "active_dispatch": {"type": "builtin", "check": "has_active_dispatch"},
                "review_passed": {"type": "builtin", "check": "review_verdict_pass"},
                "review_rework": {"type": "builtin", "check": "review_verdict_rework"}
            },
            "hooks": {
                "in_progress": {"on_enter": ["OnCardTransition"], "on_exit": []},
                "review": {"on_enter": ["OnCardTransition", "OnReviewEnter"], "on_exit": []},
                "qa_test": {"on_enter": ["OnCardTransition"], "on_exit": []},
                "done": {"on_enter": ["OnCardTransition", "OnCardTerminal"], "on_exit": []}
            },
            "clocks": {
                "requested": {"set": "requested_at"},
                "in_progress": {"set": "started_at", "mode": "coalesce"},
                "review": {"set": "review_entered_at"},
                "done": {"set": "completed_at"}
            }
        });
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE github_repos SET pipeline_config = ?1 WHERE id = 'test/qa'",
                [qa_override.to_string()],
            )
            .unwrap();
        }
        seed_card_with_repo(&db, "card-211-qa-create", "review", "test/qa", 212, None);
        seed_stamped_create_pr_state(
            &db,
            "create-pr-211-qa",
            "card-211-qa-create",
            "test/qa",
            None,
            "wt/card-211-qa-create",
            None,
            Some("oldsha"),
            "create-pr",
            "pending",
        );

        let result = dispatch::complete_dispatch(
            &db,
            &engine,
            "create-pr-211-qa",
            &serde_json::json!({"completion_source": "test_harness"}),
        );
        assert!(
            result.is_ok(),
            "create-pr completion should succeed for qa pipeline: {:?}",
            result.err()
        );

        assert_eq!(
            pr_tracking_state(&db, "card-211-qa-create").as_deref(),
            Some("wait-ci")
        );
        assert_eq!(pr_tracking_pr_number(&db, "card-211-qa-create"), Some(412));
        assert_eq!(get_card_status(&db, "card-211-qa-create"), "qa_test");

        let conn = db.lock().unwrap();
        let blocked_reason: Option<String> = conn
            .query_row(
                "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-211-qa-create'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(blocked_reason.as_deref(), Some("ci:waiting"));
    }

    #[cfg(unix)]
    #[test]
    fn scenario_211_create_pr_completion_does_not_override_reopened_card() {
        let _gh = install_mock_gh(&[MockGhReply {
            key: "pr:list",
            contains: Some("--head wt/card-211-reopened"),
            stdout: "[{\"number\":413,\"headRefName\":\"wt/card-211-reopened\",\"headRefOid\":\"abc333\"}]",
        }]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(
            &db,
            "card-211-reopened",
            "in_progress",
            "test/repo",
            214,
            None,
        );
        seed_stamped_create_pr_state(
            &db,
            "create-pr-211-reopened",
            "card-211-reopened",
            "test/repo",
            None,
            "wt/card-211-reopened",
            None,
            Some("oldsha"),
            "create-pr",
            "pending",
        );

        let result = dispatch::complete_dispatch(
            &db,
            &engine,
            "create-pr-211-reopened",
            &serde_json::json!({"completion_source": "test_harness"}),
        );
        assert!(
            result.is_ok(),
            "create-pr completion should succeed for reopened card: {:?}",
            result.err()
        );

        assert_eq!(
            pr_tracking_state(&db, "card-211-reopened").as_deref(),
            Some("wait-ci")
        );
        assert_eq!(pr_tracking_pr_number(&db, "card-211-reopened"), Some(413));
        assert_eq!(get_card_status(&db, "card-211-reopened"), "in_progress");

        let conn = db.lock().unwrap();
        let blocked_reason: Option<String> = conn
            .query_row(
                "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-211-reopened'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(blocked_reason, None);
    }

    #[cfg(unix)]
    #[test]
    fn scenario_211_ci_success_advances_tracking_to_merge_and_card_done() {
        let _gh = install_mock_gh(&[
            MockGhReply {
                key: "pr:view",
                contains: Some("--json headRefOid"),
                stdout: "bbb2222",
            },
            MockGhReply {
                key: "run:list",
                contains: Some("--branch wt/card-211-ci"),
                stdout: "[{\"databaseId\":512,\"status\":\"completed\",\"conclusion\":\"success\",\"headSha\":\"bbb2222\",\"event\":\"pull_request\"}]",
            },
        ]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-211-ci", "review", "test/repo", 213, None);
        seed_completed_review_dispatch(&db, "review-211-ci-pass", "card-211-ci", "pass");
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET blocked_reason = 'ci:waiting' WHERE id = 'card-211-ci'",
                [],
            )
            .unwrap();
        }
        seed_pr_tracking(
            &db,
            "card-211-ci",
            "test/repo",
            None,
            "wt/card-211-ci",
            Some(512),
            Some("bbb2222"),
            "wait-ci",
        );

        engine
            .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(
            pr_tracking_state(&db, "card-211-ci").as_deref(),
            Some("merge")
        );
        assert_eq!(get_card_status(&db, "card-211-ci"), "done");

        let conn = db.lock().unwrap();
        let blocked_reason: Option<String> = conn
            .query_row(
                "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-211-ci'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(blocked_reason, None);
    }

    #[cfg(unix)]
    #[test]
    fn scenario_211_terminal_done_card_tracks_pr_instead_of_direct_merge() {
        let (repo, _remote, _gh) = setup_test_repo_with_origin_and_mock_gh(&[
            MockGhReply {
                key: "pr:create",
                contains: Some("--head wt/card-211-direct"),
                stdout: "https://github.com/test/repo/pull/901",
            },
            MockGhReply {
                key: "pr:view",
                contains: Some("--json headRefOid"),
                stdout: "feature-sha-211-direct",
            },
        ]);
        let worktrees_dir = repo.path().join("worktrees");
        fs::create_dir_all(&worktrees_dir).unwrap();
        run_git(repo.path(), &["branch", "wt/card-211-direct"]);

        let worktree_path = worktrees_dir.join("card-211-direct");
        run_git(
            repo.path(),
            &[
                "worktree",
                "add",
                worktree_path.to_str().unwrap(),
                "wt/card-211-direct",
            ],
        );
        fs::write(worktree_path.join("feature.txt"), "feature\n").unwrap();
        run_git(worktree_path.as_path(), &["add", "feature.txt"]);
        run_git(
            worktree_path.as_path(),
            &["commit", "-m", "feat: direct merge path #211"],
        );
        let feature_commit = run_git_output(worktree_path.as_path(), &["rev-parse", "HEAD"]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-211-direct", "done", "test/repo", 211, None);
        set_kv(&db, "merge_automation_enabled", "true");
        seed_completed_work_dispatch_target(
            &db,
            "impl-211-direct",
            "card-211-direct",
            "implementation",
            worktree_path.to_str().unwrap(),
            "wt/card-211-direct",
            &feature_commit,
        );
        seed_worktree_session(&db, "session-211-direct", worktree_path.to_str().unwrap());

        engine
            .try_fire_hook_by_name(
                "OnCardTerminal",
                serde_json::json!({"card_id": "card-211-direct"}),
            )
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        run_git(repo.path(), &["fetch", "origin", "main"]);
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
        assert!(
            !merged.success(),
            "terminal done cards without tracked PR must use PR+CI flow and keep feature commit out of origin/main"
        );
        assert_eq!(get_card_status(&db, "card-211-direct"), "done");
        assert_eq!(
            pr_tracking_state(&db, "card-211-direct").as_deref(),
            Some("wait-ci")
        );
        assert_eq!(pr_tracking_pr_number(&db, "card-211-direct"), Some(901));

        let conn = db.lock().unwrap();
        let blocked_reason: Option<String> = conn
            .query_row(
                "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-211-direct'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(blocked_reason.as_deref(), Some("ci:waiting"));
    }

    #[cfg(unix)]
    #[test]
    fn scenario_211_terminal_direct_merge_push_rejected_falls_back_to_pr_and_resets_main() {
        let (repo, remote, gh) = setup_test_repo_with_origin_and_mock_gh(&[
            MockGhReply {
                key: "pr:create",
                contains: Some("--head wt/card-211-push-rejected"),
                stdout: "https://github.com/test/repo/pull/904",
            },
            MockGhReply {
                key: "pr:view",
                contains: Some("--json headRefOid"),
                stdout: "feature-sha-211-push-rejected",
            },
        ]);
        let worktrees_dir = repo.path().join("worktrees");
        fs::create_dir_all(&worktrees_dir).unwrap();
        run_git(repo.path(), &["branch", "wt/card-211-push-rejected"]);

        let worktree_path = worktrees_dir.join("card-211-push-rejected");
        run_git(
            repo.path(),
            &[
                "worktree",
                "add",
                worktree_path.to_str().unwrap(),
                "wt/card-211-push-rejected",
            ],
        );
        fs::write(worktree_path.join("feature.txt"), "feature\n").unwrap();
        run_git(worktree_path.as_path(), &["add", "feature.txt"]);
        run_git(
            worktree_path.as_path(),
            &["commit", "-m", "feat: push rejected fallback #211"],
        );
        let feature_commit = run_git_output(worktree_path.as_path(), &["rev-parse", "HEAD"]);

        let remote_clone = tempfile::tempdir().unwrap();
        let clone_output = Command::new("git")
            .args([
                "clone",
                remote.path().to_str().unwrap(),
                remote_clone.path().to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            clone_output.status.success(),
            "git clone failed: {}",
            String::from_utf8_lossy(&clone_output.stderr)
        );
        run_git(
            remote_clone.path(),
            &["config", "user.email", "test@test.com"],
        );
        run_git(remote_clone.path(), &["config", "user.name", "Remote Test"]);
        fs::write(remote_clone.path().join("remote-only.txt"), "remote\n").unwrap();
        run_git(remote_clone.path(), &["add", "remote-only.txt"]);
        run_git(
            remote_clone.path(),
            &["commit", "-m", "remote main advance"],
        );
        run_git(remote_clone.path(), &["push", "origin", "main"]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(
            &db,
            "card-211-push-rejected",
            "done",
            "test/repo",
            217,
            None,
        );
        set_kv(&db, "merge_automation_enabled", "true");
        seed_completed_work_dispatch_target(
            &db,
            "impl-211-push-rejected",
            "card-211-push-rejected",
            "implementation",
            worktree_path.to_str().unwrap(),
            "wt/card-211-push-rejected",
            &feature_commit,
        );
        seed_worktree_session(
            &db,
            "session-211-push-rejected",
            worktree_path.to_str().unwrap(),
        );

        engine
            .try_fire_hook_by_name(
                "OnCardTerminal",
                serde_json::json!({"card_id": "card-211-push-rejected"}),
            )
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(get_card_status(&db, "card-211-push-rejected"), "done");
        assert_eq!(
            pr_tracking_state(&db, "card-211-push-rejected").as_deref(),
            Some("wait-ci")
        );
        assert_eq!(
            pr_tracking_pr_number(&db, "card-211-push-rejected"),
            Some(904)
        );

        let conn = db.lock().unwrap();
        let blocked_reason: Option<String> = conn
            .query_row(
                "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-211-push-rejected'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(blocked_reason.as_deref(), Some("ci:waiting"));
        drop(conn);

        run_git(repo.path(), &["fetch", "origin", "main"]);
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
        assert!(
            !merged.success(),
            "push-rejected fallback must leave the feature commit out of origin/main"
        );
        assert_eq!(
            run_git_output(repo.path(), &["rev-list", "--count", "origin/main..main"]),
            "0",
            "local main must be reset after a rejected direct push"
        );

        let log = gh_log(&gh._gh);
        assert!(
            log.contains("pr create --repo test/repo --base main --head wt/card-211-push-rejected"),
            "push-rejected direct merge must fall back to PR creation"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scenario_211_pr_always_creates_pr_without_direct_merge_and_waits_for_codex_approval() {
        let (repo, _remote, gh) = setup_test_repo_with_origin_and_mock_gh(&[
            MockGhReply {
                key: "pr:create",
                contains: Some("--head wt/card-211-pr-always"),
                stdout: "https://github.com/test/repo/pull/902",
            },
            MockGhReply {
                key: "pr:view",
                contains: Some("--json headRefOid"),
                stdout: "feature-sha-211-pr-always",
            },
            MockGhReply {
                key: "run:list",
                contains: Some("--branch wt/card-211-pr-always"),
                stdout: "[{\"databaseId\":722,\"status\":\"completed\",\"conclusion\":\"success\",\"headSha\":\"feature-sha-211-pr-always\",\"event\":\"pull_request\"}]",
            },
            MockGhReply {
                key: "pr:view",
                contains: Some("--json author"),
                stdout: "itismyfield",
            },
            MockGhReply {
                key: "api:repos/test/repo/pulls/902/reviews",
                contains: None,
                stdout: "[]",
            },
        ]);
        let worktrees_dir = repo.path().join("worktrees");
        fs::create_dir_all(&worktrees_dir).unwrap();
        run_git(repo.path(), &["branch", "wt/card-211-pr-always"]);

        let worktree_path = worktrees_dir.join("card-211-pr-always");
        run_git(
            repo.path(),
            &[
                "worktree",
                "add",
                worktree_path.to_str().unwrap(),
                "wt/card-211-pr-always",
            ],
        );
        fs::write(worktree_path.join("feature.txt"), "feature\n").unwrap();
        run_git(worktree_path.as_path(), &["add", "feature.txt"]);
        run_git(
            worktree_path.as_path(),
            &["commit", "-m", "feat: pr-always path #211"],
        );
        let feature_commit = run_git_output(worktree_path.as_path(), &["rev-parse", "HEAD"]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-211-pr-always", "done", "test/repo", 215, None);
        set_kv(&db, "merge_automation_enabled", "true");
        set_kv(&db, "merge_strategy_mode", "pr-always");
        set_kv(&db, "merge_allowed_authors", "itismyfield");
        seed_completed_work_dispatch_target(
            &db,
            "impl-211-pr-always",
            "card-211-pr-always",
            "implementation",
            worktree_path.to_str().unwrap(),
            "wt/card-211-pr-always",
            &feature_commit,
        );
        seed_worktree_session(
            &db,
            "session-211-pr-always",
            worktree_path.to_str().unwrap(),
        );

        engine
            .try_fire_hook_by_name(
                "OnCardTerminal",
                serde_json::json!({"card_id": "card-211-pr-always"}),
            )
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        run_git(repo.path(), &["fetch", "origin", "main"]);
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
        assert!(
            !merged.success(),
            "pr-always must skip direct merge and leave the feature commit out of origin/main"
        );

        assert_eq!(get_card_status(&db, "card-211-pr-always"), "done");
        assert_eq!(
            pr_tracking_state(&db, "card-211-pr-always").as_deref(),
            Some("wait-ci")
        );
        assert_eq!(pr_tracking_pr_number(&db, "card-211-pr-always"), Some(902));
        assert_eq!(
            kv_value(&db, "merge_strategy_mode:card:card-211-pr-always").as_deref(),
            Some("pr-always")
        );

        let conn = db.lock().unwrap();
        let blocked_reason: Option<String> = conn
            .query_row(
                "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-211-pr-always'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(blocked_reason.as_deref(), Some("ci:waiting"));
        drop(conn);

        set_kv(&db, "merge_strategy_mode", "direct-first");

        engine
            .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(
            pr_tracking_state(&db, "card-211-pr-always").as_deref(),
            Some("merge")
        );

        engine
            .try_fire_hook_by_name("OnTick5min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(
            pr_tracking_state(&db, "card-211-pr-always").as_deref(),
            Some("merge")
        );

        let log = gh_log(&gh._gh);
        assert!(
            log.contains("pr create --repo test/repo --base main --head wt/card-211-pr-always"),
            "pr-always must create a PR for the tracked branch"
        );
        assert!(
            !log.contains("pr merge 902"),
            "pr-always must wait for Codex approval even if the global mode changes later"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scenario_211_pr_always_merges_after_codex_approval_even_if_setting_toggles() {
        let (repo, _remote, gh) = setup_test_repo_with_origin_and_mock_gh(&[
            MockGhReply {
                key: "pr:create",
                contains: Some("--head wt/card-211-pr-approved"),
                stdout: "https://github.com/test/repo/pull/903",
            },
            MockGhReply {
                key: "pr:view",
                contains: Some("--json headRefOid"),
                stdout: "feature-sha-211-pr-approved",
            },
            MockGhReply {
                key: "run:list",
                contains: Some("--branch wt/card-211-pr-approved"),
                stdout: "[{\"databaseId\":723,\"status\":\"completed\",\"conclusion\":\"success\",\"headSha\":\"feature-sha-211-pr-approved\",\"event\":\"pull_request\"}]",
            },
            MockGhReply {
                key: "pr:view",
                contains: Some("--json author"),
                stdout: "itismyfield",
            },
            MockGhReply {
                key: "api:repos/test/repo/pulls/903/reviews",
                contains: None,
                stdout: "[{\"id\":9004,\"state\":\"APPROVED\",\"body\":\"LGTM\",\"submitted_at\":\"2026-04-13T02:00:00Z\",\"user\":{\"login\":\"chatgpt-codex-connector\"}}]",
            },
            MockGhReply {
                key: "api:graphql",
                contains: None,
                stdout: "{\"data\":{\"repository\":{\"pullRequest\":{\"reviewThreads\":{\"nodes\":[]}}}}}",
            },
            MockGhReply {
                key: "pr:merge",
                contains: Some("903"),
                stdout: "merged",
            },
        ]);
        let worktrees_dir = repo.path().join("worktrees");
        fs::create_dir_all(&worktrees_dir).unwrap();
        run_git(repo.path(), &["branch", "wt/card-211-pr-approved"]);

        let worktree_path = worktrees_dir.join("card-211-pr-approved");
        run_git(
            repo.path(),
            &[
                "worktree",
                "add",
                worktree_path.to_str().unwrap(),
                "wt/card-211-pr-approved",
            ],
        );
        fs::write(worktree_path.join("feature.txt"), "approved\n").unwrap();
        run_git(worktree_path.as_path(), &["add", "feature.txt"]);
        run_git(
            worktree_path.as_path(),
            &["commit", "-m", "feat: pr-always approval path #211"],
        );
        let feature_commit = run_git_output(worktree_path.as_path(), &["rev-parse", "HEAD"]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-211-pr-approved", "done", "test/repo", 216, None);
        set_kv(&db, "merge_automation_enabled", "true");
        set_kv(&db, "merge_strategy_mode", "pr-always");
        set_kv(&db, "merge_allowed_authors", "itismyfield");
        seed_completed_work_dispatch_target(
            &db,
            "impl-211-pr-approved",
            "card-211-pr-approved",
            "implementation",
            worktree_path.to_str().unwrap(),
            "wt/card-211-pr-approved",
            &feature_commit,
        );
        seed_worktree_session(
            &db,
            "session-211-pr-approved",
            worktree_path.to_str().unwrap(),
        );

        engine
            .try_fire_hook_by_name(
                "OnCardTerminal",
                serde_json::json!({"card_id": "card-211-pr-approved"}),
            )
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        set_kv(&db, "merge_strategy_mode", "direct-first");

        engine
            .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);
        engine
            .try_fire_hook_by_name("OnTick5min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(
            pr_tracking_state(&db, "card-211-pr-approved").as_deref(),
            Some("post-merge-cleanup")
        );
        assert_eq!(
            kv_value(&db, "merge_pending:card-211-pr-approved").as_deref(),
            Some("903")
        );

        let log = gh_log(&gh._gh);
        assert!(
            log.contains("pr merge 903 --auto --squash --repo test/repo"),
            "approved pr-always cards must enable auto-merge even after the global mode toggles"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scenario_211_terminal_direct_merge_rebase_conflict_falls_back_to_pr() {
        let (repo, remote, gh) = setup_test_repo_with_origin_and_mock_gh(&[
            MockGhReply {
                key: "pr:create",
                contains: Some("--head wt/card-211-rebase-conflict"),
                stdout: "https://github.com/test/repo/pull/906",
            },
            MockGhReply {
                key: "pr:view",
                contains: Some("--json headRefOid"),
                stdout: "feature-sha-211-rebase-conflict",
            },
        ]);
        fs::write(repo.path().join("shared.txt"), "base\n").unwrap();
        run_git(repo.path(), &["add", "shared.txt"]);
        run_git(repo.path(), &["commit", "-m", "base shared file"]);
        run_git(repo.path(), &["push", "origin", "main"]);

        let worktrees_dir = repo.path().join("worktrees");
        fs::create_dir_all(&worktrees_dir).unwrap();
        run_git(repo.path(), &["branch", "wt/card-211-rebase-conflict"]);

        let worktree_path = worktrees_dir.join("card-211-rebase-conflict");
        run_git(
            repo.path(),
            &[
                "worktree",
                "add",
                worktree_path.to_str().unwrap(),
                "wt/card-211-rebase-conflict",
            ],
        );
        fs::write(worktree_path.join("shared.txt"), "feature version\n").unwrap();
        run_git(worktree_path.as_path(), &["add", "shared.txt"]);
        run_git(
            worktree_path.as_path(),
            &["commit", "-m", "feature rebase conflict change #211"],
        );
        let feature_commit = run_git_output(worktree_path.as_path(), &["rev-parse", "HEAD"]);

        let remote_clone = tempfile::tempdir().unwrap();
        let clone_output = Command::new("git")
            .args([
                "clone",
                remote.path().to_str().unwrap(),
                remote_clone.path().to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            clone_output.status.success(),
            "git clone failed: {}",
            String::from_utf8_lossy(&clone_output.stderr)
        );
        run_git(
            remote_clone.path(),
            &["config", "user.email", "test@test.com"],
        );
        run_git(remote_clone.path(), &["config", "user.name", "Remote Test"]);
        fs::write(remote_clone.path().join("shared.txt"), "main version\n").unwrap();
        run_git(remote_clone.path(), &["add", "shared.txt"]);
        run_git(
            remote_clone.path(),
            &["commit", "-m", "remote main conflicting advance"],
        );
        run_git(remote_clone.path(), &["push", "origin", "main"]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(
            &db,
            "card-211-rebase-conflict",
            "done",
            "test/repo",
            219,
            None,
        );
        set_kv(&db, "merge_automation_enabled", "true");
        seed_completed_work_dispatch_target(
            &db,
            "impl-211-rebase-conflict",
            "card-211-rebase-conflict",
            "implementation",
            worktree_path.to_str().unwrap(),
            "wt/card-211-rebase-conflict",
            &feature_commit,
        );
        seed_worktree_session(
            &db,
            "session-211-rebase-conflict",
            worktree_path.to_str().unwrap(),
        );

        engine
            .try_fire_hook_by_name(
                "OnCardTerminal",
                serde_json::json!({"card_id": "card-211-rebase-conflict"}),
            )
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(get_card_status(&db, "card-211-rebase-conflict"), "done");
        assert_eq!(
            pr_tracking_state(&db, "card-211-rebase-conflict").as_deref(),
            Some("wait-ci")
        );
        assert_eq!(
            pr_tracking_pr_number(&db, "card-211-rebase-conflict"),
            Some(906)
        );

        let conn = db.lock().unwrap();
        let blocked_reason: Option<String> = conn
            .query_row(
                "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-211-rebase-conflict'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(blocked_reason.as_deref(), Some("ci:waiting"));
        drop(conn);

        run_git(repo.path(), &["fetch", "origin", "main"]);
        assert_eq!(
            run_git_output(repo.path(), &["show", "origin/main:shared.txt"]),
            "main version",
            "rebase-conflict fallback must keep origin/main on the remote-advanced contents"
        );
        assert_eq!(
            run_git_output(repo.path(), &["show", "main:shared.txt"]),
            "base",
            "local main must reset to the pre-merge HEAD after rebase conflict fallback"
        );

        let log = gh_log(&gh._gh);
        assert!(
            log.contains(
                "pr create --repo test/repo --base main --head wt/card-211-rebase-conflict"
            ),
            "rebase conflicts must fall back to PR creation"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scenario_211_terminal_direct_merge_conflict_creates_pr_and_wait_ci() {
        let (repo, _remote, gh) = setup_test_repo_with_origin_and_mock_gh(&[
            MockGhReply {
                key: "pr:create",
                contains: Some("--head wt/card-211-conflict"),
                stdout: "https://github.com/test/repo/pull/901",
            },
            MockGhReply {
                key: "pr:view",
                contains: Some("--json headRefOid"),
                stdout: "feature-sha-211",
            },
        ]);
        fs::write(repo.path().join("conflict.txt"), "base\n").unwrap();
        run_git(repo.path(), &["add", "conflict.txt"]);
        run_git(repo.path(), &["commit", "-m", "base conflict file"]);
        run_git(repo.path(), &["push", "origin", "main"]);

        let worktrees_dir = repo.path().join("worktrees");
        fs::create_dir_all(&worktrees_dir).unwrap();
        run_git(repo.path(), &["branch", "wt/card-211-conflict"]);

        let worktree_path = worktrees_dir.join("card-211-conflict");
        run_git(
            repo.path(),
            &[
                "worktree",
                "add",
                worktree_path.to_str().unwrap(),
                "wt/card-211-conflict",
            ],
        );
        fs::write(worktree_path.join("conflict.txt"), "feature version\n").unwrap();
        run_git(worktree_path.as_path(), &["add", "conflict.txt"]);
        run_git(
            worktree_path.as_path(),
            &["commit", "-m", "feature conflict change #211"],
        );
        let feature_commit = run_git_output(worktree_path.as_path(), &["rev-parse", "HEAD"]);

        fs::write(repo.path().join("conflict.txt"), "main version\n").unwrap();
        run_git(repo.path(), &["add", "conflict.txt"]);
        run_git(repo.path(), &["commit", "-m", "main conflict change"]);
        run_git(repo.path(), &["push", "origin", "main"]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-211-conflict", "done", "test/repo", 212, None);
        set_kv(&db, "merge_automation_enabled", "true");
        seed_completed_work_dispatch_target(
            &db,
            "impl-211-conflict",
            "card-211-conflict",
            "implementation",
            worktree_path.to_str().unwrap(),
            "wt/card-211-conflict",
            &feature_commit,
        );
        seed_worktree_session(&db, "session-211-conflict", worktree_path.to_str().unwrap());

        engine
            .try_fire_hook_by_name(
                "OnCardTerminal",
                serde_json::json!({"card_id": "card-211-conflict"}),
            )
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(get_card_status(&db, "card-211-conflict"), "done");
        assert_eq!(
            pr_tracking_state(&db, "card-211-conflict").as_deref(),
            Some("wait-ci")
        );
        assert_eq!(pr_tracking_pr_number(&db, "card-211-conflict"), Some(901));

        let conn = db.lock().unwrap();
        let blocked_reason: Option<String> = conn
            .query_row(
                "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-211-conflict'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(blocked_reason.as_deref(), Some("ci:waiting"));
        drop(conn);

        let log = gh_log(&gh._gh);
        assert!(
            log.contains("pr create --repo test/repo --base main --head wt/card-211-conflict"),
            "conflict fallback must create a PR for the tracked branch"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scenario_211_tick5min_retries_create_pr_rows_until_wait_ci() {
        let (repo, _remote, gh) = setup_test_repo_with_origin_and_mock_gh(&[
            MockGhReply {
                key: "pr:create",
                contains: Some("--head wt/card-211-create-pr-retry"),
                stdout: "https://github.com/test/repo/pull/905",
            },
            MockGhReply {
                key: "pr:view",
                contains: Some("--json headRefOid"),
                stdout: "feature-sha-211-create-pr-retry",
            },
        ]);
        let worktrees_dir = repo.path().join("worktrees");
        fs::create_dir_all(&worktrees_dir).unwrap();
        run_git(repo.path(), &["branch", "wt/card-211-create-pr-retry"]);

        let worktree_path = worktrees_dir.join("card-211-create-pr-retry");
        run_git(
            repo.path(),
            &[
                "worktree",
                "add",
                worktree_path.to_str().unwrap(),
                "wt/card-211-create-pr-retry",
            ],
        );
        fs::write(worktree_path.join("feature.txt"), "retry\n").unwrap();
        run_git(worktree_path.as_path(), &["add", "feature.txt"]);
        run_git(
            worktree_path.as_path(),
            &["commit", "-m", "feat: create-pr retry path #211"],
        );
        let feature_commit = run_git_output(worktree_path.as_path(), &["rev-parse", "HEAD"]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(
            &db,
            "card-211-create-pr-retry",
            "done",
            "test/repo",
            218,
            None,
        );
        set_kv(&db, "merge_automation_enabled", "true");
        seed_completed_work_dispatch_target(
            &db,
            "impl-211-create-pr-retry",
            "card-211-create-pr-retry",
            "implementation",
            worktree_path.to_str().unwrap(),
            "wt/card-211-create-pr-retry",
            &feature_commit,
        );
        seed_worktree_session(
            &db,
            "session-211-create-pr-retry",
            worktree_path.to_str().unwrap(),
        );
        seed_pr_tracking(
            &db,
            "card-211-create-pr-retry",
            "test/repo",
            Some(worktree_path.to_str().unwrap()),
            "wt/card-211-create-pr-retry",
            None,
            Some(feature_commit.as_str()),
            "create-pr",
        );
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET blocked_reason = 'pr:create_failed' WHERE id = 'card-211-create-pr-retry'",
                [],
            )
            .unwrap();
        }

        engine
            .try_fire_hook_by_name("OnTick5min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(get_card_status(&db, "card-211-create-pr-retry"), "done");
        assert_eq!(
            pr_tracking_state(&db, "card-211-create-pr-retry").as_deref(),
            Some("wait-ci")
        );
        assert_eq!(
            pr_tracking_pr_number(&db, "card-211-create-pr-retry"),
            Some(905)
        );

        let conn = db.lock().unwrap();
        let blocked_reason: Option<String> = conn
            .query_row(
                "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-211-create-pr-retry'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(blocked_reason.as_deref(), Some("ci:waiting"));
        drop(conn);

        let log = gh_log(&gh._gh);
        assert!(
            log.contains(
                "pr create --repo test/repo --base main --head wt/card-211-create-pr-retry"
            ),
            "create-pr rows must be retried on OnTick5min"
        );
    }

    #[test]
    fn scenario_576_terminal_merge_logs_when_card_is_missing() {
        let db = test_db();
        let policies_dir = setup_merge_policy_dir();
        let engine = test_engine_with_dir(&db, policies_dir.path());
        set_kv(&db, "merge_automation_enabled", "true");

        let (_, logs) = capture_policy_logs(|| {
            engine
                .try_fire_hook_by_name(
                    "OnCardTerminal",
                    serde_json::json!({"card_id": "card-576-missing"}),
                )
                .unwrap();
            kanban::drain_hook_side_effects(&db, &engine);
        });
        assert!(
            logs.contains("Card card-576-missing terminal merge skipped: card not found"),
            "card-not-found path must emit an explanatory merge log; logs={logs}"
        );
        assert!(
            logs.contains(
                "Card card-576-missing terminal merge candidate unresolved; skipping direct merge/PR fallback",
            ),
            "caller must log when the merge candidate cannot be resolved; logs={logs}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scenario_576_terminal_merge_logs_when_repo_id_is_missing() {
        let (repo, _repo_override) = setup_test_repo();
        let head_commit = run_git_output(repo.path(), &["rev-parse", "HEAD"]);

        let db = test_db();
        let policies_dir = setup_merge_policy_dir();
        let engine = test_engine_with_dir(&db, policies_dir.path());
        seed_agent(&db);
        seed_card(&db, "card-576-no-repo", "done");
        set_kv(&db, "merge_automation_enabled", "true");
        seed_completed_work_dispatch_target(
            &db,
            "impl-576-no-repo",
            "card-576-no-repo",
            "implementation",
            repo.path().to_str().unwrap(),
            "main",
            &head_commit,
        );

        let (_, logs) = capture_policy_logs(|| {
            engine
                .try_fire_hook_by_name(
                    "OnCardTerminal",
                    serde_json::json!({"card_id": "card-576-no-repo"}),
                )
                .unwrap();
            kanban::drain_hook_side_effects(&db, &engine);
        });
        assert!(
            logs.contains("Card card-576-no-repo terminal merge skipped: repo_id missing"),
            "repo_id-null path must emit an explanatory merge log; logs={logs}"
        );
        assert!(
            logs.contains(
                "Card card-576-no-repo terminal merge candidate unresolved; skipping direct merge/PR fallback",
            ),
            "caller must log when repo_id absence prevents merge candidate resolution; logs={logs}"
        );
    }

    #[test]
    fn scenario_576_terminal_merge_logs_when_worktree_target_is_missing() {
        let db = test_db();
        let policies_dir = setup_merge_policy_dir();
        let engine = test_engine_with_dir(&db, policies_dir.path());
        seed_agent(&db);
        seed_card_with_repo(&db, "card-576-no-target", "done", "test/repo", 576, None);
        set_kv(&db, "merge_automation_enabled", "true");

        let (_, logs) = capture_policy_logs(|| {
            engine
                .try_fire_hook_by_name(
                    "OnCardTerminal",
                    serde_json::json!({"card_id": "card-576-no-target"}),
                )
                .unwrap();
            kanban::drain_hook_side_effects(&db, &engine);
        });
        assert!(
            logs.contains(
                "Card card-576-no-target terminal merge skipped: missing worktree_path, branch",
            ),
            "missing worktree/branch path must emit an explanatory merge log; logs={logs}"
        );
        assert!(
            logs.contains(
                "Card card-576-no-target terminal merge candidate unresolved; skipping direct merge/PR fallback",
            ),
            "caller must log when worktree metadata is unavailable; logs={logs}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scenario_211_cleanup_closes_tracking_and_removes_worktree() {
        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "-b", "main"]);
        run_git(repo.path(), &["config", "user.email", "test@test.com"]);
        run_git(repo.path(), &["config", "user.name", "Test"]);
        run_git(repo.path(), &["commit", "--allow-empty", "-m", "initial"]);
        run_git(repo.path(), &["branch", "wt/card-211-cleanup"]);
        let worktrees_dir = repo.path().join("worktrees");
        fs::create_dir_all(&worktrees_dir).unwrap();
        let worktree_path = worktrees_dir.join("card-211-cleanup");
        run_git(
            repo.path(),
            &[
                "worktree",
                "add",
                worktree_path.to_str().unwrap(),
                "wt/card-211-cleanup",
            ],
        );
        let _gh = install_mock_gh(&[MockGhReply {
            key: "pr:view",
            contains: Some("--json mergedAt,headRefName"),
            stdout: "{\"mergedAt\":\"2026-04-09T00:00:00Z\",\"headRefName\":\"wt/card-211-cleanup\"}",
        }]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-211-cleanup", "done", "test/repo", 214, None);
        set_kv(&db, "merge_automation_enabled", "true");
        seed_pr_tracking(
            &db,
            "card-211-cleanup",
            "test/repo",
            Some(worktree_path.to_str().unwrap()),
            "wt/card-211-cleanup",
            Some(613),
            Some("ccc3333"),
            "post-merge-cleanup",
        );

        engine
            .try_fire_hook_by_name("OnTick5min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(
            pr_tracking_state(&db, "card-211-cleanup").as_deref(),
            Some("closed")
        );
        assert!(
            !worktree_path.exists(),
            "cleanup must remove the tracked worktree path"
        );

        let branch_output = Command::new("git")
            .args(["branch", "--list", "wt/card-211-cleanup"])
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&branch_output.stdout)
                .trim()
                .is_empty(),
            "cleanup must delete the tracked branch"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scenario_389_legacy_wait_ci_tracking_imports_into_canonical_lifecycle() {
        let _gh = install_mock_gh(&[
            MockGhReply {
                key: "pr:list",
                contains: Some("--head wt/card-389"),
                stdout: "[{\"number\":389,\"headRefName\":\"wt/card-389\",\"headRefOid\":\"eee5555\"}]",
            },
            MockGhReply {
                key: "pr:view",
                contains: Some("--json headRefOid"),
                stdout: "eee5555",
            },
            MockGhReply {
                key: "run:list",
                contains: Some("--branch wt/card-389"),
                stdout: "[{\"databaseId\":839,\"status\":\"completed\",\"conclusion\":\"success\",\"headSha\":\"eee5555\",\"event\":\"pull_request\"}]",
            },
        ]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-389", "review", "test/repo", 389, None);
        seed_completed_review_dispatch(&db, "review-389-pass", "card-389", "pass");
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET blocked_reason = 'ci:waiting' WHERE id = 'card-389'",
                [],
            )
            .unwrap();
        }
        set_kv(
            &db,
            "pr:card-389",
            r#"{"number":389,"repo":"test/repo","branch":"wt/card-389"}"#,
        );

        engine
            .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(pr_tracking_state(&db, "card-389").as_deref(), Some("merge"));
        assert_eq!(pr_tracking_pr_number(&db, "card-389"), Some(389));
        assert_eq!(
            pr_tracking_branch(&db, "card-389").as_deref(),
            Some("wt/card-389")
        );
        assert_eq!(get_card_status(&db, "card-389"), "done");

        let conn = db.lock().unwrap();
        let blocked_reason: Option<String> = conn
            .query_row(
                "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-389'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(blocked_reason, None);
    }

    #[cfg(unix)]
    #[test]
    fn scenario_690_dashboard_job_failure_becomes_code_rework() {
        let repo = tempfile::tempdir().unwrap();
        let gh = install_mock_gh(&[
            MockGhReply {
                key: "pr:view",
                contains: Some("--json headRefOid"),
                stdout: "sha-dashboard",
            },
            MockGhReply {
                key: "run:list",
                contains: Some("--branch wt/card-690-dashboard"),
                stdout: r#"[{"databaseId":6901,"status":"completed","conclusion":"failure","headSha":"sha-dashboard","event":"pull_request"}]"#,
            },
            MockGhReply {
                key: "run:view",
                contains: Some("--json jobs"),
                stdout: r#"{"jobs":[{"name":"Dashboard (Node 22)","conclusion":"failure"}]}"#,
            },
            MockGhReply {
                key: "run:view",
                contains: Some("--log-failed"),
                stdout: "dashboard build/test\nError: Vitest failed",
            },
        ]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-690-dashboard", "review", "test/repo", 690, None);
        seed_completed_review_dispatch(
            &db,
            "review-690-dashboard-pass",
            "card-690-dashboard",
            "pass",
        );
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET blocked_reason = 'ci:waiting' WHERE id = 'card-690-dashboard'",
                [],
            )
            .unwrap();
        }
        seed_pr_tracking(
            &db,
            "card-690-dashboard",
            "test/repo",
            Some(repo.path().to_str().unwrap()),
            "wt/card-690-dashboard",
            Some(690),
            Some("sha-dashboard"),
            "wait-ci",
        );

        let (_, policy_logs) = capture_policy_logs(|| {
            engine
                .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
                .unwrap();
            kanban::drain_hook_side_effects(&db, &engine);
        });

        assert_eq!(
            count_active_dispatches_by_type(&db, "card-690-dashboard", "rework"),
            1,
            "expected active rework dispatch; last_error={:?}; logs={}",
            pr_tracking_last_error(&db, "card-690-dashboard"),
            policy_logs
        );
        assert_eq!(get_card_status(&db, "card-690-dashboard"), "in_progress");
        assert_eq!(
            pr_tracking_state(&db, "card-690-dashboard").as_deref(),
            Some("wait-ci")
        );
        assert_eq!(
            latest_dispatch_title(&db, "card-690-dashboard", "rework").as_deref(),
            Some("[CI Fix] #690 Codex Card — Dashboard (Node 22)")
        );

        let conn = db.lock().unwrap();
        let blocked_reason: Option<String> = conn
            .query_row(
                "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-690-dashboard'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        assert_eq!(blocked_reason.as_deref(), Some("ci:rework"));
        assert!(
            pr_tracking_last_error(&db, "card-690-dashboard")
                .as_deref()
                .unwrap_or_default()
                .contains("code_job_match: failed jobs=Dashboard (Node 22)")
        );

        let log = gh_log(&gh);
        assert!(
            !log.contains("run rerun 6901"),
            "dashboard failures must go to rework, not rerun"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scenario_690_code_failure_duplicate_run_is_suppressed_after_review_reentry() {
        let repo = tempfile::tempdir().unwrap();
        let gh = install_mock_gh(&[
            MockGhReply {
                key: "pr:view",
                contains: Some("--json headRefOid"),
                stdout: "sha-dashboard-loop",
            },
            MockGhReply {
                key: "run:list",
                contains: Some("--branch wt/card-690-loop"),
                stdout: r#"[{"databaseId":6910,"status":"completed","conclusion":"failure","headSha":"sha-dashboard-loop","event":"pull_request"}]"#,
            },
            MockGhReply {
                key: "run:view",
                contains: Some("--json jobs"),
                stdout: r#"{"jobs":[{"name":"Script checks","conclusion":"failure"}]}"#,
            },
            MockGhReply {
                key: "run:view",
                contains: Some("--log-failed"),
                stdout: "generated docs are stale; rerun scripts/generate_inventory_docs.py",
            },
        ]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-690-loop", "review", "test/repo", 696, None);
        seed_completed_review_dispatch(&db, "review-690-loop-pass", "card-690-loop", "pass");
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET blocked_reason = 'ci:waiting' WHERE id = 'card-690-loop'",
                [],
            )
            .unwrap();
        }
        seed_pr_tracking(
            &db,
            "card-690-loop",
            "test/repo",
            Some(repo.path().to_str().unwrap()),
            "wt/card-690-loop",
            Some(696),
            Some("sha-dashboard-loop"),
            "wait-ci",
        );

        engine
            .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(count_dispatches_by_type(&db, "card-690-loop", "rework"), 1);
        assert_eq!(
            count_active_dispatches_by_type(&db, "card-690-loop", "rework"),
            1,
            "first CI failure should create exactly one active rework dispatch"
        );

        let rework_dispatch_id: String = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT id FROM task_dispatches \
                 WHERE kanban_card_id = 'card-690-loop' AND dispatch_type = 'rework' \
                 ORDER BY rowid DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        seed_assistant_response_for_dispatch(
            &db,
            &rework_dispatch_id,
            "attempted CI fix without creating a new commit",
        );
        let result = dispatch::complete_dispatch(
            &db,
            &engine,
            &rework_dispatch_id,
            &serde_json::json!({"completion_source": "test_harness"}),
        );
        assert!(
            result.is_ok(),
            "rework completion should succeed: {:?}",
            result.err()
        );
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(
            get_card_status(&db, "card-690-loop"),
            "review",
            "real rework completion should return the card to review"
        );
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "DELETE FROM kv_meta WHERE key = 'ci:card-690-loop:last_run_id'",
                [],
            )
            .unwrap();
        }

        {
            engine
                .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
                .unwrap();
        }
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(
            count_dispatches_by_type(&db, "card-690-loop", "rework"),
            1,
            "same completed CI run must not spawn another rework dispatch after the card re-enters review"
        );
        assert_eq!(
            get_card_status(&db, "card-690-loop"),
            "review",
            "duplicate failed run suppression must keep the card in review instead of looping back to rework"
        );
        let log = gh_log(&gh);
        assert_eq!(
            log.matches("issue comment 696 --repo test/repo --body")
                .count(),
            1,
            "same failed CI run must not accumulate duplicate review-status comments"
        );
        assert!(
            !log.contains("run rerun 6910"),
            "same failed CI run should be deduped rather than rerun or redispatched"
        );

        let metadata_json: String = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT metadata FROM kanban_cards WHERE id = 'card-690-loop'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        let metadata: serde_json::Value = serde_json::from_str(&metadata_json).unwrap();
        assert_eq!(
            metadata["loop_guard"]["ci_recovery"]["status"],
            "suppressed"
        );
        assert_eq!(metadata["loop_guard"]["ci_recovery"]["suppress_count"], 1);
        assert_eq!(
            metadata["loop_guard"]["ci_recovery"]["classification"],
            "code_failure"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scenario_690_new_failed_run_is_not_blocked_by_prior_loop_fingerprint() {
        let repo = tempfile::tempdir().unwrap();
        let _gh = install_mock_gh(&[
            MockGhReply {
                key: "pr:view",
                contains: Some("--json headRefOid"),
                stdout: "sha-dashboard-new-run",
            },
            MockGhReply {
                key: "run:list",
                contains: Some("--branch wt/card-690-new-run"),
                stdout: r#"[{"databaseId":6911,"status":"completed","conclusion":"failure","headSha":"sha-dashboard-new-run","event":"pull_request"}]"#,
            },
            MockGhReply {
                key: "run:view",
                contains: Some("--json jobs"),
                stdout: r#"{"jobs":[{"name":"Script checks","conclusion":"failure"}]}"#,
            },
            MockGhReply {
                key: "run:view",
                contains: Some("--log-failed"),
                stdout: "generated docs are stale; rerun scripts/generate_inventory_docs.py",
            },
        ]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-690-new-run", "review", "test/repo", 697, None);
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET blocked_reason = 'ci:waiting' WHERE id = 'card-690-new-run'",
                [],
            )
            .unwrap();
        }
        seed_pr_tracking(
            &db,
            "card-690-new-run",
            "test/repo",
            Some(repo.path().to_str().unwrap()),
            "wt/card-690-new-run",
            Some(697),
            Some("sha-dashboard-new-run"),
            "wait-ci",
        );
        set_kv(&db, "ci:card-690-new-run:last_run_id", "6910");
        set_kv(
            &db,
            "loop_guard:ci_recovery:card-690-new-run",
            &serde_json::json!({
                "status": "active",
                "action": "rework_dispatched",
                "fingerprint": "card-690-new-run::sha-dashboard-new-run::6910::code_failure",
                "base_fingerprint": "card-690-new-run::sha-dashboard-new-run::6910",
                "classification": "code_failure",
                "run_id": "6910",
                "head_sha": "sha-dashboard-new-run",
                "suppress_count": 2,
                "last_reason": "code_job_match: failed jobs=Script checks"
            })
            .to_string(),
        );

        engine
            .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(
            count_active_dispatches_by_type(&db, "card-690-new-run", "rework"),
            1,
            "a new failed run_id must still create a fresh rework dispatch"
        );

        let metadata_json: String = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT metadata FROM kanban_cards WHERE id = 'card-690-new-run'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        let metadata: serde_json::Value = serde_json::from_str(&metadata_json).unwrap();
        assert_eq!(metadata["loop_guard"]["ci_recovery"]["run_id"], "6911");
        assert_eq!(metadata["loop_guard"]["ci_recovery"]["status"], "active");
        assert_eq!(
            metadata["loop_guard"]["ci_recovery"]["action"],
            "rework_dispatched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scenario_741_review_loop_guard_escalates_same_head_review_churn() {
        let gh = install_mock_gh(&[]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(
            &db,
            "card-741-review-loop",
            "in_progress",
            "test/repo",
            741,
            None,
        );
        seed_pr_tracking(
            &db,
            "card-741-review-loop",
            "test/repo",
            None,
            "wt/card-741-review-loop",
            Some(741),
            Some("sha-review-loop"),
            "wait-ci",
        );

        for idx in 1..=3 {
            let dispatch_id = format!("rw-741-loop-{idx}");
            seed_dispatch(
                &db,
                &dispatch_id,
                "card-741-review-loop",
                "rework",
                "pending",
            );
            seed_assistant_response_for_dispatch(&db, &dispatch_id, "repeat review loop");

            let result = dispatch::complete_dispatch(
                &db,
                &engine,
                &dispatch_id,
                &serde_json::json!({"completion_source": "test_harness"}),
            );
            assert!(
                result.is_ok(),
                "rework completion should succeed on loop attempt {idx}: {:?}",
                result.err()
            );
            kanban::drain_hook_side_effects(&db, &engine);

            if idx < 3 {
                let conn = db.lock().unwrap();
                conn.execute(
                    "UPDATE task_dispatches \
                     SET status = 'completed', completed_at = COALESCE(completed_at, datetime('now')), updated_at = datetime('now') \
                     WHERE kanban_card_id = 'card-741-review-loop' AND dispatch_type = 'review' \
                     AND status IN ('pending', 'dispatched')",
                    [],
                )
                .unwrap();
                conn.execute(
                    "UPDATE kanban_cards \
                     SET status = 'in_progress', review_status = NULL, blocked_reason = NULL, updated_at = datetime('now') \
                     WHERE id = 'card-741-review-loop'",
                    [],
                )
                .unwrap();
            }
        }

        let (review_status, blocked_reason, metadata_json): (
            Option<String>,
            Option<String>,
            String,
        ) = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT review_status, blocked_reason, metadata FROM kanban_cards WHERE id = 'card-741-review-loop'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
        };
        assert_eq!(review_status.as_deref(), Some("dilemma_pending"));
        assert!(
            blocked_reason
                .as_deref()
                .unwrap_or_default()
                .contains("Review loop guard"),
            "loop guard escalation must explain itself in blocked_reason"
        );
        assert_eq!(
            count_active_dispatches_by_type(&db, "card-741-review-loop", "review"),
            0,
            "once review churn is escalated, a new review dispatch must not remain active"
        );

        let metadata: serde_json::Value = serde_json::from_str(&metadata_json).unwrap();
        assert_eq!(
            metadata["loop_guard"]["review_churn"]["status"],
            "escalated"
        );
        assert!(
            metadata["loop_guard"]["review_churn"]["enter_count"]
                .as_i64()
                .unwrap_or_default()
                >= 3,
            "review churn guard must record that the same head crossed the escalation threshold"
        );
        assert!(
            metadata["loop_guard"]["review_churn"]["escalation_reason"]
                .as_str()
                .unwrap_or_default()
                .contains("Review loop guard")
        );

        let log = gh_log(&gh);
        assert_eq!(
            log.matches("issue comment 741 --repo test/repo --body")
                .count(),
            3,
            "review churn test must observe the repeated review-status comment path before escalating"
        );
    }

    /// #751 (Codex follow-up on PR #749): reviewLoopFingerprintInfo must
    /// source head_sha from the latest completed work dispatch first, not
    /// pr_tracking. pr_tracking.head_sha is refreshed only by the CI
    /// recovery polling path (onTick1min) and lags fast rework/review
    /// cycles — if the guard used the stale pr_tracking value, three
    /// *distinct* rework completions (each with a different head_sha)
    /// would still share a fingerprint and incorrectly trip the
    /// same-head loop guard.
    ///
    /// This test seeds a stale pr_tracking.head_sha and three rework
    /// completions with distinct head_shas; the loop guard must NOT
    /// escalate — each fingerprint is unique.
    #[cfg(unix)]
    #[test]
    fn scenario_751_review_loop_fingerprint_uses_latest_work_head_not_stale_pr_tracking() {
        let _gh = install_mock_gh(&[]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(
            &db,
            "card-751-fresh-head",
            "in_progress",
            "test/repo",
            751,
            None,
        );
        // Stale pr_tracking — NEVER advances in this test. If the guard
        // fingerprints off this value, all 3 cycles share a fingerprint.
        seed_pr_tracking(
            &db,
            "card-751-fresh-head",
            "test/repo",
            None,
            "wt/card-751-fresh-head",
            Some(751),
            Some("sha-stale-tracking-never-refreshes"),
            "wait-ci",
        );

        for idx in 1..=3 {
            let dispatch_id = format!("rw-751-fresh-{idx}");
            // Distinct head_sha per iteration — simulates fast rework
            // cycles producing new commits before CI recovery polls.
            let fresh_head = format!("sha-fresh-rework-{idx}");

            seed_dispatch(
                &db,
                &dispatch_id,
                "card-751-fresh-head",
                "rework",
                "pending",
            );
            seed_assistant_response_for_dispatch(&db, &dispatch_id, "fresh rework head");

            // Pass completed_commit via the completion result so
            // loadLatestCompletedWorkTarget surfaces the fresh head when
            // reviewLoopFingerprintInfo runs inside OnDispatchCompleted.
            let result = dispatch::complete_dispatch(
                &db,
                &engine,
                &dispatch_id,
                &serde_json::json!({
                    "completion_source": "test_harness",
                    "completed_commit": fresh_head,
                    "completed_branch": "wt/card-751-fresh-head",
                }),
            );
            assert!(
                result.is_ok(),
                "rework completion should succeed on fresh-head attempt {idx}: {:?}",
                result.err()
            );
            kanban::drain_hook_side_effects(&db, &engine);

            if idx < 3 {
                let conn = db.lock().unwrap();
                conn.execute(
                    "UPDATE task_dispatches \
                     SET status = 'completed', completed_at = COALESCE(completed_at, datetime('now')), updated_at = datetime('now') \
                     WHERE kanban_card_id = 'card-751-fresh-head' AND dispatch_type = 'review' \
                     AND status IN ('pending', 'dispatched')",
                    [],
                )
                .unwrap();
                conn.execute(
                    "UPDATE kanban_cards \
                     SET status = 'in_progress', review_status = NULL, blocked_reason = NULL, updated_at = datetime('now') \
                     WHERE id = 'card-751-fresh-head'",
                    [],
                )
                .unwrap();
            }
        }

        let (review_status, metadata_json): (Option<String>, String) = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT review_status, metadata FROM kanban_cards WHERE id = 'card-751-fresh-head'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
        };
        // NOT escalated — each rework had a distinct head_sha so the guard
        // must treat them as separate fingerprints.
        assert_ne!(
            review_status.as_deref(),
            Some("dilemma_pending"),
            "distinct-head rework cycles must NOT be escalated as same-head churn"
        );
        let metadata: serde_json::Value = serde_json::from_str(&metadata_json).unwrap();
        // enter_count should be 1 (latest fingerprint, not accumulated).
        let enter_count = metadata["loop_guard"]["review_churn"]["enter_count"]
            .as_i64()
            .unwrap_or(0);
        assert!(
            enter_count < 3,
            "distinct-head fingerprints must not accumulate into same-head churn (enter_count={})",
            enter_count
        );
        let guard_head = metadata["loop_guard"]["review_churn"]["head_sha"]
            .as_str()
            .unwrap_or("");
        assert!(
            guard_head.starts_with("sha-fresh-rework-"),
            "loop guard must source head_sha from the latest completed work, not stale pr_tracking (got '{}')",
            guard_head
        );
    }

    #[cfg(unix)]
    #[test]
    fn scenario_690_high_risk_recovery_job_failure_becomes_code_rework() {
        let repo = tempfile::tempdir().unwrap();
        let gh = install_mock_gh(&[
            MockGhReply {
                key: "pr:view",
                contains: Some("--json headRefOid"),
                stdout: "sha-recovery",
            },
            MockGhReply {
                key: "run:list",
                contains: Some("--branch wt/card-690-recovery"),
                stdout: r#"[{"databaseId":6902,"status":"completed","conclusion":"failure","headSha":"sha-recovery","event":"pull_request"}]"#,
            },
            MockGhReply {
                key: "run:view",
                contains: Some("--json jobs"),
                stdout: r#"{"jobs":[{"name":"High-risk recovery","conclusion":"failure"}]}"#,
            },
            MockGhReply {
                key: "run:view",
                contains: Some("--log-failed"),
                stdout: "cargo test --bin agentdesk high_risk_recovery::\nthread '...' panicked",
            },
        ]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-690-recovery", "review", "test/repo", 691, None);
        seed_completed_review_dispatch(
            &db,
            "review-690-recovery-pass",
            "card-690-recovery",
            "pass",
        );
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET blocked_reason = 'ci:waiting' WHERE id = 'card-690-recovery'",
                [],
            )
            .unwrap();
        }
        seed_pr_tracking(
            &db,
            "card-690-recovery",
            "test/repo",
            Some(repo.path().to_str().unwrap()),
            "wt/card-690-recovery",
            Some(691),
            Some("sha-recovery"),
            "wait-ci",
        );

        let (_, policy_logs) = capture_policy_logs(|| {
            engine
                .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
                .unwrap();
            kanban::drain_hook_side_effects(&db, &engine);
        });

        assert_eq!(
            count_active_dispatches_by_type(&db, "card-690-recovery", "rework"),
            1,
            "expected active rework dispatch; last_error={:?}; logs={}",
            pr_tracking_last_error(&db, "card-690-recovery"),
            policy_logs
        );
        assert_eq!(get_card_status(&db, "card-690-recovery"), "in_progress");
        assert_eq!(
            latest_dispatch_title(&db, "card-690-recovery", "rework").as_deref(),
            Some("[CI Fix] #691 Codex Card — High-risk recovery")
        );
        assert!(
            pr_tracking_last_error(&db, "card-690-recovery")
                .as_deref()
                .unwrap_or_default()
                .contains("code_job_match: failed jobs=High-risk recovery")
        );
        assert!(
            !gh_log(&gh).contains("run rerun 6902"),
            "high-risk recovery failures must go to rework, not rerun"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scenario_690_workflow_file_issue_escalates_with_manual_bucket() {
        let gh = install_mock_gh(&[
            MockGhReply {
                key: "pr:view",
                contains: Some("--json headRefOid"),
                stdout: "sha-workflow",
            },
            MockGhReply {
                key: "run:list",
                contains: Some("--branch wt/card-690-workflow"),
                stdout: r#"[{"databaseId":6903,"status":"completed","conclusion":"failure","headSha":"sha-workflow","event":"pull_request"}]"#,
            },
            MockGhReply {
                key: "run:view",
                contains: Some("--json jobs"),
                stdout: r#"{"jobs":[]}"#,
            },
            MockGhReply {
                key: "run:view",
                contains: Some("--log-failed"),
                stdout: "failed to get run log: log not found",
            },
            MockGhReply {
                key: "run:view",
                contains: None,
                stdout: "X wt/card-690-workflow CI test/repo#690 · 6903\nTriggered via push about 3 days ago\n\nX This run likely failed because of a workflow file issue.\n\nFor more information, see: https://github.com/test/repo/actions/runs/6903",
            },
        ]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-690-workflow", "review", "test/repo", 692, None);
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET blocked_reason = 'ci:waiting' WHERE id = 'card-690-workflow'",
                [],
            )
            .unwrap();
        }
        seed_pr_tracking(
            &db,
            "card-690-workflow",
            "test/repo",
            None,
            "wt/card-690-workflow",
            Some(692),
            Some("sha-workflow"),
            "wait-ci",
        );

        engine
            .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(
            pr_tracking_state(&db, "card-690-workflow").as_deref(),
            Some("escalated")
        );
        assert_eq!(get_card_status(&db, "card-690-workflow"), "review");
        assert_eq!(
            review_state_value(&db, "card-690-workflow").as_deref(),
            Some("dilemma_pending")
        );
        assert!(
            pr_tracking_last_error(&db, "card-690-workflow")
                .as_deref()
                .unwrap_or_default()
                .contains("CI manual intervention: workflow_file_issue: workflow file issue")
        );

        let conn = db.lock().unwrap();
        let (blocked_reason, review_status): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT blocked_reason, review_status FROM kanban_cards WHERE id = 'card-690-workflow'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        drop(conn);
        assert_eq!(review_status.as_deref(), Some("dilemma_pending"));
        assert!(blocked_reason.as_deref().unwrap_or_default().contains(
            "manual intervention required for run 6903: workflow_file_issue: workflow file issue"
        ));
        assert_eq!(
            escalation_pending_reasons(&db, "card-690-workflow"),
            vec![blocked_reason.unwrap()]
        );
        assert!(
            !gh_log(&gh).contains("run rerun 6903"),
            "workflow file issues must escalate directly without rerun"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scenario_690_transient_failure_reruns_within_retry_budget() {
        let gh = install_mock_gh(&[
            MockGhReply {
                key: "pr:view",
                contains: Some("--json headRefOid"),
                stdout: "sha-transient",
            },
            MockGhReply {
                key: "run:list",
                contains: Some("--branch wt/card-690-transient"),
                stdout: r#"[{"databaseId":6904,"status":"completed","conclusion":"failure","headSha":"sha-transient","event":"pull_request"}]"#,
            },
            MockGhReply {
                key: "run:view",
                contains: Some("--json jobs"),
                stdout: r#"{"jobs":[{"name":"mystery-job","conclusion":"failure"}]}"#,
            },
            MockGhReply {
                key: "run:view",
                contains: Some("--log-failed"),
                stdout: "error: dependency fetch failed\nconnection timed out while downloading crate index",
            },
            MockGhReply {
                key: "run:rerun",
                contains: Some("--failed"),
                stdout: "rerun queued",
            },
        ]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-690-transient", "review", "test/repo", 693, None);
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET blocked_reason = 'ci:waiting' WHERE id = 'card-690-transient'",
                [],
            )
            .unwrap();
        }
        seed_pr_tracking(
            &db,
            "card-690-transient",
            "test/repo",
            None,
            "wt/card-690-transient",
            Some(693),
            Some("sha-transient"),
            "wait-ci",
        );

        engine
            .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        let conn = db.lock().unwrap();
        let blocked_reason: Option<String> = conn
            .query_row(
                "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-690-transient'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        assert_eq!(blocked_reason.as_deref(), Some("ci:rerunning"));
        assert_eq!(
            kv_value(&db, "ci:card-690-transient:retry_count").as_deref(),
            Some("1")
        );
        assert_eq!(
            pr_tracking_state(&db, "card-690-transient").as_deref(),
            Some("wait-ci")
        );
        assert!(
            gh_log(&gh).contains("run rerun 6904"),
            "transient failures must rerun failed jobs while budget remains"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scenario_690_transient_failure_stops_at_retry_limit() {
        let gh = install_mock_gh(&[
            MockGhReply {
                key: "pr:view",
                contains: Some("--json headRefOid"),
                stdout: "sha-transient-max",
            },
            MockGhReply {
                key: "run:list",
                contains: Some("--branch wt/card-690-transient-max"),
                stdout: r#"[{"databaseId":6905,"status":"completed","conclusion":"failure","headSha":"sha-transient-max","event":"pull_request"}]"#,
            },
            MockGhReply {
                key: "run:view",
                contains: Some("--json jobs"),
                stdout: r#"{"jobs":[{"name":"mystery-job","conclusion":"failure"}]}"#,
            },
            MockGhReply {
                key: "run:view",
                contains: Some("--log-failed"),
                stdout: "error: network unreachable while fetching dependency archive",
            },
        ]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(
            &db,
            "card-690-transient-max",
            "review",
            "test/repo",
            694,
            None,
        );
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET blocked_reason = 'ci:waiting' WHERE id = 'card-690-transient-max'",
                [],
            )
            .unwrap();
        }
        seed_pr_tracking(
            &db,
            "card-690-transient-max",
            "test/repo",
            None,
            "wt/card-690-transient-max",
            Some(694),
            Some("sha-transient-max"),
            "wait-ci",
        );
        set_kv(&db, "ci:card-690-transient-max:retry_count", "3");

        engine
            .try_fire_hook_by_name("OnTick1min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(
            pr_tracking_state(&db, "card-690-transient-max").as_deref(),
            Some("escalated")
        );
        assert!(
            pr_tracking_last_error(&db, "card-690-transient-max")
                .as_deref()
                .unwrap_or_default()
                .contains("CI transient failure — max retries exhausted")
        );

        let conn = db.lock().unwrap();
        let blocked_reason: Option<String> = conn
            .query_row(
                "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-690-transient-max'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);
        assert!(
            blocked_reason
                .as_deref()
                .unwrap_or_default()
                .contains("max retries (3) exhausted for run 6905")
        );
        assert!(
            !gh_log(&gh).contains("run rerun 6905"),
            "transient failures must stop rerunning after CI_MAX_RETRIES"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scenario_208_on_tick_creates_codex_follow_up_issue_and_dedups_review() {
        let (repo, env) = setup_test_repo_with_mock_gh(&[
            MockGhReply {
                key: "pr:list",
                contains: Some("--state merged"),
                stdout: "[]",
            },
            MockGhReply {
                key: "pr:list",
                contains: None,
                stdout: "[{\"number\":323,\"headRefName\":\"wt/card-208\",\"title\":\"fix: close review gap (#208)\",\"mergeable\":\"MERGEABLE\"}]",
            },
            MockGhReply {
                key: "api:repos/test/repo/pulls/323/reviews",
                contains: None,
                stdout: "[{\"id\":9001,\"state\":\"COMMENTED\",\"body\":\"P1/P2 findings\",\"submitted_at\":\"2026-04-06T00:00:00Z\",\"user\":{\"login\":\"chatgpt-codex-connector\"}}]",
            },
            MockGhReply {
                key: "api:graphql",
                contains: None,
                stdout: "{\"data\":{\"repository\":{\"pullRequest\":{\"reviewThreads\":{\"nodes\":[{\"id\":\"thread-1\",\"isResolved\":false,\"isOutdated\":false,\"comments\":{\"nodes\":[{\"id\":\"comment-1\",\"body\":\"P1 force-transition leaves dispatch alive\",\"path\":\"src/server/routes/github.rs\",\"line\":77,\"url\":\"https://example.com/comment-1\",\"author\":{\"login\":\"chatgpt-codex-connector\"},\"pullRequestReview\":{\"id\":\"PRR_9001\",\"state\":\"COMMENTED\",\"author\":{\"login\":\"chatgpt-codex-connector\"}}}]}}]}}}}}",
            },
            MockGhReply {
                key: "label:create",
                contains: Some("agent:agent-1"),
                stdout: "label ok",
            },
            MockGhReply {
                key: "issue:create",
                contains: Some("--label agent:agent-1"),
                stdout: "https://github.com/test/repo/issues/900",
            },
        ]);
        run_git(
            repo.path(),
            &["remote", "add", "origin", "git@github.com:test/repo.git"],
        );

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-208", "done", "test/repo", 208, None);
        seed_thread_session(&db, "s-208", "thread-208");
        set_kv(&db, "merge_automation_enabled", "true");
        set_kv(
            &db,
            "pr:card-208",
            r#"{"number":323,"repo":"test/repo","branch":"wt/card-208"}"#,
        );

        engine
            .try_fire_hook_by_name("OnTick5min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(get_card_status(&db, "card-208"), "done");
        assert_eq!(count_dispatches_by_type(&db, "card-208", "rework"), 0);
        assert_eq!(review_state_value(&db, "card-208").as_deref(), None);

        let conn = db.lock().unwrap();
        let (followup_status, followup_title, followup_description, followup_metadata): (
            String,
            String,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT status, title, description, metadata \
                 FROM kanban_cards WHERE repo_id = 'test/repo' AND github_issue_number = 900",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        drop(conn);
        assert_eq!(followup_status, "backlog");
        assert!(followup_title.contains("PR #323"));
        assert!(
            followup_description
                .as_deref()
                .unwrap_or_default()
                .contains("src/server/routes/github.rs:77")
        );
        let followup_metadata_json: serde_json::Value =
            serde_json::from_str(followup_metadata.as_deref().unwrap_or("{}")).unwrap();
        assert_eq!(followup_metadata_json["labels"], "agent:agent-1");

        let messages = message_outbox_rows(&db);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].0, "thread-208");
        assert!(messages[0].1.contains("PR #323"));
        assert!(messages[0].1.contains("follow-up 이슈를 생성했습니다"));
        assert!(
            messages[0]
                .1
                .contains("https://github.com/test/repo/issues/900")
        );

        engine
            .try_fire_hook_by_name("OnTick5min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(count_dispatches_by_type(&db, "card-208", "rework"), 0);
        assert_eq!(message_outbox_rows(&db).len(), 1);

        let log = gh_log(&env._gh);
        assert_eq!(
            log.matches("label create agent:agent-1").count(),
            1,
            "Codex follow-up flow must ensure the agent label only once"
        );
        assert_eq!(
            log.matches("issue create --repo test/repo").count(),
            1,
            "Codex follow-up flow must create exactly one follow-up issue per review"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scenario_615_codex_followup_dedups_even_if_backlog_insert_fails() {
        let (repo, env) = setup_test_repo_with_mock_gh(&[
            MockGhReply {
                key: "pr:list",
                contains: Some("--state merged"),
                stdout: "[]",
            },
            MockGhReply {
                key: "pr:list",
                contains: None,
                stdout: "[{\"number\":325,\"headRefName\":\"wt/card-615-dedup\",\"title\":\"fix: follow-up dedup (#615)\",\"mergeable\":\"MERGEABLE\"}]",
            },
            MockGhReply {
                key: "api:repos/test/repo/pulls/325/reviews",
                contains: None,
                stdout: "[{\"id\":9003,\"state\":\"COMMENTED\",\"body\":\"P1 blocking finding\",\"submitted_at\":\"2026-04-06T00:00:00Z\",\"user\":{\"login\":\"chatgpt-codex-connector\"}}]",
            },
            MockGhReply {
                key: "api:graphql",
                contains: None,
                stdout: "{\"data\":{\"repository\":{\"pullRequest\":{\"reviewThreads\":{\"nodes\":[{\"id\":\"thread-615\",\"isResolved\":false,\"isOutdated\":false,\"comments\":{\"nodes\":[{\"id\":\"comment-615\",\"body\":\"P1 keep dedup even if backlog insert fails\",\"path\":\"policies/merge-automation.js\",\"line\":1209,\"url\":\"https://example.com/comment-615\",\"author\":{\"login\":\"chatgpt-codex-connector\"},\"pullRequestReview\":{\"id\":\"PRR_9003\",\"state\":\"COMMENTED\",\"author\":{\"login\":\"chatgpt-codex-connector\"}}}]}}]}}}}}",
            },
            MockGhReply {
                key: "label:create",
                contains: Some("agent:agent-1"),
                stdout: "label ok",
            },
            MockGhReply {
                key: "issue:create",
                contains: Some("--label agent:agent-1"),
                stdout: "https://github.com/test/repo/issues/901",
            },
        ]);
        run_git(
            repo.path(),
            &["remote", "add", "origin", "git@github.com:test/repo.git"],
        );

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-615-dedup", "done", "test/repo", 615, None);
        seed_thread_session(&db, "s-615-dedup", "thread-615-dedup");
        set_kv(&db, "merge_automation_enabled", "true");
        set_kv(
            &db,
            "pr:card-615-dedup",
            r#"{"number":325,"repo":"test/repo","branch":"wt/card-615-dedup"}"#,
        );
        {
            let conn = db.lock().unwrap();
            conn.execute_batch(
                "CREATE TRIGGER fail_codex_followup_backlog_insert
                 BEFORE INSERT ON kanban_cards
                 WHEN NEW.id LIKE 'codex-followup-%'
                 BEGIN
                   SELECT RAISE(FAIL, 'boom');
                 END;",
            )
            .unwrap();
        }

        engine
            .try_fire_hook_by_name("OnTick5min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        engine
            .try_fire_hook_by_name("OnTick5min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        let conn = db.lock().unwrap();
        let followup_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kanban_cards \
                 WHERE repo_id = 'test/repo' AND github_issue_number = 901",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);

        assert_eq!(
            followup_count, 0,
            "failed backlog insert must not leave partial local card"
        );
        assert_eq!(
            gh_log(&env._gh).matches("issue create").count(),
            1,
            "issue creation must still dedup even when backlog insert fails"
        );
        assert!(
            message_outbox_rows(&db).is_empty(),
            "failed backlog insert should not emit success notification"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scenario_615_codex_followup_rejects_invalid_issue_url() {
        let (repo, env) = setup_test_repo_with_mock_gh(&[
            MockGhReply {
                key: "pr:list",
                contains: Some("--state merged"),
                stdout: "[]",
            },
            MockGhReply {
                key: "pr:list",
                contains: None,
                stdout: "[{\"number\":326,\"headRefName\":\"wt/card-615-url\",\"title\":\"fix: validate follow-up url (#615)\",\"mergeable\":\"MERGEABLE\"}]",
            },
            MockGhReply {
                key: "api:repos/test/repo/pulls/326/reviews",
                contains: None,
                stdout: "[{\"id\":9004,\"state\":\"COMMENTED\",\"body\":\"P2 malformed follow-up url risk\",\"submitted_at\":\"2026-04-06T00:00:00Z\",\"user\":{\"login\":\"chatgpt-codex-connector\"}}]",
            },
            MockGhReply {
                key: "api:graphql",
                contains: None,
                stdout: "{\"data\":{\"repository\":{\"pullRequest\":{\"reviewThreads\":{\"nodes\":[{\"id\":\"thread-615-url\",\"isResolved\":false,\"isOutdated\":false,\"comments\":{\"nodes\":[{\"id\":\"comment-615-url\",\"body\":\"P2 validate issue URL\",\"path\":\"policies/merge-automation.js\",\"line\":1167,\"url\":\"https://example.com/comment-615-url\",\"author\":{\"login\":\"chatgpt-codex-connector\"},\"pullRequestReview\":{\"id\":\"PRR_9004\",\"state\":\"COMMENTED\",\"author\":{\"login\":\"chatgpt-codex-connector\"}}}]}}]}}}}}",
            },
            MockGhReply {
                key: "label:create",
                contains: Some("agent:agent-1"),
                stdout: "label ok",
            },
            MockGhReply {
                key: "issue:create",
                contains: Some("--label agent:agent-1"),
                stdout: "https://github.com/test/repo/pull/901",
            },
        ]);
        run_git(
            repo.path(),
            &["remote", "add", "origin", "git@github.com:test/repo.git"],
        );

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-615-url", "done", "test/repo", 615, None);
        seed_thread_session(&db, "s-615-url", "thread-615-url");
        set_kv(&db, "merge_automation_enabled", "true");
        set_kv(
            &db,
            "pr:card-615-url",
            r#"{"number":326,"repo":"test/repo","branch":"wt/card-615-url"}"#,
        );

        engine
            .try_fire_hook_by_name("OnTick5min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        let conn = db.lock().unwrap();
        let followup_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kanban_cards \
                 WHERE repo_id = 'test/repo' AND title LIKE '%PR #326%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);

        assert_eq!(
            followup_count, 0,
            "invalid issue URL must not create a backlog card"
        );
        assert!(
            message_outbox_rows(&db).is_empty(),
            "invalid issue URL must not emit a follow-up success notification"
        );
        assert_eq!(
            gh_log(&env._gh).matches("issue create").count(),
            1,
            "invalid issue URL should fail fast without fallback loops inside one tick"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scenario_208_on_tick_notifies_clean_codex_pass() {
        let _gh = install_mock_gh(&[
            MockGhReply {
                key: "pr:list",
                contains: Some("--state merged"),
                stdout: "[]",
            },
            MockGhReply {
                key: "pr:list",
                contains: None,
                stdout: "[{\"number\":324,\"headRefName\":\"wt/card-208-pass\",\"title\":\"fix: no inline findings (#209)\",\"mergeable\":\"MERGEABLE\"}]",
            },
            MockGhReply {
                key: "api:repos/test/repo/pulls/324/reviews",
                contains: None,
                stdout: "[{\"id\":9002,\"state\":\"APPROVED\",\"body\":\"LGTM\",\"submitted_at\":\"2026-04-06T00:05:00Z\",\"user\":{\"login\":\"chatgpt-codex-connector\"}}]",
            },
            MockGhReply {
                key: "api:graphql",
                contains: None,
                stdout: "{\"data\":{\"repository\":{\"pullRequest\":{\"reviewThreads\":{\"nodes\":[]}}}}}",
            },
        ]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(
            &db,
            "card-208-pass",
            "review",
            "test/repo",
            209,
            Some("thread-pass"),
        );
        set_kv(&db, "merge_automation_enabled", "true");
        set_kv(
            &db,
            "pr:card-208-pass",
            r#"{"number":324,"repo":"test/repo","branch":"wt/card-208-pass"}"#,
        );

        engine
            .try_fire_hook_by_name("OnTick5min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(count_dispatches_by_type(&db, "card-208-pass", "rework"), 0);

        let messages = message_outbox_rows(&db);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].0, "thread-pass");
        assert!(messages[0].1.contains("Codex 리뷰 통과"));

        engine
            .try_fire_hook_by_name("OnTick5min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(message_outbox_rows(&db).len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn scenario_208_merge_guard_blocks_unresolved_codex_comments() {
        let gh = install_mock_gh(&[
            MockGhReply {
                key: "pr:view",
                contains: Some("--json author"),
                stdout: "itismyfield",
            },
            MockGhReply {
                key: "pr:view",
                contains: Some("--json headRefOid"),
                stdout: "ddd4444",
            },
            MockGhReply {
                key: "run:list",
                contains: Some("--branch wt/card-208-guard"),
                stdout: "[{\"databaseId\":725,\"status\":\"completed\",\"conclusion\":\"success\",\"headSha\":\"ddd4444\",\"event\":\"pull_request\"}]",
            },
            MockGhReply {
                key: "api:repos/test/repo/pulls/325/reviews",
                contains: None,
                stdout: "[{\"id\":9003,\"state\":\"COMMENTED\",\"body\":\"P2 findings\",\"submitted_at\":\"2026-04-06T00:10:00Z\",\"user\":{\"login\":\"chatgpt-codex-connector\"}}]",
            },
            MockGhReply {
                key: "api:graphql",
                contains: None,
                stdout: "{\"data\":{\"repository\":{\"pullRequest\":{\"reviewThreads\":{\"nodes\":[{\"id\":\"thread-2\",\"isResolved\":false,\"isOutdated\":false,\"comments\":{\"nodes\":[{\"id\":\"comment-2\",\"body\":\"P2 orphan recovery revives reverted card\",\"path\":\"src/kanban.rs\",\"line\":212,\"url\":\"https://example.com/comment-2\",\"author\":{\"login\":\"chatgpt-codex-connector\"},\"pullRequestReview\":{\"id\":\"PRR_9003\",\"state\":\"COMMENTED\",\"author\":{\"login\":\"chatgpt-codex-connector\"}}}]}}]}}}}}",
            },
            MockGhReply {
                key: "pr:merge",
                contains: None,
                stdout: "merged",
            },
        ]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-208-guard", "review", "test/repo", 210, None);
        seed_thread_session(&db, "s-208-guard", "thread-guard");
        set_kv(&db, "merge_automation_enabled", "true");
        set_kv(&db, "merge_allowed_authors", "itismyfield");
        seed_pr_tracking(
            &db,
            "card-208-guard",
            "test/repo",
            None,
            "wt/card-208-guard",
            Some(325),
            Some("ddd4444"),
            "merge",
        );

        assert!(
            kanban::transition_status_with_opts(
                &db,
                &engine,
                "card-208-guard",
                "done",
                "test",
                true,
            )
            .is_ok()
        );

        assert_eq!(get_card_status(&db, "card-208-guard"), "done");

        let log = gh_log(&gh);
        assert!(log.contains("pr view 325"));
        assert!(
            !log.contains("pr merge 325"),
            "merge guard must prevent gh pr merge when unresolved Codex comments exist"
        );

        let messages = message_outbox_rows(&db);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].0, "thread-guard");
        assert!(messages[0].1.contains("merge를 차단했습니다"));

        let conn = db.lock().unwrap();
        let blocked: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kv_meta WHERE key = 'merge_blocked:card-208-guard'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(blocked, 1);
    }

    // ── #256: Consultation dispatch does not advance card from requested ────

    #[test]
    fn consultation_dispatch_stays_in_requested() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-consult", "requested");

        // Create consultation dispatch — should NOT move card from requested
        let result = dispatch::create_dispatch(
            &db,
            &engine,
            "card-consult",
            "agent-1",
            "consultation",
            "[Consultation] Test",
            &serde_json::json!({}),
        );
        assert!(
            result.is_ok(),
            "consultation dispatch creation must succeed"
        );

        let card_status = get_card_status(&db, "card-consult");
        assert_eq!(
            card_status, "requested",
            "#256: consultation dispatch must NOT advance card from requested"
        );
    }

    #[test]
    fn consultation_dispatch_uses_alt_channel() {
        // Verified via unit test in dispatches.rs — this is a smoke test
        assert!(
            crate::server::routes::dispatches::use_counter_model_channel(Some("consultation")),
            "#256: consultation must route to counter-model channel"
        );
    }

    #[test]
    fn requested_preflight_preserves_existing_metadata_keys() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-preflight-meta", "ready");

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET description = ?1, metadata = ?2 WHERE id = ?3",
                libsql_rusqlite::params![
                    "too short",
                    serde_json::json!({
                        "deps": "#42",
                        "triage_label": "needs-spec"
                    })
                    .to_string(),
                    "card-preflight-meta"
                ],
            )
            .unwrap();
        }

        let result = kanban::transition_status(&db, &engine, "card-preflight-meta", "requested");
        assert!(
            result.is_ok(),
            "ready -> requested preflight should succeed"
        );

        let metadata_json: String = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT metadata FROM kanban_cards WHERE id = 'card-preflight-meta'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        };
        let metadata: serde_json::Value = serde_json::from_str(&metadata_json).unwrap();

        assert_eq!(metadata["deps"], "#42");
        assert_eq!(metadata["triage_label"], "needs-spec");
        assert_eq!(metadata["preflight_status"], "consult_required");
        assert!(metadata["preflight_summary"].is_string());
        assert!(metadata["preflight_checked_at"].is_string());
    }

    #[test]
    fn triage_requested_key_uses_ttl_and_requeues_after_expiry() {
        let db = test_db();
        let policy_dir = setup_triage_policy_dir();
        let engine = test_engine_with_dir(&db, policy_dir.path());
        set_config_key(&db, "kanban_manager_channel_id", json!("channel-triage"));

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (
                    id, title, status, metadata, github_issue_url, github_issue_number, created_at, updated_at
                 ) VALUES (
                    'card-triage-ttl', 'Needs classification', 'backlog', ?1,
                    'https://github.com/test/repo/issues/777', 777, datetime('now'), datetime('now')
                 )",
                libsql_rusqlite::params![serde_json::json!({"labels": ""}).to_string()],
            )
            .unwrap();
        }

        engine
            .try_fire_hook_by_name("OnTick", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        let first_expiry: Option<String> = db
            .lock()
            .unwrap()
            .query_row(
                "SELECT expires_at FROM kv_meta WHERE key = 'triage_requested:card-triage-ttl'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            first_expiry.is_some(),
            "#654: triage dedup key must use TTL"
        );
        assert_eq!(message_outbox_rows(&db).len(), 1);

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kv_meta
                 SET expires_at = datetime('now', '-1 minute')
                 WHERE key = 'triage_requested:card-triage-ttl'",
                [],
            )
            .unwrap();
        }

        engine
            .try_fire_hook_by_name("OnTick", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(
            message_outbox_rows(&db).len(),
            2,
            "#654: expired triage dedup key must allow a fresh backlog classification request"
        );
    }

    #[test]
    fn consultation_clear_redispatches_linked_auto_queue_entry() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-consult-clear", "requested");
        ensure_auto_queue_tables(&db);

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE kanban_cards SET metadata = ?1 WHERE id = ?2",
                libsql_rusqlite::params![
                    serde_json::json!({
                        "preflight_status": "consult_required",
                        "deps": "#42"
                    })
                    .to_string(),
                    "card-consult-clear"
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status, created_at) \
                 VALUES ('run-consult-clear', 'repo-1', 'agent-1', 'active', datetime('now'))",
                [],
            )
            .unwrap();
        }

        let consultation = dispatch::create_dispatch(
            &db,
            &engine,
            "card-consult-clear",
            "agent-1",
            "consultation",
            "[Consultation] Clarify",
            &serde_json::json!({}),
        )
        .unwrap();
        let consultation_id = consultation["id"].as_str().unwrap().to_string();

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank, dispatch_id, dispatched_at) \
                 VALUES ('entry-consult-clear', 'run-consult-clear', 'card-consult-clear', 'agent-1', 'dispatched', 1, ?1, datetime('now'))",
                libsql_rusqlite::params![consultation_id],
            )
            .unwrap();
        }

        let completed = dispatch::complete_dispatch(
            &db,
            &engine,
            &consultation_id,
            &serde_json::json!({
                "verdict": "clear",
                "summary": "clarified"
            }),
        )
        .unwrap();
        assert_eq!(completed["status"], "completed");

        let (card_status, latest_dispatch_id, metadata_json): (String, String, String) = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT status, latest_dispatch_id, metadata FROM kanban_cards WHERE id = 'card-consult-clear'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
        };
        let metadata: serde_json::Value = serde_json::from_str(&metadata_json).unwrap();
        assert_eq!(card_status, "in_progress");
        assert_eq!(metadata["consultation_status"], "completed");
        assert_eq!(metadata["consultation_result"]["verdict"], "clear");
        assert_eq!(metadata["preflight_status"], "clear");
        assert_eq!(metadata["deps"], "#42");

        let (dispatch_type, dispatch_status, parent_dispatch_id): (String, String, Option<String>) = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT dispatch_type, status, parent_dispatch_id FROM task_dispatches WHERE id = ?1",
                libsql_rusqlite::params![latest_dispatch_id.clone()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
        };
        assert_eq!(dispatch_type, "implementation");
        assert_eq!(dispatch_status, "pending");
        assert_eq!(
            parent_dispatch_id.as_deref(),
            Some(consultation_id.as_str())
        );

        let (entry_status, entry_dispatch_id): (String, String) = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT status, dispatch_id FROM auto_queue_entries WHERE id = 'entry-consult-clear'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
        };
        assert_eq!(entry_status, "dispatched");
        assert_eq!(entry_dispatch_id, latest_dispatch_id);

        let history: Vec<String> = {
            let conn = db.lock().unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT dispatch_id
                     FROM auto_queue_entry_dispatch_history
                     WHERE entry_id = 'entry-consult-clear'
                     ORDER BY id ASC",
                )
                .unwrap();
            stmt.query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .filter_map(|row| row.ok())
                .collect()
        };
        assert_eq!(history, vec![consultation_id, latest_dispatch_id]);
    }

    #[cfg(unix)]
    #[test]
    fn gemini_streaming_cancel_integration_stops_after_partial_output() {
        use std::sync::Arc;
        use std::sync::mpsc::RecvTimeoutError;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let gemini_path = dir.path().join("gemini");
        write_executable_script(
            &gemini_path,
            r#"#!/bin/sh
set -eu
printf '%s\n' '{"type":"init","session_id":"latest","model":"gemini-2.5-flash"}'
printf '%s\n' '{"type":"message","role":"assistant","content":"partial"}'
while true; do
  printf '%s\n' 'heartbeat'
  sleep 0.05
done
"#,
        );
        let _gemini_override = GeminiStreamingEnvOverride::new(&gemini_path, dir.path());

        let (tx, rx) = std::sync::mpsc::channel();
        let token = Arc::new(crate::services::provider::CancelToken::new());
        let token_for_thread = token.clone();
        let working_dir = dir.path().display().to_string();
        let started_at = Instant::now();
        let handle = std::thread::spawn(move || {
            crate::services::gemini::execute_command_streaming(
                "say hello",
                None,
                &working_dir,
                tx,
                None,
                None,
                Some(token_for_thread),
                None,
                None,
                None,
                None,
                None,
                None,
            )
        });

        match rx.recv_timeout(Duration::from_secs(2)).unwrap() {
            crate::services::agent_protocol::StreamMessage::Init { session_id, .. } => {
                assert_eq!(session_id, "latest");
            }
            other => panic!("expected Init, got {:?}", other),
        }
        match rx.recv_timeout(Duration::from_secs(2)).unwrap() {
            crate::services::agent_protocol::StreamMessage::Text { content } => {
                assert_eq!(content, "partial");
            }
            other => panic!("expected Text, got {:?}", other),
        }

        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let result = handle.join().unwrap();
        assert!(result.is_ok());
        assert!(
            started_at.elapsed() < Duration::from_secs(2),
            "cancellation should complete without waiting for provider shutdown"
        );

        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(crate::services::agent_protocol::StreamMessage::Done { .. }) => {
                    panic!("unexpected terminal Done after cancellation")
                }
                Ok(crate::services::agent_protocol::StreamMessage::Error { message, .. }) => {
                    panic!("unexpected terminal Error after cancellation: {message}")
                }
                Ok(_) => {}
            }
        }
    }
}
