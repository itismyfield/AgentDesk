//! Layer rendering — system / role / policy / memory / recovery prompt
//! sections that are concatenated into the final system prompt string.

use super::DispatchProfile;
use crate::services::discord::UserRecord;

pub(super) const CONTEXT_COMPRESSION_SECTION_ORDER: &str =
    "`Goal`, `Progress`, `Decisions`, `Files`, `Next`";
pub(super) const STALE_TOOL_RESULT_PLACEHOLDER_EXAMPLE: &str =
    "[이전 결과 — 3줄 요약: cargo test failed in src/foo.rs because ...]";

pub(super) fn context_compression_guidance() -> String {
    format!(
        "[Context Compression]\n\
         Only when asked to write a continuation summary (`/compact`, handoff, recovery), use \
         {CONTEXT_COMPRESSION_SECTION_ORDER}; provider-managed automatic compaction uses its own contract.\n\
         - Preserve latest intent, progress, decisions, unresolved blockers/failures, relevant files, and next action.\n\
         - Replace raw logs and stale tool chatter with {STALE_TOOL_RESULT_PLACEHOLDER_EXAMPLE}; omit resolved repetition."
    )
}

pub(super) fn tool_output_efficiency_guidance() -> &'static str {
    "[Tool Output Efficiency]\n\
     Constrain output before invoking a tool; a post-hoc summary cannot reclaim dumped context.\n\
     - Bash: use SQL LIMIT, targeted `rg`, or `head`; avoid broad grep and large tail counts\n\
     - Read/Grep: narrow ranges, globs, types, and head limits; do not dump whole files\n\
     - Keep only task-relevant output"
}

pub(super) fn api_friction_guidance(profile: DispatchProfile) -> Option<String> {
    (profile == DispatchProfile::Full).then_some(
        "\n\n[ADK API Usage]\n\
         Before ADK API work, inspect `GET /api/docs`, then `/api/docs/{group}/{category}`; docs override guessed CLI/source patterns. If missing or wrong, do not query the DB directly; emit one standalone `API_FRICTION: {...}` final-response line (endpoint, friction_type, summary, workaround, suggested_fix); runtime strips it."
            .to_string(),
    )
}

/// True when the agent's current working directory is (or is rooted at) an
/// AgentDesk checkout — detected by the presence of *both* repo-relative
/// source-of-truth documents. Used to gate repo-relative prompt references
/// (`docs/source-of-truth.md`, `docs/memory-scope.md`) so they are never
/// injected into agents whose workspace is a *different* repository (#4314),
/// where those files do not exist and the reference would point at nothing.
///
/// `exists` is the filesystem-injectable seam so tests can drive the
/// path-existence decision deterministically without touching the real
/// filesystem.
pub(super) fn workspace_has_agentdesk_docs_with(
    current_path: &str,
    exists: impl Fn(&str) -> bool,
) -> bool {
    let base = current_path.trim().trim_end_matches('/');
    if base.is_empty() {
        return false;
    }
    exists(&format!("{base}/docs/source-of-truth.md"))
        && exists(&format!("{base}/docs/memory-scope.md"))
}

pub(super) fn shared_agent_rules_lookup(current_path: &str) -> String {
    shared_agent_rules_lookup_with(current_path, |p| std::path::Path::new(p).exists())
}

/// Filesystem-injectable seam for [`shared_agent_rules_lookup`]. The generic
/// rules and the absolute `~/ObsidianVault/...` shared-prompt source are always
/// injected; only the two repo-relative `docs/*` references are gated on the
/// current workspace actually being an AgentDesk checkout (#4314).
fn shared_agent_rules_lookup_with(current_path: &str, exists: impl Fn(&str) -> bool) -> String {
    let mut section = String::from(
        "\n\n[Shared Agent Rules Index]\n\
         - Keep changes scoped, verified, and no broader than the current request.\n\
         - Verify user claims against code/data before acting.",
    );
    if workspace_has_agentdesk_docs_with(current_path, &exists) {
        section.push_str(
            "\n- Source-of-truth map: `docs/source-of-truth.md`; read it before editing prompts, config, skills, policies, or memory surfaces.",
        );
    }
    section.push_str(
        "\n- Full shared prompt source: `~/ObsidianVault/RemoteVault/adk-config/agents/_shared.prompt.md`; read it only when the task needs the detailed shared policy.",
    );
    section
}

