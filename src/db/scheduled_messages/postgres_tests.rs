use super::*;
use chrono::Duration;
use sqlx::Row;

async fn create_test_pool(
    prefix: &str,
    label: &str,
) -> (
    crate::dispatch::test_support::DispatchPostgresTestDb,
    PgPool,
) {
    let pg_db = crate::dispatch::test_support::DispatchPostgresTestDb::create(prefix, label).await;
    let pool = pg_db.connect_and_migrate_with_max_connections(4).await;
    (pg_db, pool)
}

async fn insert_due_message(pool: &PgPool, delivery_kind: &str) -> ScheduledMessageRow {
    let agent_id = if delivery_kind == KIND_AGENT {
        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id)
             VALUES ('scheduled-test-agent', 'Scheduled Test Agent', '123456789')",
        )
        .execute(pool)
        .await
        .expect("seed scheduled-message agent");
        Some("scheduled-test-agent".to_string())
    } else {
        None
    };

    insert_scheduled_message_pg(
        pool,
        &NewScheduledMessage {
            content: "scheduled test message".to_string(),
            title: None,
            target_channel_id: (delivery_kind == KIND_PUSH).then(|| "123456789".to_string()),
            bot: "announce".to_string(),
            delivery_kind: delivery_kind.to_string(),
            agent_id,
            agent_instruction: None,
            on_agent_failure: "fail".to_string(),
            scheduled_at: Utc::now() - Duration::seconds(1),
            schedule: None,
            timezone: "UTC".to_string(),
            expires_at: None,
            source: "postgres_test".to_string(),
            created_by: Some("postgres_test".to_string()),
            dedupe_key: None,
            context_strategy: "fresh".to_string(),
            context_snapshot_id: None,
            on_context_failure: "fail".to_string(),
        },
    )
    .await
    .expect("insert due scheduled message")
}

