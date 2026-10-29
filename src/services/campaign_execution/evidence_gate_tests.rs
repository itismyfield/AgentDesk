use super::*;
use crate::services::campaign_execution::{
    adk_workspace::WorkspaceObservation,
    candidate_seal::CandidateSeal,
    control::tests as domain,
    definition::{AcceptanceCondition, VerificationKind},
    evidence::{EvidenceEnvelope, TuiPayload, TuiSubmission},
    model::*,
    profile::*,
    verification_run::*,
};

pub(crate) const OUTPUT: &str = "output-h1";
pub(crate) const REPO: &str = "github.com/itismyfield/AgentDesk";
const CHECK: &str = "targeted-tests";

pub(crate) fn workspace(head: &str) -> WorkspaceObservation {
    WorkspaceObservation {
        repository: Repository::Adk,
        repository_identity: REPO.into(),
        host: "mac".into(),
        path: "/verify".into(),
        head_sha: head.into(),
        tracked_dirty: false,
        untracked_dirty: false,
        observed_at: domain::now(),
    }
}

pub(crate) fn claim(binding: &AttemptBinding, output: &str) -> EvidenceEnvelope {
    let payload = TuiPayload::OutputClaim {
        output_sha: output.into(),
    };
    let submission = TuiSubmission {
        evidence_id: "output".into(),
        payload,
    };
    EvidenceEnvelope::from_tui(submission, binding, "pin", domain::now()).unwrap()
}

pub(crate) fn seal(binding: &AttemptBinding) -> CandidateSeal {
    let claim = claim(binding, OUTPUT);
    CandidateSeal::seal(
        binding,
        &claim,
        REPO,
        &workspace(OUTPUT),
        "pin",
        domain::now(),
    )
    .unwrap()
}

pub(crate) fn registered(binding: &AttemptBinding, cycle: u32) -> VerificationRun {
    let execution_id = execution_id(binding, cycle, CHECK);
    let phases: Vec<_> = [("archive", PhaseKind::Archive), ("test", PhaseKind::Test)]
        .into_iter()
        .map(|(id, kind)| VerificationPhase {
            id: id.into(),
            action_id: phase_action_id(&binding.run_id, &execution_id, id),
            kind,
            target: binding.session.gate.clone(),
            requires: Vec::new(),
            input: Slot::Unbound,
            initiations: Vec::new(),
            result: None,
        })
        .collect();
    VerificationRun {
        origin: VerificationOrigin {
            execution_id,
            campaign_id: binding.campaign_id.clone(),
            run_id: binding.run_id.clone(),
            node_id: binding.node_id.clone(),
            series: binding.series,
            repair_cycle: cycle,
            owner_attempt: binding.attempt_id.clone(),
            owner_binding: binding.clone(),
            binding_digest: digest_of(binding).unwrap(),
            registered_node_epoch: 1,
            stage: binding.stage,
            pin_hash: "pin".into(),
            spec_hash: "spec".into(),
            check_id: CHECK.into(),
            gate: binding.session.gate.clone(),
            phase_ids: phases.iter().map(|p| p.id.clone()).collect(),
            registered_at: domain::now(),
        },
        subject: Slot::Unbound,
        plan: Slot::Unbound,
        phases,
        collection: None,
        cancellation: None,
    }
}

pub(crate) fn actions(run: &VerificationRun, state: ActionState) -> BTreeMap<String, Action> {
    run.phases
        .iter()
        .map(|p| {
            let action = Action {
                id: p.action_id.clone(),
                binding: run.origin.owner_binding.clone(),
                kind: ActionKind::VerificationPhase,
                target: p.target.clone(),
                state,
                initiation_receipt: None,
                reconciliation: None,
            };
            (p.action_id.clone(), action)
        })
        .collect()
}

fn planned() -> Vec<PlannedTest> {
    [("t1", true), ("t2", false)]
        .map(|(id, required)| PlannedTest {
            id: id.into(),
            check_id: CHECK.into(),
            required,
        })
        .into()
}

