//! Verified owed delivery through the real writer and actor: exact permits, FIFO holds, kept
//! debt and per-source GC protection, with the readiness gate opened only by the test.
#[path = "tool_card_provenance_tests.rs"]
mod tool_card_provenance;
#[path = "tool_card_codex_tests.rs"]
mod tool_cards;
#[path = "turn_summary_codex_verified_tests.rs"]
mod turn_summaries;
use super::*;
use crate::services::provider::CancelToken;
use crate::services::tui_o::store::ledger::{LedgerEntry, PieceOutcome};
use crate::services::tui_o::writer::deliver::Step;
use crate::services::tui_o::writer::owed::{PROBES, Reevaluation, deliver_owed};
use crate::services::tui_prompt_dedupe::codex_verified::provenance::admission::{
    self, forget_channel_for_tests, issue_for_tests, set_ready_for_tests,
};
use std::sync::atomic::Ordering;
/// Opens the readiness gate for one channel on this thread and forgets its records on drop.
struct Gate(u64);
impl Gate {
    fn open(channel: u64, ready: bool) -> Self {
        set_ready_for_tests(channel, ready);
        Self(channel)
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        set_ready_for_tests(self.0, false);
        forget_channel_for_tests(self.0);
    }
}

fn len(source: &SourceId) -> u64 {
    std::fs::metadata(&source.path).unwrap().len()
}

fn episode_span(
    channel: u64,
    source: &SourceId,
    message: u64,
    range: (u64, u64),
) -> CodexEpisodeSpan {
    let mut span = span(source, range.0, Some(range.1));
    span.episode.channel_id = channel;
    (span.delivery_channel_id, span.offset_authority_channel_id) = (channel, channel);
    span.episode.user_message_id = message;
    span.episode.turn_nonce = format!("turn-{message}");
    span.episode.native_turn_id = format!("n{message}");
    span
}

/// Appends one closed native turn, stores its span, and issues its permit when `permit`.
fn turn(
    store: &mut ChannelStore,
    source: &SourceId,
    message: u64,
    bodies: &[&str],
    permit: bool,
) -> (CodexEpisodeSpan, Option<Arc<CancelToken>>) {
    let native = format!("n{message}");
    let start = len(source);
    let mut lines = vec![event("task_started", &native)];
    lines.extend(bodies.iter().map(|text| body(text, text)));
    lines.push(event("task_complete", &native));
    append(&source.path, &lines);
    let channel = store.init().channel;
    let span = episode_span(channel, source, message, (start, len(source)));
    store.persist_codex_span(span.clone()).unwrap();
    let token = permit.then(|| {
        let nonce = Some(span.episode.turn_nonce.clone());
        let token = Arc::new(CancelToken::from_persisted_turn_nonce(nonce));
        let (execution, episode) = (span.execution.clone(), span.episode.clone());
        issue_for_tests(execution, episode, &token).unwrap();
        token
    });
    (span, token)
}

/// A switched-over Codex channel with an empty sidecar and its sources resumed.
struct Verified {
    _gate: Gate,
    _runtime: tempfile::TempDir,
    store: OStore,
    a: SourceId,
    writer: Writer,
    port: Arc<Port>,
    collector: Collector,
}

impl Verified {
    fn new(channel: u64, ready: bool) -> Self {
        let gate = Gate::open(channel, ready);
        let (runtime, store, a) = switched_on(channel);
        let (mut writer, port) = owed_writer(opened(&store));
        writer.store().init_codex_provenance().unwrap();
        let bindings = Bindings::on(channel);
        bindings.bind(None, &a, BindingCause::Startup);
        let collector = kept(Collector::resumed(&mut writer, &bindings), "resume");
        Self {
            _gate: gate,
            _runtime: runtime,
            store,
            a,
            writer,
            port,
            collector,
        }
    }

    fn turn(&mut self, message: u64, bodies: &[&str], permit: bool) -> Option<Arc<CancelToken>> {
        turn(
            self.writer.store(),
            &self.a.clone(),
            message,
            bodies,
            permit,
        )
        .1
    }

