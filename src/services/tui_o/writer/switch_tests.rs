use std::path::Path;

use super::*;
use crate::services::tui_o::store::rotation::{Boundary, ResolveFrom, Rotation, SourceLink};
use crate::services::tui_o::store::spool::source_key;
use crate::services::tui_o::store::{CURSOR_DIR, StoreError};
use crate::services::tui_o::writer::binding::BindingLog;
use crate::services::tui_o::writer::switch::{Excluded, begin_era_checked};
use crate::services::tui_prompt_dedupe::binding_events as p5;

fn init(channel: u64, sources: &[&SourceId]) -> Initialized {
    let init_source = |source: &&SourceId| InitSource {
        source_id: (*source).clone(),
        delivery_start: 0,
        prefix_hash: hex::encode(Sha256::digest(b"")),
    };
    Initialized {
        channel,
        sources: sources.iter().map(init_source).collect(),
        initial_anchor: 100,
        build_digest: "b".into(),
        at: Utc::now(),
    }
}

fn startup_on(channel: u64, seq: u64, source: &SourceId) -> BindingEvent {
    let target = BindingTarget::Source(source.clone());
    let mut event = bound(seq, None, target, BindingCause::Startup, None);
    event.channel_id = channel;
    event
}

#[test]
fn the_era_leaves_out_and_reports_channels_without_a_binding_baseline() {
    let runtime = tempfile::tempdir().unwrap();
    let (_, a) = transcript(&runtime.path().join("t"), "a.jsonl", "sa", b"");
    let (_, b) = transcript(&runtime.path().join("t"), "b.jsonl", "sb", b"");
    let (_, x) = transcript(&runtime.path().join("t"), "x.jsonl", "sx", b"");
    let bindings = FakeBindings::new();
    bindings.commit(startup_on(7, 1, &a));
    bindings.commit(startup_on(8, 1, &x));
    let inits = |channel| match channel {
        7 => Ok(init(7, &[&a])),
        8 => Ok(init(8, &[&b])),
        _ => Ok(init(channel, &[])),
    };
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, runtime.path())
        .unwrap()
        .unwrap();
    let (era, excluded) =
        begin_era_checked(&store, &[7, 8, 9], Utc::now(), &bindings, inits).unwrap();
    assert_eq!(era.initial_channels, [7]);
    let out = |channel, reason: &str| Excluded {
        channel,
        reason: reason.into(),
    };
    let expected = [
        out(8, "no binding event binds a source attached at the switch"),
        out(9, "no source is attached at the switch"),
    ];
    assert_eq!(excluded, expected);
    assert!(store.read_init(7).unwrap().is_some());
    assert!(store.read_init(8).unwrap().is_none() && store.read_init(9).unwrap().is_none());
    let again = begin_era_checked(&store, &[7, 8, 9], Utc::now(), &bindings, inits).unwrap();
    assert_eq!(
        again,
        (era, Vec::new()),
        "a sealed era is not checked again"
    );
    let unreadable = tempfile::tempdir().unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, unreadable.path())
        .unwrap()
        .unwrap();
    bindings.fail(Some("no log"));
    let (era, excluded) = begin_era_checked(&store, &[7], Utc::now(), &bindings, inits).unwrap();
    assert!(era.initial_channels.is_empty());
    assert_eq!(excluded, [out(7, "binding log unreadable: no log")]);
}

/// Points the P5 log at `root` for this thread and writes `lines` as channel 7's log.
fn p5_log(root: &Path, lines: &[Vec<u8>]) {
    p5_log_on(CHANNEL, root, lines);
}

fn p5_log_on(channel: u64, root: &Path, lines: &[Vec<u8>]) {
    p5::set_test_root(Some(root));
    let dir = root.join(p5::BINDING_EVENTS_DIR);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{channel}.log"));
    std::fs::write(path, lines.concat()).unwrap();
}

fn p5_line(seq: u64, provider: &str, new: p5::BindingTarget) -> Vec<u8> {
    p5_line_on(CHANNEL, seq, provider, new)
}

