//! Recovery custody: one shared PG row per channel names the single consumer of its pending
//! input and keeps an undecided result until reconciled. Only tests connect it.

pub(crate) mod capability;
pub(crate) mod denial_index;
pub(crate) mod model;
pub(crate) mod operation;
pub(crate) mod reconcile;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use sqlx::PgPool;

use self::capability::{ApplyCapability, AttemptKey, RevokeHandle};
use self::denial_index::{CommittedArm, DenialIndex};
use self::model::{
    CanonicalSeal, CustodyRecord, HomeIdentity, Inspection, IssuedEpisode, Namespace, Owner, Phase,
};
use self::operation::{
    ActiveOperation, OperationKind, OutcomeReport, Root, Stage, StageStatus, StepOutcome,
};
use self::reconcile::{Action, Evidence, LegacyResult, Verdict};
#[cfg(unix)]
use super::recovery_registry::entry::{ArmRefusal, QuiescencePermit, WorkerPermit};
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
    /// A closed row with an arm here that has not ended, or another owner's without the registry's
    /// current permit that its attempt is closed and drained.
    NotQuiescent,
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

/// One stage to arm: the row revision it expects, its operation and stage, the seal it leaves and
/// the grant it records or reuses.
pub(crate) struct ArmStep<'a> {
    pub(crate) channel_id: &'a str,
    pub(crate) expected_revision: i64,
    pub(crate) operation_id: &'a str,
    pub(crate) stage: Stage,
    pub(crate) expected_after: CanonicalSeal,
    pub(crate) grant: Option<String>,
}

/// The registry's drain evidence as a reservation of a closed row asks it.
trait Vouch: Sync {
    /// Whether it vouches, now, for the process that owns `row`.
    fn vouches(&self, row: &CustodyRecord) -> bool;
    /// Holds the slot for this succession until the value drops; `None` once it no longer vouches.
    fn begin(&self, row: &CustodyRecord) -> Option<Box<dyn Send>>;
}

#[cfg(unix)]
impl Vouch for QuiescencePermit {
    fn vouches(&self, row: &CustodyRecord) -> bool {
        QuiescencePermit::vouches(self, row)
    }

    fn begin(&self, row: &CustodyRecord) -> Option<Box<dyn Send>> {
        let succession = QuiescencePermit::begin(self, row)?;
        Some(Box::new(succession))
    }
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

    /// Off or a scope change: refuses new admission and every later arm, including one whose
    /// write has not committed yet; armed stages still record.
    pub(crate) fn close_admission(&self) {
        self.admission_open.store(false, Ordering::SeqCst);
    }

    /// The index entry the sync writers see for the channel, without creating one.
    pub(crate) fn mirrored(&self, channel_id: &str) -> Option<denial_index::IndexState> {
        self.index.entry(channel_id)
    }

    /// Shows the sync writers a row found in PG as undecided before anything is rebuilt from it.
    pub(crate) fn attach(&self, channel_id: &str, revision: i64) {
        self.index.attach(channel_id, revision);
    }

    pub(crate) async fn inspect(&self, channel_id: &str) -> Result<Inspection, CustodyError> {
        let mut conn = self.pool.acquire().await.map_err(store_error)?;
        let stored = store::read(&mut conn, channel_id)
            .await
            .map_err(store_error)?;
        Ok(stored.map_or(Inspection::Absent, CustodyRecord::decode))
    }

    /// Reserves inside the admission's own transaction and commits it; the transaction's lock ends
    /// there, and the custody row holds the responsibility from then on. A closed row is reserved
    /// again only once its arms here ended; another owner's needs [`Self::reserve_successor`].
    pub(crate) async fn reserve(
        &self,
        admission: AdmissionGuard,
        request: ReserveRequest,
    ) -> Result<Reservation, CustodyError> {
        self.reserve_with(admission, request, None).await
    }

    /// Reserves a closed row again with the registry's permit for the process that owns it. The
    /// slot stays held for this succession from the last check until the commit is decided.
    #[cfg(unix)]
    pub(crate) async fn reserve_successor(
        &self,
        admission: AdmissionGuard,
        request: ReserveRequest,
        permit: QuiescencePermit,
    ) -> Result<Reservation, CustodyError> {
        self.reserve_with(admission, request, Some(&permit)).await
    }

