use chrono::TimeDelta;
use serde_json::json;

use super::*;
use crate::domain::campaign::{
    decision_item::{ApprovalTarget, ForumApproval},
    definition::validate_definition,
    landing::{
        LandingPreflight, LandingTarget, MergeJob, MergeReceipt, PreflightMode, TlcEvidence,
        TlcOutcome,
    },
    model::ReviewedCandidate,
    profile::{Candidate, IntegrationMethod, NodeArtifact, ReviewExtent, ReviewRange},
};
use crate::services::campaign_execution::control::{self, tests as domain};

pub(crate) fn sha(c: char) -> String {
    c.to_string().repeat(40)
}

// Node `a` after this run's PR was merged and witnessed on main. Its conditions need the test
// evidence, main, a deployment and one operational check.
pub(crate) fn landed_on(
    host: &HostGate,
    base: &str,
    head: &str,
    merge: &str,
    deploy_gate: bool,
) -> CampaignState {
    let mut input = domain::input();
    input.workflow_options.as_mut().unwrap().deploy = deploy_gate;
    input.nodes[0].acceptance = serde_json::from_value(json!([
        {"id": "ac-test", "description": "Tests pass", "verification": "test"},
        {"id": "ac-main", "description": "Main has it", "verification": "main"},
        {"id": "ac-deploy", "description": "Hosts run it", "verification": "deployment"},
        {"id": "ac-live", "description": "It works live", "verification": "operational"}
    ]))
    .unwrap();
    let now = domain::now();
    let mut state = domain::running();
    state.definition = validate_definition(input).unwrap();
    let run = state.run.as_mut().unwrap();
    let attempt = run.attempts.get_mut("attempt").unwrap();
    attempt.binding.hashes = state.definition.hashes()["a"].clone();
    attempt.binding.base_sha = base.into();
    attempt.outcome = Some(AttemptOutcome::Passed);
    let work = attempt.binding.clone();
    let pr = OpenedPr {
        repository: "owner/project".into(),
        number: 42,
        head_sha: head.into(),
        head_ref: "campaign/a".into(),
        base_ref: "main".into(),
    };
    let receipt = MergeReceipt {
        artifact: NodeArtifact {
            node_id: "a".into(),
            candidate: Candidate {
                repository: Repository::Adk,
                base_sha: base.into(),
                head_sha: head.into(),
            },
            hashes: work.hashes.clone(),
            review: ReviewRange {
                base_sha: base.into(),
                head_sha: head.into(),
                extent: ReviewExtent::Full,
            },
        },
        pr_number: pr.number,
        merge_sha: merge.into(),
        ci_run_id: 130,
        ci_run_attempt: 1,
        tlc: TlcEvidence {
            head_sha: head.into(),
            started_at: now - TimeDelta::minutes(1),
            finished_at: now,
            reference: "tlc receipt".into(),
            outcome: TlcOutcome::Passed {
                selected: 1,
                passed: 1,
            },
        },
        review_refs: vec!["review-evidence".into()],
        runner_losses: Vec::new(),
    };
    for (id, kind, receipt) in [
        (
            "opening",
            ActionKind::CreatePr,
            serde_json::to_string(&pr).unwrap(),
        ),
        (
            "merge:opening",
            ActionKind::Merge,
            serde_json::to_string(&receipt).unwrap(),
        ),
    ] {
        run.actions.insert(
            id.into(),
            Action {
                id: id.into(),
                binding: work.clone(),
                kind,
                target: host.clone(),
                state: ActionState::Reconciled,
                initiation_receipt: Some(receipt),
                reconciliation: None,
            },
        );
    }
    run.landings.insert(
        "a".into(),
        Landing {
            target: LandingTarget {
                repository: pr.repository.clone(),
                head_sha: head.into(),
                head_ref: pr.head_ref.clone(),
                base_ref: pr.base_ref.clone(),
                pin_hash: "pin".into(),
            },
            epoch: 1,
            control_version: 1,
            host: host.clone(),
            action_id: "opening".into(),
            deadline: now,
            preflight: Some(LandingPreflight {
                head_sha: head.into(),
                mode: PreflightMode::PreTlc,
                transcript_digest: "fixture".into(),
            }),
            status: LandingStatus::CiHold { pr },
        },
    );
    run.merges.insert(
        "a".into(),
        MergeJob {
            deadline: now,
            status: MergeStatus::Adopted(Box::new(MainIntegrationEvidence {
                repository: Repository::Adk,
                target_branch: "main".into(),
                target_main_sha: merge.into(),
                pr_number: 42,
                merge_sha: merge.into(),
                method: IntegrationMethod::Squash,
                nodes: Vec::new(),
            })),
        },
    );
    let fence = run.fences.entry("a".into()).or_default();
    fence.epoch = 2;
    fence.candidate = Some(ReviewedCandidate {
        work_attempt: work.attempt_id,
        base_sha: base.into(),
        head_sha: head.into(),
        pin_hash: "pin".into(),
    });
    let node = state.nodes.get_mut("a").unwrap();
    node.stage = Stage::MainVerification;
    node.disposition = NodeDisposition::Verifying;
    node.current_attempt = None;
    node.main_integrated = true;
    state
}

