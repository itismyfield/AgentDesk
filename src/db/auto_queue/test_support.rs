use sqlx::PgPool;

pub(crate) struct TestPostgresDb {
    _lock: crate::db::postgres::PostgresTestLifecycleGuard,
    admin_url: String,
    database_name: String,
    pub(crate) database_url: String,
    cleanup_armed: bool,
}

impl TestPostgresDb {
    pub(crate) async fn create() -> Self {
        let lock = crate::db::postgres::lock_test_lifecycle();
        let admin_url = postgres_admin_database_url();
        let database_name = format!("agentdesk_db_auto_queue_{}", uuid::Uuid::new_v4().simple());
        let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
        crate::db::postgres::create_test_database(
            &admin_url,
            &database_name,
            "db::auto_queue tests",
        )
        .await
        .expect("create postgres auto_queue test db");

        Self {
            _lock: lock,
            admin_url,
            database_name,
            database_url,
            cleanup_armed: true,
        }
    }

    pub(crate) async fn connect_and_migrate(&self) -> PgPool {
        crate::db::postgres::connect_test_pool_and_migrate(
            &self.database_url,
            "db::auto_queue tests",
        )
        .await
        .expect("connect + migrate postgres auto_queue test db")
    }

    pub(crate) async fn drop(mut self) {
        let drop_result = crate::db::postgres::drop_test_database(
            &self.admin_url,
            &self.database_name,
            "db::auto_queue tests",
        )
        .await;
        if drop_result.is_ok() {
            self.cleanup_armed = false;
        }
        drop_result.expect("drop postgres auto_queue test db");
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
    let thread_name = format!("db::auto_queue tests cleanup {cleanup_database_name}");
    let spawn_result = std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!(
                        "db::auto_queue tests cleanup runtime failed for {database_name}: {error}"
                    );
                    return;
                }
            };
            if let Err(error) = runtime.block_on(crate::db::postgres::drop_test_database(
                &admin_url,
                &database_name,
                "db::auto_queue tests",
            )) {
                eprintln!("db::auto_queue tests cleanup failed for {database_name}: {error}");
            }
        });

    match spawn_result {
        Ok(handle) => {
            if handle.join().is_err() {
                eprintln!(
                    "db::auto_queue tests cleanup thread panicked for {cleanup_database_name}"
                );
            }
        }
        Err(error) => {
            eprintln!(
                "db::auto_queue tests cleanup thread spawn failed for {cleanup_database_name}: {error}"
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
