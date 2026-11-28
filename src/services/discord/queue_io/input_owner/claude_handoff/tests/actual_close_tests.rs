#[path = "custody_tests.rs"]
mod custody_tests;
#[path = "left_herdr_tests.rs"]
mod left_herdr_tests;

use crate::db::dispatched_sessions::hosted_execution::{HostedRecord, HostedState};
use crate::services::claude::session_handoff::pinned_tmux;
use crate::services::discord::queue_io::input_owner::claude_handoff::hook_tests::{
    close_attempts, force_host, herdr_host, install_forced, refuse_host, retry_waiting,
};
use crate::services::discord::queue_io::input_owner::claude_handoff::{
    HELD, now_ms, recover, schedule,
};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::ChannelMailboxHandle;
use poise::serenity_prelude::ChannelId as Channel;
use std::sync::{Arc, Mutex};

use crate::db::dispatched_sessions::hosted_execution::{
    HostedLookup, HostedLookupKey, HostedObservation, load_hosted_execution_pg,
};
use crate::services::discord::router::message_handler::PrivateOldTmuxForTest;
use crate::services::provider::CancelToken;
use crate::services::turn_orchestrator::{PendingQueueItem, QueuePersistenceContext};
use poise::serenity_prelude::{MessageId, UserId};
use std::sync::atomic::{AtomicUsize, Ordering};

async fn seeded(
    pool: &sqlx::PgPool,
    key: &str,
    token: &str,
    channel: u64,
    raw: Option<serde_json::Value>,
) -> HostedObservation {
    sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id, hosted_execution) VALUES ($1,'claude','idle','discord_channel',$2,$3,$4)")
        .bind(key).bind(token).bind(channel.to_string()).bind(raw).execute(pool).await.unwrap();
    let HostedLookup::Found(row) =
        load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(key)).await
    else {
        panic!("canonical fixture row not found")
    };
    row
}
fn original(channel: u64, id: u64) -> PendingQueueItem {
    serde_json::from_value(serde_json::json!({"author_id":7,"message_id":id,"channel_id":channel,"source_message_ids":[id],"queued_generation":1,"text":"same text, distinct original"})).unwrap()
}
#[tokio::test]
async fn active_legacy_first_message_begins_before_busy_registration_and_two_inputs_stay_queued_pg()
{
    let root_guard = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    shared
        .restart
        .reconcile_done
        .store(true, std::sync::atomic::Ordering::Release);
    let channel = Channel::new(8077001);
    let logical = "i8077-active-test";
    let key = format!("claude/{}/test-node:{logical}", shared.token_hash);
    let row = seeded(&pool, &key, &shared.token_hash, channel.get(), None).await;
    assert!(matches!(row.record, HostedRecord::Legacy));
    let forced = install_forced(channel.get(), key, row, false, None);
    let mailbox = shared.mailbox(channel);
    let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, &shared.token_hash, None);
    let (path, namespace) =
        crate::services::turn_orchestrator::v2_queue(&persistence, channel).unwrap();
    mailbox.v2_adopt(path, namespace).await.unwrap();
    mailbox
        .restore_active_turn(
            Arc::new(CancelToken::new()),
            UserId::new(7),
            MessageId::new(8077000),
        )
        .await;
    assert!(mailbox.has_active_turn().await.unwrap());
    let admit = |message| {
        let mailbox = mailbox.clone();
        async move {
            mailbox
                .v2_request(move |slot| {
                    slot.as_mut()
                        .unwrap()
                        .enqueue_fixture(&original(channel.get(), message))
                        .map_err(|e| format!("{e:?}"))
                })
                .await
                .unwrap()
        }
    };
    let (first, second) = tokio::join!(admit(8_000_000_008_077_001), admit(8_000_000_008_077_002));
    assert!(first && second);
    let idle: crate::services::discord::queue_io::input_owner::drive::Handoff = Arc::new(|_| {
        Box::pin(async { panic!("fixed close failure cannot launch or use old busy input") })
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
    assert!(previous.is_none());
    let mut saved = None;
    for _ in 0..3 {
        let front = mailbox
            .v2_request(|slot| {
                let owner = slot.as_mut().unwrap();
                let front = owner.front().unwrap();
                owner.grant_front(&front)?;
                Ok(front)
            })
            .await
            .unwrap();
        assert_eq!((drive.handoff)(front).await.unwrap_err(), HELD);
        let state = mailbox
            .v2_request(|slot| slot.as_ref().unwrap().json())
            .await
            .unwrap();
        assert_eq!(state["items"].as_array().unwrap().len(), 2);
        assert!(state["head_injection"].is_null());
        assert!(state["retained_inputs"].as_array().unwrap().is_empty());
        let id = state["unfinished_handoff"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        if let Some(saved) = &saved {
            assert_eq!(saved, &id);
        } else {
            saved = Some(id);
        }
    }
    assert_eq!(close_attempts(channel.get()), 3);
    tokio::time::timeout(std::time::Duration::from_secs(6), wake)
        .await
        .unwrap()
        .unwrap();
    crate::services::discord::queue_io::cancel_backstop_test_support::set_owner_wake_for_tests(
        channel, None,
    );
    let notice_key = format!("claude_handoff:{}:retry", saved.as_ref().unwrap());
    let notices: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM message_outbox WHERE session_key=$1 AND source='lifecycle_notifier'",
    )
    .bind(&notice_key)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        notices, 1,
        "persistent close failure owes exactly one notice"
    );
    let id = saved.unwrap();
    mailbox
        .v2_request(move |slot| slot.as_mut().unwrap().finish_handoff(&id))
        .await
        .unwrap();
    drop(forced);
    pool.close().await;
    db.drop().await;
    drop(root_guard);
}
#[tokio::test]
async fn journal_only_boot_census_selects_actor_and_real_owner_recovery_finishes_it_pg() {
    input_free_journal_recovers(false).await;
}
#[tokio::test]
async fn positive_retry_timer_finishes_an_input_free_bound_journal_pg() {
    input_free_journal_recovers(true).await;
}
async fn input_free_journal_recovers(timer: bool) {
    let root_guard = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let channel = Channel::new(8077002);
    let logical = "i8077-boot-test";
    let key = format!("claude/{}/test-node:{logical}", shared.token_hash);
    let mut owner = crate::db::dispatched_sessions::hosted_execution::test_support::owner(
        &channel.get().to_string(),
    );
    owner.discord_token_hash = shared.token_hash.clone();
    owner.logical_key = logical.into();
    let record = crate::db::dispatched_sessions::hosted_execution::test_support::record(
        &owner,
        "boot-nonce",
        HostedState::Bound,
    );
    let row = seeded(
        &pool,
        &key,
        &shared.token_hash,
        channel.get(),
        Some(serde_json::to_value(record).unwrap()),
    )
    .await;
    let (done, completed) = tokio::sync::oneshot::channel();
    let forced = install_forced(channel.get(), key.clone(), row, false, Some(done));
    let mailbox = shared.mailbox(channel);
    let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, &shared.token_hash, None);
    let (path, namespace) =
        crate::services::turn_orchestrator::v2_queue(&persistence, channel).unwrap();
    mailbox
        .v2_adopt(path.clone(), namespace.clone())
        .await
        .unwrap();
    let key_copy = key.clone();
    let raw = mailbox
        .v2_request(move |slot| {
            let owner = slot.as_mut().unwrap();
            owner
                .enqueue_fixture(&original(channel.get(), 8_000_000_008_077_003))
                .unwrap();
            let front = owner.front().unwrap();
            owner.grant_front(&front)?;
            let journal = owner.begin_handoff(&front, logical.into(), key_copy)?;
            owner.handoff_ended(&journal.id)?;
            owner.json()
        })
        .await
        .unwrap();
    // A saved input-free journal still owes completion independently of an input head.
    let mut raw = raw;
    raw["items"] = serde_json::json!([]);
    if timer {
        raw["unfinished_handoff"]["retry_after_ms"] = serde_json::json!(now_ms() + 1_000);
    }
    let journal_id = raw["unfinished_handoff"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    std::fs::write(&path, serde_json::to_vec_pretty(&raw).unwrap()).unwrap();
    drop(mailbox);
    drop(shared);
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    assert!(
        crate::services::turn_orchestrator::v2_channels(&ProviderKind::Claude, &shared.token_hash)
            .contains(&channel)
    );
    if timer {
        shared
            .mailbox(channel)
            .v2_adopt(path, namespace)
            .await
            .unwrap();
        schedule(shared.clone(), channel, journal_id, 1);
    } else {
        crate::services::discord::router::message_handler::owner_recover(&shared, channel).await;
    }
    tokio::time::timeout(std::time::Duration::from_secs(10), completed)
        .await
        .unwrap()
        .unwrap();
    let state = shared
        .mailbox(channel)
        .v2_request(|slot| slot.as_ref().unwrap().json())
        .await
        .unwrap();
    assert!(state.get("unfinished_handoff").is_none());
    assert!(state["items"].as_array().unwrap().is_empty());
    assert_eq!(close_attempts(channel.get()), 0);
    drop(forced);
    pool.close().await;
    db.drop().await;
    drop(root_guard);
}

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
    let forced = install_forced(channel.get(), key, row, true, None);
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

