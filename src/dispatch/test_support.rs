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
