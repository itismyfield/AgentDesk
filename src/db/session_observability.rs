use sqlx::{PgPool, Row};

use crate::db::session_status::{
    ABORTED, AWAITING_BG, AWAITING_USER, DISCONNECTED, IDLE, TURN_ACTIVE,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundChildSpawn {
    pub parent_session_key: String,
    pub provider: Option<String>,
    pub tool_name: String,
    pub tool_input: String,
}

pub async fn mark_session_tool_use_pg(
    pool: &PgPool,
    session_key: &str,
) -> Result<bool, sqlx::Error> {
    let session_key = session_key.trim();
    if session_key.is_empty() {
        return Ok(false);
    }

    let result = sqlx::query(
        "UPDATE sessions
            SET last_tool_at = NOW(),
                status = $2
          WHERE session_key = $1
            AND status NOT IN ($3, $4)",
    )
    .bind(session_key)
    .bind(TURN_ACTIVE)
    .bind(DISCONNECTED)
    .bind(ABORTED)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

pub async fn insert_background_child_pg(
    pool: &PgPool,
    spawn: &BackgroundChildSpawn,
) -> Result<Option<i64>, sqlx::Error> {
    let parent_session_key = spawn.parent_session_key.trim();
    if parent_session_key.is_empty() {
        return Ok(None);
    }

    let mut tx = pool.begin().await?;
    let parent = sqlx::query(
        "SELECT id, agent_id, cwd, thread_channel_id
           FROM sessions
          WHERE session_key = $1
          FOR UPDATE",
    )
    .bind(parent_session_key)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(parent) = parent else {
        tx.commit().await?;
        return Ok(None);
    };

    let parent_id: i64 = parent.get("id");
    let agent_id: Option<String> = parent.try_get("agent_id").ok();
    let cwd: Option<String> = parent.try_get("cwd").ok();
    let thread_channel_id: Option<String> = parent.try_get("thread_channel_id").ok();
    let child_session_key = format!(
        "{}:child:{}",
        parent_session_key,
        uuid::Uuid::new_v4().simple()
    );
    let purpose = background_child_purpose(&spawn.tool_name, &spawn.tool_input);
    let provider = spawn
        .provider
        .as_deref()
        .filter(|value| !value.trim().is_empty());

    let child_id: i64 = sqlx::query_scalar(
        "INSERT INTO sessions (
            session_key,
            agent_id,
            provider,
            status,
            cwd,
            thread_channel_id,
            parent_session_id,
            spawned_at,
            purpose,
            created_at
         ) VALUES ($1, $2, COALESCE($3, 'claude'), $8, $4, $5, $6, NOW(), $7, NOW())
         RETURNING id",
    )
    .bind(child_session_key)
    .bind(agent_id)
    .bind(provider)
    .bind(cwd)
    .bind(thread_channel_id)
    .bind(parent_id)
    .bind(purpose)
    .bind(TURN_ACTIVE)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query("UPDATE sessions SET active_children = active_children + 1 WHERE id = $1")
        .bind(parent_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(Some(child_id))
}

pub async fn close_background_child_pg(
    pool: &PgPool,
    child_session_id: i64,
    status: &str,
) -> Result<bool, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let row = sqlx::query(
        "SELECT parent_session_id, closed_at
           FROM sessions
          WHERE id = $1
          FOR UPDATE",
    )
    .bind(child_session_id)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(row) = row else {
        tx.commit().await?;
        return Ok(false);
    };

    let parent_session_id: Option<i64> = row.try_get("parent_session_id").ok();
    let closed_at: Option<chrono::DateTime<chrono::Utc>> = row.try_get("closed_at").ok();
    if closed_at.is_some() {
        tx.commit().await?;
        return Ok(false);
    }

    let status = normalized_close_status(status);
    sqlx::query("UPDATE sessions SET closed_at = NOW(), status = $2 WHERE id = $1")
        .bind(child_session_id)
        .bind(status)
        .execute(&mut *tx)
        .await?;

    if let Some(parent_session_id) = parent_session_id {
        sqlx::query(
            "UPDATE sessions
                SET active_children = GREATEST(active_children - 1, 0),
                    status = CASE
                        WHEN GREATEST(active_children - 1, 0) = 0 AND status = $2 THEN $3
                        ELSE status
                    END
              WHERE id = $1",
        )
        .bind(parent_session_id)
        .bind(AWAITING_BG)
        .bind(AWAITING_USER)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(true)
}

pub fn background_child_purpose(tool_name: &str, tool_input: &str) -> String {
    let tool_name = tool_name.trim();
    let tool_label = if tool_name.is_empty() {
        "Tool"
    } else {
        tool_name
    };
    let input = serde_json::from_str::<serde_json::Value>(tool_input).ok();
    let detail = match tool_label.to_ascii_lowercase().as_str() {
        "bash" => input
            .as_ref()
            .and_then(|value| value.get("command"))
            .and_then(|value| value.as_str())
            .unwrap_or(tool_input),
        "agent" | "task" => input
            .as_ref()
            .and_then(|value| value.get("description"))
            .and_then(|value| value.as_str())
            .unwrap_or(tool_input),
        _ => tool_input,
    };
    let detail = truncate_utf8_bytes(detail.trim(), 80);
    if detail.is_empty() {
        tool_label.to_string()
    } else {
        format!("{tool_label}: {detail}")
    }
}

fn normalized_close_status(status: &str) -> &'static str {
    match status.trim().to_ascii_lowercase().as_str() {
        "aborted" | "abort" | "cancelled" | "canceled" | "failed" | "error" => ABORTED,
        _ => IDLE,
    }
}

fn truncate_utf8_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        BackgroundChildSpawn, background_child_purpose, close_background_child_pg,
        insert_background_child_pg, mark_session_tool_use_pg,
    };
    use sqlx::Row;

    struct SessionObservabilityPgDatabase {
        _lifecycle: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl SessionObservabilityPgDatabase {
        async fn create() -> Option<Self> {
            let base = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE")
                .ok()
                .filter(|value| !value.trim().is_empty())?;
            let lifecycle = crate::db::postgres::lock_test_lifecycle();
            let base = base.trim().trim_end_matches('/').to_string();
            let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "postgres".to_string());
            let admin_url = format!("{base}/{admin_db}");
            let database_name = format!("agentdesk_session_obs_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{base}/{database_name}");
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "session observability pg",
            )
            .await
            .expect("create session observability postgres test db");
            Some(Self {
                _lifecycle: lifecycle,
                admin_url,
                database_name,
                database_url,
            })
        }

        async fn migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "session observability pg",
            )
            .await
            .expect("connect + migrate session observability postgres test db")
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "session observability pg",
            )
            .await
            .expect("drop session observability postgres test db");
        }
    }

    #[test]
    fn background_child_purpose_prefers_tool_specific_summary() {
        assert_eq!(
            background_child_purpose(
                "Bash",
                r#"{"command":"cargo check --bin agentdesk --tests && echo done"}"#
            ),
            "Bash: cargo check --bin agentdesk --tests && echo done"
        );
        assert_eq!(
            background_child_purpose("Agent", r#"{"description":"review session lifecycle"}"#),
            "Agent: review session lifecycle"
        );
    }

    #[tokio::test]
    async fn sessions_last_tool_at_pg_updates_parent() {
        let Some(test_db) = SessionObservabilityPgDatabase::create().await else {
            eprintln!("skipping sessions_last_tool_at_pg_updates_parent: postgres unavailable");
            return;
        };
        let pool = test_db.migrate().await;

        sqlx::query("INSERT INTO sessions (session_key, provider, status) VALUES ($1, $2, $3)")
            .bind("parent-session")
            .bind("codex")
            .bind(crate::db::session_status::TURN_ACTIVE)
            .execute(&pool)
            .await
            .expect("insert parent session");

        let before: Option<chrono::DateTime<chrono::Utc>> =
            sqlx::query_scalar("SELECT last_tool_at FROM sessions WHERE session_key = $1")
                .bind("parent-session")
                .fetch_one(&pool)
                .await
                .expect("load last_tool_at before");
        assert!(before.is_none());

        assert!(
            mark_session_tool_use_pg(&pool, "parent-session")
                .await
                .expect("mark tool use")
        );

        let (after, status): (Option<chrono::DateTime<chrono::Utc>>, String) =
            sqlx::query_as("SELECT last_tool_at, status FROM sessions WHERE session_key = $1")
                .bind("parent-session")
                .fetch_one(&pool)
                .await
                .expect("load last_tool_at after");
        assert!(after.is_some());
        assert_eq!(status, crate::db::session_status::TURN_ACTIVE);

        crate::db::postgres::close_test_pool(pool, "session observability pg")
            .await
            .expect("close test pool");
        test_db.drop().await;
    }

    #[tokio::test]
    async fn sessions_background_child_lifecycle_pg_tracks_parent_count() {
        let Some(test_db) = SessionObservabilityPgDatabase::create().await else {
            eprintln!(
                "skipping sessions_background_child_lifecycle_pg_tracks_parent_count: postgres unavailable"
            );
            return;
        };
        let pool = test_db.migrate().await;

        let parent_id: i64 = sqlx::query_scalar(
            "INSERT INTO sessions (session_key, provider, status, cwd, thread_channel_id)
             VALUES ($1, $2, $3, $4, $5)
             RETURNING id",
        )
        .bind("parent-session")
        .bind("codex")
        .bind(crate::db::session_status::TURN_ACTIVE)
        .bind("/tmp/worktree")
        .bind("12345")
        .fetch_one(&pool)
        .await
        .expect("insert parent session");

        let child_id = insert_background_child_pg(
            &pool,
            &BackgroundChildSpawn {
                parent_session_key: "parent-session".to_string(),
                provider: Some("codex".to_string()),
                tool_name: "Bash".to_string(),
                tool_input: r#"{"command":"sleep 10 && echo ready","run_in_background":true}"#
                    .to_string(),
            },
        )
        .await
        .expect("insert background child")
        .expect("child id");

        let (parent_children, parent_status): (i32, String) =
            sqlx::query_as("SELECT active_children, status FROM sessions WHERE id = $1")
                .bind(parent_id)
                .fetch_one(&pool)
                .await
                .expect("load active_children after spawn");
        assert_eq!(parent_children, 1);
        assert_eq!(parent_status, crate::db::session_status::TURN_ACTIVE);

        let child = sqlx::query(
            "SELECT parent_session_id, spawned_at, closed_at, purpose, cwd, thread_channel_id
               FROM sessions
              WHERE id = $1",
        )
        .bind(child_id)
        .fetch_one(&pool)
        .await
        .expect("load child");
        assert_eq!(child.get::<i64, _>("parent_session_id"), parent_id);
        assert!(
            child
                .get::<Option<chrono::DateTime<chrono::Utc>>, _>("spawned_at")
                .is_some()
        );
        assert!(
            child
                .get::<Option<chrono::DateTime<chrono::Utc>>, _>("closed_at")
                .is_none()
        );
        assert_eq!(
            child.get::<Option<String>, _>("purpose").as_deref(),
            Some("Bash: sleep 10 && echo ready")
        );
        assert_eq!(
            child.get::<Option<String>, _>("cwd").as_deref(),
            Some("/tmp/worktree")
        );
        assert_eq!(
            child
                .get::<Option<String>, _>("thread_channel_id")
                .as_deref(),
            Some("12345")
        );

        assert!(
            close_background_child_pg(&pool, child_id, "completed")
                .await
                .expect("close child")
        );
        assert!(
            !close_background_child_pg(&pool, child_id, "completed")
                .await
                .expect("close child idempotently")
        );

        let (parent_children, parent_status, child_closed_at): (
            i32,
            String,
            Option<chrono::DateTime<chrono::Utc>>,
        ) = sqlx::query_as(
            "SELECT p.active_children, p.status, c.closed_at
               FROM sessions p
               JOIN sessions c ON c.id = $2
              WHERE p.id = $1",
        )
        .bind(parent_id)
        .bind(child_id)
        .fetch_one(&pool)
        .await
        .expect("load close state");
        assert_eq!(parent_children, 0);
        assert_eq!(parent_status, crate::db::session_status::TURN_ACTIVE);
        assert!(child_closed_at.is_some());

        crate::db::postgres::close_test_pool(pool, "session observability pg")
            .await
            .expect("close test pool");
        test_db.drop().await;
    }
}
