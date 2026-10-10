use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox::{self, FailedPreAcceptSweepOutcome, InsertPendingPayload};
use sqlx::{Executor, Postgres};
use std::time::Duration;

fn is_replay_fence_refusal(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|error| error.constraint())
        == Some("replay_disposition_fence")
}

async fn setup() -> (TestPostgresDb, PgPool) {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate_with_max_connections(4).await;
    apply_test_mutant(&pool).await;
    let version: String = sqlx::query_scalar("SELECT version()")
        .fetch_one(&pool)
        .await
        .expect("real PostgreSQL must answer before replay fence assertions");
    eprintln!("replay disposition real-PG fixture: {version}");
    (fixture, pool)
}

// Mutants exist only inside disposable test databases; production has no environment bypass.
pub(crate) async fn apply_test_mutant(pool: &PgPool) {
    let sql = match std::env::var("ADK_REPLAY_FENCE_MUTANT").ok().as_deref() {
        None | Some("") => return,
        Some("source-fence") => {
            "CREATE OR REPLACE FUNCTION replay_sources_blocked(
            p_provider TEXT, p_channel TEXT, p_sources TEXT[], p_except BIGINT DEFAULT NULL)
            RETURNS BOOLEAN LANGUAGE sql STABLE AS $$ SELECT FALSE $$"
        }
        Some("disposition-fence") => {
            "CREATE OR REPLACE FUNCTION replay_disposition_blocks_rerun(
            disposition TEXT) RETURNS BOOLEAN LANGUAGE sql IMMUTABLE AS $$ SELECT FALSE $$"
        }
        Some(value) => panic!("unrecognized replay fence test mutant: {value}"),
    };
    sqlx::raw_sql(sql)
        .execute(pool)
        .await
        .expect("install replay fence test mutant");
}

async fn finish(fixture: TestPostgresDb, pool: PgPool) {
    pool.close().await;
    fixture.drop().await;
}

async fn receipt_on<'e, E: Executor<'e, Database = Postgres>>(
    executor: E,
    channel: &str,
    provider: &str,
    sources: &[&str],
    disposition: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO intake_outbox (
            target_instance_id, forwarded_by_instance_id, channel_id, user_msg_id,
            request_owner_id, user_text, turn_kind, agent_id, provider, status,
            replay_only, replay_disposition, replay_source_message_ids,
            replay_episode_nonce, replay_owner_incarnation, replay_request_hash,
            replay_request_key, replay_hold_reason, replay_preserved)
         VALUES ('worker-a', 'leader', $1, $2, 'user', 'original merged request',
                 'foreground', 'agent', $3, 'unknown', TRUE, $4, $5, 'episode-1',
                 'incarnation-1', 'hash', $6, 'activity or incomplete observation',
                 '{\"partial_body\":\"retained result\",\"delivery_owner\":\"owner\"}'::jsonb)
         RETURNING id",
    )
    .bind(channel)
    .bind(sources[sources.len() - 1])
    .bind(provider)
    .bind(disposition)
    .bind(sources)
    .bind(uuid::Uuid::new_v4().to_string())
    .fetch_one(executor)
    .await
}

async fn legacy_on<'e, E: Executor<'e, Database = Postgres>>(
    executor: E,
    channel: &str,
    provider: &str,
    message: &str,
    status: &str,
) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO intake_outbox (target_instance_id, forwarded_by_instance_id,
            channel_id, user_msg_id, request_owner_id, user_text, turn_kind,
            agent_id, provider, status)
         VALUES ('worker-a', 'leader', $1, $2, 'user', 'original request',
                 'foreground', 'agent', $3, $4) RETURNING id",
    )
    .bind(channel)
    .bind(message)
    .bind(provider)
    .bind(status)
    .fetch_one(executor)
    .await
}

fn assert_fenced<T: std::fmt::Debug>(result: Result<T, sqlx::Error>, operation: &str) {
    let error = result.expect_err(operation);
    assert!(is_replay_fence_refusal(&error), "{operation}: {error}");
}

