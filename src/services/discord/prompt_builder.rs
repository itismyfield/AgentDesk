use super::settings::{
    MemoryBackendKind, ResolvedMemorySettings, discord_token_hash, load_review_tuning_guidance,
    load_role_prompt, load_shared_prompt, render_peer_agent_guidance,
};
use super::*;

const CONTEXT_COMPRESSION_SECTION_ORDER: &str = "`Goal`, `Progress`, `Decisions`, `Files`, `Next`";
const STALE_TOOL_RESULT_PLACEHOLDER_EXAMPLE: &str =
    "[이전 결과 — 3줄 요약: cargo test failed in src/foo.rs because ...]";

fn context_compression_guidance() -> String {
    format!(
        "[Context Compression]\n\
         When conversation compaction happens (`/compact`, automatic compaction, or equivalent summarization), \
         rewrite prior context using these sections in order: {CONTEXT_COMPRESSION_SECTION_ORDER}.\n\
         - Keep each section short, factual, and focused on the latest state.\n\
         - Preserve unresolved blockers, assumptions, failures, and the latest user intent.\n\
         - In `Files`, list only files that still matter and why they matter.\n\
         - Replace stale tool chatter, raw logs, and old command output with placeholders like {STALE_TOOL_RESULT_PLACEHOLDER_EXAMPLE}.\n\
         - Prefer outcomes and follow-up implications over verbatim output, and drop already-resolved repetition once summarized."
    )
}

pub(crate) fn build_followup_turn_system_reminder() -> String {
    format!(
        "<system-reminder>\n\
         Discord formatting: minimize code blocks, keep messages concise.\n\
         If the session was compacted, treat the compacted summary as authoritative.\n\
         Keep prior context organized as {CONTEXT_COMPRESSION_SECTION_ORDER}.\n\
         Replace stale tool chatter, raw logs, and old command output with placeholders like {STALE_TOOL_RESULT_PLACEHOLDER_EXAMPLE} instead of replaying them verbatim.\n\
         </system-reminder>"
    )
}

fn proactive_memory_guidance(
    memory_settings: Option<&ResolvedMemorySettings>,
    profile: DispatchProfile,
) -> Option<String> {
    if profile != DispatchProfile::Full {
        return None;
    }

    let settings = memory_settings?;
    let (backend_name, read_tool, write_tool, extra_note) = match settings.backend {
        MemoryBackendKind::File => ("local", "`memory-read` skill", "`memory-write` skill", ""),
        MemoryBackendKind::Mem0 => (
            "mem0",
            "`search_memory` MCP tool",
            "`add_memories` MCP tool",
            "",
        ),
        MemoryBackendKind::Memento => (
            "memento",
            "`recall` MCP tool",
            "`remember` MCP tool",
            "\n- 참고: 턴 시작 `context` 주입과 세션 종료 시 `reflect`는 서버가 담당한다. 턴 중 보강만 `recall`/`remember`로 수행한다.",
        ),
    };

    Some(format!(
        "\n\n[Proactive Memory Guidance]\n\
         이 세션에서 `{backend_name}` 메모리를 사용할 수 있습니다.\n\
         - 읽기: {read_tool} — 새로운 맥락 발견 시 추가 조회\n\
         - 쓰기: {write_tool} — 중요한 결정/에러/절차 발견 시 기록\n\
         - 트리거: 에러 원인 확정, 아키텍처 결정, 설정 변경, \"이전에\" 언급 시{extra_note}"
    ))
}
/// Dispatch prompt profile — controls which system prompt sections are injected.
/// `Full` includes everything (used for implementation dispatches and normal turns).
/// `ReviewLite` strips peer agents, long-term memory, and skills to reduce token cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DispatchProfile {
    /// Full system prompt — all sections included (implementation, normal turns)
    Full,
    /// Minimal prompt for review/review-decision dispatches.
    /// Includes: base context, shared agent rules, role binding.
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
}

