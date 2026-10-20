//! The health snapshot against mailbox actors the scheduler has not reached: each such actor
//! lives on a runtime nothing drives until the test lets it, the shape of a busy dcserver.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use serde_json::Value;
use tokio::runtime::Runtime;
use tokio::sync::oneshot;

use crate::services::discord::health::{
    HealthRegistry, build_health_snapshot, build_public_health_snapshot,
};
use crate::services::discord::host_teardown_gate::test_support::nameless_turn;
use crate::services::discord::queue_park_ledger::Origin;
use crate::services::discord::relay_health::RelayStallState;
use crate::services::discord::{self, SharedData};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::ChannelMailboxRegistry;

const BUDGET: Duration = Duration::from_secs(2);

fn main_runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap()
}

/// A runtime whose tasks run only while something drives it.
pub(crate) fn parked_runtime() -> Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// Starts a turn on `channel` whose mailbox actor lives on `parked`.
pub(crate) fn parked_turn(parked: &Runtime, shared: &Arc<SharedData>, channel: ChannelId) {
    parked.block_on(nameless_turn(shared, channel));
}

/// Drives a parked runtime on its own thread from `after` until `work` ends, then drops it.
pub(crate) struct Driver {
    stop: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<Runtime>>,
}

impl Driver {
    pub(crate) fn start<F>(
        parked: Runtime,
        after: Duration,
        work: impl FnOnce(oneshot::Receiver<()>) -> F + Send + 'static,
    ) -> Self
    where
        F: Future<Output = ()>,
    {
        let (stop, stopped) = oneshot::channel();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(after);
            parked.block_on(work(stopped));
            parked
        });
        Self {
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    /// Actors on `parked` answer from `after` until the driver is dropped.
    pub(crate) fn answer_from(parked: Runtime, after: Duration) -> Self {
        Self::start(parked, after, |stopped| async move {
            let _ = stopped.await;
        })
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            drop(thread.join());
        }
    }
}

fn channel(base: u64, n: u64) -> ChannelId {
    ChannelId::new(base + n)
}

async fn provider(registry: &HealthRegistry, name: &str) -> Arc<SharedData> {
    let shared = discord::make_shared_data_for_tests();
    registry.register(name.to_string(), shared.clone()).await;
    shared
}

/// The detail snapshot and how long it took; a snapshot still running after 6s fails the test.
async fn detail(registry: &HealthRegistry) -> (Value, Duration) {
    let started = Instant::now();
    let snapshot = tokio::time::timeout(Duration::from_secs(6), build_health_snapshot(registry))
        .await
        .expect("the health snapshot must finish");
    (serde_json::to_value(snapshot).unwrap(), started.elapsed())
}

pub(crate) fn mailbox<'a>(
    json: &'a Value,
    provider: &str,
    channel: ChannelId,
) -> Option<&'a Value> {
    json["mailboxes"]
        .as_array()?
        .iter()
        .find(|entry| entry["provider"] == provider && entry["channel_id"] == channel.get())
}

