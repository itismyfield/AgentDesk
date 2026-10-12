use super::*;
use crate::services::tui_o::store::rotation::{Boundary, ResolveFrom};

fn prompt(uuid: &str, text: &str) -> Vec<u8> {
    codex_line(serde_json::json!({"type": "user", "uuid": uuid, "message": {"content": text}}))
}

fn boundary(harness: &Harness, source: &SourceId) -> Boundary {
    harness
        .channel()
        .rotation()
        .unwrap()
        .link(source)
        .unwrap()
        .boundary
        .clone()
}

#[tokio::test(start_paused = true)]
async fn pending_parent_resolved_past_row_rebuilds_child_debt_once() {
    let (harness, a_path, a, bindings) = started(&prompt("a", "adopted"));
    let z = prompt("z", "unrelated");
    let r = row("r", "shared row");
    let parent_body = [z.clone(), r.clone()].concat();
    let parent_start = z.len() as u64;
    let parent_from = parent_body.len() as u64;
    let fresh = prompt("fresh", &"x".repeat(r.len()));
    let child_start = (z.len() + fresh.len()) as u64;
    assert!(parent_start < parent_from && parent_from <= child_start);
    assert_eq!(parent_from, parent_start + r.len() as u64);
    let (p_path, p) = transcript(&a_path, "p.jsonl", "sp", &parent_body);
    let (_, b) = transcript(&a_path, "b.jsonl", "sb", &[z, fresh, r].concat());
    let attempts = Arc::new(AtomicUsize::new(0));
    let counted = attempts.clone();
    *harness.lease.on_acquire.lock().unwrap() = Some(Box::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
    }));
    harness.lease.busy.store(true, Ordering::SeqCst);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    bindings.commit(rotate(2, &a, &p, BindingCause::Fork));
    polls(3).await;
    assert!(matches!(boundary(&harness, &p), Boundary::Pending { .. }));
    assert_eq!(
        harness.channel().cursor(&p).unwrap().captured_through,
        parent_from
    );
    bindings.commit(rotate(3, &p, &b, BindingCause::Fork));
    polls(3).await;
    assert_eq!(
        boundary(&harness, &b),
        Boundary::Owed { from: parent_start }
    );
    assert_eq!(
        harness.channel().cursor(&b).unwrap().captured_through,
        std::fs::metadata(&b.path).unwrap().len()
    );
    assert!(
        attempts.load(Ordering::SeqCst) > 0,
        "r really reached the busy lease"
    );
    assert!(harness.port.posts().is_empty());
    assert_eq!(
        harness.channel().ledger().next_serial(),
        0,
        "no Prepared before crash"
    );
    halt(stop, task).await;
    let resolution = harness
        .store
        .record_boundary_resolved(
            CHANNEL,
            &p_path.display().to_string(),
            &ResolveFrom::Offset(parent_from),
            "op",
        )
        .unwrap();
    assert_eq!(resolution, (p.clone(), parent_from));
    harness.lease.busy.store(false, Ordering::SeqCst);
    for _ in 0..2 {
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        polls(4).await;
        assert_eq!(boundary(&harness, &p), Boundary::Owed { from: parent_from });
        assert_eq!(
            harness.port.posts(),
            ["shared row"],
            "B.r delivered exactly once"
        );
        assert_eq!(harness.channel().ledger().next_serial(), 1);
        assert!(harness.channel().ledger().unresolved().is_none());
        assert!(matches!(
            harness.channel().ledger().piece(0).unwrap().outcome,
            Some(PieceOutcome::Posted(_))
        ));
        halt(stop, task).await;
    }
}

use crate::services::tui_o::store::spool::source_key;
use crate::services::tui_o::writer::rotation::{Owe, Sources};

#[derive(Default)]
struct RecordingOwe {
    records: Vec<(SourceId, u64, u64)>,
    derived: VecDeque<Derived>,
}

impl Owe for RecordingOwe {
    fn owe(&mut self, source: &SourceId, record: &CapturedRecord, derived: Vec<Derived>) {
        self.records
            .push((source.clone(), record.start, record.end));
        self.derived.extend(derived);
    }
}

