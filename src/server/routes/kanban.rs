use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::engine::hooks::Hook;

// ── Query / Body types ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ListCardsQuery {
    pub status: Option<String>,
    pub repo_id: Option<String>,
    pub assigned_agent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateCardBody {
    pub title: String,
    pub repo_id: Option<String>,
    pub priority: Option<String>,
    pub github_issue_url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateCardBody {
    pub title: Option<String>,
    pub status: Option<String>,
    pub priority: Option<String>,
    pub assigned_agent_id: Option<String>,
    pub repo_id: Option<String>,
    pub github_issue_url: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct AssignCardBody {
    pub agent_id: String,
}

#[derive(Debug, Deserialize)]
pub struct RetryCardBody {
    pub assignee_agent_id: Option<String>,
    pub request_now: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct RedispatchCardBody {
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DeferDodBody {
    pub items: Option<Vec<String>>,
    pub verify: Option<Vec<String>>,
    pub unverify: Option<Vec<String>>,
    pub remove: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
pub struct BulkActionBody {
    pub action: String,
    pub card_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct AssignIssueBody {
    pub github_repo: String,
    pub github_issue_number: i64,
    pub github_issue_url: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub assignee_agent_id: String,
}

// ── Handlers ───────────────────────────────────────────────────

/// GET /api/kanban-cards
pub async fn list_cards(
    State(state): State<AppState>,
    Query(params): Query<ListCardsQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let result = state.db.lock().map_err(|e| format!("{e}"));
    let conn = match result {
        Ok(c) => c,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": e}))),
    };

    // Only show cards from registered repos (unless a specific repo_id filter is given)
    let registered_repos: Vec<String> = {
        let repo_sql = "SELECT id FROM github_repos";
        match conn.prepare(repo_sql) {
            Ok(mut stmt) => stmt
                .query_map([], |row| row.get::<_, String>(0))
                .ok()
                .map(|iter| iter.filter_map(|r| r.ok()).collect())
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    };

    let mut sql = String::from(
        "SELECT id, repo_id, title, status, priority, assigned_agent_id, github_issue_url, github_issue_number, latest_dispatch_id, review_round, metadata, created_at, updated_at FROM kanban_cards WHERE 1=1"
    );
    let mut bind_values: Vec<String> = Vec::new();

    if let Some(ref status) = params.status {
        bind_values.push(status.clone());
        sql.push_str(&format!(" AND status = ?{}", bind_values.len()));
    }
    if let Some(ref repo_id) = params.repo_id {
        bind_values.push(repo_id.clone());
        sql.push_str(&format!(" AND repo_id = ?{}", bind_values.len()));
    } else if !registered_repos.is_empty() {
        let placeholders: Vec<String> = registered_repos
            .iter()
            .enumerate()
            .map(|(i, r)| {
                bind_values.push(r.clone());
                format!("?{}", bind_values.len())
            })
            .collect();
        sql.push_str(&format!(" AND repo_id IN ({})", placeholders.join(",")));
    }
    if let Some(ref agent_id) = params.assigned_agent_id {
        bind_values.push(agent_id.clone());
        sql.push_str(&format!(" AND assigned_agent_id = ?{}", bind_values.len()));
    }

    sql.push_str(" ORDER BY created_at DESC");

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("prepare: {e}")})),
            )
        }
    };

    let params_ref: Vec<&dyn rusqlite::types::ToSql> =
        bind_values.iter().map(|v| v as &dyn rusqlite::types::ToSql).collect();

    let rows = stmt
        .query_map(params_ref.as_slice(), |row| card_row_to_json(row))
        .ok();

    let cards: Vec<serde_json::Value> = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    };

    (StatusCode::OK, Json(json!({"cards": cards})))
}

