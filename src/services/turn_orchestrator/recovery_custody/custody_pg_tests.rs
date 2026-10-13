use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::PgPool;

use super::capability::CapabilityRefusal;
use super::denial_index::{Denial, IndexState};
use super::model::{CanonicalSeal, IssuedEpisode, Namespace, Observed};
use super::reconcile::{
    Action, Evidence, LegacyResult, LiveOwner, NoApplyReason, PriorExecution, Verdict,
};
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::agent_recovery::{RecoveryCatalog, admission, test_store};
use crate::services::provider::ProviderKind;

pub(super) const NODE: &str = "node-a";
pub(super) const INCARNATION: &str = "incarnation-1";

pub(super) fn channel(n: u32) -> String {
    format!("14867233242593{n:05}")
}

pub(super) fn namespace() -> Namespace {
    Namespace {
        provider: "claude".into(),
        token_hash: "token-a".into(),
    }
}

pub(super) fn stored(identity: &str) -> Observed {
    Observed::Present {
        identity: identity.into(),
        digest: format!("sha256:{identity}"),
    }
}

pub(super) fn seal(queue: &str, marker: Option<&str>, inflight: Option<&str>) -> CanonicalSeal {
    CanonicalSeal {
        owner_node: NODE.into(),
        namespace: namespace(),
        queue: stored(queue),
        marker: marker.map_or(Observed::Absent, stored),
        inflight: inflight.map_or(Observed::Absent, stored),
        placeholder: Observed::Absent,
    }
}

pub(super) fn episode(nonce: Option<&str>) -> IssuedEpisode {
    IssuedEpisode::V1 {
        provider: "claude".into(),
        channel: 1,
        request_owner: 7,
        user_msg_id: 11,
        nonce: nonce.map(str::to_string),
        born_generation: 3,
        started_at: "2026-10-14T00:00:00Z".into(),
    }
}

pub(super) fn legacy(channel_id: &str, before: CanonicalSeal) -> ReserveRequest {
    ReserveRequest {
        channel_id: channel_id.to_string(),
        root: Root::Legacy,
        namespace: namespace(),
        sources: vec![11],
        allowed: vec![
            Stage::MarkerPublish,
            Stage::QueueCommit,
            Stage::ActorInstall,
            Stage::SourceReturn,
        ],
        before,
        scoped_fence: None,
        issued_episode: None,
    }
}

pub(super) fn scoped(channel_id: &str, before: CanonicalSeal, epoch: i64) -> ReserveRequest {
    ReserveRequest {
        root: Root::Scoped,
        allowed: vec![
            Stage::QueueCommit,
            Stage::MarkerPublish,
            Stage::ActorInstall,
            Stage::SourceReturn,
            Stage::ExactRelease,
        ],
        scoped_fence: Some((epoch, 1)),
        issued_episode: Some(episode(None)),
        ..legacy(channel_id, before)
    }
}

pub(super) fn evidence(observed: CanonicalSeal, prior: PriorExecution) -> Evidence {
    Evidence {
        observed: Some(observed),
        prior,
        live_actor: None,
        debt: None,
        release: None,
        followup_granted: false,
    }
}

/// This process still serves `operation_id` and its armed applier has finished.
pub(super) fn live(operation_id: &str) -> PriorExecution {
    PriorExecution::SameOwnerLive(LiveOwner {
        process_incarnation: INCARNATION.into(),
        operation_id: operation_id.into(),
        applier_ended: true,
    })
}

pub(super) struct Fx {
    db: TestPostgresDb,
    pub(super) pool: PgPool,
    pub(super) index: Arc<DenialIndex>,
    pub(super) custody: CustodyStore,
}

impl Fx {
    pub(super) async fn create() -> Self {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate_with_max_connections(6).await;
        let index = Arc::new(DenialIndex::default());
        let owner = Owner {
            node: NODE.into(),
            process_incarnation: INCARNATION.into(),
            attempt_id: "attempt-1".into(),
        };
        let custody = CustodyStore::new(pool.clone(), owner, index.clone());
        Self {
            db,
            pool,
            index,
            custody,
        }
    }

    pub(super) async fn close(self) {
        self.pool.close().await;
        self.db.drop().await;
    }

    pub(super) async fn admit(&self, channel_id: &str) -> AdmissionGuard {
        test_store::with_store(
            self.pool.clone(),
            RecoveryCatalog::default(),
            admission::admit(channel_id, &ProviderKind::Claude),
        )
        .await
        .expect("admitted")
    }

    /// A worker home row held by `holder` at `epoch`.
    pub(super) async fn home(&self, channel_id: &str, holder: &str, epoch: i64) {
        sqlx::query(
            "INSERT INTO o_channel_homes (channel_id, provider, state, holder, epoch, renewed_at)
             VALUES ($1, 'claude', 'worker', $2, $3, NOW())",
        )
        .bind(channel_id)
        .bind(holder)
        .bind(epoch)
        .execute(&self.pool)
        .await
        .unwrap();
    }

    pub(super) async fn reserve(&self, request: ReserveRequest) -> (i64, String) {
        let guard = self.admit(&request.channel_id).await;
        match self
            .custody
            .reserve(guard, request)
            .await
            .expect("reserved")
        {
            Reservation::Reserved {
                revision,
                operation_id,
                ..
            } => (revision, operation_id),
            other => panic!("not reserved: {other:?}"),
        }
    }

    pub(super) async fn record(&self, channel_id: &str) -> CustodyRecord {
        match self.custody.inspect(channel_id).await.expect("inspect") {
            Inspection::Row(record) => *record,
            other => panic!("no decodable row: {other:?}"),
        }
    }

    pub(super) async fn raw(&self, channel_id: &str) -> Option<String> {
        sqlx::query_scalar(
            "SELECT row_to_json(c)::text FROM channel_recovery_custody c WHERE channel_id = $1",
        )
        .bind(channel_id)
        .fetch_optional(&self.pool)
        .await
        .unwrap()
    }

    pub(super) async fn arm(
        &self,
        channel_id: &str,
        revision: i64,
        operation_id: &str,
        stage: Stage,
        after: CanonicalSeal,
        grant: Option<&str>,
    ) -> Armed {
        self.custody
            .arm(
                channel_id,
                revision,
                operation_id,
                stage,
                after,
                grant.map(str::to_string),
            )
            .await
            .expect("armed")
    }

    /// Arms `stage` and reports it applied; returns the recorded revision.
    pub(super) async fn apply(
        &self,
        channel_id: &str,
        revision: i64,
        operation_id: &str,
        stage: Stage,
        after: CanonicalSeal,
    ) -> i64 {
        let armed = self
            .arm(
                channel_id,
                revision,
                operation_id,
                stage,
                after.clone(),
                None,
            )
            .await;
        armed.report.send(StepOutcome::Applied {
            after: Box::new(after),
            handoff: None,
        });
        armed.collector.await.unwrap().expect("recorded")
    }

