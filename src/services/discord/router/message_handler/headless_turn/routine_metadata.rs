//! Routine metadata and session identity helpers for headless turns.
use super::*;

pub(super) fn valid_routine_metadata(
    metadata: Option<&serde_json::Value>,
) -> Option<&serde_json::Value> {
    let metadata = metadata?;
    metadata
        .get("routine_id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    Some(metadata)
}
pub(super) fn routine_metadata_agent_id(metadata: Option<&serde_json::Value>) -> Option<&str> {
    valid_routine_metadata(metadata)?
        .get("agent_id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}
/// #4658: the isolated session-key basis for a scheduled-snapshot turn. When
/// present, the turn (a) derives its ADK session key from this label instead of
/// the channel name and (b) severs provider/transcript continuity so the frozen
/// snapshot is the only conversation context. Absent for every other caller.
pub(super) fn scheduled_snapshot_session_label(
    metadata: Option<&serde_json::Value>,
) -> Option<String> {
    metadata?
        .get("scheduled_snapshot_session_label")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}
/// Select the session-key basis for isolated headless turns. Fresh routine
/// turns already receive a synthetic per-routine tmux label; using that same
/// label for the persisted session key prevents an old agent-channel row from
/// owning the new routine's heartbeat and model metadata.
pub(super) fn session_key_basis_override<'a>(
    scheduled_snapshot_label: Option<&'a str>,
    metadata: Option<&serde_json::Value>,
    tmux_session_label: Option<&'a str>,
) -> Option<&'a str> {
    scheduled_snapshot_label.or_else(|| {
        fresh_routine_turn(metadata)
            .then_some(tmux_session_label)
            .flatten()
    })
}
/// Whether an explicit routine turn must sever provider and transcript continuity.
/// Only `persistent` routines retain continuity; absent strategy preserves the
/// legacy routine default of `fresh`. Non-routine metadata must never reset a
/// provider session.
pub(super) fn fresh_routine_turn(metadata: Option<&serde_json::Value>) -> bool {
    let Some(metadata) = valid_routine_metadata(metadata) else {
        return false;
    };
    metadata
        .get("execution_strategy")
        .and_then(|value| value.as_str())
        != Some("persistent")
}
pub(super) fn routine_metadata_role_binding(
    metadata: Option<&serde_json::Value>,
    provider: &ProviderKind,
) -> Option<settings::RoleBinding> {
    let metadata = valid_routine_metadata(metadata)?;
    let agent_id = routine_metadata_agent_id(Some(metadata))?;
    // Resolve the agent's configured prompt path instead of hardcoding
    // IDENTITY.md under config/agents: `default_prompt_path` reads the managed
    // agents root and falls back to the legacy `<id>.prompt.md` layout, so
    // agents on either layout still get their role prompt for routine turns
    // (#3463). Falls back to the canonical IDENTITY.md path when unset.
    let prompt_file = crate::services::discord::agentdesk_config::default_prompt_path(agent_id)
        .unwrap_or_default();

    Some(settings::RoleBinding {
        role_id: agent_id.to_string(),
        prompt_file,
        provider: Some(provider.clone()),
        model: None,
        reasoning_effort: None,
        peer_agents_enabled: true,
        quality_feedback_injection_enabled: true,
        memory: settings::resolve_memory_settings(None, None),
    })
}