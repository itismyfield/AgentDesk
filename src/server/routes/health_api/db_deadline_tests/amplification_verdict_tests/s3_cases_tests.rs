//! Fixture runs for the judge's claim profile: claim load that never ran, never reached SQL or
//! never finished, runs under different claim or watcher conditions, and a failed pool acquire.

use super::cases_tests::Shape;
use super::s2_cases_tests::s2_data;
use super::*;

const CLAIM_RPS: u64 = 20;
const WATCHERS: u64 = 2;
const WATCHER_PAUSE: u64 = 200_000;

/// An `s3` run: the `s2` run plus claims shaped like an endpoint, each claiming one row, and
/// every watcher read answered within the window.
fn s3_data(name: &str, binary: &str, claims: Shape) -> RunData {
    let mut run = s2_data(name, binary, Shape(10_000, 0, None));
    let plan = run.plan().unwrap();
    for (key, value) in [
        ("profile", "s3".to_string()),
        (CLAIM_RPS_KEY, CLAIM_RPS.to_string()),
        (WATCHERS_KEY, WATCHERS.to_string()),
        (CONFIG_SHA_KEY, "c0ffee".to_string()),
        (CONFIG_BYTES_KEY, "21600".to_string()),
    ] {
        run.conditions.insert(key.to_string(), value);
    }
    let Shape(latency, lag, unfinished_every) = claims;
    for (seq, scheduled) in plan.claim_schedule(CLAIM_RPS).into_iter().enumerate() {
        let measured = scheduled >= plan.warmup_end();
        let unfinished = unfinished_every.is_some_and(|every| measured && seq % every == 0);
        run.claims.push(ClaimAttempt {
            seq,
            measured,
            scheduled,
            sent: Some(scheduled + lag),
            last_byte: (!unfinished).then_some(scheduled + lag + latency),
            outcome: if unfinished {
                ClaimOutcome::Timeout
            } else {
                ClaimOutcome::Answered {
                    status: 200,
                    claimed: Some(1),
                }
            },
        });
    }
    for watcher in 0..WATCHERS as usize {
        for submitted in (0..plan.end()).step_by(WATCHER_PAUSE as usize) {
            run.blocking.push(BlockingRead {
                watcher,
                submitted,
                started: Some(submitted + 100),
                finished: Some(submitted + 300),
            });
        }
    }
    run
}

fn s3_version(name: &str, binary: &str, claims: Shape) -> Vec<RunSummary> {
    (1..=3)
        .map(|run| summarize(&s3_data(&format!("{name}-{run}"), binary, claims)))
        .collect()
}

fn has(reasons: &[String], wanted: &str) -> bool {
    reasons.iter().any(|reason| reason.contains(wanted))
}

