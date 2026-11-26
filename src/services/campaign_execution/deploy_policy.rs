use chrono::{DateTime, TimeDelta, Utc};

use crate::domain::campaign::decision_item::ApprovalBinding;
use crate::domain::campaign::definition::{AcceptanceCondition, VerificationKind};
#[cfg(test)]
use crate::domain::campaign::deployment::DeployWitness;
use crate::domain::campaign::deployment::{
    DeployFact, DeployHost, DeployIntent, DeployJob, DeployStatus,
};
use crate::domain::campaign::profile::MainIntegrationEvidence;

use super::landing_effects::{Landing, LandingStatus, MergeStatus, OpenedPr};
use super::lifecycle;
use super::model::*;
use super::stage_fsm::Transition;

// The confirmation limit of the profile. It starts at the first request and covers the approval
// wait, the executor's own window and the witness; a moved head never extends it.
const CONFIRM_LIMIT: TimeDelta = TimeDelta::hours(24);

fn rejected(code: RejectionCode) -> Rejection {
    Rejection::new(code)
}

pub(super) struct Landed<'a> {
    pub run: &'a Run,
    pub landing: &'a Landing,
    pub pr: &'a OpenedPr,
    pub opened: &'a Action,
    pub main: &'a MainIntegrationEvidence,
}

// The node's own PR, opened by this run and witnessed on main.
pub(super) fn landed<'a>(state: &'a CampaignState, node: &str) -> Result<Landed<'a>, Rejection> {
    let run = state
        .run
        .as_ref()
        .ok_or_else(|| rejected(RejectionCode::StaleRun))?;
    let landing = run
        .landings
        .get(node)
        .ok_or_else(|| rejected(RejectionCode::StaleBinding))?;
    let LandingStatus::CiHold { pr } = &landing.status else {
        return Err(rejected(RejectionCode::InvalidState));
    };
    let opened = run
        .actions
        .get(&landing.action_id)
        .filter(|action| {
            action.binding.node_id == node
                && action.binding.run_id == run.id
                && action.kind == ActionKind::CreatePr
        })
        .ok_or_else(|| rejected(RejectionCode::StaleBinding))?;
    let integrated = state.nodes.get(node).is_some_and(|n| n.main_integrated);
    match run.merges.get(node).map(|job| &job.status) {
        Some(MergeStatus::Adopted(main)) if integrated => Ok(Landed {
            run,
            landing,
            pr,
            opened,
            main,
        }),
        _ => Err(rejected(RejectionCode::GateNotPassed)),
    }
}

pub(super) fn conditions<'a>(state: &'a CampaignState, node: &str) -> &'a [AcceptanceCondition] {
    let nodes = &state.definition.input().nodes;
    nodes
        .iter()
        .find(|definition| definition.id == node)
        .map_or(&[], |definition| &definition.acceptance)
}

// A deployment is owed when a condition can only be proven on the deployed binary.
pub(super) fn required(state: &CampaignState, node: &str) -> bool {
    conditions(state, node).iter().any(|condition| {
        matches!(
            condition.verification,
            VerificationKind::Deployment | VerificationKind::Operational
        )
    })
}