/// `(provider, channel, read)` of every mailbox read the snapshot reports unanswered.
pub(crate) fn read_failures(json: &Value) -> Vec<(String, u64, String)> {
    json["mailbox_observation_failures"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|failure| {
            assert_eq!(failure["observation_failure"], "unreachable");
            (
                failure["provider"].as_str().unwrap().to_string(),
                failure["channel_id"].as_u64().unwrap(),
                failure["read"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn failed(provider: &str, channel: ChannelId, read: &str) -> (String, u64, String) {
    (provider.to_string(), channel.get(), read.to_string())
}

pub(crate) fn reasons(json: &Value) -> Vec<String> {
    json["degraded_reasons"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|reason| reason.as_str().map(str::to_string))
        .collect()
}

fn provider_entry<'a>(json: &'a Value, name: &str) -> &'a Value {
    json["providers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap()
}

/// Two stalled actors and three answering ones over two providers: the answering ones keep their
/// entries, the stalled ones are named and never read as idle, and the build ends in time.
#[test]
fn health_snapshot_parallel_actors_keep_other_channels() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let main = main_runtime();
    let parked = parked_runtime();
    let registry = HealthRegistry::new();
    let (claude, codex) = main.block_on(async {
        (
            provider(&registry, "claude").await,
            provider(&registry, "codex").await,
        )
    });
    let stalled = [channel(6_871_310_000, 1), channel(6_871_310_000, 2)];
    for stalled in stalled {
        parked_turn(&parked, &claude, stalled);
    }
    let answering = [
        (&claude, "claude", channel(6_871_310_000, 3)),
        (&claude, "claude", channel(6_871_310_000, 4)),
        (&codex, "codex", channel(6_871_310_000, 5)),
    ];
    main.block_on(async {
        for (shared, _, channel) in answering {
            nameless_turn(shared, channel).await;
        }
        let (json, elapsed) = detail(&registry).await;
        assert!(elapsed < BUDGET, "detail snapshot took {elapsed:?}");
        for (_, provider, channel) in answering {
            let entry =
                mailbox(&json, provider, channel).expect("an answering actor keeps its entry");
            assert_eq!(entry["has_cancel_token"], true);
        }
        for stalled in stalled {
            assert!(
                mailbox(&json, "claude", stalled).is_none(),
                "a stalled actor must not become an idle entry"
            );
        }
        // The stalled first reads hold the deadline, so the answering turns' reconfirms find
        // no time left: named as unanswered, never as a failed snapshot.
        let mut expected: Vec<_> = stalled
            .iter()
            .map(|stalled| failed("claude", *stalled, "snapshot"))
            .collect();
        for (_, provider, channel) in answering {
            expected.push(failed(provider, channel, "reconfirm"));
        }
        assert_eq!(read_failures(&json), expected);
        let claude = provider_entry(&json, "claude");
        assert_eq!(claude["runtime_state_complete"], false);
        assert_eq!(
            claude["active_turns"], 2,
            "only answered actors are counted"
        );
        assert_eq!(
            provider_entry(&json, "codex")["runtime_state_complete"],
            true
        );
        assert!(reasons(&json).contains(&"mailbox_snapshot_unavailable".to_string()));
        assert_ne!(json["status"], "healthy");
        assert_eq!(json["fully_recovered"], false);

        let started = Instant::now();
        let public = build_public_health_snapshot(&registry).await;
        assert!(
            started.elapsed() < BUDGET,
            "public snapshot took {:?}",
            started.elapsed()
        );
        let public = serde_json::to_value(public).unwrap();
        assert!(reasons(&public).contains(&"mailbox_snapshot_unavailable".to_string()));
    });
}

/// An actor that answers after 1.2s, inside the 1.5s deadline, is a live mailbox, not an
/// unreachable one.
#[test]
fn health_snapshot_slow_actor_within_deadline_is_ok() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let main = main_runtime();
    let parked = parked_runtime();
    let registry = HealthRegistry::new();
    let claude = main.block_on(provider(&registry, "claude"));
    let slow = channel(6_871_320_000, 1);
    parked_turn(&parked, &claude, slow);
    let _driver = Driver::answer_from(parked, Duration::from_millis(1_200));
    main.block_on(async {
        let (json, elapsed) = detail(&registry).await;
        assert!(elapsed < BUDGET, "detail snapshot took {elapsed:?}");
        assert!(
            elapsed >= Duration::from_millis(1_150),
            "the actor was not slow: {elapsed:?}"
        );
        let entry =
            mailbox(&json, "claude", slow).expect("a slow actor that answered keeps its entry");
        assert_eq!(entry["has_cancel_token"], true);
        assert_eq!(read_failures(&json), Vec::new());
        assert!(!reasons(&json).contains(&"mailbox_snapshot_unavailable".to_string()));
        assert_eq!(
            provider_entry(&json, "claude")["runtime_state_complete"],
            true
        );
    });
}

/// A build that starts 800ms after its request set the deadline still ends by that deadline: the
/// reconfirm that never answers is cut there and the other channels keep their entries.
#[test]
fn health_snapshot_by_late_start_keeps_the_request_deadline() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let main = main_runtime();
    let registry = HealthRegistry::new();
    let claude = main.block_on(provider(&registry, "claude"));
    let (silent, normal) = (channel(6_871_390_000, 1), channel(6_871_390_000, 2));
    let silent_runtime = parked_runtime();
    parked_turn(&silent_runtime, &claude, silent);
    let handle = claude.mailbox_peek(silent).unwrap();
    main.block_on(nameless_turn(&claude, normal));
    let started = Instant::now();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1_500);
    // Answers the first read and one more, then stalls before the reconfirm arrives.
    let _driver = Driver::start(
        silent_runtime,
        Duration::from_millis(1_100),
        |_| async move {
            let _ = handle.try_snapshot().await;
        },
    );
    main.block_on(async {
        tokio::time::sleep(Duration::from_millis(800)).await;
        let snapshot = registry.health_snapshot_by(true, deadline);
        let snapshot = tokio::time::timeout(Duration::from_secs(6), snapshot)
            .await
            .expect("the health snapshot must finish");
        let elapsed = started.elapsed();
        assert!(elapsed < BUDGET, "request took {elapsed:?}");
        let json = serde_json::to_value(snapshot).unwrap();
        assert_eq!(
            read_failures(&json),
            vec![failed("claude", silent, "reconfirm")]
        );
        let entry = mailbox(&json, "claude", silent).expect("the first read answered");
        assert_eq!(entry["has_cancel_token"], true);
        assert!(mailbox(&json, "claude", normal).is_some());
    });
}

