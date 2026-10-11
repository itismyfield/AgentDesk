use super::*;
use crate::services::turn_orchestrator::{
    QueuePersistenceContext, load_channel_pending_queue_checked,
};

struct RecordingApi {
    strict: StrictApi,
    outcomes: Mutex<Vec<(u64, bool, bool)>>,
    first_fetch: Option<Arc<tokio::sync::Barrier>>,
}

impl RecordingApi {
    fn new(strict: StrictApi) -> Self {
        Self {
            strict,
            outcomes: Mutex::new(Vec::new()),
            first_fetch: None,
        }
    }

    fn accepted(&self, message_id: MessageId) -> usize {
        self.outcomes
            .lock()
            .unwrap()
            .iter()
            .filter(|(id, enqueued, failed)| *id == message_id.get() && *enqueued && !failed)
            .count()
    }
}

#[async_trait::async_trait]
impl CatchUpDiscordApi for RecordingApi {
    async fn current_user_id(&self) -> Result<Option<u64>, String> {
        self.strict.current_user_id().await
    }

    async fn resolve_runtime_channel_binding_status(
        &self,
        channel_id: ChannelId,
    ) -> RuntimeChannelBindingStatus {
        self.strict
            .resolve_runtime_channel_binding_status(channel_id)
            .await
    }

    async fn fetch_messages(
        &self,
        channel_id: ChannelId,
        request: CatchUpFetchRequest,
    ) -> Result<Vec<serenity::Message>, String> {
        if self.strict.fetch_log().is_empty()
            && let Some(barrier) = &self.first_fetch
        {
            barrier.wait().await;
        }
        self.strict.fetch_messages(channel_id, request).await
    }

    async fn cleanup_recovered_catch_up_hourglass(
        &self,
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
        message_id: MessageId,
    ) {
        self.strict
            .cleanup_recovered_catch_up_hourglass(shared, channel_id, message_id)
            .await;
    }

    async fn enqueue_intervention(
        &self,
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel_id: ChannelId,
        intervention: Intervention,
        observed: ClaimObservation,
    ) -> MailboxEnqueueOutcome {
        let message_id = intervention.message_id.get();
        let outcome = self
            .strict
            .enqueue_intervention(shared, provider, channel_id, intervention, observed)
            .await;
        self.outcomes.lock().unwrap().push((
            message_id,
            outcome.enqueued,
            outcome.persistence_error.is_some(),
        ));
        outcome
    }
}

fn api(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    checkpoint: MessageId,
    messages: &[MessageId],
    phase2: bool,
    isolate: bool,
) -> RecordingApi {
    let mut strict = StrictApi::new(shared);
    if phase2 {
        strict = strict.with_history(channel_id, vec![own_reply(channel_id, checkpoint)]);
    }
    for message_id in messages {
        strict = strict.arriving_at(
            usize::from(phase2 && isolate),
            human(channel_id, *message_id),
        );
    }
    RecordingApi::new(strict)
}

fn sources(items: &[Intervention]) -> Vec<MessageId> {
    items
        .iter()
        .flat_map(|item| item.source_message_ids.iter().copied())
        .collect()
}

