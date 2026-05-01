use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Row};

const DEFAULT_RESOURCE_LOCK_TTL_SECS: i64 = 15 * 60;

#[derive(Debug, Clone, Serialize)]
pub struct ResourceLock {
    pub lock_key: String,
    pub holder_instance_id: String,
    pub holder_job_id: String,
    pub metadata: Value,
    pub expires_at: DateTime<Utc>,
    pub heartbeat_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResourceLockAcquireOutcome {
    pub acquired: bool,
    pub lock: Option<ResourceLock>,
    pub current: Option<ResourceLock>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ResourceLockRequest {
    pub lock_key: String,
    pub holder_instance_id: String,
    pub holder_job_id: String,
    #[serde(default)]
    pub ttl_secs: Option<i64>,
    #[serde(default)]
    pub metadata: Option<Value>,
}

pub fn default_resource_lock_ttl_secs() -> i64 {
    DEFAULT_RESOURCE_LOCK_TTL_SECS
}

pub fn unreal_project_lock_key(repo: &str) -> String {
    format!("unreal:project:{}", repo.trim())
}

pub async fn acquire_resource_lock(
    pool: &PgPool,
    request: &ResourceLockRequest,
) -> Result<ResourceLockAcquireOutcome, String> {
    validate_request(request)?;
    let ttl_secs = normalized_ttl_secs(request.ttl_secs);
    let metadata = request.metadata.clone().unwrap_or_else(|| json!({}));

    let row = sqlx::query(
        r#"
        INSERT INTO resource_locks (
            lock_key, holder_instance_id, holder_job_id, metadata, expires_at,
            heartbeat_at, created_at, updated_at
        )
        VALUES ($1, $2, $3, $4, NOW() + ($5 * INTERVAL '1 second'), NOW(), NOW(), NOW())
        ON CONFLICT (lock_key) DO UPDATE
        SET holder_instance_id = EXCLUDED.holder_instance_id,
            holder_job_id = EXCLUDED.holder_job_id,
            metadata = EXCLUDED.metadata,
            expires_at = EXCLUDED.expires_at,
            heartbeat_at = NOW(),
            updated_at = NOW()
        WHERE resource_locks.expires_at <= NOW()
           OR (
                resource_locks.holder_instance_id = EXCLUDED.holder_instance_id
            AND resource_locks.holder_job_id = EXCLUDED.holder_job_id
           )
        RETURNING lock_key, holder_instance_id, holder_job_id, metadata,
                  expires_at, heartbeat_at, created_at, updated_at
        "#,
    )
    .bind(request.lock_key.trim())
    .bind(request.holder_instance_id.trim())
    .bind(request.holder_job_id.trim())
    .bind(metadata)
    .bind(ttl_secs)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("acquire resource lock {}: {error}", request.lock_key))?;

    if let Some(row) = row {
        return Ok(ResourceLockAcquireOutcome {
            acquired: true,
            lock: Some(lock_from_row(row)?),
            current: None,
        });
    }

    Ok(ResourceLockAcquireOutcome {
        acquired: false,
        lock: None,
        current: get_resource_lock(pool, request.lock_key.trim()).await?,
    })
}

pub async fn heartbeat_resource_lock(
    pool: &PgPool,
    request: &ResourceLockRequest,
) -> Result<Option<ResourceLock>, String> {
    validate_request(request)?;
    let ttl_secs = normalized_ttl_secs(request.ttl_secs);
    let row = sqlx::query(
        r#"
        UPDATE resource_locks
           SET expires_at = NOW() + ($4 * INTERVAL '1 second'),
               heartbeat_at = NOW(),
               updated_at = NOW()
         WHERE lock_key = $1
           AND holder_instance_id = $2
           AND holder_job_id = $3
           AND expires_at > NOW()
        RETURNING lock_key, holder_instance_id, holder_job_id, metadata,
                  expires_at, heartbeat_at, created_at, updated_at
        "#,
    )
    .bind(request.lock_key.trim())
    .bind(request.holder_instance_id.trim())
    .bind(request.holder_job_id.trim())
    .bind(ttl_secs)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("heartbeat resource lock {}: {error}", request.lock_key))?;
    row.map(lock_from_row).transpose()
}

