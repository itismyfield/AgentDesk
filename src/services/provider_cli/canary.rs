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
