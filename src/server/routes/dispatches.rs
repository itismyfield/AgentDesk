use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::dispatch;
use crate::engine::hooks::Hook;

// ── Query / Body types ─────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct ListDispatchesQuery {
    pub status: Option<String>,
    pub kanban_card_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateDispatchBody {
    pub kanban_card_id: String,
    pub to_agent_id: String,
    pub dispatch_type: Option<String>,
    pub title: String,
    pub context: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateDispatchBody {
    pub status: Option<String>,
    pub result: Option<serde_json::Value>,
}

// ── Handlers ───────────────────────────────────────────────────

/// GET /api/dispatches
pub async fn list_dispatches(
    State(state): State<AppState>,
    Query(params): Query<ListDispatchesQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let mut sql = String::from(
        "SELECT id, kanban_card_id, from_agent_id, to_agent_id, dispatch_type, status, title, context, result, parent_dispatch_id, chain_depth, created_at, updated_at FROM task_dispatches WHERE 1=1",
    );
    let mut bind_values: Vec<String> = Vec::new();

    if let Some(ref status) = params.status {
        bind_values.push(status.clone());
        sql.push_str(&format!(" AND status = ?{}", bind_values.len()));
    }
    if let Some(ref card_id) = params.kanban_card_id {
        bind_values.push(card_id.clone());
        sql.push_str(&format!(" AND kanban_card_id = ?{}", bind_values.len()));
    }

    sql.push_str(" ORDER BY created_at DESC");

    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("prepare: {e}")})),
            );
        }
    };

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = bind_values
        .iter()
        .map(|v| v as &dyn rusqlite::types::ToSql)
        .collect();

    let rows = stmt
        .query_map(params_ref.as_slice(), |row| dispatch_row_to_json(row))
        .ok();

    let dispatches: Vec<serde_json::Value> = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    };

    (StatusCode::OK, Json(json!({"dispatches": dispatches})))
}