// A V of the sealed output with its actual plan bound and the given executed tests collected.
pub(crate) fn collected(
    binding: &AttemptBinding,
    seal: &CandidateSeal,
    outcomes: &[(&str, TestOutcome)],
    exit_code: Option<i32>,
) -> VerificationRun {
    let mut run = registered(binding, 0);
    run.subject.bind(seal.bound_subject()).unwrap();
    let manifest = planned();
    let actual_plan = ActualPlan {
        archive_digest: "archive".into(),
        manifest: manifest.clone(),
        plan_digest: "plan".into(),
        plan_artifact_digest: "plan-file".into(),
        config_digest: "config".into(),
    };
    run.plan.bind(actual_plan).unwrap();
    let actual: Vec<_> = outcomes
        .iter()
        .map(|(id, outcome)| ExecutedTest {
            id: (*id).into(),
            check_id: CHECK.into(),
            outcome: outcome.clone(),
            log_refs: vec!["junit".into()],
        })
        .collect();
    let audit = ExecutionAudit {
        planned: manifest.len(),
        executed: actual.len(),
        failed: Vec::new(),
        skipped: Vec::new(),
        missing: Vec::new(),
        duplicate: Vec::new(),
        extra: Vec::new(),
        exit_code,
        audit_exit_code: None,
        complete: true,
        all_passed: true,
    };
    let evidence = TestEvidence {
        execution_id: run.origin.execution_id.clone(),
        binding: binding.clone(),
        environment: ExecutionEnvironment {
            os: "macos".into(),
            checkout: workspace(OUTPUT).checkout(),
            components: Vec::new(),
            allocations: Vec::new(),
        },
        planned: manifest,
        actual,
        log_refs: vec!["nextest".into()],
    };
    run.collection = Some(VerificationCollection::Executed {
        evidence: Box::new(evidence),
        audit,
    });
    run
}

pub(crate) fn plan(binding: &AttemptBinding) -> VerificationPlan {
    VerificationPlan {
        candidate: Candidate {
            repository: binding.repository,
            base_sha: binding.base_sha.clone(),
            head_sha: OUTPUT.into(),
        },
        stage: binding.stage,
        acceptance: vec![AcceptanceCondition {
            id: "ac-a".into(),
            description: "Prove A".into(),
            verification: VerificationKind::Test,
        }],
        checks: vec![VerificationCheck {
            id: CHECK.into(),
            kind: CheckKind::Test,
            required: true,
            acceptance_ids: vec!["ac-a".into()],
        }],
        environment_requirements: Vec::new(),
    }
}

pub(crate) fn assess(run: &VerificationRun) -> StageAssessment {
    let binding = &run.origin.owner_binding;
    assess_verification(run, &actions(run, ActionState::Reconciled), &plan(binding))
}

fn failed() -> TestOutcome {
    TestOutcome::Failed
}

#[test]
fn complete_failed_run_is_valid_evidence_but_a_quality_failure() {
    let binding = domain::binding(&domain::running());
    let seal = seal(&binding);
    let run = collected(
        &binding,
        &seal,
        &[("t1", failed()), ("t2", TestOutcome::Passed)],
        Some(0),
    );
    let assessment = assess(&run);
    assert_eq!(assessment.verdict, Verdict::QualityFail);
    assert_eq!(
        assessment.reasons,
        vec![GateReason::TestFailed("t1".into())]
    );
    assert!(assessment.adopted.is_empty());
    // An optional test failing does not block a complete required pass.
    let run = collected(
        &binding,
        &seal,
        &[("t1", TestOutcome::Passed), ("t2", failed())],
        Some(0),
    );
    assert_eq!(assess(&run).verdict, Verdict::Pass);
    assert_eq!(assess(&run).adopted, vec![run.origin.execution_id.clone()]);
}

#[test]
fn zero_missing_skipped_drifted_or_exit_mismatched_runs_never_pass() {
    let binding = domain::binding(&domain::running());
    let seal = seal(&binding);
    let passed = [("t1", TestOutcome::Passed)];
    let rejected = |run: &VerificationRun| {
        let assessment = assess(run);
        assert_eq!(assessment.verdict, Verdict::Reject, "{assessment:?}");
        assessment.reasons
    };
    rejected(&collected(&binding, &seal, &[], Some(0)));
    rejected(&collected(
        &binding,
        &seal,
        &[("t2", TestOutcome::Passed)],
        Some(0),
    ));
    let skipped = TestOutcome::Skipped {
        reason: "filtered".into(),
    };
    rejected(&collected(&binding, &seal, &[("t1", skipped)], Some(0)));
    let reasons = rejected(&collected(&binding, &seal, &passed, Some(1)));
    assert!(matches!(reasons[..], [GateReason::ExitMismatch(_)]));
    let mut drifted = collected(&binding, &seal, &passed, Some(0));
    if let Some(VerificationCollection::Executed { evidence, .. }) = &mut drifted.collection {
        evidence.planned.retain(|t| t.required);
        evidence.planned[0].id = "old-list-only".into();
    }
    assert!(matches!(
        rejected(&drifted)[..],
        [GateReason::PlanMismatch(_)]
    ));
    let mut incomplete = collected(&binding, &seal, &passed, Some(0));
    if let Some(VerificationCollection::Executed { audit, .. }) = &mut incomplete.collection {
        audit.complete = false;
    }
    rejected(&incomplete);
    assert_eq!(
        assess(&collected(&binding, &seal, &passed, Some(0))).verdict,
        Verdict::Pass
    );
}