pub async fn release_resource_lock(
    pool: &PgPool,
    lock_key: &str,
    holder_instance_id: &str,
    holder_job_id: &str,
) -> Result<bool, String> {
    let lock_key = lock_key.trim();
    let holder_instance_id = holder_instance_id.trim();
    let holder_job_id = holder_job_id.trim();
    if lock_key.is_empty() || holder_instance_id.is_empty() || holder_job_id.is_empty() {
        return Err("lock_key, holder_instance_id, and holder_job_id are required".to_string());
    }
    let result = sqlx::query(
        "DELETE FROM resource_locks
          WHERE lock_key = $1 AND holder_instance_id = $2 AND holder_job_id = $3",
    )
    .bind(lock_key)
    .bind(holder_instance_id)
    .bind(holder_job_id)
    .execute(pool)
    .await
    .map_err(|error| format!("release resource lock {lock_key}: {error}"))?;
    Ok(result.rows_affected() > 0)
}

pub async fn reclaim_expired_resource_locks(pool: &PgPool) -> Result<u64, String> {
    let result = sqlx::query("DELETE FROM resource_locks WHERE expires_at <= NOW()")
        .execute(pool)
        .await
        .map_err(|error| format!("reclaim expired resource locks: {error}"))?;
    Ok(result.rows_affected())
}

pub async fn list_resource_locks(
    pool: &PgPool,
    include_expired: bool,
) -> Result<Vec<ResourceLock>, String> {
    let sql = if include_expired {
        "SELECT lock_key, holder_instance_id, holder_job_id, metadata,
                expires_at, heartbeat_at, created_at, updated_at
           FROM resource_locks
          ORDER BY lock_key"
    } else {
        "SELECT lock_key, holder_instance_id, holder_job_id, metadata,
                expires_at, heartbeat_at, created_at, updated_at
           FROM resource_locks
          WHERE expires_at > NOW()
          ORDER BY lock_key"
    };
    let rows = sqlx::query(sql)
        .fetch_all(pool)
        .await
        .map_err(|error| format!("list resource locks: {error}"))?;
    rows.into_iter().map(lock_from_row).collect()
}

pub async fn get_resource_lock(
    pool: &PgPool,
    lock_key: &str,
) -> Result<Option<ResourceLock>, String> {
    let row = sqlx::query(
        "SELECT lock_key, holder_instance_id, holder_job_id, metadata,
                expires_at, heartbeat_at, created_at, updated_at
           FROM resource_locks
          WHERE lock_key = $1",
    )
    .bind(lock_key)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("get resource lock {lock_key}: {error}"))?;
    row.map(lock_from_row).transpose()
}

fn validate_request(request: &ResourceLockRequest) -> Result<(), String> {
    if request.lock_key.trim().is_empty() {
        return Err("lock_key is required".to_string());
    }
    if request.holder_instance_id.trim().is_empty() {
        return Err("holder_instance_id is required".to_string());
    }
    if request.holder_job_id.trim().is_empty() {
        return Err("holder_job_id is required".to_string());
    }
    Ok(())
}

fn normalized_ttl_secs(ttl_secs: Option<i64>) -> i64 {
    ttl_secs
        .unwrap_or(DEFAULT_RESOURCE_LOCK_TTL_SECS)
        .clamp(1, 24 * 60 * 60)
}

