use serde_json::json;

use super::*;
use crate::services::tui_o::shadow::IDENTITY_VERSION;
use crate::services::tui_o::store::spool::source_key;
use crate::services::tui_o::writer::rotation::{MAX_READERS, RETIRE_QUIET, Sources};

fn fixture(count: usize) -> (Harness, Vec<SourceId>, Arc<FakeBindings>) {
    let mut sources = Vec::new();
    let harness = Harness::build(|root| {
        (0..count)
            .map(|i| {
                let path = root.join(format!("{i}.jsonl"));
                std::fs::write(&path, b"").unwrap();
                let source_id = source_id_for(&format!("s{i}"), &path).unwrap();
                sources.push(source_id.clone());
                InitSource {
                    source_id,
                    delivery_start: 0,
                    prefix_hash: hex::encode(sha2::Sha256::digest(b"")),
                }
            })
            .collect()
    });
    harness.gate.acquired();
    let bindings = Arc::new(FakeBindings::new());
    bindings.commit(bound(
        1,
        None,
        BindingTarget::Source(sources[0].clone()),
        BindingCause::Startup,
        None,
    ));
    (harness, sources, bindings)
}

fn resume(
    harness: &Harness,
    bindings: Arc<FakeBindings>,
) -> (
    Sources<FakeBindings>,
    Writer,
    UnitDeriver,
    VecDeque<Derived>,
) {
    let mut writer = harness.writer();
    let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings);
    let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    let mut owed = VecDeque::new();
    sources
        .resume(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    sources.follow(&mut writer).unwrap();
    (sources, writer, deriver, owed)
}

fn checkpoint(harness: &Harness, seq: u64) {
    harness.channel().set_binding_checkpoint(seq).unwrap();
}

fn hop(seq: u64, a: &SourceId, b: &SourceId) -> BindingEvent {
    bound(
        seq,
        Some(a),
        BindingTarget::Source(b.clone()),
        BindingCause::Startup,
        None,
    )
}

#[test]
fn h1_adoption_history_restores_relations_without_native_proof_or_boundary_changes() {
    for seeded in [false, true] {
        let (harness, ids, bindings) = fixture(2);
        append(&ids[0].path, &row("a", "한글 old tail"));
        bindings.commit(hop(2, &ids[0], &ids[1]));
        if seeded {
            checkpoint(&harness, 2);
        }
        let cursors: Vec<_> = harness.channel().cursors().cloned().collect();
        let init_before = harness.channel().init().clone();
        let canonical_before = bindings.events.lock().unwrap().clone();
        let (mut sources, mut writer, _, _) = resume(&harness, bindings.clone());
        assert_eq!(writer.store().init(), &init_before);
        assert_eq!(*bindings.events.lock().unwrap(), canonical_before);
        let rotation = writer.store().rotation().unwrap();
        let next = &rotation.successors[&source_key(&ids[0])];
        assert_eq!(
            (next.seq, next.tmux_session.as_deref(), next.proof),
            (Some(2), Some("tmux"), None)
        );
        assert_eq!(
            next.drain_to,
            Some(std::fs::metadata(&ids[0].path).unwrap().len())
        );
        assert!(rotation.links.is_empty());
        assert_eq!(
            writer.store().cursors().cloned().collect::<Vec<_>>(),
            cursors
        );
        sources.tend(&mut writer).unwrap();
        assert!(!writer.store().cursor(&ids[0]).unwrap().retired);
    }
    restoration_precedes_retained_spool_ordering();
}

