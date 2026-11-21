use std::io;
use std::path::Path;
use std::time::Duration;

use chrono::Utc;
use sqlx::PgPool;

use crate::db::server_campaigns::StoreError;
use crate::domain::campaign::decision_item::ApprovalBinding;
use crate::services::git::GitCommand;

use super::adk_observe::is_sha;
use super::deploy_observe::{
    DeploymentExpectation, DeploymentHealth, DeploymentProgress, DeploymentWatch, step_deployment,
};
pub use super::deploy_policy::DeployHost;
use super::deploy_policy::{self, DeployFact, DeployJob, DeployStatus, DeployWitness, Permit};
use super::effect_envelope::execution_nonce;
use super::host_gate::{EffectAuthority, EffectEnvelope, Initiated, Initiation, JournalState};
use super::host_worker::{HostWorker, WorkerError};
use super::model::*;
use super::verification_run::digest_of;

/// What the existing deployment path would release now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeployPlan {
    pub main_sha: String,
    pub hosts: Vec<DeployHost>,
}

/// The owning executor's existing deployment path. The server reaches it only through this port,
/// and `start_exact` only from inside the fixed host's final gate.
pub trait DeployExecutor: Send + Sync {
    /// Fetch main into `checkout` and report the commit and hosts a deployment would cover now.
    fn plan(&self, checkout: &Path) -> io::Result<DeployPlan>;
    /// Start the deployment of exactly the watched commit on the watched hosts and return its
    /// receipt. An error means the start is unproven; the same nonce must never start it twice.
    fn start_exact(&self, watch: &DeploymentWatch, nonce: &str) -> io::Result<String>;
}

#[derive(Debug, PartialEq, Eq)]
pub enum DeployProgress {
    NotRequired,
    Waiting,
    AwaitApproval(Box<ApprovalBinding>),
    Observing,
    Confirmed,
    Failed,
}

fn watch_of(job: &DeployJob) -> DeploymentWatch {
    let intent = &job.intent;
    DeploymentWatch {
        action_id: job.action_id.clone(),
        not_before: intent.requested_at,
        deadline: intent.deadline,
        targets: intent
            .hosts
            .iter()
            .map(|target| DeploymentExpectation {
                action_id: job.action_id.clone(),
                host: target.host.clone(),
                expected_sha: intent.target_sha.clone(),
                expected_binary_digest: target.expected_binary_digest.clone(),
                previous_incarnation: target.previous_incarnation.clone(),
            })
            .collect(),
    }
}

fn envelope(
    run: &Run,
    job: &DeployJob,
    opened: &Action,
    watch: &DeploymentWatch,
) -> Result<EffectEnvelope, WorkerError> {
    let binding = digest_of(&opened.binding).map_err(StoreError::from)?;
    let payload = watch.payload_digest()?;
    Ok(EffectEnvelope {
        execution_nonce: execution_nonce(&job.action_id, &payload),
        payload_digest: payload,
        action_id: job.action_id.clone(),
        kind: ActionKind::Deploy,
        run_id: run.id.clone(),
        node_id: opened.binding.node_id.clone(),
        origin_attempt_id: opened.binding.attempt_id.clone(),
        origin_binding_digest: binding.clone(),
        target_host_gate: job.host.clone(),
        phase: None,
        authority: EffectAuthority::Deployment {
            action_id: job.action_id.clone(),
            binding_digest: binding,
            node_epoch: job.epoch,
            run_control_version: job.control_version,
        },
    })
}

// The executor's current plan, accepted only when its main commit contains the node's merge.
fn current_plan(
    executor: &dyn DeployExecutor,
    run: &Run,
    opened: &Action,
    merge_sha: &str,
) -> Option<DeployPlan> {
    let seal = run.seals.get(&opened.binding.attempt_id)?;
    let checkout = Path::new(&seal.workspace.path);
    let plan = executor.plan(checkout).ok()?;
    let contains = plan.main_sha == merge_sha
        || GitCommand::new()
            .repo(checkout)
            .args(["merge-base", "--is-ancestor", merge_sha, &plan.main_sha])
            .run_output()
            .is_ok();
    (is_sha(&plan.main_sha) && contains).then_some(plan)
}

fn fail(reason: impl Into<String>, uncertain: bool) -> (DeployFact, DeployProgress) {
    let reason = reason.into();
    (
        DeployFact::Failed { reason, uncertain },
        DeployProgress::Failed,
    )
}

