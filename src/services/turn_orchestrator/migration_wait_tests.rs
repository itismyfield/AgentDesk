use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox::migration_wait::tests::{pending_row, root, setup};
use crate::db::intake_outbox::migration_wait::{WaitIntent, fenced_waiting};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn persistence() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, "migration-token", None)
}

async fn fixture(channel: u64) -> (TestPostgresDb, sqlx::PgPool, WaitIntent, i64) {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate_with_max_connections(1).await;
    let (row, intent) = setup(&pool, channel, false).await;
    pending_row(&pool, row.id).await;
    (db, pool, intent, row.id)
}

fn item(id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(100),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: 7,
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "original queue body".into(),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

fn request(fence: FencedWaiting, count: &Arc<AtomicUsize>) -> PublicationRequest {
    let count = count.clone();
    PublicationRequest::new(fence, Vec::new(), persistence(), move || {
        count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    })
}

#[tokio::test]
async fn migration_publication_rechecks_actual_active_turn_before_callback_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let channel = ChannelId::new(750330101);
    let (db, pool, intent, _) = fixture(channel.get()).await;
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(channel);
    assert!(
        handle
            .try_start_turn(
                Arc::new(CancelToken::new()),
                UserId::new(100),
                MessageId::new(33)
            )
            .await
    );
    assert!(handle.try_snapshot().await.unwrap().cancel_token.is_some());
    let count = Arc::new(AtomicUsize::new(0));
    let fence = fenced_waiting(&pool, &intent).await.unwrap();
    let result = handle
        .publish_migration(request(fence, &count))
        .await
        .unwrap();
    let outcome = result.release().await.unwrap();
    handle.hard_stop().await;
    pool.close().await;
    db.drop().await;
    assert_eq!(
        outcome,
        Err(Refusal::Active),
        "an actual active actor turn must refuse migration publication"
    );
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "active custody must keep the publication callback untouched"
    );
}

#[tokio::test]
async fn migration_publication_rechecks_actual_unverified_queue_before_callback_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let channel = ChannelId::new(750330102);
    let (db, pool, intent, _) = fixture(channel.get()).await;
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(channel);
    let mut original = item(750330103);
    original.source_message_queued_generations =
        vec![SourceMessageQueuedGeneration::user_instruction(
            original.message_id,
            original.queued_generation,
        )];
    original.source_message_queued_generations[0].origin = Some("LiveMessage".into());
    assert!(
        handle
            .enqueue(original.clone(), persistence())
            .await
            .enqueued
    );
    let before = handle.try_snapshot().await.unwrap();
    assert_eq!(before.intervention_queue[0].message_id, original.message_id);
    let count = Arc::new(AtomicUsize::new(0));
    let fence = fenced_waiting(&pool, &intent).await.unwrap();
    let result = handle
        .publish_migration(request(fence, &count))
        .await
        .unwrap();
    let outcome = result.release().await.unwrap();
    let after = handle.try_snapshot().await.unwrap();
    pool.close().await;
    db.drop().await;
    assert_eq!(
        outcome,
        Err(Refusal::QueueChanged),
        "every actual queued source needs its own verified custody witness"
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(after.intervention_queue.len(), 1);
    assert_eq!(after.intervention_queue[0].created_at, original.created_at);
    assert_eq!(
        after.intervention_queue[0].source_message_ids,
        original.source_message_ids
    );
}

#[tokio::test]
async fn migration_publication_discarded_receiver_never_runs_late_callback_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let channel = ChannelId::new(750330104);
    let (db, pool, intent, _) = fixture(channel.get()).await;
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(channel);
    let count = Arc::new(AtomicUsize::new(0));
    let fence = fenced_waiting(&pool, &intent).await.unwrap();
    let (reply, answer) = oneshot::channel();
    drop(answer);
    handle
        .sender
        .send(ChannelMailboxMsg::MigrationPublication {
            request: Box::new(request(fence, &count)),
            reply,
        })
        .ok()
        .unwrap();
    handle.try_snapshot().await.unwrap();
    pool.close().await;
    db.drop().await;
    assert_eq!(
        count.load(Ordering::SeqCst),
        0,
        "a canceled publication receiver cannot run a late callback"
    );
}