#[tokio::test]
async fn exact_close_interleaves_with_idle_keep_without_closing_the_remaining_peer_pg() {
    use crate::services::platform::tmux::{SessionPresence, session_presence};
    use crate::services::tmux_turn_liveness::{
        kill_proven_idle_provider_session, provider_session_is_proven_idle,
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 1_479_671_301_387_059_302;
    let logical = "AgentDesk-claude-t1479671301387059302";
    let old = PrivateOldTmuxForTest::new_for_channel(logical, None, channel);
    old.start();
    assert!(old.exists() && old.peer_exists());
    let entered = tokio::sync::Barrier::new(2);
    let release = tokio::sync::Barrier::new(2);
    let checks = AtomicUsize::new(0);
    let close = pinned_tmux::end(logical, channel, None, |_| {
        let first = checks.fetch_add(1, Ordering::SeqCst) == 0;
        let (entered, release) = (&entered, &release);
        async move {
            if first {
                entered.wait().await;
                release.wait().await;
            }
            Ok(true)
        }
    });
    let keep = async {
        entered.wait().await;
        assert!(old.exists());
        assert_eq!(session_presence(logical), SessionPresence::Present);
        assert!(!provider_session_is_proven_idle(logical));
        assert!(
            kill_proven_idle_provider_session(logical, "private overlap test", || Err(None))
                .is_err()
        );
        release.wait().await;
    };
    let (closed, ()) = tokio::join!(close, keep);
    closed.unwrap();
    assert!(checks.load(Ordering::SeqCst) >= 2);
    assert!(!old.exists(), "the pinned old pane really ended");
    assert!(old.peer_exists());
    assert_eq!(old.closes(), 1);
    assert_eq!(session_presence(logical), SessionPresence::Present);
    assert!(!provider_session_is_proven_idle(logical));
    assert!(
        kill_proven_idle_provider_session(logical, "private closed-pane test", || Err(None))
            .is_err()
    );
    assert_eq!(old.closes(), 1);
    assert!(
        old.peer_exists(),
        "idle Keep must preserve the surviving peer"
    );
    old.assert_no_input();
}