/// Advance the node's deployment by one step: request it, wait for the Deploy approval, start the
/// executor inside the final gate, then reconcile the running binaries. The journal is replayed
/// before any start, so a restart or a lost reply never starts the same deployment again.
pub async fn step_deploy(
    worker: &mut HostWorker,
    pool: &PgPool,
    executor: &dyn DeployExecutor,
    probe: &dyn DeploymentHealth,
    per_request: Duration,
) -> Result<DeployProgress, WorkerError> {
    let (tx, state) = worker.locked(pool).await?;
    let node = worker.node().to_owned();
    let landed = deploy_policy::landed(&state, &node).map_err(WorkerError::Rejected)?;
    let (run, opened) = (landed.run, landed.opened);
    if !deploy_policy::required(&state, &node) {
        return Ok(DeployProgress::NotRequired);
    }
    let record = |fact| Command::RecordDeployment {
        node_id: node.clone(),
        fact: Box::new(fact),
    };
    let request = |plan: DeployPlan| {
        record(DeployFact::Request {
            target_sha: plan.main_sha,
            hosts: plan.hosts,
        })
    };
    let now = Utc::now();
    let Some(job) = run.deployments.get(&node) else {
        if let Some(plan) = current_plan(executor, run, opened, &landed.main.merge_sha) {
            worker
                .commit(tx, state.event_sequence, request(plan), now)
                .await?;
        }
        return Ok(DeployProgress::Waiting);
    };
    let watch = watch_of(job);
    let (fact, progress) = match &job.status {
        DeployStatus::Confirmed { .. } => return Ok(DeployProgress::Confirmed),
        DeployStatus::Failed { .. } => return Ok(DeployProgress::Failed),
        DeployStatus::Started { .. } | DeployStatus::Indeterminate { .. } => {
            drop(tx);
            let outcome = step_deployment(worker, pool, &watch, probe, per_request).await?;
            let (fact, progress) = match outcome {
                DeploymentProgress::Waiting { .. } => return Ok(DeployProgress::Observing),
                DeploymentProgress::Confirmed { witnesses } => {
                    let witnesses = witnesses
                        .into_iter()
                        .map(|seen| DeployWitness {
                            host: seen.deployment.host,
                            source_sha: seen.deployment.actual_source_sha,
                            boot_incarnation: seen.deployment.boot_incarnation,
                            binary_identity: seen.deployment.binary_identity,
                            boot_started_at_ms: seen.boot_started_at_ms,
                            observed_at: seen.observed_at,
                        })
                        .collect();
                    (
                        DeployFact::Confirmed { witnesses },
                        DeployProgress::Confirmed,
                    )
                }
                DeploymentProgress::Failed { missing_hosts } => fail(
                    format!("no running-binary witness before the deadline: {missing_hosts:?}"),
                    false,
                ),
            };
            let (tx, state) = worker.locked(pool).await?;
            worker
                .commit(tx, state.event_sequence, record(fact), Utc::now())
                .await?;
            return Ok(progress);
        }
        DeployStatus::Waiting => {
            let envelope = envelope(run, job, opened, &watch)?;
            if let Some(entry) = worker.effect_entry(&job.action_id) {
                if entry.nonce != envelope.execution_nonce || entry.identity != envelope.identity()
                {
                    return Err(WorkerError::Binding);
                }
                match &entry.state {
                    JournalState::Started {
                        receipt,
                        process: None,
                    } => (
                        DeployFact::Started {
                            receipt: receipt.clone(),
                        },
                        DeployProgress::Observing,
                    ),
                    JournalState::Cancelled { evidence } | JournalState::NotSent { evidence } => {
                        fail(evidence.clone(), false)
                    }
                    _ => (
                        DeployFact::Indeterminate {
                            evidence: "the journal holds a start without a receipt".into(),
                        },
                        DeployProgress::Observing,
                    ),
                }
            } else if &job.host != worker.identity() {
                fail("deploy host incarnation changed", true)
            } else {
                // A stop or a passed deadline is settled without asking the executor anything.
                let plan = if deploy_policy::revoked(&state, run, &node, job, now).is_some() {
                    None
                } else {
                    match current_plan(executor, run, opened, &landed.main.merge_sha) {
                        Some(plan) => Some(plan),
                        None => return Ok(DeployProgress::Waiting),
                    }
                };
                let (main_sha, hosts) = plan
                    .as_ref()
                    .map_or(("", &[][..]), |plan| (&plan.main_sha, &plan.hosts));
                match deploy_policy::permit(&state, run, &node, job, main_sha, hosts, now) {
                    Permit::Refuse(reason) => fail(reason, false),
                    Permit::Retarget => {
                        let command = request(plan.expect("a retarget follows a plan"));
                        worker
                            .commit(tx, state.event_sequence, command, now)
                            .await?;
                        return Ok(DeployProgress::Waiting);
                    }
                    Permit::Approve(binding) => {
                        let command = Command::OpenApprovalGate {
                            binding: binding.clone(),
                        };
                        worker
                            .commit(tx, state.event_sequence, command, now)
                            .await?;
                        return Ok(DeployProgress::AwaitApproval(binding));
                    }
                    Permit::Start => {
                        let admitted =
                            worker.admit_deploy(&state, &envelope, Utc::now(), |nonce| {
                                let receipt = executor.start_exact(&watch, nonce)?;
                                if receipt.trim().is_empty() {
                                    return Err(io::Error::other(
                                        "the executor returned no receipt",
                                    ));
                                }
                                Ok(Initiated::Started {
                                    receipt,
                                    process: None,
                                    handle: (),
                                })
                            })?;
                        match admitted.initiation {
                            Initiation::Started { receipt, .. } => {
                                (DeployFact::Started { receipt }, DeployProgress::Observing)
                            }
                            Initiation::NotSent { evidence, .. } => fail(evidence, false),
                            Initiation::Indeterminate { evidence, .. } => (
                                DeployFact::Indeterminate { evidence },
                                DeployProgress::Observing,
                            ),
                        }
                    }
                }
            }
        }
    };
    worker
        .commit(tx, state.event_sequence, record(fact), Utc::now())
        .await?;
    Ok(progress)
}

#[cfg(test)]
#[path = "deploy_executor_tests.rs"]
mod tests;
