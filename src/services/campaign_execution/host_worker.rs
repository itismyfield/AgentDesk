use std::io;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, Transaction};

use crate::db::server_campaigns::{
    self as store, CommandOutcome, ReceiptResponse, StoreError, StoreResult,
    command_transaction::StoreRequest,
};
use crate::domain::campaign::{
    definition::valid_id, lifecycle::NextAction, model::command_fingerprint,
};

use super::adk_resources::{AdkResources, ResourceObservation};
use super::commands;
use super::effect_envelope::{action_envelope, execution_nonce, host_fences, phase_launch};
use super::host_gate::{
    self, Admitted, EffectGate, FenceReport, GateError, Initiated, JournalEntry, JournalState,
    PhaseStart, Recovered, Resolution,
};
use super::host_identity;
use super::model::*;
use super::resource_admission::ResourceAdmission;
use super::scheduler::next_action;
use super::verification_run::{Digest, PhaseKind, PhaseResult, TreeState, digest_of};
use super::verification_runner::{LaunchContext, process_tree_state};

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Gate(#[from] GateError),
    #[error(transparent)]
    Host(#[from] io::Error),
    #[error("campaign, node, action or host does not match this worker")]
    Binding,
    #[error("the proposed lifecycle step is stale")]
    StaleStep,
    #[error("{0:?}")]
    Rejected(Rejection),
    #[error("host action {0} needs reconciliation before another effect can start")]
    Reconcile(String),
    #[error("host resource admission: {0:?}")]
    Resource(ResourceAdmission),
}

#[derive(Debug)]
pub struct WorkerPoll {
    pub fence: Option<FenceReport>,
    pub recovered: Vec<Recovered>,
    pub turn: WorkerTurn,
}

#[derive(Debug, PartialEq, Eq)]
pub enum WorkerTurn {
    Next(NextAction),
    Reconcile { action_id: String },
    Replayed { action_id: String },
    LaunchNotSent { action_id: String, evidence: String },
}

// A private journal root is provisioned once on the fixed host. All incarnations derive the
// same directory for a campaign node, so opening the gate also takes that node's worker lock.
pub struct HostWorker {
    campaign: String,
    node: String,
    gate: EffectGate,
    resources: AdkResources,
}

impl HostWorker {
    pub fn open(
        root: &Path,
        campaign: &str,
        node: &str,
        fixed_host: &str,
        gate: &str,
        resources: AdkResources,
        now: DateTime<Utc>,
    ) -> Result<Self, WorkerError> {
        let identity = host_identity::observe(fixed_host, gate)?;
        Self::open_observed(root, campaign, node, identity, resources, now)
    }

    fn open_observed(
        root: &Path,
        campaign: &str,
        node: &str,
        identity: HostGate,
        resources: AdkResources,
        now: DateTime<Utc>,
    ) -> Result<Self, WorkerError> {
        if !root.is_absolute() || !root.is_dir() || !valid_id(campaign) || !valid_id(node) {
            return Err(WorkerError::Binding);
        }
        let key = format!("{campaign}\n{node}");
        let path = root.join(blake3::hash(key.as_bytes()).to_hex().as_str());
        let gate = EffectGate::recover_journal(&path, identity, now)?;
        Ok(Self {
            campaign: campaign.into(),
            node: node.into(),
            gate,
            resources,
        })
    }

    pub fn identity(&self) -> &HostGate {
        self.gate.identity()
    }

    pub(super) fn effect_entry(&self, action_id: &str) -> Option<&JournalEntry> {
        self.gate.entry(action_id)
    }

    pub(super) fn serves(&self, binding: &AttemptBinding) -> bool {
        self.campaign == binding.campaign_id && self.node == binding.node_id
    }

    async fn locked<'a>(
        &self,
        pool: &'a PgPool,
    ) -> Result<(Transaction<'a, Postgres>, CampaignState), WorkerError> {
        let mut tx = pool.begin().await.map_err(StoreError::from)?;
        let state = store::get_for_worker(&mut tx, &self.campaign)
            .await?
            .filter(|s| s.nodes.contains_key(&self.node))
            .ok_or(WorkerError::Binding)?;
        Ok((tx, state))
    }

    fn apply_fences(&mut self, state: &CampaignState) -> Result<FenceReport, WorkerError> {
        let (run, node) = host_fences(state, &self.node).map_err(WorkerError::Rejected)?;
        let version = run.control_version;
        let report = self.gate.apply_run_fence(run)?;
        if report.acknowledgement.control_version != version {
            return Err(WorkerError::Rejected(Rejection::new(
                RejectionCode::StaleFence,
            )));
        }
        self.gate.apply_node_fence(node.clone())?;
        if !self.gate.has_node_fence(&node) {
            return Err(WorkerError::Rejected(Rejection::new(
                RejectionCode::StaleFence,
            )));
        }
        Ok(report)
    }

    pub async fn poll(
        &mut self,
        pool: &PgPool,
        now: DateTime<Utc>,
    ) -> Result<WorkerPoll, WorkerError> {
        let (tx, state) = self.locked(pool).await?;
        let Some(run) = &state.run else {
            return Ok(WorkerPoll {
                fence: None,
                recovered: Vec::new(),
                turn: WorkerTurn::Next(next_action(&state, &self.node)),
            });
        };
        let fence = self.apply_fences(&state)?;
        let mut recovered = self.gate.recovery_snapshot_with(
            Some(&run.id),
            |entry, process| {
                if entry.identity.kind != ActionKind::VerificationPhase
                    || process.host != entry.identity.target.host
                    || process.nonce != entry.nonce
                {
                    return TreeState::Unknown;
                }
                if host_identity::previous_boot(&entry.identity.target, self.identity()) {
                    return TreeState::Gone;
                }
                if &entry.identity.target == self.identity()
                    && process_tree_state(process) == TreeState::Gone
                {
                    return TreeState::Gone;
                }
                // A reused process group can look alive after a restart.
                TreeState::Unknown
            },
            now,
        );
        let mut turn = None;
        // Keep an unapplied receipt ahead of the next lifecycle step. Recovery never invokes a
        // starter, and a missing journal record never proves that another target stayed quiet.
        for entry in &fence.entries {
            let action = self.check_entry(run, entry)?;
            let action_id = &action.id;
            if let JournalState::Finished {
                receipt, result, ..
            } = &entry.state
                && !entry.delivered
                && let Some(phase) = &entry.identity.phase
            {
                let result: PhaseResult =
                    serde_json::from_value(result.clone()).map_err(StoreError::from)?;
                let stored = run
                    .verifications
                    .get(&phase.execution_id)
                    .and_then(|v| v.phase(&phase.phase_id).ok())
                    .ok_or(WorkerError::Binding)?;
                if result.receipt != *receipt || stored.action_id != *action_id {
                    return Err(WorkerError::Binding);
                }
                if stored.result.as_ref() == Some(&result) {
                    self.gate.mark_delivered(action_id, now)?;
                    recovered.retain(|item| !finished_action(item, action_id));
                    continue;
                }
                if stored.result.is_some() {
                    return Err(WorkerError::Binding);
                }
                if matches!(action.state, ActionState::Started | ActionState::Unknown)
                    && action.initiation_receipt.as_ref() == Some(receipt)
                {
                    let command = Command::RecordVerificationPhase {
                        execution_id: phase.execution_id.clone(),
                        phase_id: phase.phase_id.clone(),
                        fact: Box::new(PhaseFact::Finished {
                            result: Box::new(result),
                        }),
                    };
                    self.commit(tx, state.event_sequence, command, now).await?;
                    self.gate.mark_delivered(action_id, now)?;
                    recovered.retain(|item| !finished_action(item, action_id));
                    return Ok(WorkerPoll {
                        fence: Some(fence),
                        recovered,
                        turn: WorkerTurn::Replayed {
                            action_id: action_id.clone(),
                        },
                    });
                }
            }
            turn = turn.or_else(|| recovery_turn(action, entry));
        }
        let turn = turn
            .or_else(|| {
                self.other_incarnation(run)
                    .map(|action| WorkerTurn::Reconcile {
                        action_id: action.id.clone(),
                    })
            })
            .unwrap_or_else(|| WorkerTurn::Next(next_action(&state, &self.node)));
        Ok(WorkerPoll {
            fence: Some(fence),
            recovered,
            turn,
        })
    }

    fn other_incarnation<'a>(&self, run: &'a Run) -> Option<&'a Action> {
        run.actions.values().find(|action| {
            action.binding.node_id == self.node
                && &action.target != self.identity()
                && matches!(
                    action.state,
                    ActionState::StartPending | ActionState::Started | ActionState::Unknown
                )
        })
    }

    fn check_entry<'a>(
        &self,
        run: &'a Run,
        entry: &JournalEntry,
    ) -> Result<&'a Action, WorkerError> {
        let id = &entry.identity;
        let action = run.actions.get(&id.action_id).ok_or(WorkerError::Binding)?;
        if id.run_id != run.id
            || id.node_id != self.node
            || action.binding.campaign_id != self.campaign
            || action.binding.node_id != self.node
            || action.binding.run_id != run.id
            || action.binding.attempt_id != id.origin_attempt_id
            || action.target != id.target
            || action.kind != id.kind
            || (id.kind == ActionKind::VerificationPhase) != id.phase.is_some()
            || digest_of(&action.binding).map_err(StoreError::from)? != id.origin_binding_digest
            || entry.nonce != execution_nonce(&id.action_id, &id.payload_digest)
        {
            return Err(WorkerError::Binding);
        }
        Ok(action)
    }

    pub async fn advance(
        &mut self,
        pool: &PgPool,
        command: Command,
        now: DateTime<Utc>,
    ) -> Result<CommandResult, WorkerError> {
        let (tx, state) = self.locked(pool).await?;
        let turn = next_action(&state, &self.node);
        if !matches_step(&turn, &command, &self.node, self.identity()) {
            return Err(WorkerError::StaleStep);
        }
        let run = state.run.as_ref().ok_or(WorkerError::Binding)?;
        let fence = self.apply_fences(&state)?;
        if let Some(action) = self.other_incarnation(run) {
            return Err(WorkerError::Reconcile(action.id.clone()));
        }
        for entry in &fence.entries {
            let action = self.check_entry(run, entry)?;
            if recovery_turn(action, entry).is_some() {
                return Err(WorkerError::StaleStep);
            }
        }
        self.commit(tx, state.event_sequence, command, now).await
    }

    async fn commit(
        &self,
        tx: Transaction<'_, Postgres>,
        sequence: u64,
        command: Command,
        now: DateTime<Utc>,
    ) -> Result<CommandResult, WorkerError> {
        let key = format!(
            "host-worker:{sequence}:{}",
            command_fingerprint(&command).map_err(StoreError::from)?
        );
        let (tx, reply) = commands::apply_in(
            tx,
            &self.campaign,
            &key,
            now,
            StoreRequest::Command(command),
            render_receipt,
        )
        .await?;
        tx.commit().await.map_err(StoreError::from)?;
        match serde_json::from_value(reply.response.body).map_err(StoreError::from)? {
            CommandOutcome::Decided(Ok(result)) => Ok(result),
            CommandOutcome::Decided(Err(rejection)) => Err(WorkerError::Rejected(rejection)),
            CommandOutcome::NotFound => Err(WorkerError::Binding),
        }
    }

    pub async fn start_action<T>(
        &mut self,
        pool: &PgPool,
        action_id: &str,
        payload: Digest,
        wait: Duration,
        now: DateTime<Utc>,
        initiate: impl FnOnce(&str) -> io::Result<Initiated<T>>,
    ) -> Result<Admitted<T>, WorkerError> {
        self.wait_for_headroom(wait).await?;
        let (_lock, state) = self.locked(pool).await?;
        let action = state
            .run
            .as_ref()
            .and_then(|r| r.actions.get(action_id))
            .filter(|a| {
                a.binding.node_id == self.node
                    && &a.target == self.identity()
                    && matches!(a.kind, ActionKind::Launch | ActionKind::DeliverStage)
            })
            .ok_or(WorkerError::Binding)?;
        let envelope = action_envelope(&state, action_id, &action.binding, payload)
            .map_err(WorkerError::Rejected)?;
        self.fence_start(&state, action_id)?;
        Ok(self.gate.admit_and_initiate(&envelope, now, initiate)?)
    }

    pub async fn start_phase(
        &mut self,
        pool: &PgPool,
        execution_id: &str,
        phase_id: &str,
        observations: &[ResourceObservation],
        ctx: LaunchContext<'_>,
        wait: Duration,
    ) -> Result<PhaseStart, WorkerError> {
        self.wait_for_headroom(wait).await?;
        let (_lock, state) = self.locked(pool).await?;
        let consumer = state.nodes[&self.node]
            .current_attempt
            .as_ref()
            .and_then(|id| state.run.as_ref()?.attempts.get(id))
            .ok_or(WorkerError::Binding)?;
        let launch = phase_launch(&state, execution_id, phase_id, &consumer.binding)
            .map_err(WorkerError::Rejected)?;
        if launch.envelope().node_id != self.node
            || &launch.envelope().target_host_gate != self.identity()
        {
            return Err(WorkerError::Binding);
        }
        if launch.envelope().phase.as_ref().is_some_and(|p| {
            matches!(
                p.kind,
                PhaseKind::Archive | PhaseKind::List | PhaseKind::Test
            )
        }) {
            self.resources
                .require_cargo_shim(&launch.input.input.command)?;
        }
        self.fence_start(&state, &launch.envelope().action_id)?;
        Ok(host_gate::start_phase(
            &mut self.gate,
            &launch,
            observations,
            ctx,
        )?)
    }

    fn fence_start(&mut self, state: &CampaignState, starting: &str) -> Result<(), WorkerError> {
        let run = state.run.as_ref().ok_or(WorkerError::Binding)?;
        let fence = self.apply_fences(state)?;
        if let Some(action) = self.other_incarnation(run) {
            return Err(WorkerError::Reconcile(action.id.clone()));
        }
        for entry in &fence.entries {
            let action = self.check_entry(run, entry)?;
            match recovery_turn(action, entry) {
                Some(WorkerTurn::LaunchNotSent { .. }) => {
                    return Err(WorkerError::Reconcile(action.id.clone()));
                }
                Some(_) if action.id != starting => {
                    return Err(WorkerError::Reconcile(action.id.clone()));
                }
                _ => {}
            }
        }
        if let Some(action) = run.actions.values().find(|a| {
            a.binding.node_id == self.node && a.id != starting && a.state == ActionState::Unknown
        }) {
            return Err(WorkerError::Reconcile(action.id.clone()));
        }
        Ok(())
    }

    pub fn finish_phase(
        &mut self,
        action_id: &str,
        result: &PhaseResult,
        now: DateTime<Utc>,
    ) -> Result<(), WorkerError> {
        let entry = self.gate.entry(action_id).ok_or(WorkerError::Binding)?;
        if entry.identity.kind != ActionKind::VerificationPhase {
            return Err(WorkerError::Binding);
        }
        Ok(host_gate::finish_phase(
            &mut self.gate,
            action_id,
            result,
            now,
        )?)
    }

    pub fn resolve_indeterminate(
        &mut self,
        action_id: &str,
        initiation_id: &str,
        resolution: Resolution,
        now: DateTime<Utc>,
    ) -> Result<(), WorkerError> {
        Ok(self
            .gate
            .resolve_indeterminate(action_id, initiation_id, resolution, now)?)
    }

    async fn wait_for_headroom(&self, wait: Duration) -> Result<(), WorkerError> {
        match self.resources.admission(wait).await {
            ResourceAdmission::Ready => Ok(()),
            refusal => Err(WorkerError::Resource(refusal)),
        }
    }
}

