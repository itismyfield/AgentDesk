use super::settings::{
    ResolvedMemorySettings, discord_token_hash, load_review_tuning_guidance, load_role_prompt,
    load_shared_prompt_for_profile, render_peer_agent_guidance,
};
use super::*;
use crate::db::prompt_manifests::{PromptContentVisibility, PromptManifest};

mod channel_recent_context;
mod dispatch_contract;
mod layer_rendering;
mod manifest;
mod memory_guidance;

pub(crate) use channel_recent_context::{
    ChannelRecentContextManifestInput, load_channel_recent_context,
};
pub(crate) use dispatch_contract::CurrentTaskContext;
pub(crate) use manifest::RecoveryContextManifestInput;
pub(crate) use memory_guidance::MemoryRecallManifestInput;

use dispatch_contract::{render_current_task_section, render_dispatch_contract};
use layer_rendering::{
    agent_performance_prompt_section, api_friction_guidance, context_compression_guidance,
    render_channel_participants, shared_agent_rules_lookup, tool_output_efficiency_guidance,
};
use manifest::{
    build_prompt_manifest, channel_recent_context_manifest_layer, current_task_manifest_layer,
    dispatch_contract_manifest_layer, memory_recall_manifest_layer, prompt_manifest_layer,
    recovery_context_manifest_layer, role_prompt_manifest_layer,
};
use memory_guidance::proactive_memory_guidance;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct BuiltSystemPrompt {
    pub(super) system_prompt: String,
    pub(super) manifest: Option<PromptManifest>,
}

struct PromptManifestLayerHash<'a> {
    name: &'a str,
    sha256: &'a str,
}

impl std::fmt::Debug for PromptManifestLayerHash<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PromptManifestLayerHash")
            .field("name", &self.name)
            .field("sha256", &self.sha256)
            .finish()
    }
}

fn prompt_manifest_layer_hashes(manifest: &PromptManifest) -> Vec<PromptManifestLayerHash<'_>> {
    manifest
        .layers
        .iter()
        .map(|layer| PromptManifestLayerHash {
            name: layer.layer_name.as_str(),
            sha256: layer.content_sha256.as_str(),
        })
        .collect()
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SharedPromptProfile {
    Full,
    ReviewLite,
    Headless,
}