/// GET /api/dispatches/:id
pub async fn get_dispatch(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    match conn.query_row(
        "SELECT id, kanban_card_id, from_agent_id, to_agent_id, dispatch_type, status, title, context, result, parent_dispatch_id, chain_depth, created_at, updated_at FROM task_dispatches WHERE id = ?1",
        [&id],
        |row| dispatch_row_to_json(row),
    ) {
        Ok(d) => (StatusCode::OK, Json(json!({"dispatch": d}))),
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            (StatusCode::NOT_FOUND, Json(json!({"error": "dispatch not found"})))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// POST /api/dispatches
pub async fn create_dispatch(
    State(state): State<AppState>,
    Json(body): Json<CreateDispatchBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let dispatch_type = body
        .dispatch_type
        .unwrap_or_else(|| "implementation".to_string());
    let context = body.context.unwrap_or(json!({}));

    match dispatch::create_dispatch(
        &state.db,
        &state.engine,
        &body.kanban_card_id,
        &body.to_agent_id,
        &dispatch_type,
        &body.title,
        &context,
    ) {
        Ok(d) => {
            // Send dispatch message to the target agent's Discord channel
            let to_agent_id = body.to_agent_id.clone();
            let title = body.title.clone();
            let card_id = body.kanban_card_id.clone();
            let dispatch_id = d["id"].as_str().unwrap_or("").to_string();
            let db = state.db.clone();
            tokio::spawn(async move {
                send_dispatch_to_discord(&db, &to_agent_id, &title, &card_id, &dispatch_id).await;
            });
            (StatusCode::CREATED, Json(json!({"dispatch": d})))
        }
        Err(e) => {
            let msg = format!("{e}");
            if msg.contains("not found") {
                (StatusCode::NOT_FOUND, Json(json!({"error": msg})))
            } else {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": msg})),
                )
            }
        }
    }
}

/// PATCH /api/dispatches/:id
pub async fn update_dispatch(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateDispatchBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    // If status is "completed", use the dispatch engine's complete_dispatch
    if body.status.as_deref() == Some("completed") {
        let result = body.result.unwrap_or(json!({}));
        match dispatch::complete_dispatch(&state.db, &state.engine, &id, &result) {
            Ok(d) => {
                let db_clone = state.db.clone();
                let dispatch_id = id.clone();
                tokio::spawn(async move {
                    handle_completed_dispatch_followups(&db_clone, &dispatch_id).await;
                });
                return (StatusCode::OK, Json(json!({"dispatch": d})));
            }
            Err(e) => {
                let msg = format!("{e}");
                if msg.contains("not found") {
                    return (StatusCode::NOT_FOUND, Json(json!({"error": msg})));
                }
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": msg})),
                );
            }
        }
    }

    // Generic status update
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let mut sets: Vec<String> = Vec::new();
    let mut values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut idx = 1;

    if let Some(ref status) = body.status {
        sets.push(format!("status = ?{}", idx));
        values.push(Box::new(status.clone()));
        idx += 1;
    }

    if let Some(ref result) = body.result {
        let result_str = serde_json::to_string(result).unwrap_or_default();
        sets.push(format!("result = ?{}", idx));
        values.push(Box::new(result_str));
        idx += 1;
    }

    if sets.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no fields to update"})),
        );
    }

    sets.push("updated_at = datetime('now')".to_string());

    let sql = format!(
        "UPDATE task_dispatches SET {} WHERE id = ?{}",
        sets.join(", "),
        idx
    );
    values.push(Box::new(id.clone()));

    let params_ref: Vec<&dyn rusqlite::types::ToSql> = values.iter().map(|v| v.as_ref()).collect();
    match conn.execute(&sql, params_ref.as_slice()) {
        Ok(0) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "dispatch not found"})),
            );
        }
        Ok(_) => {}
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    }

    // If the new status is "completed" (edge case: should have been caught above), fire hook
    if body.status.as_deref() == Some("completed") {
        let kanban_card_id: Option<String> = conn
            .query_row(
                "SELECT kanban_card_id FROM task_dispatches WHERE id = ?1",
                [&id],
                |row| row.get(0),
            )
            .ok();
        drop(conn);

        let _ = state.engine.fire_hook(
            Hook::OnDispatchCompleted,
            json!({
                "dispatch_id": id,
                "kanban_card_id": kanban_card_id,
            }),
        );
    } else {
        drop(conn);
    }

    // Read back
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    match conn.query_row(
        "SELECT id, kanban_card_id, from_agent_id, to_agent_id, dispatch_type, status, title, context, result, parent_dispatch_id, chain_depth, created_at, updated_at FROM task_dispatches WHERE id = ?1",
        [&id],
        |row| dispatch_row_to_json(row),
    ) {
        Ok(d) => (StatusCode::OK, Json(json!({"dispatch": d}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

// ── Helpers ────────────────────────────────────────────────────

fn dispatch_row_to_json(row: &rusqlite::Row) -> rusqlite::Result<serde_json::Value> {
    let status = row.get::<_, String>(5)?;
    let created_at = row.get::<_, Option<String>>(11).ok().flatten().or_else(|| {
        row.get::<_, Option<i64>>(11)
            .ok()
            .flatten()
            .map(|v| v.to_string())
    });
    let updated_at = row.get::<_, Option<String>>(12).ok().flatten().or_else(|| {
        row.get::<_, Option<i64>>(12)
            .ok()
            .flatten()
            .map(|v| v.to_string())
    });
    let completed_at = if status == "completed" {
        updated_at.clone()
    } else {
        None
    };
    Ok(json!({
        "id": row.get::<_, String>(0)?,
        "kanban_card_id": row.get::<_, Option<String>>(1)?,
        "from_agent_id": row.get::<_, Option<String>>(2)?,
        "to_agent_id": row.get::<_, Option<String>>(3)?,
        "dispatch_type": row.get::<_, Option<String>>(4)?,
        "status": status,
        "title": row.get::<_, Option<String>>(6)?,
        "context": row.get::<_, Option<String>>(7)?.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()),
        "result": row.get::<_, Option<String>>(8)?.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok()),
        "context_file": serde_json::Value::Null,
        "result_file": serde_json::Value::Null,
        "result_summary": serde_json::Value::Null,
        "parent_dispatch_id": row.get::<_, Option<String>>(9)?,
        "chain_depth": row.get::<_, i64>(10).unwrap_or(0),
        "created_at": created_at,
        "dispatched_at": row.get::<_, Option<String>>(11).ok().flatten().or_else(|| row.get::<_, Option<i64>>(11).ok().flatten().map(|v| v.to_string())),
        "updated_at": updated_at,
        "completed_at": completed_at,
    }))
}

/// Send a dispatch notification to the target agent's Discord channel.
/// Message format: `DISPATCH:<dispatch_id> - <title>\n<issue_url>`
/// The `DISPATCH:<uuid>` prefix is required for the dcserver to link the
/// resulting Claude session back to the kanban card (via `parse_dispatch_id`).
pub(super) async fn send_dispatch_to_discord(
    db: &crate::db::Db,
    agent_id: &str,
    title: &str,
    card_id: &str,
    dispatch_id: &str,
) {
    // Determine dispatch type to choose the right channel
    let dispatch_type: Option<String> = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        conn.query_row(
            "SELECT dispatch_type FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get(0),
        )
        .ok()
        .flatten()
    };

    // For review dispatches, use the alternate channel (counter-model)
    let use_alt = use_counter_model_channel(dispatch_type.as_deref());

    // Look up agent's discord channel
    let channel_id: Option<String> = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        let col = if use_alt {
            "discord_channel_alt"
        } else {
            "discord_channel_id"
        };
        conn.query_row(
            &format!("SELECT {col} FROM agents WHERE id = ?1"),
            [agent_id],
            |row| row.get(0),
        )
        .ok()
    };

    let channel_id = match channel_id {
        Some(id) if !id.is_empty() => id,
        _ => {
            tracing::warn!(
                "[dispatch] No discord_channel_id for agent {agent_id}, skipping message"
            );
            return;
        }
    };

    // Parse channel ID as u64, or resolve alias via role_map.json
    let channel_id_num: u64 = match channel_id.parse() {
        Ok(n) => n,
        Err(_) => {
            // Try resolving channel name alias from role_map.json
            match resolve_channel_alias(&channel_id) {
                Some(n) => n,
                None => {
                    tracing::warn!(
                        "[dispatch] Cannot resolve channel '{channel_id}' for agent {agent_id}"
                    );
                    return;
                }
            }
        }
    };

    // Look up the issue URL and number for context
    let (issue_url, issue_number): (Option<String>, Option<i64>) = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        conn.query_row(
            "SELECT github_issue_url, github_issue_number FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap_or_default()
    };

    let message = format_dispatch_message(
        dispatch_id,
        title,
        issue_url.as_deref(),
        issue_number,
        use_alt,
    );

    // Send via Discord HTTP API using the announce bot
    let config = crate::config::load_graceful();
    let token = match config
        .discord
        .bots
        .get("announce")
        .or_else(|| config.discord.bots.get("command"))
    {
        Some(bot) => bot.token.clone(),
        None => {
            tracing::warn!("[dispatch] No 'announce' bot configured");
            return;
        }
    };

    // Use reqwest to send directly via Discord REST API
    let url = format!(
        "https://discord.com/api/v10/channels/{}/messages",
        channel_id_num
    );
    let client = reqwest::Client::new();
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bot {}", token))
        .json(&serde_json::json!({"content": message}))
        .send()
        .await;

    match resp {
        Ok(resp) if resp.status().is_success() => {
            // Try to create a thread from the dispatch message
            if let Ok(body) = resp.json::<serde_json::Value>().await {
                let message_id = body.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if !message_id.is_empty() {
                    // Create thread from this message
                    let thread_name = if let Some(num) = issue_number {
                        format!("#{} {}", num, &title[..title.len().min(90)])
                    } else {
                        title[..title.len().min(100)].to_string()
                    };

                    let thread_url = format!(
                        "https://discord.com/api/v10/channels/{}/messages/{}/threads",
                        channel_id_num, message_id
                    );
                    let thread_resp = client
                        .post(&thread_url)
                        .header("Authorization", format!("Bot {}", token))
                        .json(&serde_json::json!({
                            "name": thread_name,
                            "auto_archive_duration": 1440, // 24h
                        }))
                        .send()
                        .await;

                    if let Ok(tr) = thread_resp {
                        if tr.status().is_success() {
                            if let Ok(thread_body) = tr.json::<serde_json::Value>().await {
                                let thread_id = thread_body.get("id").and_then(|v| v.as_str()).unwrap_or("");
                                if !thread_id.is_empty() {
                                    // Save thread_id to dispatch
                                    if let Ok(conn) = db.lock() {
                                        conn.execute(
                                            "UPDATE task_dispatches SET context = json_set(COALESCE(context, '{}'), '$.thread_id', ?1) WHERE id = ?2",
                                            rusqlite::params![thread_id, dispatch_id],
                                        ).ok();
                                    }
                                    tracing::info!(
                                        "[dispatch] Created thread {} for dispatch {dispatch_id}",
                                        thread_id
                                    );
                                }
                            }
                        } else {
                            tracing::warn!("[dispatch] Thread creation failed: {}", tr.status());
                        }
                    }
                }
            }
            tracing::info!("[dispatch] Sent message to {agent_id} (channel {channel_id})");
        }
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!("[dispatch] Discord API error {status}: {body}");
        }
        Err(e) => {
            tracing::warn!("[dispatch] Request failed: {e}");
        }
    }
}