fn render_receipt(outcome: &CommandOutcome) -> StoreResult<ReceiptResponse> {
    Ok(ReceiptResponse {
        status: 200,
        body: serde_json::to_value(outcome)?,
    })
}

fn finished_action(recovered: &Recovered, id: &str) -> bool {
    matches!(recovered, Recovered::Finished { action_id, .. } if action_id == id)
}

fn recovery_turn(action: &Action, entry: &JournalEntry) -> Option<WorkerTurn> {
    let action_id = action.id.clone();
    match &entry.state {
        JournalState::Finished { .. } if !entry.delivered => {
            Some(WorkerTurn::Reconcile { action_id })
        }
        _ if matches!(
            action.state,
            ActionState::Reconciled | ActionState::Cancelled
        ) =>
        {
            None
        }
        JournalState::NotSent { evidence } if action.kind == ActionKind::Launch => {
            Some(WorkerTurn::LaunchNotSent {
                action_id,
                evidence: evidence.clone(),
            })
        }
        JournalState::Initiating
        | JournalState::Indeterminate { .. }
        | JournalState::Cancelled { .. } => Some(WorkerTurn::Reconcile { action_id }),
        JournalState::Started { receipt, .. }
            if action.initiation_receipt.as_ref() != Some(receipt) =>
        {
            Some(WorkerTurn::Reconcile { action_id })
        }
        _ => None,
    }
}

