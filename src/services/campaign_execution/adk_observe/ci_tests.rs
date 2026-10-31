use serde_json::json;

use super::super::tests::{FakeGh, SLUG, at, pr_json, sha};
use super::super::{PrObservation, parse_pr};
use super::*;
use crate::services::campaign_execution::model::Repository;

const RUN: u64 = 900;
const RULES: &str = "repos/itismyfield/AgentDesk/rules/branches/main?per_page=100";
const BRANCH: &str = "repos/itismyfield/AgentDesk/branches/main";

fn pr() -> PrObservation {
    parse_pr(Repository::Adk, SLUG, 77, &pr_json(&sha('a'), None)).unwrap()
}

fn meta(worker: u32, attempt: u32, tested: &str) -> Value {
    json!({"version": 1, "worker": worker, "run_attempt": attempt.to_string(), "plan_digest": "pd",
        "sha": tested, "run_id": RUN.to_string(), "archive_sha256": "x"})
}

fn rules(contexts: &[&str]) -> Value {
    json!([{"type": "required_status_checks", "parameters": {"required_status_checks":
        contexts.iter().map(|c| json!({"context": c})).collect::<Vec<_>>()}}])
}

// The classic protection summary of `GET repos/{slug}/branches/{branch}`.
fn protection(contexts: &[&str]) -> Value {
    json!({"name": "main", "protected": true, "protection": {"enabled": true, "required_status_checks": {
        "enforcement_level": "non_admins", "contexts": contexts,
        "checks": contexts.iter().map(|c| json!({"context": c, "app_id": null})).collect::<Vec<_>>()}}})
}

// Attempt n starts at minute 10n; a job that ran in attempt n runs from minute 10n+1 to 10n+5.
fn stamp(minute: u32) -> String {
    format!("2026-10-30T09:{minute:02}:00Z")
}

struct Ci {
    tested: String,
    parents: Vec<String>,
    run_head: String,
    run_attempt: u32,
    // (worker, attempt that ran its job); every attempt lists every job under its own number.
    jobs: Vec<(u32, u32)>,
    // Workers whose job record in earlier attempts shows other times than the current listing.
    rewritten: Vec<u32>,
    // (worker, attempt, worker.json)
    artifacts: Vec<(u32, u32, Value)>,
    plan: bool,
    required: Result<Value, String>,
    classic: Result<Value, String>,
    check_conclusion: &'static str,
    // Further check runs of this run: (context, status, conclusion).
    other_checks: Vec<(&'static str, &'static str, Option<&'static str>)>,
}

impl Ci {
    // A merge-ref run of PR head a on base b, single attempt, two workers.
    fn merge_ref() -> Self {
        let tested = sha('c');
        Self {
            parents: vec![sha('b'), sha('a')],
            run_head: sha('a'),
            run_attempt: 1,
            jobs: vec![(0, 1), (1, 1)],
            artifacts: vec![(0, 1, meta(0, 1, &tested)), (1, 1, meta(1, 1, &tested))],
            rewritten: Vec::new(),
            plan: true,
            required: Ok(rules(&["CI required"])),
            classic: Ok(protection(&["CI required"])),
            check_conclusion: "success",
            other_checks: Vec::new(),
            tested,
        }
    }

    // The job list GitHub returns for one attempt: a job carried over keeps the times of its own run.
    fn job_list(&self, attempt: u32) -> Value {
        let jobs = self.jobs.iter().map(|(worker, ran)| {
            let ran = (*ran).min(attempt);
            let drift = u32::from(attempt < self.run_attempt && self.rewritten.contains(worker));
            json!({"id": 1000 + worker * 10 + ran, "name": format!("Rust tests (worker {worker})"),
                "run_attempt": attempt, "status": "completed", "conclusion": "success",
                "started_at": stamp(10 * ran + 1 + drift), "completed_at": stamp(10 * ran + 5)})
        });
        json!({"total_count": self.jobs.len(), "jobs": jobs.collect::<Vec<_>>()})
    }

