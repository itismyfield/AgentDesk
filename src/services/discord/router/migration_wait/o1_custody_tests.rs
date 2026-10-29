#[tokio::test(flavor = "current_thread")]
async fn migration_distinct_same_body_sources_keep_durable_fifo_pg() {
    same_body_sources_keep_durable_fifo(false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn migration_whole_original_witness_prevents_cross_request_merge_pg() {
    same_body_sources_keep_durable_fifo(true).await;
}

use super::*;

async fn same_body_sources_keep_durable_fifo(merge: bool) {
    let mut f = Fixture::new("disabled").await;
    let (db, pool) = attach_pool(&mut f).await;
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")]);
    let mut a = submission(8_917_080, IntakeOrigin::LiveMessage);
    a.request.user_text = "same original body".into();
    a.request.merge_consecutive = merge;
    let mut b = submission(8_917_081, IntakeOrigin::LiveMessage);
    b.request.user_text = a.request.user_text.clone();
    b.request.merge_consecutive = merge;
    let wait = intent(&f, &a.request).await;
    let at = std::time::Instant::now();
    let mut first = Capability::new(a.request.clone(), wait.clone());
    first.original_created_at = Some(at);
    let first_original = first.original_observed.clone();
    let mut second = Capability::new(b.request.clone(), wait);
    second.original_created_at = Some(at + std::time::Duration::from_millis(1));
    let second_original = second.original_observed.clone();
    let checkpoints = Arc::new(AtomicUsize::new(0));
    let first_checkpoint = checkpoints.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let _hooks = final_spy(calls.clone());
    staged::scope(
        first,
        intake::dispatch_live_text_intake(&f.deps(), a, move || {
            first_checkpoint.fetch_add(1, Ordering::SeqCst);
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        f.disk().len(),
        1,
        "the first original is durably parked before its same-body successor"
    );
    let second_checkpoint = checkpoints.clone();
    let disk_pair = Arc::new(AtomicBool::new(false));
    let pair_seen = disk_pair.clone();
    let token = f.shared.token_hash.clone();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        staged::scope(
            second,
            intake::dispatch_live_text_intake(&f.deps(), b, move || {
                let disk = crate::services::turn_orchestrator::load_channel_pending_queue_checked(
                    &ProviderKind::Claude,
                    &token,
                    ChannelId::new(discord_mock::CHANNEL_ID),
                )
                .unwrap()
                .0;
                pair_seen.store(
                    disk.iter().map(|i| i.message_id.get()).collect::<Vec<_>>()
                        == vec![8_917_080, 8_917_081],
                    Ordering::SeqCst,
                );
                second_checkpoint.fetch_add(1, Ordering::SeqCst);
            }),
        ),
    )
    .await;
    let disk = f.disk();
    let observed = f
        .shared
        .mailbox(ChannelId::new(discord_mock::CHANNEL_ID))
        .snapshot()
        .await;
    pool.close().await;
    db.drop().await;
    assert_eq!(
        disk.iter().map(|i| i.message_id.get()).collect::<Vec<_>>(),
        vec![8_917_080, 8_917_081],
        "migration keeps distinct same-body source custody instead of content dedup"
    );
    assert!(
        matches!(result, Ok(Ok(_))),
        "source-preserving enqueue finishes without draining A"
    );
    assert_eq!(checkpoints.load(Ordering::SeqCst), 2);
    assert!(
        disk_pair.load(Ordering::SeqCst),
        "both original sources are durable before B checkpoint"
    );
    assert_original(&disk[0], first_original.lock().unwrap().as_ref().unwrap());
    assert_original(&disk[1], second_original.lock().unwrap().as_ref().unwrap());
    assert_eq!(observed.intervention_queue[0].created_at, at);
    assert_eq!(
        observed.intervention_queue[1].created_at,
        at + std::time::Duration::from_millis(1)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn migration_local_witness_is_issued_only_after_real_durable_ack_pg() {
    let mut f = Fixture::new("disabled").await;
    let (db, pool) = attach_pool(&mut f).await;
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")]);
    let input = submission(8_917_082, IntakeOrigin::LiveMessage);
    let wait = intent(&f, &input.request).await;
    let cap = Capability::new(input.request.clone(), wait.clone());
    let witnesses = cap.witnesses.clone();
    let original = cap.original_observed.clone();
    let called = Arc::new(AtomicUsize::new(0));
    let hook_calls = called.clone();
    let _hooks = final_spy(called);
    let checkpoints = Arc::new(AtomicUsize::new(0));
    let advanced = checkpoints.clone();
    staged::scope(
        cap,
        intake::dispatch_live_text_intake(&f.deps(), input, move || {
            advanced.fetch_add(1, Ordering::SeqCst);
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        f.disk().len(),
        1,
        "the actual producer durably owns its original input"
    );
    let witnessed = witnesses.lock().unwrap().clone();
    assert_eq!(
        witnessed.len(),
        1,
        "only the actual durable ACK producer issues a local custody witness"
    );
    let committed = Arc::new(AtomicUsize::new(0));
    let count = committed.clone();
    let fence = custody::fenced_waiting(&pool, &wait).await.unwrap();
    let publication = crate::services::turn_orchestrator::migration_wait::PublicationRequest::new(
        fence,
        witnessed.clone(),
        QueuePersistenceContext::new(&ProviderKind::Claude, &f.shared.token_hash, None),
        move || {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    );
    let published = f
        .shared
        .mailbox(ChannelId::new(discord_mock::CHANNEL_ID))
        .publish_migration(publication)
        .await
        .unwrap();
    assert!(
        published.outcome().is_ok(),
        "the actual actor accepts only the real producer witness"
    );
    assert!(published.release().await.unwrap().is_ok());
    assert_eq!(
        committed.load(Ordering::SeqCst),
        1,
        "the actual actor publishes with its owned DB fence"
    );
    let original_item = original.lock().unwrap().as_ref().unwrap().clone();
    let persistence =
        QueuePersistenceContext::new(&ProviderKind::Claude, &f.shared.token_hash, None);
    let actor = f.shared.mailbox(ChannelId::new(discord_mock::CHANNEL_ID));
    let queue = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join(&f.shared.token_hash)
        .join(format!("{}.json", discord_mock::CHANNEL_ID));
    let original_bytes = std::fs::read(&queue).unwrap();
    let original_json: serde_json::Value = serde_json::from_slice(&original_bytes).unwrap();
    for variant in [
        "payload",
        "item-generation",
        "source-generation",
        "source-epoch",
    ] {
        let mut changed = original_json.clone();
        let items = if changed.is_array() {
            changed.as_array_mut().unwrap()
        } else {
            changed.get_mut("items").unwrap().as_array_mut().unwrap()
        };
        assert_eq!(
            items.len(),
            1,
            "the negative disk observation retains the actual producer's one original"
        );
        let item = &mut items[0];
        assert_eq!(
            item["message_id"].as_u64(),
            Some(original_item.message_id.get())
        );
        match variant {
            "payload" => item["text"] = serde_json::json!(format!("{}!", original_item.text)),
            "item-generation" => item["queued_generation"] = serde_json::json!(0),
            "source-generation" => {
                item["source_message_queued_generations"][0]["queued_generation"] =
                    serde_json::json!(0)
            }
            "source-epoch" => {
                item["source_message_queued_generations"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("enqueued_at_epoch_us");
            }
            _ => unreachable!(),
        }
        std::fs::write(&queue, serde_json::to_vec(&changed).unwrap()).unwrap();
        let count = committed.clone();
        let fence = custody::fenced_waiting(&pool, &wait).await.unwrap();
        let request = crate::services::turn_orchestrator::migration_wait::PublicationRequest::new(
            fence,
            witnessed.clone(),
            persistence.clone(),
            move || {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        );
        let published = actor.publish_migration(request).await.unwrap();
        let disk_outcome = published.release().await.unwrap();
        std::fs::write(&queue, &original_bytes).unwrap();
        if variant == "payload" {
            assert!(
                disk_outcome.is_err(),
                "the actual actor rejects an unobserved durable payload change"
            );
        } else {
            assert!(
                disk_outcome.is_err(),
                "raw missing queue metadata is never promoted into publication authority: {variant}"
            );
        }
        assert_eq!(
            committed.load(Ordering::SeqCst),
            1,
            "altered durable input never disappears from the composite publication: {variant}"
        );
    }
    assert_eq!(
        std::fs::read(&queue).unwrap(),
        original_bytes,
        "the raw disk negative controls restore every original byte before the real head control"
    );
    let claim = actor
        .claim_queued_head(
            crate::services::turn_orchestrator::QueueIdentity::of(&original_item),
            persistence.clone(),
        )
        .await;
    let crate::services::turn_orchestrator::QueuedHeadClaim::Reserved(ticket) = claim else {
        panic!("the real actor must reserve the original head for the negative publication control")
    };
    let count = committed.clone();
    let fence = custody::fenced_waiting(&pool, &wait).await.unwrap();
    let request = crate::services::turn_orchestrator::migration_wait::PublicationRequest::new(
        fence,
        witnessed.clone(),
        persistence.clone(),
        move || {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    );
    let refused = actor.publish_migration(request).await.unwrap();
    let outcome = refused.release().await.unwrap();
    let settled = actor
        .settle_queued_head(
            ticket,
            crate::services::turn_orchestrator::HeadOutcome::NotSent,
            persistence,
        )
        .await;
    assert!(
        matches!(
            settled,
            crate::services::turn_orchestrator::HeadSettle::Committed(_)
        ),
        "the original head remains owned and can be restored"
    );
    pool.close().await;
    db.drop().await;
    assert!(
        matches!(
            outcome,
            Err(crate::services::turn_orchestrator::migration_wait::Refusal::Head)
        ),
        "a reserved real head prevents publication despite a valid local witness"
    );
    assert_eq!(
        committed.load(Ordering::SeqCst),
        1,
        "the real head reservation cannot be exempted as waiting"
    );
    let original = original.lock().unwrap();
    let original = original.as_ref().unwrap();
    assert!(witnessed[0].intent_matches(&wait));
    assert!(
        !witnessed[0].namespace_matches(
            &QueuePersistenceContext::new(&ProviderKind::Claude, "foreign-queue", None),
            ChannelId::new(discord_mock::CHANNEL_ID)
        ),
        "a local witness cannot switch queue namespace"
    );

    assert!(witnessed[0].matches_exact(original));
    let mut changed = original.clone();
    changed.text.push('!');
    assert!(
        !witnessed[0].matches_exact(&changed),
        "a witness cannot approve a changed original payload"
    );
    changed = original.clone();
    changed.created_at += std::time::Duration::from_millis(1);
    assert!(
        !witnessed[0].matches_exact(&changed),
        "a witness cannot approve changed FIFO ownership"
    );
    assert_eq!(checkpoints.load(Ordering::SeqCst), 1);
    assert_eq!(hook_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn migration_selector_change_returns_same_worker_input_to_reclassification_pg() {
    use crate::services::cluster::intake_worker::{self as worker, test_executor};
    let mut f = Fixture::new("disabled").await;
    let _rest = crate::services::discord::shared_state::test_rest::install(f.http.clone());
    let (db, pool) = attach_pool(&mut f).await;
    let registry = Arc::new(crate::services::discord::health::HealthRegistry::new());
    Arc::get_mut(&mut f.shared).unwrap().health_registry = Arc::downgrade(&registry);
    f.shared
        .http
        .cached_bot_token
        .set("isolated-worker-token".into())
        .unwrap();
    registry.register("claude".into(), f.shared.clone()).await;
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")]);
    let id = crate::db::intake_outbox::owner_fence_pg_tests::seed(
        &pool,
        &discord_mock::CHANNEL_ID.to_string(),
        "pending",
        true,
    )
    .await;
    let original_upload = upload(&f, discord_mock::CHANNEL_ID).await;
    let Upload::Bundle(reference) = &original_upload else {
        unreachable!()
    };
    sqlx::query(
        "UPDATE intake_outbox SET attachment_refs=$2,replay_source_message_ids=$3 WHERE id=$1",
    )
    .bind(id)
    .bind(serde_json::json!([reference]))
    .bind(vec![
        discord_mock::CHANNEL_ID.to_string(),
        (discord_mock::CHANNEL_ID - 1).to_string(),
    ])
    .execute(&pool)
    .await
    .unwrap();
    let row: crate::db::intake_outbox::IntakeOutboxRow =
        sqlx::query_as("SELECT * FROM intake_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    let request = worker::request_from_row_for_tests(&row);
    let old = intent(&f, &request).await;
    let cap = Capability::new(request.clone(), old.clone());
    let pool_at_spawn = pool.clone();
    let key = old.authority.session_key.clone();
    let spawned = Arc::new(AtomicBool::new(false));
    let saw_spawn = spawned.clone();
    let first_claims = Arc::new(AtomicUsize::new(0));
    let claimed_first = first_claims.clone();
    let checkpoint = test_executor::hook(Box::new(move |point| {
        let pool = pool_at_spawn.clone();
        let key = key.clone();
        let saw = saw_spawn.clone();
        let claimed = claimed_first.clone();
        Box::pin(async move {
            if point == test_executor::Checkpoint::AfterClaim {
                claimed.fetch_add(1, Ordering::SeqCst);
            }
            if point == test_executor::Checkpoint::Spawned {
                sqlx::query("UPDATE sessions SET claude_session_id='changed-selector',raw_provider_session_id='changed-selector' WHERE session_key=$1")
                    .bind(key).execute(&pool).await.unwrap();
                saw.store(true, Ordering::SeqCst);
            }
        })
    }));
    let before = crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, id).await;
    let first = staged::scope(
        cap,
        worker::run_intake_worker_tick(
            &pool,
            &f.shared,
            "fence-worker",
            "claude",
            "selector-worker",
            &|| false,
        ),
    )
    .await;
    drop(checkpoint);
    let after = crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, id).await;
    assert!(
        spawned.load(Ordering::SeqCst),
        "the actual worker changed only selector after issuing its spawned proof"
    );
    assert!(matches!(first, Ok(worker::TickOutcome::Held)));
    assert_eq!(
        first_claims.load(Ordering::SeqCst),
        1,
        "the first real tick owns one original claim even with a reusable worker label"
    );
    assert_eq!(
        after["status"], "pending",
        "stale intent must return the same original row to reclassification"
    );
    assert!(
        !after["last_error"]
            .as_str()
            .is_some_and(|s| s.starts_with("claude-migration-wait:v1:")),
        "stale intent cannot grant waiting authority"
    );
    assert!(custody::fenced_waiting(&pool, &old).await.is_err());
    let canonical = sqlx::query_scalar("SELECT id FROM sessions WHERE session_key=$1")
        .bind(&old.authority.session_key)
        .fetch_one(&pool)
        .await
        .unwrap();
    let fresh = Arc::new(
        custody::capture_intent(
            &pool,
            &old.authority,
            canonical,
            8,
            "reclassified-migration",
        )
        .await
        .unwrap(),
    );
    let claims = Arc::new(AtomicUsize::new(0));
    let claimed = claims.clone();
    let next_pool = pool.clone();
    let checkpoint = test_executor::hook(Box::new(move |point| {
        let claimed = claimed.clone();
        let pool = next_pool.clone();
        Box::pin(async move {
            if point == test_executor::Checkpoint::AfterClaim {
                let state =
                    crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, id).await;
                assert_eq!(state["id"], id);
                assert_eq!(state["claim_owner"], "selector-worker");
                claimed.fetch_add(1, Ordering::SeqCst);
            }
        })
    }));
    let second = staged::scope(
        Capability::new(request, fresh),
        worker::run_intake_worker_tick(
            &pool,
            &f.shared,
            "fence-worker",
            "claude",
            "selector-worker",
            &|| false,
        ),
    )
    .await;
    drop(checkpoint);
    let final_row = crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, id).await;
    let count = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM intake_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    pool.close().await;
    db.drop().await;
    assert_eq!(
        claims.load(Ordering::SeqCst),
        1,
        "the next real tick claims the same original input exactly once"
    );
    assert!(matches!(second, Ok(worker::TickOutcome::Held)));
    assert_eq!(final_row["status"], "pending");
    for field in [
        "id",
        "provider",
        "channel_id",
        "user_msg_id",
        "user_text",
        "replay_source_message_ids",
        "attachment_refs",
        "attempt_no",
        "parent_outbox_id",
        "created_at",
        "owner_instance_id",
        "owner_generation",
    ] {
        assert_eq!(
            final_row[field], before[field],
            "reclassification preserves original custody: {field}"
        );
    }
    assert_eq!(count, 1);
    assert_eq!(f.state.local_note_posts.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_enqueue_keeps_content_dedup_without_migration_policy() {
    let f = Fixture::new("disabled").await;
    let mut first = submission(8_917_083, IntakeOrigin::LiveMessage);
    first.request.user_text = "same ordinary body".into();
    first.request.merge_consecutive = false;
    let mut second = submission(8_917_084, IntakeOrigin::LiveMessage);
    second.request.user_text = first.request.user_text.clone();
    second.request.merge_consecutive = false;
    let (_, channel, a) = intake::deferred_input(first, true);
    let (_, _, mut b) = intake::deferred_input(second, true);
    b.created_at = a.created_at + std::time::Duration::from_millis(1);
    let actor = f.shared.mailbox(channel);
    let p = QueuePersistenceContext::new(&ProviderKind::Claude, &f.shared.token_hash, None);
    assert!(actor.enqueue(a, p.clone()).await.enqueued);
    let refused = actor.enqueue(b, p).await;
    assert_eq!(
        refused.refusal_reason,
        Some(crate::services::turn_orchestrator::EnqueueRefusalReason::LastItemDedup),
        "ordinary enqueue preserves its original content dedup policy"
    );
    assert_eq!(
        f.disk()
            .iter()
            .map(|i| i.message_id.get())
            .collect::<Vec<_>>(),
        vec![8_917_083]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn migration_capacity_refusal_retains_all_queued_originals() {
    let f = Fixture::new("disabled").await;
    let channel = ChannelId::new(discord_mock::CHANNEL_ID);
    let actor = f.shared.mailbox(channel);
    let persistence =
        QueuePersistenceContext::new(&ProviderKind::Claude, &f.shared.token_hash, None);
    let originals: Vec<_> = (0..crate::services::turn_orchestrator::MAX_INTERVENTIONS_PER_CHANNEL)
        .map(|index| {
            let mut input = submission(8_918_000 + index as u64, IntakeOrigin::LiveMessage);
            input.request.merge_consecutive = false;
            intake::deferred_input(input, true).2
        })
        .collect();
    actor
        .replace_queue(originals.clone(), persistence.clone())
        .await;
    assert_eq!(
        f.disk().len(),
        originals.len(),
        "the actual actor durably starts at the queue capacity"
    );
    let incoming = intake::deferred_input(submission(8_919_000, IntakeOrigin::LiveMessage), true).2;
    let refused = actor.enqueue_migration(incoming, persistence).await;
    let disk = f.disk();
    assert_eq!(
        disk.iter().map(|item| item.message_id).collect::<Vec<_>>(),
        originals
            .iter()
            .map(|item| item.message_id)
            .collect::<Vec<_>>(),
        "migration custody never evicts an earlier original to acknowledge a full queue"
    );
    assert_eq!(
        refused.refusal_reason,
        Some(crate::services::turn_orchestrator::EnqueueRefusalReason::MigrationCapacity)
    );
    assert!(!refused.enqueued && !refused.merged && refused.persistence_error.is_none());
    assert!(refused.queue_exit_events.is_empty());
    for (actual, original) in disk.iter().zip(&originals) {
        assert_original(actual, original);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn migration_duplicate_ack_republishes_before_accepting_custody() {
    let f = Fixture::new("disabled").await;
    let mut input = submission(8_919_020, IntakeOrigin::LiveMessage);
    input.request.merge_consecutive = false;
    let (_, channel, original) = intake::deferred_input(input, true);
    let persistence =
        QueuePersistenceContext::new(&ProviderKind::Claude, &f.shared.token_hash, None);
    let actor = f.shared.mailbox(channel);
    assert!(
        actor
            .enqueue(original.clone(), persistence.clone())
            .await
            .enqueued,
        "the original queue item has an actual durable ACK before testing a duplicate"
    );
    let queue = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join(&f.shared.token_hash)
        .join(format!("{}.json", channel.get()));
    let fault = crate::services::discord::runtime_store::rename_fault::Armed::new(&queue);
    let first = actor
        .enqueue_migration(original.clone(), persistence.clone())
        .await;
    let first_evidence = fault.evidence();
    let first_disk = f.disk();
    let second = actor.enqueue_migration(original.clone(), persistence).await;
    let second_evidence = fault.evidence();
    drop(fault);
    let disk = f.disk();
    assert!(
        first
            .persistence_error
            .as_deref()
            .is_some_and(|error| error.contains("rename")),
        "migration duplicate ACK must perform a real durable publication"
    );
    assert_eq!(
        first_evidence.0, 1,
        "the first duplicate reaches the filesystem rename"
    );
    assert_eq!(
        first_evidence.1.len(),
        1,
        "the OS rejects the first duplicate publication"
    );
    assert_eq!(first_disk.len(), 1);
    assert_original(&first_disk[0], &original);
    assert!(second.persistence_error.is_none());
    assert_eq!(
        second.refusal_reason,
        Some(crate::services::turn_orchestrator::EnqueueRefusalReason::SourceIdAlreadyQueued)
    );
    assert_eq!(
        second_evidence.0, 2,
        "the duplicate retry actually writes a second time"
    );
    assert_eq!(second_evidence.1.len(), 1);
    assert_eq!(
        disk.len(),
        1,
        "a duplicate ACK never creates another original request"
    );
    assert_original(&disk[0], &original);
}
