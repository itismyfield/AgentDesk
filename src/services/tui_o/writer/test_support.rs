#![cfg(unix)]
//! Writer fixtures for tests here and for dependent crates' tests: a fake Discord port, lease and
//! alarm sink, a harness over a scratch O store, actor and binding-log helpers, and two drivers.

use std::collections::VecDeque;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use super::actor::{POLL_INTERVAL, run_channel};
use super::binding::{
    BindingCause, BindingEvent, BindingEvents, BindingEvidence, BindingLog, BindingRecord,
    BindingTarget,
};
use super::confirm;
use super::deliver::{ChannelWriter, Step};
use super::pieces::{self, Derived, PieceWork, UnitDeriver};
use super::rotation::Sources;
use super::*;
use crate::services::claude_tui::hook_server::HookEventKind;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::shadow::{CapturedRecord, ShadowProvider, UnitKey, UnitKind};
use crate::services::tui_o::store::ledger::PieceOutcome;
use crate::services::tui_o::store::{ChannelStore, InitSource, Initialized, OStore, StoreConfig};
use crate::services::tui_prompt_dedupe::binding_events::SourceId;

pub(crate) mod transition;

pub(crate) const BOT: u64 = 42;
pub(crate) const CHANNEL: u64 = 7;

thread_local! {
    pub(crate) static RESTART_POLICY_CALLS: std::cell::Cell<Option<(u64, usize)>> = const {
        std::cell::Cell::new(None)
    };
}

pub(crate) fn restart_policy_call(channel: u64) {
    RESTART_POLICY_CALLS.with(|calls| {
        if let Some((selected, count)) = calls.get()
            && selected == channel
        {
            calls.set(Some((selected, count + 1)));
        }
    });
}

#[derive(Clone, Copy)]
pub(crate) enum Reply {
    Created,
    /// Discord created the message but the response was lost.
    CreatedUnseen,
    /// The request never reached Discord and the response was lost.
    Unsent,
    Refused(u16),
    /// Created, but Discord stored different content.
    Transformed,
    /// A created message whose author is not this bot.
    CreatedByOther,
    /// Created by this bot with an id behind the anchor, which the ledger treats as a violation.
    CreatedBehind,
    /// Created by this bot with an id just behind its previous post, as Discord can mint one.
    CreatedReordered,
}

type PostHook = Arc<Mutex<Option<Box<dyn Fn() + Send>>>>;

#[derive(Default)]
pub(crate) struct FakePort {
    usage_footer_pool: std::sync::OnceLock<sqlx::PgPool>,
    pub(crate) history: Mutex<Vec<SeenMessage>>,
    pub(crate) replies: Mutex<VecDeque<Reply>>,
    pub(super) posts: Mutex<Vec<String>>,
    pub(super) prepared_before_post: Mutex<Vec<bool>>,
    pub(crate) ledger: Mutex<Option<PathBuf>>,
    pub(crate) unreadable: AtomicBool,
    /// When set, a POST's HTTP request starts at the future's first poll, like the real client.
    pub(crate) lazy: AtomicBool,
    /// Each lazy request logs `http` when it starts.
    pub(crate) started: Arc<Mutex<Vec<&'static str>>>,
    /// A lazy request waits on this before it completes.
    pub(crate) hold: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    pub(crate) on_post: PostHook,
    pub(crate) history_reads: AtomicUsize,
    pub(crate) on_construct: Mutex<Option<Box<dyn Fn() + Send>>>,
    /// Each POST answers only after this long.
    pub(crate) delay: Mutex<Option<std::time::Duration>>,
}

impl FakePort {
    pub(crate) fn say(&self, author_id: u64, content: &str) -> SeenMessage {
        let mut history = self.history.lock().unwrap();
        let id = 1000 + history.len() as u64;
        let content = content.to_string();
        let message = SeenMessage {
            id,
            author_id,
            content,
        };
        history.push(message.clone());
        message
    }

    pub(crate) fn posts(&self) -> Vec<String> {
        self.posts.lock().unwrap().clone()
    }
}

impl DiscordPort for FakePort {
    fn usage_footer_pool(&self) -> Option<&sqlx::PgPool> {
        self.usage_footer_pool.get()
    }

    fn bot_id(&self) -> u64 {
        BOT
    }

