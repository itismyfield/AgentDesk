use super::*;

async fn seed_receipt(
    pool: &sqlx::PgPool,
    channel: ChannelId,
    provider: &str,
    sources: &[u64],
    disposition: &str,
) -> i64 {
    let sources: Vec<String> = sources.iter().map(u64::to_string).collect();
    sqlx::query_scalar(
        r#"INSERT INTO intake_outbox (
          target_instance_id, forwarded_by_instance_id, channel_id, user_msg_id,
          request_owner_id, user_text, turn_kind, agent_id, provider, status,
          replay_only, replay_disposition, replay_source_message_ids,
          replay_episode_nonce, replay_owner_incarnation, replay_request_hash,
          replay_request_key, replay_hold_reason, replay_preserved)
         VALUES ('held-owner', 'original-owner', $1, $2, '4350',
          'original absorbed request', 'foreground', 'held-agent', $3, 'unknown',
          TRUE, $4, $5, 'held-episode', 'held-incarnation', 'request-hash', $6, 'execution evidence',
          '{"partial_body":"already produced","delivery_debt":true}'::jsonb)
         RETURNING id"#,
    )
    .bind(channel.get().to_string())
    .bind(sources.last().expect("receipt needs representative source"))
    .bind(provider)
    .bind(disposition)
    .bind(&sources)
    .bind(uuid::Uuid::new_v4().to_string())
    .fetch_one(pool)
    .await
    .expect("seed durable replay receipt")
}

async fn assert_original_kept(pool: &sqlx::PgPool, receipt: i64, disposition: &str) {
    let kept: (String, String, String, serde_json::Value) = sqlx::query_as(
        "SELECT user_text, replay_disposition, replay_hold_reason, replay_preserved
         FROM intake_outbox WHERE id = $1",
    )
    .bind(receipt)
    .fetch_one(pool)
    .await
    .expect("original receipt and partial output remain");
    assert_eq!(
        kept,
        (
            "original absorbed request".to_string(),
            disposition.to_string(),
            "execution evidence".to_string(),
            serde_json::json!({"partial_body":"already produced","delivery_debt":true}),
        )
    );
}

