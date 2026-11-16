use super::tests::{Frame, Runtime, debt};
use super::*;
use crate::services::tui_o::{
    shadow::ShadowProvider,
    store::ledger::{LedgerEntry, PieceOutcome, PieceRecord, PostConfirmation},
    store::{ChannelStore, LEDGER_FILE, OStore, STORE_DIR_NAME, StoreConfig},
    writer::herdr_body_debt::{self, Plan, tests as snapshots},
};
use std::sync::atomic::{AtomicBool, Ordering};

impl super::Runtime for Runtime {
    fn channel(&self) -> u64 {
        self.binding.channel.parse().unwrap()
    }
    fn run(&self) -> Uuid {
        self.binding.run
    }
    fn store_sha(&self) -> &str {
        &self.binding.store_sha
    }
    fn bot(&self) -> u64 {
        self.binding.bot_id
    }
    fn live(&self) -> bool {
        authority::Live::permits(self, &self.binding)
    }
}
impl Frame {
    fn plan(&self) -> Plan {
        snapshots::seed(
            self.root.path(),
            snapshots::row(7651201),
            serde_json::json!({"source":null}),
        );
        herdr_body_debt::read(self.root.path(), ShadowProvider::Claude, 7651201, None)
            .unwrap()
            .pop()
            .unwrap()
    }
    fn store(&self) -> ChannelStore {
        let disk = OStore::open_if_enabled(&StoreConfig { enabled: true }, self.root.path())
            .unwrap()
            .unwrap();
        disk.open_channel(&disk.read_era().unwrap().unwrap(), 7651201)
            .unwrap()
            .unwrap()
    }
    async fn session(&self) -> Session {
        self.scope
            .open_debt_session(
                self.runtime.clone(),
                Custodian::execution(&self.runtime.permit),
            )
            .await
            .unwrap()
    }
    async fn successor_session(&self) -> Session {
        let mut binding = self.runtime.binding.clone();
        binding.run = Uuid::new_v4();
        let runtime = Arc::new(Runtime {
            binding,
            permit: self.runtime.permit.clone(),
            active: AtomicBool::new(true),
        });
        self.scope
            .open_debt_session(runtime, Custodian::execution(&self.runtime.permit))
            .await
            .unwrap()
    }
}
fn prepared(artifact: &super::super::Artifact, anchor: u64) -> LedgerEntry {
    let r = &debt(artifact).debt_recovery;
    LedgerEntry::DebtPrepared {
        serial: 0,
        artifact: Box::new(artifact.clone()),
        unit_key: serde_json::from_value(serde_json::to_value(&r.unit_key).unwrap()).unwrap(),
        piece_index: r.piece_index,
        payload: "retained answer".into(),
        anchor_id: anchor,
        epoch: 1,
    }
}
fn posted(store: &mut ChannelStore, message: u64) -> (PieceRecord, PostConfirmation) {
    let mut piece = store.ledger().piece(0).unwrap().clone();
    piece.outcome = Some(PieceOutcome::Posted(message));
    let proof = PostConfirmation::new(0, &piece, message, 42);
    store
        .append_ledger(LedgerEntry::Posted {
            serial: 0,
            msg_id: message,
            confirmation: Some(Box::new(proof.clone())),
        })
        .unwrap();
    (piece, proof)
}

