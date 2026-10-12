use super::*;

fn crumbs(started: u64) -> Breadcrumbs {
    Breadcrumbs {
        db_in_flight: 0,
        db_probes_started: started,
        db_probes_failed: 0,
        last_db_ok_age_ms: None,
        last_db_err_age_ms: None,
        runtime_ticks: 50,
        runtime_tick_age_ms: Some(10),
        runtime_workers: 1,
    }
}

fn event(verdict: HealthVerdict, observed_at: Instant, progress: bool) -> WatchdogEvent {
    let mut current = crumbs(if progress { 101 } else { 100 });
    let liveness = match verdict {
        HealthVerdict::RuntimeStalled | HealthVerdict::Responsive => {
            current.runtime_tick_age_ms = Some(5_001);
            RuntimeLiveness::Stalled { age_ms: 5_001 }
        }
        HealthVerdict::UndeterminedNoBeacon => {
            current.runtime_tick_age_ms = None;
            RuntimeLiveness::Unknown
        }
        _ => RuntimeLiveness::Scheduling { age_ms: 10 },
    };
    if verdict == HealthVerdict::HandlerBlockedOnDb {
        current.db_in_flight = 1;
    }
    let outcome = match verdict {
        HealthVerdict::Responsive => HealthProbeOutcome::Responded { elapsed_ms: 1 },
        HealthVerdict::ListenerGone => HealthProbeOutcome::ConnectFailed {
            elapsed_ms: 1,
            error: "closed listener".into(),
        },
        HealthVerdict::ConnectionResetBeforeRequest => HealthProbeOutcome::RequestFailed {
            elapsed_ms: 1,
            error: "write failed".into(),
        },
        _ => HealthProbeOutcome::NoResponse {
            elapsed_ms: 1,
            error: "no bytes".into(),
        },
    };
    WatchdogEvent::ProbeFinished {
        outcome,
        observed_at,
        current,
        liveness,
    }
}

fn check_row(verdict: HealthVerdict, progress: bool, reason: Reason, fast: bool) {
    let policy = WatchdogPolicy::DEFAULT;
    let start = Instant::now();
    for old_fast in [0_u32, 2, 9] {
        for at in [Duration::from_secs(1), policy.blackout_limit] {
            let mut initial = WatchdogState::new(start);
            initial.observed_failures = 7;
            initial.fast_failures = old_fast;
            initial.previous = Some(crumbs(100));
            let observed_at = start + at;
            let (state, decision) = reduce(&policy, initial, event(verdict, observed_at, progress));
            if verdict == HealthVerdict::Responsive {
                assert_eq!(decision, Decision::Continue(Reason::Responsive));
                assert_eq!(state.observed_failures, 0);
                assert_eq!(state.fast_failures, 0);
                assert_eq!(state.last_response_at, Some(observed_at));
                assert_eq!(state.deadline(&policy), observed_at + policy.blackout_limit);
            } else if at == policy.blackout_limit {
                assert_eq!(
                    decision,
                    Decision::Exit(Reason::HealthResponseBlackoutLimit)
                );
                assert_eq!(state.last_response_at, None);
                assert_eq!(state.deadline(&policy), start + policy.blackout_limit);
            } else {
                let next_fast = if fast { old_fast + 1 } else { 0 };
                let expected = if fast && next_fast >= policy.max_failures {
                    Decision::Exit(reason)
                } else {
                    Decision::Warn(reason)
                };
                assert_eq!(
                    decision, expected,
                    "{verdict:?}, progress={progress}, old_fast={old_fast}"
                );
                assert_eq!(state.observed_failures, 8);
                assert_eq!(state.fast_failures, next_fast);
                assert_eq!(state.last_response_at, None);
                assert_eq!(state.deadline(&policy), start + policy.blackout_limit);
            }
            assert_eq!(
                state.previous.unwrap().db_probes_started,
                if progress { 101 } else { 100 }
            );
        }
    }
}