async fn dispatch(pool: &PgPool, id: &str, receipt: i64, card: Option<&str>) {
    sqlx::query(
        "INSERT INTO task_dispatches (id, status, replay_receipt_id, kanban_card_id,
                                     result, claim_owner, claimed_at, claim_expires_at)
         VALUES ($1, 'dispatched', $2, $3, 'preserved output', 'old-node',
                 NOW() - INTERVAL '2 hours', NOW() - INTERVAL '1 hour')",
    )
    .bind(id)
    .bind(receipt)
    .bind(card)
    .execute(pool)
    .await
    .expect("seed dispatch projection");
}

fn payload(channel: &str, message: &str) -> InsertPendingPayload {
    InsertPendingPayload {
        target_instance_id: "worker-a".into(),
        forwarded_by_instance_id: "leader".into(),
        required_labels: serde_json::json!([]),
        execution_requirements: serde_json::json!({}),
        attachment_refs: serde_json::json!([]),
        channel_id: channel.into(),
        user_msg_id: message.into(),
        request_owner_id: "user".into(),
        request_owner_name: None,
        user_text: "original request".into(),
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: None,
        turn_kind: "foreground".into(),
        merge_consecutive: false,
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        preserve_on_cancel: false,
        agent_id: "agent".into(),
        provider: "claude".into(),
        home_epoch: None,
    }
}

