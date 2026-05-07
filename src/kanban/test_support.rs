use crate::db::Db;
use crate::engine::PolicyEngine;
use std::path::PathBuf;

pub(crate) fn test_db() -> Db {
    let conn = sqlite_test::Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
    crate::db::schema::migrate(&conn).unwrap();
    crate::db::wrap_conn(conn)
}

pub(crate) fn test_engine(db: &Db) -> PolicyEngine {
    let mut config = crate::config::Config::default();
    config.policies.dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
    config.policies.hot_reload = false;
    PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap()
}

pub(crate) fn test_engine_with_dir(db: &Db, dir: &std::path::Path) -> PolicyEngine {
    let mut config = crate::config::Config::default();
    config.policies.dir = dir.to_path_buf();
    config.policies.hot_reload = false;
    PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap()
}

pub(crate) fn test_engine_with_pg(pg_pool: sqlx::PgPool) -> PolicyEngine {
    let mut config = crate::config::Config::default();
    config.policies.dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
    config.policies.hot_reload = false;
    PolicyEngine::new_with_pg(&config, Some(pg_pool)).unwrap()
}

pub(crate) fn test_engine_with_pg_and_dir(
    pg_pool: sqlx::PgPool,
    dir: &std::path::Path,
) -> PolicyEngine {
    let mut config = crate::config::Config::default();
    config.policies.dir = dir.to_path_buf();
    config.policies.hot_reload = false;
    PolicyEngine::new_with_pg(&config, Some(pg_pool)).unwrap()
}

pub(crate) struct KanbanPgDatabase {
    _lock: crate::db::postgres::PostgresTestLifecycleGuard,
    admin_url: String,
    database_name: String,
    database_url: String,
    cleanup_armed: bool,
}

impl KanbanPgDatabase {
    pub(crate) async fn create() -> Self {
        let lock = crate::db::postgres::lock_test_lifecycle();
        let admin_url = postgres_admin_database_url();
        let database_name = format!("agentdesk_kanban_{}", uuid::Uuid::new_v4().simple());
        let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
        crate::db::postgres::create_test_database(&admin_url, &database_name, "kanban tests")
            .await
            .expect("create kanban postgres test db");

        Self {
            _lock: lock,
            admin_url,
            database_name,
            database_url,
            cleanup_armed: true,
        }
    }

    pub(crate) async fn connect_and_migrate(&self) -> sqlx::PgPool {
        crate::db::postgres::connect_test_pool_and_migrate(&self.database_url, "kanban tests")
            .await
            .expect("connect + migrate kanban postgres test db")
    }

    pub(crate) async fn drop(mut self) {
        let drop_result = crate::db::postgres::drop_test_database(
            &self.admin_url,
            &self.database_name,
            "kanban tests",
        )
        .await;
        if drop_result.is_ok() {
            self.cleanup_armed = false;
        }
        drop_result.expect("drop kanban postgres test db");
    }

    pub(crate) async fn close_pool_and_drop(self, pool: sqlx::PgPool) {
        crate::db::postgres::close_test_pool(pool, "kanban tests")
            .await
            .expect("close kanban postgres test pool");
        self.drop().await;
    }
}

impl Drop for KanbanPgDatabase {
    fn drop(&mut self) {
        if !self.cleanup_armed {
            return;
        }
        cleanup_test_postgres_db_from_drop(self.admin_url.clone(), self.database_name.clone());
    }
}

