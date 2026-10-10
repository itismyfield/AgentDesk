//! A Claude hook may fill the empty session of a stored source on a renumbered dev: O reads it as
//! that source, keeps sources already stored apart, and halts on changed earlier bytes.

use std::io::{Seek, SeekFrom};

use super::*;
use crate::services::tui_o::shadow::capture::renumber;
use crate::services::tui_o::store::rotation::{Boundary, ResolveFrom, SourceLink};
use crate::services::tui_o::store::spool::source_key;

const REBOOT: u64 = 1 << 40;
const SECOND_REBOOT: u64 = 1 << 41;

/// A switched-over channel whose startup bound `t.jsonl` with an empty session.
fn started_empty(body: &[u8]) -> (Harness, PathBuf, SourceId, Arc<FakeBindings>) {
    let mut stored = None;
    let harness = Harness::build(|runtime| {
        let path = runtime.join("t.jsonl");
        std::fs::write(&path, body).unwrap();
        let source_id = source_id_for("", &path).unwrap();
        stored = Some((path, source_id.clone()));
        let (delivery_start, prefix_hash) = (body.len() as u64, hex::encode(Sha256::digest(body)));
        vec![InitSource {
            source_id,
            delivery_start,
            prefix_hash,
        }]
    });
    let (path, source) = stored.unwrap();
    let bindings = Arc::new(FakeBindings::new());
    let target = BindingTarget::Source(source.clone());
    bindings.commit(bound(1, None, target, BindingCause::Startup, None));
    harness.gate.acquired();
    (harness, path, source, bindings)
}

/// `source`'s file as a stat names it now, under `session`.
fn named(source: &SourceId, session: &str) -> SourceId {
    source_id_for(session, &source.path).unwrap()
}

fn resume(seq: u64, old: &SourceId, new: &SourceId) -> BindingEvent {
    let target = BindingTarget::Source(new.clone());
    bound(seq, Some(old), target, BindingCause::Resume, None)
}

/// Every stored cursor on `source`'s path and inode, whatever its session or dev.
fn copies(harness: &Harness, source: &SourceId) -> usize {
    let cursors = harness.channel().cursors().cloned().collect::<Vec<_>>();
    let same = cursors
        .iter()
        .filter(|c| c.source.path == source.path && c.source.ino == source.ino);
    same.count()
}

fn halted_with(alarms: &[WriterAlarm], wanted: &str) -> bool {
    matches!(alarms.last(), Some(WriterAlarm::Halted { detail }) if detail.contains(wanted))
}

/// Stores `d` beside `w` as a writer before this change applied seq 2's resume: link, cursor,
/// then checkpoint. `ahead` also spools `d` past `w`'s cursor and names `w` its parent.
fn stored_apart(harness: &Harness, w: &SourceId, d: &SourceId, ahead: bool) -> SourceLink {
    let mut store = harness.channel();
    let link = SourceLink {
        source: d.clone(),
        seq: 2,
        parent: ahead.then(|| w.clone()),
        committed_at: Utc::now(),
        boundary: Boundary::Pending {
            candidates: vec![0],
        },
    };
    let mut rotation = store.rotation().unwrap();
    rotation.links.insert(source_key(d), link.clone());
    store.write_rotation(&rotation).unwrap();
    let attached = store.attach_source(d).unwrap();
    if ahead {
        append(&w.path, &row("m2", "second"));
        let mut capture = SourceCapture::reopen(d.clone(), 0, &attached.prefix_hash).unwrap();
        let CaptureOutcome::Batch(batch) = capture.poll(MAX_READ_BYTES) else {
            panic!("capture failed");
        };
        store.append_spool(&batch, &capture.prefix_hash()).unwrap();
        assert!(
            store.cursor(d).unwrap().captured_through > store.cursor(w).unwrap().captured_through
        );
    }
    store.set_binding_checkpoint(2).unwrap();
    link
}

