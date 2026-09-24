//! Sweep-level settled-frontier barrier tests over a fetch fixture that honours
//! the Discord cursor contract, so a retry cursor is proven by what it rereads.

use std::collections::{HashMap, VecDeque};
use std::io::Write;

use super::super::CatchUpRetryState;
use super::*;
use crate::services::discord::{self as discord, MailboxEnqueueOutcome, SharedData};
use crate::services::turn_orchestrator::EnqueueRefusalReason;

#[derive(Clone, Copy)]
enum Hook {
    /// Another producer queues the id between the scan snapshot and the enqueue.
    PreQueue,
    Defer,
}

#[derive(Debug)]
struct FetchRecord {
    channel_id: ChannelId,
    request: CatchUpFetchRequest,
    returned: Vec<u64>,
    /// Published retry cursors at the moment this fetch ran.
    pending: HashMap<ChannelId, u64>,
}

type EnqueueRecord = (u64, bool, Option<EnqueueRefusalReason>);

struct StrictApi {
    shared: Arc<SharedData>,
    /// Channel history as `(message, first fetch call index that can see it)`.
    channels: HashMap<ChannelId, Vec<(serenity::Message, usize)>>,
    hooks: Mutex<HashMap<u64, VecDeque<Hook>>>,
    fetches: Mutex<Vec<FetchRecord>>,
    enqueues: Mutex<Vec<EnqueueRecord>>,
}

impl StrictApi {
    fn new(shared: &Arc<SharedData>) -> Self {
        Self {
            shared: Arc::clone(shared),
            channels: HashMap::new(),
            hooks: Mutex::new(HashMap::new()),
            fetches: Mutex::new(Vec::new()),
            enqueues: Mutex::new(Vec::new()),
        }
    }

    fn with_history(mut self, channel_id: ChannelId, messages: Vec<serenity::Message>) -> Self {
        let entries = self.channels.entry(channel_id).or_default();
        entries.extend(messages.into_iter().map(|message| (message, 0)));
        self
    }

    fn arriving_at(mut self, fetch_call: usize, message: serenity::Message) -> Self {
        let entries = self.channels.entry(message.channel_id).or_default();
        entries.push((message, fetch_call));
        self
    }

    fn with_hooks(self, message_id: MessageId, hooks: &[Hook]) -> Self {
        let queue = hooks.iter().copied().collect();
        self.hooks.lock().unwrap().insert(message_id.get(), queue);
        self
    }

    fn fetch_log(&self) -> Vec<(ChannelId, CatchUpFetchRequest, Vec<u64>)> {
        let fetches = self.fetches.lock().unwrap();
        let rows = fetches.iter();
        rows.map(|f| (f.channel_id, f.request, f.returned.clone()))
            .collect()
    }