#[tokio::test(flavor = "current_thread")]
async fn replay_live_absorbed_sources_cannot_forward_and_keep_original_pg() {
    let _env = ScopedIntakeTestEnv::enforce();
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    crate::db::replay_disposition::tests::apply_test_mutant(&pool).await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let http = Arc::new(serenity::Http::new("Bot replay-admission-test"));
    let deps = deps(&http, &shared);

    for (offset, disposition) in [
        "started_unclassified",
        "startup_failed_no_effect",
        "withheld",
    ]
    .iter()
    .enumerate()
    {
        let channel = ChannelId::new(6_008_100 + offset as u64);
        let source_a = 6_008_200 + 10 * offset as u64;
        let source_b = source_a + 1;
        seed_foreign_owner(&pool, channel, &format!("replay-worker-{offset}")).await;
        let receipt = seed_receipt(
            &pool,
            channel,
            " Claude ",
            &[source_a, source_b],
            disposition,
        )
        .await;
        // The representative source is insufficient: a replay of the earlier absorbed source is held.
        let submission = submission_for_admission(channel, source_a);
        let admission = super::super::admit_text_intake(&deps, &submission).await;
        assert!(matches!(admission, IntakeAdmission::ConsumedToHold));
        super::super::finish_text_intake_admission(&deps, admission, submission)
            .await
            .expect("held live admission is consumed without provider execution");

        let mut mixed = submission_for_admission(channel, source_b + 1);
        mixed.request.source_message_ids =
            vec![MessageId::new(source_a), mixed.request.user_msg_id];
        assert!(matches!(
            super::super::admit_text_intake(&deps, &mixed).await,
            IntakeAdmission::ReplayDeferred(_)
        ));
        // With replay starts active a mixed input is undecided: it waits whole in the queue.
        let _active = super::super::replay_admission::test_activation::activate(channel.get());
        let admission = super::super::admit_text_intake(&deps, &mixed).await;
        assert!(matches!(admission, IntakeAdmission::ReplayDeferred(_)));
        super::super::finish_text_intake_admission(&deps, admission, mixed)
            .await
            .expect("the deferred mixed input is in custody");
        let queue = shared.mailbox(channel).snapshot().await.intervention_queue;
        let kept: Vec<_> = queue
            .iter()
            .map(|item| {
                (
                    item.message_id.get(),
                    item.source_message_ids.clone(),
                    item.text.clone(),
                )
            })
            .collect();
        let sources = vec![MessageId::new(source_b + 1), MessageId::new(source_a)];
        assert_eq!(
            kept,
            vec![(source_b + 1, sources, "admission policy".to_string())]
        );
        let route_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE channel_id = $1 AND NOT replay_only",
        )
        .bind(channel.get().to_string())
        .fetch_one(&pool)
        .await
        .expect("read live routes");
        assert_eq!(
            route_count, 0,
            "held or mixed live sources must never forward"
        );
        assert_original_kept(&pool, receipt, disposition).await;
    }
    assert!(shared.core.lock().await.sessions.is_empty());
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn replay_admission_dormant_and_other_identity_keeps_local_policy_pg() {
    let _env = ScopedIntakeTestEnv::with_mode("disabled");
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    crate::db::replay_disposition::tests::apply_test_mutant(&pool).await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let http = Arc::new(serenity::Http::new("Bot replay-admission-test"));
    let deps = deps(&http, &shared);
    let channel = ChannelId::new(6_008_301);
    let source = 6_008_302;
    let mut submission = submission_for_admission(channel, source);
    assert!(matches!(
        super::super::admit_text_intake(&deps, &submission).await,
        IntakeAdmission::Local(_)
    ));
    let before: i64 = sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM intake_outbox")
        .fetch_one(&pool)
        .await
        .expect("dormant intake count");
    assert_eq!(before, 0, "read-side admission must not create receipts");

    let receipt = seed_receipt(&pool, channel, "claude", &[source], "withheld").await;
    submission.provider = ProviderKind::Codex;
    assert!(matches!(
        super::super::admit_text_intake(&deps, &submission).await,
        IntakeAdmission::Local(_)
    ));
    submission.provider = ProviderKind::Claude;
    submission.request.channel_id = ChannelId::new(channel.get() + 1);
    assert!(matches!(
        super::super::admit_text_intake(&deps, &submission).await,
        IntakeAdmission::Local(_)
    ));
    submission.request.channel_id = channel;
    submission.request.user_msg_id = MessageId::new(source + 1);
    assert!(matches!(
        super::super::admit_text_intake(&deps, &submission).await,
        IntakeAdmission::Local(_),
    ));
    assert_original_kept(&pool, receipt, "withheld").await;
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn replay_queued_whole_consumed_and_mixed_original_restored_pg() {
    let _env = ScopedIntakeTestEnv::enforce();
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    crate::db::replay_disposition::tests::apply_test_mutant(&pool).await;
    let channel = ChannelId::new(6_008_401);
    let source_a = 6_008_402;
    let source_b = 6_008_403;
    seed_foreign_owner(&pool, channel, "replay-queue-worker").await;
    let receipt = seed_receipt(&pool, channel, "claude", &[source_a, source_b], "withheld").await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let http = Arc::new(serenity::Http::new("Bot replay-admission-test"));
    let deps = deps(&http, &shared);
    let persistence = crate::services::discord::queue_persistence_context(
        &shared,
        &ProviderKind::Claude,
        channel,
    );
    let mut whole = queued_intervention(source_b, Vec::new());
    whole.source_message_ids = vec![MessageId::new(source_a), MessageId::new(source_b)];
    let mut mixed = queued_intervention(source_b + 1, Vec::new());
    mixed.source_message_ids = vec![MessageId::new(source_a), mixed.message_id];
    mixed.text = "held original plus brand new instruction".to_string();
    mixed.reply_context = Some("original reply context".to_string());

    for (mut item, wholly_held) in [(whole, true), (mixed, false)] {
        // Real merged human inputs carry the existing preservation marker for every absorbed source.
        item.source_message_queued_generations = item
            .source_message_ids
            .iter()
            .copied()
            .map(|source| {
                crate::services::turn_orchestrator::SourceMessageQueuedGeneration::user_instruction(
                    source,
                    item.queued_generation,
                )
            })
            .collect();
        shared
            .mailbox(channel)
            .replace_queue(vec![item.clone()], persistence.clone())
            .await;
        let dequeued = shared
            .mailbox(channel)
            .take_next_soft(persistence.clone())
            .await;
        let intervention = dequeued.intervention.expect("actual queued item dequeued");
        let disposition = admit_queued_intake(
            &deps,
            ProviderKind::Claude,
            channel,
            &intervention,
            intervention.author_id,
            "replay-source-owner".to_string(),
            false,
            false,
            "replay_admission_test",
            dequeued.dispatch_lease,
        )
        .await;
        let snapshot = shared.mailbox(channel).snapshot().await;
        if wholly_held {
            assert!(matches!(
                disposition,
                QueuedAdmissionDisposition::ConsumedToHold
            ));
            assert!(snapshot.intervention_queue.is_empty());
            assert_eq!(
                shared
                    .restart
                    .deferred_hook_backlog
                    .load(std::sync::atomic::Ordering::Relaxed),
                0
            );
        } else {
            assert!(matches!(disposition, QueuedAdmissionDisposition::Deferred));
            assert_eq!(snapshot.intervention_queue.len(), 1);
            let restored = &snapshot.intervention_queue[0];
            assert_eq!(restored.message_id, item.message_id);
            assert_eq!(restored.source_message_ids, item.source_message_ids);
            assert_eq!(restored.text, item.text);
            assert_eq!(restored.reply_context, item.reply_context);
            let generations = |item: &Intervention| {
                item.source_message_queued_generations
                    .iter()
                    .map(|source| {
                        (
                            source.message_id,
                            source.queued_generation,
                            source.enqueued_at_epoch_us,
                            source.preserve_on_cancel,
                        )
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(generations(restored), generations(&item));
        }
    }
    assert_original_kept(&pool, receipt, "withheld").await;
    assert!(shared.core.lock().await.sessions.is_empty());
    let rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE channel_id=$1")
            .bind(channel.get().to_string())
            .fetch_one(&pool)
            .await
            .expect("count queued routes");
    assert_eq!(
        rows, 1,
        "whole and mixed held sources must not create provider routes"
    );
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn p5_off_intake_custody_consumes_every_prior_state_pg() {
    let _env = ScopedIntakeTestEnv::with_mode("disabled");
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let http = Arc::new(serenity::Http::new("Bot replay-admission-test"));
    let deps = deps(&http, &shared);
    for (n, state) in [
        "registered_not_started",
        "started_unclassified",
        "startup_failed_no_effect",
        "withheld",
        "classified_normal",
    ]
    .into_iter()
    .enumerate()
    {
        let channel = ChannelId::new(7_330_510 + n as u64);
        let source = 7_331_510 + n as u64;
        let id = seed_receipt(&pool, channel, "claude", &[source], state).await;
        let key = format!("discord:claude:{channel}:{source}");
        sqlx::query("UPDATE intake_outbox SET replay_request_key=$2 WHERE id=$1")
            .bind(id)
            .bind(key)
            .execute(&pool)
            .await
            .unwrap();
        let submission = submission_for_admission(channel, source);
        let admission = super::super::admit_text_intake(&deps, &submission).await;
        if state == "registered_not_started" {
            assert!(matches!(admission, IntakeAdmission::ReplayDeferred(_)));
        } else {
            assert!(
                matches!(admission, IntakeAdmission::ConsumedToHold),
                "{state}"
            );
        }
        let mut worker = request(channel, source, "admission policy");
        worker.intake_outbox_id = Some(id);
        let result = crate::services::discord::router::message_handler::execute_intake_turn_core(
            &http,
            &shared,
            "test-token",
            worker,
            Vec::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            if state == "registered_not_started" {
                crate::services::discord::router::IntakeExecution::Deferred
            } else {
                crate::services::discord::router::IntakeExecution::ConsumedProtected
            }
        );
        assert_original_kept(&pool, id, state).await;
    }
    assert!(shared.core.lock().await.sessions.is_empty());
    pool.close().await;
    fixture.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn p5_off_intake_query_error_notifies_and_preserves_retry_pg() {
    let _env = ScopedIntakeTestEnv::with_mode("disabled");
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let channel = ChannelId::new(7_330_516);
    let discord =
        crate::services::discord::recovery_engine::o_cut_recorder::start(channel.get()).await;
    let deps = deps(&discord.http, &shared);
    sqlx::query("ALTER TABLE intake_outbox RENAME TO p5_intake_unavailable")
        .execute(&pool)
        .await
        .unwrap();
    let submission = submission_for_admission(channel, 7_331_516);
    let admission = super::super::admit_text_intake(&deps, &submission).await;
    assert!(matches!(
        admission,
        IntakeAdmission::ReplayDeferred(super::super::replay_admission::ReplayDefer::LookupFailed)
    ));
    let result = super::super::finish_text_intake_admission(&deps, admission, submission).await;
    assert_eq!(
        discord.contents().len(),
        1,
        "a failed lookup cannot silently discard the input"
    );
    assert!(shared.core.lock().await.sessions.is_empty());
    sqlx::query("ALTER TABLE p5_intake_unavailable RENAME TO intake_outbox")
        .execute(&pool)
        .await
        .unwrap();
    assert!(matches!(
        super::super::admit_text_intake(&deps, &submission_for_admission(channel, 7_331_516)).await,
        IntakeAdmission::Local(_)
    ));
    println!("P5 OFF_QUERY_ERROR visible_notices=1 retry_custody={result:?}");
    pool.close().await;
    fixture.drop().await;
}