#[tokio::test(start_paused = true)]
async fn a_filled_session_rebind_after_a_renumber_reads_the_stored_empty_session_source() {
    let (harness, path, w, bindings) = started_empty(&row("m0", "before the switch"));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    append(&path, &row("m1", "first"));
    polls(3).await;
    halt(stop, task).await;
    let reboot = renumber::shift(&path, REBOOT);
    let a1 = named(&w, "A");
    assert_ne!(a1.dev, w.dev);
    bindings.commit(resume(2, &w, &a1));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    append(&path, &row("m2", "tail"));
    polls(3).await;
    assert!(!task.is_finished());
    assert_eq!(harness.port.posts(), ["first", "tail"]);
    assert_eq!(copies(&harness, &w), 1);
    assert!(harness.channel().cursor(&w).is_some());
    let rotation = harness.channel().rotation().unwrap();
    assert!(rotation.links.is_empty() && rotation.successors.is_empty());
    assert_eq!(harness.alarms.readers(), Some(1));
    assert_eq!(harness.alarms.taken(), []);
    assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(2));
    halt(stop, task).await;

    // Renumbered again, then restarted once past the checkpoint and once from before it.
    drop(reboot);
    let _again = renumber::shift(&path, SECOND_REBOOT);
    bindings.commit(resume(3, &a1, &named(&w, "A")));
    let texts = ["first", "tail", "third", "fourth"];
    for (round, (id, posted)) in [("m3", 3), ("m4", 4)].into_iter().enumerate() {
        if round == 1 {
            harness.channel().set_binding_checkpoint(2).unwrap();
        }
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        append(&path, &row(id, texts[posted - 1]));
        polls(3).await;
        assert!(!task.is_finished(), "round {round}");
        assert_eq!(harness.port.posts(), &texts[..posted], "round {round}");
        assert_eq!(harness.channel().cursors().count(), 1, "round {round}");
        assert!(harness.channel().cursor(&w).is_some(), "round {round}");
        let rotation = harness.channel().rotation().unwrap();
        assert!(rotation.links.is_empty(), "round {round}");
        assert!(rotation.successors.is_empty(), "round {round}");
        assert_eq!(harness.alarms.taken(), [], "round {round}");
        assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(3));
        halt(stop, task).await;
    }
}

#[tokio::test(start_paused = true)]
async fn an_empty_session_bind_and_its_filled_rebind_in_one_batch_attach_one_source() {
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    append(&a_path, &row("m1", "first"));
    polls(3).await;
    halt(stop, task).await;
    let (b_path, b0) = transcript(&a_path, "b.jsonl", "", &row("n0", "b zero"));
    let target = BindingTarget::Source(b0.clone());
    bindings.commit(bound(2, Some(&a), target, BindingCause::Clear, None));
    let _reboot = renumber::shift(&b_path, REBOOT);
    bindings.commit(resume(3, &b0, &named(&b0, "B")));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
    append(&b_path, &row("n1", "b one"));
    polls(3).await;
    assert!(!task.is_finished());
    assert_eq!(harness.port.posts(), ["first", "b zero", "b one"]);
    assert_eq!(copies(&harness, &b0), 1);
    let rotation = harness.channel().rotation().unwrap();
    assert!(!rotation.successors.contains_key(&source_key(&b0)));
    assert_eq!(rotation.links.len(), 1);
    assert_eq!(harness.alarms.taken(), []);
    assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(3));
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn a_filled_return_to_an_empty_session_source_survives_restarts_without_reviving_its_old_hop()
{
    let (harness, path, w, bindings) = started_empty(&row("m0", "before the switch"));
    let (x_path, x) = transcript(&path, "x.jsonl", "x", b"");
    let target = BindingTarget::Source(x.clone());
    bindings.commit(bound(2, Some(&w), target, BindingCause::Clear, None));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    append(&x_path, &row("x1", "x one"));
    polls(3).await;
    halt(stop, task).await;
    let _reboot = renumber::shift(&path, REBOOT);
    bindings.commit(resume(3, &x, &named(&w, "A")));
    for round in 0..3 {
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        polls(3).await;
        assert!(!task.is_finished(), "round {round}");
        let rotation = harness.channel().rotation().unwrap();
        assert!(
            !rotation.successors.contains_key(&source_key(&w)),
            "round {round}"
        );
        let back = rotation
            .successors
            .get(&source_key(&x))
            .map(|next| &next.source);
        assert_eq!(back, Some(&w), "round {round}");
        assert_eq!(harness.channel().cursors().count(), 2, "round {round}");
        let pending = |link: &SourceLink| matches!(link.boundary, Boundary::Pending { .. });
        assert!(!rotation.links.values().any(pending), "round {round}");
        let alarms = harness.alarms.taken();
        assert!(
            !alarms
                .iter()
                .any(|a| matches!(a, WriterAlarm::Halted { .. }))
        );
        assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(3));
        halt(stop, task).await;
    }
}

