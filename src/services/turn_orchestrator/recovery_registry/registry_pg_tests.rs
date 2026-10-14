use std::future::Future;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

use super::super::recovery_custody::capability::{
    AttemptKey, CapabilityRefusal, Load, RevokeHandle, Unsent,
};
use super::super::recovery_custody::commit_fault;
use super::super::recovery_custody::custody_pg_tests::{
    Fx, NODE, channel, evidence, legacy, namespace, scoped, seal,
};
use super::super::recovery_custody::denial_index::{Denial, DenialIndex, IndexState};
use super::super::recovery_custody::model::CanonicalSeal;
use super::super::recovery_custody::model::{Namespace, Owner, Phase};
use super::super::recovery_custody::operation::{Stage, StepOutcome};
use super::super::recovery_custody::reconcile::{
    Action, Evidence, LegacyResult, PriorExecution, ReturnPath, Verdict,
};
use super::super::recovery_custody::seam::{self, Point};
use super::super::recovery_custody::{ArmStep, Armed, CustodyStore, Reservation, ReserveRequest};
use super::super::recovery_custody::{CustodyError, Refusal};
use super::entry::{ArmRefusal, AttemptHandle, Begun, Slot};
use super::rehydrate::{EarlierProcess, Next, Rehydrated, rehydrate, settle};
use super::witness::Superseded;
use super::{Binding, RecoveryRegistry, RegistryRefusal};
use crate::db::o_channel_homes::HomeState;
use crate::services::cluster::channel_home_port::scoped_restore::{
    ExactWitness, InflightObservation, RestoreScope, RestoreStatus,
};

const EPOCH: i64 = 41;

fn id(channel_id: &str) -> u64 {
    channel_id.parse().expect("a numeric channel id")
}

fn scope(channel: u64, epoch: i64) -> RestoreScope {
    RestoreScope {
        channel,
        provider: "claude".into(),
        epoch,
        state: HomeState::Worker,
        holder: Some(NODE.into()),
        target: None,
        local: NODE.into(),
    }
}

fn status(attempt: &AttemptHandle) -> Option<RestoreStatus> {
    let witness = attempt.witness().reader().read();
    witness.map(|witness| witness.status)
}

/// The channel's home row, its slot and the slot's first attempt.
async fn started(fx: &Fx, registry: &RecoveryRegistry, ch: &str) -> (Arc<Slot>, AttemptHandle) {
    fx.home(ch, NODE, EPOCH).await;
    let slot = registry.resolve(id(ch)).expect("a slot");
    let Ok(Begun::Started(attempt)) = slot.begin(scope(id(ch), EPOCH), 1) else {
        panic!("the first observation starts the attempt");
    };
    (slot, attempt)
}

/// A process of its own on the same PG: its registry, its custody store and its empty index.
fn process(fx: &Fx) -> (RecoveryRegistry, CustodyStore, Arc<DenialIndex>) {
    let registry = RecoveryRegistry::isolated(8, 2);
    let index = Arc::new(DenialIndex::default());
    let owner = Owner {
        node: NODE.into(),
        process_incarnation: registry.process_incarnation().into(),
        attempt_id: "attempt-2".into(),
    };
    let custody = CustodyStore::new(fx.pool.clone(), owner, index.clone());
    (registry, custody, index)
}

fn arm_step<'a>(
    channel_id: &'a str,
    expected_revision: i64,
    operation_id: &'a str,
    stage: Stage,
    expected_after: CanonicalSeal,
    grant: Option<&str>,
) -> ArmStep<'a> {
    ArmStep {
        channel_id,
        expected_revision,
        operation_id,
        stage,
        expected_after,
        grant: grant.map(str::to_string),
    }
}

async fn reserve(fx: &Fx, custody: &CustodyStore, request: ReserveRequest) -> (i64, String) {
    let admission = fx.admit(&request.channel_id).await;
    match custody.reserve(admission, request).await.expect("reserved") {
        Reservation::Reserved {
            revision,
            operation_id,
            ..
        } => (revision, operation_id),
        other => panic!("not reserved: {other:?}"),
    }
}

/// Runs `work` to its end; at each of its suspension points no registry, slot or index lock may
/// be held. Spawning it also requires that no such guard lives across an await.
async fn free_at_every_await<T: Send + 'static>(
    registry: &RecoveryRegistry,
    slot: &Slot,
    index: &DenialIndex,
    work: impl Future<Output = T> + Send + 'static,
) -> T {
    let task = tokio::spawn(work);
    let mut suspensions = 0;
    while !task.is_finished() {
        tokio::task::yield_now().await;
        assert!(
            registry.unlocked() && slot.unlocked() && index.unlocked(),
            "a registry or index lock is held across an await"
        );
        suspensions += 1;
    }
    assert!(suspensions > 0, "the work never suspended");
    task.await.expect("the work ended")
}