/// GET /api/kanban-cards/:id
pub async fn get_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            )
        }
    };

    match conn.query_row(
        "SELECT id, repo_id, title, status, priority, assigned_agent_id, github_issue_url, github_issue_number, latest_dispatch_id, review_round, metadata, created_at, updated_at FROM kanban_cards WHERE id = ?1",
        [&id],
        |row| card_row_to_json(row),
    ) {
        Ok(card) => (StatusCode::OK, Json(json!({"card": card}))),
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            (StatusCode::NOT_FOUND, Json(json!({"error": "card not found"})))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// POST /api/kanban-cards
pub async fn create_card(
    State(state): State<AppState>,
    Json(body): Json<CreateCardBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let id = uuid::Uuid::new_v4().to_string();
    let priority = body.priority.unwrap_or_else(|| "medium".to_string());

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            )
        }
    };

    let result = conn.execute(
        "INSERT INTO kanban_cards (id, repo_id, title, status, priority, github_issue_url, created_at, updated_at)
         VALUES (?1, ?2, ?3, 'backlog', ?4, ?5, datetime('now'), datetime('now'))",
        rusqlite::params![id, body.repo_id, body.title, priority, body.github_issue_url],
    );

    if let Err(e) = result {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        );
    }

    match conn.query_row(
        "SELECT id, repo_id, title, status, priority, assigned_agent_id, github_issue_url, github_issue_number, latest_dispatch_id, review_round, metadata, created_at, updated_at FROM kanban_cards WHERE id = ?1",
        [&id],
        |row| card_row_to_json(row),
    ) {
        Ok(card) => (StatusCode::CREATED, Json(json!({"card": card}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// PATCH /api/kanban-cards/:id
pub async fn update_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateCardBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    // Read old status for transition hook
    let old_status: Option<String> = {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                )
            }
        };

        conn.query_row(
            "SELECT status FROM kanban_cards WHERE id = ?1",
            [&id],
            |row| row.get(0),
        )
        .ok()
    };

    if old_status.is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "card not found"})));
    }
    let old_status = old_status.unwrap();

    // Build dynamic UPDATE
    let mut sets: Vec<String> = Vec::new();
    let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut idx = 1;

    macro_rules! push_field {
        ($field:expr, $val:expr) => {
            if let Some(ref v) = $val {
                sets.push(format!("{} = ?{}", $field, idx));
                values.push(Box::new(v.clone()));
                idx += 1;
            }
        };
    }

    push_field!("title", body.title);
    push_field!("status", body.status);
    push_field!("priority", body.priority);
    push_field!("assigned_agent_id", body.assigned_agent_id);
    push_field!("repo_id", body.repo_id);
    push_field!("github_issue_url", body.github_issue_url);

    if let Some(ref meta) = body.metadata {
        let meta_str = serde_json::to_string(meta).unwrap_or_default();
        sets.push(format!("metadata = ?{}", idx));
        values.push(Box::new(meta_str));
        idx += 1;
    }

    if sets.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "no fields to update"})));
    }

    sets.push(format!("updated_at = datetime('now')"));

    let sql = format!("UPDATE kanban_cards SET {} WHERE id = ?{}", sets.join(", "), idx);
    values.push(Box::new(id.clone()));

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            )
        }
    };

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|v| v.as_ref()).collect();
    match conn.execute(&sql, params_ref.as_slice()) {
        Ok(0) => {
            return (StatusCode::NOT_FOUND, Json(json!({"error": "card not found"})));
        }
        Ok(_) => {}
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    }

    let card = conn.query_row(
        "SELECT id, repo_id, title, status, priority, assigned_agent_id, github_issue_url, github_issue_number, latest_dispatch_id, review_round, metadata, created_at, updated_at FROM kanban_cards WHERE id = ?1",
        [&id],
        |row| card_row_to_json(row),
    );

    let new_status = body.status.clone();
    drop(conn);

    // Fire hooks if status changed
    if let Some(ref new_s) = new_status {
        if new_s != &old_status {
            let _ = state.engine.fire_hook(
                Hook::OnCardTransition,
                json!({
                    "card_id": id,
                    "from": old_status,
                    "to": new_s,
                }),
            );

            // Terminal states
            let terminal = ["done", "failed", "cancelled"];
            if terminal.contains(&new_s.as_str()) {
                let _ = state.engine.fire_hook(
                    Hook::OnCardTerminal,
                    json!({
                        "card_id": id,
                        "status": new_s,
                    }),
                );
            }

            // Auto-create dispatch when transitioning to "requested"
            if new_s == "requested" {
                let (to_agent_id, title) = {
                    let conn = state.db.lock().unwrap();
                    let agent_id: Option<String> = conn
                        .query_row("SELECT assigned_agent_id FROM kanban_cards WHERE id = ?1", [&id], |row| row.get(0))
                        .ok().flatten();
                    let title: String = conn
                        .query_row("SELECT title FROM kanban_cards WHERE id = ?1", [&id], |row| row.get(0))
                        .unwrap_or_else(|_| "Dispatch".to_string());
                    (agent_id, title)
                };
                if let Some(ref agent_id) = to_agent_id {
                    match crate::dispatch::create_dispatch(
                        &state.db, &state.engine, &id, agent_id, "implementation", &title, &json!({}),
                    ) {
                        Ok(_) => {
                            let db = state.db.clone();
                            let agent_id = agent_id.clone();
                            let title = title.clone();
                            let card_id = id.clone();
                            tokio::spawn(async move {
                                super::dispatches::send_dispatch_to_discord(&db, &agent_id, &title, &card_id).await;
                            });
                        }
                        Err(e) => tracing::warn!("[update_card] auto-dispatch creation failed: {e}"),
                    }
                }
            }
        }
    }

    match card {
        Ok(c) => (StatusCode::OK, Json(json!({"card": c}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// POST /api/kanban-cards/:id/assign
pub async fn assign_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<AssignCardBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let old_status: Option<String> = {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                )
            }
        };
        conn.query_row(
            "SELECT status FROM kanban_cards WHERE id = ?1",
            [&id],
            |row| row.get(0),
        )
        .ok()
    };

    if old_status.is_none() {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "card not found"})));
    }
    let old_status = old_status.unwrap();

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            )
        }
    };

    match conn.execute(
        "UPDATE kanban_cards SET assigned_agent_id = ?1, status = 'ready', updated_at = datetime('now') WHERE id = ?2",
        rusqlite::params![body.agent_id, id],
    ) {
        Ok(0) => {
            return (StatusCode::NOT_FOUND, Json(json!({"error": "card not found"})));
        }
        Ok(_) => {}
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    }

    let card = conn.query_row(
        "SELECT id, repo_id, title, status, priority, assigned_agent_id, github_issue_url, github_issue_number, latest_dispatch_id, review_round, metadata, created_at, updated_at FROM kanban_cards WHERE id = ?1",
        [&id],
        |row| card_row_to_json(row),
    );
    drop(conn);

    // Fire transition hook
    if old_status != "ready" {
        let _ = state.engine.fire_hook(
            Hook::OnCardTransition,
            json!({
                "card_id": id,
                "from": old_status,
                "to": "ready",
            }),
        );
    }

    match card {
        Ok(c) => (StatusCode::OK, Json(json!({"card": c}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// DELETE /api/kanban-cards/:id
pub async fn delete_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            )
        }
    };

    match conn.execute("DELETE FROM kanban_cards WHERE id = ?1", [&id]) {
        Ok(0) => (StatusCode::NOT_FOUND, Json(json!({"error": "card not found"}))),
        Ok(_) => (StatusCode::OK, Json(json!({"ok": true}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// POST /api/kanban-cards/:id/retry
pub async fn retry_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<RetryCardBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    // 1. Update assignee if provided
    {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("{e}")}))),
        };

        let exists: bool = conn
            .query_row("SELECT COUNT(*) > 0 FROM kanban_cards WHERE id = ?1", [&id], |row| row.get(0))
            .unwrap_or(false);
        if !exists {
            return (StatusCode::NOT_FOUND, Json(json!({"error": "card not found"})));
        }

        // Reset to 'ready' + optionally update assignee
        if let Some(ref agent_id) = body.assignee_agent_id {
            conn.execute(
                "UPDATE kanban_cards SET status = 'ready', assigned_agent_id = ?1, updated_at = datetime('now') WHERE id = ?2",
                rusqlite::params![agent_id, id],
            ).ok();
        } else {
            conn.execute(
                "UPDATE kanban_cards SET status = 'ready', updated_at = datetime('now') WHERE id = ?1",
                [&id],
            ).ok();
        }
    } // drop conn lock

    // 2. Resolve assignee for dispatch
    let (to_agent_id, title) = {
        let conn = state.db.lock().unwrap();
        let agent_id: Option<String> = conn
            .query_row("SELECT assigned_agent_id FROM kanban_cards WHERE id = ?1", [&id], |row| row.get(0))
            .ok().flatten();
        let title: String = conn
            .query_row("SELECT title FROM kanban_cards WHERE id = ?1", [&id], |row| row.get(0))
            .unwrap_or_else(|_| "Retry".to_string());
        (agent_id, title)
    };

    // 3. Create dispatch and send Discord message (if assignee exists)
    if let Some(ref agent_id) = to_agent_id {
        match crate::dispatch::create_dispatch(
            &state.db, &state.engine, &id, agent_id, "retry", &title, &json!({}),
        ) {
            Ok(_) => {
                let db = state.db.clone();
                let agent_id = agent_id.clone();
                let title = title.clone();
                let card_id = id.clone();
                tokio::spawn(async move {
                    super::dispatches::send_dispatch_to_discord(&db, &agent_id, &title, &card_id).await;
                });
            }
            Err(e) => tracing::warn!("[retry] dispatch creation failed: {e}"),
        }
    }

    // 4. Return updated card
    let conn = state.db.lock().unwrap();
    match conn.query_row(
        "SELECT id, repo_id, title, status, priority, assigned_agent_id, github_issue_url, github_issue_number, latest_dispatch_id, review_round, metadata, created_at, updated_at FROM kanban_cards WHERE id = ?1",
        [&id],
        |row| card_row_to_json(row),
    ) {
        Ok(card) => (StatusCode::OK, Json(json!({"card": card}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("{e}")}))),
    }
}