    fn post(
        &self,
        _channel: u64,
        content: String,
    ) -> impl Future<Output = PostOutcome> + Send + 'static {
        if let Some(hook) = self.on_construct.lock().unwrap().as_ref() {
            hook();
        }
        let ledger = self
            .ledger
            .lock()
            .unwrap()
            .clone()
            .map(std::fs::read_to_string);
        let prepared = ledger
            .and_then(Result::ok)
            .is_some_and(|text| text.contains(&format!("\"payload\":{content:?}")));
        self.prepared_before_post.lock().unwrap().push(prepared);
        self.posts.lock().unwrap().push(content.clone());
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Reply::Created);
        let outcome = match reply {
            Reply::Created => PostOutcome::Created(self.say(BOT, &content)),
            Reply::CreatedUnseen => {
                self.say(BOT, &content);
                PostOutcome::Uncertain("response lost".into())
            }
            Reply::Unsent => PostOutcome::Uncertain("connection reset".into()),
            Reply::Refused(status) => PostOutcome::Refused(status),
            Reply::Transformed => PostOutcome::Created(self.say(BOT, &content.to_uppercase())),
            Reply::CreatedByOther => PostOutcome::Created(self.say(BOT + 1, &content)),
            Reply::CreatedBehind => PostOutcome::Created(SeenMessage {
                id: 50,
                author_id: BOT,
                content,
            }),
            Reply::CreatedReordered => {
                let id = self.say(BOT, &content).id - 2;
                PostOutcome::Created(SeenMessage {
                    id,
                    author_id: BOT,
                    content,
                })
            }
        };
        let on_post = Arc::clone(&self.on_post);
        let lazy = self.lazy.load(Ordering::SeqCst).then(|| {
            let hold = self.hold.lock().unwrap().take();
            (Arc::clone(&self.started), hold)
        });
        let delay = *self.delay.lock().unwrap();
        async move {
            if let Some(hook) = on_post.lock().unwrap().as_ref() {
                hook();
            }
            if let Some(delay) = delay {
                tokio::time::sleep(delay).await;
            }
            if let Some((started, hold)) = lazy {
                started.lock().unwrap().push("http");
                if let Some(hold) = hold {
                    let _ = hold.await;
                }
            }
            outcome
        }
    }

    fn history_after(
        &self,
        _channel: u64,
        after: u64,
    ) -> impl Future<Output = Result<Vec<SeenMessage>, String>> + Send {
        self.history_reads.fetch_add(1, Ordering::SeqCst);
        // Without history permission Discord lists nothing rather than refusing.
        let readable = !self.unreadable.load(Ordering::SeqCst);
        let history = self.history.lock().unwrap();
        let page: Vec<SeenMessage> = history
            .iter()
            .filter(|m| readable && m.id > after)
            .take(confirm::HISTORY_PAGE)
            .cloned()
            .collect();
        async move { Ok(page) }
    }

    fn message(
        &self,
        _channel: u64,
        id: u64,
    ) -> impl Future<Output = Result<Option<SeenMessage>, String>> + Send {
        let found = match self.unreadable.load(Ordering::SeqCst) {
            true => Err("HTTP 403 Forbidden".to_owned()),
            false => Ok(self
                .history
                .lock()
                .unwrap()
                .iter()
                .find(|m| m.id == id)
                .cloned()),
        };
        async move { found }
    }
}

#[derive(Default)]
pub(crate) struct FakeLease {
    pub(crate) busy: AtomicBool,
    pub(crate) released: Arc<AtomicUsize>,
    pub(crate) on_acquire: Mutex<Option<Box<dyn Fn() + Send>>>,
    /// Channels whose writer the host reported stopped for good.
    pub(crate) unavailable: Mutex<Vec<u64>>,
}

pub(crate) struct Held(Arc<AtomicUsize>);

impl Drop for Held {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl DeliveryLease for Arc<FakeLease> {
    type Held = Held;

    fn try_acquire(&self, _channel: u64, _serial: u64) -> Option<Held> {
        if let Some(hook) = self.on_acquire.lock().unwrap().as_ref() {
            hook();
        }
        (!self.busy.load(Ordering::SeqCst)).then(|| Held(Arc::clone(&self.released)))
    }

    fn writer_unavailable(&self, channel: u64) {
        self.unavailable.lock().unwrap().push(channel);
    }
}

/// The alarms of one harness channel; an alarm for any other channel fails the test.
#[derive(Clone)]
pub(crate) struct Alarms {
    channel: u64,
    pub(crate) raised: Arc<Mutex<Vec<WriterAlarm>>>,
    pub(crate) cleared: Arc<AtomicUsize>,
    /// The reader count the writer last reported.
    pub(super) readers: Arc<Mutex<Option<usize>>>,
}

impl AlarmSink for Alarms {
    fn raise(&self, channel: u64, alarm: WriterAlarm) {
        assert_eq!(channel, self.channel);
        self.raised.lock().unwrap().push(alarm);
    }