    fn enqueue_log(&self) -> Vec<EnqueueRecord> {
        self.enqueues.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl CatchUpDiscordApi for StrictApi {
    async fn current_user_id(&self) -> Result<Option<u64>, String> {
        Ok(Some(CURRENT_BOT_ID))
    }

    async fn resolve_runtime_channel_binding_status(
        &self,
        _channel_id: ChannelId,
    ) -> RuntimeChannelBindingStatus {
        RuntimeChannelBindingStatus::Owned
    }

    async fn fetch_messages(
        &self,
        channel_id: ChannelId,
        request: CatchUpFetchRequest,
    ) -> Result<Vec<serenity::Message>, String> {
        let mut fetches = self.fetches.lock().unwrap();
        let call = fetches.len();
        let mut visible: Vec<serenity::Message> = (self.channels.get(&channel_id))
            .into_iter()
            .flatten()
            .filter(|(_, from)| call >= *from)
            .map(|(message, _)| message.clone())
            .collect();
        // Discord pages are newest-first; `after` selects the oldest ids past
        // the cursor, `before` and no cursor the newest ids below it.
        visible.sort_by_key(|message| std::cmp::Reverse(message.id.get()));
        // Serenity's `GetMessages::limit` clamps to 100.
        let limit = usize::from(request.limit.min(100));
        let page: Vec<serenity::Message> = match request.cursor {
            Some(CatchUpFetchCursor::After(after)) => {
                let mut past: Vec<_> = visible.into_iter().filter(|m| m.id.get() > after).collect();
                let skip = past.len().saturating_sub(limit);
                past.drain(..skip);
                past
            }
            Some(CatchUpFetchCursor::Before(before)) => (visible.into_iter())
                .filter(|m| m.id.get() < before)
                .take(limit)
                .collect(),
            None => visible.into_iter().take(limit).collect(),
        };
        let pending = (self.shared.catch_up_retry_pending.iter())
            .map(|entry| (*entry.key(), entry.value().checkpoint))
            .collect();
        fetches.push(FetchRecord {
            channel_id,
            request,
            returned: page.iter().map(|message| message.id.get()).collect(),
            pending,
        });
        Ok(page)
    }

    async fn cleanup_recovered_catch_up_hourglass(
        &self,
        _shared: &Arc<SharedData>,
        _channel_id: ChannelId,
        _message_id: MessageId,
    ) {
    }

    async fn enqueue_intervention(
        &self,
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel_id: ChannelId,
        intervention: Intervention,
    ) -> MailboxEnqueueOutcome {
        let message_id = intervention.message_id;
        let hook =
            (self.hooks.lock().unwrap().get_mut(&message_id.get())).and_then(VecDeque::pop_front);
        let outcome = match hook {
            Some(Hook::Defer) => MailboxEnqueueOutcome {
                enqueued: false,
                merged: false,
                refusal_reason: Some(EnqueueRefusalReason::ActorUnreachable),
                persistence_error: None,
            },
            Some(Hook::PreQueue) => {
                queue(shared, provider, channel_id, message_id).await;
                discord::mailbox_enqueue_intervention(shared, provider, channel_id, intervention)
                    .await
            }
            None => {
                discord::mailbox_enqueue_intervention(shared, provider, channel_id, intervention)
                    .await
            }
        };
        let record = (message_id.get(), outcome.enqueued, outcome.refusal_reason);
        self.enqueues.lock().unwrap().push(record);
        outcome
    }
}

struct Fixture {
    root: ScopedRuntimeRoot,
    shared: Arc<SharedData>,
    provider: ProviderKind,
}

impl Fixture {
    async fn new() -> Self {
        let root = scoped_runtime_root();
        let shared = discord::make_shared_data_for_tests();
        shared.settings.write().await.allow_all_users = true;
        Self {
            root,
            shared,
            provider: ProviderKind::Claude,
        }
    }

    /// Seeds the same checkpoint on disk and in memory, so both surfaces are
    /// asserted by exact value rather than by absence.
    fn seed_checkpoint(&self, channel_id: ChannelId, checkpoint: MessageId) {
        write_checkpoint(
            self.root.path(),
            &self.provider,
            channel_id,
            checkpoint.get(),
        );
        (self.shared.last_message_ids).insert(channel_id, checkpoint.get());
    }

    fn surfaces(&self, channel_id: ChannelId) -> (Option<u64>, Option<u64>) {
        let path = checkpoint_path(self.root.path(), &self.provider, channel_id);
        let disk = std::fs::read_to_string(path).ok();
        let memory = self.shared.last_message_ids.get(&channel_id).map(|id| *id);
        (
            memory,
            disk.map(|raw| raw.trim().parse().expect("numeric checkpoint")),
        )
    }

    fn pending(&self, channel_id: ChannelId) -> Option<CatchUpRetryState> {
        let entry = self.shared.catch_up_retry_pending.get(&channel_id);
        entry.map(|state| *state)
    }

    async fn sweep(&self, api: &StrictApi) {
        run_catch_up_sweep(CatchUpDeps::new(api, &self.shared, &self.provider)).await;
    }

    async fn retry_sweep(&self, api: &StrictApi, channel_id: ChannelId) {
        let pending = HashSet::from([channel_id]);
        let deps = CatchUpDeps::new(api, &self.shared, &self.provider);
        run_catch_up_sweep(deps.with_pending_retry_channels(&pending)).await;
    }

    async fn queue(&self, channel_id: ChannelId, message_id: MessageId) {
        queue(&self.shared, &self.provider, channel_id, message_id).await;
    }
}

async fn queue(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
) {
    let intervention = queued_intervention(message_id, 0);
    let outcome =
        discord::mailbox_enqueue_intervention(shared, provider, channel_id, intervention).await;
    assert!(super::super::catch_up_enqueue_accepted(&outcome));
}

fn id(sequence: u64, age_secs: u64) -> MessageId {
    message_id_with_age(sequence, Duration::from_secs(age_secs))
}

fn human(channel_id: ChannelId, message_id: MessageId) -> serenity::Message {
    let text = format!("request {}", message_id.get());
    discord_message(channel_id, message_id, HUMAN_ID, false, &text)
}

fn own_reply(channel_id: ChannelId, message_id: MessageId) -> serenity::Message {
    discord_message(channel_id, message_id, CURRENT_BOT_ID, true, "bot reply")
}

/// A bot outside `allowed_bot_ids`: a terminal `NotAllowed` classification.
fn foreign_bot(channel_id: ChannelId, message_id: MessageId) -> serenity::Message {
    discord_message(channel_id, message_id, INFO_BOT_ID, true, "status noise")
}

fn after(checkpoint: MessageId) -> CatchUpFetchRequest {
    CatchUpFetchRequest::new(50).after(checkpoint.get())
}

fn assert_phase1_read(api: &StrictApi, request: CatchUpFetchRequest, ids: &[MessageId]) {
    let log = api.fetch_log();
    let (_, first, returned) = log.first().expect("phase 1 fetched");
    assert_eq!(*first, request, "phase 1 request");
    for message_id in ids {
        assert!(
            returned.contains(&message_id.get()),
            "phase 1 saw {message_id}"
        );
    }
}

#[test]
fn f1_unsealed_frontier_matches_the_max_and_retry_formula() {
    let mut frontier = SettledFrontier::default();
    assert_eq!(frontier.retry_after(Some(40), 90), 40);
    assert_eq!(frontier.retry_after(None, 90), 89);
    for message_id in [50, 70, 60] {
        frontier.settle(message_id);
    }
    assert_eq!(frontier.newest(), Some(70));
    assert_eq!(frontier.retry_after(Some(40), 90), 70);
    assert_eq!(frontier.retained_barrier(Some(40), false), None);
}

#[test]
fn f2_a_later_settle_cannot_leap_the_first_sealed_message() {
    let mut frontier = SettledFrontier::default();
    frontier.settle(50);
    frontier.seal(100);
    frontier.settle(200);
    frontier.seal(150);
    assert_eq!(
        frontier.newest(),
        Some(50),
        "200 must not leap the barrier at 100"
    );
    assert_eq!(frontier.retry_after(None, 300), 50);
    let retained = frontier
        .retained_barrier(None, false)
        .expect("barrier kept");
    assert_eq!((retained.barrier, retained.retry_after), (100, 50));
    frontier.seal(80);
    assert_eq!(frontier.retained_barrier(None, false).unwrap().barrier, 80);

    let mut unsettled = SettledFrontier::default();
    unsettled.seal(100);
    unsettled.settle(200);
    assert_eq!(unsettled.newest(), None);
    assert_eq!(unsettled.retry_after(None, 300), 99);
}

#[test]
fn f3_a_durable_candidate_at_or_past_the_barrier_is_dropped_not_lowered() {
    use super::super::settled_frontier::safe_durable_candidate;
    assert_eq!(safe_durable_candidate(Some(200), Some(100)), None);
    assert_eq!(safe_durable_candidate(Some(100), Some(100)), None);
    assert_eq!(safe_durable_candidate(Some(99), Some(100)), Some(99));
    assert_eq!(safe_durable_candidate(Some(200), None), Some(200));
}

/// T1 + T8: a queue-only message moves neither checkpoint surface, and the
/// retry left behind keeps the consumed state's origin and budgets.
#[tokio::test(flavor = "current_thread")]
async fn t1_t8_queue_only_message_holds_both_surfaces_and_keeps_retry_origin() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_501);
    let (checkpoint, m) = (id(1, 600), id(2, 120));
    fx.seed_checkpoint(channel_id, checkpoint);
    fx.queue(channel_id, m).await;
    let armed_at = Instant::now() - Duration::from_secs(5);
    let consumed = CatchUpRetryState {
        checkpoint: checkpoint.get(),
        fetch_failures: 1,
        deferred_rearms: 3,
        armed_at,
    };
    fx.shared
        .catch_up_retry_pending
        .insert(channel_id, consumed);
    let api = StrictApi::new(&fx.shared).with_history(
        channel_id,
        vec![own_reply(channel_id, checkpoint), human(channel_id, m)],
    );

