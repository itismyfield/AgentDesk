//! The witnessed release finishes only the exact episode and runs its withdrawal only
//! on that match, before the token leaves.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;

/// A withdrawal that counts its runs and reports `parents`.
struct Probe {
    runs: Arc<AtomicUsize>,
    parents: Vec<ChannelId>,
}

impl ReleaseWithdrawal for Probe {
    fn withdraw(self: Box<Self>) -> Vec<ChannelId> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        self.parents
    }
}

fn persistence() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, "witnessed-release", None)
}

fn release(
    anchor: MessageId,
    nonce: &str,
    started_before: Instant,
    runs: &Arc<AtomicUsize>,
) -> WitnessedEpisodeRelease {
    let probe = Probe {
        runs: runs.clone(),
        parents: vec![ChannelId::new(6_199_900)],
    };
    let probe = Box::new(probe);
    WitnessedEpisodeRelease::new(anchor, nonce.into(), started_before, probe, persistence())
        .expect("a named episode")
}

/// A turn on `channel` under `message`, with its token's own nonce.
async fn started(handle: &ChannelMailboxHandle, message: MessageId) -> (Arc<CancelToken>, String) {
    let token = Arc::new(CancelToken::new());
    let owner = UserId::new(7);
    assert!(handle.try_start_turn(token.clone(), owner, message).await);
    let nonce = token.turn_nonce().expect("a minted nonce").to_string();
    (token, nonce)
}

#[tokio::test]
async fn the_witnessed_episode_is_released_after_its_withdrawal() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let handle = ChannelMailboxRegistry::default().handle(ChannelId::new(6_199_901));
    let anchor = MessageId::new(6_199_902);
    let (token, nonce) = started(&handle, anchor).await;
    let runs = Arc::new(AtomicUsize::new(0));

    let result = handle
        .release_witnessed_episode(release(anchor, &nonce, Instant::now(), &runs))
        .await;

    let removed = result.finish.removed_token.expect("the witnessed token");
    assert!(Arc::ptr_eq(&removed, &token));
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(result.withdrawn, vec![ChannelId::new(6_199_900)]);
    assert!(handle.snapshot().await.cancel_token.is_none());
}

// B carries A's message but its own nonce and started before the cutoff: only the nonce tells
// it apart, so the release takes nothing and withdraws nothing.
#[tokio::test]
async fn a_same_message_episode_with_another_nonce_is_kept() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let handle = ChannelMailboxRegistry::default().handle(ChannelId::new(6_199_911));
    let anchor = MessageId::new(6_199_912);
    let (a, a_nonce) = started(&handle, anchor).await;
    assert!(
        handle
            .finish_turn(persistence())
            .await
            .removed_token
            .is_some()
    );
    let (b, b_nonce) = started(&handle, anchor).await;
    assert!(!Arc::ptr_eq(&a, &b) && a_nonce != b_nonce);
    let runs = Arc::new(AtomicUsize::new(0));

    let result = handle
        .release_witnessed_episode(release(anchor, &a_nonce, Instant::now(), &runs))
        .await;

    assert!(result.finish.removed_token.is_none());
    assert!(result.withdrawn.is_empty());
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    let held = handle.snapshot().await;
    assert!(
        held.cancel_token
            .as_ref()
            .is_some_and(|t| Arc::ptr_eq(t, &b))
    );
    assert_eq!(held.active_turn_nonce.as_deref(), Some(b_nonce.as_str()));
}

// A purge-closed actor drops the request unanswered: nothing released, nothing withdrawn.
#[tokio::test]
async fn a_closed_actor_runs_no_withdrawal() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let handle = ChannelMailboxRegistry::default().handle(ChannelId::new(6_199_921));
    let closed = handle.request(|reply| ChannelMailboxMsg::CloseIfIdle {
        expected_claim: None,
        reply,
    });
    assert_eq!(closed.await.ok(), Some(Ok(())));
    let runs = Arc::new(AtomicUsize::new(0));

    let anchor = MessageId::new(6_199_922);
    let result = handle
        .release_witnessed_episode(release(anchor, "nonce", Instant::now(), &runs))
        .await;

    assert!(!result.finish.mailbox_online && result.finish.removed_token.is_none());
    assert!(result.withdrawn.is_empty());
    assert_eq!(runs.load(Ordering::SeqCst), 0);
}

#[test]
fn an_empty_nonce_names_no_episode() {
    let probe = Box::new(Probe {
        runs: Arc::new(AtomicUsize::new(0)),
        parents: Vec::new(),
    });
    let anchor = MessageId::new(6_199_931);
    let release =
        WitnessedEpisodeRelease::new(anchor, String::new(), Instant::now(), probe, persistence());
    assert!(release.is_none());
}
