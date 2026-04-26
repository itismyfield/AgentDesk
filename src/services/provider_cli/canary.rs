use std::collections::HashMap;
use std::path::Path;

use chrono::{DateTime, Utc};

use super::io::load_launch_artifacts;
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

pub fn verified_candidate_launch_artifact(
    root: &Path,
    provider: &str,
    agent_id: &str,
    candidate: &ProviderCliChannel,
    not_before: DateTime<Utc>,
    force_recreate_active: bool,
) -> Result<Option<LaunchArtifact>, String> {
    let mut artifacts = load_launch_artifacts(root, provider)
        .into_iter()
        .filter(|artifact| {
            artifact.agent_id.as_deref() == Some(agent_id)
                && artifact.channel == "candidate"
                && artifact.launched_at >= not_before
        })
        .collect::<Vec<_>>();
    artifacts.sort_by_key(|artifact| artifact.launched_at);

    let Some(artifact) = artifacts.pop() else {
        if force_recreate_active {
            return Ok(None);
        }
        return Err(format!(
            "no candidate launch artifact recorded for {provider}/{agent_id} after canary activation; run a canary turn before promotion"
        ));
    };

    if artifact.canonical_path != candidate.canonical_path
        || artifact.cli_version != candidate.version
    {
        return Err(format!(
            "candidate launch artifact for {provider}/{agent_id} does not match registered candidate channel"
        ));
    }

    Ok(Some(artifact))
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

    fn candidate_channel(canonical_path: &str, version: &str) -> ProviderCliChannel {
        ProviderCliChannel {
            path: canonical_path.to_string(),
            canonical_path: canonical_path.to_string(),
            version: version.to_string(),
            version_output: None,
            source: "test".to_string(),
            checked_at: Utc::now(),
            evidence: Default::default(),
        }
    }

    #[test]
    fn verified_artifact_errors_without_force_when_no_artifact() {
        let root = tempfile::tempdir().unwrap();
        let candidate = candidate_channel("/tmp/codex", "1.0.0");
        let result = verified_candidate_launch_artifact(
            root.path(),
            "codex",
            "codex-agent",
            &candidate,
            Utc::now() - chrono::Duration::seconds(60),
            false,
        );
        assert!(result.is_err());
    }

    #[test]
    fn verified_artifact_returns_none_with_force_when_no_artifact() {
        let root = tempfile::tempdir().unwrap();
        let candidate = candidate_channel("/tmp/codex", "1.0.0");
        let result = verified_candidate_launch_artifact(
            root.path(),
            "codex",
            "codex-agent",
            &candidate,
            Utc::now() - chrono::Duration::seconds(60),
            true,
        );
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }
}