pub(super) fn build_system_prompt(
    discord_context: &str,
    current_path: &str,
    channel_id: ChannelId,
    token: &str,
    disabled_notice: &str,
    skills_notice: &str,
    narrate_progress: bool,
    role_binding: Option<&RoleBinding>,
    queued_turn: bool,
    profile: DispatchProfile,
    dispatch_type: Option<&str>,
    shared_knowledge: Option<&str>,
    longterm_catalog: Option<&str>,
    memory_settings: Option<&ResolvedMemorySettings>,
) -> String {
    let narration_guidance = if narrate_progress {
        "\n\nAlways keep the user informed about what you are doing. Briefly explain each step as you work \
         (e.g. \"Reading the file...\", \"Creating the script...\", \"Running tests...\")."
    } else {
        ""
    };
    let mut system_prompt_owned = format!(
        "You are chatting with a user through Discord.\n\
         {}\n\
         Current working directory: {}\n\n\
         When your work produces a file the user would want (generated code, reports, images, archives, etc.),\n\
         send it by running this bash command:\n\n\
         agentdesk discord-sendfile <filepath> --channel {} --key {}\n\n\
         This delivers the file directly to the user's Discord channel.\n\
         Do NOT tell the user to use /down — use the command above instead.{}\n\n\
         IMPORTANT: When reading, editing, or searching files, ALWAYS mention the specific file path and what you're looking for \
         (e.g. \"mod.rs:2700 부근의 시스템 프롬프트를 확인합니다\" not just \"코드를 확인합니다\"). \
         The user sees only your text output, not the tool calls themselves.\n\n\
         Discord formatting rules:\n\
         - Minimize code blocks. Use inline `code` for short references. Only use code blocks for actual code snippets the user needs.\n\
         - Keep messages concise and scannable on mobile screens. Prefer short paragraphs and bullet points.\n\
         - Avoid long horizontal lines or decorative separators.\n\n\
         IMPORTANT: The user is on Discord and CANNOT interact with any interactive prompts, dialogs, or confirmation requests. \
         All tools that require user interaction (such as AskUserQuestion, EnterPlanMode, ExitPlanMode) will NOT work. \
         Never use tools that expect user interaction. If you need clarification, just ask in plain text.{}{}",
        discord_context,
        current_path,
        channel_id.get(),
        discord_token_hash(token),
        narration_guidance,
        disabled_notice,
        // ReviewLite: omit skills to save tokens — reviewer only submits verdict
        if profile == DispatchProfile::ReviewLite {
            ""
        } else {
            skills_notice
        }
    );
    if profile == DispatchProfile::Full {
        system_prompt_owned.push_str("\n\n");
        system_prompt_owned.push_str(&context_compression_guidance());
    }

    if let Some(binding) = role_binding {
        // ReviewLite: inject minimal review rules instead of full shared prompt.
        // review and review-decision have different contracts:
        //   review          → read code, post review comment, submit verdict via /api/review-verdict
        //   review-decision → read counter-review feedback, submit accept/dispute/dismiss via /api/review-decision
        if profile == DispatchProfile::ReviewLite {
            system_prompt_owned.push_str(&match dispatch_type {
                Some("review-decision") => "\n\n[Review Decision Rules]\n\
                     - 한국어로 소통한다\n\
                     - 카운터 리뷰 피드백을 읽고 accept/dispute/dismiss 중 결정한다\n\
                     - POST /api/review-decision {card_id, decision, comment}로 결정을 제출한다\n\
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
        } else if let Some(shared_prompt) = load_shared_prompt() {
            // Full profile: inject complete shared agent prompt (AGENTS.md)
            system_prompt_owned.push_str("\n\n[Shared Agent Rules]\n");
            system_prompt_owned.push_str(&shared_prompt);
            eprintln!(
                "  [role-map] Injected shared prompt ({} chars) for channel {}",
                shared_prompt.len(),
                channel_id.get()
            );
        }

        match load_role_prompt(binding) {
            Some(role_prompt) => {
                system_prompt_owned.push_str(
                    "\n\n[Channel Role Binding]\n\
                     The following role definition is authoritative for this Discord channel.\n\
                     You MUST answer as this role, stay within its scope, and follow its response contract.\n\
                     Do NOT override it with a generic assistant persona or by inferring a different role from repository files,\n\
                     unless the user explicitly asks you to audit or compare role definitions.\n\n",
                );
                system_prompt_owned.push_str(&role_prompt);
                eprintln!(
                    "  [role-map] Applied role '{}' for channel {}",
                    binding.role_id,
                    channel_id.get()
                );
            }
            None => {
                eprintln!(
                    "  [role-map] Failed to load prompt file '{}' for role '{}' (channel {})",
                    binding.prompt_file,
                    binding.role_id,
                    channel_id.get()
                );
            }
        }

        // SAK before LTM: placed here for cache prefix stability — SAK and
        // everything above it rarely changes, maximising Anthropic prefix cache hits.
        if let Some(sak) = shared_knowledge {
            system_prompt_owned.push_str("\n\n");
            system_prompt_owned.push_str(sak);
        }

        // ReviewLite: skip long-term memory and peer agents to save tokens
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
    } else if let Some(sak) = shared_knowledge {
        // No role binding — still inject SAK (no LTM/peer agents to worry about)
        system_prompt_owned.push_str("\n\n");
        system_prompt_owned.push_str(sak);
    }

    if let Some(memory_guidance) = proactive_memory_guidance(memory_settings, profile) {
        system_prompt_owned.push_str(&memory_guidance);
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

    if profile == DispatchProfile::ReviewLite {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!(
            "  [{ts}] 📉 ReviewLite prompt: {} chars (channel {})",
            system_prompt_owned.len(),
            channel_id.get()
        );
    }

    system_prompt_owned
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: call build_system_prompt with minimal/default arguments (Full profile).
    fn call_build(
        discord_context: &str,
        current_path: &str,
        channel_id: u64,
        token: &str,
        disabled_notice: &str,
        skills_notice: &str,
    ) -> String {
        build_system_prompt(
            discord_context,
            current_path,
            ChannelId::new(channel_id),
            token,
            disabled_notice,
            skills_notice,
            true,  // narrate_progress
            None,  // role_binding
            false, // queued_turn
            DispatchProfile::Full,
            None, // dispatch_type
            None, // shared_knowledge
            None, // longterm_catalog
            None, // memory_settings
        )
    }

    #[test]
    fn test_build_system_prompt_includes_discord_context() {
        let output = call_build(
            "Channel: #general (guild: TestServer)",
            "/tmp/work",
            123456789,
            "fake-token",
            "",
            "",
        );
        assert!(
            output.contains("Channel: #general (guild: TestServer)"),
            "System prompt should contain the discord_context string"
        );
    }

    #[test]
    fn test_build_system_prompt_includes_cwd() {
        let output = call_build("ctx", "/home/user/projects", 1, "tok", "", "");
        assert!(
            output.contains("Current working directory: /home/user/projects"),
            "System prompt should contain the current working directory"
        );
    }

    #[test]
    fn test_build_system_prompt_includes_file_send_command() {
        let output = call_build("ctx", "/tmp", 1, "tok", "", "");
        assert!(
            output.contains("agentdesk discord-sendfile"),
            "System prompt should contain the agentdesk discord-sendfile command"
        );
    }

    #[test]
    fn test_build_system_prompt_disables_interactive_tools() {
        let output = call_build("ctx", "/tmp", 1, "tok", "", "");
        assert!(
            output.contains("CANNOT interact with any interactive prompts"),
            "System prompt should warn that interactive tools are disabled"
        );
        assert!(
            output.contains("Never use tools that expect user interaction"),
            "System prompt should instruct not to use interactive tools"
        );
    }

    #[test]
    fn test_build_system_prompt_includes_context_compression_guidance() {
        let output = call_build("ctx", "/tmp", 1, "tok", "", "");
        assert!(output.contains("[Context Compression]"));
        assert!(output.contains(CONTEXT_COMPRESSION_SECTION_ORDER));
        assert!(output.contains(STALE_TOOL_RESULT_PLACEHOLDER_EXAMPLE));
    }

    #[test]
    fn test_build_system_prompt_includes_narration_when_enabled() {
        let output = call_build("ctx", "/tmp", 1, "tok", "", "");
        assert!(output.contains("Always keep the user informed about what you are doing."));
        assert!(!output.contains("The user cannot see your tool calls"));
    }

    #[test]
    fn test_build_system_prompt_omits_narration_when_disabled() {
        let output = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            "",
            false,
            None,
            false,
            DispatchProfile::Full,
            None,
            None,
            None,
            None,
        );

        assert!(!output.contains("Always keep the user informed about what you are doing."));
        assert!(!output.contains("The user cannot see your tool calls"));
        assert!(output.contains("ALWAYS mention the specific file path"));
    }

    #[test]
    fn test_followup_turn_reminder_reinjects_compaction_rules() {
        let reminder = build_followup_turn_system_reminder();

        assert!(reminder.contains("<system-reminder>"));
        assert!(reminder.contains("Discord formatting: minimize code blocks"));
        assert!(reminder.contains("treat the compacted summary as authoritative"));
        assert!(reminder.contains(CONTEXT_COMPRESSION_SECTION_ORDER));
        assert!(reminder.contains(STALE_TOOL_RESULT_PLACEHOLDER_EXAMPLE));
    }

    #[test]
    fn test_dispatch_profile_from_dispatch_type() {
        assert_eq!(
            DispatchProfile::from_dispatch_type(None),
            DispatchProfile::Full
        );
        assert_eq!(
            DispatchProfile::from_dispatch_type(Some("implementation")),
            DispatchProfile::Full
        );
        assert_eq!(
            DispatchProfile::from_dispatch_type(Some("review")),
            DispatchProfile::ReviewLite
        );
        assert_eq!(
            DispatchProfile::from_dispatch_type(Some("review-decision")),
            DispatchProfile::ReviewLite
        );
        assert_eq!(
            DispatchProfile::from_dispatch_type(Some("e2e-test")),
            DispatchProfile::Full
        );
        assert_eq!(
            DispatchProfile::from_dispatch_type(Some("consultation")),
            DispatchProfile::Full
        );
        assert_eq!(
            DispatchProfile::from_dispatch_type(Some("rework")),
            DispatchProfile::Full
        );
    }

    #[test]
    fn test_review_lite_omits_skills() {
        let skills_notice = "\n\nAvailable skills:\n\
            The entries below are descriptions only, not the full skill body.\n\
            If a skill is relevant or explicitly requested, load that skill's `SKILL.md` before acting.\n\
            Read files under `references/` only when the `SKILL.md` points to them or you need extra detail.\n\
              - /commit: Commit changes";
        let with_skills = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            skills_notice,
            true,
            None,
            false,
            DispatchProfile::Full,
            None,
            None,
            None,
            None,
        );
        let without_skills = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            skills_notice,
            true,
            None,
            false,
            DispatchProfile::ReviewLite,
            Some("review"),
            None,
            None,
            None,
        );
        assert!(with_skills.contains("Available skills"));
        assert!(with_skills.contains("descriptions only"));
        assert!(with_skills.contains("`SKILL.md`"));
        assert!(!without_skills.contains("Available skills"));
        assert!(!without_skills.contains("[Context Compression]"));
        // ReviewLite prompt should be shorter
        assert!(without_skills.len() < with_skills.len());
    }

    #[test]
    fn test_review_lite_omits_context_compression_guidance() {
        let prompt = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            "",
            true,
            None,
            false,
            DispatchProfile::ReviewLite,
            Some("review"),
            None,
            None,
            None,
        );

        assert!(!prompt.contains("[Context Compression]"));
        assert!(!prompt.contains(CONTEXT_COMPRESSION_SECTION_ORDER));
        assert!(!prompt.contains(STALE_TOOL_RESULT_PLACEHOLDER_EXAMPLE));
    }

    #[test]
    fn test_review_decision_gets_decision_rules() {
        use super::super::settings::RoleBinding;
        let binding = RoleBinding {
            role_id: "test-agent".to_string(),
            prompt_file: "/nonexistent".to_string(),
            provider: None,
            model: None,
            reasoning_effort: None,
            peer_agents_enabled: true,
            memory: Default::default(),
        };
        let review_prompt = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            "",
            true,
            Some(&binding),
            false,
            DispatchProfile::ReviewLite,
            Some("review"),
            None,
            None,
            None,
        );
        let decision_prompt = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            "",
            true,
            Some(&binding),
            false,
            DispatchProfile::ReviewLite,
            Some("review-decision"),
            None,
            None,
            None,
        );
        // review should NOT contain decision API
        assert!(!review_prompt.contains("/api/review-decision"));
        assert!(review_prompt.contains("[Review Rules]"));
        // review-decision should contain decision API and options
        assert!(decision_prompt.contains("/api/review-decision"));
        assert!(decision_prompt.contains("accept/dispute/dismiss"));
        assert!(decision_prompt.contains("[Review Decision Rules]"));
    }

    #[test]
    fn test_full_prompt_omits_peer_agent_directory_when_disabled() {
        use super::super::settings::RoleBinding;

        let binding = RoleBinding {
            role_id: "spark".to_string(),
            prompt_file: "/nonexistent".to_string(),
            provider: None,
            model: None,
            reasoning_effort: None,
            peer_agents_enabled: false,
            memory: Default::default(),
        };

        let prompt = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1488022491992424448),
            "tok",
            "",
            "",
            true,
            Some(&binding),
            false,
            DispatchProfile::Full,
            None,
            None,
            None,
            None,
        );

        assert!(!prompt.contains("[Peer Agent Directory]"));
    }

    #[test]
    fn test_full_prompt_renders_supplied_longterm_catalog() {
        use super::super::settings::RoleBinding;

        let binding = RoleBinding {
            role_id: "spark".to_string(),
            prompt_file: "/nonexistent".to_string(),
            provider: None,
            model: None,
            reasoning_effort: None,
            peer_agents_enabled: false,
            memory: Default::default(),
        };

        let prompt = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            "",
            true,
            Some(&binding),
            false,
            DispatchProfile::Full,
            None,
            None,
            Some("- facts.md: deployment notes"),
            None,
        );

        assert!(prompt.contains("[Long-term Memory]"));
        assert!(prompt.contains("facts.md"));
    }

    #[test]
    fn test_full_prompt_injects_mem0_memory_guidance() {
        let prompt = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            "",
            true,
            None,
            false,
            DispatchProfile::Full,
            None,
            None,
            None,
            Some(&ResolvedMemorySettings {
                backend: MemoryBackendKind::Mem0,
                ..ResolvedMemorySettings::default()
            }),
        );

        assert!(prompt.contains("[Proactive Memory Guidance]"));
        assert!(prompt.contains("`search_memory` MCP tool"));
        assert!(prompt.contains("`add_memories` MCP tool"));
    }

    #[test]
    fn test_full_prompt_injects_memento_memory_guidance() {
        let prompt = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            "",
            true,
            None,
            false,
            DispatchProfile::Full,
            None,
            None,
            None,
            Some(&ResolvedMemorySettings {
                backend: MemoryBackendKind::Memento,
                ..ResolvedMemorySettings::default()
            }),
        );

        assert!(prompt.contains("[Proactive Memory Guidance]"));
        assert!(prompt.contains("`recall` MCP tool"));
        assert!(prompt.contains("`remember` MCP tool"));
        assert!(prompt.contains("`context`"));
        assert!(prompt.contains("`reflect`"));
    }

    #[test]
    fn test_review_lite_omits_memory_guidance() {
        let prompt = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            "",
            true,
            None,
            false,
            DispatchProfile::ReviewLite,
            Some("review"),
            None,
            None,
            Some(&ResolvedMemorySettings {
                backend: MemoryBackendKind::Mem0,
                ..ResolvedMemorySettings::default()
            }),
        );

        assert!(!prompt.contains("[Proactive Memory Guidance]"));
        assert!(!prompt.contains("`search_memory`"));
        assert!(!prompt.contains("`add_memories`"));
    }
}
