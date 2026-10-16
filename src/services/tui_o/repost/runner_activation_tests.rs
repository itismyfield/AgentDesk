use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use chrono::Utc;

use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::repost::admission_tests::{BOT, CHANNEL, admit_on, sidecar, switch};
use crate::services::tui_o::repost::identity::{marker, payload_sha256, piece_of};
use crate::services::tui_o::repost::io::probe::evidence::{FIRST_PASS_AFTER, SECOND_PASS_AFTER};
use crate::services::tui_o::repost::io::probe::matcher::ObservedMessage;
use crate::services::tui_o::repost::io::probe::tests::{Fake, absent_evidence};
use crate::services::tui_o::repost::o_piece_delivery::DeliveryRow;
use crate::services::tui_o::repost::send::{
    AttemptGuard, CreatedMessage, DispatchReport, RepostEnvelope, RepostIds, WireOutcome, hand_over,
};
use crate::services::tui_o::shadow::{ShadowProvider, UnitKey, UnitKind};
use crate::services::tui_o::store::ledger::LedgerEntry;
use crate::services::tui_o::store::{ChannelStore, Initialized, OEra, OStore, StoreConfig};

#[path = "backlog_tests.rs"]
mod backlog_tests;

struct Writer {
    store: ChannelStore,
    gate: Arc<OwnershipGate>,
    live: Arc<AtomicBool>,
    source_allowed: Arc<AtomicBool>,
    prepared: usize,
}

impl Writer {
    fn new(dir: &tempfile::TempDir) -> Self {
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, dir.path())
            .unwrap()
            .unwrap();
        store
            .init_channel(&Initialized {
                channel: CHANNEL,
                sources: Vec::new(),
                initial_anchor: 40,
                build_digest: "d4act".into(),
                at: Utc::now(),
            })
            .unwrap();
        let era = OEra {
            switch_at: Utc::now(),
            initial_channels: vec![CHANNEL],
        };
        let store = store.open_channel(&era, CHANNEL).unwrap().unwrap();
        let gate = Arc::new(OwnershipGate::default());
        gate.acquired();
        Self {
            store,
            gate,
            live: Arc::new(AtomicBool::new(true)),
            source_allowed: Arc::new(AtomicBool::new(true)),
            prepared: 0,
        }
    }

    fn settled(&mut self, native: &str, payload: &str, outcome: LedgerEntry) {
        let serial = self.store.ledger().next_serial();
        self.store
            .append_ledger(LedgerEntry::Prepared {
                local_retry: None,
                serial,
                unit_key: unit(native),
                piece_index: 0,
                payload: payload.into(),
                anchor_id: self.store.ledger().anchor(),
                epoch: 1,
            })
            .unwrap();
        self.store.append_ledger(outcome).unwrap();
    }
}

impl RepostWriter for Writer {
    type Lease = ();

    async fn settle_open(&mut self) -> bool {
        self.store.ledger().unresolved().is_none()
    }

    fn guard(&self) -> Result<(), String> {
        if self.live.load(Ordering::SeqCst) && self.source_allowed.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err("source or turn withdrawn".into())
        }
    }

    fn try_lease(&self) -> Option<Self::Lease> {
        Some(())
    }

    fn gate(&self) -> Arc<OwnershipGate> {
        Arc::clone(&self.gate)
    }

    fn live(&self) -> Arc<dyn Fn() -> bool + Send + Sync> {
        let (live, source) = (Arc::clone(&self.live), Arc::clone(&self.source_allowed));
        Arc::new(move || live.load(Ordering::SeqCst) && source.load(Ordering::SeqCst))
    }

    fn prepare(&mut self, _epoch: u64, _permit: &DispatchPermit) -> Result<(), String> {
        self.prepared += 1;
        Ok(())
    }

    async fn record(&mut self, _report: &DispatchReport) {}
}

impl TurnWriter for Writer {
    fn channel(&self) -> u64 {
        CHANNEL
    }

