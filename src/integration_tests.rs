//! #124: Pipeline integration test harness — 6 mandatory scenarios
//!
//! These tests verify pipeline correctness end-to-end before #106 data-driven transition.

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::Mutex;

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
        let conn = rusqlite::Connection::open_in_memory().unwrap();
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

    struct RepoDirOverride {
        _guard: std::sync::MutexGuard<'static, ()>,
        previous: Option<OsString>,
    }

    impl RepoDirOverride {
        fn new(path: &std::path::Path) -> Self {
            let guard = repo_dir_env_lock().lock().unwrap();
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
            let guard = repo_dir_env_lock().lock().unwrap();
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
            let guard = repo_dir_env_lock().lock().unwrap();
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

        let lock = repo_dir_env_lock().lock().unwrap();
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

        let lock = repo_dir_env_lock().lock().unwrap();
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
            rusqlite::params![card_id, status],
        )
        .unwrap();
    }

    fn set_config_key(db: &db::Db, key: &str, value: serde_json::Value) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value.to_string()],
        )
        .unwrap();
    }

    fn seed_dispatch(db: &db::Db, dispatch_id: &str, card_id: &str, dtype: &str, status: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at) \
             VALUES (?1, ?2, 'agent-1', ?3, ?4, 'Test Dispatch', datetime('now'), datetime('now'))",
            rusqlite::params![dispatch_id, card_id, dtype, status],
        )
        .unwrap();
        conn.execute(
            "UPDATE kanban_cards SET latest_dispatch_id = ?1 WHERE id = ?2",
            rusqlite::params![dispatch_id, card_id],
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
            rusqlite::params![
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
            rusqlite::params![
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
            rusqlite::params![
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

    fn seed_completed_review_dispatch_with_context(
        db: &db::Db,
        dispatch_id: &str,
        card_id: &str,
        verdict: &str,
        worktree_path: &str,
        branch: &str,
        commit: &str,
    ) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title,
                result, context, created_at, updated_at, completed_at
            ) VALUES (
                ?1, ?2, 'agent-1', 'review', 'completed', 'Completed review',
                ?3, ?4, datetime('now', '-1 minutes'), datetime('now', '-1 minutes'), datetime('now', '-1 minutes')
            )",
            rusqlite::params![
                dispatch_id,
                card_id,
                serde_json::json!({
                    "verdict": verdict,
                })
                .to_string(),
                serde_json::json!({
                    "completed_worktree_path": worktree_path,
                    "completed_branch": branch,
                    "reviewed_commit": commit,
                    "head_sha": commit,
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
            rusqlite::params![
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
            rusqlite::params![session_key, thread_channel_id],
        )
        .unwrap();
    }

    fn set_kv(db: &db::Db, key: &str, value: &str) {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
            rusqlite::params![key, value],
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
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("policies")
            .join("timeouts.js");
        fs::copy(&source, dir.path().join("timeouts.js")).unwrap();
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
                    return {
                        ok: true,
                        tmux_killed: true
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
            rusqlite::params![card_id, repo_id, worktree_path, branch, pr_number, head_sha, state],
        )
        .unwrap();
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

    fn count_dispatches_by_type(db: &db::Db, card_id: &str, dispatch_type: &str) -> i64 {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM task_dispatches WHERE kanban_card_id = ?1 AND dispatch_type = ?2",
            rusqlite::params![card_id, dispatch_type],
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
            rusqlite::params![card_id, dispatch_type],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn latest_dispatch_title(db: &db::Db, card_id: &str, dispatch_type: &str) -> Option<String> {
        let conn = db.lock().unwrap();
        conn.query_row(
            "SELECT title FROM task_dispatches WHERE kanban_card_id = ?1 AND dispatch_type = ?2 ORDER BY created_at DESC, id DESC LIMIT 1",
            rusqlite::params![card_id, dispatch_type],
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
            let gh_cmd_path = dir.path().join("gh.cmd");
            let gh_ps1_path = dir.path().join("gh.ps1");
            let (wrapper, script) = build_mock_gh_script(replies);
            fs::write(&gh_cmd_path, wrapper).unwrap();
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
                std::env::set_var("AGENTDESK_GH_PATH", &gh_cmd_path);
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
            );",
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
    fn scenario_5_timeout_recovery_stale_to_pending_decision() {
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

        let status = get_card_status(&db, "card-s5");
        assert_eq!(
            status, "pending_decision",
            "stale requested card with exhausted retries → pending_decision"
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
            "requested preflight cards without a dispatch must not be forced to pending_decision"
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
        seed_card(&db, "card-escalation", "pending_decision");
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
                "status": "pending_decision"
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
    fn entering_force_only_state_preserves_pending_escalation_bundle() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-force-enter", "requested");
        set_kv(
            &db,
            "pm_pending:card-force-enter",
            r#"{"title":"Test Card","reasons":["new pending decision"]}"#,
        );
        set_kv(
            &db,
            "pm_decision_sent:card-force-enter",
            r#"{"sent_at":123,"status":"requested"}"#,
        );

        kanban::transition_status_with_opts(
            &db,
            &engine,
            "card-force-enter",
            "pending_decision",
            "test",
            true,
        )
        .unwrap();

        assert!(kv_value(&db, "pm_pending:card-force-enter").is_some());
        assert!(kv_value(&db, "pm_decision_sent:card-force-enter").is_some());
    }

    #[test]
    fn leaving_force_only_state_clears_escalation_cooldown_keys() {
        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_card(&db, "card-force-leave", "pending_decision");
        set_kv(
            &db,
            "pm_pending:card-force-leave",
            r#"{"title":"Test Card","reasons":["awaiting PM"]}"#,
        );
        set_kv(
            &db,
            "pm_decision_sent:card-force-leave",
            r#"{"sent_at":123,"status":"pending_decision"}"#,
        );

        kanban::transition_status_with_opts(
            &db,
            &engine,
            "card-force-leave",
            "requested",
            "test",
            true,
        )
        .unwrap();

        assert!(
            kv_value(&db, "pm_pending:card-force-leave").is_none(),
            "resolving a force-only state must clear pending escalation bundle"
        );
        assert!(
            kv_value(&db, "pm_decision_sent:card-force-leave").is_none(),
            "resolving a force-only state must clear resend cooldown"
        );
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
    async fn scenario_6c_review_verdict_pass_closes_issue_and_completes_auto_queue_run() {
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
        drop(conn);

        assert_eq!(card_status, "done");
        assert_eq!(
            entry_status, "done",
            "pass verdict terminal transition must close the active auto-queue entry"
        );
        assert_eq!(
            run_status, "completed",
            "pass verdict terminal transition must still fire OnCardTerminal auto-queue completion"
        );

        let log = gh_log(&gh);
        assert!(
            log.contains("issue close 483 --repo test/repo"),
            "pass verdict path must close the linked GitHub issue"
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
                {"id": "pending_decision", "label": "Pending"},
                {"id": "done", "label": "Done", "terminal": true}
            ],
            "transitions": [
                {"from": "backlog", "to": "ready", "type": "free"},
                {"from": "ready", "to": "requested", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "requested", "to": "in_progress", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "in_progress", "to": "review", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "review", "to": "qa_test", "type": "gated", "gates": ["review_passed"]},
                {"from": "review", "to": "in_progress", "type": "gated", "gates": ["review_rework"]},
                {"from": "qa_test", "to": "done", "type": "gated", "gates": ["active_dispatch"]},
                {"from": "qa_test", "to": "in_progress", "type": "force_only"},
                {"from": "requested", "to": "pending_decision", "type": "force_only"},
                {"from": "pending_decision", "to": "done", "type": "force_only"}
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

        // qa_test → in_progress force transition
        let qa_rework = effective.find_transition("qa_test", "in_progress");
        assert!(
            qa_rework.is_some(),
            "qa_test → in_progress (force) must exist"
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
        let result = crate::engine::intent::execute_intents(&db, vec![insert_intent]);
        assert_eq!(
            result.errors, 1,
            "INSERT into card_review_state via ExecuteSQL must be rejected"
        );

        // Attempt INSERT OR REPLACE via ExecuteSQL intent — must also fail
        let replace_intent = crate::engine::intent::Intent::ExecuteSQL {
            sql: "INSERT OR REPLACE INTO card_review_state (card_id, state, updated_at) VALUES ('card-158b', 'idle', datetime('now'))".to_string(),
            params: vec![],
        };
        let result_replace = crate::engine::intent::execute_intents(&db, vec![replace_intent]);
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
            crate::engine::intent::execute_intents(&db, vec![replace_into_intent]);
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
        let result2 = crate::engine::intent::execute_intents(&db, vec![update_intent]);
        assert_eq!(
            result2.errors, 1,
            "UPDATE card_review_state via ExecuteSQL must be rejected"
        );

        // Attempt DELETE via ExecuteSQL intent — must also fail
        let delete_intent = crate::engine::intent::Intent::ExecuteSQL {
            sql: "DELETE FROM card_review_state WHERE card_id = 'card-158b'".to_string(),
            params: vec![],
        };
        let result3 = crate::engine::intent::execute_intents(&db, vec![delete_intent]);
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
    fn scenario_review_disabled_on_review_enter_closes_issue_and_completes_auto_queue_run() {
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
        assert_eq!(
            run_status, "completed",
            "review-disabled JS terminal path must still fire OnCardTerminal auto-queue completion"
        );

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
        let phase_gate_state: String = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'aq_phase_gate:run-review-disabled-phase:1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        drop(conn);

        let phase_gate_json: serde_json::Value = serde_json::from_str(&phase_gate_state).unwrap();
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
    fn scenario_332_implementation_noop_completion_returns_card_to_ready_and_closes_auto_queue() {
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
            "ready",
            "#332: explicit noop outcome must return implementation card to ready"
        );

        let conn = db.lock().unwrap();
        let review_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task_dispatches \
                 WHERE kanban_card_id = 'card-332' AND dispatch_type = 'review' \
                 AND status IN ('pending', 'dispatched')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            review_count, 0,
            "#332: noop completion must not create a follow-up review dispatch"
        );

        let auto_queue_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_entries WHERE id = 'entry-332'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            auto_queue_status, "done",
            "#332: noop completion must close the active auto-queue entry"
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

    #[test]
    fn scenario_558_review_pass_falls_back_to_review_context_target() {
        let (repo, _repo_guard) = setup_test_repo();
        run_git(
            repo.path(),
            &["checkout", "-b", "wt/card-558-review-fallback"],
        );
        let head = run_git_output(repo.path(), &["rev-parse", "HEAD"]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(
            &db,
            "card-558-review-fallback",
            "review",
            "test/repo",
            558,
            Some("123456789012345679"),
        );
        seed_completed_review_dispatch_with_context(
            &db,
            "review-558-pass",
            "card-558-review-fallback",
            "pass",
            repo.path().to_str().unwrap(),
            "wt/card-558-review-fallback",
            &head,
        );

        engine
            .try_fire_hook_by_name(
                "OnReviewVerdict",
                serde_json::json!({"card_id": "card-558-review-fallback", "verdict": "pass"}),
            )
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(
            count_active_dispatches_by_type(&db, "card-558-review-fallback", "create-pr"),
            1,
            "#558: review pass should still seed create-pr from review context"
        );
        assert_eq!(
            pr_tracking_state(&db, "card-558-review-fallback").as_deref(),
            Some("create-pr")
        );
        assert_eq!(
            pr_tracking_branch(&db, "card-558-review-fallback").as_deref(),
            Some("wt/card-558-review-fallback")
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
        seed_pr_tracking(
            &db,
            "card-211-create",
            "test/repo",
            None,
            "wt/card-211-create",
            None,
            Some("oldsha"),
            "create-pr",
        );
        seed_dispatch(
            &db,
            "create-pr-211",
            "card-211-create",
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
    fn scenario_211_terminal_direct_merge_merges_branch_without_pr() {
        let (repo, _remote, _repo_guard) = setup_test_repo_with_origin();
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
            merged.success(),
            "feature commit must be reachable from origin/main after direct merge"
        );
        assert_eq!(get_card_status(&db, "card-211-direct"), "done");
        assert_eq!(
            pr_tracking_state(&db, "card-211-direct").as_deref(),
            Some("closed")
        );
        assert_eq!(pr_tracking_pr_number(&db, "card-211-direct"), None);

        let conn = db.lock().unwrap();
        let blocked_reason: Option<String> = conn
            .query_row(
                "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-211-direct'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(blocked_reason, None);
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
    fn scenario_558_missing_merge_source_does_not_create_conflict_pr() {
        let (repo, _remote, repo_env) = setup_test_repo_with_origin_and_mock_gh(&[]);
        let gh = &repo_env._gh;
        let worktrees_dir = repo.path().join("worktrees");
        fs::create_dir_all(&worktrees_dir).unwrap();
        run_git(repo.path(), &["branch", "wt/card-558-missing"]);

        let worktree_path = worktrees_dir.join("card-558-missing");
        run_git(
            repo.path(),
            &[
                "worktree",
                "add",
                worktree_path.to_str().unwrap(),
                "wt/card-558-missing",
            ],
        );
        fs::write(worktree_path.join("feature.txt"), "feature\n").unwrap();
        run_git(worktree_path.as_path(), &["add", "feature.txt"]);
        run_git(
            worktree_path.as_path(),
            &["commit", "-m", "feat: missing merge source #558"],
        );
        let feature_commit = run_git_output(worktree_path.as_path(), &["rev-parse", "HEAD"]);
        run_git(
            repo.path(),
            &[
                "worktree",
                "remove",
                worktree_path.to_str().unwrap(),
                "--force",
            ],
        );
        run_git(repo.path(), &["branch", "-D", "wt/card-558-missing"]);

        let db = test_db();
        let engine = test_engine(&db);
        seed_agent(&db);
        seed_repo(&db, "test/repo");
        seed_card_with_repo(&db, "card-558-missing", "done", "test/repo", 559, None);
        set_kv(&db, "merge_automation_enabled", "true");
        seed_completed_work_dispatch_target(
            &db,
            "impl-558-missing",
            "card-558-missing",
            "implementation",
            worktree_path.to_str().unwrap(),
            "wt/card-558-missing",
            &feature_commit,
        );

        engine
            .try_fire_hook_by_name(
                "OnCardTerminal",
                serde_json::json!({"card_id": "card-558-missing"}),
            )
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(
            pr_tracking_state(&db, "card-558-missing").as_deref(),
            Some("source-missing")
        );
        assert_eq!(pr_tracking_pr_number(&db, "card-558-missing"), None);

        let conn = db.lock().unwrap();
        let blocked_reason: Option<String> = conn
            .query_row(
                "SELECT blocked_reason FROM kanban_cards WHERE id = 'card-558-missing'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(blocked_reason.as_deref(), Some("merge:source_missing"));
        drop(conn);

        let log = gh_log(gh);
        assert!(
            !log.contains("pr create "),
            "#558: missing merge source must not fall back to PR creation"
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
    fn scenario_208_on_tick_creates_codex_rework_and_dedups_review() {
        let (repo, _env) = setup_test_repo_with_mock_gh(&[
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

        assert_eq!(get_card_status(&db, "card-208"), "in_progress");
        assert_eq!(count_dispatches_by_type(&db, "card-208", "rework"), 1);
        let title = latest_dispatch_title(&db, "card-208", "rework").unwrap();
        assert!(title.contains("src/server/routes/github.rs:77"));
        assert!(title.contains("P1 force-transition leaves dispatch alive"));
        assert_eq!(
            review_state_value(&db, "card-208").as_deref(),
            Some("rework_pending")
        );

        let messages = message_outbox_rows(&db);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].0, "thread-208");
        assert!(messages[0].1.contains("PR #323"));
        assert!(messages[0].1.contains("rework dispatch"));

        engine
            .try_fire_hook_by_name("OnTick5min", serde_json::json!({}))
            .unwrap();
        kanban::drain_hook_side_effects(&db, &engine);

        assert_eq!(count_dispatches_by_type(&db, "card-208", "rework"), 1);
        assert_eq!(message_outbox_rows(&db).len(), 1);
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
                rusqlite::params![
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
                rusqlite::params![
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
                rusqlite::params![consultation_id],
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
                rusqlite::params![latest_dispatch_id.clone()],
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
        let _gemini_override = GeminiPathOverride::new(&gemini_path);

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
            crate::services::agent_protocol::StreamMessage::Init { session_id } => {
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
