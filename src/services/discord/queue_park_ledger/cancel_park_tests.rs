use std::io::{self, Write};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use tracing_subscriber::fmt::MakeWriter;

use super::{Origin, evaluate_provider};
use crate::services::discord::health::{self as recovery, HealthRegistry};
use crate::services::discord::relay_recovery::tests::isolated_agentdesk_root;
use crate::services::discord::{self, SharedData, inflight};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::{ChannelMailboxSnapshot, Intervention, InterventionMode};

#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl Write for LogCapture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("log capture").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for LogCapture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl LogCapture {
    fn install(&self) -> tracing::subscriber::DefaultGuard {
        crate::logging::test_capture::pin_callsite_interest();
        tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_ansi(false)
                .without_time()
                .with_writer(self.clone())
                .finish(),
        )
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("log capture").clone()).expect("utf8 logs")
    }

    fn errors(&self) -> usize {
        self.text()
            .lines()
            .filter(|line| line.contains("queue_park") && line.contains("ERROR"))
            .count()
    }

    fn outcome(&self, outcome: &str) -> usize {
        self.text()
            .lines()
            .filter(|line| line.contains("queue_park") && line.contains(outcome))
            .count()
    }
}

fn queued(id: u64, merge: bool) -> Intervention {
    Intervention {
        author_id: UserId::new(42),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: discord::runtime_store::process_generation(),
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("source {id}"),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: merge,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

async fn enqueue(shared: &Arc<SharedData>, channel: ChannelId, id: u64, merge: bool) {
    let result = discord::mailbox_enqueue_intervention(
        shared,
        &ProviderKind::Claude,
        channel,
        queued(id, merge),
    )
    .await;
    assert!(
        result.enqueued,
        "operational enqueue must retain source {id}"
    );
}

async fn held_fixture(channel: ChannelId) -> (HealthRegistry, Arc<SharedData>) {
    let registry = HealthRegistry::new();
    let shared = discord::make_shared_data_for_tests();
    registry.register("claude".into(), shared.clone()).await;
    let token = Arc::new(CancelToken::new());
    assert!(
        shared
            .mailbox(channel)
            .try_start_turn(token.clone(), UserId::new(42), MessageId::new(6_016_001))
            .await
    );
    token.cancelled.store(true, Ordering::Relaxed);
    let state = inflight::InflightTurnState::new(
        ProviderKind::Claude,
        channel.get(),
        None,
        42,
        6_016_002,
        6_016_001,
        "held turn".into(),
        None,
        None,
        None,
        None,
        0,
    );
    inflight::save_inflight_state(&state).expect("save held row");
    (registry, shared)
}

async fn snapshot(shared: &SharedData, channel: ChannelId) -> ChannelMailboxSnapshot {
    shared.mailbox(channel).snapshot().await
}

async fn evaluate(shared: &Arc<SharedData>, channel: ChannelId) {
    let snapshot = snapshot(shared, channel).await;
    shared
        .queue_park_ledger
        .evaluate(shared, &ProviderKind::Claude, channel, &snapshot);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn drain_registers_sources_before_first_evaluation_with_preserved_origin() {
    let _root = isolated_agentdesk_root();
    let channel = ChannelId::new(6_016_101);
    let (registry, shared) = held_fixture(channel).await;
    enqueue(&shared, channel, 6_016_102, false).await;
    let observed_at = tokio::time::Instant::now();
    let result = recovery::schedule_pending_queue_drain_after_cancel(
        &registry,
        "claude",
        channel,
        "queue-park-registration-test",
    )
    .await;
    assert!(result.scheduled);
    assert_eq!(result.queue_depth_after, Some(1));
    assert_eq!(
        shared.queue_park_ledger.evaluations.load(Ordering::SeqCst),
        0
    );
    let channels = shared
        .queue_park_ledger
        .channels
        .lock()
        .expect("park ledger");
    let sources = channels
        .get(&channel)
        .expect("drain registered before any evaluator");
    assert_eq!(sources.len(), 1);
    let source = sources.get(&6_016_102).expect("preserved source");
    assert!(matches!(source.origin, Origin::PostCancelPreserved));
    assert_eq!(source.first_seen, observed_at);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn periodic_discovery_records_cancelled_anchor_origin_without_drain() {
    let _root = isolated_agentdesk_root();
    let channel = ChannelId::new(6_016_111);
    let (_, shared) = held_fixture(channel).await;
    enqueue(&shared, channel, 6_016_112, false).await;
    assert!(shared.queue_park_ledger.channels.lock().unwrap().is_empty());
    evaluate(&shared, channel).await;
    let channels = shared.queue_park_ledger.channels.lock().unwrap();
    let source = &channels[&channel][&6_016_112];
    assert!(matches!(source.origin, Origin::CancelledAnchorObserved));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn cancel_preserved_source_parked_errors_once_at_600s() {
    let _root = isolated_agentdesk_root();
    let capture = LogCapture::default();
    let _capture = capture.install();
    let channel = ChannelId::new(6_016_121);
    let (registry, shared) = held_fixture(channel).await;
    enqueue(&shared, channel, 6_016_122, false).await;
    recovery::schedule_pending_queue_drain_after_cancel(
        &registry,
        "claude",
        channel,
        "queue-park-threshold-test",
    )
    .await;
    tokio::time::advance(Duration::from_secs(599)).await;
    evaluate(&shared, channel).await;
    assert_eq!(capture.errors(), 0, "599 seconds is below the threshold");
    tokio::time::advance(Duration::from_secs(1)).await;
    evaluate(&shared, channel).await;
    assert_eq!(
        capture.errors(),
        1,
        "the same capture must observe the 600-second event"
    );
    let current = snapshot(&shared, channel).await;
    let projected =
        shared
            .queue_park_ledger
            .project(&shared, &ProviderKind::Claude, channel, &current);
    assert_eq!(projected.oldest_tracked_secs, Some(600));
    assert_eq!(
        projected.reason.as_deref(),
        Some("cancelled_anchor_held:hold_inflight_present")
    );
    assert_eq!(projected.owner, Some("idle_queue_backstop"));
    assert_eq!(projected.tracked_source_count, 1);
    tokio::time::advance(Duration::from_secs(600)).await;
    evaluate(&shared, channel).await;
    assert_eq!(capture.errors(), 1, "a source escalates once");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn stall_watchdog_task_evaluates_parks_without_backstop_or_health_calls() {
    let _root = isolated_agentdesk_root();
    let capture = LogCapture::default();
    let _capture = capture.install();
    let channel = ChannelId::new(6_016_131);
    let (registry, shared) = held_fixture(channel).await;
    enqueue(&shared, channel, 6_016_132, false).await;
    let initial = snapshot(&shared, channel).await;
    let held_token = initial.cancel_token.clone().expect("held cancelled token");
    shared
        .queue_park_ledger
        .register(channel, &initial, Origin::PostCancelPreserved);
    assert!(!shared.restart.deferred_hook_channels.contains_key(&channel));
    let registry = Arc::new(registry);
    recovery::spawn_stall_watchdog(registry, ProviderKind::Claude);
    crate::services::agent_recovery::recovery_wakeup(&ProviderKind::Claude).notify_one();
    wait_for_evaluation(&shared, 0).await;
    assert_eq!(capture.errors(), 0);
    let completed = shared.queue_park_ledger.evaluations.load(Ordering::SeqCst);
    tokio::time::advance(Duration::from_secs(600)).await;
    crate::services::agent_recovery::recovery_wakeup(&ProviderKind::Claude).notify_one();
    wait_for_evaluation(&shared, completed).await;
    assert_eq!(
        capture.errors(),
        1,
        "watchdog alone must evaluate the threshold"
    );
    let after = snapshot(&shared, channel).await;
    assert!(Arc::ptr_eq(
        after
            .cancel_token
            .as_ref()
            .expect("watchdog keeps the held anchor"),
        &held_token,
    ));
    assert!(inflight::inflight_state_file_exists(
        &ProviderKind::Claude,
        channel.get(),
    ));
    assert!(!shared.restart.deferred_hook_channels.contains_key(&channel));
}

async fn wait_for_evaluation(shared: &SharedData, previous: usize) {
    for _ in 0..1_000 {
        if shared.queue_park_ledger.evaluations.load(Ordering::SeqCst) > previous {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("watchdog did not complete the queue park evaluation");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn merged_carrier_tracks_more_than_queue_capacity_and_keeps_original_age() {
    let _root = isolated_agentdesk_root();
    let capture = LogCapture::default();
    let _capture = capture.install();
    let channel = ChannelId::new(6_016_141);
    let (_, shared) = held_fixture(channel).await;
    enqueue(&shared, channel, 6_016_200, true).await;
    let original = snapshot(&shared, channel).await;
    shared
        .queue_park_ledger
        .register(channel, &original, Origin::PostCancelPreserved);
    let first_seen =
        shared.queue_park_ledger.channels.lock().unwrap()[&channel][&6_016_200].first_seen;
    tokio::time::advance(Duration::from_secs(599)).await;
    for id in 6_016_201..=6_016_234 {
        enqueue(&shared, channel, id, true).await;
    }
    let merged = snapshot(&shared, channel).await;
    assert_eq!(
        merged.intervention_queue.len(),
        1,
        "sources share one operational carrier"
    );
    assert_eq!(merged.intervention_queue[0].source_message_ids.len(), 35);
    assert_eq!(merged.intervention_queue[0].message_id.get(), 6_016_234);
    shared
        .queue_park_ledger
        .register(channel, &merged, Origin::PostCancelPreserved);
    {
        let channels = shared.queue_park_ledger.channels.lock().unwrap();
        assert_eq!(
            channels[&channel].len(),
            35,
            "queue capacity cannot cap tracked sources"
        );
        assert!(
            channels[&channel].contains_key(&6_016_234),
            "the source beyond 30 remains tracked"
        );
        assert_eq!(channels[&channel][&6_016_200].first_seen, first_seen);
    }
    tokio::time::advance(Duration::from_secs(1)).await;
    evaluate(&shared, channel).await;
    assert_eq!(
        capture.errors(),
        1,
        "only the original source has reached 600 seconds"
    );
    let projected =
        shared
            .queue_park_ledger
            .project(&shared, &ProviderKind::Claude, channel, &merged);
    assert_eq!(projected.tracked_source_count, 35);
    assert_eq!(projected.oldest_tracked_secs, Some(600));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn live_uncancelled_turn_never_escalates() {
    let _root = isolated_agentdesk_root();
    let capture = LogCapture::default();
    let _capture = capture.install();
    let channel = ChannelId::new(6_016_151);
    let (_, shared) = held_fixture(channel).await;
    enqueue(&shared, channel, 6_016_152, false).await;
    let live = snapshot(&shared, channel).await;
    live.cancel_token
        .as_ref()
        .unwrap()
        .cancelled
        .store(false, Ordering::Relaxed);
    shared
        .queue_park_ledger
        .register(channel, &live, Origin::PostCancelPreserved);
    tokio::time::advance(Duration::from_secs(1_200)).await;
    evaluate(&shared, channel).await;
    assert_eq!(capture.errors(), 0);
    let projected =
        shared
            .queue_park_ledger
            .project(&shared, &ProviderKind::Claude, channel, &live);
    assert_eq!(projected.reason.as_deref(), Some("live_turn_active"));
    live.cancel_token
        .as_ref()
        .unwrap()
        .cancelled
        .store(true, Ordering::Relaxed);
    evaluate(&shared, channel).await;
    assert_eq!(
        capture.errors(),
        1,
        "positive control proves this capture and source can escalate"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn disappearance_without_claim_or_exit_is_unknown_not_resumed() {
    let _root = isolated_agentdesk_root();
    let capture = LogCapture::default();
    let _capture = capture.install();
    let shared = discord::make_shared_data_for_tests();
    let channel = ChannelId::new(6_016_161);
    let tracked = ChannelMailboxSnapshot {
        intervention_queue: vec![queued(6_016_162, false)],
        ..Default::default()
    };
    shared
        .queue_park_ledger
        .register(channel, &tracked, Origin::PostCancelPreserved);
    shared
        .queue_park_ledger
        .evaluate(&shared, &ProviderKind::Claude, channel, &Default::default());
    assert_eq!(capture.outcome("unknown"), 1);
    assert_eq!(capture.outcome("resumed"), 0);
    assert!(
        shared
            .queue_park_ledger
            .channels
            .lock()
            .unwrap()
            .get(&channel)
            .is_none_or(|sources| sources.is_empty())
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn explicit_queue_cancel_is_explicitly_removed_not_resumed() {
    let _root = isolated_agentdesk_root();
    let capture = LogCapture::default();
    let _capture = capture.install();
    let shared = discord::make_shared_data_for_tests();
    let channel = ChannelId::new(6_016_171);
    // System-authored sources still need the observation hook before UI feedback filters them.
    let mut item = queued(6_016_172, false);
    item.author_id = UserId::new(1);
    let result =
        discord::mailbox_enqueue_intervention(&shared, &ProviderKind::Claude, channel, item).await;
    assert!(result.enqueued);
    let before = snapshot(&shared, channel).await;
    shared
        .queue_park_ledger
        .register(channel, &before, Origin::PostCancelPreserved);
    let removed = discord::queue_io::mailbox_cancel_queued_primary_message(
        &shared,
        &ProviderKind::Claude,
        channel,
        MessageId::new(6_016_172),
    )
    .await;
    assert!(
        removed.is_some(),
        "the operational explicit cancellation removed the source"
    );
    evaluate(&shared, channel).await;
    assert_eq!(capture.outcome("explicitly_removed"), 1);
    assert!(capture.text().contains("Cancelled"));
    assert_eq!(capture.outcome("resumed"), 0);
    assert_eq!(capture.outcome("unknown"), 0);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn claimed_merged_sources_resolve_resumed_once() {
    let _root = isolated_agentdesk_root();
    let capture = LogCapture::default();
    let _capture = capture.install();
    let shared = discord::make_shared_data_for_tests();
    let channel = ChannelId::new(6_016_181);
    enqueue(&shared, channel, 6_016_182, true).await;
    enqueue(&shared, channel, 6_016_183, true).await;
    let before = snapshot(&shared, channel).await;
    assert_eq!(before.intervention_queue.len(), 1);
    shared
        .queue_park_ledger
        .register(channel, &before, Origin::PostCancelPreserved);
    let taken = shared
        .mailbox(channel)
        .take_next_soft(discord::queue_persistence_context(
            &shared,
            &ProviderKind::Claude,
            channel,
        ))
        .await;
    let item = taken.intervention.expect("dequeued merged carrier");
    assert!(
        shared
            .mailbox(channel)
            .try_start_turn(
                Arc::new(CancelToken::new()),
                item.author_id,
                item.message_id,
            )
            .await
    );
    let claimed = snapshot(&shared, channel).await;
    assert_eq!(
        claimed.active_user_message_id,
        Some(MessageId::new(6_016_183))
    );
    assert!(
        claimed
            .active_absorbed_source_ids
            .contains(&MessageId::new(6_016_182))
    );
    shared
        .queue_park_ledger
        .evaluate(&shared, &ProviderKind::Claude, channel, &claimed);
    assert_eq!(
        capture.outcome("resumed"),
        2,
        "primary and absorbed source both reached a claim"
    );
    evaluate(&shared, channel).await;
    assert_eq!(
        capture.outcome("resumed"),
        2,
        "resolved sources are removed once"
    );
    assert_eq!(capture.outcome("unknown"), 0);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn selective_dequeue_of_later_item_keeps_head_waiting() {
    let _root = isolated_agentdesk_root();
    let capture = LogCapture::default();
    let _capture = capture.install();
    let shared = discord::make_shared_data_for_tests();
    let channel = ChannelId::new(6_016_191);
    enqueue(&shared, channel, 6_016_192, false).await;
    enqueue(&shared, channel, 6_016_193, false).await;
    let before = snapshot(&shared, channel).await;
    shared
        .queue_park_ledger
        .register(channel, &before, Origin::PostCancelPreserved);
    let taken = shared
        .mailbox(channel)
        .take_soft_matching(
            discord::queue_persistence_context(&shared, &ProviderKind::Claude, channel),
            Some(MessageId::new(6_016_193)),
        )
        .await;
    assert_eq!(
        taken
            .intervention
            .as_ref()
            .map(|item| item.message_id.get()),
        Some(6_016_193)
    );
    let pending = snapshot(&shared, channel).await;
    assert_eq!(pending.intervention_queue[0].message_id.get(), 6_016_192);
    assert!(
        pending
            .pending_user_dispatch_source_ids
            .contains(&MessageId::new(6_016_193))
    );
    shared
        .queue_park_ledger
        .evaluate(&shared, &ProviderKind::Claude, channel, &pending);
    assert_eq!(capture.outcome("resumed"), 0);
    assert_eq!(capture.outcome("unknown"), 0);
    assert_eq!(
        shared.queue_park_ledger.channels.lock().unwrap()[&channel].len(),
        2
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn dequeued_then_represerved_source_remains_waiting() {
    let _root = isolated_agentdesk_root();
    let capture = LogCapture::default();
    let _capture = capture.install();
    let shared = discord::make_shared_data_for_tests();
    let channel = ChannelId::new(6_016_251);
    enqueue(&shared, channel, 6_016_252, false).await;
    let before = snapshot(&shared, channel).await;
    shared
        .queue_park_ledger
        .register(channel, &before, Origin::PostCancelPreserved);
    let taken = shared
        .mailbox(channel)
        .take_next_soft(discord::queue_persistence_context(
            &shared,
            &ProviderKind::Claude,
            channel,
        ))
        .await;
    let restored = discord::mailbox_restore_dequeued_head(
        &shared,
        &ProviderKind::Claude,
        channel,
        taken.intervention.expect("dequeued source"),
        taken.dispatch_lease.expect("authorized restore lease"),
    )
    .await;
    assert!(restored.enqueued);
    let preserved = snapshot(&shared, channel).await;
    assert_eq!(preserved.intervention_queue[0].message_id.get(), 6_016_252);
    assert!(preserved.active_user_message_id.is_none());
    shared
        .queue_park_ledger
        .evaluate(&shared, &ProviderKind::Claude, channel, &preserved);
    assert_eq!(capture.outcome("resumed"), 0);
    assert_eq!(capture.outcome("unknown"), 0);
    assert_eq!(
        shared.queue_park_ledger.channels.lock().unwrap()[&channel].len(),
        1
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn unobserved_source_age_is_none_instead_of_zero() {
    let _root = isolated_agentdesk_root();
    let shared = discord::make_shared_data_for_tests();
    let channel = ChannelId::new(6_016_261);
    let projection = shared.queue_park_ledger.project(
        &shared,
        &ProviderKind::Claude,
        channel,
        &ChannelMailboxSnapshot::default(),
    );
    assert_eq!(projection.oldest_tracked_secs, None);
    assert_eq!(projection.tracked_source_count, 0);
    assert!(projection.tracked_source_ids.is_empty());
    assert!(!projection.ids_truncated);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn provider_evaluator_visits_all_registered_runtimes() {
    let _root = isolated_agentdesk_root();
    let capture = LogCapture::default();
    let _capture = capture.install();
    let registry = HealthRegistry::new();
    let channel = ChannelId::new(6_016_241);
    let first = discord::make_shared_data_for_tests();
    let (_, second) = held_fixture(channel).await;
    registry.register("claude".into(), first.clone()).await;
    registry.register("claude".into(), second.clone()).await;
    enqueue(&second, channel, 6_016_242, false).await;
    evaluate_provider(&registry, &ProviderKind::Claude).await;
    assert_eq!(
        second.queue_park_ledger.channels.lock().unwrap()[&channel].len(),
        1
    );
    tokio::time::advance(Duration::from_secs(600)).await;
    evaluate_provider(&registry, &ProviderKind::Claude).await;
    assert_eq!(
        capture.errors(),
        1,
        "later same-provider runtime is evaluated"
    );
    assert!(first.queue_park_ledger.channels.lock().unwrap().is_empty());
}