    fn waiting_cleared(&self, channel: u64) {
        assert_eq!(channel, self.channel);
        self.cleared.fetch_add(1, Ordering::SeqCst);
    }

    fn reconcile_reader_count(&self, channel: u64, count: usize) {
        assert_eq!(channel, self.channel);
        *self.readers.lock().unwrap() = Some(count);
    }
}

impl Alarms {
    pub(crate) fn taken(&self) -> Vec<WriterAlarm> {
        std::mem::take(&mut self.raised.lock().unwrap())
    }

    pub(super) fn readers(&self) -> Option<usize> {
        *self.readers.lock().unwrap()
    }
}

pub(crate) type Writer = ChannelWriter<FakePort, Arc<FakeLease>, Alarms>;

pub(crate) struct Harness {
    pub(crate) channel_id: u64,
    pub(crate) _runtime: tempfile::TempDir,
    pub(crate) store: OStore,
    pub(crate) gate: Arc<OwnershipGate>,
    pub(crate) port: Arc<FakePort>,
    pub(crate) lease: Arc<FakeLease>,
    pub(crate) alarms: Alarms,
}

impl Harness {
    pub(crate) fn new() -> Self {
        Self::build(|_| Vec::new())
    }

    /// `sources` may create transcripts under the runtime root before the era begins.
    pub(crate) fn build(sources: impl FnOnce(&std::path::Path) -> Vec<InitSource>) -> Self {
        Self::build_channel(CHANNEL, sources)
    }

    pub(crate) fn build_channel(
        channel_id: u64,
        sources: impl FnOnce(&std::path::Path) -> Vec<InitSource>,
    ) -> Self {
        let runtime = tempfile::tempdir().unwrap();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, runtime.path())
            .unwrap()
            .unwrap();
        let sources = sources(runtime.path());
        let init = |channel| {
            let (initial_anchor, build_digest, at) = (100, "b".to_string(), Utc::now());
            Ok(Initialized {
                policy_parent: None,
                channel,
                sources: sources.clone(),
                initial_anchor,
                build_digest,
                at,
            })
        };
        store
            .begin_fixture_era(&[channel_id], Utc::now(), init)
            .unwrap();
        let port = Arc::new(FakePort::default());
        let ledger = runtime
            .path()
            .join("o_store")
            .join(channel_id.to_string())
            .join("ledger.jsonl");
        *port.ledger.lock().unwrap() = Some(ledger);
        let (gate, lease) = (
            Arc::new(OwnershipGate::default()),
            Arc::new(FakeLease::default()),
        );
        Self {
            channel_id,
            _runtime: runtime,
            store,
            gate,
            port,
            lease,
            alarms: Alarms {
                channel: channel_id,
                raised: Arc::default(),
                cleared: Arc::default(),
                readers: Arc::default(),
            },
        }
    }

    pub(crate) fn channel(&self) -> ChannelStore {
        let era = self.store.read_era().unwrap().unwrap();
        self.store
            .open_channel(&era, self.channel_id)
            .unwrap()
            .unwrap()
    }

    pub(crate) fn writer(&self) -> Writer {
        let (gate, port) = (Arc::clone(&self.gate), Arc::clone(&self.port));
        ChannelWriter::new(
            self.channel(),
            gate,
            port,
            Arc::clone(&self.lease),
            self.alarms.clone(),
        )
    }
}

pub(crate) fn unit(native_key: &str) -> UnitKey {
    let (provider, kind) = (ShadowProvider::Claude, UnitKind::Body);
    UnitKey {
        channel_id: CHANNEL,
        provider,
        native_key: native_key.into(),
        kind,
    }
}

pub(crate) async fn post_codex_records_for_test(channel: u64, raw: &str) -> (Vec<String>, String) {
    let harness = Harness::build_channel(channel, |_| Vec::new());
    harness.gate.acquired();
    let mut writer = harness.writer();
    let mut deriver =
        UnitDeriver::new(channel, ShadowProvider::Codex).with_turn_summary(pieces::SummaryInputs {
            turn_marks: Default::default(),
            disposed: Default::default(),
            context_window: None,
        });
    let mut start = 0;
    for line in raw.lines() {
        let end = start + line.len() as u64 + 1;
        let record = CapturedRecord {
            start,
            end,
            line: line.as_bytes().to_vec(),
        };
        for item in deriver.derive(&record) {
            assert_eq!(writer.deliver(&item).await, Step::Done);
        }
        start = end;
    }
    let ledger = std::fs::read_to_string(
        harness
            ._runtime
            .path()
            .join("o_store")
            .join(channel.to_string())
            .join("ledger.jsonl"),
    )
    .unwrap();
    (harness.port.posts(), ledger)
}

