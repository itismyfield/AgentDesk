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
    if is_environment_context_prompt(prompt) {
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

pub(super) fn is_environment_context_prompt(prompt: &str) -> bool {
    let prompt = prompt.trim();
    prompt.starts_with("<environment_context>") && prompt.ends_with("</environment_context>")
}

/// Codex's AGENTS.md text: its header line, a blank line, one `<INSTRUCTIONS>` block, then at most
/// its environment block. Anything else around it is typed input and stays a prompt.
pub(super) fn is_codex_agents_md_prompt(prompt: &str) -> bool {
    let Some((header, rest)) = prompt.trim().split_once('\n') else {
        return false;
    };
    let Some(scope) = header.strip_prefix("# AGENTS.md instructions") else {
        return false;
    };
    let scoped = scope.strip_prefix(" for ");
    if !scope.is_empty() && !scoped.is_some_and(|dir| !dir.trim().is_empty()) {
        return false;
    }
    let Some((_, tail)) = rest
        .strip_prefix("\n<INSTRUCTIONS>")
        .and_then(|body| body.split_once("</INSTRUCTIONS>"))
    else {
        return false;
    };
    let tail = tail.trim();
    tail.is_empty()
        || is_environment_context_prompt(tail) && tail.matches("<environment_context>").count() == 1
}
