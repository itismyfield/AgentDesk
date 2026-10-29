use super::*;
use crate::db::intake_outbox::migration_wait::{self as custody, Authority};
use crate::services::cluster::attachment_transfer::{self as attachments, uploads::Upload};
use crate::services::discord::router::migration_wait::{self as staged, Capability};
use std::sync::atomic::{AtomicBool, AtomicUsize};

async fn attach_pool(
    f: &mut Fixture,
) -> (
    crate::db::auto_queue::test_support::TestPostgresDb,
    sqlx::PgPool,
) {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(1).await;
    f.shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    (db, pool)
}

async fn intent(f: &Fixture, request: &IntakeRequest) -> Arc<custody::WaitIntent> {
    let channel = request.channel_id.get();
    let logical = format!("AgentDesk-claude-migration-{channel}");
    let session_key = format!("claude/{}/fence-worker:{logical}", f.shared.token_hash);
    let pool = f.shared.pg_pool.as_ref().unwrap();
    let row = sqlx::query_scalar("INSERT INTO sessions (session_key,provider,status,identity_kind,discord_token_hash,channel_id,instance_id,cwd,claude_session_id,raw_provider_session_id) VALUES ($1,'claude','idle','discord_channel',$2,$3,'fence-worker',$4,'old-native','old-native') RETURNING id")
        .bind(&session_key).bind(&f.shared.token_hash).bind(channel.to_string())
        .bind(f.root.path().display().to_string()).fetch_one(pool).await.unwrap();
    Arc::new(
        custody::capture_intent(
            pool,
            &Authority {
                provider: "claude".into(),
                token_hash: f.shared.token_hash.clone(),
                channel_id: channel,
                local_instance: "fence-worker".into(),
                runtime_root: crate::config::runtime_root()
                    .unwrap()
                    .canonicalize()
                    .unwrap(),
                logical_key: logical,
                session_key,
            },
            row,
            7,
            "staged-migration-wait",
        )
        .await
        .expect("real canonical migration selector is captured"),
    )
}

async fn upload(f: &Fixture, message: u64) -> Upload {
    let identity = attachments::AttachmentMessageIdentity {
        provider: "claude".into(),
        channel_id: discord_mock::CHANNEL_ID.to_string(),
        user_msg_id: message.to_string(),
    };
    let bytes = format!("original bytes {message}").into_bytes();
    let validated = attachments::validate_attachment_bundle_v1(
        attachments::AttachmentBundleV1 {
            version: attachments::ATTACHMENT_BUNDLE_V1,
            identity: identity.clone(),
            source_attachment_count: 1,
            entries: vec![attachments::AttachmentEntryV1 {
                filename: "original.bin".into(),
                sha256: attachments::attachment_sha256_hex(&bytes),
                bytes,
            }],
        },
        &identity,
    )
    .unwrap();
    Upload::Bundle(
        attachments::store::put(f.shared.pg_pool.as_ref().unwrap(), &validated)
            .await
            .unwrap(),
    )
}

fn final_spy(calls: Arc<AtomicUsize>) -> Hooks {
    FINAL.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(crate::services::discord::IntakeExecution::Executed)
        }))
    });
    Hooks
}

