use serde_json::json;

use super::super::tests::{FakeGh, SLUG, at, pr_json, sha};
use super::super::{PrObservation, parse_pr};
use super::*;
use crate::services::campaign_execution::model::Repository;

const RUN: u64 = 900;
const RULES: &str = "repos/itismyfield/AgentDesk/rules/branches/main?per_page=100";

fn pr() -> PrObservation {
    parse_pr(Repository::Adk, SLUG, 77, &pr_json(&sha('a'), None)).unwrap()
}

fn meta(worker: u32, attempt: u32, tested: &str) -> Value {
    json!({"version": 1, "worker": worker, "run_attempt": attempt.to_string(), "plan_digest": "pd",
        "sha": tested, "run_id": RUN.to_string(), "archive_sha256": "x"})
}

struct Ci {
    tested: String,
    parents: Vec<String>,
    run_head: String,
    run_attempt: u32,
    // (worker, attempt its job reports in the current run attempt)
    jobs: Vec<(u32, u32)>,
    // (worker, attempt, worker.json)
    artifacts: Vec<(u32, u32, Value)>,
    required: Result<Value, String>,
    check_conclusion: &'static str,
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
            required: Ok(json!([{"type": "required_status_checks",
                "parameters": {"required_status_checks": [{"context": "CI required"}]}}])),
            check_conclusion: "success",
            tested,
        }
    }

    fn fake(&self) -> FakeGh {
        let runs = format!("repos/{SLUG}/actions/runs/{RUN}");
        let mut artifacts = vec![json!({"id": 1, "name": "nextest-plan", "expired": false})];
        let mut gh = FakeGh::default()
            .with(
                &runs,
                json!({"id": RUN, "repository": {"full_name": SLUG}, "head_sha": self.run_head,
                    "status": "completed", "conclusion": "success", "run_attempt": self.run_attempt,
                    "path": ".github/workflows/ci-pr.yml", "name": "CI PR", "event": "pull_request"}),
            )
            .file(
                "nextest-plan",
                "plan/plan.json",
                json!({"identity": {"sha": self.tested, "run_id": RUN.to_string()},
                    "plan_digest": "pd", "workers": [{}, {}]}),
            )
            .with(
                &format!("repos/{SLUG}/commits/{}", self.tested),
                json!({"parents": self.parents.iter().map(|p| json!({"sha": p})).collect::<Vec<_>>()}),
            )
            .with(
                &format!("{runs}/attempts/{}/jobs?per_page=100", self.run_attempt),
                json!({"total_count": self.jobs.len(), "jobs": self.jobs.iter().map(|(w, a)|
                    json!({"name": format!("Rust tests (worker {w})"), "run_attempt": a})).collect::<Vec<_>>()}),
            )
            .with(
                &format!("repos/{SLUG}/commits/{}/check-runs?per_page=100", sha('a')),
                json!({"total_count": 2, "check_runs": [
                    {"id": 5, "name": "CI required", "status": "completed", "conclusion": "failure",
                     "head_sha": sha('a'), "details_url": format!("https://github.com/{SLUG}/actions/runs/{RUN}/job/5")},
                    {"id": 6, "name": "CI required", "status": "completed", "conclusion": self.check_conclusion,
                     "head_sha": sha('a'), "details_url": format!("https://github.com/{SLUG}/actions/runs/{RUN}/job/6")}]}),
            );
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
        match &self.required {
            Ok(rules) => gh.with(RULES, rules.clone()),
            Err(error) => gh.failing(RULES, error),
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
    assert_eq!(ci.mode, CiMode::MergeRef);
    assert_eq!(ci.tested_sha, sha('c'));
    assert_eq!(ci.tested_base, Some(sha('b')));
    assert_eq!(ci.required, vec!["CI required".to_owned()]);
    assert_eq!(ci.verdict(), CiVerdict::Passed);
    assert!(ci.binds_head(&sha('a')));
    assert!(ci.workers.iter().all(|w| !w.inherited && w.attempt == 1));
    assert_eq!(ci.workers[1].artifact_digest.as_deref(), Some("sha256:11"));

    let mut head = Ci::merge_ref();
    head.tested = sha('a');
    head.artifacts = vec![(0, 1, meta(0, 1, &sha('a'))), (1, 1, meta(1, 1, &sha('a')))];
    let observed = head.observe().await;
    let ci = observed.observed_evidence().expect("observed");
    assert_eq!((ci.mode, ci.tested_base.clone()), (CiMode::Head, None));
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
    let evidence = observed.observed_evidence().expect("observed");
    assert_eq!(
        evidence
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

    let mut empty = Ci::merge_ref();
    empty.required = Ok(json!([]));
    let observed = empty.observe().await;
    let evidence = observed.observed_evidence().expect("observed");
    assert!(evidence.required.is_empty());
    assert!(matches!(evidence.verdict(), CiVerdict::Incomplete { .. }));
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
