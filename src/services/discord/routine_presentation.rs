//! Wire-only policy pinned at a routine start; terminal evidence never replaces the raw body.
use crate::services::provider::ProviderKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct RoutinePresentation {
    policy: Policy,
    terminal: TerminalEvidence,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Policy {
    #[default]
    Legacy,
    ClaudeProcessC1,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TerminalEvidence {
    #[default]
    Missing,
    NoReply,
    Full,
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TerminalPresentation {
    Full,
    NoVisibleResult,
}

impl RoutinePresentation {
    pub(super) fn is_legacy_default(&self) -> bool {
        self.policy == Policy::Legacy && self.terminal == TerminalEvidence::Missing
    }

    pub(super) fn at_start(
        provider: &ProviderKind,
        source: Option<&str>,
        routine_process: bool,
    ) -> Self {
        Self {
            policy: if *provider == ProviderKind::Claude
                && source == Some("routine")
                && routine_process
            {
                Policy::ClaudeProcessC1
            } else {
                Policy::Legacy
            },
            terminal: TerminalEvidence::Missing,
        }
    }

    pub(super) fn holds_body(&self) -> bool {
        self.policy == Policy::ClaudeProcessC1
    }

    pub(super) fn observe_terminal(&mut self, result: &str) {
        if !self.holds_body() {
            return;
        }
        self.terminal = match self.terminal {
            TerminalEvidence::Missing if result.trim().is_empty() => TerminalEvidence::Ambiguous,
            TerminalEvidence::Missing if result.trim().eq_ignore_ascii_case("NO_REPLY") => {
                TerminalEvidence::NoReply
            }
            TerminalEvidence::Missing => TerminalEvidence::Full,
            _ => TerminalEvidence::Ambiguous,
        };
    }

    pub(super) fn terminal_presentation(&self) -> TerminalPresentation {
        if self.holds_body() {
            match self.terminal {
                TerminalEvidence::NoReply => return TerminalPresentation::NoVisibleResult,
                TerminalEvidence::Missing | TerminalEvidence::Ambiguous => {
                    tracing::warn!(classification = ?self.terminal, "routine terminal classification preserves full body");
                }
                TerminalEvidence::Full => {}
            }
        }
        TerminalPresentation::Full
    }
}

#[cfg(test)]
mod tests;
