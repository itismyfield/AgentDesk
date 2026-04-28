use serde_json::json;

use super::io::{load_registry, save_registry};
use super::registry::{MigrationState, ProviderChannels, ProviderCliMigrationState};
use super::session_guard::{SessionGuardEvaluation, evaluate_session_migration_guards};

pub fn apply_canary_override(
    root: &std::path::Path,
    provider: &str,
    agent_id: &str,
) -> Result<(), String> {
    let mut registry = load_registry(root)
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    let channels = registry.providers.entry(provider.to_string()).or_default();
    if channels.candidate.is_none() {
        return Err(format!(
            "no candidate channel registered for provider: {provider}"
        ));
    }
    channels
        .agent_overrides
        .insert(agent_id.to_string(), "candidate".to_string());
    save_registry(root, &registry).map_err(|e| e.to_string())
}

pub fn clear_canary_override(
    root: &std::path::Path,
    provider: &str,
    agent_id: &str,
) -> Result<(), String> {
    let mut registry = load_registry(root)
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    if let Some(channels) = registry.providers.get_mut(provider) {
        channels.agent_overrides.remove(agent_id);
    }
    save_registry(root, &registry).map_err(|e| e.to_string())
}

pub fn clear_provider_channel_overrides(
    root: &std::path::Path,
    provider: &str,
) -> Result<(), String> {
    let mut registry = load_registry(root)
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    if let Some(channels) = registry.providers.get_mut(provider) {
        channels.agent_overrides.clear();
    }
    save_registry(root, &registry).map_err(|e| e.to_string())
}

pub fn promote_registry_candidate(root: &std::path::Path, provider: &str) -> Result<(), String> {
    let mut registry = load_registry(root)
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    let channels = registry.providers.entry(provider.to_string()).or_default();
    let candidate = channels
        .candidate
        .clone()
        .ok_or_else(|| format!("no candidate channel registered for provider: {provider}"))?;
    channels.previous = channels
        .current
        .clone()
        .or_else(|| channels.previous.clone());
    channels.current = Some(candidate.clone());
    channels.default = Some(candidate);
    channels.agent_overrides.clear();
    save_registry(root, &registry).map_err(|e| e.to_string())
}

pub fn rollback_registry_previous(root: &std::path::Path, provider: &str) -> Result<(), String> {
    let mut registry = load_registry(root)
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    let channels = registry
        .providers
        .entry(provider.to_string())
        .or_insert_with(ProviderChannels::default);
    if let Some(previous) = channels.previous.clone() {
        channels.current = Some(previous);
        channels.candidate = None;
    }
    channels.agent_overrides.clear();
    save_registry(root, &registry).map_err(|e| e.to_string())
}

pub fn evaluate_provider_session_guard(
    root: &std::path::Path,
    provider: &str,
    selected_agent_id: Option<&str>,
    target_channel: &str,
) -> SessionGuardEvaluation {
    let target_agent_ids = provider_target_agent_ids(provider, selected_agent_id);
    evaluate_session_migration_guards(root, provider, &target_agent_ids, target_channel)
}

pub fn session_guard_evidence(
    operator_evidence: Option<&str>,
    guard: &SessionGuardEvaluation,
) -> String {
    serde_json::to_string(&json!({
        "operator_evidence": operator_evidence,
        "session_guard": guard,
    }))
    .unwrap_or_else(|_| guard.evidence_json())
}

pub fn canary_promotion_evidence(
    root: &std::path::Path,
    state: &ProviderCliMigrationState,
    operator_evidence: Option<&str>,
) -> Result<String, String> {
    let operator_evidence = operator_evidence
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            "promotion from canary_active requires --evidence describing canary verification"
                .to_string()
        })?;
    let agent_id = state
        .selected_agent_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "promotion requires a selected canary agent".to_string())?;
    let candidate = state
        .candidate_channel
        .as_ref()
        .ok_or_else(|| "promotion requires a candidate channel in migration state".to_string())?;
    let canary_launch = super::canary::verified_candidate_launch_artifact(
        root,
        &state.provider,
        agent_id,
        candidate,
        canary_active_since(state),
    )?;

    match canary_launch {
        Some(launch) => Ok(serde_json::to_string(&json!({
            "operator_evidence": operator_evidence,
            "canary_launch": launch,
        }))
        .unwrap_or_else(|_| operator_evidence.to_string())),
        None => {
            eprintln!(
                "WARNING: no candidate launch artifact recorded for {}/{} after canary activation — proceeding without canary verification (possible quota exhaustion)",
                state.provider, agent_id
            );
            Ok(serde_json::to_string(&json!({
                "operator_evidence": operator_evidence,
                "warning": "no canary turn recorded; proceeding without canary verification (possible quota exhaustion)",
            }))
            .unwrap_or_else(|_| operator_evidence.to_string()))
        }
    }
}

fn canary_active_since(state: &ProviderCliMigrationState) -> chrono::DateTime<chrono::Utc> {
    state
        .history
        .iter()
        .rev()
        .find(|entry| entry.to_state == MigrationState::CanaryActive)
        .map(|entry| entry.transitioned_at)
        .unwrap_or(state.updated_at)
}

fn provider_target_agent_ids(provider: &str, selected_agent_id: Option<&str>) -> Vec<String> {
    let mut agent_ids = configured_provider_agent_ids(provider);
    if agent_ids.is_empty() {
        if let Some(agent_id) = selected_agent_id.filter(|value| !value.trim().is_empty()) {
            agent_ids.push(agent_id.to_string());
        }
    }
    agent_ids.sort();
    agent_ids.dedup();
    agent_ids
}

fn configured_provider_agent_ids(provider: &str) -> Vec<String> {
    configured_provider_agents(provider)
        .into_iter()
        .map(|agent| agent.agent_id)
        .collect()
}

pub fn configured_provider_agents(provider: &str) -> Vec<super::AgentInfo> {
    let Some(root) = crate::config::runtime_root() else {
        return Vec::new();
    };
    let config = [
        crate::runtime_layout::config_file_path(&root),
        crate::runtime_layout::legacy_config_file_path(&root),
    ]
    .into_iter()
    .find_map(|path| crate::config::load_from_path(&path).ok());
    let Some(config) = config else {
        return Vec::new();
    };

    config
        .agents
        .iter()
        .filter(|agent| agent_supports_provider(agent, provider))
        .map(|agent| super::AgentInfo {
            agent_id: agent.id.clone(),
            provider: provider.to_string(),
            has_active_session: false,
            tmux_session: None,
            launch_artifact: None,
        })
        .collect()
}

fn agent_supports_provider(agent: &crate::config::AgentDef, provider: &str) -> bool {
    if agent.provider.eq_ignore_ascii_case(provider) {
        return true;
    }

    agent
        .channels
        .iter()
        .into_iter()
        .any(|(provider_key, channel)| {
            let Some(channel) = channel else {
                return false;
            };
            if let Some(channel_provider) = channel.provider() {
                return channel_provider.eq_ignore_ascii_case(provider);
            }
            provider_key.eq_ignore_ascii_case(provider) && channel.target().is_some()
        })
}
