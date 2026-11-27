//! Input notice text the writer renders for a human prompt it observed.

use super::injected_prompt_policy::{
    self, InjectedPromptClass, format_ssh_direct_prompt_notification,
};
use super::input_notice_memo;
use super::observed_prompt_decision::relay_observed_prompt_injected_prompt_decision;

/// Shares the live human-only policy and renderer with transcript projections.
pub fn input_notice_content(prompt: &str) -> Option<String> {
    input_notice_memo::content(prompt, |prompt| {
        let decision = relay_observed_prompt_injected_prompt_decision(prompt);
        (decision.injected_class.is_human_active_turn() && !decision.local_only_slash)
            .then(|| input_notice_line(prompt))
    })
}
#[cfg(any(test, feature = "test-support"))]
pub use input_notice_memo::RUNS as INPUT_NOTICE_RUNS;

/// The same notice for a scheduled-task fire the writer read off its own row.
pub(crate) fn scheduled_input_notice_content(prompt: &str) -> Option<String> {
    input_notice_content(prompt).map(|_| {
        injected_prompt_policy::format_direct_prompt_notification(
            InjectedPromptClass::ScheduledPrompt,
            "claude",
            "",
            prompt,
        )
    })
}

pub fn input_notice_line(text: &str) -> String {
    format_ssh_direct_prompt_notification("claude", "", text)
}