    async fn reserve_with(
        &self,
        mut admission: AdmissionGuard,
        request: ReserveRequest,
        permit: Option<&dyn Vouch>,
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
        let (expected, ever_scoped, closed) = match current.map(CustodyRecord::decode) {
            None => (None, false, None),
            Some(Inspection::Row(row)) if row.phase == Phase::Closed => {
                (Some(row.revision), row.ever_scoped, Some(row))
            }
            Some(Inspection::Row(row)) => return Err(refused(Refusal::Busy(row.phase))),
            Some(Inspection::Unsupported { .. } | Inspection::Absent) => {
                return Err(refused(Refusal::Held));
            }
        };
        // Every arm this store issued on the row has ended; a row the registry answered for, or
        // another owner's, also needs the registry's permit that its attempt drained.
        let drained = |row: &CustodyRecord| {
            let vouched = permit.is_some_and(|permit| permit.vouches(row));
            let own = row.owner == *self.owner && !row.ever_scoped;
            self.index.arms_ended(&channel_id) && (vouched || own)
        };
        if closed.as_deref().is_some_and(|row| !drained(row)) {
            return Err(refused(Refusal::NotQuiescent));
        }
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
                reuse: None,
                arm_id: None,
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
        #[cfg(test)]
        commit_fault::pause(&channel_id, "reserve-checked").await;
        // The last check takes the slot for the succession until the commit is decided.
        let succession = match (closed.as_deref(), permit) {
            (Some(row), _) if !drained(row) => return Err(refused(Refusal::NotQuiescent)),
            (Some(row), Some(permit)) => match permit.begin(row) {
                Some(succession) => Some(succession),
                None => return Err(refused(Refusal::NotQuiescent)),
            },
            _ => None,
        };
        #[cfg(test)]
        commit_fault::pause(&channel_id, "reserve-held").await;
        // The commit and the slot it holds outlive a caller that leaves; the slot is let go only
        // once the commit's result is known.
        let (custody, ours) = (self.clone(), operation_id.clone());
        let decided = tokio::spawn(async move {
            let committed = match admission.commit().await {
                Ok(()) => Ok(true),
                Err(error) => custody.landed(&channel_id, &ours).await.ok_or(error),
            };
            #[cfg(test)]
            commit_fault::pause(&channel_id, "reserve-committed").await;
            match committed {
                Ok(true) => custody.index.block(&channel_id, revision),
                Ok(false) => {}
                // Unknown even after a read: the slot stays held, and nothing starts on it.
                Err(_) => std::mem::forget(succession),
            }
            committed.map(|landed| landed.then_some(channel_id))
        });
        match decided.await.map_err(store_error)? {
            Ok(Some(channel_id)) => Ok(Reservation::Reserved {
                channel_id,
                revision,
                operation_id,
            }),
            Ok(None) => Err(store_error(
                "the reservation commit failed and was rolled back",
            )),
            Err(error) => Err(store_error(error)),
        }
    }

    /// Whether the row now carries `operation_id`; `None` when it cannot be read.
    async fn landed(&self, channel_id: &str, operation_id: &str) -> Option<bool> {
        let row = self.inspect(channel_id).await.ok()?;
        let op = match row {
            Inspection::Row(row) => row.active_operation,
            _ => None,
        };
        Some(op.is_some_and(|op| op.operation_id == operation_id))
    }

    /// Reads the row (at `expected_revision` when given), writes `step`'s next row by CAS and
    /// commits. An `issuing` write commits only if admission is still open right before the commit.
    async fn rewrite<T>(
        &self,
        channel_id: &str,
        expected_revision: Option<i64>,
        issuing: bool,
        step: impl FnOnce(&CustodyRecord) -> Result<(CustodyRecord, T), CustodyError>,
    ) -> Result<(T, i64), CustodyError> {
        let mut tx = self.pool.begin().await.map_err(store_error)?;
        let record = match store::read(&mut *tx, channel_id)
            .await
            .map_err(store_error)?
            .map(CustodyRecord::decode)
        {
            Some(Inspection::Row(record))
                if expected_revision.is_none_or(|expected| record.revision == expected) =>
            {
                *record
            }
            Some(Inspection::Unsupported { .. }) => return Err(refused(Refusal::Held)),
            _ => return Err(refused(Refusal::Stale)),
        };
        let (next, value) = step(&record)?;
        let stored = next.encode().map_err(CustodyError::Store)?;
        let CasWrite::Applied { revision } = store::replace(&mut *tx, record.revision, &stored)
            .await
            .map_err(store_error)?
        else {
            return Err(refused(Refusal::Stale));
        };
        #[cfg(test)]
        commit_fault::run(channel_id).map_err(CustodyError::Store)?;
        // An off that lands before this check refuses the issuance; one after it is ordered later.
        if issuing && !self.admission_open.load(Ordering::SeqCst) {
            return Err(refused(Refusal::AdmissionClosed));
        }
        tx.commit().await.map_err(store_error)?;
        #[cfg(test)]
        commit_fault::committed(channel_id).await;
        Ok((value, revision))
    }

