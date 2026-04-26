use std::collections::HashMap;

use super::registry::{LaunchArtifact, ProviderCliChannel};

/// Preference order when selecting a canary agent:
/// 1. Explicitly requested agent_id (`requested_agent_id`)
/// 2. Idle agent (no active session)
/// 3. Any agent
pub fn select_canary_agent(
    provider: &str,
    agents: &[AgentInfo],
    requested_agent_id: Option<&str>,
) -> Option<String> {
    if let Some(id) = requested_agent_id {
        if agents
            .iter()
            .any(|a| a.agent_id == id && a.provider == provider)
        {
            return Some(id.to_string());
        }
    }

    // Prefer idle agents.
    if let Some(idle) = agents
        .iter()
        .find(|a| a.provider == provider && !a.has_active_session)
    {
        return Some(idle.agent_id.clone());
    }

    // Fall back to any agent for this provider.
    agents
        .iter()
        .find(|a| a.provider == provider)
        .map(|a| a.agent_id.clone())
}

/// Lightweight description of a running agent.
#[derive(Clone, Debug)]
pub struct AgentInfo {
    pub agent_id: String,
    pub provider: String,
    pub has_active_session: bool,
    pub tmux_session: Option<String>,
    pub launch_artifact: Option<LaunchArtifact>,
}

/// Evidence keys written into the launch artifact when a canary session starts.
pub fn canary_evidence(agent_id: &str, channel: &ProviderCliChannel) -> HashMap<String, String> {
    let mut m = HashMap::new();
    m.insert("canary_agent_id".to_string(), agent_id.to_string());
    m.insert("candidate_path".to_string(), channel.path.clone());
    m.insert("candidate_version".to_string(), channel.version.clone());
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str, provider: &str, active: bool) -> AgentInfo {
        AgentInfo {
            agent_id: id.to_string(),
            provider: provider.to_string(),
            has_active_session: active,
            tmux_session: None,
            launch_artifact: None,
        }
    }

    #[test]
    fn select_requested_agent_when_present() {
        let agents = vec![
            agent("codex-1", "codex", true),
            agent("codex-2", "codex", false),
        ];
        let selected = select_canary_agent("codex", &agents, Some("codex-1")).unwrap();
        assert_eq!(selected, "codex-1");
    }

    #[test]
    fn prefer_idle_agent() {
        let agents = vec![
            agent("codex-1", "codex", true),
            agent("codex-2", "codex", false),
        ];
        let selected = select_canary_agent("codex", &agents, None).unwrap();
        assert_eq!(selected, "codex-2");
    }

    #[test]
    fn fallback_to_active_when_all_busy() {
        let agents = vec![
            agent("codex-1", "codex", true),
            agent("codex-2", "codex", true),
        ];
        let selected = select_canary_agent("codex", &agents, None).unwrap();
        assert!(!selected.is_empty());
    }

    #[test]
    fn no_agent_for_wrong_provider() {
        let agents = vec![agent("claude-1", "claude", false)];
        let selected = select_canary_agent("codex", &agents, None);
        assert!(selected.is_none());
    }
}
