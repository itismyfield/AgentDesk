//! Single authorization registry for Discord outbound source labels.

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

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum StaticSendSource {
    KanbanRules,
    TriageRules,
    ReviewAutomation,
    AutoQueue,
    Pipeline,
    System,
    Timeouts,
    MergeAutomation,
    LifecycleNotifier,
    RoutineRuntime,
    ScheduledMessage,
    HeadlessTurn,
    SloAlerter,
    QualityRegressionAlerter,
    AutoQueueMonitor,
    Inventory,
    Voice,
    GithubSync,
    CatchUpTooOld,
    QueueOverflowNotice,
    OutboxDeliveryAlert,
    LongTurnWatchdog,
    RelaySignalRollup,
    DispatchWatchdog,
    StallWatchdog,
    AgentdeskCli,
    Operator,
    Dashboard,
}

#[derive(Copy, Clone, Debug)]
struct SourcePolicy {
    #[allow(dead_code)]
    source: StaticSendSource,
    label: &'static str,
    caller_mask: u8,
}

const LOOPBACK: u8 = SendCallerClass::LoopbackInternal.mask();
const CLI: u8 = SendCallerClass::Cli.mask();
const DASHBOARD: u8 = SendCallerClass::Dashboard.mask();

macro_rules! policy {
    ($source:ident, $label:literal, $mask:expr) => {
        SourcePolicy {
            source: StaticSendSource::$source,
            label: $label,
            caller_mask: $mask,
        }
    };
}

const POLICIES: &[SourcePolicy] = &[
    policy!(KanbanRules, "kanban-rules", LOOPBACK),
    policy!(TriageRules, "triage-rules", LOOPBACK),
    policy!(ReviewAutomation, "review-automation", LOOPBACK),
    policy!(AutoQueue, "auto-queue", LOOPBACK),
    policy!(Pipeline, "pipeline", LOOPBACK),
    policy!(System, "system", LOOPBACK),
    policy!(Timeouts, "timeouts", LOOPBACK),
    policy!(MergeAutomation, "merge-automation", LOOPBACK),
    policy!(LifecycleNotifier, "lifecycle_notifier", LOOPBACK),
    policy!(RoutineRuntime, "routine-runtime", LOOPBACK),
    policy!(ScheduledMessage, "scheduled_message", LOOPBACK),
    policy!(HeadlessTurn, "headless_turn", LOOPBACK),
    policy!(SloAlerter, "slo_alerter", LOOPBACK),
    policy!(
        QualityRegressionAlerter,
        "quality_regression_alerter",
        LOOPBACK
    ),
    policy!(AutoQueueMonitor, "auto-queue-monitor", LOOPBACK),
    policy!(Inventory, "inventory", LOOPBACK),
    policy!(Voice, "voice", LOOPBACK),
    policy!(GithubSync, "github_sync", LOOPBACK),
    policy!(CatchUpTooOld, "catch_up_too_old", LOOPBACK),
    policy!(QueueOverflowNotice, "queue_overflow_notice", LOOPBACK),
    policy!(OutboxDeliveryAlert, "outbox_delivery_alert", LOOPBACK),
    policy!(LongTurnWatchdog, "long_turn_watchdog", LOOPBACK),
    policy!(RelaySignalRollup, "relay_signal_rollup", LOOPBACK),
    policy!(DispatchWatchdog, "dispatch_watchdog", LOOPBACK),
    // #4460: stall watchdog now MENTIONS the owner instead of force-terminating
    // a suspected-stall turn — it posts a rate-limited outbox alert.
    policy!(StallWatchdog, "stall_watchdog", LOOPBACK),
    policy!(AgentdeskCli, "agentdesk-cli", CLI),
    policy!(Operator, "operator", CLI),
    policy!(Dashboard, "dashboard", DASHBOARD),
];

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("source `{label}` is not allowed for {caller:?}")]
pub struct SendSourcePolicyError {
    pub label: String,
    pub caller: SendCallerClass,
}

/// Exact, case-sensitive and fail-closed source authorization.
pub fn validate_send_source_for(
    source: &str,
    caller: SendCallerClass,
) -> Result<(), SendSourcePolicyError> {
    let exact = !source.is_empty() && source.trim() == source;
    let static_allowed = exact
        && POLICIES
            .iter()
            .any(|policy| policy.label == source && policy.caller_mask & caller.mask() != 0);
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
    use super::{POLICIES, SendCallerClass, validate_send_source_for};

    const CURRENT_LOOPBACK_LABELS: &[&str] = &[
        "kanban-rules",
        "triage-rules",
        "review-automation",
        "auto-queue",
        "pipeline",
        "system",
        "timeouts",
        "merge-automation",
        "lifecycle_notifier",
        "routine-runtime",
        "headless_turn",
        "slo_alerter",
        "quality_regression_alerter",
        "auto-queue-monitor",
        "inventory",
        "voice",
    ];
    const NEW_LOOPBACK_LABELS: &[&str] = &[
        "scheduled_message",
        "github_sync",
        "catch_up_too_old",
        "queue_overflow_notice",
        "outbox_delivery_alert",
        "long_turn_watchdog",
        "relay_signal_rollup",
        "dispatch_watchdog",
        "stall_watchdog",
    ];

    #[test]
    fn producer_contract_lists_every_loopback_source() {
        let mut expected = CURRENT_LOOPBACK_LABELS.to_vec();
        expected.extend_from_slice(NEW_LOOPBACK_LABELS);
        for label in expected {
            assert!(
                validate_send_source_for(label, SendCallerClass::LoopbackInternal).is_ok(),
                "registered producer `{label}` must enqueue and send as LoopbackInternal"
            );
        }
        assert_eq!(
            POLICIES
                .iter()
                .filter(|p| p.caller_mask == super::LOOPBACK)
                .count(),
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
