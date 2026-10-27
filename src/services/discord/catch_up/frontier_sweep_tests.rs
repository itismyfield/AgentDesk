//! Sweep-level settled-frontier barrier tests over a fetch fixture that honours
//! the Discord cursor contract, so a retry cursor is proven by what it rereads.

use std::collections::{HashMap, VecDeque};
use std::io::Write;

use super::super::CatchUpRetryState;
use super::*;
use crate::services::discord::outbound::completed_turn_ledger;
use crate::services::discord::queue_io::mailbox_enqueue_observed_intervention as observed_enqueue;
use crate::services::discord::{self as discord, MailboxEnqueueOutcome, SharedData};
use crate::services::turn_orchestrator::{ClaimObservation, EnqueueRefusalReason};
#[path = "frontier_sweep_tests/dispatch_retirement_tests.rs"]
mod dispatch_retirement_tests;
#[path = "frontier_sweep_tests/scanner_progress_tests.rs"]
mod scanner_progress_tests;

#[derive(Clone, Copy)]
enum Hook {
    /// Another producer queues the id between the scan snapshot and the enqueue.
    PreQueue,
    Defer,
    /// #6035: the given primary's merged head absorbs the id and claims its
    /// turn between the scan snapshot and the enqueue.
    AbsorbInto(MessageId),
    /// Like `AbsorbInto`, then the turn ends (delivered or not)
    /// before the enqueue lands.
    AbsorbAndEnd {
        primary: MessageId,
        delivered: bool,
    },
    /// The channel's actor is purged first, so a fresh actor runs
    /// the undelivered `AbsorbAndEnd`.
    PurgeAbsorbAndEnd(MessageId),
    AbsorbAndDeliverEpisode(MessageId),
    /// The id's own text-intake turn is claimed between the scan snapshot and the enqueue.
    StartTurn,
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
    unowned: bool,
    fetch_failure: Option<(bool, String)>,
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
            unowned: false,
            fetch_failure: None,
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
        if self.unowned {
            RuntimeChannelBindingStatus::Unowned
        } else {
            RuntimeChannelBindingStatus::Owned
        }
    }

    async fn fetch_messages(
        &self,
        channel_id: ChannelId,
        request: CatchUpFetchRequest,
    ) -> Result<Vec<serenity::Message>, String> {
        if let Some((recent_only, error)) = &self.fetch_failure
            && (!recent_only || request.cursor.is_none())
        {
            return Err(error.clone());
        }
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
        observed: ClaimObservation,
    ) -> MailboxEnqueueOutcome {
        let message_id = intervention.message_id;
        let enqueue = |intervention| {
            let observed = Some(observed);
            observed_enqueue(shared, provider, channel_id, intervention, observed)
        };
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
                enqueue(intervention).await
            }
            Some(Hook::AbsorbInto(primary)) => {
                let absorbed = [message_id];
                absorbed_active_tests::absorb_and_claim(
                    shared, provider, channel_id, &absorbed, primary,
                )
                .await;
                enqueue(intervention).await
            }
            Some(Hook::AbsorbAndEnd { primary, delivered }) => {
                let end = claim_cas_tests::absorb_and_end;
                end(shared, provider, channel_id, message_id, primary, delivered).await;
                enqueue(intervention).await
            }
            Some(Hook::AbsorbAndDeliverEpisode(primary)) => {
                let nonce = absorbed_active_tests::absorb_and_claim(
                    shared,
                    provider,
                    channel_id,
                    &[message_id],
                    primary,
                )
                .await;
                completed_turn_ledger::append_completed_episode(
                    provider,
                    channel_id.get(),
                    primary.get(),
                    Some(&nonce),
                );
                discord::mailbox_finish_turn(shared, provider, channel_id).await;
                enqueue(intervention).await
            }
            Some(Hook::PurgeAbsorbAndEnd(primary)) => {
                claim_cas_tests::purge(shared, channel_id).await;
                let end = claim_cas_tests::absorb_and_end;
                end(shared, provider, channel_id, message_id, primary, false).await;
                enqueue(intervention).await
            }
            Some(Hook::StartTurn) => {
                let token = Arc::new(crate::services::provider::CancelToken::new());
                let owner = serenity::UserId::new(HUMAN_ID);
                // The Discord text-intake claim: it waits behind anything already queued.
                let start = discord::queue_io::mailbox_try_start_turn_behind_queue;
                let started = start(shared, channel_id, token, owner, message_id);
                assert!(started.await, "{message_id} claims its turn");
                enqueue(intervention).await
            }
            None => enqueue(intervention).await,
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
        {
            let mut settings = shared.settings.write().await;
            settings.owner_user_id = Some(OWNER_ID);
            settings.allow_all_users = true;
        }
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
    /// M is the newest primary of a merged head that also carries older H. H is
    /// held while M's turn runs, then settled by M's delivered episode.
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
    let turn_nonce = token.turn_nonce().map(str::to_owned);
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
    let mut second = StrictApi::new(&fx.shared).with_history(channel_id, history.clone());
    if let Resolution::IntentionallyCleared | Resolution::OrphanedReservationCleared = resolution {
        fx.sweep(&second).await;
        assert_phase1_read(&second, after(m), &[]);
    } else {
        fx.retry_sweep(&second, channel_id).await;
        if let Resolution::MergedHeadClaimed = resolution {
            // The absorbing turn is not evidence: H is neither offered nor leapt.
            assert_eq!(accepted(&second), Vec::<u64>::new(), "{resolution:?}");
            let held = (Some(checkpoint.get()), Some(checkpoint.get()));
            assert_eq!(fx.surfaces(channel_id), held, "{resolution:?}");
            let retry = fx.pending(channel_id).expect("H keeps the barrier retry");
            assert_eq!(retry.checkpoint, checkpoint.get(), "{resolution:?}");
            // M's episode is delivered, so its durable alias settles H.
            let (provider, nonce) = (&fx.provider, turn_nonce.as_deref());
            completed_turn_ledger::append_completed_episode(
                provider,
                channel_id.get(),
                m.get(),
                nonce,
            );
            discord::mailbox_finish_turn(&fx.shared, &fx.provider, channel_id).await;
            second = StrictApi::new(&fx.shared).with_history(channel_id, history);
            fx.retry_sweep(&second, channel_id).await;
        }
        assert_phase1_read(&second, after(checkpoint), &[m]);
    }
    let rerun = accepted(&second);
    let expected_rerun = match resolution {
        Resolution::LeftQueueUnprocessed => vec![m.get()],
        Resolution::MergedHeadClaimed
        | Resolution::BecameActiveTurn
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

fn accepted(api: &StrictApi) -> Vec<u64> {
    let log = api.enqueue_log().into_iter();
    log.filter(|(_, ok, _)| *ok).map(|(id, ..)| id).collect()
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

    assert_eq!(
        super::super::scan_progress::read(&fx.provider, channel_id),
        Some(m.get())
    );
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

/// A `/clear` whose writes fail twice still discarded queued M with the released
/// turn, so it drops the retry and lifts the checkpoint past both: no sweep reruns them.
#[tokio::test(flavor = "current_thread")]
async fn t9c_clear_that_fails_to_persist_drops_the_retry_and_advances_the_checkpoint() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(4_603_525);
    let (checkpoint, m, active) = (id(1, 600), id(2, 120), id(3, 60));
    fx.seed_checkpoint(channel_id, checkpoint);
    fx.queue(channel_id, m).await;
    let token = Arc::new(crate::services::provider::CancelToken::new());
    let owner = serenity::UserId::new(HUMAN_ID);
    assert!(discord::mailbox_try_start_turn(&fx.shared, channel_id, token, owner, active).await);
    let retry = CatchUpRetryState::new(checkpoint.get());
    fx.shared.catch_up_retry_pending.insert(channel_id, retry);
    // A directory in the queue file's place fails both the removal and the publish, twice.
    let queue_file = crate::services::discord::runtime_store::discord_pending_queue_root()
        .expect("queue root")
        .join(fx.provider.as_str())
        .join(&fx.shared.token_hash)
        .join(format!("{}.json", channel_id.get()));
    std::fs::remove_file(&queue_file).unwrap();
    std::fs::create_dir(&queue_file).unwrap();

    let discard = super::super::retry_state::clear_channel_discarding_catch_up_backlog;
    let cleared = discard(&fx.shared, &fx.provider, channel_id).await;

    assert_eq!(
        super::super::scan_progress::read(&fx.provider, channel_id),
        Some(m.get().max(active.get()))
    );
    assert_eq!(
        cleared.persistence.failed,
        ["queue_unlink", "queue_publish"]
    );
    let newest = m.get().max(active.get());
    let lifted = (Some(newest), Some(newest));
    assert_eq!(
        fx.surfaces(channel_id),
        lifted,
        "the checkpoint passes M and the turn"
    );
    assert_eq!(fx.pending(channel_id), None, "the retry is dropped");
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
    let hold = fx
        .pending(channel_id)
        .expect("exhausted origin remains held");
    assert!(hold.is_exhausted_hold());
    assert_eq!(hold.checkpoint, checkpoint.get());
    assert!(super::collect_catch_up_retry_pending_channels(&fx.shared).is_empty());
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
    let hold = fx
        .pending(channel_id)
        .expect("exhausted origin remains held");
    assert!(hold.is_exhausted_hold());
    assert_eq!(hold.checkpoint, a.get());
    assert_eq!(hold.armed_at, last_budget.armed_at);
    assert!(super::collect_catch_up_retry_pending_channels(&fx.shared).is_empty());
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

#[path = "absorbed_active_tests.rs"]
mod absorbed_active_tests;
#[path = "claim_cas_tests.rs"]
mod claim_cas_tests;
#[path = "duplicate_race_tests.rs"]
mod duplicate_race_tests;

#[path = "merged_alias_tests.rs"]
mod merged_alias_tests;

#[path = "phase2_age_tests.rs"]
mod phase2_age_tests;

pub(super) mod exhausted_recovery_tests {
    use super::super::super::clock::CatchUpClock;
    use super::super::too_old_drop_pg_tests::{StoredDeadLetter, stored_dead_letters};
    use super::*;
    use crate::services::turn_orchestrator::{
        QueuePersistenceContext, test_support::load_channel_pending_queue_checked,
    };

    struct RecoveryClock(Mutex<(chrono::DateTime<chrono::Utc>, Instant)>);

    impl RecoveryClock {
        fn new() -> Self {
            Self(Mutex::new((chrono::Utc::now(), Instant::now())))
        }

        fn advance(&self, secs: u64) {
            let mut pair = self.0.lock().unwrap();
            pair.0 += chrono::Duration::seconds(i64::try_from(secs).unwrap());
            pair.1 += Duration::from_secs(secs);
        }

        fn message_id(&self, sequence: u64, age_secs: i64) -> MessageId {
            let wall = self.sample().0 - chrono::Duration::seconds(age_secs);
            let discord_ms = u64::try_from(wall.timestamp_millis() - 1_420_070_400_000).unwrap();
            MessageId::new((discord_ms << 22) | sequence)
        }
    }

    impl CatchUpClock for RecoveryClock {
        fn sample(&self) -> (chrono::DateTime<chrono::Utc>, Instant) {
            *self.0.lock().unwrap()
        }
    }

    struct RecordingApi {
        strict: StrictApi,
        outcomes: Mutex<Vec<(u64, bool, bool)>>,
        dead_letters: Mutex<Vec<crate::db::relay_dead_letter::RelayDeadLetterRecord>>,
        entry_barrier: Option<Arc<tokio::sync::Barrier>>,
        blocked_fetch: Option<(usize, Arc<tokio::sync::Notify>)>,
        resumable_fetch: Option<(usize, Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
        utility_ids: Option<(UtilityBotUserIdResolution, UtilityBotUserIdResolution)>,
        first_binding_unknown: std::sync::atomic::AtomicBool,
        fail_after_cursor: Option<u64>,
        failed_fetches: Mutex<Vec<CatchUpFetchRequest>>,
    }

    impl RecordingApi {
        fn new(strict: StrictApi) -> Self {
            Self {
                strict,
                outcomes: Mutex::new(Vec::new()),
                dead_letters: Mutex::new(Vec::new()),
                entry_barrier: None,
                blocked_fetch: None,
                resumable_fetch: None,
                utility_ids: None,
                first_binding_unknown: std::sync::atomic::AtomicBool::new(false),
                fail_after_cursor: None,
                failed_fetches: Mutex::new(Vec::new()),
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
            if let Some(barrier) = &self.entry_barrier {
                barrier.wait().await;
            }
            self.strict.current_user_id().await
        }

        async fn resolve_runtime_channel_binding_status(
            &self,
            channel_id: ChannelId,
        ) -> RuntimeChannelBindingStatus {
            if self
                .first_binding_unknown
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return RuntimeChannelBindingStatus::Unknown;
            }
            self.strict
                .resolve_runtime_channel_binding_status(channel_id)
                .await
        }

        async fn utility_bot_user_ids(
            &self,
            shared: &SharedData,
        ) -> (UtilityBotUserIdResolution, UtilityBotUserIdResolution) {
            if let Some(ids) = self.utility_ids {
                ids
            } else {
                self.strict.utility_bot_user_ids(shared).await
            }
        }

        async fn fetch_messages(
            &self,
            channel_id: ChannelId,
            request: CatchUpFetchRequest,
        ) -> Result<Vec<serenity::Message>, String> {
            if self
                .fail_after_cursor
                .is_some_and(|cursor| request.cursor == Some(CatchUpFetchCursor::After(cursor)))
            {
                self.failed_fetches.lock().unwrap().push(request);
                return Err("temporary held-gap fetch failure".to_owned());
            }
            if let Some((call, entered)) = &self.blocked_fetch
                && self.strict.fetch_log().len() == *call
            {
                entered.notify_one();
                std::future::pending::<()>().await;
            }
            if let Some((call, entered, resume)) = &self.resumable_fetch
                && self.strict.fetch_log().len() == *call
            {
                entered.notify_one();
                resume.notified().await;
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

        async fn record_too_old_dead_letter(
            &self,
            pool: Option<&sqlx::PgPool>,
            record: crate::db::relay_dead_letter::RelayDeadLetterRecord,
        ) -> Option<crate::db::relay_dead_letter::TooOldRecordOutcome> {
            self.dead_letters.lock().unwrap().push(record.clone());
            if pool.is_some() {
                crate::db::relay_dead_letter::record_too_old(pool, record).await
            } else {
                Some(crate::db::relay_dead_letter::TooOldRecordOutcome::Inserted)
            }
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

    async fn periodic_sweep(
        api: &RecordingApi,
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        clock: &RecoveryClock,
    ) {
        let pending = super::super::super::collect_catch_up_retry_pending_channels(shared);
        let deps = CatchUpDeps::new(api, shared, provider).with_clock(clock);
        run_catch_up_sweep(deps.with_pending_retry_channels(&pending)).await;
    }

    async fn assert_exhausted_recovery(phase2: bool) {
        let fx = Fixture::new().await;
        let clock = RecoveryClock::new();
        let channel_id = ChannelId::new(6_813_200 + u64::from(phase2));
        let (a, b) = (clock.message_id(1, 600), clock.message_id(2, 30));
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
            if attempt > 1 {
                clock.advance(60);
            }
            if attempt == 8 {
                messages.push(clock.message_id(3, 90));
            } else if attempt == 9 {
                messages.push(clock.message_id(4, 60));
            }
            let scan = api(&fx.shared, channel_id, a, &messages, phase2, true);
            periodic_sweep(&scan, &fx.shared, &fx.provider, &clock).await;
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
            }
        }
        clock.advance(60);
        let before_repair = api(&fx.shared, channel_id, a, &messages, phase2, true);
        periodic_sweep(&before_repair, &fx.shared, &fx.provider, &clock).await;
        assert_eq!(fx.surfaces(channel_id), (Some(a.get()), Some(a.get())));
        assert!(
            before_repair.dead_letters.lock().unwrap().is_empty(),
            "held B never ages into DLQ"
        );
        assert_eq!(
            *before_repair.outcomes.lock().unwrap(),
            [(b.get(), false, true)]
        );
        let hold = fx
            .pending(channel_id)
            .expect("exhausted B stays held until repaired");
        assert!(
            hold.deferred_rearms > 8,
            "hold carries no fresh retry budget"
        );
        assert_eq!(hold.armed_at, origin.unwrap());
        assert_eq!(hold.checkpoint, a.get());
        assert_hold_excluded(&fx, channel_id);
        let checkpoint_file = checkpoint_path(fx.root.path(), &fx.provider, channel_id);
        let times = std::fs::FileTimes::new()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(11 * 60));
        // Windows needs a writable handle to change file times.
        std::fs::File::options()
            .write(true)
            .open(&checkpoint_file)
            .unwrap()
            .set_times(times)
            .unwrap();
        clock.advance(60);
        let pruned = api(&fx.shared, channel_id, a, &messages, phase2, true);
        periodic_sweep(&pruned, &fx.shared, &fx.provider, &clock).await;
        assert_eq!(fx.surfaces(channel_id), (Some(a.get()), None));
        assert_eq!(*pruned.outcomes.lock().unwrap(), [(b.get(), false, true)]);
        assert!(pruned.dead_letters.lock().unwrap().is_empty());
        assert!(fx.pending(channel_id).unwrap().deferred_rearms > 8);
        assert!(
            queue_file.is_dir(),
            "failed writes preserve the broken path"
        );
        std::fs::remove_dir(&queue_file).expect("repair the enqueue persistence failure");
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut first = api(&fx.shared, channel_id, a, &messages, phase2, false);
        let mut second = api(&fx.shared, channel_id, a, &messages, phase2, false);
        first.entry_barrier = Some(Arc::clone(&barrier));
        second.entry_barrier = Some(barrier);
        let rescan = periodic_sweep;
        clock.advance(60);
        tokio::join!(
            rescan(&first, &fx.shared, &fx.provider, &clock),
            rescan(&second, &fx.shared, &fx.provider, &clock)
        );
        let mut reread_b = false;
        for scan in [&first, &second] {
            if scan
                .strict
                .fetch_log()
                .first()
                .is_some_and(|(_, request, _)| *request == after(a))
            {
                assert_phase1_read(&scan.strict, after(a), &messages);
                reread_b = true;
            }
        }
        assert!(
            reread_b,
            "a real recovery fetch sees B behind the held cursor"
        );
        assert_eq!(first.accepted(b) + second.accepted(b), 1);
        assert!(
            fx.pending(channel_id).is_none(),
            "a completed held scan releases its hold"
        );
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
        rescan(&extra, &fx.shared, &fx.provider, &clock).await;
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
        rescan(&restart_scan, &restarted, &fx.provider, &clock).await;
        assert_phase1_read(&restart_scan.strict, after(a), &messages);
        assert_eq!(restart_scan.accepted(b), 0);
        assert_eq!(sources(&read_disk()), messages);

        let logs = logs.finish();
        assert_eq!(
            logs.lines()
                .filter(|line| line.contains("giving up after 8 re-arm(s)"))
                .count(),
            1,
            "repeated Deferred holds emit no new exhaustion warning"
        );
        let exhausted = logs
            .lines()
            .find(|line| line.contains("giving up after 8 re-arm(s)"))
            .expect("the actual exhausted path emits its WARN");
        assert!(exhausted.contains("repair the cause"), "{exhausted}");
        assert!(exhausted.contains("next periodic sweep"), "{exhausted}");
        assert!(exhausted.contains("in-process"), "{exhausted}");
        assert!(exhausted.contains("restart"), "{exhausted}");
        assert!(exhausted.contains("TooOld DLQ"), "{exhausted}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn phase1_exhausted_persistence_retry_recovers_once_after_full_rescan() {
        assert_exhausted_recovery(false).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn phase2_exhausted_persistence_retry_recovers_once_after_full_rescan() {
        assert_exhausted_recovery(true).await;
    }

    fn assert_hold_excluded(fx: &Fixture, channel_id: ChannelId) {
        let hold = fx.pending(channel_id).expect("hold remains pending");
        assert_eq!(hold.deferred_rearms, 9);
        assert!(
            !super::super::super::collect_catch_up_retry_pending_channels(&fx.shared)
                .contains(&channel_id)
        );
        assert!(
            super::super::super::take_catch_up_retry_checkpoint_after_queue_drain(
                &fx.shared, channel_id, 0
            )
            .is_none()
        );
        assert_eq!(
            fx.pending(channel_id),
            Some(hold),
            "queue drain cannot consume the hold"
        );
    }

    async fn fail_actual_queue_writes(
        fx: &Fixture,
        clock: &RecoveryClock,
        channel_id: ChannelId,
        a: MessageId,
        messages: &[MessageId],
        phase2: bool,
        attempts: u8,
    ) -> std::path::PathBuf {
        fx.seed_checkpoint(channel_id, a);
        let path = discord::runtime_store::discord_pending_queue_root()
            .unwrap()
            .join(fx.provider.as_str())
            .join(&fx.shared.token_hash)
            .join(format!("{}.json", channel_id.get()));
        std::fs::create_dir_all(&path).unwrap();
        for attempt in 1..=attempts {
            if attempt > 1 {
                clock.advance(60);
            }
            let scan = api(&fx.shared, channel_id, a, messages, phase2, true);
            periodic_sweep(&scan, &fx.shared, &fx.provider, clock).await;
            assert_eq!(
                *scan.outcomes.lock().unwrap(),
                [(messages[0].get(), false, true)]
            );
            assert_eq!(fx.surfaces(channel_id), (Some(a.get()), Some(a.get())));
            assert!(
                discord::mailbox_snapshot(&fx.shared, channel_id)
                    .await
                    .intervention_queue
                    .is_empty()
            );
            assert_eq!(fx.pending(channel_id).unwrap().deferred_rearms, attempt);
        }
        path
    }

    async fn exhaust_actual_failures(
        fx: &Fixture,
        clock: &RecoveryClock,
        channel_id: ChannelId,
        a: MessageId,
        messages: &[MessageId],
        phase2: bool,
    ) -> std::path::PathBuf {
        let path = fail_actual_queue_writes(fx, clock, channel_id, a, messages, phase2, 9).await;
        assert_hold_excluded(fx, channel_id);
        path
    }

    async fn dead_letter_count(pool: &sqlx::PgPool, message_id: MessageId) -> usize {
        let key = message_id.to_string();
        stored_dead_letters(pool)
            .await
            .into_iter()
            .filter(|row| row.3.as_deref() == Some(key.as_str()))
            .count()
    }

    async fn stored_dead_letter(pool: &sqlx::PgPool, message_id: MessageId) -> StoredDeadLetter {
        let key = message_id.to_string();
        stored_dead_letters(pool)
            .await
            .into_iter()
            .find(|row| row.3.as_deref() == Some(key.as_str()))
            .expect("read back the actual TooOld row")
    }

    async fn recovery_pg_fixture(
        label: &str,
    ) -> (
        Fixture,
        crate::dispatch::test_support::DispatchPostgresTestDb,
        sqlx::PgPool,
    ) {
        let mut fx = Fixture::new().await;
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_catch_up_hold_ttl",
            label,
        )
        .await;
        let pool = db.connect_and_migrate().await;
        fx.shared = discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        {
            let mut settings = fx.shared.settings.write().await;
            settings.owner_user_id = Some(OWNER_ID);
            settings.allow_all_users = true;
        }
        (fx, db, pool)
    }

    pub(in super::super) async fn assert_hold_ttl_and_cancel_pg(phase2: bool) {
        let (fx, db, pool) = recovery_pg_fixture("exhausted hold TTL and cancellation").await;
        let clock = RecoveryClock::new();
        let channel_id = ChannelId::new(6_813_220 + u64::from(phase2));
        let (a, b) = (clock.message_id(1, 600), clock.message_id(2, 30));
        exhaust_actual_failures(&fx, &clock, channel_id, a, &[b], phase2).await;
        let origin = fx.pending(channel_id).unwrap().armed_at;
        let logs = LogWriter::capture();
        clock.advance(24 * 60 * 60 - 480 - 1);
        let before = api(&fx.shared, channel_id, a, &[b], phase2, true);
        periodic_sweep(&before, &fx.shared, &fx.provider, &clock).await;
        assert_eq!(*before.outcomes.lock().unwrap(), [(b.get(), false, true)]);
        assert!(before.dead_letters.lock().unwrap().is_empty());
        assert_eq!(dead_letter_count(&pool, b).await, 0);
        assert_eq!(fx.surfaces(channel_id), (Some(a.get()), Some(a.get())));
        assert_eq!(fx.pending(channel_id).unwrap().armed_at, origin);
        assert_hold_excluded(&fx, channel_id);

        clock.advance(1);
        let entered = Arc::new(tokio::sync::Notify::new());
        let mut parked = api(&fx.shared, channel_id, a, &[b], phase2, true);
        parked.blocked_fetch = Some((usize::from(phase2), Arc::clone(&entered)));
        let mut in_flight = Box::pin(periodic_sweep(&parked, &fx.shared, &fx.provider, &clock));
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                _ = &mut in_flight => panic!("the held scan must park before classifying B"),
                _ = entered.notified() => {}
            }
        })
        .await
        .expect("held scan obtains its fetch lease");
        let held = fx.pending(channel_id).unwrap();
        assert_eq!(held.deferred_rearms, 9);
        assert!(
            fx.shared
                .catch_up_scan_in_progress
                .contains_key(&channel_id)
        );
        let busy = api(&fx.shared, channel_id, a, &[b], phase2, true);
        let handed_retry = CatchUpRetryState {
            deferred_rearms: 8,
            ..held
        };
        let handed = HashMap::from([(channel_id, handed_retry)]);
        let deps = CatchUpDeps::new(&busy, &fx.shared, &fx.provider).with_clock(&clock);
        run_catch_up_sweep(deps.with_retry_checkpoints(&handed)).await;
        assert_eq!(
            fx.shared
                .catch_up_scan_in_progress
                .get(&channel_id)
                .map(|entry| *entry),
            Some((true, Some(handed_retry))),
            "the busy caller hands its stale retry to the owner"
        );
        assert!(
            busy.strict.fetch_log().is_empty(),
            "a leased channel is skipped in both phases"
        );
        assert!(busy.dead_letters.lock().unwrap().is_empty());
        assert_eq!(fx.surfaces(channel_id), (Some(a.get()), Some(a.get())));
        assert_eq!(
            fx.pending(channel_id),
            Some(held),
            "the busy scan cannot change the owner's hold"
        );
        assert!(
            fx.shared
                .catch_up_scan_in_progress
                .contains_key(&channel_id)
        );
        drop(in_flight);
        assert!(
            !fx.shared
                .catch_up_scan_in_progress
                .contains_key(&channel_id)
        );
        assert_eq!(
            fx.pending(channel_id),
            Some(held),
            "cancellation preserves the hold's cursor and original TTL"
        );
        assert_hold_excluded(&fx, channel_id);
        assert_eq!(fx.pending(channel_id).unwrap().armed_at, origin);
        assert_eq!(dead_letter_count(&pool, b).await, 0);

        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let mut first = api(&fx.shared, channel_id, a, &[b], phase2, true);
        let mut second = api(&fx.shared, channel_id, a, &[b], phase2, true);
        first.entry_barrier = Some(Arc::clone(&barrier));
        second.entry_barrier = Some(barrier);
        tokio::join!(
            periodic_sweep(&first, &fx.shared, &fx.provider, &clock),
            periodic_sweep(&second, &fx.shared, &fx.provider, &clock),
        );
        assert_eq!(
            first.dead_letters.lock().unwrap().len() + second.dead_letters.lock().unwrap().len(),
            1
        );
        assert_eq!(dead_letter_count(&pool, b).await, 1);
        let row = stored_dead_letter(&pool, b).await;
        assert_eq!(
            row,
            (
                crate::db::relay_dead_letter::KIND_CATCH_UP_TOO_OLD.to_owned(),
                channel_id.to_string(),
                Some(HUMAN_ID.to_string()),
                Some(b.to_string()),
                human(channel_id, b).content,
                "exhausted catch-up hold expired at 24h TTL".to_owned(),
            )
        );
        assert_eq!(fx.surfaces(channel_id), (Some(b.get()), Some(b.get())));
        assert!(fx.pending(channel_id).is_none());
        assert!(
            discord::mailbox_snapshot(&fx.shared, channel_id)
                .await
                .intervention_queue
                .is_empty()
        );
        assert_eq!(first.accepted(b) + second.accepted(b), 0);

        clock.advance(1);
        let after = api(&fx.shared, channel_id, a, &[b], phase2, true);
        periodic_sweep(&after, &fx.shared, &fx.provider, &clock).await;
        assert!(after.dead_letters.lock().unwrap().is_empty());
        assert_eq!(dead_letter_count(&pool, b).await, 1);
        assert_eq!(fx.surfaces(channel_id), (Some(b.get()), Some(b.get())));
        assert!(fx.pending(channel_id).is_none());
        let late_drain = api(&fx.shared, channel_id, a, &[b], phase2, true);
        let late_handed = HashMap::from([(channel_id, handed_retry)]);
        let deps = CatchUpDeps::new(&late_drain, &fx.shared, &fx.provider).with_clock(&clock);
        run_catch_up_sweep(deps.with_retry_checkpoints(&late_handed)).await;
        assert_eq!(
            late_drain.accepted(b),
            0,
            "the late original eighth arm cannot resurrect TTL-retired B"
        );
        assert!(late_drain.dead_letters.lock().unwrap().is_empty());
        assert_eq!(dead_letter_count(&pool, b).await, 1);
        assert_eq!(fx.surfaces(channel_id), (Some(b.get()), Some(b.get())));
        assert!(fx.pending(channel_id).is_none());
        assert!(logs.finish().contains("exhausted hold expired at 24h TTL"));
        pool.close().await;
        db.drop().await;
    }

    pub(in super::super) async fn assert_exhausted_recovery_sender_authorization_pg(phase2: bool) {
        let (fx, db, pool) = recovery_pg_fixture("exhausted hold sender authorization").await;
        let clock = RecoveryClock::new();
        let channel_id = ChannelId::new(6_813_260 + u64::from(phase2));
        let (a, b) = (clock.message_id(1, 600), clock.message_id(2, 30));
        exhaust_actual_failures(&fx, &clock, channel_id, a, &[b], phase2).await;
        {
            let mut settings = fx.shared.settings.write().await;
            settings.allow_all_users = false;
            settings.allowed_user_ids.clear();
        }
        assert!(
            !crate::services::discord::discord_io::author_authorized(&fx.shared, HUMAN_ID).await,
            "B's sender loses authorization after the nine real persistence failures"
        );
        clock.advance(24 * 60 * 60 - 480);
        for offset in 0..=1 {
            clock.advance(offset);
            let scan = api(&fx.shared, channel_id, a, &[b], phase2, true);
            periodic_sweep(&scan, &fx.shared, &fx.provider, &clock).await;
            if offset == 0 {
                assert!(
                    scan.strict
                        .fetch_log()
                        .iter()
                        .any(|(_, _, ids)| ids.contains(&b.get())),
                    "the actual TTL sweep reads unauthorized B"
                );
            }
            assert!(scan.outcomes.lock().unwrap().is_empty());
            assert!(scan.dead_letters.lock().unwrap().is_empty());
            assert_eq!(dead_letter_count(&pool, b).await, 0);
            assert_eq!(scan.accepted(b), 0);
            assert!(
                discord::mailbox_snapshot(&fx.shared, channel_id)
                    .await
                    .intervention_queue
                    .is_empty()
            );
        }
        pool.close().await;
        db.drop().await;
    }

    fn unresolved_dispatch_api(
        shared: &Arc<SharedData>,
        channel_id: ChannelId,
        a: MessageId,
        b: MessageId,
        phase2: bool,
    ) -> RecordingApi {
        let mut strict = StrictApi::new(shared);
        if phase2 {
            strict = strict.with_history(channel_id, vec![own_reply(channel_id, a)]);
        }
        strict = strict.arriving_at(
            usize::from(phase2),
            discord_message(
                channel_id,
                b,
                INFO_BOT_ID,
                true,
                "DISPATCH:6813-unresolved-announce",
            ),
        );
        let mut api = RecordingApi::new(strict);
        api.utility_ids = Some((
            UtilityBotUserIdResolution::Unavailable,
            UtilityBotUserIdResolution::Unconfigured,
        ));
        api
    }

    pub(in super::super) async fn assert_exhausted_recovery_unresolved_dispatch_ttl_pg(
        phase2: bool,
    ) {
        let (fx, db, pool) =
            recovery_pg_fixture("exhausted unresolved announce DISPATCH TTL").await;
        let clock = RecoveryClock::new();
        let channel_id = ChannelId::new(6_813_300 + u64::from(phase2));
        let (a, b) = (clock.message_id(1, 600), clock.message_id(2, 30));
        fx.seed_checkpoint(channel_id, a);
        {
            let mut settings = fx.shared.settings.write().await;
            assert!(!settings.allowed_bot_ids.contains(&INFO_BOT_ID));
            settings.allow_all_users = false;
            settings.allowed_user_ids.clear();
        }
        let mut origin = None;
        for attempt in 1..=9 {
            if attempt > 1 {
                clock.advance(60);
            }
            let scan = unresolved_dispatch_api(&fx.shared, channel_id, a, b, phase2);
            periodic_sweep(&scan, &fx.shared, &fx.provider, &clock).await;
            assert!(
                scan.strict
                    .fetch_log()
                    .iter()
                    .any(|(_, _, ids)| ids.contains(&b.get()))
            );
            assert!(
                scan.outcomes.lock().unwrap().is_empty(),
                "identity uncertainty defers before enqueue"
            );
            assert!(scan.dead_letters.lock().unwrap().is_empty());
            assert_eq!(fx.surfaces(channel_id), (Some(a.get()), Some(a.get())));
            let pending = fx
                .pending(channel_id)
                .expect("ambiguous DISPATCH stays retryable");
            assert_eq!(pending.deferred_rearms, attempt);
            assert_eq!(pending.armed_at, *origin.get_or_insert(pending.armed_at));
        }
        assert_hold_excluded(&fx, channel_id);
        assert_eq!(dead_letter_count(&pool, b).await, 0);
        clock.advance(24 * 60 * 60 - 480 - 1);
        let before = unresolved_dispatch_api(&fx.shared, channel_id, a, b, phase2);
        periodic_sweep(&before, &fx.shared, &fx.provider, &clock).await;
        assert!(
            before
                .strict
                .fetch_log()
                .iter()
                .any(|(_, _, ids)| ids.contains(&b.get()))
        );
        assert!(before.outcomes.lock().unwrap().is_empty());
        assert_hold_excluded(&fx, channel_id);
        assert!(before.dead_letters.lock().unwrap().is_empty());
        assert_eq!(fx.pending(channel_id).unwrap().armed_at, origin.unwrap());
        assert_eq!(fx.surfaces(channel_id), (Some(a.get()), Some(a.get())));
        clock.advance(1);
        let expired = unresolved_dispatch_api(&fx.shared, channel_id, a, b, phase2);
        periodic_sweep(&expired, &fx.shared, &fx.provider, &clock).await;
        assert!(
            expired
                .strict
                .fetch_log()
                .iter()
                .any(|(_, _, ids)| ids.contains(&b.get()))
        );
        assert_eq!(expired.dead_letters.lock().unwrap().len(), 1);
        assert_eq!(dead_letter_count(&pool, b).await, 1);
        let row = stored_dead_letter(&pool, b).await;
        assert_eq!(
            row,
            (
                crate::db::relay_dead_letter::KIND_CATCH_UP_TOO_OLD.to_owned(),
                channel_id.to_string(),
                Some(INFO_BOT_ID.to_string()),
                Some(b.to_string()),
                "DISPATCH:6813-unresolved-announce".to_owned(),
                "exhausted catch-up hold expired at 24h TTL".to_owned(),
            )
        );
        assert!(expired.outcomes.lock().unwrap().is_empty());
        assert_eq!(fx.surfaces(channel_id), (Some(b.get()), Some(b.get())));
        assert!(fx.pending(channel_id).is_none());
        clock.advance(1);
        let after = unresolved_dispatch_api(&fx.shared, channel_id, a, b, phase2);
        periodic_sweep(&after, &fx.shared, &fx.provider, &clock).await;
        assert!(after.dead_letters.lock().unwrap().is_empty());
        assert_eq!(after.accepted(b), 0);
        assert_eq!(dead_letter_count(&pool, b).await, 1);
        pool.close().await;
        db.drop().await;
    }

    pub(in super::super) async fn assert_exhausted_recovery_fresh_dispatch_ttl_pg(phase2: bool) {
        let (fx, db, pool) =
            recovery_pg_fixture("exhausted hold does not authorize fresh DISPATCH").await;
        let clock = RecoveryClock::new();
        let channel_id = ChannelId::new(6_813_320 + u64::from(phase2));
        let (a, b) = (clock.message_id(1, 600), clock.message_id(2, 30));
        fx.seed_checkpoint(channel_id, a);
        {
            let mut settings = fx.shared.settings.write().await;
            assert!(!settings.allowed_bot_ids.contains(&INFO_BOT_ID));
            settings.allow_all_users = false;
            settings.allowed_user_ids.clear();
        }
        for attempt in 1..=9 {
            if attempt > 1 {
                clock.advance(60);
            }
            let scan = unresolved_dispatch_api(&fx.shared, channel_id, a, b, phase2);
            periodic_sweep(&scan, &fx.shared, &fx.provider, &clock).await;
            assert!(scan.outcomes.lock().unwrap().is_empty());
            assert_eq!(fx.pending(channel_id).unwrap().deferred_rearms, attempt);
        }
        let hold = fx.pending(channel_id).unwrap();
        clock.advance(24 * 60 * 60 - 480);
        let c = clock.message_id(3, 30);
        // The next page exposes only fresh C, so the expired arm must not authorize it.
        let fresh = unresolved_dispatch_api(&fx.shared, channel_id, a, c, phase2);
        periodic_sweep(&fresh, &fx.shared, &fx.provider, &clock).await;
        assert!(
            fresh
                .strict
                .fetch_log()
                .iter()
                .any(|(_, _, ids)| ids.contains(&c.get()))
        );
        assert!(
            fresh.outcomes.lock().unwrap().is_empty(),
            "fresh unresolved C never reaches enqueue"
        );
        assert!(fresh.dead_letters.lock().unwrap().is_empty());
        assert_eq!(dead_letter_count(&pool, c).await, 0);
        assert_eq!(dead_letter_count(&pool, b).await, 0);
        assert_eq!(fx.pending(channel_id), Some(hold));
        assert_eq!(fx.surfaces(channel_id), (Some(a.get()), Some(a.get())));
        assert!(
            discord::mailbox_snapshot(&fx.shared, channel_id)
                .await
                .intervention_queue
                .is_empty()
        );
        pool.close().await;
        db.drop().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exhausted_recovery_hold_moves_to_accepted_b_before_deferred_c_without_renewing_ttl() {
        let fx = Fixture::new().await;
        let clock = RecoveryClock::new();
        let channel_id = ChannelId::new(6_813_240);
        let (a, b, c) = (
            clock.message_id(1, 600),
            clock.message_id(2, 30),
            clock.message_id(3, 20),
        );
        let path = exhaust_actual_failures(&fx, &clock, channel_id, a, &[b, c], false).await;
        let origin = fx.pending(channel_id).unwrap().armed_at;
        std::fs::remove_dir(path).expect("repair the real persistence failure");
        clock.advance(60);
        let mut scan = api(&fx.shared, channel_id, a, &[b, c], false, false);
        scan.strict = scan.strict.with_hooks(c, &[Hook::Defer]);
        periodic_sweep(&scan, &fx.shared, &fx.provider, &clock).await;
        assert_eq!(
            *scan.outcomes.lock().unwrap(),
            [(b.get(), true, false), (c.get(), false, false)]
        );
        assert_eq!(fx.surfaces(channel_id), (Some(b.get()), Some(b.get())));
        let hold = fx.pending(channel_id).expect("C's defer retains the hold");
        assert_eq!(hold.checkpoint, b.get());
        assert_eq!(hold.armed_at, origin);
        assert_hold_excluded(&fx, channel_id);
        clock.advance(60);
        let recovered = api(&fx.shared, channel_id, a, &[b, c], false, false);
        periodic_sweep(&recovered, &fx.shared, &fx.provider, &clock).await;
        assert_phase1_read(&recovered.strict, after(b), &[c]);
        assert_eq!(recovered.accepted(c), 1);
        assert_eq!(recovered.accepted(b), 0);
        assert!(fx.pending(channel_id).is_none());
        assert_eq!(fx.surfaces(channel_id), (Some(c.get()), Some(c.get())));
        let stored =
            load_channel_pending_queue_checked(&fx.provider, &fx.shared.token_hash, channel_id)
                .unwrap()
                .0;
        assert_eq!(sources(&stored), [b, c]);
    }

    async fn assert_drained_retry_handoff(phase2: bool) {
        let fx = Fixture::new().await;
        let clock = RecoveryClock::new();
        let channel_id = ChannelId::new(6_813_280 + u64::from(phase2));
        let (a, b) = (clock.message_id(1, 600), clock.message_id(2, 30));
        let path = fail_actual_queue_writes(&fx, &clock, channel_id, a, &[b], phase2, 8).await;
        let armed = fx.pending(channel_id).unwrap();
        let drained = super::super::super::take_catch_up_retry_checkpoint_after_queue_drain(
            &fx.shared, channel_id, 0,
        )
        .expect("the real drain consumes the bounded eighth arm");
        assert_eq!(drained, armed);
        assert_eq!(drained.deferred_rearms, 8);
        assert!(fx.pending(channel_id).is_none());
        assert_eq!(
            fx.shared
                .catch_up_scan_in_progress
                .get(&channel_id)
                .map(|entry| *entry),
            Some((false, Some(drained))),
            "drain atomically reserves the original arm"
        );
        clock.advance(240);
        assert!(
            clock
                .sample()
                .0
                .signed_duration_since(*b.created_at())
                .num_seconds()
                > 600
        );
        let entered = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        let mut owner = api(&fx.shared, channel_id, a, &[b], phase2, true);
        owner.resumable_fetch = Some((0, Arc::clone(&entered), Arc::clone(&resume)));
        let mut running = Box::pin(periodic_sweep(&owner, &fx.shared, &fx.provider, &clock));
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                _ = &mut running => panic!("periodic owner must park before reading B"),
                _ = entered.notified() => {}
            }
        })
        .await
        .expect("periodic owner acquires its channel lease");
        assert_eq!(
            fx.shared
                .catch_up_scan_in_progress
                .get(&channel_id)
                .map(|entry| *entry),
            Some((true, None)),
            "the periodic owner inherits the reserved arm without explicit retry input"
        );
        let drain = api(&fx.shared, channel_id, a, &[b], phase2, true);
        let handed = HashMap::from([(channel_id, drained)]);
        let deps = CatchUpDeps::new(&drain, &fx.shared, &fx.provider).with_clock(&clock);
        run_catch_up_sweep(deps.with_retry_checkpoints(&handed)).await;
        assert!(drain.strict.fetch_log().is_empty());
        assert_eq!(
            fx.shared
                .catch_up_scan_in_progress
                .get(&channel_id)
                .map(|entry| *entry),
            Some((true, Some(drained))),
            "the busy drain publishes its actual state8 handoff"
        );
        resume.notify_one();
        tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .expect("owner resumes");
        assert!(
            owner
                .strict
                .fetch_log()
                .iter()
                .any(|(_, _, ids)| ids.contains(&b.get()))
        );
        assert_eq!(*owner.outcomes.lock().unwrap(), [(b.get(), false, true)]);
        assert!(owner.dead_letters.lock().unwrap().is_empty());
        assert!(drain.dead_letters.lock().unwrap().is_empty());
        assert_eq!(fx.surfaces(channel_id), (Some(a.get()), Some(a.get())));
        let hold = fx
            .pending(channel_id)
            .expect("the handed eighth arm becomes an exhausted hold");
        assert_eq!(hold.deferred_rearms, 9);
        assert_eq!(hold.checkpoint, a.get());
        assert_eq!(hold.armed_at, drained.armed_at);
        assert_hold_excluded(&fx, channel_id);
        assert!(
            !fx.shared
                .catch_up_scan_in_progress
                .contains_key(&channel_id)
        );
        std::fs::remove_dir(path).expect("repair the actual durable enqueue path");
        clock.advance(60);
        let recovery = api(&fx.shared, channel_id, a, &[b], phase2, true);
        periodic_sweep(&recovery, &fx.shared, &fx.provider, &clock).await;
        assert_eq!(recovery.accepted(b), 1);
        assert!(recovery.dead_letters.lock().unwrap().is_empty());
        assert!(fx.pending(channel_id).is_none());
        let stored =
            load_channel_pending_queue_checked(&fx.provider, &fx.shared.token_hash, channel_id)
                .expect("read the recovered durable queue")
                .0;
        assert_eq!(sources(&stored), [b]);
        assert_eq!(fx.surfaces(channel_id), (Some(b.get()), Some(b.get())));
        let again = api(&fx.shared, channel_id, a, &[b], phase2, true);
        periodic_sweep(&again, &fx.shared, &fx.provider, &clock).await;
        assert_eq!(again.accepted(b), 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn phase1_exhausted_recovery_adopts_busy_drain_retry_before_age_and_budget() {
        assert_drained_retry_handoff(false).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn phase2_exhausted_recovery_adopts_busy_drain_retry_before_age_and_budget() {
        assert_drained_retry_handoff(true).await;
    }

    // A reply after B hides B from phase 2's unanswered slice, while phase 1 still owes B.
    async fn assert_phase2_successor_cannot_release_unsettled_hold(active_successor: bool) {
        let fx = Fixture::new().await;
        let clock = RecoveryClock::new();
        let channel_id = ChannelId::new(6_813_290 + u64::from(active_successor));
        let (a, b) = (clock.message_id(1, 600), clock.message_id(2, 30));
        let (r, successor_id) = (clock.message_id(3, 20), clock.message_id(4, 10));
        fx.seed_checkpoint(channel_id, a);
        if active_successor {
            let token = Arc::new(crate::services::provider::CancelToken::new());
            assert!(
                discord::mailbox_try_start_turn(
                    &fx.shared,
                    channel_id,
                    token,
                    serenity::UserId::new(HUMAN_ID),
                    successor_id,
                )
                .await
            );
        }
        let successor = if active_successor {
            human(channel_id, successor_id)
        } else {
            foreign_bot(channel_id, successor_id)
        };
        let queue_file = discord::runtime_store::discord_pending_queue_root()
            .expect("queue root")
            .join(fx.provider.as_str())
            .join(&fx.shared.token_hash)
            .join(format!("{}.json", channel_id.get()));
        std::fs::create_dir_all(&queue_file).expect("directory blocks durable enqueue");
        let build = || {
            RecordingApi::new(StrictApi::new(&fx.shared).with_history(
                channel_id,
                vec![
                    human(channel_id, b),
                    own_reply(channel_id, r),
                    successor.clone(),
                ],
            ))
        };
        let mut origin = None;
        for attempt in 1..=9 {
            if attempt > 1 {
                clock.advance(60);
            }
            let scan = build();
            periodic_sweep(&scan, &fx.shared, &fx.provider, &clock).await;
            assert_phase1_read(&scan.strict, after(a), &[b, r, successor_id]);
            let fetches = scan.strict.fetch_log();
            assert_eq!(fetches.len(), 2, "the same sweep actually runs both phases");
            assert_eq!(
                fetches[1],
                (
                    channel_id,
                    CatchUpFetchRequest::new(20),
                    vec![successor_id.get(), r.get(), b.get()]
                )
            );
            assert_eq!(*scan.outcomes.lock().unwrap(), [(b.get(), false, true)]);
            assert!(scan.dead_letters.lock().unwrap().is_empty());
            assert_eq!(fx.surfaces(channel_id), (Some(a.get()), Some(a.get())));
            let snapshot = discord::mailbox_snapshot(&fx.shared, channel_id).await;
            assert!(snapshot.intervention_queue.is_empty());
            assert_eq!(
                snapshot.active_user_message_id,
                active_successor.then_some(successor_id)
            );
            let state = fx
                .pending(channel_id)
                .expect("B's retry survives its sweep");
            assert_eq!(state.deferred_rearms, attempt);
            assert_eq!(state.checkpoint, a.get());
            assert_eq!(state.armed_at, *origin.get_or_insert(state.armed_at));
        }
        assert_hold_excluded(&fx, channel_id);
        clock.advance(60);
        std::fs::remove_dir(&queue_file).expect("repair the actual durable enqueue path");
        let repaired = build();
        periodic_sweep(&repaired, &fx.shared, &fx.provider, &clock).await;
        assert_phase1_read(&repaired.strict, after(a), &[b, r, successor_id]);
        assert_eq!(
            repaired.accepted(b),
            1,
            "B is recovered after repair within 24h"
        );
        assert_eq!(*repaired.outcomes.lock().unwrap(), [(b.get(), true, false)]);
        assert!(repaired.dead_letters.lock().unwrap().is_empty());
        assert!(fx.pending(channel_id).is_none());
        let (memory, disk) = fx.surfaces(channel_id);
        assert_eq!(memory, disk);
        assert!(memory.is_some_and(|checkpoint| checkpoint >= b.get()));
        let stored = || {
            load_channel_pending_queue_checked(&fx.provider, &fx.shared.token_hash, channel_id)
                .expect("read the actual durable recovered queue")
                .0
        };
        assert_eq!(sources(&stored()), [b]);
        clock.advance(60);
        let again = build();
        periodic_sweep(&again, &fx.shared, &fx.provider, &clock).await;
        assert_eq!(again.accepted(b), 0);
        assert!(again.dead_letters.lock().unwrap().is_empty());
        assert_eq!(sources(&stored()), [b]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rv_phase2_unblocked_pass_releases_hold_kept_by_phase1() {
        assert_phase2_successor_cannot_release_unsettled_hold(false).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exhausted_recovery_active_turn_after_later_reply_does_not_release_b_hold() {
        assert_phase2_successor_cannot_release_unsettled_hold(true).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exhausted_recovery_empty_phase1_releases_after_phase2_durable_acceptance() {
        let fx = Fixture::new().await;
        let clock = RecoveryClock::new();
        let channel_id = ChannelId::new(6_813_292);
        let (a, b) = (clock.message_id(1, 600), clock.message_id(2, 30));
        let path = exhaust_actual_failures(&fx, &clock, channel_id, a, &[b], false).await;
        std::fs::remove_dir(path).expect("repair the actual durable enqueue path");
        clock.advance(60);
        let build = || {
            RecordingApi::new(
                StrictApi::new(&fx.shared)
                    .with_history(channel_id, vec![own_reply(channel_id, a)])
                    .arriving_at(1, human(channel_id, b)),
            )
        };
        let repaired = build();
        periodic_sweep(&repaired, &fx.shared, &fx.provider, &clock).await;
        assert_eq!(
            repaired.strict.fetch_log(),
            [
                (channel_id, after(a), vec![]),
                (
                    channel_id,
                    CatchUpFetchRequest::new(20),
                    vec![b.get(), a.get()]
                ),
            ]
        );
        assert_eq!(*repaired.outcomes.lock().unwrap(), [(b.get(), true, false)]);
        assert_eq!(repaired.accepted(b), 1);
        assert!(repaired.dead_letters.lock().unwrap().is_empty());
        assert!(
            fx.pending(channel_id).is_none(),
            "phase 2 earns release with B's durable frontier"
        );
        assert_eq!(fx.surfaces(channel_id), (Some(b.get()), Some(b.get())));
        let stored = || {
            load_channel_pending_queue_checked(&fx.provider, &fx.shared.token_hash, channel_id)
                .expect("read phase 2's durable B")
                .0
        };
        assert_eq!(sources(&stored()), [b]);
        clock.advance(60);
        let again = build();
        periodic_sweep(&again, &fx.shared, &fx.provider, &clock).await;
        assert_eq!(again.accepted(b), 0);
        assert!(again.dead_letters.lock().unwrap().is_empty());
        assert_eq!(sources(&stored()), [b]);
    }
    // A partial phase-2 frontier keeps C's original hold while B is already durable.
    #[tokio::test(flavor = "current_thread")]
    async fn exhausted_recovery_empty_phase1_phase2_partial_progress_keeps_c_origin() {
        let fx = Fixture::new().await;
        let clock = RecoveryClock::new();
        let channel_id = ChannelId::new(6_813_293);
        let (a, b, c) = (
            clock.message_id(1, 600),
            clock.message_id(2, 30),
            clock.message_id(3, 20),
        );
        let path = exhaust_actual_failures(&fx, &clock, channel_id, a, &[b, c], false).await;
        let original = fx
            .pending(channel_id)
            .expect("nine actual failures retain the original hold");
        std::fs::remove_dir(path).expect("repair the actual durable enqueue path");
        clock.advance(60);
        let partial = RecordingApi::new(
            StrictApi::new(&fx.shared)
                .with_history(channel_id, vec![own_reply(channel_id, a)])
                .arriving_at(1, human(channel_id, b))
                .arriving_at(1, human(channel_id, c))
                .with_hooks(c, &[Hook::Defer]),
        );
        periodic_sweep(&partial, &fx.shared, &fx.provider, &clock).await;
        assert_eq!(
            partial.strict.fetch_log(),
            [
                (channel_id, after(a), vec![]),
                (
                    channel_id,
                    CatchUpFetchRequest::new(20),
                    vec![c.get(), b.get(), a.get()]
                ),
            ]
        );
        assert_eq!(
            *partial.outcomes.lock().unwrap(),
            [(b.get(), true, false), (c.get(), false, false)]
        );
        assert_eq!(partial.accepted(b), 1);
        assert_eq!(partial.accepted(c), 0);
        assert!(partial.dead_letters.lock().unwrap().is_empty());
        assert_eq!(fx.surfaces(channel_id), (Some(b.get()), Some(b.get())));
        assert_eq!(
            fx.pending(channel_id),
            Some(CatchUpRetryState {
                checkpoint: b.get(),
                ..original
            }),
            "C inherits the original arm and exhausted budgets after B's durable progress"
        );
        assert_hold_excluded(&fx, channel_id);
        let stored = || {
            load_channel_pending_queue_checked(&fx.provider, &fx.shared.token_hash, channel_id)
                .expect("read B and C from the actual durable queue")
                .0
        };
        assert_eq!(sources(&stored()), [b]);
        clock.advance(60);
        let build = || {
            RecordingApi::new(StrictApi::new(&fx.shared).with_history(
                channel_id,
                vec![
                    own_reply(channel_id, a),
                    human(channel_id, b),
                    human(channel_id, c),
                ],
            ))
        };
        let recovered = build();
        periodic_sweep(&recovered, &fx.shared, &fx.provider, &clock).await;
        assert_phase1_read(&recovered.strict, after(a), &[b, c]);
        assert_eq!(
            *recovered.outcomes.lock().unwrap(),
            [(c.get(), true, false)]
        );
        assert_eq!(recovered.accepted(c), 1);
        assert_eq!(recovered.accepted(b), 0);
        assert!(recovered.dead_letters.lock().unwrap().is_empty());
        assert_eq!(fx.surfaces(channel_id), (Some(b.get()), Some(b.get())));
        assert_eq!(
            fx.pending(channel_id),
            Some(CatchUpRetryState {
                checkpoint: b.get(),
                ..original
            })
        );
        assert_eq!(
            super::super::super::scan_progress::read(&fx.provider, channel_id),
            Some(a.get())
        );
        assert_eq!(sources(&stored()), [b, c]);
        clock.advance(60);
        let again = build();
        periodic_sweep(&again, &fx.shared, &fx.provider, &clock).await;
        assert_eq!(again.accepted(b), 0);
        assert_eq!(again.accepted(c), 0);
        assert!(again.dead_letters.lock().unwrap().is_empty());
        assert_eq!(sources(&stored()), [b, c]);
    }

    // A failed held-gap read cannot let a later recent page prove progress over B.
    async fn assert_failed_phase1_keeps_hold_without_phase2(binding_unknown: bool) {
        let fx = Fixture::new().await;
        let clock = RecoveryClock::new();
        let channel_id = ChannelId::new(6_813_294 + u64::from(binding_unknown));
        let (a, b) = (clock.message_id(1, 600), clock.message_id(2, 30));
        let path = exhaust_actual_failures(&fx, &clock, channel_id, a, &[b], false).await;
        let hold = fx
            .pending(channel_id)
            .expect("nine actual failures retain B");
        assert_eq!(hold.deferred_rearms, 9);
        assert_eq!(hold.checkpoint, a.get());
        assert!(
            clock.sample().1.duration_since(hold.armed_at) >= Duration::from_secs(480),
            "the failure sequence crosses eight real periodic intervals"
        );
        std::fs::remove_dir(path).expect("C could now be accepted durably if phase 2 ran");
        clock.advance(60);
        let (reply, c) = (clock.message_id(3, 20), clock.message_id(4, 10));
        assert!(a < b && b < reply && reply < c);
        let mut scan = RecordingApi::new(StrictApi::new(&fx.shared).with_history(
            channel_id,
            vec![
                human(channel_id, b),
                own_reply(channel_id, reply),
                human(channel_id, c),
            ],
        ));
        if binding_unknown {
            scan.first_binding_unknown
                .store(true, std::sync::atomic::Ordering::SeqCst);
        } else {
            scan.fail_after_cursor = Some(a.get());
        }
        periodic_sweep(&scan, &fx.shared, &fx.provider, &clock).await;
        assert_eq!(
            *scan.failed_fetches.lock().unwrap(),
            if binding_unknown {
                vec![]
            } else {
                vec![after(a)]
            },
            "the fetch-error case actually attempts the held cursor"
        );
        assert!(
            scan.strict.fetch_log().is_empty(),
            "phase 2 performs zero actual recent fetches after the held gap was unread"
        );
        assert!(scan.outcomes.lock().unwrap().is_empty());
        assert_eq!(scan.accepted(c), 0);
        assert!(scan.dead_letters.lock().unwrap().is_empty());
        assert_eq!(
            fx.pending(channel_id),
            Some(hold),
            "hold origin and budgets remain exact"
        );
        assert_eq!(fx.surfaces(channel_id), (Some(a.get()), Some(a.get())));
        assert!(
            discord::mailbox_snapshot(&fx.shared, channel_id)
                .await
                .intervention_queue
                .is_empty()
        );
        assert_hold_excluded(&fx, channel_id);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exhausted_recovery_failed_phase1_fetch_preserves_unread_hold_gap() {
        assert_failed_phase1_keeps_hold_without_phase2(false).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exhausted_recovery_unknown_phase1_binding_preserves_unread_hold_gap() {
        assert_failed_phase1_keeps_hold_without_phase2(true).await;
    }
    // A full terminal-only After page cannot discharge a hold before the later B is read.
    #[tokio::test(flavor = "current_thread")]
    async fn exhausted_recovery_full_held_after_page_reads_through_its_upper_snapshot() {
        let fx = Fixture::new().await;
        let clock = RecoveryClock::new();
        let channel_id = ChannelId::new(6_813_296);
        let (a, b) = (clock.message_id(1, 600), clock.message_id(2, 30));
        let (r, f) = (clock.message_id(3, 20), clock.message_id(4, 10));
        let noise: Vec<_> = (0_u64..50)
            .map(|index| clock.message_id(100 + index, 590 - i64::try_from(index).unwrap()))
            .collect();
        let d = *noise.last().unwrap();
        assert!(a < noise[0] && d < b && b < r && r < f);
        let path = exhaust_actual_failures(&fx, &clock, channel_id, a, &[b], true).await;
        let original = fx
            .pending(channel_id)
            .expect("nine failures retain B's original hold");
        clock.advance(60);
        let mut history: Vec<_> = noise.iter().map(|id| own_reply(channel_id, *id)).collect();
        history.extend([
            human(channel_id, b),
            own_reply(channel_id, r),
            foreign_bot(channel_id, f),
        ]);
        let build = || {
            RecordingApi::new(StrictApi::new(&fx.shared).with_history(channel_id, history.clone()))
        };
        let mut unread = build();
        unread.fail_after_cursor = Some(d.get());
        periodic_sweep(&unread, &fx.shared, &fx.provider, &clock).await;
        assert_eq!(*unread.failed_fetches.lock().unwrap(), [after(d)]);
        assert_eq!(
            unread.strict.fetch_log(),
            [
                (
                    channel_id,
                    after(a),
                    noise.iter().rev().map(|id| id.get()).collect()
                ),
                (channel_id, CatchUpFetchRequest::new(1), vec![f.get()]),
            ]
        );
        assert!(unread.outcomes.lock().unwrap().is_empty());
        assert!(unread.dead_letters.lock().unwrap().is_empty());
        assert_eq!(fx.pending(channel_id), Some(original));
        assert_eq!(fx.surfaces(channel_id), (Some(a.get()), Some(a.get())));
        clock.advance(60);
        let blocked = build();
        periodic_sweep(&blocked, &fx.shared, &fx.provider, &clock).await;
        assert_eq!(
            *blocked.outcomes.lock().unwrap(),
            [(b.get(), false, true)],
            "the held-range continuation actually reads and fails B's durable enqueue"
        );
        let first_page: Vec<_> = noise.iter().rev().map(|id| id.get()).collect();
        let recent: Vec<_> = [f.get(), r.get(), b.get()]
            .into_iter()
            .chain(noise.iter().rev().map(|id| id.get()))
            .take(20)
            .collect();
        assert_eq!(
            blocked.strict.fetch_log(),
            [
                (channel_id, after(a), first_page),
                (channel_id, CatchUpFetchRequest::new(1), vec![f.get()]),
                (channel_id, after(d), vec![f.get(), r.get(), b.get()]),
                (channel_id, CatchUpFetchRequest::new(20), recent),
            ]
        );
        assert!(blocked.dead_letters.lock().unwrap().is_empty());
        assert_eq!(fx.surfaces(channel_id), (Some(d.get()), Some(d.get())));
        assert_eq!(
            fx.pending(channel_id),
            Some(CatchUpRetryState {
                checkpoint: d.get(),
                ..original
            }),
            "terminal noise moves the cursor without releasing B or renewing its arm"
        );
        assert_hold_excluded(&fx, channel_id);
        assert!(
            discord::mailbox_snapshot(&fx.shared, channel_id)
                .await
                .intervention_queue
                .is_empty()
        );
        std::fs::remove_dir(path).expect("repair B's actual durable enqueue path");
        clock.advance(60);
        let repaired = build();
        periodic_sweep(&repaired, &fx.shared, &fx.provider, &clock).await;
        assert_phase1_read(&repaired.strict, after(d), &[b, r, f]);
        assert_eq!(
            repaired.strict.fetch_log()[0],
            (channel_id, after(d), vec![f.get(), r.get(), b.get()])
        );
        assert_eq!(*repaired.outcomes.lock().unwrap(), [(b.get(), true, false)]);
        assert_eq!(repaired.accepted(b), 1);
        assert!(repaired.dead_letters.lock().unwrap().is_empty());
        assert!(fx.pending(channel_id).is_none());
        assert_eq!(fx.surfaces(channel_id), (Some(f.get()), Some(f.get())));
        let stored = || {
            load_channel_pending_queue_checked(&fx.provider, &fx.shared.token_hash, channel_id)
                .expect("read B from the actual durable queue")
                .0
        };
        assert_eq!(sources(&stored()), [b]);
        clock.advance(60);
        let again = build();
        periodic_sweep(&again, &fx.shared, &fx.provider, &clock).await;
        assert_eq!(again.accepted(b), 0);
        assert!(again.dead_letters.lock().unwrap().is_empty());
        assert_eq!(sources(&stored()), [b]);
    }
}