fn cleanup_test_postgres_db_from_drop(admin_url: String, database_name: String) {
    let cleanup_database_name = database_name.clone();
    let thread_name = format!("kanban tests cleanup {cleanup_database_name}");
    let spawn_result = std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("kanban tests cleanup runtime failed for {database_name}: {error}");
                    return;
                }
            };
            if let Err(error) = runtime.block_on(crate::db::postgres::drop_test_database(
                &admin_url,
                &database_name,
                "kanban tests",
            )) {
                eprintln!("kanban tests cleanup failed for {database_name}: {error}");
            }
        });

    match spawn_result {
        Ok(handle) => {
            if handle.join().is_err() {
                eprintln!("kanban tests cleanup thread panicked for {cleanup_database_name}");
            }
        }
        Err(error) => {
            eprintln!(
                "kanban tests cleanup thread spawn failed for {cleanup_database_name}: {error}"
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

pub(crate) struct EnvVarGuard {
    key: &'static str,
    original: Option<String>,
}

impl EnvVarGuard {
    pub(crate) fn set(key: &'static str, value: &str) -> Self {
        let original = std::env::var(key).ok();
        unsafe { std::env::set_var(key, value) };
        Self { key, original }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(value) => unsafe { std::env::set_var(self.key, value) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

#[cfg(unix)]
pub(crate) fn write_executable_script(path: &std::path::Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, contents).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

pub(crate) fn seed_card(db: &Db, card_id: &str, status: &str) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
        [],
    )
    .ok();
    conn.execute(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at)
         VALUES (?1, 'Test Card', ?2, 'agent-1', datetime('now'), datetime('now'))",
        sqlite_test::params![card_id, status],
    )
    .unwrap();
}

pub(crate) fn seed_card_with_repo(db: &Db, card_id: &str, status: &str, repo_id: &str) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
        [],
    )
    .ok();
    conn.execute(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, repo_id, created_at, updated_at)
         VALUES (?1, 'Test Card', ?2, 'agent-1', ?3, datetime('now'), datetime('now'))",
        sqlite_test::params![card_id, status, repo_id],
    )
    .unwrap();
}

pub(crate) fn seed_dispatch(db: &Db, card_id: &str, dispatch_status: &str) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
         VALUES (?1, ?2, 'agent-1', 'implementation', ?3, 'Test Dispatch', datetime('now'), datetime('now'))",
        sqlite_test::params![
            format!("dispatch-{}-{}", card_id, dispatch_status),
            card_id,
            dispatch_status
        ],
    )
    .unwrap();
}

pub(crate) fn seed_dispatch_with_type(
    db: &Db,
    dispatch_id: &str,
    card_id: &str,
    dispatch_type: &str,
    dispatch_status: &str,
) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
         VALUES (?1, ?2, 'agent-1', ?3, ?4, 'Typed Dispatch', datetime('now'), datetime('now'))",
        sqlite_test::params![dispatch_id, card_id, dispatch_type, dispatch_status],
    )
    .unwrap();
}

pub(crate) async fn seed_card_pg(pool: &sqlx::PgPool, card_id: &str, status: &str) {
    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
         VALUES ('agent-1', 'Agent 1', '123', '456')
         ON CONFLICT (id) DO NOTHING",
    )
    .execute(pool)
    .await
    .expect("seed postgres agent");
    sqlx::query(
        "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at)
         VALUES ($1, 'Test Card', $2, 'agent-1', NOW(), NOW())",
    )
    .bind(card_id)
    .bind(status)
    .execute(pool)
    .await
    .expect("seed postgres card");
}

pub(crate) async fn seed_card_with_repo_pg(
    pool: &sqlx::PgPool,
    card_id: &str,
    status: &str,
    repo_id: &str,
) {
    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
         VALUES ('agent-1', 'Agent 1', '123', '456')
         ON CONFLICT (id) DO NOTHING",
    )
    .execute(pool)
    .await
    .expect("seed postgres agent");
    sqlx::query(
        "INSERT INTO kanban_cards (
            id, title, status, assigned_agent_id, repo_id, created_at, updated_at
         )
         VALUES ($1, 'Test Card', $2, 'agent-1', $3, NOW(), NOW())",
    )
    .bind(card_id)
    .bind(status)
    .bind(repo_id)
    .execute(pool)
    .await
    .expect("seed postgres card with repo");
}

