use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::{PgPool, QueryBuilder, Row};

use super::AppState;

// ── Body types ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CreateOfficeBody {
    pub name: String,
    pub layout: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateOfficeBody {
    pub name: Option<String>,
    pub layout: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AddAgentBody {
    pub agent_id: String,
    pub department_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateOfficeAgentBody {
    pub department_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct BatchAddAgentsBody {
    pub agent_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct ReorderOfficeItem {
    pub id: String,
    pub sort_order: i32,
}

fn pg_unavailable() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"error": "postgres pool not configured"})),
    )
}

// ── Handlers ──────────────────────────────────────────────────

/// GET /api/offices
pub async fn list_offices(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        return match list_offices_pg(pool).await {
            Ok(offices) => (StatusCode::OK, Json(json!({"offices": offices}))),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    pg_unavailable()
}

/// PATCH /api/offices/reorder
pub async fn reorder_offices(
    State(state): State<AppState>,
    Json(body): Json<Vec<ReorderOfficeItem>>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        let mut tx = match pool.begin().await {
            Ok(tx) => tx,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("begin tx: {error}")})),
                );
            }
        };

        let mut updated = 0usize;
        for item in &body {
            match sqlx::query("UPDATE offices SET sort_order = $1 WHERE id = $2")
                .bind(item.sort_order)
                .bind(&item.id)
                .execute(&mut *tx)
                .await
            {
                Ok(result) => updated += result.rows_affected() as usize,
                Err(error) => {
                    let _ = tx.rollback().await;
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("update id={}: {error}", item.id)})),
                    );
                }
            }
        }

        if let Err(error) = tx.commit().await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("commit: {error}")})),
            );
        }

        return (
            StatusCode::OK,
            Json(json!({"ok": true, "updated": updated})),
        );
    }

    pg_unavailable()
}

/// POST /api/offices
pub async fn create_office(
    State(state): State<AppState>,
    Json(body): Json<CreateOfficeBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let id = uuid::Uuid::new_v4().to_string();

    if let Some(pool) = state.pg_pool_ref() {
        if let Err(error) = sqlx::query(
            "INSERT INTO offices (id, name, layout, sort_order, created_at)
             VALUES ($1, $2, $3, 0, NOW())",
        )
        .bind(&id)
        .bind(body.name.as_str())
        .bind(body.layout.as_deref())
        .execute(pool)
        .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            );
        }

        return (
            StatusCode::CREATED,
            Json(json!({
                "office": {
                    "id": id,
                    "name": body.name,
                    "layout": body.layout,
                }
            })),
        );
    }

    pg_unavailable()
}

/// PATCH /api/offices/:id
pub async fn update_office(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateOfficeBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        let mut updated_any = false;
        let mut builder = QueryBuilder::<sqlx::Postgres>::new("UPDATE offices SET ");
        let mut separated = builder.separated(", ");

        if let Some(ref name) = body.name {
            updated_any = true;
            separated.push("name = ").push_bind_unseparated(name);
        }
        if let Some(ref layout) = body.layout {
            updated_any = true;
            separated.push("layout = ").push_bind_unseparated(layout);
        }

        if !updated_any {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "no fields to update"})),
            );
        }

        builder.push(" WHERE id = ");
        builder.push_bind(&id);

        match builder.build().execute(pool).await {
            Ok(result) if result.rows_affected() == 0 => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "office not found"})),
                );
            }
            Ok(_) => {}
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                );
            }
        }

        return match sqlx::query("SELECT id, name, layout FROM offices WHERE id = $1")
            .bind(&id)
            .fetch_one(pool)
            .await
        {
            Ok(row) => (
                StatusCode::OK,
                Json(json!({
                    "office": {
                        "id": row.get::<String, _>("id"),
                        "name": row.get::<Option<String>, _>("name"),
                        "layout": row.get::<Option<String>, _>("layout"),
                    }
                })),
            ),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    pg_unavailable()
}

/// DELETE /api/offices/:id
pub async fn delete_office(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query("DELETE FROM offices WHERE id = $1")
            .bind(&id)
            .execute(pool)
            .await
        {
            Ok(result) if result.rows_affected() == 0 => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "office not found"})),
            ),
            Ok(_) => {
                let _ = sqlx::query("DELETE FROM office_agents WHERE office_id = $1")
                    .bind(&id)
                    .execute(pool)
                    .await;
                (StatusCode::OK, Json(json!({"ok": true})))
            }
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    pg_unavailable()
}

/// POST /api/offices/:id/agents
pub async fn add_agent(
    State(state): State<AppState>,
    Path(office_id): Path<String>,
    Json(body): Json<AddAgentBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        return match office_exists_or_response(
            office_exists_pg(pool, &office_id).await,
            &office_id,
            "add office agent",
        ) {
            Ok(true) => match sqlx::query(
                "INSERT INTO office_agents (office_id, agent_id, department_id)
                 VALUES ($1, $2, $3)
                 ON CONFLICT (office_id, agent_id)
                 DO UPDATE SET department_id = EXCLUDED.department_id",
            )
            .bind(&office_id)
            .bind(body.agent_id.as_str())
            .bind(body.department_id.as_deref())
            .execute(pool)
            .await
            {
                Ok(_) => (StatusCode::OK, Json(json!({"ok": true}))),
                Err(error) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{error}")})),
                ),
            },
            Ok(false) => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "office not found"})),
            ),
            Err(response) => response,
        };
    }

    pg_unavailable()
}

