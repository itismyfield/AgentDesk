use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use super::super::actor::{POLL_INTERVAL, run_channel, spawn_if_enabled};
use super::super::binding::{
    BindingCause, BindingEvent, BindingEvents, BindingEvidence, BindingRecord, BindingTarget,
};
use super::super::round_trip::{RoundTrip, cases, round_trip};
use super::*;
use crate::services::claude_tui::hook_server::HookEventKind;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::shadow::capture::SourceCapture;
use crate::services::tui_o::shadow::{CaptureOutcome, CaptureSource, MAX_READ_BYTES, SourceId};

fn row(id: &str, text: &str) -> Vec<u8> {
    let row = serde_json::json!({
        "type": "assistant", "uuid": format!("u-{id}"), "apiBlockIndex": 0,
        "message": {"id": id, "content": [{"type": "text", "text": text}]},
    });
    let mut line = serde_json::to_vec(&row).unwrap();
    line.push(b'\n');
    line
}

fn append(path: &Path, bytes: &[u8]) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
}

/// A harness whose channel was switched over a transcript already holding `body`.
fn switched_over(body: &[u8]) -> (Harness, PathBuf, SourceId) {
    let mut bound = None;
    let harness = Harness::build(|runtime| {
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

async fn polls(count: u32) {
    tokio::time::sleep(POLL_INTERVAL * count).await;
}

fn spawn(writer: Writer) -> (watch::Sender<bool>, tokio::task::JoinHandle<()>) {
    spawn_as(writer, ShadowProvider::Claude)
}

fn spawn_as(
    mut writer: Writer,
    provider: ShadowProvider,
) -> (watch::Sender<bool>, tokio::task::JoinHandle<()>) {
    let bindings = startup_log(&mut writer);
    spawn_with(writer, provider, bindings)
}

fn spawn_with(
    writer: Writer,
    provider: ShadowProvider,
    bindings: Arc<impl BindingEvents>,
) -> (watch::Sender<bool>, tokio::task::JoinHandle<()>) {
    let (stop, stopped) = watch::channel(false);
    let resumed = watch::channel(false).0;
    let task = tokio::spawn(run_channel(writer, provider, bindings, stopped, resumed));
    (stop, task)
}

fn event(seq: u64, record: BindingRecord, committed_at: DateTime<Utc>) -> BindingEvent {
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

fn bound(
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
fn startup_log(writer: &mut Writer) -> Arc<FakeBindings> {
    let bindings = Arc::new(FakeBindings::new());
    let sources: Vec<SourceId> = writer.store().cursors().map(|c| c.source.clone()).collect();
    for (seq, source) in (1..).zip(sources) {
        let target = BindingTarget::Source(source);
        bindings.commit(bound(seq, None, target, BindingCause::Startup, None));
    }
    bindings
}

/// An in-memory binding log with the channel's seq notice.
struct FakeBindings {
    events: Mutex<Vec<BindingEvent>>,
    notice: watch::Sender<u64>,
    /// Set while every read of the log fails with this error.
    failing: Mutex<Option<String>>,
}

impl FakeBindings {
    fn new() -> Self {
        let (events, notice) = (Mutex::new(Vec::new()), watch::channel(0).0);
        let failing = Mutex::new(None);
        Self {
            events,
            notice,
            failing,
        }
    }

    fn fail(&self, error: Option<&str>) {
        *self.failing.lock().unwrap() = error.map(str::to_string);
    }

    /// Commits `event` and moves the notice to its seq.
    fn commit(&self, event: BindingEvent) {
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

fn writer_over(harness: &Harness, store: ChannelStore) -> Writer {
    let (gate, port) = (Arc::clone(&harness.gate), Arc::clone(&harness.port));
    let lease = Arc::clone(&harness.lease);
    ChannelWriter::new(store, gate, port, lease, harness.alarms.clone())
}

async fn halt(stop: watch::Sender<bool>, task: tokio::task::JoinHandle<()>) {
    stop.send(true).unwrap();
    task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn the_actor_posts_each_unit_after_the_switch_once_even_across_a_restart() {
    let (harness, path, _) = switched_over(&row("m0", "before the switch"));
    harness.gate.acquired();
    let (stop, task) = spawn(harness.writer());
    append(&path, &row("m1", "first"));
    polls(3).await;
    append(&path, &row("m2", "second"));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["first", "second"]);
    halt(stop, task).await;
    let (stop, task) = spawn(harness.writer());
    append(&path, &row("m3", "third"));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["first", "second", "third"]);
    assert_eq!(harness.alarms.taken(), []);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn without_ownership_the_actor_keeps_spooling_and_posts_once_ownership_returns() {
    let (harness, path, source) = switched_over(&row("m0", "before the switch"));
    let (stop, task) = spawn(harness.writer());
    append(&path, &row("m1", "first"));
    polls(3).await;
    assert!(harness.port.posts().is_empty());
    let spooled = harness.channel().cursor(&source).unwrap().captured_through;
    assert_eq!(spooled, std::fs::metadata(&path).unwrap().len());
    assert_eq!(harness.alarms.taken(), [WriterAlarm::PausedNoGateway]);
    harness.gate.acquired();
    polls(3).await;
    assert_eq!(harness.port.posts(), ["first"]);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn a_stop_an_abort_or_eof_settles_nothing_the_channel_still_owes() {
    let codex = |value: serde_json::Value| {
        let mut line = serde_json::to_vec(&value).unwrap();
        line.push(b'\n');
        line
    };
    let message = |id: &str, text: &str| {
        codex(serde_json::json!({"type": "response_item", "payload": {
            "type": "message", "role": "assistant", "id": id,
            "content": [{"type": "output_text", "text": text}]}}))
    };
    let turn = |kind: &str, id: &str| {
        codex(serde_json::json!({"type": "event_msg", "payload": {"type": kind, "turn_id": id}}))
    };
    let (harness, path, source) = switched_over(b"");
    let (stop, task) = spawn_as(harness.writer(), ShadowProvider::Codex);
    for line in [
        turn("task_started", "t1"),
        message("msg_1", "first"),
        turn("task_complete", "t1"),
        turn("task_started", "t2"),
        message("msg_2", "second"),
        turn("turn_aborted", "t2"),
    ] {
        append(&path, &line);
    }
    polls(6).await;
    // The reader sits at EOF past both turn ends while no gateway can post.
    let spooled = harness.channel().cursor(&source).unwrap().captured_through;
    assert_eq!(spooled, std::fs::metadata(&path).unwrap().len());
    assert!(harness.port.posts().is_empty());
    assert_eq!(harness.channel().ledger().next_serial(), 0);
    assert_eq!(harness.alarms.taken(), [WriterAlarm::PausedNoGateway]);
    harness.gate.acquired();
    polls(3).await;
    assert_eq!(
        harness.port.posts(),
        ["first", "second"],
        "a Stop, an abort or EOF is not a delivery"
    );
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn a_full_spool_pauses_capture_until_delivered_segments_are_collected() {
    let body = row("m0", "before the switch");
    let (harness, path, source) = switched_over(&body);
    let long = "x".repeat(1500);
    append(&path, &row("m1", &long));
    let mut store = harness.channel();
    let mut capture = SourceCapture::open(source.clone(), body.len() as u64).unwrap();
    let CaptureOutcome::Batch(batch) = capture.poll(MAX_READ_BYTES) else {
        panic!("capture failed");
    };
    store.append_spool(&batch, &capture.prefix_hash()).unwrap();
    let used = store.spool_bytes();
    store.set_limits_for_test(1, used);
    append(&path, &row("m2", "two"));
    append(&path, &row("m3", "six"));
    let (stop, task) = spawn(writer_over(&harness, store));
    polls(3).await;
    assert!(harness.port.posts().is_empty());
    let paused = [WriterAlarm::PausedNoGateway, WriterAlarm::SpoolFull];
    assert_eq!(harness.alarms.taken(), paused);
    harness.gate.acquired();
    polls(3).await;
    assert_eq!(harness.port.posts(), [long.as_str(), "two", "six"]);
    assert_eq!(harness.alarms.taken(), []);
    halt(stop, task).await;
    let reopened = harness.channel();
    assert_eq!(reopened.ledger().gc_segments(&source).len(), 1);
    assert_eq!(reopened.retained_segments(&source), 1);
    let (stop, task) = spawn(harness.writer());
    polls(3).await;
    assert_eq!(harness.port.posts().len(), 3, "a restart reposts nothing");
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn an_announced_unit_keeps_its_segment_until_it_is_sealed_and_posted() {
    let codex = |value: serde_json::Value| {
        let mut line = serde_json::to_vec(&value).unwrap();
        line.push(b'\n');
        line
    };
    let message = |id: &str, text: &str| {
        codex(serde_json::json!({"type": "response_item", "payload": {
            "type": "message", "role": "assistant", "id": id,
            "content": [{"type": "output_text", "text": text}]}}))
    };
    let (harness, path, source) = switched_over(b"");
    let mut store = harness.channel();
    store.set_limits_for_test(1, u64::MAX);
    harness.gate.acquired();
    let (stop, task) = spawn_as(writer_over(&harness, store), ShadowProvider::Codex);
    append(
        &path,
        &codex(serde_json::json!({"type": "event_msg", "payload": {
        "type": "item_completed", "item": {"type": "AgentMessage", "id": "msg_c1"}}})),
    );
    polls(3).await;
    append(&path, &message("msg_x", "x"));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["x"]);
    assert_eq!(harness.channel().retained_segments(&source), 2);
    assert!(harness.channel().ledger().gc_segments(&source).is_empty());
    append(&path, &message("msg_c1", "hello"));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["x", "hello"]);
    let reopened = harness.channel();
    assert_eq!(reopened.retained_segments(&source), 1);
    assert_eq!(reopened.ledger().gc_segments(&source).len(), 2);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn a_source_rewritten_behind_the_cursor_halts_the_channel_before_any_post() {
    let body = row("m0", "before the switch");
    let (harness, path, _) = switched_over(&body);
    let rewritten = row("m0", "BEFORE THE SWITCH");
    let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.write_all(&rewritten).unwrap();
    append(&path, &row("m1", "first"));
    harness.gate.acquired();
    let (_stop, task) = spawn(harness.writer());
    polls(3).await;
    assert!(task.is_finished(), "a stopped channel ends its actor");
    assert!(harness.port.posts().is_empty());
    assert!(matches!(
        harness.alarms.taken().as_slice(),
        [WriterAlarm::Halted { .. }]
    ));
}

#[tokio::test(start_paused = true)]
async fn the_writer_stays_dormant_unless_enabled() {
    let (harness, path, _) = switched_over(&row("m0", "before the switch"));
    append(&path, &row("m1", "first"));
    harness.gate.acquired();
    let config: WriterConfig = serde_json::from_str("{}").unwrap();
    assert!(!config.enabled);
    let (_stop, stopped) = watch::channel(false);
    let mut writer = harness.writer();
    let log = startup_log(&mut writer);
    let resumed = watch::channel(false).0;
    let spawned = spawn_if_enabled(
        &config,
        writer,
        ShadowProvider::Claude,
        log,
        stopped,
        resumed,
    );
    assert!(spawned.is_none());
    polls(3).await;
    assert!(harness.port.posts().is_empty());
    let enabled = WriterConfig { enabled: true };
    let (stop, stopped) = watch::channel(false);
    let mut writer = harness.writer();
    let log = startup_log(&mut writer);
    let resumed = watch::channel(false).0;
    let spawned = spawn_if_enabled(
        &enabled,
        writer,
        ShadowProvider::Claude,
        log,
        stopped,
        resumed,
    );
    polls(3).await;
    assert_eq!(harness.port.posts(), ["first"]);
    halt(stop, spawned.unwrap()).await;
}

#[tokio::test(start_paused = true)]
async fn the_round_trip_reads_back_every_case_and_reports_any_difference() {
    let all = cases(BOT);
    let names: Vec<&str> = all.iter().map(|(name, _)| name.as_str()).collect();
    for case in ["plain#0", "code_fence#0", "emoji#0", "mention#0", "limit#1"] {
        assert!(names.contains(&case), "{case} missing from {names:?}");
    }
    let units = |piece: &String| piece.encode_utf16().count();
    assert!(all.iter().all(|(_, piece)| units(piece) <= 2000));
    assert!(all.iter().any(|(_, piece)| units(piece) > 1900));
    let port = FakePort::default();
    let trips = round_trip(&port, CHANNEL).await;
    assert_eq!(port.posts().len(), all.len());
    assert!(trips.iter().all(RoundTrip::matched));
    let port = FakePort::default();
    port.replies
        .lock()
        .unwrap()
        .extend([Reply::Transformed, Reply::Refused(403)]);
    let trips = round_trip(&port, CHANNEL).await;
    assert!(!trips[0].matched() && trips[0].posted.is_some());
    assert!(!trips[1].matched() && trips[1].error.is_some());
    assert!(trips[2..].iter().all(RoundTrip::matched));
}

#[path = "rotation_tests.rs"]
mod rotation;

#[path = "host_tests.rs"]
mod host_start;