    fx.retry_sweep(&api, channel_id).await;

    assert_phase1_read(&api, after(checkpoint), &[m]);
    let expected = (Some(checkpoint.get()), Some(checkpoint.get()));
    assert_eq!(
        fx.surfaces(channel_id),
        expected,
        "memory and disk stay before M"
    );
    assert_eq!(
        fx.pending(channel_id),
        Some(consumed),
        "the open barrier re-arms with the consumed origin and budgets unchanged"
    );
}

/// T2 + T12: later terminal and recoverable messages are still classified and
/// recovered, but neither carries the checkpoint past M.
#[tokio::test(flavor = "current_thread")]
async fn t2_later_terminal_and_accepted_messages_do_not_leap_m() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_502);
    let (checkpoint, m, terminal, n) = (id(1, 900), id(2, 700), id(3, 200), id(4, 30));
    fx.seed_checkpoint(channel_id, checkpoint);
    fx.queue(channel_id, m).await;
    let api = StrictApi::new(&fx.shared).with_history(
        channel_id,
        vec![
            own_reply(channel_id, checkpoint),
            human(channel_id, m),
            foreign_bot(channel_id, terminal),
            human(channel_id, n),
        ],
    );

    let logs = LogWriter::capture();

    fx.sweep(&api).await;

    let logs = logs.finish();
    assert_phase1_read(&api, after(checkpoint), &[m, terminal, n]);
    assert_eq!(api.enqueue_log().first(), Some(&(n.get(), true, None)));
    let expected = (Some(checkpoint.get()), Some(checkpoint.get()));
    assert_eq!(fx.surfaces(channel_id), expected);
    assert!(
        logs.contains("CATCH-UP: total 1 message(s) recovered across channels"),
        "the phase-1 total counts the accepted N with no durable write: {logs}"
    );
}

/// T3: a live dispatch reservation covers its primary and absorbed source
/// ids, but a reservation is not a turn.
#[tokio::test(flavor = "current_thread")]
async fn t3_live_pending_dispatch_ids_are_not_dispatch_evidence() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_503);
    let (checkpoint, source, primary, terminal) = (id(1, 600), id(2, 200), id(3, 150), id(4, 60));
    fx.seed_checkpoint(channel_id, checkpoint);
    let mut head = queued_intervention(primary, 0);
    head.source_message_ids = vec![source, primary];
    let outcome =
        discord::mailbox_enqueue_intervention(&fx.shared, &fx.provider, channel_id, head).await;
    assert!(super::super::catch_up_enqueue_accepted(&outcome));
    let _taken =
        discord::mailbox_take_next_soft_intervention(&fx.shared, &fx.provider, channel_id).await;
    let snapshot = discord::mailbox_snapshot(&fx.shared, channel_id).await;
    assert!(snapshot.intervention_queue.is_empty());
    assert_eq!(snapshot.pending_user_dispatch, Some(primary));
    let api = StrictApi::new(&fx.shared).with_history(
        channel_id,
        vec![
            human(channel_id, source),
            human(channel_id, primary),
            foreign_bot(channel_id, terminal),
        ],
    );

    fx.sweep(&api).await;

    assert_phase1_read(&api, after(checkpoint), &[source, primary, terminal]);
    let expected = (Some(checkpoint.get()), Some(checkpoint.get()));
    assert_eq!(fx.surfaces(channel_id), expected);
}

