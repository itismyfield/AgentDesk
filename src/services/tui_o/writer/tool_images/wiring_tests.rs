//! Tool images through the real deriver, ledger and writer: turn spans across restarts and GC,
//! late confirmation, refusals, notices and the shared Prepared.

use base64::Engine as _;
use serde_json::json;

use super::actor::{append, halt, polls, spawn, switched_over, writer_over};
use super::*;
use crate::services::tui_o::shadow::capture::SourceCapture;
use crate::services::tui_o::shadow::{CaptureOutcome, MAX_READ_BYTES, SourceId};
use crate::services::tui_o::writer::actor::Actor;
use crate::services::tui_o::writer::binding::ChannelBindingLog;
use crate::services::tui_o::writer::restart_end::Ended;
use crate::services::tui_o::writer::rotation::Sources;
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

/// GC may collect an open turn's opener before its first image: it records the turn's span
/// first, so a restart keeps the image on that turn and reposts nothing.
#[tokio::test(start_paused = true)]
async fn image_turn_keeps_its_identity_after_opener_gc_and_restart() {
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
        &[prompt("t1"), super::actor::row("m1", "reading")].concat(),
    );
    polls(4).await;
    let ledger = harness.channel();
    assert!(
        !ledger.ledger().gc_segments(&source).is_empty(),
        "the opener is collected"
    );
    let span = ledger
        .ledger()
        .tool_images()
        .span_of(&source, "t1")
        .cloned();
    assert_eq!(
        span.map(|span| span.opener_offset),
        Some(0),
        "its span is durable first"
    );
    // Restart before the turn's first image: only the span names the turn now.
    halt(stop, task).await;
    let (stop, task) = spawn(writer_over(&harness, store()));
    let rows = [png("r1", "c1", 1), prompt("mid"), png("r2", "c2", 1)];
    append(&path, &rows.concat());
    polls(4).await;
    assert_eq!(
        attachments(&harness),
        1,
        "the same image of the same turn posts once"
    );
    append(&path, &idle());
    polls(4).await;
    let reopened = harness.channel();
    let images = reopened.ledger().tool_images();
    assert!(
        images
            .span_of(&source, "t1")
            .is_some_and(|span| span.end_offset.is_some())
    );
    assert!(images.span_of(&source, "mid").is_none());
    assert!(reopened.ledger().violation().is_none());
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
    // The opener's segment is gone: the restarted deriver starts at the second result.
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
/// and leaves the channel running, now and on replay. One that never arrived is NotFound past
/// that body, and ends in one omission notice after its allowed retry.
#[tokio::test(start_paused = true)]
async fn unresolved_image_reconfirmed_behind_newer_anchor_without_stopping() {
    for arrived in [true, false] {
        reconfirmed_behind_newer_anchor(arrived).await;
    }
}

async fn reconfirmed_behind_newer_anchor(arrived: bool) {
    let (harness, path, source) = switched_over(b"");
    harness.gate.acquired();
    let mut writer = harness.writer();
    let mut deriver = images(&mut writer);
    append(&path, &[prompt("t1"), png("r1", "c1", 1)].concat());
    let derived = derive(&mut writer, &mut deriver, &source, 0);
    let first = if arrived {
        Reply::CreatedUnseen
    } else {
        Reply::Unsent
    };
    harness.port.replies.lock().unwrap().push_back(first);
    harness.port.unreadable.store(true, Ordering::SeqCst);
    deliver_all(&mut writer, &derived).await;
    let key = image_key(&mut writer);
    let (serial, _) = writer.store().ledger().latest_piece(&key, 0).unwrap();
    assert!(matches!(
        writer.store().ledger().piece(serial).unwrap().outcome,
        Some(PieceOutcome::Unresolved(_))
    ));
    let bindings = std::sync::Arc::new(ChannelBindingLog::new(CHANNEL, ShadowProvider::Claude));
    let sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings.clone());
    let mut actor = Actor::new_for_test(writer, deriver, sources, bindings);
    // What a child retirement reads: the open image is owed until it settles.
    assert_eq!(actor.undelivered_for_test(1).unwrap().images, 1);
    let writer = actor.writer_mut_for_test();
    harness.port.unreadable.store(false, Ordering::SeqCst);
    harness.port.id_gap.store(40_000 << 22, Ordering::SeqCst);
    assert_eq!(writer.deliver(&piece("m1", "later body")).await, Step::Done);
    let body_anchor = writer.store().ledger().anchor();
    let later = Utc::now() + chrono::Duration::minutes(6);
    assert_eq!(writer.select_images(later).await, Step::Done);
    if !arrived {
        harness
            .port
            .replies
            .lock()
            .unwrap()
            .push_back(Reply::Unsent);
        for minutes in [12, 18, 24] {
            writer
                .select_images(Utc::now() + chrono::Duration::minutes(minutes))
                .await;
        }
        let posts = harness.port.posts();
        let notices = posts
            .iter()
            .filter(|post| post.starts_with("-# 이미지 1장 생략"));
        let open = open_count(writer.store().ledger());
        assert_eq!((notices.count(), open, attachments(&harness)), (1, 0, 2));
        assert_eq!(actor.undelivered_for_test(2).unwrap().images, 0);
        return;
    }
    let ledger = writer.store().ledger();
    assert_eq!(
        ledger.piece(serial).unwrap().outcome,
        Some(PieceOutcome::Posted(1000))
    );
    assert_eq!((ledger.anchor(), ledger.violation()), (body_anchor, None));
    assert_eq!(attachments(&harness), 1);
    assert_eq!(writer.deliver(&piece("m2", "next body")).await, Step::Done);
    assert!(harness.port.posts().contains(&"next body".to_owned()));
    assert_eq!(actor.undelivered_for_test(2).unwrap().images, 0);
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
            // Crash after the Rejected append, before the ImageOmitted one: restart completes it.
            let ledger = ledger_path(&harness);
            let text = std::fs::read_to_string(&ledger).unwrap();
            let (prefix, last) = text.trim_end().rsplit_once('\n').unwrap();
            assert!(last.contains("\"image_omitted\""), "{last}");
            std::fs::write(&ledger, format!("{prefix}\n")).unwrap();
            let mut writer = harness.writer();
            assert!(
                !writer.is_stopped(),
                "a 400 prefix is not a blocked channel"
            );
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

