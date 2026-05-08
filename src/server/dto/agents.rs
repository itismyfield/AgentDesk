use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Serialize)]
pub struct AgentOfficesResponse {
    pub offices: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct AgentCronResponse {
    pub jobs: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct AgentSkillsResponse {
    pub skills: Vec<Value>,
    #[serde(rename = "sharedSkills")]
    pub shared_skills: Vec<Value>,
    #[serde(rename = "totalCount")]
    pub total_count: usize,
}

#[derive(Debug, Serialize)]
pub struct AgentDispatchedSessionsResponse {
    pub sessions: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct AgentTimelineResponse {
    pub events: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct AgentTranscriptsResponse {
    pub agent_id: String,
    pub transcripts: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct AgentOfficeView {
    pub id: String,
    pub name: Option<String>,
    pub layout: Option<String>,
    pub assigned: bool,
    pub office_department_id: Option<String>,
    pub joined_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AgentSkillView {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub source_path: Option<String>,
    pub trigger_patterns: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct DispatchedSessionView {
    pub id: i64,
    pub session_key: Option<String>,
    pub agent_id: Option<String>,
    pub provider: Option<String>,
    pub status: String,
    pub active_dispatch_id: Option<String>,
    pub model: Option<String>,
    pub tokens: i64,
    pub cwd: Option<String>,
    pub last_heartbeat: Option<String>,
    pub thread_channel_id: Option<String>,
    pub channel_id: Option<String>,
    pub thread_id: Option<String>,
    pub guild_id: Option<String>,
    pub channel_web_url: Option<String>,
    pub channel_deeplink_url: Option<String>,
    pub deeplink_url: Option<String>,
    pub thread_deeplink_url: Option<String>,
    pub kanban_card_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TimelineEventView {
    pub id: String,
    pub source: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub title: Option<String>,
    pub status: Option<String>,
    pub timestamp: Option<i64>,
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct TranscriptView {
    pub id: i64,
    pub turn_id: String,
    pub session_key: Option<String>,
    pub channel_id: Option<String>,
    pub agent_id: Option<String>,
    pub provider: Option<String>,
    pub dispatch_id: Option<String>,
    pub kanban_card_id: Option<String>,
    pub dispatch_title: Option<String>,
    pub card_title: Option<String>,
    pub github_issue_number: Option<i64>,
    pub user_message: String,
    pub assistant_message: String,
    pub events: Value,
    pub duration_ms: Option<i64>,
    pub created_at: String,
}

pub fn agent_office_json(
    id: String,
    name: Option<String>,
    layout: Option<String>,
    office_department_id: Option<String>,
    joined_at: Option<String>,
) -> Value {
    json!(AgentOfficeView {
        id,
        name,
        layout,
        assigned: true,
        office_department_id,
        joined_at,
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
    json!(AgentSkillView {
        id,
        name,
        description,
        source_path,
        trigger_patterns,
        updated_at,
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
    json!(DispatchedSessionView {
        id,
        session_key,
        agent_id,
        provider,
        status: status.to_string(),
        active_dispatch_id,
        model,
        tokens,
        cwd,
        last_heartbeat,
        thread_channel_id: thread_channel_id.clone(),
        channel_id: thread_channel_id.clone(),
        thread_id: thread_channel_id,
        guild_id,
        channel_web_url: channel_web_url.clone(),
        channel_deeplink_url: channel_deeplink_url.clone(),
        deeplink_url: channel_web_url,
        thread_deeplink_url: channel_deeplink_url,
        kanban_card_id,
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
    json!(TimelineEventView {
        id,
        source,
        event_type,
        title,
        status,
        timestamp,
        duration_ms,
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
    json!(TranscriptView {
        id,
        turn_id,
        session_key,
        channel_id,
        agent_id,
        provider,
        dispatch_id,
        kanban_card_id,
        dispatch_title,
        card_title,
        github_issue_number,
        user_message,
        assistant_message,
        events,
        duration_ms,
        created_at,
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
    let channel = crate::utils::discord::normalize_discord_snowflake(channel_id);
    let guild = crate::utils::discord::normalize_discord_snowflake(guild_id);
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

        let (web_bad_guild, deep_bad_guild) =
            build_channel_deeplinks(Some("1485506232256168011"), Some("123"));
        assert!(web_bad_guild.is_none());
        assert!(deep_bad_guild.is_none());

        let (web_bad_channel, deep_bad_channel) =
            build_channel_deeplinks(Some("thread-work-completed"), Some("1490141479707086938"));
        assert!(web_bad_channel.is_none());
        assert!(deep_bad_channel.is_none());
    }

    #[test]
    fn agent_container_response_shapes_are_stable() {
        let skills = serde_json::to_value(AgentSkillsResponse {
            skills: vec![json!({"id": "skill-1"})],
            shared_skills: Vec::new(),
            total_count: 1,
        })
        .unwrap();
        assert_eq!(
            skills,
            json!({
                "skills": [{"id": "skill-1"}],
                "sharedSkills": [],
                "totalCount": 1,
            })
        );

        let transcripts = serde_json::to_value(AgentTranscriptsResponse {
            agent_id: "agent-1".to_string(),
            transcripts: vec![json!({"turn_id": "turn-1"})],
        })
        .unwrap();
        assert_eq!(
            transcripts,
            json!({
                "agent_id": "agent-1",
                "transcripts": [{"turn_id": "turn-1"}],
            })
        );
    }

    #[test]
    fn agent_leaf_response_shapes_are_stable() {
        assert_eq!(
            agent_office_json(
                "office-1".to_string(),
                Some("Ops".to_string()),
                Some("grid".to_string()),
                Some("dept-1".to_string()),
                Some("2026-05-06T01:00:00+00:00".to_string()),
            ),
            json!({
                "id": "office-1",
                "name": "Ops",
                "layout": "grid",
                "assigned": true,
                "office_department_id": "dept-1",
                "joined_at": "2026-05-06T01:00:00+00:00",
            })
        );

        assert_eq!(
            timeline_event_json(
                "dispatch-1".to_string(),
                "dispatch".to_string(),
                "implementation".to_string(),
                Some("Build".to_string()),
                Some("completed".to_string()),
                Some(1777777000),
                Some(4200),
            ),
            json!({
                "id": "dispatch-1",
                "source": "dispatch",
                "type": "implementation",
                "title": "Build",
                "status": "completed",
                "timestamp": 1777777000,
                "duration_ms": 4200,
            })
        );
    }

    #[test]
    fn dispatched_session_response_shape_keeps_dashboard_aliases() {
        let session = dispatched_session_json(
            7,
            Some("host:session".to_string()),
            Some("agent-1".to_string()),
            Some("codex".to_string()),
            "working",
            Some("dispatch-1".to_string()),
            Some("gpt-5.3-codex".to_string()),
            123,
            Some("/work/repo".to_string()),
            Some("2026-05-06T01:00:00+00:00".to_string()),
            Some("channel-1".to_string()),
            Some("guild-1".to_string()),
            Some("https://discord.com/channels/guild-1/channel-1".to_string()),
            Some("discord://discord.com/channels/guild-1/channel-1".to_string()),
            Some("card-1".to_string()),
        );

        assert_eq!(
            session,
            json!({
                "id": 7,
                "session_key": "host:session",
                "agent_id": "agent-1",
                "provider": "codex",
                "status": "working",
                "active_dispatch_id": "dispatch-1",
                "model": "gpt-5.3-codex",
                "tokens": 123,
                "cwd": "/work/repo",
                "last_heartbeat": "2026-05-06T01:00:00+00:00",
                "thread_channel_id": "channel-1",
                "channel_id": "channel-1",
                "thread_id": "channel-1",
                "guild_id": "guild-1",
                "channel_web_url": "https://discord.com/channels/guild-1/channel-1",
                "channel_deeplink_url": "discord://discord.com/channels/guild-1/channel-1",
                "deeplink_url": "https://discord.com/channels/guild-1/channel-1",
                "thread_deeplink_url": "discord://discord.com/channels/guild-1/channel-1",
                "kanban_card_id": "card-1",
            })
        );
    }
}