    fn fake(&self) -> FakeGh {
        let runs = format!("repos/{SLUG}/actions/runs/{RUN}");
        let mut artifacts = Vec::new();
        if self.plan {
            artifacts.push(json!({"id": 1, "name": "nextest-plan", "expired": false}));
        }
        let detail = |id: u64| format!("https://github.com/{SLUG}/actions/runs/{RUN}/job/{id}");
        let mut checks = vec![
            json!({"id": 5, "name": "CI required", "status": "completed", "conclusion": "failure",
                "head_sha": sha('a'), "details_url": detail(5)}),
            json!({"id": 6, "name": "CI required", "status": "completed",
                "conclusion": self.check_conclusion, "head_sha": sha('a'), "details_url": detail(6)}),
        ];
        for (id, (context, status, conclusion)) in (7..).zip(&self.other_checks) {
            checks.push(json!({"id": id, "name": context, "status": status,
                "conclusion": conclusion, "head_sha": sha('a'), "details_url": detail(id)}));
        }
        let mut gh = FakeGh::default()
            .with(
                &runs,
                json!({"id": RUN, "repository": {"full_name": SLUG}, "head_sha": self.run_head,
                    "status": "completed", "conclusion": "success", "run_attempt": self.run_attempt,
                    "run_started_at": stamp(10 * self.run_attempt),
                    "path": ".github/workflows/ci-pr.yml", "name": "CI PR", "event": "pull_request"}),
            )
            .file(
                "nextest-plan",
                "plan/plan.json",
                json!({"identity": {"sha": self.tested, "run_id": RUN.to_string()},
                    "plan_digest": "pd", "workers": vec![json!({}); self.jobs.len()]}),
            )
            .with(
                &format!("repos/{SLUG}/commits/{}", self.tested),
                json!({"parents": self.parents.iter().map(|p| json!({"sha": p})).collect::<Vec<_>>()}),
            )
            .with(
                &format!("repos/{SLUG}/commits/{}/check-runs?per_page=100", sha('a')),
                json!({"total_count": checks.len(), "check_runs": checks}),
            );
        for attempt in 1..=self.run_attempt {
            gh = gh.with(
                &format!("{runs}/attempts/{attempt}/jobs?per_page=100"),
                self.job_list(attempt),
            );
            if attempt < self.run_attempt {
                gh = gh.with(
                    &format!("{runs}/attempts/{attempt}"),
                    json!({"id": RUN, "run_attempt": attempt, "run_started_at": stamp(10 * attempt)}),
                );
            }
        }
        for (worker, attempt, meta) in &self.artifacts {
            let name = format!("nextest-worker-{worker}-attempt-{attempt}");
            artifacts.push(
                json!({"id": 100 + worker * 10 + attempt, "name": name, "expired": false,
                "digest": format!("sha256:{worker}{attempt}")}),
            );
            gh = gh.file(&name, "worker.json", meta.clone());
        }
        gh = gh.with(
            &format!("{runs}/artifacts?per_page=100"),
            json!({"total_count": artifacts.len(), "artifacts": artifacts}),
        );
        gh = match &self.required {
            Ok(rules) => gh.with(RULES, rules.clone()),
            Err(error) => gh.failing(RULES, error),
        };
        match &self.classic {
            Ok(branch) => gh.with(BRANCH, branch.clone()),
            Err(error) => gh.failing(BRANCH, error),
        }
    }

