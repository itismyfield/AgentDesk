pub(super) fn reject_synthetic_tui_user_prompt(prompt: String) -> Option<String> {
    (!is_synthetic_tui_user_prompt(&prompt)).then_some(prompt)
}

pub(super) fn reject_synthetic_claude_user_prompt(prompt: String) -> Option<String> {
    (!is_synthetic_tui_user_prompt_for_provider("claude", &prompt)).then_some(prompt)
}

const CLAUDE_INTERRUPT_USER_PROMPT_MARKERS: [&str; 2] = [
    "[Request interrupted by user]",
    "[Request interrupted by user for tool use]",
];

fn is_synthetic_tui_user_prompt(prompt: &str) -> bool {
    let prompt = prompt.trim();
    if prompt.starts_with("<environment_context>") && prompt.ends_with("</environment_context>") {
        return true;
    }
    prompt.starts_with("[Shared Agent Knowledge]\n")
        || prompt.starts_with("[Proactive Memory Guidance]\n")
        || prompt == "No response requested."
}

pub(super) fn is_synthetic_tui_user_prompt_for_provider(provider: &str, prompt: &str) -> bool {
    if is_synthetic_tui_user_prompt(prompt) {
        return true;
    }
    provider.trim().eq_ignore_ascii_case("claude") && is_claude_interrupt_marker(prompt)
}

fn is_claude_interrupt_marker(prompt: &str) -> bool {
    let prompt = prompt.trim();
    CLAUDE_INTERRUPT_USER_PROMPT_MARKERS.contains(&prompt)
}