    fn read(&mut self) {
        kept(self.collector.read(&mut self.writer), "capture");
    }

    async fn deliver(&mut self) {
        deliver_owed(&mut self.writer, &mut self.collector.log).await;
    }

    fn queued(&self) -> Vec<&str> {
        self.collector.log.work.iter().map(payload).collect()
    }
}

/// P2-A: a cancellation before the exact check leaves no `Prepared`, POST or serial, and neither
/// that retry nor an independent next episode halts the writer.
#[tokio::test(start_paused = true)]
async fn a_cancel_before_the_exact_check_leaves_no_prepared_post_or_serial_and_never_halts() {
    let mut verified = Verified::new(584_611, true);
    let token = verified.turn(1, &["one"], true).unwrap();
    verified.read();
    let serial = verified.writer.store().ledger().next_serial();
    token.publish_cancel("user stop");
    for _ in 0..2 {
        verified.deliver().await;
        let ledger = verified.writer.store().ledger();
        assert_eq!(ledger.next_serial(), serial, "a Prepared was appended");
        assert!(ledger.unresolved().is_none() && ledger.violation().is_none());
        assert!(verified.port.posts().is_empty());
        assert!(!verified.writer.is_stopped());
        assert_eq!(verified.queued(), ["one"], "withheld work stays owed");
    }
    verified.turn(2, &["two"], true);
    verified.read();
    verified.deliver().await;
    assert_eq!(verified.port.posts(), ["two"]);
    assert!(!verified.writer.is_stopped());
    assert_eq!(verified.queued(), ["one"]);
}

/// P2-A: a cancellation published during the first poll waits for the record lock, so the POST
/// started under the permit settles as Posted.
#[tokio::test(start_paused = true)]
async fn a_cancel_during_the_first_poll_waits_and_the_piece_settles_as_posted() {
    let mut verified = Verified::new(584_612, true);
    let token = verified.turn(1, &["one"], true).unwrap();
    verified.read();
    let canceller = Arc::new(Mutex::new(None));
    let raced = Arc::new(Mutex::new(None));
    let (hook_token, hook_canceller, hook_raced) = (
        Arc::clone(&token),
        Arc::clone(&canceller),
        Arc::clone(&raced),
    );
    *verified.port.on_post.lock().unwrap() = Some(Box::new(move || {
        let (trying, tried) = std::sync::mpsc::channel();
        let token = Arc::clone(&hook_token);
        let cancel = std::thread::spawn(move || {
            trying.send(()).unwrap();
            token.publish_cancel("user stop");
        });
        tried.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        *hook_raced.lock().unwrap() = Some(hook_token.cancelled.load(Ordering::Acquire));
        *hook_canceller.lock().unwrap() = Some(cancel);
    }));
    verified.deliver().await;
    canceller.lock().unwrap().take().unwrap().join().unwrap();
    assert_eq!(
        *raced.lock().unwrap(),
        Some(false),
        "the cancel landed inside the first poll"
    );
    assert_eq!(verified.port.posts(), ["one"]);
    let ledger = verified.writer.store().ledger();
    assert!(matches!(
        ledger.piece(0).unwrap().outcome,
        Some(PieceOutcome::Posted(_))
    ));
    assert!(verified.queued().is_empty() && token.cancelled.load(Ordering::Acquire));
}

