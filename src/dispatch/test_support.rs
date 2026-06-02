use std::sync::MutexGuard;
#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
use std::sync::{Mutex, OnceLock};

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
use crate::db::Db;

pub(crate) struct DispatchPostgresTestDb {
    _lock: crate::db::postgres::PostgresTestLifecycleGuard,
    admin_url: String,
    database_name: String,
    database_url: String,
    label: String,
}

impl DispatchPostgresTestDb {
    pub(crate) async fn create(prefix: &str, label: &str) -> Self {
        let lock = crate::db::postgres::lock_test_lifecycle();
        let admin_url = postgres_admin_database_url();
        let database_name = format!("{}_{}", prefix, uuid::Uuid::new_v4().simple());
        let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
        crate::db::postgres::create_test_database(&admin_url, &database_name, label)
            .await
            .unwrap_or_else(|err| panic!("create {label} postgres test db: {err}"));

        Self {
            _lock: lock,
            admin_url,
            database_name,
            database_url,
            label: label.to_string(),
        }
    }

    pub(crate) async fn connect_and_migrate(&self) -> sqlx::PgPool {
        crate::db::postgres::connect_test_pool_and_migrate(&self.database_url, &self.label)
            .await
            .unwrap_or_else(|err| {
                panic!("connect + migrate {} postgres test db: {err}", self.label)
            })
    }

    pub(crate) async fn connect_and_migrate_with_max_connections(
        &self,
        max_connections: u32,
    ) -> sqlx::PgPool {
        let pool = crate::db::postgres::connect_test_pool_with_max_connections(
            &self.database_url,
            &self.label,
            max_connections,
        )
        .await
        .unwrap_or_else(|err| panic!("connect {} postgres test db: {err}", self.label));
        crate::db::postgres::migrate(&pool)
            .await
            .unwrap_or_else(|err| panic!("migrate {} postgres test db: {err}", self.label));
        pool
    }

    pub(crate) async fn drop(self) {
        crate::db::postgres::drop_test_database(&self.admin_url, &self.database_name, &self.label)
            .await
            .unwrap_or_else(|err| panic!("drop {} postgres test db: {err}", self.label));
    }
}

pub(crate) fn postgres_base_database_url() -> String {
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

pub(crate) fn postgres_admin_database_url() -> String {
    let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "postgres".to_string());
    format!("{}/{}", postgres_base_database_url(), admin_db)
}

pub(crate) async fn seed_pg_dispatch(pool: &sqlx::PgPool, dispatch_id: &str, title: &str) {
    sqlx::query(
        "INSERT INTO task_dispatches (id, status, title, created_at, updated_at)
         VALUES ($1, 'pending', $2, NOW(), NOW())",
    )
    .bind(dispatch_id)
    .bind(title)
    .execute(pool)
    .await
    .unwrap_or_else(|err| panic!("seed postgres dispatch {dispatch_id}: {err}"));
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
fn lock_dispatch_test_env() -> MutexGuard<'static, ()> {
    crate::services::discord::runtime_store::lock_test_env()
}

#[cfg(not(all(test, feature = "legacy-sqlite-tests")))]
fn lock_dispatch_test_env() -> MutexGuard<'static, ()> {
    static TEST_ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("dispatch test env lock poisoned")
}

pub(crate) struct DispatchEnvOverride {
    _lock: MutexGuard<'static, ()>,
    previous_repo_dir: Option<String>,
    previous_config: Option<String>,
}

impl DispatchEnvOverride {
    pub(crate) fn new(repo_dir: Option<&str>, config_path: Option<&str>) -> Self {
        let lock = lock_dispatch_test_env();
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(crate) fn seed_card(db: &Db, card_id: &str, status: &str) {
    let conn = db.separate_conn().unwrap();
    conn.execute(
        "INSERT INTO kanban_cards (id, title, status, created_at, updated_at) VALUES (?1, 'Test Card', ?2, datetime('now'), datetime('now'))",
        sqlite_test::params![card_id, status],
    )
    .unwrap();
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
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
