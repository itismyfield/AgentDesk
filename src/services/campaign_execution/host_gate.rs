use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::adk_resources::ResourceObservation;
use super::host_journal::{JournalDir, OpenError};
use super::model::{ActionKind, FenceAcknowledgement, HostGate, StopMode};
use super::verification_run::{
    AuthoritySource, BoundInput, Digest, NotSentReason, PhaseInitiation, PhaseKind, PhaseResult,
    ProcessIdentity, TerminationObservation, TreeState, VerificationRun,
    VerificationStartAuthority,
};
use super::verification_runner::{self, Launch, LaunchContext, RunningPhase};

const FENCES: &str = "fences.json";
const HOME: &str = "gate.json";

// Who may start an effect now. It is never part of the effect's identity or payload, so a handover
// changes the holder without changing the effect or its action ID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "holder", rename_all = "snake_case")]
pub enum EffectAuthority {
    CurrentAttempt {
        attempt_id: String,
        binding_digest: Digest,
        node_epoch: u64,
        run_control_version: u64,
    },
    Verification(VerificationStartAuthority),
}

impl EffectAuthority {
    fn fence(&self) -> (u64, u64) {
        match self {
            Self::CurrentAttempt {
                run_control_version,
                node_epoch,
                ..
            } => (*run_control_version, *node_epoch),
            Self::Verification(a) => (a.run_control_version, a.consumer_node_epoch),
        }
    }

    // An authority only ever speaks for the effect it was resolved for.
    fn names(&self, effect: &EffectIdentity) -> bool {
        match self {
            Self::CurrentAttempt {
                attempt_id,
                binding_digest,
                ..
            } => {
                effect.kind != ActionKind::VerificationPhase
                    && (attempt_id, binding_digest)
                        == (&effect.origin_attempt_id, &effect.origin_binding_digest)
            }
            Self::Verification(a) => {
                effect.kind == ActionKind::VerificationPhase
                    && a.original_binding_digest == effect.origin_binding_digest
            }
        }
    }

    fn finish_plan(&self) -> bool {
        matches!(self, Self::Verification(a) if matches!(a.source, AuthoritySource::FinishPlan { .. }))
    }
}

// One effect as the final gate sees it: the identity and payload fixed when the action was
// registered, and the authority the server resolved from stored state for this request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectEnvelope {
    pub action_id: String,
    pub run_id: String,
    pub node_id: String,
    pub origin_attempt_id: String,
    pub origin_binding_digest: Digest,
    pub target_host_gate: HostGate,
    pub payload_digest: Digest,
    pub execution_nonce: String,
    pub kind: ActionKind,
    pub authority: EffectAuthority,
}

// What a journal entry must match exactly; a different value under the same action ID conflicts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectIdentity {
    pub action_id: String,
    pub run_id: String,
    pub node_id: String,
    pub origin_attempt_id: String,
    pub origin_binding_digest: Digest,
    pub target: HostGate,
    pub payload_digest: Digest,
    pub kind: ActionKind,
}

