use super::*;
use crate::services::discord::router::message_handler::PrivateOldTmuxForTest;
use std::sync::atomic::{AtomicUsize, Ordering};

struct WakeGuard(Channel);
impl Drop for WakeGuard {
    fn drop(&mut self) {
        crate::services::discord::queue_io::cancel_backstop_test_support::set_owner_wake_for_tests(
            self.0, None,
        );
    }
}

#[tokio::test]
async fn f3_a_changed_pin_holds_the_original_without_launching_or_closing_a_peer_pg() {
    let root_guard = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    shared.restart.reconcile_done.store(true, Ordering::Release);
    let channel = Channel::new(1_479_671_301_387_059_301);
    let logical = "i8077-actual-pin-revocation";
    let old = PrivateOldTmuxForTest::new(logical, None);
    old.start();
    assert!(old.exists());
    assert!(old.peer_exists());
    let key = format!("claude/{}/test-node:{logical}", shared.token_hash);
    let row = seeded(&pool, &key, &shared.token_hash, channel.get(), None).await;
    assert!(matches!(row.record, HostedRecord::Legacy));
    FORCED.lock().unwrap().insert(
        channel.get(),
        Forced {
            key,
            row,
            attempts: 0,
            done: None,
            real_close: true,
        },
    );
    let forced = Guard(channel.get());
    let mailbox = shared.mailbox(channel);
    let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, &shared.token_hash, None);
    let (path, namespace) =
        crate::services::turn_orchestrator::v2_queue(&persistence, channel).unwrap();
    mailbox.v2_adopt(path, namespace).await.unwrap();
    assert!(!mailbox.has_active_turn().await.unwrap());
    let queued = original(channel.get(), 8_000_000_008_077_011);
    let expected = serde_json::to_value(&queued).unwrap();
    let front = mailbox
        .v2_request(move |slot| {
            let owner = slot.as_mut().unwrap();
            assert!(owner.enqueue_fixture(&queued).unwrap());
            let front = owner.front().unwrap();
            owner.grant_front(&front)?;
            Ok(front)
        })
        .await
        .unwrap();
    let launches = Arc::new(AtomicUsize::new(0));
    let counted = launches.clone();
    let idle: crate::services::discord::queue_io::input_owner::drive::Handoff =
        Arc::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err("a rejected close must never reach the launch callback".into()) })
        });
    let drive = crate::services::discord::router::message_handler::owner_drive_for_test(
        (mailbox.clone(), Some(pool.clone())),
        (ProviderKind::Claude, channel.get()),
        idle,
        Some(shared.clone()),
    );
    let (woken, wake) = tokio::sync::oneshot::channel();
    let woken = Arc::new(Mutex::new(Some(woken)));
    let hook = Arc::new(move || {
        if let Some(woken) = woken.lock().unwrap().take() {
            woken.send(()).unwrap();
        }
    });
    let previous =
        crate::services::discord::queue_io::cancel_backstop_test_support::set_owner_wake_for_tests(
            channel,
            Some(hook),
        );
    let wake_guard = WakeGuard(channel);
    assert!(previous.is_none());
    old.revoke_nonce_on_recheck();
    assert_eq!((drive.handoff)(front).await.unwrap_err(), HELD);
    let state = mailbox
        .v2_request(|slot| slot.as_ref().unwrap().json())
        .await
        .unwrap();
    let items = state["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["message_id"], expected["message_id"]);
    assert_eq!(items[0]["text"], expected["text"]);
    assert_eq!(
        items[0]["source_message_ids"],
        expected["source_message_ids"]
    );
    assert!(state["head_injection"].is_null());
    assert!(state["retained_inputs"].as_array().unwrap().is_empty());
    let journal = &state["unfinished_handoff"];
    assert_eq!(journal["logical"], logical);
    assert!(
        !journal["pin"].is_null(),
        "the real close must pin before its failed recheck"
    );
    use crate::services::tui_prompt_dedupe::binding_context::{
        SpawnNonceMarker, observe_spawn_nonce_marker,
    };
    let SpawnNonceMarker::Known(current) = observe_spawn_nonce_marker(logical) else {
        panic!("actual recheck must leave a known replacement nonce");
    };
    assert_eq!(current, "fedcba9876543210fedcba9876543210");
    assert_ne!(journal["pin"]["nonce"].as_str(), Some(current.as_str()));
    assert_eq!(journal["ended"], false);
    assert_eq!(journal["failures"], 1);
    assert_eq!(launches.load(Ordering::SeqCst), 0);
    assert_eq!(old.closes(), 0);
    assert!(old.exists());
    assert!(old.peer_exists());
    old.assert_no_input();
    tokio::time::timeout(std::time::Duration::from_secs(6), wake)
        .await
        .unwrap()
        .unwrap();
    let after_wake = mailbox
        .v2_request(|slot| slot.as_ref().unwrap().json())
        .await
        .unwrap();
    assert_eq!(after_wake["items"], state["items"]);
    assert_eq!(
        after_wake["unfinished_handoff"],
        state["unfinished_handoff"]
    );
    assert_eq!(launches.load(Ordering::SeqCst), 0);
    assert_eq!(old.closes(), 0);
    assert!(old.exists());
    assert!(old.peer_exists());
    old.assert_no_input();
    // Remove the fixture's owed input so a scheduled wake cannot restart it during teardown.
    mailbox
        .v2_request(|slot| {
            let owner = slot.as_mut().unwrap();
            let cleared = owner.clear_waiting()?;
            assert_eq!(cleared.queued.len(), 1);
            assert!(owner.handoff().is_some_and(|journal| !journal.ended));
            Ok(())
        })
        .await
        .unwrap();
    drop(wake_guard);
    drop(forced);
    pool.close().await;
    db.drop().await;
    drop(old);
    drop(root_guard);
}