/// With the production readiness gate, a valid record and a closed span still admit nothing.
#[tokio::test(start_paused = true)]
async fn with_the_readiness_gate_off_a_valid_permit_and_span_post_nothing() {
    let mut verified = Verified::new(584_613, false);
    verified.turn(1, &["one"], true);
    verified.read();
    let mut work = verified.collector.log.work.clone();
    crate::services::tui_o::writer::owed::reevaluate(verified.writer.store(), &mut work).unwrap();
    let [owed] = work.as_slice() else {
        panic!("one body: {work:?}")
    };
    let origin = owed.origin.clone().expect("the span attributes the body");
    let record = admission::lookup(&origin);
    assert_eq!(record.as_ref().err(), Some(&"verified_owed_not_ready"));
    set_ready_for_tests(584_613, true);
    let record = admission::lookup(&origin).unwrap();
    set_ready_for_tests(584_613, false);
    let serial = verified.writer.store().ledger().next_serial();
    let step = (verified.writer)
        .deliver_admitted(&owed.derived, Some((&record, &origin)))
        .await;
    assert_eq!(step, Step::Withheld);
    assert!(verified.port.posts().is_empty());
    assert_eq!(verified.writer.store().ledger().next_serial(), serial);
}

/// A NotFound piece is kept debt: it is not posted again, and the rest of its episode waits,
/// while Posted and Excluded work leaves the queue.
#[tokio::test(start_paused = true)]
async fn not_found_work_stays_owed_and_holds_its_episode_while_posted_and_excluded_leave() {
    let mut verified = Verified::new(584_614, true);
    verified.turn(1, &["first", "second"], true);
    let tool = serde_json::json!({"type": "response_item", "payload": {
        "type": "function_call", "id": "tool", "call_id": "call", "name": "shell", "arguments": "{}"}});
    let start = len(&verified.a);
    append(
        &verified.a.path,
        &[
            event("task_started", "n2"),
            format!("{tool}\n"),
            body("third", "third"),
            event("task_complete", "n2"),
        ],
    );
    let channel = verified.writer.store().init().channel;
    let span = episode_span(channel, &verified.a, 2, (start, len(&verified.a)));
    verified
        .writer
        .store()
        .persist_codex_span(span.clone())
        .unwrap();
    let token = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        span.episode.turn_nonce.clone(),
    )));
    issue_for_tests(span.execution.clone(), span.episode.clone(), &token).unwrap();
    verified.read();
    let Derived::Piece(first) = &verified.collector.log.work[0].derived else {
        panic!("first piece")
    };
    let ledger = verified.writer.store().ledger();
    let prepared = LedgerEntry::Prepared {
        local_retry: None,
        serial: ledger.next_serial(),
        unit_key: first.unit_key.clone(),
        piece_index: first.index,
        payload: first.payload.clone(),
        anchor_id: ledger.anchor(),
        epoch: 1,
    };
    verified.writer.store().append_ledger(prepared).unwrap();
    verified.deliver().await;
    let ledger = verified.writer.store().ledger();
    assert!(matches!(
        ledger.piece(0).unwrap().outcome,
        Some(PieceOutcome::NotFound)
    ));
    assert_eq!(
        verified.port.posts(),
        ["third"],
        "NotFound is never re-posted"
    );
    assert_eq!(
        verified.queued(),
        ["first", "second"],
        "the episode waits behind its debt"
    );
    assert!(!verified.writer.is_stopped());
    for serial in 0..verified.writer.store().ledger().next_serial() {
        assert!(
            verified
                .writer
                .store()
                .ledger()
                .piece(serial)
                .unwrap()
                .local_retry
                .is_none()
        );
    }
    verified
        .writer
        .retry_local(Utc::now() + chrono::Duration::days(1))
        .await;
    assert!(!verified.writer.has_local_retry());
}

