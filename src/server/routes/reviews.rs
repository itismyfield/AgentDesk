use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;

use super::AppState;

// ── Body types ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct DecisionItem {
    pub item_id: i64,
    pub decision: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateDecisionsBody {
    pub decisions: Vec<DecisionItem>,
}

fn validate_review_decision(decision: &str) -> bool {
    decision == "accept" || decision == "reject"
}

async fn update_decisions_pg(
    pool: &sqlx::PgPool,
    dispatch_id: &str,
    decisions: &[DecisionItem],
) -> Result<Vec<serde_json::Value>, String> {
    for item in decisions {
        let affected = sqlx::query(
            "UPDATE review_decisions
             SET decision = $1, decided_at = NOW()
             WHERE dispatch_id = $2 AND id = $3",
        )
        .bind(&item.decision)
        .bind(dispatch_id)
        .bind(item.item_id)
        .execute(pool)
        .await
        .map_err(|error| format!("update review_decisions: {error}"))?
        .rows_affected();

        if affected == 0 {
            sqlx::query(
                "INSERT INTO review_decisions (id, dispatch_id, decision, decided_at)
                 VALUES ($1, $2, $3, NOW())
                 ON CONFLICT (id) DO UPDATE SET
                    dispatch_id = EXCLUDED.dispatch_id,
                    decision = EXCLUDED.decision,
                    decided_at = EXCLUDED.decided_at",
            )
            .bind(item.item_id)
            .bind(dispatch_id)
            .bind(&item.decision)
            .execute(pool)
            .await
            .map_err(|error| format!("upsert review_decisions: {error}"))?;
        }
    }

    let rows = sqlx::query(
        "SELECT id, kanban_card_id, dispatch_id, item_index::BIGINT AS item_index, decision, decided_at::text AS decided_at
         FROM review_decisions
         WHERE dispatch_id = $1
         ORDER BY id",
    )
    .bind(dispatch_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load review_decisions: {error}"))?;

    Ok(rows
        .into_iter()
        .map(|row| {
            json!({
                "id": row.try_get::<i64, _>("id").ok(),
                "kanban_card_id": row.try_get::<Option<String>, _>("kanban_card_id").ok().flatten(),
                "dispatch_id": row.try_get::<Option<String>, _>("dispatch_id").ok().flatten(),
                "item_index": row.try_get::<Option<i64>, _>("item_index").ok().flatten(),
                "decision": row.try_get::<Option<String>, _>("decision").ok().flatten(),
                "decided_at": row.try_get::<Option<String>, _>("decided_at").ok().flatten(),
            })
        })
        .collect())
}

async fn resolve_review_card_id_pg(
    pool: &sqlx::PgPool,
    dispatch_id: &str,
) -> Result<Option<String>, String> {
    if let Some(row) = sqlx::query(
        "SELECT kanban_card_id
         FROM review_decisions
         WHERE dispatch_id = $1
         LIMIT 1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load review decision card: {error}"))?
    {
        let card_id = row
            .try_get::<Option<String>, _>("kanban_card_id")
            .map_err(|error| format!("decode review decision card: {error}"))?;
        if card_id.is_some() {
            return Ok(card_id);
        }
    }

    let row = sqlx::query(
        "SELECT kanban_card_id
         FROM task_dispatches
         WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load dispatch card: {error}"))?;

    match row {
        Some(row) => row
            .try_get::<Option<String>, _>("kanban_card_id")
            .map_err(|error| format!("decode dispatch card: {error}")),
        None => Ok(None),
    }
}

// ── Handlers ───────────────────────────────────────────────────

/// PATCH /api/kanban-reviews/:id/decisions
pub async fn update_decisions(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateDecisionsBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    for item in &body.decisions {
        if !validate_review_decision(&item.decision) {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    json!({"error": format!("invalid decision '{}', must be 'accept' or 'reject'", item.decision)}),
                ),
            );
        }
    }

    if let Some(pg_pool) = state.pg_pool.as_ref() {
        return match update_decisions_pg(pg_pool, &id, &body.decisions).await {
            Ok(decisions) => (
                StatusCode::OK,
                Json(json!({"review": {"dispatch_id": id, "decisions": decisions}})),
            ),
            Err(error) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": error})),
            ),
        };
    }

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // The id here refers to a dispatch_id that groups review decisions
    // Update each decision by item_id within this dispatch
    for item in &body.decisions {
        let affected = conn.execute(
            "UPDATE review_decisions SET decision = ?1, decided_at = datetime('now') WHERE dispatch_id = ?2 AND id = ?3",
            libsql_rusqlite::params![item.decision, id, item.item_id],
        ).unwrap_or(0);

        // If no row was updated, try inserting (upsert pattern)
        if affected == 0 {
            conn.execute(
                "INSERT OR REPLACE INTO review_decisions (id, dispatch_id, decision, decided_at) VALUES (?1, ?2, ?3, datetime('now'))",
                libsql_rusqlite::params![item.item_id, id, item.decision],
            ).ok();
        }
    }

    // Return all decisions for this dispatch
    let mut stmt = match conn.prepare(
        "SELECT id, kanban_card_id, dispatch_id, item_index, decision, decided_at
         FROM review_decisions
         WHERE dispatch_id = ?1
         ORDER BY id",
    ) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("prepare: {e}")})),
            );
        }
    };

    let rows = stmt
        .query_map([&id], |row| {
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "kanban_card_id": row.get::<_, Option<String>>(1)?,
                "dispatch_id": row.get::<_, Option<String>>(2)?,
                "item_index": row.get::<_, Option<i64>>(3)?,
                "decision": row.get::<_, Option<String>>(4)?,
                "decided_at": row.get::<_, Option<String>>(5)?,
            }))
        })
        .ok();

    let decisions: Vec<serde_json::Value> = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    };

    (
        StatusCode::OK,
        Json(json!({"review": {"dispatch_id": id, "decisions": decisions}})),
    )
}

