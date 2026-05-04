use super::settings::{
    ResolvedMemorySettings, discord_token_hash, load_review_tuning_guidance, load_role_prompt,
    render_peer_agent_guidance,
};
use super::*;
use crate::db::prompt_manifests::PromptManifest;

mod dispatch_contract;
mod layer_rendering;
mod manifest;
mod memory_guidance;

pub(crate) use dispatch_contract::CurrentTaskContext;
pub(crate) use manifest::RecoveryContextManifestInput;
pub(crate) use memory_guidance::MemoryRecallManifestInput;

use dispatch_contract::render_current_task_section;
use layer_rendering::{
    agent_performance_prompt_section, api_friction_guidance, context_compression_guidance,
    render_channel_participants, shared_agent_rules_lookup, tool_output_efficiency_guidance,
};
use manifest::{
    build_prompt_manifest, current_task_manifest_layer, dispatch_contract_manifest_layer,
    memory_recall_manifest_layer, recovery_context_manifest_layer, role_prompt_manifest_layer,
};
use memory_guidance::proactive_memory_guidance;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BuiltSystemPrompt {
    pub(super) system_prompt: String,
    pub(super) manifest: Option<PromptManifest>,
}

/// Dispatch prompt profile — controls which system prompt sections are injected.
/// `Full` includes the normal Discord contract plus compact lookup indexes
/// (used for implementation dispatches and normal turns).
/// `Lite` is an opt-in channel profile for low-frequency general channels.
/// `ReviewLite` strips peer agents, long-term memory, and skills to reduce token cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DispatchProfile {
    /// Full system prompt — normal turns with compact shared-rule indexes.
    Full,
    /// Lightweight general-channel prompt. Keeps the base Discord/tooling
    /// context but skips shared prompt files, role prompts, and heavy memory.
    Lite,
    /// Minimal prompt for review/review-decision dispatches.
    /// Includes: base context, review rules.
    /// Excludes: skills, peer agent directory, long-term memory.
    ReviewLite,
}

impl DispatchProfile {
    /// Derive profile from dispatch type string.
    pub fn from_dispatch_type(dispatch_type: Option<&str>) -> Self {
        match dispatch_type {
            Some("review") | Some("review-decision") => Self::ReviewLite,
            _ => Self::Full,
        }
    }

    pub fn for_turn(dispatch_type: Option<&str>, channel_profile: Option<Self>) -> Self {
        match Self::from_dispatch_type(dispatch_type) {
            Self::ReviewLite => Self::ReviewLite,
            Self::Full | Self::Lite => channel_profile.unwrap_or(Self::Full),
        }
    }
}

pub(super) fn build_system_prompt(
    discord_context: &str,
    channel_participants: &[UserRecord],
    current_path: &str,
    channel_id: ChannelId,
    token: &str,
    role_binding: Option<&RoleBinding>,
    queued_turn: bool,
    profile: DispatchProfile,
    dispatch_type: Option<&str>,
    current_task: Option<&CurrentTaskContext<'_>>,
    shared_knowledge: Option<&str>,
    longterm_catalog: Option<&str>,
    memory_settings: Option<&ResolvedMemorySettings>,
    memento_mcp_available: bool,
) -> String {
    build_system_prompt_with_manifest(
        discord_context,
        channel_participants,
        current_path,
        channel_id,
        token,
        role_binding,
        queued_turn,
        profile,
        dispatch_type,
        current_task,
        shared_knowledge,
        longterm_catalog,
        memory_settings,
        memento_mcp_available,
        None,
        None,
        None,
    )
    .system_prompt
}