impl SharedPromptProfile {
    const fn for_dispatch(profile: DispatchProfile) -> Self {
        match profile {
            DispatchProfile::ReviewLite => Self::ReviewLite,
            DispatchProfile::Full | DispatchProfile::Lite => Self::Full,
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::ReviewLite => "review-lite",
            Self::Headless => "headless",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PromptProfiles {
    dispatch: DispatchProfile,
    shared: SharedPromptProfile,
}

impl PromptProfiles {
    const fn new(dispatch: DispatchProfile, shared: SharedPromptProfile) -> Self {
        Self { dispatch, shared }
    }

    pub(super) const fn foreground(dispatch: DispatchProfile) -> Self {
        Self::new(dispatch, SharedPromptProfile::for_dispatch(dispatch))
    }

    pub(super) const fn headless(dispatch: DispatchProfile) -> Self {
        Self::new(dispatch, SharedPromptProfile::Headless)
    }
}

// #3034: system-prompt assembly exercised by the dispatch-contract tests;
// the prod path builds the prompt through other entry points. Test contract.
#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
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
    is_claude_harness: bool,
) -> String {
    build_system_prompt_with_manifest(
        discord_context,
        channel_participants,
        current_path,
        channel_id,
        channel_id,
        token,
        role_binding,
        queued_turn,
        PromptProfiles::foreground(profile),
        dispatch_type,
        current_task,
        shared_knowledge,
        longterm_catalog,
        memory_settings,
        memento_mcp_available,
        is_claude_harness,
        None,
        None,
        None,
        None,
    )
    .system_prompt
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_system_prompt_with_manifest(
    discord_context: &str,
    channel_participants: &[UserRecord],
    current_path: &str,
    channel_id: ChannelId,
    memory_scope_channel_id: ChannelId,
    token: &str,
    role_binding: Option<&RoleBinding>,
    queued_turn: bool,
    profiles: PromptProfiles,
    dispatch_type: Option<&str>,
    current_task: Option<&CurrentTaskContext<'_>>,
    shared_knowledge: Option<&str>,
    longterm_catalog: Option<&str>,
    memory_settings: Option<&ResolvedMemorySettings>,
    memento_mcp_available: bool,
    is_claude_harness: bool,
    recovery_context: Option<&RecoveryContextManifestInput<'_>>,
    channel_recent_context: Option<&ChannelRecentContextManifestInput>,
    memory_recall_manifest: Option<&MemoryRecallManifestInput<'_>>,
    turn_id: Option<&str>,
) -> BuiltSystemPrompt {
    let PromptProfiles {
        dispatch: profile,
        shared: shared_prompt_profile,
    } = profiles;
    let mut prompt_manifest_layers = Vec::new();
    // Issue #2659: track per-build appendages so identical large content
    // (SAK / longterm_catalog / future skill listings) is never pushed
    // twice into the same system prompt. Static prelude blocks (the
    // "You are chatting..." preamble, Discord rules, etc.) are *not*
    // routed through the tracker because they are unique by construction
    // and don't benefit from a hash check.
    let mut dedupe_tracker = section_dedupe::PromptSectionTracker::new();
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
         If another instruction says to plan first, write a brief plan in plain text and proceed without entering plan mode. \
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
    prompt_manifest_layers.push(prompt_manifest_layer(
        "base_discord",
        "prompt_builder.base_discord",
        Some(format!(
            "profile={}",
            manifest::prompt_manifest_profile(profile)
        )),
        PromptContentVisibility::UserDerived,
        &system_prompt_owned,
    ));
    let tool_output_guidance = tool_output_efficiency_guidance();
    system_prompt_owned.push_str("\n\n");
    system_prompt_owned.push_str(tool_output_guidance);
    prompt_manifest_layers.push(prompt_manifest_layer(
        "tool_output_efficiency",
        "prompt_builder.tool_output_efficiency_guidance",
        None,
        PromptContentVisibility::AdkProvided,
        tool_output_guidance,
    ));

    if profile == DispatchProfile::Full {
        let compression_guidance = context_compression_guidance();
        system_prompt_owned.push_str("\n\n");
        system_prompt_owned.push_str(&compression_guidance);
        prompt_manifest_layers.push(prompt_manifest_layer(
            "context_compression",
            "prompt_builder.context_compression_guidance",
            None,
            PromptContentVisibility::AdkProvided,
            &compression_guidance,
        ));
    }

    if let Some(binding) = role_binding {
        // ReviewLite: inject minimal review rules instead of full shared prompt.
        // review and review-decision have different contracts:
        //   review          → read code, post review comment, submit verdict via /api/reviews/verdict
        //   review-decision → read counter-review feedback, submit accept/dispute/dismiss via /api/reviews/decision
        if profile == DispatchProfile::ReviewLite {
            let review_rules = match dispatch_type {
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
            };
            system_prompt_owned.push_str(&review_rules);
            prompt_manifest_layers.push(prompt_manifest_layer(
                "review_rules",
                "prompt_builder.review_rules",
                dispatch_type.map(|value| format!("dispatch_type={value}")),
                PromptContentVisibility::AdkProvided,
                review_rules.trim_start_matches("\n\n"),
            ));

            // #119: Inject review tuning guidance only for review dispatches (not review-decision).
            // Injecting into review-decision would bias the labeler's accept/dispute/dismiss judgment,
            // contaminating the FP/TP dataset that the guidance itself is derived from.
            if dispatch_type != Some("review-decision") {
                if let Some(guidance) = load_review_tuning_guidance() {
                    let section =
                        format!("[Review Tuning — 과거 리뷰 정확도 기반 가이던스]\n{guidance}");
                    system_prompt_owned.push_str("\n\n");
                    system_prompt_owned.push_str(&section);
                    prompt_manifest_layers.push(prompt_manifest_layer(
                        "review_tuning",
                        "settings.load_review_tuning_guidance",
                        None,
                        PromptContentVisibility::AdkProvided,
                        &section,
                    ));
                }
            }
        } else if profile == DispatchProfile::Lite {
            let lite_rules = "[Lite Channel Rules]\n\
                 - 한국어로 간결하게 소통한다\n\
                 - 현재 요청에 필요한 범위만 확인하고 불필요한 파일 탐색을 피한다\n\
                 - 큰 변경이나 장시간 작업이 필요하면 먼저 범위와 다음 행동을 짧게 확인한다";
            system_prompt_owned.push_str("\n\n");
            system_prompt_owned.push_str(lite_rules);
            prompt_manifest_layers.push(prompt_manifest_layer(
                "lite_channel_rules",
                "prompt_builder.lite_channel_rules",
                None,
                PromptContentVisibility::AdkProvided,
                lite_rules,
            ));
        }

        if profile != DispatchProfile::Lite {
            let shared_profile = shared_prompt_profile.as_str();
            if let Some(shared_prompt) = load_shared_prompt_for_profile(shared_profile) {
                let shared_rules = format!("\n\n[Shared Agent Rules]\n{shared_prompt}");
                if dedupe_tracker.record("shared_agent_rules", &shared_rules) {
                    tracing::warn!(
                        "  [role-map] Injected {} shared agent rules ({} chars) for channel {}",
                        shared_profile,
                        shared_rules.len(),
                        channel_id.get()
                    );
                    system_prompt_owned.push_str(&shared_rules);
                    prompt_manifest_layers.push(prompt_manifest_layer(
                        "shared_agent_rules",
                        "settings.load_shared_prompt_for_profile",
                        Some(format!("profile={shared_profile}")),
                        PromptContentVisibility::AdkProvided,
                        &shared_rules,
                    ));
                }
            } else {
                // #4314: the shared-rules index now depends on the agent's cwd —
                // repo-relative `docs/*` references are injected only when the
                // workspace actually is an AgentDesk checkout. Compute once and
                // reuse for both the log and the append.
                let shared_rules = shared_agent_rules_lookup(current_path);
                tracing::warn!(
                    "  [role-map] Injected compact shared rule index ({} chars) for channel {}",
                    shared_rules.len(),
                    channel_id.get()
                );
                system_prompt_owned.push_str(&shared_rules);
                prompt_manifest_layers.push(prompt_manifest_layer(
                    "shared_agent_rules",
                    "prompt_builder.shared_agent_rules_lookup",
                    Some(format!("workspace={current_path}")),
                    PromptContentVisibility::AdkProvided,
                    &shared_rules,
                ));
            }
        }

        if profile != DispatchProfile::Lite {
            match load_role_prompt(binding) {
                Some(role_prompt) => {
                    prompt_manifest_layers.push(role_prompt_manifest_layer(
                        binding,
                        true,
                        Some(role_prompt.clone()),
                    ));
                    let role_binding_contract = "[Channel Role Binding]\n\
                         The following role definition is authoritative for this Discord channel.\n\
                         You MUST answer as this role, stay within its scope, and follow its response contract.\n\
                         Do NOT override it with a generic assistant persona or by inferring a different role from repository files,\n\
                         unless the user explicitly asks you to audit or compare role definitions.";
                    system_prompt_owned.push_str("\n\n");
                    system_prompt_owned.push_str(role_binding_contract);
                    system_prompt_owned.push_str("\n\n");
                    system_prompt_owned.push_str(&role_prompt);
                    prompt_manifest_layers.push(prompt_manifest_layer(
                        "role_binding_contract",
                        "prompt_builder.role_binding_contract",
                        Some(format!("agent_id={}", binding.role_id)),
                        PromptContentVisibility::AdkProvided,
                        role_binding_contract,
                    ));
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
        //
        // Issue #2659: route every large externally-sourced section through
        // `dedupe_tracker.record(...)` so the same SHA-256-identical block
        // is never appended twice in one build. Behavior is preserved on
        // the happy path (first-time appendage always records); duplicate
        // attempts only trip a WARN log and skip the push. SAK remains Full-only:
        // ReviewLite retains its review contract without the broad operational context.
        if profile == DispatchProfile::Full {
            if let Some(sak) = shared_knowledge
                && dedupe_tracker.record("shared_knowledge", sak)
            {
                system_prompt_owned.push_str("\n\n");
                system_prompt_owned.push_str(sak);
                prompt_manifest_layers.push(prompt_manifest_layer(
                    "shared_knowledge",
                    "memory.shared_knowledge",
                    None,
                    PromptContentVisibility::AdkProvided,
                    sak,
                ));
            }
        }

        // ReviewLite/Lite: skip shared operational context, long-term memory, and peer agents.
        if profile == DispatchProfile::Full {
            if let Some(catalog) = longterm_catalog
                && dedupe_tracker.record("longterm_catalog", catalog)
            {
                let longterm_contract = "[Long-term Memory]\n\
                     Available memory files for this agent. Use the Read tool to load full content when needed:";
                system_prompt_owned.push_str("\n\n");
                system_prompt_owned.push_str(longterm_contract);
                system_prompt_owned.push('\n');
                system_prompt_owned.push_str(catalog);
                prompt_manifest_layers.push(prompt_manifest_layer(
                    "longterm_memory_contract",
                    "prompt_builder.longterm_memory_contract",
                    None,
                    PromptContentVisibility::AdkProvided,
                    longterm_contract,
                ));
                prompt_manifest_layers.push(prompt_manifest_layer(
                    "longterm_catalog",
                    "memory.longterm_catalog",
                    None,
                    PromptContentVisibility::AdkProvided,
                    catalog,
                ));
            }

            if binding.peer_agents_enabled {
                if let Some(peer_guidance) = render_peer_agent_guidance(&binding.role_id) {
                    system_prompt_owned.push_str("\n\n");
                    system_prompt_owned.push_str(&peer_guidance);
                    prompt_manifest_layers.push(prompt_manifest_layer(
                        "peer_agent_directory",
                        "settings.render_peer_agent_guidance",
                        Some(format!("agent_id={}", binding.role_id)),
                        PromptContentVisibility::AdkProvided,
                        &peer_guidance,
                    ));
                }
            }
        }
    } else if profile == DispatchProfile::Full {
        if let Some(sak) = shared_knowledge
            && dedupe_tracker.record("shared_knowledge", sak)
        {
            // No role binding — Full still injects SAK (no LTM/peer agents to worry about)
            system_prompt_owned.push_str("\n\n");
            system_prompt_owned.push_str(sak);
            prompt_manifest_layers.push(prompt_manifest_layer(
                "shared_knowledge",
                "memory.shared_knowledge",
                Some("unbound_channel".to_string()),
                PromptContentVisibility::AdkProvided,
                sak,
            ));
        }
    }

    if let Some(memory_guidance) = proactive_memory_guidance(
        memory_settings,
        current_path,
        memory_scope_channel_id,
        role_binding,
        profile,
        memento_mcp_available,
        is_claude_harness,
    ) {
        system_prompt_owned.push_str(&memory_guidance);
        prompt_manifest_layers.push(prompt_manifest_layer(
            "memory_guidance",
            "prompt_builder.proactive_memory_guidance",
            None,
            PromptContentVisibility::AdkProvided,
            memory_guidance.trim_start_matches("\n\n"),
        ));
    }
    if let Some(api_friction_guidance) = api_friction_guidance(profile) {
        system_prompt_owned.push_str(&api_friction_guidance);
        prompt_manifest_layers.push(prompt_manifest_layer(
            "api_friction_guidance",
            "prompt_builder.api_friction_guidance",
            None,
            PromptContentVisibility::AdkProvided,
            api_friction_guidance.trim_start_matches("\n\n"),
        ));
    }
    if let Some(performance_section) = agent_performance_prompt_section(role_binding, profile) {
        system_prompt_owned.push_str("\n\n");
        system_prompt_owned.push_str(&performance_section);
        prompt_manifest_layers.push(prompt_manifest_layer(
            "agent_performance",
            "prompt_builder.agent_performance_prompt_section",
            None,
            PromptContentVisibility::AdkProvided,
            &performance_section,
        ));
    }

    if queued_turn {
        let queued_rules = "[Queued Turn Rules]\n\
             This user message was queued while another turn was running.\n\
             Treat ONLY the latest queued user message in this turn as actionable.\n\
             Do NOT repeat, combine, or continue prior queued messages unless the latest user message explicitly asks for that.\n\
             If the latest user message asks for an exact literal output, return exactly that literal output and nothing else.";
        system_prompt_owned.push_str("\n\n");
        system_prompt_owned.push_str(queued_rules);
        prompt_manifest_layers.push(prompt_manifest_layer(
            "queued_turn_rules",
            "prompt_builder.queued_turn_rules",
            None,
            PromptContentVisibility::AdkProvided,
            queued_rules,
        ));
    }
    if let Some((task, current_task_section)) = current_task.and_then(|task| {
        render_current_task_section(task, dispatch_type).map(|section| (task, section))
    }) {
        let current_task_without_contract = render_dispatch_contract(dispatch_type, task)
            .and_then(|contract| {
                current_task_section
                    .strip_suffix(&contract)
                    .map(str::trim_end)
                    .filter(|section| *section != "[Current Task]")
                    .map(str::to_string)
            })
            .unwrap_or_else(|| current_task_section.clone());
        prompt_manifest_layers.push(current_task_manifest_layer(
            task,
            &current_task_without_contract,
        ));
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
    prompt_manifest_layers.push(channel_recent_context_manifest_layer(
        channel_recent_context,
    ));

    if profile != DispatchProfile::Full {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 📉 {:?} prompt: {} chars (channel {})",
            profile,
            system_prompt_owned.len(),
            channel_id.get()
        );
    }
    // Issue #2659: always-on structured telemetry — captures the final
    // build size + dedup tracker's accounting so log scrapers can spot a
    // regression where the prompt grew by ~25KB unexpectedly.
    tracing::debug!(
        target: "agentdesk.prompt_section_dedupe",
        profile = ?profile,
        channel_id = channel_id.get(),
        prompt_bytes = system_prompt_owned.len(),
        tracked_section_bytes = dedupe_tracker.appended_bytes(),
        "prompt build complete"
    );

    let mut manifest = build_prompt_manifest(
        turn_id,
        channel_id,
        profile,
        current_task,
        prompt_manifest_layers,
    );
    if let Some(prompt_manifest) = manifest.as_mut() {
        let external_context_bytes = prompt_manifest
            .layers
            .iter()
            .filter(|layer| {
                layer.enabled
                    && matches!(
                        layer.layer_name.as_str(),
                        manifest::MEMORY_RECALL_LAYER_NAME
                            | manifest::RECOVERY_CONTEXT_LAYER_NAME
                            | manifest::CHANNEL_RECENT_CONTEXT_LAYER_NAME
                    )
            })
            .fold(0_i64, |sum, layer| {
                sum.saturating_add(layer.original_bytes.unwrap_or(layer.chars).max(0))
            });
        prompt_manifest.total_input_bytes = i64::try_from(system_prompt_owned.len())
            .unwrap_or(i64::MAX)
            .saturating_add(external_context_bytes);
    }
    if let Some(prompt_manifest) = manifest.as_ref() {
        let layer_hashes = prompt_manifest_layer_hashes(prompt_manifest);
        tracing::info!(
            target: "agentdesk.prompt_manifest",
            turn_id = %prompt_manifest.turn_id,
            channel_id = %prompt_manifest.channel_id,
            layer_count = prompt_manifest.layer_count,
            total_input_bytes = prompt_manifest.total_input_bytes,
            total_input_tokens_est = prompt_manifest.total_input_tokens_est,
            layer_hashes = ?layer_hashes,
            "recorded prompt manifest"
        );
    }

    BuiltSystemPrompt {
        system_prompt: system_prompt_owned,
        manifest,
    }
}

mod section_dedupe;

#[cfg(test)]
mod dispatch_contract_tests;
