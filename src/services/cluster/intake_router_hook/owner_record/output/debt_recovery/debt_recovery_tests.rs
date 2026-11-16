use super::*;
use crate::services::{
    cluster::intake_router_hook::owner_record::{
        output::tests as original_tests, transfer_owner_in_tx,
    },
    tui_o::{
        ownership::{GatewayOwnership, execution},
        store::ledger::{PieceOutcome, PieceRecord, PostConfirmation},
        store::{INIT_FILE, Initialized, OStore, STORE_DIR_NAME, StoreConfig},
    },
};
use chrono::Utc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
struct Runtime {
    binding: Binding,
    permit: Arc<execution::OwnerPermit>,
    active: AtomicBool,
}
impl authority::Live for Runtime {
    fn permits(&self, b: &Binding) -> bool {
        self.active.load(Ordering::Acquire)
            && &self.binding == b
            && matches!(self.permit.gate().current(),GatewayOwnership::ExecutionOwned{generation,..} if generation==b.revision)
    }
}
struct Frame {
    db: original_tests::FixtureDb,
    pool: sqlx::PgPool,
    root: tempfile::TempDir,
    original: super::super::Origin,
    scope: Scope,
    runtime: Arc<Runtime>,
}
impl Frame {
    async fn new() -> Self {
        let (db, pool, original) = original_tests::fixture("7651201").await;
        original_tests::revoke(&pool, &original).await;
        let permit = execution::read(&pool, "claude", 7651201, "returned-home", || Ok(false))
            .await
            .unwrap()
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, root.path())
            .unwrap()
            .unwrap();
        store
            .begin_fixture_era(&[7651201], Utc::now(), |channel| {
                Ok(Initialized {
                    channel,
                    sources: vec![],
                    initial_anchor: 0,
                    build_digest: "fixture".into(),
                    at: Utc::now(),
                    policy_parent: None,
                })
            })
            .unwrap();
        let bytes = std::fs::read_to_string(
            root.path()
                .join(STORE_DIR_NAME)
                .join("7651201")
                .join(INIT_FILE),
        )
        .unwrap();
        let binding = Binding {
            provider: "claude".into(),
            channel: "7651201".into(),
            custodian: permit.executor.clone(),
            domain: "execution".into(),
            revision: permit.generation,
            run: Uuid::new_v4(),
            store_sha: hash(&bytes),
            bot_id: 42,
        };
        let scope = Scope {
            pool: pool.clone(),
            provider: "claude".into(),
        };
        Self {
            db,
            pool,
            root,
            original,
            scope,
            runtime: Arc::new(Runtime {
                binding,
                permit,
                active: AtomicBool::new(true),
            }),
        }
    }
    async fn grant(&self) -> super::super::Artifact {
        let snapshot = serde_json::to_string(&self.original).unwrap();
        let episode = hash(&format!(
            "old:{}:{}",
            self.original.nonce, self.original.delivery
        ));
        let r = Reference {
            version: 1,
            provider: "claude".into(),
            channel: "7651201".into(),
            episode,
            input_key: hash("accepted:90001"),
            coverage: "native:old-body:0".into(),
            snapshot_sha: hash(&snapshot),
            plan_sha: hash("sealed-old-plan"),
            payload_sha: hash("retained answer"),
            unit_key: Unit {
                channel_id: 7651201,
                provider: "claude".into(),
                native_key: "old-native-answer".into(),
                kind: "body".into(),
            },
            piece_index: 0,
            effect_id: Uuid::new_v4(),
            grant_id: Uuid::new_v4(),
            authority: self.runtime.binding.clone(),
        };
        authority::issue(
            &self.pool,
            &self.runtime.binding,
            self.runtime.clone(),
            r,
            serde_json::from_str(&snapshot).unwrap(),
            "retained answer",
        )
        .await
        .unwrap()
    }
    async fn finish(self) {
        self.pool.close().await;
        drop(self.db);
        drop(self.root);
    }
}
fn debt(artifact: &super::super::Artifact) -> &Artifact {
    let super::super::Artifact::Debt(a) = artifact else {
        panic!("recovery variant")
    };
    a
}
fn debt_mut(artifact: &mut super::super::Artifact) -> &mut Artifact {
    let super::super::Artifact::Debt(a) = artifact else {
        panic!("recovery variant")
    };
    a
}
async fn acquire(f: &Frame, a: &super::super::Artifact) -> Result<EffectGuard, AcquireError> {
    f.scope.acquire("7651201", "retained answer", Some(a)).await
}
fn posted(r: &Reference) -> (PieceRecord, PostConfirmation) {
    let piece = PieceRecord {
        unit_key: serde_json::from_value(serde_json::to_value(&r.unit_key).unwrap()).unwrap(),
        piece_index: r.piece_index,
        payload: "retained answer".into(),
        anchor_id: 0,
        epoch: 1,
        prepared_at: Utc::now(),
        outcome: Some(PieceOutcome::Posted(12345)),
        local_retry: None,
        notice_origin: None,
        outcome_at: Some(Utc::now()),
    };
    let confirmation = PostConfirmation::new(0, &piece, 12345, 42);
    (piece, confirmation)
}
#[tokio::test]
async fn debt_recovery_capability_for_retained_old_input_pg() {
    let f = Frame::new().await;
    let old = f.original.seal("retained answer");
    assert!(acquire(&f, &old).await.is_err());
    assert!(
        f.scope
            .acquire("7651201", "retained answer", None)
            .await
            .is_err()
    );
    let grant = f.grant().await;
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    acquire(&f, &grant)
        .await
        .expect("verified returned custodian can acquire only this retained debt")
        .run(async move {
            observed.fetch_add(1, Ordering::SeqCst);
        })
        .await
        .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(
        acquire(&f, &old).await.is_err(),
        "old native stays revoked after debt POST"
    );
    f.finish().await;
}
#[tokio::test]
async fn debt_recovery_episode_binding_refuses_a_different_old_input_pg() {
    let f = Frame::new().await;
    let mut grant = f.grant().await;
    debt_mut(&mut grant).debt_recovery.episode = hash("different original episode");
    assert!(
        acquire(&f, &grant).await.is_err(),
        "grant cannot attach to another old episode"
    );
    f.finish().await;
}
#[tokio::test]
async fn debt_recovery_revoked_capability_never_starts_a_request_pg() {
    let f = Frame::new().await;
    let grant = f.grant().await;
    sqlx::query("UPDATE o_herdr_debt_grants SET state='revoked' WHERE id=$1")
        .bind(debt(&grant).debt_recovery.grant_id)
        .execute(&f.pool)
        .await
        .unwrap();
    assert!(
        acquire(&f, &grant).await.is_err(),
        "revoked grant cannot acquire"
    );
    f.finish().await;
}
#[tokio::test]
async fn debt_recovery_confirmed_effect_refuses_a_second_preissued_grant_pg() {
    let f = Frame::new().await;
    let first = f.grant().await;
    let second = f.grant().await;
    let r = &debt(&first).debt_recovery;
    assert_eq!(r.effect_id, debt(&second).debt_recovery.effect_id);
    acquire(&f, &first)
        .await
        .unwrap()
        .run(async {})
        .await
        .unwrap();
    let (piece, confirmation) = posted(r);
    receipt::confirm(&f.pool, r, 0, &piece, &confirmation)
        .await
        .unwrap();
    assert!(
        acquire(&f, &second).await.is_err(),
        "durable receipt prohibits another grant from posting the debt"
    );
    f.finish().await;
}
#[tokio::test]
async fn debt_recovery_owner_return_and_new_run_do_not_revive_an_old_capability_pg() {
    let f = Frame::new().await;
    let grant = f.grant().await;
    let mut new = f.runtime.binding.clone();
    new.run = Uuid::new_v4();
    sqlx::query(
        "UPDATE o_herdr_debt_runs SET binding=$1 WHERE provider='claude' AND channel_id='7651201'",
    )
    .bind(serde_json::to_value(new).unwrap())
    .execute(&f.pool)
    .await
    .unwrap();
    assert!(
        acquire(&f, &grant).await.is_err(),
        "old run is not restored from a token"
    );
    f.finish().await;
}
#[tokio::test]
async fn debt_recovery_new_owner_refuses_an_old_issued_grant_pg() {
    let f = Frame::new().await;
    let grant = f.grant().await;
    let mut tx = f.pool.begin().await.unwrap();
    let id = OwnerIdentity::new("claude", "7651201");
    crate::db::channel_custody_lock::lock(&mut tx, &id)
        .await
        .unwrap();
    transfer_owner_in_tx(
        &mut tx,
        &id,
        "returned-home",
        f.runtime.binding.revision,
        "successor",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(
        acquire(&f, &grant).await.is_err(),
        "successor owner invalidates the old revision"
    );
    f.finish().await;
}
#[tokio::test]
async fn debt_recovery_reserved_crash_and_run_change_keep_pending_without_proof_pg() {
    let f = Frame::new().await;
    let first = f.grant().await;
    let next = f.grant().await;
    let r = &debt(&first).debt_recovery;
    drop(acquire(&f, &first).await.unwrap());
    let mut binding = f.runtime.binding.clone();
    binding.run = Uuid::new_v4();
    sqlx::query(
        "UPDATE o_herdr_debt_runs SET binding=$1 WHERE provider='claude' AND channel_id='7651201'",
    )
    .bind(serde_json::to_value(binding).unwrap())
    .execute(&f.pool)
    .await
    .unwrap();
    assert!(acquire(&f, &next).await.is_err());
    let (state,proof):(String,Option<Value>)=sqlx::query_as("SELECT e.state,a.no_effect_witness FROM o_herdr_debt_effects e JOIN o_herdr_debt_attempts a ON a.id=e.attempt_id WHERE e.id=$1").bind(r.effect_id).fetch_one(&f.pool).await.unwrap();
    assert_eq!(state, "reserved");
    assert_eq!(proof, None, "run changes never invent no-effect proof");
    f.finish().await;
}
#[tokio::test]
async fn debt_recovery_only_owning_unpolled_attempt_records_no_effect_pg() {
    let f = Frame::new().await;
    let grant = f.grant().await;
    let r = &debt(&grant).debt_recovery;
    let guard = acquire(&f, &grant).await.unwrap();
    f.runtime.active.store(false, Ordering::Release);
    let count = AtomicUsize::new(0);
    assert!(
        guard
            .run(async {
                count.fetch_add(1, Ordering::SeqCst);
            })
            .await
            .is_err()
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let (state,id,proof):(String,Uuid,Value)=sqlx::query_as("SELECT e.state,a.id,a.no_effect_witness FROM o_herdr_debt_effects e JOIN o_herdr_debt_attempts a ON a.id=e.attempt_id WHERE e.id=$1").bind(r.effect_id).fetch_one(&f.pool).await.unwrap();
    assert_eq!(state, "ready");
    assert_eq!(proof["attempt"], id.to_string());
    assert_eq!(proof["payload_sha"], r.payload_sha);
    assert!(
        acquire(&f, &grant).await.is_err(),
        "spent attachment cannot be revived"
    );
    f.runtime.active.store(true, Ordering::Release);
    let retry = f.grant().await;
    assert_eq!(debt(&retry).debt_recovery.effect_id, r.effect_id);
    acquire(&f, &retry)
        .await
        .unwrap()
        .run(async {
            count.fetch_add(1, Ordering::SeqCst);
        })
        .await
        .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let next: Uuid = sqlx::query_scalar("SELECT attempt_id FROM o_herdr_debt_effects WHERE id=$1")
        .bind(r.effect_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_ne!(next, id, "proof never resets a different attempt");
    f.finish().await;
}
#[tokio::test]
async fn debt_recovery_receipt_checks_exact_piece_and_survives_an_owner_transfer_pg() {
    let f = Frame::new().await;
    let grant = f.grant().await;
    let r = &debt(&grant).debt_recovery;
    acquire(&f, &grant)
        .await
        .unwrap()
        .run(async {})
        .await
        .unwrap();
    let (piece, confirmation) = posted(r);
    let mut wrong = piece.clone();
    wrong.payload = "different body".into();
    assert!(
        receipt::confirm(&f.pool, r, 0, &wrong, &confirmation)
            .await
            .is_err()
    );
    let mut tx = f.pool.begin().await.unwrap();
    let id = OwnerIdentity::new("claude", "7651201");
    crate::db::channel_custody_lock::lock(&mut tx, &id)
        .await
        .unwrap();
    transfer_owner_in_tx(
        &mut tx,
        &id,
        "returned-home",
        f.runtime.binding.revision,
        "successor",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    receipt::confirm(&f.pool, r, 0, &piece, &confirmation)
        .await
        .unwrap();
    let state: String = sqlx::query_scalar("SELECT state FROM o_herdr_debt_effects WHERE id=$1")
        .bind(r.effect_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(state, "confirmed");
    f.finish().await;
}
#[tokio::test]
async fn debt_recovery_wire_roundtrip_cannot_restore_live_authority_pg() {
    let f = Frame::new().await;
    let grant = f.grant().await;
    let wire = serde_json::to_value(&grant).unwrap();
    let restored: super::super::Artifact = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(restored, grant);
    assert!(
        acquire(&f, &restored).await.is_err(),
        "serialized tokens have no live attachment"
    );
    let mut mixed = wire.clone();
    mixed["origin"] = serde_json::to_value(&f.original).unwrap();
    mixed["payload_hash"] = Value::String(hash("retained answer"));
    assert!(serde_json::from_value::<super::super::Artifact>(mixed).is_err());
    let mut future = wire;
    future["debt_recovery"]["version"] = serde_json::json!(2);
    let unsupported: super::super::Artifact = serde_json::from_value(future).unwrap();
    assert!(acquire(&f, &unsupported).await.is_err());
    f.finish().await;
}

#[tokio::test]
async fn debt_recovery_no_effect_witness_rejects_an_already_started_attempt_pg() {
    let f = Frame::new().await;
    let grant = f.grant().await;
    let mut guard = acquire(&f, &grant).await.unwrap();
    let attempt = guard.1.as_mut().unwrap();
    attempt.started = true;
    assert!(no_request(guard.0.take().unwrap(), attempt).await.is_err());
    let state: String = sqlx::query_scalar("SELECT state FROM o_herdr_debt_effects WHERE id=$1")
        .bind(debt(&grant).debt_recovery.effect_id)
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(state, "reserved");
    f.finish().await;
}

#[tokio::test]
async fn debt_recovery_scope_snapshot_and_payload_are_immutable_pg() {
    let f = Frame::new().await;
    for field in [
        "provider", "channel", "input", "snapshot", "effect", "store", "bot",
    ] {
        let mut grant = f.grant().await;
        let r = &mut debt_mut(&mut grant).debt_recovery;
        match field {
            "provider" => r.provider = "codex".into(),
            "channel" => r.channel = "7651202".into(),
            "input" => r.input_key = hash("other input"),
            "snapshot" => r.snapshot_sha = hash("other snapshot"),
            "effect" => r.effect_id = Uuid::new_v4(),
            "store" => r.authority.store_sha = hash("other store"),
            "bot" => r.authority.bot_id = 43,
            other => panic!("unexpected field {other}"),
        }
        assert!(acquire(&f, &grant).await.is_err(), "changed {field}");
    }
    let grant = f.grant().await;
    assert!(
        f.scope
            .acquire("7651201", "rewritten body", Some(&grant))
            .await
            .is_err()
    );
    let wire = serde_json::to_value(f.original.seal("retained answer")).unwrap();
    assert_eq!(
        wire,
        serde_json::json!({"origin":f.original,"payload_hash":hash("retained answer")})
    );
    assert!(
        serde_json::from_value::<super::super::OriginalArtifact>(
            serde_json::to_value(&grant).unwrap()
        )
        .is_err(),
        "old strict reader rejects recovery rather than treating it as native"
    );
    f.finish().await;
}

#[tokio::test]
async fn debt_recovery_shared_reservation_has_one_concurrent_winner_pg() {
    let f = Frame::new().await;
    let first = f.grant().await;
    let second = f.grant().await;
    let posts = AtomicUsize::new(0);
    async fn send(f: &Frame, grant: &super::super::Artifact, posts: &AtomicUsize) {
        if let Ok(guard) = acquire(f, grant).await {
            guard
                .run(async {
                    posts.fetch_add(1, Ordering::SeqCst);
                })
                .await
                .unwrap();
        }
    }
    tokio::join!(send(&f, &first, &posts), send(&f, &second, &posts));
    assert_eq!(
        posts.load(Ordering::SeqCst),
        1,
        "one shared reservation across distinct grants"
    );
    assert!(
        acquire(&f, &first.clone()).await.is_err(),
        "clones share one-use authority"
    );
    assert!(acquire(&f, &second.clone()).await.is_err());
    f.finish().await;
}

#[tokio::test]
async fn debt_recovery_live_acquisition_refusal_keeps_its_exact_unpolled_proof_pg() {
    let f = Frame::new().await;
    let grant = f.grant().await;
    let a = debt(&grant);
    let r = &a.debt_recovery;
    let attachment = a.attachment.as_ref().unwrap();
    attachment.claim(r).unwrap();
    let id = OwnerIdentity::new("claude", "7651201");
    let mut tx = f.pool.begin().await.unwrap();
    crate::db::channel_custody_lock::lock(&mut tx, &id)
        .await
        .unwrap();
    let reserved = registry::reserve(&mut tx, r).await.unwrap();
    tx.commit().await.unwrap();
    let owning = Attempt {
        reference: r.clone(),
        attachment: attachment.clone(),
        id: reserved,
        started: false,
    };
    let mut tx = f.pool.begin().await.unwrap();
    crate::db::channel_custody_lock::lock(&mut tx, &id)
        .await
        .unwrap();
    transfer_owner_in_tx(
        &mut tx,
        &id,
        "returned-home",
        f.runtime.binding.revision,
        "successor",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert!(guarded(&f.scope, owning).await.is_err());
    let (state,witness):(String,Option<Value>)=sqlx::query_as("SELECT e.state,a.no_effect_witness FROM o_herdr_debt_effects e JOIN o_herdr_debt_attempts a ON a.id=e.attempt_id WHERE e.id=$1").bind(r.effect_id).fetch_one(&f.pool).await.unwrap();
    assert_eq!(
        state, "ready",
        "a live unpolled acquisition does not lose its proof on refusal"
    );
    let witness = witness.expect("live unpolled refusal retains its witness");
    assert_eq!(witness["attempt"], reserved.to_string());
    assert_eq!(witness["effect"], r.effect_id.to_string());
    f.finish().await;
}
