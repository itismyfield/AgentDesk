use super::*;
use std::io::Write;

struct Fixture {
    dir: tempfile::TempDir,
    cursor: Cursor,
    archive: PathBuf,
}

impl Fixture {
    fn new(body: &[u8]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rollout.jsonl");
        fs::write(&path, body).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let (dev, ino) = crate::services::tui_o::shadow::capture::file_identity(&metadata);
        let source = SourceId {
            session_id: "archive-test".into(),
            path,
            dev,
            ino,
        };
        let archive = archive_path(ShadowProvider::Codex, &source).unwrap();
        fs::write(&archive, zstd::stream::encode_all(body, 1).unwrap()).unwrap();
        fs::remove_file(&source.path).unwrap();
        let cursor = Cursor {
            source,
            captured_through: body.len() as u64,
            prefix_hash: hex::encode(Sha256::digest(body)),
            retired: false,
            transition_cleanup: None,
        };
        Self {
            dir,
            cursor,
            archive,
        }
    }

    fn verify(&self) -> io::Result<VerifiedArchiveEof> {
        verify(ShadowProvider::Codex, &self.cursor, SystemTime::now())
    }

    fn replace_archive(&self, bytes: &[u8]) {
        let replacement = self.dir.path().join("replacement.zst");
        fs::write(&replacement, bytes).unwrap();
        fs::rename(replacement, &self.archive).unwrap();
    }
}

fn with_hook<T>(hook: impl FnMut(TestPoint) + 'static, run: impl FnOnce() -> T) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_HOOK.with_borrow_mut(|slot| *slot = None);
        }
    }
    TEST_HOOK.with_borrow_mut(|slot| assert!(slot.replace(Box::new(hook)).is_none()));
    let reset = Reset;
    let result = run();
    drop(reset);
    result
}

#[test]
fn exact_eof_and_empty_frames_prove_only_the_unchanged_cursor() {
    for body in [&b""[..], &b"first\nsecond\n"[..]] {
        let fixture = Fixture::new(body);
        let proof = fixture.verify().unwrap();
        proof.recheck(&fixture.cursor, SystemTime::now()).unwrap();
        let mut changed = fixture.cursor.clone();
        changed.retired = true;
        assert!(proof.recheck(&changed, SystemTime::now()).is_err());
        assert!(!fixture.cursor.source.path.exists());
    }
}

#[test]
fn concatenated_and_skippable_frames_are_read_through_real_eof() {
    let fixture = Fixture::new(b"first\nsecond\n");
    let mut bytes = zstd::stream::encode_all(&b"first\n"[..], 1).unwrap();
    bytes.extend(zstd::stream::encode_all(&b"second\n"[..], 1).unwrap());
    bytes.extend(zstd::stream::encode_all(&b""[..], 1).unwrap());
    bytes.extend(0x184d2a50u32.to_le_bytes());
    bytes.extend(3u32.to_le_bytes());
    bytes.extend(b"end");
    fixture.replace_archive(&bytes);
    fixture.verify().unwrap();
    bytes.push(0xff);
    fixture.replace_archive(&bytes);
    assert!(
        fixture.verify().is_err(),
        "trailing garbage is not decoder EOF"
    );
}

#[test]
fn length_hash_and_partial_tail_never_become_eof_proofs() {
    let fixture = Fixture::new(b"line\n");
    for decoded in [
        &b"line"[..],
        &b"line\nx"[..],
        &b"line\npartial"[..],
        &b"xxxx\n"[..],
    ] {
        fixture.replace_archive(&zstd::stream::encode_all(decoded, 1).unwrap());
        assert!(fixture.verify().is_err(), "decoded={decoded:?}");
    }
    let mut bytes = zstd::stream::encode_all(&b"line\n"[..], 1).unwrap();
    bytes.extend(zstd::stream::encode_all(&b"x"[..], 1).unwrap());
    fixture.replace_archive(&bytes);
    assert!(
        fixture.verify().is_err(),
        "extra bytes in a later frame are rejected"
    );
}