async fn assert_exhausted_recovery(phase2: bool) {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(6_813_200 + u64::from(phase2));
    let (a, b, c, d) = (id(1, 600), id(2, 120), id(3, 90), id(4, 60));
    fx.seed_checkpoint(channel_id, a);
    let queue_file = discord::runtime_store::discord_pending_queue_root()
        .expect("queue root")
        .join(fx.provider.as_str())
        .join(&fx.shared.token_hash)
        .join(format!("{}.json", channel_id.get()));
    std::fs::create_dir_all(&queue_file).expect("directory blocks the durable queue file");
    let logs = LogWriter::capture();
    let mut messages = vec![b];
    let mut origin = None;
    for attempt in 1..=9 {
        if attempt == 8 {
            messages.push(c);
        } else if attempt == 9 {
            messages.push(d);
        }
        let scan = api(&fx.shared, channel_id, a, &messages, phase2, true);
        let pending = super::super::super::collect_catch_up_retry_pending_channels(&fx.shared);
        let deps = CatchUpDeps::new(&scan, &fx.shared, &fx.provider);
        run_catch_up_sweep(deps.with_pending_retry_channels(&pending)).await;
        assert_eq!(
            *scan.outcomes.lock().unwrap(),
            [(b.get(), false, true)],
            "attempt {attempt} must actually fail B's durable enqueue"
        );
        assert_eq!(fx.surfaces(channel_id), (Some(a.get()), Some(a.get())));
        assert!(
            discord::mailbox_snapshot(&fx.shared, channel_id)
                .await
                .intervention_queue
                .is_empty()
        );
        if attempt <= 8 {
            let pending = fx.pending(channel_id).expect("bounded retry remains armed");
            assert_eq!(pending.deferred_rearms, attempt);
            assert_eq!(pending.checkpoint, a.get());
            assert_eq!(pending.fetch_failures, 0);
            assert_eq!(pending.armed_at, *origin.get_or_insert(pending.armed_at));
        } else {
            assert_eq!(
                fx.pending(channel_id),
                None,
                "ninth defer exhausts eight rearms"
            );
        }
    }
    assert!(
        queue_file.is_dir(),
        "failed writes preserve the broken path"
    );
    std::fs::remove_dir(&queue_file).expect("repair the enqueue persistence failure");
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut first = api(&fx.shared, channel_id, a, &messages, phase2, false);
    let mut second = api(&fx.shared, channel_id, a, &messages, phase2, false);
    first.first_fetch = Some(Arc::clone(&barrier));
    second.first_fetch = Some(barrier);
    let rescan = super::super::super::catch_up_missed_messages_with_api;
    tokio::join!(
        rescan(&first, &fx.shared, &fx.provider),
        rescan(&second, &fx.shared, &fx.provider)
    );
    for scan in [&first, &second] {
        assert_phase1_read(&scan.strict, after(a), &[b, c, d]);
    }
    assert_eq!(first.accepted(b) + second.accepted(b), 1);
    let snapshot = discord::mailbox_snapshot(&fx.shared, channel_id).await;
    assert_eq!(sources(&snapshot.intervention_queue), messages);
    let read_disk = || {
        load_channel_pending_queue_checked(&fx.provider, &fx.shared.token_hash, channel_id)
            .expect("durable queue reader succeeds")
            .0
    };
    assert_eq!(sources(&read_disk()), messages);
    assert_eq!(
        sources(&read_disk()).iter().filter(|id| **id == b).count(),
        1
    );
    let extra = api(&fx.shared, channel_id, a, &messages, phase2, false);
    rescan(&extra, &fx.shared, &fx.provider).await;
    assert_eq!(extra.accepted(b), 0);
    assert_eq!(sources(&read_disk()), messages);

    // A fresh actor restores the durable sources, with no retry state and a plain old cursor.
    write_checkpoint(fx.root.path(), &fx.provider, channel_id, a.get());
    let restarted = discord::make_shared_data_for_tests();
    {
        let mut settings = restarted.settings.write().await;
        settings.owner_user_id = Some(OWNER_ID);
        settings.allow_all_users = true;
    }
    assert!(restarted.catch_up_retry_pending.is_empty());
    let persistence = QueuePersistenceContext::new(&fx.provider, &restarted.token_hash, None);
    let hydrated = restarted
        .mailbox(channel_id)
        .hydrate_pending_queue_from_disk(persistence)
        .await;
    assert!(hydrated.persistence_error.is_none());
    let restored = discord::mailbox_snapshot(&restarted, channel_id).await;
    assert_eq!(sources(&restored.intervention_queue), messages);
    let restart_scan = api(&restarted, channel_id, a, &messages, phase2, false);
    rescan(&restart_scan, &restarted, &fx.provider).await;
    assert_phase1_read(&restart_scan.strict, after(a), &[b, c, d]);
    assert_eq!(restart_scan.accepted(b), 0);
    assert_eq!(sources(&read_disk()), messages);

    let logs = logs.finish();
    let exhausted = logs
        .lines()
        .find(|line| line.contains("giving up after 8 re-arm(s)"))
        .expect("the actual exhausted path emits its WARN");
    assert!(
        exhausted.contains("repair the enqueue/persistence failure"),
        "{exhausted}"
    );
    assert!(
        exhausted.contains("rerun a full catch-up sweep"),
        "{exhausted}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn phase1_exhausted_persistence_retry_recovers_once_after_full_rescan() {
    assert_exhausted_recovery(false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn phase2_exhausted_persistence_retry_recovers_once_after_full_rescan() {
    assert_exhausted_recovery(true).await;
}