/// First reads answer, then a paired thread's read and an unpaired-token reconfirm stall: both
/// end at the deadline, are named, prove nothing, and the other channels keep their entries.
#[test]
fn health_snapshot_actor_timeout_covers_secondary_reads() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let main = main_runtime();
    let registry = HealthRegistry::new();
    let claude = main.block_on(provider(&registry, "claude"));
    let base = 6_871_330_000;
    let (parent, thread, reconfirm, normal) = (
        channel(base, 1),
        channel(base, 2),
        channel(base, 3),
        channel(base, 4),
    );
    // Each actor answers its pending first read and one more, then stalls before the later
    // reads arrive; a stalled first read would leave no time for any later read.
    let mut drivers = Vec::new();
    for channel in [thread, reconfirm] {
        let runtime = parked_runtime();
        parked_turn(&runtime, &claude, channel);
        let handle = claude.mailbox_peek(channel).unwrap();
        drivers.push(Driver::start(
            runtime,
            Duration::from_millis(300),
            |_| async move {
                let _ = handle.try_snapshot().await;
            },
        ));
    }
    main.block_on(async {
        claude.mailboxes.handle(parent);
        claude.dispatch.thread_parents.insert(parent, thread);
        nameless_turn(&claude, normal).await;
        let (json, elapsed) = detail(&registry).await;
        assert!(elapsed < BUDGET, "detail snapshot took {elapsed:?}");
        let failures = read_failures(&json);
        assert!(
            failures.contains(&failed("claude", thread, "reconfirm")),
            "{failures:?}"
        );
        assert!(
            failures.contains(&failed("claude", parent, "thread_proof")),
            "{failures:?}"
        );
        assert!(
            failures.contains(&failed("claude", reconfirm, "reconfirm")),
            "{failures:?}"
        );
        assert_eq!(
            failures.len(),
            3,
            "the answering channel's reads all answered: {failures:?}"
        );
        let parent_entry = mailbox(&json, "claude", parent).expect("parent keeps its entry");
        assert_eq!(
            parent_entry["relay_health"]["thread_channel_id"],
            thread.get()
        );
        assert_eq!(
            parent_entry["relay_health"]["stale_thread_proof"], false,
            "an unanswered thread read must not mark the proof stale"
        );
        let entry = mailbox(&json, "claude", reconfirm).expect("first read answered");
        assert_eq!(entry["has_cancel_token"], true);
        assert!(mailbox(&json, "claude", normal).is_some());
        assert!(reasons(&json).contains(&"mailbox_snapshot_unavailable".to_string()));
    });
}

