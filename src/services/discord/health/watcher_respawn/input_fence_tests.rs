//! The absence retry under the input fence: a closed gate refuses before the retry's first read
//! and keeps the entry's budget; an admitted retry holds the drain until it returns.

use super::super::claude_original_tests::respawn_gap;
use super::*;
use crate::services::discord::input_runtime::{self, fence::Gate};
use futures::FutureExt;

#[tokio::test]
async fn c2b_closed_gate_refuses_the_retry_before_any_read_and_keeps_its_budget() {
    let _absence = lock_watcher_absence_for_test().await;
    let provider = ProviderKind::Codex;
    let channel = ChannelId::new(6_325_955);
    seed_live_bridge_respawn_test(channel);
    let key = WatcherAbsenceKey::new(&provider, channel);
    {
        let mut state = WATCHER_ABSENCE.get_mut(&key).unwrap();
        state.failed_attempts = 2;
        state.next_attempt_unix_secs = 500;
    }
    let gate = Gate::protect(provider.clone(), channel.get()).unwrap();
    let _health = input_runtime::fence::test_health::Clear::new(&gate);
    let _closing = gate.close().unwrap();
    let registry = HealthRegistry::new();
    let runtimes = [discord::make_shared_data_for_tests()];

    let attempted =
        retry_pending_watcher_respawn(&registry, &provider, &runtimes, channel, 1_000).await;

    assert!(!attempted);
    assert_eq!(
        live_bridge_respawn_test_counts(channel),
        [0, 0, 2],
        "no snapshot, no reclaim, no charged attempt"
    );
    assert_eq!(
        WATCHER_ABSENCE.get(&key).unwrap().next_attempt_unix_secs,
        500
    );
    assert!(
        input_runtime::health_reasons()
            .iter()
            .any(|reason| reason.contains(&format!("channel={}", channel.get()))),
        "the refusal is a health reason"
    );
    clear_watcher_absence(&provider, channel);
}

#[tokio::test]
async fn c2b_admitted_retry_holds_the_input_drain_until_it_returns() {
    let _absence = lock_watcher_absence_for_test().await;
    let provider = ProviderKind::Codex;
    let channel = ChannelId::new(6_325_956);
    seed_live_bridge_respawn_test(channel);
    let gate = Gate::protect(provider.clone(), channel.get()).unwrap();
    let _health = input_runtime::fence::test_health::Clear::new(&gate);
    // An empty registry makes the respawn fail fast once the retry resumes.
    let registry = HealthRegistry::new();
    let runtimes = [discord::make_shared_data_for_tests()];
    let (reached, resume) = respawn_gap::arm(channel.get());

    let retry = retry_pending_watcher_respawn(&registry, &provider, &runtimes, channel, 1_000);
    tokio::pin!(retry);
    tokio::select! {
        _ = &mut retry => panic!("the retry must pause inside its admitted body"),
        _ = reached.notified() => {}
    }
    let closing = gate.close().unwrap();
    assert!(
        closing.drain().now_or_never().is_none(),
        "the admitted retry holds the drain"
    );
    resume.notify_one();
    assert!(retry.await, "the admitted retry attempted its respawn");
    assert!(
        closing.drain().now_or_never().is_some(),
        "returning releases the drain"
    );
    clear_watcher_absence(&provider, channel);
}
