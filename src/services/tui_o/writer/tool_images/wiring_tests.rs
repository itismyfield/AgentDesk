//! Tool images through the real deriver, ledger and writer: turn spans across restarts and GC,
//! late confirmation, refusals, notices and the shared Prepared.

use base64::Engine as _;
use serde_json::json;

use super::actor::{append, halt, polls, spawn, switched_over, writer_over};
use super::*;
use crate::services::tui_o::shadow::capture::SourceCapture;
use crate::services::tui_o::shadow::{CaptureOutcome, MAX_READ_BYTES, SourceId};
use crate::services::tui_o::writer::restart_end::Ended;
use crate::services::tui_o::writer::tool_images::derive::ImageRun;
use crate::services::tui_o::writer::tool_images::select::{kept_sources, open_count};

fn line(value: serde_json::Value) -> Vec<u8> {
    let mut line = serde_json::to_vec(&value).unwrap();
    line.push(b'\n');
    line
}

fn prompt(uuid: &str) -> Vec<u8> {
    line(json!({"type": "user", "uuid": uuid, "message": {"role": "user", "content": "go"}}))
}

fn idle() -> Vec<u8> {
    line(json!({"type": "system", "subtype": "turn_duration", "durationMs": 1}))
}

/// One image block; `tag` varies the PNG bytes, `media` may name an unsupported type.
fn image(tag: u8, media: &str) -> serde_json::Value {
    let bytes = [b"\x89PNG\r\n\x1a\n".as_slice(), &[tag]].concat();
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    json!({"type": "image", "source": {"type": "base64", "media_type": media, "data": data}})
}

fn result_of(uuid: &str, call: &str, parts: Vec<serde_json::Value>) -> Vec<u8> {
    line(
        json!({"type": "user", "uuid": uuid, "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": call, "content": parts}]}}),
    )
}

fn result(uuid: &str, call: &str, tag: u8, media: &str) -> Vec<u8> {
    result_of(uuid, call, vec![image(tag, media)])
}

fn png(uuid: &str, call: &str, tag: u8) -> Vec<u8> {
    result(uuid, call, tag, "image/png")
}

fn images(writer: &mut Writer) -> UnitDeriver {
    let store = writer.store();
    let run = ImageRun::new(
        CHANNEL,
        ShadowProvider::Claude,
        store.ledger(),
        &store.init().sources,
    );
    UnitDeriver::new(CHANNEL, ShadowProvider::Claude).with_tool_images(run)
}

/// Spools what the transcript holds past `from`.
fn spool(writer: &mut Writer, source: &SourceId, from: u64) -> Vec<CapturedRecord> {
    let mut capture = SourceCapture::open(source.clone(), from).unwrap();
    let CaptureOutcome::Batch(batch) = capture.poll(MAX_READ_BYTES) else {
        panic!("capture failed");
    };
    writer
        .store()
        .append_spool(&batch, &capture.prefix_hash())
        .unwrap();
    batch.records
}

/// Derives `records` in order, as capture or boot replay does, then appends the image entries.
fn run(
    writer: &mut Writer,
    deriver: &mut UnitDeriver,
    source: &SourceId,
    records: &[CapturedRecord],
) -> Vec<Derived> {
    let mut out = Vec::new();
    for record in records {
        deriver.enter_source(source);
        out.extend(deriver.derive(record));
    }
    writer.record_images(deriver);
    out
}

fn derive(
    writer: &mut Writer,
    deriver: &mut UnitDeriver,
    source: &SourceId,
    from: u64,
) -> Vec<Derived> {
    let records = spool(writer, source, from);
    run(writer, deriver, source, &records)
}

/// The spooled records from `from` on, oldest first.
fn spooled(writer: &mut Writer, source: &SourceId, from: u64) -> Vec<CapturedRecord> {
    let mut records = Vec::new();
    let scan = writer.store().for_each_frame(source, |frame, _| {
        if let crate::services::tui_o::store::spool::SpoolFrame::Record(record) = frame
            && record.start >= from
        {
            records.push(record);
        }
    });
    scan.unwrap();
    records
}

async fn deliver_all(writer: &mut Writer, items: &[Derived]) {
    for item in items {
        assert_eq!(writer.deliver(item).await, Step::Done);
    }
}