/// T4: the id was queued after the phase-1 snapshot, so the real enqueue
/// refuses it as `SourceIdAlreadyQueued`; that commit arm seals too.
#[tokio::test(flavor = "current_thread")]
async fn t4_queued_race_refusal_seals_the_frontier() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_504);
    let (checkpoint, m, n) = (id(1, 600), id(2, 120), id(3, 30));
    fx.seed_checkpoint(channel_id, checkpoint);
    let api = StrictApi::new(&fx.shared)
        .with_history(channel_id, vec![human(channel_id, m), human(channel_id, n)])
        .with_hooks(m, &[Hook::PreQueue]);

    fx.sweep(&api).await;

    assert_phase1_read(&api, after(checkpoint), &[m, n]);
    let refused = (
        m.get(),
        false,
        Some(EnqueueRefusalReason::SourceIdAlreadyQueued),
    );
    assert_eq!(api.enqueue_log()[..2], [refused, (n.get(), true, None)]);
    let expected = (Some(checkpoint.get()), Some(checkpoint.get()));
    assert_eq!(fx.surfaces(channel_id), expected);
}

/// T5: Recent mode, no checkpoint, M seals, N defers: N's retry is published
/// before M (the end-of-sweep merge would hide it) so the next `After` returns M.
#[tokio::test(flavor = "current_thread")]
async fn t5_recent_mode_defer_retry_stays_before_m_and_rereads_it() {
    let fx = Fixture::new().await;
    let (channel_id, observer) = (ChannelId::new(4_603_505), ChannelId::new(4_603_515));
    write_role_map(fx.root.path(), &fx.provider, channel_id);
    write_checkpoint(fx.root.path(), &fx.provider, observer, id(1, 900).get());
    let (m, n) = (id(1, 120), id(2, 30));
    fx.queue(channel_id, m).await;
    let api = StrictApi::new(&fx.shared)
        .with_history(channel_id, vec![human(channel_id, m), human(channel_id, n)])
        .with_hooks(n, &[Hook::Defer]);

    fx.sweep(&api).await;

    assert_phase1_read(&api, CatchUpFetchRequest::new(50), &[m, n]);
    let published = {
        let fetches = api.fetches.lock().unwrap();
        let observed = fetches.iter().find(|fetch| fetch.channel_id == observer);
        observed
            .expect("observer scanned")
            .pending
            .get(&channel_id)
            .copied()
    };
    let published = published.expect("N's defer published a retry before the observer scan");
    assert!(
        published < m.get(),
        "published retry {published} must stay before M {m}"
    );
    assert_eq!(
        api.enqueue_log(),
        [(n.get(), false, Some(EnqueueRefusalReason::ActorUnreachable))]
    );
    let retry = fx.pending(channel_id).expect("deferred N arms a retry");
    assert!(
        retry.checkpoint < m.get(),
        "retry {} must stay before M {m}",
        retry.checkpoint
    );
    assert_eq!(fx.surfaces(channel_id), (None, None));

    let reread = StrictApi::new(&fx.shared)
        .with_history(channel_id, vec![human(channel_id, m), human(channel_id, n)]);
    fx.retry_sweep(&reread, channel_id).await;
    assert_phase1_read(&reread, after(MessageId::new(retry.checkpoint)), &[m]);
}

/// T6 + T12: N appears only on the phase-2 page and is accepted; the phase-2
/// durable write must not pass M, and the recovery total still counts N.
#[tokio::test(flavor = "current_thread")]
async fn t6_t12_phase2_accept_is_counted_but_not_persisted_past_m() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_506);
    let (checkpoint, m, n) = (id(1, 400), id(2, 120), id(3, 30));
    fx.seed_checkpoint(channel_id, checkpoint);
    fx.queue(channel_id, m).await;
    let api = StrictApi::new(&fx.shared)
        .with_history(
            channel_id,
            vec![own_reply(channel_id, checkpoint), human(channel_id, m)],
        )
        .arriving_at(1, human(channel_id, n));
    let logs = LogWriter::capture();

    fx.sweep(&api).await;

    let log = api.fetch_log();
    assert_eq!(log[0].2, [m.get()], "phase 1 did not see N");
    assert_eq!(log[1].1, CatchUpFetchRequest::new(20));
    assert!(log[1].2.contains(&n.get()), "phase 2 saw N");
    assert_eq!(api.enqueue_log(), [(n.get(), true, None)]);
    let expected = (Some(checkpoint.get()), Some(checkpoint.get()));
    assert_eq!(
        fx.surfaces(channel_id),
        expected,
        "phase 2 must not persist N past M"
    );
    let logs = logs.finish();
    assert!(
        logs.contains("CATCH-UP phase2: total 1 unanswered message(s) recovered"),
        "the recovery total counts the accepted N: {logs}"
    );
}