    /// Arms one stage outside any registry attempt, for this tree's own fixtures only; every other
    /// caller arms through [`Self::arm_for`], so the registry sees each arm it must drain.
    async fn arm(
        &self,
        channel_id: &str,
        expected_revision: i64,
        operation_id: &str,
        stage: Stage,
        expected_after: CanonicalSeal,
        grant: Option<String>,
    ) -> Result<Armed, CustodyError> {
        let step = ArmStep {
            channel_id,
            expected_revision,
            operation_id,
            stage,
            expected_after,
            grant,
        };
        self.arm_step(step, None).await
    }

    /// Arms one stage for `worker`'s open attempt and keeps it there before returning, so the
    /// worker covers the arm from commit to keeping; a refused keep settles it as not applied.
    #[cfg(unix)]
    pub(crate) async fn arm_for(
        &self,
        worker: &WorkerPermit,
        step: ArmStep<'_>,
    ) -> Result<Armed, ArmRefusal> {
        worker.open().map_err(ArmRefusal::Registry)?;
        let armed = self.arm_step(step, Some(worker.key())).await;
        let armed = armed.map_err(ArmRefusal::Custody)?;
        if let Err(refusal) = worker.keep(&armed) {
            armed.report.send(StepOutcome::NotApplied);
            return Err(ArmRefusal::Registry(refusal));
        }
        Ok(armed)
    }

    /// Arms one stage durably; only after that commit is the exact operation published and the
    /// capability issued. The owned collector records the applier's report even if the caller left.
    async fn arm_step(
        &self,
        step: ArmStep<'_>,
        attempt: Option<AttemptKey>,
    ) -> Result<Armed, CustodyError> {
        let ArmStep {
            channel_id,
            expected_revision,
            operation_id,
            stage,
            expected_after,
            grant,
        } = step;
        let open = || self.admission_open.load(Ordering::SeqCst);
        let arm_id = uuid::Uuid::new_v4().to_string();
        let (namespace, revision) = self
            .rewrite(channel_id, Some(expected_revision), true, |record| {
                let arm = (operation_id, stage, expected_after, grant);
                let armed = armed_record(record, arm, &arm_id, open())?;
                Ok((armed, record.namespace.clone()))
            })
            .await?;
        self.index.publish(CommittedArm {
            channel_id,
            revision,
            operation_id,
            stage,
            arm_id: &arm_id,
        });
        let target = (channel_id, operation_id, stage, arm_id.as_str());
        let (capability, revoke) = ApplyCapability::issue(target, &namespace, attempt);
        self.index.track(channel_id, revoke.clone());
        let (report, receiver) = OutcomeReport::pair();
        let collector = tokio::spawn({
            let (custody, revoke) = (self.clone(), revoke.clone());
            let channel_id = channel_id.to_string();
            async move {
                let outcome = operation::await_report(receiver).await;
                revoke.revoke();
                custody.record_outcome(&channel_id, &arm_id, outcome).await
            }
        });
        Ok(Armed {
            capability,
            report,
            revoke,
            collector,
        })
    }

    /// Records the outcome of arm `arm_id` only, also after a reconcile held it while its applier
    /// still ran; it ends that arm's index entry and never another arm's.
    async fn record_outcome(
        &self,
        channel_id: &str,
        arm_id: &str,
        outcome: StepOutcome,
    ) -> Result<i64, CustodyError> {
        let result = self
            .rewrite(channel_id, None, false, |record| {
                let armed = record.active_operation.as_ref().is_some_and(|op| {
                    op.arm_id.as_deref() == Some(arm_id)
                        && matches!(
                            op.status,
                            StageStatus::ApplyArmed | StageStatus::Indeterminate
                        )
                });
                let next = armed
                    .then(|| reconcile::settled_record(record, &outcome))
                    .flatten()
                    .ok_or(refused(Refusal::Stale))?;
                Ok((next, ()))
            })
            .await
            .map(|((), revision)| revision);
        self.index.block_arm(channel_id, arm_id);
        result
    }