fn markers(items: &[Derived]) -> usize {
    let image = |item: &&Derived| matches!(item, Derived::Piece(p) if p.unit_key.kind == UnitKind::ToolImage);
    items.iter().filter(image).count()
}

fn attachments(harness: &Harness) -> usize {
    harness.port.attachments.lock().unwrap().len()
}

/// Opened without Closed keeps the opener segment across a restart; the durable Closed frees it.
#[tokio::test(start_paused = true)]
async fn image_open_span_keeps_opener_until_closed_across_restart() {
    let (harness, path, source) = switched_over(b"");
    harness.gate.acquired();
    let store = || {
        let mut store = harness.channel();
        store.set_limits_for_test(1, u64::MAX);
        store
    };
    let (stop, task) = spawn(writer_over(&harness, store()));
    append(
        &path,
        &[prompt("t1"), png("r1", "c1", 1), png("r2", "c2", 2)].concat(),
    );
    polls(4).await;
    assert_eq!(attachments(&harness), 2);
    assert!(
        harness.channel().ledger().gc_segments(&source).is_empty(),
        "the open turn keeps its opener"
    );
    halt(stop, task).await;
    let (stop, task) = spawn(writer_over(&harness, store()));
    polls(4).await;
    assert_eq!(attachments(&harness), 2, "a restart reposts nothing");
    assert!(
        harness.channel().ledger().gc_segments(&source).is_empty(),
        "a span without Closed keeps it"
    );
    append(&path, &idle());
    polls(4).await;
    let reopened = harness.channel();
    let span = reopened
        .ledger()
        .tool_images()
        .span_of(&source, "t1")
        .cloned();
    assert!(span.and_then(|span| span.end_offset).is_some());
    assert!(
        !reopened.ledger().gc_segments(&source).is_empty(),
        "the durable Closed frees the opener"
    );
    assert_eq!(harness.alarms.taken(), []);
    halt(stop, task).await;
}

/// A physical end closes t1 at its cutoff; replaying the retained tail after the opener is gone
/// keeps t1's image settled, and the next prompt's same image is t2's own.
#[tokio::test(start_paused = true)]
async fn image_closed_physical_turn_tail_replay_after_opener_gc_then_new_turn() {
    let (harness, path, source) = switched_over(b"");
    harness.gate.acquired();
    let mut writer = harness.writer();
    let mut live = images(&mut writer);
    let head = [prompt("t1"), png("r1", "c1", 1)].concat();
    append(&path, &[head.clone(), png("r2", "c2", 1)].concat());
    let derived = derive(&mut writer, &mut live, &source, 0);
    deliver_all(&mut writer, &derived).await;
    let cutoff = path.metadata().unwrap().len();
    let last = CapturedRecord {
        start: head.len() as u64,
        end: cutoff,
        line: Vec::new(),
    };
    let ended = Ended {
        source: source.clone(),
        record: last,
        release: Default::default(),
    };
    assert!(live.end_projection(&ended).is_empty());
    writer.record_images(&mut live);
    let span = writer
        .store()
        .ledger()
        .tool_images()
        .span_of(&source, "t1")
        .cloned();
    assert_eq!(span.unwrap().end_offset, Some(cutoff));
    // The same end replayed by a fresh deriver is already durable and queues nothing.
    let mut again = images(&mut writer);
    let records = spooled(&mut writer, &source, 0);
    run(&mut writer, &mut again, &source, &records);
    assert!(again.end_projection(&ended).is_empty());
    assert!(again.images().unwrap().take_entries().is_empty());
    // The opener's segment is gone: the restarted deriver reads from r2.
    append(
        &path,
        &[prompt("t2"), png("r3", "c3", 1), png("r4", "c4", 1)].concat(),
    );
    spool(&mut writer, &source, cutoff);
    let mut restarted = images(&mut writer);
    let tail = spooled(&mut writer, &source, head.len() as u64);
    let tail = run(&mut writer, &mut restarted, &source, &tail);
    assert_eq!(markers(&tail), 1, "only t2's image is new");
    deliver_all(&mut writer, &tail).await;
    assert_eq!(attachments(&harness), 2);
    assert!(writer.store().ledger().violation().is_none());
}