    fn ledger(&mut self) -> &LedgerState {
        self.store.ledger()
    }

    fn set_live(&self, live: bool) {
        self.live.store(live, Ordering::SeqCst);
    }
}

struct Io {
    reader: Fake,
    posts: Arc<AtomicUsize>,
}

impl Io {
    fn new() -> Self {
        let message = |id, payload: &str| ObservedMessage {
            id,
            channel_id: CHANNEL,
            author_id: BOT,
            content: payload.into(),
            rich_embeds: 0,
            footers: Vec::new(),
            nonce: None,
        };
        Self {
            reader: Fake::with([message(40, "anchor"), message(900, "permission proof")]),
            posts: Arc::default(),
        }
    }
}

impl ProbeRead for Io {
    fn credentials(&self) -> String {
        self.reader.credentials()
    }

    async fn message(&self, channel: u64, id: u64) -> Result<Option<ObservedMessage>, String> {
        self.reader.message(channel, id).await
    }

    async fn history(
        &self,
        channel: u64,
        before: Option<u64>,
        limit: u8,
    ) -> Result<Vec<ObservedMessage>, String> {
        self.reader.history(channel, before, limit).await
    }
}

impl BoundedTransport for Io {
    fn create(
        &self,
        envelope: &RepostEnvelope,
        guard: AttemptGuard,
    ) -> impl Future<Output = WireOutcome> + Send + 'static {
        let posts = Arc::clone(&self.posts);
        let body = serde_json::to_value(envelope.message()).unwrap();
        let footer = body["embeds"][0]["footer"]["text"]
            .as_str()
            .unwrap()
            .to_owned();
        async move {
            if let Err(reason) = guard.begin() {
                return WireOutcome::Unsent(reason);
            }
            posts.fetch_add(1, Ordering::SeqCst);
            WireOutcome::Created(CreatedMessage {
                id: 7_000,
                author_id: BOT,
                content: String::new(),
                footers: vec![footer],
            })
        }
    }
}

fn unit(native: &str) -> UnitKey {
    UnitKey {
        channel_id: CHANNEL,
        provider: ShadowProvider::Claude,
        native_key: native.into(),
        kind: UnitKind::Body,
    }
}

fn run() -> RunScope {
    RunScope {
        holder: "d4act".into(),
        run: "activation".into(),
        credentials: "creds".into(),
        evidence_generation: 1,
    }
}

async fn ready_probe(pool: &PgPool, key: &PieceKey, io: &Io) -> (EvidenceScope, ProbeSession) {
    let row = o_piece_delivery::load(pool, key).await.unwrap().unwrap();
    let attempts = o_piece_attempts::attempts(pool, key).await.unwrap();
    let scope = EvidenceScope::of(&row, &attempts, &run()).unwrap();
    let mut probe = ProbeSession::new(scope.clone(), vec![900], Instant::now() - FIRST_PASS_AFTER);
    let known = AttributionSnapshot::Known {
        receipts: BTreeMap::new(),
        same_payload: Vec::new(),
    };
    assert!(matches!(
        probe.advance(io, &run(), &known).await,
        Progress::FirstPassDone { .. }
    ));
    // Advance only the probe clock, then resume before any PostgreSQL future is polled.
    tokio::time::pause();
    tokio::time::advance(SECOND_PASS_AFTER).await;
    tokio::time::resume();
    (scope, probe)
}

#[derive(Clone, Copy)]
enum Failure {
    Open,
    Activation,
    Admission,
    Resume,
    Missing,
    IdentityConflict,
}

