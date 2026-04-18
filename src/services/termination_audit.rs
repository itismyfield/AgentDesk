use std::sync::{Mutex, OnceLock};

#[derive(Clone, Default)]
struct AuditRuntime {
    db: Option<crate::db::Db>,
    pg_pool: Option<sqlx::PgPool>,
}

#[derive(Clone)]
struct TerminationAuditRecord {
    session_key: String,
    dispatch_id: Option<String>,
    killer_component: String,
    reason_code: String,
    reason_text: Option<String>,
    probe_snapshot: Option<String>,
    last_offset: Option<i64>,
    tmux_alive: Option<bool>,
}

static AUDIT_RUNTIME: OnceLock<Mutex<AuditRuntime>> = OnceLock::new();

fn audit_runtime_slot() -> &'static Mutex<AuditRuntime> {
    AUDIT_RUNTIME.get_or_init(|| Mutex::new(AuditRuntime::default()))
}

fn build_record(
    session_key: &str,
    dispatch_id: Option<&str>,
    killer_component: &str,
    reason_code: &str,
    reason_text: Option<&str>,
    probe_snapshot: Option<&str>,
    last_offset: Option<u64>,
    tmux_alive: Option<bool>,
) -> TerminationAuditRecord {
    TerminationAuditRecord {
        session_key: session_key.to_string(),
        dispatch_id: dispatch_id.map(str::to_string),
        killer_component: killer_component.to_string(),
        reason_code: reason_code.to_string(),
        reason_text: reason_text.map(str::to_string),
        probe_snapshot: probe_snapshot.map(str::to_string),
        last_offset: last_offset.map(|value| value as i64),
        tmux_alive,
    }
}

/// Initialize the audit DB handle. Call during startup and after PG is available.
pub fn init_audit_db(db: crate::db::Db, pg_pool: Option<sqlx::PgPool>) {
    let Ok(mut runtime) = audit_runtime_slot().lock() else {
        return;
    };
    runtime.db = Some(db);
    if let Some(pool) = pg_pool {
        runtime.pg_pool = Some(pool);
    }
}

/// Record a session termination event. Fire-and-forget -- never blocks the kill path.
pub fn record_termination(
    session_key: &str,
    dispatch_id: Option<&str>,
    killer_component: &str,
    reason_code: &str,
    reason_text: Option<&str>,
    probe_snapshot: Option<&str>,
    last_offset: Option<u64>,
    tmux_alive: Option<bool>,
) {
    let Ok(runtime) = audit_runtime_slot().lock() else {
        return;
    };
    let Some(db) = runtime.db.clone() else { return };
    let record = build_record(
        session_key,
        dispatch_id,
        killer_component,
        reason_code,
        reason_text,
        probe_snapshot,
        last_offset,
        tmux_alive,
    );
    persist_record(db, runtime.pg_pool.clone(), record);
}

/// Record a session termination event against an explicit DB handle.
pub fn record_termination_with_db(
    db: &crate::db::Db,
    session_key: &str,
    dispatch_id: Option<&str>,
    killer_component: &str,
    reason_code: &str,
    reason_text: Option<&str>,
    probe_snapshot: Option<&str>,
    last_offset: Option<u64>,
    tmux_alive: Option<bool>,
) {
    let record = build_record(
        session_key,
        dispatch_id,
        killer_component,
        reason_code,
        reason_text,
        probe_snapshot,
        last_offset,
        tmux_alive,
    );
    insert_record_sqlite(db, &record);
}

/// Record against explicit handles; prefers PostgreSQL when available and falls back to SQLite.
pub fn record_termination_with_handles(
    db: &crate::db::Db,
    pg_pool: Option<&sqlx::PgPool>,
    session_key: &str,
    dispatch_id: Option<&str>,
    killer_component: &str,
    reason_code: &str,
    reason_text: Option<&str>,
    probe_snapshot: Option<&str>,
    last_offset: Option<u64>,
    tmux_alive: Option<bool>,
) {
    let record = build_record(
        session_key,
        dispatch_id,
        killer_component,
        reason_code,
        reason_text,
        probe_snapshot,
        last_offset,
        tmux_alive,
    );
    persist_record(db.clone(), pg_pool.cloned(), record);
}

fn persist_record(
    db: crate::db::Db,
    pg_pool: Option<sqlx::PgPool>,
    record: TerminationAuditRecord,
) {
    let Some(pool) = pg_pool else {
        insert_record_sqlite(&db, &record);
        return;
    };

    let db_for_task = db.clone();
    let record_for_task = record.clone();
    let write_task = async move {
        if let Err(error) = insert_record_pg(&pool, &record_for_task).await {
            tracing::warn!("  [termination_audit] postgres insert failed: {error}");
            insert_record_sqlite(&db_for_task, &record_for_task);
        }
    };

    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(write_task);
        return;
    }

    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        match runtime {
            Ok(runtime) => runtime.block_on(write_task),
            Err(error) => {
                tracing::warn!("  [termination_audit] runtime bootstrap failed: {error}");
                insert_record_sqlite(&db, &record);
            }
        }
    });
}