/// T7: phase 2's last-bot-reply fallback lands past an off-page M; the next
/// channel's fetch must see the clamped cursor published, not the fallback.
#[tokio::test(flavor = "current_thread")]
async fn t7_phase2_fallback_retry_is_clamped_before_it_is_published() {
    let fx = Fixture::new().await;
    let (x, y) = (ChannelId::new(4_603_507), ChannelId::new(4_603_508));
    let (checkpoint, m, reply, n) = (id(1, 900), id(2, 500), id(40, 200), id(41, 30));
    write_checkpoint(fx.root.path(), &fx.provider, x, checkpoint.get());
    write_checkpoint(fx.root.path(), &fx.provider, y, checkpoint.get());
    fx.queue(x, m).await;
    let mut history = vec![human(x, m), own_reply(x, reply)];
    history.extend((3..21).map(|seq| foreign_bot(x, id(seq, 480 - seq * 5))));
    // Fetch calls: phase 1 x (0), phase 1 y (1), phase 2 x (2), phase 2 y (3).
    let api = StrictApi::new(&fx.shared)
        .with_history(x, history)
        .arriving_at(2, human(x, n))
        .with_hooks(n, &[Hook::Defer]);

    fx.sweep(&api).await;

    let fetches = api.fetches.lock().unwrap();
    let (phase2_x, phase2_y) = (&fetches[2], &fetches[3]);
    assert_eq!((phase2_x.channel_id, phase2_y.channel_id), (x, y));
    assert!(!phase2_x.returned.contains(&m.get()) && phase2_x.returned.contains(&n.get()));
    let published = phase2_y.pending.get(&x).copied().expect("phase 2 armed x");
    assert!(
        published < m.get(),
        "published retry {published} must stay before M {m}"
    );
    drop(fetches);
    let deferred = (n.get(), false, Some(EnqueueRefusalReason::ActorUnreachable));
    assert_eq!(api.enqueue_log(), [deferred]);
    assert_eq!(
        fx.pending(x).map(|state| state.checkpoint),
        Some(checkpoint.get())
    );
}

/// T6b: a live checkpoint already past M is clamped for the phase-2 scan
/// only, so N is still recovered, and the persisted value is not lowered.
#[tokio::test(flavor = "current_thread")]
async fn t6b_phase2_scan_checkpoint_is_clamped_without_lowering_the_live_one() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_516);
    let (checkpoint, m, n, live) = (id(1, 400), id(2, 120), id(3, 30), id(4, 5));
    write_checkpoint(fx.root.path(), &fx.provider, channel_id, checkpoint.get());
    fx.shared.last_message_ids.insert(channel_id, live.get());
    let consumed = CatchUpRetryState::new(checkpoint.get());
    fx.shared
        .catch_up_retry_pending
        .insert(channel_id, consumed);
    fx.queue(channel_id, m).await;
    let api = StrictApi::new(&fx.shared)
        .with_history(
            channel_id,
            vec![own_reply(channel_id, checkpoint), human(channel_id, m)],
        )
        .arriving_at(1, human(channel_id, n));

    fx.retry_sweep(&api, channel_id).await;

    assert_phase1_read(&api, after(checkpoint), &[m]);
    assert_eq!(
        api.enqueue_log(),
        [(n.get(), true, None)],
        "phase 2 still scans N"
    );
    let expected = (Some(live.get()), Some(checkpoint.get()));
    assert_eq!(fx.surfaces(channel_id), expected, "neither surface moves");
}

#[derive(Clone, Copy, Debug)]
enum Resolution {
    /// Teardown/recovery clear: an unintended loss, so M is recovered again.
    LeftQueueUnprocessed,
    BecameActiveTurn,
    /// M is the newest primary of a merged head that also carries older H. The
    /// claim drops H's evidence, so H is re-offered, never leapt (#6205 dedups).
    MergedHeadClaimed,
    /// `/clear`: the user discarded M, so neither sweep may run it again.
    IntentionallyCleared,
    /// `/clear` while M sits in an orphaned dequeue→claim reservation.
    OrphanedReservationCleared,
}

