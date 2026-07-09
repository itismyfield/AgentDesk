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
            return Err(status_error_message(code, &body));
        }
        Err(ureq::Error::Transport(err)) => {
            return Err(connection_error_hint(
                &format!("Request failed: {err}"),
                &api_base(),
                "AGENTDESK_API_URL",
            ));
        }
    };

    resp.into_json().map_err(|e| format!("Parse error: {e}"))
}

/// Assemble the error message for an HTTP *status* failure — the server
/// answered with a non-2xx code. Deliberately hint-free: the server is
/// reachable, so a "dcserver not running" hint would only mislead.
fn status_error_message(code: u16, body: &str) -> String {
    parse_error_message(body)
        .map(|msg| format!("Request failed ({code}): {msg}"))
        .unwrap_or_else(|| format!("Request failed ({code})"))
}

/// Append an advisory connection hint to a raw transport-error message.
///
/// Pure + `pub(crate)` so `monitoring.rs` reuses the exact same wording and
/// both can be unit-tested. Only the ureq `Transport` branch calls this — an
/// HTTP status response means the server *did* answer, so it stays hint-free.
/// The tone is advisory ("may not be running"), never a flat assertion that
/// the server is down.
///
/// `env_hint` names the environment variable(s) the *caller's* `api_base()`
/// actually honors — client.rs resolves `AGENTDESK_API_URL` only, while
/// monitoring.rs prefers `ADK_API_URL` over `AGENTDESK_API_URL` — so the hint
/// never steers the operator toward a variable that path would ignore.
pub(crate) fn connection_error_hint(raw: &str, effective_url: &str, env_hint: &str) -> String {
    format!(
        "{raw}\n  힌트: dcserver가 실행 중이 아닐 수 있습니다 — `agentdesk doctor`로 진단하고, \
         접속 URL(현재: {effective_url})이 맞는지 {env_hint} 환경변수를 확인하세요."
    )
}

#[cfg(test)]
mod connection_hint_tests {
    use super::*;

    #[test]
    fn transport_error_gets_connection_hint() {
        // client.rs path — api_base() honors AGENTDESK_API_URL only.
        let msg = connection_error_hint(
            "Request failed: Network Error: Connection refused (os error 61)",
            "http://127.0.0.1:8791",
            "AGENTDESK_API_URL",
        );
        // Raw error is preserved verbatim …
        assert!(msg.contains("Connection refused"));
        // … then the advisory hint is appended.
        assert!(msg.contains("agentdesk doctor"));
        assert!(msg.contains("AGENTDESK_API_URL"));
        assert!(msg.contains("http://127.0.0.1:8791"));
        // Advisory, not a flat "server is down" assertion.
        assert!(msg.contains("아닐 수 있습니다"));
        // The client path must not mention ADK_API_URL — its api_base()
        // never reads it.
        assert!(!msg.contains("ADK_API_URL(우선)"));
    }

    #[test]
    fn monitoring_env_hint_mentions_both_vars_in_priority_order() {
        // monitoring.rs path — its api_base() prefers ADK_API_URL over
        // AGENTDESK_API_URL, and the hint must say so.
        let msg = connection_error_hint(
            "monitoring API request failed: Connection refused",
            "http://127.0.0.1:8791",
            "ADK_API_URL(우선) 또는 AGENTDESK_API_URL",
        );
        assert!(msg.contains("agentdesk doctor"));
        assert!(msg.contains("ADK_API_URL(우선) 또는 AGENTDESK_API_URL"));
        assert!(msg.contains("http://127.0.0.1:8791"));
        assert!(msg.contains("아닐 수 있습니다"));
    }

