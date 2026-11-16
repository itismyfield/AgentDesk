use super::*;
use crate::db::dispatched_sessions::hosted_execution::{self as hosted, HostedState};
use crate::services::discord::queue_io::input_owner::{claude_handoff, notice};
use serde_json::Value;

#[derive(Clone, Default)]
pub(super) struct Trace(pub(super) Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for Trace {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
async fn state(mailbox: &ChannelMailboxHandle) -> Value {
    mailbox
        .v2_request(|slot| slot.as_ref().ok_or("owner")?.json())
        .await
        .unwrap()
}
async fn grant(mailbox: &ChannelMailboxHandle) -> Front {
    mailbox
        .v2_request(|slot| {
            let owner = slot.as_mut().ok_or("owner")?;
            let front = owner.front().ok_or("front")?;
            owner.grant_front(&front)?;
            Ok(front)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn expired_launch_deadline_keeps_backoff_on_a_prepare_refusal() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Claude;
    let channel = ChannelId::new(8_146_002);
    // The journal keeps its custody only while boot config still routes the channel to Herdr.
    let _hosts = crate::config::session_hosts::force_for_test(Some("n"), &[(channel.get(), "n")]);
    let mailbox = shared.mailbox(channel);
    let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, &shared.token_hash, None);
    let (path, namespace) =
        crate::services::turn_orchestrator::v2_queue(&persistence, channel).unwrap();
    mailbox.v2_adopt(path, namespace).await.unwrap();
    let original: PendingQueueItem = serde_json::from_value(json!({
        "author_id":7,"message_id":8146012,"channel_id":channel.get(),"queued_generation":1,"text":"held original"
    })).unwrap();
    mailbox
        .v2_admit_ticket(original, "expired-deadline".into())
        .await
        .unwrap();
    let front = grant(&mailbox).await;
    let begin = front.clone();
    mailbox
        .v2_request(move |slot| {
            let owner = slot.as_mut().unwrap();
            let journal = owner.begin_handoff(&begin, "owned".into(), "key".into())?;
            owner.handoff_ended(&journal.id)?;
            owner
                .snapshot
                .unfinished_handoff
                .as_mut()
                .unwrap()
                .retry_after_ms = 1;
            Ok(())
        })
        .await
        .unwrap();
    let (wake, mut woke) = tokio::sync::mpsc::unbounded_channel();
    counted::set_owner_wake_for_tests(
        channel,
        Some(Arc::new(move || {
            let _ = wake.send(());
        })),
    );
    let _scope = Scope(channel);
    let started = Instant::now();
    assert!(
        claude_handoff::prepare(
            Some(&shared),
            &mailbox,
            &ProviderKind::Claude,
            channel.get(),
            &front,
            claude_handoff::Start::Person
        )
        .await
        .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), woke.recv())
            .await
            .is_err(),
        "an expired launch deadline must not turn another hold into an immediate retry"
    );
    tokio::time::timeout(Duration::from_secs(5), woke.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(started.elapsed() >= Duration::from_secs(1));
    let snapshot = state(&mailbox).await;
    assert_eq!(snapshot["unfinished_handoff"]["failures"], 1);
    assert_eq!(snapshot["items"].as_array().unwrap().len(), 1);
    assert!(snapshot["retained_inputs"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn restored_ended_retired_launch_retries_backoff_then_retains_and_notifies_once_pg() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Claude;
    let channel = ChannelId::new(8_146_001);
    let message = 8_146_011;
    let logical = "handoff-launch-retry";
    let key = format!("claude/{}/test-node:{logical}", shared.token_hash);
    let mut owner = hosted::tests::owner(&channel.get().to_string());
    owner.discord_token_hash = shared.token_hash.clone();
    owner.logical_key = logical.into();
    let record = hosted::tests::record(&owner, "retired-nonce", HostedState::Retired);
    sqlx::query("INSERT INTO sessions (session_key,provider,status,identity_kind,discord_token_hash,channel_id,hosted_execution) VALUES ($1,'claude','idle','discord_channel',$2,$3,$4)")
        .bind(&key).bind(&shared.token_hash).bind(channel.to_string()).bind(serde_json::to_value(record).unwrap()).execute(&pool).await.unwrap();
    let hosted::HostedLookup::Found(row) =
        hosted::load_hosted_execution_pg(&pool, hosted::HostedLookupKey::SessionKey(&key)).await
    else {
        panic!("retired row")
    };
    let _host = claude_handoff::tests::force_host(channel.get(), key.clone(), row);
    let mailbox = shared.mailbox(channel);
    let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, &shared.token_hash, None);
    let (path, namespace) =
        crate::services::turn_orchestrator::v2_queue(&persistence, channel).unwrap();
    mailbox
        .v2_adopt(path.clone(), namespace.clone())
        .await
        .unwrap();
    let original: PendingQueueItem = serde_json::from_value(json!({"author_id":7,"message_id":message,"channel_id":channel.get(),"queued_generation":7,"text":"launch original, exact bytes"})).unwrap();
    mailbox
        .v2_admit_ticket(original.clone(), "handoff-original".into())
        .await
        .unwrap();
    let first = grant(&mailbox).await;
    let raw = mailbox
        .v2_request(move |slot| {
            let owner = slot.as_mut().unwrap();
            let journal = owner.begin_handoff(&first, logical.into(), key)?;
            owner.handoff_ended(&journal.id)?;
            owner.json()
        })
        .await
        .unwrap();
    // Restore exactly the older ended journal format, with no deadline field.
    let mut old = raw;
    old["unfinished_handoff"]
        .as_object_mut()
        .unwrap()
        .remove("retry_after_ms");
    std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    mailbox
        .v2_adopt(path.clone(), namespace.clone())
        .await
        .unwrap();
    let (wake, mut woke) = tokio::sync::mpsc::unbounded_channel();
    counted::set_owner_wake_for_tests(
        channel,
        Some(Arc::new(move || {
            let _ = wake.send(());
        })),
    );
    let _scope = Scope(channel);
    let trace = Trace::default();
    let writer = trace.clone();
    crate::logging::test_capture::pin_callsite_interest();
    let _tracing = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish(),
    );
    for attempt in 1..=5 {
        let front = grant(&mailbox).await;
        assert_eq!(front.1.message_id, message);
        assert_eq!(
            claude_handoff::prepare(
                Some(&shared),
                &mailbox,
                &ProviderKind::Claude,
                channel.get(),
                &front,
                claude_handoff::Start::Person
            )
            .await,
            Ok(true)
        );
        let claim = claim_turn(&mailbox, message, &front)
            .await
            .unwrap()
            .unwrap();
        assert!(
            claim.followers().is_empty(),
            "a new suffix never inherits the original's failure budget"
        );
        let duplicate = Failure::claimed(&mailbox, &claim);
        let before = chrono::Utc::now().timestamp_millis().max(0) as u64;
        assert_eq!(
            Failure::claimed(&mailbox, &claim)
                .commit(
                    Kind::Opaque,
                    "herdr launch auth_overlay: forced launch failure".into(),
                    Ok(None),
                    Some(shared.clone()),
                    channel,
                    None
                )
                .await
                .unwrap(),
            Some(attempt == 5)
        );
        assert_eq!(
            duplicate
                .commit(
                    Kind::Opaque,
                    "late duplicate".into(),
                    Ok(None),
                    Some(shared.clone()),
                    channel,
                    None
                )
                .await
                .unwrap(),
            None
        );
        let snapshot = state(&mailbox).await;
        if attempt == 5 {
            assert!(snapshot["unfinished_handoff"].is_null());
            assert_eq!(snapshot["retained_inputs"].as_array().unwrap().len(), 1);
            assert_eq!(
                snapshot["retained_inputs"][0]["input"]["original"],
                serde_json::to_value(&original).unwrap()
            );
            assert_eq!(
                snapshot["retained_inputs"][0]["reason"]["start_failure"]["failures"],
                5
            );
            assert_eq!(
                snapshot["retained_inputs"][0]["disposition"],
                "known_not_sent"
            );
            break;
        }
        assert_eq!(snapshot["unfinished_handoff"]["failures"], attempt);
        assert_eq!(
            snapshot["items"][0],
            serde_json::to_value(&original).unwrap()
        );
        assert!(snapshot["retained_inputs"].as_array().unwrap().is_empty());
        let due = snapshot["unfinished_handoff"]["retry_after_ms"]
            .as_u64()
            .unwrap();
        let delay = 1_000u64 << attempt;
        assert!(due >= before + delay);
        let front = grant(&mailbox).await;
        assert!(
            claude_handoff::prepare(
                Some(&shared),
                &mailbox,
                &ProviderKind::Claude,
                channel.get(),
                &front,
                claude_handoff::Start::Person
            )
            .await
            .is_err(),
            "an unrelated early wake cannot launch"
        );
        if attempt == 1 {
            let mut following = original.clone();
            following.message_id += 1;
            following.text = "following input".into();
            mailbox
                .v2_admit_ticket(following, "handoff-following".into())
                .await
                .unwrap();
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), woke.recv())
                .await
                .is_err(),
            "no immediate wake on launch failure"
        );
        tokio::time::timeout(Duration::from_millis(delay + 2_000), woke.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(chrono::Utc::now().timestamp_millis().max(0) as u64 >= due);
        // A process restart must preserve the failure budget and deadline.
        mailbox
            .v2_adopt(path.clone(), namespace.clone())
            .await
            .unwrap();
    }
    let snapshot = state(&mailbox).await;
    let retained = snapshot["retained_inputs"][0].clone();
    assert_eq!(
        notice::drain_synchronously_for_tests(&pool, &mailbox, "claude", channel.get())
            .await
            .unwrap(),
        1
    );
    crate::services::discord::queue_io::retained_notice::enqueue(
        &pool,
        "claude",
        channel.get(),
        &namespace,
        &retained,
    )
    .await
    .unwrap();
    assert_eq!(
        notice::drain_synchronously_for_tests(&pool, &mailbox, "claude", channel.get())
            .await
            .unwrap(),
        0
    );
    let posts: Vec<String> = sqlx::query_scalar(
        "SELECT content FROM message_outbox WHERE target=$1 AND source='retained_input_notice'",
    )
    .bind(format!("channel:{channel}"))
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(posts.len(), 1);
    assert!(posts[0].contains("다음 입력은 계속 처리합니다"));
    assert_eq!(
        state(&mailbox).await["retained_inputs"][0]["notice"]["enqueued"],
        true
    );
    let front = grant(&mailbox).await;
    assert_eq!(front.1.message_id, message + 1);
    assert_eq!(
        claude_handoff::prepare(
            Some(&shared),
            &mailbox,
            &ProviderKind::Claude,
            channel.get(),
            &front,
            claude_handoff::Start::Person
        )
        .await,
        Ok(false),
        "Retired starts no new migration journal"
    );
    let claim = claim_turn(&mailbox, message + 1, &front)
        .await
        .unwrap()
        .unwrap();
    mailbox
        .v2_request(move |slot| {
            slot.as_mut()
                .unwrap()
                .complete_claimed_locally((claim.front(), claim.number()), 0)
        })
        .await
        .unwrap();
    let snapshot = state(&mailbox).await;
    assert!(snapshot["items"].as_array().unwrap().is_empty());
    assert_eq!(snapshot["retained_inputs"].as_array().unwrap().len(), 1);
    let log = String::from_utf8(trace.0.lock().unwrap().clone()).unwrap();
    assert!(log.contains("channel_id=8146001"), "{log}");
    assert!(log.contains("front_id=8146011"), "{log}");
    assert!(log.contains("stage=\"provider_start\""), "{log}");
    assert!(log.contains("auth_overlay: forced launch failure"), "{log}");
    pool.close().await;
    db.drop().await;
}
