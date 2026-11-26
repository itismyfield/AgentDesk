use std::io;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};

use crate::db::server_campaigns::{
    self as store, CommandOutcome, ReceiptResponse, StoreError, StoreResult,
    command_transaction::StoreRequest,
};
use crate::domain::campaign::execution_pin::{ExecutionPin, PinError, RequirementsSnapshot};
use crate::domain::campaign::model::*;

use super::adk_adapter::{AdapterError, AdkAdapter};
use super::commands;
use super::host_worker::{HostWorker, WorkerError, WorkerTurn};
use super::runtime_profile::{RuntimeError, RuntimeProfile};
use super::stage_fsm::validate_verification_start;
use super::verification_runner::{LaunchContext, RunningPhase, Settled};
use super::verification_spec::adk_verification_spec;
use crate::domain::campaign::verification_run::{PhaseInitiation, PhaseKind, VerificationError};

pub struct VerificationRequest<'a> {
    pub execution: &'a str,
    pub pin: &'a ExecutionPin,
    pub requirements: &'a RequirementsSnapshot,
    pub headroom_wait: Duration,
}

pub struct VerificationDriver<'a> {
    campaign: &'a str,
    node: &'a str,
    worker: HostWorker,
    adapter: AdkAdapter<'a>,
    runtime: RuntimeProfile<'a>,
}

pub enum VerificationStep {
    Progress,
    Awaiting,
    Settled,
    Reconcile { action_id: String },
    Running(Box<ActivePhase>),
}

pub struct ActivePhase {
    action: String,
    running: RunningPhase,
}