pub(crate) fn piece(native_key: &str, payload: &str) -> Derived {
    Derived::Piece(PieceWork {
        output_origin: None,
        unit_key: unit(native_key),
        index: 0,
        payload: payload.into(),
        notice_origin: None,
    })
}

pub(crate) fn outcome(writer: &mut Writer, native_key: &str) -> Option<PieceOutcome> {
    let ledger = writer.store().ledger();
    ledger
        .latest_piece(&unit(native_key), 0)
        .and_then(|(_, piece)| piece.outcome.clone())
}

/// The ledger file a channel store appends to.
pub(crate) fn ledger_path(harness: &Harness) -> PathBuf {
    let dir = harness
        ._runtime
        .path()
        .join("o_store")
        .join(CHANNEL.to_string());
    dir.join("ledger.jsonl")
}

pub(crate) fn row(id: &str, text: &str) -> Vec<u8> {
    let row = serde_json::json!({
        "type": "assistant", "uuid": format!("u-{id}"), "apiBlockIndex": 0,
        "message": {"id": id, "content": [{"type": "text", "text": text}]},
    });
    let mut line = serde_json::to_vec(&row).unwrap();
    line.push(b'\n');
    line
}
pub(crate) fn append(path: &Path, bytes: &[u8]) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
}
/// A harness whose channel was switched over a transcript already holding `body`.
pub(crate) fn switched_over(body: &[u8]) -> (Harness, PathBuf, SourceId) {
    switched_over_on(CHANNEL, body)
}
pub(crate) fn switched_over_on(channel: u64, body: &[u8]) -> (Harness, PathBuf, SourceId) {
    let mut bound = None;
    let harness = Harness::build_channel(channel, |runtime| {
        let path = runtime.join("t.jsonl");
        std::fs::write(&path, body).unwrap();
        let source_id = source_id_for("s1", &path).unwrap();
        bound = Some((path, source_id.clone()));
        let (delivery_start, prefix_hash) = (body.len() as u64, hex::encode(Sha256::digest(body)));
        vec![InitSource {
            source_id,
            delivery_start,
            prefix_hash,
        }]
    });
    let (path, source) = bound.unwrap();
    (harness, path, source)
}
pub(crate) async fn polls(count: u32) {
    tokio::time::sleep(POLL_INTERVAL * count).await;
}
/// A spawned actor; it returns why it stopped.
pub(crate) type Actor = tokio::task::JoinHandle<Option<super::deliver::StopCause>>;
pub(crate) fn spawn(writer: Writer) -> (watch::Sender<bool>, Actor) {
    spawn_as(writer, ShadowProvider::Claude)
}
pub(crate) fn spawn_as(
    mut writer: Writer,
    provider: ShadowProvider,
) -> (watch::Sender<bool>, Actor) {
    let bindings = startup_log(&mut writer);
    spawn_with(writer, provider, bindings)
}
pub(crate) fn spawn_with(
    writer: Writer,
    provider: ShadowProvider,
    bindings: Arc<impl BindingEvents>,
) -> (watch::Sender<bool>, Actor) {
    let (stop, stopped) = watch::channel(false);
    let resumed = watch::channel(false).0;
    let task = tokio::spawn(run_channel(writer, provider, bindings, stopped, resumed));
    (stop, task)
}
pub(crate) fn event(seq: u64, record: BindingRecord, committed_at: DateTime<Utc>) -> BindingEvent {
    BindingEvent {
        seq,
        channel_id: CHANNEL,
        provider: ShadowProvider::Claude,
        tmux_session: "tmux".into(),
        execution_nonce: "nonce".into(),
        record,
        committed_at,
    }
}
pub(crate) fn bound(
    seq: u64,
    old: Option<&SourceId>,
    new: BindingTarget,
    cause: BindingCause,
    parent_hint: Option<&SourceId>,
) -> BindingEvent {
    let evidence = BindingEvidence {
        hook_event: HookEventKind::SessionStart.as_str().into(),
        received_at: Utc::now(),
        reclaims: false,
    };
    let record = BindingRecord::Bound {
        old: old.cloned(),
        new,
        cause,
        parent_hint: parent_hint.cloned(),
        evidence,
    };
    event(seq, record, Utc::now())
}
/// A binding log whose startup binds name every source the channel was switched over.
pub(crate) fn startup_log(writer: &mut Writer) -> Arc<FakeBindings> {
    let bindings = Arc::new(FakeBindings::new());
    let sources: Vec<SourceId> = writer.store().cursors().map(|c| c.source.clone()).collect();
    for (seq, source) in (1..).zip(sources) {
        let target = BindingTarget::Source(source);
        bindings.commit(bound(seq, None, target, BindingCause::Startup, None));
    }
    bindings
}
/// An in-memory binding log with the channel's seq notice.
pub(crate) struct FakeBindings {
    pub(crate) events: Mutex<Vec<BindingEvent>>,
    pub(super) notice: watch::Sender<u64>,
    /// Set while every read of the log fails with this error.
    pub(super) failing: Mutex<Option<String>>,
}
impl FakeBindings {
    pub(crate) fn new() -> Self {
        let (events, notice) = (Mutex::new(Vec::new()), watch::channel(0).0);
        let failing = Mutex::new(None);
        Self {
            events,
            notice,
            failing,
        }
    }
    pub(crate) fn fail(&self, error: Option<&str>) {
        *self.failing.lock().unwrap() = error.map(str::to_string);
    }
    /// Commits `event` and moves the notice to its seq.
    pub(crate) fn commit(&self, event: BindingEvent) {
        let seq = event.seq;
        self.events.lock().unwrap().push(event);
        self.notice.send_replace(seq);
    }
}