#[tokio::test]
async fn debt_session_is_bound_to_a_verified_custodian_and_live_store_pg() {
    let f = Frame::new().await;
    let plan = f.plan();
    let session = f.session().await;
    session.register(&plan).await.unwrap();
    let grant = session.issue(&plan, 0).await.unwrap();
    assert!(
        f.scope
            .acquire("7651202", "retained answer", Some(&grant))
            .await
            .is_err()
    );
    assert!(
        Scope {
            pool: f.pool.clone(),
            provider: "codex".into()
        }
        .open_debt_session(f.runtime.clone(), Custodian::execution(&f.runtime.permit))
        .await
        .is_err()
    );
    let gateway = crate::services::tui_o::ownership::OwnershipGate::default();
    gateway.acquired();
    assert!(
        f.scope
            .open_debt_session(
                f.runtime.clone(),
                Custodian::gateway(&gateway, "boot".into())
            )
            .await
            .is_err(),
        "gateway cannot replace active execution custody"
    );
    let mut binding = f.runtime.binding.clone();
    binding.store_sha = "not-a-verified-digest".into();
    let invalid = Arc::new(Runtime {
        binding,
        permit: f.runtime.permit.clone(),
        active: AtomicBool::new(true),
    });
    assert!(
        f.scope
            .open_debt_session(invalid, Custodian::execution(&f.runtime.permit))
            .await
            .is_err()
    );
    f.runtime.active.store(false, Ordering::Release);
    assert!(session.issue(&plan, 0).await.is_err());
    session.close().await.unwrap();
    assert!(
        f.scope
            .acquire("7651201", "retained answer", Some(&grant))
            .await
            .is_err()
    );
    f.finish().await;
}

#[tokio::test]
async fn debt_session_restart_preserves_an_uncertain_reservation_without_proof_pg() {
    let f = Frame::new().await;
    let plan = f.plan();
    let session = f.session().await;
    let grant = session.issue(&plan, 0).await.unwrap();
    drop(
        f.scope
            .acquire("7651201", "retained answer", Some(&grant))
            .await
            .unwrap(),
    );
    let next = f.successor_session().await;
    session.close().await.unwrap();
    assert!(
        next.issue(&plan, 0).await.is_err(),
        "a new run cannot reset an uncertain effect"
    );
    let (state,witness):(String,Option<Value>)=sqlx::query_as("SELECT e.state,a.no_effect_witness FROM o_herdr_debt_effects e JOIN o_herdr_debt_attempts a ON a.id=e.attempt_id").fetch_one(&f.pool).await.unwrap();
    assert_eq!(state, "reserved");
    assert_eq!(witness, None);
    sqlx::query("UPDATE o_herdr_debt_effects SET state='ready'")
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(
        next.issue(&plan, 0).await.is_err(),
        "a ready label without the exact attempt witness cannot reset an uncertain reservation"
    );
    assert!(
        f.scope
            .acquire("7651201", "retained answer", Some(&grant))
            .await
            .is_err()
    );
    next.close().await.unwrap();
    f.finish().await;
}

#[tokio::test]
async fn debt_session_retries_only_its_owned_unpolled_attempt_and_keeps_exact_receipts_pg() {
    let f = Frame::new().await;
    let plan = f.plan();
    let session = f.session().await;
    let grant = session.issue(&plan, 0).await.unwrap();
    f.scope
        .acquire("7651201", "retained answer", Some(&grant))
        .await
        .unwrap()
        .cancel_unpolled()
        .await
        .unwrap();
    let first = debt(&grant).debt_recovery.effect_id;
    let next = f.successor_session().await;
    session.close().await.unwrap();
    let retry = next.issue(&plan, 0).await.unwrap();
    assert_eq!(debt(&retry).debt_recovery.effect_id, first);
    f.scope
        .acquire("7651201", "retained answer", Some(&retry))
        .await
        .unwrap()
        .run(async {})
        .await
        .unwrap();
    let mut store = f.store();
    store.append_ledger(prepared(&retry, 0)).unwrap();
    let (piece, proof) = posted(&mut store, 12345);
    next.confirm(&retry, 0, &piece, &proof).await.unwrap();
    next.confirm(&retry, 0, &piece, &proof).await.unwrap();
    let effects = next.inspect(&plan).await.unwrap();
    assert_eq!(effects[0].index, 0);
    assert_eq!(effects[0].state, "confirmed");
    assert!(
        next.issue(&plan, 0).await.is_err(),
        "a receipt consumes every grant for the effect"
    );
    next.close().await.unwrap();
    f.finish().await;
}

