//! Authorization combines static source policy with the live agent registry.

use crate::domain::outbound::outbox::source::is_allowed_static_source;
pub(crate) fn is_static_notice_kind(source: &str) -> bool {
    crate::domain::outbound::outbox::source::is_static_notice_kind(source)
}

pub(crate) const IMESSAGE_INPUT_NOTICE_SOURCE: &str = "imessage_input_notice";

pub(crate) const RETIRED_SEND_SOURCES: &[&str] = &["merge-automation"];

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SendCallerClass {
    LoopbackInternal,
    #[allow(dead_code)]
    Cli,
    #[allow(dead_code)]
    Dashboard,
    #[allow(dead_code)]
    Unknown,
}

impl Default for SendCallerClass {
    fn default() -> Self {
        Self::LoopbackInternal
    }
}

impl SendCallerClass {
    #[allow(dead_code)]
    pub fn from_header(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "loopback" | "dcserver" | "internal" => Some(Self::LoopbackInternal),
            "cli" | "agentdesk-cli" => Some(Self::Cli),
            "dashboard" | "browser" => Some(Self::Dashboard),
            _ => None,
        }
    }

    const fn mask(self) -> u8 {
        match self {
            Self::LoopbackInternal => 1,
            Self::Cli => 2,
            Self::Dashboard => 4,
            Self::Unknown => 8,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("source `{label}` is not allowed for {caller:?}")]
pub struct SendSourcePolicyError {
    pub label: String,
    pub caller: SendCallerClass,
}

/// Capture static notice identity at production time, before agent configuration changes.
pub(crate) fn is_registered_nonbody_source(source: &str) -> bool {
    crate::config::snapshot()
        .is_some_and(|config| !config.agents.iter().any(|agent| agent.id == source))
        && is_static_notice_kind(source)
}

/// Exact, case-sensitive and fail-closed source authorization.
pub fn validate_send_source_for(
    source: &str,
    caller: SendCallerClass,
) -> Result<(), SendSourcePolicyError> {
    let exact = !source.is_empty() && source.trim() == source;
    let static_allowed = exact && is_allowed_static_source(source, caller.mask());
    let known_agent = exact && crate::services::discord::settings::is_known_agent(source);
    if static_allowed || known_agent {
        Ok(())
    } else {
        Err(SendSourcePolicyError {
            label: source.to_string(),
            caller,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{SendCallerClass, validate_send_source_for};
    use crate::domain::outbound::outbox::source::static_source_count;

    const CURRENT_LOOPBACK_LABELS: &[&str] = &[
        "kanban-rules",
        "triage-rules",
        "review-automation",
        "auto-queue",
        "pipeline",
        "system",
        "timeouts",
        "lifecycle_notifier",
        "routine-runtime",
        "headless_turn",
        "slo_alerter",
        "auto-queue-monitor",
        "inventory",
    ];
    const NEW_LOOPBACK_LABELS: &[&str] = &[
        "scheduled_message",
        "github_sync",
        "catch_up_too_old",
        "boot_custody_notice",
        "queue_overflow_notice",
        "retained_input_notice",
        "imessage_input_notice",
        "outbox_delivery_alert",
        "long_turn_watchdog",
        "relay_signal_rollup",
        "dispatch_watchdog",
        "stall_watchdog",
        "tui_o_alarm",
        "adopt_fence_forward_notice",
        "routing_transition",
    ];

    #[test]
    fn imessage_input_notice_is_authorized_and_classified_as_nonbody() {
        assert!(super::is_static_notice_kind(
            super::IMESSAGE_INPUT_NOTICE_SOURCE
        ));
        assert!(
            validate_send_source_for(
                super::IMESSAGE_INPUT_NOTICE_SOURCE,
                SendCallerClass::LoopbackInternal
            )
            .is_ok()
        );
    }

    #[test]
    fn producer_contract_lists_every_loopback_source() {
        assert!(
            validate_send_source_for("merge-automation", SendCallerClass::LoopbackInternal)
                .is_err()
        );
        let mut expected = CURRENT_LOOPBACK_LABELS.to_vec();
        expected.extend_from_slice(NEW_LOOPBACK_LABELS);
        for label in expected {
            assert!(
                validate_send_source_for(label, SendCallerClass::LoopbackInternal).is_ok(),
                "registered producer `{label}` must enqueue and send as LoopbackInternal"
            );
        }
        assert_eq!(
            static_source_count(SendCallerClass::LoopbackInternal.mask()),
            CURRENT_LOOPBACK_LABELS.len() + NEW_LOOPBACK_LABELS.len()
        );
    }

    #[test]
    fn new_sources_are_loopback_only() {
        for label in NEW_LOOPBACK_LABELS {
            assert!(validate_send_source_for(label, SendCallerClass::LoopbackInternal).is_ok());
            for caller in [
                SendCallerClass::Cli,
                SendCallerClass::Dashboard,
                SendCallerClass::Unknown,
            ] {
                assert!(
                    validate_send_source_for(label, caller).is_err(),
                    "new source `{label}` must be denied for {caller:?}"
                );
            }
        }
    }

    #[test]
    fn original_external_contract_and_fail_closed_variants_are_pinned() {
        for label in ["agentdesk-cli", "operator"] {
            assert!(validate_send_source_for(label, SendCallerClass::Cli).is_ok());
        }
        assert!(validate_send_source_for("dashboard", SendCallerClass::Dashboard).is_ok());
        for source in ["", "unknown", "SYSTEM", " system", "system ", "system\n"] {
            for caller in [
                SendCallerClass::LoopbackInternal,
                SendCallerClass::Cli,
                SendCallerClass::Dashboard,
                SendCallerClass::Unknown,
            ] {
                assert!(validate_send_source_for(source, caller).is_err());
            }
        }
    }
}
