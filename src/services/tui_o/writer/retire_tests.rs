use super::*;
use crate::services::claude_tui::hook_server::HookEventKind;
use crate::services::tui_o::store::rotation::{
    BOUNDARY_FILE, Boundary, Rotation, SourceLink, Successor,
};
use crate::services::tui_o::store::spool::source_key;

fn session_start() -> String {
    HookEventKind::SessionStart.as_str().to_owned()
}

/// A bind of `new` in pane `tmux`, named by hook `hook` (empty for a record no hook made).
fn hop_at(
    seq: u64,
    tmux: &str,
    old: Option<&SourceId>,
    new: &SourceId,
    cause: BindingCause,
    hook: &str,
) -> BindingEvent {
    let target = BindingTarget::Source(new.clone());
    let mut event = bound(seq, old, target, cause, None);
    event.tmux_session = tmux.into();
    if let BindingRecord::Bound { evidence, .. } = &mut event.record {
        evidence.hook_event = hook.into();
    }
    event
}

/// A resume naming the source the pane already reads, as a relaunch's SessionStart records it.
fn resumed(seq: u64, tmux: &str, source: &SourceId) -> BindingEvent {
    let hook = session_start();
    hop_at(seq, tmux, Some(source), source, BindingCause::Resume, &hook)
}

/// Six 256 KiB thinking records, longer than one read, that post nothing.
fn backlog(first: u32) -> Vec<u8> {
    let thinking = |i: u32| {
        let text = "x".repeat(256 << 10);
        codex_line(
            serde_json::json!({"type": "assistant", "uuid": format!("u-t{i}"),
            "apiBlockIndex": 0, "message": {"id": format!("t{i}"),
            "content": [{"type": "thinking", "thinking": text}]}}),
        )
    };
    let lines: Vec<u8> = (first..first + 6).flat_map(thinking).collect();
    assert!(lines.len() as u64 > MAX_READ_BYTES);
    lines
}

fn len(path: &Path) -> u64 {
    std::fs::metadata(path).unwrap().len()
}

fn owed_link(source: &SourceId, seq: u64) -> SourceLink {
    SourceLink {
        source: source.clone(),
        seq,
        parent: None,
        committed_at: Utc::now(),
        boundary: Boundary::Owed { from: 0 },
    }
}

fn successor(harness: &Harness, old: &SourceId) -> Successor {
    let rotation = harness.channel().rotation().unwrap();
    rotation.successors[&source_key(old)].clone()
}

fn grew(alarm: &WriterAlarm) -> bool {
    matches!(alarm, WriterAlarm::RetiredSourceGrew { .. })
}

#[tokio::test(start_paused = true)]
async fn an_old_source_is_not_retired_without_provider_evidence_however_long_it_stays_quiet() {
    let ss = session_start();
    let cases = [
        ("launch", BindingCause::Startup, ""),
        ("launch_resume", BindingCause::Resume, ""),
        ("prompt", BindingCause::Unknown, "user_prompt_submit"),
        ("startup", BindingCause::Startup, ss.as_str()),
    ];
    for (case, cause, hook) in cases {
        let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
        let (b_path, b) = transcript(&a_path, "b.jsonl", "s2", &row("n1", "b one"));
        bindings.commit(hop_at(2, "tmux", Some(&a), &b, cause, hook));
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        polls(3).await;
        let b_read = harness.channel().cursor(&b).unwrap().captured_through;
        assert_eq!(b_read, len(&b_path), "[T1b:captured:{case}]");
        polls(30).await;
        assert!(!retired(&harness, &a), "[T1b:unproven:{case}]");
        append(&a_path, &row("m1", "a late"));
        polls(3).await;
        let posts = harness.port.posts();
        assert_eq!(
            posts.last().map(String::as_str),
            Some("a late"),
            "[T1b:read:{case}]"
        );
        assert!(
            !harness.alarms.taken().iter().any(grew),
            "[T1b:no_regrowth:{case}]"
        );
        bindings.commit(resumed(3, "tmux", &b));
        polls(12).await;
        assert!(retired(&harness, &a), "[T1b:proven:{case}]");
        halt(stop, task).await;
    }
}

