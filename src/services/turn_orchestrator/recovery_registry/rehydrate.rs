//! Rebuilds this process's view of a channel's custody row after a restart or a lost owner. The
//! row is shown as undecided before it is judged, and a PG `installed` restores no witness.

use super::super::recovery_custody::capability::EndProof;
use super::super::recovery_custody::model::{Inspection, Phase};
use super::super::recovery_custody::operation::Root;
use super::super::recovery_custody::reconcile::{Action, Evidence, PriorExecution, Verdict};
use super::super::recovery_custody::{CustodyError, CustodyStore};
use super::RecoveryRegistry;

/// Whether the caller saw the row's owning process end: its kill and wait, or this node's restart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EarlierProcess {
    Gone,
    Unknown,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Rehydrated {
    /// No row or a closed one: nothing to carry, nothing registered.
    Nothing,
    /// Registered as undecided. `prior` separates a live owner in this process, which returns
    /// through its lease, from a lost process, which is rebuilt from the durable row.
    Row {
        revision: i64,
        prior: PriorExecution,
    },
    /// Registered as undecided and left held, missing what the reason names.
    Held(&'static str),
}

pub(crate) async fn rehydrate(
    registry: &RecoveryRegistry,
    custody: &CustodyStore,
    node: &str,
    channel: u64,
    earlier: EarlierProcess,
) -> Result<Rehydrated, CustodyError> {
    let channel_id = channel.to_string();
    let record = match custody.inspect(&channel_id).await? {
        Inspection::Absent => return Ok(Rehydrated::Nothing),
        Inspection::Unsupported { revision, .. } => {
            custody.attach(&channel_id, revision);
            return Ok(Rehydrated::Held("a row this build can decode"));
        }
        Inspection::Row(record) => record,
    };
    if record.phase == Phase::Closed {
        return Ok(Rehydrated::Nothing);
    }
    custody.attach(&channel_id, record.revision);
    let op = record.active_operation.as_ref();
    // Only a scoped responsibility takes a recovery slot; the index alone carries a legacy row.
    let slot = if op.map_or(record.ever_scoped, |op| op.root == Root::Scoped) {
        match registry.resolve(channel) {
            Ok(slot) => Some(slot),
            Err(_) => return Ok(Rehydrated::Held("a free recovery slot")),
        }
    } else {
        registry.peek(channel)
    };
    if record.owner.node != node {
        return Ok(Rehydrated::Held("the owning node's own reconcile"));
    }
    let prior = if record.owner.process_incarnation == registry.process_incarnation() {
        slot.map_or(PriorExecution::Unknown, |slot| slot.prior(&record))
    } else if earlier == EarlierProcess::Gone {
        op.map_or(PriorExecution::Unknown, |op| {
            PriorExecution::Ended(EndProof::of_lost_process(op))
        })
    } else {
        return Ok(Rehydrated::Held("the end of the earlier process"));
    };
    Ok(Rehydrated::Row {
        revision: record.revision,
        prior,
    })
}

/// What a reconcile leaves the registry to do.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// The responsibility ended with evidence.
    Ended,
    /// The same operation goes on from its working phase; a `Restored` hold confirmed nothing new.
    Continue(Verdict),
    Held(&'static str),
}

pub(crate) async fn settle(
    custody: &CustodyStore,
    channel: u64,
    revision: i64,
    action: Action,
    evidence: &Evidence,
) -> Result<(Next, i64), CustodyError> {
    let channel_id = channel.to_string();
    let (verdict, revision) = custody
        .reconcile(&channel_id, revision, action, evidence)
        .await?;
    let next = match verdict {
        Verdict::NoApply | Verdict::Close => Next::Ended,
        Verdict::Held { missing } => Next::Held(missing),
        Verdict::Refused { reason } => Next::Held(reason),
        going @ (Verdict::Resume { .. }
        | Verdict::Confirmed
        | Verdict::Owned { .. }
        | Verdict::Return { .. }
        | Verdict::Restored) => Next::Continue(going),
    };
    Ok((next, revision))
}
