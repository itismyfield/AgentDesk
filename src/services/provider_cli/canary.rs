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

/// Returns the most recent candidate launch artifact for the given agent recorded after
/// `not_before`, verifying path and version match the registered candidate channel.
///
/// Returns `Ok(None)` when no qualifying artifact exists — this allows promotion to proceed
/// with a warning (e.g. when quota exhaustion prevented a canary turn from running).
/// Returns `Err` only on version/path mismatch against the registered candidate channel.
pub fn verified_candidate_launch_artifact(
    root: &Path,
    provider: &str,
    agent_id: &str,
    candidate: &ProviderCliChannel,
    not_before: DateTime<Utc>,
) -> Result<Option<LaunchArtifact>, String> {
    let mut candidates: Vec<_> = load_launch_artifacts(root, provider)
        .map_err(|error| format!("failed to load launch artifacts for {provider}: {error}"))?
        .into_iter()
        .filter(|artifact| {
            artifact.agent_id.as_deref() == Some(agent_id)
                && artifact.channel == "candidate"
                && artifact.launched_at >= not_before
        })
        .collect();
    candidates.sort_by_key(|artifact| artifact.launched_at);

    let Some(artifact) = candidates.pop() else {
        return Ok(None);
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
    fn verified_artifact_returns_none_when_agent_never_launched() {
        // No launch artifact exists — returns Ok(None) so caller can warn and proceed.
        let root = tempfile::tempdir().unwrap();
        let candidate = candidate_channel("/tmp/codex", "1.0.0");
        let result = verified_candidate_launch_artifact(
            root.path(),
            "codex",
            "codex-agent",
            &candidate,
            Utc::now() - chrono::Duration::seconds(60),
        );
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn verified_artifact_returns_none_when_launched_but_canary_turn_skipped() {
        let root = tempfile::tempdir().unwrap();
        // Agent has a pre-migration artifact on the current channel
        crate::services::provider_cli::io::save_launch_artifact(
            root.path(),
            &LaunchArtifact {
                provider: "codex".to_string(),
                agent_id: Some("codex-agent".to_string()),
                channel_id: None,
                session_key: Some("codex-agent-current-session".to_string()),
                channel: "current".to_string(),
                cli_path: "/tmp/codex".to_string(),
                canonical_path: "/tmp/codex".to_string(),
                cli_version: "0.9.0".to_string(),
                process_id: None,
                tmux_session: None,
                launched_at: Utc::now() - chrono::Duration::seconds(120),
            },
        )
        .unwrap();

        let candidate = candidate_channel("/tmp/codex", "1.0.0");
        let result = verified_candidate_launch_artifact(
            root.path(),
            "codex",
            "codex-agent",
            &candidate,
            Utc::now() - chrono::Duration::seconds(60),
        );
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    #[test]
    fn verified_artifact_fails_when_launch_artifact_is_corrupt() {
        let root = tempfile::tempdir().unwrap();
        let artifact_dir = crate::services::provider_cli::paths::launch_artifacts_dir(root.path());
        std::fs::create_dir_all(&artifact_dir).unwrap();
        std::fs::write(
            artifact_dir.join("corrupt.json"),
            r#"{"provider":"codex","agent_id":123}"#,
        )
        .unwrap();

        let candidate = candidate_channel("/tmp/codex", "1.0.0");
        let result = verified_candidate_launch_artifact(
            root.path(),
            "codex",
            "codex-agent",
            &candidate,
            Utc::now() - chrono::Duration::seconds(60),
        );

        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("failed to load launch artifacts")
        );
    }
}
