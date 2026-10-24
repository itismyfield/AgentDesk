//! Fixture runs for the judge's policy profile and its run-to-run guards: policy load that never
//! ran or never finished, mixed profiles, a shifted health answer, ages, and growing connections.

use super::cases_tests::{FIXTURE_PLAN, STEADY, Shape, fixture_data, intact, version};
use super::*;

const MAIN_INTERVAL_MS: u64 = 250;
const SELECTS: u64 = 12;
const CONTEXTS: usize = 4;

/// An `s2` run: health as in `fixture_data`, hooks on both lanes shaped like an endpoint, each
/// whole hook's SELECTs counted, and a steady acquire probe.
pub(super) fn s2_data(name: &str, binary: &str, hooks: Shape) -> RunData {
    let mut run = fixture_data(name, binary, STEADY, STEADY);
    let plan = FIXTURE_PLAN;
    for (key, value) in [
        ("profile", "s2".to_string()),
        (Lane::Main.interval_key(), MAIN_INTERVAL_MS.to_string()),
        (Lane::Tick.interval_key(), "1000".to_string()),
        ("selects_per_hook", SELECTS.to_string()),
        ("contexts_with_pg", CONTEXTS.to_string()),
    ] {
        run.conditions.insert(key.to_string(), value);
    }
    let Shape(latency, lag, unfinished_every) = hooks;
    for (lane, interval) in [(Lane::Main, MAIN_INTERVAL_MS), (Lane::Tick, 1_000)] {
        for (seq, scheduled) in plan.policy_schedule(lane, interval).into_iter().enumerate() {
            let measured = scheduled >= plan.warmup_end();
            let unfinished = unfinished_every.is_some_and(|every| measured && seq % every == 0);
            run.policy.push(PolicyAttempt {
                lane,
                seq,
                measured,
                scheduled,
                sent: Some(scheduled + lag),
                end: (!unfinished).then_some(scheduled + lag + latency),
                outcome: if unfinished {
                    PolicyOutcome::Timeout
                } else {
                    PolicyOutcome::Read
                },
            });
        }
    }
    let hooks = run.policy.len();
    let r = &mut run.resources;
    r.peak = Peak {
        proxy_open: Some(5),
        pool_size: Some(3),
        beyond_pool: Some(2),
    };
    r.policy_statements = Some(hooks * SELECTS as usize);
    run.acquires = (0..plan.end() / SEC)
        .map(|second| Acquire {
            measured: second * SEC >= plan.warmup_end(),
            waited: Some(200),
        })
        .collect();
    run
}

fn s2_version(name: &str, binary: &str, hooks: Shape) -> Vec<RunSummary> {
    (1..=3)
        .map(|run| summarize(&s2_data(&format!("{name}-{run}"), binary, hooks)))
        .collect()
}

fn has(reasons: &[String], wanted: &str) -> bool {
    reasons.iter().any(|reason| reason.contains(wanted))
}

#[test]
fn amplification_verdict_refuses_s2_runs_whose_policy_load_did_not_run() {
    let base = s2_version("base", "b", Shape(30_000, 0, None));
    let candidate = s2_version("s2", "c", Shape(10_000, 0, None));
    let comparison = compare(&base, &candidate);
    assert!(comparison.invalid.is_empty(), "{:?}", comparison.invalid);
    let policy = comparison
        .policy
        .expect("an s2 comparison judges the policy");
    assert_eq!(policy.verdict, Verdict::Improved);

    // Hooks returned but recorded no reads and the proxy saw no policy SELECT.
    let mut idle = s2_data("idle", "c", Shape(10_000, 0, None));
    for hook in &mut idle.policy {
        if hook.outcome == PolicyOutcome::Read {
            hook.outcome = PolicyOutcome::Mismatch("unrecorded".to_string());
        }
    }
    idle.resources.policy_statements = Some(0);
    let idle = summarize(&idle);
    assert!(has(&idle.invalid, "policy_unqueried"), "{:?}", idle.invalid);
    assert!(has(&idle.invalid, "policy_read_mismatch:main"));
    assert!(has(&idle.invalid, "policy_no_completed:main"));

    // A hot reload moved the hooks to another Context, so their records never came back.
    let mut moved = s2_data("moved", "c", Shape(10_000, 0, None));
    for hook in moved
        .policy
        .iter_mut()
        .filter(|hook| hook.lane == Lane::Main)
    {
        hook.outcome = PolicyOutcome::Mismatch(RELOADED.to_string());
    }
    assert!(has(&summarize(&moved).invalid, "policy_reloaded:main"));

    // No hook was ever fired: every attempt unsent.
    let mut unsent = s2_data("unsent", "c", Shape(10_000, 0, None));
    for hook in &mut unsent.policy {
        (hook.sent, hook.end, hook.outcome) = (None, None, PolicyOutcome::Unsent);
    }
    let unsent = summarize(&unsent);
    assert!(has(&unsent.invalid, "policy_no_completed:tick"));

    // Fewer policy SELECTs than the whole hooks owe, or no record of them at all.
    let mut short = s2_data("short", "c", Shape(10_000, 0, None));
    short.resources.policy_statements = Some(10);
    assert!(has(&summarize(&short).invalid, "policy_reads_short:10<"));
    let mut lost = s2_data("lost", "c", Shape(10_000, 0, None));
    lost.resources.policy_statements = None;
    lost.policy.retain(|hook| hook.lane == Lane::Main);
    let lost = summarize(&lost);
    assert!(has(&lost.invalid, "resource_unobserved:policy_statements"));
    assert!(has(&lost.invalid, "policy_reconciliation_mismatch:tick"));

    let mixed = compare(&base, &[candidate[0].clone(), idle, unsent]);
    assert_eq!(mixed.policy.unwrap().verdict, Verdict::Unresolved);
    assert_eq!(mixed.detail.verdict, Verdict::Unresolved);
}