fn p5_line_on(channel: u64, seq: u64, provider: &str, new: p5::BindingTarget) -> Vec<u8> {
    let event = p5::BindingEvent {
        seq,
        channel_id: channel,
        provider: provider.into(),
        tmux_session: "tmux".into(),
        execution_nonce: None,
        old: None,
        new,
        cause: p5::BindingCause::Startup,
        parent_hint: None,
        evidence: p5::BindingEvidence {
            hook_event: None,
            received_at: Utc::now(),
        },
        committed_at: Utc::now(),
    };
    let mut line = serde_json::to_vec(&event).unwrap();
    line.push(b'\n');
    line
}

#[test]
fn the_binding_log_port_reads_p5_events_and_refuses_what_it_cannot_carry() {
    let root = tempfile::tempdir().unwrap();
    let (_, a) = transcript(&root.path().join("t"), "a.jsonl", "sa", b"");
    let pending = p5::BindingTarget::Pending {
        payload_session_id: "sb".into(),
        payload_transcript_path: None,
    };
    let resolved = p5::BindingTarget::Resolved {
        pending_seq: 2,
        source: a.clone(),
    };
    let lines = [
        p5_line(1, "claude", p5::BindingTarget::Source(a.clone())),
        p5_line(2, "codex", pending),
        p5_line(3, "codex", resolved),
    ];
    p5_log(root.path(), &lines);
    let events = BindingLog.binding_events_since(CHANNEL, 0).unwrap();
    let records: Vec<_> = events
        .iter()
        .map(|e| (e.seq, e.provider, e.record.clone()))
        .collect();
    assert!(
        matches!(records[0], (1, ShadowProvider::Claude, BindingRecord::Bound { new: BindingTarget::Source(ref s), cause: BindingCause::Startup, .. }) if *s == a)
    );
    assert!(
        matches!(records[1], (2, ShadowProvider::Codex, BindingRecord::Bound { new: BindingTarget::Pending { ref payload_session_id, .. }, .. }) if payload_session_id == "sb")
    );
    assert!(
        matches!(records[2], (3, _, BindingRecord::Resolved { resolves_seq: 2, ref source }) if *source == a)
    );
    assert_eq!(
        BindingLog.binding_events_since(CHANNEL, 2).unwrap().len(),
        1
    );
    p5_log(
        root.path(),
        &[
            lines[0].clone(),
            p5_line(2, "qwen", p5::BindingTarget::Source(a)),
        ],
    );
    let unknown = BindingLog.binding_events_since(CHANNEL, 0).unwrap_err();
    assert!(unknown.contains("qwen"), "{unknown}");
    p5_log(root.path(), &[lines[0].clone(), b"{not json\n".to_vec()]);
    let corrupt = BindingLog.binding_events_since(CHANNEL, 0).unwrap_err();
    assert!(corrupt.contains("unreadable"), "{corrupt}");
    p5::forget_channel_for_tests(CHANNEL);
    p5::set_test_root(None);
}

/// No other test maps a pane to this channel. The actor judges a pane mapped to its channel
/// against this thread's corrupt log, and that judgment holds its delivery.
const CORRUPT_LOG_CHANNEL: u64 = 7_017;