/// POST /api/kanban-cards/:id/redispatch
pub async fn redispatch_card(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(_body): Json<RedispatchCardBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    // 1. Cancel current dispatch if any, reset card to ready
    let (to_agent_id, title) = {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("{e}")}))),
        };

        // Cancel existing dispatch
        let dispatch_id: Option<String> = conn
            .query_row("SELECT latest_dispatch_id FROM kanban_cards WHERE id = ?1", [&id], |row| row.get(0))
            .ok().flatten();
        if let Some(ref did) = dispatch_id {
            conn.execute(
                "UPDATE task_dispatches SET status = 'cancelled', updated_at = datetime('now') WHERE id = ?1",
                [did],
            ).ok();
        }

        // Reset to ready (keep assignee)
        match conn.execute(
            "UPDATE kanban_cards SET status = 'ready', review_status = NULL, updated_at = datetime('now') WHERE id = ?1",
            [&id],
        ) {
            Ok(0) => return (StatusCode::NOT_FOUND, Json(json!({"error": "card not found"}))),
            Ok(_) => {}
            Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("{e}")}))),
        }

        let agent_id: Option<String> = conn
            .query_row("SELECT assigned_agent_id FROM kanban_cards WHERE id = ?1", [&id], |row| row.get(0))
            .ok().flatten();
        let title: String = conn
            .query_row("SELECT title FROM kanban_cards WHERE id = ?1", [&id], |row| row.get(0))
            .unwrap_or_else(|_| "Redispatch".to_string());
        (agent_id, title)
    };

    // 2. Create new dispatch and send Discord message
    if let Some(ref agent_id) = to_agent_id {
        match crate::dispatch::create_dispatch(
            &state.db, &state.engine, &id, agent_id, "redispatch", &title, &json!({}),
        ) {
            Ok(_) => {
                let db = state.db.clone();
                let agent_id = agent_id.clone();
                let title = title.clone();
                let card_id = id.clone();
                tokio::spawn(async move {
                    super::dispatches::send_dispatch_to_discord(&db, &agent_id, &title, &card_id).await;
                });
            }
            Err(e) => tracing::warn!("[redispatch] dispatch creation failed: {e}"),
        }
    }

    // 3. Return updated card
    let conn = state.db.lock().unwrap();
    match conn.query_row(
        "SELECT id, repo_id, title, status, priority, assigned_agent_id, github_issue_url, github_issue_number, latest_dispatch_id, review_round, metadata, created_at, updated_at FROM kanban_cards WHERE id = ?1",
        [&id],
        |row| card_row_to_json(row),
    ) {
        Ok(card) => (StatusCode::OK, Json(json!({"card": card}))),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": format!("{e}")}))),
    }
}

