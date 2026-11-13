use super::*;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::discord::queue_io::cancel_backstop_test_support as counted;
use crate::services::discord::queue_io::input_owner::drive::claim_turn;
use crate::services::turn_orchestrator::QueuePersistenceContext;
use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[path = "handoff_retry_tests.rs"]
mod handoff_retry;

struct Scope(ChannelId);
impl Drop for Scope {
    fn drop(&mut self) {
        counted::set_owner_wake_for_tests(self.0, None);
        *FACTS.lock().unwrap() = None;
    }
}
async fn scenario(token_first: bool, handoff: bool) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Claude;
    let channel = ChannelId::new(match (token_first, handoff) {
        (true, false) => 7787951,
        (false, false) => 7787952,
        (true, true) => 7787961,
        (false, true) => 7787962,
    });
    let message = 7787001;
    let mailbox = shared.mailbox(channel);
    let queue = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join(&shared.token_hash)
        .join(format!("{channel}.json"));
    mailbox
        .v2_adopt(queue, shared.token_hash.clone())
        .await
        .unwrap();
    let item:PendingQueueItem=serde_json::from_value(json!({"author_id":7,"message_id":message,"channel_id":channel.get(),"queued_generation":1,"text":"exact original"})).unwrap();
    mailbox
        .v2_admit_ticket(item, "start-facts".into())
        .await
        .unwrap();
    let front = mailbox
        .v2_request(|slot| {
            let owner = slot.as_mut().ok_or("owner")?;
            let front = owner.front().ok_or("front")?;
            owner.grant_front(&front)?;
            Ok(front)
        })
        .await
        .unwrap();
    if handoff {
        let original = front.clone();
        mailbox
            .v2_request(move |slot| {
                let owner = slot.as_mut().unwrap();
                let journal = owner.begin_handoff(&original, "owned".into(), "key".into())?;
                owner.handoff_ended(&journal.id).map(drop)
            })
            .await
            .unwrap();
    }
    let claim = claim_turn(&mailbox, message, &front)
        .await
        .unwrap()
        .unwrap();
    let token = Arc::new(CancelToken::new());
    let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, &shared.token_hash, None);
    assert!(
        mailbox
            .try_start_owner_front((token.clone(), UserId::new(7)), front, persistence.clone())
            .await
            .started
    );
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        channel.get(),
        Some("fact-fixture".into()),
        7,
        message,
        900001,
        "original".into(),
        None,
        None,
        None,
        None,
        0,
    );
    row.turn_nonce = token.turn_nonce().map(str::to_string);
    let path = crate::services::discord::runtime_store::discord_inflight_root()
        .unwrap()
        .join("claude")
        .join(format!("{channel}.json"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, serde_json::to_vec(&row).unwrap()).unwrap();
    let watch = row_removed::watch(&ProviderKind::Claude, channel.get())
        .unwrap()
        .unwrap();
    let wakes = Arc::new(AtomicUsize::new(0));
    let count = wakes.clone();
    let (wake, mut woke) = tokio::sync::mpsc::unbounded_channel();
    counted::set_owner_wake_for_tests(
        channel,
        Some(Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
            let _ = wake.send(());
        })),
    );
    let (facts, mut observed) = tokio::sync::mpsc::unbounded_channel();
    *FACTS.lock().unwrap() = Some((channel.get(), facts));
    let _scope = Scope(channel);
    assert!(
        Failure::claimed(&mailbox, &claim)
            .commit(
                Kind::Opaque,
                "unreadable row".into(),
                Ok(None),
                Some(shared.clone()),
                channel,
                Some(token.clone())
            )
            .await
            .is_err(),
        "a wrapper without fresh row proof cannot count or wake"
    );
    assert_eq!(wakes.load(Ordering::SeqCst), 0);
    assert_eq!(
        Failure::claimed(&mailbox, &claim)
            .commit(
                Kind::Opaque,
                "failure".into(),
                Ok(Some(watch)),
                Some(shared.clone()),
                channel,
                Some(token.clone())
            )
            .await
            .unwrap(),
        Some(false)
    );
    let wrong = mailbox
        .finish_turn_if_matches_episode_started_before(
            MessageId::new(message),
            Some("other-birth".into()),
            Instant::now(),
            persistence.clone(),
        )
        .await;
    assert!(wrong.removed_token.is_none());
    assert!(Arc::ptr_eq(
        &mailbox.cancel_token().await.unwrap().unwrap(),
        &token
    ));
    if token_first {
        assert!(
            mailbox
                .finish_turn_if_matches_episode_started_before(
                    MessageId::new(message),
                    token.turn_nonce().map(str::to_string),
                    Instant::now(),
                    persistence.clone()
                )
                .await
                .removed_token
                .is_some()
        );
    } else {
        assert_eq!(
            crate::services::discord::policy_op::remove_row(&path),
            crate::services::discord::policy_op::RemoveOutcome::Removed
        );
    }
    let wanted = if token_first {
        "token_released"
    } else {
        "row_removed"
    };
    loop {
        let (nonce, fact) = tokio::time::timeout(Duration::from_secs(10), observed.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(Some(nonce.as_str()), token.turn_nonce());
        if fact == wanted {
            break;
        }
    }
    if handoff {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let deadline = mailbox
            .v2_request(|slot| Ok(slot.as_ref().unwrap().handoff().unwrap().retry_after_ms))
            .await
            .unwrap();
        assert!(deadline <= chrono::Utc::now().timestamp_millis().max(0) as u64);
    }
    assert!(
        mailbox
            .v2_request(|slot| Ok(slot.as_ref().ok_or("owner")?.front().is_none()))
            .await
            .unwrap(),
        "one fact never clears closing"
    );
    assert_eq!(wakes.load(Ordering::SeqCst), 0, "one fact never wakes");
    if token_first {
        assert!(path.exists());
        assert_eq!(
            crate::services::discord::policy_op::remove_row(&path),
            crate::services::discord::policy_op::RemoveOutcome::Removed
        );
    } else {
        assert!(mailbox.has_active_turn().await.unwrap());
        assert!(
            mailbox
                .finish_turn_if_matches_episode_started_before(
                    MessageId::new(message),
                    token.turn_nonce().map(str::to_string),
                    Instant::now(),
                    persistence
                )
                .await
                .removed_token
                .is_some()
        );
    }
    tokio::time::timeout(Duration::from_secs(10), woke.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
    assert_eq!(
        crate::services::discord::policy_op::remove_row(&path),
        crate::services::discord::policy_op::RemoveOutcome::Missing
    );
    assert!(
        mailbox
            .v2_request(|slot| Ok(slot.as_ref().ok_or("owner")?.front().is_some()))
            .await
            .unwrap()
    );
    assert_eq!(wakes.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn token_release_with_row_remaining_keeps_closing_and_wake_zero() {
    scenario(true, false).await;
}
#[tokio::test]
async fn row_removal_with_exact_token_active_keeps_closing_and_wake_zero() {
    scenario(false, false).await;
}
#[tokio::test]
async fn handoff_elapsed_deadline_waits_for_row_removal_after_token_release() {
    scenario(true, true).await;
}
#[tokio::test]
async fn handoff_elapsed_deadline_waits_for_token_release_after_row_removal() {
    scenario(false, true).await;
}
#[test]
fn only_typed_confirmed_refusals_enter_first_failure_allowlist() {
    use crate::db::dispatched_sessions::hosted_execution::HostedState;
    assert!(
        confirmed(&HerdrRefusal::ProviderUnsupported {
            provider: "provider".into()
        })
        .is_some()
    );
    assert!(
        confirmed(&HerdrRefusal::EndpointNotLocal {
            node: "node".into()
        })
        .is_some()
    );
    assert!(confirmed(&HerdrRefusal::ExecutorNotWired).is_some());
    assert!(
        confirmed(&HerdrRefusal::HostedHerdrUnconfigured {
            state: Some(HostedState::Bound)
        })
        .is_some()
    );
    assert!(
        confirmed(&HerdrRefusal::HostedHerdrUnconfigured {
            state: Some(HostedState::Pending)
        })
        .is_some()
    );
    assert!(confirmed(&HerdrRefusal::HostedHerdrUnconfigured { state: None }).is_none());
    assert!(
        confirmed(&HerdrRefusal::HostedRowUnreadable {
            detail: "ExecutorNotWired".into()
        })
        .is_none()
    );
    assert!(confirmed(&HerdrRefusal::OWriterNotReady).is_none());
}

#[tokio::test]
async fn start_failure_guard_preserves_effect_owned_error_settlement_before_completion() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let trace = handoff_retry::Trace::default();
    let writer = trace.clone();
    crate::logging::test_capture::pin_callsite_interest();
    let _tracing = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish(),
    );
    let shared = crate::services::discord::make_shared_data_for_tests();
    let mailbox = shared.mailbox(ChannelId::new(7787953));
    let queue = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join(&shared.token_hash)
        .join("7787953.json");
    mailbox
        .v2_adopt(queue, shared.token_hash.clone())
        .await
        .unwrap();
    let item: PendingQueueItem = serde_json::from_value(json!({
        "author_id":7,"message_id":7787003,"channel_id":7787953,
        "queued_generation":1,"text":"effect owned original"
    }))
    .unwrap();
    mailbox
        .v2_admit_ticket(item, "settled-before-completion".into())
        .await
        .unwrap();
    let front = mailbox
        .v2_request(|slot| {
            let owner = slot.as_mut().ok_or("owner")?;
            let front = owner.front().ok_or("front")?;
            owner.grant_front(&front)?;
            Ok(front)
        })
        .await
        .unwrap();
    let claim = claim_turn(&mailbox, 7787003, &front)
        .await
        .unwrap()
        .unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let failures = [
        Failure::claimed(&mailbox, &claim),
        Failure::claimed(&mailbox, &claim),
    ];
    let reply = failures[0].guard_reply(Arc::new(move |result| {
        tx.send(result.clone()).unwrap();
    }));
    let error = Err("NotSent(text cannot override reserve)".to_string());
    let before = reply.clone();
    let first = error.clone();
    tokio::task::spawn_blocking(move || before(&first))
        .await
        .unwrap();
    assert!(
        rx.try_recv().is_err(),
        "pre-input error waits for the failure commit"
    );
    mailbox.v2_request(move |slot| {
        let attempt = serde_json::from_value(json!({
            "nonce":"effect-owned","provider":"claude",
            "target":{"host":"tmux","owner":"fixture","root":"fixture","execution_nonce":"born"},
            "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}
        })).map_err(|e| e.to_string())?;
        let payload = serde_json::from_value(json!({
            "frame":"exact frame","digest":"digest","attribution":{},"followers":[]
        })).map_err(|e| e.to_string())?;
        slot.as_mut().ok_or("owner")?.reserve_claimed((claim.front(), claim.number()), attempt, payload).map(drop)
    }).await.unwrap();
    let expected = error.clone();
    tokio::task::spawn_blocking(move || reply(&error))
        .await
        .unwrap();
    assert_eq!(
        rx.try_recv()
            .expect("EffectOwned settles without an AI completion event"),
        expected
    );
    let mut decisions = Vec::new();
    for (failure, watch) in failures
        .into_iter()
        .zip([Ok(None), Err("read denied".into())])
    {
        decisions.push(
            failure
                .commit(
                    Kind::ExecutorNotWired,
                    "NotSent(typed text cannot override reserve)".into(),
                    watch,
                    None,
                    ChannelId::new(7787953),
                    Some(Arc::new(CancelToken::new())),
                )
                .await,
        );
    }
    assert_eq!(
        decisions,
        vec![Ok(None), Ok(None)],
        "reserve precedes missing, mismatched and unreadable wrapper proof"
    );
    let log = String::from_utf8(trace.0.lock().unwrap().clone()).unwrap();
    assert!(!log.contains("owner launch failed before reserve"), "{log}");
}