/// Reads the transcript from `from` without spooling.
fn derive_from(
    deriver: &mut UnitDeriver,
    path: &std::path::Path,
    source: &SourceId,
    from: u64,
) -> Vec<Derived> {
    let raw = std::fs::read(path).unwrap();
    let mut out = Vec::new();
    let mut start = from;
    for row in raw[from as usize..].split_inclusive(|b| *b == b'\n') {
        let end = start + row.len() as u64;
        deriver.enter_source(source);
        out.extend(deriver.derive(&CapturedRecord {
            start,
            end,
            line: row.trim_ascii_end().to_vec(),
        }));
        start = end;
    }
    out
}

/// An Unresolved image found behind a newer body posts nothing again, keeps the body's anchor
/// and leaves the channel running, now and on replay.
#[tokio::test(start_paused = true)]
async fn unresolved_image_reconfirmed_behind_newer_anchor_without_stopping() {
    let (harness, path, source) = switched_over(b"");
    harness.gate.acquired();
    let mut writer = harness.writer();
    let mut deriver = images(&mut writer);
    append(&path, &[prompt("t1"), png("r1", "c1", 1)].concat());
    let derived = derive(&mut writer, &mut deriver, &source, 0);
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::CreatedUnseen);
    harness.port.unreadable.store(true, Ordering::SeqCst);
    deliver_all(&mut writer, &derived).await;
    let key = image_key(&mut writer);
    let (serial, _) = writer.store().ledger().latest_piece(&key, 0).unwrap();
    assert!(matches!(
        writer.store().ledger().piece(serial).unwrap().outcome,
        Some(PieceOutcome::Unresolved(_))
    ));
    harness.port.unreadable.store(false, Ordering::SeqCst);
    harness.port.id_gap.store(40_000 << 22, Ordering::SeqCst);
    assert_eq!(writer.deliver(&piece("m1", "later body")).await, Step::Done);
    let body_anchor = writer.store().ledger().anchor();
    let later = Utc::now() + chrono::Duration::minutes(6);
    assert_eq!(writer.select_images(later).await, Step::Done);
    let ledger = writer.store().ledger();
    assert_eq!(
        ledger.piece(serial).unwrap().outcome,
        Some(PieceOutcome::Posted(1000))
    );
    assert_eq!((ledger.anchor(), ledger.violation()), (body_anchor, None));
    assert_eq!(attachments(&harness), 1);
    assert_eq!(writer.deliver(&piece("m2", "next body")).await, Step::Done);
    assert!(harness.port.posts().contains(&"next body".to_owned()));
    let mut replayed = harness.writer();
    assert!(!replayed.is_stopped());
    let ledger = replayed.store().ledger();
    assert_eq!(
        ledger.piece(serial).unwrap().outcome,
        Some(PieceOutcome::Posted(1000))
    );
    assert_eq!(ledger.violation(), None);
}

fn image_key(writer: &mut Writer) -> UnitKey {
    let ledger = writer.store().ledger();
    let open = ledger.tool_images().open_obligations(ledger);
    let key = open.into_iter().find_map(|open| match open {
        crate::services::tui_o::store::ledger::tool_images::OpenObligation::Image {
            key, ..
        } => Some(key),
        _ => None,
    });
    key.expect("one open image")
}

/// 400 is the image's own refusal: omitted with a notice, channel running. 403 refuses the
/// channel: the same blocked stop a body gets, before and after a restart.
#[tokio::test(start_paused = true)]
async fn image_refused_400_omits_with_notice_and_403_blocks_like_body() {
    for status in [400, 403] {
        let (harness, path, source) = switched_over(b"");
        harness.gate.acquired();
        let mut writer = harness.writer();
        let mut deriver = images(&mut writer);
        append(&path, &[prompt("t1"), png("r1", "c1", 1)].concat());
        let derived = derive(&mut writer, &mut deriver, &source, 0);
        harness
            .port
            .replies
            .lock()
            .unwrap()
            .push_back(Reply::Refused(status));
        for item in &derived {
            writer.deliver(item).await;
        }
        let blocked = harness
            .alarms
            .taken()
            .contains(&WriterAlarm::Blocked { status });
        assert_eq!(
            (writer.is_stopped(), blocked),
            (status == 403, status == 403),
            "{status}"
        );
        if status == 400 {
            assert_eq!(writer.select_images(Utc::now()).await, Step::Done);
            let notice = harness.port.posts().last().cloned().unwrap();
            assert!(
                notice.starts_with("-# 이미지 1장 생략(형식 미지원 1)"),
                "{notice}"
            );
            assert_eq!(open_count(writer.store().ledger()), 0);
        }
        assert_eq!(
            harness.writer().is_stopped(),
            status == 403,
            "{status} after restart"
        );
    }
}

