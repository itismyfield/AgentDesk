//! Pure observed-prompt lifecycle decisions.
//!
//! Local-only command classification belongs to the service-level observation
//! policy because it must run before the dedupe layer records any relay state.

use super::injected_prompt_policy::{InjectedPromptClass, classify_injected_prompt};
use crate::services::tui_prompt_control::{
    LocalOnlySlashControl, classify_local_only_slash_control,
};

#[derive(Debug, PartialEq, Eq)]
pub(super) struct RelayObservedPromptInjectionDecision {
    pub(super) injected_class: InjectedPromptClass,
    pub(super) slash_command_kind: Option<String>,
    pub(super) local_only_slash: bool,
    pub(super) local_only_control: Option<LocalOnlySlashControl>,
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
    let local_only_control = classify_local_only_slash_control(prompt);
    let slash_command_kind = matches!(injected_class, InjectedPromptClass::SlashCommandControl)
        .then(|| {
            local_only_control
                .as_ref()
                .map(|control| control.kind.clone())
                .unwrap_or_else(|| {
                    super::injected_prompt_policy::slash_command_control_kind(prompt)
                })
        });
    let local_only_slash = local_only_control.is_some();

    RelayObservedPromptInjectionDecision {
        injected_class,
        slash_command_kind,
        local_only_slash,
        local_only_control,
    }
}

/// Local-completing slash-control prompts skip synthetic turn ownership.
pub(super) fn is_local_only_slash_command_prompt(prompt: &str) -> bool {
    classify_local_only_slash_control(prompt).is_some()
}
