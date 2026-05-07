use crate::db::Db;
use crate::engine::PolicyEngine;
use crate::services::git::GitCommand;
use std::sync::MutexGuard;

pub(crate) struct DispatchEnvOverride {
    _lock: MutexGuard<'static, ()>,
    previous_repo_dir: Option<String>,
    previous_config: Option<String>,
}

impl DispatchEnvOverride {
    pub(crate) fn new(repo_dir: Option<&str>, config_path: Option<&str>) -> Self {
        let lock = crate::services::discord::runtime_store::lock_test_env();
        let previous_repo_dir = std::env::var("AGENTDESK_REPO_DIR").ok();
        let previous_config = std::env::var("AGENTDESK_CONFIG").ok();

        match repo_dir {
            Some(path) => unsafe { std::env::set_var("AGENTDESK_REPO_DIR", path) },
            None => unsafe { std::env::remove_var("AGENTDESK_REPO_DIR") },
        }
        match config_path {
            Some(path) => unsafe { std::env::set_var("AGENTDESK_CONFIG", path) },
            None => unsafe { std::env::remove_var("AGENTDESK_CONFIG") },
        }

        Self {
            _lock: lock,
            previous_repo_dir,
            previous_config,
        }
    }
}

impl Drop for DispatchEnvOverride {
    fn drop(&mut self) {
        if let Some(value) = self.previous_repo_dir.as_deref() {
            unsafe { std::env::set_var("AGENTDESK_REPO_DIR", value) };
        } else {
            unsafe { std::env::remove_var("AGENTDESK_REPO_DIR") };
        }

        if let Some(value) = self.previous_config.as_deref() {
            unsafe { std::env::set_var("AGENTDESK_CONFIG", value) };
        } else {
            unsafe { std::env::remove_var("AGENTDESK_CONFIG") };
        }
    }
}

pub(crate) struct RepoDirOverride {
    _lock: MutexGuard<'static, ()>,
    previous: Option<String>,
}

impl RepoDirOverride {
    pub(crate) fn new(path: &str) -> Self {
        let lock = crate::services::discord::runtime_store::lock_test_env();
        let previous = std::env::var("AGENTDESK_REPO_DIR").ok();
        unsafe { std::env::set_var("AGENTDESK_REPO_DIR", path) };
        Self {
            _lock: lock,
            previous,
        }
    }
}

impl Drop for RepoDirOverride {
    fn drop(&mut self) {
        if let Some(value) = self.previous.as_deref() {
            unsafe { std::env::set_var("AGENTDESK_REPO_DIR", value) };
        } else {
            unsafe { std::env::remove_var("AGENTDESK_REPO_DIR") };
        }
    }
}

pub(crate) fn test_db() -> Db {
    let conn = sqlite_test::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    crate::db::schema::migrate(&conn).unwrap();
    let db = crate::db::wrap_conn(conn);
    // Seed common test agents with valid primary/alternate channels so the
    // canonical dispatch target validation can run in unit tests.
    {
        let c = db.separate_conn().unwrap();
        c.execute_batch(
            "INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '111', '222');
             INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-2', 'Agent 2', '333', '444');"
        ).unwrap();
    }
    db
}

pub(crate) fn test_engine(db: &Db) -> PolicyEngine {
    let config = crate::config::Config::default();
    PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap()
}

pub(crate) fn run_git(repo_dir: &str, args: &[&str]) -> std::process::Output {
    GitCommand::new()
        .repo(repo_dir)
        .args(args)
        .run_output()
        .unwrap_or_else(|err| panic!("git {args:?} failed: {err}"))
}

pub(crate) fn init_test_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    let repo_dir = repo.path().to_str().unwrap();

    run_git(repo_dir, &["init", "-b", "main"]);
    run_git(repo_dir, &["config", "user.email", "test@test.com"]);
    run_git(repo_dir, &["config", "user.name", "Test"]);
    run_git(repo_dir, &["commit", "--allow-empty", "-m", "initial"]);

    repo
}

pub(crate) fn setup_test_repo() -> (tempfile::TempDir, RepoDirOverride) {
    let repo = init_test_repo();
    let repo_dir = repo.path().to_str().unwrap();
    let override_guard = RepoDirOverride::new(repo_dir);
    (repo, override_guard)
}