#[tokio::test(start_paused = true)]
async fn sources_stored_apart_before_this_change_stay_two_readers_and_the_pending_one_stays() {
    for ahead in [false, true] {
        let (harness, path, w, bindings) = started_empty(&row("m0", "before the switch"));
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        append(&path, &row("m1", "first"));
        polls(3).await;
        halt(stop, task).await;
        let _reboot = renumber::shift(&path, REBOOT);
        let d = named(&w, "A");
        let (cause, parent) = if ahead {
            (BindingCause::Unknown, Some(&w))
        } else {
            (BindingCause::Resume, None)
        };
        let target = BindingTarget::Source(d.clone());
        bindings.commit(bound(2, Some(&w), target, cause, parent));
        let link = stored_apart(&harness, &w, &d, ahead);
        let before: Vec<_> = harness.channel().cursors().cloned().collect();
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        append(&path, &row("m3", "tail"));
        polls(3).await;
        assert!(!task.is_finished(), "ahead {ahead}");
        let posted: &[&str] = if ahead {
            &["first", "second", "tail"]
        } else {
            &["first", "tail"]
        };
        assert_eq!(harness.port.posts(), posted, "ahead {ahead}");
        assert_eq!(harness.alarms.readers(), Some(2), "ahead {ahead}");
        let pending = WriterAlarm::BoundaryPending { source: d.clone() };
        assert_eq!(harness.alarms.taken(), [pending], "ahead {ahead}");
        let store = harness.channel();
        assert_eq!(
            store.rotation().unwrap().link(&d),
            Some(&link),
            "ahead {ahead}"
        );
        assert_eq!(store.cursors().count(), 2, "ahead {ahead}");
        for cursor in &before {
            let now = store.cursor(&cursor.source).unwrap().captured_through;
            assert!(now >= cursor.captured_through, "ahead {ahead}");
        }
        assert!(store.retained_segments(&d) > 0, "ahead {ahead}");
        assert_eq!(store.binding_checkpoint().unwrap(), Some(2));
        halt(stop, task).await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_resolved_copy_keeps_its_exact_name_and_the_empty_one_its_own_after_another_renumber() {
    for filled in [true, false] {
        let (harness, path, w, bindings) = started_empty(&row("m0", "before the switch"));
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        append(&path, &row("m1", "first"));
        polls(3).await;
        halt(stop, task).await;
        let reboot = renumber::shift(&path, REBOOT);
        let d = named(&w, "A");
        bindings.commit(resume(2, &w, &d));
        stored_apart(&harness, &w, &d, false);
        let eof = std::fs::metadata(&path).unwrap().len();
        let from = ResolveFrom::Offset(eof);
        let key = source_key(&d);
        let resolved = harness
            .store
            .record_boundary_resolved(CHANNEL, &key, &from, "operator");
        assert_eq!(resolved.unwrap(), (d.clone(), eof));
        drop(reboot);
        let _again = renumber::shift(&path, SECOND_REBOOT);
        let (old, new) = if filled {
            (&d, named(&w, "A"))
        } else {
            (&w, named(&w, ""))
        };
        bindings.commit(resume(3, old, &new));
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        append(&path, &row("m3", "tail"));
        polls(3).await;
        assert!(!task.is_finished(), "filled {filled}");
        assert_eq!(harness.port.posts(), ["first", "tail"], "filled {filled}");
        assert_eq!(harness.alarms.readers(), Some(2), "filled {filled}");
        assert_eq!(harness.alarms.taken(), [], "filled {filled}");
        let store = harness.channel();
        let boundary = store
            .rotation()
            .unwrap()
            .link(&d)
            .map(|l| l.boundary.clone());
        assert_eq!(
            boundary,
            Some(Boundary::Owed { from: eof }),
            "filled {filled}"
        );
        assert_eq!(store.rotation().unwrap().links.len(), 1, "filled {filled}");
        assert_eq!(store.cursors().count(), 2, "filled {filled}");
        assert!(store.cursor(&w).is_some() && store.cursor(&d).is_some());
        assert_eq!(
            store.binding_checkpoint().unwrap(),
            Some(3),
            "filled {filled}"
        );
        halt(stop, task).await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_filled_rebind_onto_a_stored_source_whose_bytes_changed_halts_without_attaching() {
    let body = row("m0", &"x".repeat(6000));
    for case in ["rewritten", "other inode"] {
        let (harness, path, w, bindings) = started_empty(&body);
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        append(&path, &row("m1", "first"));
        polls(3).await;
        let mut filled = named(&w, "A");
        if case == "rewritten" {
            // Far behind the tail a poll re-reads, so only the bind's own check sees it.
            let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.seek(SeekFrom::Start(40)).unwrap();
            file.write_all(b"y").unwrap();
        } else {
            filled.ino += 1;
        }
        bindings.commit(resume(2, &w, &filled));
        polls(3).await;
        assert!(task.is_finished(), "{case}");
        let wanted = match case {
            "rewritten" => "source bytes before the cursor changed",
            _ => "bound source: source replaced",
        };
        let alarms = harness.alarms.taken();
        assert!(halted_with(&alarms, wanted), "{case}: {alarms:?}");
        assert_eq!(copies(&harness, &w), 1, "{case}");
        if case == "rewritten" {
            let rotation = harness.channel().rotation().unwrap();
            assert_eq!(alarms.len(), 1);
            assert!(rotation.links.is_empty() && rotation.successors.is_empty());
        }
        assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(1));
        assert_eq!(harness.port.posts(), ["first"], "{case}");
        drop(stop);
    }
}

#[tokio::test(start_paused = true)]
async fn an_empty_session_source_taken_for_one_session_is_not_taken_for_another() {
    for restart in [false, true] {
        let (harness, path, w, bindings) = started_empty(&row("m0", "before the switch"));
        let (mut stop, mut task) =
            spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        polls(3).await;
        bindings.commit(resume(2, &w, &named(&w, "A")));
        if restart {
            polls(3).await;
            halt(stop, task).await;
            (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
            polls(3).await;
        }
        // No old name of this bind says which session the file was taken for.
        let b = named(&w, "B");
        let target = BindingTarget::Source(b.clone());
        bindings.commit(bound(3, None, target, BindingCause::Startup, None));
        append(&path, &row("m1", "first"));
        polls(3).await;
        assert!(!task.is_finished(), "restart {restart}");
        assert_eq!(copies(&harness, &w), 2, "restart {restart}");
        assert!(harness.channel().cursor(&b).is_some(), "restart {restart}");
        assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(3));
        halt(stop, task).await;
    }
}

#[tokio::test(start_paused = true)]
async fn an_empty_session_name_two_stored_sessions_share_stays_its_own_source() {
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    let (c_path, first) = transcript(&a_path, "c.jsonl", "A", b"");
    let second = named(&first, "B");
    let empty = named(&first, "");
    let mut old = a;
    for (seq, new) in (2..).zip([&first, &second, &empty]) {
        let target = BindingTarget::Source(new.clone());
        bindings.commit(bound(seq, Some(&old), target, BindingCause::Clear, None));
        old = new.clone();
    }
    append(&c_path, &row("c1", "c one"));
    polls(3).await;
    assert!(!task.is_finished());
    assert_eq!(copies(&harness, &first), 3);
    for source in [&first, &second, &empty] {
        assert!(harness.channel().cursor(source).is_some(), "{source:?}");
    }
    let alarms = harness.alarms.taken();
    assert!(
        !alarms
            .iter()
            .any(|a| matches!(a, WriterAlarm::Halted { .. }))
    );
    assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(4));
    assert_eq!(harness.port.posts(), ["c one"]);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn logged_sessions_decide_supersede_and_proof_while_the_stored_name_binds() {
    let (harness, path, w, bindings) = started_empty(&row("m0", "before the switch"));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(3).await;
    let a = named(&w, "A");
    // Only the logged sessions show the filled rebind moved the pane, superseding the Pending.
    let pending = BindingTarget::Pending {
        payload_session_id: "A".into(),
        payload_transcript_path: path.clone(),
    };
    bindings.commit(bound(2, Some(&w), pending, BindingCause::Resume, None));
    bindings.commit(resume(3, &w, &a));
    polls(3).await;
    assert!(!task.is_finished());
    assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(3));
    assert_eq!(copies(&harness, &w), 1);
    let (_, y) = transcript(&path, "y.jsonl", "y", b"");
    let target = BindingTarget::Source(y.clone());
    bindings.commit(bound(4, Some(&a), target, BindingCause::Startup, None));
    // Session A, which the empty-session source was taken for, is not a session it left.
    let (_, z) = transcript(&path, "z.jsonl", "A", b"");
    let target = BindingTarget::Source(z.clone());
    bindings.commit(bound(5, Some(&y), target, BindingCause::Clear, None));
    polls(3).await;
    let proof = |harness: &Harness| {
        let rotation = harness.channel().rotation().unwrap();
        let hop = rotation.successors.get(&source_key(&w)).cloned().unwrap();
        assert_eq!(hop.source, y);
        hop.proof
    };
    assert_eq!(proof(&harness), None);
    let (_, v) = transcript(&path, "v.jsonl", "B", b"");
    let target = BindingTarget::Source(v);
    bindings.commit(bound(6, Some(&z), target, BindingCause::Clear, None));
    polls(3).await;
    assert_eq!(proof(&harness), Some(6));
    assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(6));
    assert!(!task.is_finished());
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn a_codex_filled_rebind_keeps_its_own_source() {
    let (harness, _, w, bindings) = started_empty(&row("m0", "before the switch"));
    let a = named(&w, "A");
    bindings.commit(resume(2, &w, &a));
    // Both binds come from the Codex hook, as a Codex channel's own log names them.
    for event in bindings.events.lock().unwrap().iter_mut() {
        event.provider = ShadowProvider::Codex;
    }
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Codex, bindings);
    polls(3).await;
    assert!(!task.is_finished());
    assert_eq!(copies(&harness, &w), 2);
    let rotation = harness.channel().rotation().unwrap();
    let boundary = rotation.link(&a).map(|link| link.boundary.clone());
    let candidates = vec![0];
    assert_eq!(boundary, Some(Boundary::Pending { candidates }));
    // A Codex resume does not show the old source was left.
    let hop = rotation.successors.get(&source_key(&w)).cloned().unwrap();
    assert_eq!((hop.source, hop.proof), (a.clone(), None));
    let pending = WriterAlarm::BoundaryPending { source: a };
    assert_eq!(harness.alarms.taken(), [pending]);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn a_writer_that_started_without_its_log_rebuilds_taken_sessions_before_binding() {
    let (harness, path, w, bindings) = started_empty(&row("m0", "before the switch"));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(3).await;
    bindings.commit(resume(2, &w, &named(&w, "A")));
    polls(3).await;
    halt(stop, task).await;
    assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(2));
    assert_eq!(harness.alarms.taken(), []);
    bindings.fail(Some("log down"));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(1).await;
    bindings.fail(None);
    let b = named(&w, "B");
    let target = BindingTarget::Source(b.clone());
    bindings.commit(bound(3, None, target, BindingCause::Startup, None));
    append(&path, &row("m1", "first"));
    polls(3).await;
    assert!(!task.is_finished());
    assert_eq!(copies(&harness, &w), 2);
    assert!(harness.channel().cursor(&b).is_some());
    let detail = "log down".to_string();
    let unavailable = WriterAlarm::BindingLogUnavailable {
        checkpoint: Some(2),
        detail,
    };
    assert_eq!(harness.alarms.taken(), [unavailable]);
    assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(3));
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn a_pending_resolved_onto_an_empty_session_source_keeps_its_session_after_a_restart() {
    for log_down in [false, true] {
        let (harness, path, w, bindings) = started_empty(&row("m0", "before the switch"));
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        polls(3).await;
        let pending = BindingTarget::Pending {
            payload_session_id: "A".into(),
            payload_transcript_path: path.clone(),
        };
        bindings.commit(bound(2, Some(&w), pending, BindingCause::Resume, None));
        let resolved = BindingRecord::Resolved {
            resolves_seq: 2,
            source: named(&w, "A"),
        };
        bindings.commit(event(3, resolved, Utc::now()));
        polls(3).await;
        assert_eq!(copies(&harness, &w), 1, "log down {log_down}");
        assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(3));
        halt(stop, task).await;
        // Stopped once the Pending's bind was durable, before its Resolved row passed.
        harness.channel().set_binding_checkpoint(2).unwrap();
        bindings.fail(log_down.then_some("log down"));
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        polls(1).await;
        bindings.fail(None);
        // No old name of this bind says which session the file was taken for.
        let b = named(&w, "B");
        let target = BindingTarget::Source(b.clone());
        bindings.commit(bound(4, None, target, BindingCause::Startup, None));
        polls(3).await;
        assert!(!task.is_finished(), "log down {log_down}");
        assert_eq!(copies(&harness, &w), 2, "log down {log_down}");
        let boundary = harness.channel().rotation().unwrap().link(&b).cloned();
        let boundary = boundary.map(|link| link.boundary);
        assert_eq!(
            boundary,
            Some(Boundary::Owed { from: 0 }),
            "log down {log_down}"
        );
        assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(4));
        halt(stop, task).await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_first_start_halts_rather_than_take_a_changed_source_as_its_baseline() {
    let (harness, path, w, bindings) = started_empty(&row("m0", &"x".repeat(6000)));
    bindings.commit(resume(2, &w, &named(&w, "A")));
    bindings.fail(Some("log down"));
    let (_stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(2).await;
    // The start reopened the whole prefix; this change sits behind the tail a poll re-reads.
    let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.seek(SeekFrom::Start(40)).unwrap();
    file.write_all(b"y").unwrap();
    bindings.fail(None);
    polls(3).await;
    assert!(task.is_finished());
    let alarms = harness.alarms.taken();
    let wanted = "source bytes before the cursor changed";
    assert!(halted_with(&alarms, wanted), "{alarms:?}");
    let store = harness.channel();
    assert_eq!(store.binding_checkpoint().unwrap(), None);
    assert_eq!(store.cursors().count(), 1);
    let rotation = store.rotation().unwrap();
    assert!(rotation.links.is_empty() && rotation.successors.is_empty());
    assert!(harness.port.posts().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_source_bound_again_waits_its_own_quiet_in_its_next_rotation() {
    let (harness, path, w, bindings) = started_empty(&row("m0", "before the switch"));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(3).await;
    // An unproven hop: W stays read, quiet since X was first read, and is never retired.
    let (_, x) = transcript(&path, "x.jsonl", "x", &row("x1", "x one"));
    let target = BindingTarget::Source(x.clone());
    bindings.commit(bound(2, Some(&w), target, BindingCause::Startup, None));
    polls(15).await;
    assert!(!retired(&harness, &w));
    let a = named(&w, "A");
    bindings.commit(resume(3, &x, &a));
    polls(3).await;
    let rotation = harness.channel().rotation().unwrap();
    assert!(!rotation.successors.contains_key(&source_key(&w)));
    let (_, y) = transcript(&path, "y.jsonl", "y", &row("y1", "y one"));
    let target = BindingTarget::Source(y);
    bindings.commit(bound(4, Some(&a), target, BindingCause::Clear, None));
    polls(2).await;
    assert!(
        !retired(&harness, &w),
        "the proven hop still waits its own quiet"
    );
    polls(13).await;
    assert!(retired(&harness, &w));
    assert!(!task.is_finished());
    assert_eq!(harness.port.posts(), ["x one", "y one"]);
    halt(stop, task).await;
}
