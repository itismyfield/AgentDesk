use serde_json::{Value, json};

pub fn agent_office_json(
    id: String,
    name: Option<String>,
    layout: Option<String>,
    office_department_id: Option<String>,
    joined_at: Option<String>,
) -> Value {
    json!({
        "id": id,
        "name": name,
        "layout": layout,
        "assigned": true,
        "office_department_id": office_department_id,
        "joined_at": joined_at,
    })
}

pub fn agent_skill_json(
    id: String,
    name: Option<String>,
    description: Option<String>,
    source_path: Option<String>,
    trigger_patterns: Option<String>,
    updated_at: Option<String>,
) -> Value {
    json!({
        "id": id,
        "name": name,
        "description": description,
        "source_path": source_path,
        "trigger_patterns": trigger_patterns,
        "updated_at": updated_at,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn dispatched_session_json(
    id: i64,
    session_key: Option<String>,
    agent_id: Option<String>,
    provider: Option<String>,
    status: &str,
    active_dispatch_id: Option<String>,
    model: Option<String>,
    tokens: i64,
    cwd: Option<String>,
    last_heartbeat: Option<String>,
    thread_channel_id: Option<String>,
    guild_id: Option<String>,
    channel_web_url: Option<String>,
    channel_deeplink_url: Option<String>,
    kanban_card_id: Option<String>,
) -> Value {
    json!({
        "id": id,
        "session_key": session_key,
        "agent_id": agent_id,
        "provider": provider,
        "status": status,
        "active_dispatch_id": active_dispatch_id,
        "model": model,
        "tokens": tokens,
        "cwd": cwd,
        "last_heartbeat": last_heartbeat,
        "thread_channel_id": thread_channel_id.clone(),
        "channel_id": thread_channel_id.clone(),
        "thread_id": thread_channel_id,
        "guild_id": guild_id,
        "channel_web_url": channel_web_url.clone(),
        "channel_deeplink_url": channel_deeplink_url.clone(),
        "deeplink_url": channel_web_url,
        "thread_deeplink_url": channel_deeplink_url,
        "kanban_card_id": kanban_card_id,
    })
}

pub fn timeline_event_json(
    id: String,
    source: String,
    event_type: String,
    title: Option<String>,
    status: Option<String>,
    timestamp: Option<i64>,
    duration_ms: Option<i64>,
) -> Value {
    json!({
        "id": id,
        "source": source,
        "type": event_type,
        "title": title,
        "status": status,
        "timestamp": timestamp,
        "duration_ms": duration_ms,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn transcript_json(
    id: i64,
    turn_id: String,
    session_key: Option<String>,
    channel_id: Option<String>,
    agent_id: Option<String>,
    provider: Option<String>,
    dispatch_id: Option<String>,
    kanban_card_id: Option<String>,
    dispatch_title: Option<String>,
    card_title: Option<String>,
    github_issue_number: Option<i64>,
    user_message: String,
    assistant_message: String,
    events: Value,
    duration_ms: Option<i64>,
    created_at: String,
) -> Value {
    json!({
        "id": id,
        "turn_id": turn_id,
        "session_key": session_key,
        "channel_id": channel_id,
        "agent_id": agent_id,
        "provider": provider,
        "dispatch_id": dispatch_id,
        "kanban_card_id": kanban_card_id,
        "dispatch_title": dispatch_title,
        "card_title": card_title,
        "github_issue_number": github_issue_number,
        "user_message": user_message,
        "assistant_message": assistant_message,
        "events": events,
        "duration_ms": duration_ms,
        "created_at": created_at,
    })
}

/// Issue #1241: dedupe dispatched-session rows by `(channel_id, agent_id)`.
///
/// The previous key was `(channel_id, provider)`; that let two rows for the
/// same agent in the same Discord channel survive whenever a stale alt-provider
/// session lingered. Using `(channel_id, agent_id)` collapses each agent to one
/// canonical row per channel even when a legacy session row carries a different
/// provider snapshot.
pub fn dedup_dispatched_sessions(resolved: Vec<Value>) -> Vec<Value> {
    fn effective_priority(value: &Value) -> u8 {
        let status = value.get("status").and_then(|v| v.as_str()).unwrap_or("");
        let has_dispatch = value
            .get("active_dispatch_id")
            .map(|v| !v.is_null())
            .unwrap_or(false);
        match status {
            "working" => 0,
            _ if has_dispatch => 1,
            "idle" => 2,
            _ => 3,
        }
    }

    let mut best_index_for_key: std::collections::HashMap<(String, String), usize> =
        std::collections::HashMap::new();
    let mut keep: Vec<bool> = vec![true; resolved.len()];
    for (idx, value) in resolved.iter().enumerate() {
        let channel = value
            .get("channel_id")
            .and_then(|v| v.as_str())
            .or_else(|| value.get("thread_channel_id").and_then(|v| v.as_str()));
        let agent_id = value.get("agent_id").and_then(|v| v.as_str());
        if let (Some(cid), Some(aid)) = (channel, agent_id) {
            let key = (cid.to_string(), aid.to_string());
            match best_index_for_key.get(&key) {
                None => {
                    best_index_for_key.insert(key, idx);
                }
                Some(&prev_idx) => {
                    let prev_priority = effective_priority(&resolved[prev_idx]);
                    let curr_priority = effective_priority(value);
                    if curr_priority < prev_priority {
                        keep[prev_idx] = false;
                        best_index_for_key.insert(key, idx);
                    } else {
                        keep[idx] = false;
                    }
                }
            }
        }
    }

    resolved
        .into_iter()
        .enumerate()
        .filter_map(|(idx, value)| if keep[idx] { Some(value) } else { None })
        .collect()
}

/// Build Discord web and deep-link URLs for a channel. Returns `(None, None)`
/// when either channel id or guild id is missing so callers can render a plain
/// text fallback.
pub fn build_channel_deeplinks(
    channel_id: Option<&str>,
    guild_id: Option<&str>,
) -> (Option<String>, Option<String>) {
    let channel = channel_id.map(str::trim).filter(|s| !s.is_empty());
    let guild = guild_id.map(str::trim).filter(|s| !s.is_empty());
    match (channel, guild) {
        (Some(c), Some(g)) => (
            Some(format!("https://discord.com/channels/{g}/{c}")),
            Some(format!("discord://discord.com/channels/{g}/{c}")),
        ),
        _ => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dedup_dispatched_sessions_collapses_same_agent_channel_across_providers() {
        let stale = json!({
            "agent_id": "project-agentdesk",
            "provider": "codex",
            "status": "idle",
            "active_dispatch_id": null,
            "thread_channel_id": "1485506232256168011",
            "channel_id": "1485506232256168011",
        });
        let fresh = json!({
            "agent_id": "project-agentdesk",
            "provider": "claude",
            "status": "working",
            "active_dispatch_id": "dispatch-1",
            "thread_channel_id": "1485506232256168011",
            "channel_id": "1485506232256168011",
        });

        let result = dedup_dispatched_sessions(vec![stale, fresh]);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["status"], "working");
        assert_eq!(result[0]["provider"], "claude");
    }

    #[test]
    fn build_channel_deeplinks_emits_https_and_discord_scheme_pair() {
        let (web, deep) =
            build_channel_deeplinks(Some("1485506232256168011"), Some("1490141479707086938"));

        assert_eq!(
            web.as_deref(),
            Some("https://discord.com/channels/1490141479707086938/1485506232256168011"),
        );
        assert_eq!(
            deep.as_deref(),
            Some("discord://discord.com/channels/1490141479707086938/1485506232256168011"),
        );

        let (web_none, deep_none) = build_channel_deeplinks(Some("1485506232256168011"), None);
        assert!(web_none.is_none());
        assert!(deep_none.is_none());
    }
}