/// T9: once M's membership resolves, the next sweep either re-recovers M or
/// settles it, runs nothing already run or cleared, and leaves no barrier.
async fn t9_case(channel_id: ChannelId, resolution: Resolution) {
    let fx = Fixture::new().await;
    let (checkpoint, h, m) = (id(1, 600), id(2, 150), id(3, 120));
    fx.seed_checkpoint(channel_id, checkpoint);
    let mut history = vec![own_reply(channel_id, checkpoint)];
    if let Resolution::MergedHeadClaimed = resolution {
        let mut head = queued_intervention(m, 0);
        head.source_message_ids = vec![h, m];
        let queued =
            discord::mailbox_enqueue_intervention(&fx.shared, &fx.provider, channel_id, head);
        assert!(super::super::catch_up_enqueue_accepted(&queued.await));
        history.push(human(channel_id, h));
    } else {
        fx.queue(channel_id, m).await;
    }
    history.push(human(channel_id, m));
    let first = StrictApi::new(&fx.shared).with_history(channel_id, history.clone());
    fx.sweep(&first).await;
    let held = fx.pending(channel_id).expect("barrier kept a retry");
    assert_eq!(held.checkpoint, checkpoint.get(), "{resolution:?}");

    let token = Arc::new(crate::services::provider::CancelToken::new());
    let owner = serenity::UserId::new(HUMAN_ID);
    match resolution {
        Resolution::LeftQueueUnprocessed => {
            discord::mailbox_clear_channel(&fx.shared, &fx.provider, channel_id).await;
        }
        Resolution::IntentionallyCleared => {
            let discard = super::super::retry_state::clear_channel_discarding_catch_up_backlog;
            discard(&fx.shared, &fx.provider, channel_id).await;
            assert_eq!(fx.pending(channel_id), None, "/clear drops the retry");
        }
        Resolution::OrphanedReservationCleared => {
            let taken =
                discord::mailbox_take_next_soft_intervention(&fx.shared, &fx.provider, channel_id);
            drop(taken.await.into_intervention().expect("head taken"));
            let orphaned =
                crate::services::turn_orchestrator::PENDING_USER_DISPATCH_LEASE_ORPHAN_AFTER;
            let mailbox = fx.shared.mailbox(channel_id);
            mailbox
                .age_inbound_waits_for_test(orphaned + Duration::from_secs(60))
                .await;
            let snapshot = discord::mailbox_snapshot(&fx.shared, channel_id).await;
            assert_eq!(
                snapshot.pending_user_dispatch,
                Some(m),
                "M is only reserved"
            );
            assert!(
                !discord::recovery_known_arms_and_ids(&snapshot)
                    .1
                    .contains(&m.get())
            );
            let discard = super::super::retry_state::clear_channel_discarding_catch_up_backlog;
            discard(&fx.shared, &fx.provider, channel_id).await;
        }
        Resolution::BecameActiveTurn => {
            let started =
                discord::mailbox_try_start_turn(&fx.shared, channel_id, token, owner, m).await;
            assert!(started, "{resolution:?}");
        }
        Resolution::MergedHeadClaimed => {
            let taken =
                discord::mailbox_take_next_soft_intervention(&fx.shared, &fx.provider, channel_id);
            let (_head, _, _lease) = taken.await.into_intervention().expect("head taken");
            let started =
                discord::mailbox_try_start_turn(&fx.shared, channel_id, token, owner, m).await;
            assert!(started, "{resolution:?}");
        }
    }
    let second = StrictApi::new(&fx.shared).with_history(channel_id, history);
    if let Resolution::IntentionallyCleared | Resolution::OrphanedReservationCleared = resolution {
        fx.sweep(&second).await;
        assert_phase1_read(&second, after(m), &[]);
    } else {
        fx.retry_sweep(&second, channel_id).await;
        assert_phase1_read(&second, after(checkpoint), &[m]);
    }
    let log = second.enqueue_log().into_iter();
    let rerun: Vec<u64> = log.filter(|(_, ok, _)| *ok).map(|(id, ..)| id).collect();
    let expected_rerun = match resolution {
        Resolution::LeftQueueUnprocessed => vec![m.get()],
        Resolution::MergedHeadClaimed => vec![h.get()],
        Resolution::BecameActiveTurn
        | Resolution::IntentionallyCleared
        | Resolution::OrphanedReservationCleared => Vec::new(),
    };
    assert_eq!(rerun, expected_rerun, "{resolution:?}");
    let expected = (Some(m.get()), Some(m.get()));
    assert_eq!(fx.surfaces(channel_id), expected, "{resolution:?}");
    assert_eq!(
        fx.pending(channel_id),
        None,
        "{resolution:?}: no residual barrier"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn t9_unprocessed_m_is_recovered_once_it_leaves_the_queue() {
    t9_case(ChannelId::new(4_603_509), Resolution::LeftQueueUnprocessed).await;
}

#[tokio::test(flavor = "current_thread")]
async fn t9_m_settles_once_a_turn_takes_it() {
    t9_case(ChannelId::new(4_603_510), Resolution::BecameActiveTurn).await;
}

#[tokio::test(flavor = "current_thread")]
async fn t9_absorbed_h_is_reoffered_not_leapt_once_its_merged_head_is_claimed() {
    t9_case(ChannelId::new(4_603_521), Resolution::MergedHeadClaimed).await;
}

#[tokio::test(flavor = "current_thread")]
async fn t9_m_discarded_by_clear_is_not_recovered() {
    t9_case(ChannelId::new(4_603_522), Resolution::IntentionallyCleared).await;
}

#[tokio::test(flavor = "current_thread")]
async fn t9_m_in_an_orphaned_reservation_discarded_by_clear_is_not_recovered() {
    t9_case(
        ChannelId::new(4_603_523),
        Resolution::OrphanedReservationCleared,
    )
    .await;
}

/// T9b: a synthetic headless active id is not a Discord cursor, so `/clear` lifts the
/// checkpoint to the newest real discarded id and a later real message is still recovered.
#[tokio::test(flavor = "current_thread")]
async fn t9b_clear_during_a_synthetic_active_turn_keeps_later_messages_visible() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_524);
    let (checkpoint, m, n) = (id(1, 600), id(2, 120), id(3, 30));
    fx.seed_checkpoint(channel_id, checkpoint);
    fx.queue(channel_id, m).await;
    let synthetic = MessageId::new(9_100_000_000_000_000_001);
    let token = Arc::new(crate::services::provider::CancelToken::new());
    let owner = serenity::UserId::new(HUMAN_ID);
    let started =
        discord::mailbox_try_start_turn(&fx.shared, channel_id, token, owner, synthetic).await;
    assert!(started, "headless turn holds the slot");

    let discard = super::super::retry_state::clear_channel_discarding_catch_up_backlog;
    discard(&fx.shared, &fx.provider, channel_id).await;

    let expected = (Some(m.get()), Some(m.get()));
    assert_eq!(
        fx.surfaces(channel_id),
        expected,
        "checkpoint stops at real M"
    );
    let history = vec![own_reply(channel_id, checkpoint), human(channel_id, m)];
    let api = StrictApi::new(&fx.shared)
        .with_history(channel_id, history)
        .arriving_at(0, human(channel_id, n));
    fx.sweep(&api).await;
    assert_phase1_read(&api, after(m), &[n]);
    assert_eq!(api.enqueue_log(), [(n.get(), true, None)]);
}