#[test]
fn amplification_verdict_refuses_s3_runs_whose_claims_did_not_reach_sql() {
    let base = s3_version("base", "b", Shape(30_000, 0, None));
    let candidate = s3_version("s3", "c", Shape(10_000, 0, None));
    let comparison = compare(&base, &candidate);
    assert!(comparison.invalid.is_empty(), "{:?}", comparison.invalid);
    assert_eq!(comparison.claim.unwrap().verdict, Verdict::Improved);
    assert_eq!(candidate[0].blocking.as_ref().unwrap().submitted, 3_400);

    // The driver never sent a claim.
    let mut unsent = s3_data("unsent", "c", Shape(10_000, 0, None));
    for claim in &mut unsent.claims {
        (claim.sent, claim.last_byte, claim.outcome) = (None, None, ClaimOutcome::Unsent);
    }
    let unsent = summarize(&unsent);
    assert!(
        has(&unsent.invalid, "claim_no_completed"),
        "{:?}",
        unsent.invalid
    );
    assert!(has(&unsent.invalid, "claim_unclaimed"));

    // No claim record at all.
    let mut lost = s3_data("lost", "c", Shape(10_000, 0, None));
    lost.claims.clear();
    assert!(has(
        &summarize(&lost).invalid,
        "claim_reconciliation_mismatch"
    ));

    // Every claim answered but none claimed a row, as when the copied config was not parsed.
    let mut skipped = s3_data("skipped", "c", Shape(10_000, 0, None));
    for claim in &mut skipped.claims {
        claim.outcome = ClaimOutcome::Answered {
            status: 200,
            claimed: Some(0),
        };
    }
    let skipped = summarize(&skipped);
    assert!(
        has(&skipped.invalid, "claim_unclaimed"),
        "{:?}",
        skipped.invalid
    );
    assert!(has(&skipped.invalid, "claim_empty:"));

    // A 400 is an error, not an answer; one empty claim among the rest is still named.
    let mut refused = s3_data("refused", "c", Shape(10_000, 0, None));
    let last = refused.claims.len() - 1;
    refused.claims[last].outcome = ClaimOutcome::Answered {
        status: 400,
        claimed: None,
    };
    refused.claims[last - 1].outcome = ClaimOutcome::Answered {
        status: 200,
        claimed: Some(0),
    };
    let refused = summarize(&refused);
    assert_eq!(refused.claim.as_ref().unwrap().counts.errors, 1);
    assert_eq!(refused.invalid, vec!["claim_empty:1".to_string()]);

    // The config copy's identity was not recorded.
    let mut unread = s3_data("unread", "c", Shape(10_000, 0, None));
    unread
        .conditions
        .insert(CONFIG_BYTES_KEY.to_string(), "0".to_string());
    assert!(has(&summarize(&unread).invalid, "config_unrecorded"));

    // Watchers were configured but no read went in, or one never came back.
    let mut idle = s3_data("idle", "c", Shape(10_000, 0, None));
    idle.blocking.clear();
    assert!(has(&summarize(&idle).invalid, "blocking_unloaded"));
    let mut stuck = s3_data("stuck", "c", Shape(10_000, 0, None));
    let read = stuck
        .blocking
        .iter_mut()
        .find(|read| read.submitted >= 3 * SEC)
        .unwrap();
    read.finished = None;
    assert!(has(&summarize(&stuck).invalid, "blocking_incomplete:"));

    let mixed = compare(&base, &[candidate[0].clone(), unsent, skipped]);
    assert_eq!(mixed.claim.unwrap().verdict, Verdict::Unresolved);
}

#[test]
fn amplification_verdict_withholds_the_claim_p99_while_claims_never_finished() {
    let base = s3_version("base", "b", Shape(30_000, 0, None));
    let candidate = s3_version("s3", "c", Shape(10_000, 0, Some(40)));
    let claim = candidate[0].claim.as_ref().unwrap();
    assert!(claim.counts.timeouts > claim.counts.attempts / 100);
    assert!(matches!(
        percentile(&claim.judged, 990),
        Stat::Unresolved { .. }
    ));
    let comparison = compare(&base, &candidate);
    assert!(comparison.invalid.is_empty(), "{:?}", comparison.invalid);
    assert_eq!(comparison.claim.unwrap().verdict, Verdict::Unresolved);
}

#[test]
fn amplification_verdict_refuses_to_compare_s3_runs_under_different_conditions() {
    let base = s3_version("base", "b", Shape(30_000, 0, None));
    for (key, value) in [
        (WATCHERS_KEY, "0"),
        (CONFIG_SHA_KEY, "decade"),
        ("profile", "s2"),
    ] {
        let mut changed = s3_data("s3-1", "c", Shape(10_000, 0, None));
        changed
            .conditions
            .insert(key.to_string(), value.to_string());
        if key == WATCHERS_KEY {
            changed.blocking.clear();
        }
        let comparison = compare(&base, &[summarize(&changed)]);
        assert!(
            has(&comparison.invalid, &format!("condition_mismatch:{key}")),
            "{key}: {:?}",
            comparison.invalid
        );
        assert_eq!(comparison.detail.verdict, Verdict::Unresolved);
    }
}

#[test]
fn amplification_verdict_refuses_a_run_whose_server_pool_acquire_failed() {
    let base = s3_version("base", "b", Shape(30_000, 0, None));
    let mut failed = s3_data("s3-1", "c", Shape(10_000, 0, None));
    let acquire = failed
        .acquires
        .iter_mut()
        .rfind(|acquire| acquire.measured)
        .unwrap();
    acquire.waited = None;
    let failed = summarize(&failed);
    assert_eq!(failed.invalid, vec!["acquire_failed:1".to_string()]);
    let comparison = compare(&base, &[failed]);
    assert!(has(
        &comparison.invalid,
        "run_invalid:s3-1:acquire_failed:1"
    ));
    assert_eq!(comparison.policy.unwrap().verdict, Verdict::Unresolved);

    let mut s2 = s2_data("s2-1", "c", Shape(10_000, 0, None));
    s2.acquires[5].waited = None;
    assert!(has(&summarize(&s2).invalid, "acquire_failed:1"));
}