fn lock_from_row(row: sqlx::postgres::PgRow) -> Result<ResourceLock, String> {
    Ok(ResourceLock {
        lock_key: row.get("lock_key"),
        holder_instance_id: row.get("holder_instance_id"),
        holder_job_id: row.get("holder_job_id"),
        metadata: row.get("metadata"),
        expires_at: row.get("expires_at"),
        heartbeat_at: row.get("heartbeat_at"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    struct TestPostgresDb {
        database_url: String,
        database_name: String,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let base = postgres_base_database_url();
            let database_name = format!("agentdesk_resource_locks_{}", Uuid::new_v4().simple());
            let admin_url = format!("{base}/postgres");
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "resource_locks tests",
            )
            .await
            .expect("create resource_locks postgres test database");
            Self {
                database_url: format!("{base}/{database_name}"),
                database_name,
            }
        }

        async fn connect_and_migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "resource_locks tests",
            )
            .await
            .expect("connect + migrate resource_locks postgres test db")
        }

        async fn drop(self) {
            let base = postgres_base_database_url();
            let admin_url = format!("{base}/postgres");
            crate::db::postgres::drop_test_database(
                &admin_url,
                &self.database_name,
                "resource_locks tests",
            )
            .await
            .expect("drop resource_locks postgres test database");
        }
    }

    #[tokio::test]
    async fn resource_lock_allows_only_one_active_holder() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let lock_key = unreal_project_lock_key("CookingHeart");

        let first = acquire_resource_lock(
            &pool,
            &ResourceLockRequest {
                lock_key: lock_key.clone(),
                holder_instance_id: "mac-mini-release".to_string(),
                holder_job_id: "phase-compile".to_string(),
                ttl_secs: Some(60),
                metadata: Some(json!({"phase": "compile"})),
            },
        )
        .await
        .unwrap();
        assert!(first.acquired);

        let second = acquire_resource_lock(
            &pool,
            &ResourceLockRequest {
                lock_key: lock_key.clone(),
                holder_instance_id: "mac-book-release".to_string(),
                holder_job_id: "phase-compile".to_string(),
                ttl_secs: Some(60),
                metadata: None,
            },
        )
        .await
        .unwrap();
        assert!(!second.acquired);
        assert_eq!(
            second.current.unwrap().holder_instance_id,
            "mac-mini-release"
        );

        let heartbeat = heartbeat_resource_lock(
            &pool,
            &ResourceLockRequest {
                lock_key: lock_key.clone(),
                holder_instance_id: "mac-mini-release".to_string(),
                holder_job_id: "phase-compile".to_string(),
                ttl_secs: Some(60),
                metadata: None,
            },
        )
        .await
        .unwrap();
        assert!(heartbeat.is_some());

        assert!(
            release_resource_lock(&pool, &lock_key, "mac-mini-release", "phase-compile")
                .await
                .unwrap()
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn resource_lock_reclaims_expired_holder() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let lock_key = unreal_project_lock_key("CookingHeart");

        sqlx::query(
            "INSERT INTO resource_locks (
                lock_key, holder_instance_id, holder_job_id, metadata,
                expires_at, heartbeat_at, created_at, updated_at
             )
             VALUES ($1, 'mac-mini-release', 'old-job', '{}'::jsonb,
                     NOW() - INTERVAL '5 minutes', NOW() - INTERVAL '10 minutes',
                     NOW() - INTERVAL '10 minutes', NOW() - INTERVAL '10 minutes')",
        )
        .bind(&lock_key)
        .execute(&pool)
        .await
        .unwrap();

        let acquired = acquire_resource_lock(
            &pool,
            &ResourceLockRequest {
                lock_key: lock_key.clone(),
                holder_instance_id: "mac-book-release".to_string(),
                holder_job_id: "new-job".to_string(),
                ttl_secs: Some(60),
                metadata: None,
            },
        )
        .await
        .unwrap();
        assert!(acquired.acquired);
        assert_eq!(
            acquired.lock.unwrap().holder_instance_id,
            "mac-book-release"
        );

        sqlx::query("UPDATE resource_locks SET expires_at = NOW() - INTERVAL '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(reclaim_expired_resource_locks(&pool).await.unwrap(), 1);
        assert!(get_resource_lock(&pool, &lock_key).await.unwrap().is_none());

        pool.close().await;
        pg_db.drop().await;
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
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| "postgres".to_string());
        format!("postgres://{user}@127.0.0.1:5432")
    }
}