/// P2-3/P2-5: four sidecar states, one read per spooled record, and a failed second source
/// read publishes no origin from the first.
#[tokio::test(start_paused = true)]
async fn reevaluation_reports_each_sidecar_state_reads_each_record_once_and_publishes_all_or_none()
{
    use crate::services::tui_o::writer::owed::reevaluate;
    let mut verified = Verified::new(584_615, true);
    verified.turn(1, &["one", "two", "three"], false);
    verified.read();
    let mut work = verified.collector.log.work.clone();
    PROBES.with(|probes| probes.set(0));
    let read = reevaluate(verified.writer.store(), &mut work).unwrap();
    assert_eq!(read, Reevaluation::Present { attributed: 3 });
    assert_eq!(
        PROBES.with(|probes| probes.get()),
        5,
        "one probe per spooled record"
    );
    let sidecar = channel_file_on(&verified._runtime, 584_615, CODEX_PROVENANCE_FILE);
    let saved = std::fs::read(&sidecar).unwrap();
    let present = Reevaluation::Present { attributed: 3 };
    std::fs::write(&sidecar, b"{damaged").unwrap();
    let read = reevaluate(verified.writer.store(), &mut work).unwrap();
    let unreadable = Reevaluation::Unavailable("codex_provenance_unreadable");
    assert_eq!(read, unreadable);
    assert!(
        work.iter().all(|w| w.origin.is_none()),
        "an origin outlived a damaged sidecar"
    );
    std::fs::write(&sidecar, &saved).unwrap();
    assert_eq!(
        reevaluate(verified.writer.store(), &mut work).unwrap(),
        present
    );
    std::fs::remove_file(&sidecar).unwrap();
    let read = reevaluate(verified.writer.store(), &mut work).unwrap();
    assert_eq!(read, Reevaluation::Missing);
    assert!(
        work.iter().all(|w| w.origin.is_none()),
        "an origin outlived a missing sidecar"
    );
    std::fs::write(&sidecar, &saved).unwrap();
    assert_eq!(
        reevaluate(verified.writer.store(), &mut work).unwrap(),
        present
    );
    let mut elsewhere = work.clone();
    elsewhere.iter_mut().for_each(|w| w.range.start += 1);
    let read = reevaluate(verified.writer.store(), &mut elsewhere).unwrap();
    assert_eq!(read, Reevaluation::Present { attributed: 0 });
    let b = transcript(&verified.a.path, "b.jsonl", "s2", &[body("b-one", "b-one")]);
    let mut b_work = work[0].clone();
    (b_work.source, b_work.range) = (b.clone(), 0..1);
    verified.writer.store().attach_source(&b).unwrap();
    let mut capture =
        crate::services::tui_o::shadow::capture::SourceCapture::open(b.clone(), 0).unwrap();
    let crate::services::tui_o::shadow::CaptureOutcome::Batch(batch) =
        crate::services::tui_o::shadow::CaptureSource::poll(&mut capture, 1 << 20)
    else {
        panic!("capture failed")
    };
    verified
        .writer
        .store()
        .append_spool(&batch, &capture.prefix_hash())
        .unwrap();
    let mut both = vec![work[0].clone(), b_work];
    let spool = channel_file_on(&verified._runtime, 584_615, "spool");
    for entry in std::fs::read_dir(&spool).unwrap() {
        let path = entry.unwrap().path();
        if path
            .to_string_lossy()
            .contains(&crate::services::tui_o::store::spool::source_key(&b))
        {
            std::fs::remove_dir_all(&path).ok();
            std::fs::remove_file(&path).ok();
        }
    }
    both[0].origin = None;
    PROBES.with(|probes| probes.set(0));
    let failed = reevaluate(verified.writer.store(), &mut both);
    assert!(failed.is_err(), "the second source read must fail");
    assert!(
        both.iter().all(|w| w.origin.is_none()),
        "a partial origin was published"
    );
    assert!(
        PROBES.with(|probes| probes.get()) > 0,
        "the first source was read before the failure"
    );
}