/// Stalled or missing tracked actors keep their park sources, and the read changes no actor, park
/// source or file.
#[test]
fn health_snapshot_timeout_preserves_park_and_episode() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let root = std::path::PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap());
    let main = main_runtime();
    let registry = HealthRegistry::new();
    let claude = main.block_on(provider(&registry, "claude"));
    let base = 6_871_340_000;
    let (parked_channel, missing, row_channel) =
        (channel(base, 1), channel(base, 2), channel(base, 5));
    let source = base + 90;
    let parked = parked_runtime();
    let queued = parked.block_on(async {
        nameless_turn(&claude, parked_channel).await;
        let intervention = ChannelMailboxRegistry::queued_for_test(source);
        let enqueued = discord::mailbox_enqueue_intervention(
            &claude,
            &ProviderKind::Claude,
            parked_channel,
            intervention,
        )
        .await;
        assert!(enqueued.enqueued);
        claude
            .mailbox_peek(parked_channel)
            .unwrap()
            .try_snapshot()
            .await
            .unwrap()
    });
    for tracked in [parked_channel, missing] {
        claude
            .queue_park_ledger
            .register(tracked, &queued, Origin::PostCancelPreserved);
    }
    let row = crate::services::discord::inflight::InflightTurnState::new(
        ProviderKind::Claude,
        row_channel.get(),
        None,
        7,
        row_channel.get() + 1,
        row_channel.get() + 2,
        "b2 row fixture".to_string(),
        None,
        None,
        None,
        None,
        0,
    );
    super::super::super::test_support::seed_backfill_row(&row);
    let files_before = super::super::super::test_support::tree_fingerprint(&root);
    let tracked = || {
        let mut channels = claude.queue_park_ledger.tracked_channels();
        channels.sort();
        channels
    };
    let tracked_before = tracked();
    let json = main.block_on(async {
        claude.mailboxes.handle(row_channel);
        let (json, elapsed) = detail(&registry).await;
        assert!(elapsed < BUDGET, "detail snapshot took {elapsed:?}");
        json
    });

    let park = json["queue_park_observation_failures"]
        .as_array()
        .expect("tracked failures");
    for (channel, failure) in [(parked_channel, "unreachable"), (missing, "missing")] {
        let entry = park
            .iter()
            .find(|entry| entry["channel_id"] == channel.get())
            .unwrap_or_else(|| panic!("{failure} tracked channel is named: {park:?}"));
        assert_eq!(entry["observation_failure"], failure);
        assert_eq!(
            entry["queue_park_tracked_source_ids"],
            serde_json::json!([source])
        );
    }
    assert_eq!(
        read_failures(&json),
        vec![failed("claude", parked_channel, "snapshot")]
    );
    assert_eq!(
        super::super::super::test_support::tree_fingerprint(&root),
        files_before
    );
    assert_eq!(tracked(), tracked_before);
    let after = parked.block_on(async {
        claude
            .mailbox_peek(parked_channel)
            .unwrap()
            .try_snapshot()
            .await
            .unwrap()
    });
    assert_eq!(after.active_user_message_id, queued.active_user_message_id);
    assert!(
        after.cancel_token.is_some(),
        "health must not cancel the stalled turn"
    );
    assert_eq!(
        after.intervention_queue.len(),
        1,
        "health must not drain the stalled queue"
    );
}

/// The reconfirm is a fresh read: an episode replaced after the first read is not confirmed, while
/// an unchanged one is.
#[test]
fn health_snapshot_reconfirm_does_not_confirm_a_replaced_episode() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let main = main_runtime();
    let registry = HealthRegistry::new();
    let claude = main.block_on(provider(&registry, "claude"));
    let (replaced, control) = (channel(6_871_340_000, 3), channel(6_871_340_000, 4));
    // Aged turns behind a watcher: a confirmed unpaired token classifies as `unpaired_active_token`.
    let replaced_runtime = parked_runtime();
    for (runtime, channel) in [(&replaced_runtime, replaced), (&main, control)] {
        runtime.block_on(async {
            nameless_turn(&claude, channel).await;
            let handle = claude.mailbox_peek(channel).unwrap();
            handle
                .age_active_turn_for_test(Duration::from_secs(700))
                .await;
        });
        discord::register_resume_watcher_for_tests(
            &claude,
            channel,
            &format!("b2-{}", channel.get()),
        );
    }
    let shared = claude.clone();
    let _replace = Driver::start(
        replaced_runtime,
        Duration::from_millis(300),
        move |stopped| async move {
            let handle = shared.mailbox_peek(replaced).unwrap();
            handle.hard_stop().await;
            let token = Arc::new(CancelToken::new());
            let user_message = MessageId::new(replaced.get() + 2);
            assert!(
                handle
                    .try_start_turn(token, UserId::new(8), user_message)
                    .await
            );
            let _ = stopped.await;
        },
    );
    let json = main.block_on(async {
        let (json, elapsed) = detail(&registry).await;
        assert!(elapsed < BUDGET, "detail snapshot took {elapsed:?}");
        json
    });
    assert_eq!(read_failures(&json), Vec::new());
    let unpaired = serde_json::to_value(RelayStallState::UnpairedActiveToken).unwrap();
    let state = |channel| mailbox(&json, "claude", channel).unwrap()["relay_stall_state"].clone();
    assert_eq!(
        state(control),
        unpaired,
        "the unchanged episode is confirmed"
    );
    assert_ne!(
        state(replaced),
        unpaired,
        "a replaced episode must not be confirmed"
    );
}
