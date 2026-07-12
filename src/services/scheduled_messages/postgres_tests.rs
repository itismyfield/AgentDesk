use super::*;
use chrono::Duration;
use sqlx::Row;

fn at_postgres_precision(value: chrono::DateTime<Utc>) -> chrono::DateTime<Utc> {
    chrono::DateTime::from_timestamp_micros(value.timestamp_micros())
        .expect("PostgreSQL-compatible timestamp should be representable")
}

#[test]
fn postgres_precision_normalizes_linux_nanosecond_timestamps() {
    let linux_clock_value = chrono::DateTime::<Utc>::from_timestamp(1_784_000_000, 248_030_315)
        .expect("representative Linux nanosecond timestamp");
    let normalized = at_postgres_precision(linux_clock_value);
    assert_eq!(normalized.timestamp_subsec_nanos(), 248_030_000);
    assert_eq!(
        normalized.timestamp_micros(),
        linux_clock_value.timestamp_micros()
    );
}

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

async fn insert_agent_message(
    pool: &PgPool,
    agent_id: &str,
    on_agent_failure: &str,
    schedule: Option<&str>,
) -> ScheduledMessageRow {
    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id)
         VALUES ($1, $2, '123456789')",
    )
    .bind(agent_id)
    .bind(format!("Scheduled Test {agent_id}"))
    .execute(pool)
    .await
    .expect("seed scheduled-message agent");

    db::insert_scheduled_message_pg(
        pool,
        &db::NewScheduledMessage {
            content: format!("scheduled agent payload for {agent_id}"),
            title: None,
            target_channel_id: Some("123456789".to_string()),
            bot: "announce".to_string(),
            delivery_kind: db::KIND_AGENT.to_string(),
            agent_id: Some(agent_id.to_string()),
            agent_instruction: None,
            on_agent_failure: on_agent_failure.to_string(),
            scheduled_at: Utc::now() - Duration::minutes(1),
            schedule: schedule.map(str::to_string),
            timezone: "UTC".to_string(),
            expires_at: None,
            source: "postgres_test".to_string(),
            created_by: Some("postgres_test".to_string()),
            dedupe_key: None,
        },
    )
    .await
    .expect("insert scheduled agent message")
}

async fn insert_recurring_agent_message(
    pool: &PgPool,
    agent_id: &str,
    on_agent_failure: &str,
) -> ScheduledMessageRow {
    insert_agent_message(pool, agent_id, on_agent_failure, Some("@every 10m")).await
}

async fn insert_one_shot_agent_message(
    pool: &PgPool,
    agent_id: &str,
    on_agent_failure: &str,
) -> ScheduledMessageRow {
    insert_agent_message(pool, agent_id, on_agent_failure, None).await
}

async fn insert_due_push_message(pool: &PgPool) -> ScheduledMessageRow {
    db::insert_scheduled_message_pg(
        pool,
        &db::NewScheduledMessage {
            content: "guarded push payload".to_string(),
            title: None,
            target_channel_id: Some("123456789".to_string()),
            bot: "notify".to_string(),
            delivery_kind: db::KIND_PUSH.to_string(),
            agent_id: None,
            agent_instruction: None,
            on_agent_failure: "fail".to_string(),
            scheduled_at: Utc::now() - Duration::minutes(1),
            schedule: None,
            timezone: "UTC".to_string(),
            expires_at: None,
            source: "postgres_test".to_string(),
            created_by: Some("postgres_test".to_string()),
            dedupe_key: None,
        },
    )
    .await
    .expect("insert due push scheduled message")
}

async fn claim_one(pool: &PgPool, owner: &str) -> ClaimedFire {
    let mut claimed = db::claim_due_fires_pg(pool, owner, true, 10, LEASE_SECS, Utc::now())
        .await
        .expect("claim due scheduled message");
    assert_eq!(claimed.len(), 1, "exactly one definition should be due");
    claimed.pop().expect("claimed scheduled-message fire")
}

async fn record_confirmed_agent_turn(pool: &PgPool, fire: &ClaimedFire, turn_id: &str) {
    assert!(
        db::record_delivery_agent_turn_intent_pg(
            pool,
            &fire.message.id,
            &fire.delivery_id,
            &fire.claim_token,
            turn_id,
        )
        .await
        .expect("record agent turn intent")
    );
    assert!(
        db::commit_delivery_agent_launch_pg(
            pool,
            &fire.message.id,
            &fire.delivery_id,
            &fire.claim_token,
            turn_id,
            LEASE_SECS,
        )
        .await
        .expect("commit agent turn launch")
    );
    assert!(
        db::mark_delivery_agent_turn_started_pg(
            pool,
            &fire.message.id,
            &fire.delivery_id,
            &fire.claim_token,
            turn_id,
            LEASE_SECS,
        )
        .await
        .expect("confirm agent turn start")
    );
}

