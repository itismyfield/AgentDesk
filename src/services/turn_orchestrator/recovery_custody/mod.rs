//! Recovery custody: one shared PG row per channel names the single consumer — scoped recovery
//! install or legacy consumption — that may consume the channel's pending input, and keeps an
//! undecided result until it is reconciled. Only tests connect it.

pub(crate) mod capability;
pub(crate) mod denial_index;
pub(crate) mod model;
pub(crate) mod operation;
pub(crate) mod reconcile;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sqlx::PgPool;

use self::capability::{ApplyCapability, RevokeHandle};
use self::denial_index::{CommittedArm, DenialIndex};
use self::model::{
    CanonicalSeal, CustodyRecord, HomeIdentity, Inspection, IssuedEpisode, Namespace, Owner, Phase,
};
use self::operation::{
    ActiveOperation, OperationKind, OutcomeReport, Root, Stage, StageStatus, StepOutcome,
};
use self::reconcile::{Action, Evidence, LegacyResult, Verdict};
use crate::db::channel_recovery_custody::{self as store, CasWrite};
use crate::services::agent_recovery::admission::AdmissionGuard;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Off or a scope change closed new admission; already armed work still records.
    AdmissionClosed,
    StoreUnavailable,
    HomeMismatch,
    Busy(Phase),
    /// The row is held, including a row this build cannot decode.
    Held,
    Stale,
    NotArmable,
    /// The verdict refused the action; nothing was written.
    Unfit(&'static str),
}

/// A store or home failure is reported as such, never as "no responsibility".
#[derive(Debug)]
pub(crate) enum CustodyError {
    Store(String),
    Refused(Refusal),
}

impl std::fmt::Display for CustodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(f, "recovery custody store failed: {error}"),
            Self::Refused(refusal) => write!(f, "recovery custody refused: {refusal:?}"),
        }
    }
}

fn store_error(error: impl std::fmt::Display) -> CustodyError {
    CustodyError::Store(error.to_string())
}

fn refused(refusal: Refusal) -> CustodyError {
    CustodyError::Refused(refusal)
}