fn restoration_precedes_retained_spool_ordering() {
    let (harness, mut ids, bindings) = fixture(2);
    ids.sort_by_key(source_key);
    let (new, old) = (&ids[0], &ids[1]);
    append(&old.path, &row("old", "old retained"));
    append(&new.path, &row("new", "new retained"));
    checkpoint(&harness, 1);
    let (mut sources, mut writer, mut deriver, mut owed) = resume(&harness, bindings.clone());
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    bindings.commit(hop(2, old, new));
    checkpoint(&harness, 2);
    drop(writer);
    let (_, _, _, owed) = resume(&harness, bindings);
    let payloads: Vec<_> = owed
        .iter()
        .filter_map(|item| match item {
            Derived::Piece(piece) => Some(piece.payload.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        payloads,
        ["old retained", "new retained"],
        "the restored relation must order retained replay before opening readers"
    );
}

#[test]
fn h2_rebind_removes_new_successor_including_oldless_binds() {
    for oldless in [false, true] {
        let (harness, ids, bindings) = fixture(3);
        bindings.commit(hop(2, &ids[0], &ids[1]));
        bindings.commit(hop(3, &ids[1], &ids[2]));
        bindings.commit(bound(
            4,
            (!oldless).then_some(&ids[2]),
            BindingTarget::Source(ids[1].clone()),
            BindingCause::Startup,
            None,
        ));
        checkpoint(&harness, 4);
        let (_, mut writer, _, _) = resume(&harness, bindings);
        let rotation = writer.store().rotation().unwrap();
        assert!(!rotation.successors.contains_key(&source_key(&ids[1])));
        assert_eq!(rotation.successors[&source_key(&ids[0])].source, ids[1]);
        assert_eq!(rotation.successors.len(), if oldless { 1 } else { 2 });
    }
}

#[test]
fn h3_future_resolution_and_future_bind_do_not_restore_an_applied_hop() {
    let (harness, ids, bindings) = fixture(3);
    bindings.commit(bound(
        2,
        Some(&ids[0]),
        BindingTarget::Pending {
            payload_session_id: ids[1].session_id.clone(),
            payload_transcript_path: ids[1].path.clone(),
        },
        BindingCause::Clear,
        None,
    ));
    bindings.commit(event(
        3,
        BindingRecord::Rejected {
            detail: "audit only".into(),
        },
        Utc::now(),
    ));
    bindings.commit(event(
        4,
        BindingRecord::Resolved {
            resolves_seq: 2,
            source: ids[1].clone(),
        },
        Utc::now(),
    ));
    bindings.commit(hop(5, &ids[1], &ids[2]));
    checkpoint(&harness, 3);
    let mut writer = harness.writer();
    let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings.clone());
    sources
        .resume(
            &mut writer,
            &mut UnitDeriver::new(CHANNEL, ShadowProvider::Claude),
            &mut VecDeque::new(),
        )
        .unwrap();
    assert!(writer.store().rotation().unwrap().successors.is_empty());
    checkpoint(&harness, 4);
    let (_, mut writer, _, _) = resume(
        &harness,
        Arc::new(FakeBindings {
            events: Mutex::new(bindings.events.lock().unwrap()[..4].to_vec()),
            notice: tokio::sync::watch::channel(4).0,
            failing: Mutex::new(None),
        }),
    );
    assert_eq!(
        writer.store().rotation().unwrap().successors[&source_key(&ids[0])].source,
        ids[1]
    );
}

#[tokio::test]
async fn h4_retained_prepared_payload_identity_and_posted_delivery_survive_restoration() {
    let (harness, ids, bindings) = fixture(2);
    append(
        &ids[0].path,
        &row("a", "보존할 긴 본문".repeat(400).as_str()),
    );
    append(&ids[1].path, &row("b", "새 출력"));
    checkpoint(&harness, 1);
    let (mut sources, mut writer, mut deriver, mut owed) = resume(&harness, bindings.clone());
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    let expected = owed.clone();
    assert!(expected.len() >= 3, "exercise split text plus a successor");
    assert_eq!(writer.deliver(&owed.pop_front().unwrap()).await, Step::Done);
    let Derived::Piece(open) = owed.front().unwrap() else {
        panic!("expected piece")
    };
    let serial = writer.store().ledger().next_serial();
    let anchor_id = writer.store().ledger().anchor();
    writer
        .store()
        .append_ledger(LedgerEntry::Prepared {
            serial,
            unit_key: open.unit_key.clone(),
            piece_index: open.index,
            payload: open.payload.clone(),
            anchor_id,
            epoch: 1,
        })
        .unwrap();
    let posted = harness.port.say(BOT, &open.payload).id;
    let ledger_path = harness
        ._runtime
        .path()
        .join("o_store")
        .join(CHANNEL.to_string())
        .join("ledger.jsonl");
    let before = std::fs::read(&ledger_path).unwrap();
    let posts = harness.port.posts();
    bindings.commit(hop(2, &ids[0], &ids[1]));
    checkpoint(&harness, 2);
    drop(writer);
    let (_, mut writer, _, mut owed) = resume(&harness, bindings);
    let pieces = |queue: &VecDeque<Derived>| {
        let mut rows: Vec<_> = queue.iter().map(|item| format!("{item:?}")).collect();
        rows.sort();
        rows
    };
    assert_eq!(
        pieces(&owed),
        pieces(&expected),
        "restored replay preserves every split identity and payload"
    );
    assert_eq!(std::fs::read(&ledger_path).unwrap(), before);
    while let Some(item) = owed.pop_front() {
        assert_eq!(writer.deliver(&item).await, Step::Done);
    }
    let after = std::fs::read(ledger_path).unwrap();
    assert!(
        after.starts_with(&before),
        "existing Prepared/Posted bytes never change"
    );
    assert_eq!(
        writer.store().ledger().piece(serial).unwrap().outcome,
        Some(PieceOutcome::Posted(posted))
    );
    let expected_posts: Vec<_> = expected
        .iter()
        .skip(2)
        .filter_map(|item| match item {
            Derived::Piece(piece) => Some(piece.payload.clone()),
            _ => None,
        })
        .collect();
    let mut actual = harness.port.posts();
    let mut wanted = [posts, expected_posts].concat();
    actual.sort();
    wanted.sort();
    assert_eq!(actual, wanted, "Posted and open Prepared must not repost");
}

#[test]
fn h5_unattached_middle_source_is_not_attached_or_compressed() {
    let (harness, ids, bindings) = fixture(2);
    let (_, middle) = transcript(&ids[0].path, "middle.jsonl", "middle", b"");
    bindings.commit(hop(2, &ids[0], &middle));
    bindings.commit(hop(3, &middle, &ids[1]));
    checkpoint(&harness, 3);
    let (_, mut writer, _, _) = resume(&harness, bindings);
    assert!(writer.store().rotation().unwrap().successors.is_empty());
    assert!(writer.store().cursor(&middle).is_none());
    assert!(!writer.store().cursor(&ids[0]).unwrap().retired);
}

#[test]
fn h6_failed_rotation_write_aborts_resume_and_restart_keeps_cursor_hash_and_frontier() {
    use std::os::unix::fs::PermissionsExt;
    let (harness, ids, bindings) = fixture(2);
    bindings.commit(hop(2, &ids[0], &ids[1]));
    checkpoint(&harness, 2);
    let dir = harness
        ._runtime
        .path()
        .join("o_store")
        .join(CHANNEL.to_string());
    let cursors: Vec<_> = harness.channel().cursors().cloned().collect();
    let mut writer = harness.writer();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings.clone());
    let result = sources.resume(
        &mut writer,
        &mut UnitDeriver::new(CHANNEL, ShadowProvider::Claude),
        &mut VecDeque::new(),
    );
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        matches!(result, Err(WriterAlarm::Halted { .. })),
        "rotation write must fail resume: {result:?}"
    );
    drop(writer);
    let (_, mut writer, _, _) = resume(&harness, bindings.clone());
    let mut rotation = writer.store().rotation().unwrap();
    let key = source_key(&ids[0]);
    rotation.successors.get_mut(&key).unwrap().proof = Some(12);
    writer.store().write_rotation(&rotation).unwrap();
    append(&ids[0].path, &row("late", "late growth"));
    drop(writer);
    let (_, mut writer, _, _) = resume(&harness, bindings);
    assert_eq!(
        writer.store().rotation().unwrap(),
        rotation,
        "saved drain/proof must not be recalculated"
    );
    assert_eq!(
        writer.store().cursors().cloned().collect::<Vec<_>>(),
        cursors
    );
}