fn matches_step(step: &NextAction, command: &Command, node: &str, target: &HostGate) -> bool {
    match (step, command) {
        (NextAction::OpenGate { gate }, Command::OpenApprovalGate { binding }) => {
            binding.node_id == node && binding.gate == *gate
        }
        (
            NextAction::AssignWork { key },
            Command::AssignWork {
                binding,
                assignment,
                launch,
            },
        ) => {
            binding.run_id == key.run_id
                && binding.node_id == key.node_id
                && binding.attempt_id == key.attempt_id(Stage::Implementation)
                && launch.id == key.launch_id(Stage::Implementation)
                && [&binding.session.gate, &assignment.gate, &launch.target]
                    .into_iter()
                    .all(|gate| gate == target)
        }
        (
            NextAction::BeginReview { key, work_attempt },
            Command::BeginReview {
                node_id,
                expected_epoch,
                seal,
                review,
                launch,
            },
        ) => {
            node_id == &key.node_id
                && *expected_epoch == key.epoch
                && &seal.owner_attempt == work_attempt
                && review.run_id == key.run_id
                && review.attempt_id == key.attempt_id(Stage::Review)
                && launch.id == key.launch_id(Stage::Review)
                && [&review.session.gate, &launch.target]
                    .into_iter()
                    .all(|gate| gate == target)
        }
        (
            NextAction::BeginLanding { key },
            Command::BeginLanding {
                expected_run,
                node_id,
                expected_epoch,
            },
        )
        | (
            NextAction::CompleteNode { key },
            Command::CompleteNode {
                expected_run,
                node_id,
                expected_epoch,
            },
        ) => expected_run == &key.run_id && node_id == &key.node_id && *expected_epoch == key.epoch,
        _ => false,
    }
}

#[cfg(test)]
#[path = "host_worker_tests.rs"]
mod tests;