pub(crate) struct ReserveRequest {
    pub(crate) channel_id: String,
    pub(crate) root: Root,
    pub(crate) namespace: Namespace,
    pub(crate) sources: Vec<u64>,
    pub(crate) allowed: Vec<Stage>,
    pub(crate) before: CanonicalSeal,
    /// For a scoped root: the home epoch and activation generation it was admitted under.
    pub(crate) scoped_fence: Option<(i64, i64)>,
    pub(crate) issued_episode: Option<IssuedEpisode>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Reservation {
    Reserved {
        channel_id: String,
        revision: i64,
        operation_id: String,
    },
    /// No recovery store: a legacy consumer runs as before, with no custody row.
    Unmanaged,
}

/// An armed stage. Dropping every field still leaves the collector to record the result.
pub(crate) struct Armed {
    pub(crate) capability: ApplyCapability,
    pub(crate) report: OutcomeReport,
    pub(crate) revoke: RevokeHandle,
    pub(crate) collector: tokio::task::JoinHandle<Result<i64, CustodyError>>,
}

#[derive(Clone)]
pub(crate) struct CustodyStore {
    pool: PgPool,
    owner: Arc<Owner>,
    index: Arc<DenialIndex>,
    admission_open: Arc<AtomicBool>,
}

impl CustodyStore {
    pub(crate) fn new(pool: PgPool, owner: Owner, index: Arc<DenialIndex>) -> Self {
        Self {
            pool,
            owner: Arc::new(owner),
            index,
            admission_open: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Off or a scope change: refuses new admission first; armed stages still record.
    pub(crate) fn close_admission(&self) {
        self.admission_open.store(false, Ordering::SeqCst);
    }

    pub(crate) async fn inspect(&self, channel_id: &str) -> Result<Inspection, CustodyError> {
        let mut conn = self.pool.acquire().await.map_err(store_error)?;
        let stored = store::read(&mut conn, channel_id)
            .await
            .map_err(store_error)?;
        Ok(stored.map_or(Inspection::Absent, CustodyRecord::decode))
    }

    /// Reserves inside the admission's own transaction and commits it; the transaction's lock ends
    /// there, and the custody row holds the responsibility from then on.
    pub(crate) async fn reserve(
        &self,
        mut admission: AdmissionGuard,
        request: ReserveRequest,
    ) -> Result<Reservation, CustodyError> {
        if !self.admission_open.load(Ordering::SeqCst) {
            return Err(refused(Refusal::AdmissionClosed));
        }
        let Some(tx) = admission.transaction() else {
            return match request.root {
                Root::Legacy => Ok(Reservation::Unmanaged),
                Root::Scoped => Err(refused(Refusal::StoreUnavailable)),
            };
        };
        let channel_id = request.channel_id.clone();
        let home = crate::db::o_channel_homes::read_home_in(&mut **tx, &channel_id)
            .await
            .map_err(store_error)?;
        let (home, activation_generation) = match (request.root, request.scoped_fence) {
            // A legacy consumer keeps its admission whether or not the channel has a home row.
            (Root::Legacy, _) => (
                home.map_or(HomeIdentity::Absent, |home| HomeIdentity::Present {
                    epoch: home.epoch,
                }),
                None,
            ),
            (Root::Scoped, Some((epoch, generation)))
                if home.as_ref().is_some_and(|home| {
                    home.epoch == epoch && home.holder.as_deref() == Some(&self.owner.node)
                }) =>
            {
                (HomeIdentity::Present { epoch }, Some(generation))
            }
            (Root::Scoped, _) => return Err(refused(Refusal::HomeMismatch)),
        };
        let current = store::read(&mut **tx, &channel_id)
            .await
            .map_err(store_error)?;
        let (expected, ever_scoped) = match current.map(CustodyRecord::decode) {
            None => (None, false),
            Some(Inspection::Row(row)) if row.phase == Phase::Closed => {
                (Some(row.revision), row.ever_scoped)
            }
            Some(Inspection::Row(row)) => return Err(refused(Refusal::Busy(row.phase))),
            Some(Inspection::Unsupported { .. } | Inspection::Absent) => {
                return Err(refused(Refusal::Held));
            }
        };
        let operation_id = uuid::Uuid::new_v4().to_string();
        let (phase, kind) = match request.root {
            Root::Legacy => (Phase::LegacyReserved, OperationKind::LegacyConsume),
            Root::Scoped => (Phase::ScopedReserved, OperationKind::ScopedInstall),
        };
        let record = CustodyRecord {
            channel_id: channel_id.clone(),
            revision: expected.map_or(1, |revision| revision + 1),
            namespace: request.namespace.clone(),
            owner: (*self.owner).clone(),
            home,
            activation_generation,
            issued_episode: request.issued_episode,
            phase,
            active_operation: Some(ActiveOperation {
                operation_id: operation_id.clone(),
                root: request.root,
                kind,
                allowed: request.allowed,
                target: request.namespace,
                sources: request.sources,
                issued_revision: expected.map_or(1, |revision| revision + 1),
                stage: Stage::Reserved,
                status: StageStatus::ReservedNotArmed,
                applied: Vec::new(),
                before: request.before.clone(),
                expected_after: None,
                handoffs: Vec::new(),
            }),
            canonical_seal: request.before,
            ever_scoped,
        };
        let stored = record.encode().map_err(CustodyError::Store)?;
        let written = match expected {
            None => store::insert(&mut **tx, &stored).await,
            Some(revision) => store::replace(&mut **tx, revision, &stored).await,
        }
        .map_err(store_error)?;
        let CasWrite::Applied { revision } = written else {
            return Err(refused(Refusal::Stale));
        };
        admission.commit().await.map_err(store_error)?;
        self.index.block(&channel_id);
        Ok(Reservation::Reserved {
            channel_id,
            revision,
            operation_id,
        })
    }

    /// Reads the row at `expected_revision`, writes `step`'s next row by CAS and commits.
    async fn rewrite<T>(
        &self,
        channel_id: &str,
        expected_revision: i64,
        step: impl FnOnce(&CustodyRecord) -> Result<(CustodyRecord, T), CustodyError>,
    ) -> Result<(T, i64), CustodyError> {
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let record = match store::read(&mut *tx, channel_id)
            .await
            .map_err(store_error)?
            .map(CustodyRecord::decode)
        {
            Some(Inspection::Row(record)) if record.revision == expected_revision => record,
            Some(Inspection::Unsupported { .. }) => return Err(refused(Refusal::Held)),
            _ => return Err(refused(Refusal::Stale)),
        };
        let (next, value) = step(&record)?;
        let stored = next.encode().map_err(CustodyError::Store)?;
        let CasWrite::Applied { revision } = store::replace(&mut *tx, expected_revision, &stored)
            .await
            .map_err(store_error)?
        else {
            return Err(refused(Refusal::Stale));
        };
        #[cfg(test)]
        commit_fault::run(channel_id).map_err(CustodyError::Store)?;
        tx.commit().await.map_err(store_error)?;
        Ok((value, revision))
    }

    /// Arms one stage durably; only after that commit is the exact operation published and the
    /// capability issued. The owned collector records the applier's report even if the caller left.
    pub(crate) async fn arm(
        &self,
        channel_id: &str,
        expected_revision: i64,
        operation_id: &str,
        stage: Stage,
        expected_after: CanonicalSeal,
        grant: Option<String>,
    ) -> Result<Armed, CustodyError> {
        let ((), revision) = self
            .rewrite(channel_id, expected_revision, |record| {
                let mut op = record
                    .active_operation
                    .clone()
                    .filter(|op| op.operation_id == operation_id && op.allowed.contains(&stage))
                    .ok_or(refused(Refusal::NotArmable))?;
                let armable = matches!(
                    op.status,
                    StageStatus::ReservedNotArmed | StageStatus::KnownOutcome
                ) && !matches!(record.phase, Phase::Held | Phase::Closed);
                if !armable {
                    return Err(refused(Refusal::NotArmable));
                }
                op.stage = stage;
                op.status = StageStatus::ApplyArmed;
                op.before = record.canonical_seal.clone();
                op.expected_after = Some(expected_after);
                // A debt the stage will issue is recorded before it can be published.
                op.handoffs.extend(grant);
                let next = CustodyRecord {
                    active_operation: Some(op),
                    ..record.clone()
                };
                Ok((next, ()))
            })
            .await?;
        self.index.publish(CommittedArm {
            channel_id,
            operation_id,
            stage,
        });
        let (capability, revoke) = ApplyCapability::issue(channel_id, operation_id, stage);
        let (report, receiver) = OutcomeReport::pair();
        let collector = tokio::spawn({
            let (custody, revoke) = (self.clone(), revoke.clone());
            let (channel_id, operation_id) = (channel_id.to_string(), operation_id.to_string());
            async move {
                let outcome = operation::await_report(receiver).await;
                revoke.revoke();
                custody
                    .record_outcome(&channel_id, &operation_id, revision, outcome)
                    .await
            }
        });
        Ok(Armed {
            capability,
            report,
            revoke,
            collector,
        })
    }

    /// Records an armed stage's result; an indeterminate one holds the row and blocks the index.
    pub(crate) async fn record_outcome(
        &self,
        channel_id: &str,
        operation_id: &str,
        expected_revision: i64,
        outcome: StepOutcome,
    ) -> Result<i64, CustodyError> {
        let result = self
            .rewrite(channel_id, expected_revision, |record| {
                let armed = record.active_operation.as_ref().is_some_and(|op| {
                    op.operation_id == operation_id && op.status == StageStatus::ApplyArmed
                });
                let next = armed
                    .then(|| reconcile::settled_record(record, &outcome))
                    .flatten()
                    .ok_or(refused(Refusal::Stale))?;
                Ok((next, ()))
            })
            .await
            .map(|((), revision)| revision);
        self.index.block(channel_id);
        result
    }

    /// A busy transition lock before any effect: a root that applied nothing ends with no
    /// responsibility; one that already applied a stage only records that this stage did not run.
    pub(crate) async fn settle_busy(
        &self,
        channel_id: &str,
        expected_revision: i64,
        operation_id: &str,
    ) -> Result<i64, CustodyError> {
        self.write_verdict(channel_id, expected_revision, operation_id, |op| {
            match op.status {
                StageStatus::ApplyArmed | StageStatus::Indeterminate => Verdict::Refused {
                    reason: "an armed stage settles through its outcome",
                },
                _ if !op.has_applied() => Verdict::NoApply,
                StageStatus::KnownOutcome => Verdict::Confirmed,
                StageStatus::ReservedNotArmed => Verdict::Resume {
                    stage: op.stage,
                    reuse: op.handoffs.last().cloned(),
                },
            }
        })
        .await
        .map(|(_, revision)| revision)
    }

    pub(crate) async fn record_legacy_result(
        &self,
        channel_id: &str,
        expected_revision: i64,
        operation_id: &str,
        result: &LegacyResult,
    ) -> Result<(Verdict, i64), CustodyError> {
        self.write_verdict(channel_id, expected_revision, operation_id, |op| {
            if op.root != Root::Legacy {
                return Verdict::Refused {
                    reason: "not a legacy operation",
                };
            }
            reconcile::legacy_verdict(op, result)
        })
        .await
    }

    /// Reconciles the operation from evidence. A row this build cannot decode stays held untouched.
    pub(crate) async fn reconcile(
        &self,
        channel_id: &str,
        expected_revision: i64,
        action: Action,
        evidence: &Evidence,
    ) -> Result<(Verdict, i64), CustodyError> {
        let result = self
            .rewrite(channel_id, expected_revision, |record| {
                let verdict = reconcile::decide(record, action, evidence);
                let next = reconcile::next_record(record, &verdict).ok_or(match verdict {
                    Verdict::Refused { reason } => refused(Refusal::Unfit(reason)),
                    _ => refused(Refusal::Stale),
                })?;
                Ok((next, verdict))
            })
            .await;
        self.index.block(channel_id);
        result
    }

    async fn write_verdict(
        &self,
        channel_id: &str,
        expected_revision: i64,
        operation_id: &str,
        verdict: impl FnOnce(&ActiveOperation) -> Verdict,
    ) -> Result<(Verdict, i64), CustodyError> {
        let result = self
            .rewrite(channel_id, expected_revision, |record| {
                let op = record
                    .active_operation
                    .as_ref()
                    .filter(|op| op.operation_id == operation_id)
                    .ok_or(refused(Refusal::Stale))?;
                let verdict = verdict(op);
                let next = reconcile::next_record(record, &verdict).ok_or(match verdict {
                    Verdict::Refused { reason } => refused(Refusal::Unfit(reason)),
                    _ => refused(Refusal::Stale),
                })?;
                Ok((next, verdict))
            })
            .await;
        self.index.block(channel_id);
        result
    }
}

/// Runs a test's change between a custody write and its commit; an error aborts the commit.
#[cfg(test)]
pub(crate) mod commit_fault {
    type Fault = Box<dyn FnOnce() -> Result<(), String> + Send>;
    static NEXT: std::sync::Mutex<Vec<(String, Fault)>> = std::sync::Mutex::new(Vec::new());

    pub(crate) fn before_next(
        channel_id: &str,
        fault: impl FnOnce() -> Result<(), String> + Send + 'static,
    ) {
        let mut next = NEXT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        next.push((channel_id.to_string(), Box::new(fault)));
    }

    pub(super) fn run(channel_id: &str) -> Result<(), String> {
        let fault = {
            let mut next = NEXT.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let index = next.iter().position(|(channel, _)| channel == channel_id);
            index.map(|index| next.remove(index).1)
        };
        fault.map_or(Ok(()), |fault| fault())
    }
}

#[cfg(test)]
mod custody_pg_tests;
#[cfg(test)]
mod reconcile_pg_tests;