#[derive(Clone)]
struct HealthSink {
    router: Arc<crate::services::tui_o::alarm::AlarmRouter>,
    reconciled: Arc<Mutex<Vec<usize>>>,
}
impl AlarmSink for HealthSink {
    fn raise(&self, channel: u64, alarm: WriterAlarm) {
        self.router.raise(channel, alarm);
    }
    fn reconcile_reader_count(&self, channel: u64, count: usize) {
        self.reconciled.lock().unwrap().push(count);
        self.router.reconcile_reader_count(channel, count);
    }
}

fn health_writer(
    harness: &Harness,
) -> (
    ChannelWriter<FakePort, Arc<FakeLease>, HealthSink>,
    Arc<crate::services::tui_o::alarm::AlarmHealth>,
    Arc<Mutex<Vec<usize>>>,
) {
    let health = Arc::new(crate::services::tui_o::alarm::AlarmHealth::default());
    let router = Arc::new(crate::services::tui_o::alarm::AlarmRouter::new(
        None,
        None,
        health.clone(),
    ));
    let reconciled = Arc::new(Mutex::new(Vec::new()));
    let sink = HealthSink {
        router,
        reconciled: reconciled.clone(),
    };
    (
        ChannelWriter::new(
            harness.channel(),
            harness.gate.clone(),
            harness.port.clone(),
            harness.lease.clone(),
            sink,
        ),
        health,
        reconciled,
    )
}

