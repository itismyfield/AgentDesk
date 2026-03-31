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

/// `agentdesk advance <issue_number>`
///
/// Complete the pending dispatch for an issue and create a review dispatch.
/// Replaces the manual 4-step process: PATCH complete → force-transition → update review_status → POST review dispatch.
pub fn cmd_advance(issue_number: &str) -> Result<(), String> {
    // 1. Find card
    let cards = get_json("/api/kanban-cards")?;
    let card = cards["cards"]
        .as_array()
        .and_then(|arr| {
            let num: i64 = issue_number.parse().unwrap_or(0);
            arr.iter().find(|c| c["github_issue_number"] == num)
        })
        .ok_or_else(|| format!("Card not found for issue #{issue_number}"))?;
    let card_id = card["id"].as_str().unwrap_or("");
    let card_title = card["title"].as_str().unwrap_or("");
    let status = card["status"].as_str().unwrap_or("");

    // 2. Find and complete pending dispatch
    let dispatches = get_json(&format!("/api/dispatches?card_id={card_id}"))?;
    let ds = dispatches
        .as_array()
        .or_else(|| dispatches["dispatches"].as_array())
        .ok_or("No dispatches found")?;
    let pending = ds.iter().find(|d| {
        d["status"] == "pending"
            && (d["dispatch_type"] == "implementation" || d["dispatch_type"] == "rework")
    });
    if let Some(d) = pending {
        let did = d["id"].as_str().unwrap_or("");
        println!("Completing dispatch {did}...");
        request_json(
            "PATCH",
            &format!("/api/dispatches/{did}"),
            Some(&serde_json::json!({"status": "completed", "result": {"status": "done", "completion_source": "cli_advance"}}).to_string()),
        )?;
    } else {
        println!("No pending implementation/rework dispatch found.");
    }

    // 3. Force transition to review if not already
    if status != "review" {
        println!("Transitioning to review...");
        // Need PMD channel for force-transition — read from DB via API
        let kanban_mgr = get_json("/api/settings/runtime-config")
            .ok()
            .and_then(|v| {
                v["current"]["kanbanManagerChannelId"]
                    .as_str()
                    .map(|s| s.to_string())
            })
            .unwrap_or_default();
        let mut req = agent().post(&format!(
            "{}/api/kanban-cards/{card_id}/force-transition",
            api_base()
        ));
        if let Some(token) = auth_token() {
            req = req.set("Authorization", &format!("Bearer {token}"));
        }
        if !kanban_mgr.is_empty() {
            req = req.set("X-Channel-Id", &kanban_mgr);
        }
        let _ = req
            .set("Content-Type", "application/json")
            .send_string(&serde_json::json!({"status": "review"}).to_string())
            .ok();
    }

    // 4. Create review dispatch
    println!("Creating review dispatch...");
    let review_title = format!("[Review] {card_title}");
    let result = post_json(
        "/api/dispatches",
        Some(serde_json::json!({
            "kanban_card_id": card_id,
            "to_agent_id": card["assigned_agent_id"],
            "dispatch_type": "review",
            "title": review_title,
            "context": {"cli_advance": true}
        })),
    )?;
    let dispatch_id = result["dispatch"]["id"].as_str().unwrap_or("?");
    println!("✅ #{issue_number} advanced to review (dispatch: {dispatch_id})");
    Ok(())
}