#[tokio::test]
async fn debt_atomic_prepared_rejects_a_bad_anchor_without_leaving_an_origin_pg() {
    let f = Frame::new().await;
    let plan = f.plan();
    let session = f.session().await;
    let grant = session.issue(&plan, 0).await.unwrap();
    let mut store = f.store();
    store.append_ledger(prepared(&grant, 1)).unwrap();
    assert!(store.ledger().violation().is_some());
    assert!(
        store.ledger().output_origin(0).is_none(),
        "invalid Prepared must leave no orphan origin"
    );
    assert!(store.ledger().piece(0).is_none());
    session.close().await.unwrap();
    f.finish().await;
}

#[tokio::test]
async fn debt_atomic_prepared_crash_cuts_never_restore_only_an_origin_pg() {
    let f = Frame::new().await;
    let plan = f.plan();
    let session = f.session().await;
    let grant = session.issue(&plan, 0).await.unwrap();
    let mut store = f.store();
    store.append_ledger(prepared(&grant, 0)).unwrap();
    let path = f
        .root
        .path()
        .join(STORE_DIR_NAME)
        .join("7651201")
        .join(LEDGER_FILE);
    let complete = std::fs::read(&path).unwrap();
    drop(store);
    for cut in [0, complete.len() / 2, complete.len()] {
        std::fs::write(&path, &complete[..cut]).unwrap();
        let recovered = f.store();
        assert_eq!(
            recovered.ledger().output_origin(0).is_some(),
            cut == complete.len()
        );
        assert_eq!(recovered.ledger().piece(0).is_some(), cut == complete.len());
        assert_eq!(
            std::fs::read(&path).unwrap().len(),
            if cut == complete.len() { cut } else { 0 }
        );
    }
    let wire = serde_json::to_value(&grant).unwrap();
    assert!(wire.get("debt_recovery").is_some());
    assert!(!wire.to_string().contains("attachment"));
    let disk: super::super::Artifact = serde_json::from_value(wire).unwrap();
    assert!(
        f.scope
            .acquire("7651201", "retained answer", Some(&disk))
            .await
            .is_err()
    );
    let original = f.original.seal("retained answer");
    let wire = serde_json::to_value(&original).unwrap();
    assert!(wire.get("origin").is_some());
    assert!(wire.get("debt_recovery").is_none());
    assert_eq!(
        serde_json::from_value::<super::super::Artifact>(wire).unwrap(),
        original
    );
    session.close().await.unwrap();
    f.finish().await;
}

#[tokio::test]
async fn debt_receipt_rejects_zero_message_and_wrong_exact_proof_pg() {
    let f = Frame::new().await;
    let plan = f.plan();
    let session = f.session().await;
    let grant = session.issue(&plan, 0).await.unwrap();
    f.scope
        .acquire("7651201", "retained answer", Some(&grant))
        .await
        .unwrap()
        .run(async {})
        .await
        .unwrap();
    let mut store = f.store();
    store.append_ledger(prepared(&grant, 0)).unwrap();
    let mut piece = store.ledger().piece(0).unwrap().clone();
    piece.outcome = Some(PieceOutcome::Posted(0));
    let zero = PostConfirmation::new(0, &piece, 0, 42);
    assert!(
        session.confirm(&grant, 0, &piece, &zero).await.is_err(),
        "zero is not a Discord effect"
    );
    piece.outcome = Some(PieceOutcome::Posted(12345));
    let wrong = PostConfirmation::new(0, &piece, 12345, 43);
    assert!(session.confirm(&grant, 0, &piece, &wrong).await.is_err());
    let proof = PostConfirmation::new(0, &piece, 12345, 42);
    assert!(session.confirm(&grant, 1, &piece, &proof).await.is_err());
    session.confirm(&grant, 0, &piece, &proof).await.unwrap();
    sqlx::query("UPDATE o_herdr_debt_effects SET receipt=jsonb_set(receipt,'{payload_sha}',to_jsonb('wrong'::text))").execute(&f.pool).await.unwrap();
    assert!(session.inspect(&plan).await.is_err());
    session.close().await.unwrap();
    f.finish().await;
}

