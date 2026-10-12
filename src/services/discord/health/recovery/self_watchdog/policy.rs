use std::time::{Duration, Instant};

use crate::services::hang_forensics::{
    Breadcrumbs, HealthProbeOutcome, HealthVerdict, RuntimeLiveness, classify_health,
};

#[derive(Debug, Clone, Copy)]
pub(crate) struct WatchdogPolicy {
    pub(crate) check_interval: Duration,
    pub(crate) probe_limit: Duration,
    pub(crate) blackout_limit: Duration,
    pub(crate) dump_limit: Duration,
    pub(crate) max_failures: u32,
    pub(crate) db_recent_limit_ms: u64,
    pub(crate) runtime_stale_limit_ms: u64,
}

impl WatchdogPolicy {
    pub(crate) const DEFAULT: Self = Self {
        check_interval: super::CHECK_INTERVAL,
        probe_limit: super::TCP_TIMEOUT,
        blackout_limit: super::BLACKOUT_LIMIT,
        dump_limit: super::DUMP_TIMEOUT,
        max_failures: super::MAX_FAILURES,
        db_recent_limit_ms: 5_000,
        runtime_stale_limit_ms: crate::services::hang_forensics::RUNTIME_TICK_STALE_MS,
    };
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct WatchdogState {
    pub(crate) observed_failures: u32,
    pub(crate) fast_failures: u32,
    pub(crate) monitoring_started_at: Instant,
    pub(crate) last_response_at: Option<Instant>,
    pub(crate) previous: Option<Breadcrumbs>,
}

impl WatchdogState {
    pub(crate) fn new(monitoring_started_at: Instant) -> Self {
        Self {
            observed_failures: 0,
            fast_failures: 0,
            monitoring_started_at,
            last_response_at: None,
            previous: None,
        }
    }

    pub(crate) fn deadline(&self, policy: &WatchdogPolicy) -> Instant {
        self.last_response_at.unwrap_or(self.monitoring_started_at) + policy.blackout_limit
    }

    pub(crate) fn blackout_at(&self, observed_at: Instant) -> Duration {
        observed_at
            .saturating_duration_since(self.last_response_at.unwrap_or(self.monitoring_started_at))
    }
}

#[derive(Debug)]
pub(crate) enum WatchdogEvent {
    ProbeFinished {
        outcome: HealthProbeOutcome,
        observed_at: Instant,
        current: Breadcrumbs,
        liveness: RuntimeLiveness,
    },
    SilenceDeadlineReached {
        observed_at: Instant,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reason {
    Responsive,
    RuntimeStalled,
    ListenerGone,
    RequestFailed,
    HandlerDbProgress,
    HandlerNoProgressObserved,
    NoBeacon,
    HealthResponseBlackoutLimit,
}

impl Reason {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Responsive => "responsive",
            Self::RuntimeStalled => "runtime_stalled_3",
            Self::ListenerGone => "listener_gone_3",
            Self::RequestFailed => "request_failed_3",
            Self::HandlerDbProgress => "handler_db_progress",
            Self::HandlerNoProgressObserved => "handler_no_progress_observed",
            Self::NoBeacon => "no_beacon",
            Self::HealthResponseBlackoutLimit => "health_response_blackout_limit",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Continue(Reason),
    Warn(Reason),
    Exit(Reason),
}

impl Decision {
    pub(crate) fn action(self) -> &'static str {
        match self {
            Self::Continue(_) => "continue",
            Self::Warn(_) => "warn",
            Self::Exit(_) => "exit",
        }
    }

    pub(crate) fn reason(self) -> Reason {
        match self {
            Self::Continue(reason) | Self::Warn(reason) | Self::Exit(reason) => reason,
        }
    }
}

pub(crate) fn db_progress(
    previous: Option<&Breadcrumbs>,
    current: &Breadcrumbs,
    recent_limit_ms: u64,
) -> bool {
    previous.is_some_and(|previous| current.db_probes_started > previous.db_probes_started)
        || current
            .last_db_ok_age_ms
            .is_some_and(|age| age <= recent_limit_ms)
}

pub(crate) fn reduce(
    policy: &WatchdogPolicy,
    mut state: WatchdogState,
    event: WatchdogEvent,
) -> (WatchdogState, Decision) {
    let deadline = state.deadline(policy);
    let (outcome, observed_at, current, liveness) = match event {
        WatchdogEvent::ProbeFinished {
            outcome,
            observed_at,
            current,
            liveness,
        } => (outcome, observed_at, current, liveness),
        WatchdogEvent::SilenceDeadlineReached { observed_at } => {
            let decision = if observed_at >= deadline {
                Decision::Exit(Reason::HealthResponseBlackoutLimit)
            } else {
                Decision::Continue(Reason::NoBeacon)
            };
            return (state, decision);
        }
    };
    let verdict = classify_health(&outcome, liveness, &current);
    let progress = db_progress(state.previous.as_ref(), &current, policy.db_recent_limit_ms);
    state.previous = Some(current);
    if verdict == HealthVerdict::Responsive {
        // Only bytes observed within the existing deadline begin a new blackout window.
        if observed_at > deadline {
            return (state, Decision::Exit(Reason::HealthResponseBlackoutLimit));
        }
        state.observed_failures = 0;
        state.fast_failures = 0;
        state.last_response_at = Some(observed_at);
        return (state, Decision::Continue(Reason::Responsive));
    }
    state.observed_failures = state.observed_failures.saturating_add(1);
    let (reason, fast) = match verdict {
        HealthVerdict::Responsive => (Reason::Responsive, false),
        HealthVerdict::RuntimeStalled => (Reason::RuntimeStalled, true),
        HealthVerdict::ListenerGone => (Reason::ListenerGone, true),
        HealthVerdict::ConnectionResetBeforeRequest => (Reason::RequestFailed, true),
        HealthVerdict::HandlerBlockedOnDb | HealthVerdict::HandlerSlowDbIdle if progress => {
            (Reason::HandlerDbProgress, false)
        }
        HealthVerdict::HandlerBlockedOnDb | HealthVerdict::HandlerSlowDbIdle => {
            (Reason::HandlerNoProgressObserved, false)
        }
        HealthVerdict::UndeterminedNoBeacon => (Reason::NoBeacon, false),
    };
    state.fast_failures = if fast {
        state.fast_failures.saturating_add(1)
    } else {
        0
    };
    let decision = if observed_at >= deadline {
        Decision::Exit(Reason::HealthResponseBlackoutLimit)
    } else if state.fast_failures >= policy.max_failures {
        Decision::Exit(reason)
    } else {
        Decision::Warn(reason)
    };
    (state, decision)
}

#[cfg(test)]
mod tests;