impl RecordingOwe {
    fn saw(&self, source: &SourceId, start: u64, end: u64) -> bool {
        self.records.contains(&(source.clone(), start, end))
    }
}

fn observe(harness: &Harness, bindings: Arc<FakeBindings>) -> RecordingOwe {
    let mut writer = harness.writer();
    let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings);
    let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    let mut owed = RecordingOwe::default();
    sources
        .resume(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    sources.follow(&mut writer).unwrap();
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    owed
}

#[tokio::test(start_paused = true)]
async fn posted_parent_rows_are_masked_live_and_on_resume_at_the_exact_boundary() {
    for root in [true, false] {
        let prefix = prompt("prefix", &"한".repeat(500));
        let adopted = row("adopted", "before adoption");
        let (harness, a_path, a, bindings) = started(&[prefix.clone(), adopted.clone()].concat());
        let r = row("posted", "parent posted");
        let (mut stop, mut task) =
            spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        let p = if root {
            append(&a_path, &r);
            a.clone()
        } else {
            let (_, p) = transcript(
                &a_path,
                "p.jsonl",
                "sp",
                &[prefix.clone(), r.clone()].concat(),
            );
            bindings.commit(rotate(2, &a, &p, BindingCause::Fork));
            p
        };
        polls(3).await;
        assert_eq!(harness.port.posts(), ["parent posted"]);
        assert!(harness.channel().ledger().unresolved().is_none());
        let parent_from = if root { 0 } else { prefix.len() as u64 };
        if root {
            assert!(harness.channel().rotation().unwrap().link(&p).is_none());
        } else {
            assert_eq!(boundary(&harness, &p), Boundary::Owed { from: parent_from });
        }
        halt(stop, task).await;
        let inherited = prompt("prefix", "short copy");
        let fresh = prompt("fresh", "child starts");
        let child_start = (inherited.len() + fresh.len()) as u64;
        if !root {
            let parent_start = prefix.len() as u64;
            assert!(child_start < parent_from && parent_from <= parent_start);
            assert_eq!(parent_from, parent_start);
        }
        let copied = if root {
            [r.clone(), adopted.clone()].concat()
        } else {
            r.clone()
        };
        let control = row("control", "child control");
        let child_body = [inherited.clone(), fresh, copied.clone(), control.clone()].concat();
        let (_, b) = transcript(&a_path, "b.jsonl", "sb", &child_body);
        bindings.commit(rotate(if root { 2 } else { 3 }, &p, &b, BindingCause::Fork));
        for _ in 0..2 {
            let owed = observe(&harness, bindings.clone());
            assert!(
                !owed.saw(&b, child_start, child_start + r.len() as u64),
                "posted B.r must be masked"
            );
            if root {
                let start = child_start + r.len() as u64;
                assert!(
                    !owed.saw(&b, start, start + adopted.len() as u64),
                    "adopted history follows root policy"
                );
            }
            let start = child_start + copied.len() as u64;
            assert!(
                owed.saw(&b, start, start + control.len() as u64),
                "capture/replay reached the control"
            );
        }
        (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        polls(3).await;
        assert_eq!(harness.port.posts(), ["parent posted", "child control"]);
        halt(stop, task).await;
        (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
        polls(3).await;
        assert_eq!(
            harness.port.posts(),
            ["parent posted", "child control"],
            "restart adds no POST"
        );
        halt(stop, task).await;
    }
}

#[tokio::test(start_paused = true)]
async fn warm_parent_cache_is_discarded_after_replacement_truncation_or_removal() {
    for damage in ["replace", "truncate", "remove"] {
        let prefix = prompt("prefix", "adopted");
        let r = row("r", "shared");
        let (harness, a_path, a, bindings) = started(&[prefix.clone(), r.clone()].concat());
        let fresh = prompt("fresh", "new");
        let (b_path, b) = transcript(&a_path, "b.jsonl", "sb", &[prefix, fresh.clone()].concat());
        let mut store = harness.channel();
        store.set_retired(&a, true).unwrap();
        drop(store);
        let mut writer = harness.writer();
        let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings.clone());
        let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
        let mut owed = RecordingOwe::default();
        sources
            .resume(&mut writer, &mut deriver, &mut owed)
            .unwrap();
        bindings.commit(rotate(2, &a, &b, BindingCause::Fork));
        sources.follow(&mut writer).unwrap();
        sources
            .capture(&mut writer, &mut deriver, &mut owed)
            .unwrap();
        assert!(matches!(boundary(&harness, &b), Boundary::Owed { .. }));
        let start = std::fs::metadata(&b_path).unwrap().len();
        append(&b_path, &r);
        sources
            .capture(&mut writer, &mut deriver, &mut owed)
            .unwrap();
        assert!(
            !owed.saw(&b, start, start + r.len() as u64),
            "warm cache masks first copy"
        );
        match damage {
            "replace" => {
                let swap = a_path.with_extension("swap");
                std::fs::write(&swap, std::fs::read(&a_path).unwrap()).unwrap();
                std::fs::rename(swap, &a_path).unwrap();
            }
            "truncate" => std::fs::OpenOptions::new()
                .write(true)
                .open(&a_path)
                .unwrap()
                .set_len(0)
                .unwrap(),
            "remove" => std::fs::remove_file(&a_path).unwrap(),
            _ => unreachable!(),
        }
        let control = prompt("control", "capture ran");
        let start = std::fs::metadata(&b_path).unwrap().len();
        append(&b_path, &[r.clone(), control.clone()].concat());
        sources
            .capture(&mut writer, &mut deriver, &mut owed)
            .unwrap();
        assert!(
            owed.saw(&b, start, start + r.len() as u64),
            "{damage}: no stale mask"
        );
        assert!(owed.saw(
            &b,
            start + r.len() as u64,
            start + r.len() as u64 + control.len() as u64
        ));
    }
}

#[tokio::test(start_paused = true)]
async fn changed_prefix_halts_a_retired_parent_even_when_lineage_can_still_mask() {
    let original = row("r", "original");
    let changed = row("r", "tampered");
    assert_eq!(original.len(), changed.len());
    let (harness, a_path, a, bindings) = started(&original);
    let child_body = [original.clone(), prompt("fresh", "new"), original.clone()].concat();
    let (_, b) = transcript(&a_path, "b.jsonl", "sb", &child_body);
    bindings.commit(rotate(2, &a, &b, BindingCause::Fork));
    observe(&harness, bindings.clone());
    harness.channel().set_retired(&a, true).unwrap();
    let mut writer = harness.writer();
    let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings.clone());
    let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    let mut owed = RecordingOwe::default();
    sources
        .resume(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    std::fs::write(&a_path, changed).unwrap();
    let start = std::fs::metadata(&b.path).unwrap().len();
    let control = prompt("control", "capture ran");
    append(&b.path, &[original.clone(), control.clone()].concat());
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    assert!(
        !owed.saw(&b, start, start + original.len() as u64),
        "lineage checks identity/length, not prefix hash"
    );
    assert!(owed.saw(
        &b,
        start + original.len() as u64,
        start + original.len() as u64 + control.len() as u64
    ));
    drop(writer);
    let (_stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
    polls(3).await;
    assert!(
        task.is_finished(),
        "hash mismatch must stop before delivery"
    );
    let cause = task.await.unwrap().unwrap();
    assert!(
        format!("{cause:?}").contains("source bytes before the cursor changed"),
        "{cause:?}"
    );
    assert!(harness.port.posts().is_empty());
}

#[tokio::test(start_paused = true)]
async fn baseline_v2_disk_fixture_reconstructs_parent_positions_without_new_fields() {
    const PARENT: &[u8] =
        include_bytes!("../../../../tests/fixtures/tui_o/fork_mask_v2/parent.jsonl");
    const CHILD: &[u8] =
        include_bytes!("../../../../tests/fixtures/tui_o/fork_mask_v2/child.jsonl");
    const PC: &str = include_str!("../../../../tests/fixtures/tui_o/fork_mask_v2/cursor.json");
    const BOUNDARY: &str =
        include_str!("../../../../tests/fixtures/tui_o/fork_mask_v2/boundary.json");
    const PS: &str = include_str!("../../../../tests/fixtures/tui_o/fork_mask_v2/parent.segment");
    const BS: &str = include_str!("../../../../tests/fixtures/tui_o/fork_mask_v2/child.segment");
    let parent_start = PARENT.iter().position(|b| *b == b'\n').unwrap() as u64 + 1;
    let child_start = CHILD
        .split_inclusive(|b| *b == b'\n')
        .take(2)
        .map(|l| l.len() as u64)
        .sum::<u64>();
    for from in [parent_start, PARENT.len() as u64] {
        let (harness, a_path, a, bindings) = started(b"");
        let (_, p) = transcript(&a_path, "p.jsonl", "sp", PARENT);
        let (_, b) = transcript(&a_path, "b.jsonl", "sb", CHILD);
        bindings.commit(rotate(2, &a, &p, BindingCause::Fork));
        bindings.commit(rotate(3, &p, &b, BindingCause::Fork));
        drop(harness.channel());
        let dir = a_path
            .parent()
            .unwrap()
            .join("o_store")
            .join(CHANNEL.to_string());
        let source = |s: &SourceId| serde_json::to_string(s).unwrap();
        let boundary = BOUNDARY
            .replace("@PKEY@", &source_key(&p))
            .replace("@BKEY@", &source_key(&b))
            .replace("@AKEY@", &source_key(&a))
            .replace("@P@", &source(&p))
            .replace("@B@", &source(&b))
            .replace("@A@", &source(&a))
            .replace("@FROM@", &from.to_string())
            .replace("@CHILD_FROM@", &parent_start.to_string());
        std::fs::write(dir.join("boundary"), boundary).unwrap();
        std::fs::write(dir.join("binding_checkpoint"), b"{\"channel\":7,\"seq\":3}").unwrap();
        for (s, body, segment) in [(&p, PARENT, PS), (&b, CHILD, BS)] {
            let cursor = PC
                .replace("@SOURCE@", &source(s))
                .replace("@THROUGH@", &body.len().to_string())
                .replace("@HASH@", &hex::encode(Sha256::digest(body)));
            std::fs::write(
                dir.join("cursor").join(format!("{}.json", source_key(s))),
                cursor,
            )
            .unwrap();
            let spool_dir = dir.join("spool");
            let segment = segment.replace("@SOURCE@", &source(s));
            assert!(segment.contains("\"identity_version\":2"));
            std::fs::write(
                spool_dir.join(format!("{}-00000000000000000000.seg", source_key(s))),
                segment,
            )
            .unwrap();
        }
        let store = harness.channel();
        let rotation = store.rotation().unwrap();
        let old_hop = rotation.successors.get(&source_key(&p)).unwrap();
        assert_eq!(
            (
                old_hop.seq,
                old_hop.tmux_session.as_ref(),
                old_hop.drain_to,
                old_hop.proof
            ),
            (None, None, None, None)
        );
        assert_eq!(
            store.cursor(&p).unwrap().captured_through,
            PARENT.len() as u64
        );
        drop(store);
        let owed = observe(&harness, bindings.clone());
        let end = CHILD.len() as u64;
        assert_eq!(owed.saw(&b, child_start, end), from > parent_start);
        assert!(
            owed.saw(&b, parent_start, child_start),
            "nonposting new prompt is a replay control"
        );
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
        polls(3).await;
        assert_eq!(harness.port.posts(), ["fixture shared"]);
        halt(stop, task).await;
    }
}

#[tokio::test(start_paused = true)]
async fn excluded_parent_consumed_prefix_still_decides_a_new_fork() {
    let (harness, a_path, a, bindings) = started(&prompt("a", "adopted"));
    let prefix = prompt("z", "unrelated");
    let r = row("r", "excluded");
    let body = [prefix.clone(), r.clone()].concat();
    let (p_path, p) = transcript(&a_path, "p.jsonl", "sp", &body);
    bindings.commit(rotate(2, &a, &p, BindingCause::Fork));
    observe(&harness, bindings.clone());
    assert!(matches!(boundary(&harness, &p), Boundary::Pending { .. }));
    let from = body.len() as u64;
    harness
        .store
        .record_boundary_resolved(
            CHANNEL,
            &p_path.display().to_string(),
            &ResolveFrom::Offset(from),
            "op",
        )
        .unwrap();
    let fresh = prompt("fresh", "new");
    let (_, b) = transcript(
        &a_path,
        "b.jsonl",
        "sb",
        &[prefix, r, fresh.clone()].concat(),
    );
    bindings.commit(rotate(3, &p, &b, BindingCause::Fork));
    let owed = observe(&harness, bindings);
    assert_eq!(boundary(&harness, &p), Boundary::Owed { from });
    assert_eq!(boundary(&harness, &b), Boundary::Owed { from });
    assert!(
        owed.saw(&b, from, from + fresh.len() as u64),
        "consumed prefix was preserved"
    );
}

#[tokio::test(start_paused = true)]
async fn parent_spool_gc_preserves_reconstructed_mask_evidence() {
    let prefix = prompt("prefix", "adopted");
    let (harness, a_path, a, bindings) = started(&prefix);
    let r = row("r", "posted once");
    append(&a_path, &r);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(3).await;
    assert_eq!(harness.port.posts(), ["posted once"]);
    halt(stop, task).await;
    let mut store = harness.channel();
    store.gc_oldest_segment(&a).unwrap();
    assert!(
        !store.ledger().gc_segments(&a).is_empty(),
        "actual durable GC record"
    );
    let mut frames = 0;
    store.for_each_frame(&a, |_| frames += 1).unwrap();
    assert_eq!(frames, 0);
    drop(store);
    let fresh = prompt("fresh", "new");
    let control = prompt("control", "capture ran");
    let start = (prefix.len() + fresh.len()) as u64;
    let (_, b) = transcript(
        &a_path,
        "b.jsonl",
        "sb",
        &[prefix, fresh, r.clone(), control.clone()].concat(),
    );
    bindings.commit(rotate(2, &a, &b, BindingCause::Fork));
    let owed = observe(&harness, bindings);
    assert!(!owed.saw(&b, start, start + r.len() as u64));
    assert!(owed.saw(
        &b,
        start + r.len() as u64,
        start + r.len() as u64 + control.len() as u64
    ));
}

#[tokio::test(start_paused = true)]
async fn repeated_parent_identity_after_owed_start_masks_the_child_copy() {
    let inherited = row("r", "shared");
    let (harness, a_path, a, bindings) = started(&inherited);
    let parent_new = prompt("parent-new", "new");
    let parent_from = inherited.len() as u64;
    let (_, p) = transcript(
        &a_path,
        "p.jsonl",
        "sp",
        &[inherited.clone(), parent_new, inherited.clone()].concat(),
    );
    bindings.commit(rotate(2, &a, &p, BindingCause::Fork));
    observe(&harness, bindings.clone());
    assert_eq!(boundary(&harness, &p), Boundary::Owed { from: parent_from });
    let fresh = prompt("child-new", "new");
    let control = prompt("control", "capture ran");
    let start = (inherited.len() + fresh.len()) as u64;
    let (_, b) = transcript(
        &a_path,
        "b.jsonl",
        "sb",
        &[inherited.clone(), fresh, inherited.clone(), control.clone()].concat(),
    );
    bindings.commit(rotate(3, &p, &b, BindingCause::Fork));
    let owed = observe(&harness, bindings);
    assert!(!owed.saw(&b, start, start + inherited.len() as u64));
    assert!(owed.saw(
        &b,
        start + inherited.len() as u64,
        start + inherited.len() as u64 + control.len() as u64
    ));
}