fn active(health: &crate::services::tui_o::alarm::AlarmHealth) -> Vec<String> {
    health.current_at(std::time::Instant::now())
}

#[tokio::test(start_paused = true)]
async fn p2_1_successful_retirement_clears_reader_health_in_the_same_tend() {
    let (harness, ids, bindings) = fixture(MAX_READERS + 1);
    for (i, source) in ids.iter().enumerate() {
        append(&source.path, &row(&format!("m{i}"), "body"));
    }
    let (mut writer, health, counts) = health_writer(&harness);
    let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings.clone());
    let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    sources
        .resume(&mut writer, &mut deriver, &mut VecDeque::new())
        .unwrap();
    sources.follow(&mut writer).unwrap();
    sources
        .capture(&mut writer, &mut deriver, &mut VecDeque::new())
        .unwrap();
    sources.tend(&mut writer).unwrap();
    assert!(active(&health).contains(&format!("tui_o:too_many_readers:{CHANNEL}")));
    bindings.commit(bound(
        2,
        Some(&ids[0]),
        BindingTarget::Source(ids[1].clone()),
        BindingCause::Clear,
        None,
    ));
    sources.follow(&mut writer).unwrap();
    sources.tend(&mut writer).unwrap();
    tokio::time::advance(RETIRE_QUIET).await;
    sources.tend(&mut writer).unwrap();
    assert_eq!(counts.lock().unwrap().last(), Some(&MAX_READERS));
    assert!(!active(&health).contains(&format!("tui_o:too_many_readers:{CHANNEL}")));
    assert!(writer.store().cursor(&ids[0]).unwrap().retired);
}