impl BindingEvents for FakeBindings {
    fn binding_events_since(&self, channel: u64, after: u64) -> Result<Vec<BindingEvent>, String> {
        if let Some(error) = self.failing.lock().unwrap().clone() {
            return Err(error);
        }
        let events = self.events.lock().unwrap();
        let mine = events
            .iter()
            .filter(|e| e.channel_id == channel && e.seq > after);
        Ok(mine.cloned().collect())
    }

    fn subscribe(&self, _channel: u64) -> watch::Receiver<u64> {
        self.notice.subscribe()
    }
}

pub(crate) fn writer_over(harness: &Harness, store: ChannelStore) -> Writer {
    let (gate, port) = (Arc::clone(&harness.gate), Arc::clone(&harness.port));
    let lease = Arc::clone(&harness.lease);
    ChannelWriter::new(store, gate, port, lease, harness.alarms.clone())
}

pub(crate) async fn halt(stop: watch::Sender<bool>, task: Actor) {
    stop.send(true).unwrap();
    task.await.unwrap();
}

pub(crate) const O_CHANNEL: u64 = CHANNEL;

pub(crate) struct ODrive {
    harness: Harness,
    writer: Writer,
    sources: Sources<BindingLog>,
    deriver: UnitDeriver,
    owed: VecDeque<Derived>,
}

impl ODrive {
    pub(crate) fn store(&self) -> &OStore {
        &self.harness.store
    }

    /// A writer on a ready channel's store, which holds a binding checkpoint, here before any
    /// logged record; call it on a thread that sees the test log.
    pub(crate) fn new() -> Self {
        let harness = Harness::new();
        harness.channel().set_binding_checkpoint(0).unwrap();
        harness.gate.acquired();
        let mut drive = Self {
            writer: harness.writer(),
            sources: Sources::new(CHANNEL, ShadowProvider::Codex, Arc::new(BindingLog)),
            deriver: UnitDeriver::new(CHANNEL, ShadowProvider::Codex),
            owed: VecDeque::new(),
            harness,
        };
        let resumed = drive
            .sources
            .resume(&mut drive.writer, &mut drive.deriver, &mut drive.owed);
        resumed.unwrap();
        drive
    }

    /// Every post so far, after following the log and posting what it owes now.
    pub(crate) async fn step(&mut self) -> Vec<String> {
        self.sources.follow(&mut self.writer).unwrap();
        let (writer, deriver, owed) = (&mut self.writer, &mut self.deriver, &mut self.owed);
        self.sources.capture(writer, deriver, owed).unwrap();
        while let Some(item) = self.owed.pop_front() {
            assert_eq!(self.writer.deliver(&item).await, Step::Done);
        }
        self.harness.port.posts()
    }