async fn claim_one(pool: &PgPool, owner: &str, lease_secs: i64) -> ClaimedFire {
    let mut claims = claim_due_fires_pg(pool, owner, true, 10, lease_secs, Utc::now())
        .await
        .expect("claim due scheduled message");
    assert_eq!(claims.len(), 1, "exactly one definition should be due");
    claims.pop().expect("claimed fire")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_scheduled_message_rearm_rotates_token_and_clears_attempt_fields() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_rearm",
        "scheduled message rearm fencing regression",
    )
    .await;
    let message = insert_due_message(&pool, KIND_PUSH).await;
    let first = claim_one(&pool, "worker-a", 30).await;

    sqlx::query(
        "UPDATE scheduled_message_deliveries
         SET outbox_id = 101,
             turn_id = 'stale-turn',
             launch_committed_at = NOW(),
             turn_started_at = NOW(),
             fallback_outbox_id = 202,
             next_attempt_at = NOW() + INTERVAL '5 minutes',
             error = 'stale attempt error'
         WHERE id = $1",
    )
    .bind(&first.delivery_id)
    .execute(&pool)
    .await
    .expect("seed stale attempt fields");

    assert!(
        interrupt_delivery_and_rewind_pg(
            &pool,
            &first.delivery_id,
            &first.claim_token,
            &message.id,
            first.fire_scheduled_at,
            None,
            "retry this slot",
        )
        .await
        .expect("interrupt first attempt"),
        "the current claim should rewind its parent"
    );

    let second = claim_one(&pool, "worker-b", 45).await;
    assert_eq!(second.delivery_id, first.delivery_id, "slot row is reused");
    assert_ne!(
        second.claim_token, first.claim_token,
        "every rearm needs a fresh fencing token"
    );
    assert_eq!(second.retry_count, 1);

    let row = sqlx::query(
        "SELECT status, claim_owner, claim_token, retry_count,
                outbox_id, turn_id, turn_intent_at, launch_committed_at, turn_started_at,
                fallback_outbox_id, next_attempt_at,
                error, finished_at, lease_expires_at
         FROM scheduled_message_deliveries
         WHERE id = $1",
    )
    .bind(&second.delivery_id)
    .fetch_one(&pool)
    .await
    .expect("read rearmed delivery");
    assert_eq!(row.try_get::<String, _>("status").unwrap(), "running");
    assert_eq!(
        row.try_get::<Option<String>, _>("claim_owner").unwrap(),
        Some("worker-b".to_string())
    );
    assert_eq!(
        row.try_get::<String, _>("claim_token").unwrap(),
        second.claim_token
    );
    assert_eq!(row.try_get::<i32, _>("retry_count").unwrap(), 1);
    assert_eq!(row.try_get::<Option<i64>, _>("outbox_id").unwrap(), None);
    assert_eq!(row.try_get::<Option<String>, _>("turn_id").unwrap(), None);
    assert_eq!(
        row.try_get::<Option<DateTime<Utc>>, _>("turn_intent_at")
            .unwrap(),
        None
    );
    assert_eq!(
        row.try_get::<Option<DateTime<Utc>>, _>("launch_committed_at")
            .unwrap(),
        None
    );
    assert_eq!(
        row.try_get::<Option<DateTime<Utc>>, _>("turn_started_at")
            .unwrap(),
        None
    );
    assert_eq!(
        row.try_get::<Option<i64>, _>("fallback_outbox_id").unwrap(),
        None
    );
    assert_eq!(
        row.try_get::<Option<DateTime<Utc>>, _>("next_attempt_at")
            .unwrap(),
        None
    );
    assert_eq!(row.try_get::<Option<String>, _>("error").unwrap(), None);
    assert_eq!(
        row.try_get::<Option<DateTime<Utc>>, _>("finished_at")
            .unwrap(),
        None
    );
    assert!(
        row.try_get::<Option<DateTime<Utc>>, _>("lease_expires_at")
            .unwrap()
            .is_some(),
        "the replacement attempt must hold a fresh lease"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_scheduled_message_retry_waits_until_next_attempt_at() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_retry_backoff",
        "scheduled message retry backoff claim gate",
    )
    .await;
    let message = insert_due_message(&pool, KIND_AGENT).await;
    let first = claim_one(&pool, "retry-backoff-worker-a", 30).await;
    let retry_at = Utc::now() + Duration::minutes(5);
    assert!(
        interrupt_delivery_and_rewind_pg(
            &pool,
            &first.delivery_id,
            &first.claim_token,
            &message.id,
            first.fire_scheduled_at,
            Some(retry_at),
            "mailbox temporarily busy",
        )
        .await
        .expect("schedule delayed retry")
    );

    let blocked = claim_due_fires_pg(
        &pool,
        "retry-backoff-worker-b",
        true,
        10,
        30,
        retry_at - Duration::seconds(1),
    )
    .await
    .expect("scan before retry deadline");
    assert!(blocked.is_empty());

    let mut ready = claim_due_fires_pg(
        &pool,
        "retry-backoff-worker-c",
        true,
        10,
        30,
        retry_at + Duration::seconds(1),
    )
    .await
    .expect("scan after retry deadline");
    assert_eq!(ready.len(), 1);
    let second = ready.pop().expect("delayed retry should be claimable");
    assert_eq!(second.delivery_id, first.delivery_id);
    assert_eq!(second.retry_count, 1);
    assert_ne!(second.claim_token, first.claim_token);
    let next_attempt_at: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT next_attempt_at FROM scheduled_message_deliveries WHERE id = $1",
    )
    .bind(&second.delivery_id)
    .fetch_one(&pool)
    .await
    .expect("read rearmed retry deadline");
    assert_eq!(next_attempt_at, None);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_scheduled_message_backoff_does_not_block_other_due_rows() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_retry_fairness",
        "scheduled message retry backoff head-of-line regression",
    )
    .await;
    let delayed = insert_due_message(&pool, KIND_AGENT).await;
    sqlx::query(
        "UPDATE scheduled_messages SET scheduled_at = NOW() - INTERVAL '2 minutes' WHERE id = $1",
    )
    .bind(&delayed.id)
    .execute(&pool)
    .await
    .expect("make delayed definition oldest");
    let ready_message = insert_due_message(&pool, KIND_PUSH).await;
    let mut first_claim =
        claim_due_fires_pg(&pool, "retry-fairness-worker-a", true, 1, 30, Utc::now())
            .await
            .expect("claim oldest due definition");
    let first = first_claim.pop().expect("oldest definition should claim");
    assert_eq!(first.message.id, delayed.id);
    assert!(
        interrupt_delivery_and_rewind_pg(
            &pool,
            &first.delivery_id,
            &first.claim_token,
            &delayed.id,
            first.fire_scheduled_at,
            Some(Utc::now() + Duration::minutes(5)),
            "delay oldest definition",
        )
        .await
        .expect("back off oldest definition")
    );

    let claims = claim_due_fires_pg(&pool, "retry-fairness-worker-b", true, 10, 30, Utc::now())
        .await
        .expect("claim around backed-off definition");
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].message.id, ready_message.id);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_scheduled_message_stale_claim_is_fenced_and_current_claim_renews_lease() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_claim_fence",
        "scheduled message stale claim and lease renewal regression",
    )
    .await;
    let message = insert_due_message(&pool, KIND_AGENT).await;
    let first = claim_one(&pool, "worker-a", 20).await;
    assert!(
        interrupt_delivery_and_rewind_pg(
            &pool,
            &first.delivery_id,
            &first.claim_token,
            &message.id,
            first.fire_scheduled_at,
            None,
            "replace worker-a",
        )
        .await
        .expect("interrupt first agent attempt")
    );
    let second = claim_one(&pool, "worker-b", 20).await;

    assert!(
        !record_delivery_agent_turn_intent_pg(
            &pool,
            &message.id,
            &second.delivery_id,
            &first.claim_token,
            "stale-turn",
        )
        .await
        .expect("stale turn intent must be a guarded no-op")
    );
    assert!(
        !finish_delivery_and_finalize_parent_pg(
            &pool,
            &second.delivery_id,
            &first.claim_token,
            DELIVERY_SENT,
            None,
            None,
            None,
            &message.id,
            true,
            STATUS_SENT,
            None,
        )
        .await
        .expect("stale finish must be a guarded no-op")
    );
    assert!(
        !interrupt_delivery_and_rewind_pg(
            &pool,
            &second.delivery_id,
            &first.claim_token,
            &message.id,
            second.fire_scheduled_at,
            None,
            "stale rewind",
        )
        .await
        .expect("stale rewind must be a guarded no-op")
    );

    let parent_status: String =
        sqlx::query_scalar("SELECT status FROM scheduled_messages WHERE id = $1")
            .bind(&message.id)
            .fetch_one(&pool)
            .await
            .expect("read parent after stale writes");
    assert_eq!(parent_status, STATUS_FIRING);
    let (delivery_status, turn_id, claim_token, lease_before): (
        String,
        Option<String>,
        String,
        Option<DateTime<Utc>>,
    ) = sqlx::query_as(
        "SELECT status, turn_id, claim_token, lease_expires_at
         FROM scheduled_message_deliveries
         WHERE id = $1",
    )
    .bind(&second.delivery_id)
    .fetch_one(&pool)
    .await
    .expect("read delivery after stale writes");
    assert_eq!(delivery_status, "running");
    assert_eq!(turn_id, None);
    assert_eq!(claim_token, second.claim_token);
    let lease_before = lease_before.expect("claimed attempt has a lease");

    assert!(
        record_delivery_agent_turn_intent_pg(
            &pool,
            &message.id,
            &second.delivery_id,
            &second.claim_token,
            "current-turn",
        )
        .await
        .expect("current turn intent should update delivery")
    );
    let (turn_id, turn_intent_at, launch_committed_at, turn_started_at, lease_after_intent): (
        Option<String>,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
    ) = sqlx::query_as(
        "SELECT turn_id, turn_intent_at, launch_committed_at,
                turn_started_at, lease_expires_at
         FROM scheduled_message_deliveries
         WHERE id = $1",
    )
    .bind(&second.delivery_id)
    .fetch_one(&pool)
    .await
    .expect("read agent launch intent");
    assert_eq!(turn_id.as_deref(), Some("current-turn"));
    assert!(turn_intent_at.is_some());
    assert_eq!(launch_committed_at, None);
    assert_eq!(turn_started_at, None);
    assert_eq!(lease_after_intent, Some(lease_before));
    assert!(
        list_running_agent_deliveries_pg(&pool, "worker-b", 600, 10)
            .await
            .expect("poll before launch confirmation")
            .is_empty(),
        "an intent-only row must not be polled or have its lease renewed"
    );

    assert!(
        commit_delivery_agent_launch_pg(
            &pool,
            &message.id,
            &second.delivery_id,
            &second.claim_token,
            "current-turn",
            600,
        )
        .await
        .expect("current turn launch should commit")
    );
    let (launch_committed_at, lease_after_commit): (Option<DateTime<Utc>>, Option<DateTime<Utc>>) =
        sqlx::query_as(
            "SELECT launch_committed_at, lease_expires_at
         FROM scheduled_message_deliveries
         WHERE id = $1",
        )
        .bind(&second.delivery_id)
        .fetch_one(&pool)
        .await
        .expect("read committed agent launch");
    assert!(launch_committed_at.is_some());
    assert!(
        lease_after_commit.expect("launch-commit lease") > lease_before,
        "committing the launch must extend the claim lease"
    );

    assert!(
        mark_delivery_agent_turn_started_pg(
            &pool,
            &message.id,
            &second.delivery_id,
            &second.claim_token,
            "current-turn",
            600,
        )
        .await
        .expect("current turn start should confirm delivery")
    );
    let (turn_started_at, lease_after): (Option<DateTime<Utc>>, Option<DateTime<Utc>>) =
        sqlx::query_as(
            "SELECT turn_started_at, lease_expires_at
             FROM scheduled_message_deliveries
             WHERE id = $1",
        )
        .bind(&second.delivery_id)
        .fetch_one(&pool)
        .await
        .expect("read confirmed agent lease");
    assert!(turn_started_at.is_some());
    assert!(
        lease_after.expect("renewed lease") > lease_before,
        "confirming the current turn must keep the claim lease renewed"
    );

    assert!(
        finish_delivery_and_finalize_parent_pg(
            &pool,
            &second.delivery_id,
            &second.claim_token,
            DELIVERY_SENT,
            None,
            None,
            None,
            &message.id,
            true,
            STATUS_SENT,
            None,
        )
        .await
        .expect("current claim should finish delivery")
    );
    let final_parent_status: String =
        sqlx::query_scalar("SELECT status FROM scheduled_messages WHERE id = $1")
            .bind(&message.id)
            .fetch_one(&pool)
            .await
            .expect("read finalized parent");
    let final_delivery_status: String =
        sqlx::query_scalar("SELECT status FROM scheduled_message_deliveries WHERE id = $1")
            .bind(&second.delivery_id)
            .fetch_one(&pool)
            .await
            .expect("read finalized delivery");
    assert_eq!(final_parent_status, STATUS_SENT);
    assert_eq!(final_delivery_status, DELIVERY_SENT);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_running_agent_poll_rotates_before_renewed_rows() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_poll_rotation",
        "scheduled message agent poll lease rotation regression",
    )
    .await;
    let agent_id = "scheduled-poll-agent";
    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id)
         VALUES ($1, 'Scheduled Poll Agent', '123456789')",
    )
    .bind(agent_id)
    .execute(&pool)
    .await
    .expect("seed scheduled-message poll agent");

    for label in ["older", "newer"] {
        insert_scheduled_message_pg(
            &pool,
            &NewScheduledMessage {
                content: format!("scheduled poll {label}"),
                title: None,
                target_channel_id: Some("123456789".to_string()),
                bot: "announce".to_string(),
                delivery_kind: KIND_AGENT.to_string(),
                agent_id: Some(agent_id.to_string()),
                agent_instruction: None,
                on_agent_failure: "fail".to_string(),
                scheduled_at: Utc::now() - Duration::seconds(1),
                schedule: None,
                timezone: "UTC".to_string(),
                expires_at: None,
                source: "postgres_test".to_string(),
                created_by: Some("postgres_test".to_string()),
                dedupe_key: None,
                context_strategy: "fresh".to_string(),
                context_snapshot_id: None,
                on_context_failure: "fail".to_string(),
            },
        )
        .await
        .expect("insert scheduled-message poll definition");
    }

    let claims = claim_due_fires_pg(&pool, "poll-worker", true, 10, 60, Utc::now())
        .await
        .expect("claim scheduled-message poll definitions");
    assert_eq!(claims.len(), 2);
    for (index, claim) in claims.iter().enumerate() {
        let turn_id = format!("poll-turn-{index}");
        assert!(
            record_delivery_agent_turn_intent_pg(
                &pool,
                &claim.message.id,
                &claim.delivery_id,
                &claim.claim_token,
                &turn_id,
            )
            .await
            .expect("record scheduled-message poll turn intent")
        );
        assert!(
            commit_delivery_agent_launch_pg(
                &pool,
                &claim.message.id,
                &claim.delivery_id,
                &claim.claim_token,
                &turn_id,
                60,
            )
            .await
            .expect("commit scheduled-message poll turn launch")
        );
        assert!(
            mark_delivery_agent_turn_started_pg(
                &pool,
                &claim.message.id,
                &claim.delivery_id,
                &claim.claim_token,
                &turn_id,
                60,
            )
            .await
            .expect("record scheduled-message poll turn")
        );
    }

    let competing = list_running_agent_deliveries_pg(&pool, "competing-worker", 600, 10)
        .await
        .expect("poll active deliveries from a competing owner");
    assert!(
        competing.is_empty(),
        "a different owner must not process an active poll lease"
    );

    sqlx::query(
        "UPDATE scheduled_message_deliveries
         SET lease_expires_at = NOW() + INTERVAL '60 seconds',
             created_at = CASE WHEN id = $1
                 THEN NOW() - INTERVAL '2 minutes'
                 ELSE NOW() - INTERVAL '1 minute'
             END
         WHERE id = $1 OR id = $2",
    )
    .bind(&claims[0].delivery_id)
    .bind(&claims[1].delivery_id)
    .execute(&pool)
    .await
    .expect("seed deterministic poll ordering");

    let first = list_running_agent_deliveries_pg(&pool, "poll-worker", 600, 1)
        .await
        .expect("poll first running agent delivery");
    let second = list_running_agent_deliveries_pg(&pool, "poll-worker", 600, 1)
        .await
        .expect("poll second running agent delivery");
    assert_eq!(first.len(), 1);
    assert_eq!(second.len(), 1);
    assert_ne!(
        first[0].delivery_id, second[0].delivery_id,
        "renewing one batch must move it behind still-expiring deliveries"
    );
    let mut observed = vec![first[0].delivery_id.clone(), second[0].delivery_id.clone()];
    observed.sort();
    let mut expected = claims
        .iter()
        .map(|claim| claim.delivery_id.clone())
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(observed, expected);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_expired_started_turn_is_adopted_instead_of_rearmed() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_turn_adoption",
        "scheduled message started turn lease adoption regression",
    )
    .await;
    let message = insert_due_message(&pool, KIND_AGENT).await;
    let fire = claim_one(&pool, "turn-owner", 30).await;
    assert!(
        record_delivery_agent_turn_intent_pg(
            &pool,
            &message.id,
            &fire.delivery_id,
            &fire.claim_token,
            "durable-turn",
        )
        .await
        .expect("record durable turn intent")
    );
    assert!(
        commit_delivery_agent_launch_pg(
            &pool,
            &message.id,
            &fire.delivery_id,
            &fire.claim_token,
            "durable-turn",
            -1,
        )
        .await
        .expect("commit durable turn launch")
    );
    assert!(
        mark_delivery_agent_turn_started_pg(
            &pool,
            &message.id,
            &fire.delivery_id,
            &fire.claim_token,
            "durable-turn",
            -1,
        )
        .await
        .expect("record expired durable turn")
    );
    assert!(
        release_agent_delivery_to_poller_pg(&pool, &fire.delivery_id, &fire.claim_token)
            .await
            .expect("release durable turn to completion poller"),
        "a successfully started turn must become immediately adoptable"
    );

    assert_eq!(
        recover_expired_leases_pg(&pool)
            .await
            .expect("recover expired pre-turn leases"),
        0,
        "a recorded turn must be adopted rather than restarted"
    );
    let parent = get_scheduled_message_pg(&pool, &message.id)
        .await
        .expect("read adopted parent")
        .expect("adopted parent exists");
    assert_eq!(parent.status, STATUS_FIRING);
    assert_eq!(
        parent.in_flight_delivery_id.as_deref(),
        Some(fire.delivery_id.as_str())
    );

    let adopted = list_running_agent_deliveries_pg(&pool, "adopting-worker", 600, 10)
        .await
        .expect("adopt expired durable turn");
    assert_eq!(adopted.len(), 1);
    assert_eq!(adopted[0].delivery_id, fire.delivery_id);
    assert_ne!(
        adopted[0].claim_token, fire.claim_token,
        "lease adoption must fence the stale owner with a new token"
    );
    assert_eq!(adopted[0].turn_id.as_deref(), Some("durable-turn"));
    let renewed_lease: DateTime<Utc> = sqlx::query_scalar(
        "SELECT lease_expires_at
         FROM scheduled_message_deliveries
         WHERE id = $1",
    )
    .bind(&fire.delivery_id)
    .fetch_one(&pool)
    .await
    .expect("read adopted turn lease");
    assert!(renewed_lease > Utc::now());

    assert!(
        !finish_delivery_and_finalize_parent_pg(
            &pool,
            &fire.delivery_id,
            &fire.claim_token,
            DELIVERY_SENT,
            None,
            None,
            None,
            &message.id,
            true,
            STATUS_SENT,
            None,
        )
        .await
        .expect("reject stale adopted-turn owner")
    );
    assert!(
        finish_delivery_and_finalize_parent_pg(
            &pool,
            &fire.delivery_id,
            &adopted[0].claim_token,
            DELIVERY_SENT,
            None,
            None,
            None,
            &message.id,
            true,
            STATUS_SENT,
            None,
        )
        .await
        .expect("finish adopted durable turn")
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_agent_launch_intent_crash_rearms_without_phantom_lease_renewal() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_agent_intent_recovery",
        "scheduled agent pre-launch crash recovery",
    )
    .await;
    let message = insert_due_message(&pool, KIND_AGENT).await;
    let first = claim_one(&pool, "pre-launch-worker", 30).await;
    assert!(
        record_delivery_agent_turn_intent_pg(
            &pool,
            &message.id,
            &first.delivery_id,
            &first.claim_token,
            "never-launched-turn",
        )
        .await
        .expect("record launch intent")
    );
    sqlx::query(
        "UPDATE scheduled_message_deliveries
         SET lease_expires_at = NOW() - INTERVAL '1 second'
         WHERE id = $1",
    )
    .bind(&first.delivery_id)
    .execute(&pool)
    .await
    .expect("expire launch-intent lease");

    assert!(
        list_running_agent_deliveries_pg(&pool, "poller", 600, 10)
            .await
            .expect("poll confirmed agent deliveries")
            .is_empty(),
        "intent-only rows must not be selected for lease renewal"
    );
    assert_eq!(
        recover_expired_leases_pg(&pool)
            .await
            .expect("recover expired launch intent"),
        1
    );

    let retry = claim_one(&pool, "replacement-worker", 30).await;
    assert_eq!(retry.delivery_id, first.delivery_id);
    assert_eq!(retry.retry_count, 1);
    let (turn_id, turn_intent_at, launch_committed_at, turn_started_at): (
        Option<String>,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
    ) = sqlx::query_as(
        "SELECT turn_id, turn_intent_at, launch_committed_at, turn_started_at
         FROM scheduled_message_deliveries
         WHERE id = $1",
    )
    .bind(&retry.delivery_id)
    .fetch_one(&pool)
    .await
    .expect("read replacement launch state");
    assert_eq!(turn_id, None);
    assert_eq!(turn_intent_at, None);
    assert_eq!(launch_committed_at, None);
    assert_eq!(turn_started_at, None);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_old_writer_rearm_after_new_intent_recovery_stays_ambiguous() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_old_rearm_after_intent",
        "scheduled agent rolling old-writer rearm after new intent recovery",
    )
    .await;
    let message = insert_due_message(&pool, KIND_AGENT).await;
    let first = claim_one(&pool, "new-pre-launch-worker", 30).await;
    assert!(
        record_delivery_agent_turn_intent_pg(
            &pool,
            &message.id,
            &first.delivery_id,
            &first.claim_token,
            "new-writer-never-launched-turn",
        )
        .await
        .expect("record new-writer launch intent")
    );
    sqlx::query(
        "UPDATE scheduled_message_deliveries
         SET lease_expires_at = NOW() - INTERVAL '1 second'
         WHERE id = $1",
    )
    .bind(&first.delivery_id)
    .execute(&pool)
    .await
    .expect("expire new-writer launch intent");
    assert_eq!(
        recover_expired_leases_pg(&pool)
            .await
            .expect("recover new-writer pre-launch intent"),
        1
    );

    // Simulate the public pre-0086 ON CONFLICT rearm. That binary knows how
    // to clear turn_id, but cannot name either 0086 launch-phase column.
    let old_claim_token = "old-writer-rearmed-claim";
    sqlx::query(
        "UPDATE scheduled_message_deliveries
         SET status = 'running', claim_owner = 'rolling-old-writer',
             claim_token = $2, lease_expires_at = NOW() - INTERVAL '1 second',
             retry_count = retry_count + 1,
             outbox_id = NULL, turn_id = NULL, fallback_outbox_id = NULL,
             next_attempt_at = NULL, error = NULL, started_at = NOW(),
             finished_at = NULL, updated_at = NOW()
         WHERE id = $1 AND status = 'interrupted'",
    )
    .bind(&first.delivery_id)
    .bind(old_claim_token)
    .execute(&pool)
    .await
    .expect("simulate old-writer slot rearm");
    sqlx::query(
        "UPDATE scheduled_messages
         SET status = 'firing', in_flight_delivery_id = $2, updated_at = NOW()
         WHERE id = $1 AND status = 'scheduled'",
    )
    .bind(&message.id)
    .bind(&first.delivery_id)
    .execute(&pool)
    .await
    .expect("attach old-writer rearmed delivery");

    // Simulate the old writer starting and acknowledging its turn. If the
    // recovered intent marker survived, a new reader would misclassify this
    // launched turn as a safe pre-call row and replacement-rearm it.
    sqlx::query(
        "UPDATE scheduled_message_deliveries
         SET turn_id = 'old-writer-rearmed-turn', turn_started_at = NOW(),
             started_at = NOW(), lease_expires_at = NOW() - INTERVAL '1 second',
             updated_at = NOW()
         WHERE id = $1 AND claim_token = $2 AND status = 'running'",
    )
    .bind(&first.delivery_id)
    .bind(old_claim_token)
    .execute(&pool)
    .await
    .expect("simulate old-writer turn start");

    let (turn_intent_at, launch_committed_at): (Option<DateTime<Utc>>, Option<DateTime<Utc>>) =
        sqlx::query_as(
            "SELECT turn_intent_at, launch_committed_at
         FROM scheduled_message_deliveries
         WHERE id = $1",
        )
        .bind(&first.delivery_id)
        .fetch_one(&pool)
        .await
        .expect("read old-writer marker compatibility state");
    assert_eq!(turn_intent_at, None);
    assert_eq!(launch_committed_at, None);
    assert_eq!(
        recover_expired_leases_pg(&pool)
            .await
            .expect("recover after old-writer launched turn"),
        0,
        "a rearmed old-writer turn must remain ambiguous and fail closed"
    );
    let adopted = list_running_agent_deliveries_pg(&pool, "new-reader", 600, 10)
        .await
        .expect("adopt old-writer rearmed turn");
    assert_eq!(adopted.len(), 1);
    assert_eq!(
        adopted[0].turn_id.as_deref(),
        Some("old-writer-rearmed-turn")
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_launch_commit_without_runtime_ack_is_adopted_fail_closed() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_launch_commit_adoption",
        "scheduled agent ambiguous launch adoption",
    )
    .await;
    let message = insert_due_message(&pool, KIND_AGENT).await;
    let fire = claim_one(&pool, "launching-worker", 30).await;
    assert!(
        record_delivery_agent_turn_intent_pg(
            &pool,
            &message.id,
            &fire.delivery_id,
            &fire.claim_token,
            "ambiguous-launch-turn",
        )
        .await
        .expect("record ambiguous launch intent")
    );
    assert!(
        commit_delivery_agent_launch_pg(
            &pool,
            &message.id,
            &fire.delivery_id,
            &fire.claim_token,
            "ambiguous-launch-turn",
            -1,
        )
        .await
        .expect("commit ambiguous launch")
    );

    assert_eq!(
        recover_expired_leases_pg(&pool)
            .await
            .expect("recover after ambiguous launch"),
        0,
        "a launch commit must never be replacement-rearmed without an idempotent runtime"
    );
    let (launch_committed_at, turn_started_at): (Option<DateTime<Utc>>, Option<DateTime<Utc>>) =
        sqlx::query_as(
            "SELECT launch_committed_at, turn_started_at
         FROM scheduled_message_deliveries
         WHERE id = $1",
        )
        .bind(&fire.delivery_id)
        .fetch_one(&pool)
        .await
        .expect("read ambiguous launch phases");
    let launch_committed_at = launch_committed_at.expect("launch commit timestamp");
    assert_eq!(turn_started_at, None);

    let adopted = list_running_agent_deliveries_pg(&pool, "adopting-worker", 600, 10)
        .await
        .expect("adopt ambiguous launch");
    assert_eq!(adopted.len(), 1);
    assert_eq!(adopted[0].turn_id.as_deref(), Some("ambiguous-launch-turn"));
    assert_eq!(adopted[0].started_at, launch_committed_at);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_rolling_old_writer_turn_is_adopted_and_reported_as_handoff() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_old_writer_adoption",
        "scheduled agent rolling old-writer ambiguity",
    )
    .await;
    let message = insert_due_message(&pool, KIND_AGENT).await;
    let fire = claim_one(&pool, "old-writer", 30).await;
    sqlx::query(
        "UPDATE scheduled_message_deliveries
         SET turn_id = 'old-writer-turn',
             turn_intent_at = NULL,
             launch_committed_at = NULL,
             turn_started_at = NULL,
             lease_expires_at = NOW() - INTERVAL '1 second'
         WHERE id = $1",
    )
    .bind(&fire.delivery_id)
    .execute(&pool)
    .await
    .expect("simulate a turn id written by a rolling old binary");

    assert_eq!(
        recover_expired_leases_pg(&pool)
            .await
            .expect("recover rolling old-writer row"),
        0,
        "a markerless legacy turn id is ambiguous and must never be rearmed"
    );
    let adopted = list_running_agent_deliveries_pg(&pool, "new-reader", 600, 10)
        .await
        .expect("adopt rolling old-writer row");
    assert_eq!(adopted.len(), 1);
    assert_eq!(adopted[0].turn_id.as_deref(), Some("old-writer-turn"));
    assert!(matches!(
        cancel_scheduled_message_pg(&pool, &message.id)
            .await
            .expect("cancel rolling old-writer row"),
        CancelOutcome::Canceled {
            was_firing: true,
            handoff_started: true
        }
    ));

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_runtime_absence_blocks_push_claims_too() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_push_runtime_gate",
        "scheduled push runtime gate",
    )
    .await;
    let message = insert_due_message(&pool, KIND_PUSH).await;

    assert!(
        claim_due_fires_pg(&pool, "no-runtime", false, 10, 30, Utc::now())
            .await
            .expect("scan push without Discord runtime")
            .is_empty()
    );
    let waiting = get_scheduled_message_pg(&pool, &message.id)
        .await
        .expect("read waiting push")
        .expect("waiting push exists");
    assert_eq!(waiting.status, STATUS_SCHEDULED);
    let deliveries: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM scheduled_message_deliveries
         WHERE scheduled_message_id = $1",
    )
    .bind(&message.id)
    .fetch_one(&pool)
    .await
    .expect("count runtime-gated deliveries");
    assert_eq!(deliveries, 0);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_runtime_deferred_backlog_does_not_starve_ready_rows_past_batch() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_runtime_defer_fairness",
        "scheduled runtime defer due-scan fairness",
    )
    .await;
    for _ in 0..12 {
        let deferred = insert_due_message(&pool, KIND_PUSH).await;
        sqlx::query(
            "UPDATE scheduled_messages
             SET scheduled_at = NOW() - INTERVAL '2 hours',
                 runtime_defer_until = NOW() + INTERVAL '1 hour'
             WHERE id = $1",
        )
        .bind(&deferred.id)
        .execute(&pool)
        .await
        .expect("defer an older due definition");
    }
    let ready = insert_due_message(&pool, KIND_PUSH).await;

    let claims = claim_due_fires_pg(&pool, "fair-runtime-worker", true, 1, 30, Utc::now())
        .await
        .expect("claim through deferred backlog");
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].message.id, ready.id);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_definition_patch_clears_runtime_defer_gate() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_runtime_defer_patch",
        "scheduled runtime defer operator patch wakeup",
    )
    .await;
    let message = insert_due_message(&pool, KIND_PUSH).await;
    sqlx::query(
        "UPDATE scheduled_messages
         SET runtime_defer_until = NOW() + INTERVAL '1 hour'
         WHERE id = $1",
    )
    .bind(&message.id)
    .execute(&pool)
    .await
    .expect("defer definition before operator patch");

    let updated = update_scheduled_message_pg(
        &pool,
        &message.id,
        &ScheduledMessagePatch {
            content: Some("operator-adjusted payload".to_string()),
            ..ScheduledMessagePatch::default()
        },
    )
    .await
    .expect("patch runtime-deferred definition")
    .expect("runtime-deferred definition remains editable");
    assert_eq!(updated.content, "operator-adjusted payload");
    let defer_until: Option<DateTime<Utc>> =
        sqlx::query_scalar("SELECT runtime_defer_until FROM scheduled_messages WHERE id = $1")
            .bind(&message.id)
            .fetch_one(&pool)
            .await
            .expect("read runtime defer after patch");
    assert_eq!(defer_until, None);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_cancel_reports_committed_agent_handoff_not_intent() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_agent_intent_cancel",
        "scheduled agent intent cancellation fence",
    )
    .await;
    let message = insert_due_message(&pool, KIND_AGENT).await;
    let fire = claim_one(&pool, "intent-worker", 30).await;
    assert!(
        record_delivery_agent_turn_intent_pg(
            &pool,
            &message.id,
            &fire.delivery_id,
            &fire.claim_token,
            "intent-only-turn",
        )
        .await
        .expect("record agent launch intent")
    );
    assert!(matches!(
        cancel_scheduled_message_pg(&pool, &message.id)
            .await
            .expect("cancel after agent intent"),
        CancelOutcome::Canceled {
            was_firing: true,
            handoff_started: false
        }
    ));
    assert!(
        !commit_delivery_agent_launch_pg(
            &pool,
            &message.id,
            &fire.delivery_id,
            &fire.claim_token,
            "intent-only-turn",
            600,
        )
        .await
        .expect("commit canceled turn intent")
    );

    let launched_message = insert_scheduled_message_pg(
        &pool,
        &NewScheduledMessage {
            content: "already launched agent delivery".to_string(),
            title: None,
            target_channel_id: None,
            bot: "notify".to_string(),
            delivery_kind: KIND_AGENT.to_string(),
            agent_id: Some("scheduled-test-agent".to_string()),
            agent_instruction: None,
            on_agent_failure: "fail".to_string(),
            scheduled_at: Utc::now() - Duration::seconds(1),
            schedule: None,
            timezone: "UTC".to_string(),
            expires_at: None,
            source: "postgres_test".to_string(),
            created_by: Some("postgres_test".to_string()),
            dedupe_key: None,
            context_strategy: "fresh".to_string(),
            context_snapshot_id: None,
            on_context_failure: "fail".to_string(),
        },
    )
    .await
    .expect("insert launched agent message");
    let launched = claim_one(&pool, "launched-worker", 30).await;
    assert!(
        record_delivery_agent_turn_intent_pg(
            &pool,
            &launched_message.id,
            &launched.delivery_id,
            &launched.claim_token,
            "launched-turn",
        )
        .await
        .expect("record launched turn intent")
    );
    assert!(
        commit_delivery_agent_launch_pg(
            &pool,
            &launched_message.id,
            &launched.delivery_id,
            &launched.claim_token,
            "launched-turn",
            600,
        )
        .await
        .expect("commit launched turn")
    );
    assert!(matches!(
        cancel_scheduled_message_pg(&pool, &launched_message.id)
            .await
            .expect("cancel launch-committed agent turn"),
        CancelOutcome::Canceled {
            was_firing: true,
            handoff_started: true
        }
    ));
    assert!(
        !mark_delivery_agent_turn_started_pg(
            &pool,
            &launched_message.id,
            &launched.delivery_id,
            &launched.claim_token,
            "launched-turn",
            600,
        )
        .await
        .expect("post-launch acknowledgement after cancellation")
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_agent_claim_waits_for_runtime_and_cancel_fences_turn_start() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_runtime_gate",
        "scheduled message runtime and cancellation gate",
    )
    .await;
    let message = insert_due_message(&pool, KIND_AGENT).await;

    let without_runtime = claim_due_fires_pg(&pool, "no-runtime", false, 10, 30, Utc::now())
        .await
        .expect("scan without Discord runtime");
    assert!(without_runtime.is_empty());
    assert_eq!(
        get_scheduled_message_pg(&pool, &message.id)
            .await
            .expect("read waiting definition")
            .expect("waiting definition exists")
            .status,
        STATUS_SCHEDULED
    );

    let fire = claim_one(&pool, "runtime-ready", 30).await;
    assert_eq!(fire.retry_count, 0);
    assert!(matches!(
        cancel_scheduled_message_pg(&pool, &message.id)
            .await
            .expect("cancel before agent handoff"),
        CancelOutcome::Canceled {
            was_firing: true,
            handoff_started: false
        }
    ));
    assert!(
        !record_delivery_agent_turn_intent_pg(
            &pool,
            &message.id,
            &fire.delivery_id,
            &fire.claim_token,
            "must-not-start",
        )
        .await
        .expect("canceled claim must fence turn intent")
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_agent_turn_start_waits_for_parent_lock_and_observes_cancel() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_turn_start_parent_lock",
        "scheduled message turn start parent lock regression",
    )
    .await;
    let message = insert_due_message(&pool, KIND_AGENT).await;
    let fire = claim_one(&pool, "turn-start-lock-worker", 30).await;

    let mut cancel_tx = pool.begin().await.expect("begin cancellation transaction");
    sqlx::query("SELECT id FROM scheduled_messages WHERE id = $1 FOR UPDATE")
        .bind(&message.id)
        .fetch_one(&mut *cancel_tx)
        .await
        .expect("lock parent before cancellation");

    let mark_pool = pool.clone();
    let message_id = message.id.clone();
    let delivery_id = fire.delivery_id.clone();
    let claim_token = fire.claim_token.clone();
    let mut mark_task = tokio::spawn(async move {
        record_delivery_agent_turn_intent_pg(
            &mark_pool,
            &message_id,
            &delivery_id,
            &claim_token,
            "must-not-cross-cancel",
        )
        .await
    });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(150), &mut mark_task)
            .await
            .is_err(),
        "turn intent must wait behind the active parent lock"
    );

    sqlx::query(
        "UPDATE scheduled_messages
         SET status = 'canceled', in_flight_delivery_id = NULL, updated_at = NOW()
         WHERE id = $1 AND status = 'firing' AND in_flight_delivery_id = $2",
    )
    .bind(&message.id)
    .bind(&fire.delivery_id)
    .execute(&mut *cancel_tx)
    .await
    .expect("cancel locked parent");
    sqlx::query(
        "UPDATE scheduled_message_deliveries
         SET status = 'interrupted', error = 'canceled',
             finished_at = NOW(), updated_at = NOW()
         WHERE id = $1 AND status = 'running'",
    )
    .bind(&fire.delivery_id)
    .execute(&mut *cancel_tx)
    .await
    .expect("interrupt canceled child");
    cancel_tx.commit().await.expect("commit cancellation");

    let recorded = tokio::time::timeout(std::time::Duration::from_secs(2), mark_task)
        .await
        .expect("turn-intent waiter should resume after cancel")
        .expect("turn-intent task should join")
        .expect("turn-intent query should succeed");
    assert!(!recorded, "cancellation must fence the waiting turn intent");
    let (status, turn_id): (String, Option<String>) =
        sqlx::query_as("SELECT status, turn_id FROM scheduled_message_deliveries WHERE id = $1")
            .bind(&fire.delivery_id)
            .fetch_one(&pool)
            .await
            .expect("read canceled delivery");
    assert_eq!(status, DELIVERY_INTERRUPTED);
    assert_eq!(turn_id, None);

    pool.close().await;
    pg_db.drop().await;
}