/// Send review result notification to the agent's PRIMARY channel.
/// Called after a counter-model review dispatch completes.
pub(super) async fn send_review_result_to_primary(
    db: &crate::db::Db,
    card_id: &str,
    verdict: &str,
) {
    // Look up card info
    let (agent_id, title, issue_url, channel_id): (String, String, Option<String>, String) = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        let result = conn.query_row(
            "SELECT kc.assigned_agent_id, kc.title, kc.github_issue_url, a.discord_channel_id \
             FROM kanban_cards kc \
             JOIN agents a ON kc.assigned_agent_id = a.id \
             WHERE kc.id = ?1",
            [card_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        );
        match result {
            Ok(r) => r,
            Err(_) => return,
        }
    };

    // Resolve channel ID (may be a name alias)
    let channel_id_num: u64 = match channel_id.parse() {
        Ok(n) => n,
        Err(_) => match resolve_channel_alias(&channel_id) {
            Some(n) => n,
            None => return,
        },
    };

    // For pass verdict, just send a simple notification (no action needed)
    if verdict == "pass" || verdict == "accept" || verdict == "approved" {
        let url_line = issue_url.map(|u| format!("\n{u}")).unwrap_or_default();
        let message = format!("✅ [리뷰 통과] {title} — done으로 이동{url_line}");

        let config = crate::config::load_graceful();
        let token = match config
            .discord
            .bots
            .get("announce")
            .or_else(|| config.discord.bots.get("command"))
        {
            Some(bot) => bot.token.clone(),
            None => return,
        };
        let url = format!(
            "https://discord.com/api/v10/channels/{}/messages",
            channel_id_num
        );
        let client = reqwest::Client::new();
        let _ = client
            .post(&url)
            .header("Authorization", format!("Bot {}", token))
            .json(&serde_json::json!({"content": message}))
            .send()
            .await;
        return;
    }

    // For improve/rework/reject: create a review-decision dispatch to the original agent
    // This triggers a turn where the agent reads review comments and decides action
    let db_ref = db;
    let dispatch_id = uuid::Uuid::new_v4().to_string();
    {
        let conn = match db_ref.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        conn.execute(
            "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, context, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'review-decision', 'pending', ?4, ?5, datetime('now'), datetime('now'))",
            rusqlite::params![
                dispatch_id,
                card_id,
                &agent_id,
                format!("[리뷰 검토] {title}"),
                serde_json::json!({"verdict": verdict}).to_string(),
            ],
        ).ok();
        conn.execute(
            "UPDATE kanban_cards SET latest_dispatch_id = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![dispatch_id, card_id],
        ).ok();
    }

    let url_line = issue_url.map(|u| format!("\n{u}")).unwrap_or_default();
    let message = format!(
        "DISPATCH:{dispatch_id} - [리뷰 검토] {title}\n\
         📝 카운터모델 리뷰 결과: **{verdict}**\n\
         GitHub 이슈 코멘트를 확인하고 다음 중 하나를 선택하세요:\n\
         • 수용 → 리뷰 반영 수정 후 review-decision API에 accept 호출\n\
         • 반론 → GitHub 코멘트로 이의 제기 후 review-decision API에 dispute 호출\n\
         • 불수용 → review-decision API에 dismiss 호출{url_line}"
    );

    // Send to primary channel
    let config = crate::config::load_graceful();
    let token = match config
        .discord
        .bots
        .get("announce")
        .or_else(|| config.discord.bots.get("command"))
    {
        Some(bot) => bot.token.clone(),
        None => return,
    };

    let url = format!(
        "https://discord.com/api/v10/channels/{}/messages",
        channel_id_num
    );
    let client = reqwest::Client::new();
    match client
        .post(&url)
        .header("Authorization", format!("Bot {}", token))
        .json(&serde_json::json!({"content": message}))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            tracing::info!("[review] Sent review result to {agent_id} (channel {channel_id})");
        }
        Ok(resp) => {
            let status = resp.status();
            tracing::warn!("[review] Discord API error {status}");
        }
        Err(e) => {
            tracing::warn!("[review] Request failed: {e}");
        }
    }
}