#[test]
fn responsive_with_db_progress() {
    check_row(HealthVerdict::Responsive, true, Reason::Responsive, false);
}

#[test]
fn responsive_without_db_progress() {
    check_row(HealthVerdict::Responsive, false, Reason::Responsive, false);
}

#[test]
fn stalled_with_db_progress() {
    check_row(
        HealthVerdict::RuntimeStalled,
        true,
        Reason::RuntimeStalled,
        true,
    );
}

#[test]
fn stalled_without_db_progress() {
    check_row(
        HealthVerdict::RuntimeStalled,
        false,
        Reason::RuntimeStalled,
        true,
    );
}

#[test]
fn listener_gone_with_db_progress() {
    check_row(
        HealthVerdict::ListenerGone,
        true,
        Reason::ListenerGone,
        true,
    );
}

#[test]
fn listener_gone_without_db_progress() {
    check_row(
        HealthVerdict::ListenerGone,
        false,
        Reason::ListenerGone,
        true,
    );
}

#[test]
fn handler_blocked_with_db_progress() {
    check_row(
        HealthVerdict::HandlerBlockedOnDb,
        true,
        Reason::HandlerDbProgress,
        false,
    );
}

#[test]
fn handler_blocked_without_db_progress() {
    check_row(
        HealthVerdict::HandlerBlockedOnDb,
        false,
        Reason::HandlerNoProgressObserved,
        false,
    );
}

#[test]
fn handler_idle_with_db_progress() {
    check_row(
        HealthVerdict::HandlerSlowDbIdle,
        true,
        Reason::HandlerDbProgress,
        false,
    );
}

#[test]
fn handler_idle_without_db_progress() {
    check_row(
        HealthVerdict::HandlerSlowDbIdle,
        false,
        Reason::HandlerNoProgressObserved,
        false,
    );
}

#[test]
fn request_failed_with_db_progress() {
    check_row(
        HealthVerdict::ConnectionResetBeforeRequest,
        true,
        Reason::RequestFailed,
        true,
    );
}

#[test]
fn request_failed_without_db_progress() {
    check_row(
        HealthVerdict::ConnectionResetBeforeRequest,
        false,
        Reason::RequestFailed,
        true,
    );
}

#[test]
fn unknown_with_db_progress() {
    check_row(
        HealthVerdict::UndeterminedNoBeacon,
        true,
        Reason::NoBeacon,
        false,
    );
}

#[test]
fn unknown_without_db_progress() {
    check_row(
        HealthVerdict::UndeterminedNoBeacon,
        false,
        Reason::NoBeacon,
        false,
    );
}

#[test]
fn a_soft_failure_breaks_the_fast_failure_streak() {
    let policy = WatchdogPolicy::DEFAULT;
    let start = Instant::now();
    let mut state = WatchdogState::new(start);
    state.previous = Some(crumbs(100));
    for verdict in [HealthVerdict::RuntimeStalled, HealthVerdict::ListenerGone] {
        let (next, decision) = reduce(
            &policy,
            state,
            event(verdict, start + Duration::from_secs(1), false),
        );
        assert!(matches!(decision, Decision::Warn(_)));
        state = next;
    }
    let (state, decision) = reduce(
        &policy,
        state,
        event(
            HealthVerdict::HandlerSlowDbIdle,
            start + Duration::from_secs(2),
            false,
        ),
    );
    assert_eq!(decision, Decision::Warn(Reason::HandlerNoProgressObserved));
    assert_eq!(state.fast_failures, 0);
    let (state, decision) = reduce(
        &policy,
        state,
        event(
            HealthVerdict::RuntimeStalled,
            start + Duration::from_secs(3),
            false,
        ),
    );
    assert_eq!(decision, Decision::Warn(Reason::RuntimeStalled));
    assert_eq!(state.fast_failures, 1);
    assert_eq!(state.observed_failures, 4);
}

