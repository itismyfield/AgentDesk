use super::*;
use crate::db::relay_dead_letter::{self as dlq, RelayDeadLetterRecord};
use crate::services::discord::outbound::{completed_turn_ledger, delivery_record};

fn record(channel: u64, message: u64) -> RelayDeadLetterRecord {
    RelayDeadLetterRecord {
        kind: dlq::KIND_CATCH_UP_TOO_OLD.into(),
        channel_id: channel.to_string(),
        author_id: Some(HUMAN_ID.to_string()),
        message_id: Some(message.to_string()),
        content: "unanswered input".into(),
        reason: "age_secs=400 > max_age_secs=300".into(),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn i7441_existing_duplicate_rows_and_states_are_unchanged_pg() {
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "i7441_legacy",
        "existing rows untouched",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    let row = record(7441, 1);
    let mut ids = Vec::new();
    for state in ["declined", "pending"] {
        let id: i64 = sqlx::query_scalar("INSERT INTO relay_dead_letter (kind, channel_id, author_id, message_id, content, reason, redelivery_state) VALUES ($1,$2,$3,$4,$5,$6,$7) RETURNING id")
            .bind(&row.kind).bind(&row.channel_id).bind(&row.author_id).bind(&row.message_id).bind(&row.content).bind(state).bind(state).fetch_one(&pool).await.unwrap();
        ids.push(id);
    }
    let before: Vec<String> =
        sqlx::query_scalar("SELECT to_jsonb(d)::text FROM relay_dead_letter d ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(dlq::insert(&pool, &row).await.unwrap(), ids[0]);
    let after: Vec<String> =
        sqlx::query_scalar("SELECT to_jsonb(d)::text FROM relay_dead_letter d ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(
        before, after,
        "existing duplicate ids, payloads, timestamps and states must survive"
    );
    let mut fresh = row.clone();
    fresh.message_id = Some("2".into());
    assert!(!ids.contains(&dlq::insert(&pool, &fresh).await.unwrap()));
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM relay_dead_letter")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 3);
    pool.close().await;
    db.drop().await;
}

pub(in crate::services::discord) async fn assert_aged_delivery_settled(
    root: &std::path::Path,
    shared: &Arc<super::super::SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    message: MessageId,
) {
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(OWNER_ID);
        settings.allow_all_users = true;
    }
    write_checkpoint(root, provider, channel, message.get() - 1);
    let api = TestCatchUpApi::new(vec![discord_message(
        channel,
        message,
        HUMAN_ID,
        false,
        "already answered input",
    )]);
    run_catch_up_sweep(CatchUpDeps::new(&api, shared, provider)).await;
    assert!(
        api.dead_letters.lock().unwrap().is_empty(),
        "answered input became TooOld"
    );
    assert!(
        super::super::super::mailbox_snapshot(shared, channel)
            .await
            .intervention_queue
            .is_empty()
    );
    let settled = completed_turn_ledger::settled_user_msg_ids(provider, channel.get());
    assert!(
        settled.contains(&message.get()),
        "actual delivery must produce settlement evidence"
    );
    let mut input = view(HUMAN_ID, false, 400, "already answered input");
    input.message_id = message.get();
    assert_eq!(
        classify_catch_up_message(
            &input,
            Some(CURRENT_BOT_ID),
            &HashSet::new(),
            &settled,
            300,
            &[],
            None,
            None
        ),
        CatchUpClassification::Settled
    );
}

#[tokio::test(flavor = "current_thread")]
async fn i7441_repeated_too_old_sweep_one_row_pg() {
    let root = scoped_runtime_root();
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "i7441_repeat",
        "repeat TooOld",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    let shared = super::super::super::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(7_441_001);
    let message = message_id_with_age(1, Duration::from_secs(400));
    write_checkpoint(root.path(), &provider, channel, message.get() - 1);
    {
        let mut settings = shared.settings.write().await;
        settings.owner_user_id = Some(HUMAN_ID);
    }
    for _ in 0..2 {
        let api = TestCatchUpApi::new(vec![discord_message(
            channel,
            message,
            HUMAN_ID,
            false,
            "unanswered input",
        )]);
        run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM relay_dead_letter WHERE kind=$1 AND channel_id=$2 AND message_id=$3",
    )
    .bind(dlq::KIND_CATCH_UP_TOO_OLD)
    .bind(channel.to_string())
    .bind(message.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1, "one input must have one durable loss record");
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn i7441_concurrent_too_old_insert_preserves_first_row_pg() {
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "i7441_concurrent",
        "concurrent TooOld",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    let options = pool.connect_options().as_ref().clone();
    pool.close().await;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .min_connections(12)
        .max_connections(16)
        .connect_with(options)
        .await
        .unwrap();
    assert!(
        pool.size() >= 12,
        "concurrent writers need separate PG connections"
    );
    let row = record(7441, 1);
    let tasks = (0..12).map(|_| dlq::insert(&pool, &row));
    let ids = futures::future::join_all(tasks)
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(
        ids.iter().all(|id| *id == ids[0]),
        "concurrent inserts returned different rows: {ids:?}"
    );
    let mut changed = row.clone();
    changed.content = "edited input".into();
    changed.reason = "exhausted hold expired".into();
    sqlx::query("UPDATE relay_dead_letter SET redelivery_state='declined' WHERE id=$1")
        .bind(ids[0])
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(dlq::insert(&pool, &changed).await.unwrap(), ids[0]);
    let content: String = sqlx::query_scalar("SELECT content FROM relay_dead_letter WHERE id=$1")
        .bind(ids[0])
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(content, row.content);
    changed.channel_id = "7442".into();
    assert_ne!(dlq::insert(&pool, &changed).await.unwrap(), ids[0]);
    changed = row.clone();
    changed.kind = dlq::KIND_QUEUE_OVERFLOW.into();
    assert_ne!(
        dlq::insert(&pool, &changed).await.unwrap(),
        dlq::insert(&pool, &changed).await.unwrap()
    );
    let mut lock = pool.begin().await.unwrap();
    let key = format!(
        "{}:{}{}:{}",
        row.channel_id.len(),
        row.channel_id,
        row.message_id.as_ref().unwrap().len(),
        row.message_id.as_ref().unwrap()
    );
    sqlx::query("SELECT pg_advisory_xact_lock(7441, hashtext($1))")
        .bind(key)
        .execute(&mut *lock)
        .await
        .unwrap();
    assert_eq!(
        dlq::record_too_old(Some(&pool), row.clone()).await,
        None,
        "lock timeout must fail closed"
    );
    lock.rollback().await.unwrap();
    pool.close().await;
    assert_eq!(dlq::record_too_old(Some(&pool), row.clone()).await, None);
    assert_eq!(dlq::record_too_old(None, row).await, None);
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn i7441_failed_dlq_preserves_frontier_and_retry() {
    let root = scoped_runtime_root();
    let shared = super::super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(7_441_004);
    let old = message_id_with_age(4, Duration::from_secs(400));
    let fresh = message_id_with_age(5, Duration::from_secs(30));
    write_checkpoint(root.path(), &provider, channel, old.get() - 1);
    shared.settings.write().await.owner_user_id = Some(HUMAN_ID);
    let mut api = TestCatchUpApi::new(vec![
        discord_message(channel, old, HUMAN_ID, false, "unpreserved"),
        discord_message(channel, fresh, HUMAN_ID, false, "later input"),
    ]);
    api.fail_dead_letter = true;
    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;
    assert_eq!(
        super::super::scan_progress::read(&provider, channel),
        Some(old.get() - 1)
    );
    assert!(
        shared
            .catch_up_retry_pending
            .get(&channel)
            .unwrap()
            .checkpoint
            < old.get()
    );
    assert!(
        super::super::super::mailbox_snapshot(&shared, channel)
            .await
            .intervention_queue
            .is_empty()
    );
}

#[tokio::test(flavor = "current_thread")]
async fn i7441_aged_consumed_command_is_settled_without_completion() {
    let root = scoped_runtime_root();
    let shared = super::super::super::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(7_441_003);
    let message = message_id_with_age(3, Duration::from_secs(400));
    write_checkpoint(root.path(), &provider, channel, message.get() - 1);
    shared.settings.write().await.owner_user_id = Some(HUMAN_ID);
    super::super::consumed_commands::record(&provider, channel, message).await;
    let api = TestCatchUpApi::new(vec![discord_message(
        channel, message, HUMAN_ID, false, "!stop",
    )]);
    run_catch_up_sweep(CatchUpDeps::new(&api, &shared, &provider)).await;
    assert!(
        api.dead_letters.lock().unwrap().is_empty(),
        "consumed command must not become TooOld"
    );
    assert!(completed_turn_ledger::settled_user_msg_ids(&provider, channel.get()).is_empty());
}

#[test]
fn i7441_pinned_delivery_preserves_episode_nonce_for_absorbed_input() {
    let _root = scoped_runtime_root();
    let provider = ProviderKind::Codex;
    completed_turn_ledger::record_merged_alias(&provider, 7441, 10, "captured-nonce", &[9]);
    delivery_record::record_pinned_delivery_metadata(
        &delivery_record::ExactJsonlSourceIdentity {
            provider: "codex".into(),
            tmux_session_name: "pinned-delivery".into(),
            turn_nonce: "captured-nonce".into(),
            range: (10, 20),
            generation_mtime_ns: 1,
            offset_authority_channel_id: 7440,
            delivery_channel_id: 7441,
        },
        "answer",
        10,
    );
    assert_eq!(
        completed_turn_ledger::settled_user_msg_ids(&provider, 7441),
        HashSet::from([9, 10])
    );
    assert!(completed_turn_ledger::settled_user_msg_ids(&provider, 7440).is_empty());
}

#[tokio::test(flavor = "current_thread")]
async fn i7441_awaited_too_old_existing_prunes_retention_pg() {
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "i7441_retention",
        "awaited retention",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    let row = record(7441, 1);
    dlq::insert(&pool, &row).await.unwrap();
    let mut old = row.clone();
    old.kind = dlq::KIND_QUEUE_OVERFLOW.into();
    dlq::insert(&pool, &old).await.unwrap();
    sqlx::query("UPDATE relay_dead_letter SET created_at=NOW()-INTERVAL '31 days' WHERE kind=$1")
        .bind(dlq::KIND_QUEUE_OVERFLOW)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        dlq::record_too_old(Some(&pool), row.clone()).await,
        Some(dlq::TooOldRecordOutcome::Existing)
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM relay_dead_letter WHERE kind=$1")
                    .bind(dlq::KIND_QUEUE_OVERFLOW)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            if count == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("awaited duplicate recording must still run retention");
    pool.close().await;
    db.drop().await;
}