    /// A busy transition lock before any effect ends a root that applied nothing, or records that
    /// this stage did not run; it never lifts a hold, which only an evidenced reconcile does.
    pub(crate) async fn settle_busy(
        &self,
        channel_id: &str,
        expected_revision: i64,
        operation_id: &str,
    ) -> Result<i64, CustodyError> {
        self.write_verdict(channel_id, expected_revision, operation_id, |phase, op| {
            if phase == Phase::Held {
                return Verdict::Refused {
                    reason: "a hold lifts only through a reconcile",
                };
            }
            reconcile::not_run_verdict(op).unwrap_or_else(|| match op.status {
                StageStatus::KnownOutcome => Verdict::Confirmed,
                _ => Verdict::Resume {
                    stage: op.stage,
                    reuse: op.handoffs.last().cloned(),
                },
            })
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
        self.write_verdict(channel_id, expected_revision, operation_id, |_, op| {
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
            .rewrite(channel_id, Some(expected_revision), false, |record| {
                let verdict = reconcile::decide(record, action, evidence);
                let next = reconcile::next_record(record, &verdict, evidence.observed.as_ref())
                    .ok_or(match verdict {
                        Verdict::Refused { reason } => refused(Refusal::Unfit(reason)),
                        _ => refused(Refusal::Stale),
                    })?;
                Ok((next, verdict))
            })
            .await;
        self.block_after(channel_id, expected_revision, &result);
        result
    }

    /// Mirrors a transition at the revision it committed. A refusal wrote nothing; a store failure
    /// only blocks what the index knew up to the revision the call targeted.
    fn block_after<T>(
        &self,
        channel_id: &str,
        targeted: i64,
        result: &Result<(T, i64), CustodyError>,
    ) {
        match result {
            Ok((_, revision)) => self.index.block(channel_id, *revision),
            Err(CustodyError::Store(_)) => self.index.block(channel_id, targeted),
            Err(CustodyError::Refused(_)) => {}
        }
    }

    async fn write_verdict(
        &self,
        channel_id: &str,
        expected_revision: i64,
        operation_id: &str,
        verdict: impl FnOnce(Phase, &ActiveOperation) -> Verdict,
    ) -> Result<(Verdict, i64), CustodyError> {
        let result = self
            .rewrite(channel_id, Some(expected_revision), false, |record| {
                let op = record
                    .active_operation
                    .as_ref()
                    .filter(|op| op.operation_id == operation_id)
                    .ok_or(refused(Refusal::Stale))?;
                let verdict = verdict(record.phase, op);
                let next = reconcile::next_record(record, &verdict, None).ok_or(match verdict {
                    Verdict::Refused { reason } => refused(Refusal::Unfit(reason)),
                    _ => refused(Refusal::Stale),
                })?;
                Ok((next, verdict))
            })
            .await;
        self.block_after(channel_id, expected_revision, &result);
        result
    }
}

/// The one arm transition: issuance still open, the operation's next stage (the one a reconcile
/// named; an applied stage only as a return), and a grant reused or recorded once.
fn armed_record(
    record: &CustodyRecord,
    (operation_id, stage, expected_after, grant): (&str, Stage, CanonicalSeal, Option<String>),
    arm_id: &str,
    issuing_open: bool,
) -> Result<CustodyRecord, CustodyError> {
    if !issuing_open {
        return Err(refused(Refusal::AdmissionClosed));
    }
    let mut op = record
        .active_operation
        .clone()
        .filter(|op| op.operation_id == operation_id && op.allowed.contains(&stage))
        .ok_or(refused(Refusal::NotArmable))?;
    let named = op.status == StageStatus::ReservedNotArmed && op.stage != Stage::Reserved;
    let armable = matches!(
        op.status,
        StageStatus::ReservedNotArmed | StageStatus::KnownOutcome
    ) && !matches!(record.phase, Phase::Held | Phase::Closed)
        && if named {
            op.stage == stage
        } else {
            // A return may re-arm until its Marker is gone; its applier skips sources already queued.
            stage == Stage::SourceReturn || !op.applied.contains(&stage)
        }
        && (op.kind != OperationKind::ReturnOnly || stage == Stage::SourceReturn);
    if !armable {
        return Err(refused(Refusal::NotArmable));
    }
    match (op.reuse.take(), grant) {
        (Some(reuse), Some(grant)) if grant != reuse => {
            return Err(refused(Refusal::Unfit("a resumed stage reuses its grant")));
        }
        // A debt the stage will issue is recorded once, before it can be published.
        (None, Some(grant)) => op.record_handoff(&grant),
        _ => {}
    }
    op.stage = stage;
    op.status = StageStatus::ApplyArmed;
    op.before = record.canonical_seal.clone();
    op.expected_after = Some(expected_after);
    op.arm_id = Some(arm_id.to_string());
    Ok(CustodyRecord {
        active_operation: Some(op),
        ..record.clone()
    })
}

/// Runs a test's change between a custody write and its commit (an error aborts the commit), or
/// holds a committed write before it mirrors into the index.
#[cfg(test)]
pub(crate) mod commit_fault {
    use tokio::sync::oneshot;

    type Fault = Box<dyn FnOnce() -> Result<(), String> + Send>;
    static NEXT: std::sync::Mutex<Vec<(String, Fault)>> = std::sync::Mutex::new(Vec::new());
    type Hold = (oneshot::Sender<()>, oneshot::Receiver<()>);
    static HELD: std::sync::Mutex<Vec<(String, &str, Hold)>> = std::sync::Mutex::new(Vec::new());

    /// The next committed write on `channel_id` fires the first half and then waits, its index
    /// mirror not yet made, until the second half is fired.
    pub(crate) fn hold_after_next(
        channel_id: &str,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        hold_at(channel_id, "committed")
    }

    /// The next time `channel_id` reaches the pause named `at`, it fires the first half and waits
    /// for the second.
    pub(crate) fn hold_at(
        channel_id: &str,
        at: &'static str,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let ((reached, committed), (release, held)) = (oneshot::channel(), oneshot::channel());
        let mut waiting = HELD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        waiting.push((channel_id.to_string(), at, (reached, held)));
        (committed, release)
    }

    pub(super) async fn committed(channel_id: &str) {
        pause(channel_id, "committed").await;
    }

    pub(super) async fn pause(channel_id: &str, at: &str) {
        let hold = {
            let mut waiting = HELD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let index = waiting
                .iter()
                .position(|(channel, point, _)| channel == channel_id && *point == at);
            index.map(|index| waiting.remove(index).2)
        };
        if let Some((reached, held)) = hold {
            let _ = reached.send(());
            let _ = held.await;
        }
    }

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

/// Pause points on the apply path and an effect count, per channel: a test's step runs once where
/// the code reaches the point, and the code goes on after it.
#[cfg(test)]
pub(crate) mod seam {
    use std::sync::{Mutex, MutexGuard};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Point {
        /// A carrier was counted as queued, before it is sent.
        Queued,
        /// A carried apply was entered and counted as active.
        Active,
        BeforeEffect,
        AfterEffect,
    }

    type Step = Box<dyn FnOnce() + Send>;
    static AT: Mutex<Vec<(String, Point, Step)>> = Mutex::new(Vec::new());
    static EFFECTS: Mutex<Vec<(String, usize)>> = Mutex::new(Vec::new());

    fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn at(channel_id: &str, point: Point, step: impl FnOnce() + Send + 'static) {
        lock(&AT).push((channel_id.to_string(), point, Box::new(step)));
    }

    pub(super) fn hit(channel_id: &str, point: Point) {
        let step = {
            let mut at = lock(&AT);
            let index = at
                .iter()
                .position(|(channel, at, _)| channel == channel_id && *at == point);
            index.map(|index| at.remove(index).2)
        };
        if let Some(step) = step {
            step();
        }
    }

    /// Counts one effect, then reaches [`Point::AfterEffect`].
    pub(super) fn effect(channel_id: &str) {
        {
            let mut effects = lock(&EFFECTS);
            match effects
                .iter_mut()
                .find(|(channel, _)| channel == channel_id)
            {
                Some((_, count)) => *count += 1,
                None => effects.push((channel_id.to_string(), 1)),
            }
        }
        hit(channel_id, Point::AfterEffect);
    }

    pub(crate) fn effects(channel_id: &str) -> usize {
        let effects = lock(&EFFECTS);
        let found = effects.iter().find(|(channel, _)| channel == channel_id);
        found.map_or(0, |(_, count)| *count)
    }
}

#[cfg(test)]
pub(crate) mod custody_pg_tests;
#[cfg(test)]
mod reconcile_pg_tests;
