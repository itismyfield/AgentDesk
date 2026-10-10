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
    switched_over_on(CHANNEL, body)
}

fn switched_over_on(channel: u64, body: &[u8]) -> (Harness, PathBuf, SourceId) {
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

async fn polls(count: u32) {
    tokio::time::sleep(POLL_INTERVAL * count).await;
}

/// A spawned actor; it returns why it stopped.
type Actor = tokio::task::JoinHandle<Option<super::super::deliver::StopCause>>;

fn spawn(writer: Writer) -> (watch::Sender<bool>, Actor) {
    spawn_as(writer, ShadowProvider::Claude)
}

fn spawn_as(mut writer: Writer, provider: ShadowProvider) -> (watch::Sender<bool>, Actor) {
    let bindings = startup_log(&mut writer);
    spawn_with(writer, provider, bindings)
}

fn spawn_with(
    writer: Writer,
    provider: ShadowProvider,
    bindings: Arc<impl BindingEvents>,
) -> (watch::Sender<bool>, Actor) {
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

async fn halt(stop: watch::Sender<bool>, task: Actor) {
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

#[derive(Clone, Default)]
struct WaitingAlarms {
    raised: Arc<Mutex<Vec<WriterAlarm>>>,
    cleared: Arc<AtomicUsize>,
    active: Arc<AtomicBool>,
}

impl AlarmSink for WaitingAlarms {
    fn raise(&self, _channel: u64, alarm: WriterAlarm) {
        if matches!(alarm, WriterAlarm::WaitingTooLong { .. }) {
            self.active.store(true, Ordering::SeqCst);
            self.raised.lock().unwrap().push(alarm);
        }
    }

    fn waiting_cleared(&self, _channel: u64) {
        if self.active.swap(false, Ordering::SeqCst) {
            self.cleared.fetch_add(1, Ordering::SeqCst);
        }
    }
}

fn spawn_waiting(
    harness: &Harness,
    utc: DateTime<Utc>,
) -> (WaitingAlarms, watch::Sender<bool>, Actor) {
    let alarms = WaitingAlarms::default();
    let (stop, task) = spawn_waiting_sink(harness, utc, alarms.clone());
    (alarms, stop, task)
}

fn spawn_waiting_sink<A: AlarmSink + 'static>(
    harness: &Harness,
    utc: DateTime<Utc>,
    alarms: A,
) -> (watch::Sender<bool>, Actor) {
    let channel = harness.channel();
    let bindings = Arc::new(FakeBindings::new());
    for (seq, source) in (1..).zip(channel.cursors().map(|c| c.source.clone())) {
        let mut event = bound(
            seq,
            None,
            BindingTarget::Source(source),
            BindingCause::Startup,
            None,
        );
        event.channel_id = harness.channel_id;
        bindings.commit(event);
    }
    let writer = ChannelWriter::new(
        channel,
        harness.gate.clone(),
        harness.port.clone(),
        harness.lease.clone(),
        alarms,
    );
    let (stop, stopped) = watch::channel(false);
    let task = tokio::spawn(super::super::actor::run_channel_at(
        writer,
        ShadowProvider::Claude,
        bindings,
        stopped,
        utc,
    ));
    (stop, task)
}

async fn waiting_poll(stop: &watch::Sender<bool>, elapsed: std::time::Duration) {
    tokio::time::advance(elapsed).await;
    // Wake a real actor poll at sub-second boundaries without changing its stop decision.
    stop.send_replace(false);
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn overdue_ready_is_observed_by_the_loop_at_strict_boundary_and_recurrence() {
    use std::time::Duration;
    let (harness, path, source) = switched_over(&row("m0", "before"));
    append(&path, &row("m1", "first waiting piece"));
    let (alarms, stop, task) = spawn_waiting(&harness, Utc::now());
    waiting_poll(&stop, Duration::ZERO).await;
    assert_eq!(
        harness.channel().cursor(&source).unwrap().captured_through,
        std::fs::metadata(&path).unwrap().len(),
        "the loop really captured Ready work"
    );
    assert!(harness.port.posts().is_empty());
    assert_eq!(harness.channel().ledger().next_serial(), 0);
    waiting_poll(&stop, Duration::from_millis(299_999)).await;
    assert!(alarms.raised.lock().unwrap().is_empty());
    waiting_poll(&stop, Duration::from_millis(1)).await;
    assert!(
        alarms.raised.lock().unwrap().is_empty(),
        "exactly 300s is not overdue"
    );
    waiting_poll(&stop, Duration::from_millis(1)).await;
    let first = alarms.raised.lock().unwrap().clone();
    assert_eq!(
        first.len(),
        1,
        "the actor loop must produce the overdue alarm"
    );
    assert!(matches!(
        &first[0],
        WriterAlarm::WaitingTooLong {
            ready: 1,
            prepared: None,
            ..
        }
    ));
    waiting_poll(&stop, Duration::from_secs(600)).await;
    assert_eq!(
        alarms.raised.lock().unwrap().len(),
        1,
        "same incident only once"
    );
    assert!(
        harness.port.posts().is_empty(),
        "observing does not send body output"
    );
    assert_eq!(harness.channel().ledger().next_serial(), 0);
    harness.gate.acquired();
    waiting_poll(&stop, Duration::ZERO).await;
    assert_eq!(harness.port.posts(), ["first waiting piece"]);
    assert_eq!(alarms.cleared.load(Ordering::SeqCst), 1);
    harness.gate.lost();
    append(&path, &row("m2", "next waiting piece"));
    waiting_poll(&stop, Duration::ZERO).await;
    waiting_poll(&stop, Duration::from_millis(300_001)).await;
    let events = alarms.raised.lock().unwrap().clone();
    assert_eq!(events.len(), 2);
    let incident = |alarm: &WriterAlarm| match alarm {
        WriterAlarm::WaitingTooLong { incident, .. } => incident.clone(),
        other => panic!("unexpected {other:?}"),
    };
    assert_ne!(incident(&events[0]), incident(&events[1]));
    assert!(
        !task.is_finished(),
        "an overdue alarm must not stop the writer"
    );
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn restored_prepared_uses_durable_utc_age_and_clamps_future_time() {
    use std::time::Duration;
    let (harness, _, _) = switched_over(&row("m0", "before"));
    let mut store = harness.channel();
    store
        .append_ledger(LedgerEntry::Prepared {
            serial: 0,
            unit_key: unit("restore"),
            piece_index: 0,
            payload: "still unresolved".into(),
            anchor_id: 100,
            epoch: 1,
        })
        .unwrap();
    let prepared_at = store.ledger().unresolved().unwrap().1.prepared_at;
    drop(store);
    let (alarms, stop, task) =
        spawn_waiting(&harness, prepared_at - chrono::Duration::seconds(600));
    waiting_poll(&stop, Duration::ZERO).await;
    assert!(!task.is_finished(), "the restored observer really runs");
    waiting_poll(&stop, Duration::from_millis(300_001)).await;
    assert!(
        alarms.raised.lock().unwrap().is_empty(),
        "future timestamp has zero age"
    );
    waiting_poll(&stop, Duration::from_millis(599_998)).await;
    assert!(
        alarms.raised.lock().unwrap().is_empty(),
        "restored age 299.999s"
    );
    waiting_poll(&stop, Duration::from_millis(1)).await;
    assert!(
        alarms.raised.lock().unwrap().is_empty(),
        "restored age exactly 300s"
    );
    waiting_poll(&stop, Duration::from_millis(1)).await;
    let events = alarms.raised.lock().unwrap().clone();
    assert_eq!(events.len(), 1);
    assert!(
        !task.is_finished(),
        "age observation does not stop the writer"
    );
    assert!(matches!(
        &events[0],
        WriterAlarm::WaitingTooLong {
            ready: 0,
            prepared: Some(0),
            ..
        }
    ));
    let before = harness.channel().ledger().clone();
    assert!(harness.port.posts().is_empty());
    halt(stop, task).await;
    let (replayed, stop, task) =
        spawn_waiting(&harness, prepared_at + chrono::Duration::seconds(301));
    waiting_poll(&stop, Duration::ZERO).await;
    assert_eq!(
        *replayed.raised.lock().unwrap(),
        events,
        "same durable Prepared incident after restart"
    );
    assert_eq!(
        *harness.channel().ledger(),
        before,
        "observation never appends a result"
    );
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn ready_item_ages_and_channels_are_independent() {
    use std::time::Duration;
    let (first, a, _) = switched_over(&row("m0", "before"));
    let (second, b, _) = switched_over_on(632_504, &row("n0", "before"));
    append(&a, &row("m1", "older"));
    let (alarms_a, stop_a, task_a) = spawn_waiting(&first, Utc::now());
    let (alarms_b, stop_b, task_b) = spawn_waiting(&second, Utc::now());
    waiting_poll(&stop_a, Duration::ZERO).await;
    waiting_poll(&stop_b, Duration::from_secs(100)).await;
    append(&a, &row("m2", "younger"));
    append(&b, &row("n1", "second channel"));
    waiting_poll(&stop_a, Duration::ZERO).await;
    waiting_poll(&stop_b, Duration::ZERO).await;
    waiting_poll(&stop_a, Duration::from_millis(200_001)).await;
    assert_eq!(alarms_a.raised.lock().unwrap().len(), 1);
    assert!(matches!(
        alarms_a.raised.lock().unwrap()[0],
        WriterAlarm::WaitingTooLong { ready: 1, .. }
    ));
    assert!(alarms_b.raised.lock().unwrap().is_empty());
    waiting_poll(&stop_b, Duration::from_secs(100)).await;
    assert_eq!(alarms_b.raised.lock().unwrap().len(), 1);
    assert!(first.port.posts().is_empty() && second.port.posts().is_empty());
    halt(stop_a, task_a).await;
    halt(stop_b, task_b).await;
}

#[tokio::test(start_paused = true)]
async fn a_recovered_actor_clears_waiting_health_through_its_shared_sink() {
    use crate::services::tui_o::alarm::{AlarmHealth, AlarmRouter};
    use std::time::{Duration, Instant};
    let (harness, path, _) = switched_over(&row("m0", "before"));
    append(&path, &row("m1", "recover this piece"));
    let health = Arc::new(AlarmHealth::default());
    let router = Arc::new(AlarmRouter::new(None, None, health.clone()));
    let reason = format!("tui_o:waiting_too_long:{CHANNEL}");
    let (stop, task) = spawn_waiting_sink(&harness, Utc::now(), router.clone());
    waiting_poll(&stop, Duration::ZERO).await;
    waiting_poll(&stop, Duration::from_millis(300_001)).await;
    assert!(health.current_at(Instant::now()).contains(&reason));
    halt(stop, task).await;
    assert!(
        health.current_at(Instant::now()).contains(&reason),
        "stopping did not settle the work"
    );
    let (stop, task) = spawn_waiting_sink(&harness, Utc::now(), router.clone());
    waiting_poll(&stop, Duration::ZERO).await;
    assert!(
        health.current_at(Instant::now()).contains(&reason),
        "reconstructing with the gateway lost does not settle the work"
    );
    assert!(harness.port.posts().is_empty());
    waiting_poll(&stop, Duration::from_millis(300_001)).await;
    assert!(health.current_at(Instant::now()).contains(&reason));
    assert!(harness.port.posts().is_empty());
    halt(stop, task).await;
    harness.gate.acquired();
    let (stop, task) = spawn_waiting_sink(&harness, Utc::now(), router);
    waiting_poll(&stop, Duration::ZERO).await;
    assert_eq!(harness.port.posts(), ["recover this piece"]);
    assert!(
        !health.current_at(Instant::now()).contains(&reason),
        "the shared health condition clears"
    );
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
    harness.gate.acquired();
    polls(3).await;
    assert_eq!(
        harness.port.posts(),
        ["first", "second"],
        "a Stop, an abort or EOF is not a delivery"
    );
    assert_eq!(harness.alarms.taken(), [WriterAlarm::PausedNoGateway]);
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

#[path = "drain_projection_tests.rs"]
mod drain_projection;

#[tokio::test(start_paused = true)]
async fn verified_permission_hold_preserves_an_attached_actor_cursor_and_owed_output() {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::verified_permission_hold_preserves_an_attached_actor_cursor_and_owed_output"
    )) {
        return;
    }
    held_actor_preserves_cursor_and_owed_output(false).await;
}

#[tokio::test(start_paused = true)]
async fn verified_new_source_permission_does_not_release_a_prior_actor_source_or_owed_output() {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::verified_new_source_permission_does_not_release_a_prior_actor_source_or_owed_output"
    )) {
        return;
    }
    held_actor_preserves_cursor_and_owed_output(true).await;
}

async fn held_actor_preserves_cursor_and_owed_output(allowed_new_source: bool) {
    use crate::services::tui_prompt_dedupe::{
        self as dedupe,
        binding_context::{BindingContext, PreparedIncarnation},
    };
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe_lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (_context_root, _env) =
        dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(Some(_context_root.path()));
    let native = |kind: &str, payload: serde_json::Value| {
        format!("{}\n", serde_json::json!({"type":kind,"payload":payload})).into_bytes()
    };
    let message = |id: &str, text: &str| {
        native(
            "response_item",
            serde_json::json!({
                "type":"message","role":"assistant","id":id,
                "content":[{"type":"output_text","text":text}]
            }),
        )
    };
    let turn = |kind: &str| native("event_msg", serde_json::json!({"type":kind,"turn_id":"t1"}));
    let (harness, path, _) = switched_over(b"");
    harness.gate.acquired();
    let mut writer = harness.writer();
    let bindings = startup_log(&mut writer);
    for line in [
        turn("task_started"),
        message("m1", "already owed"),
        turn("task_complete"),
    ] {
        append(&path, &line);
    }
    let tmux = format!("o-held-{}", uuid::Uuid::new_v4().simple());
    super::super::actor::exercise_held_actor_for_tests(
        writer,
        ShadowProvider::Codex,
        bindings,
        || {
            let context = BindingContext {
                schema: 1,
                provider: "codex".into(),
                created_at: Utc::now(),
                execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
                tmux_session: tmux.clone(),
                channel_id: Some(CHANNEL),
                owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
                host: None,
                expected_native_session_id: None,
                launch_mode: "fresh".into(),
                provider_root: Some(_context_root.path().canonicalize().unwrap()),
                first_prompt_digest: None,
                source_policy: Some("verified".into()),
            };
            PreparedIncarnation::create(context.clone()).unwrap();
            let marker = crate::services::tmux_common::session_temp_path(&tmux, "spawn_nonce");
            std::fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
            std::fs::write(marker, &context.execution_nonce).unwrap();
            dedupe::register_tmux_channel(&tmux, CHANNEL);
            if allowed_new_source {
                use dedupe::binding_context::{BINDING_HEADER, CapturedContext, HookBindingEnvelope, ObservedHookProcess};
                let id = "019e660d-4859-7522-9cee-8ba7c4e7c743";
                let proof_path = context.provider_root.as_ref().unwrap().join(format!("rollout-{id}.jsonl"));
                let timestamp = (context.created_at + chrono::Duration::seconds(1)).to_rfc3339();
                let header = serde_json::json!({"type":"session_meta", "timestamp":timestamp,
                    "payload":{"id":id,"timestamp":timestamp,"source":"cli",
                    "cwd":_context_root.path(),"originator":"codex_cli_rs"}});
                std::fs::write(&proof_path, format!("{header}\n")).unwrap();
                dedupe::set_codex_delivery_permission_for_tests(&context, dedupe::CodexDeliveryPermissionForTests::Allowed);
                let envelope = HookBindingEnvelope { context: CapturedContext::Captured(context.clone()), observed: ObservedHookProcess::default() };
                let mut headers = axum::http::HeaderMap::new();
                headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
                let ingress = crate::services::claude_tui::hook_server::observation_ingress::observe_binding_hook(
                    "codex", "session_start", Some(id), Some(id),
                    &serde_json::json!({"session_id":id,"transcript_path":proof_path,"source":"startup"}), &headers,
                );
                let fold = dedupe::binding_events::codex::read_ownership(&context);
                assert!(dedupe::runtime_binding_for_tmux_session(&tmux).is_some(), "new source must actually publish its Allowed proof: ingress={ingress:?}, fold={fold:?}");
                assert!(dedupe::codex_verified_channel_delivery_allowed(CHANNEL), "channel guard must pass so source identity is the only hold");
            }
            append(&path, &message("m2", "must remain uncaptured"));
        },
    )
    .await;
    assert!(
        harness.port.posts().is_empty(),
        "held actor must send nothing"
    );
    assert!(
        harness.alarms.taken().is_empty(),
        "permission hold must not alarm"
    );
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(None);
}

#[path = "canary_policy_tests.rs"]
mod canary_policy;
