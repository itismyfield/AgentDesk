use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::discord::queue_io::cancel_backstop_test_support as support;
use std::sync::atomic::{AtomicUsize, Ordering};

struct WakeHook(ChannelId);
impl Drop for WakeHook {
    fn drop(&mut self) {
        support::set_owner_wake_for_tests(self.0, None);
    }
}

#[tokio::test]
async fn restart_gate_holds_owner_until_remembered_result_and_drop() {
    let _root = TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = ChannelId::new(8_128_301);
    let operation = Operation::new(
        shared.provider.as_str(),
        &shared.token_hash,
        channel.get(),
        "message:1",
    )
    .unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let saved = count.clone();
    support::set_owner_wake_for_tests(
        channel,
        Some(Arc::new(move || {
            saved.fetch_add(1, Ordering::SeqCst);
        })),
    );
    let _hook = WakeHook(channel);
    let gate = Gate::begin(&shared, channel).unwrap();
    assert!(inflight(
        shared.provider.as_str(),
        &shared.token_hash,
        channel.get()
    ));
    assert!(Gate::begin(&shared, channel).is_err());
    assert!(
        held(&shared, channel).is_some(),
        "a refused second request can say how long the first has run"
    );
    let waiting = wait_completed(&operation);
    tokio::pin!(waiting);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
            waiting.as_mut(),
            cx
        )))
        .await
        .is_pending()
    );
    assert_eq!(count.load(Ordering::SeqCst), 0);
    remember(
        &operation,
        "confirmed predecessor end; greeting unavailable".into(),
    );
    drop(gate);
    assert_eq!(
        waiting.await.as_deref(),
        Some("confirmed predecessor end; greeting unavailable")
    );
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(!inflight(
        shared.provider.as_str(),
        &shared.token_hash,
        channel.get()
    ));
    remember(&operation, "cannot replace the completed response".into());
    assert_eq!(
        completed(&operation).as_deref(),
        Some("confirmed predecessor end; greeting unavailable")
    );
}

#[tokio::test]
async fn another_restart_operation_waits_for_release_without_inheriting_a_response() {
    let _root = TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = ChannelId::new(8_128_302);
    let operation = Operation::new(
        shared.provider.as_str(),
        &shared.token_hash,
        channel.get(),
        "message:2",
    )
    .unwrap();
    support::set_owner_wake_for_tests(channel, Some(Arc::new(|| {})));
    let _hook = WakeHook(channel);
    let gate = Gate::begin(&shared, channel).unwrap();
    let waiting = wait_completed(&operation);
    tokio::pin!(waiting);
    assert!(
        std::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(
            waiting.as_mut(),
            cx
        )))
        .await
        .is_pending()
    );
    assert!(!inflight(
        "another-provider",
        &shared.token_hash,
        channel.get()
    ));
    assert!(!inflight(
        shared.provider.as_str(),
        "another-token",
        channel.get()
    ));
    assert!(!inflight(
        shared.provider.as_str(),
        &shared.token_hash,
        channel.get() + 1
    ));
    drop(gate);
    assert!(waiting.await.is_none());
}

/// A waiter belongs to the gate it observed, not a later operation reusing its channel.
#[tokio::test]
async fn a_restart_waiter_finishes_when_its_observed_gate_is_replaced() {
    let _root = TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = ChannelId::new(8_128_331);
    let unrelated_channel = ChannelId::new(8_128_332);
    let operation = Operation::new(
        shared.provider.as_str(),
        &shared.token_hash,
        channel.get(),
        "message:wait-for-original-gate",
    )
    .unwrap();
    assert!(completed(&operation).is_none());
    support::set_owner_wake_for_tests(channel, Some(Arc::new(|| {})));
    let _hook = WakeHook(channel);
    support::set_owner_wake_for_tests(unrelated_channel, Some(Arc::new(|| {})));
    let _unrelated_hook = WakeHook(unrelated_channel);
    let gate_a = Gate::begin(&shared, channel).unwrap();
    let waiting = wait_completed(&operation);
    tokio::pin!(waiting);
    let first = std::future::poll_fn(|cx| {
        std::task::Poll::Ready(std::future::Future::poll(waiting.as_mut(), cx))
    })
    .await;
    assert!(first.is_pending(), "the original gate is active");

    // A failed begin cannot replace A's generation; another channel's drop only wakes a recheck.
    assert!(Gate::begin(&shared, channel).is_err());
    let unrelated = Gate::begin(&shared, unrelated_channel).unwrap();
    drop(unrelated);
    let same_generation = std::future::poll_fn(|cx| {
        std::task::Poll::Ready(std::future::Future::poll(waiting.as_mut(), cx))
    })
    .await;
    assert!(same_generation.is_pending(), "A still owns this wait");

    // B enters before the notified waiter polls: it must not inherit B's lifetime.
    drop(gate_a);
    let gate_b = Gate::begin(&shared, channel).unwrap();
    assert!(inflight(
        shared.provider.as_str(),
        &shared.token_hash,
        channel.get()
    ));
    let replaced = std::future::poll_fn(|cx| {
        std::task::Poll::Ready(std::future::Future::poll(waiting.as_mut(), cx))
    })
    .await;
    assert_eq!(
        replaced,
        std::task::Poll::Ready(None),
        "A ended without this operation's result; waiting for B can starve forever"
    );
    assert!(
        inflight(shared.provider.as_str(), &shared.token_hash, channel.get()),
        "the waiter completed while B remains active"
    );
    assert!(
        completed(&operation).is_none(),
        "no result was manufactured"
    );
    drop(gate_b);
}