// Why an unstarted deployment can no longer start: a stop, a moved fence, or its deadline.
pub(super) fn revoked(
    state: &CampaignState,
    run: &Run,
    node: &str,
    job: &DeployJob,
    now: DateTime<Utc>,
) -> Option<&'static str> {
    if !lifecycle::deploy_position(state, run, node, job) {
        Some("deploy authority was revoked")
    } else if now >= job.intent.deadline {
        Some("deployment was not approved and started before its deadline")
    } else {
        None
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Permit {
    Start,
    Approve(Box<ApprovalBinding>),
    // The executor would release another commit or other hosts than the intent names.
    Retarget,
    Refuse(&'static str),
}

// The last decision before the executor starts, taken under the campaign row lock. `main_sha`
// and `hosts` are what the existing deployment path would release at this moment.
pub(super) fn permit(
    state: &CampaignState,
    run: &Run,
    node: &str,
    job: &DeployJob,
    main_sha: &str,
    hosts: &[DeployHost],
    now: DateTime<Utc>,
) -> Permit {
    if let Some(reason) = revoked(state, run, node, job, now) {
        return Permit::Refuse(reason);
    }
    if job.intent.target_sha != main_sha || job.intent.hosts != hosts {
        return Permit::Retarget;
    }
    match lifecycle::deploy_approval(state, run, node, job) {
        Some((binding, false)) => Permit::Approve(Box::new(binding)),
        _ => Permit::Start,
    }
}

pub(super) fn decide(
    state: &mut CampaignState,
    node: &str,
    fact: &DeployFact,
    now: DateTime<Utc>,
) -> Transition {
    let Landed {
        run,
        landing,
        opened,
        ..
    } = landed(state, node)?;
    let id = format!("deploy:{}", opened.id);
    let event = Some(EventKind::ActionChanged {
        action_id: id.clone(),
    });
    let previous = run.deployments.get(node);
    if let DeployFact::Request { target_sha, hosts } = fact {
        if previous
            .is_some_and(|job| (&job.intent.target_sha, &job.intent.hosts) == (target_sha, hosts))
        {
            return Ok(None);
        }
        let opening = lifecycle::deploy_opening(state, run, node).filter(|_| {
            required(state, node)
                && previous.is_none_or(|job| {
                    job.status == DeployStatus::Waiting
                        && lifecycle::deploy_position(state, run, node, job)
                })
        });
        let Some((epoch, candidate)) = opening else {
            return Err(rejected(RejectionCode::InvalidState));
        };
        let intent = DeployIntent {
            target_sha: target_sha.clone(),
            hosts: hosts.clone(),
            requested_at: now,
            deadline: previous.map_or(now + CONFIRM_LIMIT, |job| job.intent.deadline),
        };
        if !intent.valid() {
            return Err(rejected(RejectionCode::InvalidState));
        }
        if previous.is_none() && run.actions.contains_key(&id) {
            return Err(rejected(RejectionCode::IdempotencyConflict));
        }
        let job = DeployJob {
            intent,
            action_id: id.clone(),
            epoch,
            control_version: run.control_version,
            host: landing.host.clone(),
            pin_hash: candidate.pin_hash.clone(),
            status: DeployStatus::Waiting,
        };
        let action = Action {
            id: id.clone(),
            binding: opened.binding.clone(),
            kind: ActionKind::Deploy,
            target: landing.host.clone(),
            state: ActionState::StartPending,
            initiation_receipt: None,
            reconciliation: None,
        };
        let run = state.run.as_mut().expect("landed above");
        run.actions.insert(id, action);
        run.deployments.insert(node.into(), job);
        return Ok(event);
    }
    let job = previous.ok_or_else(|| rejected(RejectionCode::StaleBinding))?;
    let started = matches!(
        job.status,
        DeployStatus::Started { .. } | DeployStatus::Indeterminate { .. }
    );
    let (status, action_state, note) = match (fact, &job.status) {
        (DeployFact::Started { receipt }, DeployStatus::Waiting) if !receipt.trim().is_empty() => (
            DeployStatus::Started {
                receipt: receipt.clone(),
            },
            ActionState::Started,
            None,
        ),
        (DeployFact::Indeterminate { evidence }, DeployStatus::Waiting)
            if !evidence.trim().is_empty() =>
        {
            (
                DeployStatus::Indeterminate {
                    evidence: evidence.clone(),
                },
                ActionState::Unknown,
                None,
            )
        }
        // Only a started deployment can be confirmed, and only by every host's running binary.
        (DeployFact::Confirmed { witnesses }, _)
            if started && job.intent.witnessed_by(witnesses, now) =>
        {
            (
                DeployStatus::Confirmed {
                    witnesses: witnesses.clone(),
                },
                ActionState::Reconciled,
                Some("every host runs the target source".to_owned()),
            )
        }
        (DeployFact::Failed { reason, uncertain }, _)
            if !reason.trim().is_empty() && (started || job.status == DeployStatus::Waiting) =>
        {
            let uncertain = *uncertain || matches!(job.status, DeployStatus::Indeterminate { .. });
            let action = match job.status {
                _ if uncertain => ActionState::Unknown,
                DeployStatus::Waiting => ActionState::Cancelled,
                _ => ActionState::Reconciled,
            };
            (
                DeployStatus::Failed {
                    reason: reason.clone(),
                    uncertain,
                },
                action,
                Some(reason.clone()),
            )
        }
        _ => return Err(rejected(RejectionCode::InvalidState)),
    };
    let run = state.run.as_mut().expect("landed above");
    let action = run
        .actions
        .get_mut(&id)
        .ok_or_else(|| rejected(RejectionCode::StaleBinding))?;
    action.state = action_state;
    if let DeployStatus::Started { receipt } = &status {
        action.initiation_receipt = Some(receipt.clone());
    }
    if note.is_some() {
        action.reconciliation = note;
    }
    let job = run.deployments.get_mut(node).expect("read above");
    job.status = status;
    match fact {
        DeployFact::Confirmed { .. } => lifecycle::deployment_confirmed(state, node),
        DeployFact::Failed { .. } => {
            let running = state.control == CampaignControl::Running
                && state.run.as_ref().is_some_and(|run| run.stop.is_none());
            if running && let Some(node) = state.nodes.get_mut(node) {
                node.disposition = NodeDisposition::Blocked;
                node.wait_reason = Some(ReasonCode::StateNeedsInspection);
            }
            Ok(event)
        }
        _ => Ok(event),
    }
}

#[cfg(test)]
#[path = "deploy_policy_tests.rs"]
pub(crate) mod tests;