/// T10: without an earlier barrier, active-turn and terminal messages advance.
#[tokio::test(flavor = "current_thread")]
async fn t10_active_turn_and_terminal_messages_still_advance() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_511);
    let (checkpoint, active, terminal) = (id(1, 600), id(2, 200), id(3, 100));
    fx.seed_checkpoint(channel_id, checkpoint);
    let token = Arc::new(crate::services::provider::CancelToken::new());
    let owner = serenity::UserId::new(HUMAN_ID);
    assert!(discord::mailbox_try_start_turn(&fx.shared, channel_id, token, owner, active).await);
    let api = StrictApi::new(&fx.shared).with_history(
        channel_id,
        vec![human(channel_id, active), foreign_bot(channel_id, terminal)],
    );

    fx.sweep(&api).await;

    assert_phase1_read(&api, after(checkpoint), &[active, terminal]);
    assert_eq!(
        fx.surfaces(channel_id),
        (Some(terminal.get()), Some(terminal.get()))
    );
    assert_eq!(fx.pending(channel_id), None);
}

/// Fixture cursor contract: newest-first pages (`After` keeps the oldest ids past
/// the cursor), limit clamped to 100, late arrivals hidden from earlier fetches.
#[tokio::test(flavor = "current_thread")]
async fn strict_api_pages_follow_the_discord_cursor_contract() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_517);
    let ids: Vec<u64> = (1..=4).map(|seq| id(seq, 500 - seq * 100).get()).collect();
    let early = ids[..3]
        .iter()
        .map(|&m| human(channel_id, MessageId::new(m)));
    let api = StrictApi::new(&fx.shared)
        .with_history(channel_id, early.collect())
        .arriving_at(1, human(channel_id, MessageId::new(ids[3])));
    let requests = [
        CatchUpFetchRequest::new(2),
        CatchUpFetchRequest::new(2),
        CatchUpFetchRequest::new(2).after(ids[0]),
        CatchUpFetchRequest::new(2).before(ids[2]),
    ];
    for request in requests {
        let page = api.fetch_messages(channel_id, request).await;
        page.expect("scripted page");
    }

    let pages: Vec<_> = (api.fetch_log().into_iter())
        .map(|(_, request, returned)| (request, returned))
        .collect();
    let expected = [
        (requests[0], vec![ids[2], ids[1]]),
        (requests[1], vec![ids[3], ids[2]]),
        (requests[2], vec![ids[2], ids[1]]),
        (requests[3], vec![ids[1], ids[0]]),
    ];
    assert_eq!(pages, expected);

    let wide = ChannelId::new(4_603_520);
    let many = (1..=101).map(|seq| human(wide, id(seq, 200)));
    let api = StrictApi::new(&fx.shared).with_history(wide, many.collect());
    let page = api
        .fetch_messages(wide, CatchUpFetchRequest::new(101))
        .await;
    assert_eq!(page.expect("scripted page").len(), 100);
}

/// A deferred N publishes its retry at the checkpoint before the next channel's
/// fetch; the retry sweep's `After` reread recovers N and retires the retry.
#[tokio::test(flavor = "current_thread")]
async fn deferred_retry_is_published_at_the_checkpoint_and_rereads_n() {
    let fx = Fixture::new().await;
    let (channel_id, observer) = (ChannelId::new(4_603_518), ChannelId::new(4_603_519));
    let (checkpoint, n) = (id(1, 600), id(2, 30));
    fx.seed_checkpoint(channel_id, checkpoint);
    fx.seed_checkpoint(observer, checkpoint);
    let history = vec![own_reply(channel_id, checkpoint), human(channel_id, n)];
    let api = StrictApi::new(&fx.shared)
        .with_history(channel_id, history.clone())
        .with_hooks(n, &[Hook::Defer, Hook::Defer]);

    fx.sweep(&api).await;

    assert_phase1_read(&api, after(checkpoint), &[n]);
    let published = {
        let fetches = api.fetches.lock().unwrap();
        let observed = fetches.iter().find(|fetch| fetch.channel_id == observer);
        let observed = observed.expect("observer scanned");
        observed.pending.get(&channel_id).copied()
    };
    assert_eq!(
        published,
        Some(checkpoint.get()),
        "the retry is published before the observer scan"
    );
    let deferred = (n.get(), false, Some(EnqueueRefusalReason::ActorUnreachable));
    assert_eq!(api.enqueue_log(), [deferred, deferred], "both phases defer");
    let expected = (Some(checkpoint.get()), Some(checkpoint.get()));
    assert_eq!(fx.surfaces(channel_id), expected);
    let retry = fx.pending(channel_id).expect("deferred N arms a retry");
    assert_eq!(retry.checkpoint, checkpoint.get());

    let reread = StrictApi::new(&fx.shared).with_history(channel_id, history);
    let logs = LogWriter::capture();
    fx.retry_sweep(&reread, channel_id).await;
    let logs = logs.finish();

    assert_phase1_read(&reread, after(checkpoint), &[n]);
    assert_eq!(reread.enqueue_log(), [(n.get(), true, None)]);
    assert_eq!(fx.surfaces(channel_id), (Some(n.get()), Some(n.get())));
    assert_eq!(fx.pending(channel_id), None, "the reread retires the retry");
    assert!(
        logs.contains("CATCH-UP: total 1 message(s) recovered across channels"),
        "the retry sweep counts the recovered N: {logs}"
    );
}

