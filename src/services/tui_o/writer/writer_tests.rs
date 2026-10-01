use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chrono::Utc;

use super::confirm::{self, Verdict};
use super::deliver::{self, ChannelWriter, Step};
use super::pieces::{self, Derived, PieceWork, UnitDeriver};
use super::*;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::{CapturedRecord, ShadowProvider, UnitKey, UnitKind};
use crate::services::tui_o::store::ledger::{LedgerEntry, PieceOutcome};
use crate::services::tui_o::store::{ChannelStore, InitSource, Initialized, OStore, StoreConfig};

const BOT: u64 = 42;
const CHANNEL: u64 = 7;

#[derive(Clone, Copy)]
enum Reply {
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
}

#[derive(Default)]
struct FakePort {
    history: Mutex<Vec<SeenMessage>>,
    replies: Mutex<VecDeque<Reply>>,
    posts: Mutex<Vec<String>>,
    prepared_before_post: Mutex<Vec<bool>>,
    ledger: Mutex<Option<PathBuf>>,
    unreadable: AtomicBool,
    /// When set, a POST's HTTP request starts at the future's first poll, like the real client.
    lazy: AtomicBool,
    /// Each lazy request logs `http` when it starts.
    started: Arc<Mutex<Vec<&'static str>>>,
    /// A lazy request waits on this before it completes.
    hold: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    on_post: Mutex<Option<Box<dyn Fn() + Send>>>,
}

impl FakePort {
    fn say(&self, author_id: u64, content: &str) -> SeenMessage {
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

    fn posts(&self) -> Vec<String> {
        self.posts.lock().unwrap().clone()
    }
}

impl DiscordPort for FakePort {
    fn bot_id(&self) -> u64 {
        BOT
    }

    fn post(
        &self,
        _channel: u64,
        content: String,
    ) -> impl Future<Output = PostOutcome> + Send + 'static {
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
        };
        if let Some(hook) = self.on_post.lock().unwrap().as_ref() {
            hook();
        }
        let lazy = self.lazy.load(Ordering::SeqCst).then(|| {
            let hold = self.hold.lock().unwrap().take();
            (Arc::clone(&self.started), hold)
        });
        async move {
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
        let history = self.history.lock().unwrap();
        let page: Vec<SeenMessage> = history
            .iter()
            .filter(|m| m.id > after)
            .take(confirm::HISTORY_PAGE)
            .cloned()
            .collect();
        async move { Ok(page) }
    }

    fn history_readable(&self, _channel: u64) -> bool {
        !self.unreadable.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
struct FakeLease {
    busy: AtomicBool,
    released: Arc<AtomicUsize>,
    on_acquire: Mutex<Option<Box<dyn Fn() + Send>>>,
}

struct Held(Arc<AtomicUsize>);

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
}

#[derive(Clone, Default)]
struct Alarms(Arc<Mutex<Vec<WriterAlarm>>>);

impl AlarmSink for Alarms {
    fn raise(&self, channel: u64, alarm: WriterAlarm) {
        assert_eq!(channel, CHANNEL);
        self.0.lock().unwrap().push(alarm);
    }
}

impl Alarms {
    fn taken(&self) -> Vec<WriterAlarm> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

type Writer = ChannelWriter<FakePort, Arc<FakeLease>, Alarms>;

struct Harness {
    _runtime: tempfile::TempDir,
    store: OStore,
    gate: Arc<OwnershipGate>,
    port: Arc<FakePort>,
    lease: Arc<FakeLease>,
    alarms: Alarms,
}

impl Harness {
    fn new() -> Self {
        Self::build(|_| Vec::new())
    }

