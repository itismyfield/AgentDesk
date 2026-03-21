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
                // Check if OnDispatchCompleted → OnReviewEnter created a new dispatch
                // (e.g., counter-model review). If so, send async Discord notification.
                let db_clone = state.db.clone();
                let dispatch_id = id.clone();
                tokio::spawn(async move {
                    // Get the card associated with this dispatch
                    let info: Option<(String, String, String, String)> = {
                        let conn = match db_clone.lock() {
                            Ok(c) => c,
                            Err(_) => return,
                        };
                        conn.query_row(
                            "SELECT kc.id, kc.assigned_agent_id, kc.title, kc.latest_dispatch_id \
                             FROM kanban_cards kc \
                             JOIN task_dispatches td ON td.kanban_card_id = kc.id \
                             WHERE td.id = ?1",
                            [&dispatch_id],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                        )
                        .ok()
                    };
                    if let Some((card_id, agent_id, title, new_dispatch_id)) = info {
                        // Only send if a NEW dispatch was created (different from completed one)
                        if new_dispatch_id != dispatch_id {
                            send_dispatch_to_discord(
                                &db_clone, &agent_id, &title, &card_id, &new_dispatch_id,
                            )
                            .await;
                        }
                    }
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

    // Look up the issue URL for context
    let issue_url: Option<String> = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        conn.query_row(
            "SELECT github_issue_url FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| row.get(0),
        )
        .ok()
        .flatten()
    };

    let message = format_dispatch_message(dispatch_id, title, issue_url.as_deref(), use_alt);

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
    match client
        .post(&url)
        .header("Authorization", format!("Bot {}", token))
        .json(&serde_json::json!({"content": message}))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
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
    use_alt: bool,
) -> String {
    if use_alt {
        let mut message = format!(
            "DISPATCH:{dispatch_id} - {title}\n\
             ⚠️ 검토 전용 — 작업 착수 금지\n\
             코드 리뷰만 수행하고 GitHub 이슈에 코멘트로 피드백해주세요."
        );
        if let Some(url) = issue_url {
            message.push('\n');
            message.push_str(url);
        }
        message
    } else if let Some(url) = issue_url {
        format!("DISPATCH:{dispatch_id} - {title}\n{url}")
    } else {
        format!("DISPATCH:{dispatch_id} - {title}")
    }
}

fn resolve_channel_alias(alias: &str) -> Option<u64> {
    let root = crate::cli::agentdesk_runtime_root()?;
    let path = root.join("config/role_map.json");
    let content = std::fs::read_to_string(&path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;

    // role_map.json has byChannelId: { "123456": { roleId, ... } }
    // and byChannelName: { "adk-cc": { roleId, ... } }
    // We need to find the numeric ID that maps to the same role as the alias
    let by_name = json.get("byChannelName")?.as_object()?;
    let entry = by_name.get(alias)?;
    let role_id = entry.get("roleId")?.as_str()?;

    // Now find the numeric channel ID with the same roleId and matching provider
    let provider = entry
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| {
            // Infer provider from channel name convention: *-cdx = codex, *-cc = claude
            if alias.ends_with("-cdx") {
                "codex"
            } else {
                "claude"
            }
        });
    let by_id = json.get("byChannelId")?.as_object()?;
    for (ch_id, ch_entry) in by_id {
        if ch_entry.get("roleId").and_then(|v| v.as_str()) == Some(role_id)
            && ch_entry.get("provider").and_then(|v| v.as_str()) == Some(provider)
        {
            return ch_id.parse().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{format_dispatch_message, use_counter_model_channel};

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
            true,
        );

        assert!(message.starts_with("DISPATCH:dispatch-1 - [Review R1] card-1"));
        assert!(message.contains("⚠️ 검토 전용"));
        assert!(message.contains("코드 리뷰만 수행하고 GitHub 이슈에 코멘트로 피드백해주세요."));
        assert!(message.ends_with("https://github.com/itismyfield/AgentDesk/issues/19"));
    }

    #[test]
    fn implementation_dispatch_message_stays_compact() {
        let message = format_dispatch_message(
            "dispatch-2",
            "Implement feature",
            Some("https://github.com/itismyfield/AgentDesk/issues/24"),
            false,
        );

        assert_eq!(
            message,
            "DISPATCH:dispatch-2 - Implement feature\nhttps://github.com/itismyfield/AgentDesk/issues/24"
        );
        assert!(!message.contains("검토 전용"));
    }
}