/// PATCH /api/kanban-cards/:id/defer-dod
pub async fn defer_dod(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<DeferDodBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            )
        }
    };

    // Ensure deferred_dod_json column exists
    let _ = conn.execute_batch(
        "ALTER TABLE kanban_cards ADD COLUMN deferred_dod_json TEXT;"
    );

    // Check card exists
    let exists: bool = conn
        .query_row("SELECT COUNT(*) > 0 FROM kanban_cards WHERE id = ?1", [&id], |row| row.get(0))
        .unwrap_or(false);
    if !exists {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "card not found"})));
    }

    // Read current deferred_dod_json
    let current: Option<String> = conn
        .query_row("SELECT deferred_dod_json FROM kanban_cards WHERE id = ?1", [&id], |row| row.get(0))
        .unwrap_or(None);

    let mut dod: serde_json::Value = current
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({"items": [], "verified": []}));

    // Apply items (replace entire list)
    if let Some(items) = body.items {
        dod["items"] = json!(items);
    }

    // Verify items
    if let Some(verify) = body.verify {
        let verified = dod["verified"].as_array().cloned().unwrap_or_default();
        let mut v_set: Vec<serde_json::Value> = verified;
        for item in verify {
            let val = json!(item);
            if !v_set.contains(&val) {
                v_set.push(val);
            }
        }
        dod["verified"] = json!(v_set);
    }

    // Unverify items
    if let Some(unverify) = body.unverify {
        if let Some(arr) = dod["verified"].as_array() {
            let filtered: Vec<serde_json::Value> = arr
                .iter()
                .filter(|v| {
                    if let Some(s) = v.as_str() {
                        !unverify.contains(&s.to_string())
                    } else {
                        true
                    }
                })
                .cloned()
                .collect();
            dod["verified"] = json!(filtered);
        }
    }

    // Remove items
    if let Some(remove) = body.remove {
        if let Some(arr) = dod["items"].as_array() {
            let filtered: Vec<serde_json::Value> = arr
                .iter()
                .filter(|v| {
                    if let Some(s) = v.as_str() {
                        !remove.contains(&s.to_string())
                    } else {
                        true
                    }
                })
                .cloned()
                .collect();
            dod["items"] = json!(filtered);
        }
        // Also remove from verified
        if let Some(arr) = dod["verified"].as_array() {
            let filtered: Vec<serde_json::Value> = arr
                .iter()
                .filter(|v| {
                    if let Some(s) = v.as_str() {
                        !remove.contains(&s.to_string())
                    } else {
                        true
                    }
                })
                .cloned()
                .collect();
            dod["verified"] = json!(filtered);
        }
    }

    let dod_str = serde_json::to_string(&dod).unwrap_or_default();
    conn.execute(
        "UPDATE kanban_cards SET deferred_dod_json = ?1, updated_at = datetime('now') WHERE id = ?2",
        rusqlite::params![dod_str, id],
    ).ok();

    match conn.query_row(
        "SELECT id, repo_id, title, status, priority, assigned_agent_id, github_issue_url, github_issue_number, latest_dispatch_id, review_round, metadata, created_at, updated_at FROM kanban_cards WHERE id = ?1",
        [&id],
        |row| card_row_to_json(row),
    ) {
        Ok(mut card) => {
            card["deferred_dod"] = dod;
            (StatusCode::OK, Json(json!({"card": card})))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// GET /api/kanban-cards/:id/reviews
pub async fn list_card_reviews(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            )
        }
    };

    let mut stmt = match conn.prepare(
        "SELECT id, kanban_card_id, dispatch_id, item_index, decision, decided_at
         FROM review_decisions
         WHERE kanban_card_id = ?1
         ORDER BY id",
    ) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("prepare: {e}")})),
            )
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

    let reviews: Vec<serde_json::Value> = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    };

    (StatusCode::OK, Json(json!({"reviews": reviews})))
}