#[test]
fn waits_and_unclassified_preparation_failures_are_not_quality_failures() {
    let binding = domain::binding(&domain::running());
    let mut run = registered(&binding, 0);
    let resource = super::super::adk_resources::resource(ResourceKind::Build, "mac", "token");
    run.phases[0].initiations.push(PhaseInitiation::NotSent {
        reason: NotSentReason::ResourceWait { resource },
        evidence: "build token timeout 75".into(),
        observed_at: domain::now(),
    });
    let wait = assess(&run);
    assert_eq!(wait.verdict, Verdict::Wait);
    assert_eq!(wait.reasons, vec![GateReason::ResourceWait("token".into())]);
    let unknown = assess_verification(&run, &actions(&run, ActionState::Unknown), &plan(&binding));
    assert_eq!(
        unknown.reasons,
        vec![GateReason::ObservationUnknown("archive".into())]
    );
    let record = |exit_code, signal| CommandRecord {
        executable: "cargo".into(),
        cwd: "/verify".into(),
        argv: Vec::new(),
        env_identity: BTreeMap::new(),
        phase_id: "archive".into(),
        started_at: domain::now(),
        finished_at: domain::now(),
        exit_code,
        signal,
        stdout_ref: "out".into(),
        stderr_ref: "err".into(),
    };
    let preparation = |exit_code, signal| {
        let mut failed = registered(&binding, 0);
        failed.collection = Some(VerificationCollection::PreparationFailed {
            phase_id: "archive".into(),
            record: record(exit_code, signal),
        });
        assess(&failed).verdict
    };
    assert_eq!(preparation(Some(101), None), Verdict::QualityFail);
    assert_eq!(preparation(Some(75), None), Verdict::Reject);
    assert_eq!(preparation(Some(69), None), Verdict::Reject);
    assert_eq!(preparation(None, Some(9)), Verdict::Reject);
    run.cancellation = Some(Cancellation {
        evidence: "registered phases never started".into(),
        cancelled_at: domain::now(),
    });
    assert_eq!(assess(&run).verdict, Verdict::Wait);
}

#[test]
fn stage_gate_owes_every_required_check_and_strictest_part_decides() {
    let pass = StageAssessment::pass(vec!["v1".into()]);
    let wait = StageAssessment::of(Verdict::Wait, GateReason::Pending("v2".into()));
    let required = vec!["c1".to_string(), "c2".to_string()];
    let only_first = BTreeMap::from([("c1".to_string(), pass.clone())]);
    let missing = evaluate_stage(&required, &only_first);
    assert_eq!(missing.verdict, Verdict::Wait);
    assert_eq!(missing.reasons, vec![GateReason::MissingCheck("c2".into())]);
    assert!(missing.adopted.is_empty());
    assert_eq!(evaluate_stage(&[], &only_first).verdict, Verdict::Reject);
    let both = BTreeMap::from([("c1".to_string(), pass.clone()), ("c2".to_string(), wait)]);
    assert_eq!(evaluate_stage(&required, &both).verdict, Verdict::Wait);
    let failed = StageAssessment::of(Verdict::QualityFail, GateReason::TestFailed("t".into()));
    let reject = StageAssessment::of(Verdict::Reject, GateReason::Incomplete("x".into()));
    assert_eq!(
        StageAssessment::combine([reject, failed, pass.clone()]).verdict,
        Verdict::QualityFail
    );
    let all = BTreeMap::from([("c1".to_string(), pass.clone()), ("c2".to_string(), pass)]);
    assert_eq!(evaluate_stage(&required, &all).adopted, vec!["v1", "v1"]);
}