async fn claim_after_exhausting_rearms(pool: &PgPool) -> ClaimedFire {
    let mut fire = claim_one(pool, "retry-worker-0").await;
    assert_eq!(fire.retry_count, 0);

    for expected_retry_count in 1..=(MAX_FIRE_RETRIES + 1) {
        assert!(
            db::interrupt_delivery_and_rewind_pg(
                pool,
                &fire.delivery_id,
                &fire.claim_token,
                &fire.message.id,
                fire.fire_scheduled_at,
                None,
                "test retry before exhaustion",
            )
            .await
            .expect("interrupt retryable fire"),
            "the current claim should rewind its definition"
        );
        fire = claim_one(pool, &format!("retry-worker-{expected_retry_count}")).await;
        assert_eq!(fire.retry_count, expected_retry_count);
    }

    assert!(fire.retry_count > MAX_FIRE_RETRIES);
    fire
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_scheduled_message_default_notify_reaches_push_outbox() {
    let pg_db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "agentdesk_smsg_notify_default",
        "scheduled message default notify bot regression",
    )
    .await;
    let pool = pg_db.connect_and_migrate_with_max_connections(4).await;

    let stored_bot: String = sqlx::query_scalar(
        "INSERT INTO scheduled_messages
             (id, content, target_channel_id, scheduled_at, timezone)
         VALUES
             ('smsg-notify-default', 'info-only scheduled push', '123456789',
              NOW() - INTERVAL '1 second', 'UTC')
         RETURNING bot",
    )
    .fetch_one(&pool)
    .await
    .expect("insert scheduled message through the database default");
    assert_eq!(stored_bot, "notify");

    let mut claims = db::claim_due_fires_pg(
        &pool,
        "notify-default-worker",
        true,
        1,
        LEASE_SECS,
        Utc::now(),
    )
    .await
    .expect("claim default-notify scheduled push");
    assert_eq!(claims.len(), 1);
    fire_claimed(
        &pool,
        None,
        claims.pop().expect("claimed default-notify fire"),
        Utc::now(),
    )
    .await;

    let deliveries = db::list_deliveries_pg(&pool, "smsg-notify-default", 10, None)
        .await
        .expect("load default-notify delivery");
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].status, db::DELIVERY_SENT);
    let outbox_id = deliveries[0]
        .outbox_id
        .expect("push should record its outbox handoff");
    let outbox_bot: String = sqlx::query_scalar("SELECT bot FROM message_outbox WHERE id = $1")
        .bind(outbox_id)
        .fetch_one(&pool)
        .await
        .expect("load default-notify outbox row");
    assert_eq!(outbox_bot, "notify");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_scheduled_message_retry_exhaustion_terminalizes_recurring_definitions() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_retry_exhaustion",
        "scheduled message retry exhaustion regression",
    )
    .await;

    let failed_message =
        insert_recurring_agent_message(&pool, "scheduled-retry-fail-agent", "fail").await;
    let failed_fire = claim_after_exhausting_rearms(&pool).await;
    fire_claimed(&pool, None, failed_fire.clone(), Utc::now()).await;

    let failed_parent = db::get_scheduled_message_pg(&pool, &failed_message.id)
        .await
        .expect("read failed parent")
        .expect("failed parent exists");
    assert_eq!(failed_parent.status, db::STATUS_FAILED);
    assert_eq!(failed_parent.scheduled_at, failed_fire.fire_scheduled_at);
    assert_eq!(failed_parent.in_flight_delivery_id, None);
    assert_eq!(failed_parent.fire_count, 0);
    assert!(
        failed_parent
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("fire retry budget exhausted"))
    );
    let failed_deliveries = db::list_deliveries_pg(&pool, &failed_message.id, 10, None)
        .await
        .expect("list failed deliveries");
    assert_eq!(failed_deliveries.len(), 1);
    assert_eq!(failed_deliveries[0].status, db::DELIVERY_FAILED);
    assert_eq!(failed_deliveries[0].retry_count, MAX_FIRE_RETRIES + 1);
    assert_eq!(failed_deliveries[0].fallback_outbox_id, None);

    let fallback_message =
        insert_recurring_agent_message(&pool, "scheduled-retry-fallback-agent", "push_raw").await;
    let fallback_fire = claim_after_exhausting_rearms(&pool).await;
    fire_claimed(&pool, None, fallback_fire.clone(), Utc::now()).await;

    let fallback_parent = db::get_scheduled_message_pg(&pool, &fallback_message.id)
        .await
        .expect("read fallback parent")
        .expect("fallback parent exists");
    assert_eq!(fallback_parent.status, db::STATUS_FAILED);
    assert_eq!(
        fallback_parent.scheduled_at,
        fallback_fire.fire_scheduled_at
    );
    assert_eq!(fallback_parent.in_flight_delivery_id, None);
    assert_eq!(fallback_parent.fire_count, 1);
    assert!(fallback_parent.last_fired_at.is_some());
    assert!(
        fallback_parent
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("fell back to raw push"))
    );

    let fallback_deliveries = db::list_deliveries_pg(&pool, &fallback_message.id, 10, None)
        .await
        .expect("list fallback deliveries");
    assert_eq!(fallback_deliveries.len(), 1);
    let fallback_delivery = &fallback_deliveries[0];
    assert_eq!(fallback_delivery.status, db::DELIVERY_SENT);
    assert_eq!(fallback_delivery.retry_count, MAX_FIRE_RETRIES + 1);
    let fallback_outbox_id = fallback_delivery
        .fallback_outbox_id
        .expect("push_raw should record its durable outbox handoff");
    let outbox = sqlx::query(
        "SELECT target, content, source, status, reason_code
         FROM message_outbox
         WHERE id = $1",
    )
    .bind(fallback_outbox_id)
    .fetch_one(&pool)
    .await
    .expect("read push_raw outbox row");
    assert_eq!(
        outbox.try_get::<String, _>("target").unwrap(),
        "channel:123456789"
    );
    assert_eq!(
        outbox.try_get::<String, _>("content").unwrap(),
        fallback_message.content
    );
    assert_eq!(
        outbox.try_get::<String, _>("source").unwrap(),
        OUTBOX_SOURCE
    );
    assert_eq!(outbox.try_get::<String, _>("status").unwrap(), "pending");
    assert!(
        outbox
            .try_get::<Option<String>, _>("reason_code")
            .unwrap()
            .as_deref()
            .is_some_and(|reason| reason.contains(":fallback:"))
    );

    assert!(
        db::claim_due_fires_pg(
            &pool,
            "post-terminal-worker",
            true,
            10,
            LEASE_SECS,
            Utc::now(),
        )
        .await
        .expect("scan after terminal exhaustion")
        .is_empty(),
        "recurring definitions must not advance to another slot after retry exhaustion"
    );

    let expired_message =
        insert_recurring_agent_message(&pool, "scheduled-retry-expired-agent", "push_raw").await;
    let expires_at = Utc::now() + Duration::minutes(5);
    sqlx::query("UPDATE scheduled_messages SET expires_at = $2 WHERE id = $1")
        .bind(&expired_message.id)
        .bind(expires_at)
        .execute(&pool)
        .await
        .expect("set scheduled-message expiry");
    let expired_fire = claim_after_exhausting_rearms(&pool).await;
    let outbox_count_before: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
        .fetch_one(&pool)
        .await
        .expect("count outbox rows before expired fire");
    fire_claimed(
        &pool,
        None,
        expired_fire.clone(),
        expires_at + Duration::seconds(1),
    )
    .await;

    let expired_parent = db::get_scheduled_message_pg(&pool, &expired_message.id)
        .await
        .expect("read expired parent")
        .expect("expired parent exists");
    assert_eq!(expired_parent.status, db::STATUS_EXPIRED);
    assert_eq!(expired_parent.fire_count, 0);
    assert_eq!(expired_parent.last_fired_at, None);
    assert_eq!(expired_parent.in_flight_delivery_id, None);
    let expired_deliveries = db::list_deliveries_pg(&pool, &expired_message.id, 10, None)
        .await
        .expect("list expired deliveries");
    assert_eq!(expired_deliveries.len(), 1);
    assert_eq!(expired_deliveries[0].status, db::DELIVERY_INTERRUPTED);
    assert_eq!(expired_deliveries[0].fallback_outbox_id, None);
    let outbox_count_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
        .fetch_one(&pool)
        .await
        .expect("count outbox rows after expired fire");
    assert_eq!(
        outbox_count_after, outbox_count_before,
        "an expired push_raw definition must not enqueue a fallback"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_trigger_now_retry_preserves_recurring_anchor() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_trigger_retry",
        "scheduled message trigger-now retry anchor regression",
    )
    .await;
    let original_scheduled_at = at_postgres_precision(Utc::now() + Duration::hours(2));
    let message = db::insert_scheduled_message_pg(
        &pool,
        &db::NewScheduledMessage {
            content: "trigger-now cadence payload".to_string(),
            title: None,
            target_channel_id: Some("123456789".to_string()),
            bot: "announce".to_string(),
            delivery_kind: db::KIND_PUSH.to_string(),
            agent_id: None,
            agent_instruction: None,
            on_agent_failure: "fail".to_string(),
            scheduled_at: original_scheduled_at,
            schedule: Some("@every 1h".to_string()),
            timezone: "UTC".to_string(),
            expires_at: None,
            source: "postgres_test".to_string(),
            created_by: Some("postgres_test".to_string()),
            dedupe_key: None,
        },
    )
    .await
    .expect("insert trigger-now recurring definition");

    let manual = db::trigger_now_pg(&pool, &message.id, "manual-worker", LEASE_SECS)
        .await
        .expect("trigger recurring definition")
        .expect("scheduled definition should trigger");
    assert_eq!(manual.message.scheduled_at, original_scheduled_at);
    assert!(manual.fire_scheduled_at < original_scheduled_at);
    assert!(
        db::interrupt_delivery_and_rewind_pg(
            &pool,
            &manual.delivery_id,
            &manual.claim_token,
            &message.id,
            manual.fire_scheduled_at,
            None,
            "retry manual fire",
        )
        .await
        .expect("interrupt manual fire")
    );

    let mut retries =
        db::claim_due_fires_pg(&pool, "retry-worker", true, 10, LEASE_SECS, Utc::now())
            .await
            .expect("reclaim manual fire");
    assert_eq!(retries.len(), 1);
    let retry = retries.pop().expect("manual retry exists");
    assert_eq!(retry.delivery_id, manual.delivery_id);
    assert_eq!(
        retry.fire_scheduled_at,
        at_postgres_precision(manual.fire_scheduled_at),
        "PostgreSQL stores the manual NOW() slot at microsecond precision"
    );
    assert_eq!(
        retry.message.scheduled_at, original_scheduled_at,
        "a manual retry must retain the definition's regular cadence anchor"
    );

    fire_claimed(&pool, None, retry, Utc::now()).await;
    let resumed = db::get_scheduled_message_pg(&pool, &message.id)
        .await
        .expect("read resumed recurring definition")
        .expect("resumed recurring definition exists");
    assert_eq!(resumed.status, db::STATUS_SCHEDULED);
    assert_eq!(resumed.scheduled_at, original_scheduled_at);
    assert_eq!(resumed.fire_count, 1);
    assert_eq!(resumed.in_flight_delivery_id, None);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_resume_anchor_compat_migration_preserves_active_trigger_now_anchor() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_resume_compat",
        "scheduled message additive resume-anchor migration",
    )
    .await;
    let original_scheduled_at = at_postgres_precision(Utc::now() + Duration::hours(2));
    let message = db::insert_scheduled_message_pg(
        &pool,
        &db::NewScheduledMessage {
            content: "legacy trigger-now cadence payload".to_string(),
            title: None,
            target_channel_id: Some("123456789".to_string()),
            bot: "announce".to_string(),
            delivery_kind: db::KIND_PUSH.to_string(),
            agent_id: None,
            agent_instruction: None,
            on_agent_failure: "fail".to_string(),
            scheduled_at: original_scheduled_at,
            schedule: Some("@every 1h".to_string()),
            timezone: "UTC".to_string(),
            expires_at: None,
            source: "postgres_test".to_string(),
            created_by: Some("postgres_test".to_string()),
            dedupe_key: None,
        },
    )
    .await
    .expect("insert legacy trigger-now definition");
    let manual = db::trigger_now_pg(&pool, &message.id, "legacy-worker", LEASE_SECS)
        .await
        .expect("trigger legacy recurring definition")
        .expect("legacy definition should trigger");
    assert!(manual.fire_scheduled_at < original_scheduled_at);

    sqlx::query(
        "ALTER TABLE scheduled_message_deliveries
         ALTER COLUMN resume_scheduled_at DROP NOT NULL",
    )
    .execute(&pool)
    .await
    .expect("simulate the nullable 0084 schema");
    sqlx::query(
        "UPDATE scheduled_message_deliveries
         SET resume_scheduled_at = NULL
         WHERE id = $1",
    )
    .bind(&manual.delivery_id)
    .execute(&pool)
    .await
    .expect("simulate a pre-0084 active delivery");

    sqlx::raw_sql(include_str!(
        "../../../migrations/postgres/0086_scheduled_message_resume_anchor_not_null.sql"
    ))
    .execute(&pool)
    .await
    .expect("apply resume-anchor compatibility migration");

    let restored_anchor: chrono::DateTime<Utc> = sqlx::query_scalar(
        "SELECT resume_scheduled_at
         FROM scheduled_message_deliveries
         WHERE id = $1",
    )
    .bind(&manual.delivery_id)
    .fetch_one(&pool)
    .await
    .expect("read migrated resume anchor");
    assert_eq!(
        restored_anchor, original_scheduled_at,
        "an active trigger-now delivery must resume the parent's regular slot"
    );
    let is_nullable: String = sqlx::query_scalar(
        "SELECT is_nullable
         FROM information_schema.columns
         WHERE table_schema = 'public'
           AND table_name = 'scheduled_message_deliveries'
           AND column_name = 'resume_scheduled_at'",
    )
    .fetch_one(&pool)
    .await
    .expect("read migrated resume-anchor nullability");
    assert_eq!(is_nullable, "NO");

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_launch_compat_migration_backfills_legacy_turn_as_ambiguous() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_launch_compat",
        "scheduled message legacy turn launch barrier migration",
    )
    .await;
    let message =
        insert_recurring_agent_message(&pool, "scheduled-legacy-launch-agent", "fail").await;
    let fire = claim_one(&pool, "legacy-launch-worker").await;
    sqlx::query(
        "UPDATE scheduled_message_deliveries
         SET turn_id = 'legacy-ambiguous-turn',
             turn_intent_at = NULL,
             launch_committed_at = NULL,
             turn_started_at = NULL,
             lease_expires_at = NOW() - INTERVAL '1 second'
         WHERE id = $1",
    )
    .bind(&fire.delivery_id)
    .execute(&pool)
    .await
    .expect("simulate public-0084 legacy active turn");

    sqlx::raw_sql(include_str!(
        "../../../migrations/postgres/0087_scheduled_message_launch_commit_and_runtime_defer.sql"
    ))
    .execute(&pool)
    .await
    .expect("apply launch-commit compatibility migration");

    let (started_at, turn_intent_at, launch_committed_at, turn_started_at): (
        DateTime<Utc>,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
        Option<DateTime<Utc>>,
    ) = sqlx::query_as(
        "SELECT started_at, turn_intent_at, launch_committed_at, turn_started_at
         FROM scheduled_message_deliveries
         WHERE id = $1",
    )
    .bind(&fire.delivery_id)
    .fetch_one(&pool)
    .await
    .expect("read migrated legacy launch state");
    assert_eq!(turn_intent_at, None);
    assert_eq!(launch_committed_at, Some(started_at));
    assert_eq!(turn_started_at, Some(started_at));
    assert_eq!(
        db::recover_expired_leases_pg(&pool)
            .await
            .expect("recover migrated legacy turn"),
        0,
        "legacy turn ids must be adopted rather than replacement-rearmed"
    );
    let parent = db::get_scheduled_message_pg(&pool, &message.id)
        .await
        .expect("read migrated legacy parent")
        .expect("migrated legacy parent exists");
    assert_eq!(parent.status, db::STATUS_FIRING);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_agent_trigger_now_retry_preserves_recurring_anchor_through_poller() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_agent_trigger_retry",
        "scheduled message agent trigger-now retry anchor regression",
    )
    .await;
    let agent_id = "scheduled-agent-trigger-retry";
    sqlx::query(
        "INSERT INTO agents (id, name, discord_channel_id)
         VALUES ($1, 'Scheduled Agent Trigger Retry', '123456789')",
    )
    .bind(agent_id)
    .execute(&pool)
    .await
    .expect("seed trigger-now retry agent");
    let original_scheduled_at = at_postgres_precision(Utc::now() + Duration::hours(2));
    let message = db::insert_scheduled_message_pg(
        &pool,
        &db::NewScheduledMessage {
            content: "agent trigger-now cadence payload".to_string(),
            title: None,
            target_channel_id: Some("123456789".to_string()),
            bot: "announce".to_string(),
            delivery_kind: db::KIND_AGENT.to_string(),
            agent_id: Some(agent_id.to_string()),
            agent_instruction: None,
            on_agent_failure: "fail".to_string(),
            scheduled_at: original_scheduled_at,
            schedule: Some("@every 1h".to_string()),
            timezone: "UTC".to_string(),
            expires_at: None,
            source: "postgres_test".to_string(),
            created_by: Some("postgres_test".to_string()),
            dedupe_key: None,
        },
    )
    .await
    .expect("insert agent trigger-now recurring definition");

    let manual = db::trigger_now_pg(&pool, &message.id, "manual-agent-worker", LEASE_SECS)
        .await
        .expect("trigger recurring agent definition")
        .expect("scheduled agent definition should trigger");
    let retry_at = Utc::now() + Duration::minutes(5);
    assert!(
        db::interrupt_delivery_and_rewind_pg(
            &pool,
            &manual.delivery_id,
            &manual.claim_token,
            &message.id,
            manual.fire_scheduled_at,
            Some(retry_at),
            "retry manual agent fire",
        )
        .await
        .expect("interrupt manual agent fire")
    );
    let blocked = db::claim_due_fires_pg(
        &pool,
        "early-retry-agent-worker",
        true,
        10,
        LEASE_SECS,
        retry_at - Duration::seconds(1),
    )
    .await
    .expect("scan before manual retry deadline");
    assert!(blocked.is_empty());
    let mut retries = db::claim_due_fires_pg(
        &pool,
        "retry-agent-worker",
        true,
        10,
        LEASE_SECS,
        retry_at + Duration::seconds(1),
    )
    .await
    .expect("reclaim manual agent fire");
    assert_eq!(retries.len(), 1);
    let retry = retries.pop().expect("manual agent retry exists");
    record_confirmed_agent_turn(&pool, &retry, "agent-trigger-retry-turn").await;

    let running = db::list_running_agent_deliveries_pg(&pool, "retry-agent-worker", LEASE_SECS, 10)
        .await
        .expect("poll agent trigger-now retry");
    assert_eq!(running.len(), 1);
    assert_eq!(
        running[0].scheduled_at, original_scheduled_at,
        "the poller must receive the persisted regular cadence anchor"
    );
    sqlx::query(
        "INSERT INTO session_transcripts (turn_id, assistant_message)
         VALUES ('agent-trigger-retry-turn', '예약 메시지 전달 완료')",
    )
    .execute(&pool)
    .await
    .expect("seed delivered trigger-now agent transcript");
    assert!(resolve_agent_delivery(&pool, &running[0], false).await);

    let resumed = db::get_scheduled_message_pg(&pool, &message.id)
        .await
        .expect("read resumed recurring agent definition")
        .expect("resumed recurring agent definition exists");
    assert_eq!(resumed.status, db::STATUS_SCHEDULED);
    assert_eq!(resumed.scheduled_at, original_scheduled_at);
    assert_eq!(resumed.fire_count, 1);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_agent_timeout_with_push_raw_fails_closed_without_outbox() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_agent_timeout_fence",
        "scheduled message timeout fallback relay fence regression",
    )
    .await;
    let message =
        insert_recurring_agent_message(&pool, "scheduled-timeout-agent", "push_raw").await;
    let fire = claim_one(&pool, "timeout-worker").await;
    record_confirmed_agent_turn(&pool, &fire, "scheduled-timeout-turn").await;
    sqlx::query(
        "UPDATE scheduled_message_deliveries
         SET started_at = NOW() - INTERVAL '31 minutes',
             turn_started_at = NOW() - INTERVAL '31 minutes'
         WHERE id = $1",
    )
    .bind(&fire.delivery_id)
    .execute(&pool)
    .await
    .expect("age agent delivery beyond completion timeout");
    let mut running = db::list_running_agent_deliveries_pg(&pool, "timeout-worker", LEASE_SECS, 10)
        .await
        .expect("poll timed-out agent turn");
    let delivery = running.pop().expect("timed-out delivery should be polled");

    assert!(resolve_agent_delivery(&pool, &delivery, true).await);

    let outbox_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
        .fetch_one(&pool)
        .await
        .expect("count timeout fallback outbox rows");
    assert_eq!(
        outbox_count, 0,
        "an unconfirmed timeout must not race a late agent relay with raw fallback"
    );
    let deliveries = db::list_deliveries_pg(&pool, &message.id, 10, None)
        .await
        .expect("read timed-out delivery");
    assert_eq!(deliveries[0].status, db::DELIVERY_FAILED);
    assert_eq!(deliveries[0].fallback_outbox_id, None);
    assert!(
        deliveries[0]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("raw fallback suppressed"))
    );
    let parent = db::get_scheduled_message_pg(&pool, &message.id)
        .await
        .expect("read timeout parent")
        .expect("timeout parent exists");
    assert_eq!(parent.status, db::STATUS_SCHEDULED);
    assert_eq!(parent.fire_count, 0);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_agent_evidence_before_runtime_ack_is_not_missed() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_fast_agent_evidence",
        "scheduled message pre-ack agent evidence",
    )
    .await;
    let message =
        insert_recurring_agent_message(&pool, "scheduled-fast-evidence-agent", "fail").await;
    let fire = claim_one(&pool, "fast-evidence-worker").await;
    let turn_id = "scheduled-fast-evidence-turn";
    assert!(
        db::record_delivery_agent_turn_intent_pg(
            &pool,
            &message.id,
            &fire.delivery_id,
            &fire.claim_token,
            turn_id,
        )
        .await
        .expect("record fast-evidence turn intent")
    );
    assert!(
        db::commit_delivery_agent_launch_pg(
            &pool,
            &message.id,
            &fire.delivery_id,
            &fire.claim_token,
            turn_id,
            LEASE_SECS,
        )
        .await
        .expect("commit fast-evidence turn launch")
    );
    sqlx::query(
        "INSERT INTO session_transcripts (turn_id, assistant_message)
         VALUES ($1, 'fast relay before scheduler acknowledgement')",
    )
    .bind(turn_id)
    .execute(&pool)
    .await
    .expect("seed evidence before runtime acknowledgement");
    assert!(
        db::mark_delivery_agent_turn_started_pg(
            &pool,
            &message.id,
            &fire.delivery_id,
            &fire.claim_token,
            turn_id,
            LEASE_SECS,
        )
        .await
        .expect("record later runtime acknowledgement")
    );
    assert!(
        db::release_agent_delivery_to_poller_pg(&pool, &fire.delivery_id, &fire.claim_token)
            .await
            .expect("release fast-evidence turn")
    );
    let (evidence_at, turn_started_at): (DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
        "SELECT transcript.created_at, delivery.turn_started_at
         FROM session_transcripts AS transcript
         JOIN scheduled_message_deliveries AS delivery ON delivery.turn_id = transcript.turn_id
         WHERE transcript.turn_id = $1",
    )
    .bind(turn_id)
    .fetch_one(&pool)
    .await
    .expect("read evidence/runtime ordering");
    assert!(evidence_at <= turn_started_at);

    let mut running =
        db::list_running_agent_deliveries_pg(&pool, "fast-evidence-poller", LEASE_SECS, 10)
            .await
            .expect("poll fast-evidence turn");
    let delivery = running.pop().expect("fast-evidence turn should be polled");
    assert!(delivery.launch_committed_at <= evidence_at);
    assert!(resolve_agent_delivery(&pool, &delivery, false).await);
    let completed = db::get_scheduled_message_pg(&pool, &message.id)
        .await
        .expect("read fast-evidence parent")
        .expect("fast-evidence parent exists");
    assert_eq!(completed.status, db::STATUS_SCHEDULED);
    assert_eq!(completed.fire_count, 1);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_provider_error_transcripts_fail_without_false_delivery() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_provider_error_transcript",
        "scheduled message provider-error transcript regression",
    )
    .await;
    let cases = [
        (
            "scheduled-codex-usage-limit-agent",
            "codex-provider-error-worker",
            "scheduled-codex-usage-limit-turn",
            "Error: You've hit your usage limit. Try again later.",
            serde_json::json!([{
                "kind": "error",
                "content": "You've hit your usage limit. Try again later.",
                "status": "error",
                "is_error": true
            }, {
                "kind": "system",
                "content": "voluntary feedback recorded",
                "is_error": false
            }]),
        ),
        (
            "scheduled-qwen-api-error-agent",
            "qwen-provider-error-worker",
            "scheduled-qwen-api-error-turn",
            "[API Error: 400 status code (no body)]",
            serde_json::json!([
                {
                    "kind": "assistant",
                    "content": "[API Error: 400 status code (no body)]",
                    "is_error": false
                },
                {
                    "kind": "result",
                    "content": "[API Error: 400 status code (no body)]",
                    "status": "success",
                    "is_error": false
                }
            ]),
        ),
    ];

    for (agent_id, owner, turn_id, assistant_message, events_json) in cases {
        let message = insert_one_shot_agent_message(&pool, agent_id, "fail").await;
        let fire = claim_one(&pool, owner).await;
        record_confirmed_agent_turn(&pool, &fire, turn_id).await;
        let mut running = db::list_running_agent_deliveries_pg(&pool, owner, LEASE_SECS, 10)
            .await
            .expect("poll provider-error agent turn");
        let delivery = running
            .pop()
            .expect("provider-error delivery should be polled");
        sqlx::query(
            "INSERT INTO session_transcripts (turn_id, assistant_message, events_json)
             VALUES ($1, $2, $3)",
        )
        .bind(turn_id)
        .bind(assistant_message)
        .bind(sqlx::types::Json(events_json))
        .execute(&pool)
        .await
        .expect("seed provider-error transcript evidence");

        assert!(resolve_agent_delivery(&pool, &delivery, false).await);
        let deliveries = db::list_deliveries_pg(&pool, &message.id, 10, None)
            .await
            .expect("read provider-error delivery");
        assert_eq!(deliveries.len(), 1);
        assert_eq!(deliveries[0].status, db::DELIVERY_FAILED);
        assert_eq!(deliveries[0].fallback_outbox_id, None);
        assert!(
            deliveries[0].error.as_deref().is_some_and(
                |error| error == "agent turn returned terminal provider error transcript"
            )
        );

        let parent = db::get_scheduled_message_pg(&pool, &message.id)
            .await
            .expect("read provider-error parent")
            .expect("provider-error parent exists");
        assert_eq!(parent.status, db::STATUS_FAILED);
        assert_eq!(parent.fire_count, 0);
        assert!(
            parent.last_error.as_deref().is_some_and(
                |error| error == "agent turn returned terminal provider error transcript"
            )
        );
    }

    let outbox_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox WHERE source = $1")
            .bind(OUTBOX_SOURCE)
            .fetch_one(&pool)
            .await
            .expect("count provider-error fallback outbox rows");
    assert_eq!(outbox_count, 0);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_transcript_event_probe_tolerates_unknown_or_legacy_json() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_transcript_event_compat",
        "scheduled message transcript-event compatibility regression",
    )
    .await;
    let lower_bound = Utc::now() - Duration::minutes(1);
    let cases = [
        (
            "scheduled-future-event-turn",
            serde_json::json!([{"kind": "future_event", "content": "forward compatible"}]),
        ),
        (
            "scheduled-legacy-event-shape-turn",
            serde_json::json!({"legacy": true}),
        ),
        (
            "scheduled-recovered-assistant-turn",
            serde_json::json!([
                {
                    "kind": "error",
                    "content": "recoverable tool attempt failed",
                    "is_error": true
                },
                {
                    "kind": "assistant",
                    "content": "recovered without a result event",
                    "is_error": false
                }
            ]),
        ),
    ];

    for (turn_id, events_json) in cases {
        sqlx::query(
            "INSERT INTO session_transcripts (turn_id, assistant_message, events_json)
             VALUES ($1, 'normal delivered answer', $2)",
        )
        .bind(turn_id)
        .bind(sqlx::types::Json(events_json))
        .execute(&pool)
        .await
        .expect("seed forward-compatible transcript event evidence");

        assert_eq!(
            find_turn_delivery_evidence(&pool, turn_id, lower_bound)
                .await
                .expect("query forward-compatible transcript evidence"),
            Some(TurnEvidence::Delivered)
        );
    }

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_definitive_agent_failure_atomically_enqueues_one_fallback() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_agent_fallback_atomic",
        "scheduled message atomic agent fallback regression",
    )
    .await;
    let message =
        insert_recurring_agent_message(&pool, "scheduled-no-reply-agent", "push_raw").await;
    let fire = claim_one(&pool, "fallback-worker").await;
    let turn_id = "scheduled-no-reply-turn";
    record_confirmed_agent_turn(&pool, &fire, turn_id).await;
    let mut running =
        db::list_running_agent_deliveries_pg(&pool, "fallback-worker", LEASE_SECS, 10)
            .await
            .expect("poll no-reply agent turn");
    let delivery = running.pop().expect("no-reply delivery should be polled");
    sqlx::query(
        "INSERT INTO session_transcripts (turn_id, assistant_message)
         VALUES ($1, 'NO_REPLY')",
    )
    .bind(turn_id)
    .execute(&pool)
    .await
    .expect("seed definitive no-reply evidence");

    let first_delivery = delivery.clone();
    let second_delivery = delivery.clone();
    let (first, second) = tokio::join!(
        resolve_agent_delivery(&pool, &first_delivery, false),
        resolve_agent_delivery(&pool, &second_delivery, false),
    );
    assert_eq!(
        usize::from(first) + usize::from(second),
        1,
        "only one competing poller may commit the terminal handoff"
    );

    let outbox_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox WHERE source = $1")
            .bind(OUTBOX_SOURCE)
            .fetch_one(&pool)
            .await
            .expect("count definitive fallback rows");
    assert_eq!(outbox_count, 1);
    let deliveries = db::list_deliveries_pg(&pool, &message.id, 10, None)
        .await
        .expect("read definitive fallback delivery");
    assert_eq!(deliveries[0].status, db::DELIVERY_SENT);
    assert!(deliveries[0].fallback_outbox_id.is_some());
    let parent = db::get_scheduled_message_pg(&pool, &message.id)
        .await
        .expect("read definitive fallback parent")
        .expect("definitive fallback parent exists");
    assert_eq!(parent.status, db::STATUS_SCHEDULED);
    assert_eq!(parent.fire_count, 1);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_runtime_absence_defers_terminal_agent_fallback_polling() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_runtime_absent_agent_poll",
        "scheduled agent fallback polling waits for Discord runtime",
    )
    .await;
    let message =
        insert_recurring_agent_message(&pool, "scheduled-runtime-absent-poll-agent", "push_raw")
            .await;
    let fire = claim_one(&pool, "agent-launch-worker").await;
    let turn_id = "scheduled-runtime-absent-poll-turn";
    record_confirmed_agent_turn(&pool, &fire, turn_id).await;
    assert!(
        db::release_agent_delivery_to_poller_pg(&pool, &fire.delivery_id, &fire.claim_token)
            .await
            .expect("release confirmed turn to completion poller")
    );
    sqlx::query(
        "INSERT INTO session_transcripts (turn_id, assistant_message)
         VALUES ($1, 'NO_REPLY')",
    )
    .bind(turn_id)
    .execute(&pool)
    .await
    .expect("seed terminal no-reply evidence");

    assert!(
        !tick_once(&pool, None, "runtime-absent-poller").await,
        "a Postgres-only process must not consume terminal agent evidence"
    );
    let waiting_parent = db::get_scheduled_message_pg(&pool, &message.id)
        .await
        .expect("read runtime-gated parent")
        .expect("runtime-gated parent exists");
    assert_eq!(waiting_parent.status, db::STATUS_FIRING);
    assert_eq!(
        waiting_parent.in_flight_delivery_id.as_deref(),
        Some(fire.delivery_id.as_str())
    );
    assert_eq!(waiting_parent.fire_count, 0);
    let waiting_deliveries = db::list_deliveries_pg(&pool, &message.id, 10, None)
        .await
        .expect("read runtime-gated delivery");
    assert_eq!(waiting_deliveries.len(), 1);
    assert_eq!(waiting_deliveries[0].status, "running");
    assert_eq!(waiting_deliveries[0].fallback_outbox_id, None);
    let waiting_outbox_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox WHERE source = $1")
            .bind(OUTBOX_SOURCE)
            .fetch_one(&pool)
            .await
            .expect("count fallback rows while runtime is absent");
    assert_eq!(waiting_outbox_count, 0);

    let runtime = HealthRegistry::new();
    assert!(
        tick_once(&pool, Some(&runtime), "runtime-restored-poller").await,
        "a runtime-capable leader should adopt and resolve the durable turn"
    );
    let completed_parent = db::get_scheduled_message_pg(&pool, &message.id)
        .await
        .expect("read runtime-restored parent")
        .expect("runtime-restored parent exists");
    assert_eq!(completed_parent.status, db::STATUS_SCHEDULED);
    assert_eq!(completed_parent.fire_count, 1);
    assert_eq!(completed_parent.in_flight_delivery_id, None);
    let completed_deliveries = db::list_deliveries_pg(&pool, &message.id, 10, None)
        .await
        .expect("read runtime-restored delivery");
    assert_eq!(completed_deliveries[0].status, db::DELIVERY_SENT);
    assert!(completed_deliveries[0].fallback_outbox_id.is_some());
    let completed_outbox_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox WHERE source = $1")
            .bind(OUTBOX_SOURCE)
            .fetch_one(&pool)
            .await
            .expect("count fallback rows after runtime restoration");
    assert_eq!(completed_outbox_count, 1);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_missing_runtime_waits_without_consuming_agent_retry() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_runtime_wait",
        "scheduled message missing runtime retry budget regression",
    )
    .await;
    let message =
        insert_recurring_agent_message(&pool, "scheduled-runtime-wait-agent", "fail").await;

    let without_runtime =
        db::claim_due_fires_pg(&pool, "no-runtime", false, 10, LEASE_SECS, Utc::now())
            .await
            .expect("scan agent definitions without Discord runtime");
    assert!(without_runtime.is_empty());

    let first = claim_one(&pool, "runtime-missing-direct-fire").await;
    fire_claimed(&pool, None, first, Utc::now()).await;
    let waiting = db::get_scheduled_message_pg(&pool, &message.id)
        .await
        .expect("read runtime-deferred parent")
        .expect("runtime-deferred parent exists");
    assert_eq!(waiting.status, db::STATUS_SCHEDULED);
    assert_eq!(waiting.in_flight_delivery_id, None);
    assert_eq!(waiting.scheduled_at, message.scheduled_at);
    assert!(
        db::list_deliveries_pg(&pool, &message.id, 10, None)
            .await
            .expect("list runtime-deferred deliveries")
            .is_empty(),
        "a missing process-wide runtime is not a delivery attempt"
    );
    let defer_until: DateTime<Utc> = sqlx::query_scalar(
        "SELECT runtime_defer_until
         FROM scheduled_messages
         WHERE id = $1",
    )
    .bind(&message.id)
    .fetch_one(&pool)
    .await
    .expect("read runtime defer not-before");
    assert!(defer_until > Utc::now());
    assert!(
        db::claim_due_fires_pg(
            &pool,
            "runtime-hot-loop-worker",
            true,
            10,
            LEASE_SECS,
            Utc::now(),
        )
        .await
        .expect("scan before runtime defer not-before")
        .is_empty(),
        "runtime bootstrap must not hot-loop the same overdue definition"
    );
    let mut restored = db::claim_due_fires_pg(
        &pool,
        "runtime-restored-worker",
        true,
        10,
        LEASE_SECS,
        defer_until + Duration::milliseconds(1),
    )
    .await
    .expect("claim after runtime defer not-before");
    assert_eq!(restored.len(), 1);
    let second = restored.pop().expect("runtime-restored claim");
    assert_eq!(second.retry_count, 0);
    let cleared_defer: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT runtime_defer_until
         FROM scheduled_messages
         WHERE id = $1",
    )
    .bind(&message.id)
    .fetch_one(&pool)
    .await
    .expect("read cleared runtime defer gate");
    assert_eq!(cleared_defer, None);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_cancel_before_push_handoff_enqueues_nothing() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_cancel_push_handoff",
        "scheduled message canceled push handoff regression",
    )
    .await;
    let message = insert_due_push_message(&pool).await;
    let fire = claim_one(&pool, "cancel-before-push-handoff").await;
    assert!(matches!(
        db::cancel_scheduled_message_pg(&pool, &message.id)
            .await
            .expect("cancel claimed push"),
        db::CancelOutcome::Canceled {
            was_firing: true,
            handoff_started: false
        }
    ));

    fire_claimed(&pool, None, fire, Utc::now()).await;

    let canceled = db::get_scheduled_message_pg(&pool, &message.id)
        .await
        .expect("read canceled push parent")
        .expect("canceled push parent exists");
    assert_eq!(canceled.status, "canceled");
    let outbox_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox WHERE source = $1")
            .bind(OUTBOX_SOURCE)
            .fetch_one(&pool)
            .await
            .expect("count scheduled push outbox rows");
    assert_eq!(outbox_count, 0);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_expired_running_agent_never_enqueues_raw_fallback() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_expired_agent_fallback",
        "scheduled message expired running-agent fallback regression",
    )
    .await;
    let message =
        insert_recurring_agent_message(&pool, "scheduled-expired-live-agent", "push_raw").await;
    let fire = claim_one(&pool, "expired-agent-worker").await;
    let turn_id = "scheduled-expired-agent-turn";
    record_confirmed_agent_turn(&pool, &fire, turn_id).await;
    sqlx::query(
        "UPDATE scheduled_messages SET expires_at = NOW() - INTERVAL '1 second' WHERE id = $1",
    )
    .bind(&message.id)
    .execute(&pool)
    .await
    .expect("expire running agent definition");
    let mut running =
        db::list_running_agent_deliveries_pg(&pool, "expired-agent-worker", LEASE_SECS, 10)
            .await
            .expect("poll expired running agent");
    let delivery = running
        .pop()
        .expect("expired running agent should be polled");
    sqlx::query(
        "INSERT INTO session_transcripts (turn_id, assistant_message)
         VALUES ($1, 'NO_REPLY')",
    )
    .bind(turn_id)
    .execute(&pool)
    .await
    .expect("seed expired no-reply evidence");

    assert!(resolve_agent_delivery(&pool, &delivery, false).await);
    let outbox_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
        .fetch_one(&pool)
        .await
        .expect("count expired agent fallback rows");
    assert_eq!(outbox_count, 0);
    let parent = db::get_scheduled_message_pg(&pool, &message.id)
        .await
        .expect("read expired agent parent")
        .expect("expired agent parent exists");
    assert_eq!(parent.status, db::STATUS_EXPIRED);
    let deliveries = db::list_deliveries_pg(&pool, &message.id, 10, None)
        .await
        .expect("read expired agent delivery");
    assert_eq!(deliveries[0].status, db::DELIVERY_INTERRUPTED);
    assert_eq!(deliveries[0].fallback_outbox_id, None);

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_expiry_does_not_terminalize_a_still_live_agent_turn() {
    let (pg_db, pool) = create_test_pool(
        "agentdesk_smsg_expired_live_turn",
        "scheduled message live turn expiry fence regression",
    )
    .await;
    let message =
        insert_recurring_agent_message(&pool, "scheduled-expired-running-agent", "push_raw").await;
    let fire = claim_one(&pool, "expired-live-agent-worker").await;
    record_confirmed_agent_turn(&pool, &fire, "scheduled-expired-live-turn").await;
    sqlx::query(
        "UPDATE scheduled_messages SET expires_at = NOW() - INTERVAL '1 second' WHERE id = $1",
    )
    .bind(&message.id)
    .execute(&pool)
    .await
    .expect("expire definition while its turn is live");
    let mut running =
        db::list_running_agent_deliveries_pg(&pool, "expired-live-agent-worker", LEASE_SECS, 10)
            .await
            .expect("poll live expired definition");
    let delivery = running.pop().expect("live turn should remain pollable");

    assert!(
        !resolve_agent_delivery(&pool, &delivery, false).await,
        "expiry alone cannot close a turn that may still relay"
    );
    let parent = db::get_scheduled_message_pg(&pool, &message.id)
        .await
        .expect("read live expired parent")
        .expect("live expired parent exists");
    assert_eq!(parent.status, db::STATUS_FIRING);
    let deliveries = db::list_deliveries_pg(&pool, &message.id, 10, None)
        .await
        .expect("read live expired delivery");
    assert_eq!(deliveries[0].status, "running");
    assert_eq!(deliveries[0].fallback_outbox_id, None);

    pool.close().await;
    pg_db.drop().await;
}