    async fn observe(&self) -> Observation<CiObservation> {
        let pr = pr();
        let target = CiTarget {
            slug: SLUG,
            pr: &pr,
            run_id: RUN,
            recorded_base: &sha('b'),
        };
        observe_ci(&self.fake(), &target, at(20)).await
    }
}

fn reason<T: std::fmt::Debug>(observation: &Observation<T>) -> String {
    match observation {
        Observation::Unknown { reason, .. } => reason.clone(),
        other => panic!("expected Unknown, got {other:?}"),
    }
}

#[tokio::test]
async fn merge_ref_on_the_recorded_base_binds_and_passes() {
    let observed = Ci::merge_ref().observe().await;
    let ci = observed.observed_evidence().expect("observed");
    let tests = ci.tests().expect("verified tests");
    assert_eq!(tests.mode, CiMode::MergeRef);
    assert_eq!(tests.tested_sha, sha('c'));
    assert_eq!(tests.tested_base, Some(sha('b')));
    assert_eq!(ci.required.contexts(), Ok(vec!["CI required".to_owned()]));
    assert_eq!(ci.verdict(), CiVerdict::Passed);
    assert!(ci.binds_head(&sha('a')));
    assert!(tests.workers.iter().all(|w| !w.inherited && w.attempt == 1));
    assert_eq!(
        tests.workers[1].artifact_digest.as_deref(),
        Some("sha256:11")
    );

    let mut head = Ci::merge_ref();
    head.tested = sha('a');
    head.artifacts = vec![(0, 1, meta(0, 1, &sha('a'))), (1, 1, meta(1, 1, &sha('a')))];
    let observed = head.observe().await;
    let tests = observed.observed_evidence().and_then(CiObservation::tests);
    assert_eq!(
        tests.map(|t| (t.mode, t.tested_base.clone())),
        Some((CiMode::Head, None))
    );
}

#[tokio::test]
async fn unrelated_merge_ref_is_unknown() {
    for parents in [
        vec![sha('b'), sha('d')],
        vec![sha('d'), sha('a')],
        vec![sha('b')],
        vec![sha('a'), sha('b')],
    ] {
        let mut ci = Ci::merge_ref();
        ci.parents = parents.clone();
        let observed = ci.observe().await;
        assert!(reason(&observed).contains("neither PR head"), "{parents:?}");
    }
}

#[tokio::test]
async fn rerun_worker_needs_its_own_attempt_and_unrerun_worker_inherits_by_identity() {
    // Attempt 2 reran worker 1 only; worker 0 keeps attempt 1.
    let mut ci = Ci::merge_ref();
    ci.run_attempt = 2;
    ci.jobs = vec![(0, 1), (1, 2)];
    ci.artifacts = vec![
        (0, 1, meta(0, 1, &sha('c'))),
        (1, 1, meta(1, 1, &sha('c'))),
        (1, 2, meta(1, 2, &sha('c'))),
    ];
    let observed = ci.observe().await;
    let tests = observed.observed_evidence().and_then(CiObservation::tests);
    assert_eq!(
        tests
            .expect("verified tests")
            .workers
            .iter()
            .map(|w| (w.worker, w.attempt, w.inherited))
            .collect::<Vec<_>>(),
        vec![(0, 1, true), (1, 2, false)]
    );

    // The rerun worker's attempt-2 artifact is missing: its attempt-1 result is not substituted.
    let mut stale = ci;
    stale.artifacts.retain(|(w, a, _)| (*w, *a) != (1, 2));
    assert!(reason(&stale.observe().await).contains("nextest-worker-1-attempt-2"));

    // An inherited artifact whose worker.json names another SHA, attempt or worker is not inherited.
    for bad in [
        meta(0, 1, &sha('d')),
        meta(0, 2, &sha('c')),
        meta(1, 1, &sha('c')),
    ] {
        let mut wrong = Ci::merge_ref();
        wrong.run_attempt = 2;
        wrong.jobs = vec![(0, 1), (1, 2)];
        wrong.artifacts = vec![(0, 1, bad), (1, 2, meta(1, 2, &sha('c')))];
        assert!(reason(&wrong.observe().await).contains("worker.json"));
    }
}

#[tokio::test]
async fn unreadable_required_set_is_unknown_and_empty_set_never_passes() {
    for error in [
        "timeout",
        "gh exited with 1: HTTP 403 Resource not accessible",
    ] {
        let mut ci = Ci::merge_ref();
        ci.required = Err(error.into());
        assert!(reason(&ci.observe().await).starts_with("required set"));
    }
    let mut ci = Ci::merge_ref();
    ci.required = Ok(json!({"message": "Not Found"}));
    assert!(reason(&ci.observe().await).starts_with("required set"));

    // No rule and no classic protection names a check; the profile judge is still required.
    let mut empty = Ci::merge_ref();
    empty.required = Ok(json!([]));
    empty.classic = Ok(json!({"name": "main", "protected": false}));
    let observed = empty.observe().await;
    let evidence = observed.observed_evidence().expect("observed");
    assert_eq!(
        evidence.required.contexts(),
        Ok(vec!["CI required".to_owned()])
    );

    // An empty set, or a non-empty one missing a source, never passes.
    let judge = || vec!["CI required".to_owned()];
    for required in [
        RequiredSet::default(),
        RequiredSet {
            ruleset: Some(Vec::new()),
            classic: Some(Vec::new()),
            profile: Vec::new(),
        },
        RequiredSet {
            ruleset: Some(judge()),
            classic: None,
            profile: judge(),
        },
    ] {
        let mut partial = evidence.clone();
        partial.required = required;
        assert!(matches!(partial.verdict(), CiVerdict::Incomplete { .. }));
    }
}

#[tokio::test]
async fn skipped_or_failed_required_context_is_not_a_pass() {
    let mut skipped = Ci::merge_ref();
    skipped.check_conclusion = "skipped";
    let observed = skipped.observe().await;
    assert!(matches!(
        observed.observed_evidence().unwrap().verdict(),
        CiVerdict::Incomplete { .. }
    ));
    let mut failed = Ci::merge_ref();
    failed.check_conclusion = "failure";
    let observed = failed.observe().await;
    assert_eq!(
        observed.observed_evidence().unwrap().verdict(),
        CiVerdict::Failed {
            contexts: vec!["CI required".into()]
        }
    );
}

#[tokio::test]
async fn ci_for_a_previous_head_is_not_yet() {
    let mut old = Ci::merge_ref();
    old.run_head = sha('9');
    assert!(matches!(old.observe().await, Observation::NotYet { .. }));

    let observed = Ci::merge_ref().observe().await;
    assert!(!observed.observed_evidence().unwrap().binds_head(&sha('f')));
}

#[tokio::test]
async fn ruleset_success_does_not_hide_classic_required_failure() {
    let mut ci = Ci::merge_ref();
    ci.required = Ok(rules(&["Lint runner"]));
    ci.classic = Ok(protection(&["CI required"]));
    ci.other_checks = vec![("Lint runner", "completed", Some("success"))];
    ci.check_conclusion = "failure";
    let observed = ci.observe().await;
    let evidence = observed.observed_evidence().expect("observed");
    assert_eq!(
        evidence.required.classic,
        Some(vec!["CI required".to_owned()])
    );
    assert_eq!(
        evidence.verdict(),
        CiVerdict::Failed {
            contexts: vec!["CI required".into()]
        }
    );

    // A context only classic protection requires, other than the profile judge, still counts.
    let mut ci = Ci::merge_ref();
    ci.required = Ok(rules(&["Lint runner"]));
    ci.classic = Ok(protection(&["Script checks"]));
    ci.other_checks = vec![
        ("Lint runner", "completed", Some("success")),
        ("Script checks", "completed", Some("failure")),
    ];
    let observed = ci.observe().await;
    assert_eq!(
        observed.observed_evidence().expect("observed").verdict(),
        CiVerdict::Failed {
            contexts: vec!["Script checks".into()]
        }
    );
}

#[tokio::test]
async fn classic_required_read_failure_is_unknown() {
    for classic in [
        Err("gh exited with 1: HTTP 403 Resource not accessible by integration".to_owned()),
        Err("timeout".to_owned()),
        Ok(json!({"name": "main"})),
        Ok(json!({"name": "main", "protected": true})),
        Ok(
            json!({"name": "main", "protected": true, "protection": {"enabled": true,
            "required_status_checks": {"contexts": ["CI required"]}}}),
        ),
    ] {
        let mut ci = Ci::merge_ref();
        ci.classic = classic.clone();
        let observed = ci.observe().await;
        assert!(
            reason(&observed).starts_with("required set: branch protection"),
            "{classic:?}"
        );
    }

    // An unprotected branch is a complete read that adds no context.
    let mut unprotected = Ci::merge_ref();
    unprotected.classic = Ok(json!({"name": "main", "protected": false}));
    let observed = unprotected.observe().await;
    let evidence = observed.observed_evidence().expect("observed");
    assert_eq!(evidence.required.classic, Some(Vec::new()));
}

#[tokio::test]
async fn carried_worker_listed_under_the_new_attempt_adopts_the_artifact_of_its_own_run() {
    // Attempt 2 reran worker 6 only; GitHub still lists workers 0-5 under attempt 2 with attempt 1's
    // times, and only attempt-1 artifacts exist for them.
    let mut ci = Ci::merge_ref();
    ci.run_attempt = 2;
    ci.jobs = (0..6).map(|w| (w, 1)).chain([(6, 2)]).collect();
    ci.artifacts = (0..7)
        .map(|w| (w, 1, meta(w, 1, &sha('c'))))
        .chain([(6, 2, meta(6, 2, &sha('c')))])
        .collect();
    let listed = ci.job_list(2);
    assert!(
        listed["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|j| j["run_attempt"] == 2)
    );
    let observed = ci.observe().await;
    let evidence = observed.observed_evidence().expect("observed");
    assert_eq!(
        evidence
            .tests()
            .expect("verified tests")
            .workers
            .iter()
            .map(|w| (w.worker, w.attempt, w.inherited))
            .collect::<Vec<_>>(),
        (0..6)
            .map(|w| (w, 1, true))
            .chain([(6, 2, false)])
            .collect::<Vec<_>>()
    );
    assert_eq!(evidence.verdict(), CiVerdict::Passed);

    // A carried job whose times differ from the record attempt 1 lists is not adopted.
    let mut rewritten = ci;
    rewritten.rewritten = vec![3];
    let observed = rewritten.observe().await;
    assert!(reason(&observed).contains("is not the run attempt 1 recorded"));
}

#[tokio::test]
async fn failed_required_check_without_plan_remains_observed_failed() {
    // CI required failed before build and plan ran: no nextest-plan and no worker artifact exist.
    let mut ci = Ci::merge_ref();
    ci.plan = false;
    ci.artifacts.clear();
    ci.check_conclusion = "failure";
    let observed = ci.observe().await;
    let evidence = observed.observed_evidence().expect("observed failure");
    assert_eq!(
        evidence.verdict(),
        CiVerdict::Failed {
            contexts: vec!["CI required".into()]
        }
    );
    assert!(evidence.tests().is_none());
    assert!(matches!(&evidence.provenance,
        TestProvenance::Unverified { reason } if reason.contains("nextest-plan")));
}

#[tokio::test]
async fn successful_check_without_required_provenance_never_passes() {
    let mut no_plan = Ci::merge_ref();
    no_plan.plan = false;
    let mut unrelated = Ci::merge_ref();
    unrelated.parents = vec![sha('b'), sha('d')];
    let mut no_worker = Ci::merge_ref();
    no_worker.artifacts.retain(|(w, _, _)| *w != 1);
    for (ci, gap) in [
        (no_plan, "nextest-plan"),
        (unrelated, "neither PR head"),
        (no_worker, "nextest-worker-1-attempt-1"),
    ] {
        assert!(reason(&ci.observe().await).contains(gap), "{gap}");
    }

    // Successful checks without verified tests are never judged a pass.
    let observed = Ci::merge_ref().observe().await;
    let mut evidence = observed.observed_evidence().expect("observed").clone();
    evidence.provenance = TestProvenance::Unverified {
        reason: "plan missing".into(),
    };
    assert!(matches!(evidence.verdict(), CiVerdict::Incomplete { .. }));
}

#[tokio::test]
async fn failed_required_check_outranks_a_missing_or_running_one() {
    // A required context that has no check yet or is still running must not hide a definite failure.
    for others in [vec![], vec![("A required", "in_progress", None)]] {
        let mut ci = Ci::merge_ref();
        ci.required = Ok(rules(&["A required", "CI required"]));
        ci.plan = false;
        ci.artifacts.clear();
        ci.check_conclusion = "failure";
        ci.other_checks = others.clone();
        let observed = ci.observe().await;
        let evidence = observed.observed_evidence().expect("observed failure");
        assert_eq!(
            evidence.required.contexts(),
            Ok(vec!["A required".to_owned(), "CI required".to_owned()]),
            "{others:?}"
        );
        let earlier = evidence.checks.iter().find(|c| c.context == "A required");
        if others.is_empty() {
            assert!(earlier.is_none(), "{others:?}");
        } else {
            let earlier = earlier.expect("running required check");
            assert_eq!(earlier.status, "in_progress");
            assert_eq!(earlier.conclusion, None);
        }
        assert_eq!(
            evidence.verdict(),
            CiVerdict::Failed {
                contexts: vec!["CI required".into()]
            },
            "{others:?}"
        );
        assert!(matches!(
            evidence.provenance,
            TestProvenance::Unverified { .. }
        ));
    }
}