impl EffectEnvelope {
    pub fn identity(&self) -> EffectIdentity {
        EffectIdentity {
            action_id: self.action_id.clone(),
            run_id: self.run_id.clone(),
            node_id: self.node_id.clone(),
            origin_attempt_id: self.origin_attempt_id.clone(),
            origin_binding_digest: self.origin_binding_digest.clone(),
            target: self.target_host_gate.clone(),
            payload_digest: self.payload_digest.clone(),
            kind: self.kind,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateRefusal {
    // Another holder may still start the same action: nothing is recorded against it.
    StaleAuthority,
    // No holder can start it any more on this run: the unstarted action is cancelled here.
    Revoked,
    // The initiator itself declined before starting anything, e.g. a resource wait.
    Initiator,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "initiation", rename_all = "snake_case")]
pub enum Initiation {
    NotSent {
        reason: GateRefusal,
        evidence: String,
    },
    Started {
        receipt: String,
        process: Option<ProcessIdentity>,
    },
    Indeterminate {
        initiation_id: String,
        evidence: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum GateError {
    #[error("the gate directory is held by another writer")]
    Busy,
    #[error("the effect targets another host, gate or incarnation: {0}")]
    WrongHost(String),
    #[error("effect {0} was recorded with a different identity, payload or receipt")]
    Conflict(String),
    #[error("the host has not applied the fence this authority names")]
    FenceBehind,
    #[error("the initiator failed before starting: {0}")]
    Initiator(String),
    #[error("journal: {0}")]
    Journal(String),
}

fn journal_error(error: io::Error) -> GateError {
    GateError::Journal(error.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostStop {
    pub mode: StopMode,
    // FinishStage keeps exactly the frozen FinishPlan phase actions; Immediate keeps none.
    pub finish_actions: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFence {
    pub run_id: String,
    pub control_version: u64,
    pub stop: Option<HostStop>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostNodeFence {
    pub run_id: String,
    pub node_id: String,
    pub epoch: u64,
    pub interrupted: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct FenceBook {
    runs: BTreeMap<String, RunFence>,
    nodes: Vec<HostNodeFence>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum JournalState {
    // Durable before the initiator runs; found after a crash it means the start may have happened.
    Initiating,
    NotSent {
        evidence: String,
    },
    Started {
        receipt: String,
        process: Option<ProcessIdentity>,
    },
    Finished {
        receipt: String,
        process: Option<ProcessIdentity>,
        result: serde_json::Value,
    },
    Indeterminate {
        evidence: String,
    },
    Cancelled {
        evidence: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    pub identity: EffectIdentity,
    pub nonce: String,
    // Counts initiator runs, so each possibly-lost start has its own initiation ID.
    pub attempts: u32,
    pub authority: Option<EffectAuthority>,
    pub state: JournalState,
    pub updated_at: DateTime<Utc>,
}

impl JournalEntry {
    pub fn initiation_id(&self) -> String {
        format!("{}#{}", self.identity.action_id, self.attempts)
    }
}

pub struct Admitted<T> {
    pub initiation: Initiation,
    // Present only when this call started the effect.
    pub handle: Option<T>,
}

pub enum Initiated<T> {
    NotSent {
        evidence: String,
    },
    Started {
        receipt: String,
        process: Option<ProcessIdentity>,
        handle: T,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FenceReport {
    pub acknowledgement: FenceAcknowledgement,
    // This run's journal, so each in-flight action can be classified as started, unstarted or unknown.
    pub entries: Vec<JournalEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestartObservation {
    Running {
        action_id: String,
        receipt: String,
        process: ProcessIdentity,
        tree: TreeState,
        observed_at: DateTime<Utc>,
    },
    Indeterminate {
        action_id: String,
        initiation_id: String,
        evidence: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    Started {
        receipt: String,
        process: Option<ProcessIdentity>,
    },
    Unstarted {
        evidence: String,
    },
}

enum Check {
    Admit,
    Behind,
    Stale(String),
    Revoked(String),
}

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
struct GateHome {
    host: String,
    gate: String,
}

// The host's final gate: the last check before an effect starts, serialized with fence updates
// under one writer, with every start journaled durably before and after it happens.
pub struct EffectGate {
    identity: HostGate,
    journal: JournalDir,
    fences: FenceBook,
    entries: BTreeMap<String, JournalEntry>,
}

impl EffectGate {
    // Opens the directory as its single writer. A start whose receipt was never recorded becomes
    // Indeterminate and is only ever resolved by direct reconciliation, never by starting again.
    pub fn recover_journal(
        root: &Path,
        identity: HostGate,
        now: DateTime<Utc>,
    ) -> Result<Self, GateError> {
        let journal = JournalDir::open(root).map_err(|error| match error {
            OpenError::Busy => GateError::Busy,
            OpenError::Io(error) => journal_error(error),
        })?;
        let home = GateHome {
            host: identity.host.clone(),
            gate: identity.gate.clone(),
        };
        match journal.read::<GateHome>(HOME).map_err(journal_error)? {
            Some(held) if held != home => {
                return Err(GateError::WrongHost(format!("{}/{}", held.host, held.gate)));
            }
            Some(_) => {}
            None => journal.write(HOME, &home).map_err(journal_error)?,
        }
        let fences = journal.read(FENCES).map_err(journal_error)?;
        let mut gate = Self {
            identity,
            fences: fences.unwrap_or_default(),
            entries: BTreeMap::new(),
            journal,
        };
        for entry in gate
            .journal
            .entries::<JournalEntry>()
            .map_err(journal_error)?
        {
            gate.entries.insert(entry.identity.action_id.clone(), entry);
        }
        let lost: Vec<String> = gate
            .entries
            .values()
            .filter(|e| e.state == JournalState::Initiating)
            .map(|e| e.identity.action_id.clone())
            .collect();
        for id in lost {
            let evidence = "host restarted before the initiation receipt was recorded".into();
            gate.transition(&id, JournalState::Indeterminate { evidence }, now)?;
        }
        Ok(gate)
    }

    pub fn identity(&self) -> &HostGate {
        &self.identity
    }

    pub fn entry(&self, action_id: &str) -> Option<&JournalEntry> {
        self.entries.get(action_id)
    }

    fn store(&mut self, entry: JournalEntry) -> Result<(), GateError> {
        let id = entry.identity.action_id.clone();
        self.journal
            .write_entry(&id, &entry)
            .map_err(journal_error)?;
        self.entries.insert(id, entry);
        Ok(())
    }

    fn transition(
        &mut self,
        action_id: &str,
        state: JournalState,
        now: DateTime<Utc>,
    ) -> Result<(), GateError> {
        let mut entry = self
            .entries
            .get(action_id)
            .cloned()
            .ok_or_else(|| GateError::Conflict(action_id.into()))?;
        entry.state = state;
        entry.updated_at = now;
        self.store(entry)
    }

    fn store_fences(&mut self, book: FenceBook) -> Result<(), GateError> {
        if book != self.fences {
            self.journal.write(FENCES, &book).map_err(journal_error)?;
            self.fences = book;
        }
        Ok(())
    }

    // Applies a run fence durably before acknowledging it. Versions only move forward; at the same
    // version a stop may only narrow the actions it keeps, as a node interruption does.
    pub fn apply_run_fence(&mut self, fence: RunFence) -> Result<FenceReport, GateError> {
        let mut book = self.fences.clone();
        match book.runs.get(&fence.run_id) {
            Some(held) if held.control_version > fence.control_version => {}
            Some(held) if held.control_version == fence.control_version => {
                let narrows = match (&held.stop, &fence.stop) {
                    (None, None) => true,
                    (Some(held), Some(next)) => {
                        held.mode == next.mode
                            && next.finish_actions.is_subset(&held.finish_actions)
                    }
                    _ => false,
                };
                if !narrows {
                    return Err(GateError::Conflict(format!("run fence {}", fence.run_id)));
                }
                book.runs.insert(fence.run_id.clone(), fence.clone());
            }
            _ => {
                book.runs.insert(fence.run_id.clone(), fence.clone());
            }
        }
        self.store_fences(book)?;
        let control_version = self.fences.runs[&fence.run_id].control_version;
        Ok(FenceReport {
            acknowledgement: FenceAcknowledgement {
                target: self.identity.clone(),
                control_version,
            },
            entries: self
                .entries
                .values()
                .filter(|e| e.identity.run_id == fence.run_id)
                .cloned()
                .collect(),
        })
    }

    pub fn apply_node_fence(&mut self, fence: HostNodeFence) -> Result<(), GateError> {
        let mut book = self.fences.clone();
        let held = book
            .nodes
            .iter_mut()
            .find(|n| n.run_id == fence.run_id && n.node_id == fence.node_id);
        match held {
            Some(held) if (held.epoch, held.interrupted) >= (fence.epoch, fence.interrupted) => {
                return Ok(());
            }
            Some(held) => *held = fence,
            None => book.nodes.push(fence),
        }
        self.store_fences(book)
    }

    // Checks the authority against the fences this host applied. Revocation (interrupted node, or
    // a stop that does not keep the action) ends every holder; an older holder is only stale.
    fn check(&self, envelope: &EffectEnvelope) -> Check {
        let run = self.fences.runs.get(&envelope.run_id);
        let node = self
            .fences
            .nodes
            .iter()
            .find(|n| n.run_id == envelope.run_id && n.node_id == envelope.node_id);
        let (Some(run), Some(node)) = (run, node) else {
            return Check::Behind;
        };
        if node.interrupted {
            return Check::Revoked(format!(
                "node {} interrupted at epoch {}",
                node.node_id, node.epoch
            ));
        }
        if let Some(stop) = &run.stop
            && !(stop.mode == StopMode::FinishStage
                && stop.finish_actions.contains(&envelope.action_id))
        {
            return Check::Revoked(format!(
                "run stopped {:?} at control version {}",
                stop.mode, run.control_version
            ));
        }
        let (version, epoch) = envelope.authority.fence();
        if version > run.control_version || epoch > node.epoch {
            return Check::Behind;
        }
        if version < run.control_version
            || epoch < node.epoch
            || run.stop.is_some() != envelope.authority.finish_plan()
        {
            return Check::Stale(format!(
                "authority at control version {version} epoch {epoch}; host holds {} / {}",
                run.control_version, node.epoch
            ));
        }
        Check::Admit
    }

    // Replays answer from the journal; a new start passes the final check, records its intent
    // durably, starts, and records the receipt durably before it is returned.
    pub fn admit_and_initiate<T>(
        &mut self,
        envelope: &EffectEnvelope,
        now: DateTime<Utc>,
        initiate: impl FnOnce(&str) -> io::Result<Initiated<T>>,
    ) -> Result<Admitted<T>, GateError> {
        let identity = envelope.identity();
        let recorded = |initiation| {
            Ok(Admitted {
                initiation,
                handle: None,
            })
        };
        if (&identity.target.host, &identity.target.gate)
            != (&self.identity.host, &self.identity.gate)
        {
            return Err(GateError::WrongHost(identity.target.host));
        }
        if !envelope.authority.names(&identity) {
            return Err(GateError::Conflict(identity.action_id));
        }
        let previous = self.entries.get(&identity.action_id);
        if let Some(entry) = previous {
            if entry.identity != identity {
                return Err(GateError::Conflict(identity.action_id));
            }
            match &entry.state {
                JournalState::Started { receipt, process }
                | JournalState::Finished {
                    receipt, process, ..
                } => {
                    return recorded(Initiation::Started {
                        receipt: receipt.clone(),
                        process: process.clone(),
                    });
                }
                JournalState::Initiating => {
                    return recorded(Initiation::Indeterminate {
                        initiation_id: entry.initiation_id(),
                        evidence: "initiation receipt was never recorded".into(),
                    });
                }
                JournalState::Indeterminate { evidence } => {
                    return recorded(Initiation::Indeterminate {
                        initiation_id: entry.initiation_id(),
                        evidence: evidence.clone(),
                    });
                }
                JournalState::Cancelled { evidence } => {
                    return recorded(Initiation::NotSent {
                        reason: GateRefusal::Revoked,
                        evidence: evidence.clone(),
                    });
                }
                JournalState::NotSent { .. } => {}
            }
        }
        if identity.target != self.identity {
            return Err(GateError::WrongHost(identity.target.incarnation));
        }
        let attempts = previous.map_or(0, |e| e.attempts);
        let entry = |state| JournalEntry {
            identity: identity.clone(),
            nonce: envelope.execution_nonce.clone(),
            attempts,
            authority: Some(envelope.authority.clone()),
            state,
            updated_at: now,
        };
        match self.check(envelope) {
            Check::Admit => {}
            Check::Behind => return Err(GateError::FenceBehind),
            Check::Stale(evidence) => {
                return recorded(Initiation::NotSent {
                    reason: GateRefusal::StaleAuthority,
                    evidence,
                });
            }
            Check::Revoked(evidence) => {
                self.store(entry(JournalState::Cancelled {
                    evidence: evidence.clone(),
                }))?;
                return recorded(Initiation::NotSent {
                    reason: GateRefusal::Revoked,
                    evidence,
                });
            }
        }
        let mut intent = entry(JournalState::Initiating);
        intent.attempts += 1;
        self.store(intent)?;
        let id = identity.action_id;
        match initiate(&envelope.execution_nonce) {
            Ok(Initiated::Started {
                receipt,
                process,
                handle,
            }) => {
                let state = JournalState::Started {
                    receipt: receipt.clone(),
                    process: process.clone(),
                };
                self.transition(&id, state, now)?;
                Ok(Admitted {
                    initiation: Initiation::Started { receipt, process },
                    handle: Some(handle),
                })
            }
            Ok(Initiated::NotSent { evidence }) => {
                let state = JournalState::NotSent {
                    evidence: evidence.clone(),
                };
                self.transition(&id, state, now)?;
                recorded(Initiation::NotSent {
                    reason: GateRefusal::Initiator,
                    evidence,
                })
            }
            Err(error) => {
                let evidence = error.to_string();
                let state = JournalState::NotSent {
                    evidence: evidence.clone(),
                };
                self.transition(&id, state, now)?;
                Err(GateError::Initiator(evidence))
            }
        }
    }

    // Keeps the observed result, so a replay after a lost response returns it instead of rerunning.
    pub fn record_finished(
        &mut self,
        action_id: &str,
        receipt: &str,
        result: serde_json::Value,
        now: DateTime<Utc>,
    ) -> Result<(), GateError> {
        let conflict = || GateError::Conflict(action_id.into());
        let entry = self.entries.get(action_id).ok_or_else(conflict)?;
        let process = match &entry.state {
            JournalState::Started {
                receipt: started,
                process,
            } if started == receipt => process.clone(),
            JournalState::Finished {
                receipt: finished,
                result: held,
                ..
            } if finished == receipt && *held == result => return Ok(()),
            _ => return Err(conflict()),
        };
        let state = JournalState::Finished {
            receipt: receipt.into(),
            process,
            result,
        };
        self.transition(action_id, state, now)
    }

    // A direct observation of a possibly-lost start; it names the exact initiation it resolves.
    pub fn resolve_indeterminate(
        &mut self,
        action_id: &str,
        initiation_id: &str,
        resolution: Resolution,
        now: DateTime<Utc>,
    ) -> Result<(), GateError> {
        let conflict = || GateError::Conflict(action_id.into());
        let entry = self.entries.get(action_id).ok_or_else(conflict)?;
        if !matches!(entry.state, JournalState::Indeterminate { .. })
            || entry.initiation_id() != initiation_id
        {
            return Err(conflict());
        }
        let state = match resolution {
            Resolution::Started { receipt, process } => JournalState::Started { receipt, process },
            Resolution::Unstarted { evidence } => JournalState::NotSent { evidence },
        };
        self.transition(action_id, state, now)
    }

    // What survived a restart without a runner handle: each started process as its tree is seen
    // now, and each start that may or may not have happened. Neither is ever a finished result.
    pub fn restart_observations(
        &self,
        probe: impl Fn(&ProcessIdentity) -> TreeState,
        now: DateTime<Utc>,
    ) -> Vec<RestartObservation> {
        let mut observations = Vec::new();
        for entry in self.entries.values() {
            let action_id = entry.identity.action_id.clone();
            match &entry.state {
                JournalState::Started {
                    receipt,
                    process: Some(process),
                } => observations.push(RestartObservation::Running {
                    action_id,
                    receipt: receipt.clone(),
                    process: process.clone(),
                    tree: probe(process),
                    observed_at: now,
                }),
                JournalState::Indeterminate { evidence } => {
                    observations.push(RestartObservation::Indeterminate {
                        action_id,
                        initiation_id: entry.initiation_id(),
                        evidence: evidence.clone(),
                    })
                }
                _ => {}
            }
        }
        observations
    }
}

pub struct PhaseStart {
    pub initiation: PhaseInitiation,
    pub running: Option<RunningPhase>,
}

// The only path that spawns a verification phase: the runner starts inside the gate, with the
// effect's nonce, after the journal holds the intent and the final check passed.
pub fn start_phase(
    gate: &mut EffectGate,
    envelope: &EffectEnvelope,
    kind: PhaseKind,
    phase_id: &str,
    bound: &BoundInput,
    resources: &[ResourceObservation],
    ctx: LaunchContext<'_>,
) -> Result<PhaseStart, GateError> {
    let now = ctx.now;
    let mut declined = None;
    let admitted = gate.admit_and_initiate(envelope, now, |nonce| {
        let ctx = LaunchContext {
            nonce: nonce.to_owned(),
            ..ctx
        };
        Ok(
            match verification_runner::launch(kind, phase_id, bound, resources, ctx)? {
                Launch::Started(running) => {
                    let process = running.process().clone();
                    Initiated::Started {
                        receipt: running.receipt().to_owned(),
                        process: Some(process),
                        handle: *running,
                    }
                }
                Launch::NotSent(initiation) => {
                    let evidence = format!("{initiation:?}");
                    declined = Some(initiation);
                    Initiated::NotSent { evidence }
                }
            },
        )
    })?;
    let not_sent = |reason, evidence| PhaseInitiation::NotSent {
        reason,
        evidence,
        observed_at: now,
    };
    let initiation = match admitted.initiation {
        Initiation::Started {
            receipt,
            process: Some(process),
        } => PhaseInitiation::Started { receipt, process },
        Initiation::Started { process: None, .. } => {
            return Err(GateError::Conflict(envelope.action_id.clone()));
        }
        Initiation::Indeterminate {
            initiation_id,
            evidence,
        } => PhaseInitiation::Indeterminate {
            initiation_id,
            evidence,
            observed_at: now,
        },
        Initiation::NotSent {
            reason: GateRefusal::StaleAuthority,
            evidence,
        } => not_sent(NotSentReason::StaleAuthority, evidence),
        Initiation::NotSent {
            reason: GateRefusal::Revoked,
            evidence,
        } => not_sent(NotSentReason::AuthorityRevoked, evidence),
        Initiation::NotSent {
            reason: GateRefusal::Initiator,
            evidence,
        } => match declined {
            Some(initiation) => initiation,
            None => return Err(GateError::Initiator(evidence)),
        },
    };
    Ok(PhaseStart {
        initiation,
        running: admitted.handle,
    })
}

pub fn finish_phase(
    gate: &mut EffectGate,
    action_id: &str,
    result: &PhaseResult,
    now: DateTime<Utc>,
) -> Result<(), GateError> {
    let value = serde_json::to_value(result).map_err(|e| GateError::Journal(e.to_string()))?;
    gate.record_finished(action_id, &result.receipt, value, now)
}

// A restart observation of a running phase, named by the V and phase that own its action.
pub fn termination_observation(
    v: &VerificationRun,
    observation: &RestartObservation,
) -> Option<TerminationObservation> {
    let RestartObservation::Running {
        action_id,
        receipt,
        process,
        tree,
        observed_at,
    } = observation
    else {
        return None;
    };
    let phase = v.phases.iter().find(|p| p.action_id == *action_id)?;
    Some(TerminationObservation {
        execution_id: v.origin.execution_id.clone(),
        action_id: action_id.clone(),
        phase_id: phase.id.clone(),
        receipt: receipt.clone(),
        process: process.clone(),
        tree: *tree,
        evidence: format!("host journal {receipt}: process tree {tree:?} after restart"),
        observed_at: *observed_at,
    })
}

#[cfg(test)]
#[path = "host_gate_tests.rs"]
mod tests;