#[tokio::test(start_paused = true)]
async fn p2_3_regrowth_reactivates_reader_health_after_successful_recovery() {
    let (harness, ids, bindings) = fixture(MAX_READERS + 1);
    for (i, source) in ids.iter().enumerate() {
        append(&source.path, &row(&format!("m{i}"), "body"));
    }
    let (mut writer, health, _) = health_writer(&harness);
    let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings.clone());
    let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    sources
        .resume(&mut writer, &mut deriver, &mut VecDeque::new())
        .unwrap();
    sources.follow(&mut writer).unwrap();
    sources
        .capture(&mut writer, &mut deriver, &mut VecDeque::new())
        .unwrap();
    sources.tend(&mut writer).unwrap();
    assert!(active(&health).contains(&format!("tui_o:too_many_readers:{CHANNEL}")));
    bindings.commit(bound(
        2,
        Some(&ids[0]),
        BindingTarget::Source(ids[1].clone()),
        BindingCause::Clear,
        None,
    ));
    sources.follow(&mut writer).unwrap();
    sources.tend(&mut writer).unwrap();
    tokio::time::advance(RETIRE_QUIET).await;
    sources.tend(&mut writer).unwrap();
    assert!(
        !active(&health)
            .iter()
            .any(|s| s.contains("too_many_readers"))
    );
    append(&ids[0].path, &row("late", "reopened"));
    sources.tend(&mut writer).unwrap();
    assert!(active(&health).contains(&format!("tui_o:too_many_readers:{CHANNEL}")));
    assert!(!writer.store().cursor(&ids[0]).unwrap().retired);
}

#[tokio::test(start_paused = true)]
async fn p2_4_failed_tend_keeps_prior_health_evidence_until_a_successful_pass() {
    let (harness, mut ids, bindings) = fixture(MAX_READERS + 2);
    ids.sort_by_key(source_key);
    for (i, source) in ids.iter().enumerate() {
        append(&source.path, &row(&format!("m{i}"), "body"));
    }
    let (mut writer, health, counts) = health_writer(&harness);
    let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings.clone());
    let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Claude);
    sources
        .resume(&mut writer, &mut deriver, &mut VecDeque::new())
        .unwrap();
    sources.follow(&mut writer).unwrap();
    sources
        .capture(&mut writer, &mut deriver, &mut VecDeque::new())
        .unwrap();
    bindings.commit(bound(
        2,
        Some(&ids[0]),
        BindingTarget::Source(ids[1].clone()),
        BindingCause::Clear,
        None,
    ));
    sources.follow(&mut writer).unwrap();
    sources.tend(&mut writer).unwrap();
    bindings.commit(bound(
        3,
        Some(&ids[1]),
        BindingTarget::Source(ids[2].clone()),
        BindingCause::Clear,
        None,
    ));
    sources.follow(&mut writer).unwrap();
    sources.tend(&mut writer).unwrap();
    let old_counts = counts.lock().unwrap().clone();
    tokio::time::advance(RETIRE_QUIET).await;
    let cursor = harness
        ._runtime
        .path()
        .join("o_store")
        .join(CHANNEL.to_string())
        .join("cursor")
        .join(format!("{}.json", source_key(&ids[1])));
    let bytes = std::fs::read(&cursor).unwrap_or_default();
    if cursor.exists() {
        std::fs::remove_file(&cursor).unwrap();
    }
    std::fs::create_dir(&cursor).unwrap();
    let result = sources.tend(&mut writer);
    std::fs::remove_dir(&cursor).unwrap();
    if !bytes.is_empty() {
        std::fs::write(&cursor, bytes).unwrap();
    }
    assert!(result.is_err());
    assert!(
        writer.store().cursor(&ids[0]).unwrap().retired,
        "first retirement succeeded before the second failed"
    );
    assert_eq!(
        *counts.lock().unwrap(),
        old_counts,
        "failed tend must not publish a count"
    );
    assert!(active(&health).contains(&format!("tui_o:too_many_readers:{CHANNEL}")));
    drop(writer);
    let sink = HealthSink {
        router: Arc::new(crate::services::tui_o::alarm::AlarmRouter::new(
            None,
            None,
            health.clone(),
        )),
        reconciled: counts.clone(),
    };
    let mut writer = ChannelWriter::new(
        harness.channel(),
        harness.gate.clone(),
        harness.port.clone(),
        harness.lease.clone(),
        sink,
    );
    let mut reopened = Sources::new(CHANNEL, ShadowProvider::Claude, bindings);
    reopened
        .resume(&mut writer, &mut deriver, &mut VecDeque::new())
        .unwrap();
    reopened.tend(&mut writer).unwrap();
    tokio::time::advance(RETIRE_QUIET).await;
    reopened.tend(&mut writer).unwrap();
    assert_eq!(counts.lock().unwrap().last(), Some(&MAX_READERS));
    assert!(!active(&health).contains(&format!("tui_o:too_many_readers:{CHANNEL}")));

    failed_regrowth_defers_overcount_until_success();
}

