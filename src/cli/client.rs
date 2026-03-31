//! CLI client subcommands that call the AgentDesk HTTP API.

use crate::config;
use serde_json::Value;
use std::collections::BTreeMap;

/// Resolve the API base URL from config or environment.
pub fn api_base() -> String {
    if let Ok(url) = std::env::var("AGENTDESK_API_URL") {
        return url.trim_end_matches('/').to_string();
    }
    let cfg = config::load_graceful();
    cfg.server.local_base_url()
}

/// Build a ureq agent (shared across calls).
fn agent() -> ureq::Agent {
    ureq::Agent::new()
}

/// Get the auth token from config.
fn auth_token() -> Option<String> {
    let cfg = config::load_graceful();
    cfg.server.auth_token.clone()
}

fn print_json(value: &Value) {
    println!("{}", serde_json::to_string_pretty(value).unwrap());
}

fn request_json(method: &str, path: &str, body: Option<&str>) -> Result<Value, String> {
    let url = if path.starts_with('/') {
        format!("{}{}", api_base(), path)
    } else {
        format!("{}/{}", api_base(), path)
    };

    let a = agent();
    let mut req = match method.to_uppercase().as_str() {
        "GET" => a.get(&url),
        "POST" => a.post(&url),
        "PATCH" => a.patch(&url),
        "PUT" => a.put(&url),
        "DELETE" => a.delete(&url),
        other => return Err(format!("Unsupported method: {other}")),
    };
    if let Some(token) = auth_token() {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }

    let method_upper = method.to_ascii_uppercase();
    let resp = if let Some(b) = body {
        req.set("Content-Type", "application/json")
            .send_string(b)
            .map_err(|e| format!("Request failed: {e}"))?
    } else if matches!(method_upper.as_str(), "POST" | "PATCH" | "PUT") {
        req.set("Content-Type", "application/json")
            .send_string("{}")
            .map_err(|e| format!("Request failed: {e}"))?
    } else {
        req.call().map_err(|e| format!("Request failed: {e}"))?
    };

    resp.into_json().map_err(|e| format!("Parse error: {e}"))
}

pub(crate) fn get_json(path: &str) -> Result<Value, String> {
    request_json("GET", path, None)
}

fn post_json(path: &str, body: Option<Value>) -> Result<Value, String> {
    let body_string = body.map(|value| value.to_string());
    request_json("POST", path, body_string.as_deref())
}

fn api_call(method: &str, path: &str, body: Option<&str>) -> Result<Value, String> {
    request_json(method, path, body)
}

fn truncate_cell(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let len = value.chars().count();
    if len <= width {
        return value.to_string();
    }
    if width == 1 {
        return "…".to_string();
    }
    let mut out = value.chars().take(width - 1).collect::<String>();
    out.push('…');
    out
}

fn pad_cell(value: &str, width: usize) -> String {
    let rendered = truncate_cell(value, width);
    let pad = width.saturating_sub(rendered.chars().count());
    format!("{rendered}{}", " ".repeat(pad))
}

fn runtime_config_payload(value: Value) -> Result<Value, String> {
    let normalized = match value.get("current") {
        Some(current) if current.is_object() => current.clone(),
        Some(_) => return Err("runtime config `current` must be a JSON object".to_string()),
        None => value,
    };
    if normalized.is_object() {
        Ok(normalized)
    } else {
        Err("runtime config must be a JSON object".to_string())
    }
}