    /// `sources` may create transcripts under the runtime root before the era begins.
    fn build(sources: impl FnOnce(&std::path::Path) -> Vec<InitSource>) -> Self {
        let runtime = tempfile::tempdir().unwrap();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, runtime.path())
            .unwrap()
            .unwrap();
        let sources = sources(runtime.path());
        let init = |channel| {
            let (initial_anchor, build_digest, at) = (100, "b".to_string(), Utc::now());
            Ok(Initialized {
                channel,
                sources: sources.clone(),
                initial_anchor,
                build_digest,
                at,
            })
        };
        store.begin_era(&[CHANNEL], Utc::now(), init).unwrap();
        let port = Arc::new(FakePort::default());
        let ledger = runtime
            .path()
            .join("o_store")
            .join(CHANNEL.to_string())
            .join("ledger.jsonl");
        *port.ledger.lock().unwrap() = Some(ledger);
        let (gate, lease) = (
            Arc::new(OwnershipGate::default()),
            Arc::new(FakeLease::default()),
        );
        Self {
            _runtime: runtime,
            store,
            gate,
            port,
            lease,
            alarms: Alarms::default(),
        }
    }

    fn channel(&self) -> ChannelStore {
        let era = self.store.read_era().unwrap().unwrap();
        self.store.open_channel(&era, CHANNEL).unwrap().unwrap()
    }