/// An unsupported image's notice: Unresolved is re-settled, NotFound retried up to three
/// counted attempts, then kept open; the count survives a restart.
#[tokio::test(start_paused = true)]
async fn notice_unresolved_reconfirmed_then_capped_and_kept_across_restart() {
    let (harness, path, source) = switched_over(b"");
    harness.gate.acquired();
    let mut writer = harness.writer();
    assert_eq!(writer.deliver(&piece("m0", "proof")).await, Step::Done);
    let mut deriver = images(&mut writer);
    append(
        &path,
        &[prompt("t1"), result("r1", "c1", 1, "image/tiff")].concat(),
    );
    let derived = derive(&mut writer, &mut deriver, &source, 0);
    assert_eq!(
        markers(&derived),
        0,
        "an unsupported image is omitted at derivation"
    );
    deliver_all(&mut writer, &derived).await;
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .extend([Reply::Unsent, Reply::Unsent, Reply::Unsent]);
    harness.port.unreadable.store(true, Ordering::SeqCst);
    let mut now = Utc::now();
    writer.select_images(now).await;
    harness.port.unreadable.store(false, Ordering::SeqCst);
    for _ in 0..4 {
        now += chrono::Duration::minutes(6);
        writer.select_images(now).await;
    }
    let notices = harness
        .port
        .posts()
        .iter()
        .filter(|post| post.starts_with("-# 이미지 1장"))
        .count();
    assert_eq!(
        notices, 3,
        "three counted attempts, the first re-settled rather than reposted"
    );
    assert_eq!(
        open_count(writer.store().ledger()),
        1,
        "an exhausted notice stays owed"
    );
    let mut restarted = harness.writer();
    assert_eq!(
        restarted
            .select_images(now + chrono::Duration::hours(1))
            .await,
        Step::Done
    );
    assert_eq!(harness.port.posts().len(), 4);
    assert_eq!(open_count(restarted.store().ledger()), 1);
}

/// A notice retry whose post history shows twice stays Ambiguous: open, never retried.
#[tokio::test(start_paused = true)]
async fn ambiguous_notice_stays_open_without_another_post() {
    let (harness, path, source) = switched_over(b"");
    harness.gate.acquired();
    let mut writer = harness.writer();
    assert_eq!(writer.deliver(&piece("m0", "proof")).await, Step::Done);
    let mut deriver = images(&mut writer);
    append(
        &path,
        &[prompt("t1"), result("r1", "c1", 1, "image/tiff")].concat(),
    );
    let derived = derive(&mut writer, &mut deriver, &source, 0);
    deliver_all(&mut writer, &derived).await;
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .extend([Reply::Unsent, Reply::CreatedUnseen]);
    let mut now = Utc::now();
    for _ in 0..4 {
        writer.select_images(now).await;
        now += chrono::Duration::minutes(6);
    }
    let ledger = writer.store().ledger();
    let latest = (0..ledger.next_serial())
        .filter_map(|s| ledger.piece(s))
        .last()
        .unwrap();
    assert!(matches!(latest.outcome, Some(PieceOutcome::Ambiguous(_))));
    assert_eq!((harness.port.posts().len(), open_count(ledger)), (3, 1));
}