#[tokio::test]
async fn debt_shared_only_receipt_is_a_mirror_and_never_native_posted_pg() {
    let f = Frame::new().await;
    let plan = f.plan();
    let session = f.session().await;
    let grant = session.issue(&plan, 0).await.unwrap();
    f.scope
        .acquire("7651201", "retained answer", Some(&grant))
        .await
        .unwrap()
        .run(async {})
        .await
        .unwrap();
    let r = &debt(&grant).debt_recovery;
    let (piece, proof) = super::tests::posted(r);
    session.confirm(&grant, 0, &piece, &proof).await.unwrap();
    let receipt = session
        .inspect(&plan)
        .await
        .unwrap()
        .remove(0)
        .receipt
        .unwrap();
    let mut store = f.store();
    assert!(store.ledger().piece(0).is_none());
    store
        .append_ledger(LedgerEntry::DebtReceipt {
            receipt: Box::new(receipt.clone()),
        })
        .unwrap();
    assert!(store.ledger().debt_receipt(&receipt));
    assert_eq!(store.ledger().anchor(), 12345);
    assert!(
        store.ledger().piece(0).is_none(),
        "mirror cannot invent native Prepared/Posted"
    );
    assert!(store.ledger().post_confirmation(0).is_none());
    assert!(store.ledger().debt_context(7651201, "claude"));
    assert!(!store.ledger().debt_context(7651202, "claude"));
    store.cover_herdr_debt(&session.coverage(&plan).await.unwrap().unwrap());
    drop(store);
    let restored = f.store();
    assert!(restored.ledger().debt_receipt(&receipt));
    assert!(restored.ledger().piece(0).is_none());
    assert_eq!(restored.ledger().next_serial(), 0);
    let snapshots = plan.registration(42).unwrap()["snapshots"].clone();
    let reread = f.plan();
    assert_eq!(
        reread.registration(42).unwrap()["snapshots"],
        snapshots,
        "C1 v1 bytes are retained without GC or rewriting"
    );
    session.close().await.unwrap();
    f.finish().await;
}

#[tokio::test]
async fn debt_native_receipt_matches_original_identity_not_only_text_pg() {
    let f = Frame::new().await;
    let (plan, mut store) = snapshots::native_recovery_plan(f.root.path(), &f.original);
    let session = f.session().await;
    session.register(&plan).await.unwrap();
    let origin = store.ledger().output_origin(0).unwrap().clone();
    let (piece, proof) = posted(&mut store, 12345);
    let mut changed = serde_json::to_value(&origin).unwrap();
    changed["origin"]["delivery"] = serde_json::json!(999999);
    let other = serde_json::from_value(changed).unwrap();
    assert!(
        session
            .confirm_native(&plan, &other, 0, &piece, &proof)
            .await
            .is_err()
    );
    session
        .confirm_native(&plan, &origin, 0, &piece, &proof)
        .await
        .unwrap();
    session
        .confirm_native(&plan, &origin, 0, &piece, &proof)
        .await
        .expect("exact backfill is idempotent across a shared/local crash cut");
    let mut different = piece.clone();
    different.outcome = Some(PieceOutcome::Posted(12346));
    assert!(
        session
            .confirm_native(
                &plan,
                &origin,
                0,
                &different,
                &PostConfirmation::new(0, &different, 12346, 42)
            )
            .await
            .is_err()
    );
    let coverage = session.coverage(&plan).await.unwrap().unwrap();
    store.cover_herdr_debt(&coverage);
    assert_eq!(
        store.ledger().piece(0).unwrap().outcome,
        Some(PieceOutcome::Posted(12345))
    );
    session.close().await.unwrap();
    f.finish().await;
}