#[test]
fn different_fast_failure_kinds_keep_the_streak() {
    let policy = WatchdogPolicy::DEFAULT;
    let start = Instant::now();
    let mut state = WatchdogState::new(start);
    for (index, verdict) in [
        HealthVerdict::RuntimeStalled,
        HealthVerdict::ListenerGone,
        HealthVerdict::ConnectionResetBeforeRequest,
    ]
    .into_iter()
    .enumerate()
    {
        let (next, decision) = reduce(
            &policy,
            state,
            event(verdict, start + Duration::from_secs(1), true),
        );
        state = next;
        assert_eq!(state.fast_failures, index as u32 + 1);
        if index == 2 {
            assert_eq!(decision, Decision::Exit(Reason::RequestFailed));
        } else {
            assert!(matches!(decision, Decision::Warn(_)));
        }
    }
}

#[test]
fn a_response_one_tick_after_deadline_does_not_reset_blackout() {
    let policy = WatchdogPolicy::DEFAULT;
    let start = Instant::now();
    let mut initial = WatchdogState::new(start);
    initial.last_response_at = Some(start + Duration::from_secs(10));
    initial.observed_failures = 7;
    initial.fast_failures = 2;
    let deadline = initial.deadline(&policy);
    let (state, decision) = reduce(
        &policy,
        initial,
        event(
            HealthVerdict::Responsive,
            deadline + Duration::from_nanos(1),
            true,
        ),
    );
    assert_eq!(
        decision,
        Decision::Exit(Reason::HealthResponseBlackoutLimit)
    );
    assert_eq!(state.last_response_at, initial.last_response_at);
    assert_eq!(state.deadline(&policy), deadline);
    assert_eq!(state.observed_failures, 7);
}

#[test]
fn a_response_at_deadline_is_observed_before_blackout_expires() {
    let policy = WatchdogPolicy::DEFAULT;
    let start = Instant::now();
    let initial = WatchdogState::new(start);
    let deadline = initial.deadline(&policy);
    let (state, decision) = reduce(
        &policy,
        initial,
        event(HealthVerdict::Responsive, deadline, false),
    );
    assert_eq!(decision, Decision::Continue(Reason::Responsive));
    assert_eq!(state.last_response_at, Some(deadline));
    assert_eq!(state.deadline(&policy), deadline + policy.blackout_limit);
}

#[test]
fn silence_deadline_does_not_count_a_second_probe_failure() {
    let policy = WatchdogPolicy::DEFAULT;
    let start = Instant::now();
    let mut initial = WatchdogState::new(start);
    initial.observed_failures = 8;
    initial.fast_failures = 2;
    let (state, decision) = reduce(
        &policy,
        initial,
        WatchdogEvent::SilenceDeadlineReached {
            observed_at: initial.deadline(&policy),
        },
    );
    assert_eq!(
        decision,
        Decision::Exit(Reason::HealthResponseBlackoutLimit)
    );
    assert_eq!(state.observed_failures, 8);
    assert_eq!(state.fast_failures, 2);
    assert_eq!(state.previous, None);
}

#[test]
fn db_progress_uses_started_increase_or_inclusive_recent_success() {
    let previous = crumbs(100);
    assert!(db_progress(Some(&previous), &crumbs(101), 5_000));
    assert!(!db_progress(Some(&previous), &crumbs(100), 5_000));
    assert!(!db_progress(Some(&previous), &crumbs(99), 5_000));
    assert!(!db_progress(Some(&crumbs(u64::MAX)), &crumbs(0), 5_000));
    assert!(!db_progress(None, &crumbs(101), 5_000));
    let mut current = crumbs(100);
    current.last_db_ok_age_ms = Some(5_000);
    assert!(db_progress(None, &current, 5_000));
    current.last_db_ok_age_ms = Some(5_001);
    assert!(!db_progress(Some(&previous), &current, 5_000));
}

#[test]
fn in_flight_failure_and_runtime_ticks_are_not_db_progress() {
    let previous = crumbs(100);
    let mut current = previous;
    current.db_in_flight = 9;
    current.db_probes_failed = 0;
    current.last_db_err_age_ms = Some(1);
    current.runtime_ticks += 1;
    assert!(!db_progress(Some(&previous), &current, 5_000));
}

