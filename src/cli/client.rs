//! CLI client subcommands that call the AgentDesk HTTP API.

use crate::config;
use serde_json::Value;

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
    let output = crate::services::git::GitCommand::new()
        .repo(repo_dir)
        .args(["config", "--get", "remote.origin.url"])
        .run_output()
        .ok()?;
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

fn queue_status_count(entries: &[Value], status: &str) -> usize {
    entries
        .iter()
        .filter(|entry| entry.get("status").and_then(Value::as_str) == Some(status))
        .count()
}

fn format_queue_entry_ref(entry: &Value) -> String {
    let entry_id = entry.get("id").and_then(Value::as_str).unwrap_or("-");
    let card_id = entry
        .get("card_id")
        .or_else(|| entry.get("kanban_card_id"))
        .and_then(Value::as_str)
        .unwrap_or("-");
    let issue = entry
        .get("github_issue_number")
        .and_then(Value::as_i64)
        .map(|value| format!("#{value}"))
        .unwrap_or_else(|| "-".to_string());
    format!("entry={entry_id} card={card_id} issue={issue}")
}

fn format_queue_attention_entries(entries: &[Value], status: &str) -> Option<String> {
    let refs: Vec<String> = entries
        .iter()
        .filter(|entry| entry.get("status").and_then(Value::as_str) == Some(status))
        .take(3)
        .map(format_queue_entry_ref)
        .collect();
    if refs.is_empty() {
        None
    } else {
        Some(format!("{status}: {}", refs.join("; ")))
    }
}

fn render_auto_queue_status_lines(queue_summary: &str, entries: &[Value]) -> Vec<String> {
    let pending = queue_status_count(entries, "pending");
    let dispatched = queue_status_count(entries, "dispatched");
    let done = queue_status_count(entries, "done");
    let skipped = queue_status_count(entries, "skipped");
    let failed = queue_status_count(entries, "failed");
    let mut lines = vec![format!(
        "  Auto-Queue: {queue_summary} | total={} pending={pending} dispatched={dispatched} done={done} failed={failed} skipped={skipped}",
        entries.len(),
    )];

    if failed > 0 || skipped > 0 {
        let mut details = Vec::new();
        if let Some(failed_entries) = format_queue_attention_entries(entries, "failed") {
            details.push(failed_entries);
        }
        if let Some(skipped_entries) = format_queue_attention_entries(entries, "skipped") {
            details.push(skipped_entries);
        }
        lines.push(format!("  Auto-Queue Attention: {}", details.join(" | ")));
    }

    lines
}

// ── Subcommand handlers ──────────────────────────────────────

