//! Memory guidance — proactive memento prompt blocks and the
//! `MemoryRecallManifestInput` carrier consumed by the memory recall manifest
//! layer.

use poise::serenity_prelude::ChannelId;

use super::DispatchProfile;
use crate::services::discord::settings::{MemoryBackendKind, ResolvedMemorySettings, RoleBinding};
use crate::services::memory::{
    UNBOUND_MEMORY_ROLE_ID, resolve_memento_agent_id, resolve_memento_workspace,
    sanitize_memento_workspace_segment,
};

#[derive(Debug, Clone, Copy)]
pub(crate) struct MemoryRecallManifestInput<'a> {
    pub(crate) should_recall: bool,
    pub(crate) gate_reason: &'a str,
    pub(crate) external_recall: Option<&'a str>,
}

pub(super) fn proactive_memory_guidance(
    memory_settings: Option<&ResolvedMemorySettings>,
    current_path: &str,
    channel_id: ChannelId,
    role_binding: Option<&RoleBinding>,
    profile: DispatchProfile,
    memento_mcp_available: bool,
) -> Option<String> {
    if profile != DispatchProfile::Full {
        return None;
    }

    let settings = memory_settings?;
    let (backend_name, read_tool, write_tool, extra_note) = match settings.backend {
        MemoryBackendKind::File => (
            "local",
            "`memory-read` skill",
            "`memory-write` skill",
            String::new(),
        ),
        MemoryBackendKind::Memento if !memento_mcp_available => return None,
        MemoryBackendKind::Memento => {
            let role_id = role_binding
                .map(|binding| binding.role_id.as_str())
                .unwrap_or(UNBOUND_MEMORY_ROLE_ID);
            let workspace_scope = current_path
                .trim()
                .split('/')
                .rev()
                .find(|segment| !segment.trim().is_empty())
                .map(sanitize_memento_workspace_segment)
                .unwrap_or_else(|| "default".to_string());
            let agent_workspace = resolve_memento_workspace(role_id, channel_id.get(), None);
            let agent_id = resolve_memento_agent_id(role_id, channel_id.get());
            (
                "memento",
                "`recall` MCP tool",
                "`remember` MCP tool",
                format!(
                    "\n- scope hints: project=`workspace={workspace_scope}, agentId=default`; agent-private=`workspace={agent_workspace}, agentId={agent_id}`.\n\
                     - full memory policy: `docs/memory-scope.md`; read it before broad memory cleanup or scope changes."
                ),
            )
        }
    };

    Some(format!(
        "\n\n[Proactive Memory Guidance]\n\
         `{backend_name}` memory is available. Use {read_tool} for explicit past-context/error/config lookups; use {write_tool} only for confirmed decisions, root causes, or config changes.{extra_note}"
    ))
}