fn extract_review_verdict(result_json: Option<&str>) -> String {
    result_json
        .and_then(|r| serde_json::from_str::<serde_json::Value>(r).ok())
        .and_then(|v| {
            v.get("verdict")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    v.get("decision")
                        .and_then(|s| s.as_str())
                        .map(|s| s.to_string())
                })
        })
        .unwrap_or_else(|| "pass".to_string())
}

pub(super) async fn handle_completed_dispatch_followups(db: &crate::db::Db, dispatch_id: &str) {
    let info: Option<(String, String, String, String, String, Option<String>)> = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        conn.query_row(
            "SELECT td.dispatch_type, td.status, kc.id, COALESCE(kc.assigned_agent_id, ''), kc.title, td.result \
             FROM task_dispatches td \
             JOIN kanban_cards kc ON kc.id = td.kanban_card_id \
             WHERE td.id = ?1",
            [dispatch_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .ok()
    };

    let Some((dispatch_type, status, card_id, agent_id, title, result_json)) = info else {
        return;
    };
    if status != "completed" {
        return;
    }

    if dispatch_type == "review" {
        let verdict = extract_review_verdict(result_json.as_deref());
        send_review_result_to_primary(db, &card_id, &verdict).await;
    }

    let latest_dispatch_id: Option<String> = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        conn.query_row(
            "SELECT latest_dispatch_id FROM kanban_cards WHERE id = ?1",
            [&card_id],
            |row| row.get(0),
        )
        .ok()
        .flatten()
    };

    if let Some(new_dispatch_id) = latest_dispatch_id {
        if new_dispatch_id != dispatch_id && !agent_id.is_empty() {
            send_dispatch_to_discord(db, &agent_id, &title, &card_id, &new_dispatch_id).await;
        }
    }
}

