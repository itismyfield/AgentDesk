use std::collections::VecDeque;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use super::gates_m1_support::{EmptyBindings, TestWriter, writer as make_writer};
use crate::services::tui_o::shadow::capture::{SourceCapture, file_identity};
use crate::services::tui_o::shadow::{
    CaptureOutcome, CaptureSource, ShadowProvider, SourceId, UnitKey,
};
use crate::services::tui_o::store::ledger::PieceOutcome;
use crate::services::tui_o::store::spool::source_key;
use crate::services::tui_o::store::{ChannelStore, HaltReason, OStore, StoreError, fault};
use crate::services::tui_o::writer::deliver::Step;
use crate::services::tui_o::writer::pieces::{Derived, PieceWork, UnitDeriver};
use crate::services::tui_o::writer::rotation::Sources;

const ORACLE: &str = "5806d1e6038040218ec48b51bf3105fe36992a23";

#[derive(Deserialize)]
struct ExpectedPiece {
    unit_key: UnitKey,
    index: u32,
    payload: String,
    posted: Option<u64>,
}

#[derive(Deserialize)]
struct ExpectedExclusion {
    unit_key: UnitKey,
    reason: String,
}

#[derive(Deserialize)]
struct Expected {
    provider: ShadowProvider,
    channel_id: u64,
    source: SourceId,
    captured_through: u64,
    prefix_hash: String,
    delivered_frontier: u64,
    pieces: Vec<ExpectedPiece>,
    excluded: Vec<ExpectedExclusion>,
}

struct Fixture {
    _root: tempfile::TempDir,
    runtime: PathBuf,
    source: SourceId,
    expected: Expected,
}

fn rebind(value: &mut Value, old: &Value, new: &Value) {
    if value == old {
        *value = new.clone();
    } else {
        match value {
            Value::Array(items) => items.iter_mut().for_each(|item| rebind(item, old, new)),
            Value::Object(items) => items.values_mut().for_each(|item| rebind(item, old, new)),
            _ => {}
        }
    }
}

fn copy_json(path: &Path, dest: &Path, old: &Value, new: &Value) {
    let mut value: Value = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    rebind(&mut value, old, new);
    fs::write(dest, serde_json::to_vec(&value).unwrap()).unwrap();
}

impl Fixture {
    fn load(name: &str) -> Self {
        // The pinned v2 capture/spool/UnitDeriver/Prepared/Posted APIs froze these bytes (fixture README).
        // Only source path/dev/ino and their filenames are rebound to the disposable directory.
        let base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/gates_m1_v2")
            .join(name);
        let expected: Expected =
            serde_json::from_slice(&fs::read(base.join("expected.json")).unwrap()).unwrap();
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("source.jsonl");
        fs::copy(base.join("source.jsonl"), &path).unwrap();
        let (dev, ino) = file_identity(&fs::metadata(&path).unwrap());
        let source = SourceId {
            path,
            dev,
            ino,
            ..expected.source.clone()
        };
        let old = serde_json::to_value(&expected.source).unwrap();
        let new = serde_json::to_value(&source).unwrap();
        let runtime = root.path().join("runtime");
        let dest = runtime.join("o_store");
        let channel = dest.join(expected.channel_id.to_string());
        fs::create_dir_all(channel.join("cursor")).unwrap();
        fs::create_dir_all(channel.join("spool")).unwrap();
        let original = base.join("runtime/o_store");
        copy_json(&original.join("o_era"), &dest.join("o_era"), &old, &new);
        let original = original.join(expected.channel_id.to_string());
        copy_json(&original.join("init"), &channel.join("init"), &old, &new);
        fs::copy(original.join("ledger.jsonl"), channel.join("ledger.jsonl")).unwrap();
        let key = source_key(&source);
        for cursor in fs::read_dir(original.join("cursor")).unwrap() {
            copy_json(
                &cursor.unwrap().path(),
                &channel.join("cursor").join(format!("{key}.json")),
                &old,
                &new,
            );
        }
        for segment in fs::read_dir(original.join("spool")).unwrap() {
            let path = segment.unwrap().path();
            let bytes = fs::read(&path).unwrap();
            let split = bytes.iter().position(|byte| *byte == b'\n').unwrap();
            let mut header: Value = serde_json::from_slice(&bytes[..split]).unwrap();
            assert_eq!(header["identity_version"], 2, "oracle must be native v2");
            rebind(&mut header, &old, &new);
            let start = header["start_offset"].as_u64().unwrap();
            let mut rebound = serde_json::to_vec(&header).unwrap();
            rebound.extend_from_slice(&bytes[split..]);
            fs::write(
                channel.join("spool").join(format!("{key}-{start:020}.seg")),
                rebound,
            )
            .unwrap();
        }
        Self {
            _root: root,
            runtime,
            source,
            expected,
        }
    }

