//! A tool frame's session-row activity: the tool-use mark and a background child's registration.
//! A reader that released its session row records neither; its frame bookkeeping goes on.

use sqlx::PgPool;

use crate::db::session_observability::{
    BackgroundChildSpawn, insert_background_child_pg, mark_session_tool_use_pg,
};

pub(super) async fn mark_tool_use(
    pool: &PgPool,
    session_key: &str,
    released: bool,
) -> Result<bool, sqlx::Error> {
    if released {
        return Ok(false);
    }
    mark_session_tool_use_pg(pool, session_key).await
}

pub(super) async fn register_background_child(
    pool: &PgPool,
    spawn: &BackgroundChildSpawn,
    released: bool,
) -> Result<Option<i64>, sqlx::Error> {
    if released {
        return Ok(None);
    }
    insert_background_child_pg(pool, spawn).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A released reader's late tool frame changes neither a retired parent nor a successor's row
    /// and registers no child; the owning reader still marks the row and registers its child.
    #[tokio::test(flavor = "current_thread")]
    async fn a_released_reader_records_no_tool_activity_pg() {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate_with_max_connections(4).await;
        let seed = "INSERT INTO sessions (session_key, provider, status, active_turn_nonce,
                      hosted_execution) VALUES ($1, 'claude', $2, $3, $4::JSONB)";
        let read = "SELECT status || '/' || active_turn_nonce || '/' || (last_tool_at IS NOT NULL)
                      || '/' || active_children || '/'
                      || (SELECT COUNT(*) FROM sessions c WHERE c.parent_session_id = s.id)
                    FROM sessions s WHERE session_key = $1";
        for (key, status, nonce, hosted, released, after) in [
            (
                "tool/retired",
                "disconnected",
                "a",
                r#"{"state":"retired"}"#,
                true,
                "disconnected/a/false/0/0",
            ),
            (
                "tool/successor",
                "turn_active",
                "b",
                r#"{"state":"pending"}"#,
                true,
                "turn_active/b/false/0/0",
            ),
            (
                "tool/owner",
                "turn_active",
                "c",
                r#"{"state":"bound"}"#,
                false,
                "turn_active/c/true/1/1",
            ),
        ] {
            let insert = sqlx::query(seed)
                .bind(key)
                .bind(status)
                .bind(nonce)
                .bind(hosted);
            insert.execute(&pool).await.unwrap();
            let spawn = BackgroundChildSpawn {
                parent_session_key: key.into(),
                provider: Some("claude".into()),
                tool_name: "Task".into(),
                tool_input: "{}".into(),
                tool_use_id: Some(format!("{key}-tool")),
            };
            assert_eq!(
                mark_tool_use(&pool, key, released).await.unwrap(),
                !released,
                "{key}"
            );
            let child = register_background_child(&pool, &spawn, released)
                .await
                .unwrap();
            assert_eq!(child.is_some(), !released, "{key}");
            let row: String = sqlx::query_scalar(read)
                .bind(key)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(row, after, "{key}");
        }
        pool.close().await;
        db.drop().await;
    }
}