pub(crate) async fn seed_dispatch_pg(pool: &sqlx::PgPool, card_id: &str, dispatch_status: &str) {
    seed_dispatch_with_type_pg(
        pool,
        &format!("dispatch-{}-{}", card_id, dispatch_status),
        card_id,
        "implementation",
        dispatch_status,
    )
    .await;
}

pub(crate) async fn seed_dispatch_with_type_pg(
    pool: &sqlx::PgPool,
    dispatch_id: &str,
    card_id: &str,
    dispatch_type: &str,
    dispatch_status: &str,
) {
    sqlx::query(
        "INSERT INTO task_dispatches (
            id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
         )
         VALUES ($1, $2, 'agent-1', $3, $4, 'Typed Dispatch', NOW(), NOW())",
    )
    .bind(dispatch_id)
    .bind(card_id)
    .bind(dispatch_type)
    .bind(dispatch_status)
    .execute(pool)
    .await
    .expect("seed postgres dispatch");
}

pub(crate) fn ensure_auto_queue_tables(db: &Db) {
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
            unified_thread INTEGER DEFAULT 0,
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
        );",
    )
    .unwrap();
}

pub(crate) fn seed_pipeline_stages(db: &Db, repo_id: &str) -> (i64, i64) {
    let conn = db.lock().unwrap();
    conn.execute(
        "INSERT INTO pipeline_stages (repo_id, stage_name, stage_order, trigger_after)
         VALUES (?1, 'Build', 1, 'ready')",
        [repo_id],
    )
    .unwrap();
    let stage1 = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO pipeline_stages (repo_id, stage_name, stage_order, trigger_after)
         VALUES (?1, 'Deploy', 2, 'review_pass')",
        [repo_id],
    )
    .unwrap();
    let stage2 = conn.last_insert_rowid();
    (stage1, stage2)
}

pub(crate) fn seed_auto_queue_run(db: &Db, agent_id: &str) -> (String, String, String) {
    ensure_auto_queue_tables(db);
    let conn = db.lock().unwrap();
    let run_id = "run-1";
    let entry_a = "entry-a";
    let entry_b = "entry-b";
    conn.execute(
        "INSERT INTO auto_queue_runs (id, status, agent_id, created_at) VALUES (?1, 'active', ?2, datetime('now'))",
        sqlite_test::params![run_id, agent_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank)
         VALUES (?1, ?2, 'card-q1', ?3, 'dispatched', 1)",
        sqlite_test::params![entry_a, run_id, agent_id],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, priority_rank)
         VALUES (?1, ?2, 'card-q2', ?3, 'pending', 2)",
        sqlite_test::params![entry_b, run_id, agent_id],
    )
    .unwrap();
    (run_id.to_string(), entry_a.to_string(), entry_b.to_string())
}

pub(crate) async fn seed_auto_queue_run_pg(
    pool: &sqlx::PgPool,
    agent_id: &str,
) -> (String, String, String) {
    let run_id = "run-1";
    let entry_a = "entry-a";
    let entry_b = "entry-b";
    sqlx::query(
        "INSERT INTO auto_queue_runs (id, status, agent_id, created_at)
         VALUES ($1, 'active', $2, NOW())",
    )
    .bind(run_id)
    .bind(agent_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, created_at
         )
         VALUES ($1, $2, 'card-q1', $3, 'dispatched', 1, NOW())",
    )
    .bind(entry_a)
    .bind(run_id)
    .bind(agent_id)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO auto_queue_entries (
            id, run_id, kanban_card_id, agent_id, status, priority_rank, created_at
         )
         VALUES ($1, $2, 'card-q2', $3, 'pending', 2, NOW())",
    )
    .bind(entry_b)
    .bind(run_id)
    .bind(agent_id)
    .execute(pool)
    .await
    .unwrap();
    (run_id.to_string(), entry_a.to_string(), entry_b.to_string())
}