/// T11: a Deferred budget exhausted this sweep stays exhausted; the barrier
/// arm must not hand the channel a fresh retry.
#[tokio::test(flavor = "current_thread")]
async fn t11_exhausted_budget_is_not_reset_by_the_barrier_arm() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_512);
    let (checkpoint, m, n) = (id(1, 600), id(2, 120), id(3, 30));
    fx.seed_checkpoint(channel_id, checkpoint);
    fx.queue(channel_id, m).await;
    let mut exhausted = CatchUpRetryState::new(checkpoint.get());
    exhausted.deferred_rearms = super::CATCH_UP_RETRY_DEFERRED_REARM_LIMIT;
    fx.shared
        .catch_up_retry_pending
        .insert(channel_id, exhausted);
    let history = vec![
        own_reply(channel_id, checkpoint),
        human(channel_id, m),
        human(channel_id, n),
    ];
    let api = StrictApi::new(&fx.shared)
        .with_history(channel_id, history)
        .with_hooks(n, &[Hook::Defer, Hook::Defer]);

    fx.retry_sweep(&api, channel_id).await;

    assert_phase1_read(&api, after(checkpoint), &[m, n]);
    let deferred = (n.get(), false, Some(EnqueueRefusalReason::ActorUnreachable));
    assert_eq!(
        api.enqueue_log(),
        [deferred, deferred],
        "both phases hit the budget"
    );
    assert_eq!(
        fx.pending(channel_id),
        None,
        "no fresh retry after exhaustion"
    );
    let expected = (Some(checkpoint.get()), Some(checkpoint.get()));
    assert_eq!(fx.surfaces(channel_id), expected);
}

/// T11b: with a settled predecessor A, a Deferred M that spends the last budget
/// advances to A and is logged as exhausted, never as a retained retry.
#[tokio::test(flavor = "current_thread")]
async fn t11b_exhausted_barrier_after_a_settled_predecessor_is_not_logged_as_retained() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_523);
    let (checkpoint, a, m) = (id(1, 600), id(2, 300), id(3, 120));
    fx.seed_checkpoint(channel_id, checkpoint);
    let mut last_budget = CatchUpRetryState::new(checkpoint.get());
    last_budget.deferred_rearms = super::CATCH_UP_RETRY_DEFERRED_REARM_LIMIT;
    (fx.shared.catch_up_retry_pending).insert(channel_id, last_budget);
    let history = vec![
        own_reply(channel_id, checkpoint),
        foreign_bot(channel_id, a),
        human(channel_id, m),
    ];
    let api = StrictApi::new(&fx.shared)
        .with_history(channel_id, history)
        .with_hooks(m, &[Hook::Defer, Hook::Defer]);

    let logs = LogWriter::capture();
    fx.retry_sweep(&api, channel_id).await;
    let logs = logs.finish();

    assert_phase1_read(&api, after(checkpoint), &[a, m]);
    assert_eq!(fx.surfaces(channel_id), (Some(a.get()), Some(a.get())));
    assert_eq!(
        fx.pending(channel_id),
        None,
        "the spent budget arms nothing"
    );
    let exhausted = format!("retry exhausted at barrier {m} for channel {channel_id}");
    assert!(logs.contains(&exhausted), "{logs}");
    assert!(!logs.contains("retry retained at barrier"), "{logs}");
}

struct LogWriter(Arc<Mutex<Vec<u8>>>);

struct CapturedLogs {
    buffer: Arc<Mutex<Vec<u8>>>,
    _guard: tracing::subscriber::DefaultGuard,
}

impl LogWriter {
    fn capture() -> CapturedLogs {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let writer = Arc::clone(&buffer);
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || LogWriter(Arc::clone(&writer)))
            .finish();
        crate::logging::test_capture::pin_callsite_interest();
        let guard = tracing::subscriber::set_default(subscriber);
        CapturedLogs {
            buffer,
            _guard: guard,
        }
    }
}

impl CapturedLogs {
    fn finish(self) -> String {
        String::from_utf8(self.buffer.lock().unwrap().clone()).expect("utf-8 logs")
    }
}

impl Write for LogWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