#[tokio::test(start_paused = true)]
async fn a_corrupt_binding_log_alarms_through_the_port_and_capture_waits_for_a_clean_read() {
    let channel = CORRUPT_LOG_CHANNEL;
    let (harness, a_path, a) = switched_over_on(channel, &row("m0", "before the switch"));
    harness.gate.acquired();
    let root = a_path.parent().unwrap().join("p5");
    let startup = p5_line_on(channel, 1, "claude", p5::BindingTarget::Source(a));
    p5_log_on(
        channel,
        &root,
        &[startup.clone(), b"{torn middle\n".to_vec()],
    );
    let bindings = Arc::new(BindingLog);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
    append(&a_path, &row("m1", "after the switch"));
    polls(3).await;
    assert!(harness.port.posts().is_empty());
    let alarms = harness.alarms.taken();
    let unavailable = |a: &WriterAlarm| matches!(a, WriterAlarm::BindingLogUnavailable { checkpoint: None, detail } if detail.contains("unreadable"));
    assert!(
        matches!(alarms.as_slice(), [a] if unavailable(a)),
        "{alarms:?}"
    );
    p5_log_on(channel, &root, &[startup]);
    polls(3).await;
    assert_eq!(harness.port.posts(), ["after the switch"]);
    assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(1));
    halt(stop, task).await;
    p5::forget_channel_for_tests(channel);
    p5::set_test_root(None);
}

#[tokio::test(start_paused = true)]
async fn an_operator_resolution_is_applied_once_the_writer_restarts_and_only_from_its_record() {
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    // No row of the bound source is the parent's, so its start waits for the operator.
    let body = row_at("x1", "unrelated", Utc::now());
    let (b_path, b) = transcript(&a_path, "b.jsonl", "s2", &body);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    bindings.commit(rotate(2, &a, &b, BindingCause::Resume));
    polls(3).await;
    append(&b_path, &row_at("x2", "after", Utc::now()));
    polls(3).await;
    halt(stop, task).await;
    assert!(harness.port.posts().is_empty());
    let pending = WriterAlarm::BoundaryPending { source: b.clone() };
    assert_eq!(harness.alarms.taken(), [pending]);
    let path = b_path.display().to_string();
    let resolve = |from: ResolveFrom| {
        harness
            .store
            .record_boundary_resolved(CHANNEL, &path, &from, "op")
    };
    let refused = |from| matches!(resolve(from), Err(StoreError::Rejected(_)));
    assert!(
        refused(ResolveFrom::Offset(body.len() as u64 + 1)),
        "mid-record offset"
    );
    assert!(refused(ResolveFrom::Uuid("u-none".into())));
    let ledger = a_path
        .parent()
        .unwrap()
        .join("o_store")
        .join(CHANNEL.to_string())
        .join("ledger.jsonl");
    let clean = std::fs::metadata(&ledger).unwrap().len();
    append(&ledger, b"{\"at\":");
    assert!(
        refused(ResolveFrom::Uuid("u-x2".into())),
        "unfinished ledger tail"
    );
    std::fs::OpenOptions::new()
        .write(true)
        .open(&ledger)
        .unwrap()
        .set_len(clean)
        .unwrap();
    let recorded = resolve(ResolveFrom::Uuid("u-x2".into())).unwrap();
    assert_eq!(recorded, (b.clone(), body.len() as u64));
    assert!(refused(ResolveFrom::Offset(0)), "a source is resolved once");
    let on_disk = harness
        .channel()
        .rotation()
        .unwrap()
        .link(&b)
        .unwrap()
        .boundary
        .clone();
    assert!(
        matches!(on_disk, Boundary::Pending { .. }),
        "nothing applies without the writer"
    );
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(3).await;
    assert_eq!(harness.port.posts(), ["after"]);
    assert_eq!(harness.alarms.taken(), []);
    let applied = harness
        .channel()
        .rotation()
        .unwrap()
        .link(&b)
        .unwrap()
        .boundary
        .clone();
    assert_eq!(
        applied,
        Boundary::Owed {
            from: body.len() as u64
        }
    );
    halt(stop, task).await;
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
    polls(3).await;
    assert_eq!(harness.port.posts(), ["after"], "a restart reposts nothing");
    halt(stop, task).await;
}