#[test]
fn db_progress_never_moves_the_response_deadline() {
    let policy = WatchdogPolicy::DEFAULT;
    let start = Instant::now();
    let mut initial = WatchdogState::new(start);
    initial.previous = Some(crumbs(100));
    let deadline = initial.deadline(&policy);
    let (state, decision) = reduce(
        &policy,
        initial,
        event(
            HealthVerdict::HandlerBlockedOnDb,
            deadline - Duration::from_nanos(1),
            true,
        ),
    );
    assert_eq!(decision, Decision::Warn(Reason::HandlerDbProgress));
    assert_eq!(state.deadline(&policy), deadline);
    assert_eq!(state.last_response_at, None);
    let (_, decision) = reduce(
        &policy,
        state,
        WatchdogEvent::SilenceDeadlineReached {
            observed_at: deadline,
        },
    );
    assert_eq!(
        decision,
        Decision::Exit(Reason::HealthResponseBlackoutLimit)
    );
}

#[test]
fn failure_counters_saturate_and_soft_failure_clears_fast_counter() {
    let policy = WatchdogPolicy::DEFAULT;
    let start = Instant::now();
    let mut initial = WatchdogState::new(start);
    initial.observed_failures = u32::MAX;
    initial.fast_failures = u32::MAX;
    let (state, decision) = reduce(
        &policy,
        initial,
        event(
            HealthVerdict::RuntimeStalled,
            start + Duration::from_secs(1),
            false,
        ),
    );
    assert_eq!(state.observed_failures, u32::MAX);
    assert_eq!(state.fast_failures, u32::MAX);
    assert_eq!(decision, Decision::Exit(Reason::RuntimeStalled));
    let (state, decision) = reduce(
        &policy,
        initial,
        event(
            HealthVerdict::UndeterminedNoBeacon,
            start + Duration::from_secs(1),
            false,
        ),
    );
    assert_eq!(state.observed_failures, u32::MAX);
    assert_eq!(state.fast_failures, 0);
    assert_eq!(decision, Decision::Warn(Reason::NoBeacon));
}

#[test]
fn deadline_and_blackout_start_with_monitoring_then_last_response() {
    let policy = WatchdogPolicy::DEFAULT;
    let start = Instant::now();
    let mut state = WatchdogState::new(start);
    assert_eq!(state.deadline(&policy), start + policy.blackout_limit);
    assert_eq!(
        state.blackout_at(start + Duration::from_secs(1)),
        Duration::from_secs(1)
    );
    state.last_response_at = Some(start + Duration::from_secs(20));
    assert_eq!(
        state.deadline(&policy),
        start + Duration::from_secs(20) + policy.blackout_limit
    );
    assert_eq!(
        state.blackout_at(start + Duration::from_secs(21)),
        Duration::from_secs(1)
    );
}

#[test]
fn production_policy_and_log_labels_are_explicit() {
    let policy = WatchdogPolicy::DEFAULT;
    assert_eq!(policy.check_interval, Duration::from_secs(30));
    assert_eq!(policy.probe_limit, Duration::from_secs(5));
    assert_eq!(policy.blackout_limit, Duration::from_secs(300));
    assert_eq!(policy.dump_limit, Duration::from_secs(5));
    assert_eq!(policy.max_failures, 3);
    assert_eq!(policy.db_recent_limit_ms, 5_000);
    assert_eq!(policy.runtime_stale_limit_ms, 5_000);
    assert_eq!(Decision::Continue(Reason::Responsive).action(), "continue");
    assert_eq!(Decision::Warn(Reason::HandlerDbProgress).action(), "warn");
    let exit = Decision::Exit(Reason::HealthResponseBlackoutLimit);
    assert_eq!(exit.action(), "exit");
    assert_eq!(exit.reason().label(), "health_response_blackout_limit");
}