pub(crate) fn landed(deploy_gate: bool) -> CampaignState {
    let host = domain::binding(&domain::running()).session.gate;
    landed_on(&host, &sha('b'), &sha('a'), &sha('c'), deploy_gate)
}

pub(crate) fn hosts() -> Vec<DeployHost> {
    ["mac-mini", "mac-peer"]
        .map(|host| DeployHost {
            host: host.into(),
            previous_incarnation: "boot-before".into(),
            expected_binary_digest: None,
        })
        .to_vec()
}

pub(crate) fn record(state: &CampaignState, fact: DeployFact) -> Result<CampaignState, Rejection> {
    let mut envelope = domain::envelope(Command::RecordDeployment {
        node_id: "a".into(),
        fact: Box::new(fact),
    });
    envelope.idempotency_key = format!("key-{}", state.event_sequence);
    control::decide(state, &envelope).map(|decision| decision.state)
}

pub(crate) fn request(target: &str) -> DeployFact {
    DeployFact::Request {
        target_sha: target.into(),
        hosts: hosts(),
    }
}

pub(crate) fn witnesses(target: &str) -> Vec<DeployWitness> {
    let now = domain::now();
    hosts()
        .into_iter()
        .map(|host| DeployWitness {
            host: host.host,
            source_sha: target.into(),
            boot_incarnation: "boot-after".into(),
            binary_identity: None,
            boot_started_at_ms: now.timestamp_millis() + 1,
            observed_at: now,
        })
        .collect()
}

// The deployment of `c` requested, started and witnessed on both hosts.
pub(crate) fn deployed() -> CampaignState {
    let state = record(&landed(false), request(&sha('c'))).unwrap();
    let started = DeployFact::Started {
        receipt: "deploy receipt".into(),
    };
    let state = record(&state, started).unwrap();
    let witnesses = witnesses(&sha('c'));
    record(&state, DeployFact::Confirmed { witnesses }).unwrap()
}

fn permit_for(state: &CampaignState, main: &str) -> Permit {
    let run = state.run.as_ref().unwrap();
    let job = &run.deployments["a"];
    permit(state, run, "a", job, main, &hosts(), domain::now())
}

fn approved(state: &CampaignState, binding: Box<ApprovalBinding>) -> CampaignState {
    let opened = domain::step(
        state,
        Command::OpenApprovalGate {
            binding: binding.clone(),
        },
    );
    let evidence = Box::new(ForumApproval {
        decision_ref: "test forum".into(),
        response_ref: "https://example.com/answer".into(),
        response_at: domain::now(),
    });
    domain::step(&opened, Command::ApproveGate { binding, evidence })
}

fn action(state: &CampaignState) -> &Action {
    &state.run.as_ref().unwrap().actions["deploy:opening"]
}

#[test]
fn m8_no_deployment_starts_without_the_deploy_approval() {
    let main = sha('c');
    let state = record(&landed(true), request(&main)).unwrap();
    assert_eq!(action(&state).state, ActionState::StartPending);
    let Permit::Approve(binding) = permit_for(&state, &main) else {
        panic!("an unapproved deployment was permitted")
    };
    assert_eq!(binding.target, ApprovalTarget::Main { sha: main.clone() });
    let opened = domain::step(
        &state,
        Command::OpenApprovalGate {
            binding: binding.clone(),
        },
    );
    assert_eq!(permit_for(&opened, &main), Permit::Approve(binding.clone()));
    assert_eq!(permit_for(&approved(&state, binding), &main), Permit::Start);
    let ungated = record(&landed(false), request(&main)).unwrap();
    assert_eq!(permit_for(&ungated, &main), Permit::Start);
}

