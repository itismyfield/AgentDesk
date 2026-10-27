//! One watchdog tick over mailbox actors that are slow to answer or never do.

use std::sync::Arc;
use std::time::Duration;

use poise::serenity_prelude::UserId;

use super::o_stall_tests::{Captured, watcher};
use super::*;
use crate::services::provider::CancelToken;
use crate::services::turn_orchestrator::ChannelMailboxRegistry;

/// How long the test waits for a pass before calling it hung.
const HUNG: Duration = Duration::from_secs(120);

/// Runs `passes` watchdog passes; returns the WARN logs, then each pass's cleaned count and time.
async fn passes(
    registry: &HealthRegistry,
    provider: &ProviderKind,
    passes: usize,
) -> (String, Vec<(usize, Duration)>) {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .without_time()
        .with_writer(captured.clone())
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    let guard = tracing::subscriber::set_default(subscriber);
    let mut outcomes = Vec::new();
    for _ in 0..passes {
        let started = tokio::time::Instant::now();
        let cleaned = tokio::time::timeout(HUNG, run_stall_watchdog_pass(registry, provider))
            .await
            .expect("the pass ends instead of waiting on the actor");
        outcomes.push((cleaned, started.elapsed()));
    }
    drop(guard);
    let logs = String::from_utf8_lossy(&captured.0.lock().unwrap()).into_owned();
    (logs, outcomes)
}

/// The unreachable WARN lines that name `channel`.
fn unreachable_warns(logs: &str, channel: ChannelId) -> usize {
    let channel = format!("channel_id={}", channel.get());
    logs.lines()
        .filter(|line| line.contains("mailbox_unreachable") && line.contains(&channel))
        .count()
}

/// Every consumer of a tick reads one enumeration per runtime, and the next tick reads afresh.
#[tokio::test(flavor = "current_thread")]
async fn a_tick_enumerates_each_runtime_mailbox_set_once() {
    let _absence = watcher_respawn::lock_watcher_absence_for_test().await;
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let tempdir = tempfile::tempdir().expect("runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tempdir.path(),
    );
    let provider = ProviderKind::Claude;
    let registry = HealthRegistry::new();
    for _ in 0..2 {
        let shared = discord::make_shared_data_for_tests();
        registry
            .register(provider.as_str().to_string(), shared)
            .await;
    }
    let before = ChannelMailboxRegistry::enumerations_for_tests();
    run_stall_watchdog_pass(&registry, &provider).await;
    assert_eq!(
        ChannelMailboxRegistry::enumerations_for_tests() - before,
        2,
        "one per runtime"
    );
    run_stall_watchdog_pass(&registry, &provider).await;
    assert_eq!(
        ChannelMailboxRegistry::enumerations_for_tests() - before,
        4,
        "a tick never reuses the last one"
    );
}

/// An actor that answers the enumeration and then blocks costs the tick one deadline.
#[tokio::test(start_paused = true)]
async fn a_blocked_mailbox_actor_does_not_hold_the_tick_past_its_deadline() {
    let _absence = watcher_respawn::lock_watcher_absence_for_test().await;
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let tempdir = tempfile::tempdir().expect("runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tempdir.path(),
    );
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(7_333_701);
    let registry = HealthRegistry::new();
    let shared = discord::make_shared_data_for_tests();
    registry
        .register(provider.as_str().to_string(), shared.clone())
        .await;
    let user_msg = MessageId::new(channel.get() + 1);
    let token = Arc::new(CancelToken::new());
    assert!(
        discord::mailbox_try_start_turn(&shared, channel, token, UserId::new(7), user_msg).await
    );
    let output = tempdir.path().join("blocked.jsonl");
    std::fs::write(&output, "").expect("output fixture");
    let tmux = format!("AgentDesk-claude-7333-blocked-{}", std::process::id());
    shared
        .tmux_watchers
        .insert(channel, watcher(&tmux, &output));
    let _barrier = shared.mailboxes.install_health_barrier_for_test(channel);

    let (logs, outcomes) = passes(&registry, &provider, 1).await;
    let (cleaned, took) = outcomes[0];
    assert_eq!(cleaned, 0);
    assert!(
        took >= Duration::from_secs(10) && took < Duration::from_secs(20),
        "one 10 s deadline, not one per consumer: {took:?}"
    );
    assert_eq!(unreachable_warns(&logs, channel), 1, "{logs}");
}