/// `agentdesk status` — server health + auto-queue status
pub fn cmd_status() -> Result<(), String> {
    let health = get_json("/api/health")?;
    let sessions = get_json("/api/dispatched-sessions?include_merged=1")?;
    let queue = get_json("/api/queue/status")?;

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
        .ok_or_else(|| "invalid /api/queue/status response".to_string())?;
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
    for line in render_auto_queue_status_lines(&queue_summary, queue_entries) {
        println!("{line}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// `agentdesk health` / `agentdesk machine-compare`  (issue #2656)
//
// These commands replace the recurring "ssh + mtime + 자연어 보고" loop that
// every deploy / outage triggers. They are intentionally read-only and run
// against the local API only — cross-machine state is obtained through the
// existing `worker_nodes` cluster heartbeat table (no SSH, no shell-out).
// ---------------------------------------------------------------------------

/// Resolve a `cluster.lease_ttl_secs`-shaped staleness budget for treating a
/// worker_node row as offline. Falls back to the `/api/cluster/nodes`
/// response, then to a conservative 60 s default.
fn cluster_lease_ttl_secs(cluster_meta: Option<&Value>) -> u64 {
    cluster_meta
        .and_then(|meta| meta.get("lease_ttl_secs"))
        .and_then(Value::as_u64)
        .filter(|secs| *secs > 0)
        .unwrap_or(60)
}

fn human_age_seconds(secs: i64) -> String {
    if secs < 0 {
        return "future".to_string();
    }
    let secs = secs as u64;
    if secs < 60 {
        return format!("{secs}s");
    }
    let minutes = secs / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 48 {
        let leftover_min = minutes % 60;
        if leftover_min == 0 {
            return format!("{hours}h");
        }
        return format!("{hours}h{leftover_min}m");
    }
    let days = hours / 24;
    format!("{days}d")
}

fn iso_age(timestamp: Option<&str>) -> String {
    let Some(ts) = timestamp else {
        return "-".to_string();
    };
    let parsed = chrono::DateTime::parse_from_rfc3339(ts);
    match parsed {
        Ok(parsed) => {
            let now = chrono::Utc::now();
            let secs = now
                .signed_duration_since(parsed.with_timezone(&chrono::Utc))
                .num_seconds();
            human_age_seconds(secs)
        }
        Err(_) => "-".to_string(),
    }
}

fn cmd_health_collect() -> Result<Value, String> {
    let health = get_json("/api/health")?;
    let queue = get_json("/api/queue/status")?;
    // Best-effort: cluster endpoint may be unavailable when PG is offline;
    // we surface that as a warning rather than aborting the whole command.
    let cluster = get_json("/api/cluster/nodes").ok();

    let queue_entries = queue
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let queue_run = queue.get("run").cloned();
    let pending = queue_entries
        .iter()
        .filter(|entry| entry.get("status").and_then(Value::as_str) == Some("queued"))
        .count();
    let failed = queue_entries
        .iter()
        .filter(|entry| entry.get("status").and_then(Value::as_str) == Some("failed"))
        .count();
    let oldest_queued_age_secs = queue_entries
        .iter()
        .filter(|entry| entry.get("status").and_then(Value::as_str) == Some("queued"))
        .filter_map(|entry| entry.get("queued_at").and_then(Value::as_str))
        .filter_map(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
        .map(|ts| {
            chrono::Utc::now()
                .signed_duration_since(ts.with_timezone(&chrono::Utc))
                .num_seconds()
                .max(0)
        })
        .max()
        .unwrap_or(0);

    // Pick out the local node from cluster — best effort match on hostname.
    let local_node = cluster
        .as_ref()
        .and_then(|value| value.get("nodes"))
        .and_then(Value::as_array)
        .and_then(|nodes| {
            let host = crate::services::platform::hostname_short();
            nodes
                .iter()
                .find(|node| node.get("hostname").and_then(Value::as_str) == Some(host.as_str()))
                .cloned()
        });

    Ok(serde_json::json!({
        "health": health,
        "queue": {
            "summary": queue_run,
            "pending": pending,
            "failed": failed,
            "oldest_queued_age_secs": oldest_queued_age_secs,
        },
        "local_node": local_node,
    }))
}

/// `agentdesk health [--json]`
///
/// Compact, consolidated health snapshot for the *current* node — server
/// status, dcserver pid, last deploy time (process start), queue lag, Discord
/// providers, and the disk/outbox warnings already surfaced by /api/health.
pub fn cmd_health(json_output: bool) -> Result<(), String> {
    let snapshot = cmd_health_collect()?;
    if json_output {
        print_json(&snapshot);
        return Ok(());
    }

    let health = snapshot.get("health").cloned().unwrap_or(Value::Null);
    let queue = snapshot.get("queue").cloned().unwrap_or(Value::Null);
    let local = snapshot.get("local_node").cloned().unwrap_or(Value::Null);

    let version = health
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let status = health
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            if health.get("ok").and_then(Value::as_bool) == Some(true) {
                "healthy"
            } else {
                "unknown"
            }
        });
    let fully_recovered = health
        .get("fully_recovered")
        .and_then(Value::as_bool)
        .map(|b| if b { "yes" } else { "no" })
        .unwrap_or("?");
    let degraded_reasons: Vec<String> = health
        .get("degraded_reasons")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect();

    let pid = local
        .get("process_id")
        .and_then(Value::as_i64)
        .map(|p| p.to_string())
        .unwrap_or_else(|| "-".to_string());
    let started_at = local.get("started_at").and_then(Value::as_str);
    let last_heartbeat_at = local.get("last_heartbeat_at").and_then(Value::as_str);
    let instance_id = local
        .get("instance_id")
        .and_then(Value::as_str)
        .unwrap_or("-");
    let role = local
        .get("effective_role")
        .and_then(Value::as_str)
        .or_else(|| local.get("role").and_then(Value::as_str))
        .unwrap_or("-");
    let active_dispatches = local
        .get("active_dispatch_count")
        .and_then(Value::as_i64)
        .unwrap_or(0);

    let pending = queue.get("pending").and_then(Value::as_i64).unwrap_or(0);
    let failed = queue.get("failed").and_then(Value::as_i64).unwrap_or(0);
    let oldest_lag = queue
        .get("oldest_queued_age_secs")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let queue_run = queue.get("summary").cloned().unwrap_or(Value::Null);
    let queue_state = queue_run
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("idle");

    println!("AgentDesk Health");
    println!("  Base URL    : {}", api_base());
    println!("  Server      : {status} (v{version})  fully_recovered={fully_recovered}");
    if !degraded_reasons.is_empty() {
        println!("  Degraded    : {}", degraded_reasons.join(", "));
    }
    println!("  Discord     : {}", summarize_discord_health(&health));
    println!(
        "  Cluster node: {instance_id}  role={role}  pid={pid}  active_dispatch={active_dispatches}"
    );
    println!(
        "  Started     : {}  ({} ago)",
        started_at.unwrap_or("-"),
        iso_age(started_at)
    );
    println!(
        "  Heartbeat   : {}  ({} ago)",
        last_heartbeat_at.unwrap_or("-"),
        iso_age(last_heartbeat_at)
    );
    println!(
        "  Queue       : {queue_state}  pending={pending}  failed={failed}  oldest_lag={}",
        human_age_seconds(oldest_lag)
    );
    if local.is_null() {
        println!(
            "  Note        : local worker_node row not found — cluster heartbeat may be disabled."
        );
    }
    Ok(())
}

/// Per-machine row used by `cmd_machine_compare`.
#[derive(Debug, Clone, Default)]
struct MachineRow {
    label: String,
    hostname: String,
    instance_id: String,
    role: String,
    status: String,
    pid: String,
    version: String,
    started_at: Option<String>,
    last_heartbeat_at: Option<String>,
    active_dispatches: i64,
}

fn classify_machine_label(node: &Value) -> String {
    let labels = node
        .get("labels")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for label in &labels {
        if let Some(text) = label.as_str() {
            let lower = text.to_ascii_lowercase();
            if lower.contains("mac-mini") || lower.contains("mac_mini") {
                return "mac-mini".to_string();
            }
            if lower.contains("mac-book") || lower.contains("macbook") {
                return "mac-book".to_string();
            }
        }
    }
    if let Some(host) = node.get("hostname").and_then(Value::as_str) {
        let lower = host.to_ascii_lowercase();
        if lower.contains("mac-mini") || lower.contains("macmini") {
            return "mac-mini".to_string();
        }
        if lower.contains("mac-book") || lower.contains("macbook") {
            return "mac-book".to_string();
        }
        return host.to_string();
    }
    node.get("instance_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}

fn machine_row_from_node(node: &Value) -> MachineRow {
    MachineRow {
        label: classify_machine_label(node),
        hostname: node
            .get("hostname")
            .and_then(Value::as_str)
            .unwrap_or("-")
            .to_string(),
        instance_id: node
            .get("instance_id")
            .and_then(Value::as_str)
            .unwrap_or("-")
            .to_string(),
        role: node
            .get("effective_role")
            .and_then(Value::as_str)
            .or_else(|| node.get("role").and_then(Value::as_str))
            .unwrap_or("-")
            .to_string(),
        status: node
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("-")
            .to_string(),
        pid: node
            .get("process_id")
            .and_then(Value::as_i64)
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".to_string()),
        version: node
            .get("capabilities")
            .and_then(|cap| cap.get("version"))
            .and_then(Value::as_str)
            .unwrap_or("-")
            .to_string(),
        started_at: node
            .get("started_at")
            .and_then(Value::as_str)
            .map(str::to_string),
        last_heartbeat_at: node
            .get("last_heartbeat_at")
            .and_then(Value::as_str)
            .map(str::to_string),
        active_dispatches: node
            .get("active_dispatch_count")
            .and_then(Value::as_i64)
            .unwrap_or(0),
    }
}

fn pad_to(value: &str, width: usize) -> String {
    let count = value.chars().count();
    if count >= width {
        value.to_string()
    } else {
        let mut padded = String::with_capacity(value.len() + (width - count));
        padded.push_str(value);
        for _ in 0..(width - count) {
            padded.push(' ');
        }
        padded
    }
}

fn diff_marker(left: &str, right: &str) -> &'static str {
    if left == "-" && right == "-" {
        return "";
    }
    if left == right { "" } else { "!=" }
}

fn render_machine_compare_table(rows: &[MachineRow]) -> String {
    // Build a transposed table: each row is a metric, columns are machines.
    // Always present at least two slots (mac-mini left, mac-book right) even
    // if one side has no live heartbeat, so the diff column stays meaningful.
    let mut left = rows
        .iter()
        .find(|row| row.label == "mac-mini")
        .cloned()
        .unwrap_or_else(|| MachineRow {
            label: "mac-mini".to_string(),
            hostname: "-".to_string(),
            instance_id: "(no heartbeat)".to_string(),
            role: "-".to_string(),
            status: "offline".to_string(),
            pid: "-".to_string(),
            version: "-".to_string(),
            started_at: None,
            last_heartbeat_at: None,
            active_dispatches: 0,
        });
    let mut right = rows
        .iter()
        .find(|row| row.label == "mac-book")
        .cloned()
        .unwrap_or_else(|| MachineRow {
            label: "mac-book".to_string(),
            hostname: "-".to_string(),
            instance_id: "(no heartbeat)".to_string(),
            role: "-".to_string(),
            status: "offline".to_string(),
            pid: "-".to_string(),
            version: "-".to_string(),
            started_at: None,
            last_heartbeat_at: None,
            active_dispatches: 0,
        });
    // Normalise empty markers for clarity.
    if left.hostname.is_empty() {
        left.hostname = "-".to_string();
    }
    if right.hostname.is_empty() {
        right.hostname = "-".to_string();
    }

    let left_started = left.started_at.clone().unwrap_or_else(|| "-".to_string());
    let right_started = right.started_at.clone().unwrap_or_else(|| "-".to_string());
    let left_started_age = iso_age(left.started_at.as_deref());
    let right_started_age = iso_age(right.started_at.as_deref());
    let left_hb_age = iso_age(left.last_heartbeat_at.as_deref());
    let right_hb_age = iso_age(right.last_heartbeat_at.as_deref());

    let metric_rows: Vec<[String; 4]> = vec![
        [
            "hostname".to_string(),
            left.hostname.clone(),
            right.hostname.clone(),
            diff_marker(&left.hostname, &right.hostname).to_string(),
        ],
        [
            "instance_id".to_string(),
            left.instance_id.clone(),
            right.instance_id.clone(),
            diff_marker(&left.instance_id, &right.instance_id).to_string(),
        ],
        [
            "role".to_string(),
            left.role.clone(),
            right.role.clone(),
            diff_marker(&left.role, &right.role).to_string(),
        ],
        [
            "status".to_string(),
            left.status.clone(),
            right.status.clone(),
            diff_marker(&left.status, &right.status).to_string(),
        ],
        [
            "pid".to_string(),
            left.pid.clone(),
            right.pid.clone(),
            String::new(),
        ],
        [
            "version".to_string(),
            left.version.clone(),
            right.version.clone(),
            diff_marker(&left.version, &right.version).to_string(),
        ],
        [
            "started_at".to_string(),
            format!("{left_started} ({left_started_age})"),
            format!("{right_started} ({right_started_age})"),
            String::new(),
        ],
        [
            "heartbeat_age".to_string(),
            left_hb_age,
            right_hb_age,
            String::new(),
        ],
        [
            "active_dispatch".to_string(),
            left.active_dispatches.to_string(),
            right.active_dispatches.to_string(),
            String::new(),
        ],
    ];

    let metric_w = metric_rows
        .iter()
        .map(|row| row[0].chars().count())
        .max()
        .unwrap_or(8)
        .max(10);
    let left_w = metric_rows
        .iter()
        .map(|row| row[1].chars().count())
        .max()
        .unwrap_or(8)
        .clamp(10, 80);
    let right_w = metric_rows
        .iter()
        .map(|row| row[2].chars().count())
        .max()
        .unwrap_or(8)
        .clamp(10, 80);

    let mut out = String::new();
    out.push_str(&format!(
        "{}  {}  {}  diff\n",
        pad_to("metric", metric_w),
        pad_to("mac-mini", left_w),
        pad_to("mac-book", right_w),
    ));
    out.push_str(&format!(
        "{}  {}  {}  ----\n",
        "-".repeat(metric_w),
        "-".repeat(left_w),
        "-".repeat(right_w),
    ));
    for row in &metric_rows {
        out.push_str(&format!(
            "{}  {}  {}  {}\n",
            pad_to(&row[0], metric_w),
            pad_to(&row[1], left_w),
            pad_to(&row[2], right_w),
            row[3],
        ));
    }
    // Report any other registered nodes that didn't fit mac-mini / mac-book.
    let extras: Vec<&MachineRow> = rows
        .iter()
        .filter(|row| row.label != "mac-mini" && row.label != "mac-book")
        .collect();
    if !extras.is_empty() {
        out.push_str("\nOther nodes:\n");
        for extra in extras {
            out.push_str(&format!(
                "  - {label} (host={host}, instance={id}, role={role}, status={status}, pid={pid}, hb_age={hb})\n",
                label = extra.label,
                host = extra.hostname,
                id = extra.instance_id,
                role = extra.role,
                status = extra.status,
                pid = extra.pid,
                hb = iso_age(extra.last_heartbeat_at.as_deref()),
            ));
        }
    }
    out
}

/// `agentdesk machine-compare [--json]`
///
/// Side-by-side health/state table for every registered worker node. Built
/// from `/api/cluster/nodes` — no SSH, no shell-out, no `ssh user@host mtime`.
pub fn cmd_machine_compare(json_output: bool) -> Result<(), String> {
    let cluster = get_json("/api/cluster/nodes")?;
    let nodes = cluster
        .get("nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let lease_ttl_secs = cluster_lease_ttl_secs(cluster.get("cluster"));
    let rows: Vec<MachineRow> = nodes.iter().map(machine_row_from_node).collect();

    if json_output {
        // Emit a stable JSON shape: { cluster: {…}, rows: [ MachineRow… ] }
        let rows_json: Vec<Value> = rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "label": row.label,
                    "hostname": row.hostname,
                    "instance_id": row.instance_id,
                    "role": row.role,
                    "status": row.status,
                    "pid": row.pid,
                    "version": row.version,
                    "started_at": row.started_at,
                    "last_heartbeat_at": row.last_heartbeat_at,
                    "active_dispatch_count": row.active_dispatches,
                })
            })
            .collect();
        let payload = serde_json::json!({
            "cluster": cluster.get("cluster").cloned().unwrap_or(Value::Null),
            "lease_ttl_secs": lease_ttl_secs,
            "rows": rows_json,
        });
        print_json(&payload);
        return Ok(());
    }

    if rows.is_empty() {
        println!("No worker_node rows registered.");
        println!("(cluster.enabled may be false, or Postgres is unavailable.)");
        return Ok(());
    }

    println!("AgentDesk Machine Compare  (lease_ttl_secs={lease_ttl_secs})");
    print!("{}", render_machine_compare_table(&rows));
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
    let entries: Vec<Value> = groups
        .iter()
        .enumerate()
        .flat_map(|(thread_group, group)| {
            group
                .get("issues")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(move |issue| {
                    issue.as_i64().map(|issue_number| {
                        serde_json::json!({
                            "issue_number": issue_number,
                            "thread_group": thread_group as i64,
                            "batch_phase": 0,
                        })
                    })
                })
        })
        .collect();
    let mut body = serde_json::json!({
        "entries": entries,
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

    let mut value = post_json("/api/queue/generate", Some(body))?;
    if activate {
        let run_id = value
            .get("run")
            .and_then(|run| run.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| "invalid /api/queue/generate response: missing run.id".to_string())?
            .to_string();
        let mut activate_body = serde_json::json!({ "run_id": run_id });
        if unified {
            activate_body["unified_thread"] = serde_json::json!(true);
        }
        if let Some(agent_id) = agent_id.map(str::trim).filter(|value| !value.is_empty()) {
            activate_body["agent_id"] = serde_json::json!(agent_id);
        }
        if let Some(repo) = value
            .get("run")
            .and_then(|run| run.get("repo"))
            .and_then(Value::as_str)
            .filter(|repo| !repo.is_empty())
        {
            activate_body["repo"] = serde_json::json!(repo);
        }
        let dispatch = post_json("/api/queue/dispatch-next", Some(activate_body))?;
        if let Some(obj) = value.as_object_mut() {
            obj.insert("activated".to_string(), serde_json::json!(true));
            obj.insert("dispatch".to_string(), dispatch);
        }
    }
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

/// `agentdesk review-recover-target --dispatch <id> --commit <sha> --worktree <path>`
pub fn cmd_review_recover_target(
    dispatch_id: Option<&str>,
    card_id: Option<&str>,
    target_commit: Option<&str>,
    worktree_path: Option<&str>,
    reason: Option<&str>,
) -> Result<(), String> {
    let mut body = serde_json::Map::new();
    if let Some(value) = dispatch_id.map(str::trim).filter(|value| !value.is_empty()) {
        body.insert("dispatch_id".to_string(), serde_json::json!(value));
    }
    if let Some(value) = card_id.map(str::trim).filter(|value| !value.is_empty()) {
        body.insert("card_id".to_string(), serde_json::json!(value));
    }
    if let Some(value) = target_commit
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        body.insert("target_commit".to_string(), serde_json::json!(value));
    }
    if let Some(value) = worktree_path
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        body.insert("worktree_path".to_string(), serde_json::json!(value));
    }
    if let Some(value) = reason.map(str::trim).filter(|value| !value.is_empty()) {
        body.insert("reason".to_string(), serde_json::json!(value));
    }
    let value = post_json(
        "/api/reviews/recovery",
        Some(serde_json::Value::Object(body)),
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
        // #1671 — observability fields lifted from the watcher-state
        // endpoint so a single `diag` call surfaces stall fingerprints.
        "relay_stall_state",
        "inflight_age_secs",
        "pending_queue_depth",
        "task_notification_kind",
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

/// `agentdesk config sync-mcp`
pub fn cmd_config_sync_mcp() -> Result<(), String> {
    let root = crate::config::runtime_root()
        .ok_or_else(|| "Failed to resolve AGENTDESK_ROOT_DIR".to_string())?;
    crate::runtime_layout::ensure_runtime_layout(&root)?;
    let loaded = crate::services::discord_config_audit::load_runtime_config(&root)?;
    let config = loaded.config;

    let mut providers = Vec::new();
    let mut failures = Vec::new();
    for (provider, result) in [
        (
            "codex",
            crate::services::mcp_config::sync_codex_mcp_servers(&config),
        ),
        (
            "opencode",
            crate::services::mcp_config::sync_opencode_mcp_servers(&config),
        ),
        (
            "qwen",
            crate::services::mcp_config::sync_qwen_mcp_servers(&config),
        ),
        (
            "gemini",
            crate::services::mcp_config::sync_gemini_mcp_servers(&config),
        ),
    ] {
        match result {
            Ok(()) => providers.push(serde_json::json!({
                "provider": provider,
                "ok": true,
            })),
            Err(error) => {
                failures.push(provider.to_string());
                providers.push(serde_json::json!({
                    "provider": provider,
                    "ok": false,
                    "error": error,
                }));
            }
        }
    }

    print_json(&serde_json::json!({
        "ok": failures.is_empty(),
        "config_path": loaded.path,
        "providers": providers,
    }));

    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("MCP sync failed for {}", failures.join(", ")))
    }
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
    let data = get_json("/api/queue/status")?;
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
mod health_compare_tests {
    //! Unit tests for `cmd_health` / `cmd_machine_compare` helpers (issue
    //! #2656). These intentionally avoid touching the live HTTP API or
    //! `legacy-sqlite-tests` feature so they run in the default test profile.

    use super::*;
    use serde_json::json;

    #[test]
    fn human_age_seconds_buckets_match_expected_units() {
        assert_eq!(human_age_seconds(-5), "future");
        assert_eq!(human_age_seconds(0), "0s");
        assert_eq!(human_age_seconds(45), "45s");
        assert_eq!(human_age_seconds(60), "1m");
        assert_eq!(human_age_seconds(3_599), "59m");
        assert_eq!(human_age_seconds(3_600), "1h");
        assert_eq!(human_age_seconds(3_660), "1h1m");
        // > 48h flips to days.
        assert_eq!(human_age_seconds(60 * 60 * 49), "2d");
    }

    #[test]
    fn diff_marker_only_fires_on_real_mismatch() {
        assert_eq!(diff_marker("a", "a"), "");
        assert_eq!(diff_marker("-", "-"), "");
        assert_eq!(diff_marker("a", "b"), "!=");
        // A missing left side counts as a real diff — operators need to see it.
        assert_eq!(diff_marker("-", "b"), "!=");
    }

    #[test]
    fn classify_machine_label_prefers_explicit_labels() {
        let node = json!({
            "labels": ["mac-mini", "release"],
            "hostname": "mac-book.local",
        });
        assert_eq!(classify_machine_label(&node), "mac-mini");
    }

    #[test]
    fn classify_machine_label_falls_back_to_hostname() {
        let node = json!({"hostname": "mac-book"});
        assert_eq!(classify_machine_label(&node), "mac-book");
    }

    #[test]
    fn classify_machine_label_falls_back_to_instance_id_for_unknown_hosts() {
        let node = json!({"instance_id": "worker-x", "hostname": "linux-build-01"});
        // hostname doesn't match either alias, but isn't empty — we expose
        // it so operators can still see the row.
        assert_eq!(classify_machine_label(&node), "linux-build-01");
        let node = json!({"instance_id": "worker-x"});
        assert_eq!(classify_machine_label(&node), "worker-x");
    }

    #[test]
    fn render_machine_compare_table_synthesises_offline_columns() {
        // Only one machine has a live heartbeat — the other side should be
        // rendered as `(no heartbeat)` so the diff column is meaningful.
        let rows = vec![MachineRow {
            label: "mac-mini".to_string(),
            hostname: "mac-mini.local".to_string(),
            instance_id: "mac-mini-rel-1".to_string(),
            role: "leader".to_string(),
            status: "online".to_string(),
            pid: "1234".to_string(),
            version: "0.1.2".to_string(),
            started_at: Some("2026-05-19T00:00:00Z".to_string()),
            last_heartbeat_at: Some("2026-05-19T00:00:00Z".to_string()),
            active_dispatches: 3,
        }];
        let table = render_machine_compare_table(&rows);
        assert!(table.contains("mac-mini"));
        assert!(table.contains("mac-book"));
        assert!(table.contains("(no heartbeat)"));
        // The single-side row must still surface its pid so operators can
        // distinguish leader-only deployments from full clusters.
        assert!(table.contains("1234"));
    }

    #[test]
    fn cluster_lease_ttl_secs_handles_missing_and_zero_values() {
        assert_eq!(cluster_lease_ttl_secs(None), 60);
        assert_eq!(
            cluster_lease_ttl_secs(Some(&json!({"lease_ttl_secs": 0}))),
            60
        );
        assert_eq!(
            cluster_lease_ttl_secs(Some(&json!({"lease_ttl_secs": 42}))),
            42
        );
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::{
        build_cli_advance_completion_result, cmd_advance, cmd_dispatch, encode_path_segment,
        parse_github_repo_from_remote, render_auto_queue_status_lines, render_cards_table,
        render_queue_thread_links, runtime_config_payload,
    };
    use axum::extract::{Path, Query, State};
    use axum::routing::{get, patch, post};
    use axum::{Json, Router};
    use serde_json::{Value, json};
    use std::ffi::OsString;
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
        crate::services::git::GitCommand::new()
            .repo(repo_dir)
            .args(args)
            .run_output()
            .unwrap_or_else(|error| panic!("git {args:?} failed: {error}"));
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

    #[derive(Default)]
    struct DispatchMockState {
        generate: Option<serde_json::Value>,
        activate: Option<serde_json::Value>,
    }

    async fn dispatch_generate_handler(
        State(state): State<Arc<Mutex<DispatchMockState>>>,
        Json(body): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        state.lock().unwrap().generate = Some(body);
        Json(json!({
            "run": {"id": "run-dispatch", "repo": "itismyfield/AgentDesk", "status": "generated"},
            "entries": []
        }))
    }

    async fn dispatch_activate_handler(
        State(state): State<Arc<Mutex<DispatchMockState>>>,
        Json(body): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        state.lock().unwrap().activate = Some(body);
        Json(json!({
            "count": 1,
            "dispatched": []
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
    fn render_auto_queue_status_lines_includes_failed_and_skipped_counts() {
        let lines = render_auto_queue_status_lines(
            "completed for project-agentdesk",
            &[
                json!({"id": "entry-done", "status": "done", "card_id": "card-done", "github_issue_number": 1801}),
                json!({"id": "entry-failed", "status": "failed", "card_id": "card-failed", "github_issue_number": 1875}),
                json!({"id": "entry-skipped", "status": "skipped", "card_id": "card-skipped", "github_issue_number": 1807}),
            ],
        );

        assert_eq!(
            lines[0],
            "  Auto-Queue: completed for project-agentdesk | total=3 pending=0 dispatched=0 done=1 failed=1 skipped=1"
        );
        assert!(lines[1].contains("failed: entry=entry-failed card=card-failed issue=#1875"));
        assert!(lines[1].contains("skipped: entry=entry-skipped card=card-skipped issue=#1807"));
    }

    #[test]
    fn render_auto_queue_status_lines_omits_attention_when_clear() {
        let lines = render_auto_queue_status_lines(
            "idle",
            &[json!({"id": "entry-done", "status": "done", "card_id": "card-done"})],
        );

        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0],
            "  Auto-Queue: idle | total=1 pending=0 dispatched=0 done=1 failed=0 skipped=0"
        );
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

            let captured = Arc::new(Mutex::new(DispatchMockState::default()));
            let app = Router::new()
                .route("/api/queue/generate", post(dispatch_generate_handler))
                .route("/api/queue/dispatch-next", post(dispatch_activate_handler))
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

            let state = captured.lock().unwrap();
            let generate_payload = state
                .generate
                .as_ref()
                .expect("generate payload must be captured");
            assert_eq!(generate_payload["repo"], "itismyfield/AgentDesk");
            assert_eq!(generate_payload["agent_id"], "project-agentdesk");
            assert_eq!(generate_payload["auto_assign_agent"], true);
            assert_eq!(generate_payload["unified_thread"], true);
            assert_eq!(generate_payload["max_concurrent_threads"], 2);
            assert_eq!(generate_payload["entries"][0]["issue_number"], 423);
            assert_eq!(generate_payload["entries"][0]["thread_group"], 0);
            assert_eq!(generate_payload["entries"][1]["issue_number"], 405);
            assert_eq!(generate_payload["entries"][1]["thread_group"], 0);
            assert_eq!(generate_payload["entries"][2]["issue_number"], 407);
            assert_eq!(generate_payload["entries"][2]["thread_group"], 1);

            let activate_payload = state
                .activate
                .as_ref()
                .expect("dispatch-next payload must be captured");
            assert_eq!(activate_payload["run_id"], "run-dispatch");
            assert_eq!(activate_payload["repo"], "itismyfield/AgentDesk");
            assert_eq!(activate_payload["agent_id"], "project-agentdesk");
            assert_eq!(activate_payload["unified_thread"], true);
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
