use super::WatchdogMode;
use super::policy::{
    Decision, Reason, WatchdogEvent, WatchdogPolicy, WatchdogState, db_progress, reduce,
};
use crate::services::hang_forensics::{Breadcrumbs, HealthProbeOutcome};
use std::time::{Duration, Instant};

pub(super) type Observation = (HealthProbeOutcome, Instant);

pub(super) struct Sample {
    pub observation: Observation,
    pub current: Breadcrumbs,
    pub db_progress: bool,
    pub previous_failures: u32,
}

pub(super) struct Hooks<P, S, D, L, E> {
    pub probe: P,
    pub snapshot: S,
    pub dump: D,
    pub log: L,
    pub exit: E,
}

pub(super) fn tcp_probe(addr: &str, host: &str, deadline: Instant) -> Observation {
    let observation = crate::services::hang_forensics::probe_health_once(addr, host, deadline);
    (observation.outcome, observation.observed_at)
}

pub(super) fn wake_at(
    mode: WatchdogMode,
    next_probe_at: Instant,
    deadline: Instant,
    now: Instant,
) -> Instant {
    if mode == WatchdogMode::Observe && now >= deadline {
        next_probe_at
    } else {
        next_probe_at.min(deadline)
    }
}

pub(super) fn run(
    policy: &WatchdogPolicy,
    mode: WatchdogMode,
    mut state: WatchdogState,
    mut next_probe_at: Instant,
    hooks: Hooks<
        impl FnMut(Instant) -> Observation,
        impl FnMut() -> Breadcrumbs,
        impl FnOnce() -> Result<(), String> + Send + 'static,
        impl FnMut(&WatchdogState, &Decision, Option<&Sample>, Instant),
        impl FnMut(Reason, Instant),
    >,
) {
    let Hooks {
        mut probe,
        mut snapshot,
        dump,
        mut log,
        mut exit,
    } = hooks;
    loop {
        let deadline = state.deadline(policy);
        let wake = wake_at(mode, next_probe_at, deadline, Instant::now());
        std::thread::sleep(wake.saturating_duration_since(Instant::now()));
        let probe_started_at = Instant::now();
        if probe_started_at >= deadline && mode == WatchdogMode::Enforce {
            let (new_state, decision) = reduce(
                policy,
                state,
                WatchdogEvent::SilenceDeadlineReached {
                    observed_at: probe_started_at,
                },
            );
            state = new_state;
            log(&state, &decision, None, Instant::now());
            exit(decision.reason(), Instant::now());
            return;
        }
        // Observe mode continues ordinary probes after expiry without spinning or exiting.
        let probe_deadline = if mode == WatchdogMode::Observe && probe_started_at >= deadline {
            probe_started_at + policy.probe_limit
        } else {
            (probe_started_at + policy.probe_limit).min(deadline)
        };
        let observation = probe(probe_deadline);
        let current = snapshot();
        let sample = Sample {
            previous_failures: state.observed_failures,
            db_progress: db_progress(state.previous.as_ref(), &current, policy.db_recent_limit_ms),
            observation,
            current,
        };
        let (mut new_state, mut decision) = reduce(
            policy,
            state,
            WatchdogEvent::ProbeFinished {
                outcome: sample.observation.0.clone(),
                observed_at: sample.observation.1,
                liveness: current.runtime_liveness_with_threshold(policy.runtime_stale_limit_ms),
                current,
            },
        );
        // A timely byte may reset D; other observations retain the same pre-probe deadline.
        let after_probe = Instant::now();
        if after_probe >= new_state.deadline(policy) {
            (new_state, decision) = reduce(
                policy,
                new_state,
                WatchdogEvent::SilenceDeadlineReached {
                    observed_at: after_probe,
                },
            );
        }
        state = new_state;
        log(&state, &decision, Some(&sample), Instant::now());
        if let Decision::Exit(reason) = decision
            && mode == WatchdogMode::Enforce
        {
            if reason != Reason::HealthResponseBlackoutLimit {
                wait_for_dump(dump, policy.dump_limit, deadline);
            }
            exit(reason, Instant::now());
            return;
        }
        next_probe_at = Instant::now() + policy.check_interval;
    }
}

fn wait_for_dump(
    dump: impl FnOnce() -> Result<(), String> + Send + 'static,
    limit: Duration,
    deadline: Instant,
) {
    let started = Instant::now();
    let dump_deadline = (started + limit).min(deadline);
    if started >= dump_deadline {
        return;
    }
    let (sent, result) = std::sync::mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("watchdog-dump".into())
        .spawn(move || {
            let _ = sent.send(dump());
        });
    if let Err(error) = spawned {
        tracing::warn!("watchdog: dump helper could not start ({error})");
        return;
    }
    // Recompute after thread creation so its setup cannot add a fresh wait budget.
    match result.recv_timeout(dump_deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!("watchdog: dump capture failed ({error})"),
        Err(error) => tracing::warn!("watchdog: dump wait ended ({error})"),
    }
}
