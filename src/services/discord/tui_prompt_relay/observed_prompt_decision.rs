//! Pure observed-prompt lifecycle decisions and exact local-only cleanup.
//!
//! Generic prompt observation publishes relay side effects before this relay
//! loop classifies command text. Keep the classification and its generation-
//! exact cleanup together so no text/time correlation is needed afterward.

use super::injected_prompt_policy::{
    InjectedPromptClass, classify_injected_prompt, is_slash_command_control_prompt,
    slash_command_control_kind, slash_command_control_prompt_is_caveat_only,
    slash_command_control_prompt_is_local_command_stdout,
};
use crate::services::tui_prompt_dedupe::{ObservedTuiPrompt, clear_observed_tui_prompt_effects};

#[derive(Debug, PartialEq, Eq)]
pub(super) struct RelayObservedPromptInjectionDecision {
    pub(super) injected_class: InjectedPromptClass,
    pub(super) slash_command_kind: Option<String>,
    pub(super) local_only_slash: bool,
}

impl RelayObservedPromptInjectionDecision {
    pub(super) fn starts_external_turn_lifecycle(&self) -> bool {
        !self.local_only_slash
            && !self.injected_class.suppresses_user_turn_lifecycle()
            && !self.injected_class.is_subagent_notification_event()
    }
}

pub(in crate::services::discord) fn observed_prompt_starts_external_turn_lifecycle(
    prompt: &str,
) -> bool {
    relay_observed_prompt_injected_prompt_decision(prompt).starts_external_turn_lifecycle()
}

/// Pure classification used before relay lease/ownership side effects.
pub(super) fn relay_observed_prompt_injected_prompt_decision(
    prompt: &str,
) -> RelayObservedPromptInjectionDecision {
    let injected_class = classify_injected_prompt(prompt);
    let slash_command_kind = matches!(injected_class, InjectedPromptClass::SlashCommandControl)
        .then(|| slash_command_control_kind(prompt));
    let local_only_slash = matches!(injected_class, InjectedPromptClass::SlashCommandControl)
        && is_local_only_slash_command_prompt(prompt);

    RelayObservedPromptInjectionDecision {
        injected_class,
        slash_command_kind,
        local_only_slash,
    }
}

/// Local-completing slash-control prompts skip synthetic turn ownership.
pub(super) fn is_local_only_slash_command_prompt(prompt: &str) -> bool {
    if !is_slash_command_control_prompt(prompt) {
        return false;
    }
    let kind = slash_command_control_kind(prompt);
    super::super::commands::is_local_only_slash_command_kind(&kind)
        || slash_command_control_prompt_is_caveat_only(prompt)
        || slash_command_control_prompt_is_local_command_stdout(prompt)
}

/// Generic observation has already created a lease and SSH marker when this
/// classifier runs. Clear only the exact generations carried by this event;
/// a later human `/compact` has different generations and remains untouched.
pub(super) fn clear_local_only_observation_effects(
    prompt: &ObservedTuiPrompt,
    decision: &RelayObservedPromptInjectionDecision,
) {
    if decision.local_only_slash {
        clear_observed_tui_prompt_effects(prompt);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_only_compact_clears_only_its_exact_observation_effects() {
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        crate::services::tui_prompt_dedupe::reset_state_for_tests();

        let tmux = "tmux-4591-local-only-effects";
        let mut observed = crate::services::tui_prompt_dedupe::subscribe_observed_prompts();
        assert_eq!(
            crate::services::tui_prompt_dedupe::observe_prompt_by_tmux("claude", tmux, "/compact"),
            crate::services::tui_prompt_dedupe::PromptObservation::PublishedSshDirect,
        );
        let prompt = observed
            .try_recv()
            .expect("published local-only observation");
        assert!(
            crate::services::tui_prompt_dedupe::external_input_relay_lease("claude", tmux, 1)
                .is_some()
        );
        assert!(
            crate::services::tui_prompt_dedupe::is_ssh_direct_observation_pending("claude", tmux)
        );

        let decision = relay_observed_prompt_injected_prompt_decision(&prompt.prompt);
        assert!(decision.local_only_slash);
        clear_local_only_observation_effects(&prompt, &decision);

        assert!(
            crate::services::tui_prompt_dedupe::external_input_relay_lease("claude", tmux, 1)
                .is_none()
        );
        assert!(
            !crate::services::tui_prompt_dedupe::is_ssh_direct_observation_pending("claude", tmux)
        );
    }
}