/// Runs A→B→C with A's backlog longer than one read and B's unread, stopping mid-read when
/// `restart` is set, then grows B after C was bound; returns what was posted.
async fn three_hops(restart: bool) -> (Vec<String>, Harness, [SourceId; 3]) {
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    append(&a_path, &[backlog(0), row("m1", "a last")].concat());
    let (b_path, b) = transcript(&a_path, "b.jsonl", "s2", &row("n1", "b last"));
    let (_, c) = transcript(&a_path, "c.jsonl", "s3", &row("k1", "c one"));
    bindings.commit(rotate(2, &a, &b, BindingCause::Clear));
    bindings.commit(rotate(3, &b, &c, BindingCause::Clear));
    let (mut stop, mut task) =
        spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    tokio::time::sleep(POLL_INTERVAL / 2).await;
    if restart {
        halt(stop, task).await;
        let a_read = harness.channel().cursor(&a).unwrap().captured_through;
        assert!(a_read < len(&a_path), "stopped mid-read");
        (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    }
    polls(4).await;
    append(&b_path, &row("n2", "b late"));
    polls(3).await;
    let posts = harness.port.posts();
    polls(12).await;
    assert_eq!(harness.alarms.taken(), [], "[T2:alarms] restart={restart}");
    halt(stop, task).await;
    (posts, harness, [a, b, c])
}

#[tokio::test(start_paused = true)]
async fn a_b_c_hops_with_both_old_tails_unread_post_every_row_once_in_order_across_a_restart() {
    let expected = ["a last", "b last", "c one", "b late"];
    let (live, _, _) = three_hops(false).await;
    assert_eq!(live, expected, "[T2:order] live");
    let (restarted, harness, [a, b, c]) = three_hops(true).await;
    assert_eq!(restarted, expected, "[T2:order] restarted");
    let states = [&a, &b, &c].map(|source| retired(&harness, source));
    assert_eq!(states, [true, true, false], "[T2:retired]");
}

/// Binds A→B while A's backlog is longer than one read, grows A by another such backlog during
/// the first read, and stops there when `restart` is set; returns posts and A's rotation length.
async fn grown_after_rotation(restart: bool) -> (Vec<String>, u64, Option<u64>) {
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    append(&a_path, &[backlog(0), row("m1", "a last")].concat());
    let at_rotation = len(&a_path);
    let (_, b) = transcript(&a_path, "b.jsonl", "s2", &row("n1", "new first"));
    bindings.commit(rotate(2, &a, &b, BindingCause::Clear));
    let (mut stop, mut task) =
        spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    tokio::time::sleep(POLL_INTERVAL / 2).await;
    let stored = successor(&harness, &a).drain_to;
    let late = [backlog(10), row("m2", "late old")].concat();
    if restart {
        halt(stop, task).await;
        append(&a_path, &late);
        (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
    } else {
        append(&a_path, &late);
    }
    polls(6).await;
    assert_eq!(harness.alarms.taken(), [], "[T4:alarms] restart={restart}");
    halt(stop, task).await;
    (harness.port.posts(), at_rotation, stored)
}

#[tokio::test(start_paused = true)]
async fn a_restart_holds_the_successor_only_to_the_old_length_its_rotation_measured() {
    let (live, _, _) = grown_after_rotation(false).await;
    let (restarted, at_rotation, stored) = grown_after_rotation(true).await;
    assert_eq!(stored, Some(at_rotation), "[T4:stored]");
    assert_eq!(live, ["a last", "new first", "late old"], "[T4:live]");
    assert_eq!(restarted, live, "[T4:order]");
}

/// A store whose A→B successor record was written before the hop fields were kept: the boundary
/// file holds only B's identity under A. `hop` is the log's seq 2, read up to the checkpoint.
fn legacy_rotation(hop: impl FnOnce(&SourceId, &SourceId) -> BindingEvent) -> Legacy {
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    append(&a_path, &row("m1", "a tail"));
    let (_, b) = transcript(&a_path, "b.jsonl", "s2", &row("n1", "b first"));
    bindings.commit(hop(&a, &b));
    let mut store = harness.channel();
    let mut rotation = Rotation::default();
    rotation.links.insert(source_key(&b), owed_link(&b, 2));
    store.write_rotation(&rotation).unwrap();
    store.attach_source(&b).unwrap();
    store.set_binding_checkpoint(2).unwrap();
    drop(store);
    let file = (a_path.parent().unwrap().join("o_store"))
        .join(CHANNEL.to_string())
        .join(BOUNDARY_FILE);
    let mut written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    written["successors"] = serde_json::json!({ source_key(&a): {
        "session_id": b.session_id, "path": b.path, "dev": b.dev, "ino": b.ino,
    }});
    std::fs::write(&file, serde_json::to_vec(&written).unwrap()).unwrap();
    Legacy {
        harness,
        a_path,
        a,
        b,
        bindings,
    }
}

struct Legacy {
    harness: Harness,
    a_path: PathBuf,
    a: SourceId,
    b: SourceId,
    bindings: Arc<FakeBindings>,
}

#[tokio::test(start_paused = true)]
async fn a_successor_record_without_rotation_fields_holds_to_the_current_length_and_waits_for_evidence()
 {
    let legacy = legacy_rotation(|a, b| rotate(2, a, b, BindingCause::Clear));
    let Legacy {
        harness,
        a_path,
        a,
        b,
        bindings,
        ..
    } = &legacy;
    append(a_path, &[backlog(0), row("m2", "a late")].concat());
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(6).await;
    assert_eq!(
        harness.port.posts(),
        ["a tail", "a late", "b first"],
        "[T5:order]"
    );
    assert_eq!(successor(harness, a).source, *b, "[T5:read]");
    polls(15).await;
    assert!(!retired(harness, a), "[T5:unproven]");
    bindings.commit(resumed(3, "tmux", b));
    polls(12).await;
    assert!(retired(harness, a), "[T5:proven]");
    assert_eq!(harness.alarms.taken(), [], "[T5:alarms]");
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn a_legacy_hop_is_proven_only_by_evidence_from_the_pane_the_log_names() {
    let legacy = legacy_rotation(|a, b| rotate(2, a, b, BindingCause::Clear));
    let Legacy {
        harness,
        a_path,
        a,
        b,
        bindings,
        ..
    } = &legacy;
    let (_, c) = transcript(a_path, "c.jsonl", "s3", &row("k1", "c one"));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(3).await;
    let ss = session_start();
    bindings.commit(hop_at(3, "other", None, &c, BindingCause::Clear, &ss));
    polls(15).await;
    assert!(!retired(harness, a), "[P22:other_pane]");
    bindings.commit(resumed(4, "tmux", b));
    polls(12).await;
    assert!(retired(harness, a), "[P22:own_pane]");
    halt(stop, task).await;

    // A log whose applied records name no hop from A cannot place the record in any pane.
    let unnamed = legacy_rotation(|_, _| {
        let refused = BindingRecord::Rejected {
            detail: "regression".into(),
        };
        event(2, refused, Utc::now())
    });
    let Legacy {
        harness,
        a,
        b,
        bindings,
        ..
    } = &unnamed;
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(3).await;
    bindings.commit(resumed(3, "tmux", b));
    polls(15).await;
    assert!(!retired(harness, a), "[P22:unrestorable]");
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn a_rotation_applied_again_after_a_crash_keeps_its_measured_length_and_proof() {
    // Before the proof write the record is unproven; after it, the checkpoint alone was lost.
    for proof in [None, Some(2)] {
        let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
        append(&a_path, &row("m1", "old tail"));
        let at_rotation = len(&a_path);
        let (_, b) = transcript(&a_path, "b.jsonl", "s2", &row("n1", "new first"));
        bindings.commit(rotate(2, &a, &b, BindingCause::Clear));
        let mut store = harness.channel();
        store.set_binding_checkpoint(1).unwrap();
        let mut rotation = Rotation::default();
        rotation.links.insert(source_key(&b), owed_link(&b, 2));
        let next = Successor {
            source: b.clone(),
            seq: Some(2),
            tmux_session: Some("tmux".into()),
            drain_to: Some(at_rotation),
            proof,
        };
        rotation.successors.insert(source_key(&a), next);
        store.write_rotation(&rotation).unwrap();
        store.attach_source(&b).unwrap();
        drop(store);
        append(&a_path, &[backlog(0), row("m2", "late old")].concat());
        assert_ne!(len(&a_path), at_rotation);
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
        polls(4).await;
        let kept = successor(&harness, &a);
        assert_eq!(kept.drain_to, Some(at_rotation), "[crash:drain] {proof:?}");
        assert_eq!(kept.proof, Some(2), "[crash:proof] {proof:?}");
        let posts = harness.port.posts();
        assert_eq!(
            posts,
            ["old tail", "new first", "late old"],
            "[crash:order] {proof:?}"
        );
        let checkpoint = harness.channel().binding_checkpoint().unwrap();
        assert_eq!(checkpoint, Some(2), "[crash:checkpoint] {proof:?}");
        halt(stop, task).await;
    }
}

#[tokio::test(start_paused = true)]
async fn a_tail_without_its_newline_keeps_a_proven_old_source_read_until_it_ends() {
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    let (_, b) = transcript(&a_path, "b.jsonl", "s2", &row("n1", "b one"));
    bindings.commit(hop_at(2, "tmux", Some(&a), &b, BindingCause::Startup, ""));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(15).await;
    assert!(!retired(&harness, &a), "[EOF:unproven]");
    let mut split = row("m1", "a split");
    split.pop();
    append(&a_path, &split);
    bindings.commit(resumed(3, "tmux", &b));
    polls(15).await;
    assert!(!retired(&harness, &a), "[EOF:partial]");
    assert_eq!(harness.port.posts(), ["b one"], "[EOF:withheld]");
    append(&a_path, b"\n");
    polls(3).await;
    assert_eq!(harness.port.posts(), ["b one", "a split"], "[EOF:read]");
    polls(12).await;
    assert!(retired(&harness, &a), "[EOF:retired]");
    halt(stop, task).await;
}