/// An actor that never answers leaves its channel unknown: no consumer acts on it as idle,
/// and the WARN fires once for every consumer of both ticks.
#[tokio::test(start_paused = true)]
async fn an_unanswering_mailbox_is_unknown_not_idle() {
    let _absence = watcher_respawn::lock_watcher_absence_for_test().await;
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let tempdir = tempfile::tempdir().expect("runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tempdir.path(),
    );
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(7_333_711);
    let registry = HealthRegistry::new();
    let shared = discord::make_shared_data_for_tests();
    registry
        .register(provider.as_str().to_string(), shared.clone())
        .await;
    shared.mailboxes.insert_silent_for_test(channel);

    let (logs, outcomes) = passes(&registry, &provider, 2).await;
    shared.mailboxes.remove_fixture_for_test(channel);
    for (cleaned, took) in outcomes {
        assert_eq!(cleaned, 0);
        assert!(
            took >= Duration::from_secs(10) && took < Duration::from_secs(20),
            "one 10 s deadline per tick: {took:?}"
        );
    }
    assert_eq!(unreachable_warns(&logs, channel), 1, "{logs}");
}

/// A due watcher respawn retry of a channel whose actor did not answer this tick neither waits
/// on the actor nor spends the retry's budget, and the next answering tick retries it.
#[tokio::test(start_paused = true)]
async fn a_due_retry_skips_a_channel_whose_actor_did_not_answer() {
    let _absence = watcher_respawn::lock_watcher_absence_for_test().await;
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let tempdir = tempfile::tempdir().expect("runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tempdir.path(),
    );
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(7_333_721);
    let registry = HealthRegistry::new();
    let shared = discord::make_shared_data_for_tests();
    registry
        .register(provider.as_str().to_string(), shared.clone())
        .await;
    shared.mailboxes.insert_silent_for_test(channel);
    let due = chrono::Utc::now().timestamp();
    watcher_respawn::seed_due_absence_for_test(&provider, channel, due);

    let (logs, outcomes) = passes(&registry, &provider, 1).await;
    let (_, took) = outcomes[0];
    assert!(
        took >= Duration::from_secs(10) && took < Duration::from_secs(20),
        "the enumeration's deadline only: {took:?}"
    );
    assert_eq!(unreachable_warns(&logs, channel), 1, "{logs}");
    assert_eq!(
        watcher_respawn::retry_trace_for_test(&provider, channel),
        ([0, 0], Some((0, due))),
        "no read or reclaim, and the entry stays due with its budget"
    );

    shared.mailboxes.remove_fixture_for_test(channel);
    run_stall_watchdog_pass(&registry, &provider).await;
    let (counts, _) = watcher_respawn::retry_trace_for_test(&provider, channel);
    watcher_respawn::clear_watcher_absence(&provider, channel);
    assert_eq!(counts[0], 1, "an answering tick retries the channel");
}

/// An actor that answers the enumeration and then blocks costs a due retry one deadline and
/// none of its budget.
#[tokio::test(start_paused = true)]
async fn a_due_retry_of_an_actor_blocked_after_the_enumeration_ends_at_its_deadline() {
    let _absence = watcher_respawn::lock_watcher_absence_for_test().await;
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let tempdir = tempfile::tempdir().expect("runtime root");
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tempdir.path(),
    );
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(7_333_731);
    let registry = HealthRegistry::new();
    let shared = discord::make_shared_data_for_tests();
    registry
        .register(provider.as_str().to_string(), shared.clone())
        .await;
    shared.mailboxes.handle(channel);
    let _barrier = shared.mailboxes.install_health_barrier_for_test(channel);
    let due = chrono::Utc::now().timestamp();
    watcher_respawn::seed_due_absence_for_test(&provider, channel, due);

    let (logs, outcomes) = passes(&registry, &provider, 1).await;
    let (_, took) = outcomes[0];
    let trace = watcher_respawn::retry_trace_for_test(&provider, channel);
    watcher_respawn::clear_watcher_absence(&provider, channel);
    assert!(
        took >= Duration::from_secs(10) && took < Duration::from_secs(20),
        "one 10 s deadline: {took:?}"
    );
    assert_eq!(unreachable_warns(&logs, channel), 1, "{logs}");
    assert_eq!(
        trace,
        ([1, 0], Some((0, due))),
        "the read is cut at the deadline before any reclaim or charge"
    );
}
