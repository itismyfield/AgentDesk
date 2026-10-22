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
            (
                &crate::services::session_host::policy::durable::key(&consent.key).unwrap(),
                &consent.nonce
            )
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
        (
            &crate::services::session_host::policy::durable::key(&consent.key).unwrap(),
            &consent.nonce,
        ),
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
        (
            &crate::services::session_host::policy::durable::key(&consent.key).unwrap(),
            &consent.nonce,
        ),
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
        ("891206", "claim"),
        ("891207", "generation"),
        ("891208", "consent"),
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
            "claim" => {
                sqlx::query("UPDATE intake_outbox SET claim_owner='different-claim' WHERE id=$1")
                    .bind(row.id)
                    .execute(&pool)
                    .await
                    .unwrap();
            }
            "generation" => {
                sqlx::query(
                    "UPDATE intake_outbox SET owner_generation=owner_generation+1 WHERE id=$1",
                )
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
        let mut consent_key =
            crate::services::session_host::policy::durable::key(&consent.key).unwrap();
        if change == "consent" {
            consent_key = "0".repeat(64);
        }
        assert!(
            release(
                &pool,
                row.id,
                "claude",
                intent.channel,
                source,
                intent.fifo,
                (&consent_key, &consent.nonce)
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
    store: &ConsentStore,
    expected: &crate::services::session_host::policy::consent::Consent,
    intent: &IntentRef,
) -> Result<(), String> {
    super::hold_before_effects(
        pool,
        crate::services::cluster::intake_worker::BeforeEffects::for_test(row),
        store,
        expected,
        intent,
    )
    .await
}

#[tokio::test]
async fn consent_handoff_first_release_after_regeneration_uses_stable_consent_pg() {
    use crate::services::session_host::policy::{
        consent::{ApplyResult, State},
        durable,
    };
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let (row, _root, store, first, intent) = setup(&pool, "891209").await;
    hold_before_effects(&pool, &row, &store, &first, &intent)
        .await
        .unwrap();
    let denied = store.decide(&first, first.owner, false, 101).unwrap();
    assert_eq!(denied.state, State::Denied);
    let second = store
        .retry(
            Question {
                key: denied.key.clone(),
                preview: denied.preview.clone(),
                owner: denied.owner,
                bot: denied.bot,
                provider: denied.provider.clone(),
                channel: denied.channel,
                message: denied.message,
                intent: intent.clone(),
            },
            102,
        )
        .unwrap();
    assert_ne!(second.nonce, first.nonce);
    let accepted = store.decide(&second, second.owner, true, 103).unwrap();
    let applied = store
        .record_apply(
            &accepted,
            ApplyResult {
                before: Some([1; 32]),
                after: Some([2; 32]),
                backup: None,
                detail: None,
            },
        )
        .unwrap();
    assert_eq!(applied.state, State::Applied);
    let key = durable::key(&applied.key).unwrap();
    let outcome = release(
        &pool,
        row.id,
        &row.provider,
        intent.channel,
        intent.source_message,
        intent.fifo,
        (&key, &applied.nonce),
    )
    .await;
    assert!(
        outcome.is_ok(),
        "consent_handoff_regenerated_first_release_contract: {outcome:?}"
    );
    let first_release = super::super::owner_fence_pg_tests::snapshot(&pool, row.id).await;
    assert_eq!(
        first_release["status"], "pending",
        "consent_handoff_regenerated_original_pending_contract"
    );
    let receipt = marker(first_release["last_error"].as_str()).unwrap();
    assert_eq!(
        receipt.nonce, applied.nonce,
        "consent_handoff_nonce_is_receipt_only_contract"
    );
    assert_eq!(receipt.consent, key);
    assert!(
        !super::super::mark_accepted(&pool, row.id, "token")
            .await
            .unwrap()
    );
    sqlx::query("UPDATE intake_outbox SET status='spawned',claim_owner='new-claim' WHERE id=$1")
        .bind(row.id)
        .execute(&pool)
        .await
        .unwrap();
    let reclaimed = super::super::owner_fence_pg_tests::snapshot(&pool, row.id).await;
    for nonce in [&first.nonce, &applied.nonce, "later-generation"] {
        assert!(
            release(
                &pool,
                row.id,
                &row.provider,
                intent.channel,
                intent.source_message,
                intent.fifo,
                (&key, nonce)
            )
            .await
            .is_ok(),
            "consent_handoff_regenerated_replay_idempotent_contract"
        );
        assert_eq!(
            super::super::owner_fence_pg_tests::snapshot(&pool, row.id).await,
            reclaimed,
            "consent_handoff_one_row_one_release_contract"
        );
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn consent_handoff_stale_before_effects_proof_never_changes_row_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (channel, mutation) in [
        ("891210", "payload"),
        ("891211", "claim"),
        ("891212", "generation"),
    ] {
        let (row, _root, store, consent, intent) = setup(&pool, channel).await;
        let sql = match mutation {
            "payload" => "UPDATE intake_outbox SET user_text='changed' WHERE id=$1",
            "claim" => "UPDATE intake_outbox SET claim_owner='changed' WHERE id=$1",
            _ => "UPDATE intake_outbox SET owner_generation=owner_generation+1 WHERE id=$1",
        };
        sqlx::query(sql).bind(row.id).execute(&pool).await.unwrap();
        let before = super::super::owner_fence_pg_tests::snapshot(&pool, row.id).await;
        assert!(
            hold_before_effects(&pool, &row, &store, &consent, &intent)
                .await
                .is_err(),
            "consent_handoff_stale_before_effects_authority_contract: {mutation}"
        );
        assert_eq!(
            super::super::owner_fence_pg_tests::snapshot(&pool, row.id).await,
            before
        );
    }
    pool.close().await;
    db.drop().await;
}