/// Image, its notice and a body all ready: the ledger's one open Prepared holds, so nothing
/// trips a violation and each posts once.
#[tokio::test(start_paused = true)]
async fn image_notice_and_body_share_one_prepared() {
    let (harness, path, _) = switched_over(b"");
    harness.gate.acquired();
    let (stop, task) = spawn(harness.writer());
    let two = result_of(
        "r1",
        "c1",
        vec![image(1, "image/png"), image(2, "image/tiff")],
    );
    append(
        &path,
        &[prompt("t1"), two, super::actor::row("m1", "after")].concat(),
    );
    polls(6).await;
    assert_eq!(attachments(&harness), 1);
    let posts = harness.port.posts();
    assert_eq!(
        posts
            .iter()
            .filter(|post| post.starts_with("-# 이미지 1장 생략"))
            .count(),
        1
    );
    assert!(posts.contains(&"after".to_owned()));
    assert!(
        !harness
            .alarms
            .taken()
            .iter()
            .any(|alarm| matches!(alarm, WriterAlarm::LedgerViolation { .. }))
    );
    halt(stop, task).await;
}

/// A crash after the capture cursor but before the queued entries reach the ledger re-derives
/// the same turn, key and obligation; GC never runs while entries are queued.
#[tokio::test(start_paused = true)]
async fn crash_before_opened_append_rederives_same_turn_key_and_obligation() {
    let (harness, path, source) = switched_over(b"");
    let mut writer = harness.writer();
    let mut lost = images(&mut writer);
    append(&path, &[prompt("t1"), png("r1", "c1", 1)].concat());
    for record in &spool(&mut writer, &source, 0) {
        lost.enter_source(&source);
        lost.derive(record);
    }
    assert!(
        kept_sources(writer.store().ledger(), &mut lost).is_none(),
        "queued entries hold GC"
    );
    let first = lost.images().unwrap().take_entries();
    assert_eq!(first.len(), 2, "Opened, then the obligation");
    let mut writer = harness.writer();
    let mut again = images(&mut writer);
    for record in &spooled(&mut writer, &source, 0) {
        again.enter_source(&source);
        again.derive(record);
    }
    assert_eq!(again.images().unwrap().take_entries(), first);
}

/// A crash after the turn's end but before its Closed is durable keeps the opener; replay
/// closes the same span.
#[tokio::test(start_paused = true)]
async fn crash_before_closed_append_keeps_opener_then_replay_closes() {
    let (harness, path, source) = switched_over(b"");
    harness.gate.acquired();
    let mut writer = harness.writer();
    let mut live = images(&mut writer);
    append(&path, &[prompt("t1"), png("r1", "c1", 1)].concat());
    let derived = derive(&mut writer, &mut live, &source, 0);
    deliver_all(&mut writer, &derived).await;
    let at = path.metadata().unwrap().len();
    append(&path, &idle());
    let end = spool(&mut writer, &source, at);
    live.enter_source(&source);
    live.derive(&end[0]);
    assert!(
        live.images().unwrap().has_queued(),
        "Closed is queued, not yet durable"
    );
    drop(live);
    let key = crate::services::tui_o::store::spool::source_key(&source);
    let mut fresh = images(&mut writer);
    assert!(
        kept_sources(writer.store().ledger(), &mut fresh)
            .unwrap()
            .contains(&key)
    );
    let records = spooled(&mut writer, &source, 0);
    run(&mut writer, &mut fresh, &source, &records);
    let span = writer
        .store()
        .ledger()
        .tool_images()
        .span_of(&source, "t1")
        .cloned()
        .unwrap();
    assert_eq!(span.end_offset, Some(at));
    let mut after = images(&mut writer);
    assert!(
        !kept_sources(writer.store().ledger(), &mut after)
            .unwrap()
            .contains(&key)
    );
}

/// Results without images derive exactly as a deriver without the image run and queue nothing.
#[test]
fn image_less_results_derive_unchanged_and_queue_nothing() {
    let (harness, path, source) = switched_over(b"");
    let mut writer = harness.writer();
    let text = line(
        json!({"type": "user", "uuid": "r1", "message": {"role": "user", "content": [
        {"type": "tool_result", "tool_use_id": "c1", "content": [{"type": "text", "text": "ok"}]}]}}),
    );
    append(
        &path,
        &[prompt("t1"), text, super::actor::row("m1", "done"), idle()].concat(),
    );
    let mut plain = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    let mut with = images(&mut writer);
    let a = derive_from(&mut plain, &path, &source, 0);
    let b = derive_from(&mut with, &path, &source, 0);
    assert_eq!(a, b);
    assert!(with.images().unwrap().take_entries().is_empty());
}