/// A crash after the turn's end but before its Closed is durable leaves the span open; replay
/// closes the same span at the same offset.
#[tokio::test(start_paused = true)]
async fn crash_before_closed_append_replays_the_same_close() {
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
    let open = writer
        .store()
        .ledger()
        .tool_images()
        .span_of(&source, "t1")
        .cloned();
    assert_eq!(open.unwrap().end_offset, None);
    let mut fresh = images(&mut writer);
    let records = spooled(&mut writer, &source, 0);
    run(&mut writer, &mut fresh, &source, &records);
    let span = writer
        .store()
        .ledger()
        .tool_images()
        .span_of(&source, "t1")
        .cloned();
    assert_eq!(span.unwrap().end_offset, Some(at));
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
    let rows = [
        prompt("t1"),
        tool_use("a1", "c1"),
        text,
        super::actor::row("m1", "done"),
        idle(),
    ];
    append(&path, &rows.concat());
    let mut plain = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    let mut with = images(&mut writer);
    let a = derive_from(&mut plain, &path, &source, 0);
    let b = derive_from(&mut with, &path, &source, 0);
    assert_eq!(a, b);
    assert!(!b.iter().any(|item| matches!(item, Derived::Blocked { .. })));
    assert!(with.images().unwrap().take_entries().is_empty());
}

/// An opener collected before the turn's first image leaves a span without Closed; a verified
/// end the projection cannot apply closes it even when the retained tail starts at that cut.
#[tokio::test(start_paused = true)]
async fn image_turn_closes_at_a_verified_end_without_its_opener() {
    let (harness, path, source) = switched_over(b"");
    let mut writer = harness.writer();
    let mut live = images(&mut writer);
    append(&path, &[prompt("t1"), png("r1", "c1", 1)].concat());
    derive(&mut writer, &mut live, &source, 0);
    let cutoff = path.metadata().unwrap().len();
    append(&path, &[prompt("t2"), png("r2", "c2", 1)].concat());
    spool(&mut writer, &source, cutoff);
    let mut restarted = images(&mut writer);
    let tail = spooled(&mut writer, &source, cutoff);
    restarted
        .images()
        .unwrap()
        .cut_at(&source, cutoff, Some("t1"));
    let tail = run(&mut writer, &mut restarted, &source, &tail);
    assert_eq!(markers(&tail), 1, "t2's image is its own");
    let ledger = writer.store().ledger();
    let span = ledger.tool_images().span_of(&source, "t1").cloned();
    assert_eq!(span.unwrap().end_offset, Some(cutoff));
    assert!(ledger.violation().is_none());
}

fn tool_use(id: &str, call: &str) -> Vec<u8> {
    line(
        json!({"type": "assistant", "uuid": format!("u-{id}"), "apiBlockIndex": 0, "message": {"id": id,
        "content": [{"type": "tool_use", "id": call, "name": "Read", "input": {}}]}}),
    )
}