async fn admission_failure_case(failure: Failure, broken: bool) {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let mut writer = Writer::new(&dir);
    writer.settled(
        "candidate",
        "candidate payload",
        LedgerEntry::NotFound { serial: 0 },
    );
    writer.settled(
        "proof",
        "permission proof",
        LedgerEntry::Posted {
            serial: 1,
            msg_id: 900,
        },
    );
    let record = writer.store.ledger().piece(0).unwrap().clone();
    let candidate = piece_of(&record).unwrap();
    let ready =
        admit_on(&pool, &mut sidecar(&dir), &[(3, "ready")], "ready payload").await[0].clone();
    let path = dir.path().join("turn-provenance.jsonl");
    let mut log = ProvenanceLog::open(&path).unwrap();
    let generation = log.activate(0).unwrap().generation;
    log.append(ProvenanceEntry::intent(0, &record, generation).unwrap())
        .unwrap();
    if matches!(failure, Failure::Resume | Failure::Missing) {
        let serial = if broken && matches!(failure, Failure::Missing) {
            100
        } else {
            0
        };
        log.append(ProvenanceEntry::AdmissionPending { serial })
            .unwrap();
    }
    if broken && matches!(failure, Failure::IdentityConflict) {
        admit_on(
            &pool,
            &mut sidecar(&dir),
            &[(4, "candidate")],
            "conflicting payload",
        )
        .await;
    }
    if broken && matches!(failure, Failure::Admission | Failure::Resume) {
        sqlx::raw_sql(
            "CREATE FUNCTION fail_candidate() RETURNS trigger LANGUAGE plpgsql AS $$
             BEGIN IF NEW.native_key = 'candidate' THEN RAISE EXCEPTION 'candidate only'; END IF;
             RETURN NEW; END $$;
             CREATE TRIGGER fail_candidate BEFORE INSERT ON o_piece_delivery
             FOR EACH ROW EXECUTE FUNCTION fail_candidate()",
        )
        .execute(&pool)
        .await
        .unwrap();
    }
    let io = Arc::new(Io::new());
    let opened = Arc::new(AtomicUsize::new(0));
    let mut runner = Runner::new(switch(true), pool.clone(), BOT, run());
    runner
        .probes
        .insert(ready.clone(), ready_probe(&pool, &ready, &io).await);
    let mut turn = ChannelTurn::new(
        runner,
        {
            let (io, opened) = (Arc::clone(&io), Arc::clone(&opened));
            Box::new(move || {
                opened.fetch_add(1, Ordering::SeqCst);
                Some(Arc::clone(&io))
            })
        },
        CHANNEL,
        path,
    );
    if broken && matches!(failure, Failure::Open) {
        let blocked = dir.path().join("blocked-parent");
        std::fs::write(&blocked, b"a file is not a sidecar directory").unwrap();
        turn.sidecar = blocked.join("provenance.jsonl");
    } else {
        if broken && matches!(failure, Failure::Activation) {
            log.fail_after = Some(0);
        }
        turn.log = Some(log);
    }
    turn.run(&mut writer).await;
    assert_eq!(io.posts.load(Ordering::SeqCst), usize::from(!broken));
    assert_eq!(writer.prepared, usize::from(!broken));
    assert_eq!(opened.load(Ordering::SeqCst), usize::from(!broken));
    if broken {
        assert!(!writer.live.load(Ordering::SeqCst));
        assert_eq!(
            o_piece_attempts::attempts(&pool, &ready)
                .await
                .unwrap()
                .len(),
            1
        );
        if matches!(failure, Failure::Admission | Failure::Resume) {
            assert!(
                o_piece_delivery::load(&pool, &candidate)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(
                o_piece_delivery::load(&pool, &ready)
                    .await
                    .unwrap()
                    .is_some()
            );
        }
        if matches!(failure, Failure::IdentityConflict) {
            conflict_turns(&mut turn, &mut writer, &io, &opened, &candidate, &ready).await;
        }
    }
    pool.close().await;
    db.drop().await;
}

async fn conflict_turns(
    turn: &mut ChannelTurn<Io>,
    writer: &mut Writer,
    io: &Io,
    opened: &AtomicUsize,
    candidate: &PieceKey,
    ready: &PieceKey,
) {
    let pool = turn.runner.pool.clone();
    let record = writer.store.ledger().piece(0).unwrap().clone();
    let original = o_piece_attempts::attempts(&pool, candidate).await.unwrap();
    let ready_original = o_piece_attempts::attempts(&pool, ready).await.unwrap();
    let admitted: BTreeSet<_> = o_piece_delivery::admitted_keys(&pool, CHANNEL)
        .await
        .unwrap()
        .into_iter()
        .collect();

    // Reopen the same durable sidecar; an unresolved collision must hold the next turn too.
    turn.log = None;
    turn.run(writer).await;
    assert_eq!(
        io.posts.load(Ordering::SeqCst),
        0,
        "the restored turn stays blocked until canonical identity is repaired"
    );
    assert_eq!(writer.prepared, 0);
    assert_eq!(opened.load(Ordering::SeqCst), 0);
    assert!(!writer.live.load(Ordering::SeqCst));
    assert_eq!(
        o_piece_attempts::attempts(&pool, ready).await.unwrap(),
        ready_original
    );
    assert_eq!(
        o_piece_attempts::attempts(&pool, candidate).await.unwrap(),
        original
    );
    assert_eq!(
        turn.log
            .as_ref()
            .unwrap()
            .state()
            .pending()
            .collect::<Vec<_>>(),
        [0]
    );
    assert_eq!(
        turn.original_gate(0, record.clone()).await,
        OriginalGate::Admitted
    );

    let repaired = repair_identity_conflict(&pool, candidate, &record).await;
    turn.log = None;
    turn.run(writer).await;
    assert_eq!(io.posts.load(Ordering::SeqCst), 1);
    assert_eq!(writer.prepared, 1);
    assert_eq!(opened.load(Ordering::SeqCst), 1);
    assert!(
        turn.log
            .as_ref()
            .unwrap()
            .state()
            .pending()
            .next()
            .is_none()
    );
    assert_eq!(
        o_piece_attempts::attempts(&pool, candidate).await.unwrap(),
        original
    );
    let ready_slots: Vec<_> = o_piece_attempts::attempts(&pool, ready)
        .await
        .unwrap()
        .iter()
        .map(|attempt| attempt.slot)
        .collect();
    assert_eq!(ready_slots, [0, 1]);
    assert_eq!(
        turn.original_gate(0, record.clone()).await,
        OriginalGate::Admitted
    );
    let retained: BTreeSet<_> = o_piece_delivery::admitted_keys(&pool, CHANNEL)
        .await
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(retained, admitted);
    assert_eq!(
        o_piece_delivery::load(&pool, candidate).await.unwrap(),
        Some(repaired)
    );
}

async fn repair_identity_conflict(
    pool: &PgPool,
    candidate: &PieceKey,
    record: &PieceRecord,
) -> DeliveryRow {
    let conflicted = o_piece_delivery::load(pool, candidate)
        .await
        .unwrap()
        .unwrap();
    let payload_hash = payload_sha256(&record.payload);
    assert_eq!(
        conflicted.conflict_sha256.as_deref(),
        Some(payload_hash.as_str())
    );
    // Repair only canonical identity, keeping the admitted key and every spent attempt.
    let repaired = sqlx::query(
        "UPDATE o_piece_delivery SET payload = $1, payload_sha256 = $2,
         conflict_sha256 = NULL, conflict_at = NULL, revision = revision + 1, updated_at = NOW()
         WHERE channel_id = $3 AND provider = 'claude' AND native_key = 'candidate'
         AND kind = 'body' AND piece_index = 0 AND payload_sha256 = $4
         AND conflict_sha256 = $2 AND revision = $5",
    )
    .bind(&record.payload)
    .bind(&payload_hash)
    .bind(CHANNEL.to_string())
    .bind(&conflicted.payload_sha256)
    .bind(conflicted.revision)
    .execute(pool)
    .await
    .unwrap();
    assert_eq!(repaired.rows_affected(), 1);
    let mut expected = conflicted;
    expected.payload = record.payload.clone();
    expected.payload_sha256 = payload_hash;
    expected.conflict_sha256 = None;
    expected.revision += 1;
    expected
}

#[tokio::test]
async fn f6_sidecar_failure_blocks_ready_repost_pg() {
    for failure in [Failure::Open, Failure::Activation] {
        admission_failure_case(failure, false).await;
        admission_failure_case(failure, true).await;
    }
}

#[tokio::test]
async fn f6_admission_error_blocks_other_ready_repost_pg() {
    admission_failure_case(Failure::Admission, false).await;
    admission_failure_case(Failure::Admission, true).await;
}

#[tokio::test]
async fn d4act_resume_error_blocks_other_ready_repost_pg() {
    admission_failure_case(Failure::Resume, false).await;
    admission_failure_case(Failure::Resume, true).await;
}

#[tokio::test]
async fn d4act_missing_pending_record_blocks_other_ready_repost_pg() {
    admission_failure_case(Failure::Missing, false).await;
    admission_failure_case(Failure::Missing, true).await;
}

#[tokio::test]
async fn d4act_identity_conflict_blocks_other_ready_repost_pg() {
    admission_failure_case(Failure::IdentityConflict, false).await;
    admission_failure_case(Failure::IdentityConflict, true).await;
}

#[tokio::test]
async fn f6_original_intent_failure_holds_until_durable_intent_pg() {
    enum OriginalFailure {
        Open,
        Activation,
        Intent,
    }
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for cause in [
        OriginalFailure::Open,
        OriginalFailure::Activation,
        OriginalFailure::Intent,
    ] {
        for broken in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let io = Arc::new(Io::new());
            let record = PieceRecord {
                local_retry: None,
                outcome_at: None,
                unit_key: unit("fresh"),
                piece_index: 0,
                payload: "fresh payload".into(),
                anchor_id: 40,
                epoch: 1,
                prepared_at: Utc::now(),
                outcome: None,
            };
            let key = piece_of(&record).unwrap();
            let mut turn = ChannelTurn::<Io>::new(
                Runner::new(switch(true), pool.clone(), BOT, run()),
                Box::new(|| None),
                CHANNEL,
                dir.path().join("provenance.jsonl"),
            );
            if broken && matches!(cause, OriginalFailure::Open) {
                let blocked = dir.path().join("blocked-parent");
                std::fs::write(&blocked, b"file").unwrap();
                turn.sidecar = blocked.join("provenance.jsonl");
            } else if broken && matches!(cause, OriginalFailure::Activation) {
                let mut log = ProvenanceLog::open(&turn.sidecar).unwrap();
                log.fail_after = Some(0);
                turn.log = Some(log);
            } else {
                turn.log(0).unwrap().fail_after = broken.then_some(0);
            }
            let gate = turn.original_gate(0, record).await;
            assert_eq!(
                gate,
                if broken {
                    OriginalGate::Unknown
                } else {
                    OriginalGate::SendPgManaged
                }
            );
            if gate == OriginalGate::SendPgManaged {
                let envelope = RepostEnvelope::additional(
                    CHANNEL,
                    "fresh payload".into(),
                    RepostIds::for_piece(&marker(&key)).unwrap(),
                );
                let (request, _) = hand_over(&*io, &envelope, Arc::new(|| true));
                assert!(matches!(request.await, WireOutcome::Created(_)));
            }
            assert_eq!(io.posts.load(Ordering::SeqCst), usize::from(!broken));
            if broken {
                // Reopening cuts the failed append and writes the same key's durable intent.
                turn.sidecar = dir.path().join("provenance.jsonl");
                let retry = PieceRecord {
                    local_retry: None,
                    outcome_at: None,
                    unit_key: unit("fresh"),
                    piece_index: 0,
                    payload: "fresh payload".into(),
                    anchor_id: 40,
                    epoch: 1,
                    prepared_at: Utc::now(),
                    outcome: None,
                };
                assert_eq!(
                    turn.original_gate(0, retry).await,
                    OriginalGate::SendPgManaged
                );
                assert_eq!(
                    turn.log
                        .as_ref()
                        .unwrap()
                        .state()
                        .intent(0)
                        .unwrap()
                        .key
                        .as_ref(),
                    Some(&key)
                );
            }
            assert!(o_piece_delivery::load(&pool, &key).await.unwrap().is_none());
        }
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn d4act_unsupported_original_uses_local_owner_when_sidecar_is_unavailable_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let blocked = dir.path().join("blocked-parent");
    std::fs::write(&blocked, b"file").unwrap();
    let mut turn = ChannelTurn::<Io>::new(
        Runner::new(switch(true), pool.clone(), BOT, run()),
        Box::new(|| None),
        CHANNEL,
        blocked.join("provenance.jsonl"),
    );
    let record = PieceRecord {
        local_retry: None,
        outcome_at: None,
        unit_key: unit("unsupported"),
        piece_index: 0,
        payload: "".into(),
        anchor_id: 40,
        epoch: 1,
        prepared_at: Utc::now(),
        outcome: None,
    };
    assert!(ProvenanceEntry::intent(0, &record, 0).is_none());
    assert_eq!(turn.original_gate(0, record).await, OriginalGate::SendLocal);
    assert!(turn.log.is_none());
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn d4act_source_revoked_after_tick_never_grants_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for revoke in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let native = if revoke {
            "before-grant-revoked"
        } else {
            "before-grant-positive"
        };
        let key =
            admit_on(&pool, &mut sidecar(&dir), &[(3, native)], "ready payload").await[0].clone();
        let row = o_piece_delivery::load(&pool, &key).await.unwrap().unwrap();
        let attempts = o_piece_attempts::attempts(&pool, &key).await.unwrap();
        let scope = EvidenceScope::of(&row, &attempts, &run()).unwrap();
        let evidence = absent_evidence(scope, run());
        let mut blocker = pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE o_piece_delivery IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *blocker)
            .await
            .unwrap();
        let mut writer = Writer::new(&dir);
        let source = Arc::clone(&writer.source_allowed);
        let task_pool = pool.clone();
        let waiting = tokio::spawn(async move {
            let mut runner = Runner::new(switch(true), task_pool, BOT, run());
            let admission = runner
                .try_next_dispatch(
                    &mut writer,
                    OriginalGate::Admitted,
                    DispatchIntent::Auto(Box::new(evidence)),
                )
                .await
                .unwrap();
            (admission, writer)
        });
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let count: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM pg_stat_activity WHERE datname = current_database()
                     AND wait_event_type = 'Lock'",
                )
                .fetch_one(&pool)
                .await
                .unwrap();
                if count == 1 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("row read reached its await after the first guard");
        source.store(!revoke, Ordering::SeqCst);
        blocker.commit().await.unwrap();
        let (admission, mut writer) = waiting.await.unwrap();
        let io = Io::new();
        if revoke {
            assert!(matches!(
                admission,
                Admission::GuardRejected(GuardReason::Writer(_))
            ));
            assert_eq!(
                o_piece_attempts::attempts(&pool, &key).await.unwrap().len(),
                1
            );
        } else {
            let Admission::Granted { permit, lease } = admission else {
                panic!("positive grants");
            };
            dispatch::start(&mut writer, permit, lease, &io)
                .await
                .finish(&mut writer, &pool)
                .await
                .unwrap();
        }
        assert_eq!(
            (writer.prepared, io.posts.load(Ordering::SeqCst)),
            if revoke { (0, 0) } else { (1, 1) }
        );
    }
    pool.close().await;
    db.drop().await;
}

#[path = "allowlist_runner_tests.rs"]
mod allowlist;
