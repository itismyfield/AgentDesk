pub(super) fn observe(kind: &str, path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    return supported::observe(kind, path);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (kind, path);
        Ok(())
    }
}

pub(super) fn partial_blob_write(file: &mut std::fs::File, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    return supported::partial_blob_write(file, bytes);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (file, bytes);
        Ok(())
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod supported {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;
    use std::io;
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};

    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::super::blob::BlobPin;
    use super::super::ledger::{Ledger, Record};

    type Inode = (u64, u64);

    #[derive(Clone)]
    enum DurableNode {
        File(Vec<u8>),
        Directory(BTreeMap<OsString, Inode>),
    }

    #[derive(Clone)]
    struct Projection {
        root: PathBuf,
        root_inode: Inode,
        nodes: BTreeMap<Inode, DurableNode>,
    }

    impl Projection {
        fn new(root: &Path) -> Self {
            let mut value = Self {
                root: root.to_owned(),
                root_inode: identity(root),
                nodes: BTreeMap::new(),
            };
            value.seed(root);
            value
        }

        fn seed(&mut self, path: &Path) {
            if path.is_dir() {
                for entry in fs::read_dir(path).unwrap() {
                    self.seed(&entry.unwrap().path());
                }
                self.record("dir_sync", path);
            } else {
                self.record("file_sync", path);
            }
        }

        fn record(&mut self, kind: &str, path: &Path) {
            match kind {
                "file_sync" => {
                    self.nodes
                        .insert(identity(path), DurableNode::File(fs::read(path).unwrap()));
                }
                "dir_sync" => {
                    let mut names = BTreeMap::new();
                    for entry in fs::read_dir(path).unwrap() {
                        let entry = entry.unwrap();
                        let inode = identity(&entry.path());
                        let blank = if entry.file_type().unwrap().is_dir() {
                            DurableNode::Directory(BTreeMap::new())
                        } else {
                            DurableNode::File(Vec::new())
                        };
                        self.nodes.entry(inode).or_insert(blank);
                        names.insert(entry.file_name(), inode);
                    }
                    self.nodes
                        .insert(identity(path), DurableNode::Directory(names));
                }
                _ => (),
            }
        }

        fn materialize(&self, inode: Inode, path: &Path) {
            match self.nodes.get(&inode).unwrap() {
                DurableNode::File(bytes) => fs::write(path, bytes).unwrap(),
                DurableNode::Directory(children) => {
                    fs::create_dir_all(path).unwrap();
                    for (name, child) in children {
                        self.materialize(*child, &path.join(name));
                    }
                }
            }
        }

        fn restore(&self, runtime: &Path) -> (TempDir, PathBuf) {
            let restored = sandbox();
            self.materialize(self.root_inode, restored.path());
            let path = restored
                .path()
                .join(runtime.strip_prefix(&self.root).unwrap());
            (restored, path)
        }
    }

    fn identity(path: &Path) -> Inode {
        let metadata = fs::symlink_metadata(path).unwrap();
        (metadata.dev(), metadata.ino())
    }

    struct Trace {
        projection: Projection,
        events: Vec<String>,
        fail_at: Option<usize>,
        fail_kind: Option<String>,
    }

    thread_local! {
        static TRACE: RefCell<Option<Trace>> = const { RefCell::new(None) };
    }

    struct Recording;

    impl Recording {
        fn start(root: &Path) -> Self {
            TRACE.with(|slot| {
                assert!(slot.borrow().is_none());
                *slot.borrow_mut() = Some(Trace {
                    projection: Projection::new(root),
                    events: Vec::new(),
                    fail_at: None,
                    fail_kind: None,
                });
            });
            Self
        }

        fn arm(&self, fail_at: Option<usize>) {
            TRACE.with(|slot| {
                let mut slot = slot.borrow_mut();
                let trace = slot.as_mut().unwrap();
                trace.events.clear();
                trace.fail_at = fail_at;
                trace.fail_kind = None;
            });
        }

        fn fail_on(&self, kind: &str) {
            self.arm(None);
            TRACE.with(|slot| slot.borrow_mut().as_mut().unwrap().fail_kind = Some(kind.into()));
        }

        fn finish(self) -> (Projection, Vec<String>) {
            let trace = TRACE.with(|slot| slot.borrow_mut().take().unwrap());
            (trace.projection, trace.events)
        }
    }

    impl Drop for Recording {
        fn drop(&mut self) {
            TRACE.with(|slot| *slot.borrow_mut() = None);
        }
    }

    pub(super) fn observe(kind: &str, path: &Path) -> io::Result<()> {
        TRACE.with(|slot| {
            let mut slot = slot.borrow_mut();
            let Some(trace) = slot.as_mut() else {
                return Ok(());
            };
            if !path.starts_with(&trace.projection.root) {
                return Ok(());
            }
            trace.projection.record(kind, path);
            trace.events.push(format!("{kind}:{}", path.display()));
            if trace.fail_at == Some(trace.events.len()) || trace.fail_kind.as_deref() == Some(kind)
            {
                return Err(io::Error::other("simulated power cut after syscall"));
            }
            Ok(())
        })
    }

    fn sandbox() -> TempDir {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
        fs::create_dir_all(&root).unwrap();
        tempfile::Builder::new()
            .prefix("durability-")
            .tempdir_in(root)
            .unwrap()
    }

    fn fold(ledger: &Ledger) -> Vec<Value> {
        let mut rows = ledger
            .snapshot()
            .map_or_else(Vec::new, |s| s.state.as_array().unwrap().clone());
        rows.extend(ledger.records().iter().map(|r| r.payload.clone()));
        rows
    }

    fn channel(runtime: &Path) -> PathBuf {
        runtime.join("input_ledger/7")
    }

    #[test]
    fn reopen_adopted_unsynced_prefix_survives_power_loss_without_more_writes() {
        let root = sandbox();
        let recording = Recording::start(root.path());
        let runtime = root.path().join("runtime");
        let mut ledger = Ledger::open(&runtime, 7).unwrap();
        ledger.append("transition", json!("A"), &[]).unwrap();
        recording.arm(Some(1));
        assert!(ledger.append("transition", json!("B"), &[]).is_err());
        drop(ledger);
        recording.arm(None);
        let recovered = Ledger::open(&runtime, 7).unwrap();
        assert_eq!(fold(&recovered), vec![json!("A"), json!("B")]);
        drop(recovered);
        let (projection, _) = recording.finish();
        let (_restored, runtime) = projection.restore(&runtime);
        assert_eq!(
            fold(&Ledger::open(&runtime, 7).unwrap()),
            vec![json!("A"), json!("B")]
        );
    }

    #[test]
    fn reopen_refuses_handle_when_adopted_prefix_sync_fails() {
        let root = sandbox();
        let runtime = root.path().join("runtime");
        let mut ledger = Ledger::open(&runtime, 7).unwrap();
        ledger.append("transition", json!("A"), &[]).unwrap();
        drop(ledger);
        let recording = Recording::start(root.path());
        recording.fail_on("file_sync");
        let error = Ledger::open(&runtime, 7)
            .err()
            .expect("recovery must not return a handle after a sync error");
        assert_eq!(error.kind(), io::ErrorKind::Other);
    }

    fn nested_array(depth: usize) -> Value {
        (0..depth).fold(Value::Null, |value, _| Value::Array(vec![value]))
    }

    #[test]
    fn append_rejects_unreplayable_json_before_writing_and_remains_usable() {
        let root = sandbox();
        let runtime = root.path().join("runtime");
        let mut ledger = Ledger::open(&runtime, 7).unwrap();
        ledger.append("transition", json!("A"), &[]).unwrap();
        let wal = channel(&runtime).join("wal.0.jsonl");
        let before = fs::read(&wal).unwrap();
        for (payload, reason) in [
            (nested_array(130), "recursion limit"),
            (json!(51.24817837550540_4_f64), "round-trip"),
        ] {
            let error = ledger.append("transition", payload, &[]).unwrap_err();
            assert!(error.to_string().contains(reason), "{error}");
            assert_eq!(fs::read(&wal).unwrap(), before);
        }
        ledger.append("transition", json!("B"), &[]).unwrap();
        drop(ledger);
        assert_eq!(
            fold(&Ledger::open(&runtime, 7).unwrap()),
            vec![json!("A"), json!("B")]
        );
    }

    #[test]
    fn checkpoint_rejects_unreadable_json_before_replacing_snapshot() {
        let root = sandbox();
        let runtime = root.path().join("runtime");
        let mut ledger = Ledger::open(&runtime, 7).unwrap();
        ledger.append("transition", json!("A"), &[]).unwrap();
        ledger.checkpoint(json!(["A"])).unwrap();
        ledger.append("transition", json!("B"), &[]).unwrap();
        let snapshot = channel(&runtime).join("snapshot.json");
        let wal = channel(&runtime).join("wal.1.jsonl");
        let before = (fs::read(&snapshot).unwrap(), fs::read(&wal).unwrap());
        for (state, reason) in [
            (nested_array(130), "recursion limit"),
            (json!(51.24817837550540_4_f64), "round-trip"),
        ] {
            let error = ledger.checkpoint(state).unwrap_err();
            assert!(error.to_string().contains(reason), "{error}");
            assert_eq!(
                (fs::read(&snapshot).unwrap(), fs::read(&wal).unwrap()),
                before
            );
        }
        ledger.append("transition", json!("C"), &[]).unwrap();
        drop(ledger);
        assert_eq!(
            fold(&Ledger::open(&runtime, 7).unwrap()),
            vec![json!("A"), json!("B"), json!("C")]
        );
    }

    #[test]
    fn accepted_json_round_trips_through_wal_and_snapshot() {
        let root = sandbox();
        let runtime = root.path().join("runtime");
        let expected = vec![
            json!({"null":null, "bool":true, "string":"한글/é/\n", "numbers":[0, -42, 0.125, u64::MAX]}),
            nested_array(120),
        ];
        let mut ledger = Ledger::open(&runtime, 7).unwrap();
        for value in &expected {
            ledger.append("transition", value.clone(), &[]).unwrap();
        }
        drop(ledger);
        let mut ledger = Ledger::open(&runtime, 7).unwrap();
        assert_eq!(fold(&ledger), expected);
        ledger.checkpoint(json!(expected)).unwrap();
        drop(ledger);
        assert_eq!(fold(&Ledger::open(&runtime, 7).unwrap()), expected);
    }

    fn prepare(root: &Path, previous_checkpoint: bool) -> (Ledger, PathBuf, Vec<Value>) {
        let runtime = root.join("new/ancestors/runtime");
        let mut ledger = Ledger::open(&runtime, 7).unwrap();
        ledger.append("transition", json!(1), &[]).unwrap();
        let mut expected = vec![json!(1)];
        if previous_checkpoint {
            ledger.checkpoint(json!(expected)).unwrap();
            ledger.append("transition", json!(2), &[]).unwrap();
            expected.push(json!(2));
        }
        (ledger, runtime, expected)
    }

    #[test]
    fn acknowledged_transition_survives_new_ancestor_power_loss() {
        let root = sandbox();
        let recording = Recording::start(root.path());
        let runtime = root.path().join("new/ancestors/runtime");
        let mut ledger = Ledger::open(&runtime, 7).unwrap();
        let transition = json!({"state":"held", "incident":"unverified", "notice":"pending"});
        ledger
            .append("transition", transition.clone(), &[])
            .unwrap();
        let (projection, _) = recording.finish();
        let (_restored, runtime) = projection.restore(&runtime);
        let recovered = Ledger::open(&runtime, 7).unwrap();
        assert_eq!(fold(&recovered), vec![transition]);
    }

    fn checkpoint_case(previous_checkpoint: bool, cut: Option<usize>) -> Vec<String> {
        let root = sandbox();
        let recording = Recording::start(root.path());
        let (mut ledger, runtime, mut expected) = prepare(root.path(), previous_checkpoint);
        recording.arm(cut);
        let result = ledger.checkpoint(json!(expected));
        assert_eq!(result.is_err(), cut.is_some(), "checkpoint cut {cut:?}");
        let (projection, events) = recording.finish();
        let (restored, runtime) = projection.restore(&runtime);
        let resumed_recording = Recording::start(restored.path());
        let mut recovered = Ledger::open(&runtime, 7).unwrap();
        assert_eq!(
            fold(&recovered),
            expected,
            "checkpoint cut {cut:?}: {events:?}"
        );
        recovered.append("transition", json!(3), &[]).unwrap();
        expected.push(json!(3));
        let (projection, _) = resumed_recording.finish();
        let (_again, runtime) = projection.restore(&runtime);
        assert_eq!(fold(&Ledger::open(&runtime, 7).unwrap()), expected);
        events
    }

    #[test]
    fn snapshot_replacement_survives_every_cut_and_resumes_append() {
        for previous_checkpoint in [false, true] {
            let events = checkpoint_case(previous_checkpoint, None);
            assert!(!events.is_empty());
            for cut in 1..=events.len() {
                checkpoint_case(previous_checkpoint, Some(cut));
            }
        }
    }

    thread_local! {
        static PARTIAL_BLOB_WRITE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    struct PartialBlobWrite;

    impl PartialBlobWrite {
        fn arm() -> Self {
            PARTIAL_BLOB_WRITE.with(|armed| assert!(!armed.replace(true)));
            Self
        }
    }

    impl Drop for PartialBlobWrite {
        fn drop(&mut self) {
            PARTIAL_BLOB_WRITE.with(|armed| armed.set(false));
        }
    }

    pub(super) fn partial_blob_write(file: &mut fs::File, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;

        if PARTIAL_BLOB_WRITE.with(|armed| armed.replace(false)) {
            file.write_all(&bytes[..bytes.len() / 2])?;
            return Err(io::Error::other("simulated partial blob write"));
        }
        Ok(())
    }

    #[test]
    fn partial_blob_write_retries_without_occupying_final_name() {
        let root = sandbox();
        let runtime = root.path().join("runtime");
        let ledger = Ledger::open(&runtime, 7).unwrap();
        let partial_write = PartialBlobWrite::arm();
        let error = ledger
            .pin_blob("row", 0, "payload.txt", b"attachment bytes")
            .unwrap_err();
        assert!(error.to_string().contains("simulated partial blob write"));
        drop(partial_write);
        let parent = channel(&runtime).join("blobs/att/row");
        assert!(!parent.join("0_payload.txt").exists());
        let abandoned = parent.join(".blob-abandoned.tmp");
        fs::write(&abandoned, b"incomplete staging file").unwrap();
        let pin = ledger
            .pin_blob("row", 0, "payload.txt", b"attachment bytes")
            .unwrap();
        assert_eq!(ledger.read_blob(&pin).unwrap(), b"attachment bytes");
        assert_eq!(fs::read(abandoned).unwrap(), b"incomplete staging file");
        drop(ledger);
        assert_eq!(
            Ledger::open(&runtime, 7).unwrap().read_blob(&pin).unwrap(),
            b"attachment bytes"
        );
    }

    #[test]
    fn incomplete_final_blob_is_rejected_without_overwrite() {
        let root = sandbox();
        let runtime = root.path().join("runtime");
        let ledger = Ledger::open(&runtime, 7).unwrap();
        let path = channel(&runtime).join("blobs/att/row/0_payload.txt");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"attach").unwrap();
        let error = ledger
            .pin_blob("row", 0, "payload.txt", b"attachment bytes")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("blob checksum mismatch"));
        assert_eq!(fs::read(path).unwrap(), b"attach");
    }

    fn blob_case(cut: Option<usize>) -> Vec<String> {
        let root = sandbox();
        let recording = Recording::start(root.path());
        let runtime = root.path().join("runtime");
        let mut ledger = Ledger::open(&runtime, 7).unwrap();
        ledger.append("transition", json!(0), &[]).unwrap();
        recording.arm(cut);
        let result = (|| -> io::Result<()> {
            let pin = ledger.pin_blob("row", 0, "payload.txt", b"attachment bytes")?;
            ledger.append("attachment", json!({"pin":pin}), &[pin])?;
            Ok(())
        })();
        assert_eq!(result.is_err(), cut.is_some(), "blob cut {cut:?}");
        let (projection, events) = recording.finish();
        let (_restored, runtime) = projection.restore(&runtime);
        let recovered = Ledger::open(&runtime, 7).unwrap();
        let rows = fold(&recovered);
        assert_eq!(rows.first(), Some(&json!(0)));
        assert!(rows.len() <= 2);
        if cut.is_none() {
            assert_eq!(rows.len(), 2);
        }
        for row in rows.iter().skip(1) {
            let pin: BlobPin = serde_json::from_value(row["pin"].clone()).unwrap();
            assert_eq!(
                recovered.read_blob(&pin).unwrap(),
                b"attachment bytes",
                "blob cut {cut:?}: {events:?}"
            );
        }
        events
    }

    #[test]
    fn referenced_blob_survives_every_pin_and_append_cut() {
        let events = blob_case(None);
        assert!(!events.is_empty());
        for cut in 1..=events.len() {
            blob_case(Some(cut));
        }
    }

    fn encoded(record: &Record) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(record).unwrap();
        bytes.push(b'\n');
        bytes
    }

    #[test]
    fn replay_stops_at_first_bad_record_and_discards_valid_suffix() {
        for damage in ["json", "unterminated", "crc", "sequence", "previous_crc"] {
            let root = sandbox();
            let runtime = root.path().join("runtime");
            let mut ledger = Ledger::open(&runtime, 7).unwrap();
            let first = ledger.append("transition", json!(1), &[]).unwrap().clone();
            let second = ledger.append("transition", json!(2), &[]).unwrap().clone();
            drop(ledger);
            let mut bad = second.clone();
            match damage {
                "sequence" => bad.seq += 1,
                "previous_crc" => bad.prev_crc ^= 1,
                _ => (),
            }
            bad.crc = super::super::durable::crc(
                &serde_json::to_vec(&(bad.seq, bad.prev_crc, &bad.kind, &bad.payload)).unwrap(),
            );
            if damage == "crc" {
                bad.crc ^= 1;
            }
            let mut bytes = encoded(&first);
            if damage == "unterminated" {
                let mut tail = encoded(&second);
                tail.pop();
                bytes.extend(tail);
            } else {
                bytes.extend(if damage == "json" {
                    b"{\"seq\":2\n".to_vec()
                } else {
                    encoded(&bad)
                });
                bytes.extend(encoded(&second));
            }
            let wal = channel(&runtime).join("wal.0.jsonl");
            fs::write(&wal, bytes).unwrap();
            fs::OpenOptions::new()
                .write(true)
                .open(&wal)
                .unwrap()
                .sync_all()
                .unwrap();
            let recording = Recording::start(root.path());
            let mut recovered = Ledger::open(&runtime, 7).unwrap();
            assert_eq!(fold(&recovered), vec![json!(1)], "{damage}");
            recovered.append("transition", json!(3), &[]).unwrap();
            let (projection, _) = recording.finish();
            let (_restored, runtime) = projection.restore(&runtime);
            assert_eq!(
                fold(&Ledger::open(&runtime, 7).unwrap()),
                vec![json!(1), json!(3)],
                "{damage}"
            );
        }
    }

    #[test]
    fn corrupt_snapshot_refuses_open_instead_of_empty_recovery() {
        for damage in ["json", "crc"] {
            let root = sandbox();
            let (mut ledger, runtime, expected) = prepare(root.path(), false);
            ledger.checkpoint(json!(expected)).unwrap();
            drop(ledger);
            let path = channel(&runtime).join("snapshot.json");
            if damage == "json" {
                fs::write(&path, b"{\"generation\":").unwrap();
            } else {
                let mut snapshot: Value =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                snapshot["state"] = json!([]);
                fs::write(&path, serde_json::to_vec(&snapshot).unwrap()).unwrap();
            }
            let error = Ledger::open(&runtime, 7)
                .err()
                .expect("corrupt snapshot must be rejected");
            assert_eq!(
                error.kind(),
                if damage == "json" {
                    io::ErrorKind::UnexpectedEof
                } else {
                    io::ErrorKind::InvalidData
                }
            );
            if damage == "crc" {
                assert!(error.to_string().contains("snapshot checksum"));
            }
        }
    }

    fn poison_case(checkpoint: bool, cut: Option<usize>) -> Vec<String> {
        let root = sandbox();
        let recording = Recording::start(root.path());
        let (mut ledger, _runtime, expected) = prepare(root.path(), false);
        recording.arm(cut);
        let result = if checkpoint {
            ledger.checkpoint(json!(expected))
        } else {
            ledger.append("transition", json!(2), &[]).map(|_| ())
        };
        assert_eq!(result.is_err(), cut.is_some());
        if cut.is_some() {
            let error = ledger.append("transition", json!(3), &[]).unwrap_err();
            assert!(error.to_string().contains("reopened"));
            let error = ledger.checkpoint(json!([])).unwrap_err();
            assert!(error.to_string().contains("reopened"));
        }
        recording.finish().1
    }

    #[test]
    fn failed_append_or_checkpoint_poison_handle_until_reopen() {
        for checkpoint in [false, true] {
            let events = poison_case(checkpoint, None);
            assert!(!events.is_empty());
            for cut in 1..=events.len() {
                poison_case(checkpoint, Some(cut));
            }
        }
    }

    #[test]
    fn missing_or_changed_blob_rejects_append_without_losing_checksum_reason() {
        let root = sandbox();
        let runtime = root.path().join("runtime");
        let mut ledger = Ledger::open(&runtime, 7).unwrap();
        let pin = ledger
            .pin_blob("row", 0, "payload.txt", b"original")
            .unwrap();
        assert_eq!(
            ledger
                .pin_blob("row", 0, "payload.txt", b"original")
                .unwrap(),
            pin
        );
        let error = ledger
            .pin_blob("row", 0, "payload.txt", b"different")
            .unwrap_err();
        assert!(error.to_string().contains("blob checksum"));
        assert_eq!(ledger.read_blob(&pin).unwrap(), b"original");
        let path = channel(&runtime).join(&pin.local_path);
        fs::write(&path, b"changed").unwrap();
        let error = ledger
            .append("attachment", json!({"pin":pin}), std::slice::from_ref(&pin))
            .unwrap_err();
        assert!(error.to_string().contains("blob checksum"));
        fs::remove_file(&path).unwrap();
        let error = ledger
            .append("attachment", json!({"pin":pin}), &[pin])
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        drop(ledger);
        assert!(fold(&Ledger::open(&runtime, 7).unwrap()).is_empty());
    }

    #[test]
    fn blob_paths_reject_normalized_aliases_and_symlinks() {
        let root = sandbox();
        let runtime = root.path().join("runtime");
        let ledger = Ledger::open(&runtime, 7).unwrap();
        for alias in ["row/", "row//", "row/.", "./row", "row/../row", "../row"] {
            let error = ledger
                .pin_blob(alias, 0, "payload.txt", b"bytes")
                .expect_err(alias);
            assert!(
                error.to_string().contains("invalid blob path"),
                "{alias}: {error}"
            );
        }
        for alias in [
            "payload/",
            "payload//",
            "payload/.",
            "./payload",
            "../payload",
        ] {
            assert!(
                ledger.pin_blob("row", 0, alias, b"bytes").is_err(),
                "{alias}"
            );
        }
        let pin = ledger.pin_blob("row", 0, "payload.txt", b"bytes").unwrap();
        for alias in [
            "blobs//att/row/0_payload.txt",
            "blobs/att/row/./0_payload.txt",
            "blobs/att/row/0_payload.txt/",
        ] {
            let mut aliased = pin.clone();
            aliased.local_path = alias.into();
            assert!(ledger.read_blob(&aliased).is_err(), "{alias}");
        }
        let mut unpinned = pin.clone();
        unpinned.pinned = false;
        assert!(
            ledger
                .read_blob(&unpinned)
                .unwrap_err()
                .to_string()
                .contains("invalid blob pin")
        );
        let link = channel(&runtime).join("blobs/att/alias");
        std::os::unix::fs::symlink("row", &link).unwrap();
        let mut aliased = pin;
        aliased.local_path = "blobs/att/alias/0_payload.txt".into();
        assert!(
            ledger
                .read_blob(&aliased)
                .unwrap_err()
                .to_string()
                .contains("symlink")
        );
        assert!(ledger.pin_blob("alias", 1, "other.txt", b"bytes").is_err());
    }
}
