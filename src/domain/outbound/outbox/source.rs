#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum StaticSendSource {
    KanbanRules,
    TriageRules,
    ReviewAutomation,
    AutoQueue,
    Pipeline,
    System,
    Timeouts,
    LifecycleNotifier,
    RoutingTransition,
    RoutineRuntime,
    ScheduledMessage,
    HeadlessTurn,
    SloAlerter,
    AutoQueueMonitor,
    Inventory,
    GithubSync,
    CatchUpTooOld,
    BootCustodyNotice,
    QueueOverflowNotice,
    RetainedInputNotice,
    ImessageInputNotice,
    OutboxDeliveryAlert,
    LongTurnWatchdog,
    RelaySignalRollup,
    DispatchWatchdog,
    StallWatchdog,
    TuiOAlarm,
    AdoptFenceForwardNotice,
    AgentdeskCli,
    Operator,
    Dashboard,
}

#[derive(Copy, Clone, Debug)]
struct SourcePolicy {
    source: StaticSendSource,
    label: &'static str,
    caller_mask: u8,
}

const LOOPBACK: u8 = 1;
const CLI: u8 = 2;
const DASHBOARD: u8 = 4;

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
    policy!(LifecycleNotifier, "lifecycle_notifier", LOOPBACK),
    policy!(RoutingTransition, "routing_transition", LOOPBACK),
    policy!(RoutineRuntime, "routine-runtime", LOOPBACK),
    policy!(ScheduledMessage, "scheduled_message", LOOPBACK),
    policy!(HeadlessTurn, "headless_turn", LOOPBACK),
    policy!(SloAlerter, "slo_alerter", LOOPBACK),
    policy!(AutoQueueMonitor, "auto-queue-monitor", LOOPBACK),
    policy!(Inventory, "inventory", LOOPBACK),
    policy!(GithubSync, "github_sync", LOOPBACK),
    policy!(CatchUpTooOld, "catch_up_too_old", LOOPBACK),
    policy!(BootCustodyNotice, "boot_custody_notice", LOOPBACK),
    policy!(QueueOverflowNotice, "queue_overflow_notice", LOOPBACK),
    policy!(RetainedInputNotice, "retained_input_notice", LOOPBACK),
    policy!(ImessageInputNotice, "imessage_input_notice", LOOPBACK),
    policy!(OutboxDeliveryAlert, "outbox_delivery_alert", LOOPBACK),
    policy!(LongTurnWatchdog, "long_turn_watchdog", LOOPBACK),
    policy!(RelaySignalRollup, "relay_signal_rollup", LOOPBACK),
    policy!(DispatchWatchdog, "dispatch_watchdog", LOOPBACK),
    policy!(StallWatchdog, "stall_watchdog", LOOPBACK),
    policy!(TuiOAlarm, "tui_o_alarm", LOOPBACK),
    policy!(
        AdoptFenceForwardNotice,
        "adopt_fence_forward_notice",
        LOOPBACK
    ),
    policy!(AgentdeskCli, "agentdesk-cli", CLI),
    policy!(Operator, "operator", CLI),
    policy!(Dashboard, "dashboard", DASHBOARD),
];

pub(crate) fn is_static_notice_kind(source: &str) -> bool {
    let Some(policy) = POLICIES
        .iter()
        .find(|policy| policy.label == source && policy.caller_mask & LOOPBACK != 0)
    else {
        return false;
    };
    use StaticSendSource::*;
    matches!(
        policy.source,
        KanbanRules
            | TriageRules
            | ReviewAutomation
            | AutoQueue
            | Pipeline
            | System
            | Timeouts
            | LifecycleNotifier
            | RoutingTransition
            | RoutineRuntime
            | ScheduledMessage
            | SloAlerter
            | AutoQueueMonitor
            | Inventory
            | GithubSync
            | CatchUpTooOld
            | BootCustodyNotice
            | QueueOverflowNotice
            | RetainedInputNotice
            | ImessageInputNotice
            | OutboxDeliveryAlert
            | LongTurnWatchdog
            | RelaySignalRollup
            | DispatchWatchdog
            | StallWatchdog
            | TuiOAlarm
            | AdoptFenceForwardNotice
    )
}

pub(crate) fn is_allowed_static_source(source: &str, caller_mask: u8) -> bool {
    POLICIES
        .iter()
        .any(|policy| policy.label == source && policy.caller_mask & caller_mask != 0)
}

pub(crate) fn is_static_loopback_source(source: &str) -> bool {
    is_allowed_static_source(source, LOOPBACK)
}

#[cfg(test)]
pub(crate) fn static_source_count(caller_mask: u8) -> usize {
    POLICIES
        .iter()
        .filter(|policy| policy.caller_mask == caller_mask)
        .count()
}