/// GET /api/kanban-cards/stalled
pub async fn stalled_cards(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            )
        }
    };

    // Only include registered repos
    let registered_repos: Vec<String> = {
        match conn.prepare("SELECT id FROM github_repos") {
            Ok(mut s) => s
                .query_map([], |row| row.get::<_, String>(0))
                .ok()
                .map(|iter| iter.filter_map(|r| r.ok()).collect())
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    };
    let repo_filter = if registered_repos.is_empty() {
        String::new()
    } else {
        let quoted: Vec<String> = registered_repos.iter().map(|r| format!("'{}'", r.replace('\'', "''"))).collect();
        format!(" AND repo_id IN ({})", quoted.join(","))
    };

    let mut stmt = match conn.prepare(&format!(
        "SELECT id, repo_id, title, status, priority, assigned_agent_id, github_issue_url, github_issue_number, latest_dispatch_id, review_round, metadata, created_at, updated_at
         FROM kanban_cards
         WHERE status = 'in_progress' AND updated_at < datetime('now', '-2 hours'){}
         ORDER BY updated_at ASC", repo_filter),
    ) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("prepare: {e}")})),
            )
        }
    };

    let rows = stmt
        .query_map([], |row| card_row_to_json(row))
        .ok();

    let cards: Vec<serde_json::Value> = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    };

    (StatusCode::OK, Json(json!(cards)))
}