#[tokio::test]
async fn absorbed_sources_and_all_blocked_states_are_provider_and_channel_scoped_pg() {
    let (fixture, pool) = setup().await;
    for state in [
        "started_unclassified",
        "startup_failed_no_effect",
        "withheld",
    ] {
        let channel = format!("blocked-{state}");
        let id = receipt_on(&pool, &channel, " CLAUDE ", &["earlier", "last"], state)
            .await
            .expect("seed canonical authority");
        assert_eq!(
            receipt_disposition(&pool, id).await.unwrap().as_deref(),
            Some(state)
        );
        let sources = ["earlier", "last", "new"].map(String::from);
        assert_eq!(
            unblocked_sources(&pool, "claude", &channel, &sources)
                .await
                .unwrap(),
            ["new"]
        );
        assert_eq!(
            unblocked_sources(&pool, "codex", &channel, &sources)
                .await
                .unwrap(),
            sources
        );
        assert_eq!(
            unblocked_sources(&pool, "claude", "other-channel", &sources)
                .await
                .unwrap(),
            sources
        );
        assert_fenced(
            legacy_on(&pool, &channel, "claude", "earlier", "pending").await,
            "absorbed first source must not forward again",
        );
        assert_fenced(
            legacy_on(&pool, &channel, "claude", "last", "pending").await,
            "representative last source must not forward again",
        );
        assert_fenced(
            sqlx::query("DELETE FROM intake_outbox WHERE id=$1")
                .bind(id)
                .execute(&pool)
                .await,
            "authority must survive cleanup",
        );
        legacy_on(
            &pool,
            &format!("other-{state}"),
            "claude",
            "earlier",
            "unknown",
        )
        .await
        .expect("same spelling in another logical channel is separate");
        legacy_on(&pool, &channel, "codex", "earlier", "unknown")
            .await
            .expect("same source under another provider is separate");
        let preserved: (String, serde_json::Value) =
            sqlx::query_as("SELECT user_text, replay_preserved FROM intake_outbox WHERE id=$1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(preserved.0, "original merged request");
        assert_eq!(preserved.1["partial_body"], "retained result");
    }
    finish(fixture, pool).await;
}

#[tokio::test]
async fn receipt_transitions_old_writers_and_delivery_settlement_preserve_authority_pg() {
    let (fixture, pool) = setup().await;
    let id = receipt_on(
        &pool,
        "transitions",
        "claude",
        &["source"],
        "registered_not_started",
    )
    .await
    .expect("registered receipt");
    dispatch(&pool, "d-transitions", id, None).await;
    sqlx::query("UPDATE intake_outbox SET replay_disposition='started_unclassified' WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .expect("first start");
    for sql in [
        "UPDATE intake_outbox SET replay_disposition=NULL WHERE id=$1",
        "UPDATE intake_outbox SET replay_disposition='registered_not_started' WHERE id=$1",
        "UPDATE intake_outbox SET replay_episode_nonce='stale-episode' WHERE id=$1",
        "UPDATE intake_outbox SET channel_id='other' WHERE id=$1",
        "UPDATE intake_outbox SET replay_source_message_ids=ARRAY['other'] WHERE id=$1",
        "UPDATE intake_outbox SET user_text='overwritten' WHERE id=$1",
    ] {
        assert_fenced(sqlx::query(sql).bind(id).execute(&pool).await, sql);
    }
    sqlx::query(
        "UPDATE intake_outbox SET replay_disposition='startup_failed_no_effect' WHERE id=$1",
    )
    .bind(id)
    .execute(&pool)
    .await
    .expect("live producer may classify a no-effect attempt");
    assert_fenced(
        sqlx::query(
            "UPDATE intake_outbox SET replay_disposition='started_unclassified' WHERE id=$1",
        )
        .bind(id)
        .execute(&pool)
        .await,
        "same episode cannot restart",
    );
    sqlx::query("UPDATE intake_outbox SET replay_disposition='withheld' WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .expect("lost permit holds request");
    for sql in [
        "UPDATE intake_outbox SET replay_disposition='classified_normal' WHERE id=$1",
        "UPDATE intake_outbox SET replay_preserved=NULL WHERE id=$1",
        "UPDATE intake_outbox SET replay_preserved='{}'::jsonb WHERE id=$1",
        "UPDATE intake_outbox SET replay_hold_reason=NULL WHERE id=$1",
        "UPDATE intake_outbox SET replay_hold_reason='replacement' WHERE id=$1",
    ] {
        assert_fenced(sqlx::query(sql).bind(id).execute(&pool).await, sql);
    }
    sqlx::query("UPDATE task_dispatches SET context='old writer replacement', replay_disposition=NULL WHERE id='d-transitions'")
        .execute(&pool).await.expect("ordinary metadata writer retains typed projection");
    let projected: String = sqlx::query_scalar(
        "SELECT replay_disposition FROM task_dispatches WHERE id='d-transitions'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(projected, "withheld");
    let error = crate::db::intake_outbox_force_fail::force_fail_and_retry_as_new(
        &pool,
        id,
        "operator retry",
    )
    .await
    .expect_err("replay-only receipt cannot parent a delivery retry");
    assert!(
        matches!(error, crate::db::intake_outbox_force_fail::ForceFailError::Db(ref e) if is_replay_fence_refusal(e))
    );

    // An ordinary delivery receipt may settle even while its execution is withheld.
    let normal = legacy_on(&pool, "delivery", "claude", "delivery-source", "spawned")
        .await
        .unwrap();
    sqlx::query("UPDATE intake_outbox SET replay_disposition='withheld', replay_source_message_ids=ARRAY[user_msg_id] WHERE id=$1")
        .bind(normal).execute(&pool).await.expect("test seeds held normal receipt");
    let mut connection = pool.acquire().await.unwrap();
    assert!(
        crate::db::intake_outbox_delivery_proof::settle_intake_done_from_receipt(
            &mut connection,
            normal,
            crate::db::intake_outbox_delivery_proof::IntakeSettlementSource::Committed
        )
        .await
        .unwrap()
    );
    drop(connection);
    assert_eq!(
        receipt_disposition(&pool, normal).await.unwrap().as_deref(),
        Some("withheld")
    );
    finish(fixture, pool).await;
}

#[path = "replay_disposition_tests/compatibility_tests.rs"]
mod compatibility_tests;
#[path = "replay_disposition_tests/concurrency_tests.rs"]
mod concurrency_tests;
#[path = "replay_disposition_tests/consumer_tests.rs"]
mod consumer_tests;
