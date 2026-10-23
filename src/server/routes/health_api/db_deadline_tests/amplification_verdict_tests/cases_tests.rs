//! Fixture runs for the judge: unfinished attempts, late sends, lost records, short versions,
//! broken answers and refused PostgreSQL bases.

use super::*;

const FIXTURE_PLAN: Plan = Plan {
    seed: 7,
    rps: 10,
    warmup_secs: 2,
    measure_secs: 340,
};

/// Latency after the actual send, send lag after the schedule, and every n-th measured
/// attempt left unfinished (timeouts and unsent in turn).
#[derive(Clone, Copy)]
struct Shape(u64, u64, Option<usize>);

fn intact(endpoint: Endpoint) -> Answer {
    Answer {
        status: 200,
        parsed: true,
        observation_status: "observed".to_string(),
        node_hostname: endpoint == Endpoint::Detail,
        reasons: Vec::new(),
    }
}

fn fixture_data(name: &str, binary: &str, public: Shape, detail: Shape) -> RunData {
    let plan = FIXTURE_PLAN;
    let mut attempts = Vec::new();
    for (endpoint, Shape(latency, lag, unfinished_every)) in
        [(Endpoint::Public, public), (Endpoint::Detail, detail)]
    {
        for (seq, scheduled) in plan.schedule(endpoint).into_iter().enumerate() {
            let measured = scheduled >= plan.warmup_end();
            let mut attempt = Attempt {
                endpoint,
                seq,
                measured,
                scheduled,
                sent: Some(scheduled + lag),
                last_byte: Some(scheduled + lag + latency),
                outcome: Outcome::Answered(intact(endpoint)),
            };
            if let Some(every) = unfinished_every.filter(|every| measured && seq % every == 0) {
                attempt.last_byte = None;
                attempt.outcome = if seq / every % 2 == 0 {
                    Outcome::Timeout
                } else {
                    attempt.sent = None;
                    Outcome::Unsent
                };
            }
            attempts.push(attempt);
        }
    }
    let healthy = Sample {
        threads: Some(40),
        pool_size: 3,
        pool_idle: 3,
        proxy_accepted: 3,
        proxy_open: 3,
    };
    RunData {
        name: name.to_string(),
        conditions: [
            ("seed", "7"),
            ("rps_per_endpoint", "10"),
            ("warmup_secs", "2"),
            ("measure_secs", "340"),
            (BINARY_KEY, binary),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect(),
        attempts,
        ticks: (0..plan.tick_slots())
            .map(|slot| (slot, slot * SEC + 1_000))
            .collect(),
        resources: Resources {
            start: healthy,
            end: healthy,
            pool_max: 8,
            pg_statements: 100,
        },
        invalid: Vec::new(),
    }
}

fn version(name: &str, binary: &str, public: Shape, detail: Shape) -> Vec<RunSummary> {
    (1..=3)
        .map(|run| {
            summarize(&fixture_data(
                &format!("{name}-{run}"),
                binary,
                public,
                detail,
            ))
        })
        .collect()
}

const STEADY: Shape = Shape(50_000, 0, None);

#[test]
fn amplification_verdict_withholds_p99_when_two_percent_never_completed() {
    let base = version("base", "b", STEADY, STEADY);
    let candidate = version("s1", "c", STEADY, Shape(10_000, 0, Some(50)));
    let detail = &candidate[0].endpoints[&Endpoint::Detail];
    assert_eq!(detail.counts.incomplete(), 68);
    assert_eq!((detail.counts.timeouts, detail.counts.unsent), (34, 34));
    let unresolved = Stat::Unresolved {
        incomplete: 68,
        total: 3_400,
    };
    assert_eq!(percentile(&detail.judged, 990), unresolved);
    assert_eq!(percentile(&detail.judged, 1000), unresolved);
    assert_eq!(percentile(&detail.judged, 950), Stat::Value(10_000));
    let comparison = compare(&base, &candidate);
    assert!(comparison.invalid.is_empty(), "{:?}", comparison.invalid);
    assert_eq!(comparison.detail.verdict, Verdict::Unresolved);
}

#[test]
fn amplification_verdict_judges_from_the_scheduled_send_not_the_late_actual_send() {
    let base = version("base", "b", STEADY, STEADY);
    let candidate = version("s1", "c", STEADY, Shape(10_000, 80_000, None));
    let detail = &candidate[0].endpoints[&Endpoint::Detail];
    assert_eq!(percentile(&detail.sent_basis, 990), Stat::Value(10_000));
    assert_eq!(percentile(&detail.lag, 990), Stat::Value(80_000));
    let comparison = compare(&base, &candidate);
    assert!(comparison.invalid.is_empty(), "{:?}", comparison.invalid);
    assert_eq!(comparison.detail.verdict, Verdict::Worse);
}

#[test]
fn amplification_verdict_reports_separated_complete_runs_and_a_moved_control() {
    let base = version("base", "b", STEADY, STEADY);
    let candidate = version("s1", "c", STEADY, Shape(10_000, 0, None));
    let comparison = compare(&base, &candidate);
    assert!(comparison.invalid.is_empty(), "{:?}", comparison.invalid);
    assert_eq!(comparison.detail.verdict, Verdict::Improved);
    assert_eq!(comparison.detail.difference(), Some(-40_000));
    assert_eq!(comparison.public.verdict, Verdict::Unconfirmed);
    assert_eq!(comparison.tick.verdict, Verdict::Unconfirmed);
    let moved = version("s1", "c", Shape(20_000, 0, None), Shape(10_000, 0, None));
    assert_eq!(
        compare(&base, &moved).detail.verdict,
        Verdict::UnconfirmedControlMoved
    );
}

#[test]
fn amplification_verdict_rejects_runs_that_lose_an_attempt_or_fall_short_of_the_minimum() {
    let base = version("base", "b", STEADY, STEADY);
    let mut candidate = version("s1", "c", STEADY, Shape(10_000, 0, None));
    let mut data = fixture_data("s1-1", "c", STEADY, Shape(10_000, 0, None));
    let lost = data
        .attempts
        .iter()
        .position(|attempt| attempt.endpoint == Endpoint::Detail && attempt.measured)
        .unwrap();
    data.attempts.remove(lost);
    candidate[0] = summarize(&data);
    let comparison = compare(&base, &candidate);
    assert!(
        comparison
            .invalid
            .contains(&"run_invalid:s1-1:reconciliation_mismatch:detail".to_string()),
        "{:?}",
        comparison.invalid
    );
    assert_eq!(comparison.detail.verdict, Verdict::Unresolved);
    let short = compare(&base[..1], &candidate[1..2]);
    assert!(
        short
            .invalid
            .iter()
            .any(|reason| reason.starts_with("insufficient_samples:candidate:detail")),
        "{:?}",
        short.invalid
    );
    assert_eq!(short.detail.verdict, Verdict::Unresolved);
}

#[test]
fn amplification_verdict_counts_unobserved_hostless_or_shifted_answers_as_errors() {
    let reference: Reference = (200, Vec::new());
    let attempt = |endpoint, answer| Attempt {
        endpoint,
        seq: 0,
        measured: true,
        scheduled: 0,
        sent: Some(0),
        last_byte: Some(1),
        outcome: Outcome::Answered(answer),
    };
    let (public, detail) = (Endpoint::Public, Endpoint::Detail);
    for endpoint in [public, detail] {
        let whole = attempt(endpoint, intact(endpoint));
        assert_eq!(classify(&whole, Some(&reference)), Class::Completed);
        assert_eq!(classify(&whole, None), Class::Error);
    }
    let aged = |age: &str| Answer {
        reasons: vec![format!("dispatch_outbox_oldest_pending_age:{age}")],
        ..intact(detail)
    };
    let aging: Reference = (200, vec!["dispatch_outbox_oldest_pending_age".to_string()]);
    assert_eq!(
        classify(&attempt(detail, aged("305")), Some(&aging)),
        Class::Completed
    );
    for (endpoint, answer) in [
        (
            detail,
            Answer {
                node_hostname: false,
                ..intact(detail)
            },
        ),
        (
            public,
            Answer {
                node_hostname: true,
                ..intact(public)
            },
        ),
        (
            detail,
            Answer {
                observation_status: "partial".to_string(),
                ..intact(detail)
            },
        ),
        (
            detail,
            Answer {
                parsed: false,
                ..intact(detail)
            },
        ),
        (
            detail,
            Answer {
                status: 503,
                ..intact(detail)
            },
        ),
        (
            detail,
            Answer {
                reasons: vec!["db_slow".to_string()],
                ..intact(detail)
            },
        ),
    ] {
        assert_eq!(
            classify(&attempt(endpoint, answer.clone()), Some(&reference)),
            Class::Error,
            "{answer:?}"
        );
    }
}

#[test]
fn amplification_pg_guard_refuses_the_operator_port_remote_hosts_and_a_missing_base() {
    for refused in [
        None,
        Some("postgres://postgres@127.0.0.1:15432"),
        Some("postgres://postgres@10.0.0.5:55432"),
        Some("postgres://postgres@db.example:55432"),
        Some("postgres://postgres@127.0.0.1"),
    ] {
        assert!(isolated_pg_base(refused).is_err(), "{refused:?}");
    }
    for accepted in [
        "postgres://postgres@127.0.0.1:55432",
        "postgresql://user:pw@[::1]:6000/db",
    ] {
        assert_eq!(isolated_pg_base(Some(accepted)), Ok(()), "{accepted}");
    }
}