#[test]
fn amplification_verdict_withholds_the_policy_p99_while_hooks_never_finished() {
    let base = s2_version("base", "b", Shape(30_000, 0, None));
    let candidate = s2_version("s2", "c", Shape(10_000, 0, Some(40)));
    let main = &candidate[0].policy[&Lane::Main];
    assert!(main.counts.timeouts > main.counts.attempts / 100);
    assert!(matches!(
        percentile(&main.judged, 990),
        Stat::Unresolved { .. }
    ));
    let comparison = compare(&base, &candidate);
    assert!(comparison.invalid.is_empty(), "{:?}", comparison.invalid);
    assert_eq!(comparison.policy.unwrap().verdict, Verdict::Unresolved);
}

#[test]
fn amplification_verdict_refuses_to_compare_runs_of_different_profiles() {
    let base = version("base", "b", STEADY, STEADY);
    let candidate = s2_version("s2", "c", Shape(10_000, 0, None));
    let comparison = compare(&base, &candidate);
    assert!(
        has(&comparison.invalid, "condition_mismatch:profile"),
        "{:?}",
        comparison.invalid
    );
    assert!(comparison.policy.is_none());
    assert_eq!(comparison.detail.verdict, Verdict::Unresolved);
}

#[test]
fn amplification_verdict_refuses_a_candidate_whose_detail_answers_lost_a_reason() {
    let reasoned = |name: &str, binary: &str, reasons: &[&str]| {
        let mut run = fixture_data(name, binary, STEADY, Shape(10_000, 0, None));
        for attempt in &mut run.attempts {
            if let Outcome::Answered(answer) = &mut attempt.outcome
                && attempt.endpoint == Endpoint::Detail
            {
                answer.reasons = reasons.iter().map(|reason| reason.to_string()).collect();
            }
        }
        summarize(&run)
    };
    let base: Vec<RunSummary> = (1..=3)
        .map(|n| {
            reasoned(
                &format!("base-{n}"),
                "b",
                &["db_slow", "provider:claude:disconnected"],
            )
        })
        .collect();
    let candidate: Vec<RunSummary> = (1..=3)
        .map(|n| reasoned(&format!("s1-{n}"), "c", &["provider:claude:disconnected"]))
        .collect();
    assert!(
        candidate[0].invalid.is_empty(),
        "{:?}",
        candidate[0].invalid
    );
    let comparison = compare(&base, &candidate);
    assert!(
        has(&comparison.invalid, "reference_mismatch:detail"),
        "{:?}",
        comparison.invalid
    );
    assert!(!has(&comparison.invalid, "reference_mismatch:public"));
    assert_eq!(comparison.detail.verdict, Verdict::Unresolved);
}

#[test]
fn amplification_verdict_drops_only_the_outbox_age_number() {
    let detail = Endpoint::Detail;
    let answered = |reason: &str| Attempt {
        endpoint: detail,
        seq: 0,
        measured: true,
        scheduled: 0,
        sent: Some(0),
        last_byte: Some(1),
        outcome: Outcome::Answered(Answer {
            reasons: vec![reason.to_string()],
            ..intact(detail)
        }),
    };
    let reference_of = |reason: &str| -> Reference {
        let run = RunData {
            attempts: vec![Attempt {
                measured: false,
                ..answered(reason)
            }],
            ..RunData::default()
        };
        let warmup: Vec<&Attempt> = run.attempts.iter().collect();
        super::reference(&warmup).unwrap()
    };
    let aging = reference_of("dispatch_outbox_oldest_pending_age:305");
    assert_eq!(
        classify(
            &answered("dispatch_outbox_oldest_pending_age:306"),
            Some(&aging)
        ),
        Class::Completed
    );
    let warnings = reference_of("pipeline_override_warnings:1");
    assert_eq!(
        classify(&answered("pipeline_override_warnings:2"), Some(&warnings)),
        Class::Error
    );
    assert_eq!(
        classify(&answered("pipeline_override_warnings:1"), Some(&warnings)),
        Class::Completed
    );
}

#[test]
fn amplification_verdict_refuses_connections_that_grow_where_none_should_open() {
    let mut grown = fixture_data("grown", "c", STEADY, STEADY);
    let end = &mut grown.resources.end;
    (end.pool_size, end.proxy_accepted, end.proxy_open) = (Some(5), Some(5), Some(5));
    let invalid = summarize(&grown).invalid;
    for wanted in ["connection_churn:2", "connection_growth", "pool_growth"] {
        assert!(has(&invalid, wanted), "{wanted}: {invalid:?}");
    }

    // The policy profile: churn is the baseline's to show, the candidate's to bound by `C`.
    let churned = |name: &str, binary: &str, accepted: usize| {
        let mut run = s2_data(name, binary, Shape(10_000, 0, None));
        run.resources.end.proxy_accepted = Some(3 + accepted);
        summarize(&run)
    };
    let base: Vec<RunSummary> = (1..=3)
        .map(|n| churned(&format!("base-{n}"), "b", 900))
        .collect();
    assert!(base[0].invalid.is_empty(), "{:?}", base[0].invalid);
    let within: Vec<RunSummary> = (1..=3)
        .map(|n| churned(&format!("s2-{n}"), "c", CONTEXTS))
        .collect();
    assert!(compare(&base, &within).invalid.is_empty());
    let beyond: Vec<RunSummary> = (1..=3)
        .map(|n| churned(&format!("s2-{n}"), "c", CONTEXTS + 1))
        .collect();
    assert!(has(
        &compare(&base, &beyond).invalid,
        "candidate_connection_churn:s2-1:5>4"
    ));
}