fn insert_record_sqlite(db: &crate::db::Db, record: &TerminationAuditRecord) {
    let conn = match db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("  [termination_audit] failed to open separate conn: {e}");
            return;
        }
    };
    if let Err(e) = conn.execute(
        "INSERT INTO session_termination_events \
         (session_key, dispatch_id, killer_component, reason_code, reason_text, probe_snapshot, last_offset, tmux_alive) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        libsql_rusqlite::params![
            record.session_key,
            record.dispatch_id,
            record.killer_component,
            record.reason_code,
            record.reason_text,
            record.probe_snapshot,
            record.last_offset,
            record.tmux_alive.map(|value| i32::from(value)),
        ],
    ) {
        tracing::warn!("  [termination_audit] insert failed: {e}");
    }
}

async fn insert_record_pg(
    pool: &sqlx::PgPool,
    record: &TerminationAuditRecord,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO session_termination_events
         (session_key, dispatch_id, killer_component, reason_code, reason_text, probe_snapshot, last_offset, tmux_alive)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(&record.session_key)
    .bind(&record.dispatch_id)
    .bind(&record.killer_component)
    .bind(&record.reason_code)
    .bind(&record.reason_text)
    .bind(&record.probe_snapshot)
    .bind(record.last_offset)
    .bind(record.tmux_alive.map(i32::from))
    .execute(pool)
    .await
    .map_err(|error| format!("{error}"))?;
    Ok(())
}

/// Convenience: derive session_key from tmux name, then record.
pub fn record_termination_for_tmux(
    tmux_session_name: &str,
    dispatch_id: Option<&str>,
    killer_component: &str,
    reason_code: &str,
    reason_text: Option<&str>,
    last_offset: Option<u64>,
) {
    let hostname = crate::services::platform::hostname_short();
    let session_key = format!("{}:{}", hostname, tmux_session_name);
    let tmux_alive =
        crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux_session_name);
    let probe_snapshot = if tmux_alive {
        crate::services::platform::tmux::capture_pane(tmux_session_name, -30)
    } else {
        None
    };
    record_termination(
        &session_key,
        dispatch_id,
        killer_component,
        reason_code,
        reason_text,
        probe_snapshot.as_deref(),
        last_offset,
        Some(tmux_alive),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestPostgresDb {
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let admin_url = postgres_admin_database_url();
            let database_name = format!("agentdesk_term_audit_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            let admin_pool = sqlx::PgPool::connect(&admin_url).await.unwrap();
            sqlx::query(&format!("CREATE DATABASE \"{database_name}\""))
                .execute(&admin_pool)
                .await
                .unwrap();
            admin_pool.close().await;

            Self {
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn connect_and_migrate(&self) -> sqlx::PgPool {
            let pool = sqlx::PgPool::connect(&self.database_url).await.unwrap();
            crate::db::postgres::migrate(&pool).await.unwrap();
            pool
        }

        async fn drop(self) {
            let admin_pool = sqlx::PgPool::connect(&self.admin_url).await.unwrap();
            sqlx::query(
                "SELECT pg_terminate_backend(pid)
                 FROM pg_stat_activity
                 WHERE datname = $1
                   AND pid <> pg_backend_pid()",
            )
            .bind(&self.database_name)
            .execute(&admin_pool)
            .await
            .unwrap();
            sqlx::query(&format!(
                "DROP DATABASE IF EXISTS \"{}\"",
                self.database_name
            ))
            .execute(&admin_pool)
            .await
            .unwrap();
            admin_pool.close().await;
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

    #[tokio::test]
    async fn record_termination_with_handles_persists_to_postgres_when_available() {
        let sqlite = crate::db::test_db();
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        record_termination_with_handles(
            &sqlite,
            Some(&pool),
            "host:pg-audit",
            Some("dispatch-1"),
            "cleanup",
            "idle_session_expiry",
            Some("expired"),
            None,
            Some(42),
            Some(false),
        );

        let mut persisted = false;
        for _ in 0..40 {
            let count = sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*)::BIGINT FROM session_termination_events WHERE session_key = $1",
            )
            .bind("host:pg-audit")
            .fetch_one(&pool)
            .await
            .unwrap();
            if count == 1 {
                persisted = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        assert!(
            persisted,
            "postgres termination audit row was not persisted"
        );

        pool.close().await;
        pg_db.drop().await;
    }
}
