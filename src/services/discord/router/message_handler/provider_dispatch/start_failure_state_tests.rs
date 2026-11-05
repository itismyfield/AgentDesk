use super::*;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::discord::queue_io::cancel_backstop_test_support as counted;
use crate::services::discord::queue_io::input_owner::drive::claim_turn;
use crate::services::turn_orchestrator::QueuePersistenceContext;
use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

struct Scope(ChannelId);
impl Drop for Scope {
    fn drop(&mut self) {
        counted::set_owner_wake_for_tests(self.0, None);
        *FACTS.lock().unwrap() = None;
    }
}
async fn scenario(token_first: bool) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().provider = ProviderKind::Claude;
    let channel = ChannelId::new(if token_first { 7787951 } else { 7787952 });
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
    assert_eq!(
        Failure::claimed(&mailbox, &claim)
            .commit(
                Kind::Opaque,
                "failure".into(),
                Some(watch),
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
    scenario(true).await;
}
#[tokio::test]
async fn row_removal_with_exact_token_active_keeps_closing_and_wake_zero() {
    scenario(false).await;
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
