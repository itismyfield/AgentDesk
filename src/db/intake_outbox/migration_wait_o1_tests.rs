use super::*;

#[tokio::test]
async fn stale_intent_reclassifies_only_the_original_unsubmitted_claim_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(1).await;
    let (row, intent) = setup(&pool, 75030701, true).await;
    let before = snapshot(&pool, row.id).await;
    sqlx::query("UPDATE sessions SET raw_provider_session_id='22222222-2222-4222-8222-222222222222' WHERE id=$1")
        .bind(intent.selector.canonical_row).execute(&pool).await.unwrap();
    let result = park_before_effects(&pool, BeforeEffects::for_test(&row), &intent)
        .await
        .unwrap();
    let request = match result {
        ParkOutcome::NeedsReclassification(request) => request,
        other => panic!(
            "a stale intent must retain the original proof for its chosen consumer: {other:?}"
        ),
    };
    let result = reclassify_before_effects(&pool, request).await.unwrap();
    assert_eq!(
        result,
        ParkOutcome::Reclassified,
        "a stale intent must return its still-unsubmitted original claim to reclassification"
    );
    let after = snapshot(&pool, row.id).await;
    assert_eq!(after["status"], "pending");
    assert!(
        after["last_error"].is_null(),
        "reclassification cannot issue the old intent a Waiting marker"
    );
    assert_eq!(payload_image(&after), payload_image(&before));
    assert!(
        fenced_waiting(&pool, &intent).await.is_err(),
        "the old selector cannot exempt reclassified input"
    );
    let claimed = crate::db::intake_outbox::claim_pending_for_target_except(
        &pool,
        "fence-worker",
        "claude",
        "reclassified-next-claim",
        &[],
    )
    .await
    .unwrap()
    .expect("the same original must be reclaimable");
    assert_eq!(
        claimed.id, row.id,
        "reclassification returns the original ID to the actual poller"
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM intake_outbox WHERE channel_id=$1")
        .bind(&row.channel_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn fenced_composite_reads_foreign_and_queued_facts_on_its_single_connection_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(1).await;
    let (row, intent) = setup(&pool, 75030702, false).await;
    pending_row(&pool, row.id).await;
    sqlx::query("INSERT INTO sessions(session_key,provider,status,channel_id,instance_id) VALUES('foreign-composite','claude','idle',$1,'foreign-worker')")
        .bind(&row.channel_id).execute(&pool).await.unwrap();
    let target = format!("channel:{}", row.channel_id);
    let posted = crate::services::message_outbox::enqueue_outbox_pg_returning_id_with_ttl(
        &pool,
        crate::services::message_outbox::OutboxMessage {
            target: &target,
            content: "prepared Legacy body",
            bot: "captain",
            source: "headless_turn",
            reason_code: None,
            session_key: None,
        },
        0,
    )
    .await
    .unwrap();
    assert!(
        posted.is_some(),
        "the actual body producer must create the queued responsibility"
    );
    let fence = tokio::time::timeout(Duration::from_secs(3), fenced_waiting(&pool, &intent))
        .await
        .expect("the composite must not re-enter a one-slot pool")
        .unwrap();
    let observed = (fence.foreign_sessions(), fence.queued_bodies());
    fence.release().await.unwrap();
    assert_eq!(
        observed,
        (1, 1),
        "the original transaction must retain foreign and queued-body responsibilities"
    );
    pool.close().await;
    db.drop().await;
}

async fn register(pool: &PgPool, row: &IntakeOutboxRow, user: u64, sources: Vec<u64>) -> i64 {
    use crate::db::replay_disposition::write::{CanonicalInput, register_or_reuse};
    let mut input = CanonicalInput::discord(
        "claude",
        row.channel_id.parse().unwrap(),
        user,
        sources,
        &row.user_text,
    );
    input.owner_id = row.request_owner_id.clone();
    input.agent_id = row.agent_id.clone();
    input.attachments = row.attachment_refs.clone();
    input.reply_context = row.reply_context.clone();
    register_or_reuse(pool, &input, "fence-worker")
        .await
        .unwrap()
        .id
}

#[tokio::test]
async fn replay_census_keeps_only_exact_related_registration_as_waiting_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(1).await;
    let channel = 75030703;
    let (row, intent) = setup(&pool, channel, false).await;
    pending_row(&pool, row.id).await;
    let exact = register(&pool, &row, channel, vec![channel, channel + 1]).await;
    let unrelated = register(&pool, &row, channel + 20, vec![channel + 20]).await;
    let fence = fenced_waiting(&pool, &intent).await.unwrap();
    let approvals = fence.replay_registrations().to_vec();
    let blocked: Vec<_> = fence
        .blockers()
        .iter()
        .map(|r| (r.row_id, r.reason.clone()))
        .collect();
    let waiting: Vec<_> = fence
        .waiting()
        .requests()
        .iter()
        .map(|r| r.row_id)
        .collect();
    fence.release().await.unwrap();
    assert_eq!(waiting, vec![row.id]);
    assert_eq!(
        approvals,
        vec![exact],
        "only a registered receipt proven against its exact waiting original is exempt"
    );
    assert_eq!(
        blocked,
        vec![(unrelated, BlockReason::Replay)],
        "an unrelated replay-only unknown row cannot disappear from the responsibility census"
    );
    pool.close().await;
    db.drop().await;
}