/// Spools what `source` holds past its cursor. `lag` restores the cursor file, as a crash between
/// the frame and the cursor write leaves it.
pub(super) fn spool(store: &mut ChannelStore, runtime: &Path, source: &SourceId, lag: bool) {
    let dir = runtime
        .join("o_store")
        .join(CHANNEL.to_string())
        .join(CURSOR_DIR);
    let cursor = dir.join(format!("{}.json", source_key(source)));
    let before = std::fs::read(&cursor).ok();
    let from = store.cursor(source).unwrap().captured_through;
    let mut capture = SourceCapture::open(source.clone(), from).unwrap();
    let CaptureOutcome::Batch(batch) = capture.poll(MAX_READ_BYTES) else {
        panic!("capture failed");
    };
    store.append_spool(&batch, &capture.prefix_hash()).unwrap();
    match (lag, before) {
        (false, _) => {}
        (true, Some(before)) => std::fs::write(&cursor, before).unwrap(),
        (true, None) => std::fs::remove_file(&cursor).unwrap(),
    }
}

pub(super) fn link(
    source: &SourceId,
    seq: u64,
    parent: Option<&SourceId>,
    boundary: Boundary,
) -> SourceLink {
    SourceLink {
        source: source.clone(),
        seq,
        parent: parent.cloned(),
        committed_at: Utc::now(),
        boundary,
    }
}

/// The last durable write before a crash while the A→B bind at seq 2 is applied and captured.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
enum Crash {
    Link,
    Cursor,
    Checkpoint,
    OldFrame,
    OldCursor,
    NewFrame,
    NewCursor,
}