/// POST /api/kanban-cards/bulk-action
pub async fn bulk_action(
    State(state): State<AppState>,
    Json(body): Json<BulkActionBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let target_status = match body.action.as_str() {
        "pass" => "done",
        "reset" => "backlog",
        "cancel" => "cancelled",
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("unknown action: {other}")})),
            );
        }
    };

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            )
        }
    };

    let mut results: Vec<serde_json::Value> = Vec::new();
    for card_id in &body.card_ids {
        match conn.execute(
            "UPDATE kanban_cards SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![target_status, card_id],
        ) {
            Ok(0) => results.push(json!({"id": card_id, "ok": false, "error": "not found"})),
            Ok(_) => results.push(json!({"id": card_id, "ok": true})),
            Err(e) => results.push(json!({"id": card_id, "ok": false, "error": format!("{e}")})),
        }
    }

    (StatusCode::OK, Json(json!({"action": body.action, "results": results})))
}

/// POST /api/kanban-cards/assign-issue
pub async fn assign_issue(
    State(state): State<AppState>,
    Json(body): Json<AssignIssueBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let id = uuid::Uuid::new_v4().to_string();

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            )
        }
    };

    let result = conn.execute(
        "INSERT INTO kanban_cards (id, repo_id, title, status, priority, assigned_agent_id, github_issue_url, github_issue_number, metadata, created_at, updated_at)
         VALUES (?1, ?2, ?3, 'ready', 'medium', ?4, ?5, ?6, ?7, datetime('now'), datetime('now'))",
        rusqlite::params![
            id,
            body.github_repo,
            body.title,
            body.assignee_agent_id,
            body.github_issue_url,
            body.github_issue_number,
            body.description.as_ref().map(|d| json!({"description": d}).to_string()),
        ],
    );

    if let Err(e) = result {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        );
    }

    match conn.query_row(
        "SELECT id, repo_id, title, status, priority, assigned_agent_id, github_issue_url, github_issue_number, latest_dispatch_id, review_round, metadata, created_at, updated_at FROM kanban_cards WHERE id = ?1",
        [&id],
        |row| card_row_to_json(row),
    ) {
        Ok(card) => (StatusCode::CREATED, Json(json!({"card": card}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

// ── Helpers ────────────────────────────────────────────────────

fn card_row_to_json(row: &rusqlite::Row) -> rusqlite::Result<serde_json::Value> {
    let repo_id = row.get::<_, Option<String>>(1)?;
    let assigned_agent_id = row.get::<_, Option<String>>(5)?;
    let metadata_raw = row.get::<_, Option<String>>(10).unwrap_or(None);
    let metadata_parsed = metadata_raw.as_ref().and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok());

    Ok(json!({
        "id": row.get::<_, String>(0)?,
        // existing fields
        "repo_id": repo_id,
        "title": row.get::<_, String>(2)?,
        "status": row.get::<_, String>(3)?,
        "priority": row.get::<_, String>(4)?,
        "assigned_agent_id": assigned_agent_id,
        "github_issue_url": row.get::<_, Option<String>>(6)?,
        "github_issue_number": row.get::<_, Option<i64>>(7)?,
        "latest_dispatch_id": row.get::<_, Option<String>>(8)?,
        "review_round": row.get::<_, i64>(9).unwrap_or(0),
        "metadata": metadata_parsed,
        "created_at": row.get::<_, Option<String>>(11).ok().flatten().or_else(|| row.get::<_, Option<i64>>(11).ok().flatten().map(|v| v.to_string())),
        "updated_at": row.get::<_, Option<String>>(12).ok().flatten().or_else(|| row.get::<_, Option<i64>>(12).ok().flatten().map(|v| v.to_string())),
        // alias fields for frontend compatibility
        "github_repo": repo_id,
        "assignee_agent_id": assigned_agent_id,
        "metadata_json": metadata_raw,
        // additional fields expected by frontend (defaults)
        "description": null,
        "owner_agent_id": null,
        "requester_agent_id": null,
        "parent_card_id": null,
        "sort_order": 0,
        "depth": 0,
        "blocked_reason": null,
        "review_notes": null,
        "pipeline_stage_id": null,
        "review_status": null,
        "started_at": null,
        "requested_at": null,
        "completed_at": null,
        // TODO: JOIN task_dispatches to populate these when latest_dispatch_id is set
        "latest_dispatch_status": null,
        "latest_dispatch_title": null,
        "latest_dispatch_type": null,
        "latest_dispatch_result_summary": null,
        "latest_dispatch_chain_depth": null,
        "child_count": 0,
    }))
}