#[test]
fn corrupt_truncated_checksum_and_large_window_frames_are_rejected() {
    let fixture = Fixture::new(b"checksum\n");
    let mut encoder = zstd::stream::Encoder::new(Vec::new(), 1).unwrap();
    encoder.include_checksum(true).unwrap();
    encoder.write_all(b"checksum\n").unwrap();
    let checked = encoder.finish().unwrap();
    fixture.replace_archive(&checked);
    fixture.verify().unwrap();
    let mut corrupt = checked.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    for bytes in [
        &b"invalid archive"[..],
        &checked[..checked.len() - 1],
        &corrupt,
    ] {
        fixture.replace_archive(bytes);
        assert!(fixture.verify().is_err());
    }
    let empty = Fixture::new(b"");
    empty.replace_archive(b"\x28\xb5\x2f\xfd\x00\x90\x01\x00\x00");
    assert!(
        empty.verify().is_err(),
        "a 256 MiB window exceeds the 128 MiB limit"
    );
}

#[test]
fn provider_path_retired_and_overflow_guards_are_enforced() {
    let mut fixture = Fixture::new(b"line\n");
    assert!(candidate(ShadowProvider::Codex, &fixture.cursor.source));
    assert!(!candidate(ShadowProvider::Claude, &fixture.cursor.source));
    assert!(verify(ShadowProvider::Claude, &fixture.cursor, SystemTime::now()).is_err());
    let mut other = fixture.cursor.source.clone();
    other.path.set_extension("txt");
    assert!(!candidate(ShadowProvider::Codex, &other));
    fixture.cursor.retired = true;
    assert!(fixture.verify().is_err());
    fixture.cursor.retired = false;
    fixture.cursor.captured_through = u64::MAX;
    assert!(fixture.verify().is_err());
}

#[test]
fn future_mtime_and_source_reappearance_are_rejected() {
    let fixture = Fixture::new(b"line\n");
    let now = SystemTime::now();
    let future = now + std::time::Duration::from_secs(60);
    File::options()
        .write(true)
        .open(&fixture.archive)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(future))
        .unwrap();
    assert!(verify(ShadowProvider::Codex, &fixture.cursor, now).is_err());
    fixture.replace_archive(&zstd::stream::encode_all(&b"line\n"[..], 1).unwrap());
    let proof = fixture.verify().unwrap();
    fs::write(&fixture.cursor.source.path, b"line\n").unwrap();
    assert!(fixture.verify().is_err());
    assert!(proof.recheck(&fixture.cursor, SystemTime::now()).is_err());
}

#[test]
fn archive_replacement_is_rejected_at_open_decode_and_commit_boundaries() {
    for point in [TestPoint::Opened, TestPoint::Decoded] {
        let fixture = Fixture::new(b"line\n");
        let archive = fixture.archive.clone();
        let replacement = fixture.dir.path().join("other.zst");
        fs::write(&replacement, fs::read(&archive).unwrap()).unwrap();
        let result = with_hook(
            move |seen| {
                if seen == point {
                    fs::rename(&replacement, &archive).unwrap();
                }
            },
            || fixture.verify(),
        );
        assert!(result.is_err());
    }
    let fixture = Fixture::new(b"line\n");
    let proof = fixture.verify().unwrap();
    fixture.replace_archive(&fs::read(&fixture.archive).unwrap());
    assert!(proof.recheck(&fixture.cursor, SystemTime::now()).is_err());
}

#[cfg(unix)]
#[test]
fn symlink_archives_and_dangling_source_entries_are_not_missing_candidates() {
    let fixture = Fixture::new(b"line\n");
    let actual = fixture.dir.path().join("actual.zst");
    fs::rename(&fixture.archive, &actual).unwrap();
    std::os::unix::fs::symlink(&actual, &fixture.archive).unwrap();
    assert!(!candidate(ShadowProvider::Codex, &fixture.cursor.source));
    assert!(fixture.verify().is_err());
    fs::remove_file(&fixture.archive).unwrap();
    fs::rename(actual, &fixture.archive).unwrap();
    std::os::unix::fs::symlink("gone", &fixture.cursor.source.path).unwrap();
    assert!(fixture.verify().is_err());
}
