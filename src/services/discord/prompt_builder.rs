use super::settings::{
    discord_token_hash, load_longterm_memory_catalog, load_role_prompt, load_shared_prompt,
    render_peer_agent_guidance,
};
use super::*;

pub(super) fn build_system_prompt(
    discord_context: &str,
    current_path: &str,
    channel_id: ChannelId,
    token: &str,
    disabled_notice: &str,
    skills_notice: &str,
    role_binding: Option<&RoleBinding>,
    queued_turn: bool,
) -> String {
    let mut system_prompt_owned = format!(
        "You are chatting with a user through Discord.\n\
         {}\n\
         Current working directory: {}\n\n\
         When your work produces a file the user would want (generated code, reports, images, archives, etc.),\n\
         send it by running this bash command:\n\n\
         remotecc --discord-sendfile <filepath> --channel {} --key {}\n\n\
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
        skills_notice
    );

    if let Some(binding) = role_binding {
        // Inject shared agent prompt (AGENTS.md) before role-specific identity
        if let Some(shared_prompt) = load_shared_prompt() {
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

    if queued_turn {
        system_prompt_owned.push_str(
            "\n\n[Queued Turn Rules]\n\
             This user message was queued while another turn was running.\n\
             Treat ONLY the latest queued user message in this turn as actionable.\n\
             Do NOT repeat, combine, or continue prior queued messages unless the latest user message explicitly asks for that.\n\
             If the latest user message asks for an exact literal output, return exactly that literal output and nothing else.",
        );
    }

    system_prompt_owned
}