/// Resolve a channel name alias (e.g. "adk-cc") to a numeric channel ID
/// by reading role_map.json's byChannelName section.
pub fn resolve_channel_alias_pub(alias: &str) -> Option<u64> {
    resolve_channel_alias(alias)
}

fn use_counter_model_channel(dispatch_type: Option<&str>) -> bool {
    matches!(dispatch_type, Some("review") | Some("review-decision"))
}

fn format_dispatch_message(
    dispatch_id: &str,
    title: &str,
    issue_url: Option<&str>,
    issue_number: Option<i64>,
    use_alt: bool,
) -> String {
    // Format issue link as markdown hyperlink with angle brackets to suppress embed
    let issue_link = match (issue_url, issue_number) {
        (Some(url), Some(num)) => format!("[{title} #{num}](<{url}>)"),
        (Some(url), None) => format!("[{title}](<{url}>)"),
        _ => String::new(),
    };

    if use_alt {
        let mut message = format!(
            "DISPATCH:{dispatch_id} - {title}\n\
             ⚠️ 검토 전용 — 작업 착수 금지\n\
             코드 리뷰만 수행하고 GitHub 이슈에 코멘트로 피드백해주세요."
        );
        if !issue_link.is_empty() {
            message.push('\n');
            message.push_str(&issue_link);
        }
        message
    } else if !issue_link.is_empty() {
        format!("DISPATCH:{dispatch_id} - {title}\n{issue_link}")
    } else {
        format!("DISPATCH:{dispatch_id} - {title}")
    }
}