/// DELETE /api/offices/:office_id/agents/:agent_id
pub async fn remove_agent(
    State(state): State<AppState>,
    Path((office_id, agent_id)): Path<(String, String)>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query("DELETE FROM office_agents WHERE office_id = $1 AND agent_id = $2")
            .bind(&office_id)
            .bind(&agent_id)
            .execute(pool)
            .await
        {
            Ok(result) if result.rows_affected() == 0 => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "office-agent link not found"})),
            ),
            Ok(_) => (StatusCode::OK, Json(json!({"ok": true}))),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    pg_unavailable()
}

/// PATCH /api/offices/:office_id/agents/:agent_id
pub async fn update_office_agent(
    State(state): State<AppState>,
    Path((office_id, agent_id)): Path<(String, String)>,
    Json(body): Json<UpdateOfficeAgentBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        return match sqlx::query(
            "UPDATE office_agents SET department_id = $1 WHERE office_id = $2 AND agent_id = $3",
        )
        .bind(body.department_id.as_deref())
        .bind(&office_id)
        .bind(&agent_id)
        .execute(pool)
        .await
        {
            Ok(result) if result.rows_affected() == 0 => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "office-agent link not found"})),
            ),
            Ok(_) => (StatusCode::OK, Json(json!({"ok": true}))),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ),
        };
    }

    pg_unavailable()
}

/// POST /api/offices/:id/agents/batch
pub async fn batch_add_agents(
    State(state): State<AppState>,
    Path(office_id): Path<String>,
    Json(body): Json<BatchAddAgentsBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pool) = state.pg_pool_ref() {
        return match office_exists_or_response(
            office_exists_pg(pool, &office_id).await,
            &office_id,
            "batch add office agents",
        ) {
            Ok(true) => {
                let mut tx = match pool.begin().await {
                    Ok(tx) => tx,
                    Err(error) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": format!("{error}")})),
                        );
                    }
                };

                for agent_id in &body.agent_ids {
                    if let Err(error) = sqlx::query(
                        "INSERT INTO office_agents (office_id, agent_id, department_id)
                         VALUES ($1, $2, NULL)
                         ON CONFLICT (office_id, agent_id)
                         DO UPDATE SET department_id = NULL",
                    )
                    .bind(&office_id)
                    .bind(agent_id)
                    .execute(&mut *tx)
                    .await
                    {
                        let _ = tx.rollback().await;
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error": format!("{error}")})),
                        );
                    }
                }

                if let Err(error) = tx.commit().await {
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": format!("{error}")})),
                    );
                }

                (StatusCode::OK, Json(json!({"ok": true})))
            }
            Ok(false) => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "office not found"})),
            ),
            Err(response) => response,
        };
    }

    pg_unavailable()
}

async fn list_offices_pg(pool: &PgPool) -> Result<Vec<serde_json::Value>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT o.id, o.name, o.name_ko, o.icon, o.color, o.description,
                o.sort_order::BIGINT AS sort_order,
                o.created_at::TEXT AS created_at,
                (SELECT COUNT(*)::BIGINT FROM office_agents oa WHERE oa.office_id = o.id) AS agent_count,
                (SELECT COUNT(*)::BIGINT FROM departments d WHERE d.office_id = o.id) AS dept_count
         FROM offices o
         ORDER BY o.sort_order, o.id",
    )
    .fetch_all(pool)
    .await?;

    Ok(rows.iter().map(office_row_to_json_pg).collect())
}

fn office_row_to_json_pg(row: &sqlx::postgres::PgRow) -> serde_json::Value {
    json!({
        "id": row.get::<String, _>("id"),
        "name": row.get::<Option<String>, _>("name"),
        "name_ko": row.get::<Option<String>, _>("name_ko"),
        "icon": row.get::<Option<String>, _>("icon"),
        "color": row.get::<Option<String>, _>("color"),
        "description": row.get::<Option<String>, _>("description"),
        "sort_order": row.get::<i64, _>("sort_order"),
        "created_at": row.get::<Option<String>, _>("created_at"),
        "agent_count": row.get::<i64, _>("agent_count"),
        "department_count": row.get::<i64, _>("dept_count"),
    })
}

async fn office_exists_pg(pool: &PgPool, office_id: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*)::BIGINT FROM offices WHERE id = $1")
        .bind(office_id)
        .fetch_one(pool)
        .await
        .map(|count| count > 0)
}

fn office_exists_or_response(
    result: Result<bool, sqlx::Error>,
    office_id: &str,
    action: &'static str,
) -> Result<bool, (StatusCode, Json<serde_json::Value>)> {
    match result {
        Ok(exists) => Ok(exists),
        Err(error) => {
            tracing::warn!(
                office_id = %office_id,
                action,
                "failed to look up office via postgres: {error}"
            );
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{error}")})),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::office_exists_or_response;
    use axum::{Json, http::StatusCode};
    use serde_json::json;
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct TestLogWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for TestLogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs<T>(run: impl FnOnce() -> T) -> (T, String) {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let log_buffer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .with_writer(move || TestLogWriter {
                buffer: log_buffer.clone(),
            })
            .finish();

        let result = tracing::subscriber::with_default(subscriber, run);
        let captured = buffer.lock().unwrap().clone();
        (result, String::from_utf8_lossy(&captured).to_string())
    }

    #[test]
    fn office_exists_or_response_logs_pg_lookup_errors() {
        let (result, logs) = capture_logs(|| {
            office_exists_or_response(
                Err(sqlx::Error::ColumnNotFound("id".to_string())),
                "office-1",
                "add office agent",
            )
        });

        let Err((status, Json(body))) = result else {
            panic!("expected error response");
        };
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, json!({"error": "no column found for name: id"}));
        assert!(logs.contains("failed to look up office via postgres"));
        assert!(logs.contains("office_id=office-1"));
        assert!(logs.contains("action=\"add office agent\""));
    }
}