    fn writer(&self) -> Writer {
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

fn unit(native_key: &str) -> UnitKey {
    let (provider, kind) = (ShadowProvider::Claude, UnitKind::Body);
    UnitKey {
        channel_id: CHANNEL,
        provider,
        native_key: native_key.into(),
        kind,
    }
}

fn piece(native_key: &str, payload: &str) -> Derived {
    Derived::Piece(PieceWork {
        unit_key: unit(native_key),
        index: 0,
        payload: payload.into(),
    })
}

fn outcome(writer: &mut Writer, native_key: &str) -> Option<PieceOutcome> {
    let ledger = writer.store().ledger();
    ledger
        .latest_piece(&unit(native_key), 0)
        .and_then(|(_, piece)| piece.outcome.clone())
}

#[tokio::test(start_paused = true)]
async fn a_piece_is_prepared_under_ownership_then_posted_once() {
    let harness = Harness::new();
    let epoch = harness.gate.acquired();
    let mut writer = harness.writer();
    assert_eq!(writer.deliver(&piece("m1", "hello")).await, Step::Done);
    assert_eq!(writer.deliver(&piece("m1", "hello")).await, Step::Done);
    assert_eq!(harness.port.posts(), ["hello"]);
    assert_eq!(*harness.port.prepared_before_post.lock().unwrap(), [true]);
    let ledger = writer.store().ledger();
    assert_eq!(
        (ledger.anchor(), ledger.piece(0).map(|piece| piece.epoch)),
        (1000, Some(epoch))
    );
    assert_eq!(outcome(&mut writer, "m1"), Some(PieceOutcome::Posted(1000)));
    assert_eq!(harness.lease.released.load(Ordering::SeqCst), 1);
    assert_eq!(harness.alarms.taken(), []);
}

#[tokio::test(start_paused = true)]
async fn nothing_is_prepared_or_posted_without_ownership_and_the_delivery_lease() {
    let harness = Harness::new();
    let mut writer = harness.writer();
    assert_eq!(writer.deliver(&piece("m1", "a")).await, Step::NoGateway);
    assert_eq!(writer.deliver(&piece("m1", "a")).await, Step::NoGateway);
    assert_eq!(harness.alarms.taken(), [WriterAlarm::PausedNoGateway]);
    harness.gate.acquired();
    harness.lease.busy.store(true, Ordering::SeqCst);
    assert_eq!(writer.deliver(&piece("m1", "a")).await, Step::LeaseBusy);
    harness.lease.busy.store(false, Ordering::SeqCst);
    // Ownership lost after the lease check and before admission: the gate refuses the hand-off.
    let gate = Arc::clone(&harness.gate);
    *harness.lease.on_acquire.lock().unwrap() = Some(Box::new(move || gate.uncertain()));
    assert_eq!(writer.deliver(&piece("m1", "a")).await, Step::NoGateway);
    *harness.lease.on_acquire.lock().unwrap() = None;
    assert_eq!(writer.store().ledger().next_serial(), 0);
    assert!(harness.port.posts().is_empty());
    harness.gate.acquired();
    assert_eq!(writer.deliver(&piece("m1", "a")).await, Step::Done);
    assert_eq!(harness.port.posts(), ["a"]);
}

#[tokio::test(start_paused = true)]
async fn an_unclear_post_is_settled_from_history_and_never_posted_again() {
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .extend([Reply::CreatedUnseen, Reply::Unsent]);
    assert_eq!(writer.deliver(&piece("m1", "first")).await, Step::Done);
    assert_eq!(outcome(&mut writer, "m1"), Some(PieceOutcome::Posted(1000)));
    let started = tokio::time::Instant::now();
    assert_eq!(writer.deliver(&piece("m2", "second")).await, Step::Done);
    assert!(started.elapsed() >= confirm::LAST_LOOK);
    assert_eq!(outcome(&mut writer, "m2"), Some(PieceOutcome::NotFound));
    assert_eq!(writer.deliver(&piece("m2", "second")).await, Step::Done);
    assert_eq!(harness.port.posts(), ["first", "second"]);
    assert_eq!(writer.store().ledger().anchor(), 1000);
    assert_eq!(
        harness.alarms.taken(),
        [WriterAlarm::NotFound { serial: 1 }]
    );
}

#[tokio::test(start_paused = true)]
async fn a_created_reply_by_another_author_is_settled_from_history_rather_than_trusted() {
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    let replies = [Reply::CreatedByOther];
    harness.port.replies.lock().unwrap().extend(replies);
    assert_eq!(writer.deliver(&piece("m1", "first")).await, Step::Done);
    assert_eq!(outcome(&mut writer, "m1"), Some(PieceOutcome::NotFound));
    assert_eq!(harness.port.posts(), ["first"]);
}

#[tokio::test(start_paused = true)]
async fn settlement_stays_ambiguous_or_unresolved_when_history_cannot_single_out_the_post() {
    let port = FakePort::default();
    port.say(BOT, "same");
    port.say(BOT, "same");
    assert_eq!(
        confirm::settle(&port, CHANNEL, 0, "same", false).await,
        Verdict::Ambiguous(vec![1000, 1001])
    );
    assert_eq!(
        confirm::settle(&port, CHANNEL, 1000, "same", true).await,
        Verdict::Ambiguous(vec![1001])
    );
    assert_eq!(
        confirm::settle(&port, CHANNEL, 1000, "same", false).await,
        Verdict::Posted(1001)
    );
    assert_eq!(
        confirm::settle(&port, CHANNEL, 1001, "same", false).await,
        Verdict::NotFound
    );
    port.unreadable.store(true, Ordering::SeqCst);
    assert!(matches!(
        confirm::settle(&port, CHANNEL, 1001, "same", false).await,
        Verdict::Unresolved(_)
    ));
    port.say(BOT + 1, "someone else");
    assert!(
        matches!(
            confirm::settle(&port, CHANNEL, 1001, "same", false).await,
            Verdict::Unresolved(_)
        ),
        "another author's message is no proof that history is readable"
    );
    port.unreadable.store(false, Ordering::SeqCst);
    let pages = confirm::HISTORY_PAGE * confirm::MAX_PAGES;
    for _ in 0..pages {
        port.say(7, "chatter");
    }
    assert!(matches!(
        confirm::settle(&port, CHANNEL, 0, "same", false).await,
        Verdict::Unresolved(_)
    ));
    assert_eq!(confirm::judge(&[5], false), Some(Verdict::Posted(5)));
}

#[tokio::test(start_paused = true)]
async fn an_unclear_post_sharing_an_earlier_unsettled_payload_stays_ambiguous() {
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .extend([Reply::Unsent, Reply::CreatedUnseen]);
    writer.deliver(&piece("m1", "ok")).await;
    writer.deliver(&piece("m2", "ok")).await;
    assert_eq!(outcome(&mut writer, "m1"), Some(PieceOutcome::NotFound));
    assert_eq!(
        outcome(&mut writer, "m2"),
        Some(PieceOutcome::Ambiguous(vec![1000]))
    );
    assert_eq!(writer.store().ledger().anchor(), 100);
}

#[tokio::test(start_paused = true)]
async fn a_refused_post_blocks_the_channel_even_after_reopening() {
    let harness = Harness::new();
    harness.gate.acquired();
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::Refused(403));
    let mut writer = harness.writer();
    assert_eq!(writer.deliver(&piece("m1", "x")).await, Step::Stopped);
    assert_eq!(writer.deliver(&piece("m2", "y")).await, Step::Stopped);
    assert_eq!(
        harness.alarms.taken(),
        [WriterAlarm::Blocked { status: 403 }]
    );
    let mut reopened = harness.writer();
    assert!(reopened.is_stopped());
    assert_eq!(reopened.deliver(&piece("m2", "y")).await, Step::Stopped);
    assert_eq!(harness.port.posts(), ["x"]);
}

#[tokio::test(start_paused = true)]
async fn a_prepared_left_by_a_crash_is_settled_before_the_next_post() {
    let harness = Harness::new();
    harness.gate.acquired();
    let mut channel = harness.channel();
    for (serial, payload) in [(0, "never sent"), (1, "was sent")] {
        let (unit_key, payload) = (unit(&format!("c{serial}")), payload.to_string());
        let anchor_id = channel.ledger().anchor();
        let prepared = LedgerEntry::Prepared {
            serial,
            unit_key,
            piece_index: 0,
            payload,
            anchor_id,
            epoch: 1,
        };
        channel.append_ledger(prepared).unwrap();
        if serial == 0 {
            channel
                .append_ledger(LedgerEntry::NotFound { serial })
                .unwrap();
        }
    }
    harness.port.say(BOT, "was sent");
    let mut writer = harness.writer();
    assert_eq!(writer.deliver(&piece("m1", "next")).await, Step::Done);
    assert_eq!(outcome(&mut writer, "c1"), Some(PieceOutcome::Posted(1000)));
    assert_eq!(outcome(&mut writer, "m1"), Some(PieceOutcome::Posted(1001)));
    assert_eq!(harness.port.posts(), ["next"]);
}

#[tokio::test(start_paused = true)]
async fn a_blocked_record_or_a_ledger_violation_stops_the_channel_before_any_post() {
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    let blocked = Derived::Blocked {
        reason: "unsupported block".into(),
    };
    assert_eq!(writer.deliver(&blocked).await, Step::Stopped);
    assert_eq!(writer.deliver(&piece("m1", "x")).await, Step::Stopped);
    let mut channel = harness.channel();
    channel
        .append_ledger(LedgerEntry::Posted {
            serial: 9,
            msg_id: 5,
        })
        .unwrap();
    let mut reopened = harness.writer();
    assert_eq!(reopened.deliver(&piece("m1", "x")).await, Step::Stopped);
    let alarms = harness.alarms.taken();
    assert!(matches!(
        alarms.as_slice(),
        [
            WriterAlarm::SchemaBlocked { .. },
            WriterAlarm::LedgerViolation { .. }
        ]
    ));
    assert!(harness.port.posts().is_empty());
}

#[test]
fn derivation_splits_each_unit_once_and_excludes_or_blocks_the_rest() {
    let row = |id: &str, text: &str| {
        let row = serde_json::json!({
            "type": "assistant", "uuid": format!("u-{id}"), "apiBlockIndex": 0,
            "message": {"id": id, "content": [{"type": "text", "text": text}]},
        });
        let line = serde_json::to_vec(&row).unwrap();
        CapturedRecord {
            start: 0,
            end: line.len() as u64 + 1,
            line,
        }
    };
    let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    let long = "word ".repeat(900);
    let pieces = deriver.derive(&row("msg_1", &long));
    let expected = crate::services::discord::formatting::split_for_shadow(long.trim());
    assert!(expected.len() >= 2);
    let payloads: Vec<String> = pieces
        .iter()
        .map(|item| match item {
            Derived::Piece(piece) => piece.payload.clone(),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(
        payloads,
        expected
            .into_iter()
            .map(|(text, _)| text)
            .collect::<Vec<_>>()
    );
    assert!(
        deriver.derive(&row("msg_1", &long)).is_empty(),
        "a fork copy is not owed twice"
    );
    assert!(matches!(
        deriver.derive(&row("msg_1", "changed")).as_slice(),
        [Derived::Blocked { .. }]
    ));
    let result = serde_json::json!({"type": "user", "message": {"content": [
        {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]}});
    let line = serde_json::to_vec(&result).unwrap();
    let record = CapturedRecord {
        start: 0,
        end: line.len() as u64 + 1,
        line,
    };
    assert!(matches!(
        deriver.derive(&record).as_slice(),
        [Derived::Excluded { .. }]
    ));
    let torn = CapturedRecord {
        start: 0,
        end: 3,
        line: b"{\"t".to_vec(),
    };
    assert!(matches!(
        deriver.derive(&torn).as_slice(),
        [Derived::Blocked { .. }]
    ));
}

/// A tool call is recorded excluded for the live panel and never posted; the body after it posts
/// and becomes the channel's newest O post.
#[tokio::test(start_paused = true)]
async fn a_tool_call_is_left_to_the_panel_and_only_the_body_posts() {
    let record = |row: serde_json::Value| {
        let line = serde_json::to_vec(&row).unwrap();
        let end = line.len() as u64 + 1;
        CapturedRecord {
            start: 0,
            end,
            line,
        }
    };
    let assistant = |id: &str, block: serde_json::Value| {
        record(
            serde_json::json!({"type": "assistant", "uuid": format!("u-{id}"),
            "apiBlockIndex": 0, "message": {"id": id, "content": [block]}}),
        )
    };
    let tool_use = serde_json::json!({"type": "tool_use", "id": "toolu_1", "name": "Bash",
        "input": {"command": "ls"}});
    let codex_call = record(serde_json::json!({"type": "response_item", "payload": {
        "type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "shell",
        "arguments": "{}"}}));
    let mut codex = UnitDeriver::new(CHANNEL, ShadowProvider::Codex);
    let excluded = |items: &[Derived]| {
        matches!(items, [Derived::Excluded { unit_key, reason }]
            if unit_key.kind == UnitKind::Tool && reason == pieces::TOOL_CALL_PANEL)
    };
    assert!(excluded(&codex.derive(&codex_call)));

    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    let tool = deriver.derive(&assistant("msg_t", tool_use));
    assert!(excluded(&tool), "{tool:?}");
    let body_block = serde_json::json!({"type": "text", "text": "done"});
    let items = [tool, deriver.derive(&assistant("msg_b", body_block))].concat();
    for item in &items {
        assert_eq!(writer.deliver(item).await, Step::Done);
    }
    assert_eq!(harness.port.posts(), ["done"]);
    let tool_key = UnitKey {
        native_key: "msg_t:0".into(),
        kind: UnitKind::Tool,
        ..unit("")
    };
    let ledger = writer.store().ledger();
    assert_eq!(ledger.excluded(&tool_key), Some(pieces::TOOL_CALL_PANEL));
    let Some(PieceOutcome::Posted(posted)) = outcome(&mut writer, "msg_b:0") else {
        panic!("body not posted");
    };
    assert!(deliver::last_posted(CHANNEL) >= Some(posted));
}

/// A writer opened after a restart learns O's newest post from the recovered ledger.
#[tokio::test(start_paused = true)]
async fn a_restarted_writer_recovers_the_newest_post_from_its_ledger() {
    let harness = Harness::new();
    harness.gate.acquired();
    let mut writer = harness.writer();
    assert_eq!(writer.deliver(&piece("m1", "hello")).await, Step::Done);
    let Some(PieceOutcome::Posted(posted)) = outcome(&mut writer, "m1") else {
        panic!("piece not posted");
    };
    drop(writer);
    deliver::forget_posted_for_tests(CHANNEL);
    let _restarted = harness.writer();
    assert!(deliver::last_posted(CHANNEL) >= Some(posted));
}

/// Guards that no POST's HTTP request starts after ownership closes, for Unknown and Lost alike.
#[tokio::test(start_paused = true)]
async fn no_post_starts_its_request_after_ownership_closes() {
    let closes: [fn(&OwnershipGate); 2] = [OwnershipGate::uncertain, OwnershipGate::lost];
    for close in closes {
        let harness = Harness::new();
        harness.gate.acquired();
        harness.port.lazy.store(true, Ordering::SeqCst);
        let log = Arc::clone(&harness.port.started);
        let (wake, woken) = tokio::sync::oneshot::channel::<()>();
        let (gate, closed_log) = (Arc::clone(&harness.gate), Arc::clone(&log));
        // Runs as soon as the writer yields after handing the POST over.
        let closer = tokio::spawn(async move {
            woken.await.unwrap();
            close(&gate);
            closed_log.lock().unwrap().push("closed");
        });
        tokio::task::yield_now().await;
        let wake = Mutex::new(Some(wake));
        *harness.port.on_post.lock().unwrap() = Some(Box::new(move || {
            wake.lock().unwrap().take().map(|wake| wake.send(()));
        }));
        let mut writer = harness.writer();
        writer.deliver(&piece("m1", "a")).await;
        closer.await.unwrap();
        assert_eq!(*log.lock().unwrap(), ["http", "closed"]);
    }
}

/// Guards that the delivery lease outlives an aborted writer until its POST can no longer send,
/// and that a timed-out POST hands the lease back exactly once.
#[tokio::test(start_paused = true)]
async fn the_delivery_lease_is_held_until_the_post_task_ends() {
    let harness = Harness::new();
    harness.gate.acquired();
    harness.port.lazy.store(true, Ordering::SeqCst);
    let (release, hold) = tokio::sync::oneshot::channel();
    *harness.port.hold.lock().unwrap() = Some(hold);
    let mut writer = harness.writer();
    let parent = tokio::spawn(async move { writer.deliver(&piece("m1", "a")).await });
    while harness.port.started.lock().unwrap().is_empty() {
        tokio::task::yield_now().await;
    }
    parent.abort();
    assert!(parent.await.unwrap_err().is_cancelled());
    let released = || harness.lease.released.load(Ordering::SeqCst);
    assert_eq!(
        released(),
        0,
        "lease released while the POST could still send"
    );
    release.send(()).unwrap();
    while released() == 0 {
        tokio::task::yield_now().await;
    }
    let (_never, hold) = tokio::sync::oneshot::channel();
    *harness.port.hold.lock().unwrap() = Some(hold);
    let mut writer = harness.writer();
    assert_eq!(writer.deliver(&piece("m2", "b")).await, Step::Done);
    assert_eq!(released(), 2);
    assert_eq!(harness.port.posts(), ["a", "b"]);
}

/// Guards that a ledger violation raised while running stops the same writer before its next POST.
#[tokio::test(start_paused = true)]
async fn a_violation_recorded_while_running_stops_the_writer_before_the_next_post() {
    let harness = Harness::new();
    harness.gate.acquired();
    let replies = [Reply::CreatedBehind];
    harness.port.replies.lock().unwrap().extend(replies);
    let mut writer = harness.writer();
    assert_eq!(writer.deliver(&piece("m1", "a")).await, Step::Stopped);
    assert_eq!(writer.deliver(&piece("m2", "b")).await, Step::Stopped);
    assert_eq!(harness.port.posts(), ["a"]);
    assert!(matches!(
        harness.alarms.taken().as_slice(),
        [WriterAlarm::LedgerViolation { .. }]
    ));
}

#[path = "actor_tests.rs"]
mod actor;

#[tokio::test(start_paused = true)]
async fn a_gateway_return_ends_the_pause_in_process_health() {
    use crate::services::tui_o::alarm::{AlarmRouter, health_reasons};
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::a_gateway_return_ends_the_pause_in_process_health"
    )) {
        return;
    }
    let harness = Harness::new();
    let (gate, port) = (Arc::clone(&harness.gate), Arc::clone(&harness.port));
    let lease = Arc::clone(&harness.lease);
    let router = AlarmRouter::for_process(None, None);
    let mut writer = ChannelWriter::new(harness.channel(), gate, port, lease, router);
    let paused = [format!("tui_o:paused_no_gateway:{CHANNEL}")];
    assert_eq!(writer.deliver(&piece("m1", "a")).await, Step::NoGateway);
    assert_eq!(health_reasons(), paused);
    harness.gate.acquired();
    assert_eq!(writer.deliver(&piece("m1", "a")).await, Step::Done);
    assert!(health_reasons().is_empty(), "{:?}", health_reasons());
    harness.gate.uncertain();
    assert_eq!(writer.deliver(&piece("m2", "b")).await, Step::NoGateway);
    assert_eq!(health_reasons(), paused);
}