// Drives A1's own V lifecycle for a sealed H1 of A(H0): fixed phases, plan binding and collect().
fn a1_collected(
    outcomes: &[(&str, TestOutcome)],
    test_exit: i32,
    audit_exit: i32,
    checkout_sha: &str,
) -> (VerificationRun, BTreeMap<String, Action>) {
    use crate::services::campaign_execution::verification_run::tests as a1;
    let binding = a1::binding();
    let claim = claim(&binding, "h1");
    let seal = CandidateSeal::seal(&binding, &claim, REPO, &workspace("h1"), "pin", a1::at());
    let (mut run, mut actions) = a1::registered_with(a1::spec_with_audit());
    run.subject.bind(seal.unwrap().bound_subject()).unwrap();
    let digest = |run: &VerificationRun, phase: &str| {
        digest_of(&run.phase(phase).unwrap().result.as_ref().unwrap().output).unwrap()
    };
    let tests: Vec<_> = outcomes
        .iter()
        .map(|(name, _)| ListedTest {
            binary_id: "agentdesk".into(),
            name: (*name).into(),
            ignored: false,
        })
        .collect();
    let steps = [
        (
            "archive",
            0,
            PhaseOutput::Archive {
                digest: "archive-digest".into(),
            },
        ),
        (
            "list",
            0,
            PhaseOutput::Universe {
                tests: tests.clone(),
            },
        ),
        (
            "plan",
            0,
            PhaseOutput::Plan {
                plan_digest: "plan-digest".into(),
                plan_artifact_digest: "plan-file-digest".into(),
                config_digest: "config-digest".into(),
                selected: tests,
            },
        ),
        (
            "test",
            test_exit,
            PhaseOutput::Tests {
                executed: outcomes
                    .iter()
                    .map(|(name, outcome)| ExecutedTest {
                        id: format!("agentdesk {name}"),
                        check_id: "lib-tests".into(),
                        outcome: outcome.clone(),
                        log_refs: vec![],
                    })
                    .collect(),
            },
        ),
        ("audit", audit_exit, PhaseOutput::None),
    ];
    for (phase, exit, output) in steps {
        let requires = run.phase(phase).unwrap().requires.clone();
        let mut input = a1::input(&[]);
        input.predecessors = requires
            .iter()
            .map(|r| (r.clone(), digest(&run, r)))
            .collect();
        if phase == "test" {
            run.bind_plan(&a1::spec_with_audit()).unwrap();
            for (name, digest) in [
                ("plan", "plan-file-digest"),
                ("archive", "archive-digest"),
                ("config", "config-digest"),
            ] {
                input.preflight.push(ArtifactRef {
                    name: name.into(),
                    path: format!("/scratch/{name}"),
                    digest: digest.into(),
                });
            }
        }
        run.bind_phase_input(phase, input).unwrap();
        let result = a1::result(phase, exit, output);
        let action_id = run.phase(phase).unwrap().action_id.clone();
        let action = actions.get_mut(&action_id).unwrap();
        action.state = ActionState::Started;
        action.initiation_receipt = Some(result.receipt.clone());
        let started = PhaseInitiation::Started {
            receipt: result.receipt.clone(),
            process: ProcessIdentity {
                host: "macbook".into(),
                pid: 7,
                process_group: Some(7),
                started_at: a1::at(),
                nonce: "nonce".into(),
            },
        };
        run.record_initiation(phase, started).unwrap();
        run.record_result(phase, &actions[&action_id].clone(), result)
            .unwrap();
        actions.get_mut(&action_id).unwrap().state = ActionState::Reconciled;
    }
    let mut environment = a1::environment();
    environment.checkout.observed_sha = checkout_sha.into();
    let collection = run
        .collect(|id| actions.get(id), environment, vec!["nextest".into()])
        .unwrap();
    run.collection = Some(collection);
    (run, actions)
}

#[test]
fn a1_collected_sealed_output_is_judged_against_the_seal_not_the_input_binding() {
    let judge = |(run, actions): (VerificationRun, BTreeMap<String, Action>)| {
        let mut plan = plan(&run.origin.owner_binding);
        plan.candidate.head_sha = "h1".into();
        plan.checks[0].id = "lib-tests".into();
        plan.checks[0].acceptance_ids.clear();
        plan.acceptance.clear();
        assess_verification(&run, &actions, &plan)
    };
    let passed = [("a", TestOutcome::Passed), ("b", TestOutcome::Passed)];
    let (run, actions) = a1_collected(&passed, 0, 0, "h1");
    assert_eq!(run.origin.owner_binding.candidate_sha, "h0");
    let pass = judge((run, actions));
    assert_eq!(pass.verdict, Verdict::Pass, "{pass:?}");
    let failed = [("a", TestOutcome::Passed), ("b", TestOutcome::Failed)];
    let quality = judge(a1_collected(&failed, 100, 1, "h1"));
    assert_eq!(quality.verdict, Verdict::QualityFail);
    // Every test passed but the audit phase disagreed: not a PASS.
    let audit = judge(a1_collected(&passed, 0, 1, "h1"));
    assert!(
        matches!(audit.reasons[..], [GateReason::ExitMismatch(_)]),
        "{audit:?}"
    );
    let moved = judge(a1_collected(&passed, 0, 0, "editing-h2"));
    assert_eq!(moved.verdict, Verdict::Reject);
}