fn summarize_discord_health(health: &Value) -> String {
    if let Some(providers) = health.get("providers").and_then(Value::as_array) {
        let total = providers.len();
        let connected: Vec<String> = providers
            .iter()
            .filter(|provider| provider.get("connected").and_then(Value::as_bool) == Some(true))
            .filter_map(|provider| provider.get("name").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        let disconnected: Vec<String> = providers
            .iter()
            .filter(|provider| provider.get("connected").and_then(Value::as_bool) != Some(true))
            .filter_map(|provider| provider.get("name").and_then(Value::as_str))
            .map(str::to_string)
            .collect();
        if total == 0 {
            return "no providers registered".to_string();
        }
        if connected.len() == total {
            return format!(
                "{}/{} connected ({})",
                connected.len(),
                total,
                connected.join(", ")
            );
        }
        if disconnected.is_empty() {
            return format!("{}/{} connected", connected.len(), total);
        }
        format!(
            "{}/{} connected, offline: {}",
            connected.len(),
            total,
            disconnected.join(", ")
        )
    } else {
        "standalone health only (no Discord provider data)".to_string()
    }
}

fn render_cards_table(cards: &[Value]) -> String {
    let rows: Vec<[String; 5]> = cards
        .iter()
        .map(|card| {
            let issue = card
                .get("github_issue_number")
                .and_then(Value::as_i64)
                .map(|number| format!("#{number}"))
                .or_else(|| {
                    card.get("id").and_then(Value::as_str).map(|id| {
                        let short = id.chars().take(8).collect::<String>();
                        format!("id:{short}")
                    })
                })
                .unwrap_or_else(|| "-".to_string());
            let status = match (
                card.get("status").and_then(Value::as_str),
                card.get("review_status").and_then(Value::as_str),
            ) {
                (Some(status), Some(review)) if !review.is_empty() => format!("{status}/{review}"),
                (Some(status), _) => status.to_string(),
                _ => "-".to_string(),
            };
            let priority = card
                .get("priority")
                .and_then(Value::as_str)
                .unwrap_or("-")
                .to_string();
            let agent = card
                .get("assigned_agent_id")
                .and_then(Value::as_str)
                .or_else(|| card.get("assignee_agent_id").and_then(Value::as_str))
                .unwrap_or("-")
                .to_string();
            let title = card
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("-")
                .to_string();
            [issue, status, priority, agent, title]
        })
        .collect();

    let issue_w = rows
        .iter()
        .map(|row| row[0].chars().count())
        .max()
        .unwrap_or(5)
        .clamp(5, 10);
    let status_w = rows
        .iter()
        .map(|row| row[1].chars().count())
        .max()
        .unwrap_or(6)
        .clamp(6, 20);
    let priority_w = rows
        .iter()
        .map(|row| row[2].chars().count())
        .max()
        .unwrap_or(8)
        .clamp(8, 10);
    let agent_w = rows
        .iter()
        .map(|row| row[3].chars().count())
        .max()
        .unwrap_or(5)
        .clamp(5, 20);
    let title_w = 80;

    let mut lines = Vec::new();
    lines.push(format!(
        "{}  {}  {}  {}  {}",
        pad_cell("ISSUE", issue_w),
        pad_cell("STATUS", status_w),
        pad_cell("PRIORITY", priority_w),
        pad_cell("AGENT", agent_w),
        pad_cell("TITLE", title_w),
    ));
    lines.push(format!(
        "{}  {}  {}  {}  {}",
        "-".repeat(issue_w),
        "-".repeat(status_w),
        "-".repeat(priority_w),
        "-".repeat(agent_w),
        "-".repeat(title_w),
    ));
    for row in rows {
        lines.push(format!(
            "{}  {}  {}  {}  {}",
            pad_cell(&row[0], issue_w),
            pad_cell(&row[1], status_w),
            pad_cell(&row[2], priority_w),
            pad_cell(&row[3], agent_w),
            pad_cell(&row[4], title_w),
        ));
    }
    lines.join("\n")
}

// ── Subcommand handlers ──────────────────────────────────────

/// `agentdesk status` — server health + auto-queue status
pub fn cmd_status() -> Result<(), String> {
    let health = get_json("/api/health")?;
    let sessions = get_json("/api/dispatched-sessions?include_merged=1")?;
    let queue = get_json("/api/auto-queue/status")?;

    let version = health
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let health_status = health
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            let ok = health.get("ok").and_then(Value::as_bool).unwrap_or(false);
            let db = health.get("db").and_then(Value::as_bool).unwrap_or(false);
            Some(if ok && db { "healthy" } else { "degraded" }.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());
    let sessions_list = sessions
        .get("sessions")
        .and_then(Value::as_array)
        .ok_or_else(|| "invalid /api/dispatched-sessions response".to_string())?;
    let total_sessions = sessions_list.len();
    let working_sessions = sessions_list
        .iter()
        .filter(|session| session.get("status").and_then(Value::as_str) == Some("working"))
        .count();
    let active_dispatch_sessions = sessions_list
        .iter()
        .filter(|session| {
            !session
                .get("active_dispatch_id")
                .unwrap_or(&Value::Null)
                .is_null()
        })
        .count();
    let queue_entries = queue
        .get("entries")
        .and_then(Value::as_array)
        .ok_or_else(|| "invalid /api/auto-queue/status response".to_string())?;
    let mut counts = BTreeMap::<String, usize>::new();
    for entry in queue_entries {
        let status = entry
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        *counts.entry(status).or_default() += 1;
    }
    let queue_run = queue.get("run").and_then(Value::as_object);
    let queue_summary = if let Some(run) = queue_run {
        format!(
            "{} for {}",
            run.get("status")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            run.get("agent_id").and_then(Value::as_str).unwrap_or("-")
        )
    } else {
        "idle".to_string()
    };

    println!("AgentDesk Status");
    println!("  Base URL: {}", api_base());
    println!("  Server: {} (v{})", health_status, version);
    println!("  Discord: {}", summarize_discord_health(&health));
    println!(
        "  Sessions: {} total, {} working, {} with active dispatch",
        total_sessions, working_sessions, active_dispatch_sessions
    );
    println!(
        "  Auto-Queue: {} | total={} pending={} dispatched={} done={} skipped={}",
        queue_summary,
        queue_entries.len(),
        counts.get("pending").copied().unwrap_or(0),
        counts.get("dispatched").copied().unwrap_or(0),
        counts.get("done").copied().unwrap_or(0),
        counts.get("skipped").copied().unwrap_or(0),
    );
    Ok(())
}

/// `agentdesk cards [--status <STATUS>]`
pub fn cmd_cards(status: Option<&str>) -> Result<(), String> {
    let path = match status {
        Some(s) => format!("/api/kanban-cards?status={s}"),
        None => "/api/kanban-cards".to_string(),
    };
    let value = get_json(&path)?;
    let cards = value
        .get("cards")
        .and_then(Value::as_array)
        .ok_or_else(|| "invalid /api/kanban-cards response".to_string())?;
    if cards.is_empty() {
        println!("No cards found.");
    } else {
        println!("{}", render_cards_table(cards));
    }
    Ok(())
}

/// `agentdesk dispatch list`
pub fn cmd_dispatch_list() -> Result<(), String> {
    let value = get_json("/api/dispatches")?;
    print_json(&value);
    Ok(())
}

/// `agentdesk dispatch retry <card_id>`
pub fn cmd_dispatch_retry(card_id: &str) -> Result<(), String> {
    let value = post_json(
        &format!("/api/kanban-cards/{card_id}/retry"),
        Some(serde_json::json!({})),
    )?;
    print_json(&value);
    Ok(())
}

/// `agentdesk dispatch redispatch <card_id>`
pub fn cmd_dispatch_redispatch(card_id: &str) -> Result<(), String> {
    let value = post_json(
        &format!("/api/kanban-cards/{card_id}/redispatch"),
        Some(serde_json::json!({})),
    )?;
    print_json(&value);
    Ok(())
}

/// `agentdesk resume <card_id_or_issue_number>`
///
/// The API handler resolves GitHub issue numbers automatically,
/// so this can pass the input directly.
pub fn cmd_resume(card_id: &str, force: bool, reason: Option<&str>) -> Result<(), String> {
    let mut body = serde_json::json!({});
    if force {
        body["force"] = serde_json::json!(true);
    }
    if let Some(r) = reason {
        body["reason"] = serde_json::json!(r);
    }

    let value = post_json(&format!("/api/kanban-cards/{card_id}/resume"), Some(body))?;
    print_json(&value);
    Ok(())
}

/// `agentdesk agents`
pub fn cmd_agents() -> Result<(), String> {
    let value = get_json("/api/agents")?;
    print_json(&value);
    Ok(())
}

/// `agentdesk config get`
pub fn cmd_config_get() -> Result<(), String> {
    let value = get_json("/api/settings/runtime-config")?;
    let effective = value.get("current").cloned().unwrap_or(value);
    print_json(&effective);
    Ok(())
}

/// `agentdesk config set <json>`
pub fn cmd_config_set(json_str: &str) -> Result<(), String> {
    let body: Value = serde_json::from_str(json_str).map_err(|e| format!("Invalid JSON: {e}"))?;
    let normalized = runtime_config_payload(body)?;
    let payload = normalized.to_string();
    let value = request_json("PUT", "/api/settings/runtime-config", Some(&payload))?;
    print_json(&value);
    Ok(())
}

/// `agentdesk api <method> <path> [body]`
pub fn cmd_api(method: &str, path: &str, body: Option<&str>) -> Result<(), String> {
    let value = api_call(method, path, body)?;
    print_json(&value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{render_cards_table, runtime_config_payload};
    use serde_json::json;

    #[test]
    fn runtime_config_payload_uses_current_envelope() {
        let payload = runtime_config_payload(json!({
            "current": {"maxRetries": 7},
            "defaults": {"maxRetries": 3}
        }))
        .unwrap();
        assert_eq!(payload, json!({"maxRetries": 7}));
    }

    #[test]
    fn render_cards_table_is_compact() {
        let rendered = render_cards_table(&[json!({
            "github_issue_number": 90,
            "status": "in_progress",
            "review_status": "rework_pending",
            "priority": "medium",
            "assigned_agent_id": "project-agentdesk",
            "title": "feat: AgentDesk CLI client"
        })]);
        assert!(rendered.contains("ISSUE"));
        assert!(rendered.contains("#90"));
        assert!(rendered.contains("feat: AgentDesk CLI client"));
        assert!(!rendered.contains("description"));
    }
}
