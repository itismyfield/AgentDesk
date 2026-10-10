use chrono::{DateTime, TimeDelta, Utc};

use super::*;

fn row_at(id: &str, text: &str, at: DateTime<Utc>) -> Vec<u8> {
    let mut value: serde_json::Value = serde_json::from_slice(&row(id, text)).unwrap();
    value["timestamp"] = at.to_rfc3339().into();
    let mut line = serde_json::to_vec(&value).unwrap();
    line.push(b'\n');
    line
}

fn rotate(seq: u64, old: &SourceId, new: &SourceId, cause: BindingCause) -> BindingEvent {
    let target = BindingTarget::Source(new.clone());
    bound(seq, Some(old), target, cause, Some(old))
}

/// A switched-over channel `a`, whose startup bind is seq 1, and an owned gateway.
fn started(body: &[u8]) -> (Harness, PathBuf, SourceId, Arc<FakeBindings>) {
    let (harness, path, source) = switched_over(body);
    let bindings = Arc::new(FakeBindings::new());
    let target = BindingTarget::Source(source.clone());
    bindings.commit(bound(1, None, target, BindingCause::Startup, None));
    harness.gate.acquired();
    (harness, path, source, bindings)
}

fn transcript(beside: &Path, name: &str, session: &str, body: &[u8]) -> (PathBuf, SourceId) {
    let path = beside.with_file_name(name);
    std::fs::write(&path, body).unwrap();
    let source = source_id_for(session, &path).unwrap();
    (path, source)
}

fn retired(harness: &Harness, source: &SourceId) -> bool {
    harness.channel().cursor(source).unwrap().retired
}

