use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::domain::campaign::definition::VerificationKind;
pub use crate::domain::campaign::deployment::{AcceptanceCheck, AcceptanceFact};
use crate::domain::campaign::landing::MergeReceipt;

use super::deploy_policy::{self, DeployStatus, Landed};
use super::host_worker::{HostWorker, WorkerError};
use super::lifecycle;
use super::model::*;
use super::stage_fsm::Transition;

fn rejected(code: RejectionCode) -> Rejection {
    Rejection::new(code)
}

// The deployed commit and the time its last host was witnessed. An unstarted, unknown or failed
// deployment yields nothing.
fn deployed<'a>(run: &'a Run, node: &str) -> Option<(&'a str, DateTime<Utc>)> {
    let job = run.deployments.get(node)?;
    let DeployStatus::Confirmed { witnesses } = &job.status else {
        return None;
    };
    let seen = witnesses.iter().map(|witness| witness.observed_at).max()?;
    Some((&job.intent.target_sha, seen))
}

// The first acceptance condition the run's own records do not prove for the landed candidate.
pub(super) fn unmet<'a>(
    state: &'a CampaignState,
    node: &str,
) -> Result<Option<&'a str>, Rejection> {
    let Landed {
        run,
        pr,
        opened,
        main,
        ..
    } = deploy_policy::landed(state, node)?;
    let current = lifecycle::landing_candidate(state, run, node).is_some_and(|candidate| {
        candidate.head_sha == pr.head_sha && candidate.work_attempt == opened.binding.attempt_id
    });
    if !current {
        return Err(rejected(RejectionCode::StaleBinding));
    }
    // The merge receipt carries the head's review, CI run and TLC result.
    let tested = run
        .actions
        .get(&format!("merge:{}", opened.id))
        .filter(|action| action.state == ActionState::Reconciled)
        .and_then(|action| action.initiation_receipt.as_deref())
        .and_then(|receipt| serde_json::from_str::<MergeReceipt>(receipt).ok())
        .is_some_and(|receipt| {
            receipt.matches(pr, &opened.binding) && receipt.merge_sha == main.merge_sha
        });
    let deployed = deployed(run, node);
    let checks = run.acceptance_checks.get(node);
    let proven =
        |condition: &crate::domain::campaign::definition::AcceptanceCondition| match condition
            .verification
        {
            VerificationKind::Test => tested,
            VerificationKind::Main => main.pr_number == pr.number,
            VerificationKind::Deployment => deployed.is_some(),
            VerificationKind::Operational => deployed.is_some_and(|(sha, _)| {
                checks
                    .and_then(|checks| checks.get(&condition.id))
                    .is_some_and(|check| check.deployed_sha == sha)
            }),
        };
    Ok(deploy_policy::conditions(state, node)
        .iter()
        .find(|condition| !proven(condition))
        .map(|condition| condition.id.as_str()))
}

pub(super) fn decide(
    state: &mut CampaignState,
    node: &str,
    fact: &AcceptanceFact,
    now: DateTime<Utc>,
) -> Transition {
    let stage = state
        .nodes
        .get(node)
        .map(|node| node.stage)
        .ok_or_else(|| rejected(RejectionCode::StaleBinding))?;
    let AcceptanceFact::Checked(check) = fact else {
        if lifecycle::confirmed(&state.nodes[node]) {
            return Ok(None);
        }
        if let Some(condition) = unmet(state, node)? {
            return Err(Rejection {
                target: Some(format!("acceptance:{condition}")),
                ..rejected(RejectionCode::GateNotPassed)
            });
        }
        return lifecycle::acceptance_judged(state, node);
    };
    let run = deploy_policy::landed(state, node)?.run;
    let operational = deploy_policy::conditions(state, node)
        .iter()
        .any(|condition| {
            condition.id == check.condition_id
                && condition.verification == VerificationKind::Operational
        });
    // The check has to follow the witnessed deployment of the same commit.
    let bound = deployed(run, node).is_some_and(|(sha, seen)| {
        check.deployed_sha == sha && seen <= check.observed_at && check.observed_at <= now
    });
    if !operational || !bound || check.reference.trim().is_empty() {
        return Err(rejected(RejectionCode::GateNotPassed));
    }
    let saved = run
        .acceptance_checks
        .get(node)
        .and_then(|checks| checks.get(&check.condition_id));
    if let Some(saved) = saved {
        return if saved == check {
            Ok(None)
        } else {
            Err(rejected(RejectionCode::IdempotencyConflict))
        };
    }
    let run = state.run.as_mut().expect("landed above");
    run.acceptance_checks
        .entry(node.into())
        .or_default()
        .insert(check.condition_id.clone(), check.clone());
    Ok(Some(EventKind::NodeAdvanced {
        node_id: node.into(),
        stage,
    }))
}

/// Record the server's completion judgement once every acceptance condition is proven.
pub async fn judge(worker: &HostWorker, pool: &PgPool) -> Result<bool, WorkerError> {
    let (tx, state) = worker.locked(pool).await?;
    let node = worker.node().to_owned();
    if unmet(&state, &node)
        .map_err(WorkerError::Rejected)?
        .is_some()
    {
        return Ok(false);
    }
    let command = Command::RecordAcceptance {
        node_id: node,
        fact: Box::new(AcceptanceFact::Judge),
    };
    worker
        .commit(tx, state.event_sequence, command, Utc::now())
        .await?;
    Ok(true)
}

#[cfg(test)]
#[path = "final_acceptance_tests.rs"]
mod tests;