#[tokio::test]
async fn debt_shared_receipt_rejects_an_invalid_post_confirmation_pg() {
    let f = Frame::new().await;
    let plan = f.plan();
    let session = f.session().await;
    let grant = session.issue(&plan, 0).await.unwrap();
    f.scope
        .acquire("7651201", "retained answer", Some(&grant))
        .await
        .unwrap()
        .run(async {})
        .await
        .unwrap();
    let r = &debt(&grant).debt_recovery;
    let (mut invalid, _) = super::tests::posted(r);
    invalid.outcome = Some(PieceOutcome::Posted(0));
    assert!(
        session
            .confirm(
                &grant,
                0,
                &invalid,
                &PostConfirmation::new(0, &invalid, 0, 42)
            )
            .await
            .is_err(),
        "zero is not a Discord effect receipt"
    );

    assert!(session.inspect(&plan).await.unwrap()[0].receipt.is_none());
    session.close().await.unwrap();
    f.finish().await;
}
#[tokio::test]
async fn debt_atomic_prepared_io_failure_keeps_both_origin_and_piece_absent_pg() {
    let f = Frame::new().await;
    let plan = f.plan();
    let session = f.session().await;
    let grant = session.issue(&plan, 0).await.unwrap();
    let mut store = f.store();
    let path = f
        .root
        .path()
        .join(STORE_DIR_NAME)
        .join("7651201")
        .join(LEDGER_FILE);
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(store.append_ledger(prepared(&grant, 0)).is_err());
    assert!(store.ledger().output_origin(0).is_none());
    assert!(store.ledger().piece(0).is_none());
    session.close().await.unwrap();
    f.finish().await;
}
#[tokio::test]
async fn debt_shared_coverage_cannot_settle_another_original_with_the_same_text_pg() {
    use crate::services::tui_o::{store::Initialized, store::ledger::PieceDisposition};
    let f = Frame::new().await;
    let (plan, mut native) = snapshots::native_recovery_plan(f.root.path(), &f.original);
    let session = f.session().await;
    session.register(&plan).await.unwrap();
    let origin = native.ledger().output_origin(0).unwrap().clone();
    let (piece, proof) = posted(&mut native, 12345);
    session
        .confirm_native(&plan, &origin, 0, &piece, &proof)
        .await
        .unwrap();
    let coverage = session.coverage(&plan).await.unwrap().unwrap();
    for other in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let disk = OStore::open_if_enabled(&StoreConfig { enabled: true }, dir.path())
            .unwrap()
            .unwrap();
        let era = disk
            .begin_fixture_era(&[7651201], chrono::Utc::now(), |channel| {
                Ok(Initialized {
                    channel,
                    sources: vec![],
                    initial_anchor: 0,
                    build_digest: "fixture".into(),
                    at: chrono::Utc::now(),
                    policy_parent: None,
                })
            })
            .unwrap();
        let mut store = disk.open_channel(&era, 7651201).unwrap().unwrap();
        let mut artifact = serde_json::to_value(&origin).unwrap();
        if other {
            artifact["origin"]["delivery"] = serde_json::json!(999999);
        }
        store
            .append_ledger(LedgerEntry::OutputOrigin {
                serial: 0,
                artifact: serde_json::from_value(artifact).unwrap(),
            })
            .unwrap();
        store
            .append_ledger(LedgerEntry::Prepared {
                serial: 0,
                unit_key: piece.unit_key.clone(),
                piece_index: piece.piece_index,
                payload: piece.payload.clone(),
                anchor_id: 0,
                epoch: 1,
                local_retry: None,
                notice_origin: None,
            })
            .unwrap();
        store
            .append_ledger(LedgerEntry::Rejected {
                serial: 0,
                status: 400,
            })
            .unwrap();
        store.cover_herdr_debt(&coverage);
        assert_eq!(
            store
                .ledger()
                .disposition(&piece.unit_key, piece.piece_index)
                == PieceDisposition::Settled,
            !other,
            "coverage must match Original artifact, not only text"
        );
        assert_eq!(
            store.ledger().piece(0).unwrap().outcome,
            Some(PieceOutcome::Rejected(400))
        );
        assert!(
            store.ledger().post_confirmation(0).is_none(),
            "coverage is not a native Posted line"
        );
    }
    session.close().await.unwrap();
    f.finish().await;
}