#[tokio::test(start_paused = true)]
async fn an_old_tail_is_posted_before_the_new_source_and_a_proven_old_source_retires_once_drained_and_quiet()
 {
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    append(&a_path, &row("m1", "first"));
    polls(3).await;
    append(&a_path, &row("m2", "old tail"));
    let (b_path, b) = transcript(&a_path, "b.jsonl", "s2", &row("n1", "new first"));
    bindings.commit(rotate(2, &a, &b, BindingCause::Clear));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["first", "old tail", "new first"]);
    assert!(
        !retired(&harness, &a),
        "retirement waits for a quiet old source"
    );
    polls(12).await;
    assert!(retired(&harness, &a) && !retired(&harness, &b));
    append(&a_path, &row("m3", "late old"));
    polls(3).await;
    assert_eq!(harness.port.posts().last().unwrap(), "late old");
    assert!(
        !retired(&harness, &a),
        "a grown retired source is read again"
    );
    let grew = WriterAlarm::RetiredSourceGrew { source: a.clone() };
    assert_eq!(harness.alarms.taken(), [grew]);
    halt(stop, task).await;
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
    append(&b_path, &row("n2", "new second"));
    polls(3).await;
    let posts = harness.port.posts();
    assert_eq!(posts.len(), 5, "a restart reposts nothing: {posts:?}");
    assert_eq!(posts[4], "new second");
    assert_eq!(harness.alarms.taken(), []);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn each_old_source_retires_only_after_its_own_successor_records_something() {
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    let (b_path, b) = transcript(&a_path, "b.jsonl", "s2", b"");
    bindings.commit(rotate(2, &a, &b, BindingCause::Clear));
    polls(15).await;
    assert!(!retired(&harness, &a), "B has recorded nothing yet");
    append(&b_path, &row("n1", "b one"));
    polls(15).await;
    assert!(retired(&harness, &a));
    let (c_path, c) = transcript(&a_path, "c.jsonl", "s3", b"");
    bindings.commit(rotate(3, &b, &c, BindingCause::Clear));
    polls(15).await;
    assert!(!retired(&harness, &b), "C has recorded nothing yet");
    append(&c_path, &row("k1", "c one"));
    polls(15).await;
    assert!(retired(&harness, &b) && !retired(&harness, &c));
    assert_eq!(harness.port.posts(), ["b one", "c one"]);
    assert_eq!(harness.alarms.taken(), []);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn a_fork_posts_only_what_follows_the_eight_rows_it_inherited() {
    let rows = |range: std::ops::Range<u32>| -> Vec<u8> {
        range
            .flat_map(|i| row(&format!("m{i}"), &format!("row {i}")))
            .collect()
    };
    let (harness, a_path, a, bindings) = started(&rows(0..4));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    append(&a_path, &rows(4..8));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["row 4", "row 5", "row 6", "row 7"]);
    let forked = [rows(0..8), row_at("n1", "forked new", Utc::now())].concat();
    let (_, b) = transcript(&a_path, "b.jsonl", "s2", &forked);
    bindings.commit(rotate(2, &a, &b, BindingCause::Fork));
    polls(3).await;
    let posts = harness.port.posts();
    assert_eq!(
        posts[4..],
        ["forked new"],
        "[T3] only the new row is owed: {posts:?}"
    );
    assert_eq!(harness.alarms.taken(), []);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn an_unknown_bind_holds_back_even_a_fresh_source() {
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    let (b_path, b) = transcript(&a_path, "b.jsonl", "s2", b"");
    let target = BindingTarget::Source(b.clone());
    bindings.commit(bound(2, Some(&a), target, BindingCause::Unknown, Some(&a)));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
    append(&b_path, &row_at("n1", "fresh", Utc::now()));
    polls(3).await;
    assert!(harness.port.posts().is_empty());
    assert_eq!(
        harness.alarms.taken(),
        [WriterAlarm::BoundaryPending { source: b }]
    );
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn a_gap_in_the_binding_log_stops_the_channel_at_the_last_applied_seq() {
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    let (_, b) = transcript(&a_path, "b.jsonl", "s2", &row("n1", "new"));
    bindings.commit(rotate(3, &a, &b, BindingCause::Clear));
    let (_stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
    polls(3).await;
    assert!(task.is_finished(), "a stopped channel ends its actor");
    let gap = WriterAlarm::BindingGap {
        expected: 2,
        found: 3,
    };
    assert_eq!(harness.alarms.taken(), [gap]);
    let store = harness.channel();
    assert_eq!(store.binding_checkpoint().unwrap(), Some(1));
    assert!(store.cursor(&b).is_none() && harness.port.posts().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_bind_waiting_for_its_file_alarms_once_late_and_its_resolution_attaches_it() {
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    let pending = BindingTarget::Pending {
        payload_session_id: "s2".into(),
        payload_transcript_path: a_path.with_file_name("b.jsonl"),
    };
    let mut late = bound(2, Some(&a), pending, BindingCause::Clear, None);
    late.committed_at = Utc::now() - TimeDelta::minutes(2);
    bindings.commit(late);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(3).await;
    assert_eq!(
        harness.alarms.taken(),
        [WriterAlarm::BindingPending { seq: 2 }]
    );
    assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(1));
    let (_, b) = transcript(&a_path, "b.jsonl", "s2", &row("n1", "resolved"));
    let resolved = BindingRecord::Resolved {
        resolves_seq: 2,
        source: b.clone(),
    };
    bindings.commit(event(3, resolved, Utc::now()));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["resolved"]);
    assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(3));
    assert_eq!(harness.alarms.taken(), []);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn a_long_growing_old_source_and_a_fourth_reader_alarm_while_every_row_still_posts() {
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    let (_, b) = transcript(&a_path, "b.jsonl", "s2", &row("n1", "b one"));
    bindings.commit(rotate(2, &a, &b, BindingCause::Clear));
    // A write every 5 s never leaves the old source quiet long enough to retire.
    for write in 0..130 {
        append(
            &a_path,
            &row(&format!("m{}", write + 1), &format!("a {write}")),
        );
        polls(5).await;
    }
    assert_eq!(harness.port.posts().len(), 131);
    let growing = WriterAlarm::SourceStillGrowing { source: a.clone() };
    assert_eq!(harness.alarms.taken(), [growing]);
    let (_, c) = transcript(&a_path, "c.jsonl", "s3", b"");
    let (d_path, d) = transcript(&a_path, "d.jsonl", "s4", b"");
    bindings.commit(rotate(3, &b, &c, BindingCause::Clear));
    bindings.commit(rotate(4, &c, &d, BindingCause::Clear));
    append(&d_path, &row("k1", "d one"));
    polls(3).await;
    assert_eq!(
        harness.alarms.taken(),
        [WriterAlarm::TooManyReaders { count: 4 }]
    );
    assert_eq!(harness.port.posts().last().unwrap(), "d one");
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn a_compact_that_keeps_the_same_source_reposts_nothing() {
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    append(&a_path, &row("m1", "first"));
    polls(3).await;
    bindings.commit(rotate(2, &a, &a, BindingCause::Compact));
    append(&a_path, &row("m2", "second"));
    polls(12).await;
    assert_eq!(harness.port.posts(), ["first", "second"]);
    assert_eq!(harness.channel().cursors().count(), 1);
    assert!(
        !retired(&harness, &a),
        "a source is never its own successor"
    );
    assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(2));
    assert_eq!(harness.alarms.taken(), []);
    halt(stop, task).await;
}

fn codex_line(value: serde_json::Value) -> Vec<u8> {
    let mut line = serde_json::to_vec(&value).unwrap();
    line.push(b'\n');
    line
}

#[tokio::test(start_paused = true)]
async fn a_body_the_parent_only_announced_is_posted_from_the_resumed_source() {
    let (harness, a_path, a, bindings) = started(b"");
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Codex, bindings.clone());
    let announced = codex_line(serde_json::json!({"type": "event_msg", "payload": {
        "type": "item_completed", "item": {"type": "AgentMessage", "id": "msg_k"}}}));
    append(&a_path, &announced);
    polls(3).await;
    let body = codex_line(serde_json::json!({"type": "response_item",
        "timestamp": Utc::now().to_rfc3339(), "payload": {
        "type": "message", "role": "assistant", "id": "msg_k",
        "content": [{"type": "output_text", "text": "the answer"}]}}));
    let (_, b) = transcript(&a_path, "b.jsonl", "s2", &[announced, body].concat());
    bindings.commit(rotate(2, &a, &b, BindingCause::Resume));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["the answer"]);
    assert_eq!(harness.alarms.taken(), []);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn a_channel_whose_binding_log_names_no_switched_source_halts_before_capture() {
    for other in [false, true] {
        let (harness, path, _) = switched_over(&row("m0", "before the switch"));
        harness.gate.acquired();
        append(&path, &row("m1", "after the switch"));
        let bindings = Arc::new(FakeBindings::new());
        if other {
            let (_, x) = transcript(&path, "x.jsonl", "sx", b"");
            let target = BindingTarget::Source(x);
            bindings.commit(bound(1, None, target, BindingCause::Startup, None));
        }
        let (_stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
        polls(3).await;
        assert!(task.is_finished(), "other={other}");
        assert!(harness.port.posts().is_empty(), "other={other}");
        let alarms = harness.alarms.taken();
        let baseline = |a: &WriterAlarm| matches!(a, WriterAlarm::Halted { detail } if detail.contains("baseline"));
        assert!(
            matches!(alarms.as_slice(), [a] if baseline(a)),
            "{alarms:?}"
        );
        assert_eq!(harness.channel().binding_checkpoint().unwrap(), None);
    }
}

#[tokio::test(start_paused = true)]
async fn a_failing_binding_log_holds_capture_and_binds_until_it_reads_again() {
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    bindings.fail(Some("no log yet"));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    append(&a_path, &row("m1", "a after"));
    polls(3).await;
    assert!(harness.port.posts().is_empty());
    let unseeded = WriterAlarm::BindingLogUnavailable {
        checkpoint: None,
        detail: "no log yet".into(),
    };
    assert_eq!(harness.alarms.taken(), [unseeded]);
    bindings.fail(None);
    polls(3).await;
    assert_eq!(harness.port.posts(), ["a after"]);
    bindings.fail(Some("log unreachable"));
    let (_, b) = transcript(&a_path, "b.jsonl", "s2", &row("n1", "b one"));
    let (_, c) = transcript(&a_path, "c.jsonl", "s3", &row("k1", "c one"));
    bindings.commit(rotate(2, &a, &b, BindingCause::Clear));
    bindings.commit(rotate(3, &b, &c, BindingCause::Clear));
    polls(5).await;
    assert_eq!(harness.port.posts(), ["a after"]);
    let unavailable = WriterAlarm::BindingLogUnavailable {
        checkpoint: Some(1),
        detail: "log unreachable".into(),
    };
    assert_eq!(harness.alarms.taken(), [unavailable]);
    assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(1));
    assert!(harness.channel().cursor(&b).is_none());
    bindings.fail(None);
    polls(3).await;
    assert_eq!(harness.port.posts(), ["a after", "b one", "c one"]);
    assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(3));
    assert_eq!(harness.alarms.taken(), []);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn an_old_backlog_longer_than_one_read_is_posted_before_the_new_source() {
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(3).await;
    let thinking = |i: u32| {
        let text = "x".repeat(256 << 10);
        codex_line(
            serde_json::json!({"type": "assistant", "uuid": format!("u-t{i}"),
            "apiBlockIndex": 0, "message": {"id": format!("t{i}"),
            "content": [{"type": "thinking", "thinking": text}]}}),
        )
    };
    let backlog: Vec<u8> = (0..6).flat_map(thinking).collect();
    assert!(backlog.len() as u64 > MAX_READ_BYTES);
    append(&a_path, &[backlog, row("m1", "old last")].concat());
    let (_, b) = transcript(&a_path, "b.jsonl", "s2", &row("n1", "new first"));
    bindings.commit(rotate(2, &a, &b, BindingCause::Clear));
    polls(4).await;
    assert_eq!(harness.port.posts(), ["old last", "new first"]);
    assert_eq!(harness.alarms.taken(), []);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn an_old_tail_the_full_spool_refuses_still_goes_before_the_new_source() {
    let body = row("m0", "before the switch");
    let (harness, a_path, a) = switched_over(&body);
    let bindings = Arc::new(FakeBindings::new());
    let target = BindingTarget::Source(a.clone());
    bindings.commit(bound(1, None, target, BindingCause::Startup, None));
    let long = "x".repeat(1800);
    append(&a_path, &row("m1", &long));
    let mut store = harness.channel();
    let mut capture = SourceCapture::open(a.clone(), body.len() as u64).unwrap();
    let CaptureOutcome::Batch(batch) = capture.poll(MAX_READ_BYTES) else {
        panic!("capture failed");
    };
    store.append_spool(&batch, &capture.prefix_hash()).unwrap();
    // Room for the new source's row but not for the old tail until `long` is collected.
    let room = store.spool_bytes() + 1024;
    store.set_limits_for_test(1, room);
    let tail = "y".repeat(1500);
    append(&a_path, &row("m2", &tail));
    let (stop, task) = spawn_with(
        writer_over(&harness, store),
        ShadowProvider::Claude,
        bindings.clone(),
    );
    polls(2).await;
    let (_, b) = transcript(&a_path, "b.jsonl", "s2", &row("n1", "new first"));
    bindings.commit(rotate(2, &a, &b, BindingCause::Clear));
    polls(2).await;
    harness.gate.acquired();
    polls(4).await;
    assert_eq!(
        harness.port.posts(),
        [long.as_str(), tail.as_str(), "new first"]
    );
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn a_source_bound_back_waits_for_the_tail_of_the_source_it_replaces() {
    for retire_first in [false, true] {
        let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        let (b_path, b) = transcript(&a_path, "b.jsonl", "s2", &row("n1", "b first"));
        bindings.commit(rotate(2, &a, &b, BindingCause::Clear));
        polls(if retire_first { 15 } else { 3 }).await;
        assert_eq!(retired(&harness, &a), retire_first);
        append(&b_path, &row("n2", "b tail"));
        append(&a_path, &row("m1", "a resumed"));
        bindings.commit(rotate(3, &b, &a, BindingCause::Resume));
        polls(3).await;
        let posts = harness.port.posts();
        assert_eq!(
            posts,
            ["b first", "b tail", "a resumed"],
            "retired={retire_first}"
        );
        assert_eq!(harness.alarms.taken(), [], "retired={retire_first}");
        halt(stop, task).await;
    }
}

#[tokio::test(start_paused = true)]
async fn an_old_tail_the_full_spool_refuses_behind_an_announced_unit_stops_the_channel_intact() {
    let message = |id: &str, text: &str| {
        codex_line(serde_json::json!({"type": "response_item",
            "timestamp": Utc::now().to_rfc3339(), "payload": {
            "type": "message", "role": "assistant", "id": id,
            "content": [{"type": "output_text", "text": text}]}}))
    };
    let announced = codex_line(serde_json::json!({"type": "event_msg", "payload": {
        "type": "item_completed", "item": {"type": "AgentMessage", "id": "msg_k"}}}));
    let (harness, a_path, a) = switched_over(b"");
    let bindings = Arc::new(FakeBindings::new());
    let target = BindingTarget::Source(a.clone());
    bindings.commit(bound(1, None, target, BindingCause::Startup, None));
    let settled = "x".repeat(1800);
    append(
        &a_path,
        &[message("msg_s", &settled), announced.clone()].concat(),
    );
    let mut store = harness.channel();
    let mut capture = SourceCapture::open(a.clone(), 0).unwrap();
    let CaptureOutcome::Batch(batch) = capture.poll(MAX_READ_BYTES) else {
        panic!("capture failed");
    };
    store.append_spool(&batch, &capture.prefix_hash()).unwrap();
    let spooled = capture.captured_through();
    store.set_limits_for_test(1, store.spool_bytes() + 1024);
    let tail = "y".repeat(1500);
    append(&a_path, &message("msg_t", &tail));
    harness.gate.acquired();
    let writer = writer_over(&harness, store);
    let (_stop, task) = spawn_with(writer, ShadowProvider::Codex, bindings.clone());
    polls(2).await;
    let body = message("msg_k", "the answer");
    let (_, b) = transcript(&a_path, "b.jsonl", "s2", &[announced, body].concat());
    bindings.commit(rotate(2, &a, &b, BindingCause::Resume));
    polls(2).await;
    assert!(task.is_finished(), "a stalled rotation stops the channel");
    assert_eq!(harness.port.posts(), [settled.as_str()]);
    let stalled = WriterAlarm::RotationStalled { source: a.clone() };
    assert_eq!(harness.alarms.taken(), [WriterAlarm::SpoolFull, stalled]);
    let store = harness.channel();
    assert_eq!(store.cursor(&a).unwrap().captured_through, spooled);
    assert_eq!(store.cursor(&b).unwrap().captured_through, 0);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Codex, bindings);
    polls(3).await;
    let posts = harness.port.posts();
    assert_eq!(posts, [settled.as_str(), tail.as_str(), "the answer"]);
    halt(stop, task).await;
}

#[path = "fork_tests.rs"]
mod fork_tests;
#[path = "recovery_tests.rs"]
mod recovery_tests;
#[cfg(unix)]
#[path = "renumber_tests.rs"]
mod renumber_tests;
#[path = "retire_tests.rs"]
mod retire_tests;
#[cfg(unix)]
#[path = "source_identity_tests.rs"]
mod source_identity_tests;
#[path = "switch_tests.rs"]
mod switch_tests;