pub(super) fn render_channel_participants(
    discord_context: &str,
    channel_participants: &[UserRecord],
) -> String {
    let is_dm_context = discord_context.trim() == "Discord context: DM";
    let mut lines = vec!["Channel participants:".to_string()];
    if channel_participants.is_empty() {
        lines.push("- none recorded yet".to_string());
        return lines.join("\n");
    }

    for (idx, user) in channel_participants.iter().enumerate() {
        let mut line = format!("- {}", user.label());
        if is_dm_context && channel_participants.len() == 1 && idx == 0 {
            line.push_str(" [DM requester]");
        }
        lines.push(line);
    }
    lines.join("\n")
}

#[cfg(test)]
mod workspace_aware_shared_rules_tests {
    //! #4314 — the repo-relative `docs/*` references must only appear when the
    //! agent's workspace actually is an AgentDesk checkout. The `_with(exists)`
    //! seam makes the path-existence decision injectable so these are
    //! deterministic and prove the guard bidirectionally (removing the guard
    //! makes one direction fail on its own assert, not on a compile error).
    use super::*;

    #[test]
    fn helper_requires_both_docs_and_rejects_empty() {
        // Both docs present → AgentDesk workspace.
        assert!(workspace_has_agentdesk_docs_with("/repo", |_| true));
        // Neither present → foreign workspace.
        assert!(!workspace_has_agentdesk_docs_with("/repo", |_| false));
        // Only one of the two present → still not treated as AgentDesk (AND).
        assert!(!workspace_has_agentdesk_docs_with("/repo", |p| p.ends_with("source-of-truth.md")));
        assert!(!workspace_has_agentdesk_docs_with("/repo", |p| p.ends_with("memory-scope.md")));
        // Empty / whitespace path → false without touching the filesystem.
        assert!(!workspace_has_agentdesk_docs_with("   ", |_| panic!(
            "must not probe empty path"
        )));
    }

    #[test]
    fn helper_probes_current_path_rooted_docs() {
        // The probed paths are `<cwd>/docs/source-of-truth.md` and
        // `<cwd>/docs/memory-scope.md`, and a trailing slash is tolerated.
        let mut probed: Vec<String> = Vec::new();
        let cell = std::cell::RefCell::new(&mut probed);
        let result = workspace_has_agentdesk_docs_with("/some/repo/", |p| {
            cell.borrow_mut().push(p.to_string());
            true
        });
        assert!(result);
        assert!(probed.contains(&"/some/repo/docs/source-of-truth.md".to_string()));
        assert!(probed.contains(&"/some/repo/docs/memory-scope.md".to_string()));
    }

    #[test]
    fn foreign_workspace_omits_repo_relative_doc_refs() {
        // T1 (guard mutation, A block): with the docs absent, the two
        // repo-relative references must NOT be injected, but the generic block
        // and the absolute shared-prompt source stay. Removing the guard so the
        // refs are always injected makes THIS assert fail on its own.
        let section = shared_agent_rules_lookup_with("/foreign/repo", |_| false);
        assert!(section.contains("[Shared Agent Rules Index]"));
        assert!(section.contains("Keep changes scoped, verified"));
        assert!(section.contains("_shared.prompt.md"));
        assert!(
            !section.contains("docs/source-of-truth.md"),
            "foreign workspace must not reference docs/source-of-truth.md, got: {section}"
        );
        assert!(
            !section.contains("docs/memory-scope.md"),
            "foreign workspace must not reference docs/memory-scope.md, got: {section}"
        );
    }

    #[test]
    fn agentdesk_workspace_keeps_repo_relative_doc_refs() {
        // T2 (reverse mutation, A block): with the docs present, the
        // source-of-truth reference must be injected. Memory policy is owned
        // by Proactive Memory Guidance, so the index must not duplicate it.
        // Forcing the guard to
        // always omit makes THIS assert fail on its own.
        let section = shared_agent_rules_lookup_with("/agentdesk", |_| true);
        assert!(section.contains("[Shared Agent Rules Index]"));
        assert!(
            section.contains("docs/source-of-truth.md"),
            "AgentDesk workspace must keep docs/source-of-truth.md, got: {section}"
        );
        assert!(!section.contains("docs/memory-scope.md"));
        assert!(section.contains("_shared.prompt.md"));
    }
}