/// `agentdesk queue`
///
/// Show auto-queue status with work/review thread links.
pub fn cmd_queue() -> Result<(), String> {
    let data = get_json("/api/auto-queue/status")?;
    let entries = data["entries"].as_array().ok_or("No entries")?;
    let run = &data["run"];

    let unified = run["unified_thread"].as_bool().unwrap_or(false);
    let max_threads = run["max_concurrent_threads"].as_i64().unwrap_or(1);
    println!(
        "Run: {} | unified={} max_threads={}",
        run["status"].as_str().unwrap_or("?"),
        unified,
        max_threads
    );
    println!(
        "{:<6} {:<12} {:<50} {}",
        "Issue", "Status", "Title", "Threads"
    );
    println!("{}", "-".repeat(100));

    // Get unified_thread_id map for thread links
    let thread_map: serde_json::Map<String, Value> = run["unified_thread_id"]
        .as_str()
        .and_then(|s| serde_json::from_str(s).ok())
        .or_else(|| run["unified_thread_id"].as_object().cloned())
        .unwrap_or_default();

    // Discord server ID — derive from channel IDs in thread_map
    let discord_server = "1470762182344966308"; // TODO: make configurable

    for e in entries {
        let num = e["github_issue_number"].as_i64().unwrap_or(0);
        let status = e["status"].as_str().unwrap_or("?");
        let title = e["card_title"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(48)
            .collect::<String>();

        // Build thread links
        let mut links = Vec::new();
        for (ch_id, thread_val) in &thread_map {
            if let Some(tid) = thread_val.as_str() {
                let label = if ch_id.ends_with("35") {
                    "work"
                } else {
                    "review"
                };
                links.push(format!(
                    "{label}:https://discord.com/channels/{discord_server}/{tid}"
                ));
            }
        }
        let links_str = if links.is_empty() {
            "-".to_string()
        } else {
            links.join(" | ")
        };

        println!("#{:<5} {:<12} {:<50} {}", num, status, title, links_str);
    }
    Ok(())
}

/// `agentdesk deploy`
///
/// Build + deploy dev + promote to release in one command.
pub fn cmd_deploy() -> Result<(), String> {
    let workspace = crate::cli::agentdesk_runtime_root()
        .and_then(|r| {
            let ws = r.parent()?.join("workspaces/agentdesk");
            if ws.exists() { Some(ws) } else { None }
        })
        .ok_or("Cannot find workspace directory")?;

    println!("=== Step 1: Deploy to dev ===");
    let dev_status = std::process::Command::new("bash")
        .arg("-c")
        .arg("AGENTDESK_DEV_PORT=8799 AGENTDESK_REL_PORT=8791 ./scripts/deploy-dev.sh")
        .current_dir(&workspace)
        .status()
        .map_err(|e| format!("deploy-dev failed: {e}"))?;
    if !dev_status.success() {
        return Err("deploy-dev.sh failed".to_string());
    }

    println!("\n=== Step 2: Waiting for dev health ===");
    for _ in 0..60 {
        if let Ok(resp) = ureq::Agent::new()
            .get("http://127.0.0.1:8799/api/health")
            .call()
        {
            if resp.status() == 200 {
                println!("✅ Dev healthy");
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    println!("\n=== Step 3: Promote to release ===");
    let promote_status = std::process::Command::new("bash")
        .arg("-c")
        .arg("AGENTDESK_DEV_PORT=8799 AGENTDESK_REL_PORT=8791 ./scripts/promote-release.sh --skip-review")
        .current_dir(&workspace)
        .status()
        .map_err(|e| format!("promote-release failed: {e}"))?;
    if !promote_status.success() {
        return Err("promote-release.sh failed".to_string());
    }

    println!("✅ Deploy complete — release will restart after current turn");
    Ok(())
}

/// `agentdesk terminations [--card-id X] [--dispatch-id X] [--session X] [--limit N]`
pub fn cmd_terminations(
    card_id: Option<&str>,
    dispatch_id: Option<&str>,
    session: Option<&str>,
    limit: u32,
) -> Result<(), String> {
    let mut params = vec![format!("limit={limit}")];
    if let Some(v) = card_id {
        params.push(format!("card_id={v}"));
    }
    if let Some(v) = dispatch_id {
        params.push(format!("dispatch_id={v}"));
    }
    if let Some(v) = session {
        params.push(format!("session_key={v}"));
    }
    let query = params.join("&");
    let value = get_json(&format!("/api/session-termination-events?{query}"))?;
    let events = value
        .get("events")
        .and_then(Value::as_array)
        .ok_or_else(|| "invalid response".to_string())?;

    if events.is_empty() {
        println!("No termination events found.");
        return Ok(());
    }

    // Table header
    let time_w = 19;
    let component_w = 16;
    let code_w = 26;
    let session_w = 40;
    let alive_w = 5;

    println!(
        "{}  {}  {}  {}  {}  {}",
        pad_cell("CREATED_AT", time_w),
        pad_cell("COMPONENT", component_w),
        pad_cell("REASON_CODE", code_w),
        pad_cell("SESSION", session_w),
        pad_cell("ALIVE", alive_w),
        "REASON_TEXT",
    );
    println!(
        "{}  {}  {}  {}  {}  {}",
        "-".repeat(time_w),
        "-".repeat(component_w),
        "-".repeat(code_w),
        "-".repeat(session_w),
        "-".repeat(alive_w),
        "-".repeat(40),
    );

    for event in events {
        let created = event
            .get("created_at")
            .and_then(Value::as_str)
            .unwrap_or("-");
        let component = event
            .get("killer_component")
            .and_then(Value::as_str)
            .unwrap_or("-");
        let code = event
            .get("reason_code")
            .and_then(Value::as_str)
            .unwrap_or("-");
        let session_key = event
            .get("session_key")
            .and_then(Value::as_str)
            .unwrap_or("-");
        let alive = match event.get("tmux_alive").and_then(Value::as_bool) {
            Some(true) => "Y",
            Some(false) => "N",
            None => "-",
        };
        let reason = event
            .get("reason_text")
            .and_then(Value::as_str)
            .unwrap_or("-");

        println!(
            "{}  {}  {}  {}  {}  {}",
            pad_cell(created, time_w),
            pad_cell(component, component_w),
            pad_cell(code, code_w),
            pad_cell(session_key, session_w),
            pad_cell(alive, alive_w),
            reason,
        );
    }
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