fn resolve_channel_alias(alias: &str) -> Option<u64> {
    let root = crate::cli::agentdesk_runtime_root()?;
    let path = root.join("config/role_map.json");
    let content = std::fs::read_to_string(&path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;

    // Strategy 1: Direct lookup in byChannelName → channelId field
    let by_name = json.get("byChannelName")?.as_object()?;
    if let Some(entry) = by_name.get(alias) {
        // If byChannelName entry has a channelId field, use it directly (most reliable)
        if let Some(id) = entry.get("channelId").and_then(|v| v.as_str()) {
            return id.parse().ok();
        }
        if let Some(id) = entry.get("channelId").and_then(|v| v.as_u64()) {
            return Some(id);
        }
    }

    // Strategy 2: Search byChannelId for entries whose channel name matches the alias
    // Each byChannelId entry may have been registered with a channel name
    let by_id = json.get("byChannelId")?.as_object()?;
    for (ch_id, ch_entry) in by_id {
        // Check if this entry's associated channel name matches our alias
        if let Some(ch_name) = ch_entry.get("channelName").and_then(|v| v.as_str()) {
            if ch_name == alias {
                return ch_id.parse().ok();
            }
        }
    }

    // Strategy 3: Fallback — roleId matching (original approach)
    if let Some(entry) = by_name.get(alias) {
        let role_id = entry.get("roleId").and_then(|v| v.as_str())?;
        let provider = entry.get("provider").and_then(|v| v.as_str());
        for (ch_id, ch_entry) in by_id {
            let entry_role = ch_entry.get("roleId").and_then(|v| v.as_str());
            let entry_provider = ch_entry.get("provider").and_then(|v| v.as_str());
            if entry_role == Some(role_id) {
                // If both have provider, must match. If either is missing, accept the match.
                if let (Some(p1), Some(p2)) = (provider, entry_provider) {
                    if p1 == p2 {
                        return ch_id.parse().ok();
                    }
                } else {
                    return ch_id.parse().ok();
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::{
        extract_review_verdict, format_dispatch_message, handle_completed_dispatch_followups,
        use_counter_model_channel,
    };
    use crate::db::Db;
    use std::sync::{Arc, Mutex};

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        Arc::new(Mutex::new(conn))
    }

    #[test]
    fn review_dispatch_uses_counter_model_channel() {
        assert!(use_counter_model_channel(Some("review")));
        assert!(use_counter_model_channel(Some("review-decision")));
        assert!(!use_counter_model_channel(Some("implementation")));
        assert!(!use_counter_model_channel(Some("rework")));
        assert!(!use_counter_model_channel(None));
    }

    #[test]
    fn review_dispatch_message_includes_review_only_banner() {
        let message = format_dispatch_message(
            "dispatch-1",
            "[Review R1] card-1",
            Some("https://github.com/itismyfield/AgentDesk/issues/19"),
            Some(19),
            true,
        );

        assert!(message.starts_with("DISPATCH:dispatch-1 - [Review R1] card-1"));
        assert!(message.contains("⚠️ 검토 전용"));
        assert!(message.contains("코드 리뷰만 수행하고 GitHub 이슈에 코멘트로 피드백해주세요."));
        assert!(message.contains(
            "[Review R1] card-1 #19](<https://github.com/itismyfield/AgentDesk/issues/19>)"
        ));
    }

    #[test]
    fn implementation_dispatch_message_stays_compact() {
        let message = format_dispatch_message(
            "dispatch-2",
            "Implement feature",
            Some("https://github.com/itismyfield/AgentDesk/issues/24"),
            Some(24),
            false,
        );

        assert!(message.contains(
            "[Implement feature #24](<https://github.com/itismyfield/AgentDesk/issues/24>)"
        ));
        assert!(!message.contains("검토 전용"));
    }

    #[test]
    fn review_verdict_extraction_defaults_to_pass() {
        assert_eq!(extract_review_verdict(None), "pass");
        assert_eq!(
            extract_review_verdict(Some(r#"{"decision":"dismiss"}"#)),
            "dismiss"
        );
        assert_eq!(
            extract_review_verdict(Some(r#"{"verdict":"improve"}"#)),
            "improve"
        );
    }

    #[tokio::test]
    async fn completed_review_dispatch_creates_review_decision_followup() {
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt) VALUES ('agent-1', 'Agent 1', '123', '456')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, latest_dispatch_id, created_at, updated_at)
                 VALUES ('card-1', 'Needs follow-up', 'review', 'agent-1', 'dispatch-review', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO task_dispatches (id, kanban_card_id, to_agent_id, dispatch_type, status, title, result, created_at, updated_at)
                 VALUES ('dispatch-review', 'card-1', 'agent-1', 'review', 'completed', '[Review R1] card-1', '{\"verdict\":\"improve\"}', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
        }

        handle_completed_dispatch_followups(&db, "dispatch-review").await;

        let conn = db.lock().unwrap();
        let latest_dispatch_id: String = conn
            .query_row(
                "SELECT latest_dispatch_id FROM kanban_cards WHERE id = 'card-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let followup: (String, String, String) = conn
            .query_row(
                "SELECT dispatch_type, status, context FROM task_dispatches WHERE id = ?1",
                [&latest_dispatch_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();

        assert_ne!(latest_dispatch_id, "dispatch-review");
        assert_eq!(followup.0, "review-decision");
        assert_eq!(followup.1, "pending");
        assert!(followup.2.contains("\"verdict\":\"improve\""));
    }
}