fn failed_regrowth_defers_overcount_until_success() {
    let (harness, ids, bindings) = fixture(MAX_READERS + 1);
    for (i, source) in ids.iter().enumerate() {
        append(&source.path, &row(&format!("m{i}"), "body"));
    }
    let (mut sources, mut writer, mut deriver, _) = resume(&harness, bindings.clone());
    sources
        .capture(&mut writer, &mut deriver, &mut VecDeque::new())
        .unwrap();
    writer.store().set_retired(&ids[0], true).unwrap();
    drop(writer);
    let (mut writer, health, counts) = health_writer(&harness);
    let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings.clone());
    sources
        .resume(&mut writer, &mut deriver, &mut VecDeque::new())
        .unwrap();
    sources.tend(&mut writer).unwrap();
    assert_eq!(counts.lock().unwrap().last(), Some(&MAX_READERS));
    append(&ids[0].path, &row("late", "regrowth"));
    let cursor = harness
        ._runtime
        .path()
        .join("o_store")
        .join(CHANNEL.to_string())
        .join("cursor")
        .join(format!("{}.json", source_key(&ids[0])));
    let bytes = std::fs::read(&cursor).unwrap();
    std::fs::remove_file(&cursor).unwrap();
    std::fs::create_dir(&cursor).unwrap();
    let result = sources.tend(&mut writer);
    std::fs::remove_dir(&cursor).unwrap();
    std::fs::write(&cursor, bytes).unwrap();
    assert!(result.is_err());
    assert_eq!(counts.lock().unwrap().as_slice(), &[MAX_READERS]);
    assert!(
        !active(&health).contains(&format!("tui_o:too_many_readers:{CHANNEL}")),
        "failed regrowth publishes neither recovery nor a new overcount activation"
    );
    drop(writer);
    let sink = HealthSink {
        router: Arc::new(crate::services::tui_o::alarm::AlarmRouter::new(
            None,
            None,
            health.clone(),
        )),
        reconciled: counts.clone(),
    };
    let mut writer = ChannelWriter::new(
        harness.channel(),
        harness.gate.clone(),
        harness.port.clone(),
        harness.lease.clone(),
        sink,
    );
    let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, bindings);
    sources
        .resume(&mut writer, &mut deriver, &mut VecDeque::new())
        .unwrap();
    sources.tend(&mut writer).unwrap();
    assert_eq!(counts.lock().unwrap().last(), Some(&(MAX_READERS + 1)));
    assert!(
        active(&health).contains(&format!("tui_o:too_many_readers:{CHANNEL}")),
        "new overcount activates on the next successful tend"
    );
}

fn codex(value: serde_json::Value) -> Vec<u8> {
    let mut line = serde_json::to_vec(&value).unwrap();
    line.push(b'\n');
    line
}

fn body(id: &str, text: &str) -> Vec<u8> {
    codex(json!({"type": "response_item", "payload": {
        "type": "message", "role": "assistant", "id": id,
        "content": [{"type": "output_text", "text": text}]}}))
}

fn turn_event(kind: &str, turn_id: &str) -> Vec<u8> {
    codex(json!({"type": "event_msg", "payload": {"type": kind, "turn_id": turn_id}}))
}

