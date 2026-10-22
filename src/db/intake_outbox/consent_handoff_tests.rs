use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::session_host::ServerWitness;
use crate::services::session_host::policy::consent::{
    ConsentKey, ConsentStore, IntentRef, Preview, Question,
};

pub(crate) async fn setup(
    pool: &PgPool,
    channel: &str,
) -> (
    IntakeOutboxRow,
    tempfile::TempDir,
    ConsentStore,
    crate::services::session_host::policy::consent::Consent,
    IntentRef,
) {
    let id = super::super::owner_fence_pg_tests::seed(pool, channel, "spawned", true).await;
    let row = sqlx::query_as::<_, IntakeOutboxRow>("SELECT * FROM intake_outbox WHERE id=$1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let store = ConsentStore::new(Some(temp.path().join("consents")));
    let intent = IntentRef {
        store: "intake_outbox".into(),
        id: id.to_string(),
        provider: row.provider.clone(),
        channel: channel.parse().unwrap(),
        source_message: channel.parse().unwrap(),
        fifo: row.attempt_no as u64,
    };
    let mut witness = ServerWitness::for_test(7);
    witness.socket = "/fixture/socket".into();
    let consent = store
        .hold(
            Question {
                key: ConsentKey {
                    node: "one".into(),
                    user: 501,
                    socket: "/fixture/socket".into(),
                    named_session: "agentdesk".into(),
                    config: "/fixture/config.toml".into(),
                    target: "/fixture/target.toml".into(),
                },
                preview: Preview {
                    revision: 1,
                    witness,
                    target_hash: [1; 32],
                    chain_hash: [2; 32],
                    checker_hash: [3; 32],
                },
                owner: 1,
                bot: 2,
                provider: "claude".into(),
                channel: intent.channel,
                message: 4,
                intent: intent.clone(),
            },
            100,
        )
        .unwrap()
        .into_consent();
    (row, temp, store, consent, intent)
}

#[tokio::test]
async fn consent_handoff_refuses_unmarked_post_accept_rows_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (row, _temp, _store, consent, intent) = setup(&pool, "891201").await;
    let before = super::super::owner_fence_pg_tests::snapshot(&pool, row.id).await;
    assert!(
        release(
            &pool,
            row.id,
            "claude",
            intent.channel,
            intent.source_message,
            intent.fifo,
            &consent.nonce
        )
        .await
        .is_err(),
        "consent_handoff_unmarked_spawned_never_rearms_contract"
    );
    assert_eq!(
        super::super::owner_fence_pg_tests::snapshot(&pool, row.id).await,
        before
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn consent_handoff_releases_original_row_once_across_nonce_and_reclaim_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (row, _temp, store, consent, intent) = setup(&pool, "891202").await;
    hold_before_effects(&pool, &row, &store, &consent, &intent)
        .await
        .unwrap();
    release(
        &pool,
        row.id,
        "claude",
        intent.channel,
        intent.source_message,
        intent.fifo,
        &consent.nonce,
    )
    .await
    .unwrap();
    let released = super::super::owner_fence_pg_tests::snapshot(&pool, row.id).await;
    assert_eq!(
        released["status"], "pending",
        "consent_handoff_original_row_becomes_pending_contract"
    );
    assert_eq!(released["attempt_no"], row.attempt_no);
    assert_eq!(released["user_text"], row.user_text);
    sqlx::query("UPDATE intake_outbox SET status='spawned', claim_owner='token' WHERE id=$1")
        .bind(row.id)
        .execute(&pool)
        .await
        .unwrap();
    let reclaimed = super::super::owner_fence_pg_tests::snapshot(&pool, row.id).await;
    release(
        &pool,
        row.id,
        "claude",
        intent.channel,
        intent.source_message,
        intent.fifo,
        &consent.nonce,
    )
    .await
    .unwrap();
    assert_eq!(
        super::super::owner_fence_pg_tests::snapshot(&pool, row.id).await,
        reclaimed,
        "consent_handoff_replay_never_rearms_a_reclaimed_payload_contract"
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM intake_outbox WHERE channel_id=$1")
        .bind(&row.channel_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        count, 1,
        "consent_handoff_never_mints_new_attempt_or_row_contract"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn consent_handoff_requires_exact_payload_source_and_live_owner_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (channel, change) in [
        ("891203", "payload"),
        ("891204", "owner"),
        ("891205", "source"),
    ] {
        let (row, _temp, store, consent, intent) = setup(&pool, channel).await;
        hold_before_effects(&pool, &row, &store, &consent, &intent)
            .await
            .unwrap();
        match change {
            "payload" => {
                sqlx::query("UPDATE intake_outbox SET user_text='changed' WHERE id=$1")
                    .bind(row.id)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            "owner" => {
                super::super::owner_fence_pg_tests::move_owner(&pool, channel, "other-worker")
                    .await;
            }
            _ => {}
        }
        let before = super::super::owner_fence_pg_tests::snapshot(&pool, row.id).await;
        let source = intent.source_message + u64::from(change == "source");
        assert!(
            release(
                &pool,
                row.id,
                "claude",
                intent.channel,
                source,
                intent.fifo,
                &consent.nonce
            )
            .await
            .is_err(),
            "consent_handoff_exact_before_effects_proof_contract: {change}"
        );
        assert_eq!(
            super::super::owner_fence_pg_tests::snapshot(&pool, row.id).await,
            before
        );
    }
    pool.close().await;
    db.drop().await;
}

pub(crate) async fn hold_before_effects(
    pool: &PgPool,
    row: &IntakeOutboxRow,
    store: &crate::services::session_host::policy::consent::ConsentStore,
    expected: &crate::services::session_host::policy::consent::Consent,
    intent: &crate::services::session_host::policy::consent::IntentRef,
) -> Result<(), String> {
    use crate::services::session_host::policy::{consent::State, durable};
    let current = store
        .load(&expected.key)?
        .ok_or("consent custody missing")?;
    if current != *expected
        || current.state != State::Pending
        || !current.unresolved_intents().contains(intent)
        || intent.store != "intake_outbox"
        || intent.id != row.id.to_string()
        || intent.provider != row.provider
        || intent.channel.to_string() != row.channel_id
        || intent.source_message.to_string() != row.user_msg_id
        || intent.fifo != row.attempt_no as u64
        || row.status != crate::db::intake_outbox_status::IntakeOutboxStatus::Spawned
        || row.claim_owner.is_none()
    {
        return Err(
            "before-effects consent custody does not identify this exact worker payload".into(),
        );
    }
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    let identity = OwnerIdentity::new(&row.provider, &row.channel_id);
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(identity.advisory_key())
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    let current_row: IntakeOutboxRow =
        sqlx::query_as("SELECT * FROM intake_outbox WHERE id=$1 FOR UPDATE")
            .bind(row.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;
    if payload(&current_row)? != payload(row)? {
        return Err("worker payload changed before custody handoff".into());
    }
    let marker = Marker {
        nonce: current.nonce,
        consent: durable::key(&current.key)?,
        row: row.id,
        payload: payload(row)?,
        claim: row.claim_owner.clone(),
        owner: row.owner_instance_id.clone(),
        generation: row.owner_generation,
        released: false,
    };
    let encoded = format!(
        "{PREFIX}{}",
        serde_json::to_string(&marker).map_err(|e| e.to_string())?
    );
    let changed = sqlx::query("UPDATE intake_outbox SET last_error=$2 WHERE id=$1 AND status='spawned' AND claim_owner=$3 AND owner_instance_id IS NOT DISTINCT FROM $4 AND owner_generation IS NOT DISTINCT FROM $5 AND last_error IS NULL")
        .bind(row.id).bind(encoded).bind(&row.claim_owner).bind(&row.owner_instance_id).bind(row.owner_generation)
        .execute(&mut *tx).await.map_err(|e| e.to_string())?.rows_affected();
    if changed == 1 {
        tx.commit().await.map_err(|e| e.to_string())
    } else {
        Err("worker claim changed before consent custody handoff".into())
    }
}
