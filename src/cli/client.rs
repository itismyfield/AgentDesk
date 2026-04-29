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

fn parse_error_message(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|msg| !msg.is_empty())
                .map(str::to_string)
        })
        .or_else(|| {
            let trimmed = body.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        })
}

fn encode_path_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[(byte >> 4) as usize]));
            encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    encoded
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
        req.set("Content-Type", "application/json").send_string(b)
    } else if matches!(method_upper.as_str(), "POST" | "PATCH" | "PUT") {
        req.set("Content-Type", "application/json")
            .send_string("{}")
    } else {
        req.call()
    };

    let resp = match resp {
        Ok(resp) => resp,
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            let detail = parse_error_message(&body)
                .map(|msg| format!("Request failed ({code}): {msg}"))
                .unwrap_or_else(|| format!("Request failed ({code})"));
            return Err(detail);
        }
        Err(ureq::Error::Transport(err)) => return Err(format!("Request failed: {err}")),
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

pub(crate) fn post_json_value(path: &str, body: Value) -> Result<Value, String> {
    post_json(path, Some(body))
}

pub(crate) fn patch_json_value(path: &str, body: Value) -> Result<Value, String> {
    let body_string = body.to_string();
    request_json("PATCH", path, Some(&body_string))
}

fn parse_github_repo_from_remote(remote: &str) -> Option<String> {
    crate::services::platform::shell::parse_github_repo_from_remote(remote)
}

fn infer_dispatch_repo(repo: Option<&str>) -> Option<String> {
    if let Some(repo) = repo.map(str::trim).filter(|value| !value.is_empty()) {
        return Some(repo.to_string());
    }

    let repo_dir = crate::services::platform::resolve_repo_dir()?;
    let output = std::process::Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(repo_dir)
        .output()
        .ok()
        .filter(|output| output.status.success())?;
    let remote = String::from_utf8_lossy(&output.stdout);
    parse_github_repo_from_remote(&remote)
}