    fn channel_dir(&self) -> PathBuf {
        self.runtime
            .join("o_store")
            .join(self.expected.channel_id.to_string())
    }

    fn open(&self) -> Result<ChannelStore, crate::services::tui_o::store::Halt> {
        let store = OStore::existing(&self.runtime).unwrap();
        let era = store.read_era().unwrap().unwrap();
        store
            .open_channel(&era, self.expected.channel_id)
            .map(Option::unwrap)
    }

    fn segments(&self) -> Vec<PathBuf> {
        let mut paths: Vec<_> = fs::read_dir(self.channel_dir().join("spool"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "seg"))
            .collect();
        paths.sort();
        paths
    }

    fn frontier(&self, store: &ChannelStore) {
        assert_eq!(store.init().build_digest, ORACLE);
        let cursor = store.cursor(&self.source).unwrap();
        assert_eq!(cursor.captured_through, self.expected.captured_through);
        assert_eq!(cursor.prefix_hash, self.expected.prefix_hash);
        assert_eq!(
            store.ledger().anchor(),
            self.expected.delivered_frontier,
            "v2 delivered frontier must survive recovery"
        );
        assert_eq!(store.ledger().violation(), None);
        for piece in &self.expected.pieces {
            let actual = store.ledger().latest_piece(&piece.unit_key, piece.index);
            match piece.posted {
                Some(id) => {
                    let (_, actual) = actual.expect("v2 delivered piece survives recovery");
                    assert_eq!(actual.payload, piece.payload);
                    assert_eq!(actual.outcome, Some(PieceOutcome::Posted(id)));
                }
                None => assert!(actual.is_none(), "oracle leaves this piece undelivered"),
            }
        }
    }

    fn expected_work(&self) -> Vec<Derived> {
        let mut work: Vec<_> = self
            .expected
            .pieces
            .iter()
            .map(|piece| {
                Derived::Piece(PieceWork {
                    unit_key: piece.unit_key.clone(),
                    index: piece.index,
                    payload: piece.payload.clone(),
                })
            })
            .collect();
        work.extend(self.expected.excluded.iter().map(|item| Derived::Excluded {
            unit_key: item.unit_key.clone(),
            reason: item.reason.clone(),
        }));
        work
    }
}

fn resume(fixture: &Fixture, writer: &mut TestWriter) -> Vec<Derived> {
    let mut sources = Sources::new(
        fixture.expected.channel_id,
        fixture.expected.provider,
        Arc::new(EmptyBindings),
    );
    let mut deriver = UnitDeriver::new(fixture.expected.channel_id, fixture.expected.provider);
    let mut owed = VecDeque::new();
    sources.resume(writer, &mut deriver, &mut owed).unwrap();
    println!(
        "PROBE Sources::resume derived_items={} provider={:?}",
        owed.len(),
        fixture.expected.provider
    );
    assert!(
        !owed.is_empty(),
        "production replay must derive actual work"
    );
    owed.into_iter().collect()
}

fn sorted(mut work: Vec<Derived>) -> Vec<Derived> {
    work.sort_by_key(|item| match item {
        Derived::Piece(piece) => (piece.unit_key.clone(), piece.index),
        Derived::Excluded { unit_key, .. } => (unit_key.clone(), u32::MAX),
        Derived::Blocked { reason } => panic!("unexpected blocked record: {reason}"),
    });
    work
}

