use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use chrono::Utc;

use super::confirm::{self, Verdict};
use super::deliver::{ChannelWriter, Step};
use super::pieces::{Derived, PieceWork, UnitDeriver};
use super::*;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::{CapturedRecord, ShadowProvider, UnitKey, UnitKind};
use crate::services::tui_o::store::ledger::{LedgerEntry, PieceOutcome};
use crate::services::tui_o::store::{ChannelStore, Initialized, OStore, StoreConfig};

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
}

#[derive(Default)]
struct FakePort {
    history: Mutex<Vec<SeenMessage>>,
    replies: Mutex<VecDeque<Reply>>,
    posts: Mutex<Vec<String>>,
    prepared_before_post: Mutex<Vec<bool>>,
    ledger: Mutex<Option<PathBuf>>,
    unreadable: AtomicBool,
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
        };
        async move { outcome }
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
        let runtime = tempfile::tempdir().unwrap();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, runtime.path())
            .unwrap()
            .unwrap();
        let init = |channel| {
            let (initial_anchor, build_digest, at) = (100, "b".to_string(), Utc::now());
            Ok(Initialized {
                channel,
                sources: Vec::new(),
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