    #[test]
    fn status_error_keeps_server_detail_without_hint() {
        let msg = status_error_message(404, r#"{"error":"card not found"}"#);
        assert!(msg.contains("404"));
        assert!(msg.contains("card not found"));
        // Server answered — no connection hint.
        assert!(!msg.contains("agentdesk doctor"));
        assert!(!msg.contains("AGENTDESK_API_URL"));
    }

    #[test]
    fn status_error_without_body_is_hint_free() {
        let msg = status_error_message(500, "");
        assert!(msg.contains("500"));
        assert!(!msg.contains("agentdesk doctor"));
        assert!(!msg.contains("AGENTDESK_API_URL"));
    }
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
/// Ensure a `status` API response carries the arrays both the text and JSON
/// renderers depend on. Shared by both modes so a malformed/missing payload
/// fails identically (#4372 r1) instead of the JSON path silently masking
/// server garbage as zero counts and exiting 0.
fn validate_status_payload(sessions: &Value, queue: &Value) -> Result<(), String> {
    if sessions.get("sessions").and_then(Value::as_array).is_none() {
        return Err("invalid /api/dispatched-sessions response".to_string());
    }
    if queue.get("entries").and_then(Value::as_array).is_none() {
        return Err("invalid /api/queue/status response".to_string());
    }
    Ok(())
}

pub fn cmd_status(json: bool) -> Result<(), String> {
    let health = get_json("/api/health")?;
    let sessions = get_json("/api/dispatched-sessions?include_merged=1")?;
    let queue = get_json("/api/queue/status")?;

    // Validate shape up front so `--json` fails on a malformed payload exactly
    // as text mode does — never mask it as an all-zero success (#4372 r1).
    validate_status_payload(&sessions, &queue)?;

    if json {
        print_json(&super::json_output::status(
            &api_base(),
            &health,
            &sessions,
            &queue,
        ));
        return Ok(());
    }

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
// `agentdesk activity --since … [--until …]`  (issue #2658)
//
// Time-windowed activity report that fuses four data sources:
//   1. git log         — commits landed on the local default branch
//   2. gh issue list   — issues closed inside the window
//   3. gh pr list      — PRs merged inside the window
//   4. AgentDesk API   — deploys (worker_node started_at) + recent incidents
//                        (high_risk_recovery / manual_intervention events)
//
// Sources 1–3 are always available; source 4 is best-effort and silently
// skipped when the local API is unreachable (or when `--no-agentdesk` is set).
// ---------------------------------------------------------------------------
/// Parse `--since` / `--until` into RFC3339 timestamps.
///
/// Accepted forms:
///   * RFC3339 (`2026-05-19T23:10:00+09:00`)
///   * Duration suffix relative to `now()` — `12h`, `90m`, `7d`, `3600s`
///   * `since:<short-sha>` — anchors on a commit's author time
fn parse_window_anchor(
    raw: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<chrono::DateTime<chrono::Utc>, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("activity window: empty timestamp".to_string());
    }
    if let Some(sha) = trimmed.strip_prefix("since:") {
        let output = crate::services::git::GitCommand::new()
            .args(["show", "-s", "--format=%cI", sha.trim()])
            .run_output()
            .map_err(|err| format!("git show {sha}: {err}"))?;
        if !output.status.success() {
            return Err(format!(
                "git show {sha} exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let ts = String::from_utf8_lossy(&output.stdout).trim().to_string();
        return chrono::DateTime::parse_from_rfc3339(&ts)
            .map(|parsed| parsed.with_timezone(&chrono::Utc))
            .map_err(|err| format!("git anchor decode: {err}"));
    }
    if let Some(secs) = parse_duration_suffix(trimmed) {
        return Ok(now - chrono::Duration::seconds(secs as i64));
    }
    chrono::DateTime::parse_from_rfc3339(trimmed)
        .map(|parsed| parsed.with_timezone(&chrono::Utc))
        .map_err(|err| format!("activity window: cannot parse '{trimmed}' ({err})"))
}
/// Accepts `123s`, `45m`, `12h`, `7d` (single-unit suffix only). Returns `None`
/// for anything that doesn't look like a bare duration — the caller then falls
/// back to RFC3339 parsing, so misclassification is benign.
fn parse_duration_suffix(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let last = raw.chars().last()?;
    let (num, multiplier) = match last {
        's' => (&raw[..raw.len() - 1], 1u64),
        'm' => (&raw[..raw.len() - 1], 60),
        'h' => (&raw[..raw.len() - 1], 60 * 60),
        'd' => (&raw[..raw.len() - 1], 60 * 60 * 24),
        _ => return None,
    };
    let value: u64 = num.trim().parse().ok()?;
    value.checked_mul(multiplier)
}
#[derive(Debug, Clone)]
struct ActivityEntry {
    kind: &'static str, // "commit" | "issue" | "pr" | "deploy" | "incident"
    timestamp: chrono::DateTime<chrono::Utc>,
    ref_label: String, // e.g. "abc1234", "#2658", "PR #1234"
    summary: String,
    actor: String,
}
fn shell_quote_simple(value: &str) -> String {
    // Used for human-readable log output only — not for shell-out.
    value.replace('\n', " ").replace('\r', " ")
}
fn collect_git_commits(
    since: chrono::DateTime<chrono::Utc>,
    until: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<ActivityEntry>, String> {
    let since_arg = format!("--since={}", since.to_rfc3339());
    let until_arg = format!("--until={}", until.to_rfc3339());
    let pretty = "--pretty=format:%H\x1f%cI\x1f%an\x1f%s".to_string();
    let output = crate::services::git::GitCommand::new()
        .args([
            "log",
            "--no-merges",
            since_arg.as_str(),
            until_arg.as_str(),
            pretty.as_str(),
        ])
        .run_output()
        .map_err(|err| format!("git log: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "git log exited with {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut entries = Vec::new();
    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(4, '\x1f').collect();
        if parts.len() != 4 {
            continue;
        }
        let Ok(ts) = chrono::DateTime::parse_from_rfc3339(parts[1]) else {
            continue;
        };
        let sha_short = parts[0].chars().take(7).collect::<String>();
        entries.push(ActivityEntry {
            kind: "commit",
            timestamp: ts.with_timezone(&chrono::Utc),
            ref_label: sha_short,
            summary: shell_quote_simple(parts[3]),
            actor: parts[2].to_string(),
        });
    }
    Ok(entries)
}
fn run_gh_capture(args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new("gh")
        .args(args)
        .output()
        .map_err(|err| format!("gh CLI failed to execute: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "gh exited with {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    String::from_utf8(output.stdout).map_err(|err| format!("gh stdout decode: {err}"))
}
fn collect_closed_issues(
    repo: &str,
    since: chrono::DateTime<chrono::Utc>,
    until: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<ActivityEntry>, String> {
    // `gh issue list --search closed:>=…` is the documented way to filter on
    // close time. We rely on JSON output so we don't have to scrape pretty
    // formatting.
    let search = format!(
        "is:issue closed:{}..{} sort:closed-desc",
        since.format("%Y-%m-%dT%H:%M:%S%z"),
        until.format("%Y-%m-%dT%H:%M:%S%z"),
    );
    let raw = run_gh_capture(&[
        "issue",
        "list",
        "--repo",
        repo,
        "--state",
        "closed",
        "--limit",
        "200",
        "--search",
        search.as_str(),
        "--json",
        "number,title,closedAt,author",
    ])?;
    let value: Value = serde_json::from_str(&raw).map_err(|err| format!("gh issue json: {err}"))?;
    let arr = value.as_array().cloned().unwrap_or_default();
    let mut out = Vec::new();
    for item in arr {
        let number = item.get("number").and_then(Value::as_i64).unwrap_or(0);
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let closed = item
            .get("closedAt")
            .and_then(Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|ts| ts.with_timezone(&chrono::Utc));
        let Some(closed) = closed else { continue };
        if closed < since || closed >= until {
            // gh search rounds to date-precision — re-filter precisely.
            continue;
        }
        let actor = item
            .get("author")
            .and_then(|a| a.get("login"))
            .and_then(Value::as_str)
            .unwrap_or("-")
            .to_string();
        out.push(ActivityEntry {
            kind: "issue",
            timestamp: closed,
            ref_label: format!("#{number}"),
            summary: shell_quote_simple(&title),
            actor,
        });
    }
    Ok(out)
}
fn collect_merged_prs(
    repo: &str,
    since: chrono::DateTime<chrono::Utc>,
    until: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<ActivityEntry>, String> {
    let search = format!(
        "is:pr is:merged merged:{}..{} sort:updated-desc",
        since.format("%Y-%m-%dT%H:%M:%S%z"),
        until.format("%Y-%m-%dT%H:%M:%S%z"),
    );
    let raw = run_gh_capture(&[
        "pr",
        "list",
        "--repo",
        repo,
        "--state",
        "merged",
        "--limit",
        "200",
        "--search",
        search.as_str(),
        "--json",
        "number,title,mergedAt,author",
    ])?;
    let value: Value = serde_json::from_str(&raw).map_err(|err| format!("gh pr json: {err}"))?;
    let arr = value.as_array().cloned().unwrap_or_default();
    let mut out = Vec::new();
    for item in arr {
        let number = item.get("number").and_then(Value::as_i64).unwrap_or(0);
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let merged = item
            .get("mergedAt")
            .and_then(Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|ts| ts.with_timezone(&chrono::Utc));
        let Some(merged) = merged else { continue };
        if merged < since || merged >= until {
            continue;
        }
        let actor = item
            .get("author")
            .and_then(|a| a.get("login"))
            .and_then(Value::as_str)
            .unwrap_or("-")
            .to_string();
        out.push(ActivityEntry {
            kind: "pr",
            timestamp: merged,
            ref_label: format!("PR #{number}"),
            summary: shell_quote_simple(&title),
            actor,
        });
    }
    Ok(out)
}
fn collect_agentdesk_events(
    since: chrono::DateTime<chrono::Utc>,
    until: chrono::DateTime<chrono::Utc>,
) -> Vec<ActivityEntry> {
    let mut out = Vec::new();
    // Deploys: worker_node started_at falling inside the window is, in the
    // current ops model, the closest stable signal of "node was redeployed".
    // The cluster endpoint may be missing (PG offline / cluster disabled),
    // in which case we silently skip the section rather than crashing the
    // whole activity report.
    if let Ok(cluster) = get_json("/api/cluster/nodes") {
        if let Some(nodes) = cluster.get("nodes").and_then(Value::as_array) {
            for node in nodes {
                let Some(started_at_str) = node.get("started_at").and_then(Value::as_str) else {
                    continue;
                };
                let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(started_at_str) else {
                    continue;
                };
                let started = parsed.with_timezone(&chrono::Utc);
                if started < since || started >= until {
                    continue;
                }
                let instance = node
                    .get("instance_id")
                    .and_then(Value::as_str)
                    .unwrap_or("-")
                    .to_string();
                let role = node
                    .get("effective_role")
                    .and_then(Value::as_str)
                    .or_else(|| node.get("role").and_then(Value::as_str))
                    .unwrap_or("-")
                    .to_string();
                out.push(ActivityEntry {
                    kind: "deploy",
                    timestamp: started,
                    ref_label: instance.clone(),
                    summary: format!("worker node restarted (role={role})"),
                    actor: instance,
                });
            }
        }
    }
    // Incidents: failed dispatch outbox rows. Each row carries a
    // last_error_at / last_error / target tuple — we surface anything that
    // failed within the window so deploy postmortems land in the same view
    // as the deploy itself.
    if let Ok(failures) = get_json("/api/dispatch-outbox/failures") {
        if let Some(rows) = failures.get("rows").and_then(Value::as_array) {
            for row in rows {
                let Some(ts_str) = row
                    .get("last_error_at")
                    .and_then(Value::as_str)
                    .or_else(|| row.get("updated_at").and_then(Value::as_str))
                else {
                    continue;
                };
                let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(ts_str) else {
                    continue;
                };
                let when = parsed.with_timezone(&chrono::Utc);
                if when < since || when >= until {
                    continue;
                }
                let label = row
                    .get("id")
                    .and_then(Value::as_str)
                    .map(|id| id.chars().take(8).collect::<String>())
                    .unwrap_or_else(|| "-".to_string());
                let target = row
                    .get("target")
                    .and_then(Value::as_str)
                    .unwrap_or("-")
                    .to_string();
                let detail = row
                    .get("last_error")
                    .and_then(Value::as_str)
                    .unwrap_or("dispatch_outbox failure");
                out.push(ActivityEntry {
                    kind: "incident",
                    timestamp: when,
                    ref_label: label,
                    summary: format!("{} → {}", target, shell_quote_simple(detail)),
                    actor: "dispatch_outbox".to_string(),
                });
            }
        }
    }
    out
}
fn render_activity_table(entries: &[ActivityEntry]) -> String {
    if entries.is_empty() {
        return "(no activity in the requested window)\n".to_string();
    }
    let rows: Vec<[String; 5]> = entries
        .iter()
        .map(|entry| {
            [
                entry.kind.to_string(),
                entry.timestamp.format("%Y-%m-%d %H:%M").to_string(),
                entry.ref_label.clone(),
                entry.actor.clone(),
                entry.summary.clone(),
            ]
        })
        .collect();
    let kind_w = rows
        .iter()
        .map(|r| r[0].chars().count())
        .max()
        .unwrap_or(4)
        .max(4);
    let ts_w = rows
        .iter()
        .map(|r| r[1].chars().count())
        .max()
        .unwrap_or(16)
        .max(16);
    let ref_w = rows
        .iter()
        .map(|r| r[2].chars().count())
        .max()
        .unwrap_or(8)
        .clamp(8, 18);
    let actor_w = rows
        .iter()
        .map(|r| r[3].chars().count())
        .max()
        .unwrap_or(6)
        .clamp(6, 24);
    let summary_w = 80usize;
    let mut out = String::new();
    out.push_str(&format!(
        "{}  {}  {}  {}  {}\n",
        pad_to("kind", kind_w),
        pad_to("when (UTC)", ts_w),
        pad_to("ref", ref_w),
        pad_to("actor", actor_w),
        "summary",
    ));
    out.push_str(&format!(
        "{}  {}  {}  {}  {}\n",
        "-".repeat(kind_w),
        "-".repeat(ts_w),
        "-".repeat(ref_w),
        "-".repeat(actor_w),
        "-".repeat(summary_w.min(40)),
    ));
    for row in &rows {
        let summary = truncate_cell(&row[4], summary_w);
        out.push_str(&format!(
            "{}  {}  {}  {}  {}\n",
            pad_to(&row[0], kind_w),
            pad_to(&row[1], ts_w),
            pad_to(&row[2], ref_w),
            pad_to(&row[3], actor_w),
            summary,
        ));
    }
    out
}
/// `agentdesk activity --since … [--until …] [--repo …] [--json]`
pub fn cmd_activity(
    since_raw: &str,
    until_raw: Option<&str>,
    repo: Option<&str>,
    json_output: bool,
    no_agentdesk: bool,
) -> Result<(), String> {
    let now = chrono::Utc::now();
    let since = parse_window_anchor(since_raw, now)?;
    let until = match until_raw {
        Some(raw) => parse_window_anchor(raw, now)?,
        None => now,
    };
    if until <= since {
        return Err(format!(
            "activity: --until ({}) must be strictly after --since ({})",
            until.to_rfc3339(),
            since.to_rfc3339()
        ));
    }
    let repo_full = repo
        .map(str::to_string)
        .or_else(|| infer_dispatch_repo(None))
        .ok_or_else(|| "activity: --repo required (no origin remote detected)".to_string())?;
    let mut warnings: Vec<String> = Vec::new();
    let mut entries: Vec<ActivityEntry> = Vec::new();
    match collect_git_commits(since, until) {
        Ok(mut commits) => entries.append(&mut commits),
        Err(err) => warnings.push(format!("git log failed: {err}")),
    }
    match collect_closed_issues(&repo_full, since, until) {
        Ok(mut issues) => entries.append(&mut issues),
        Err(err) => warnings.push(format!("gh issue list failed: {err}")),
    }
    match collect_merged_prs(&repo_full, since, until) {
        Ok(mut prs) => entries.append(&mut prs),
        Err(err) => warnings.push(format!("gh pr list failed: {err}")),
    }
    if !no_agentdesk {
        let mut adk = collect_agentdesk_events(since, until);
        entries.append(&mut adk);
    }
    // Sort most-recent first so eyeballing the table during an outage
    // surfaces fresh signals at the top.
    entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    if json_output {
        let payload = serde_json::json!({
            "since": since.to_rfc3339(),
            "until": until.to_rfc3339(),
            "repo": repo_full,
            "warnings": warnings,
            "counts": activity_counts(&entries),
            "entries": entries.iter().map(activity_entry_to_json).collect::<Vec<_>>(),
        });
        print_json(&payload);
        return Ok(());
    }
    println!(
        "AgentDesk Activity  {since} → {until}  ({repo})",
        since = since.to_rfc3339(),
        until = until.to_rfc3339(),
        repo = repo_full,
    );
    let counts = activity_counts(&entries);
    println!(
        "  totals: commits={} issues={} prs={} deploys={} incidents={}",
        counts.get("commit").copied().unwrap_or(0),
        counts.get("issue").copied().unwrap_or(0),
        counts.get("pr").copied().unwrap_or(0),
        counts.get("deploy").copied().unwrap_or(0),
        counts.get("incident").copied().unwrap_or(0),
    );
    for warning in &warnings {
        println!("  warning: {warning}");
    }
    print!("{}", render_activity_table(&entries));
    Ok(())
}
fn activity_counts(entries: &[ActivityEntry]) -> std::collections::HashMap<&'static str, u64> {
    let mut counts: std::collections::HashMap<&'static str, u64> = std::collections::HashMap::new();
    for entry in entries {
        *counts.entry(entry.kind).or_insert(0) += 1;
    }
    counts
}
fn activity_entry_to_json(entry: &ActivityEntry) -> Value {
    serde_json::json!({
        "kind": entry.kind,
        "timestamp": entry.timestamp.to_rfc3339(),
        "ref": entry.ref_label,
        "actor": entry.actor,
        "summary": entry.summary,
    })
}
#[cfg(test)]
mod activity_tests {
    use super::*;
    #[test]
    fn parse_duration_suffix_handles_each_unit() {
        assert_eq!(parse_duration_suffix("10s"), Some(10));
        assert_eq!(parse_duration_suffix("3m"), Some(180));
        assert_eq!(parse_duration_suffix("2h"), Some(7_200));
        assert_eq!(parse_duration_suffix("1d"), Some(86_400));
        assert_eq!(parse_duration_suffix(""), None);
        assert_eq!(parse_duration_suffix("abc"), None);
        // Bare integers are not a duration — caller falls back to RFC3339.
        assert_eq!(parse_duration_suffix("3600"), None);
    }
    #[test]
    fn parse_window_anchor_rejects_garbage() {
        let now = chrono::Utc::now();
        let err = parse_window_anchor("not-a-time", now).unwrap_err();
        assert!(err.contains("cannot parse"));
    }
    #[test]
    fn parse_window_anchor_handles_duration_against_now() {
        let now = chrono::Utc::now();
        let parsed = parse_window_anchor("1h", now).unwrap();
        let diff = (now - parsed).num_seconds();
        assert!((3599..=3601).contains(&diff), "diff was {diff}");
    }
    #[test]
    fn parse_window_anchor_accepts_rfc3339() {
        let now = chrono::Utc::now();
        let parsed = parse_window_anchor("2026-05-19T23:10:00+09:00", now).unwrap();
        assert_eq!(
            parsed.format("%Y-%m-%d %H:%M").to_string(),
            "2026-05-19 14:10"
        );
    }
    #[test]
    fn activity_counts_buckets_by_kind() {
        let now = chrono::Utc::now();
        let entries = vec![
            ActivityEntry {
                kind: "commit",
                timestamp: now,
                ref_label: "abc".into(),
                summary: "x".into(),
                actor: "y".into(),
            },
            ActivityEntry {
                kind: "commit",
                timestamp: now,
                ref_label: "def".into(),
                summary: "x".into(),
                actor: "y".into(),
            },
            ActivityEntry {
                kind: "pr",
                timestamp: now,
                ref_label: "PR #1".into(),
                summary: "x".into(),
                actor: "y".into(),
            },
        ];
        let counts = activity_counts(&entries);
        assert_eq!(counts.get("commit").copied(), Some(2));
        assert_eq!(counts.get("pr").copied(), Some(1));
        assert_eq!(counts.get("issue").copied(), None);
    }
    #[test]
    fn render_activity_table_handles_empty_window() {
        let table = render_activity_table(&[]);
        assert!(table.contains("no activity"));
    }
    #[test]
    fn render_activity_table_contains_kind_and_ref() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-05-19T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let entries = vec![ActivityEntry {
            kind: "pr",
            timestamp: now,
            ref_label: "PR #42".into(),
            summary: "ship it".into(),
            actor: "alice".into(),
        }];
        let table = render_activity_table(&entries);
        assert!(table.contains("PR #42"));
        assert!(table.contains("ship it"));
        assert!(table.contains("alice"));
        assert!(table.contains("2026-05-19 12:00"));
    }
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
pub fn cmd_cards(status: Option<&str>, json: bool) -> Result<(), String> {
    let path = match status {
        Some(s) => format!("/api/kanban-cards?status={s}"),
        None => "/api/kanban-cards".to_string(),
    };
    let value = get_json(&path)?;
    let cards = value
        .get("cards")
        .and_then(Value::as_array)
        .ok_or_else(|| "invalid /api/kanban-cards response".to_string())?;
    if json {
        print_json(&super::json_output::cards(cards));
        return Ok(());
    }
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
pub fn cmd_advance(issue_number: &str, json: bool) -> Result<(), String> {
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
        if !json {
            println!("Completing dispatch {did}...");
        }
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
            if json {
                print_json(&super::json_output::advance(
                    issue_number,
                    card_id,
                    "advanced_to_review",
                    review_dispatch_id,
                    Some(did),
                ));
            } else {
                println!("✅ #{issue_number} advanced to review (dispatch: {review_dispatch_id})");
            }
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
            if json {
                print_json(&super::json_output::advance(
                    issue_number,
                    card_id,
                    "already_in_review",
                    review_dispatch_id,
                    None,
                ));
            } else {
                println!(
                    "✅ #{issue_number} already has an active review dispatch ({review_dispatch_id})"
                );
            }
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
pub fn cmd_queue(json: bool) -> Result<(), String> {
    let data = get_json("/api/queue/status")?;
    let entries = data["entries"].as_array().ok_or("No entries")?;
    if json {
        print_json(&super::json_output::queue(&data));
        return Ok(());
    }
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

/// `agentdesk phase status` — phase-gate violation snapshot (issue #2657).
///
/// Read-only call against `/api/queue/phase-gates/violations`. Empty
/// `violations` is the clean state; partial DB failure surfaces as a
/// non-zero exit via the `error` field.
pub fn cmd_phase_status(json: bool, detailed: bool) -> Result<(), String> {
    let raw = get_json("/api/queue/phase-gates/violations")?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&raw).unwrap_or_else(|_| raw.to_string())
        );
        return Ok(());
    }

    let empty: Vec<Value> = Vec::new();
    let violations = raw
        .get("violations")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let runs_scanned = raw.get("runs_scanned").and_then(Value::as_i64).unwrap_or(0);
    let complete = raw
        .get("complete")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if violations.is_empty() {
        println!("phase-gate: clean (runs scanned: {runs_scanned}, complete: {complete})");
        return Ok(());
    }

    println!(
        "phase-gate violations: {} (runs scanned: {runs_scanned})",
        violations.len()
    );
    for v in violations {
        if detailed {
            let run_id = v.get("run_id").and_then(Value::as_str).unwrap_or("-");
            let entry_id = v.get("entry_id").and_then(Value::as_str).unwrap_or("-");
            let card_id = v
                .get("kanban_card_id")
                .and_then(Value::as_str)
                .unwrap_or("-");
            let entry_phase = v
                .get("entry_batch_phase")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let current_phase = v
                .get("current_batch_phase")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let dispatch_id = v.get("dispatch_id").and_then(Value::as_str).unwrap_or("-");
            println!(
                "- run={run_id} entry={entry_id} card={card_id} phase={entry_phase}>current={current_phase} dispatch={dispatch_id}"
            );
        } else {
            let summary = v.get("summary").and_then(Value::as_str).unwrap_or("");
            println!("- {summary}");
        }
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
    json: bool,
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

    if json {
        print_json(&super::json_output::terminations(events));
        return Ok(());
    }

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
    //! the removed SQLite-only feature so they run in the default test profile.

    use super::*;
    use serde_json::json;

    #[test]
    fn validate_status_payload_rejects_missing_arrays() {
        let sessions = json!({"sessions": []});
        let queue = json!({"entries": []});
        // Well-formed payload passes in both modes.
        assert!(validate_status_payload(&sessions, &queue).is_ok());
        // Missing `sessions` array → error (so `status --json` fails, exits
        // nonzero via exit_for_json_cli, instead of masking it as zero counts).
        let err = validate_status_payload(&json!({}), &queue).unwrap_err();
        assert!(err.contains("dispatched-sessions"), "got: {err}");
        // Non-array `entries` → error.
        let err = validate_status_payload(&sessions, &json!({"entries": "nope"})).unwrap_err();
        assert!(err.contains("queue/status"), "got: {err}");
    }

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
