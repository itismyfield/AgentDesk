use std::collections::BTreeMap;

use super::super::{control::test_support as domain, verification_run::test_support as a1};
use crate::domain::campaign::{
    evidence::{EvidenceEnvelope, TuiPayload, TuiSubmission},
    model::{Action, ActionState, AttemptBinding, Repository},
    profile::{ExecutedTest, TestOutcome},
    verification_run::{
        ArtifactRef, ListedTest, PhaseInitiation, PhaseKind, PhaseOutput, ProcessIdentity,
        VerificationRun, VerificationSpec, digest_of, plan_scope,
    },
    workspace::WorkspaceObservation,
};

pub const REPO: &str = "github.com/itismyfield/AgentDesk";

pub fn workspace(head: &str) -> WorkspaceObservation {
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

pub fn claim(binding: &AttemptBinding, output: &str) -> EvidenceEnvelope {
    let payload = TuiPayload::OutputClaim {
        output_sha: output.into(),
    };
    let submission = TuiSubmission {
        evidence_id: "output".into(),
        payload,
    };
    EvidenceEnvelope::from_tui(submission, binding, "pin", domain::now()).unwrap()
}

// Runs every fixed phase of a registered V whose subject is bound, then stores collect()'s result.
pub fn a1_finish(
    run: &mut VerificationRun,
    actions: &mut BTreeMap<String, Action>,
    spec: &VerificationSpec,
    outcomes: &[(&str, TestOutcome)],
    exit: impl Fn(&str) -> i32,
    checkout_sha: &str,
) {
    let check_id = run.origin.check_id.clone();
    let check_id = check_id.as_str();
    let selection = spec.check(check_id).unwrap().selection;
    let tests: Vec<_> = outcomes
        .iter()
        .map(|(name, _)| ListedTest {
            binary_id: "agentdesk".into(),
            name: (*name).into(),
            ignored: false,
        })
        .collect();
    let phases: Vec<_> = run.phases.iter().map(|p| (p.id.clone(), p.kind)).collect();
    for (phase, kind) in phases {
        let output = match kind {
            PhaseKind::Archive => PhaseOutput::Archive {
                digest: "archive-digest".into(),
            },
            PhaseKind::List => PhaseOutput::Universe {
                tests: tests.clone(),
            },
            PhaseKind::Plan => PhaseOutput::Plan {
                plan_digest: "plan-digest".into(),
                plan_artifact_digest: "plan-file-digest".into(),
                config_digest: "config-digest".into(),
                scope: plan_scope(selection).into(),
                selected: tests.clone(),
            },
            PhaseKind::Test => PhaseOutput::Tests {
                executed: outcomes
                    .iter()
                    .map(|(name, outcome)| ExecutedTest {
                        id: format!("agentdesk {name}"),
                        check_id: check_id.into(),
                        outcome: outcome.clone(),
                        log_refs: vec![],
                    })
                    .collect(),
            },
            _ => PhaseOutput::None,
        };
        let requires = run.phase(&phase).unwrap().requires.clone();
        let mut input = a1::input(&[]);
        input.predecessors = requires
            .iter()
            .map(|r| {
                let output = &run.phase(r).unwrap().result.as_ref().unwrap().output;
                (r.clone(), digest_of(output).unwrap())
            })
            .collect();
        if kind == PhaseKind::Test {
            run.bind_plan(spec).unwrap();
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
        run.bind_phase_input(&phase, input).unwrap();
        let result = a1::result(&phase, exit(&phase), output);
        let action_id = run.phase(&phase).unwrap().action_id.clone();
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
        run.record_initiation(&phase, started).unwrap();
        run.record_result(&phase, &actions[&action_id].clone(), result)
            .unwrap();
        actions.get_mut(&action_id).unwrap().state = ActionState::Reconciled;
        let preparation = !matches!(kind, PhaseKind::Test | PhaseKind::Audit);
        if preparation && exit(&phase) != 0 {
            break;
        }
    }
    let mut environment = a1::environment();
    environment.checkout.observed_sha = checkout_sha.into();
    let collection = run
        .collect(|id| actions.get(id), environment, vec!["nextest".into()])
        .unwrap();
    run.collection = Some(collection);
}