pub(super) fn build_system_prompt_with_manifest(
    discord_context: &str,
    channel_participants: &[UserRecord],
    current_path: &str,
    channel_id: ChannelId,
    token: &str,
    role_binding: Option<&RoleBinding>,
    queued_turn: bool,
    profile: DispatchProfile,
    dispatch_type: Option<&str>,
    current_task: Option<&CurrentTaskContext<'_>>,
    shared_knowledge: Option<&str>,
    longterm_catalog: Option<&str>,
    memory_settings: Option<&ResolvedMemorySettings>,
    memento_mcp_available: bool,
    recovery_context: Option<&RecoveryContextManifestInput<'_>>,
    memory_recall_manifest: Option<&MemoryRecallManifestInput<'_>>,
    turn_id: Option<&str>,
) -> BuiltSystemPrompt {
    let mut prompt_manifest_layers = Vec::new();
    let mut system_prompt_owned = format!(
        "You are chatting with a user through Discord.\n\
         {}\n\
         {}\n\
         Current working directory: {}\n\n\
         When your work produces a file the user would want (generated code, reports, images, archives, etc.),\n\
         send it by running this bash command:\n\n\
         agentdesk discord-sendfile <filepath> --channel {} --key {}\n\n\
         This delivers the file directly to the user's Discord channel.\n\
         Do NOT tell the user to use /down — use the command above instead.\n\n\
         When referencing files in your text, include the specific path (e.g. \"mod.rs:2700\"). \
         The user sees only your text output, not the tool calls themselves.\n\n\
         Discord formatting rules:\n\
         - Use inline `code` for short references. Reserve code blocks for actual code snippets.\n\
         - Keep messages concise and scannable on mobile. Prefer short paragraphs and bullet points.\n\
         - Avoid decorative separators or long horizontal lines.\n\n\
         This Discord channel does not support interactive prompts. Do NOT call AskUserQuestion, EnterPlanMode, or ExitPlanMode. \
         Ask in plain text if you need clarification.\n\n\
         Message author prefix: Direct user messages are prefixed as `[User: NAME (ID: N)]`; use that marker to distinguish speakers in shared channels.\n\n\
         Reply context: When a user message includes a [Reply context] tag, the user is responding to the **replied-to message**, \
         not necessarily your most recent message. Prioritize the reply target; ask if ambiguous.",
        discord_context,
        render_channel_participants(discord_context, channel_participants),
        current_path,
        channel_id.get(),
        discord_token_hash(token),
    );
    system_prompt_owned.push_str("\n\n");
    system_prompt_owned.push_str(tool_output_efficiency_guidance());

    if profile == DispatchProfile::Full {
        system_prompt_owned.push_str("\n\n");
        system_prompt_owned.push_str(&context_compression_guidance());
    }

    if let Some(binding) = role_binding {
        // ReviewLite: inject minimal review rules instead of full shared prompt.
        // review and review-decision have different contracts:
        //   review          → read code, post review comment, submit verdict via /api/reviews/verdict
        //   review-decision → read counter-review feedback, submit accept/dispute/dismiss via /api/reviews/decision
        if profile == DispatchProfile::ReviewLite {
            system_prompt_owned.push_str(&match dispatch_type {
                Some("review-decision") => "\n\n[Review Decision Rules]\n\
                     - 한국어로 소통한다\n\
                     - 카운터 리뷰 피드백을 읽고 accept/dispute/dismiss 중 결정한다\n\
                     - POST /api/reviews/decision {card_id, decision, comment}로 결정을 제출한다\n\
                     - decision: accept(피드백 수용→rework), dispute(반박→재리뷰), dismiss(무시→done)"
                        .to_string(),
                _ => "\n\n[Review Rules]\n\
                     - 한국어로 소통한다\n\
                     - 리뷰 결과는 GitHub issue 코멘트로 남긴다\n\
                     - 리뷰 verdict 제출 후 dispatch를 완료한다"
                        .to_string(),
            });

            // #119: Inject review tuning guidance only for review dispatches (not review-decision).
            // Injecting into review-decision would bias the labeler's accept/dispute/dismiss judgment,
            // contaminating the FP/TP dataset that the guidance itself is derived from.
            if dispatch_type != Some("review-decision") {
                if let Some(guidance) = load_review_tuning_guidance() {
                    system_prompt_owned
                        .push_str("\n\n[Review Tuning — 과거 리뷰 정확도 기반 가이던스]\n");
                    system_prompt_owned.push_str(&guidance);
                }
            }
        } else if profile == DispatchProfile::Lite {
            system_prompt_owned.push_str(
                "\n\n[Lite Channel Rules]\n\
                 - 한국어로 간결하게 소통한다\n\
                 - 현재 요청에 필요한 범위만 확인하고 불필요한 파일 탐색을 피한다\n\
                 - 큰 변경이나 장시간 작업이 필요하면 먼저 범위와 다음 행동을 짧게 확인한다",
            );
        } else {
            system_prompt_owned.push_str(shared_agent_rules_lookup());
            tracing::warn!(
                "  [role-map] Injected compact shared rule index ({} chars) for channel {}",
                shared_agent_rules_lookup().len(),
                channel_id.get()
            );
        }

        if profile != DispatchProfile::Lite {
            match load_role_prompt(binding) {
                Some(role_prompt) => {
                    prompt_manifest_layers.push(role_prompt_manifest_layer(
                        binding,
                        true,
                        Some(role_prompt.clone()),
                    ));
                    system_prompt_owned.push_str(
                        "\n\n[Channel Role Binding]\n\
                         The following role definition is authoritative for this Discord channel.\n\
                         You MUST answer as this role, stay within its scope, and follow its response contract.\n\
                         Do NOT override it with a generic assistant persona or by inferring a different role from repository files,\n\
                         unless the user explicitly asks you to audit or compare role definitions.\n\n",
                    );
                    system_prompt_owned.push_str(&role_prompt);
                    tracing::warn!(
                        "  [role-map] Applied role '{}' for channel {}",
                        binding.role_id,
                        channel_id.get()
                    );
                }
                None => {
                    prompt_manifest_layers.push(role_prompt_manifest_layer(binding, false, None));
                    tracing::warn!(
                        "  [role-map] Failed to load prompt file '{}' for role '{}' (channel {})",
                        binding.prompt_file,
                        binding.role_id,
                        channel_id.get()
                    );
                }
            }
        } else {
            prompt_manifest_layers.push(role_prompt_manifest_layer(binding, false, None));
        }

        // SAK before LTM: placed here for cache prefix stability — SAK and
        // everything above it rarely changes, maximising Anthropic prefix cache hits.
        if profile != DispatchProfile::Lite {
            if let Some(sak) = shared_knowledge {
                system_prompt_owned.push_str("\n\n");
                system_prompt_owned.push_str(sak);
            }
        }

        // ReviewLite/Lite: skip long-term memory and peer agents to save tokens
        if profile == DispatchProfile::Full {
            if let Some(catalog) = longterm_catalog {
                system_prompt_owned.push_str(
                    "\n\n[Long-term Memory]\n\
                     Available memory files for this agent. Use the Read tool to load full content when needed:\n",
                );
                system_prompt_owned.push_str(&catalog);
            }

            if binding.peer_agents_enabled {
                if let Some(peer_guidance) = render_peer_agent_guidance(&binding.role_id) {
                    system_prompt_owned.push_str("\n\n");
                    system_prompt_owned.push_str(&peer_guidance);
                }
            }
        }
    } else if profile != DispatchProfile::Lite {
        if let Some(sak) = shared_knowledge {
            // No role binding — still inject SAK (no LTM/peer agents to worry about)
            system_prompt_owned.push_str("\n\n");
            system_prompt_owned.push_str(sak);
        }
    }

    if let Some(memory_guidance) = proactive_memory_guidance(
        memory_settings,
        current_path,
        channel_id,
        role_binding,
        profile,
        memento_mcp_available,
    ) {
        system_prompt_owned.push_str(&memory_guidance);
    }
    if let Some(api_friction_guidance) = api_friction_guidance(profile) {
        system_prompt_owned.push_str(&api_friction_guidance);
    }
    if let Some(performance_section) = agent_performance_prompt_section(role_binding, profile) {
        system_prompt_owned.push_str("\n\n");
        system_prompt_owned.push_str(&performance_section);
    }

    if queued_turn {
        system_prompt_owned.push_str(
            "\n\n[Queued Turn Rules]\n\
             This user message was queued while another turn was running.\n\
             Treat ONLY the latest queued user message in this turn as actionable.\n\
             Do NOT repeat, combine, or continue prior queued messages unless the latest user message explicitly asks for that.\n\
             If the latest user message asks for an exact literal output, return exactly that literal output and nothing else.",
        );
    }
    if let Some((task, current_task_section)) = current_task.and_then(|task| {
        render_current_task_section(task, dispatch_type).map(|section| (task, section))
    }) {
        prompt_manifest_layers.push(current_task_manifest_layer(task, &current_task_section));
        system_prompt_owned.push_str("\n\n");
        system_prompt_owned.push_str(&current_task_section);
    }
    prompt_manifest_layers.push(dispatch_contract_manifest_layer(
        dispatch_type,
        current_task,
    ));
    if let Some(layer) = memory_recall_manifest_layer(
        memory_settings,
        memento_mcp_available,
        memory_recall_manifest,
    ) {
        prompt_manifest_layers.push(layer);
    }
    match recovery_context_manifest_layer(recovery_context) {
        Ok(layer) => prompt_manifest_layers.push(layer),
        Err(error) => {
            tracing::warn!(
                target: "agentdesk.prompt_manifest",
                "failed to record recovery_context prompt manifest layer: {error}"
            );
            prompt_manifest_layers
                .push(recovery_context_manifest_layer(None).expect("disabled recovery layer"));
        }
    }

    if profile != DispatchProfile::Full {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 📉 {:?} prompt: {} chars (channel {})",
            profile,
            system_prompt_owned.len(),
            channel_id.get()
        );
    }

    let manifest = build_prompt_manifest(
        turn_id,
        channel_id,
        profile,
        current_task,
        prompt_manifest_layers,
    );
    if let Some(prompt_manifest) = manifest.as_ref() {
        if let Ok(manifest_json) = serde_json::to_string(prompt_manifest) {
            tracing::info!(
                target: "agentdesk.prompt_manifest",
                prompt_manifest = %manifest_json,
                "recorded prompt manifest"
            );
        }
    }

    BuiltSystemPrompt {
        system_prompt: system_prompt_owned,
        manifest,
    }
}

#[cfg(test)]
mod dispatch_contract_tests;

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests;
