use axum::{
    Json,
    extract::{Query, State},
};
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;

use super::AppState;

#[derive(Deserialize)]
pub struct TerminationQueryParams {
    pub dispatch_id: Option<String>,
    pub card_id: Option<String>,
    pub session_key: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_limit() -> u32 {
    50
}

pub async fn list_termination_events(
    State(state): State<AppState>,
    Query(params): Query<TerminationQueryParams>,
) -> Json<serde_json::Value> {
    let limit = params.limit.min(500);

    let Some(pool) = state.pg_pool_ref() else {
        return Json(json!({
            "error": "PostgreSQL pool unavailable for termination events",
            "events": []
        }));
    };

    match list_termination_events_pg(pool, &params, limit).await {
        Ok(events) => Json(json!({"events": events})),
        Err(error) => Json(json!({"error": format!("Query error: {error}"), "events": []})),
    }
}

async fn list_termination_events_pg(
    pool: &sqlx::PgPool,
    params: &TerminationQueryParams,
    limit: u32,
) -> Result<Vec<serde_json::Value>, String> {
    let limit = limit as i64;

    let rows = if let Some(card_id) = params.card_id.as_deref() {
        sqlx::query(
            "SELECT ste.id,
                    ste.session_key,
                    ste.dispatch_id,
                    ste.killer_component,
                    ste.reason_code,
                    ste.reason_text,
                    ste.probe_snapshot,
                    ste.last_offset::BIGINT AS last_offset,
                    ste.tmux_alive,
                    TO_CHAR(ste.created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS created_at
             FROM session_termination_events ste
             LEFT JOIN task_dispatches td ON ste.dispatch_id = td.id
             WHERE td.kanban_card_id = $1
             ORDER BY ste.created_at DESC, ste.id DESC
             LIMIT $2",
        )
        .bind(card_id)
        .bind(limit)
        .fetch_all(pool)
        .await
    } else if let Some(dispatch_id) = params.dispatch_id.as_deref() {
        sqlx::query(
            "SELECT id,
                    session_key,
                    dispatch_id,
                    killer_component,
                    reason_code,
                    reason_text,
                    probe_snapshot,
                    last_offset::BIGINT AS last_offset,
                    tmux_alive,
                    TO_CHAR(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS created_at
             FROM session_termination_events
             WHERE dispatch_id = $1
             ORDER BY created_at DESC, id DESC
             LIMIT $2",
        )
        .bind(dispatch_id)
        .bind(limit)
        .fetch_all(pool)
        .await
    } else if let Some(session_key) = params.session_key.as_deref() {
        sqlx::query(
            "SELECT id,
                    session_key,
                    dispatch_id,
                    killer_component,
                    reason_code,
                    reason_text,
                    probe_snapshot,
                    last_offset::BIGINT AS last_offset,
                    tmux_alive,
                    TO_CHAR(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS created_at
             FROM session_termination_events
             WHERE session_key = $1
             ORDER BY created_at DESC, id DESC
             LIMIT $2",
        )
        .bind(session_key)
        .bind(limit)
        .fetch_all(pool)
        .await
    } else {
        sqlx::query(
            "SELECT id,
                    session_key,
                    dispatch_id,
                    killer_component,
                    reason_code,
                    reason_text,
                    probe_snapshot,
                    last_offset::BIGINT AS last_offset,
                    tmux_alive,
                    TO_CHAR(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS created_at
             FROM session_termination_events
             ORDER BY created_at DESC, id DESC
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(pool)
        .await
    }
    .map_err(|error| format!("{error}"))?;

    rows.into_iter()
        .map(|row| {
            Ok(json!({
                "id": row.try_get::<i64, _>("id").map_err(|error| format!("{error}"))?,
                "session_key": row.try_get::<String, _>("session_key").map_err(|error| format!("{error}"))?,
                "dispatch_id": row.try_get::<Option<String>, _>("dispatch_id").map_err(|error| format!("{error}"))?,
                "killer_component": row.try_get::<String, _>("killer_component").map_err(|error| format!("{error}"))?,
                "reason_code": row.try_get::<String, _>("reason_code").map_err(|error| format!("{error}"))?,
                "reason_text": row.try_get::<Option<String>, _>("reason_text").map_err(|error| format!("{error}"))?,
                "probe_snapshot": row.try_get::<Option<String>, _>("probe_snapshot").map_err(|error| format!("{error}"))?,
                "last_offset": row.try_get::<Option<i64>, _>("last_offset").map_err(|error| format!("{error}"))?,
                "tmux_alive": row.try_get::<Option<i64>, _>("tmux_alive").map_err(|error| format!("{error}"))?.map(|value| value != 0),
                "created_at": row.try_get::<String, _>("created_at").map_err(|error| format!("{error}"))?,
            }))
        })
        .collect()
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
            let database_name = format!("agentdesk_term_events_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "termination events tests",
            )
            .await
            .unwrap();

            Self {
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn connect_and_migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "termination events tests",
            )
            .await
            .unwrap()
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "termination events tests",
            )
            .await
            .unwrap();
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
    async fn list_termination_events_pg_filters_by_dispatch_id() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;

        sqlx::query(
            "INSERT INTO session_termination_events
             (session_key, dispatch_id, killer_component, reason_code, reason_text, last_offset, tmux_alive)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind("host:term-1")
        .bind("dispatch-1")
        .bind("cleanup")
        .bind("idle_session_expiry")
        .bind("expired")
        .bind(17_i64)
        .bind(0_i32)
        .execute(&pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO session_termination_events
             (session_key, dispatch_id, killer_component, reason_code)
             VALUES ($1, $2, $3, $4)",
        )
        .bind("host:term-2")
        .bind("dispatch-2")
        .bind("force_kill_api")
        .bind("force_kill")
        .execute(&pool)
        .await
        .unwrap();

        let events = list_termination_events_pg(
            &pool,
            &TerminationQueryParams {
                dispatch_id: Some("dispatch-1".to_string()),
                card_id: None,
                session_key: None,
                limit: 50,
            },
            50,
        )
        .await
        .unwrap();

        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["dispatch_id"], json!("dispatch-1"));
        assert_eq!(events[0]["session_key"], json!("host:term-1"));
        assert_eq!(events[0]["last_offset"], json!(17));
        assert_eq!(events[0]["tmux_alive"], json!(false));

        pool.close().await;
        pg_db.drop().await;
    }
}