#[tokio::test]
async fn g2_gate_v2_v3_rederive_same_unitkey_body_frontier() {
    for name in ["claude", "codex"] {
        let fixture = Fixture::load(name);
        let (mut writer, port) = make_writer(fixture.open().unwrap());
        fixture.frontier(writer.store());
        let work = resume(&fixture, &mut writer);
        assert_eq!(
            sorted(work.clone()),
            sorted(fixture.expected_work()),
            "v2/v3 output tuples differ: {name}"
        );
        let pending: Vec<_> = fixture
            .expected
            .pieces
            .iter()
            .filter(|piece| piece.posted.is_none())
            .map(|piece| piece.payload.clone())
            .collect();
        assert!(!pending.is_empty());
        assert!(
            fixture
                .expected
                .pieces
                .iter()
                .any(|piece| piece.posted.is_some())
        );
        for item in &work {
            assert_eq!(writer.deliver(item).await, Step::Done);
        }
        assert_eq!(
            port.posts(),
            pending,
            "already delivered v2 pieces must never repost"
        );
        drop(writer);
        let (mut reopened, second_port) = make_writer(fixture.open().unwrap());
        for item in resume(&fixture, &mut reopened) {
            assert_eq!(reopened.deliver(&item).await, Step::Done);
        }
        assert!(
            second_port.posts().is_empty(),
            "a second recovery must post nothing"
        );
        println!(
            "PROBE ChannelWriter::deliver provider={name} posts={} delivered_reposts=0",
            port.posts().len()
        );
    }
}

fn version(path: &Path) -> u64 {
    let bytes = fs::read(path).unwrap();
    let split = bytes.iter().position(|byte| *byte == b'\n').unwrap();
    serde_json::from_slice::<Value>(&bytes[..split]).unwrap()["identity_version"]
        .as_u64()
        .unwrap()
}

fn set_version(path: &Path, version: u64) {
    let bytes = fs::read(path).unwrap();
    let split = bytes.iter().position(|byte| *byte == b'\n').unwrap();
    let mut header: Value = serde_json::from_slice(&bytes[..split]).unwrap();
    header["identity_version"] = version.into();
    let mut changed = serde_json::to_vec(&header).unwrap();
    changed.extend_from_slice(&bytes[split..]);
    fs::write(path, changed).unwrap();
}

