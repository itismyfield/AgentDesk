use super::*;
use crate::services::claude_tui::hook_server::HookEventKind;
use crate::services::tui_o::store::rotation::{
    BOUNDARY_FILE, Boundary, Rotation, SourceLink, Successor,
};
use crate::services::tui_o::store::spool::source_key;

#[path = "legacy_hop_retry_tests.rs"]
mod legacy_hop_retry_tests;
#[path = "transition_actor_tests.rs"]
mod transition_actor_tests;

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

fn reader_outcome(writer: &mut Writer, id: &str) -> Option<PieceOutcome> {
    outcome(writer, &format!("{id}:0"))
}

type ReaderEngine = (
    super::super::super::rotation::Sources<FakeBindings>,
    Writer,
    UnitDeriver,
    VecDeque<Derived>,
);

fn reader_engine(
    harness: &Harness,
    bindings: Arc<FakeBindings>,
    policy: Option<super::super::super::ReaderConfig>,
) -> ReaderEngine {
    reader_engine_at(harness, bindings, policy, std::time::SystemTime::now())
}

fn reader_engine_at(
    harness: &Harness,
    bindings: Arc<FakeBindings>,
    policy: Option<super::super::super::ReaderConfig>,
    now: std::time::SystemTime,
) -> ReaderEngine {
    let mut sources =
        super::super::super::rotation::Sources::new(CHANNEL, ShadowProvider::Claude, bindings);
    if let Some(policy) = policy {
        sources = sources.with_readers(policy);
    }
    let mut writer = harness.writer();
    let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    let mut owed = VecDeque::new();
    sources
        .resume_at(&mut writer, &mut deriver, &mut owed, now)
        .unwrap();
    sources.follow(&mut writer).unwrap();
    (sources, writer, deriver, owed)
}

#[tokio::test(start_paused = true)]
async fn readers_s1a_on_closes_drained_old_fd_without_retiring() {
    use super::super::super::{ReaderConfig, rotation::probe};
    let (harness, path, old, bindings) = started(&row("m0", "before"));
    let (_, new) = transcript(&path, "next.jsonl", "s2", b"");
    bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
    let (mut sources, mut writer, mut deriver, mut owed) =
        reader_engine(&harness, bindings, Some(ReaderConfig { dormant: true }));
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    let now = std::time::SystemTime::now();
    reader_tend(&mut sources, &mut writer, now).await.unwrap();
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    reader_tend(
        &mut sources,
        &mut writer,
        now + std::time::Duration::from_secs(31),
    )
    .await
    .unwrap();
    assert!(
        !probe::reading(&sources, &old),
        "old EOF reader releases its FD"
    );
    assert!(probe::reading(&sources, &new), "latest keeps its FD");
    assert!(!writer.store().cursor(&old).unwrap().retired);
}

#[tokio::test(start_paused = true)]
async fn readers_s1a_capture_none_keeps_barrier_and_halts() {
    use super::super::super::{ReaderConfig, rotation::probe};
    let (harness, path, old, bindings) = started(&row("m0", "before"));
    let (_, new) = transcript(
        &path,
        "next.jsonl",
        "s2",
        &row("waiting", "must stay behind barrier"),
    );
    bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
    let (mut sources, mut writer, mut deriver, mut owed) =
        reader_engine(&harness, bindings, Some(ReaderConfig { dormant: true }));
    let before = writer.store().cursor(&old).unwrap().clone();
    probe::keep_barrier(&mut sources, &old, len(&path));
    let captured = sources.capture(&mut writer, &mut deriver, &mut owed);
    assert!(
        matches!(captured, Err(WriterAlarm::Halted { .. })),
        "FD absence cannot release a kept barrier"
    );
    assert_eq!(writer.store().cursor(&new).unwrap().captured_through, 0);
    assert_eq!(writer.store().cursor(&old).unwrap(), &before);
    let mut frames = 0;
    writer
        .store()
        .for_each_frame(&new, |_| frames += 1)
        .unwrap();
    assert_eq!(frames, 0);
    assert!(matches!(
        sources.capture(&mut writer, &mut deriver, &mut owed),
        Err(WriterAlarm::Halted { .. })
    ));
    assert_eq!(writer.store().cursor(&new).unwrap().captured_through, 0);
}