fn parse_dispatch_groups(issue_groups: &[String]) -> Result<Vec<Value>, String> {
    if issue_groups.is_empty() {
        return Err("provide one or more issue groups or use a dispatch subcommand".to_string());
    }

    let mut groups = Vec::with_capacity(issue_groups.len());
    for raw_group in issue_groups {
        let issues: Vec<i64> = raw_group
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| {
                value
                    .parse::<i64>()
                    .map_err(|_| format!("invalid issue number in group '{raw_group}': {value}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if issues.is_empty() {
            return Err(format!("issue group '{raw_group}' is empty"));
        }
        groups.push(serde_json::json!({
            "issues": issues,
            "sequential": raw_group.contains(','),
        }));
    }

    Ok(groups)
}

fn find_card_for_issue(issue_number: &str) -> Result<Value, String> {
    let cards = get_json("/api/kanban-cards")?;
    let issue_number: i64 = issue_number
        .parse()
        .map_err(|_| format!("Invalid issue number: {issue_number}"))?;
    cards["cards"]
        .as_array()
        .and_then(|arr| {
            arr.iter()
                .find(|card| card["github_issue_number"] == issue_number)
                .cloned()
        })
        .ok_or_else(|| format!("Card not found for issue #{issue_number}"))
}

fn load_card_dispatches(card_id: &str) -> Result<Vec<Value>, String> {
    let dispatches = get_json(&format!("/api/dispatches?card_id={card_id}"))?;
    dispatches
        .as_array()
        .or_else(|| dispatches["dispatches"].as_array())
        .cloned()
        .ok_or_else(|| format!("No dispatches found for card {card_id}"))
}

fn find_active_dispatch_by_type<'a>(
    dispatches: &'a [Value],
    dispatch_type: &str,
) -> Option<&'a Value> {
    dispatches.iter().find(|dispatch| {
        dispatch["dispatch_type"] == dispatch_type
            && matches!(
                dispatch["status"].as_str(),
                Some("pending") | Some("dispatched")
            )
    })
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

fn dispatch_context_string_field(dispatch: Option<&Value>, key: &str) -> Option<String> {
    dispatch
        .and_then(|value| value.get("context"))
        .and_then(|context| context.get(key))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn dispatch_target_repo_ref(card: &Value, pending_dispatch: Option<&Value>) -> Option<String> {
    dispatch_context_string_field(pending_dispatch, "target_repo")
        .or_else(|| dispatch_context_string_field(pending_dispatch, "worktree_path"))
        .or_else(|| {
            card.get("repo_id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn render_queue_thread_links(entry: &Value) -> String {
    let rendered: Vec<String> = entry
        .get("thread_links")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|link| {
            let label = link
                .get("label")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())?;
            if let Some(url) = link
                .get("url")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            {
                return Some(format!("{label}:{url}"));
            }
            link.get("thread_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(|thread_id| format!("{label}:thread:{thread_id}"))
        })
        .collect();

    if rendered.is_empty() {
        "-".to_string()
    } else {
        rendered.join(" | ")
    }
}

fn build_cli_advance_completion_result(card: &Value, pending_dispatch: Option<&Value>) -> Value {
    let issue_number = card.get("github_issue_number").and_then(Value::as_i64);
    let target_repo = dispatch_target_repo_ref(card, pending_dispatch);
    let target_repo_dir =
        crate::services::platform::shell::resolve_repo_dir_for_target(target_repo.as_deref())
            .ok()
            .flatten();

    let mut completed_worktree_path =
        dispatch_context_string_field(pending_dispatch, "worktree_path");
    let mut completed_branch = dispatch_context_string_field(pending_dispatch, "branch");
    let mut completed_commit = dispatch_context_string_field(pending_dispatch, "completed_commit")
        .or_else(|| dispatch_context_string_field(pending_dispatch, "reviewed_commit"));

    if completed_worktree_path.is_none() {
        if let Some(issue_number) = issue_number {
            if let Some(repo_dir) = target_repo_dir.clone() {
                if let Some(worktree) =
                    crate::services::platform::find_worktree_for_issue(&repo_dir, issue_number)
                {
                    completed_worktree_path = Some(worktree.path);
                    completed_branch.get_or_insert(worktree.branch);
                    completed_commit.get_or_insert(worktree.commit);
                }
            }
        }
    }

    if completed_worktree_path.is_none() {
        completed_worktree_path =
            target_repo_dir.or_else(crate::services::platform::resolve_repo_dir);
    }
    if completed_branch.is_none() {
        completed_branch = completed_worktree_path
            .as_deref()
            .and_then(crate::services::platform::shell::git_branch_name);
    }
    if completed_commit.is_none() {
        completed_commit = completed_worktree_path
            .as_deref()
            .and_then(crate::services::platform::git_head_commit);
    }

    let mut result = serde_json::Map::new();
    result.insert("status".to_string(), Value::String("done".to_string()));
    result.insert(
        "completion_source".to_string(),
        Value::String("cli_advance".to_string()),
    );
    if let Some(path) = completed_worktree_path {
        result.insert("completed_worktree_path".to_string(), Value::String(path));
    }
    if let Some(target_repo) = target_repo {
        result.insert("target_repo".to_string(), Value::String(target_repo));
    }
    if let Some(branch) = completed_branch {
        result.insert("completed_branch".to_string(), Value::String(branch));
    }
    if let Some(commit) = completed_commit {
        result.insert("completed_commit".to_string(), Value::String(commit));
    }
    Value::Object(result)
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
        .filter(|session| {
            matches!(
                session.get("status").and_then(Value::as_str),
                Some("turn_active" | "awaiting_bg" | "working")
            )
        })
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

/// `agentdesk dispatch 423,405 407 --unified --concurrent 2`
pub fn cmd_dispatch(
    issue_groups: &[String],
    repo: Option<&str>,
    agent_id: Option<&str>,
    unified: bool,
    concurrent: Option<i64>,
    activate: bool,
) -> Result<(), String> {
    if let Some(value) = concurrent {
        if value < 1 {
            return Err("--concurrent must be >= 1".to_string());
        }
    }

    let groups = parse_dispatch_groups(issue_groups)?;
    let mut body = serde_json::json!({
        "groups": groups,
        "activate": activate,
    });
    if unified {
        body["unified_thread"] = serde_json::json!(true);
    }
    if let Some(concurrent) = concurrent {
        body["max_concurrent_threads"] = serde_json::json!(concurrent);
    }
    if let Some(agent_id) = agent_id.map(str::trim).filter(|value| !value.is_empty()) {
        body["agent_id"] = serde_json::json!(agent_id);
        body["auto_assign_agent"] = serde_json::json!(true);
    }
    if let Some(repo) = infer_dispatch_repo(repo) {
        body["repo"] = serde_json::json!(repo);
    }

    let value = post_json("/api/auto-queue/dispatch", Some(body))?;
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

/// `agentdesk diag <agent_id_or_channel_id>`
pub fn cmd_diag(identifier: &str, json_output: bool) -> Result<(), String> {
    let identifier = identifier.trim();
    if identifier.is_empty() {
        return Err("identifier must not be empty".to_string());
    }

    let encoded_identifier = encode_path_segment(identifier);
    let value = get_json(&format!("/api/agents/diag/{encoded_identifier}"))?;
    if json_output {
        print_json(&value);
        return Ok(());
    }

    let target = value
        .get("agent_name")
        .and_then(Value::as_str)
        .or_else(|| value.get("agent_id").and_then(Value::as_str))
        .unwrap_or(identifier);
    let visual_status = value
        .get("visual_status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    println!("{target}: {visual_status}");

    for key in [
        "provider",
        "session_key",
        "status",
        "last_tool_elapsed_secs",
        "active_children",
        "oldest_child_spawned_at",
    ] {
        if let Some(value) = value.get(key).filter(|value| !value.is_null()) {
            println!("{key}: {}", render_diag_value(value));
        }
    }

    if let Some(last_tool) = value.get("last_tool").filter(|value| !value.is_null()) {
        println!("last_tool: {}", render_diag_value(last_tool));
    }
    if let Some(loop_suspicion) = value.get("recent_loop_suspicion") {
        println!(
            "recent_loop_suspicion: {}",
            render_diag_value(loop_suspicion)
        );
    }

    Ok(())
}

fn render_diag_value(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
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

/// `agentdesk config audit [--dry-run]`
pub fn cmd_config_audit(dry_run: bool) -> Result<(), String> {
    let root = crate::config::runtime_root()
        .ok_or_else(|| "Failed to resolve AGENTDESK_ROOT_DIR".to_string())?;
    let legacy_scan = crate::services::discord_config_audit::scan_legacy_sources(&root);

    if !dry_run {
        crate::runtime_layout::ensure_runtime_layout(&root)?;
    }

    let loaded = crate::services::discord_config_audit::load_runtime_config(&root)?;
    let outcome = crate::services::discord_config_audit::audit_and_reconcile_config_only(
        &root,
        loaded.config,
        loaded.path,
        loaded.existed,
        &legacy_scan,
        dry_run,
    )?;
    print_json(&serde_json::to_value(outcome.report).map_err(|err| err.to_string())?);
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
/// Complete the pending implementation/rework dispatch for an issue and verify
/// that the server created the follow-up review dispatch.
pub fn cmd_advance(issue_number: &str) -> Result<(), String> {
    let card = find_card_for_issue(issue_number)?;
    let card_id = card["id"].as_str().unwrap_or("");
    let card_title = card["title"].as_str().unwrap_or("");

    let dispatches = load_card_dispatches(card_id)?;
    let pending = dispatches.iter().find(|d| {
        d["status"] == "pending"
            && (d["dispatch_type"] == "implementation" || d["dispatch_type"] == "rework")
    });
    if let Some(d) = pending {
        let did = d["id"].as_str().unwrap_or("");
        println!("Completing dispatch {did}...");
        let completion_result = build_cli_advance_completion_result(&card, Some(d));
        request_json(
            "PATCH",
            &format!("/api/dispatches/{did}"),
            Some(
                &serde_json::json!({"status": "completed", "result": completion_result})
                    .to_string(),
            ),
        )?;

        let refreshed_card = find_card_for_issue(issue_number)?;
        let refreshed_status = refreshed_card["status"].as_str().unwrap_or("");
        let refreshed_dispatches = load_card_dispatches(card_id)?;
        if let Some(review_dispatch) = find_active_dispatch_by_type(&refreshed_dispatches, "review")
        {
            let review_dispatch_id = review_dispatch["id"].as_str().unwrap_or("?");
            println!("✅ #{issue_number} advanced to review (dispatch: {review_dispatch_id})");
            return Ok(());
        }

        let card_label = if card_title.is_empty() {
            format!("#{issue_number}")
        } else {
            format!("#{issue_number} ({card_title})")
        };
        return match refreshed_status {
            "review" => Err(format!(
                "Dispatch {did} completed, but {card_label} is in review without an active review dispatch. Check server logs for OnReviewEnter/create_dispatch errors."
            )),
            "done" => Err(format!(
                "Dispatch {did} completed, but {card_label} ended in done without an active review dispatch. Review was bypassed before a review dispatch could be created."
            )),
            other => Err(format!(
                "Dispatch {did} completed, but {card_label} is now '{other}' without an active review dispatch."
            )),
        };
    } else {
        if let Some(review_dispatch) = find_active_dispatch_by_type(&dispatches, "review") {
            let review_dispatch_id = review_dispatch["id"].as_str().unwrap_or("?");
            println!(
                "✅ #{issue_number} already has an active review dispatch ({review_dispatch_id})"
            );
            return Ok(());
        }
        return Err(format!(
            "No pending implementation/rework dispatch found for #{issue_number}."
        ));
    }
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

    for e in entries {
        let num = e["github_issue_number"].as_i64().unwrap_or(0);
        let status = e["status"].as_str().unwrap_or("?");
        let title = e["card_title"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(48)
            .collect::<String>();
        let links_str = render_queue_thread_links(e);

        println!("#{:<5} {:<12} {:<50} {}", num, status, title, links_str);
    }
    Ok(())
}

/// `agentdesk deploy`
///
/// Build the workspace for release and promote directly to release.
pub fn cmd_deploy() -> Result<(), String> {
    let workspace = crate::cli::agentdesk_runtime_root()
        .and_then(|r| {
            let ws = r.parent()?.join("workspaces/agentdesk");
            if ws.exists() { Some(ws) } else { None }
        })
        .ok_or("Cannot find workspace directory")?;

    println!("=== Step 1: Build workspace for release ===");
    let build_status = std::process::Command::new("bash")
        .arg("-c")
        .arg("./scripts/build-release.sh")
        .current_dir(&workspace)
        .status()
        .map_err(|e| format!("build-release failed: {e}"))?;
    if !build_status.success() {
        return Err("build-release.sh failed".to_string());
    }

    println!("\n=== Step 2: Deploy to release ===");
    let deploy_status = std::process::Command::new("bash")
        .arg("-c")
        .arg("AGENTDESK_REL_PORT=8791 ./scripts/deploy-release.sh --skip-review")
        .current_dir(&workspace)
        .status()
        .map_err(|e| format!("deploy-release failed: {e}"))?;
    if !deploy_status.success() {
        return Err("deploy-release.sh failed".to_string());
    }

    println!("✅ Deploy complete — release runtime updated");
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
    use super::{
        build_cli_advance_completion_result, cmd_advance, cmd_dispatch, encode_path_segment,
        parse_github_repo_from_remote, render_cards_table, render_queue_thread_links,
        runtime_config_payload,
    };
    use axum::extract::{Path, Query, State};
    use axum::routing::{get, patch, post};
    use axum::{Json, Router};
    use serde_json::{Value, json};
    use std::ffi::OsString;
    use std::process::Command;
    use std::sync::MutexGuard;
    use std::sync::{Arc, Mutex};

    fn env_lock() -> MutexGuard<'static, ()> {
        crate::services::discord::runtime_store::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn encode_path_segment_preserves_unreserved_and_escapes_path_chars() {
        assert_eq!(encode_path_segment("agent-01_~.x"), "agent-01_~.x");
        assert_eq!(
            encode_path_segment("thread/channel one"),
            "thread%2Fchannel%20one"
        );
    }

    struct EnvVarGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }

        fn set_path(key: &'static str, value: &std::path::Path) -> Self {
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self { key, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    fn run_git(repo_dir: &std::path::Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo_dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[derive(Clone)]
    struct AdvanceMockState {
        completed: bool,
        final_status: &'static str,
        force_transition_calls: usize,
        create_dispatch_calls: usize,
    }

    #[derive(serde::Deserialize)]
    struct DispatchQuery {
        card_id: Option<String>,
    }

    async fn advance_cards_handler(
        State(state): State<Arc<Mutex<AdvanceMockState>>>,
    ) -> Json<serde_json::Value> {
        let state = state.lock().unwrap();
        let status = if state.completed {
            state.final_status
        } else {
            "in_progress"
        };
        let latest_dispatch_id = if state.completed && state.final_status == "review" {
            json!("review-1")
        } else if state.completed {
            serde_json::Value::Null
        } else {
            json!("impl-1")
        };
        Json(json!({
            "cards": [{
                "id": "card-383",
                "github_issue_number": 383,
                "title": "Issue 383",
                "status": status,
                "assigned_agent_id": "agent-1",
                "latest_dispatch_id": latest_dispatch_id
            }]
        }))
    }

    async fn advance_dispatches_handler(
        State(state): State<Arc<Mutex<AdvanceMockState>>>,
        Query(query): Query<DispatchQuery>,
    ) -> Json<serde_json::Value> {
        assert_eq!(query.card_id.as_deref(), Some("card-383"));
        let state = state.lock().unwrap();
        let dispatches = if state.completed && state.final_status == "review" {
            json!({
                "dispatches": [
                    {
                        "id": "impl-1",
                        "dispatch_type": "implementation",
                        "status": "completed"
                    },
                    {
                        "id": "review-1",
                        "dispatch_type": "review",
                        "status": "pending"
                    }
                ]
            })
        } else if state.completed {
            json!({
                "dispatches": [{
                    "id": "impl-1",
                    "dispatch_type": "implementation",
                    "status": "completed"
                }]
            })
        } else {
            json!({
                "dispatches": [{
                    "id": "impl-1",
                    "dispatch_type": "implementation",
                    "status": "pending",
                    "context": {
                        "worktree_path": "/tmp/worktree-383",
                        "branch": "feature/383",
                        "completed_commit": "b2c2f8ead0cedec5db3d724bb2eabaeccd713136"
                    }
                }]
            })
        };
        Json(dispatches)
    }

    async fn advance_patch_handler(
        State(state): State<Arc<Mutex<AdvanceMockState>>>,
        Path(dispatch_id): Path<String>,
    ) -> Json<serde_json::Value> {
        assert_eq!(dispatch_id, "impl-1");
        state.lock().unwrap().completed = true;
        Json(json!({"dispatch": {"id": dispatch_id, "status": "completed"}}))
    }

    async fn advance_force_transition_handler(
        State(state): State<Arc<Mutex<AdvanceMockState>>>,
    ) -> Json<serde_json::Value> {
        let mut state = state.lock().unwrap();
        state.force_transition_calls += 1;
        Json(json!({"ok": true}))
    }

    async fn advance_create_dispatch_handler(
        State(state): State<Arc<Mutex<AdvanceMockState>>>,
    ) -> (axum::http::StatusCode, Json<serde_json::Value>) {
        let mut state = state.lock().unwrap();
        state.create_dispatch_calls += 1;
        (
            axum::http::StatusCode::CONFLICT,
            Json(json!({"error": "should not be called"})),
        )
    }

    fn run_cmd_advance_against_mock_server(
        final_status: &'static str,
    ) -> (Result<(), String>, AdvanceMockState) {
        let _lock = env_lock();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let state = Arc::new(Mutex::new(AdvanceMockState {
                completed: false,
                final_status,
                force_transition_calls: 0,
                create_dispatch_calls: 0,
            }));
            let app = Router::new()
                .route("/api/kanban-cards", get(advance_cards_handler))
                .route(
                    "/api/dispatches",
                    get(advance_dispatches_handler).post(advance_create_dispatch_handler),
                )
                .route("/api/dispatches/{id}", patch(advance_patch_handler))
                .route(
                    "/api/kanban-cards/{id}/transition",
                    post(advance_force_transition_handler),
                )
                .with_state(state.clone());

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let _api_url = EnvVarGuard::set("AGENTDESK_API_URL", &format!("http://{addr}"));

            let result = cmd_advance("383");
            server.abort();
            let state = state.lock().unwrap().clone();
            (result, state)
        })
    }

    async fn dispatch_handler(
        State(state): State<Arc<Mutex<Option<serde_json::Value>>>>,
        Json(body): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        *state.lock().unwrap() = Some(body);
        Json(json!({
            "run": {"id": "run-dispatch", "status": "active"},
            "entries": [],
            "thread_groups": {},
            "activated": true
        }))
    }

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

    #[test]
    fn render_queue_thread_links_prefers_server_urls() {
        let rendered = render_queue_thread_links(&json!({
            "thread_links": [
                {
                    "label": "work",
                    "url": "https://discord.com/channels/guild-1/thread-1"
                },
                {
                    "label": "review",
                    "url": "https://discord.com/channels/guild-1/thread-2"
                }
            ]
        }));

        assert_eq!(
            rendered,
            "work:https://discord.com/channels/guild-1/thread-1 | review:https://discord.com/channels/guild-1/thread-2"
        );
    }

    #[test]
    fn render_queue_thread_links_falls_back_to_thread_id_without_guessing_url() {
        let rendered = render_queue_thread_links(&json!({
            "thread_links": [
                {
                    "label": "active",
                    "thread_id": "1485506232256168011",
                    "url": null
                }
            ]
        }));

        assert_eq!(rendered, "active:thread:1485506232256168011");
    }

    #[test]
    fn parse_github_repo_from_remote_supports_common_formats() {
        assert_eq!(
            parse_github_repo_from_remote("git@github.com:itismyfield/AgentDesk.git"),
            Some("itismyfield/AgentDesk".to_string())
        );
        assert_eq!(
            parse_github_repo_from_remote("https://github.com/itismyfield/AgentDesk.git"),
            Some("itismyfield/AgentDesk".to_string())
        );
        assert_eq!(parse_github_repo_from_remote("/tmp/local-origin.git"), None);
    }

    #[test]
    fn cmd_dispatch_posts_declarative_auto_queue_payload() {
        let _lock = env_lock();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async move {
            let repo = tempfile::tempdir().unwrap();
            run_git(repo.path(), &["init", "-b", "main"]);
            run_git(repo.path(), &["config", "user.email", "test@test.com"]);
            run_git(repo.path(), &["config", "user.name", "Test"]);
            run_git(
                repo.path(),
                &[
                    "remote",
                    "add",
                    "origin",
                    "https://github.com/itismyfield/AgentDesk.git",
                ],
            );

            let captured = Arc::new(Mutex::new(None));
            let app = Router::new()
                .route("/api/auto-queue/dispatch", post(dispatch_handler))
                .with_state(captured.clone());

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });

            let _api_url = EnvVarGuard::set("AGENTDESK_API_URL", &format!("http://{addr}"));
            let _repo_env = EnvVarGuard::set_path("AGENTDESK_REPO_DIR", repo.path());

            let result = cmd_dispatch(
                &["423,405".to_string(), "407".to_string()],
                None,
                Some("project-agentdesk"),
                true,
                Some(2),
                true,
            );

            server.abort();
            assert!(result.is_ok(), "cmd_dispatch failed: {result:?}");

            let payload = captured
                .lock()
                .unwrap()
                .clone()
                .expect("dispatch payload must be captured");
            assert_eq!(payload["repo"], "itismyfield/AgentDesk");
            assert_eq!(payload["agent_id"], "project-agentdesk");
            assert_eq!(payload["auto_assign_agent"], true);
            assert_eq!(payload["activate"], true);
            assert_eq!(payload["unified_thread"], true);
            assert_eq!(payload["max_concurrent_threads"], 2);
            assert_eq!(payload["groups"][0]["issues"], json!([423, 405]));
            assert_eq!(payload["groups"][0]["sequential"], true);
            assert_eq!(payload["groups"][1]["issues"], json!([407]));
            assert_eq!(payload["groups"][1]["sequential"], false);
        });
    }

    #[test]
    fn cli_advance_completion_result_prefers_dispatch_worktree_context() {
        let _lock = env_lock();
        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "-b", "main"]);
        run_git(repo.path(), &["config", "user.email", "test@test.com"]);
        run_git(repo.path(), &["config", "user.name", "Test"]);
        std::fs::write(repo.path().join("README.md"), "hello\n").unwrap();
        run_git(repo.path(), &["add", "README.md"]);
        run_git(repo.path(), &["commit", "-m", "feat: seed (#331)"]);

        let result = build_cli_advance_completion_result(
            &json!({"github_issue_number": 331}),
            Some(&json!({
                "context": {
                    "worktree_path": repo.path().to_string_lossy().to_string(),
                    "branch": "feature/331-review"
                }
            })),
        );

        assert_eq!(
            result["completed_worktree_path"],
            repo.path().to_string_lossy().to_string()
        );
        assert_eq!(result["completed_branch"], "feature/331-review");
        assert_eq!(
            result["completed_commit"],
            crate::services::platform::git_head_commit(&repo.path().to_string_lossy()).unwrap()
        );
        assert_eq!(
            result["target_repo"],
            repo.path().to_string_lossy().to_string()
        );
    }

    #[test]
    fn cli_advance_completion_result_falls_back_to_repo_dir() {
        let _lock = env_lock();
        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "-b", "main"]);
        run_git(repo.path(), &["config", "user.email", "test@test.com"]);
        run_git(repo.path(), &["config", "user.name", "Test"]);
        std::fs::write(repo.path().join("README.md"), "hello\n").unwrap();
        run_git(repo.path(), &["add", "README.md"]);
        run_git(repo.path(), &["commit", "-m", "feat: seed (#340)"]);
        let _repo_env = EnvVarGuard::set_path("AGENTDESK_REPO_DIR", repo.path());

        let result =
            build_cli_advance_completion_result(&json!({"github_issue_number": 340}), None);

        assert_eq!(
            result["completed_worktree_path"],
            repo.path().to_string_lossy().to_string()
        );
        assert_eq!(result["completed_branch"], "main");
        assert_eq!(
            result["completed_commit"],
            crate::services::platform::git_head_commit(&repo.path().to_string_lossy()).unwrap()
        );
        assert_eq!(result["target_repo"], Value::Null);
    }

    #[test]
    fn cli_advance_completion_result_uses_target_repo_context_for_fallback() {
        let _lock = env_lock();
        let default_repo = tempfile::tempdir().unwrap();
        run_git(default_repo.path(), &["init", "-b", "main"]);
        run_git(
            default_repo.path(),
            &["config", "user.email", "test@test.com"],
        );
        run_git(default_repo.path(), &["config", "user.name", "Test"]);
        std::fs::write(default_repo.path().join("README.md"), "default\n").unwrap();
        run_git(default_repo.path(), &["add", "README.md"]);
        run_git(
            default_repo.path(),
            &["commit", "-m", "feat: default (#627)"],
        );
        let _repo_env = EnvVarGuard::set_path("AGENTDESK_REPO_DIR", default_repo.path());

        let target_repo = tempfile::tempdir().unwrap();
        run_git(target_repo.path(), &["init", "-b", "main"]);
        run_git(
            target_repo.path(),
            &["config", "user.email", "test@test.com"],
        );
        run_git(target_repo.path(), &["config", "user.name", "Test"]);
        std::fs::write(target_repo.path().join("README.md"), "target\n").unwrap();
        run_git(target_repo.path(), &["add", "README.md"]);
        run_git(target_repo.path(), &["commit", "-m", "feat: target (#627)"]);

        let result = build_cli_advance_completion_result(
            &json!({"github_issue_number": 627}),
            Some(&json!({
                "context": {
                    "target_repo": target_repo.path().to_string_lossy().to_string()
                }
            })),
        );
        let expected_target_repo = std::fs::canonicalize(target_repo.path())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let actual_completed_worktree =
            std::fs::canonicalize(result["completed_worktree_path"].as_str().unwrap())
                .unwrap()
                .to_string_lossy()
                .into_owned();
        let actual_target_repo = std::fs::canonicalize(result["target_repo"].as_str().unwrap())
            .unwrap()
            .to_string_lossy()
            .into_owned();

        assert_eq!(actual_completed_worktree, expected_target_repo);
        assert_eq!(
            result["completed_commit"],
            crate::services::platform::git_head_commit(&target_repo.path().to_string_lossy())
                .unwrap()
        );
        assert_eq!(actual_target_repo, expected_target_repo);
    }

    #[test]
    fn cmd_advance_uses_server_created_review_dispatch() {
        let (result, state) = run_cmd_advance_against_mock_server("review");
        assert!(result.is_ok(), "advance should succeed: {result:?}");
        assert_eq!(
            state.force_transition_calls, 0,
            "advance must not force-transition after finalize_dispatch"
        );
        assert_eq!(
            state.create_dispatch_calls, 0,
            "advance must not create a second review dispatch"
        );
    }

    #[test]
    fn cmd_advance_reports_done_without_review_dispatch() {
        let (result, state) = run_cmd_advance_against_mock_server("done");
        let err = result.expect_err("advance should fail when review dispatch is missing");
        assert!(
            err.contains("ended in done without an active review dispatch"),
            "unexpected error: {err}"
        );
        assert_eq!(
            state.force_transition_calls, 0,
            "advance must not try to force-transition a terminal card"
        );
        assert_eq!(
            state.create_dispatch_calls, 0,
            "advance must not post a review dispatch after terminal completion"
        );
    }
}