/// The real actor posts an episode only under its permit, keeps an unpermitted one, stops at work
/// it cannot attribute, and re-reads the sidecar each pass so a later span lets it continue.
#[tokio::test(start_paused = true)]
async fn the_actor_posts_permitted_episodes_holds_unknown_ones_and_waits_behind_unattributed_work()
{
    let channel = 584_616;
    let _gate = Gate::open(channel, true);
    let (_runtime, store, a) = switched_on(channel);
    let (mut writer, port) = owed_writer(opened(&store));
    writer.store().init_codex_provenance().unwrap();
    turn(writer.store(), &a, 1, &["zero"], false);
    turn(writer.store(), &a, 2, &["one"], true);
    let loose = len(&a);
    append(&a.path, &[body("loose", "loose")]);
    let loose = (loose, len(&a));
    turn(writer.store(), &a, 3, &["two"], true);
    let bindings = Bindings::on(channel);
    bindings.bind(None, &a, BindingCause::Startup);
    let (stop, stopped) = watch::channel(false);
    let resumed = watch::channel(false).0;
    let task = tokio::spawn(run_channel(
        writer,
        ShadowProvider::Codex,
        bindings,
        stopped,
        resumed,
    ));
    tokio::time::sleep(POLL_INTERVAL * 4).await;
    assert_eq!(port.posts(), ["one", "-# ✅ 완료"], "unattributed work stops the pass");
    let span = episode_span(channel, &a, 9, loose);
    opened(&store).persist_codex_span(span.clone()).unwrap();
    let token = Arc::new(CancelToken::from_persisted_turn_nonce(Some(
        span.episode.turn_nonce.clone(),
    )));
    issue_for_tests(span.execution.clone(), span.episode.clone(), &token).unwrap();
    tokio::time::sleep(POLL_INTERVAL * 4).await;
    assert_eq!(port.posts(), ["one", "-# ✅ 완료", "loose", "two", "-# ✅ 완료"]);
    stop.send(true).unwrap();
    assert_eq!(task.await.unwrap(), None);
}

/// GC keeps every source owed work still names, directly or through a repeated unit key, and
/// collects nothing while an announced unit waits for its seal.
#[tokio::test(start_paused = true)]
async fn gc_keeps_owed_and_associated_sources_and_nothing_while_a_unit_is_unsealed() {
    let channel = 584_617;
    let _gate = Gate::open(channel, true);
    let (runtime, store, a) = switched_on(channel);
    let (mut writer, port) = owed_writer(opened(&store));
    writer.store().init_codex_provenance().unwrap();
    writer.store().set_segment_max_for_tests(1);
    let b = transcript(&a.path, "b.jsonl", "s2", &[body("zero", "zero")]);
    writer.store().attach_source(&b).unwrap();
    for upto in [header("s2").len() as u64, len(&b)] {
        let cursor = writer.store().cursor(&b).unwrap().clone();
        let mut capture = crate::services::tui_o::shadow::capture::SourceCapture::reopen(
            b.clone(),
            cursor.captured_through,
            &cursor.prefix_hash,
        )
        .unwrap();
        let read = upto - cursor.captured_through;
        let crate::services::tui_o::shadow::CaptureOutcome::Batch(batch) =
            crate::services::tui_o::shadow::CaptureSource::poll(&mut capture, read)
        else {
            panic!("capture failed")
        };
        writer
            .store()
            .append_spool(&batch, &capture.prefix_hash())
            .unwrap();
    }
    let bindings = Bindings::on(channel);
    bindings.bind(None, &a, BindingCause::Startup);
    let (stop, stopped) = watch::channel(false);
    let task = tokio::spawn(run_channel(
        writer,
        ShadowProvider::Codex,
        bindings,
        stopped,
        watch::channel(false).0,
    ));
    append(&a.path, &[body("zero", "zero")]);
    tokio::time::sleep(POLL_INTERVAL * 2).await;
    append(&a.path, &[event("task_started", "n7")]);
    tokio::time::sleep(POLL_INTERVAL * 3).await;
    let retained = |source: &SourceId| opened(&store).retained_segments(source);
    assert!(port.posts().is_empty(), "an unattributed body never posts");
    assert_eq!(
        retained(&b),
        2,
        "the source of unattributed work keeps its segments"
    );
    assert_eq!(
        retained(&a),
        2,
        "a source the owed unit key was repeated from is kept"
    );
    stop.send(true).unwrap();
    task.await.unwrap();
    drop(runtime);

    let channel = 584_618;
    let _gate = Gate::open(channel, true);
    let (_runtime, store, a) = switched_on(channel);
    let (mut writer, port) = owed_writer(opened(&store));
    writer.store().init_codex_provenance().unwrap();
    writer.store().set_segment_max_for_tests(1);
    let bindings = Bindings::on(channel);
    bindings.bind(None, &a, BindingCause::Startup);
    let (stop, stopped) = watch::channel(false);
    let task = tokio::spawn(run_channel(
        writer,
        ShadowProvider::Codex,
        bindings,
        stopped,
        watch::channel(false).0,
    ));
    let mut sidecar = opened(&store);
    turn(&mut sidecar, &a, 1, &["one"], true);
    tokio::time::sleep(POLL_INTERVAL * 2).await;
    append(&a.path, &[event("task_started", "n2")]);
    tokio::time::sleep(POLL_INTERVAL * 3).await;
    assert_eq!(port.posts(), ["one", "-# ✅ 완료"]);
    assert_eq!(
        opened(&store).retained_segments(&a),
        1,
        "settled segments are collected"
    );
    let announced = serde_json::json!({"type": "event_msg", "payload": {"type": "item_completed",
        "item": {"type": "AgentMessage", "id": "pending"}}});
    append(&a.path, &[format!("{announced}\n")]);
    tokio::time::sleep(POLL_INTERVAL * 2).await;
    append(&a.path, &[event("task_started", "n3")]);
    tokio::time::sleep(POLL_INTERVAL * 3).await;
    assert_eq!(
        opened(&store).retained_segments(&a),
        3,
        "an unsealed unit keeps every segment"
    );
    stop.send(true).unwrap();
    task.await.unwrap();
}