async fn stale(pool: &PgPool, row: &IntakeOutboxRow, intent: &WaitIntent) -> Box<Reclassification> {
    sqlx::query("UPDATE sessions SET raw_provider_session_id='33333333-3333-4333-8333-333333333333' WHERE id=$1")
        .bind(intent.selector.canonical_row).execute(pool).await.unwrap();
    match park_before_effects(pool, BeforeEffects::for_test(row), intent)
        .await
        .unwrap()
    {
        ParkOutcome::NeedsReclassification(request) => request,
        other => panic!("stale proof must remain owned until the chosen consumer runs: {other:?}"),
    }
}

#[tokio::test]
async fn reclassification_cannot_bypass_a_changed_claim_owner_payload_or_effect_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(1).await;
    for (index, change) in [
        "claim_owner='successor'",
        "owner_instance_id='foreign'",
        "attachment_refs='[]'::JSONB",
        "dispatched_at=NOW()",
        "created_at=created_at+INTERVAL '1 millisecond'",
    ]
    .iter()
    .enumerate()
    {
        let (row, intent) = setup(&pool, 75030710 + index as u64, true).await;
        let request = stale(&pool, &row, &intent).await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE intake_outbox SET {change} WHERE id=$1"
        )))
        .bind(row.id)
        .execute(&pool)
        .await
        .unwrap();
        let before = snapshot(&pool, row.id).await;
        let result = reclassify_before_effects(&pool, request).await;
        assert!(
            matches!(result, Ok(ParkOutcome::Blocked(_)) | Err(_)),
            "reclassification cannot replace changed custody or a possible provider effect: {change}"
        );
        assert_eq!(snapshot(&pool, row.id).await, before);
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn reclassification_commit_unknown_is_never_a_waiting_permission_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(1).await;
    let (row, intent) = setup(&pool, 75030720, true).await;
    let before = snapshot(&pool, row.id).await;
    let request = stale(&pool, &row, &intent).await;
    let result = with_lost_commit_ack(reclassify_before_effects(&pool, request))
        .await
        .unwrap();
    assert!(
        matches!(result, ParkOutcome::Unknown(_)),
        "an unacknowledged reclassification cannot claim successful waiting custody"
    );
    assert!(fenced_waiting(&pool, &intent).await.is_err());
    let after = snapshot(&pool, row.id).await;
    assert!(after["last_error"].is_null());
    assert_eq!(payload_image(&after), payload_image(&before));
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn failed_preaccept_spelling_cannot_hide_an_unsettled_provider_clock_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(1).await;
    let (row, intent) = setup(&pool, 75030730, false).await;
    pending_row(&pool, row.id).await;
    let older: i64 = sqlx::query_scalar("INSERT INTO intake_outbox(target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,request_owner_id,user_text,turn_kind,agent_id,provider,status,accepted_at)
        VALUES('fence-worker','leader',$1,'75030750','100','prior accepted request','standard','fence-agent','claude','failed_pre_accept',NOW()) RETURNING id")
        .bind(&row.channel_id).fetch_one(&pool).await.unwrap();
    let before = snapshot(&pool, older).await;
    let fence = fenced_waiting(&pool, &intent).await.unwrap();
    let blocked: Vec<_> = fence.blockers().iter().map(|r| r.row_id).collect();
    assert_eq!(fence.waiting().requests().len(), 1);
    fence.release().await.unwrap();
    assert_eq!(
        blocked,
        vec![older],
        "failed_pre_accept cannot erase a prior accepted/spawned/dispatched responsibility"
    );
    assert_eq!(snapshot(&pool, older).await, before);
    pool.close().await;
    db.drop().await;
}