/// Replay that begins at a prompt inside an open image turn keeps the durable turn: no new
/// Opened, no ledger violation, and the next image is keyed under the original turn.
#[tokio::test(start_paused = true)]
async fn replay_tail_starting_at_mid_turn_prompt_keeps_durable_turn() {
    let (harness, path, source) = switched_over(b"");
    let mut writer = harness.writer();
    let mut live = images(&mut writer);
    let head = [prompt("t1"), png("r1", "c1", 1)].concat();
    append(
        &path,
        &[head.clone(), prompt("mid"), png("r2", "c2", 2)].concat(),
    );
    let derived = derive(&mut writer, &mut live, &source, 0);
    assert_eq!(markers(&derived), 2);
    let mut restarted = images(&mut writer);
    let tail = spooled(&mut writer, &source, head.len() as u64);
    run(&mut writer, &mut restarted, &source, &tail);
    let ledger = writer.store().ledger();
    assert!(ledger.violation().is_none());
    assert!(ledger.tool_images().span_of(&source, "mid").is_none());
}

/// A result arriving after its turn ended keeps the call's turn once the call, opener and end
/// are collected and the actor restarts: no second obligation and no second upload.
#[tokio::test(start_paused = true)]
async fn late_image_call_turn_survives_call_gc_and_restart() {
    for unresolved in [false, true] {
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
            &[prompt("t1"), tool_use("a1", "c1"), idle()].concat(),
        );
        polls(4).await;
        if unresolved {
            harness
                .port
                .replies
                .lock()
                .unwrap()
                .push_back(Reply::CreatedUnseen);
            harness.port.unreadable.store(true, Ordering::SeqCst);
        }
        append(&path, &png("r1", "c1", 1));
        polls(4).await;
        assert_eq!(attachments(&harness), 1, "{unresolved}");
        halt(stop, task).await;
        let gc = harness.channel().ledger().gc_segments(&source).len();
        assert!(gc >= 3, "call, opener and end are collected ({gc})");
        let (stop, task) = spawn(writer_over(&harness, store()));
        polls(4).await;
        halt(stop, task).await;
        assert_eq!(attachments(&harness), 1, "no second upload ({unresolved})");
        let ledger = harness.channel();
        let ledger = ledger.ledger();
        assert_eq!(ledger.tool_images().call_turn(&source, "c1"), Some("t1"));
        let images = ledger.tool_images().open_obligations(ledger).len();
        assert_eq!(
            images,
            usize::from(unresolved),
            "one obligation, still open only if unresolved"
        );
    }
}

/// An image-less turn's call outlives its opener and call rows (collected) and a restart; the
/// next turn posts the same bytes first, and the late result still gets its own turn and upload.
#[tokio::test(start_paused = true)]
async fn late_result_of_an_image_less_turn_keeps_its_turn_after_the_next_turn_posts_the_same_bytes()
{
    use sha2::{Digest, Sha256};
    let (harness, path, source) = switched_over(b"");
    harness.gate.acquired();
    let store = || {
        let mut store = harness.channel();
        store.set_limits_for_test(1, u64::MAX);
        store
    };
    let (stop, task) = spawn(writer_over(&harness, store()));
    let rows = [
        prompt("u1"),
        tool_use("a1", "c1"),
        super::actor::row("m1", "working"),
    ];
    append(&path, &rows.concat());
    polls(4).await;
    halt(stop, task).await;
    let gc = harness.channel().ledger().gc_segments(&source).len();
    assert!(gc >= 2, "u1's opener and call rows are collected ({gc})");
    let (stop, task) = spawn(writer_over(&harness, store()));
    let u2 = [
        idle(),
        prompt("u2"),
        tool_use("a2", "c2"),
        png("r2", "c2", 1),
    ];
    append(&path, &u2.concat());
    polls(4).await;
    assert_eq!(attachments(&harness), 1, "u2 posts the bytes first");
    append(&path, &png("r1", "c1", 1));
    polls(4).await;
    halt(stop, task).await;
    assert_eq!(
        attachments(&harness),
        2,
        "u1's late image is its own upload"
    );
    let sha = format!("{:x}", Sha256::digest(b"\x89PNG\r\n\x1a\n\x01"));
    let key = |turn: &str| UnitKey {
        channel_id: CHANNEL,
        provider: ShadowProvider::Claude,
        native_key: format!("img:{turn}:{sha}"),
        kind: UnitKind::ToolImage,
    };
    let store = harness.channel();
    let ledger = store.ledger();
    for turn in ["u1", "u2"] {
        let posted = ledger
            .latest_piece(&key(turn), 0)
            .map(|(_, piece)| piece.outcome.clone());
        assert!(
            matches!(posted, Some(Some(PieceOutcome::Posted(_)))),
            "{turn}"
        );
    }
    assert!(ledger.violation().is_none());
}