impl ActivePhase {
    // A phase result is first retained on the host; poll owns its transactional delivery.
    async fn finish(
        self,
        worker: &mut HostWorker,
        timeout: Duration,
    ) -> Result<VerificationStep, DriverError> {
        let Self { action, running } = self;
        let settled = tokio::task::spawn_blocking(move || running.wait(timeout, Utc::now()))
            .await
            .map_err(io::Error::other)?;
        match settled {
            Settled::Finished(result) => {
                worker.finish_phase(&action, &result, Utc::now())?;
                Ok(VerificationStep::Progress)
            }
            Settled::Lingering { phase, .. } => Ok(VerificationStep::Running(Box::new(Self {
                action,
                running: *phase,
            }))),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DriverError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Worker(#[from] WorkerError),
    #[error(transparent)]
    Adapter(#[from] AdapterError),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    Pin(#[from] PinError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("verification run, subject or consumer could not be matched")]
    Binding,
    #[error("{0:?}")]
    Rejected(Rejection),
}

impl<'a> VerificationDriver<'a> {
    pub fn open(
        journal_root: &Path,
        campaign: &'a str,
        node: &'a str,
        gate: &str,
        adapter: AdkAdapter<'a>,
        runtime: RuntimeProfile<'a>,
    ) -> Result<Self, DriverError> {
        let worker = HostWorker::open(
            journal_root,
            campaign,
            node,
            &runtime.scope.host,
            gate,
            adapter.resources.clone(),
            Utc::now(),
        )?;
        Ok(Self {
            campaign,
            node,
            worker,
            adapter,
            runtime,
        })
    }

    pub fn worker(&mut self) -> &mut HostWorker {
        &mut self.worker
    }

    pub async fn finish(
        &mut self,
        active: ActivePhase,
        timeout: Duration,
    ) -> Result<VerificationStep, DriverError> {
        active.finish(&mut self.worker, timeout).await
    }

    pub async fn step(
        &mut self,
        pool: &PgPool,
        request: VerificationRequest<'_>,
    ) -> Result<VerificationStep, DriverError> {
        let now = Utc::now();
        let polled = self.worker.poll(pool, now).await?;
        match polled.turn {
            WorkerTurn::Replayed { .. } => return Ok(VerificationStep::Progress),
            WorkerTurn::Reconcile { action_id } | WorkerTurn::LaunchNotSent { action_id, .. } => {
                return Ok(VerificationStep::Reconcile { action_id });
            }
            WorkerTurn::Next(_) => {}
        }
        let mut tx = pool.begin().await.map_err(StoreError::from)?;
        let state = store::get_for_worker(&mut tx, self.campaign)
            .await?
            .ok_or(DriverError::Binding)?;
        let run = state.run.as_ref().ok_or(DriverError::Binding)?;
        let v = run
            .verifications
            .get(request.execution)
            .ok_or(DriverError::Binding)?;
        if v.collection.is_some() || v.cancellation.is_some() {
            return Ok(VerificationStep::Settled);
        }
        let current = state
            .nodes
            .get(&v.origin.node_id)
            .ok_or(DriverError::Binding)?;
        let spec = adk_verification_spec();
        let spec_hash = spec.hash().map_err(io::Error::other)?;
        let hashes = state
            .definition
            .hashes()
            .get(&v.origin.node_id)
            .ok_or(DriverError::Binding)?;
        if request.pin.hash()? != v.origin.pin_hash
            || v.origin.spec_hash != spec_hash
            || &v.origin.gate != self.worker.identity()
            || v.origin.node_id != self.node
            || request.requirements.observation.issue_number != self.runtime.scope.issue_number
        {
            return Err(DriverError::Binding);
        }
        // Collection uses only recorded evidence and can finish after start authority is revoked.
        let cancelled = v.phases.iter().any(|p| {
            run.actions
                .get(&p.action_id)
                .is_some_and(|a| a.state == ActionState::Cancelled)
        });
        let collectible = run.environments.get(request.execution).is_some_and(|env| {
            !matches!(
                v.collect(|id| run.actions.get(id), env.clone(), Vec::new()),
                Err(VerificationError::NotFinished)
            )
        });
        if collectible
            || cancelled
                && v.phases.iter().all(|p| {
                    run.actions.get(&p.action_id).is_some_and(|a| {
                        matches!(
                            a.state,
                            ActionState::Planned | ActionState::Reconciled | ActionState::Cancelled
                        )
                    })
                })
        {
            return send(
                tx,
                &state,
                Command::SettleVerification {
                    execution_id: request.execution.into(),
                    evidence: "host verification is quiet".into(),
                },
                now,
            )
            .await;
        }
        let consumer = current
            .current_attempt
            .as_ref()
            .and_then(|id| run.attempts.get(id))
            .ok_or(DriverError::Binding)?
            .binding
            .clone();
        validate_verification_start(&state, v, &consumer).map_err(DriverError::Rejected)?;
        let runtime = self.runtime.observe(self.worker.identity(), &spec)?;
        runtime.require_session(consumer.role, &consumer.session)?;
        let profile_hash = runtime.profile().hash().map_err(io::Error::other)?;
        request
            .pin
            .revalidate(hashes, request.requirements, &profile_hash, &spec_hash)?;
        let execution_id = request.execution.to_owned();
        let consumer = Box::new(consumer);
        if v.subject.bound().is_none() {
            let command = Command::BindVerificationSubject {
                execution_id,
                consumer,
            };
            return send(tx, &state, command, now).await;
        }
        let Some(phase) = v.phases.iter().find(|p| p.result.is_none()) else {
            return Ok(VerificationStep::Awaiting);
        };
        let action = run
            .actions
            .get(&phase.action_id)
            .ok_or(DriverError::Binding)?;
        if phase.kind == PhaseKind::Test && v.plan.bound().is_none() {
            let command = Command::BindVerificationPlan {
                execution_id,
                consumer,
            };
            return send(tx, &state, command, now).await;
        }
        if !matches!(
            action.state,
            ActionState::Planned | ActionState::Claimed | ActionState::StartPending
        ) {
            return Ok(VerificationStep::Awaiting);
        }
        let material = self
            .adapter
            .materialize(v, phase, &spec, self.worker.identity(), now)?;
        let phase_id = phase.id.clone();
        let Some(bound) = phase.input.bound() else {
            let command = Command::BindVerificationInput {
                execution_id,
                phase_id,
                consumer,
                input: Box::new(material.input),
            };
            return send(tx, &state, command, now).await;
        };
        if bound.input != material.input {
            return Err(DriverError::Binding);
        }
        if action.state != ActionState::StartPending {
            let command = Command::AdvanceVerificationPhase {
                execution_id,
                phase_id,
                consumer,
                state: if action.state == ActionState::Planned {
                    ActionState::Claimed
                } else {
                    ActionState::StartPending
                },
            };
            return send(tx, &state, command, now).await;
        }
        drop(tx);
        let host = self.worker.identity().host.clone();
        let started = self
            .worker
            .start_phase(
                pool,
                request.execution,
                &phase.id,
                &[],
                LaunchContext {
                    check_id: &v.origin.check_id,
                    log_dir: &material.logs,
                    host: &host,
                    now,
                    handoff: material.handoff,
                    nonce: String::new(),
                },
                request.headroom_wait,
            )
            .await?;
        let fact = match started.initiation {
            PhaseInitiation::Started { receipt, process } => PhaseFact::Started {
                receipt,
                process,
                environment: Box::new(material.environment),
            },
            PhaseInitiation::NotSent { evidence, .. } => PhaseFact::NotStarted { evidence },
            PhaseInitiation::Indeterminate {
                initiation_id,
                evidence,
                ..
            } => PhaseFact::Indeterminate {
                initiation_id,
                evidence,
            },
        };
        let receipt_tx = pool.begin().await.map_err(StoreError::from)?;
        send(
            receipt_tx,
            &state,
            Command::RecordVerificationPhase {
                execution_id,
                phase_id,
                fact: Box::new(fact),
            },
            now,
        )
        .await?;
        Ok(started
            .running
            .map_or(VerificationStep::Progress, |running| {
                VerificationStep::Running(Box::new(ActivePhase {
                    action: phase.action_id.clone(),
                    running,
                }))
            }))
    }
}

async fn send(
    tx: Transaction<'_, Postgres>,
    state: &CampaignState,
    command: Command,
    now: DateTime<Utc>,
) -> Result<VerificationStep, DriverError> {
    let fingerprint = command_fingerprint(&command).map_err(StoreError::from)?;
    let key = format!("adk-verification:{}:{fingerprint}", state.event_sequence);
    let (tx, reply) = commands::apply_in(
        tx,
        &state.id,
        &key,
        now,
        StoreRequest::Command(command),
        render,
    )
    .await?;
    tx.commit().await.map_err(StoreError::from)?;
    match serde_json::from_value(reply.response.body).map_err(StoreError::from)? {
        CommandOutcome::Decided(Ok(_)) => Ok(VerificationStep::Progress),
        CommandOutcome::Decided(Err(rejection)) => Err(DriverError::Rejected(rejection)),
        CommandOutcome::NotFound => Err(DriverError::Binding),
    }
}

fn render(outcome: &CommandOutcome) -> StoreResult<ReceiptResponse> {
    Ok(ReceiptResponse {
        status: 200,
        body: serde_json::to_value(outcome)?,
    })
}

#[cfg(all(test, target_os = "linux"))]
#[path = "verification_driver_tests.rs"]
mod tests;