#[test]
fn s3act_b3b4a2a_r01_every_default_and_resolve_sees_one_core_slot_and_attempt() {
    let (a, b) = (RecoveryRegistry::default(), RecoveryRegistry::default());
    assert!(
        Arc::ptr_eq(a.core(), b.core()),
        "each Default made a core of its own"
    );
    let channel = id(&channel(801));
    let slot = a.resolve(channel).expect("a slot");
    for registry in [&a, &b, &a.clone(), &RecoveryRegistry::default()] {
        let again = registry.resolve(channel).expect("the same slot");
        assert!(Arc::ptr_eq(&slot, &again) && again.id() == slot.id());
    }
    let lane = scope(channel, EPOCH);
    let Ok(Begun::Started(attempt)) = slot.begin(lane.clone(), 7) else {
        panic!("the first observation starts the attempt");
    };
    let _worker = attempt.worker().expect("a worker place");
    // The same lane observed again through any handle joins that attempt; nothing new is issued.
    for registry in [&a, &b] {
        let slot = registry.resolve(channel).expect("the same slot");
        let joined = match slot.begin(lane.clone(), 7) {
            Ok(Begun::Joined(reader)) => Some(reader.identity().clone()),
            _ => None,
        };
        assert_eq!(
            joined.as_ref(),
            Some(attempt.identity()),
            "a repeated observation started another attempt"
        );
    }
    let running = RegistryRefusal::NotQuiescent {
        open: true,
        workers: 1,
        arms: 0,
    };
    assert_eq!(slot.drained(), Err(running));
    assert_eq!(status(&attempt), Some(RestoreStatus::Restoring));
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a2a_r02_a_full_registry_evicts_nothing_and_reclaims_only_a_closed_slot_pg() {
    let fx = Fx::create().await;
    let registry = RecoveryRegistry::isolated(2, 2);
    let (first, second, third) = (channel(811), channel(812), channel(813));
    let before = seal("q1", None, None);
    let (slot1, attempt1) = started(&fx, &registry, &first).await;
    fx.reserve(scoped(&first, before.clone(), EPOCH)).await;
    let (slot2, attempt2) = started(&fx, &registry, &second).await;
    let (revision, op) = fx.reserve(scoped(&second, before.clone(), EPOCH)).await;
    // N+1: the first unfinished slot keeps its identity, witness and lane; the newcomer is refused.
    let overflow = registry.resolve(id(&third)).map(|slot| slot.id());
    let kept = registry.peek(id(&first));
    assert!(
        kept.is_some_and(|kept| Arc::ptr_eq(&kept, &slot1) && kept.id() == slot1.id()),
        "the first unfinished slot was evicted"
    );
    assert_eq!(status(&attempt1), Some(RestoreStatus::Restoring));
    let other_lane = registry
        .resolve(id(&first))
        .and_then(|slot| slot.begin(scope(id(&first), EPOCH + 1), 2).map(|_| ()));
    assert_eq!(other_lane, Err(RegistryRefusal::Foreign));
    assert_eq!(overflow, Err(RegistryRefusal::SlotsFull));
    // A legacy consumer takes no slot and is not capped by them.
    fx.reserve(legacy(&third, before.clone())).await;
    assert!(registry.peek(id(&third)).is_none());
    // The second attempt really ends with nothing applied; the first only closes its admission.
    let settled = fx.custody.settle_busy(&second, revision, &op).await;
    settled.expect("nothing applied");
    let open = RegistryRefusal::NotQuiescent {
        open: true,
        workers: 0,
        arms: 0,
    };
    let reclaim = async |ch: &str| {
        let (reclaimer, custody) = (registry.clone(), fx.custody.clone());
        let channel = id(ch);
        let work = async move { reclaimer.reclaim(&custody, channel).await };
        free_at_every_await(&registry, &slot2, &fx.index, work).await
    };
    assert_eq!(registry.reclaim(&fx.custody, id(&second)).await, Err(open));
    slot1.close_admission();
    slot2.close_admission();
    let raw = (fx.raw(&first).await, fx.raw(&second).await);
    assert_eq!(reclaim(&first).await, Err(RegistryRefusal::NotClosed));
    let kept = registry.peek(id(&first));
    assert!(kept.is_some_and(|kept| Arc::ptr_eq(&kept, &slot1)));
    assert_eq!(reclaim(&second).await, Ok(()));
    let after = (fx.raw(&first).await, fx.raw(&second).await);
    assert_eq!(after, raw, "a reclaim rewrote a PG row");
    let closed = fx.record(&second).await;
    assert_eq!((closed.phase, closed.ever_scoped), (Phase::Closed, true));
    // The reclaimed slot's handles are over, and the channel's next slot is another one.
    let old = attempt2.witness().reader();
    assert_eq!(old.read(), None);
    let late = attempt2.witness().record(RestoreStatus::Installed);
    assert_eq!(late, Err(Superseded));
    let rejoin = slot2.begin(scope(id(&second), EPOCH), 1).map(|_| ());
    assert_eq!(rejoin, Err(RegistryRefusal::Retired));
    let renewed = registry.resolve(id(&second)).expect("the freed place");
    assert_ne!(
        renewed.id(),
        slot2.id(),
        "a reclaimed slot's identity was reused"
    );
    let Ok(Begun::Started(next)) = renewed.begin(scope(id(&second), EPOCH), 1) else {
        panic!("the renewed slot starts its own attempt");
    };
    assert_ne!(next.identity(), attempt2.identity());
    drop(attempt2);
    assert_eq!(status(&next), Some(RestoreStatus::Restoring));
    assert_eq!(old.read(), None);
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a2a_r03_a_successor_waits_for_every_worker_apply_and_capability_pg() {
    let fx = Fx::create().await;
    let registry = RecoveryRegistry::isolated(4, 2);
    let ch = channel(821);
    let (slot, attempt) = started(&fx, &registry, &ch).await;
    let worker = attempt.worker().expect("a worker place");
    let request = scoped(&ch, seal("q1", None, None), EPOCH);
    let (revision, op) = fx.reserve(request).await;
    let exited = seal("q0", None, None);
    // A: the exit is armed and entered (check → pause); it reports before its apply returns.
    let step = arm_step(
        &ch,
        revision,
        &op,
        Stage::QueueCommit,
        exited.clone(),
        Some("debt-1"),
    );
    let first = worker.arm(&fx.custody, step).await.expect("armed and kept");
    let entered_a = fx.index.admit_write(&ch, Some(&first.capability));
    let entered_a = entered_a.expect("entered while armed");
    first.report.send(StepOutcome::Applied {
        after: Box::new(exited),
        handoff: None,
    });
    let revision = first.collector.await.unwrap().expect("recorded");
    // B: the same attempt's next arm, entered as well.
    let installed = seal("q0", None, Some("i1"));
    let step = arm_step(&ch, revision, &op, Stage::ActorInstall, installed, None);
    let second = worker.arm(&fx.custody, step).await.expect("armed and kept");
    let entered_b = fx.index.admit_write(&ch, Some(&second.capability));
    let entered_b = entered_b.expect("entered while armed");
    // revoke: off closes the attempt to a new apply, worker and arm; entered applies go on.
    slot.close_admission();
    let denied = fx
        .index
        .admit_write(&ch, Some(&second.capability))
        .map(drop);
    assert!(
        matches!(denied, Err(Denial(IndexState::ExactOperation { .. }))),
        "a revoked arm admitted a new apply: {denied:?}"
    );
    let late_worker = attempt.worker().map(|_| ());
    assert_eq!(late_worker, Err(RegistryRefusal::AdmissionClosed));
    second.report.send(StepOutcome::NotApplied);
    second.collector.await.unwrap().expect("recorded");
    // Succession attempts during the pause: none starts an attempt or writes the row.
    let raw = fx.raw(&ch).await;
    let next = scope(id(&ch), EPOCH + 1);
    let succeed = async || {
        let successor = slot.retry(&fx.custody, next.clone(), 2).await;
        let successor = successor.map(|successor| successor.identity().attempt);
        if successor.is_err() {
            assert_eq!(fx.raw(&ch).await, raw, "a refused succession wrote the row");
            assert_eq!(status(&attempt), Some(RestoreStatus::Restoring));
        }
        successor
    };
    let waiting = |workers, arms| {
        Err(RegistryRefusal::NotQuiescent {
            open: false,
            workers,
            arms,
        })
    };
    let busy = succeed().await;
    assert!(
        matches!(busy, Err(RegistryRefusal::NotQuiescent { workers: 1, .. })),
        "a successor started beside a live worker: {busy:?}"
    );
    let unknown = slot.prior(&fx.record(&ch).await);
    assert_eq!(unknown, PriorExecution::Unknown);
    drop(worker);
    let busy = succeed().await;
    assert!(
        matches!(busy, Err(RegistryRefusal::NotQuiescent { workers: 0, .. })),
        "a successor started beside entered applies: {busy:?}"
    );
    // B drains first: the earlier arm's apply still counts under its own arm.
    drop((entered_b, second.capability));
    let busy = succeed().await;
    assert_eq!(
        busy,
        waiting(0, 1),
        "a successor started while an earlier arm applied"
    );
    // A's apply returns, but its capability is still held in a queue.
    drop(entered_a);
    let queued = vec![first.capability];
    let busy = succeed().await;
    assert_eq!(
        busy,
        waiting(0, 1),
        "a successor started beside a queued capability"
    );
    drop(queued);
    // resume complete: the registry vouches for the row's own arm, and the successor starts.
    let record = fx.record(&ch).await;
    let current = record.active_operation.as_ref().expect("the operation");
    let prior = slot.prior(&record);
    assert!(
        matches!(&prior, PriorExecution::Ended(proof) if proof.names(current)),
        "{prior:?}"
    );
    let earlier = attempt.identity().attempt;
    let successor = slot.retry(&fx.custody, next, 2).await;
    let successor = successor.expect("everything ended");
    assert_ne!(successor.identity().attempt, earlier);
    assert_eq!(
        fx.raw(&ch).await,
        raw,
        "starting the successor wrote the row"
    );
    let late = attempt.witness().record(RestoreStatus::Installed);
    assert_eq!(late, Err(Superseded));
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a2a_a00_an_undecided_row_settles_first_and_no_lock_crosses_an_await_pg() {
    let fx = Fx::create().await;
    let (registry, custody, index) = process(&fx);
    let ch = channel(831);
    let channel = id(&ch);
    let (slot, attempt) = started(&fx, &registry, &ch).await;
    let request = scoped(&ch, seal("q1", None, None), EPOCH);
    let (revision, op) = reserve(&fx, &custody, request).await;
    let exited = seal("q0", None, None);
    let worker = attempt.worker().expect("a worker place");
    let step = arm_step(
        &ch,
        revision,
        &op,
        Stage::QueueCommit,
        exited.clone(),
        Some("debt-1"),
    );
    let armed = worker.arm(&custody, step).await.expect("armed and kept");
    drop(worker);
    // The applier is lost: everything ended, yet the outcome is undecided.
    let Armed {
        capability,
        report,
        collector,
        ..
    } = armed;
    drop((capability, report));
    collector.await.unwrap().expect("recorded");
    slot.close_admission();
    let raw = fx.raw(&ch).await;
    let next = scope(channel, EPOCH + 1);
    let retry = async || {
        let (retried, custody, next) = (slot.clone(), custody.clone(), next.clone());
        let work = async move { retried.retry(&custody, next, 2).await };
        let successor = free_at_every_await(&registry, &slot, &index, work).await;
        successor.map(|successor| successor.identity().attempt)
    };
    assert_eq!(retry().await, Err(RegistryRefusal::Undecided));
    assert_eq!(fx.raw(&ch).await, raw, "a refused succession wrote the row");
    // The registry's proof of that arm settles it through a reconcile; then the successor starts.
    let work = {
        let (registry, custody) = (registry.clone(), custody.clone());
        async move { rehydrate(&registry, &custody, NODE, channel, EarlierProcess::Unknown).await }
    };
    let rehydrated = free_at_every_await(&registry, &slot, &index, work).await;
    let Ok(Rehydrated::Row { revision, prior }) = rehydrated else {
        panic!("the row is this process's own: {rehydrated:?}");
    };
    assert!(matches!(prior, PriorExecution::Ended(_)), "{prior:?}");
    let found = Evidence {
        debt: Some("debt-1".into()),
        ..evidence(exited, prior)
    };
    let work = {
        let custody = custody.clone();
        async move { settle(&custody, channel, revision, Action::RecheckApply, &found).await }
    };
    let settled = free_at_every_await(&registry, &slot, &index, work).await;
    let (step, _) = settled.expect("settled");
    assert_eq!(step, Next::Continue(Verdict::Confirmed));
    let earlier = attempt.identity().attempt;
    let successor = retry()
        .await
        .expect("a decided operation admits the successor");
    assert_ne!(successor, earlier);
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a2a_r04_a_late_result_or_drop_of_an_earlier_attempt_changes_nothing_pg() {
    let fx = Fx::create().await;
    let registry = RecoveryRegistry::isolated(4, 2);
    let ch = channel(851);
    let lane = scope(id(&ch), EPOCH);
    let (slot, first) = started(&fx, &registry, &ch).await;
    let old_reader = first.witness().reader();
    slot.close_admission();
    // The same generation, another attempt.
    let second = slot.retry(&fx.custody, lane.clone(), 1).await;
    let second = second.expect("the next attempt");
    let (earlier, current) = (first.identity(), second.identity());
    assert_eq!(earlier.generation, current.generation);
    assert_ne!(earlier.attempt, current.attempt);
    second.witness().record(RestoreStatus::Installed).unwrap();
    let installed = second.witness().reader().read();
    assert_eq!(status(&second), Some(RestoreStatus::Installed));
    // The earlier attempt's late results and its live read handle are not the current authority.
    let late = first
        .witness()
        .record(RestoreStatus::Blocked("late".into()));
    assert_eq!(late, Err(Superseded));
    let late = first.witness().observe(InflightObservation::Active);
    assert_eq!(late, Err(Superseded));
    assert_eq!(old_reader.read(), None);
    assert_eq!(second.witness().reader().read(), installed);
    // Its late Drop, and a Drop racing a third attempt of the same generation, change nothing.
    drop(first);
    assert_eq!(second.witness().reader().read(), installed);
    slot.close_admission();
    let third = slot.retry(&fx.custody, lane, 1).await;
    let third = third.expect("the next attempt");
    drop(second);
    assert_eq!(status(&third), Some(RestoreStatus::Restoring));
    assert_eq!(old_reader.read(), None);
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a2a_rehydrate_shows_a_row_undecided_and_tells_a_live_owner_from_a_lost_one_pg() {
    let fx = Fx::create().await;
    let before = seal("q1", None, None);
    let exited = seal("q0", None, None);
    let found = |prior| Evidence {
        debt: Some("debt-1".into()),
        ..evidence(exited.clone(), prior)
    };
    let returned = |path| {
        Next::Continue(Verdict::Return {
            path,
            confirmed: Some(Stage::QueueCommit),
        })
    };
    let exit = (Stage::QueueCommit, exited.clone(), Some("debt-1"));
    // An earlier process armed the exit and was lost.
    let (lost, unnamed) = (channel(841), channel(843));
    let mut armed_at = Vec::new();
    for ch in [&lost, &unnamed] {
        fx.home(ch, NODE, EPOCH).await;
        let (revision, op) = fx.reserve(scoped(ch, before.clone(), EPOCH)).await;
        let (stage, after, grant) = exit.clone();
        let crashed = fx.arm_and_crash(ch, revision, &op, stage, after, grant);
        armed_at.push(crashed.await);
    }
    let (registry, custody, index) = process(&fx);
    let rehydrated = async |ch: &str, node, earlier| {
        let rehydrated = rehydrate(&registry, &custody, node, id(ch), earlier).await;
        rehydrated.expect("read")
    };
    // Registered as undecided before anything is judged; without the process's end it is held.
    assert_eq!(index.state(&lost), IndexState::Unknown);
    let held = rehydrated(&lost, NODE, EarlierProcess::Unknown).await;
    assert_eq!(held, Rehydrated::Held("the end of the earlier process"));
    assert_eq!(index.state(&lost), IndexState::Blocked);
    assert!(
        registry.peek(id(&lost)).is_some(),
        "a scoped row takes a slot"
    );
    let foreign = rehydrated(&lost, "node-b", EarlierProcess::Gone).await;
    assert_eq!(foreign, Rehydrated::Held("the owning node's own reconcile"));
    // The lost process's row is rebuilt from the durable row: the lease is gone.
    let Rehydrated::Row { revision, prior } = rehydrated(&lost, NODE, EarlierProcess::Gone).await
    else {
        panic!("the row is judged once its process is known gone");
    };
    assert_eq!(revision, armed_at[0]);
    let evidence_lost = found(prior);
    let settled = settle(
        &custody,
        id(&lost),
        revision,
        Action::CompleteReturn,
        &evidence_lost,
    );
    let (step, _) = settled.await.expect("settled");
    assert_eq!(step, returned(ReturnPath::DurableReconstruct));
    // A row armed before arms had an identity: no live process vouches for it, a restart does.
    sqlx::query(
        "UPDATE channel_recovery_custody SET active_operation = active_operation - 'arm_id'
          WHERE channel_id = $1",
    )
    .bind(&unnamed)
    .execute(&fx.pool)
    .await
    .unwrap();
    let (slot, _attempt) = started(&fx, &RecoveryRegistry::isolated(1, 1), &channel(844)).await;
    let live = slot.prior(&fx.record(&unnamed).await);
    assert!(
        matches!(&live, PriorExecution::SameOwnerLive(owner) if owner.applier_ended.is_none()),
        "{live:?}"
    );
    let Rehydrated::Row { revision, prior } =
        rehydrated(&unnamed, NODE, EarlierProcess::Gone).await
    else {
        panic!("the row is judged once its process is known gone");
    };
    let unpublished = evidence(before.clone(), prior);
    let settled = settle(
        &custody,
        id(&unnamed),
        revision,
        Action::RecheckApply,
        &unpublished,
    );
    let (step, _) = settled.await.expect("settled");
    let resumed = Verdict::Resume {
        stage: Stage::QueueCommit,
        reuse: Some("debt-1".into()),
    };
    assert_eq!(step, Next::Continue(resumed));
    // This process's own row, its applier over while the attempt lives: the live lease returns.
    let own = channel(842);
    let (_slot, attempt) = started(&fx, &registry, &own).await;
    let (revision, op) = reserve(&fx, &custody, scoped(&own, before.clone(), EPOCH)).await;
    let (stage, after, grant) = exit;
    let worker = attempt.worker().expect("a worker place");
    let step = arm_step(&own, revision, &op, stage, after, grant);
    let armed = worker.arm(&custody, step).await.expect("armed and kept");
    drop(worker);
    let Armed {
        capability,
        report,
        collector,
        ..
    } = armed;
    drop((capability, report));
    collector.await.unwrap().expect("recorded");
    let Rehydrated::Row { revision, prior } = rehydrated(&own, NODE, EarlierProcess::Unknown).await
    else {
        panic!("the row is this process's own");
    };
    assert!(
        matches!(&prior, PriorExecution::SameOwnerLive(owner) if owner.applier_ended.is_some()),
        "{prior:?}"
    );
    let evidence_own = found(prior);
    let settled = settle(
        &custody,
        id(&own),
        revision,
        Action::CompleteReturn,
        &evidence_own,
    );
    let (step, _) = settled.await.expect("settled");
    assert_eq!(step, returned(ReturnPath::LiveLease));
    // A legacy hold with nothing undecided returns to work; it takes no slot.
    let legacy_held = channel(845);
    let marker = seal("q1", Some("m1"), None);
    let (revision, op) = fx.reserve(legacy(&legacy_held, before.clone())).await;
    let stage = Stage::MarkerPublish;
    let revision = fx.apply(&legacy_held, revision, &op, stage, marker.clone());
    let revision = revision.await;
    let commit = (Stage::QueueCommit, seal("q0", Some("m1"), None));
    let armed = fx.arm(&legacy_held, revision, &op, commit.0, commit.1, None);
    let armed = armed.await;
    armed.report.send(StepOutcome::NotApplied);
    let revision = armed.collector.await.unwrap().expect("recorded");
    let unknown =
        fx.custody
            .record_legacy_result(&legacy_held, revision, &op, &LegacyResult::Unknown);
    assert!(matches!(unknown.await, Ok((Verdict::Held { .. }, _))));
    let settled_hold = fx.record(&legacy_held).await.active_operation;
    let Rehydrated::Row { revision, prior } =
        rehydrated(&legacy_held, NODE, EarlierProcess::Gone).await
    else {
        panic!("the row is judged once its process is known gone");
    };
    assert!(
        registry.peek(id(&legacy_held)).is_none(),
        "a legacy row took a slot"
    );
    let seen = evidence(marker, prior);
    let settled = settle(
        &custody,
        id(&legacy_held),
        revision,
        Action::RecheckApply,
        &seen,
    );
    let (step, _) = settled.await.expect("settled");
    assert_eq!(step, Next::Continue(Verdict::Restored));
    let restored = fx.record(&legacy_held).await;
    assert_eq!(restored.phase, Phase::LegacyReserved);
    assert_eq!(restored.active_operation, settled_hold);
    // A PG `installed` restores no witness: the slot starts again from a first attempt.
    let installed = channel(846);
    fx.home(&installed, NODE, EPOCH).await;
    let (revision, op) = fx.reserve(scoped(&installed, before, EPOCH)).await;
    let stage = Stage::QueueCommit;
    fx.apply(&installed, revision, &op, stage, exited).await;
    sqlx::query("UPDATE channel_recovery_custody SET phase = 'installed' WHERE channel_id = $1")
        .bind(&installed)
        .execute(&fx.pool)
        .await
        .unwrap();
    let judged = rehydrated(&installed, NODE, EarlierProcess::Gone).await;
    assert!(matches!(judged, Rehydrated::Row { .. }), "{judged:?}");
    assert_eq!(index.state(&installed), IndexState::Blocked);
    let slot = registry
        .peek(id(&installed))
        .expect("a kept install takes a slot");
    let Ok(Begun::Started(again)) = slot.begin(scope(id(&installed), EPOCH), 1) else {
        panic!("a rehydrated slot has no attempt yet");
    };
    assert_eq!(status(&again), Some(RestoreStatus::Restoring));
    // No row: nothing is registered.
    let absent = channel(847);
    let nothing = rehydrated(&absent, NODE, EarlierProcess::Gone).await;
    assert_eq!(nothing, Rehydrated::Nothing);
    assert_eq!(index.state(&absent), IndexState::Unknown);
    assert!(registry.peek(id(&absent)).is_none());
    fx.close().await;
}

/// A payload no retry could copy: what comes back is the one that was sent.
#[derive(Debug, PartialEq, Eq)]
struct Payload(u32);

/// Records the arm's load once, when `channel_id`'s apply reaches `point`.
fn observe(channel_id: &str, point: Point, arm: &RevokeHandle) -> Arc<Mutex<Option<Load>>> {
    let seen = Arc::new(Mutex::new(None));
    let (into, arm) = (seen.clone(), arm.clone());
    let record = move || *into.lock().unwrap_or_else(|p| p.into_inner()) = Some(arm.load());
    seam::at(channel_id, point, record);
    seen
}

fn seen(at: &Mutex<Option<Load>>) -> Option<Load> {
    at.lock().unwrap_or_else(|p| p.into_inner()).take()
}

fn load(held: bool, queued: usize, active: usize) -> Load {
    Load {
        held,
        queued,
        active,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a2b0_a_a_lookup_creates_nothing_and_never_settles_unmanaged_pg() {
    let fx = Fx::create().await;
    let (registry, custody, index) = process(&fx);
    // An earlier process's row this process has not rehydrated yet.
    let earlier = channel(861);
    fx.home(&earlier, NODE, EPOCH).await;
    fx.reserve(scoped(&earlier, seal("q1", None, None), EPOCH))
        .await;
    let found = registry.binding(&custody, id(&earlier)).await;
    assert!(
        registry.peek(id(&earlier)).is_none(),
        "the lookup created a slot"
    );
    assert_eq!(
        index.entry(&earlier),
        None,
        "the lookup made an index entry"
    );
    assert_eq!(found, Binding::Attached(IndexState::Unknown));
    // Only a read that found no row answers unmanaged.
    let absent = channel(863);
    let found = registry.binding(&custody, id(&absent)).await;
    assert_eq!(found, Binding::Unmanaged);
    assert!(registry.peek(id(&absent)).is_none() && index.entry(&absent).is_none());
    // The same request after the row is rehydrated finds it attached and undecided.
    let rehydrated = rehydrate(
        &registry,
        &custody,
        NODE,
        id(&earlier),
        EarlierProcess::Gone,
    );
    let rehydrated = rehydrated.await.expect("read");
    assert!(
        matches!(rehydrated, Rehydrated::Row { .. }),
        "{rehydrated:?}"
    );
    let found = registry.binding(&custody, id(&earlier)).await;
    assert_eq!(found, Binding::Attached(IndexState::Blocked));
    // An attempt with nothing decided is attached but unknown, then follows the index.
    let ch = channel(862);
    let (_slot, attempt) = started(&fx, &registry, &ch).await;
    assert_eq!(index.entry(&ch), None);
    let found = registry.binding(&custody, id(&ch)).await;
    assert_eq!(found, Binding::Attached(IndexState::Unknown));
    let (revision, op) = reserve(&fx, &custody, scoped(&ch, seal("q1", None, None), EPOCH)).await;
    let found = registry.binding(&custody, id(&ch)).await;
    assert_eq!(found, Binding::Attached(IndexState::Blocked));
    let worker = attempt.worker().expect("a worker place");
    let step = arm_step(
        &ch,
        revision,
        &op,
        Stage::QueueCommit,
        seal("q0", None, None),
        None,
    );
    let armed = worker.arm(&custody, step).await.expect("armed and kept");
    let exact = IndexState::ExactOperation {
        operation_id: op,
        stage: Stage::QueueCommit,
        arm_id: armed.revoke.arm_id().into(),
    };
    let found = registry.binding(&custody, id(&ch)).await;
    assert_eq!(found, Binding::Attached(exact));
    armed.report.send(StepOutcome::NotApplied);
    armed.collector.await.unwrap().expect("recorded");
    // A failed read is not unmanaged either.
    fx.pool.close().await;
    let found = registry.binding(&custody, id(&absent)).await;
    assert_eq!(found, Binding::Unread);
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a2b0_bcdg_a_carried_apply_never_idles_and_a_refusal_hands_it_back_pg() {
    let fx = Fx::create().await;
    let registry = RecoveryRegistry::isolated(4, 2);
    let ch = channel(871);
    let (_slot, attempt) = started(&fx, &registry, &ch).await;
    let worker = attempt.worker().expect("a worker place");
    let (revision, op) = fx.reserve(scoped(&ch, seal("q1", None, None), EPOCH)).await;
    let exited = seal("q0", None, None);
    let step = arm_step(
        &ch,
        revision,
        &op,
        Stage::QueueCommit,
        exited,
        Some("debt-1"),
    );
    let Armed {
        capability,
        report,
        revoke,
        collector,
    } = worker.arm(&fx.custody, step).await.expect("armed and kept");
    assert_eq!(revoke.load(), load(true, 0, 0));
    // The apply counts as queued before the send.
    let at_queue = observe(&ch, Point::Queued, &revoke);
    let carrier = capability.carry(Payload(7));
    assert_eq!(seen(&at_queue), Some(load(false, 1, 0)));
    let raw = fx.raw(&ch).await;
    // A refused entry hands back the same carrier, still queued, with its payload.
    let elsewhere = channel(872);
    let Err(refused) = fx.index.admit_carried(&elsewhere, carrier) else {
        panic!("a carrier entered another channel");
    };
    assert_eq!(
        revoke.load(),
        load(false, 1, 0),
        "a refused carrier stopped counting"
    );
    assert_eq!(
        refused.carrier.payload(),
        Some(&Payload(7)),
        "the refused payload was lost"
    );
    let unknown = Unsent::Denied(Denial(IndexState::Unknown));
    assert_eq!(refused.refusal, unknown);
    assert_eq!(refused.carrier.arm_id(), revoke.arm_id());
    // So do a gone mailbox and a closed applier.
    let (gone, receiver) = mpsc::unbounded_channel();
    drop(receiver);
    let Err(unsent) = refused.carrier.send(&gone) else {
        panic!("sent to a gone mailbox");
    };
    assert_eq!(unsent.refusal, Unsent::SendFailed);
    assert_eq!(unsent.carrier.payload(), Some(&Payload(7)));
    let closed = unsent.carrier.refuse(Unsent::Closed);
    assert_eq!(closed.carrier.payload(), Some(&Payload(7)));
    assert_eq!(revoke.load(), load(false, 1, 0));
    assert_eq!(
        fx.raw(&ch).await,
        raw,
        "a refusal re-armed or wrote the row"
    );
    // The retry sends that carrier; its entry turns queued into active in one step.
    let (mailbox, mut received) = mpsc::unbounded_channel();
    closed.carrier.send(&mailbox).expect("sent");
    let carrier = received.recv().await.expect("received");
    let at_entry = observe(&ch, Point::Active, &revoke);
    let Ok((guard, payload)) = fx.index.admit_carried(&ch, carrier) else {
        panic!("the retried carrier was refused");
    };
    assert_eq!(seen(&at_entry), Some(load(false, 0, 1)));
    assert_eq!(payload, Payload(7));
    // A nested writer borrows the entered apply for its own target only.
    let other_namespace = Namespace {
        token_hash: "token-b".into(),
        ..namespace()
    };
    let wrong = [
        guard.lend(&elsewhere, &namespace()).map(|_| ()),
        guard.lend(&ch, &other_namespace).map(|_| ()),
    ];
    assert_eq!(
        wrong,
        [
            Err(CapabilityRefusal::WrongTarget),
            Err(CapabilityRefusal::WrongTarget)
        ]
    );
    let lent = guard.lend(&ch, &namespace()).expect("its own target");
    assert_eq!(
        (lent.operation_id(), lent.stage(), lent.arm_id()),
        (op.as_str(), Stage::QueueCommit, revoke.arm_id())
    );
    let effects = seam::effects(&ch);
    let before = observe(&ch, Point::BeforeEffect, &revoke);
    let after = observe(&ch, Point::AfterEffect, &revoke);
    lent.effect(|| ());
    assert_eq!(seam::effects(&ch), effects + 1);
    assert_eq!(seen(&before), Some(load(false, 0, 1)));
    assert_eq!(seen(&after), Some(load(false, 0, 1)));
    // The borrow neither ends nor outlives the apply: a revoke waits for the guard.
    revoke.revoke();
    assert_eq!(revoke.load(), load(false, 0, 1), "the lent apply ended it");
    assert_eq!(revoke.end_proof(), None);
    drop(guard);
    assert_eq!(revoke.load(), load(false, 0, 0));
    assert!(revoke.end_proof().is_some());
    report.send(StepOutcome::NotApplied);
    collector.await.unwrap().expect("recorded");
    drop(worker);
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a2b0_e_an_arm_is_kept_only_by_its_own_attempt_and_namespace_pg() {
    let fx = Fx::create().await;
    let registry = RecoveryRegistry::isolated(4, 2);
    let (ch, other) = (channel(881), channel(882));
    let (_slot, attempt) = started(&fx, &registry, &ch).await;
    let (other_slot, other_attempt) = started(&fx, &registry, &other).await;
    let worker = attempt.worker().expect("a worker place");
    let other_worker = other_attempt.worker().expect("a worker place");
    let (revision, op) = fx.reserve(scoped(&ch, seal("q1", None, None), EPOCH)).await;
    let exited = seal("q0", None, None);
    // A custody refusal comes back as such and keeps nothing.
    let stale = arm_step(
        &ch,
        revision + 1,
        &op,
        Stage::QueueCommit,
        exited.clone(),
        None,
    );
    let stale = worker.arm(&fx.custody, stale).await.map(drop);
    assert!(
        matches!(
            stale,
            Err(ArmRefusal::Custody(CustodyError::Refused(Refusal::Stale)))
        ),
        "{stale:?}"
    );
    let step = arm_step(&ch, revision, &op, Stage::QueueCommit, exited.clone(), None);
    let first = worker.arm(&fx.custody, step).await.expect("armed and kept");
    assert_eq!(first.revoke.attempt(), Some(&worker.key()));
    assert_ne!(first.revoke.attempt(), Some(&other_worker.key()));
    assert_ne!(first.revoke.attempt(), Some(&AttemptKey::default()));
    assert_eq!(first.revoke.namespace(), &namespace());
    // Another attempt does not take the arm, and leaves it to its own.
    let kept = other_worker.keep(&first);
    assert_eq!(
        kept,
        Err(RegistryRefusal::Foreign),
        "another attempt kept the arm"
    );
    let live = first.capability.check(&ch, &op, Stage::QueueCommit);
    assert_eq!(live, Ok(()));
    other_slot.close_admission();
    drop(other_worker);
    assert!(
        other_slot.quiesce().is_ok(),
        "the refused arm counted there"
    );
    // Giving a refused carrier up returns its payload and ends the arm's count of it.
    let carried = first.capability.carry(Payload(1));
    let Err(refused) = fx.index.admit_carried(&other, carried) else {
        panic!("a carrier entered another channel");
    };
    assert_eq!(refused.carrier.into_payload(), Some(Payload(1)));
    assert_eq!(first.revoke.load(), load(false, 0, 0));
    first.report.send(StepOutcome::Applied {
        after: Box::new(exited),
        handoff: None,
    });
    let revision = first.collector.await.unwrap().expect("recorded");
    // An arm made outside any attempt is not kept either.
    let installed = seal("q0", None, Some("i1"));
    let plain = fx.arm(
        &ch,
        revision,
        &op,
        Stage::ActorInstall,
        installed.clone(),
        None,
    );
    let plain = plain.await;
    assert_eq!(plain.revoke.attempt(), None);
    assert_eq!(worker.keep(&plain), Err(RegistryRefusal::Foreign));
    plain.report.send(StepOutcome::NotApplied);
    let revision = plain.collector.await.unwrap().expect("recorded");
    // The row now names another namespace: an arm there, though issued to this attempt, is not.
    sqlx::query("UPDATE channel_recovery_custody SET token_hash = 'token-b' WHERE channel_id = $1")
        .bind(&ch)
        .execute(&fx.pool)
        .await
        .unwrap();
    let step = arm_step(&ch, revision, &op, Stage::ActorInstall, installed, None);
    let moved = fx.custody.arm_for(&worker, step).await.map(drop);
    assert!(
        matches!(moved, Err(ArmRefusal::Registry(RegistryRefusal::Foreign))),
        "an arm in another namespace was kept: {moved:?}"
    );
    drop(worker);
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a2b0_f_an_owner_change_waits_for_the_registry_quiescence_permit_pg() {
    let fx = Fx::create().await;
    let (registry, custody, index) = process(&fx);
    let ch = channel(891);
    let (slot, attempt) = started(&fx, &registry, &ch).await;
    let worker = attempt.worker().expect("a worker place");
    let (revision, op) = reserve(&fx, &custody, scoped(&ch, seal("q1", None, None), EPOCH)).await;
    // A arms and keeps the arm, enters its apply, and reports its first stage not applied.
    let step = arm_step(
        &ch,
        revision,
        &op,
        Stage::QueueCommit,
        seal("q0", None, None),
        None,
    );
    let Armed {
        capability,
        report,
        collector,
        ..
    } = worker.arm(&custody, step).await.expect("armed and kept");
    let entered = index.admit_write(&ch, Some(&capability));
    let entered = entered.expect("entered while armed");
    report.send(StepOutcome::NotApplied);
    collector.await.unwrap().expect("recorded");
    let closed = fx.record(&ch).await;
    assert_eq!(
        (closed.phase, closed.active_operation),
        (Phase::Closed, None)
    );
    slot.close_admission();
    let (raw, earlier) = (fx.raw(&ch).await, attempt.identity().attempt);
    let step = arm_step(
        &ch,
        closed.revision,
        &op,
        Stage::QueueCommit,
        seal("q0", None, None),
        None,
    );
    let late = worker.arm(&custody, step).await.map(drop);
    assert!(
        matches!(
            late,
            Err(ArmRefusal::Registry(RegistryRefusal::AdmissionClosed))
        ),
        "a closed attempt armed: {late:?}"
    );
    // Another store of this node takes the closed row over while A's guard, capability and worker
    // live: no owner change, no write, no new attempt and no new worker.
    let successor = || scoped(&ch, seal("q9", None, None), EPOCH);
    let taken = fx.custody.reserve(fx.admit(&ch).await, successor()).await;
    assert_eq!(
        fx.record(&ch).await.owner,
        closed.owner,
        "the owner changed while A still applied"
    );
    assert_eq!(fx.raw(&ch).await, raw, "a refused succession wrote the row");
    assert!(
        matches!(taken, Err(CustodyError::Refused(Refusal::NotQuiescent))),
        "{taken:?}"
    );
    let busy = |workers, arms| {
        Err(RegistryRefusal::NotQuiescent {
            open: false,
            workers,
            arms,
        })
    };
    assert_eq!(slot.quiesce().map(drop), busy(1, 1));
    let next = scope(id(&ch), EPOCH + 1);
    let retried = slot.retry(&fx.custody, next.clone(), 2).await;
    assert_eq!(retried.map(drop), busy(1, 1));
    assert_eq!(
        attempt.worker().map(drop),
        Err(RegistryRefusal::AdmissionClosed)
    );
    drop(worker);
    assert_eq!(slot.quiesce().map(drop), busy(0, 1));
    drop(entered);
    assert_eq!(slot.quiesce().map(drop), busy(0, 1));
    drop(capability);
    // A permit is asked again at the commit: a new attempt since then voids it.
    let stale = slot.quiesce().expect("A is closed and drained");
    let started_since = slot.retry(&fx.custody, next.clone(), 2).await;
    let started_since = started_since.expect("a decided row admits the next attempt");
    assert_ne!(started_since.identity().attempt, earlier);
    let voided = fx
        .custody
        .reserve_successor(fx.admit(&ch).await, successor(), stale)
        .await;
    assert!(
        matches!(voided, Err(CustodyError::Refused(Refusal::NotQuiescent))),
        "{voided:?}"
    );
    assert_eq!(
        fx.raw(&ch).await,
        raw,
        "a voided permit let the owner change"
    );
    // With a current permit the owner changes, and no lock is held across any of its awaits.
    slot.close_admission();
    let permit = slot
        .quiesce()
        .expect("the next attempt is closed and drained");
    let admission = fx.admit(&ch).await;
    let work = {
        let (custody, request) = (fx.custody.clone(), successor());
        async move { custody.reserve_successor(admission, request, permit).await }
    };
    let taken = free_at_every_await(&registry, &slot, &fx.index, work).await;
    assert!(
        matches!(taken, Ok(Reservation::Reserved { .. })),
        "{taken:?}"
    );
    let row = fx.record(&ch).await;
    assert_ne!(row.owner, closed.owner);
    assert_eq!(row.phase, Phase::ScopedReserved);
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a2b0_r1b_a_worker_covers_its_arm_until_the_attempt_keeps_it_pg() {
    let fx = Fx::create().await;
    let (registry, custody, _index) = process(&fx);
    let ch = channel(895);
    let (slot, attempt) = started(&fx, &registry, &ch).await;
    let worker = attempt.worker().expect("a worker place");
    let (revision, op) = reserve(&fx, &custody, scoped(&ch, seal("q1", None, None), EPOCH)).await;
    let (committed, release) = commit_fault::hold_after_next(&ch);
    let arming = tokio::spawn({
        let (custody, ch) = (custody.clone(), ch.clone());
        async move {
            let exited = seal("q0", None, None);
            let step = arm_step(&ch, revision, &op, Stage::QueueCommit, exited, None);
            let armed = worker.arm(&custody, step).await.map(drop);
            (worker, armed)
        }
    });
    // The arm committed and is not kept yet: its worker keeps the attempt from quiescing.
    committed.await.expect("the arm committed");
    slot.close_admission();
    let busy = RegistryRefusal::NotQuiescent {
        open: false,
        workers: 1,
        arms: 0,
    };
    assert_eq!(slot.quiesce().map(drop), Err(busy));
    release.send(()).expect("released");
    let (worker, armed) = arming.await.expect("ended");
    assert!(
        matches!(
            armed,
            Err(ArmRefusal::Registry(RegistryRefusal::AdmissionClosed))
        ),
        "{armed:?}"
    );
    // The closed attempt counted the arm and revoked it; given up, it drained with the worker.
    drop(worker);
    assert!(slot.quiesce().is_ok(), "the refused arm stayed live");
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a2b0_r1b_an_arm_the_registry_did_not_keep_never_lets_the_owner_change_pg() {
    let fx = Fx::create().await;
    let (registry, custody, _index) = process(&fx);
    let ch = channel(896);
    let (slot, attempt) = started(&fx, &registry, &ch).await;
    let worker = attempt.worker().expect("a worker place");
    let (revision, op) = reserve(&fx, &custody, scoped(&ch, seal("q1", None, None), EPOCH)).await;
    let exited = seal("q0", None, None);
    let step = arm_step(&ch, revision, &op, Stage::QueueCommit, exited, None);
    let Armed {
        capability,
        report,
        collector,
        ..
    } = worker.arm(&custody, step).await.expect("armed and kept");
    drop(worker);
    report.send(StepOutcome::NotApplied);
    collector.await.unwrap().expect("recorded");
    slot.close_admission();
    let (owner, raw) = (fx.record(&ch).await.owner, fx.raw(&ch).await);
    // Another process takes the row over as far as this registry lets it, while A's capability
    // lives: the owner and the row stay as they are.
    let (permit, refused) = match slot.quiesce() {
        Ok(permit) => (Some(permit), None),
        Err(refusal) => (None, Some(refusal)),
    };
    if let Some(permit) = permit {
        let successor = scoped(&ch, seal("q9", None, None), EPOCH);
        let admission = fx.admit(&ch).await;
        let taken = fx.custody.reserve_successor(admission, successor, permit);
        let _ = taken.await;
    }
    assert_eq!(
        fx.record(&ch).await.owner,
        owner,
        "the owner changed while A's capability lived"
    );
    assert_eq!(fx.raw(&ch).await, raw, "a refused succession wrote the row");
    let busy = RegistryRefusal::NotQuiescent {
        open: false,
        workers: 0,
        arms: 1,
    };
    assert_eq!(refused, Some(busy));
    drop(capability);
    assert!(
        slot.quiesce().is_ok(),
        "the given-up capability stayed live"
    );
    fx.close().await;
}

type Pause = (oneshot::Receiver<()>, oneshot::Sender<()>);

/// Another process's reservation of `ch` with the slot's permit, spawned and paused at `at`.
async fn succession(
    fx: &Fx,
    slot: &Arc<Slot>,
    ch: &str,
    at: &'static str,
) -> (Pause, JoinHandle<Result<Reservation, CustodyError>>) {
    let pause = commit_fault::hold_at(ch, at);
    let permit = slot.quiesce().expect("closed and drained");
    let (admission, custody) = (fx.admit(ch).await, fx.custody.clone());
    let request = scoped(ch, seal("q9", None, None), EPOCH);
    let task = async move { custody.reserve_successor(admission, request, permit).await };
    (pause, tokio::spawn(task))
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a2b0_r1b_a_succession_holds_the_slot_until_its_commit_is_decided_pg() {
    let fx = Fx::create().await;
    let (registry, custody, _index) = process(&fx);
    let ch = channel(897);
    let (slot, _attempt) = started(&fx, &registry, &ch).await;
    let (revision, op) = reserve(&fx, &custody, scoped(&ch, seal("q1", None, None), EPOCH)).await;
    custody
        .settle_busy(&ch, revision, &op)
        .await
        .expect("closed");
    slot.close_admission();
    let (owner, raw) = (fx.record(&ch).await.owner, fx.raw(&ch).await);
    // An attempt started after the first check voids the permit: the write rolls back.
    let ((checked, release), task) = succession(&fx, &slot, &ch, "reserve-checked").await;
    checked.await.expect("passed its first check");
    let next = slot.retry(&fx.custody, scope(id(&ch), EPOCH), 2).await;
    let next = next.expect("nothing holds the slot yet");
    release.send(()).expect("released");
    let voided = task.await.expect("ended");
    assert_eq!(fx.raw(&ch).await, raw, "a voided permit's write committed");
    assert!(
        matches!(voided, Err(CustodyError::Refused(Refusal::NotQuiescent))),
        "{voided:?}"
    );
    // A permit taken at the last check holds the slot: no attempt starts and no reclaim retires
    // it until the commit is decided.
    let earlier = next.identity().attempt;
    slot.close_admission();
    let ((held, release), task) = succession(&fx, &slot, &ch, "reserve-held").await;
    held.await.expect("took the slot");
    let during = slot.retry(&fx.custody, scope(id(&ch), EPOCH), 3).await;
    assert_eq!(
        slot.drained(),
        Ok(Some(earlier)),
        "an attempt started during the succession"
    );
    assert_eq!(during.map(drop), Err(RegistryRefusal::Succeeding));
    let reclaimed = registry.reclaim(&fx.custody, id(&ch)).await;
    assert_eq!(reclaimed, Err(RegistryRefusal::Succeeding));
    assert_eq!(
        next.worker().map(drop),
        Err(RegistryRefusal::AdmissionClosed)
    );
    release.send(()).expect("released");
    let taken = task.await.expect("ended");
    assert!(
        matches!(taken, Ok(Reservation::Reserved { .. })),
        "{taken:?}"
    );
    assert_ne!(fx.record(&ch).await.owner, owner);
    // The decided commit lets the slot go.
    let reclaimed = registry.reclaim(&fx.custody, id(&ch)).await;
    assert_eq!(reclaimed, Err(RegistryRefusal::NotClosed));
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a2b0_r1b_the_same_owner_reserves_a_closed_row_again_only_once_drained_pg() {
    let fx = Fx::create().await;
    let (registry, custody, index) = process(&fx);
    let ch = channel(898);
    let (_slot, attempt) = started(&fx, &registry, &ch).await;
    let worker = attempt.worker().expect("a worker place");
    let (revision, op) = reserve(&fx, &custody, scoped(&ch, seal("q1", None, None), EPOCH)).await;
    let exited = seal("q0", None, None);
    let step = arm_step(&ch, revision, &op, Stage::QueueCommit, exited, None);
    let Armed {
        capability,
        report,
        collector,
        ..
    } = worker.arm(&custody, step).await.expect("armed and kept");
    let entered = index.admit_write(&ch, Some(&capability));
    let entered = entered.expect("entered while armed");
    report.send(StepOutcome::NotApplied);
    collector.await.unwrap().expect("recorded");
    let closed = fx.record(&ch).await;
    let raw = fx.raw(&ch).await;
    // The same store reserves again while A's guard, capability and worker live, then without
    // the guard and the worker: no new operation and no write.
    let again = || legacy(&ch, seal("q9", None, None));
    let early = custody.reserve(fx.admit(&ch).await, again()).await;
    drop((worker, entered));
    let late = custody.reserve(fx.admit(&ch).await, again()).await;
    let row = fx.record(&ch).await;
    assert_eq!(
        (row.revision, row.active_operation),
        (closed.revision, None),
        "a new operation was reserved while A still applied"
    );
    assert_eq!(
        fx.raw(&ch).await,
        raw,
        "a refused reservation wrote the row"
    );
    for refused in [early, late] {
        assert!(
            matches!(refused, Err(CustodyError::Refused(Refusal::NotQuiescent))),
            "{refused:?}"
        );
    }
    // Once the capability is given up the arm has ended, and the row is reserved again.
    drop(capability);
    let (revision, next) = reserve(&fx, &custody, again()).await;
    assert_eq!(revision, closed.revision + 1);
    assert_ne!(next, op);
    fx.close().await;
}
