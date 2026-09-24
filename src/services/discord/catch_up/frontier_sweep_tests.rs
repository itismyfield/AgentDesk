//! Sweep-level tests over a fetch fixture that honours the Discord cursor
//! contract, so a retry cursor is proven by what it rereads.

use std::collections::{HashMap, VecDeque};
use std::io::Write;

use super::super::CatchUpRetryState;
use super::*;
use crate::services::discord::{self as discord, MailboxEnqueueOutcome, SharedData};
use crate::services::turn_orchestrator::EnqueueRefusalReason;

#[derive(Clone, Copy)]
enum Hook {
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
