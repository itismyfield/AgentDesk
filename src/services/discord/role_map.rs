use std::fs;

use poise::serenity_prelude::ChannelId;

use super::meeting::{MeetingAgentConfig, MeetingConfig, SummaryAgentConfig, SummaryAgentRule};
use super::runtime_store::role_map_path;
use super::settings::{PeerAgentInfo, RoleBinding};
use crate::services::provider::ProviderKind;

/// Expand `~` or `~/` prefix to the user's home directory.
fn expand_tilde(path: &str) -> String {
    if let Some(home) = dirs::home_dir() {
        if path == "~" {
            return home.display().to_string();
        }
        if path.starts_with("~/") {
            return format!("{}{}", home.display(), &path[1..]);
        }
    }
    path.to_string()
}

fn load_role_map_json() -> Option<serde_json::Value> {
    let path = role_map_path()?;
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn parse_role_binding(value: &serde_json::Value) -> Option<RoleBinding> {
    let obj = value.as_object()?;
    let role_id = obj.get("roleId")?.as_str()?.to_string();
    let prompt_file = expand_tilde(obj.get("promptFile")?.as_str()?);
    let provider = obj
        .get("provider")
        .and_then(|v| v.as_str())
        .and_then(ProviderKind::from_str);
    let model = obj
        .get("model")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let reasoning_effort = obj
        .get("reasoningEffort")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Some(RoleBinding {
        role_id,
        prompt_file,
        provider,
        model,
        reasoning_effort,
    })
}

fn fallback_enabled(json: &serde_json::Value) -> bool {
    json.get("fallbackByChannelName")
        .and_then(|v| v.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

pub(super) fn resolve_role_binding(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<RoleBinding> {
    let json = load_role_map_json()?;

    if let Some(by_id) = json.get("byChannelId").and_then(|v| v.as_object()) {
        let key = channel_id.get().to_string();
        if let Some(binding) = by_id.get(&key).and_then(parse_role_binding) {
            return Some(binding);
        }
    }

    if !fallback_enabled(&json) {
        return None;
    }

    let cname = channel_name?;
    let by_name = json.get("byChannelName").and_then(|v| v.as_object())?;
    by_name.get(cname).and_then(parse_role_binding)
}

pub(super) fn resolve_workspace(
    channel_id: ChannelId,
    channel_name: Option<&str>,
) -> Option<String> {
    let json = load_role_map_json()?;

    if let Some(by_id) = json.get("byChannelId").and_then(|v| v.as_object()) {
        let key = channel_id.get().to_string();
        if let Some(entry) = by_id.get(&key) {
            if let Some(ws) = entry.get("workspace").and_then(|v| v.as_str()) {
                return Some(expand_tilde(ws));
            }
        }
    }

    if !fallback_enabled(&json) {
        return None;
    }

    let cname = channel_name?;
    let by_name = json.get("byChannelName").and_then(|v| v.as_object())?;
    by_name
        .get(cname)
        .and_then(|entry| entry.get("workspace"))
        .and_then(|v| v.as_str())
        .map(|s| expand_tilde(s))
}

pub(super) fn load_shared_prompt_path() -> Option<String> {
    let json = load_role_map_json()?;
    json.get("sharedPromptFile")
        .and_then(|v| v.as_str())
        .map(|s| expand_tilde(s))
}

/// Check if a role_id exists in any channel binding in role_map.json.
pub(super) fn is_known_agent(role_id: &str) -> bool {
    let Some(json) = load_role_map_json() else {
        return false;
    };
    // Check byChannelId
    if let Some(by_id) = json.get("byChannelId").and_then(|v| v.as_object()) {
        for entry in by_id.values() {
            if entry.get("roleId").and_then(|v| v.as_str()) == Some(role_id) {
                return true;
            }
        }
    }
    // Check byChannelName
    if let Some(by_name) = json.get("byChannelName").and_then(|v| v.as_object()) {
        for entry in by_name.values() {
            if entry.get("roleId").and_then(|v| v.as_str()) == Some(role_id) {
                return true;
            }
        }
    }
    false
}

pub(super) fn load_peer_agents() -> Vec<PeerAgentInfo> {
    let Some(json) = load_role_map_json() else {
        return Vec::new();
    };
    let Some(agents) = json
        .get("meeting")
        .and_then(|meeting| meeting.get("available_agents"))
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };

    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::new();
    for agent in agents {
        let Some(obj) = agent.as_object() else {
            continue;
        };
        let Some(role_id) = obj.get("role_id").and_then(|v| v.as_str()) else {
            continue;
        };
        if !seen.insert(role_id.to_string()) {
            continue;
        }
        let Some(display_name) = obj.get("display_name").and_then(|v| v.as_str()) else {
            continue;
        };
        let keywords = obj
            .get("keywords")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        result.push(PeerAgentInfo {
            role_id: role_id.to_string(),
            display_name: display_name.to_string(),
            keywords,
        });
    }

    result
}

fn parse_summary_agent_config(value: &serde_json::Value) -> Option<SummaryAgentConfig> {
    // Backward-compatible: accept plain string
    if let Some(s) = value.as_str() {
        return Some(SummaryAgentConfig::Static(s.to_string()));
    }

    // Rule-based: { "rules": [...], "default": "..." }
    let obj = value.as_object()?;
    let default_agent = obj.get("default")?.as_str()?.to_string();
    let rules_arr = obj.get("rules").and_then(|v| v.as_array());
    let mut rules = Vec::new();
    if let Some(arr) = rules_arr {
        for rule in arr {
            let agent = rule.get("agent").and_then(|v| v.as_str());
            let keywords = rule.get("keywords").and_then(|v| v.as_array()).map(|kws| {
                kws.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect::<Vec<_>>()
            });
            if let (Some(agent), Some(keywords)) = (agent, keywords) {
                rules.push(SummaryAgentRule {
                    keywords,
                    agent: agent.to_string(),
                });
            }
        }
    }
    Some(SummaryAgentConfig::Dynamic {
        rules,
        default: default_agent,
    })
}

pub(super) fn load_meeting_config() -> Option<MeetingConfig> {
    let json = load_role_map_json()?;
    let meeting = json.get("meeting")?;

    let channel_name = meeting.get("channel_name")?.as_str()?.to_string();
    let max_rounds = meeting
        .get("max_rounds")
        .and_then(|v| v.as_u64())
        .unwrap_or(3) as u32;
    let summary_agent = parse_summary_agent_config(meeting.get("summary_agent")?)?;

    let agents_arr = meeting.get("available_agents")?.as_array()?;
    let mut available_agents = Vec::new();
    for agent in agents_arr {
        let Some(role_id) = agent.get("role_id").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(display_name) = agent.get("display_name").and_then(|v| v.as_str()) else {
            continue;
        };
        let prompt_file = expand_tilde(
            agent
                .get("prompt_file")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        );
        let keywords = agent
            .get("keywords")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        available_agents.push(MeetingAgentConfig {
            role_id: role_id.to_string(),
            display_name: display_name.to_string(),
            keywords,
            prompt_file,
        });
    }

    Some(MeetingConfig {
        channel_name,
        max_rounds,
        summary_agent,
        available_agents,
    })
}
