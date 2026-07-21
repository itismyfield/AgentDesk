use std::time::Duration;

use crate::services::claude_tui;
use crate::services::codex_tui;
use crate::services::provider::ProviderKind;
use crate::services::provider_hosting::{ProviderSessionDriver, ProviderSessionSelection};

const TUI_STEERING_ENV: &str = "AGENTDESK_TUI_STEERING";
const RETRY_BACKOFF: [Duration; 3] = [
    Duration::from_millis(0),
    Duration::from_millis(80),
    Duration::from_millis(160),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SteeringRoute {
    ExistingMailbox,
    NativeTui,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SteeringSnapshot {
    Claude(claude_tui::input::PromptReadinessSnapshot),
    Codex(codex_tui::input::PromptReadinessSnapshot),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SteeringOutcome {
    ExistingMailbox,
    Injected,
    Unsafe(&'static str),
    Failed(String),
}

pub(crate) fn tui_steering_enabled_from(value: Option<&str>) -> bool {
    value == Some("1")
}

pub(crate) fn tui_steering_enabled() -> bool {
    tui_steering_enabled_from(std::env::var(TUI_STEERING_ENV).ok().as_deref())
}

pub(crate) fn route_input_by_session_driver(selection: &ProviderSessionSelection) -> SteeringRoute {
    match selection.driver {
        ProviderSessionDriver::TuiHosting => SteeringRoute::NativeTui,
        ProviderSessionDriver::LegacyPrompt | ProviderSessionDriver::ClaudeE => {
            SteeringRoute::ExistingMailbox
        }
    }
}

pub(crate) fn classify_steering_snapshot(snapshot: &SteeringSnapshot) -> Result<(), &'static str> {
    match snapshot {
        SteeringSnapshot::Claude(snapshot) => {
            claude_tui::input::steering_snapshot_decision(snapshot)
        }
        SteeringSnapshot::Codex(snapshot) => codex_tui::input::steering_snapshot_decision(snapshot),
    }
}

fn capture_snapshot(provider: &ProviderKind, session_name: &str) -> Option<SteeringSnapshot> {
    match provider {
        ProviderKind::Claude => Some(SteeringSnapshot::Claude(
            claude_tui::input::prompt_readiness_snapshot(session_name),
        )),
        ProviderKind::Codex => Some(SteeringSnapshot::Codex(
            codex_tui::input::prompt_readiness_snapshot(session_name),
        )),
        _ => None,
    }
}

fn inject_once(provider: &ProviderKind, session_name: &str, prompt: &str) -> Result<(), String> {
    match provider {
        ProviderKind::Claude => claude_tui::input::inject_steering_prompt(session_name, prompt),
        ProviderKind::Codex => codex_tui::input::inject_steering_prompt(session_name, prompt),
        _ => Err(format!(
            "native TUI steering unsupported for provider {}",
            provider.as_str()
        )),
    }
}

fn inject_with_bounded_retry_using<C, I>(
    selection: &ProviderSessionSelection,
    mut capture: C,
    mut inject: I,
) -> SteeringOutcome
where
    C: FnMut() -> Option<SteeringSnapshot>,
    I: FnMut() -> Result<(), String>,
{
    if route_input_by_session_driver(selection) == SteeringRoute::ExistingMailbox {
        return SteeringOutcome::ExistingMailbox;
    }

    let mut last_unsafe = "snapshot unavailable";
    for backoff in RETRY_BACKOFF {
        if !backoff.is_zero() {
            std::thread::sleep(backoff);
        }
        let Some(snapshot) = capture() else {
            return SteeringOutcome::ExistingMailbox;
        };
        match classify_steering_snapshot(&snapshot) {
            Ok(()) => {
                return match inject() {
                    Ok(()) => SteeringOutcome::Injected,
                    Err(error) => SteeringOutcome::Failed(error),
                };
            }
            Err(reason) => last_unsafe = reason,
        }
    }
    SteeringOutcome::Unsafe(last_unsafe)
}

pub(crate) fn inject_with_bounded_retry(
    provider: &ProviderKind,
    selection: &ProviderSessionSelection,
    session_name: &str,
    prompt: &str,
) -> SteeringOutcome {
    inject_with_bounded_retry_using(
        selection,
        || capture_snapshot(provider, session_name),
        || inject_once(provider, session_name, prompt),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn selection(driver: ProviderSessionDriver) -> ProviderSessionSelection {
        ProviderSessionSelection {
            provider_id: "claude".to_string(),
            requested_tui_hosting: driver == ProviderSessionDriver::TuiHosting,
            driver,
            fallback_reason: None,
        }
    }

    #[test]
    fn flag_default_off_keeps_existing_mailbox_path() {
        assert!(!tui_steering_enabled_from(None));
        assert!(!tui_steering_enabled_from(Some("0")));
        assert!(!tui_steering_enabled_from(Some("true")));
        assert!(tui_steering_enabled_from(Some("1")));
    }

    #[test]
    fn legacy_prompt_never_enters_native_tui_route_or_probes_pane() {
        let mut probes = 0;
        let mut injections = 0;
        let outcome = inject_with_bounded_retry_using(
            &selection(ProviderSessionDriver::LegacyPrompt),
            || {
                probes += 1;
                None
            },
            || {
                injections += 1;
                Ok(())
            },
        );

        assert_eq!(outcome, SteeringOutcome::ExistingMailbox);
        assert_eq!(probes, 0, "pipe mode must not inspect a TUI pane");
        assert_eq!(injections, 0, "pipe mode must preserve mailbox delivery");
        assert_eq!(
            route_input_by_session_driver(&selection(ProviderSessionDriver::TuiHosting)),
            SteeringRoute::NativeTui
        );
    }

    #[test]
    fn unsafe_snapshots_fail_closed_before_injection() {
        let cases = [
            SteeringSnapshot::Claude(claude_tui::input::PromptReadinessSnapshot {
                prompt_marker_detected: false,
                prompt_draft_detected: false,
                tmux_pane_alive: false,
                capture_available: true,
                pane_tail: String::new(),
            }),
            SteeringSnapshot::Claude(claude_tui::input::PromptReadinessSnapshot {
                prompt_marker_detected: false,
                prompt_draft_detected: false,
                tmux_pane_alive: true,
                capture_available: true,
                pane_tail: "⚠ 1 MCP server needs authentication · run /mcp\n❯".to_string(),
            }),
            SteeringSnapshot::Claude(claude_tui::input::PromptReadinessSnapshot {
                prompt_marker_detected: true,
                prompt_draft_detected: false,
                tmux_pane_alive: true,
                capture_available: true,
                pane_tail: "busy without composer".to_string(),
            }),
            SteeringSnapshot::Codex(codex_tui::input::PromptReadinessSnapshot {
                composer_marker_detected: false,
                prompt_draft_detected: false,
                tmux_pane_alive: true,
                capture_available: true,
                pane_tail: "ordinary busy output".to_string(),
            }),
            SteeringSnapshot::Codex(codex_tui::input::PromptReadinessSnapshot {
                composer_marker_detected: true,
                prompt_draft_detected: false,
                tmux_pane_alive: true,
                capture_available: true,
                pane_tail: "Approval required: allow command?".to_string(),
            }),
            SteeringSnapshot::Codex(codex_tui::input::PromptReadinessSnapshot {
                composer_marker_detected: true,
                prompt_draft_detected: true,
                tmux_pane_alive: true,
                capture_available: true,
                pane_tail: "draft".to_string(),
            }),
        ];
        for snapshot in cases {
            assert!(classify_steering_snapshot(&snapshot).is_err());
        }

        let mut attempts = 0;
        let unsafe_snapshot = SteeringSnapshot::Codex(codex_tui::input::PromptReadinessSnapshot {
            composer_marker_detected: false,
            prompt_draft_detected: false,
            tmux_pane_alive: true,
            capture_available: true,
            pane_tail: "ordinary busy output".to_string(),
        });
        let outcome = inject_with_bounded_retry_using(
            &selection(ProviderSessionDriver::TuiHosting),
            || Some(unsafe_snapshot.clone()),
            || {
                attempts += 1;
                Ok(())
            },
        );
        assert!(matches!(outcome, SteeringOutcome::Unsafe(_)));
        assert_eq!(attempts, 0, "unsafe modal state must block injection");
    }
}