/// POST /api/kanban-reviews/:id/trigger-rework
pub async fn trigger_rework(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if let Some(pg_pool) = state.pg_pool.as_ref() {
        let card_id = match resolve_review_card_id_pg(pg_pool, &id).await {
            Ok(card_id) => card_id,
            Err(error) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": error})),
                );
            }
        };

        let card_id = match card_id {
            Some(card_id) => card_id,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "review or dispatch not found"})),
                );
            }
        };

        return match crate::kanban::transition_status_with_opts_pg(
            Some(&state.db),
            pg_pool,
            &state.engine,
            &card_id,
            "in_progress",
            "trigger-rework",
            crate::engine::transition::ForceIntent::OperatorOverride,
        )
        .await
        {
            Ok(_) => (StatusCode::OK, Json(json!({"ok": true}))),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            ),
        };
    }

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // Find the kanban_card_id from review_decisions for this dispatch
    let card_id: Option<String> = conn
        .query_row(
            "SELECT kanban_card_id FROM review_decisions WHERE dispatch_id = ?1 LIMIT 1",
            [&id],
            |row| row.get(0),
        )
        .ok()
        .flatten();

    // Also try looking up from task_dispatches if no review_decision found
    let card_id = card_id.or_else(|| {
        conn.query_row(
            "SELECT kanban_card_id FROM task_dispatches WHERE id = ?1",
            [&id],
            |row| row.get(0),
        )
        .ok()
        .flatten()
    });

    let card_id = match card_id {
        Some(cid) => cid,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "review or dispatch not found"})),
            );
        }
    };

    drop(conn);
    match crate::kanban::transition_status_with_opts(
        &state.db,
        &state.engine,
        &card_id,
        "in_progress",
        "trigger-rework",
        crate::engine::transition::ForceIntent::OperatorOverride,
    ) {
        Ok(_) => (StatusCode::OK, Json(json!({"ok": true}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::engine::PolicyEngine;
    use std::sync::Arc;

    fn test_db() -> Db {
        let conn = libsql_rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    fn test_engine(db: &Db) -> PolicyEngine {
        let config = crate::config::Config::default();
        PolicyEngine::new_with_legacy_db(&config, db.clone()).unwrap()
    }

    fn test_state_with_pg(db: Db, engine: PolicyEngine, pg_pool: sqlx::PgPool) -> AppState {
        let tx = crate::server::ws::new_broadcast();
        let buf = crate::server::ws::spawn_batch_flusher(tx.clone());
        AppState {
            db,
            pg_pool: Some(pg_pool),
            engine,
            config: Arc::new(crate::config::Config::default()),
            broadcast_tx: tx,
            batch_buffer: buf,
            health_registry: None,
        }
    }

    struct TestPostgresDb {
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let admin_url = postgres_admin_database_url();
            let database_name = format!("agentdesk_reviews_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            crate::db::postgres::create_test_database(&admin_url, &database_name, "reviews tests")
                .await
                .unwrap();
            Self {
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(&self.database_url, "reviews tests")
                .await
                .unwrap()
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "reviews tests",
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
    async fn update_decisions_pg_round_trip() {
        let pg_db = TestPostgresDb::create().await;
        let pg_pool = pg_db.migrate().await;
        let sqlite_db = test_db();
        let engine = test_engine(&sqlite_db);
        let state = test_state_with_pg(sqlite_db, engine, pg_pool.clone());

        sqlx::query(
            "INSERT INTO kanban_cards (id, title, status, created_at, updated_at)
             VALUES ($1, $2, $3, NOW(), NOW())",
        )
        .bind("card-1")
        .bind("Review card")
        .bind("review")
        .execute(&pg_pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO review_decisions (id, kanban_card_id, dispatch_id, item_index, decision)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(1_i64)
        .bind("card-1")
        .bind("dispatch-1")
        .bind(0_i64)
        .bind("reject")
        .execute(&pg_pool)
        .await
        .unwrap();

        let (status, Json(body)) = update_decisions(
            State(state),
            Path("dispatch-1".to_string()),
            Json(UpdateDecisionsBody {
                decisions: vec![
                    DecisionItem {
                        item_id: 1,
                        decision: "accept".to_string(),
                    },
                    DecisionItem {
                        item_id: 2,
                        decision: "reject".to_string(),
                    },
                ],
            }),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["review"]["dispatch_id"], "dispatch-1");
        assert_eq!(body["review"]["decisions"][0]["decision"], "accept");
        assert_eq!(body["review"]["decisions"][1]["decision"], "reject");

        let stored: Vec<(i64, Option<String>)> = sqlx::query_as(
            "SELECT id, decision
             FROM review_decisions
             WHERE dispatch_id = $1
             ORDER BY id",
        )
        .bind("dispatch-1")
        .fetch_all(&pg_pool)
        .await
        .unwrap();
        assert_eq!(
            stored,
            vec![
                (1, Some("accept".to_string())),
                (2, Some("reject".to_string()))
            ]
        );

        pg_pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test]
    async fn trigger_rework_pg_only_transitions_review_card_to_in_progress() {
        let pg_db = TestPostgresDb::create().await;
        let pg_pool = pg_db.migrate().await;
        let sqlite_db = test_db();
        let engine = test_engine(&sqlite_db);
        let state = test_state_with_pg(sqlite_db, engine, pg_pool.clone());

        sqlx::query(
            "INSERT INTO kanban_cards (
                id, title, status, review_status, latest_dispatch_id, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, NOW(), NOW())",
        )
        .bind("card-review-1")
        .bind("Review card")
        .bind("review")
        .bind("reviewing")
        .bind("dispatch-review-1")
        .execute(&pg_pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO task_dispatches (
                id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at
             ) VALUES ($1, $2, $3, $4, $5, $6, NOW(), NOW())",
        )
        .bind("dispatch-review-1")
        .bind("card-review-1")
        .bind("project-agentdesk")
        .bind("review")
        .bind("completed")
        .bind("Review dispatch")
        .execute(&pg_pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO review_decisions (id, kanban_card_id, dispatch_id, item_index, decision)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(1_i64)
        .bind("card-review-1")
        .bind("dispatch-review-1")
        .bind(0_i64)
        .bind("reject")
        .execute(&pg_pool)
        .await
        .unwrap();

        sqlx::query(
            "INSERT INTO card_review_state (
                card_id, state, review_round, last_verdict, review_entered_at, updated_at
             ) VALUES ($1, $2, $3, $4, NOW(), NOW())",
        )
        .bind("card-review-1")
        .bind("reviewing")
        .bind(1_i64)
        .bind("reject")
        .execute(&pg_pool)
        .await
        .unwrap();

        let (status, Json(body)) =
            trigger_rework(State(state), Path("dispatch-review-1".to_string())).await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);

        let card_row = sqlx::query(
            "SELECT status, started_at IS NOT NULL AS has_started_at
             FROM kanban_cards
             WHERE id = $1",
        )
        .bind("card-review-1")
        .fetch_one(&pg_pool)
        .await
        .unwrap();
        let status_text: String = card_row.try_get("status").unwrap();
        let has_started_at: bool = card_row.try_get("has_started_at").unwrap();
        assert_eq!(status_text, "in_progress");
        assert!(
            has_started_at,
            "started_at should be set on in_progress entry"
        );

        let review_state_row = sqlx::query(
            "SELECT state, last_verdict
             FROM card_review_state
             WHERE card_id = $1",
        )
        .bind("card-review-1")
        .fetch_one(&pg_pool)
        .await
        .unwrap();
        let review_state: String = review_state_row.try_get("state").unwrap();
        let last_verdict: Option<String> = review_state_row.try_get("last_verdict").unwrap();
        assert_eq!(review_state, "reviewing");
        assert!(
            last_verdict.is_none(),
            "rework entry should clear last verdict"
        );

        let audit_log_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT
             FROM kanban_audit_logs
             WHERE card_id = $1 AND from_status = $2 AND to_status = $3",
        )
        .bind("card-review-1")
        .bind("review")
        .bind("in_progress")
        .fetch_one(&pg_pool)
        .await
        .unwrap();
        assert_eq!(audit_log_count, 1);

        pg_pool.close().await;
        pg_db.drop().await;
    }
}
