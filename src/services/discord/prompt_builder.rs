use super::settings::{
    discord_token_hash, load_longterm_memory_catalog, load_review_tuning_guidance,
    load_role_prompt, load_shared_prompt, render_peer_agent_guidance,
};
use super::*;

/// Dispatch prompt profile — controls which system prompt sections are injected.
/// `Full` includes everything (used for implementation dispatches and normal turns).
/// `ReviewLite` strips peer agents, long-term memory, and skills to reduce token cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DispatchProfile {
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
    role_binding: Option<&RoleBinding>,
    queued_turn: bool,
    profile: DispatchProfile,
    dispatch_type: Option<&str>,
) -> String {
    let mut system_prompt_owned = format!(
        "You are chatting with a user through Discord.\n\
         {}\n\
         Current working directory: {}\n\n\
         When your work produces a file the user would want (generated code, reports, images, archives, etc.),\n\
         send it by running this bash command:\n\n\
         agentdesk discord-sendfile <filepath> --channel {} --key {}\n\n\
         This delivers the file directly to the user's Discord channel.\n\
         Do NOT tell the user to use /down — use the command above instead.\n\n\
         Always keep the user informed about what you are doing. Briefly explain each step as you work \
         (e.g. \"Reading the file...\", \"Creating the script...\", \"Running tests...\"). \
         The user cannot see your tool calls, so narrate your progress so they know what is happening.\n\
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
        disabled_notice,
        // ReviewLite: omit skills to save tokens — reviewer only submits verdict
        if profile == DispatchProfile::ReviewLite {
            ""
        } else {
            skills_notice
        }
    );

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

        // ReviewLite: skip long-term memory and peer agents to save tokens
        if profile == DispatchProfile::Full {
            if let Some(catalog) = load_longterm_memory_catalog(&binding.role_id) {
                system_prompt_owned.push_str(
                    "\n\n[Long-term Memory]\n\
                     Available memory files for this agent. Use the Read tool to load full content when needed:\n",
                );
                system_prompt_owned.push_str(&catalog);
            }

            if let Some(peer_guidance) = render_peer_agent_guidance(&binding.role_id) {
                system_prompt_owned.push_str("\n\n");
                system_prompt_owned.push_str(&peer_guidance);
            }
        }
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
            None,  // role_binding
            false, // queued_turn
            DispatchProfile::Full,
            None, // dispatch_type
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
            DispatchProfile::from_dispatch_type(Some("rework")),
            DispatchProfile::Full
        );
    }

    #[test]
    fn test_review_lite_omits_skills() {
        let with_skills = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            "\n\nAvailable skills:\n  - /commit: Commit changes",
            None,
            false,
            DispatchProfile::Full,
            None,
        );
        let without_skills = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            "\n\nAvailable skills:\n  - /commit: Commit changes",
            None,
            false,
            DispatchProfile::ReviewLite,
            Some("review"),
        );
        assert!(with_skills.contains("Available skills"));
        assert!(!without_skills.contains("Available skills"));
        // ReviewLite prompt should be shorter
        assert!(without_skills.len() < with_skills.len());
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
        };
        let review_prompt = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            "",
            Some(&binding),
            false,
            DispatchProfile::ReviewLite,
            Some("review"),
        );
        let decision_prompt = build_system_prompt(
            "ctx",
            "/tmp",
            ChannelId::new(1),
            "tok",
            "",
            "",
            Some(&binding),
            false,
            DispatchProfile::ReviewLite,
            Some("review-decision"),
        );
        // review should NOT contain decision API
        assert!(!review_prompt.contains("/api/review-decision"));
        assert!(review_prompt.contains("[Review Rules]"));
        // review-decision should contain decision API and options
        assert!(decision_prompt.contains("/api/review-decision"));
        assert!(decision_prompt.contains("accept/dispute/dismiss"));
        assert!(decision_prompt.contains("[Review Decision Rules]"));
    }
}