    /// Arms `stage` and then loses its process: no report and no collector, the row stays armed.
    pub(super) async fn arm_and_crash(
        &self,
        channel_id: &str,
        revision: i64,
        operation_id: &str,
        stage: Stage,
        after: CanonicalSeal,
        grant: Option<&str>,
    ) -> i64 {
        let armed = self
            .arm(channel_id, revision, operation_id, stage, after, grant)
            .await;
        armed.collector.abort();
        assert!(armed.collector.await.unwrap_err().is_cancelled());
        drop(armed.report);
        let record = self.record(channel_id).await;
        let op = record.active_operation.unwrap();
        assert_eq!((op.stage, op.status), (stage, StageStatus::ApplyArmed));
        record.revision
    }
}

async fn custody_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM channel_recovery_custody")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_a00_reserve_writes_in_the_admission_transaction_pg() {
    let fx = Fx::create().await;
    let ch = channel(1);
    let mut guard = fx.admit(&ch).await;
    let (pid, xid): (i32, i64) =
        sqlx::query_as("SELECT pg_backend_pid(), txid_current() % 4294967296")
            .fetch_one(&mut **guard.transaction().expect("admission transaction"))
            .await
            .unwrap();
    let custody = fx.custody.clone();
    let request = legacy(&ch, seal("q1", None, None));
    let task = tokio::spawn(async move { custody.reserve(guard, request).await });
    // A second backend asking for the admission lock would wait behind this admission for good.
    let mut observer = fx.pool.acquire().await.unwrap();
    while !task.is_finished() {
        let foreign: Vec<(i32, bool)> = sqlx::query_as(
            "SELECT pid, granted FROM pg_locks
              WHERE locktype = 'advisory' AND classid = 1894 AND pid <> $1",
        )
        .bind(pid)
        .fetch_all(&mut *observer)
        .await
        .unwrap();
        assert!(
            foreign.is_empty(),
            "another backend requested the admission lock: {foreign:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let reserved = task.await.unwrap().expect("reserved");
    assert!(matches!(
        reserved,
        Reservation::Reserved { revision: 1, .. }
    ));
    let (row_xid, locks): (i64, i64) = sqlx::query_as(
        "SELECT xmin::text::bigint,
                (SELECT COUNT(*) FROM pg_locks WHERE locktype = 'advisory' AND classid = 1894)
           FROM channel_recovery_custody WHERE channel_id = $1",
    )
    .bind(&ch)
    .fetch_one(&mut *observer)
    .await
    .unwrap();
    assert_eq!(row_xid, xid, "written by the admission's own transaction");
    assert_eq!(locks, 0, "the commit ended the admission lock");
    drop(observer);
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_a00_transition_busy_ends_only_an_unapplied_reservation_pg() {
    let fx = Fx::create().await;
    let ch = channel(2);
    let before = seal("q1", None, None);
    let (revision, op) = fx.reserve(legacy(&ch, before.clone())).await;
    let revision = fx.custody.settle_busy(&ch, revision, &op).await.unwrap();
    let row = fx.record(&ch).await;
    assert_eq!(
        (row.phase, row.revision, row.active_operation),
        (Phase::Closed, revision, None)
    );
    // Nothing leaked: a new admission reserves the channel again.
    let (revision, op) = fx.reserve(legacy(&ch, before)).await;
    let marker = seal("q1", Some("m1"), None);
    let revision = fx
        .apply(&ch, revision, &op, Stage::MarkerPublish, marker)
        .await;
    fx.custody.settle_busy(&ch, revision, &op).await.unwrap();
    let row = fx.record(&ch).await;
    let kept = row
        .active_operation
        .expect("the applied root keeps its operation");
    assert_eq!(row.phase, Phase::LegacyReserved);
    assert_eq!(
        (kept.operation_id, kept.applied),
        (op, vec![Stage::MarkerPublish])
    );
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_l01_legacy_needs_no_home_and_read_errors_never_mean_unmanaged_pg() {
    let fx = Fx::create().await;
    let (no_home, homed, scoped_ch, broken) = (channel(11), channel(12), channel(13), channel(14));
    let reserved = fx
        .custody
        .reserve(
            fx.admit(&no_home).await,
            legacy(&no_home, seal("q1", None, None)),
        )
        .await;
    assert!(
        matches!(reserved, Ok(Reservation::Reserved { .. })),
        "a no-home legacy consumer keeps its admission: {reserved:?}"
    );
    let stored_epoch: Option<i64> =
        sqlx::query_scalar("SELECT home_epoch FROM channel_recovery_custody WHERE channel_id = $1")
            .bind(&no_home)
            .fetch_one(&fx.pool)
            .await
            .unwrap();
    assert_eq!(
        stored_epoch, None,
        "an explicit absence, not a placeholder epoch"
    );
    assert_eq!(fx.record(&no_home).await.home, HomeIdentity::Absent);
    fx.home(&homed, "node-b", 41).await;
    fx.reserve(legacy(&homed, seal("q1", None, None))).await;
    assert_eq!(
        fx.record(&homed).await.home,
        HomeIdentity::Present { epoch: 41 }
    );
    let refused_scoped = fx
        .custody
        .reserve(
            fx.admit(&scoped_ch).await,
            scoped(&scoped_ch, seal("q1", None, None), 41),
        )
        .await;
    assert!(matches!(
        refused_scoped,
        Err(CustodyError::Refused(Refusal::HomeMismatch))
    ));
    assert_eq!(fx.raw(&scoped_ch).await, None);
    // No recovery store at all: legacy runs unmanaged, scoped is refused.
    let without_store = channel(15);
    let guard = admission::admit(&without_store, &ProviderKind::Claude)
        .await
        .expect("admitted without a store");
    assert_eq!(
        fx.custody
            .reserve(guard, legacy(&without_store, seal("q1", None, None)))
            .await
            .unwrap(),
        Reservation::Unmanaged
    );
    let guard = admission::admit(&without_store, &ProviderKind::Claude)
        .await
        .unwrap();
    assert!(matches!(
        fx.custody
            .reserve(guard, scoped(&without_store, seal("q1", None, None), 41))
            .await,
        Err(CustodyError::Refused(Refusal::StoreUnavailable))
    ));
    // A failed home read is an error, never "no home".
    sqlx::query("ALTER TABLE o_channel_homes RENAME TO o_channel_homes_away")
        .execute(&fx.pool)
        .await
        .unwrap();
    let failed = fx
        .custody
        .reserve(
            fx.admit(&broken).await,
            legacy(&broken, seal("q1", None, None)),
        )
        .await;
    assert!(matches!(failed, Err(CustodyError::Store(_))), "{failed:?}");
    sqlx::query("ALTER TABLE o_channel_homes_away RENAME TO o_channel_homes")
        .execute(&fx.pool)
        .await
        .unwrap();
    assert_eq!(fx.raw(&broken).await, None);
    // So is a home row this build cannot decode, which fails without aborting the transaction.
    let undecodable = channel(16);
    sqlx::query("ALTER TABLE o_channel_homes DROP CONSTRAINT o_channel_homes_state_check")
        .execute(&fx.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO o_channel_homes (channel_id, provider, state, target, epoch)
         VALUES ($1, 'claude', 'moved', 'node-b', 43)",
    )
    .bind(&undecodable)
    .execute(&fx.pool)
    .await
    .unwrap();
    let failed = fx
        .custody
        .reserve(
            fx.admit(&undecodable).await,
            legacy(&undecodable, seal("q1", None, None)),
        )
        .await;
    assert!(matches!(failed, Err(CustodyError::Store(_))), "{failed:?}");
    assert_eq!(fx.raw(&undecodable).await, None);
    // A failed custody read is an error too, never an absent row.
    sqlx::query("ALTER TABLE channel_recovery_custody RENAME TO custody_away")
        .execute(&fx.pool)
        .await
        .unwrap();
    let inspected = fx.custody.inspect(&no_home).await;
    assert!(
        matches!(inspected, Err(CustodyError::Store(_))),
        "{inspected:?}"
    );
    sqlx::query("ALTER TABLE custody_away RENAME TO channel_recovery_custody")
        .execute(&fx.pool)
        .await
        .unwrap();
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_cas_concurrent_reserves_and_writers_have_one_winner_pg() {
    let fx = Fx::create().await;
    let ch = channel(21);
    let one = async {
        fx.custody
            .reserve(fx.admit(&ch).await, legacy(&ch, seal("q1", None, None)))
            .await
    };
    let two = async {
        fx.custody
            .reserve(fx.admit(&ch).await, legacy(&ch, seal("q1", None, None)))
            .await
    };
    let (one, two) = tokio::join!(one, two);
    let won = [&one, &two]
        .iter()
        .filter(|result| matches!(result, Ok(Reservation::Reserved { .. })))
        .count();
    assert_eq!(won, 1, "{one:?} {two:?}");
    assert!(
        [&one, &two].iter().any(|result| matches!(
            result,
            Err(CustodyError::Refused(Refusal::Busy(Phase::LegacyReserved)))
        )),
        "{one:?} {two:?}"
    );
    let row = fx.record(&ch).await;
    let stored = CustodyRecord {
        phase: Phase::Held,
        ..row.clone()
    }
    .encode()
    .unwrap();
    let (mut first, mut second) = (
        fx.pool.acquire().await.unwrap(),
        fx.pool.acquire().await.unwrap(),
    );
    let (a, b) = tokio::join!(
        crate::db::channel_recovery_custody::replace(&mut first, row.revision, &stored),
        crate::db::channel_recovery_custody::replace(&mut second, row.revision, &stored),
    );
    let mut writes = [a.unwrap(), b.unwrap()];
    writes.sort_by_key(|write| matches!(write, CasWrite::Stale));
    assert_eq!(
        writes,
        [
            CasWrite::Applied {
                revision: row.revision + 1
            },
            CasWrite::Stale
        ]
    );
    drop((first, second));
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_cas_stale_writes_and_undecodable_rows_change_nothing_pg() {
    let fx = Fx::create().await;
    let (ch, future) = (channel(31), channel(32));
    let before = seal("q1", None, None);
    let (revision, op) = fx.reserve(legacy(&ch, before.clone())).await;
    let raw = fx.raw(&ch).await;
    let stale = fx
        .custody
        .reconcile(
            &ch,
            revision + 1,
            Action::RecheckApply,
            &evidence(before.clone(), PriorExecution::Ended),
        )
        .await;
    assert!(matches!(stale, Err(CustodyError::Refused(Refusal::Stale))));
    let stale_arm = fx
        .custody
        .arm(
            &ch,
            revision + 1,
            &op,
            Stage::MarkerPublish,
            before.clone(),
            None,
        )
        .await;
    assert!(matches!(
        stale_arm,
        Err(CustodyError::Refused(Refusal::Stale))
    ));
    assert_eq!(fx.raw(&ch).await, raw);
    // A row from a newer format is reported as held and never rewritten.
    sqlx::query(
        "INSERT INTO channel_recovery_custody
           (channel_id, revision, provider, token_hash, owner_node, process_incarnation,
            attempt_id, phase, canonical_seal, format_version, active_operation)
         VALUES ($1, 1, 'claude', 'token-a', 'node-z', 'incarnation-9', 'attempt-9',
                 'future_phase', '{\"next\": true}', 2, '{\"operation_id\": \"x\"}')",
    )
    .bind(&future)
    .execute(&fx.pool)
    .await
    .unwrap();
    let raw = fx.raw(&future).await;
    let inspected = fx.custody.inspect(&future).await.unwrap();
    assert!(
        matches!(inspected, Inspection::Unsupported { revision: 1, .. }),
        "{inspected:?}"
    );
    assert_eq!(inspected.logical_phase(), Some(Phase::Held));
    let reserved = fx
        .custody
        .reserve(fx.admit(&future).await, legacy(&future, before.clone()))
        .await;
    assert!(matches!(
        reserved,
        Err(CustodyError::Refused(Refusal::Held))
    ));
    let reconciled = fx
        .custody
        .reconcile(
            &future,
            1,
            Action::RecheckApply,
            &evidence(before.clone(), PriorExecution::Ended),
        )
        .await;
    assert!(matches!(
        reconciled,
        Err(CustodyError::Refused(Refusal::Held))
    ));
    let armed = fx
        .custody
        .arm(&future, 1, "x", Stage::MarkerPublish, before, None)
        .await;
    assert!(matches!(armed, Err(CustodyError::Refused(Refusal::Held))));
    assert_eq!(fx.raw(&future).await, raw);
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_ever_scoped_survives_close_and_legacy_reuse_pg() {
    let fx = Fx::create().await;
    let ch = channel(41);
    fx.home(&ch, NODE, 41).await;
    let before = seal("q1", None, None);
    let (revision, op) = fx.reserve(scoped(&ch, before.clone(), 41)).await;
    let row = fx.record(&ch).await;
    assert_eq!((row.phase, row.ever_scoped), (Phase::ScopedReserved, true));
    fx.custody.settle_busy(&ch, revision, &op).await.unwrap();
    let row = fx.record(&ch).await;
    assert_eq!((row.phase, row.ever_scoped), (Phase::Closed, true));
    fx.reserve(legacy(&ch, before)).await;
    let row = fx.record(&ch).await;
    assert_eq!((row.phase, row.ever_scoped), (Phase::LegacyReserved, true));
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_f04_exact_operation_is_published_only_after_the_arm_commits_pg() {
    let fx = Fx::create().await;
    let (ch, unattached) = (channel(51), channel(52));
    assert_eq!(fx.index.state(&unattached), IndexState::Unknown);
    assert_eq!(
        fx.index.admit_write(&unattached, None),
        Err(Denial(IndexState::Unknown))
    );
    let before = seal("q1", None, None);
    let (revision, op) = fx.reserve(legacy(&ch, before)).await;
    assert_eq!(
        fx.index.admit_write(&ch, None),
        Err(Denial(IndexState::Blocked))
    );
    let seen = Arc::new(Mutex::new(None));
    commit_fault::before_next(&ch, {
        let (index, seen, ch) = (fx.index.clone(), seen.clone(), ch.clone());
        move || {
            *seen.lock().unwrap() = Some(index.state(&ch));
            Err("commit lost".into())
        }
    });
    let after = seal("q1", Some("m1"), None);
    let lost = fx
        .custody
        .arm(
            &ch,
            revision,
            &op,
            Stage::MarkerPublish,
            after.clone(),
            None,
        )
        .await;
    assert!(matches!(lost, Err(CustodyError::Store(_))));
    assert_eq!(*seen.lock().unwrap(), Some(IndexState::Blocked));
    assert_eq!(fx.index.state(&ch), IndexState::Blocked);
    let row = fx.record(&ch).await;
    assert_eq!(
        (row.revision, row.active_operation.unwrap().status),
        (revision, StageStatus::ReservedNotArmed)
    );
    let armed = fx
        .arm(&ch, revision, &op, Stage::MarkerPublish, after, None)
        .await;
    assert_eq!(
        fx.index.state(&ch),
        IndexState::ExactOperation {
            operation_id: op.clone(),
            stage: Stage::MarkerPublish
        }
    );
    assert_eq!(fx.index.admit_write(&ch, Some(&armed.capability)), Ok(()));
    let (other_stage, _) = ApplyCapability::issue(&ch, &op, Stage::QueueCommit);
    assert!(fx.index.admit_write(&ch, Some(&other_stage)).is_err());
    assert!(fx.index.admit_write(&ch, None).is_err());
    assert_eq!(
        armed.capability.check(&ch, &op, Stage::MarkerPublish),
        Ok(())
    );
    assert_eq!(
        armed.capability.check(&ch, &op, Stage::QueueCommit),
        Err(CapabilityRefusal::WrongTarget)
    );
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_f04_an_indeterminate_stage_blocks_and_keeps_its_operation_pg() {
    let fx = Fx::create().await;
    let ch = channel(61);
    let before = seal("q1", None, None);
    let (revision, op) = fx.reserve(legacy(&ch, before)).await;
    let armed = fx
        .arm(
            &ch,
            revision,
            &op,
            Stage::MarkerPublish,
            seal("q1", Some("m1"), None),
            None,
        )
        .await;
    drop(armed.report);
    let revision = armed.collector.await.unwrap().expect("recorded");
    let row = fx.record(&ch).await;
    let kept = row.active_operation.clone().unwrap();
    assert_eq!(
        (row.phase, kept.operation_id.as_str(), kept.status),
        (Phase::Held, op.as_str(), StageStatus::Indeterminate)
    );
    assert_eq!(fx.index.state(&ch), IndexState::Blocked);
    assert_eq!(
        armed.capability.check(&ch, &op, Stage::MarkerPublish),
        Err(CapabilityRefusal::Revoked)
    );
    // Missing metadata and missing files do not release it.
    let gone = CanonicalSeal {
        queue: Observed::Absent,
        marker: Observed::Absent,
        ..seal("unused", None, None)
    };
    let (verdict, revision) = fx
        .custody
        .reconcile(
            &ch,
            revision,
            Action::RecheckApply,
            &evidence(gone, PriorExecution::Ended),
        )
        .await
        .unwrap();
    assert!(matches!(verdict, Verdict::Held { .. }), "{verdict:?}");
    let unread = Evidence {
        observed: None,
        ..evidence(seal("q1", None, None), PriorExecution::Ended)
    };
    let (verdict, _) = fx
        .custody
        .reconcile(&ch, revision, Action::RecheckApply, &unread)
        .await
        .unwrap();
    assert!(matches!(verdict, Verdict::Held { .. }), "{verdict:?}");
    let row = fx.record(&ch).await;
    assert_eq!(row.phase, Phase::Held);
    assert_eq!(row.active_operation.unwrap().operation_id, op);
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_f04_off_closes_new_admission_and_keeps_armed_results_pg() {
    let fx = Fx::create().await;
    let (ch, other) = (channel(71), channel(72));
    let (revision, op) = fx.reserve(legacy(&ch, seal("q1", None, None))).await;
    let after = seal("q1", Some("m1"), None);
    let armed = fx
        .arm(
            &ch,
            revision,
            &op,
            Stage::MarkerPublish,
            after.clone(),
            None,
        )
        .await;
    fx.custody.close_admission();
    let refused = fx
        .custody
        .reserve(
            fx.admit(&other).await,
            legacy(&other, seal("q2", None, None)),
        )
        .await;
    assert!(matches!(
        refused,
        Err(CustodyError::Refused(Refusal::AdmissionClosed))
    ));
    assert_eq!(fx.raw(&other).await, None);
    armed.report.send(StepOutcome::Applied {
        after: Box::new(after),
        handoff: None,
    });
    armed.collector.await.unwrap().expect("recorded after off");
    let kept = fx.record(&ch).await.active_operation.unwrap();
    assert_eq!(
        (kept.status, kept.applied),
        (StageStatus::KnownOutcome, vec![Stage::MarkerPublish])
    );
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_owned_outcome_is_recorded_after_the_caller_leaves_pg() {
    let fx = Fx::create().await;
    let ch = channel(81);
    let (revision, op) = fx.reserve(legacy(&ch, seal("q1", None, None))).await;
    let after = seal("q1", Some("m1"), None);
    let Armed {
        capability,
        report,
        revoke,
        collector,
    } = fx
        .arm(
            &ch,
            revision,
            &op,
            Stage::MarkerPublish,
            after.clone(),
            None,
        )
        .await;
    drop((revoke, collector));
    let applier = tokio::spawn({
        let ch = ch.clone();
        async move {
            capability.check(&ch, capability.operation_id(), capability.stage())?;
            report.send(StepOutcome::Applied {
                after: Box::new(after),
                handoff: None,
            });
            Ok::<_, CapabilityRefusal>(capability)
        }
    });
    let capability = applier.await.unwrap().expect("applied while armed");
    let mut recorded = None;
    for _ in 0..500 {
        let row = fx.record(&ch).await;
        if row.revision > revision {
            recorded = Some(row);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let row = recorded.expect("the owned collector recorded the outcome");
    assert_eq!(
        row.active_operation.unwrap().status,
        StageStatus::KnownOutcome
    );
    assert_eq!(
        capability.check(&ch, &op, Stage::MarkerPublish),
        Err(CapabilityRefusal::Revoked)
    );
    assert_eq!(custody_rows(&fx.pool).await, 1);
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_f3_arm_issues_each_stage_and_grant_once_and_only_while_open_pg() {
    let fx = Fx::create().await;
    let (exit, marker) = (seal("q0", None, None), seal("q1", Some("m1"), None));
    // An applied queue commit is never armed again.
    let ch = channel(91);
    let (revision, id) = fx.reserve(legacy(&ch, seal("q1", None, None))).await;
    let revision = fx
        .apply(&ch, revision, &id, Stage::QueueCommit, exit.clone())
        .await;
    let again = fx
        .custody
        .arm(&ch, revision, &id, Stage::QueueCommit, exit.clone(), None)
        .await
        .map(|_| ());
    assert!(
        matches!(again, Err(CustodyError::Refused(Refusal::NotArmable))),
        "{again:?}"
    );
    // A resumed exit reuses its debt: another debt is refused, the same one adds nothing.
    let ch = channel(92);
    fx.home(&ch, NODE, 41).await;
    let (revision, id) = fx.reserve(scoped(&ch, seal("q1", None, None), 41)).await;
    let revision = fx
        .arm_and_crash(
            &ch,
            revision,
            &id,
            Stage::QueueCommit,
            exit.clone(),
            Some("debt-1"),
        )
        .await;
    let old = evidence(seal("q1", None, None), PriorExecution::Ended);
    let (verdict, revision) = fx
        .custody
        .reconcile(&ch, revision, Action::RecheckApply, &old)
        .await
        .unwrap();
    assert_eq!(
        verdict,
        Verdict::Resume {
            stage: Stage::QueueCommit,
            reuse: Some("debt-1".into())
        }
    );
    let other = fx
        .custody
        .arm(
            &ch,
            revision,
            &id,
            Stage::QueueCommit,
            exit.clone(),
            Some("debt-2".into()),
        )
        .await
        .map(|_| ());
    assert!(
        matches!(other, Err(CustodyError::Refused(Refusal::Unfit(_)))),
        "{other:?}"
    );
    let _armed = fx
        .arm(&ch, revision, &id, Stage::QueueCommit, exit, Some("debt-1"))
        .await;
    let kept = fx.record(&ch).await.active_operation.unwrap();
    assert_eq!(kept.handoffs, ["debt-1"]);
    // Off refuses a first arm and the next stage's arm alike.
    let (first, next) = (channel(93), channel(94));
    let (first_revision, first_id) = fx.reserve(legacy(&first, seal("q1", None, None))).await;
    let (next_revision, next_id) = fx.reserve(legacy(&next, seal("q1", None, None))).await;
    let next_revision = fx
        .apply(
            &next,
            next_revision,
            &next_id,
            Stage::MarkerPublish,
            marker.clone(),
        )
        .await;
    fx.custody.close_admission();
    let arms = [
        (&first, first_revision, &first_id, Stage::MarkerPublish),
        (&next, next_revision, &next_id, Stage::QueueCommit),
    ];
    for (ch, revision, id, stage) in arms {
        let raw = fx.raw(ch).await;
        let refused = fx
            .custody
            .arm(ch, revision, id, stage, marker.clone(), None)
            .await
            .map(|_| ());
        assert!(
            matches!(
                refused,
                Err(CustodyError::Refused(Refusal::AdmissionClosed))
            ),
            "{stage:?}: {refused:?}"
        );
        assert_eq!(fx.raw(ch).await, raw);
    }
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_f3_an_off_before_the_arm_commits_refuses_the_arm_pg() {
    let fx = Fx::create().await;
    let ch = channel(95);
    let (revision, id) = fx.reserve(legacy(&ch, seal("q1", None, None))).await;
    let raw = fx.raw(&ch).await;
    commit_fault::before_next(&ch, {
        let custody = fx.custody.clone();
        move || {
            custody.close_admission();
            Ok(())
        }
    });
    let marker = seal("q1", Some("m1"), None);
    let refused = fx
        .custody
        .arm(&ch, revision, &id, Stage::MarkerPublish, marker, None)
        .await
        .map(|_| ());
    assert!(
        matches!(
            refused,
            Err(CustodyError::Refused(Refusal::AdmissionClosed))
        ),
        "{refused:?}"
    );
    assert_eq!(fx.raw(&ch).await, raw);
    assert_eq!(fx.index.state(&ch), IndexState::Blocked);
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_f4_legacy_no_apply_never_ends_an_armed_stage_pg() {
    let fx = Fx::create().await;
    let ch = channel(96);
    let (revision, id) = fx.reserve(legacy(&ch, seal("q1", None, None))).await;
    let marker = seal("q1", Some("m1"), None);
    let armed = fx
        .arm(&ch, revision, &id, Stage::MarkerPublish, marker, None)
        .await;
    let raw = fx.raw(&ch).await;
    for reason in [
        NoApplyReason::Empty,
        NoApplyReason::StaleSelection,
        NoApplyReason::TransitionBusy,
    ] {
        let result = fx
            .custody
            .record_legacy_result(&ch, revision + 1, &id, &LegacyResult::NoApply(reason))
            .await;
        assert!(
            matches!(result, Err(CustodyError::Refused(Refusal::Unfit(_)))),
            "{reason:?}: {result:?}"
        );
        assert_eq!(fx.raw(&ch).await, raw, "{reason:?}");
    }
    let busy = fx
        .custody
        .reserve(fx.admit(&ch).await, legacy(&ch, seal("q1", None, None)))
        .await;
    assert!(
        matches!(
            busy,
            Err(CustodyError::Refused(Refusal::Busy(Phase::LegacyReserved)))
        ),
        "{busy:?}"
    );
    assert_eq!(
        armed.capability.check(&ch, &id, Stage::MarkerPublish),
        Ok(())
    );
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_f6_a_writer_needs_the_live_capability_pg() {
    let fx = Fx::create().await;
    let ch = channel(97);
    let before = seal("q1", None, None);
    let marker = seal("q1", Some("m1"), None);
    let (revision, id) = fx.reserve(legacy(&ch, before.clone())).await;
    let first = fx
        .arm(
            &ch,
            revision,
            &id,
            Stage::MarkerPublish,
            marker.clone(),
            None,
        )
        .await;
    assert_eq!(fx.index.admit_write(&ch, Some(&first.capability)), Ok(()));
    // Revoked: refused at once, while the index still names the operation and nothing is recorded.
    first.revoke.revoke();
    let exact = IndexState::ExactOperation {
        operation_id: id.clone(),
        stage: Stage::MarkerPublish,
    };
    assert_eq!(
        fx.index.admit_write(&ch, Some(&first.capability)),
        Err(Denial(exact.clone()))
    );
    assert_eq!(fx.record(&ch).await.revision, revision + 1);
    // Re-armed after its applier was lost: the old capability names the same operation and stage.
    drop(first.report);
    let revision = first.collector.await.unwrap().expect("recorded");
    let (verdict, revision) = fx
        .custody
        .reconcile(
            &ch,
            revision,
            Action::RecheckApply,
            &evidence(before, PriorExecution::Ended),
        )
        .await
        .unwrap();
    assert!(matches!(verdict, Verdict::Resume { .. }), "{verdict:?}");
    let second = fx
        .arm(&ch, revision, &id, Stage::MarkerPublish, marker, None)
        .await;
    assert_eq!(fx.index.state(&ch), exact);
    assert_eq!(
        fx.index.admit_write(&ch, Some(&first.capability)),
        Err(Denial(exact))
    );
    assert_eq!(fx.index.admit_write(&ch, Some(&second.capability)), Ok(()));
    fx.close().await;
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_a_v1_row_with_one_unreadable_field_is_held_untouched_pg() {
    let fx = Fx::create().await;
    let before = seal("q1", None, None);
    let cases = [
        ("phase", "'future_phase'"),
        ("active_operation", "'{\"operation_id\": 7}'::jsonb"),
        ("issued_episode", "'{\"v\": \"9\"}'::jsonb"),
        ("canonical_seal", "'{\"next\": true}'::jsonb"),
    ];
    for (n, (column, value)) in cases.into_iter().enumerate() {
        let ch = channel(801 + n as u32);
        let (revision, id) = fx.reserve(legacy(&ch, before.clone())).await;
        sqlx::query(&format!(
            "UPDATE channel_recovery_custody SET {column} = {value} WHERE channel_id = $1"
        ))
        .bind(&ch)
        .execute(&fx.pool)
        .await
        .unwrap();
        let raw = fx.raw(&ch).await;
        let inspected = fx.custody.inspect(&ch).await.unwrap();
        let Inspection::Unsupported {
            revision: held,
            detail,
        } = &inspected
        else {
            panic!("{column}: {inspected:?}");
        };
        assert_eq!(*held, revision, "{column}");
        assert!(detail.starts_with(column), "{column}: {detail}");
        let reconciled = fx
            .custody
            .reconcile(
                &ch,
                revision,
                Action::RecheckApply,
                &evidence(before.clone(), PriorExecution::Ended),
            )
            .await;
        let armed = fx
            .custody
            .arm(
                &ch,
                revision,
                &id,
                Stage::MarkerPublish,
                before.clone(),
                None,
            )
            .await;
        let reserved = fx
            .custody
            .reserve(fx.admit(&ch).await, legacy(&ch, before.clone()))
            .await;
        for result in [
            reconciled.map(|_| ()),
            armed.map(|_| ()),
            reserved.map(|_| ()),
        ] {
            assert!(
                matches!(result, Err(CustodyError::Refused(Refusal::Held))),
                "{column}: {result:?}"
            );
        }
        let version: i32 = sqlx::query_scalar(
            "SELECT format_version FROM channel_recovery_custody WHERE channel_id = $1",
        )
        .bind(&ch)
        .fetch_one(&fx.pool)
        .await
        .unwrap();
        assert_eq!((version, fx.raw(&ch).await), (1, raw), "{column}");
    }
    fx.close().await;
}

async fn pg_text_len(pool: &PgPool, value: &serde_json::Value) -> usize {
    let len: i32 = sqlx::query_scalar("SELECT octet_length($1::jsonb::text)")
        .bind(value)
        .fetch_one(pool)
        .await
        .unwrap();
    len as usize
}

#[tokio::test(flavor = "current_thread")]
async fn s3act_b3b4a1_bounds_admit_up_to_their_limit_and_refuse_past_it_pg() {
    let fx = Fx::create().await;
    let before = seal("q1", None, None);
    // Sources: 64 at most, refused before any write.
    for (n, count) in [63u64, 64, 65].into_iter().enumerate() {
        let ch = channel(811 + n as u32);
        let request = ReserveRequest {
            sources: (0..count).collect(),
            ..legacy(&ch, before.clone())
        };
        let reserved = fx.custody.reserve(fx.admit(&ch).await, request).await;
        assert_eq!(reserved.is_ok(), count <= 64, "{count}: {reserved:?}");
        assert_eq!(fx.raw(&ch).await.is_some(), count <= 64, "{count}");
    }
    let ch = channel(815);
    fx.home(&ch, NODE, 41).await;
    fx.reserve(scoped(&ch, before.clone(), 41)).await;
    // Handoffs: 8 at most.
    for count in [7usize, 8, 9] {
        let mut row = fx.record(&ch).await;
        let op = row.active_operation.as_mut().unwrap();
        op.handoffs = (0..count).map(|i| format!("debt-{i}")).collect();
        let stored = row.encode();
        assert_eq!(stored.is_ok(), count <= 8, "{count}");
        if let Ok(stored) = stored {
            let mut conn = fx.pool.acquire().await.unwrap();
            let written =
                crate::db::channel_recovery_custody::replace(&mut conn, row.revision, &stored)
                    .await
                    .unwrap();
            assert!(matches!(written, CasWrite::Applied { .. }), "{count}");
            assert_eq!(
                fx.record(&ch)
                    .await
                    .active_operation
                    .unwrap()
                    .handoffs
                    .len(),
                count
            );
        }
    }
    // Each JSON column stores up to its byte limit as PostgreSQL measures it, and not past it.
    type Pad = fn(&mut CustodyRecord, String);
    let columns: [(&str, usize, Pad); 3] = [
        ("issued_episode", 4096, |row, pad| {
            let Some(IssuedEpisode::V1 { started_at, .. }) = row.issued_episode.as_mut() else {
                unreachable!()
            };
            *started_at = pad;
        }),
        ("active_operation", 16384, |row, pad| {
            row.active_operation.as_mut().unwrap().handoffs = vec![pad];
        }),
        ("canonical_seal", 4096, |row, pad| {
            row.canonical_seal.placeholder = Observed::Present {
                identity: pad,
                digest: "d".into(),
            };
        }),
    ];
    for (column, limit, pad) in columns {
        let base = fx.record(&ch).await;
        let mut empty = base.clone();
        pad(&mut empty, String::new());
        let measure = |stored: &crate::db::channel_recovery_custody::StoredCustody| match column {
            "issued_episode" => stored.issued_episode.clone().unwrap(),
            "active_operation" => stored.active_operation.clone().unwrap(),
            _ => stored.canonical_seal.clone(),
        };
        let floor = pg_text_len(&fx.pool, &measure(&empty.encode().unwrap())).await;
        for size in [limit - 1, limit, limit + 1] {
            let row = fx.record(&ch).await;
            let mut padded = row.clone();
            pad(&mut padded, "p".repeat(size - floor));
            let stored = padded.encode().unwrap();
            assert_eq!(
                pg_text_len(&fx.pool, &measure(&stored)).await,
                size,
                "{column}"
            );
            let mut conn = fx.pool.acquire().await.unwrap();
            let written =
                crate::db::channel_recovery_custody::replace(&mut conn, row.revision, &stored)
                    .await;
            assert_eq!(
                matches!(written, Ok(CasWrite::Applied { .. })),
                size <= limit,
                "{column} {size}: {written:?}"
            );
        }
        // Back to the unpadded row for the next column.
        let row = fx.record(&ch).await;
        let mut conn = fx.pool.acquire().await.unwrap();
        let restored = CustodyRecord {
            revision: row.revision,
            ..base
        };
        crate::db::channel_recovery_custody::replace(
            &mut conn,
            row.revision,
            &restored.encode().unwrap(),
        )
        .await
        .unwrap();
    }
    fx.close().await;
}

/// The 0140 migration on an empty and on a 0139 schema, its re-run, and an older migrator.
mod migration {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use sqlx::PgPool;
    use sqlx::migrate::{MigrateError, Migrator};

    const TABLE: &str = "channel_recovery_custody";

    fn migrator_through(version: i64) -> Migrator {
        let all = sqlx::migrate!("./migrations/postgres");
        Migrator {
            migrations: std::borrow::Cow::Owned(
                all.iter()
                    .filter(|m| m.version <= version)
                    .cloned()
                    .collect(),
            ),
            ..Migrator::DEFAULT
        }
    }

    async fn bare_pool(db: &TestPostgresDb) -> PgPool {
        crate::db::postgres::connect_test_pool_with_max_connections(
            &db.database_url,
            "recovery custody migration",
            2,
        )
        .await
        .expect("connect without migrating")
    }

    async fn table_exists(pool: &PgPool) -> bool {
        sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(TABLE)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// Every catalog fact about objects other than the new table, so an additive migration leaves it
    /// equal: relations, columns, constraints, indexes, triggers, views, sequences and functions.
    async fn other_objects(pool: &PgPool) -> String {
        sqlx::query_scalar(
            "SELECT string_agg(fact, E'\\n' ORDER BY fact) FROM (
           SELECT format('rel %s %s', c.relname, c.relkind) AS fact
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = 'public' AND c.relname NOT LIKE 'channel_recovery_custody%'
           UNION ALL
           SELECT format('col %s.%s %s %s %s', c.relname, a.attname,
                         format_type(a.atttypid, a.atttypmod), a.attnotnull,
                         pg_get_expr(d.adbin, d.adrelid))
             FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace
             LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
            WHERE n.nspname = 'public' AND a.attnum > 0 AND NOT a.attisdropped
              AND c.relname NOT LIKE 'channel_recovery_custody%'
           UNION ALL
           SELECT format('con %s %s %s', c.relname, k.conname, pg_get_constraintdef(k.oid))
             FROM pg_constraint k JOIN pg_class c ON c.oid = k.conrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = 'public' AND c.relname <> 'channel_recovery_custody'
           UNION ALL
           SELECT format('idx %s', pg_get_indexdef(i.indexrelid))
             FROM pg_index i JOIN pg_class c ON c.oid = i.indrelid
             JOIN pg_namespace n ON n.oid = c.relnamespace
            WHERE n.nspname = 'public' AND c.relname <> 'channel_recovery_custody'
           UNION ALL
           SELECT format('trg %s %s', t.tgrelid::regclass, pg_get_triggerdef(t.oid))
             FROM pg_trigger t WHERE NOT t.tgisinternal
           UNION ALL
           SELECT format('view %s %s', viewname, definition) FROM pg_views
            WHERE schemaname = 'public'
           UNION ALL
           SELECT format('fn %s %s', p.proname, md5(pg_get_functiondef(p.oid)))
             FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
            WHERE n.nspname = 'public' AND p.prokind = 'f'
         ) facts",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Exact row count of every public table.
    async fn row_counts(pool: &PgPool) -> Vec<(String, i64)> {
        sqlx::query_as(
            "SELECT table_name::text,
                (xpath('/row/c/text()', query_to_xml(
                    format('SELECT COUNT(*) AS c FROM %I', table_name), false, true, '')))[1]
                    ::text::bigint
           FROM information_schema.tables
          WHERE table_schema = 'public' AND table_type = 'BASE TABLE'
          ORDER BY table_name",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    async fn applied(pool: &PgPool) -> Vec<(i64, Vec<u8>, bool)> {
        sqlx::query_as("SELECT version, checksum, success FROM _sqlx_migrations ORDER BY version")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "current_thread")]
    async fn s3act_b3b4a1_migration_applies_to_an_empty_database_and_reruns_as_a_no_op_pg() {
        let db = TestPostgresDb::create().await;
        let pool = bare_pool(&db).await;
        crate::db::postgres::migrate(&pool)
            .await
            .expect("migrate empty");
        assert!(table_exists(&pool).await, "0140 must create the table");
        let once = applied(&pool).await;
        assert_eq!(once.last().map(|(v, _, ok)| (*v, *ok)), Some((140, true)));
        let objects = other_objects(&pool).await;
        crate::db::postgres::migrate(&pool)
            .await
            .expect("re-run migrator");
        sqlx::raw_sql(include_str!(
            "../../../../migrations/postgres/0140_channel_recovery_custody.sql"
        ))
        .execute(&pool)
        .await
        .expect("0140 re-executes in place");
        assert_eq!(applied(&pool).await, once, "a re-run records nothing new");
        assert_eq!(other_objects(&pool).await, objects);
        let shape: Vec<(String, String, bool)> = sqlx::query_as(
            "SELECT column_name::text, data_type::text, is_nullable = 'YES'
           FROM information_schema.columns WHERE table_name = $1 ORDER BY ordinal_position",
        )
        .bind(TABLE)
        .fetch_all(&pool)
        .await
        .unwrap();
        let nullable: Vec<&str> = shape
            .iter()
            .filter(|(_, _, nullable)| *nullable)
            .map(|(name, _, _)| name.as_str())
            .collect();
        assert_eq!(
            nullable,
            [
                "home_epoch",
                "activation_generation",
                "issued_episode",
                "active_operation"
            ]
        );
        let references: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_constraint
          WHERE contype = 'f' AND (conrelid = $1::regclass OR confrelid = $1::regclass)",
        )
        .bind(TABLE)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(references, 0, "no home row reference or cascade");
        pool.close().await;
        db.drop().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn s3act_b3b4a1_migration_on_a_0139_schema_only_adds_its_table_pg() {
        let db = TestPostgresDb::create().await;
        let pool = bare_pool(&db).await;
        migrator_through(139)
            .run(&pool)
            .await
            .expect("migrate through 0139");
        assert!(!table_exists(&pool).await);
        let before = other_objects(&pool).await;
        let migrations_before = applied(&pool).await;
        let rows_before = row_counts(&pool).await;
        crate::db::postgres::migrate(&pool)
            .await
            .expect("apply 0140");
        assert!(table_exists(&pool).await);
        assert_eq!(
            other_objects(&pool).await,
            before,
            "no existing object changes"
        );
        let mut rows_after = row_counts(&pool).await;
        rows_after.retain(|(table, _)| table != TABLE);
        let bookkeeping =
            |rows: &mut Vec<(String, i64)>| rows.retain(|(t, _)| t != "_sqlx_migrations");
        let mut rows_before = rows_before;
        bookkeeping(&mut rows_before);
        bookkeeping(&mut rows_after);
        assert_eq!(rows_after, rows_before, "no backfill or row change");
        let migrations_after = applied(&pool).await;
        assert_eq!(
            migrations_after[..migrations_before.len()],
            migrations_before[..]
        );
        let added: Vec<(i64, bool)> = migrations_after[migrations_before.len()..]
            .iter()
            .map(|(version, _, success)| (*version, *success))
            .collect();
        assert_eq!(added, [(140, true)], "only 0140 is recorded");
        let custody_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM channel_recovery_custody")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(custody_rows, 0);
        pool.close().await;
        db.drop().await;
    }

    /// A binary that embeds only migrations through 0139 refuses this schema; it does not run on it.
    #[tokio::test(flavor = "current_thread")]
    async fn s3act_b3b4a1_a_0139_migrator_refuses_a_schema_with_0140_pg() {
        let db = TestPostgresDb::create().await;
        let pool = bare_pool(&db).await;
        crate::db::postgres::migrate(&pool)
            .await
            .expect("migrate through 0140");
        let before = applied(&pool).await;
        let error = migrator_through(139).run(&pool).await.unwrap_err();
        assert!(
            matches!(error, MigrateError::VersionMissing(140)),
            "{error:?}"
        );
        assert_eq!(applied(&pool).await, before);
        assert!(table_exists(&pool).await);
        pool.close().await;
        db.drop().await;
    }
}

/// L00-admission/home only: recovery admission and the home read return and store what the base
/// build did, and nothing writes a custody row. The store-bytes L00 belongs to a later slice.
mod dormant {
    use sqlx::PgPool;

    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::services::agent_recovery::{RecoveryCatalog, admission, test_store};
    use crate::services::provider::ProviderKind;

    const OK: &str = "1486723324259370001";
    const HOME_HELD: &str = "1486723324259370002";
    const HOME_OFF: &str = "1486723324259370003";
    const FENCED: &str = "1486723324259370004";
    const NO_PG: &str = "1486723324259370005";

    /// Recorded at base 945aac2668ce010b92053fdb39f541ae51629e20 by running `observe` there.
    const ORACLE: &[&str] = &[
        "success-no-home admit=Ok(admission_locks=1) home=Ok(None)",
        "home-held admit=Ok(admission_locks=1) home=Ok(Some(ChannelHome { channel_id: \"1486723324259370002\", provider: \"claude\", state: Worker, holder: Some(\"node-b\"), target: None, epoch: 41, renewed_at: Some(2026-10-14T00:00:00Z), updated_at: 2026-10-14T00:00:01Z, detail: None }))",
        "home-off admit=Ok(admission_locks=1) home=Ok(Some(ChannelHome { channel_id: \"1486723324259370003\", provider: \"claude\", state: Released, holder: None, target: Some(\"node-gw\"), epoch: 42, renewed_at: None, updated_at: 2026-10-14T00:00:02Z, detail: Some(\"off\") }))",
        "fenced admit=Err(channel is fenced by another recovery lease or a pending handoff) home=Ok(None)",
        "no-pg admit=Ok(admission_locks=0) home=Ok(None)",
        "agent_recovery_channel_state=[\"{\\\"channel_id\\\":\\\"1486723324259370004\\\",\\\"status\\\":\\\"fallback_done\\\",\\\"owner_agent_id\\\":\\\"owner\\\",\\\"fallback_agent_id\\\":\\\"fallback\\\",\\\"active_writer_agent_id\\\":\\\"fallback\\\",\\\"workspace\\\":\\\"inherit\\\",\\\"primary_turn_id\\\":\\\"turn-1\\\",\\\"next_seq\\\":3,\\\"updated_at\\\":\\\"2026-10-14T00:00:03+00:00\\\",\\\"generation\\\":5,\\\"recovery_context\\\":null}\"]",
        "agent_recovery_checkpoint_events=[]",
        "o_channel_homes=[\"{\\\"channel_id\\\":\\\"1486723324259370002\\\",\\\"provider\\\":\\\"claude\\\",\\\"state\\\":\\\"worker\\\",\\\"holder\\\":\\\"node-b\\\",\\\"target\\\":null,\\\"epoch\\\":41,\\\"renewed_at\\\":\\\"2026-10-14T00:00:00+00:00\\\",\\\"updated_at\\\":\\\"2026-10-14T00:00:01+00:00\\\",\\\"detail\\\":null}\", \"{\\\"channel_id\\\":\\\"1486723324259370003\\\",\\\"provider\\\":\\\"claude\\\",\\\"state\\\":\\\"released\\\",\\\"holder\\\":null,\\\"target\\\":\\\"node-gw\\\",\\\"epoch\\\":42,\\\"renewed_at\\\":null,\\\"updated_at\\\":\\\"2026-10-14T00:00:02+00:00\\\",\\\"detail\\\":\\\"off\\\"}\"]",
    ];

    async fn seed(pool: &PgPool) {
        sqlx::raw_sql(&format!(
            "INSERT INTO o_channel_homes
           (channel_id, provider, state, holder, target, epoch, renewed_at, updated_at, detail)
         VALUES ('{HOME_HELD}', 'claude', 'worker', 'node-b', NULL, 41,
                 '2026-10-14T00:00:00Z', '2026-10-14T00:00:01Z', NULL),
                ('{HOME_OFF}', 'claude', 'released', NULL, 'node-gw', 42,
                 NULL, '2026-10-14T00:00:02Z', 'off');
         INSERT INTO agent_recovery_channel_state
           (channel_id, status, owner_agent_id, fallback_agent_id, active_writer_agent_id,
            primary_turn_id, next_seq, updated_at, generation)
         VALUES ('{FENCED}', 'fallback_done', 'owner', 'fallback', 'fallback', 'turn-1', 3,
                 '2026-10-14T00:00:03Z', 5);"
        ))
        .execute(pool)
        .await
        .unwrap();
    }

    async fn admit_shape(pool: &PgPool, channel: &str, with_store: bool) -> String {
        let admitted = if with_store {
            test_store::with_store(
                pool.clone(),
                RecoveryCatalog::default(),
                admission::admit(channel, &ProviderKind::Claude),
            )
            .await
        } else {
            admission::admit(channel, &ProviderKind::Claude).await
        };
        match admitted {
            Ok(guard) => {
                let locks: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM pg_locks
                  WHERE locktype = 'advisory' AND classid = 1894 AND granted",
                )
                .fetch_one(pool)
                .await
                .unwrap();
                drop(guard);
                format!("Ok(admission_locks={locks})")
            }
            Err(error) => format!("Err({error})"),
        }
    }

    /// Raw returns and rows, unnormalized: Debug of each result and `row_to_json` of each table.
    async fn observe(pool: &PgPool) -> Vec<String> {
        let mut seen = Vec::new();
        for (label, channel, with_store) in [
            ("success-no-home", OK, true),
            ("home-held", HOME_HELD, true),
            ("home-off", HOME_OFF, true),
            ("fenced", FENCED, true),
            ("no-pg", NO_PG, false),
        ] {
            let admit = admit_shape(pool, channel, with_store).await;
            let home = crate::db::o_channel_homes::read_home(pool, channel)
                .await
                .map_err(|error| error.to_string());
            seen.push(format!("{label} admit={admit} home={home:?}"));
        }
        for table in [
            "agent_recovery_channel_state",
            "agent_recovery_checkpoint_events",
            "o_channel_homes",
        ] {
            // Timestamps render in the session zone, so pin it for the comparison.
            let mut tx = pool.begin().await.unwrap();
            sqlx::query("SET LOCAL TIME ZONE 'UTC'")
                .execute(&mut *tx)
                .await
                .unwrap();
            let rows: Vec<String> = sqlx::query_scalar(&format!(
                "SELECT row_to_json(t)::text FROM {table} t ORDER BY 1"
            ))
            .fetch_all(&mut *tx)
            .await
            .unwrap();
            tx.rollback().await.unwrap();
            seen.push(format!("{table}={rows:?}"));
        }
        seen
    }

    #[tokio::test(flavor = "current_thread")]
    async fn s3act_b3b4a1_l00_admission_and_home_read_match_the_base_oracle_pg() {
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate_with_max_connections(4).await;
        seed(&pool).await;
        let seen = observe(&pool).await;
        assert_eq!(seen, ORACLE);
        let custody: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM channel_recovery_custody")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(custody, 0, "nothing in the dormant paths writes custody");
        pool.close().await;
        db.drop().await;
    }
}