#[tokio::test(start_paused = true)]
async fn a_crash_after_each_durable_write_of_a_rotation_resumes_without_loss_or_reorder() {
    use Crash::*;
    for crash in [
        Link, Cursor, Checkpoint, OldFrame, OldCursor, NewFrame, NewCursor,
    ] {
        let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
        let runtime = a_path.parent().unwrap().to_path_buf();
        append(&a_path, &row("m1", "old tail"));
        let (b_path, b) = transcript(&a_path, "b.jsonl", "s2", &row("n1", "new first"));
        bindings.commit(rotate(2, &a, &b, BindingCause::Clear));
        let mut store = harness.channel();
        store.set_binding_checkpoint(1).unwrap();
        let mut rotation = Rotation::default();
        let owed = Boundary::Owed { from: 0 };
        rotation
            .links
            .insert(source_key(&b), link(&b, 2, None, owed));
        rotation.successors.insert(source_key(&a), b.clone().into());
        store.write_rotation(&rotation).unwrap();
        if crash >= Cursor {
            store.attach_source(&b).unwrap();
        }
        if crash >= Checkpoint {
            store.set_binding_checkpoint(2).unwrap();
        }
        if crash >= OldFrame {
            spool(&mut store, &runtime, &a, crash == OldFrame);
        }
        if crash >= NewFrame {
            let store = &mut harness.channel();
            spool(store, &runtime, &b, crash == NewFrame);
        }
        drop(store);
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
        polls(3).await;
        assert_eq!(harness.port.posts(), ["old tail", "new first"], "{crash:?}");
        assert_eq!(harness.alarms.taken(), [], "{crash:?}");
        let store = harness.channel();
        assert_eq!(store.binding_checkpoint().unwrap(), Some(2), "{crash:?}");
        let b_len = std::fs::metadata(&b_path).unwrap().len();
        assert_eq!(
            store.cursor(&b).unwrap().captured_through,
            b_len,
            "{crash:?}"
        );
        halt(stop, task).await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_restart_replays_the_replaced_source_before_the_source_bound_back() {
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    let runtime = a_path.parent().unwrap().to_path_buf();
    let (b_path, b) = transcript(&a_path, "b.jsonl", "s2", &row("n1", "b first"));
    bindings.commit(rotate(2, &a, &b, BindingCause::Clear));
    bindings.commit(rotate(3, &b, &a, BindingCause::Resume));
    let mut store = harness.channel();
    let mut rotation = Rotation::default();
    let owed = Boundary::Owed { from: 0 };
    rotation
        .links
        .insert(source_key(&b), link(&b, 2, None, owed));
    rotation.successors.insert(source_key(&b), a.clone().into());
    store.write_rotation(&rotation).unwrap();
    store.attach_source(&b).unwrap();
    store.set_binding_checkpoint(3).unwrap();
    append(&b_path, &row("n2", "b tail"));
    spool(&mut store, &runtime, &b, false);
    append(&a_path, &row("m1", "a resumed"));
    spool(&mut store, &runtime, &a, false);
    drop(store);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
    polls(3).await;
    assert_eq!(harness.port.posts(), ["b first", "b tail", "a resumed"]);
    assert_eq!(harness.alarms.taken(), []);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn a_restart_holds_the_new_source_behind_an_old_backlog_even_mid_read() {
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    let thinking = |i: u32| {
        let text = "x".repeat(256 << 10);
        codex_line(
            serde_json::json!({"type": "assistant", "uuid": format!("u-t{i}"),
            "apiBlockIndex": 0, "message": {"id": format!("t{i}"),
            "content": [{"type": "thinking", "thinking": text}]}}),
        )
    };
    let backlog: Vec<u8> = (0..6).flat_map(thinking).collect();
    append(&a_path, &[backlog, row("m1", "old last")].concat());
    let (_, b) = transcript(&a_path, "b.jsonl", "s2", &row("n1", "new first"));
    bindings.commit(rotate(2, &a, &b, BindingCause::Clear));
    let mut store = harness.channel();
    let mut rotation = Rotation::default();
    let owed = Boundary::Owed { from: 0 };
    rotation
        .links
        .insert(source_key(&b), link(&b, 2, None, owed));
    rotation.successors.insert(source_key(&a), b.clone().into());
    store.write_rotation(&rotation).unwrap();
    store.attach_source(&b).unwrap();
    store.set_binding_checkpoint(2).unwrap();
    drop(store);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    tokio::time::sleep(POLL_INTERVAL / 2).await;
    halt(stop, task).await;
    let a_len = std::fs::metadata(&a_path).unwrap().len();
    let store = harness.channel();
    assert!(
        store.cursor(&a).unwrap().captured_through < a_len,
        "stopped mid-read"
    );
    assert_eq!(store.cursor(&b).unwrap().captured_through, 0);
    drop(store);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
    polls(4).await;
    assert_eq!(harness.port.posts(), ["old last", "new first"]);
    assert_eq!(harness.alarms.taken(), []);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn a_crash_before_a_decided_boundary_is_written_decides_it_again_from_the_spool() {
    let rows = |range: std::ops::Range<u32>| -> Vec<u8> {
        range
            .flat_map(|i| row(&format!("m{i}"), &format!("row {i}")))
            .collect()
    };
    let (harness, a_path, a, bindings) = started(&rows(0..8));
    let runtime = a_path.parent().unwrap().to_path_buf();
    let inherited = rows(0..8);
    let body = [inherited.clone(), row_at("n1", "forked new", Utc::now())].concat();
    let (_, b) = transcript(&a_path, "b.jsonl", "s2", &body);
    bindings.commit(rotate(2, &a, &b, BindingCause::Fork));
    let mut store = harness.channel();
    let mut rotation = Rotation::default();
    let undecided = link(&b, 2, Some(&a), Boundary::Undecided);
    rotation.links.insert(source_key(&b), undecided);
    rotation.successors.insert(source_key(&a), b.clone().into());
    store.write_rotation(&rotation).unwrap();
    store.attach_source(&b).unwrap();
    store.set_binding_checkpoint(2).unwrap();
    spool(&mut store, &runtime, &b, false);
    drop(store);
    for _ in 0..2 {
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        polls(3).await;
        assert_eq!(harness.port.posts(), ["forked new"]);
        assert_eq!(harness.alarms.taken(), []);
        let decided = harness
            .channel()
            .rotation()
            .unwrap()
            .link(&b)
            .unwrap()
            .boundary
            .clone();
        assert_eq!(
            decided,
            Boundary::Owed {
                from: inherited.len() as u64
            }
        );
        halt(stop, task).await;
    }
}