#[tokio::test]
async fn g2_gate_v2_to_v3_rollover_crash_matrix() {
    for name in ["claude", "codex"] {
        for cut in ["before_segment", "after_header", "after_frame_sync"] {
            let fixture = Fixture::load(name);
            let mut channel = fixture.open().unwrap();
            fixture.frontier(&channel);
            let old = fixture.segments();
            assert_eq!(old.len(), 1);
            let old_bytes = fs::read(&old[0]).unwrap();
            let tail = match fixture.expected.provider {
                ShadowProvider::Claude => {
                    serde_json::json!({"type":"assistant","uuid":"tail-row","apiBlockIndex":0,"message":{"id":"tail","content":[{"type":"text","text":"rollover tail"}]}})
                }
                ShadowProvider::Codex => {
                    serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","id":"tail","content":[{"type":"output_text","text":"rollover tail"}]}})
                }
            };
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&fixture.source.path)
                .unwrap();
            writeln!(file, "{tail}").unwrap();
            file.sync_all().unwrap();
            let before = channel.cursor(&fixture.source).unwrap().clone();
            let mut capture = SourceCapture::reopen(
                fixture.source.clone(),
                before.captured_through,
                &before.prefix_hash,
            )
            .unwrap();
            let CaptureOutcome::Batch(batch) = capture.poll(u64::MAX) else {
                panic!("tail capture did not execute")
            };
            assert_eq!(batch.records.len(), 1);
            let (under, step) = match cut {
                "before_segment" => (fixture.channel_dir().join("spool"), fault::Step::Write),
                "after_header" => (
                    fixture.channel_dir().join("spool"),
                    fault::Step::Append(fault::Keep::Nothing),
                ),
                _ => (fixture.channel_dir().join("cursor"), fault::Step::Write),
            };
            let planted = fault::plant(&under, step, io::ErrorKind::StorageFull, Some(1));
            let interrupted = channel.append_spool(&batch, &capture.prefix_hash());
            assert_eq!(
                fs::read(&old[0]).unwrap(),
                old_bytes,
                "v2 nonempty segment must remain byte-identical at every cut"
            );
            assert!(
                matches!(interrupted, Err(StoreError::Io(error)) if error.kind() == io::ErrorKind::StorageFull),
                "cutpoint must fire: {name}/{cut}"
            );
            let cut_segments = fixture.segments();
            assert_eq!(
                cut_segments.len(),
                if cut == "before_segment" { 1 } else { 2 }
            );
            if cut == "after_header" {
                assert_eq!(
                    fs::read(&cut_segments[1])
                        .unwrap()
                        .iter()
                        .filter(|byte| **byte == b'\n')
                        .count(),
                    1,
                    "crash must leave a header only"
                );
            }
            drop(planted);
            drop(channel);
            let persisted: Value = serde_json::from_slice(
                &fs::read(
                    fixture
                        .channel_dir()
                        .join("cursor")
                        .join(format!("{}.json", source_key(&fixture.source))),
                )
                .unwrap(),
            )
            .unwrap();
            assert_eq!(
                persisted["captured_through"], before.captured_through,
                "the cut leaves the durable cursor behind"
            );
            let mut recovered = fixture.open().unwrap();
            let cursor = recovered.cursor(&fixture.source).unwrap();
            assert_eq!(
                cursor.captured_through,
                if cut == "after_frame_sync" {
                    batch.captured_through
                } else {
                    before.captured_through
                },
                "recovery frontier: {name}/{cut}"
            );
            assert_eq!(
                recovered.ledger().anchor(),
                fixture.expected.delivered_frontier
            );
            if cut != "after_frame_sync" {
                recovered
                    .append_spool(&batch, &capture.prefix_hash())
                    .unwrap();
            }
            assert_eq!(
                recovered.cursor(&fixture.source).unwrap().captured_through,
                batch.captured_through
            );
            assert_eq!(
                recovered.cursor(&fixture.source).unwrap().prefix_hash,
                capture.prefix_hash()
            );
            let segments = fixture.segments();
            assert_eq!(segments.len(), 2);
            assert_eq!(
                fs::read(&old[0]).unwrap(),
                old_bytes,
                "v2 nonempty segment must remain byte-identical"
            );
            assert_eq!(version(&segments[1]), 3);
            let (mut writer, port) = make_writer(recovered);
            let work = resume(&fixture, &mut writer);
            for item in work {
                assert_eq!(writer.deliver(&item).await, Step::Done);
            }
            let mut pending: Vec<_> = fixture
                .expected
                .pieces
                .iter()
                .filter(|piece| piece.posted.is_none())
                .map(|piece| piece.payload.clone())
                .collect();
            pending.push("rollover tail".into());
            assert_eq!(
                port.posts(),
                pending,
                "crash recovery must preserve delivered frontier"
            );
            drop(writer);
            let (mut reopened, second_port) = make_writer(fixture.open().unwrap());
            for item in resume(&fixture, &mut reopened) {
                assert_eq!(reopened.deliver(&item).await, Step::Done);
            }
            assert!(
                second_port.posts().is_empty(),
                "repeat recovery after rollover must post nothing"
            );
            drop(reopened);
            for unknown in [1, 4] {
                set_version(&segments[1], unknown);
                assert_eq!(
                    fixture
                        .open()
                        .err()
                        .expect("unknown identity version must halt")
                        .reason,
                    HaltReason::StoreDamage
                );
            }
            set_version(&segments[0], 3);
            set_version(&segments[1], 2);
            assert_eq!(
                fixture
                    .open()
                    .err()
                    .expect("3 to 2 regression must halt")
                    .reason,
                HaltReason::StoreDamage
            );
            println!(
                "PROBE append_spool cut={cut} provider={name} recovery_posts={} delivered_reposts=0",
                port.posts().len()
            );
        }
    }
}
