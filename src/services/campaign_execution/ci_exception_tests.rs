use super::*;
use crate::services::campaign_execution::adk_observe::ci::{
    CheckResult, CiMode, CompletedJob, RequiredSet, TestProvenance, VerifiedTests, WorkerAdoption,
    WorkflowIdentity,
};

fn pair() -> (CiObservation, RunnerException) {
    let current_head = "a".repeat(40);
    let prior_head = "b".repeat(40);
    let workflow = WorkflowIdentity {
        path: ".github/workflows/ci-pr.yml".into(),
        name: "CI PR".into(),
        event: "pull_request".into(),
    };
    let ci = CiObservation {
        repository_identity: "owner/project".into(),
        pr_number: 1,
        pr_head_sha: current_head.clone(),
        pr_base_ref: "main".into(),
        workflow: workflow.clone(),
        run_id: 4,
        run_attempt: 2,
        run_conclusion: Some("failure".into()),
        required: RequiredSet {
            ruleset: Some(vec!["Lint".into()]),
            classic: Some(Vec::new()),
            profile: Vec::new(),
        },
        checks: vec![CheckResult {
            context: "Lint".into(),
            check_run_id: 11,
            status: "completed".into(),
            conclusion: Some("failure".into()),
        }],
        provenance: TestProvenance::Verified(VerifiedTests {
            tested_sha: current_head.clone(),
            tested_base: None,
            mode: CiMode::Head,
            plan_digest: "plan".into(),
            workers: vec![WorkerAdoption {
                worker: 0,
                attempt: 2,
                artifact_id: 1,
                artifact_digest: None,
                inherited: false,
            }],
        }),
    };
    let witness = |run_id, run_attempt, job_id, head_sha| JobInputWitness {
        run_id,
        run_attempt,
        job_id,
        head_sha,
        digest: "a".repeat(64),
        reference: "input receipt".into(),
    };
    let batch = |head, run_id, run_attempt, id, conclusion: &str| CompletedJobs {
        repository: "owner/project".into(),
        workflow: workflow.clone(),
        head,
        run_id,
        run_attempt,
        jobs: vec![CompletedJob {
            id,
            check_run_id: 11,
            name: "Lint".into(),
            conclusion: conclusion.into(),
        }],
    };
    let evidence = RunnerException {
        current: batch(current_head.clone(), 4, 2, 21, "failure"),
        losses: vec![RunnerLoss {
            failed_job: 21,
            diagnostic: "lost communication with the server".into(),
            log_ref: "https://github.com/owner/project/actions/runs/4/job/21".into(),
            failed_inputs: witness(4, 2, 21, current_head),
            prior: batch(prior_head.clone(), 3, 1, 20, "success"),
            successful_job: 20,
            successful_inputs: witness(3, 1, 20, prior_head),
        }],
    };
    (ci, evidence)
}

#[test]
fn m7_runner_exception_requires_equivalent_job_inputs() {
    let (ci, mut evidence) = pair();
    assert_eq!(runner_exception(&ci, &evidence).unwrap()[0].run_id, 3);
    evidence.losses[0].successful_inputs.digest = "b".repeat(64);
    assert!(runner_exception(&ci, &evidence).is_none());
}

#[test]
fn m7_runner_exception_needs_every_failure_and_a_different_run() {
    let (mut ci, mut evidence) = pair();
    let mut second = evidence.current.jobs[0].clone();
    second.id = 22;
    evidence.current.jobs.push(second);
    assert!(runner_exception(&ci, &evidence).is_none());
    evidence.current.jobs.pop();

    let verified = std::mem::replace(
        &mut ci.provenance,
        TestProvenance::Unverified {
            reason: "another required artifact is missing".into(),
        },
    );
    assert!(runner_exception(&ci, &evidence).is_none());
    ci.provenance = verified;
    ci.required.profile.push("missing".into());
    assert!(runner_exception(&ci, &evidence).is_none());
    ci.required.profile.clear();
    evidence.losses[0].diagnostic = "test assertion failed".into();
    assert!(runner_exception(&ci, &evidence).is_none());
    evidence.losses[0].diagnostic = "lost communication with the server".into();

    evidence.losses[0].prior.run_id = ci.run_id;
    evidence.losses[0].successful_inputs.run_id = ci.run_id;
    assert!(runner_exception(&ci, &evidence).is_none());
}

#[tokio::test]
async fn m7_job_observation_refuses_partial_attempts() {
    use crate::services::campaign_execution::adk_observe::{
        ci::observe_completed_jobs, tests::FakeGh,
    };
    use serde_json::json;

    let slug = "owner/project";
    let path = format!("repos/{slug}/actions/runs/7");
    let run = json!({
        "id": 7, "repository": {"full_name": slug}, "path": ".github/workflows/ci-pr.yml",
        "name": "CI PR", "event": "pull_request", "head_sha": "a".repeat(40),
        "status": "completed", "run_attempt": 3
    });
    let jobs = format!("{path}/attempts/3/jobs?per_page=100");
    let job = json!({
        "id": 8, "check_run_url": format!("https://api.github.com/repos/{slug}/check-runs/9"),
        "name": "Lint", "status": "completed", "conclusion": "success", "run_id": 7, "head_sha": "a".repeat(40)
    });
    let gh = FakeGh::default()
        .with(&path, run.clone())
        .with(&jobs, json!({"total_count": 1, "jobs": [job.clone()]}));
    let observed = observe_completed_jobs(&gh, slug, 7).await.unwrap();
    assert_eq!(observed.run_attempt, 3);
    assert_eq!(observed.job(8).unwrap().check_run_id, 9);

    let gh = FakeGh::default()
        .with(&path, run)
        .with(&jobs, json!({"total_count": 2, "jobs": [job]}));
    assert!(observe_completed_jobs(&gh, slug, 7).await.is_err());
}