#[test]
fn m8_both_stops_revoke_an_unstarted_deployment() {
    let main = sha('c');
    let waiting = record(&landed(false), request(&main)).unwrap();
    assert_eq!(permit_for(&waiting, &main), Permit::Start);
    for mode in [StopMode::Immediate, StopMode::FinishStage] {
        let stopped = domain::stop(&waiting, mode);
        assert!(matches!(permit_for(&stopped, &main), Permit::Refuse(_)));
        let cancelled = DeployFact::Failed {
            reason: "deploy authority was revoked".into(),
            uncertain: false,
        };
        let cancelled = record(&stopped, cancelled).unwrap();
        assert_eq!(action(&cancelled).state, ActionState::Cancelled);
        // A stopped run requests no deployment either.
        let unrequested = domain::stop(&landed(false), mode);
        assert_eq!(
            record(&unrequested, request(&main)).unwrap_err().code,
            RejectionCode::InvalidState
        );
    }
    let mut late = waiting.clone();
    let deadline = late.run.as_ref().unwrap().deployments["a"].intent.deadline;
    let run = late.run.as_ref().unwrap();
    let job = &run.deployments["a"];
    assert_eq!(
        permit(&late, run, "a", job, &main, &hosts(), deadline),
        Permit::Refuse("deployment was not approved and started before its deadline")
    );
    late.run
        .as_mut()
        .unwrap()
        .fences
        .get_mut("a")
        .unwrap()
        .epoch += 1;
    assert!(matches!(permit_for(&late, &main), Permit::Refuse(_)));
}

#[test]
fn m8_a_head_that_moved_after_approval_needs_its_own_approval() {
    let (first, moved) = (sha('c'), sha('d'));
    let state = record(&landed(true), request(&first)).unwrap();
    let Permit::Approve(binding) = permit_for(&state, &first) else {
        panic!("missing gate")
    };
    let state = approved(&state, binding);
    assert_eq!(permit_for(&state, &first), Permit::Start);
    // The existing deployment path would now release another commit.
    assert_eq!(permit_for(&state, &moved), Permit::Retarget);
    let deadline = state.run.as_ref().unwrap().deployments["a"].intent.deadline;
    let state = record(&state, request(&moved)).unwrap();
    let job = &state.run.as_ref().unwrap().deployments["a"];
    assert_eq!(
        (&job.intent.target_sha, job.intent.deadline),
        (&moved, deadline)
    );
    let Permit::Approve(binding) = permit_for(&state, &moved) else {
        panic!("the approval of another commit started this deployment")
    };
    assert_eq!(binding.target, ApprovalTarget::Main { sha: moved.clone() });
    let state = approved(&state, binding);
    assert_eq!(permit_for(&state, &moved), Permit::Start);
    // A started deployment keeps its target.
    let started = DeployFact::Started {
        receipt: "deploy receipt".into(),
    };
    let state = record(&state, started).unwrap();
    assert_eq!(
        record(&state, request(&first)).unwrap_err().code,
        RejectionCode::InvalidState
    );
}

#[test]
fn m8_only_every_hosts_running_binary_confirms_a_started_deployment() {
    let main = sha('c');
    let waiting = record(&landed(false), request(&main)).unwrap();
    let confirmed = |witnesses| DeployFact::Confirmed { witnesses };
    // A deployment this run never started is not this run's deployment.
    assert!(record(&waiting, confirmed(witnesses(&main))).is_err());
    let started = DeployFact::Started {
        receipt: "deploy receipt".into(),
    };
    let lost = DeployFact::Indeterminate {
        evidence: "lost start reply".into(),
    };
    for begun in [started, lost] {
        let state = record(&waiting, begun).unwrap();
        let uncertain = action(&state).state == ActionState::Unknown;
        let mut other = witnesses(&main);
        other[1].source_sha = sha('d');
        let mut same_boot = witnesses(&main);
        same_boot[0].boot_incarnation = "boot-before".into();
        let mut old_boot = witnesses(&main);
        old_boot[0].boot_started_at_ms = domain::now().timestamp_millis();
        let mut one_host = witnesses(&main);
        one_host.pop();
        for refused in [other, same_boot, old_boot, one_host, Vec::new()] {
            assert!(record(&state, confirmed(refused)).is_err());
        }
        let failed = DeployFact::Failed {
            reason: "no witness before the deadline".into(),
            uncertain: false,
        };
        let failed = record(&state, failed).unwrap();
        let run = failed.run.as_ref().unwrap();
        assert_eq!(
            run.deployments["a"].status,
            DeployStatus::Failed {
                reason: "no witness before the deadline".into(),
                uncertain
            }
        );
        let settled = if uncertain {
            ActionState::Unknown
        } else {
            ActionState::Reconciled
        };
        assert_eq!(action(&failed).state, settled);
        assert_eq!(failed.nodes["a"].disposition, NodeDisposition::Blocked);
        assert!(record(&failed, confirmed(witnesses(&main))).is_err());

        let done = record(&state, confirmed(witnesses(&main))).unwrap();
        assert_eq!(action(&done).state, ActionState::Reconciled);
        assert_eq!(done.nodes["a"].stage, Stage::FinalVerification);
        assert!(!done.nodes["a"].acceptance_complete);
    }
}