    /// What a dcserver restart keeps of O: its store. The writer, its sources and what it owes
    /// start again from there.
    pub(crate) fn restart(&mut self) {
        self.writer = self.harness.writer();
        self.sources = Sources::new(CHANNEL, ShadowProvider::Codex, Arc::new(BindingLog));
        self.deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Codex);
        self.owed.clear();
        let (writer, deriver, owed) = (&mut self.writer, &mut self.deriver, &mut self.owed);
        self.sources.resume(writer, deriver, owed).unwrap();
    }

    /// How far O has captured `source`, from its own store.
    pub(crate) fn captured_through(&mut self, source: &SourceId) -> Option<u64> {
        let cursor = self.writer.store().cursor(source);
        cursor.map(|cursor| cursor.captured_through)
    }
}

pub(super) fn captured(value: serde_json::Value) -> CapturedRecord {
    let line = serde_json::to_vec(&value).unwrap();
    CapturedRecord {
        start: 0,
        end: line.len() as u64 + 1,
        line,
    }
}

pub(super) fn timestamp(second: u8) -> String {
    format!("2026-10-19T00:00:{second:02}Z")
}

pub(super) fn prompt(id: &str, second: u8) -> CapturedRecord {
    captured(serde_json::json!({
        "type":"user", "uuid":format!("prompt-{id}"), "timestamp":timestamp(second),
        "message":{"content":"request"},
    }))
}

pub(super) fn body(id: &str, second: u8) -> CapturedRecord {
    captured(serde_json::json!({
        "type":"assistant", "uuid":format!("uuid-{id}"), "apiBlockIndex":0,
        "timestamp":timestamp(second), "message":{"id":id, "usage":{"input_tokens":100},
            "content":[{"type":"text", "text":format!("answer {id}")}]},
    }))
}

pub(super) fn idle(second: u8) -> CapturedRecord {
    captured(serde_json::json!({
        "type":"system", "subtype":"stop_hook_summary", "timestamp":timestamp(second),
    }))
}

pub(super) fn records(id: &str, second: u8) -> [CapturedRecord; 3] {
    [prompt(id, second), body(id, second + 1), idle(second + 10)]
}

pub(super) async fn deliver_all(writer: &mut Writer, items: &[Derived]) {
    for item in items {
        assert_eq!(writer.deliver(item).await, Step::Done, "{item:?}");
    }
}

pub(crate) struct FooterWriter {
    harness: Harness,
    writer: Writer,
}

impl FooterWriter {
    pub(crate) fn new(channel: u64, pool: sqlx::PgPool) -> Self {
        let harness = Harness::build_channel(channel, |_| Vec::new());
        harness.gate.acquired();
        harness.port.usage_footer_pool.set(pool).unwrap();
        let writer = harness.writer();
        Self { harness, writer }
    }

    pub(crate) fn root(&self) -> &std::path::Path {
        self.harness._runtime.path()
    }

    pub(crate) fn restart(&mut self) {
        self.writer = self.harness.writer();
    }

    pub(crate) async fn turn(&mut self, id: &str) -> String {
        use crate::services::claude_tui::hook_bundle::statusline::{record_dir, record_path};

        let session = uuid::Uuid::new_v4().to_string();
        let now = Utc::now();
        let root = record_dir();
        std::fs::create_dir_all(&root).unwrap();
        let path = record_path(&root, &session);
        std::fs::write(
            &path,
            serde_json::json!({"session_id":session, "rate_limits":{
                "five_hour":{"used_percentage":24,"resets_at": now.timestamp()+18000}
            }})
            .to_string(),
        )
        .unwrap();
        let ledger = self.writer.store().ledger();
        let mut derive = UnitDeriver::new(self.harness.channel_id, ShadowProvider::Claude)
            .with_turn_summary(pieces::SummaryInputs {
                turn_marks: ledger.summary_turn_marks(),
                disposed: ledger.disposed_unit_hashes(),
                context_window: None,
            });
        let [prompt, body, terminal] = records(id, 0).map(|record| {
            let mut value: serde_json::Value = serde_json::from_slice(&record.line).unwrap();
            value["sessionId"] = serde_json::json!(session);
            captured(value)
        });
        let before = self.harness.port.posts().len();
        for record in [prompt, body, terminal] {
            deliver_all(&mut self.writer, &derive.derive(&record)).await;
        }
        let posts = self.harness.port.posts();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(posts.len(), before + 2, "{posts:?}");
        assert_eq!(posts[before], format!("answer {id}"));
        posts[before + 1].clone()
    }
}