/// Counts the actor's re-post turns; a fresh original may always go.
struct CountTurns(Arc<std::sync::atomic::AtomicUsize>);

impl<W> crate::services::tui_o::repost::runner::RepostHook<W> for CountTurns {
    fn turn<'a>(
        &'a mut self,
        _: &'a mut W,
    ) -> crate::services::tui_o::repost::runner::Turned<'a, ()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }

    fn original(
        &mut self,
        _: u64,
        _: crate::services::tui_o::store::ledger::PieceRecord,
    ) -> crate::services::tui_o::repost::runner::Turned<
        '_,
        crate::services::tui_o::repost::admission::OriginalGate,
    > {
        Box::pin(async { crate::services::tui_o::repost::admission::OriginalGate::SendLocal })
    }

    fn receipts(
        &mut self,
    ) -> crate::services::tui_o::repost::runner::Turned<
        '_,
        crate::services::tui_o::repost::projection::ReceiptOverlay,
    > {
        Box::pin(async { crate::services::tui_o::repost::projection::ReceiptOverlay::Disabled })
    }
}

/// Re-post holds no exact permit, so a verified channel's actor never runs its turn; the same
/// actor runs it once the readiness gate is off.
#[tokio::test(start_paused = true)]
async fn a_verified_channel_never_runs_the_permitless_repost_turn() {
    for (channel, ready) in [(584_619, true), (584_620, false)] {
        let _gate = Gate::open(channel, ready);
        let (_runtime, store, a) = switched_on(channel);
        let (writer, _) = owed_writer(opened(&store));
        let turns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let writer = writer.with_repost(Some(Box::new(CountTurns(Arc::clone(&turns)))));
        let bindings = Bindings::on(channel);
        bindings.bind(None, &a, BindingCause::Startup);
        let (stop, stopped) = watch::channel(false);
        let resumed = watch::channel(false).0;
        let codex = ShadowProvider::Codex;
        let task = tokio::spawn(run_channel(writer, codex, bindings, stopped, resumed));
        tokio::time::sleep(POLL_INTERVAL * 3).await;
        stop.send(true).unwrap();
        task.await.unwrap();
        let ran = turns.load(Ordering::SeqCst);
        assert_eq!(ran == 0, ready, "ready {ready}: {ran} re-post turns");
    }
}