#[tokio::test]
async fn migration_publication_quiet_actor_keeps_guard_until_reply_release_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let channel = ChannelId::new(750330105);
    let (db, pool, intent, _) = fixture(channel.get()).await;
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(channel);
    let count = Arc::new(AtomicUsize::new(0));
    let fence = fenced_waiting(&pool, &intent).await.unwrap();
    assert_eq!(fence.waiting().requests().len(), 1);
    let result = handle
        .publish_migration(request(fence, &count))
        .await
        .unwrap();
    assert_eq!(result.outcome(), &Ok(()));
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "the positive actor publication path must execute once"
    );
    result.release().await.unwrap().unwrap();
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn migration_publication_closed_actor_refuses_without_replacing_successor_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let channel = ChannelId::new(750330106);
    let (db, pool, intent, _) = fixture(channel.get()).await;
    let registry = ChannelMailboxRegistry::default();
    let stale = registry.handle(channel);
    assert_eq!(
        registry.remove_idle_entry(channel).await,
        registry_purge::MailboxPurgeOutcome::Removed
    );
    let successor = registry.handle(channel);
    assert!(
        successor
            .enqueue(item(750330107), persistence())
            .await
            .enqueued
    );
    let count = Arc::new(AtomicUsize::new(0));
    let fence = fenced_waiting(&pool, &intent).await.unwrap();
    let result = stale
        .publish_migration(request(fence, &count))
        .await
        .unwrap();
    let outcome = result.release().await.unwrap();
    let snapshot = successor.try_snapshot().await.unwrap();
    pool.close().await;
    db.drop().await;
    assert_eq!(
        outcome,
        Err(Refusal::Closed),
        "a purge-closed actor cannot publish over its successor"
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(
        snapshot.intervention_queue[0].message_id,
        MessageId::new(750330107)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn migration_publication_owns_db_fence_after_caller_cancel_and_serializes_enqueue_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let channel = ChannelId::new(750330108);
    let (db, pool, intent, row) = fixture(channel.get()).await;
    let peer = crate::db::postgres::connect_test_pool_with_max_connections(
        &db.database_url,
        "actor publication peer",
        2,
    )
    .await
    .unwrap();
    let mut connection = peer.acquire().await.unwrap();
    let peer_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *connection)
        .await
        .unwrap();
    sqlx::query("SET lock_timeout='200ms'")
        .execute(&mut *connection)
        .await
        .unwrap();
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(channel);
    let fence = fenced_waiting(&pool, &intent).await.unwrap();
    let (entered, arrived) = oneshot::channel();
    let (leave, resume) = std::sync::mpsc::channel();
    let publication = PublicationRequest::new(fence, Vec::new(), persistence(), move || {
        entered.send(()).ok().unwrap();
        resume.recv_timeout(Duration::from_secs(10)).unwrap();
        Ok(())
    });
    let publisher = handle.clone();
    let caller = tokio::spawn(async move { publisher.publish_migration(publication).await });
    tokio::time::timeout(Duration::from_secs(3), arrived)
        .await
        .unwrap()
        .unwrap();
    caller.abort();
    assert!(matches!(caller.await, Err(error) if error.is_cancelled()));
    let (reply, mut answer) = oneshot::channel();
    handle
        .sender
        .send(ChannelMailboxMsg::Enqueue {
            intervention: item(750330109),
            persistence: persistence(),
            observed: None,
            reply,
        })
        .ok()
        .unwrap();
    let early_enqueue = answer.try_recv();
    let enqueue_waited = matches!(&early_enqueue, Err(oneshot::error::TryRecvError::Empty));
    let mut writer = tokio::spawn(async move {
        sqlx::query(
            "UPDATE intake_outbox SET last_error='actor peer after publication' WHERE id=$1",
        )
        .bind(row)
        .execute(&mut *connection)
        .await
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut observation_error = None;
    let blocked = loop {
        let blocked: bool = match sqlx::query_scalar("SELECT cardinality(pg_blocking_pids($1))>0")
            .bind(peer_pid)
            .fetch_one(&peer)
            .await
        {
            Ok(value) => value,
            Err(error) => {
                observation_error = Some(error);
                break false;
            }
        };
        if blocked || writer.is_finished() || tokio::time::Instant::now() >= deadline {
            break blocked;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let written = match tokio::time::timeout(Duration::from_secs(3), &mut writer).await {
        Ok(done) => Some(done),
        Err(_) => {
            writer.abort();
            let _ = writer.await;
            None
        }
    };
    let held_code = written
        .as_ref()
        .and_then(|joined| joined.as_ref().ok())
        .and_then(|result| result.as_ref().err())
        .and_then(sqlx::Error::as_database_error)
        .and_then(|error| error.code())
        .map(|code| code.into_owned());
    let left = leave.send(());
    let enqueued = match early_enqueue {
        Ok(enqueued) => Some(enqueued),
        Err(oneshot::error::TryRecvError::Empty) => {
            tokio::time::timeout(Duration::from_secs(3), answer)
                .await
                .ok()
                .and_then(Result::ok)
        }
        Err(oneshot::error::TryRecvError::Closed) => None,
    };
    let after_release = tokio::time::timeout(Duration::from_secs(3), async {
        let mut connection = peer.acquire().await?;
        sqlx::query("SET lock_timeout='2s'")
            .execute(&mut *connection)
            .await?;
        sqlx::query("UPDATE intake_outbox SET last_error='actor peer after release' WHERE id=$1")
            .bind(row)
            .execute(&mut *connection)
            .await
    })
    .await;
    peer.close().await;
    pool.close().await;
    db.drop().await;
    left.expect("callback release must reach the original actor");
    tracing::info!(
        blocked,
        ?observation_error,
        ?held_code,
        "observed the peer's physical lock retention"
    );
    assert!(
        enqueue_waited,
        "peer enqueue must remain behind the synchronous publication callback"
    );
    assert_eq!(
        held_code.as_deref(),
        Some("55P03"),
        "a paused publication must retain the real SHARE lock through the peer lock_timeout"
    );

    assert_eq!(
        after_release
            .expect("fence drop must release the new peer DML")
            .expect("the new real peer update must succeed")
            .rows_affected(),
        1
    );
    assert!(
        enqueued
            .expect("the original peer enqueue must answer")
            .enqueued,
        "ordinary enqueue must continue in the original actor order after publication"
    );
}

#[tokio::test]
async fn migration_publication_keeps_unknown_database_responsibility_blocking_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let channel = ChannelId::new(750330110);
    let (db, pool, intent, id) = fixture(channel.get()).await;
    sqlx::query("UPDATE intake_outbox SET status='unknown' WHERE id=$1")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(channel);
    let count = Arc::new(AtomicUsize::new(0));
    let fence = fenced_waiting(&pool, &intent).await.unwrap();
    assert_eq!(
        fence.blockers().len(),
        1,
        "the actual unknown DB responsibility must be observed"
    );
    assert!(fence.waiting().requests().is_empty());
    let result = handle
        .publish_migration(request(fence, &count))
        .await
        .unwrap();
    let outcome = result.release().await.unwrap();
    pool.close().await;
    db.drop().await;
    assert_eq!(
        outcome,
        Err(Refusal::Database),
        "unknown database custody must block the actual actor publication"
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn migration_publication_rejects_changed_queue_namespace_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let channel = ChannelId::new(750330111);
    let (db, pool, intent, _) = fixture(channel.get()).await;
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(channel);
    let count = Arc::new(AtomicUsize::new(0));
    let observed = count.clone();
    let fence = fenced_waiting(&pool, &intent).await.unwrap();
    assert_eq!(fence.waiting().requests().len(), 1);
    let wrong =
        QueuePersistenceContext::new(&ProviderKind::Claude, "another-migration-token", None);
    let publication = PublicationRequest::new(fence, Vec::new(), wrong, move || {
        observed.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    let result = handle.publish_migration(publication).await.unwrap();
    let outcome = result.release().await.unwrap();
    pool.close().await;
    db.drop().await;
    assert_eq!(
        outcome,
        Err(Refusal::Namespace),
        "database authority cannot publish under another queue namespace"
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn migration_publication_keeps_dequeued_original_dispatch_blocking_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let channel = ChannelId::new(750330112);
    let (db, pool, intent, _) = fixture(channel.get()).await;
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(channel);
    let original = item(750330113);
    assert!(
        handle
            .enqueue(original.clone(), persistence())
            .await
            .enqueued
    );
    let taken = handle.take_next_soft(persistence()).await;
    assert_eq!(
        taken.intervention.as_ref().unwrap().message_id,
        original.message_id
    );
    let before = handle.try_snapshot().await.unwrap();
    assert!(before.intervention_queue.is_empty());
    assert_eq!(before.pending_user_dispatch, Some(original.message_id));
    let count = Arc::new(AtomicUsize::new(0));
    let fence = fenced_waiting(&pool, &intent).await.unwrap();
    let result = handle
        .publish_migration(request(fence, &count))
        .await
        .unwrap();
    let outcome = result.release().await.unwrap();
    let restored = handle
        .restore_dequeued_head(
            taken.intervention.unwrap(),
            persistence(),
            taken.dispatch_lease.unwrap(),
        )
        .await;
    let after = handle.try_snapshot().await.unwrap();
    pool.close().await;
    db.drop().await;
    assert_eq!(
        outcome,
        Err(Refusal::PendingDispatch),
        "a dequeued original outside the actor remains blocking custody"
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert!(restored.enqueued);
    assert_eq!(after.intervention_queue[0].message_id, original.message_id);
    assert_eq!(after.intervention_queue[0].created_at, original.created_at);
}

#[tokio::test]
async fn migration_publication_dangling_queue_snapshot_is_not_empty_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    let channel = ChannelId::new(750330114);
    let (db, pool, intent, _) = fixture(channel.get()).await;
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(channel);
    let path =
        crate::services::turn_orchestrator::pending_queue_persistence::pending_queue_file_path(
            &ProviderKind::Claude,
            "migration-token",
            channel,
        )
        .unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink("absent-queue-target", &path).unwrap();
    assert!(
        std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let count = Arc::new(AtomicUsize::new(0));
    let fence = fenced_waiting(&pool, &intent).await.unwrap();
    let result = handle
        .publish_migration(request(fence, &count))
        .await
        .unwrap();
    let outcome = result.release().await.unwrap();
    pool.close().await;
    db.drop().await;
    assert_eq!(
        outcome,
        Err(Refusal::Head),
        "an unknown queue snapshot is not proof of empty custody"
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn migration_publication_cold_actor_cannot_ignore_previous_durable_queue_pg() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_dir, _root) = root();
    for (known, id) in [(false, 750330115), (true, 750330118)] {
        let channel = ChannelId::new(id);
        let (db, pool, intent, _) = fixture(channel.get()).await;
        let previous = ChannelMailboxRegistry::default();
        let previous_handle = previous.handle(channel);
        let mut original = item(id + 2);
        if known {
            original.source_message_queued_generations =
                vec![SourceMessageQueuedGeneration::user_instruction(
                    original.message_id,
                    original.queued_generation,
                )];
            original.source_message_queued_generations[0].origin = Some("LiveMessage".into());
        }
        assert!(
            previous_handle
                .enqueue(original.clone(), persistence())
                .await
                .enqueued
        );
        let disk = crate::services::turn_orchestrator::load_channel_pending_queue_checked(
            &ProviderKind::Claude,
            "migration-token",
            channel,
        )
        .unwrap();
        assert_eq!(
            disk.0.len(),
            1,
            "the previous real producer owns a durable queue item"
        );
        let path =
            crate::services::turn_orchestrator::pending_queue_persistence::pending_queue_file_path(
                &ProviderKind::Claude,
                "migration-token",
                channel,
            )
            .unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        let (_, meta) = queued_head::parse_snapshot(&content).unwrap();
        assert!(
            meta.is_none_or(|meta| !meta.is_set()),
            "the regression is ordinary queue custody with no head metadata"
        );
        assert_eq!(disk.0[0].message_id, original.message_id);
        drop(previous_handle);
        drop(previous);
        let restarted = ChannelMailboxRegistry::default();
        let handle = restarted.handle(channel);
        let fresh = handle.try_snapshot().await.unwrap();
        assert!(
            fresh.intervention_queue.is_empty(),
            "the new actual actor has not hydrated prior custody"
        );
        assert!(fresh.cancel_token.is_none() && fresh.pending_user_dispatch.is_none());
        let count = Arc::new(AtomicUsize::new(0));
        let fence = fenced_waiting(&pool, &intent).await.unwrap();
        assert_eq!(fence.waiting().requests().len(), 1);
        assert!(fence.blockers().is_empty());
        let result = handle
            .publish_migration(request(fence, &count))
            .await
            .unwrap();
        let outcome = result.release().await.unwrap();
        let after = handle.try_snapshot().await.unwrap();
        let disk_after = crate::services::turn_orchestrator::load_channel_pending_queue_checked(
            &ProviderKind::Claude,
            "migration-token",
            channel,
        )
        .unwrap()
        .0;
        pool.close().await;
        db.drop().await;
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "a fresh actor cannot publish while prior durable queue custody is unobserved"
        );
        assert!(
            outcome.is_err(),
            "unhydrated durable custody stays a typed refusal"
        );
        assert!(
            after.intervention_queue.is_empty(),
            "publication cannot implicitly hydrate or take prior queue custody"
        );
        assert_eq!(disk_after.len(), 1);
        assert_eq!(disk_after[0].message_id, original.message_id);
        assert_eq!(disk_after[0].queued_generation, original.queued_generation);
    }
}
