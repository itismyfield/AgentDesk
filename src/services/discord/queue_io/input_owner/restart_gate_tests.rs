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