#[tokio::test(start_paused = true)]
async fn readers_s1a_proof_clears_prior_quiet() {
    use super::super::super::{ReaderConfig, rotation::probe};
    let (harness, path, old, bindings) = started(&row("m0", "before"));
    let (_, new) = transcript(&path, "next.jsonl", "s2", b"");
    bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
    let (mut sources, mut writer, mut deriver, mut owed) = reader_engine(
        &harness,
        bindings.clone(),
        Some(ReaderConfig { dormant: true }),
    );
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    let now = std::time::SystemTime::now();
    reader_tend(&mut sources, &mut writer, now).await.unwrap();
    tokio::time::advance(std::time::Duration::from_secs(31)).await;
    reader_tend(
        &mut sources,
        &mut writer,
        now + std::time::Duration::from_secs(31),
    )
    .await
    .unwrap();
    assert!(!probe::reading(&sources, &old));
    assert!(probe::quiet(&mut sources, &old));
    bindings.commit(resumed(3, "tmux", &new));
    sources.follow(&mut writer).unwrap();
    assert!(
        !probe::quiet(&mut sources, &old),
        "proof starts a fresh retirement quiet"
    );
    assert!(probe::reading(&sources, &old));
    let next_path = new.path.clone();
    tokio::time::advance(std::time::Duration::from_secs(20)).await;
    append(&next_path, &row("first", "successor first"));
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    reader_tend(
        &mut sources,
        &mut writer,
        now + std::time::Duration::from_secs(51),
    )
    .await
    .unwrap();
    tokio::time::advance(std::time::Duration::from_secs(9)).await;
    reader_tend(
        &mut sources,
        &mut writer,
        now + std::time::Duration::from_secs(60),
    )
    .await
    .unwrap();
    assert!(!writer.store().cursor(&old).unwrap().retired);
    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    reader_tend(
        &mut sources,
        &mut writer,
        now + std::time::Duration::from_secs(61),
    )
    .await
    .unwrap();
    assert!(writer.store().cursor(&old).unwrap().retired);
    assert!(
        writer.store().rotation().unwrap().successors[&source_key(&old)]
            .proof
            .is_some()
    );
    while let Some(item) = owed.pop_front() {
        writer.deliver(&item).await;
    }
    assert_eq!(harness.port.posts(), ["successor first"]);
    assert!(matches!(
        reader_outcome(&mut writer, "first"),
        Some(PieceOutcome::Posted(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn readers_s1a_restore_flushes_init_only_eof_pending() {
    use super::super::super::rotation::probe;
    use super::super::super::{ReaderConfig, rotation::Sources};
    let (harness, path, old, bindings) = started(&row("m0", "before"));
    let (_, new) = transcript(&path, "next.jsonl", "s2", b"");
    bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
    let (_, mut writer, _, _) = reader_engine(&harness, bindings.clone(), None);
    assert!(
        !path
            .parent()
            .unwrap()
            .join("o_store")
            .join(CHANNEL.to_string())
            .join("cursor")
            .join(format!("{}.json", source_key(&old)))
            .exists()
    );
    let mut rotation = writer.store().rotation().unwrap();
    let mut link = owed_link(&old, 1);
    link.boundary = Boundary::Undecided;
    rotation.links.insert(source_key(&old), link);
    writer.store().write_rotation(&rotation).unwrap();
    drop(writer);
    let mut writer = harness.writer();
    let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings)
        .with_readers(ReaderConfig { dormant: true });
    let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    sources
        .resume(&mut writer, &mut deriver, &mut VecDeque::new())
        .unwrap();
    assert_eq!(
        writer
            .store()
            .rotation()
            .unwrap()
            .link(&old)
            .unwrap()
            .boundary,
        Boundary::Pending {
            candidates: vec![0, len(&path)]
        }
    );
    assert!(
        harness
            .alarms
            .taken()
            .contains(&WriterAlarm::BoundaryPending {
                source: old.clone()
            })
    );
    assert!(
        !probe::reading(&sources, &old),
        "validated EOF restoration releases the old FD"
    );
}

async fn reader_tend(
    sources: &mut super::super::super::rotation::Sources<FakeBindings>,
    writer: &mut Writer,
    now: std::time::SystemTime,
) -> Result<(), WriterAlarm> {
    use super::super::super::rotation::probe;
    for _ in 0..1000 {
        sources.tend_at(writer, now)?;
        if !probe::validating(sources) {
            return Ok(());
        }
        tokio::task::yield_now().await;
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    panic!("bounded prefix validation did not finish");
}

#[tokio::test(start_paused = true)]
async fn readers_s1a_unchanged_memory_survives_online_cleanup_after_25h() {
    reader_cleanup_25h(false).await;
}

#[tokio::test(start_paused = true)]
async fn readers_s1a_hourly_witness_survives_missing_restart_after_25h() {
    reader_cleanup_25h(true).await;
}

async fn reader_cleanup_25h(restart: bool) {
    use super::super::super::{ReaderConfig, rotation::Sources};
    use std::time::{Duration, SystemTime};
    let (harness, path, old, bindings) = started(&row("m0", "before"));
    let (next_path, new) = transcript(&path, "next.jsonl", "s2", b"");
    let t0 = SystemTime::now();
    std::fs::File::open(&path)
        .unwrap()
        .set_modified(t0 - Duration::from_secs(48 * 3600))
        .unwrap();
    bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
    let (mut sources, mut writer, mut deriver, mut owed) = reader_engine_at(
        &harness,
        bindings.clone(),
        Some(ReaderConfig { dormant: true }),
        t0,
    );
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    reader_tend(&mut sources, &mut writer, t0).await.unwrap();
    for hour in 1..=25 {
        tokio::time::advance(Duration::from_secs(3600)).await;
        reader_tend(
            &mut sources,
            &mut writer,
            t0 + Duration::from_secs(hour * 3600),
        )
        .await
        .unwrap();
    }
    let witness = path
        .parent()
        .unwrap()
        .join("o_store")
        .join(CHANNEL.to_string())
        .join("cleanup_witness")
        .join(format!("{}.json", source_key(&old)));
    assert!(
        witness.exists(),
        "verified observation is durable, including unchanged files"
    );
    let observed = super::super::super::rotation::probe::observed_at(&sources, &old);
    let durable: crate::services::tui_o::store::cleanup_witness::CleanupWitness =
        serde_json::from_slice(&std::fs::read(&witness).unwrap()).unwrap();
    let now = t0 + Duration::from_secs(25 * 3600);
    assert_eq!(
        observed,
        Some(now),
        "unchanged observations advance memory time"
    );
    assert_eq!(
        durable.observed_at, now,
        "hourly refresh advances durable time"
    );
    std::fs::remove_file(&path).unwrap();
    if restart {
        drop((sources, writer, deriver, owed));
        writer = harness.writer();
        sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings.clone())
            .with_readers(ReaderConfig { dormant: true });
        deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
        owed = VecDeque::new();
        sources
            .resume_at(&mut writer, &mut deriver, &mut owed, now)
            .unwrap();
    } else {
        tokio::time::advance(Duration::from_secs(60)).await;
        reader_tend(&mut sources, &mut writer, now + Duration::from_secs(60))
            .await
            .unwrap();
    }
    assert!(
        durable.bound_to(&crate::services::tui_o::store::spool::Cursor {
            retired: false,
            ..writer.store().cursor(&old).unwrap().clone()
        })
    );
    assert!(
        writer.store().cursor(&old).unwrap().retired,
        "cleanup retires durably"
    );
    assert!(
        harness
            .alarms
            .taken()
            .iter()
            .any(|a| matches!(a, WriterAlarm::CleanupRetired { source, .. } if source == &old))
    );
    append(&next_path, &row("fresh", "latest continues"));
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    while let Some(item) = owed.pop_front() {
        writer.deliver(&item).await;
    }
    assert_eq!(harness.port.posts(), ["latest continues"]);
    assert!(matches!(
        reader_outcome(&mut writer, "fresh"),
        Some(PieceOutcome::Posted(_))
    ));
    drop((sources, writer, deriver, owed));
    let mut writer = harness.writer();
    let mut restored = Sources::new(CHANNEL, ShadowProvider::Claude, bindings)
        .with_readers(ReaderConfig { dormant: true });
    let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    let mut owed = VecDeque::new();
    restored
        .resume_at(&mut writer, &mut deriver, &mut owed, now)
        .unwrap();
    restored
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    while let Some(item) = owed.pop_front() {
        writer.deliver(&item).await;
    }
    assert!(writer.store().cursor(&old).unwrap().retired);
    assert_eq!(harness.port.posts(), ["latest continues"]);
}

#[tokio::test(start_paused = true)]
async fn readers_s1a_default_sources_keeps_legacy_readers() {
    use super::super::super::{ReaderConfig, rotation::probe};
    use std::time::{Duration, SystemTime};
    assert!(!ReaderConfig::default().dormant);
    let (h, path, old, bindings) = started(&row("m0", "before"));
    let (next, new) = transcript(&path, "next.jsonl", "s2", b"");
    bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
    let (mut s, mut w, mut d, mut owed) = reader_engine(&h, bindings, None);
    let before = w.store().cursor(&old).unwrap().clone();
    let dir = path
        .parent()
        .unwrap()
        .join("o_store")
        .join(CHANNEL.to_string())
        .join("cleanup_witness");
    for hour in 0..=25 {
        tokio::time::advance(Duration::from_secs(3600)).await;
        s.capture(&mut w, &mut d, &mut owed).unwrap();
        reader_tend(
            &mut s,
            &mut w,
            SystemTime::now() + Duration::from_secs(hour * 3600),
        )
        .await
        .unwrap();
        assert!(probe::reading(&s, &old) && probe::reading(&s, &new));
        assert_eq!(w.store().cursor(&old).unwrap(), &before);
        assert!(!dir.exists());
        assert_eq!(probe::jobs_started(&s), 0);
    }
    append(&next, &row("fresh", "legacy continues"));
    s.capture(&mut w, &mut d, &mut owed).unwrap();
    while let Some(item) = owed.pop_front() {
        w.deliver(&item).await;
    }
    assert_eq!(h.port.posts(), ["legacy continues"]);
    assert!(matches!(
        reader_outcome(&mut w, "fresh"),
        Some(PieceOutcome::Posted(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn readers_s1a_dormant_partial_tail_commits_and_posts_once() {
    use super::super::super::{ReaderConfig, deliver::Step, rotation::probe};
    use crate::services::tui_o::store::spool::SpoolFrame;
    use sha2::Digest;
    use std::time::{Duration, SystemTime};
    let (h, path, old, bindings) = started(&row("m0", "before"));
    let (_, new) = transcript(&path, "next.jsonl", "s2", b"");
    bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
    let (mut s, mut w, mut d, mut owed) =
        reader_engine(&h, bindings, Some(ReaderConfig { dormant: true }));
    let now = SystemTime::now();
    s.capture(&mut w, &mut d, &mut owed).unwrap();
    reader_tend(&mut s, &mut w, now).await.unwrap();
    tokio::time::advance(Duration::from_secs(31)).await;
    reader_tend(&mut s, &mut w, now + Duration::from_secs(31))
        .await
        .unwrap();
    assert!(!probe::reading(&s, &old));
    let before = w.store().cursor(&old).unwrap().clone();
    let mut tail = row("late", "late exactly once");
    assert_eq!(tail.pop(), Some(b'\n'));
    append(&path, &tail);
    tokio::time::advance(Duration::from_secs(61)).await;
    reader_tend(&mut s, &mut w, now + Duration::from_secs(92))
        .await
        .unwrap();
    s.capture(&mut w, &mut d, &mut owed).unwrap();
    assert!(probe::reading(&s, &old));
    assert_eq!(w.store().cursor(&old).unwrap(), &before);
    assert!(owed.is_empty() && h.port.posts().is_empty());
    assert_eq!(reader_outcome(&mut w, "late"), None);
    append(&path, b"\n");
    s.capture(&mut w, &mut d, &mut owed).unwrap();
    while let Some(item) = owed.pop_front() {
        assert_eq!(w.deliver(&item).await, Step::Done);
    }
    let mut ranges = Vec::new();
    w.store()
        .for_each_frame(&old, |frame| {
            if let SpoolFrame::Record(record) = frame {
                ranges.push((record.start, record.end));
            }
        })
        .unwrap();
    assert_eq!(ranges, [(before.captured_through, len(&path))]);
    let cursor = w.store().cursor(&old).unwrap();
    assert_eq!(cursor.captured_through, len(&path));
    assert_eq!(
        cursor.prefix_hash,
        hex::encode(sha2::Sha256::digest(std::fs::read(&path).unwrap()))
    );
    assert!(!cursor.retired);
    assert!(matches!(
        reader_outcome(&mut w, "late"),
        Some(PieceOutcome::Posted(_))
    ));
    s.capture(&mut w, &mut d, &mut owed).unwrap();
    assert!(owed.is_empty());
    assert_eq!(h.port.posts(), ["late exactly once"]);
}

#[tokio::test(start_paused = true)]
async fn readers_s1a_unsafe_missing_never_retires() {
    use super::super::super::{
        ReaderConfig,
        rotation::{Sources, probe},
    };
    use std::time::{Duration, SystemTime};
    for case in [
        "no_witness",
        "recent",
        "future",
        "partial",
        "barrier",
        "old_witness",
        "latest",
        "malformed",
        "retire_write",
    ] {
        let (h, path, old, bindings) = started(&row("m0", "before"));
        let (_, new) = transcript(&path, "next.jsonl", "s2", b"");
        let now = SystemTime::now();
        if case != "recent" {
            std::fs::File::open(&path)
                .unwrap()
                .set_modified(now - Duration::from_secs(48 * 3600))
                .unwrap();
        }
        if case == "future" {
            std::fs::File::open(&path)
                .unwrap()
                .set_modified(now + Duration::from_secs(3600))
                .unwrap();
        }
        bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
        if case == "future" {
            let (_, writer, _, _) = reader_engine(&h, bindings.clone(), None);
            drop(writer);
            let mut w = h.writer();
            let mut s = Sources::new(CHANNEL, ShadowProvider::Claude, bindings)
                .with_readers(ReaderConfig { dormant: true });
            assert!(matches!(
                s.resume_at(
                    &mut w,
                    &mut UnitDeriver::new(CHANNEL, ShadowProvider::Claude),
                    &mut VecDeque::new(),
                    now
                ),
                Err(WriterAlarm::Halted { .. })
            ));
            assert!(!w.store().cursor(&old).unwrap().retired);
            assert!(h.port.posts().is_empty());
            continue;
        }
        let (mut s, mut w, mut d, mut owed) = reader_engine_at(
            &h,
            bindings.clone(),
            Some(ReaderConfig { dormant: true }),
            now,
        );
        s.capture(&mut w, &mut d, &mut owed).unwrap();
        reader_tend(&mut s, &mut w, now).await.unwrap();
        let before = w.store().cursor(&old).unwrap().clone();
        if case == "latest" {
            bindings.commit(hop_at(3, "tmux", Some(&new), &old, BindingCause::Clear, ""));
            s.follow(&mut w).unwrap();
        }
        if case == "partial" {
            let mut tail = row("late", "unfinished");
            tail.pop();
            append(&path, &tail);
            s.capture(&mut w, &mut d, &mut owed).unwrap();
            assert_eq!(w.store().cursor(&old).unwrap(), &before);
        }
        if case == "barrier" {
            probe::keep_barrier(&mut s, &old, len(&path));
        }
        if matches!(case, "no_witness" | "malformed") {
            let witness = path
                .parent()
                .unwrap()
                .join("o_store")
                .join(CHANNEL.to_string())
                .join("cleanup_witness")
                .join(format!("{}.json", source_key(&old)));
            if case == "no_witness" {
                std::fs::remove_file(&witness).unwrap();
            } else {
                std::fs::write(&witness, b"{torn").unwrap();
            }
        }
        let cursor_dir = path
            .parent()
            .unwrap()
            .join("o_store")
            .join(CHANNEL.to_string())
            .join("cursor");
        let fault = (case == "retire_write").then(|| {
            crate::services::tui_o::store::fault::plant(
                &cursor_dir,
                crate::services::tui_o::store::fault::Step::Write,
                std::io::ErrorKind::StorageFull,
                Some(1),
            )
        });
        std::fs::remove_file(&path).unwrap();
        let at = now + Duration::from_secs(if case == "old_witness" { 24 * 3600 } else { 62 });
        let failed = if matches!(case, "no_witness" | "old_witness" | "malformed") {
            drop((s, w, d, owed));
            w = h.writer();
            s = Sources::new(CHANNEL, ShadowProvider::Claude, bindings)
                .with_readers(ReaderConfig { dormant: true });
            s.resume_at(
                &mut w,
                &mut UnitDeriver::new(CHANNEL, ShadowProvider::Claude),
                &mut VecDeque::new(),
                at,
            )
        } else {
            match s.capture(&mut w, &mut d, &mut owed) {
                Err(alarm) => Err(alarm),
                Ok(()) => reader_tend(&mut s, &mut w, at).await,
            }
        };
        assert!(matches!(failed, Err(WriterAlarm::Halted { .. })), "{case}");
        assert_eq!(w.store().cursor(&old).unwrap(), &before, "{case}");
        drop(fault);
        assert_eq!(h.channel().cursor(&old).unwrap(), &before, "durable {case}");
        assert_eq!(
            w.store().cursor(&new).unwrap().captured_through,
            0,
            "{case}"
        );
        assert!(h.port.posts().is_empty(), "{case}");
        assert!(
            !h.alarms
                .taken()
                .iter()
                .any(|alarm| matches!(alarm, WriterAlarm::CleanupRetired { .. })),
            "{case}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn readers_s1a_active_missing_checks_retained_fd_before_cleanup() {
    use super::super::super::ReaderConfig;
    use std::time::{Duration, SystemTime};
    for unseen_tail in [false, true] {
        let (h, path, old, bindings) = started(&row("m0", "before"));
        let (next, new) = transcript(&path, "next.jsonl", "s2", b"");
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(48 * 3600))
            .unwrap();
        bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
        let (mut s, mut w, mut d, mut owed) =
            reader_engine(&h, bindings, Some(ReaderConfig::default()));
        s.capture(&mut w, &mut d, &mut owed).unwrap();
        reader_tend(&mut s, &mut w, SystemTime::now())
            .await
            .unwrap();
        let before = w.store().cursor(&old).unwrap().clone();
        if unseen_tail {
            append(&path, &row("unseen", "must halt"));
        }
        std::fs::remove_file(&path).unwrap();
        let captured = s.capture(&mut w, &mut d, &mut owed);
        if unseen_tail {
            assert!(matches!(captured, Err(WriterAlarm::Halted { .. })));
            assert_eq!(w.store().cursor(&old).unwrap(), &before);
        } else {
            captured.unwrap();
            assert!(w.store().cursor(&old).unwrap().retired);
            assert!(h.alarms.taken().iter().any(|a| matches!(a, WriterAlarm::CleanupRetired { source, offset, age, .. } if source == &old && *offset == before.captured_through && *age >= Duration::from_secs(48 * 3600))));
            append(&next, &row("fresh", "active continues"));
            s.capture(&mut w, &mut d, &mut owed).unwrap();
            while let Some(item) = owed.pop_front() {
                w.deliver(&item).await;
            }
            assert_eq!(h.port.posts(), ["active continues"]);
            assert!(matches!(
                reader_outcome(&mut w, "fresh"),
                Some(PieceOutcome::Posted(_))
            ));
        }
    }
}

#[tokio::test(start_paused = true)]
async fn readers_s1a_worker_rechecks_cursor_and_fingerprint_before_applying() {
    use super::super::super::{ReaderConfig, rotation::probe};
    use std::time::{Duration, SystemTime};
    for change_cursor in [false, true] {
        let (h, path, old, bindings) = started(&row("m0", "before"));
        let (_, new) = transcript(&path, "next.jsonl", "s2", b"");
        bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
        let (mut s, mut w, mut d, mut owed) =
            reader_engine(&h, bindings, Some(ReaderConfig { dormant: true }));
        s.capture(&mut w, &mut d, &mut owed).unwrap();
        let now = SystemTime::now();
        probe::hold_validation(&mut s, true);
        s.tend_at(&mut w, now).unwrap();
        assert_eq!(probe::jobs_started(&s), 1);
        for _ in 0..1000 {
            if probe::ready(&s) {
                break;
            }
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            probe::ready(&s),
            "the old snapshot actually finished validating"
        );
        if change_cursor {
            append(&path, &row("late", "race tail"));
            s.capture(&mut w, &mut d, &mut owed).unwrap();
        } else {
            std::fs::File::open(&path)
                .unwrap()
                .set_modified(now - Duration::from_secs(1))
                .unwrap();
        }
        probe::hold_validation(&mut s, false);
        assert_eq!(probe::apply_validation(&mut s, &mut w, now).unwrap(), None);
        assert!(
            w.store().cleanup_witness(&old).unwrap().is_none(),
            "discarded result cannot publish old evidence"
        );
        assert!(probe::reading(&s, &old));
        assert!(!w.store().cursor(&old).unwrap().retired);
        reader_tend(&mut s, &mut w, SystemTime::now())
            .await
            .unwrap();
        let witness = w.store().cleanup_witness(&old).unwrap().unwrap();
        assert!(witness.bound_to(w.store().cursor(&old).unwrap()));
        assert_eq!(
            witness.fingerprint,
            crate::services::tui_o::shadow::capture::Fingerprint::path(&old).unwrap()
        );
        while let Some(item) = owed.pop_front() {
            w.deliver(&item).await;
        }
        if change_cursor {
            assert_eq!(h.port.posts(), ["race tail"]);
            assert!(matches!(
                reader_outcome(&mut w, "late"),
                Some(PieceOutcome::Posted(_))
            ));
        } else {
            assert!(h.port.posts().is_empty());
        }
    }
}

#[tokio::test(start_paused = true)]
async fn readers_s1a_append_at_quiet_boundary_keeps_reader_active() {
    use super::super::super::{ReaderConfig, rotation::probe};
    use std::time::{Duration, SystemTime};
    let (h, path, old, bindings) = started(&row("m0", "before"));
    let (_, new) = transcript(&path, "next.jsonl", "s2", b"");
    bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
    let (mut s, mut w, mut d, mut owed) =
        reader_engine(&h, bindings, Some(ReaderConfig { dormant: true }));
    let now = SystemTime::now();
    s.capture(&mut w, &mut d, &mut owed).unwrap();
    reader_tend(&mut s, &mut w, now).await.unwrap();
    tokio::time::advance(Duration::from_millis(29_999)).await;
    append(&path, &row("late", "boundary append"));
    tokio::time::advance(Duration::from_millis(1)).await;
    reader_tend(&mut s, &mut w, now + Duration::from_secs(30))
        .await
        .unwrap();
    assert!(probe::reading(&s, &old));
    s.capture(&mut w, &mut d, &mut owed).unwrap();
    assert_eq!(w.store().cursor(&old).unwrap().captured_through, len(&path));
    assert!(!w.store().cursor(&old).unwrap().retired);
    while let Some(item) = owed.pop_front() {
        w.deliver(&item).await;
    }
    assert_eq!(h.port.posts(), ["boundary append"]);
    assert!(matches!(
        reader_outcome(&mut w, "late"),
        Some(PieceOutcome::Posted(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn readers_s1a_clock_regression_invalidates_dormant_cleanup_evidence() {
    use super::super::super::{ReaderConfig, rotation::probe};
    use std::time::{Duration, SystemTime};
    let (h, path, old, bindings) = started(&row("m0", "before"));
    let (_, new) = transcript(&path, "next.jsonl", "s2", b"");
    let now = SystemTime::now();
    std::fs::File::open(&path)
        .unwrap()
        .set_modified(now - Duration::from_secs(48 * 3600))
        .unwrap();
    bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
    let (mut s, mut w, mut d, mut owed) =
        reader_engine_at(&h, bindings, Some(ReaderConfig { dormant: true }), now);
    s.capture(&mut w, &mut d, &mut owed).unwrap();
    reader_tend(&mut s, &mut w, now).await.unwrap();
    tokio::time::advance(Duration::from_secs(31)).await;
    reader_tend(&mut s, &mut w, now + Duration::from_secs(31))
        .await
        .unwrap();
    assert!(!probe::reading(&s, &old));
    tokio::time::advance(Duration::from_secs(10)).await;
    s.tend_at(&mut w, now + Duration::from_secs(50)).unwrap();
    assert!(matches!(
        s.tend_at(&mut w, now + Duration::from_secs(40)),
        Err(WriterAlarm::Halted { .. })
    ));
    assert_eq!(probe::observed_at(&s, &old), None);
    std::fs::remove_file(&path).unwrap();
    tokio::time::advance(Duration::from_secs(100)).await;
    assert!(matches!(
        s.tend_at(&mut w, now + Duration::from_secs(100)),
        Err(WriterAlarm::Halted { .. })
    ));
    assert!(!w.store().cursor(&old).unwrap().retired);
    assert!(
        !h.alarms
            .taken()
            .iter()
            .any(|a| matches!(a, WriterAlarm::CleanupRetired { .. }))
    );
}

#[tokio::test(start_paused = true)]
async fn readers_s1a_worker_preserves_replaced_or_unreadable() {
    use super::super::super::{ReaderConfig, rotation::probe};
    use std::time::{Duration, SystemTime};
    for replace in [false, true] {
        let (h, path, old, bindings) = started(&row("m0", "before"));
        let (_, new) = transcript(&path, "next.jsonl", "s2", b"");
        bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
        let (mut s, mut w, mut d, mut owed) =
            reader_engine(&h, bindings, Some(ReaderConfig { dormant: true }));
        s.capture(&mut w, &mut d, &mut owed).unwrap();
        let now = SystemTime::now();
        probe::hold_validation(&mut s, true);
        s.tend_at(&mut w, now).unwrap();
        for _ in 0..1000 {
            if probe::ready(&s) {
                break;
            }
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(probe::ready(&s));
        std::fs::rename(&path, path.with_extension("retained")).unwrap();
        if replace {
            std::fs::write(&path, row("replacement", "must halt")).unwrap();
        } else {
            #[cfg(unix)]
            std::os::unix::fs::symlink(&path, &path).unwrap();
            #[cfg(not(unix))]
            std::fs::create_dir(&path).unwrap();
        }
        probe::hold_validation(&mut s, false);
        let result = probe::apply_validation(&mut s, &mut w, now);
        assert!(matches!(result, Err(WriterAlarm::Halted { .. })));
        assert!(w.store().cleanup_witness(&old).unwrap().is_none());
        assert!(!w.store().cursor(&old).unwrap().retired);
        assert!(h.port.posts().is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn readers_s1a_spoolfull_pending_keeps_fd_and_successor_barrier() {
    use super::super::super::{ReaderConfig, deliver::Step, rotation::probe};
    use crate::services::tui_o::store::spool::SpoolFrame;
    use std::time::{Duration, SystemTime};
    for unlink in [false, true] {
        let (h, path, old, bindings) = started(&row("m0", "before"));
        let (next_path, new) = transcript(&path, "next.jsonl", "s2", b"");
        let now = SystemTime::now();
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(now - Duration::from_secs(48 * 3600))
            .unwrap();
        bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
        let (mut s, mut w, mut d, mut owed) = reader_engine_at(
            &h,
            bindings.clone(),
            Some(ReaderConfig { dormant: true }),
            now,
        );
        s.capture(&mut w, &mut d, &mut owed).unwrap();
        reader_tend(&mut s, &mut w, now).await.unwrap();
        assert!(w.store().cleanup_witness(&old).unwrap().is_some());
        let before = w.store().cursor(&old).unwrap().clone();
        let payload = "x".repeat(1500);
        append(&path, &row("blocked", &payload));
        append(&next_path, &row("next", "successor"));
        bindings.commit(hop_at(3, "tmux", Some(&old), &new, BindingCause::Clear, ""));
        s.follow(&mut w).unwrap();
        let kept = w.store().rotation().unwrap().successors[&source_key(&old)]
            .drain_to
            .unwrap();
        assert_eq!(kept, len(&path));
        let room = w.store().spool_bytes() + 1024;
        w.store().set_limits_for_test(1, room);
        s.capture(&mut w, &mut d, &mut owed).unwrap();
        assert!(h.alarms.taken().contains(&WriterAlarm::SpoolFull));
        assert!(
            s.collectable()
                .iter()
                .any(|(source, keep)| source == &old && *keep == 0)
        );
        assert_eq!(w.store().cursor(&old).unwrap(), &before);
        assert_eq!(w.store().cursor(&new).unwrap().captured_through, 0);
        assert!(owed.is_empty() && h.port.posts().is_empty());
        tokio::time::advance(Duration::from_secs(31)).await;
        reader_tend(&mut s, &mut w, now + Duration::from_secs(31))
            .await
            .unwrap();
        assert!(probe::reading(&s, &old));
        assert!(!w.store().cursor(&old).unwrap().retired);
        if unlink {
            std::fs::remove_file(&path).unwrap();
            s.capture(&mut w, &mut d, &mut owed).unwrap();
            assert!(matches!(
                reader_tend(&mut s, &mut w, now + Duration::from_secs(32)).await,
                Err(WriterAlarm::Halted { .. })
            ));
            assert_eq!(w.store().cursor(&old).unwrap(), &before);
            assert_eq!(w.store().cursor(&new).unwrap().captured_through, 0);
            assert!(
                s.collectable()
                    .iter()
                    .any(|(source, keep)| source == &old && *keep == 0)
            );
            assert!(
                !h.alarms
                    .taken()
                    .iter()
                    .any(|a| matches!(a, WriterAlarm::CleanupRetired { .. }))
            );
            continue;
        }
        w.store().set_limits_for_test(1, u64::MAX);
        s.capture(&mut w, &mut d, &mut owed).unwrap();
        while let Some(item) = owed.pop_front() {
            assert_eq!(w.deliver(&item).await, Step::Done);
        }
        let mut ranges = Vec::new();
        w.store()
            .for_each_frame(&old, |frame| {
                if let SpoolFrame::Record(record) = frame {
                    ranges.push((record.start, record.end));
                }
            })
            .unwrap();
        assert_eq!(ranges, [(before.captured_through, kept)]);
        assert_eq!(h.port.posts(), [payload.as_str(), "successor"]);
        assert!(matches!(
            reader_outcome(&mut w, "blocked"),
            Some(PieceOutcome::Posted(_))
        ));
        assert!(matches!(
            reader_outcome(&mut w, "next"),
            Some(PieceOutcome::Posted(_))
        ));
        s.capture(&mut w, &mut d, &mut owed).unwrap();
        assert!(owed.is_empty());
        assert_eq!(h.port.posts().len(), 2);
    }
}

#[tokio::test(start_paused = true)]
async fn readers_s1a_hourly_refresh_unlink_discards_result_and_keeps_cleanup_available() {
    use super::super::super::{
        ReaderConfig,
        deliver::Step,
        rotation::{Sources, probe},
    };
    use std::time::{Duration, SystemTime};
    for restart in [false, true] {
        let (h, path, old, bindings) = started(&row("m0", "before"));
        let (next_path, new) = transcript(&path, "next.jsonl", "s2", b"");
        let now = SystemTime::now();
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(now - Duration::from_secs(48 * 3600))
            .unwrap();
        bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
        let (mut s, mut w, mut d, mut owed) = reader_engine_at(
            &h,
            bindings.clone(),
            Some(ReaderConfig { dormant: true }),
            now,
        );
        s.capture(&mut w, &mut d, &mut owed).unwrap();
        reader_tend(&mut s, &mut w, now).await.unwrap();
        for hour in 1..=25 {
            tokio::time::advance(Duration::from_secs(3600)).await;
            let at = now + Duration::from_secs(hour * 3600);
            reader_tend(&mut s, &mut w, at).await.unwrap();
            let witness = w.store().cleanup_witness(&old).unwrap().unwrap();
            assert_eq!(witness.observed_at, at, "hour {hour} really persisted");
            assert!(witness.bound_to(w.store().cursor(&old).unwrap()));
        }
        assert!(!probe::reading(&s, &old));
        let file = path
            .parent()
            .unwrap()
            .join("o_store")
            .join(CHANNEL.to_string())
            .join("cleanup_witness")
            .join(format!("{}.json", source_key(&old)));
        let prior = std::fs::read(&file).unwrap();
        let at = now + Duration::from_secs(26 * 3600);
        tokio::time::advance(Duration::from_secs(3600)).await;
        probe::hold_validation(&mut s, true);
        let jobs = probe::jobs_started(&s);
        s.tend_at(&mut w, at).unwrap();
        assert_eq!(probe::jobs_started(&s), jobs + 1);
        for _ in 0..1000 {
            if probe::ready(&s) {
                break;
            }
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(probe::ready(&s));
        std::fs::remove_file(&path).unwrap();
        probe::hold_validation(&mut s, false);
        assert_eq!(probe::apply_validation(&mut s, &mut w, at).unwrap(), None);
        assert_eq!(std::fs::read(&file).unwrap(), prior);
        assert!(!w.store().cursor(&old).unwrap().retired);
        if restart {
            drop((s, w, d, owed));
            w = h.writer();
            s = Sources::new(CHANNEL, ShadowProvider::Claude, bindings)
                .with_readers(ReaderConfig { dormant: true });
            d = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
            owed = VecDeque::new();
            s.resume_at(&mut w, &mut d, &mut owed, at).unwrap();
        } else {
            tokio::time::advance(Duration::from_secs(61)).await;
            reader_tend(&mut s, &mut w, at + Duration::from_secs(61))
                .await
                .unwrap();
        }
        assert!(w.store().cursor(&old).unwrap().retired);
        assert_eq!(std::fs::read(&file).unwrap(), prior);
        assert!(
            h.alarms
                .taken()
                .iter()
                .any(|a| matches!(a, WriterAlarm::CleanupRetired { source, .. } if source == &old))
        );
        append(&next_path, &row("fresh", "continues after refresh race"));
        s.capture(&mut w, &mut d, &mut owed).unwrap();
        while let Some(item) = owed.pop_front() {
            assert_eq!(w.deliver(&item).await, Step::Done);
        }
        assert_eq!(h.port.posts(), ["continues after refresh race"]);
        assert!(matches!(
            reader_outcome(&mut w, "fresh"),
            Some(PieceOutcome::Posted(_))
        ));
    }
}

fn s1b_spawn_reader_actor(
    harness: &Harness,
    bindings: Arc<FakeBindings>,
) -> (watch::Sender<bool>, Actor, watch::Receiver<bool>) {
    use crate::services::tui_o::writer::{ReaderConfig, actor};
    let (stop, stopped) = watch::channel(false);
    let (resumed, ready) = watch::channel(false);
    let unsettled = watch::channel(None).0;
    let task = actor::spawn_projecting(
        &WriterConfig { enabled: true },
        Some(ReaderConfig { dormant: true }),
        harness.writer(),
        ShadowProvider::Claude,
        bindings,
        (stopped, resumed, unsettled, actor::Owing::default()),
    )
    .expect("the production actor is enabled");
    (stop, task, ready)
}

async fn s1b_actor_until(mut observed: impl FnMut() -> bool) {
    for _ in 0..160 {
        if observed() {
            return;
        }
        polls(1).await;
        tokio::task::yield_now().await;
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(
        observed(),
        "the production actor reached the required state"
    );
}

#[tokio::test(start_paused = true)]
async fn readers_s1b_actor_on_cleanup_retires_and_latest_still_posts() {
    use std::time::{Duration, SystemTime};
    let (h, path, old, bindings) = started(&row("m0", "before"));
    std::fs::File::open(&path)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(48 * 3600))
        .unwrap();
    let (next_path, new) = transcript(&path, "next.jsonl", "s2", b"");
    bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
    let (stop, task, ready) = s1b_spawn_reader_actor(&h, bindings);
    s1b_actor_until(|| *ready.borrow() && h.alarms.readers() == Some(1)).await;
    let before = h.channel().cursor(&old).unwrap().clone();
    let witness = h.channel().cleanup_witness(&old).unwrap().unwrap();
    assert!(witness.bound_to(&before) && witness.eligible(SystemTime::now()));
    assert!(!before.retired, "Dormant does not mean retired");
    std::fs::remove_file(&path).unwrap();
    s1b_actor_until(|| retired(&h, &old)).await;
    assert!(h.alarms.taken().iter().any(|alarm| {
        matches!(alarm, WriterAlarm::CleanupRetired { source, .. } if source == &old)
    }));
    assert!(!task.is_finished(), "cleanup keeps the actor running");
    append(&next_path, &row("fresh", "latest after cleanup"));
    s1b_actor_until(|| h.port.posts() == ["latest after cleanup"]).await;
    halt(stop, task).await;
    let mut writer = h.writer();
    let cursor = writer.store().cursor(&old).unwrap();
    assert!(cursor.retired);
    assert_eq!(cursor.captured_through, before.captured_through);
    assert_eq!(cursor.prefix_hash, before.prefix_hash);
    assert!(matches!(
        reader_outcome(&mut writer, "fresh"),
        Some(PieceOutcome::Posted(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn readers_s1b_actor_on_late_tail_spools_and_posts_once() {
    use crate::services::tui_o::store::spool::SpoolFrame;
    let (h, path, old, bindings) = started(&row("m0", "before"));
    let (_, new) = transcript(&path, "next.jsonl", "s2", b"");
    bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
    let (stop, task, ready) = s1b_spawn_reader_actor(&h, bindings);
    s1b_actor_until(|| *ready.borrow() && h.alarms.readers() == Some(1)).await;
    let before = h.channel().cursor(&old).unwrap().clone();
    append(&path, &row("late", "late actor tail"));
    s1b_actor_until(|| h.port.posts() == ["late actor tail"]).await;
    polls(3).await;
    assert!(!task.is_finished());
    halt(stop, task).await;
    let mut writer = h.writer();
    let mut spans = Vec::new();
    writer
        .store()
        .for_each_frame(&old, |frame| {
            if let SpoolFrame::Record(record) = frame {
                spans.push((record.start, record.end));
            }
        })
        .unwrap();
    assert_eq!(spans, [(before.captured_through, len(&path))]);
    let cursor = writer.store().cursor(&old).unwrap();
    assert!(!cursor.retired);
    assert_eq!(cursor.captured_through, len(&path));
    assert_eq!(
        cursor.prefix_hash,
        hex::encode(Sha256::digest(std::fs::read(&path).unwrap()))
    );
    assert_eq!(h.port.posts(), ["late actor tail"]);
    assert!(matches!(
        reader_outcome(&mut writer, "late"),
        Some(PieceOutcome::Posted(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn readers_s1b_actor_on_recent_missing_halts_without_retirement() {
    let (h, path, old, bindings) = started(&row("m0", "before"));
    let (_, new) = transcript(&path, "next.jsonl", "s2", b"");
    bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
    let (stop, task, ready) = s1b_spawn_reader_actor(&h, bindings);
    s1b_actor_until(|| *ready.borrow() && h.alarms.readers() == Some(1)).await;
    let before = h.channel().cursor(&old).unwrap().clone();
    std::fs::remove_file(&path).unwrap();
    s1b_actor_until(|| task.is_finished()).await;
    let cause = task.await.unwrap().expect("unsafe missing stops the actor");
    assert!(matches!(cause.alarm, WriterAlarm::Halted { .. }));
    assert!(
        cause.io.is_none(),
        "source damage is not a transient store retry"
    );
    assert!(ready.has_changed().is_err(), "the resumed publisher ended");
    assert_eq!(h.channel().cursor(&old).unwrap(), &before);
    assert!(h.port.posts().is_empty());
    assert!(
        !h.alarms
            .taken()
            .iter()
            .any(|alarm| { matches!(alarm, WriterAlarm::CleanupRetired { .. }) })
    );
    drop(stop);
}

#[tokio::test(start_paused = true)]
async fn readers_s1b_actor_on_resume_publishes_after_init_only_eof_pending_flush() {
    let (h, path, old, bindings) = started(&row("m0", "before"));
    let (_, new) = transcript(&path, "next.jsonl", "s2", b"");
    bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
    let (_, mut writer, _, _) = reader_engine(&h, bindings.clone(), None);
    let cursor_path = path
        .parent()
        .unwrap()
        .join("o_store")
        .join(CHANNEL.to_string())
        .join("cursor")
        .join(format!("{}.json", source_key(&old)));
    assert!(
        !cursor_path.exists(),
        "the old source still comes from init"
    );
    let mut rotation = writer.store().rotation().unwrap();
    let mut link = owed_link(&old, 1);
    link.boundary = Boundary::Undecided;
    rotation.links.insert(source_key(&old), link);
    writer.store().write_rotation(&rotation).unwrap();
    drop(writer);
    let (stop, task, ready) = s1b_spawn_reader_actor(&h, bindings);
    s1b_actor_until(|| *ready.borrow()).await;
    assert_eq!(
        h.channel().rotation().unwrap().link(&old).unwrap().boundary,
        Boundary::Pending {
            candidates: vec![0, len(&path)]
        }
    );
    assert!(
        h.alarms
            .taken()
            .contains(&WriterAlarm::BoundaryPending { source: old })
    );
    assert!(h.port.posts().is_empty());
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn readers_s1b_actor_on_drains_predecessor_before_successor() {
    let (h, path, old, bindings) = started(&row("m0", "before"));
    append(
        &path,
        &[backlog(0), row("last", "old barrier tail")].concat(),
    );
    let (_, new) = transcript(
        &path,
        "next.jsonl",
        "s2",
        &row("first", "new after barrier"),
    );
    bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
    let (stop, task, ready) = s1b_spawn_reader_actor(&h, bindings);
    s1b_actor_until(|| *ready.borrow() && h.port.posts().len() == 2).await;
    assert_eq!(h.port.posts(), ["old barrier tail", "new after barrier"]);
    assert_eq!(
        h.channel().cursor(&old).unwrap().captured_through,
        len(&path)
    );
    assert!(successor(&h, &old).proof.is_none());
    assert!(!retired(&h, &old));
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn readers_s1b_configless_actor_keeps_the_legacy_off_path() {
    let (h, path, old, bindings) = started(&row("m0", "before"));
    let (_, new) = transcript(&path, "next.jsonl", "s2", b"");
    bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
    let (stop, task) = spawn_with(h.writer(), ShadowProvider::Claude, bindings);
    s1b_actor_until(|| h.alarms.readers() == Some(2)).await;
    polls(35).await;
    assert_eq!(h.alarms.readers(), Some(2));
    assert!(!retired(&h, &old));
    assert!(
        !path
            .parent()
            .unwrap()
            .join("o_store")
            .join(CHANNEL.to_string())
            .join("cleanup_witness")
            .exists()
    );
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn readers_s1b_actor_on_proof_waits_for_successor_and_fresh_quiet() {
    let (h, path, old, bindings) = started(&row("m0", "before"));
    let (next_path, new) = transcript(&path, "next.jsonl", "s2", b"");
    bindings.commit(hop_at(2, "tmux", Some(&old), &new, BindingCause::Clear, ""));
    let (stop, task, ready) = s1b_spawn_reader_actor(&h, bindings.clone());
    s1b_actor_until(|| *ready.borrow() && h.alarms.readers() == Some(1)).await;
    bindings.commit(resumed(3, "tmux", &new));
    s1b_actor_until(|| successor(&h, &old).proof.is_some()).await;
    polls(20).await;
    assert!(
        !retired(&h, &old),
        "proof alone cannot retire an empty successor"
    );
    append(&next_path, &row("first", "successor starts quiet"));
    s1b_actor_until(|| h.port.posts() == ["successor starts quiet"]).await;
    polls(7).await;
    assert!(
        !retired(&h, &old),
        "old Dormant quiet is not retirement quiet"
    );
    s1b_actor_until(|| retired(&h, &old)).await;
    assert!(!task.is_finished());
    halt(stop, task).await;
    let mut writer = h.writer();
    assert!(matches!(
        reader_outcome(&mut writer, "first"),
        Some(PieceOutcome::Posted(_))
    ));
    assert_eq!(h.port.posts(), ["successor starts quiet"]);
}
