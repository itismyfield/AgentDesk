use super::policy::{RecoveryPolicy, TriggerKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MailboxStallKind {
    Healthy,
    ActiveForegroundStream,
    ExplicitBackgroundWork,
    TmuxAliveRelayDead,
    OrphanPendingToken,
    UnpairedActiveToken,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DetectorSignal {
    StreamIdleTimeout,
    GeminiIdleTimeout,
    Mailbox {
        kind: MailboxStallKind,
        elapsed_secs: u32,
        claimed_turn: bool,
    },
    TurnRateLimit,
    RateLimitCacheDanger,
    WatchdogUnresponsive,
    TmuxSessionDead,
}

pub fn classify_trigger(
    signal: &DetectorSignal,
    policy: Option<&RecoveryPolicy>,
) -> Option<TriggerKind> {
    let policy = policy.filter(|policy| policy.enabled)?;
    let trigger = match signal {
        DetectorSignal::StreamIdleTimeout | DetectorSignal::GeminiIdleTimeout => {
            TriggerKind::IdleTimeout
        }
        DetectorSignal::Mailbox {
            kind,
            elapsed_secs,
            claimed_turn,
        } => {
            if !matches!(
                kind,
                MailboxStallKind::TmuxAliveRelayDead
                    | MailboxStallKind::OrphanPendingToken
                    | MailboxStallKind::UnpairedActiveToken
            ) {
                return None;
            }
            if !*claimed_turn || *elapsed_secs < policy.stall_secs {
                return None;
            }
            TriggerKind::MailboxStall
        }
        DetectorSignal::TurnRateLimit | DetectorSignal::RateLimitCacheDanger => {
            TriggerKind::RateLimit
        }
        DetectorSignal::WatchdogUnresponsive | DetectorSignal::TmuxSessionDead => {
            TriggerKind::ProcessDeath
        }
    };
    policy.allows(trigger).then_some(trigger)
}

pub fn trigger_from_error_message(message: &str) -> Option<DetectorSignal> {
    let lower = message.to_ascii_lowercase();
    if lower.contains("produced no output for") {
        if lower.contains("gemini") {
            return Some(DetectorSignal::GeminiIdleTimeout);
        }
        return Some(DetectorSignal::StreamIdleTimeout);
    }
    if lower.contains("429")
        || lower.contains("rate limit")
        || lower.contains("quota")
        || lower.contains("resource_exhausted")
    {
        return Some(DetectorSignal::TurnRateLimit);
    }
    if lower.contains("unresponsive") {
        return Some(DetectorSignal::WatchdogUnresponsive);
    }
    if lower.contains("tmux") && (lower.contains("dead") || lower.contains("not found")) {
        return Some(DetectorSignal::TmuxSessionDead);
    }
    None
}

pub fn mailbox_kind_from_name(name: &str) -> MailboxStallKind {
    match name {
        "healthy" => MailboxStallKind::Healthy,
        "active_foreground_stream" => MailboxStallKind::ActiveForegroundStream,
        "explicit_background_work" => MailboxStallKind::ExplicitBackgroundWork,
        "tmux_alive_relay_dead" => MailboxStallKind::TmuxAliveRelayDead,
        "orphan_pending_token" => MailboxStallKind::OrphanPendingToken,
        "unpaired_active_token" => MailboxStallKind::UnpairedActiveToken,
        _ => MailboxStallKind::Other,
    }
}
