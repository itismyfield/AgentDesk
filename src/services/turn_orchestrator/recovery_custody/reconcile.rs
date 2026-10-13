//! Reconciles an operation from its durable progress, the canonical owner's observation and
//! whether the earlier execution can still run; only a truly undecided case holds.

use super::model::{CanonicalSeal, CustodyRecord, IssuedEpisode, Observed, Phase};
use super::operation::{ActiveOperation, OperationKind, Root, Stage, StageStatus};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    RecheckApply,
    CompleteReturn,
    RevalidateInstall,
    ConfirmExactRelease,
}

/// Whether the earlier execution of the operation can still apply anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PriorExecution {
    /// Revoked, cooperatively ended, or its process is gone.
    Ended,
    /// This process incarnation still runs it (a live lease, the actual actor).
    SameOwnerLive {
        process_incarnation: String,
    },
    Unknown,
}

/// How an owned return gets its sources back into the queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReturnPath {
    /// The same process still holds the dispatch lease and returns through it.
    LiveLease,
    /// A new process rebuilds the return from the canonical payload; the old lease is gone.
    DurableReconstruct,
    /// The queue already holds the sources; only the exact Marker and the reservation remain.
    CleanupOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReleaseEvidence {
    pub(crate) episode: IssuedEpisode,
    pub(crate) released: bool,
    /// Pending and unacknowledged debt completed or handed over.
    pub(crate) owed_settled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Evidence {
    /// The canonical stores as their owning node sees them; `None` when that could not be read.
    pub(crate) observed: Option<CanonicalSeal>,
    pub(crate) prior: PriorExecution,
    /// The issued identity of the actual actor installed in this process, Pending registered.
    pub(crate) live_actor: Option<IssuedEpisode>,
    /// The queue-exit debt event-id found in the canonical queue.
    pub(crate) debt: Option<String>,
    pub(crate) release: Option<ReleaseEvidence>,
    /// A later claim or external effect was already granted after this step.
    pub(crate) followup_granted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Nothing was applied and nothing can be; the reservation ends with no responsibility.
    NoApply,
    /// The same operation re-arms `stage`, reusing any debt it was already granted.
    Resume {
        stage: Stage,
        reuse: Option<String>,
    },
    /// The stage's effect is confirmed; responsibility stays with the operation.
    Confirmed,
    /// A legacy claim bound to its executing owner.
    Owned {
        by: String,
    },
    /// The operation becomes an owned return of its sources.
    Return {
        path: ReturnPath,
    },
    /// Responsibility ended with evidence.
    Close,
    Held {
        missing: &'static str,
    },
    /// The action does not fit the operation; nothing is written.
    Refused {
        reason: &'static str,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NoApplyReason {
    Empty,
    StaleSelection,
    TransitionBusy,
}

/// What a legacy consumer reported for its dequeue/recovery/claim sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LegacyResult {
    NoApply(NoApplyReason),
    LocalClaim {
        bound_to: Option<String>,
    },
    ConsumedToHold {
        hold_id: Option<String>,
        handed_off: bool,
    },
    Forwarded {
        receipt: Option<String>,
    },
    Duplicate {
        evidence: Option<String>,
    },
    /// Capped restore, cancellation or error: the sources must go back through an owned return.
    ReturnRequired,
    Unknown,
}

fn last_debt(op: &ActiveOperation) -> Option<String> {
    op.handoffs.last().cloned()
}

/// A legacy result ends the responsibility only with the evidence its kind names.
pub(crate) fn legacy_verdict(op: &ActiveOperation, result: &LegacyResult) -> Verdict {
    match result {
        LegacyResult::NoApply(_)
            if !op.has_applied() && op.status != StageStatus::Indeterminate =>
        {
            Verdict::NoApply
        }
        LegacyResult::LocalClaim { bound_to: Some(by) } => Verdict::Owned { by: by.clone() },
        LegacyResult::ConsumedToHold {
            hold_id: Some(_),
            handed_off: true,
        }
        | LegacyResult::Forwarded { receipt: Some(_) }
        | LegacyResult::Duplicate { evidence: Some(_) } => Verdict::Close,
        LegacyResult::ReturnRequired => Verdict::Return {
            path: ReturnPath::LiveLease,
        },
        LegacyResult::NoApply(_) => Verdict::Held {
            missing: "proof that nothing was applied",
        },
        _ => Verdict::Held {
            missing: "the legacy result's evidence",
        },
    }
}

fn present(observed: &Observed) -> bool {
    matches!(observed, Observed::Present { .. })
}

/// The six-row decision for an operation's current stage.
pub(crate) fn decide(record: &CustodyRecord, action: Action, evidence: &Evidence) -> Verdict {
    let Some(op) = &record.active_operation else {
        return Verdict::Refused {
            reason: "no active operation",
        };
    };
    // Another node's stale local files never stand in for the canonical owner's view.
    let Some(seen) = evidence.observed.as_ref().filter(|seen| {
        (&seen.owner_node, &seen.namespace) == (&op.before.owner_node, &op.before.namespace)
    }) else {
        return Verdict::Held {
            missing: "an observation by the canonical owner",
        };
    };
    let ended = evidence.prior == PriorExecution::Ended;
    let same_live = matches!(&evidence.prior, PriorExecution::SameOwnerLive { process_incarnation }
        if *process_incarnation == record.owner.process_incarnation);
    match op.status {
        StageStatus::ReservedNotArmed if !op.has_applied() => {
            if action != Action::RecheckApply {
                return Verdict::Refused {
                    reason: "a reservation is only rechecked",
                };
            }
            if *seen == op.before {
                return Verdict::NoApply;
            }
            return Verdict::Held {
                missing: "stores that still match the reservation",
            };
        }
        StageStatus::ReservedNotArmed | StageStatus::KnownOutcome => {
            return Verdict::Refused {
                reason: "nothing is undecided",
            };
        }
        StageStatus::ApplyArmed | StageStatus::Indeterminate => {}
    }
    let Some(expected) = op.expected_after.as_ref() else {
        return Verdict::Held {
            missing: "the armed stage's expected state",
        };
    };
    match op.stage {
        Stage::MarkerPublish if present(&expected.marker) && seen.marker == expected.marker => {
            if seen.queue != op.before.queue {
                return Verdict::Held {
                    missing: "a queue still in its old state",
                };
            }
            marker_only(action, ended, evidence.followup_granted)
        }
        Stage::QueueCommit if seen.queue == expected.queue && seen.marker == expected.marker => {
            queue_committed(op, action, evidence, ended, same_live)
        }
        Stage::ActorInstall => actor_install(record, op, action, evidence, seen, ended, same_live),
        Stage::SourceReturn => source_return(op, expected, action, seen, ended, same_live),
        Stage::ExactRelease => match (&evidence.release, action) {
            (Some(release), Action::ConfirmExactRelease)
                if Some(&release.episode) == record.issued_episode.as_ref()
                    && release.released
                    && release.owed_settled =>
            {
                Verdict::Close
            }
            _ => Verdict::Held {
                missing: "the exact release result",
            },
        },
        // An armed step whose stores still read as before, with its execution ended, never published.
        stage if *seen == op.before && ended && action == Action::RecheckApply => Verdict::Resume {
            stage,
            reuse: last_debt(op),
        },
        _ => Verdict::Held {
            missing: "the step's outcome",
        },
    }
}

fn marker_only(action: Action, ended: bool, followup_granted: bool) -> Verdict {
    if !ended {
        return Verdict::Held {
            missing: "the end of the earlier execution",
        };
    }
    if followup_granted {
        return Verdict::Held {
            missing: "the result of the granted follow-up",
        };
    }
    match action {
        Action::RecheckApply => Verdict::Resume {
            stage: Stage::QueueCommit,
            reuse: None,
        },
        // The queue still holds the sources, so returning them leaves only the exact Marker.
        Action::CompleteReturn => Verdict::Return {
            path: ReturnPath::CleanupOnly,
        },
        _ => Verdict::Refused {
            reason: "a published Marker resumes or returns",
        },
    }
}

fn queue_committed(
    op: &ActiveOperation,
    action: Action,
    evidence: &Evidence,
    ended: bool,
    same_live: bool,
) -> Verdict {
    let debt = last_debt(op);
    if op.root == Root::Scoped && (debt.is_none() || evidence.debt != debt) {
        return Verdict::Held {
            missing: "the debt this exit was issued",
        };
    }
    match action {
        Action::RecheckApply => Verdict::Confirmed,
        Action::RevalidateInstall if op.root == Root::Scoped => Verdict::Resume {
            stage: Stage::ActorInstall,
            reuse: debt,
        },
        Action::CompleteReturn if same_live => Verdict::Return {
            path: ReturnPath::LiveLease,
        },
        Action::CompleteReturn if ended => Verdict::Return {
            path: ReturnPath::DurableReconstruct,
        },
        Action::CompleteReturn => Verdict::Held {
            missing: "the end of the earlier execution",
        },
        _ => Verdict::Refused {
            reason: "a committed exit is confirmed, revalidated or returned",
        },
    }
}

fn actor_install(
    record: &CustodyRecord,
    op: &ActiveOperation,
    action: Action,
    evidence: &Evidence,
    seen: &CanonicalSeal,
    ended: bool,
    same_live: bool,
) -> Verdict {
    if action != Action::RevalidateInstall {
        return Verdict::Refused {
            reason: "an install is revalidated",
        };
    }
    let issued = record.issued_episode.as_ref();
    if same_live {
        // An empty slot is not proof the claim never ran.
        return match evidence.live_actor.as_ref() {
            Some(actor) if Some(actor) == issued => Verdict::Confirmed,
            _ => Verdict::Held {
                missing: "the installed actor",
            },
        };
    }
    let debt = last_debt(op);
    let rebuildable = op.kind == OperationKind::ScopedInstall
        && issued.is_some()
        && op.expected_after.as_ref().map(|after| &after.inflight) == Some(&seen.inflight)
        && (debt.is_none() || evidence.debt == debt)
        && !evidence.followup_granted;
    if ended && rebuildable {
        return Verdict::Resume {
            stage: Stage::ActorInstall,
            reuse: debt,
        };
    }
    Verdict::Held {
        missing: "the install's durable admission, episode and debt",
    }
}

fn source_return(
    op: &ActiveOperation,
    expected: &CanonicalSeal,
    action: Action,
    seen: &CanonicalSeal,
    ended: bool,
    same_live: bool,
) -> Verdict {
    if action != Action::CompleteReturn {
        return Verdict::Refused {
            reason: "a return is completed",
        };
    }
    if seen.queue == expected.queue {
        return if seen.marker == Observed::Absent {
            Verdict::Close
        } else {
            Verdict::Return {
                path: ReturnPath::CleanupOnly,
            }
        };
    }
    if seen.queue != op.before.queue {
        return Verdict::Held {
            missing: "a queue in its old or returned state",
        };
    }
    if same_live {
        return Verdict::Return {
            path: ReturnPath::LiveLease,
        };
    }
    if ended && present(&seen.marker) && seen.marker == op.before.marker {
        return Verdict::Return {
            path: ReturnPath::DurableReconstruct,
        };
    }
    Verdict::Held {
        missing: "the source payload to rebuild the return",
    }
}

fn working_phase(op: &ActiveOperation) -> Phase {
    match op.root {
        Root::Legacy if op.applied.contains(&Stage::ActorInstall) => Phase::LegacyOwned,
        Root::Legacy => Phase::LegacyReserved,
        Root::Scoped if op.has_applied() => Phase::Installing,
        Root::Scoped => Phase::ScopedReserved,
    }
}

fn closed(record: &CustodyRecord) -> CustodyRecord {
    CustodyRecord {
        phase: Phase::Closed,
        active_operation: None,
        ..record.clone()
    }
}

/// The row a verdict writes; `None` for a refusal, which writes nothing.
pub(crate) fn next_record(record: &CustodyRecord, verdict: &Verdict) -> Option<CustodyRecord> {
    let mut op = record.active_operation.clone()?;
    let note_applied = |op: &mut ActiveOperation, stage| {
        if !op.applied.contains(&stage) {
            op.applied.push(stage);
        }
    };
    match verdict {
        Verdict::Refused { .. } => return None,
        Verdict::NoApply | Verdict::Close => return Some(closed(record)),
        Verdict::Held { .. } => {
            if op.status == StageStatus::ApplyArmed {
                op.status = StageStatus::Indeterminate;
            }
            return Some(CustodyRecord {
                phase: Phase::Held,
                active_operation: Some(op),
                ..record.clone()
            });
        }
        Verdict::Resume { stage, .. } => {
            // Moving on to a later stage confirms the current one.
            if *stage != op.stage {
                let current = op.stage;
                note_applied(&mut op, current);
            }
            op.stage = *stage;
            op.status = StageStatus::ReservedNotArmed;
            op.expected_after = None;
        }
        Verdict::Confirmed => {
            // Only an armed stage is confirmed applied; a known outcome stays as it was.
            if matches!(
                op.status,
                StageStatus::ApplyArmed | StageStatus::Indeterminate
            ) {
                let stage = op.stage;
                note_applied(&mut op, stage);
            }
            op.status = StageStatus::KnownOutcome;
        }
        Verdict::Owned { by } => {
            note_applied(&mut op, Stage::ActorInstall);
            op.handoffs.push(by.clone());
            op.status = StageStatus::KnownOutcome;
        }
        Verdict::Return { .. } => {
            op.kind = OperationKind::ReturnOnly;
            op.stage = Stage::SourceReturn;
            op.status = StageStatus::ReservedNotArmed;
            op.expected_after = None;
        }
    }
    Some(CustodyRecord {
        phase: working_phase(&op),
        active_operation: Some(op),
        ..record.clone()
    })
}

/// The row an applier's report writes for the armed stage.
pub(crate) fn settled_record(
    record: &CustodyRecord,
    outcome: &super::operation::StepOutcome,
) -> Option<CustodyRecord> {
    use super::operation::StepOutcome;
    let mut op = record.active_operation.clone()?;
    match outcome {
        StepOutcome::Applied { after, handoff } => {
            if matches!(op.stage, Stage::SourceReturn | Stage::ExactRelease) {
                return Some(CustodyRecord {
                    canonical_seal: after.clone(),
                    ..closed(record)
                });
            }
            if !op.applied.contains(&op.stage) {
                op.applied.push(op.stage);
            }
            op.handoffs.extend(handoff.iter().cloned());
            op.status = StageStatus::KnownOutcome;
            Some(CustodyRecord {
                phase: working_phase(&op),
                canonical_seal: after.clone(),
                active_operation: Some(op),
                ..record.clone()
            })
        }
        StepOutcome::NotApplied if !op.has_applied() => Some(closed(record)),
        // A refused later step, such as a failed return, keeps the earlier responsibility.
        StepOutcome::NotApplied => {
            op.status = StageStatus::KnownOutcome;
            Some(CustodyRecord {
                phase: working_phase(&op),
                active_operation: Some(op),
                ..record.clone()
            })
        }
        StepOutcome::Indeterminate { .. } => next_record(
            record,
            &Verdict::Held {
                missing: "the armed stage's outcome",
            },
        ),
    }
}