fn assert_original(
    actual: &crate::services::turn_orchestrator::Intervention,
    expected: &crate::services::turn_orchestrator::Intervention,
) {
    assert_eq!(
        actual.message_id, expected.message_id,
        "migration restores the original message identity"
    );
    assert_eq!(
        actual.queued_generation, expected.queued_generation,
        "migration keeps the queue generation"
    );
    assert_eq!(
        actual.source_message_ids, expected.source_message_ids,
        "migration keeps every absorbed source"
    );
    let source_view = |item: &crate::services::turn_orchestrator::Intervention| {
        item.source_message_queued_generations()
            .into_iter()
            .map(|s| {
                (
                    s.message_id,
                    s.queued_generation,
                    s.enqueued_at_epoch_us,
                    s.preserve_on_cancel,
                    s.route,
                    s.origin,
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        source_view(actual),
        source_view(expected),
        "migration preserves FIFO and source ownership"
    );
    let text_view = |item: &crate::services::turn_orchestrator::Intervention| {
        item.source_text_segments()
            .into_iter()
            .map(|s| (s.message_id, s.text))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        text_view(actual),
        text_view(expected),
        "migration preserves each original source body"
    );
    assert_eq!(actual.author_id, expected.author_id);
    assert_eq!(actual.author_is_bot, expected.author_is_bot);
    assert_eq!(actual.text, expected.text);
    assert_eq!(actual.reply_context, expected.reply_context);
    assert_eq!(actual.has_reply_boundary, expected.has_reply_boundary);
    assert_eq!(actual.merge_consecutive, expected.merge_consecutive);
    assert_eq!(
        actual.pending_uploads, expected.pending_uploads,
        "migration retains attachments once in the original custody"
    );
    assert_eq!(actual.voice_announcement, expected.voice_announcement);
}

#[tokio::test(flavor = "current_thread")]
async fn actual_worker_migration_wait_after_preaccept_rearms_same_input_pg() {
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
    let wait_intent = intent(&f, &request).await;
    let cap = Capability::new(request.clone(), wait_intent.clone());
    cap.enabled.store(false, Ordering::SeqCst);
    let enabled = cap.enabled.clone();
    let observations = cap.observations.clone();
    let preaccept = Arc::new(AtomicBool::new(false));
    let spawned = Arc::new(AtomicBool::new(false));
    let pre = preaccept.clone();
    let spawn = spawned.clone();
    let _checkpoint = test_executor::hook(Box::new(move |point| {
        let enabled = enabled.clone();
        let pre = pre.clone();
        let spawn = spawn.clone();
        Box::pin(async move {
            if point == test_executor::Checkpoint::PreAccept {
                assert!(
                    !enabled.swap(true, Ordering::SeqCst),
                    "migration wait begins only after real final preaccept"
                );
                pre.store(true, Ordering::SeqCst);
            }
            if point == test_executor::Checkpoint::Spawned {
                spawn.store(true, Ordering::SeqCst);
            }
        })
    }));
    let before = crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, id).await;
    let result = staged::scope(
        cap,
        worker::run_intake_worker_tick(
            &pool,
            &f.shared,
            "fence-worker",
            "claude",
            "token",
            &|| false,
        ),
    )
    .await;
    assert!(
        preaccept.load(Ordering::SeqCst) && spawned.load(Ordering::SeqCst),
        "actual worker reached preaccept and spawned proof issuance"
    );
    assert!(
        observations.load(Ordering::SeqCst) > 0,
        "actual worker reached the selected migration wait root"
    );
    assert!(
        matches!(result, Ok(worker::TickOutcome::Held)),
        "migration wait returns literal Deferred through worker settlement: {result:?}"
    );
    let after = crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, id).await;
    assert_eq!(
        after["status"], "pending",
        "before-effects migration returns the same row to retryable custody"
    );
    for field in ["claim_owner", "claimed_at", "accepted_at", "spawned_at"] {
        assert!(
            after[field].is_null(),
            "retryable original input resets only its worker claim clocks: {field}"
        );
    }
    assert!(
        after["last_error"]
            .as_str()
            .is_some_and(|s| s.starts_with("claude-migration-wait:v1:")),
        "migration publishes its own exact waiting marker"
    );
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
        "retry_count",
        "created_at",
        "owner_instance_id",
        "owner_generation",
    ] {
        assert_eq!(
            after[field], before[field],
            "original worker input is unchanged: {field}"
        );
    }
    assert_eq!(
        attachments::worker_uploads(&pool, &row).await.unwrap(),
        vec![original_upload.clone()]
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM intake_outbox")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1,
        "migration never creates another input attempt"
    );
    assert_eq!(
        f.state.local_note_posts.load(Ordering::SeqCst),
        0,
        "migration wait posts no refusal/card/provider output"
    );
    drop(_checkpoint);
    let claimed_again = Arc::new(std::sync::Mutex::new(None));
    let captured = claimed_again.clone();
    let second_pool = pool.clone();
    let spawned_again = Arc::new(AtomicBool::new(false));
    let second_spawn = spawned_again.clone();
    let second_checkpoint = test_executor::hook(Box::new(move |point| {
        let captured = captured.clone();
        let pool = second_pool.clone();
        let spawned = second_spawn.clone();
        Box::pin(async move {
            if point == test_executor::Checkpoint::AfterClaim {
                let row = crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, id).await;
                *captured.lock().unwrap() = Some(row);
            }
            if point == test_executor::Checkpoint::Spawned {
                spawned.store(true, Ordering::SeqCst);
            }
        })
    }));
    let second_cap = Capability::new(request.clone(), wait_intent);
    let second_observations = second_cap.observations.clone();
    let second_result = staged::scope(
        second_cap,
        worker::run_intake_worker_tick(
            &pool,
            &f.shared,
            "fence-worker",
            "claude",
            "migration-reclaim-owner",
            &|| false,
        ),
    )
    .await;
    drop(second_checkpoint);
    let claimed = claimed_again.lock().unwrap().take();
    assert!(
        claimed.is_some(),
        "same original input reaches the real worker claim a second time"
    );
    let claimed = claimed.unwrap();
    assert_eq!(
        claimed["id"], before["id"],
        "the next worker claims the same original row identity"
    );
    assert_eq!(claimed["status"], "claimed");
    assert_eq!(
        claimed["claim_owner"], "migration-reclaim-owner",
        "a new worker claim owns the original input"
    );
    assert!(
        spawned_again.load(Ordering::SeqCst),
        "reclaimed input reaches the production spawned proof issuing path"
    );
    assert!(
        second_observations.load(Ordering::SeqCst) > 0,
        "reclaimed input reaches the same migration intent consumer"
    );
    assert!(
        matches!(second_result, Ok(worker::TickOutcome::Held)),
        "the reclaimed original input returns literal Deferred: {second_result:?}"
    );
    let again = crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, id).await;
    assert_eq!(
        again["status"], "pending",
        "reclaimed input returns to the same retryable custody"
    );
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
        "retry_count",
        "created_at",
        "owner_instance_id",
        "owner_generation",
    ] {
        assert_eq!(
            again[field], before[field],
            "same-row reclaim preserves original input: {field}"
        );
    }
    for field in ["claim_owner", "claimed_at", "accepted_at", "spawned_at"] {
        assert!(
            again[field].is_null(),
            "second exact park resets only the worker claim clocks: {field}"
        );
    }
    let marker: serde_json::Value = serde_json::from_str(
        again["last_error"]
            .as_str()
            .unwrap()
            .strip_prefix("claude-migration-wait:v1:")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        marker["claim"], "migration-reclaim-owner",
        "second park consumes the newly issued exact worker proof"
    );
    let replayed: crate::db::intake_outbox::IntakeOutboxRow =
        sqlx::query_as("SELECT * FROM intake_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        worker::request_from_row_for_tests(&replayed),
        request,
        "same-row reclaim preserves the complete intake request"
    );
    assert_eq!(
        attachments::worker_uploads(&pool, &replayed).await.unwrap(),
        vec![original_upload],
        "reclaimed worker resolves the same original attachments"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM intake_outbox")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1,
        "reclaim never creates another request or attempt"
    );
    assert_eq!(
        f.state.local_note_posts.load(Ordering::SeqCst),
        0,
        "reclaimed wait has zero provider/card/refusal effects"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn actual_live_migration_wait_keeps_every_source_and_attachment_before_checkpoint_pg() {
    let mut f = Fixture::new("disabled").await;
    let (db, pool) = attach_pool(&mut f).await;
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")]);
    let mut input = submission(8_917_010, IntakeOrigin::SlashSkill);
    input
        .request
        .source_message_ids
        .insert(0, MessageId::new(8_917_009));
    input.preloaded_uploads.push(upload(&f, 8_917_010).await);
    let expected = input.request.clone();
    let uploads = input.preloaded_uploads.clone();
    let cap = Capability::new(expected.clone(), intent(&f, &expected).await);
    let observed = cap.observations.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let _hooks = final_spy(calls.clone());
    let checkpoint = Arc::new(AtomicUsize::new(0));
    let advanced = checkpoint.clone();
    let result = staged::scope(
        cap,
        intake::dispatch_live_text_intake(&f.deps(), input, move || {
            advanced.fetch_add(1, Ordering::SeqCst);
        }),
    )
    .await;
    assert!(
        observed.load(Ordering::SeqCst) > 0,
        "actual live dispatch reaches migration wait classification"
    );
    assert!(
        result.is_ok(),
        "selected live wait must acquire durable custody: {result:?}"
    );
    let disk = f.disk();
    assert_eq!(
        disk.len(),
        1,
        "selected live migration waits in its original durable input"
    );
    assert_eq!(disk[0].message_id, expected.user_msg_id);
    assert_eq!(disk[0].text, expected.user_text);
    let mut sources = disk[0].source_message_ids.clone();
    sources.sort_unstable();
    let mut expected_sources = expected.source_message_ids;
    expected_sources.sort_unstable();
    assert_eq!(
        sources, expected_sources,
        "migration custody retains every absorbed source identity"
    );
    assert_eq!(
        disk[0].pending_uploads, uploads,
        "migration retains the original attachment references once"
    );
    assert_eq!(disk[0].reply_context, expected.reply_context);
    assert_eq!(
        checkpoint.load(Ordering::SeqCst),
        1,
        "checkpoint follows verified durable custody"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "migration wait never enters the ordinary executor"
    );
    pool.close().await;
    db.drop().await;
}

async fn queue(f: &Fixture, message: u64) -> crate::services::turn_orchestrator::Intervention {
    let mut value = submission(message, IntakeOrigin::SlashSkill);
    value
        .request
        .source_message_ids
        .insert(0, MessageId::new(message - 100));
    value.preloaded_uploads.push(upload(f, message).await);
    assert!(intake::defer_live_submission(&f.deps(), value, true).await);
    f.disk()
        .into_iter()
        .find(|item| item.message_id.get() == message)
        .unwrap()
}

async fn queued_case(early: bool) {
    let mut f = Fixture::new("disabled").await;
    let (db, pool) = attach_pool(&mut f).await;
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")]);
    let first = queue(&f, 8_917_020).await;
    let second = queue(&f, 8_917_021).await;
    let channel = ChannelId::new(discord_mock::CHANNEL_ID);
    let taken = f
        .shared
        .mailbox(channel)
        .take_next_soft(QueuePersistenceContext::new(
            &ProviderKind::Claude,
            &f.shared.token_hash,
            None,
        ))
        .await;
    assert!(
        taken.intervention.is_some() && taken.dispatch_lease.is_some(),
        "migration actual queue take owns its original head and lease"
    );
    let original = taken.intervention.as_ref().unwrap();
    let mut request = request(original.message_id.get());
    request.source_message_ids = original.source_message_ids.clone();
    request.reply_to_user_message = true;
    let cap = Capability::new(request.clone(), intent(&f, &request).await);
    let observed = cap.observations.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let _hooks = final_spy(calls.clone());
    let deps = f.deps();
    let admit = intake::admit_queued_intake(
        &deps,
        ProviderKind::Claude,
        channel,
        original,
        original.author_id,
        "owner".into(),
        false,
        false,
        "migration wait test",
        taken.dispatch_lease.clone(),
    );
    if early {
        let result = staged::scope(cap, admit).await;
        assert!(
            observed.load(Ordering::SeqCst) > 0,
            "actual queued admission reaches migration wait classification"
        );
        assert!(
            matches!(result, QueuedAdmissionDisposition::Deferred),
            "queued early migration wait restores original custody"
        );
    } else {
        let admitted = match admit.await {
            QueuedAdmissionDisposition::Admitted(a) => a,
            _ => panic!("positive actual local queued admission required"),
        };
        let result = staged::scope(
            cap,
            intake::finish_admitted_queued_intake(&f.deps(), admitted),
        )
        .await;
        assert!(
            observed.load(Ordering::SeqCst) > 0,
            "actual queued finish reaches migration wait classification"
        );
        assert_eq!(
            result.unwrap(),
            crate::services::discord::IntakeExecution::Deferred,
            "queued final migration wait returns literal Deferred"
        );
    }
    let disk = f.disk();
    assert_eq!(
        disk.iter().map(|i| i.message_id).collect::<Vec<_>>(),
        vec![first.message_id, second.message_id],
        "migration keeps the original FIFO head and tail"
    );
    assert_original(&disk[0], &first);
    assert_original(&disk[1], &second);
    assert!(
        f.shared
            .mailbox(channel)
            .snapshot()
            .await
            .pending_user_dispatch
            .is_none(),
        "authorized restore releases the original pending dispatch lease"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "queued migration wait does not execute a provider turn"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn actual_queued_early_migration_wait_restores_original_fifo_pg() {
    queued_case(true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn actual_queued_final_migration_wait_restores_original_fifo_pg() {
    queued_case(false).await;
}
#[tokio::test(flavor = "current_thread")]
async fn actual_live_final_migration_wait_precedes_checkpoint_and_execution_pg() {
    let mut f = Fixture::new("disabled").await;
    let (db, pool) = attach_pool(&mut f).await;
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")]);
    let mut input = submission(8_917_030, IntakeOrigin::LiveMessage);
    input
        .request
        .source_message_ids
        .insert(0, MessageId::new(8_917_029));
    input.preloaded_uploads.push(upload(&f, 8_917_030).await);
    let expected = input.request.clone();
    let expected_uploads = input.preloaded_uploads.clone();
    let cap = Capability::new(expected.clone(), intent(&f, &expected).await);
    cap.enabled.store(false, Ordering::SeqCst);
    let enabled = cap.enabled.clone();
    let observed = cap.observations.clone();
    let saw_local = Arc::new(AtomicBool::new(false));
    let seen = saw_local.clone();
    PREFLIGHT.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |_| {
            let enabled = enabled.clone();
            let seen = seen.clone();
            Box::pin(async move {
                assert!(!enabled.swap(true, Ordering::SeqCst));
                seen.store(true, Ordering::SeqCst);
            })
        }))
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let _hooks = final_spy(calls.clone());
    let checkpoints = Arc::new(AtomicUsize::new(0));
    let advanced = checkpoints.clone();
    let result = staged::scope(
        cap,
        intake::dispatch_live_text_intake(&f.deps(), input, move || {
            advanced.fetch_add(1, Ordering::SeqCst);
        }),
    )
    .await;
    assert!(
        saw_local.load(Ordering::SeqCst),
        "actual preflight was Local before migration wait arrived"
    );
    assert!(
        observed.load(Ordering::SeqCst) > 0,
        "actual final local entry consumed selected migration wait"
    );
    assert!(
        result.is_ok(),
        "final local wait preserves custody: {result:?}"
    );
    let disk = f.disk();
    assert_eq!(disk.len(), 1);
    assert_eq!(disk[0].message_id, expected.user_msg_id);
    assert_eq!(disk[0].text, expected.user_text);
    assert_eq!(disk[0].pending_uploads, expected_uploads);
    assert_eq!(
        checkpoints.load(Ordering::SeqCst),
        1,
        "final wait advances checkpoint only after custody"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "selected migration never becomes ordinary Executed"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn actual_live_migration_persistence_retry_retains_the_same_original_owner_pg() {
    let mut f = Fixture::new("disabled").await;
    let (db, pool) = attach_pool(&mut f).await;
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")]);
    let mut input = submission(8_917_040, IntakeOrigin::RawAttachment);
    input
        .request
        .source_message_ids
        .insert(0, MessageId::new(8_917_039));
    input.preloaded_uploads.push(upload(&f, 8_917_040).await);
    let expected = input.request.clone();
    let cap = Capability::new(expected.clone(), intent(&f, &expected).await);
    let mut cap = cap;
    let failures = cap.persistence_failures.clone();
    let successes = cap.persistence_successes.clone();
    let retained = cap.retained.clone();
    let original = cap.original_observed.clone();
    let witnesses = cap.witnesses.clone();
    let retry = Arc::new(tokio::sync::Notify::new());
    cap.retry_after_failure = Some(retry.clone());
    let calls = Arc::new(AtomicUsize::new(0));
    let _hooks = final_spy(calls.clone());
    let checkpoints = Arc::new(AtomicUsize::new(0));
    let advanced = checkpoints.clone();
    let channel = ChannelId::new(discord_mock::CHANNEL_ID);
    let persistence =
        QueuePersistenceContext::new(&ProviderKind::Claude, &f.shared.token_hash, None);
    let warmed = f
        .shared
        .mailbox(channel)
        .hydrate_pending_queue_from_disk(persistence)
        .await;
    assert!(
        warmed.persistence_error.is_none(),
        "the real actor reads its queue before the storage fault"
    );
    let queue = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join(&f.shared.token_hash)
        .join(format!("{}.json", channel.get()));
    let fault = crate::services::discord::runtime_store::rename_fault::Armed::new(&queue);
    let deps = f.deps();
    let mut dispatch = Box::pin(staged::scope(
        cap,
        intake::dispatch_live_text_intake(&deps, input, move || {
            advanced.fetch_add(1, Ordering::SeqCst);
        }),
    ));
    let arrived = tokio::select! {
        _ = &mut dispatch => false,
        result = tokio::time::timeout(std::time::Duration::from_secs(2),async {
            while failures.load(Ordering::SeqCst)==0 {tokio::task::yield_now().await;}
        }) => result.is_ok(),
    };
    let first_evidence = fault.evidence();
    let held = retained.load(Ordering::SeqCst);
    let before_checkpoint = checkpoints.load(Ordering::SeqCst);
    let absent = f.disk().is_empty();
    let before_witnesses = witnesses.lock().unwrap().len();
    let result = if arrived {
        retry.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(5), &mut dispatch)
            .await
            .ok()
    } else {
        None
    };
    drop(dispatch);
    let final_evidence = fault.evidence();
    drop(fault);
    let disk = f.disk();
    pool.close().await;
    db.drop().await;
    assert_eq!(
        first_evidence.1.len(),
        1,
        "the OS refused the actual first queue rename"
    );
    assert!(
        arrived,
        "first persistence failure must reach its completion barrier"
    );
    assert!(
        held && absent,
        "failed persistence leaves the original migration input in one retained owner"
    );
    assert_eq!(
        before_witnesses, 0,
        "no local witness exists before a durable ACK"
    );
    assert_eq!(
        before_checkpoint, 0,
        "unverified custody never spends the input checkpoint"
    );
    assert!(
        matches!(result, Some(Ok(_))),
        "actual second persistence must finish with original custody: {result:?}"
    );
    assert_eq!(failures.load(Ordering::SeqCst), 1);
    assert_eq!(
        successes.load(Ordering::SeqCst),
        1,
        "the successful second persistence is observed after ACK"
    );
    assert_eq!(
        final_evidence.0, 2,
        "retry performs a second real queue rename"
    );
    assert_eq!(disk.len(), 1, "retry cannot duplicate the original request");
    assert_original(&disk[0], original.lock().unwrap().as_ref().unwrap());
    assert_eq!(checkpoints.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(
        !retained.load(Ordering::SeqCst),
        "verified queue custody replaces the retained owner"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn actual_worker_consent_is_the_only_before_effects_consumer_pg() {
    use crate::services::cluster::intake_worker as worker;
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
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")])
        .with_mode(EndpointMode::UserDefault);
    let id = crate::db::intake_outbox::owner_fence_pg_tests::seed(
        &pool,
        &discord_mock::CHANNEL_ID.to_string(),
        "pending",
        true,
    )
    .await;
    let row = sqlx::query_as("SELECT * FROM intake_outbox WHERE id=$1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let request = worker::request_from_row_for_tests(&row);
    let cap = Capability::new(request.clone(), intent(&f, &request).await);
    let observations = cap.observations.clone();
    f.hold()
        .record(vec![IntentRef {
            store: "intake_outbox".into(),
            id: id.to_string(),
            provider: "claude".into(),
            channel: discord_mock::CHANNEL_ID,
            source_message: discord_mock::CHANNEL_ID,
            fifo: 1,
        }])
        .await
        .unwrap();
    let result = staged::scope(
        cap,
        worker::run_intake_worker_tick(
            &pool,
            &f.shared,
            "fence-worker",
            "claude",
            "token",
            &|| false,
        ),
    )
    .await;
    assert!(
        matches!(result, Ok(worker::TickOutcome::Held)),
        "the actual consent consumer preserves worker custody: {result:?}"
    );
    let after = crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, id).await;
    assert_eq!(
        after["status"], "spawned",
        "consent retains its original release authority"
    );
    assert!(
        after["last_error"]
            .as_str()
            .is_some_and(|s| s.starts_with("herdr-consent-custody:v1:")),
        "migration never overwrites the selected consent custody marker"
    );
    assert_eq!(
        observations.load(Ordering::SeqCst),
        0,
        "one before-effects proof is delivered to the selected consent consumer alone"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM intake_outbox")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn actual_worker_entry_without_before_effects_proof_keeps_original_pending_pg() {
    use crate::services::cluster::intake_worker as worker;
    let mut f = Fixture::new("disabled").await;
    let (db, pool) = attach_pool(&mut f).await;
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")]);
    let id = crate::db::intake_outbox::owner_fence_pg_tests::seed(
        &pool,
        &discord_mock::CHANNEL_ID.to_string(),
        "pending",
        true,
    )
    .await;
    let row = sqlx::query_as("SELECT * FROM intake_outbox WHERE id=$1")
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let request = worker::request_from_row_for_tests(&row);
    let cap = Capability::new(request.clone(), intent(&f, &request).await);
    let observed = cap.observations.clone();
    let before = crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, id).await;
    let result = staged::scope(
        cap,
        crate::services::discord::execute_intake_turn_core(
            &f.http,
            &f.shared,
            "fixture-worker",
            request,
            Vec::new(),
        ),
    )
    .await;
    assert!(
        observed.load(Ordering::SeqCst) > 0,
        "the actual worker entry classified the selected migration wait"
    );
    assert_eq!(
        result.unwrap(),
        crate::services::discord::IntakeExecution::Deferred,
        "missing production-issued proof cannot become Executed"
    );
    assert_eq!(
        crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, id).await,
        before,
        "capability cannot manufacture or reissue a before-effects worker proof"
    );
    assert_eq!(f.state.local_note_posts.load(Ordering::SeqCst), 0);
    pool.close().await;
    db.drop().await;
}

async fn protect_sources(
    pool: &sqlx::PgPool,
    request: &IntakeRequest,
    uploads: &[Upload],
    disposition: &str,
) -> i64 {
    let mut sources: Vec<_> = request
        .source_message_ids
        .iter()
        .map(|s| s.get().to_string())
        .collect();
    sources.push(request.user_msg_id.get().to_string());
    sources.sort_unstable();
    sources.dedup();
    let refs: Vec<_> = uploads
        .iter()
        .filter_map(|u| match u {
            Upload::Bundle(r) => Some(r.clone()),
            _ => None,
        })
        .collect();
    sqlx::query_scalar(r#"INSERT INTO intake_outbox (target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,request_owner_id,user_text,turn_kind,agent_id,provider,status,replay_only,replay_disposition,replay_source_message_ids,replay_episode_nonce,replay_owner_incarnation,replay_request_hash,replay_request_key,replay_hold_reason,replay_preserved,attachment_refs) VALUES ('original-owner','original-gateway',$1,$2,'1',$3,'foreground','held-agent','claude','unknown',TRUE,$4,$5,'original-episode','original-incarnation','original-hash',$6,'unclassified previous submission','{"body":"original partial output","owed":true}'::jsonb,$7) RETURNING id"#)
        .bind(request.channel_id.get().to_string()).bind(request.user_msg_id.get().to_string())
        .bind(&request.user_text).bind(disposition).bind(sources)
        .bind(format!("discord:claude:{}:{}",request.channel_id.get(),request.user_msg_id.get()))
        .bind(serde_json::json!(refs)).fetch_one(pool).await.unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn actual_migration_admission_keeps_protected_source_authority_ahead_of_wait_pg() {
    for disposition in ["started_unclassified", "withheld"] {
        let mut f = Fixture::new("disabled").await;
        let (db, pool) = attach_pool(&mut f).await;
        let _boot =
            session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")]);
        let mut input = submission(8_917_050, IntakeOrigin::LiveMessage);
        input
            .request
            .source_message_ids
            .insert(0, MessageId::new(8_917_049));
        input.preloaded_uploads.push(upload(&f, 8_917_050).await);
        let cap = Capability::new(input.request.clone(), intent(&f, &input.request).await);
        let observed = cap.observations.clone();
        let receipt =
            protect_sources(&pool, &input.request, &input.preloaded_uploads, disposition).await;
        let before = crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, receipt).await;
        let admission = staged::scope(cap, intake::admit_text_intake(&f.deps(), &input)).await;
        assert!(
            matches!(admission, IntakeAdmission::ConsumedToHold),
            "protected original sources never become migration-unsubmitted: {admission:?}"
        );
        intake::finish_text_intake_admission(&f.deps(), admission, input)
            .await
            .unwrap();
        assert_eq!(
            observed.load(Ordering::SeqCst),
            0,
            "migration classification follows the existing source and owner admission"
        );
        assert!(
            f.disk().is_empty(),
            "protected request is never cloned into a local migration queue"
        );
        assert_eq!(
            crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, receipt).await,
            before
        );
        assert_eq!(f.state.local_note_posts.load(Ordering::SeqCst), 0);
        pool.close().await;
        db.drop().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn actual_migration_admission_keeps_already_forwarded_original_custody_pg() {
    let mut f = Fixture::new("enforce").await;
    let (db, pool) = attach_pool(&mut f).await;
    let _boot = session_hosts::force_for_test(Some("node"), &[]);
    let input = submission(8_917_060, IntakeOrigin::LiveMessage);
    let cap = Capability::new(input.request.clone(), intent(&f, &input.request).await);
    let observed = cap.observations.clone();
    sqlx::query("UPDATE sessions SET status='disconnected' WHERE channel_id=$1")
        .bind(discord_mock::CHANNEL_ID.to_string())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO agents (id,name,provider,discord_channel_id,preferred_intake_node_labels) VALUES ('migration-foreign-agent','Foreign','claude',$1,'[\"migration-remote\"]'::jsonb)").bind(discord_mock::CHANNEL_ID.to_string()).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO worker_nodes(instance_id,status,role,effective_role,labels,capabilities,last_heartbeat_at,started_at,updated_at) VALUES ('migration-remote-worker','online','worker','worker','[\"migration-remote\"]'::jsonb,'{\"intake_worker\":{\"enabled\":true,\"providers\":[\"claude\"],\"features\":[\"preserve_on_cancel_v1\"]}}'::jsonb,NOW(),NOW(),NOW())").execute(&pool).await.unwrap();
    assert!(
        crate::services::cluster::intake_router_hook::effective_intake_routing_config()
            .mode_is_enforce(),
        "foreign routing fixture uses actual enforce mode"
    );
    let nodes = crate::services::cluster::node_registry::list_worker_nodes(&pool, 120)
        .await
        .unwrap();
    let foreign = nodes
        .iter()
        .find(|node| node["instance_id"] == "migration-remote-worker")
        .expect("actual worker registry returns the seeded foreign owner");
    assert!(
        crate::services::cluster::node_registry::node_supports_intake_request(
            foreign,
            "claude",
            input.preserve_on_cancel
        ),
        "foreign worker supports this preserving input's actual protocol shape"
    );
    let id = match intake::admit_text_intake(&f.deps(), &input).await {
        IntakeAdmission::Forwarded {
            target_instance_id,
            outbox_id,
        } => {
            assert_eq!(target_instance_id, "migration-remote-worker");
            outbox_id
        }
        other => panic!(
            "positive actual owner routing must first forward the original request: {other:?}"
        ),
    };
    let before = crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, id).await;
    let admission = staged::scope(cap, intake::admit_text_intake(&f.deps(), &input)).await;
    assert!(
        matches!(admission, IntakeAdmission::SkippedDuplicate),
        "an already forwarded original is never local migration custody: {admission:?}"
    );
    intake::finish_text_intake_admission(&f.deps(), admission, input)
        .await
        .unwrap();
    assert_eq!(
        observed.load(Ordering::SeqCst),
        0,
        "migration preserves actual foreign routing before classification"
    );
    assert!(f.disk().is_empty());
    assert_eq!(
        crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, id).await,
        before
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM intake_outbox")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn actual_queued_late_protected_migration_source_releases_only_original_lease_pg() {
    let mut f = Fixture::new("disabled").await;
    let (db, pool) = attach_pool(&mut f).await;
    let _boot = session_hosts::force_for_test(Some("node"), &[(discord_mock::CHANNEL_ID, "node")]);
    let first = queue(&f, 8_917_070).await;
    let tail = queue(&f, 8_917_071).await;
    let channel = ChannelId::new(discord_mock::CHANNEL_ID);
    let taken = f
        .shared
        .mailbox(channel)
        .take_next_soft(QueuePersistenceContext::new(
            &ProviderKind::Claude,
            &f.shared.token_hash,
            None,
        ))
        .await;
    assert!(
        taken.intervention.is_some() && taken.dispatch_lease.is_some(),
        "actual queue take owns the original input and DispatchLease"
    );
    let original = taken.intervention.as_ref().unwrap();
    let mut request = request(original.message_id.get());
    request.source_message_ids = original.source_message_ids.clone();
    request.reply_to_user_message = true;
    let cap = Capability::new(request.clone(), intent(&f, &request).await);
    let observed = cap.observations.clone();
    let admitted = match intake::admit_queued_intake(
        &f.deps(),
        ProviderKind::Claude,
        channel,
        original,
        original.author_id,
        "owner".into(),
        false,
        false,
        "late protected migration test",
        taken.dispatch_lease.clone(),
    )
    .await
    {
        QueuedAdmissionDisposition::Admitted(a) => a,
        _ => panic!("positive local queued admission must precede the new protected receipt"),
    };
    let receipt = protect_sources(
        &pool,
        &request,
        &original.pending_uploads,
        "started_unclassified",
    )
    .await;
    let before = crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, receipt).await;
    let calls = Arc::new(AtomicUsize::new(0));
    let _hooks = final_spy(calls.clone());
    let result = staged::scope(
        cap,
        intake::finish_admitted_queued_intake(&f.deps(), admitted),
    )
    .await
    .unwrap();
    assert!(
        observed.load(Ordering::SeqCst) > 0,
        "late migration final entry rechecks the existing replay source authority"
    );
    assert_eq!(
        result,
        crate::services::discord::IntakeExecution::ConsumedProtected,
        "late protected original stays with its existing request"
    );
    let disk = f.disk();
    assert_eq!(disk.len(), 1);
    assert_original(&disk[0], &tail);
    assert!(
        !disk.iter().any(|i| i.message_id == first.message_id),
        "protected head is not copied into a new migration queue"
    );
    assert!(
        f.shared
            .mailbox(channel)
            .snapshot()
            .await
            .pending_user_dispatch
            .is_none(),
        "only the original protected head's pending dispatch lease is released"
    );
    assert!(
        crate::services::turn_orchestrator::load_channel_pending_dispatch_marker(
            &ProviderKind::Claude,
            &f.shared.token_hash,
            channel
        )
        .is_none()
    );
    assert_eq!(
        crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, receipt).await,
        before
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.state.local_note_posts.load(Ordering::SeqCst), 0);
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn actual_worker_without_migration_capability_keeps_main_refusal_settlement_pg() {
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
    let channel = ChannelId::new(discord_mock::CHANNEL_ID);
    let _boot = session_hosts::force_for_test(Some("node"), &[(channel.get(), "node")]);
    crate::services::discord::rebind_channel_session(
        &f.shared,
        &ProviderKind::Claude,
        channel,
        &f.root.path().display().to_string(),
        "old-native",
    )
    .await;
    f.shared
        .core
        .lock()
        .await
        .sessions
        .get_mut(&channel)
        .unwrap()
        .channel_name = Some("migration-no-capability".into());
    let session_key = crate::services::discord::build_adk_session_key(
        &f.shared,
        channel,
        &ProviderKind::Claude,
        None,
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO sessions(session_key,provider,status,identity_kind,discord_token_hash,channel_id,instance_id,cwd,claude_session_id,raw_provider_session_id) VALUES ($1,'claude','idle','discord_channel',$2,$3,'fence-worker',$4,'old-native','old-native')")
        .bind(&session_key).bind(&f.shared.token_hash).bind(channel.get().to_string()).bind(f.root.path().display().to_string()).execute(&pool).await.unwrap();
    assert_eq!(
        crate::services::turn_host::intake_refusal_before_turn(
            Some(&pool),
            &ProviderKind::Claude,
            channel.get(),
            || std::future::ready(Some(session_key.clone()))
        )
        .await,
        Some(crate::services::turn_host::HerdrRefusal::OWriterNotReady),
        "actual canonical Legacy host keeps main's ordinary Herdr refusal"
    );
    let id = crate::db::intake_outbox::owner_fence_pg_tests::seed(
        &pool,
        &channel.get().to_string(),
        "pending",
        true,
    )
    .await;
    let original_upload = upload(&f, channel.get()).await;
    let Upload::Bundle(reference) = &original_upload else {
        unreachable!()
    };
    sqlx::query(
        "UPDATE intake_outbox SET attachment_refs=$2,replay_source_message_ids=$3 WHERE id=$1",
    )
    .bind(id)
    .bind(serde_json::json!([reference]))
    .bind(vec![
        channel.get().to_string(),
        (channel.get() - 1).to_string(),
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
    assert!(
        !staged::channel_selected(channel)
            && staged::waiting(&ProviderKind::Claude, &request).is_none(),
        "actual upper-entry control has no migration capability"
    );
    let preaccept = Arc::new(AtomicBool::new(false));
    let spawned = Arc::new(AtomicBool::new(false));
    let pre = preaccept.clone();
    let spawn = spawned.clone();
    let expected = request.clone();
    let checkpoint = test_executor::hook(Box::new(move |point| {
        let pre = pre.clone();
        let spawn = spawn.clone();
        let request = expected.clone();
        Box::pin(async move {
            if matches!(
                point,
                test_executor::Checkpoint::PreAccept | test_executor::Checkpoint::Spawned
            ) {
                assert!(
                    !staged::channel_selected(request.channel_id)
                        && staged::waiting(&ProviderKind::Claude, &request).is_none(),
                    "real worker checkpoints remain outside every migration capability scope"
                );
            }
            if point == test_executor::Checkpoint::PreAccept {
                pre.store(true, Ordering::SeqCst);
            }
            if point == test_executor::Checkpoint::Spawned {
                spawn.store(true, Ordering::SeqCst);
            }
        })
    }));
    let before = crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, id).await;
    let result = worker::run_intake_worker_tick(
        &pool,
        &f.shared,
        "fence-worker",
        "claude",
        "no-capability-owner",
        &|| false,
    )
    .await;
    drop(checkpoint);
    assert!(
        preaccept.load(Ordering::SeqCst) && spawned.load(Ordering::SeqCst),
        "actual no-capability worker reaches preaccept and production spawned proof issuance"
    );
    assert!(
        matches!(result, Ok(worker::TickOutcome::Processed)),
        "no capability preserves main worker settlement: {result:?}"
    );
    let after = crate::db::intake_outbox::owner_fence_pg_tests::snapshot(&pool, id).await;
    assert_eq!(
        after["status"], "done",
        "ordinary main refusal remains Executed instead of staged migration Deferred"
    );
    assert!(
        after["last_error"].is_null(),
        "no migration capability publishes no waiting marker or rearm"
    );
    assert!(
        !after["completed_at"].is_null(),
        "main worker records its existing completion settlement"
    );
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
        "retry_count",
        "created_at",
        "owner_instance_id",
        "owner_generation",
    ] {
        assert_eq!(
            after[field], before[field],
            "dormant upper-entry leaves original payload intact: {field}"
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM intake_outbox")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1,
        "no capability creates another request or attempt"
    );
    assert_eq!(
        f.shared.core.lock().await.sessions[&channel].pending_uploads,
        vec![original_upload],
        "ordinary main refusal returns its original uploads once"
    );
    let texts = f.state.written_texts();
    assert!(
        !texts.is_empty()
            && texts
                .iter()
                .all(|s| s == "herdr turn refused: o_writer_not_ready"),
        "the only Discord effect is main's ordinary refusal, never provider/card output"
    );
    assert_eq!(f.state.local_note_posts.load(Ordering::SeqCst), 1);
    assert!(
        crate::services::discord::inflight::load_inflight_state_read_only_result(
            &ProviderKind::Claude,
            channel.get()
        )
        .unwrap()
        .is_none(),
        "no capability does not reach any provider start"
    );
    assert!(f.state.unhandled.lock().unwrap().is_empty());
    pool.close().await;
    db.drop().await;
}

#[path = "o1_custody_tests.rs"]
mod o1_custody_tests;