fn resume_codex(
    harness: &Harness,
    bindings: Arc<FakeBindings>,
) -> (
    Sources<FakeBindings>,
    Writer,
    UnitDeriver,
    VecDeque<Derived>,
) {
    let mut writer = harness.writer();
    let mut sources = Sources::new(CHANNEL, ShadowProvider::Codex, bindings);
    let mut deriver = UnitDeriver::new(CHANNEL, ShadowProvider::Codex);
    let mut owed = VecDeque::new();
    sources
        .resume(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    sources.follow(&mut writer).unwrap();
    (sources, writer, deriver, owed)
}

fn segments(harness: &Harness) -> Vec<PathBuf> {
    let dir = harness._runtime.path().join("o_store");
    let dir = dir.join(CHANNEL.to_string()).join("spool");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "seg"))
        .collect();
    paths.sort();
    paths
}

/// The segment header line's identity version, rewritten when `set` is given.
fn header_version(segment: &std::path::Path, set: Option<u32>) -> u64 {
    let bytes = std::fs::read(segment).unwrap();
    let split = bytes.iter().position(|byte| *byte == b'\n').unwrap();
    let mut header: serde_json::Value = serde_json::from_slice(&bytes[..split]).unwrap();
    if let Some(version) = set {
        header["identity_version"] = version.into();
        let mut rewritten = serde_json::to_vec(&header).unwrap();
        rewritten.extend_from_slice(&bytes[split..]);
        std::fs::write(segment, rewritten).unwrap();
    }
    header["identity_version"].as_u64().unwrap()
}

fn payloads(owed: &VecDeque<Derived>) -> Vec<String> {
    let pieces = owed.iter().filter_map(|item| match item {
        Derived::Piece(piece) => Some(piece.payload.clone()),
        _ => None,
    });
    pieces.collect()
}

#[tokio::test]
async fn a_v2_codex_spool_resumes_under_v3_without_reposting_a_delivered_piece() {
    let (harness, ids, bindings) = fixture(1);
    let long = "중단 전 긴 본문".repeat(400);
    let turn = [
        turn_event("task_started", "t1"),
        body("a1", &long),
        body("a2", "pending body"),
        turn_event("turn_aborted", "t1"),
    ];
    append(&ids[0].path, &turn.concat());
    let (mut sources, mut writer, mut deriver, mut owed) = resume_codex(&harness, bindings.clone());
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    let expected = payloads(&owed);
    assert!(
        expected.len() >= 3,
        "exercise a split body and a pending body"
    );
    assert_eq!(writer.deliver(&owed.pop_front().unwrap()).await, Step::Done);
    drop((sources, writer));
    let written = segments(&harness);
    for segment in &written {
        header_version(segment, Some(2));
    }
    let (mut sources, mut writer, mut deriver, mut owed) = resume_codex(&harness, bindings);
    assert_eq!(
        payloads(&owed),
        expected,
        "v2 frames derive the same pieces"
    );
    while let Some(item) = owed.pop_front() {
        assert_eq!(writer.deliver(&item).await, Step::Done);
    }
    assert_eq!(
        harness.port.posts(),
        expected,
        "each piece posts exactly once"
    );
    append(&ids[0].path, &turn_event("task_started", "t2"));
    append(&ids[0].path, &body("a3", "after the upgrade"));
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    while let Some(item) = owed.pop_front() {
        assert_eq!(writer.deliver(&item).await, Step::Done);
    }
    let posts = harness.port.posts();
    assert_eq!(posts[expected.len()..], ["after the upgrade"]);
    let rolled = segments(&harness);
    assert_eq!(rolled[..written.len()], written[..]);
    assert_eq!(rolled.len(), written.len() + 1);
    let current = header_version(rolled.last().unwrap(), None);
    assert_eq!(current, u64::from(IDENTITY_VERSION));
}