pub(crate) fn setup_test_repo_with_origin()
-> (tempfile::TempDir, tempfile::TempDir, RepoDirOverride) {
    let origin = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let origin_dir = origin.path().to_str().unwrap();
    let repo_dir = repo.path().to_str().unwrap();

    run_git(origin_dir, &["init", "--bare", "--initial-branch=main"]);
    run_git(repo_dir, &["init", "-b", "main"]);
    run_git(repo_dir, &["config", "user.email", "test@test.com"]);
    run_git(repo_dir, &["config", "user.name", "Test"]);
    run_git(repo_dir, &["remote", "add", "origin", origin_dir]);
    run_git(repo_dir, &["commit", "--allow-empty", "-m", "initial"]);
    run_git(repo_dir, &["push", "-u", "origin", "main"]);

    let override_guard = RepoDirOverride::new(repo_dir);
    (repo, origin, override_guard)
}

pub(crate) fn git_commit(repo_dir: &str, message: &str) -> String {
    run_git(repo_dir, &["commit", "--allow-empty", "-m", message]);
    crate::services::platform::git_head_commit(repo_dir).unwrap()
}

pub(crate) fn seed_card(db: &Db, card_id: &str, status: &str) {
    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO kanban_cards (id, title, status, created_at, updated_at) VALUES (?1, 'Test Card', ?2, datetime('now'), datetime('now'))",
        sqlite_test::params![card_id, status],
    )
    .unwrap();
}

pub(crate) fn set_card_issue_number(db: &Db, card_id: &str, issue_number: i64) {
    let conn = db.separate_conn().unwrap();
    conn.execute(
        "UPDATE kanban_cards SET github_issue_number = ?1 WHERE id = ?2",
        sqlite_test::params![issue_number, card_id],
    )
    .unwrap();
}

pub(crate) fn set_card_repo_id(db: &Db, card_id: &str, repo_id: &str) {
    let conn = db.separate_conn().unwrap();
    conn.execute(
        "UPDATE kanban_cards SET repo_id = ?1 WHERE id = ?2",
        sqlite_test::params![repo_id, card_id],
    )
    .unwrap();
}

pub(crate) fn set_card_description(db: &Db, card_id: &str, description: &str) {
    let conn = db.separate_conn().unwrap();
    conn.execute(
        "UPDATE kanban_cards SET description = ?1 WHERE id = ?2",
        sqlite_test::params![description, card_id],
    )
    .unwrap();
}

pub(crate) fn write_repo_mapping_config(entries: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    for (repo_id, repo_dir) in entries {
        config
            .github
            .repo_dirs
            .insert((*repo_id).to_string(), (*repo_dir).to_string());
    }
    crate::config::save_to_path(&dir.path().join("agentdesk.yaml"), &config).unwrap();
    dir
}

pub(crate) fn count_notify_outbox(conn: &sqlite_test::Connection, dispatch_id: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM dispatch_outbox WHERE dispatch_id = ?1 AND action = 'notify'",
        [dispatch_id],
        |row| row.get(0),
    )
    .unwrap()
}

pub(crate) fn count_status_reaction_outbox(
    conn: &sqlite_test::Connection,
    dispatch_id: &str,
) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM dispatch_outbox WHERE dispatch_id = ?1 AND action = 'status_reaction'",
        [dispatch_id],
        |row| row.get(0),
    )
    .unwrap()
}

pub(crate) fn load_dispatch_events(
    conn: &sqlite_test::Connection,
    dispatch_id: &str,
) -> Vec<(Option<String>, String, String)> {
    let mut stmt = conn
        .prepare(
            "SELECT from_status, to_status, transition_source
             FROM dispatch_events
             WHERE dispatch_id = ?1
             ORDER BY id ASC",
        )
        .unwrap();
    stmt.query_map([dispatch_id], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })
    .unwrap()
    .filter_map(|row| row.ok())
    .collect()
}

pub(crate) fn seed_assistant_response_for_dispatch(db: &Db, dispatch_id: &str, message: &str) {
    crate::db::session_transcripts::persist_turn(
        db,
        crate::db::session_transcripts::PersistSessionTranscript {
            turn_id: &format!("dispatch-test:{dispatch_id}"),
            session_key: Some("dispatch-test-session"),
            channel_id: Some("123"),
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
