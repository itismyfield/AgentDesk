//! Memory guidance — the proactive memory prompt block, including the memento
//! scope hint (channel -> workspace/agentId) the model needs to scope its own
//! `context`/`recall`/`remember` calls.

use poise::serenity_prelude::ChannelId;

use super::DispatchProfile;
use crate::services::discord::settings::{MemoryBackendKind, ResolvedMemorySettings, RoleBinding};
use crate::services::memory::{
    UNBOUND_MEMORY_ROLE_ID, resolve_memento_agent_id, resolve_memento_workspace,
    sanitize_memento_workspace_segment,
};

pub(super) fn proactive_memory_guidance(
    memory_settings: Option<&ResolvedMemorySettings>,
    current_path: &str,
    channel_id: ChannelId,
    role_binding: Option<&RoleBinding>,
    profile: DispatchProfile,
    memento_mcp_available: bool,
    is_claude_harness: bool,
) -> Option<String> {
    proactive_memory_guidance_with(
        memory_settings,
        current_path,
        channel_id,
        role_binding,
        profile,
        memento_mcp_available,
        is_claude_harness,
        |p| std::path::Path::new(p).exists(),
    )
}

/// Filesystem-injectable seam for [`proactive_memory_guidance`] so the
/// workspace-aware gate on the repo-relative `docs/memory-scope.md` reference
/// (#4314) can be tested deterministically. The `exists` closure decides
/// whether the current workspace is an AgentDesk checkout; everything else
/// (scope hints, the always-on `tool_feedback` contract) is unchanged.
#[allow(clippy::too_many_arguments)]
pub(super) fn proactive_memory_guidance_with(
    memory_settings: Option<&ResolvedMemorySettings>,
    current_path: &str,
    channel_id: ChannelId,
    role_binding: Option<&RoleBinding>,
    profile: DispatchProfile,
    memento_mcp_available: bool,
    is_claude_harness: bool,
    exists: impl Fn(&str) -> bool,
) -> Option<String> {
    let settings = memory_settings?;
    if profile != DispatchProfile::Full {
        return None;
    }

    // Fallback mode: memento is the configured backend but is degraded, so
    // memory silently ran on the local file store. This is NOT a deliberate
    // file backend — authoritative writes must be held, not committed to the
    // degraded store. Guidance branches on the CAUSE (memento_fallback), not on
    // the resulting `File` backend alone.
    if settings.backend == MemoryBackendKind::File && settings.memento_fallback {
        return Some(String::from(
            "\n\n[Proactive Memory Guidance — Fallback Mode]\n\
             memento is configured but unreachable; memory has degraded to the local file store. \
             Use `memory-read` skill for explicit past-context/error/config lookups. \
             HOLD permanent writes: do NOT commit confirmed decisions, root causes, or config \
             changes to the degraded store — record them with `remember` once memento recovers.",
        ));
    }

    let (backend_name, read_tool, write_tool, extra_note) = match settings.backend {
        MemoryBackendKind::File => (
            "local",
            "`memory-read` skill",
            "`memory-write` skill",
            String::new(),
        ),
        MemoryBackendKind::Memento if !memento_mcp_available => return None,
        MemoryBackendKind::Memento => {
            let deferred_tool_instruction = if is_claude_harness {
                " If the tool is deferred, load it first via ToolSearch `select:mcp__memento__tool_feedback`."
            } else {
                ""
            };
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
            // #4314: `docs/memory-scope.md` is an AgentDesk repo-relative path.
            // Only reference it when the agent's workspace actually is an
            // AgentDesk checkout — otherwise it points at a nonexistent file.
            let memory_policy_line = if super::layer_rendering::workspace_has_agentdesk_docs_with(
                current_path,
                &exists,
            ) {
                "\n- full memory policy: `docs/memory-scope.md`; read it before broad memory cleanup or scope changes."
            } else {
                ""
            };
            (
                "memento",
                "`recall` MCP tool",
                "`remember` MCP tool",
                format!(
                    "\n- scope hints: project=`workspace={workspace_scope}, agentId=default`; agent-private=`workspace={agent_workspace}, agentId={agent_id}`.{memory_policy_line}\n\
                     - feedback contract: in the same turn you use `recall`/`context` results, call `mcp__memento__tool_feedback` once (required: `tool_name`, `relevant`, `sufficient`; when the response carries `_meta.searchEventId`, also pass it as `search_event_id` — recommended).{deferred_tool_instruction}"
                ),
            )
        }
    };

    Some(format!(
        "\n\n[Proactive Memory Guidance]\n\
         `{backend_name}` memory is available. Use {read_tool} for explicit past-context/error/config lookups; use {write_tool} only for confirmed decisions, root causes, or config changes.{extra_note}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_settings(memento_fallback: bool) -> ResolvedMemorySettings {
        ResolvedMemorySettings {
            backend: MemoryBackendKind::File,
            memento_fallback,
            ..ResolvedMemorySettings::default()
        }
    }

    #[test]
    fn configured_file_backend_yields_unchanged_file_guidance() {
        // #4316 (a): a deliberately configured file backend (memento_fallback =
        // false) must keep the existing, unchanged file guidance and must NOT
        // show the fallback wording.
        let settings = file_settings(false);
        let guidance = proactive_memory_guidance(
            Some(&settings),
            "/tmp/agentdesk",
            ChannelId::new(1),
            None,
            DispatchProfile::Full,
            false,
            true,
        )
        .expect("file backend must produce proactive guidance");

        assert!(guidance.contains("[Proactive Memory Guidance]"));
        assert!(guidance.contains("`local` memory is available"));
        assert!(guidance.contains("`memory-read` skill"));
        assert!(guidance.contains("`memory-write` skill"));
        assert!(
            !guidance.contains("Fallback Mode"),
            "configured file backend must not show fallback wording, got: {guidance}"
        );
        assert!(!guidance.contains("HOLD permanent"));
    }

    #[test]
    fn memento_fallback_yields_hold_permanent_writes_guidance() {
        // #4316 (b): memento configured but degraded → File. Guidance must flag
        // FALLBACK mode and instruct holding permanent/authoritative writes
        // rather than committing them to the degraded store.
        let settings = file_settings(true);
        let guidance = proactive_memory_guidance(
            Some(&settings),
            "/tmp/agentdesk",
            ChannelId::new(1),
            None,
            DispatchProfile::Full,
            false,
            true,
        )
        .expect("fallback mode must produce proactive guidance");

        assert!(guidance.contains("[Proactive Memory Guidance — Fallback Mode]"));
        assert!(guidance.contains("memento is configured but unreachable"));
        // Core semantics: hold permanent writes; record after memento recovers.
        assert!(
            guidance.contains("HOLD permanent writes"),
            "fallback guidance must instruct holding permanent writes, got: {guidance}"
        );
        assert!(guidance.contains("once memento recovers"));
        // Must not read like the deliberate-file backend guidance.
        assert!(!guidance.contains("`local` memory is available"));
    }

    fn memento_settings() -> ResolvedMemorySettings {
        ResolvedMemorySettings {
            backend: MemoryBackendKind::Memento,
            ..ResolvedMemorySettings::default()
        }
    }

    #[test]
    fn foreign_workspace_memento_omits_memory_scope_but_keeps_feedback_contract() {
        // #4314 T4 (guard mutation, memento branch): a foreign workspace (docs
        // absent) must drop the repo-relative `docs/memory-scope.md` reference
        // while keeping the always-on scope hints and tool_feedback contract.
        // Removing the guard so the reference is always injected makes THIS
        // assert fail on its own — not on a compile error.
        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _runtime_guard = crate::config::set_agentdesk_root_for_test(runtime_root.path());
        let settings = memento_settings();
        let guidance = proactive_memory_guidance_with(
            Some(&settings),
            "/foreign/repo",
            ChannelId::new(1),
            None,
            DispatchProfile::Full,
            true,
            true,
            |_| false,
        )
        .expect("memento guidance must be produced");

        assert!(guidance.contains("[Proactive Memory Guidance]"));
        assert!(guidance.contains("scope hints:"));
        assert!(guidance.contains("mcp__memento__tool_feedback"));
        assert!(
            !guidance.contains("docs/memory-scope.md"),
            "foreign workspace must not reference docs/memory-scope.md, got: {guidance}"
        );
    }

    #[test]
    fn agentdesk_workspace_memento_keeps_memory_scope_reference() {
        // #4314 T4 reverse: an AgentDesk workspace (docs present) must keep the
        // `docs/memory-scope.md` reference. Forcing the guard to always omit
        // makes THIS assert fail on its own.
        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _runtime_guard = crate::config::set_agentdesk_root_for_test(runtime_root.path());
        let settings = memento_settings();
        let guidance = proactive_memory_guidance_with(
            Some(&settings),
            "/agentdesk",
            ChannelId::new(1),
            None,
            DispatchProfile::Full,
            true,
            true,
            |_| true,
        )
        .expect("memento guidance must be produced");

        assert!(
            guidance.contains("full memory policy: `docs/memory-scope.md`"),
            "AgentDesk workspace must keep docs/memory-scope.md, got: {guidance}"
        );
        assert!(guidance.contains("mcp__memento__tool_feedback"));
    }

    /// #5168: path A is gone, so AgentDesk no longer claims ownership of
    /// automatic turn recall. Non-Full profiles now emit no memory guidance at
    /// all — including for an available Memento backend, which previously got
    /// the `[Memory Recall Ownership]` override block on every profile.
    #[test]
    fn non_full_memento_profiles_suppress_guidance() {
        let settings = memento_settings();
        for profile in [DispatchProfile::ReviewLite, DispatchProfile::Lite] {
            for memento_mcp_available in [true, false] {
                assert!(
                    proactive_memory_guidance(
                        Some(&settings),
                        "/tmp/agentdesk",
                        ChannelId::new(1),
                        None,
                        profile,
                        memento_mcp_available,
                        true,
                    )
                    .is_none(),
                    "non-Full profile {profile:?} must emit no memory guidance"
                );
            }
        }
    }

    /// #5168: the model now decides its own recall, so the system prompt must
    /// not carry any instruction telling it that AgentDesk owns that decision.
    /// The scope hint (retained) and the tool_feedback contract (retained) stay.
    #[test]
    fn full_memento_guidance_carries_scope_hint_without_recall_ownership() {
        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _runtime_guard = crate::config::set_agentdesk_root_for_test(runtime_root.path());
        let settings = memento_settings();
        let guidance = proactive_memory_guidance_with(
            Some(&settings),
            "/foreign/repo",
            ChannelId::new(4_242),
            None,
            DispatchProfile::Full,
            true,
            true,
            |_| false,
        )
        .expect("memento guidance must be produced");

        // Retained target #1: the channel -> workspace/agentId mapping. This is
        // now its ONLY production consumer (`resolve_memento_workspace` +
        // `resolve_memento_agent_id` at the top of this module), so the
        // assertion below is what keeps the mapping from being collected as
        // dead code along with the removed injection path. The expected values
        // are exactly what those two helpers return for an unregistered channel
        // bound to no role.
        let channel_id = 4_242_u64;
        assert_eq!(
            resolve_memento_workspace(UNBOUND_MEMORY_ROLE_ID, channel_id, None),
            "channel-4242"
        );
        assert_eq!(
            resolve_memento_agent_id(UNBOUND_MEMORY_ROLE_ID, channel_id),
            "agentdesk-channel-4242"
        );
        assert!(
            guidance.contains(
                "scope hints: project=`workspace=repo, agentId=default`; \
                 agent-private=`workspace=channel-4242, agentId=agentdesk-channel-4242`."
            ),
            "the channel->workspace/agentId scope hint must survive, got: {guidance}"
        );
        assert!(
            !guidance.contains("automatic recall"),
            "no automatic-recall ownership claim may remain, got: {guidance}"
        );
        assert!(
            !guidance.contains("AgentDesk owns"),
            "no AgentDesk-owns-recall claim may remain, got: {guidance}"
        );
        assert!(
            !guidance.contains("Do not call `context` or `recall`"),
            "the do-not-call-context instruction must be gone, got: {guidance}"
        );
    }

    #[test]
    fn non_full_file_profile_suppresses_guidance() {
        // File/fallback guidance stays gated to Full; only an available
        // Memento backend needs the cross-profile automatic-recall override.
        let settings = file_settings(true);
        assert!(
            proactive_memory_guidance(
                Some(&settings),
                "/tmp/agentdesk",
                ChannelId::new(1),
                None,
                DispatchProfile::ReviewLite,
                false,
                true,
            )
            .is_none()
        );
    }
}
