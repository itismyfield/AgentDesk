use super::*;
use std::cell::RefCell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stage {
    Open,
    Metadata,
    Read,
    AfterScan,
}

enum Hook {
    Fail(Stage),
    #[cfg(unix)]
    Mutate(Stage, Box<dyn FnOnce(&Path)>),
}

thread_local! {
    static NEXT: RefCell<Option<Hook>> = const { RefCell::new(None) };
}

pub(crate) struct Injection;

impl Drop for Injection {
    fn drop(&mut self) {
        let _ = NEXT.with(|next| next.replace(None));
    }
}

pub(crate) fn fail_once(stage: Stage) -> Injection {
    NEXT.with(|next| assert!(next.replace(Some(Hook::Fail(stage))).is_none()));
    Injection
}

#[cfg(unix)]
fn mutate_once(stage: Stage, change: impl FnOnce(&Path) + 'static) -> Injection {
    NEXT.with(|next| {
        assert!(
            next.replace(Some(Hook::Mutate(stage, Box::new(change))))
                .is_none()
        );
    });
    Injection
}

pub(super) fn point(stage: Stage, _path: &Path) -> std::io::Result<()> {
    let hook = NEXT.with(|next| {
        let mut next = next.borrow_mut();
        let armed = match next.as_ref() {
            Some(Hook::Fail(armed)) => *armed == stage,
            #[cfg(unix)]
            Some(Hook::Mutate(armed, _)) => *armed == stage,
            None => false,
        };
        if armed { next.take() } else { None }
    });
    match hook {
        Some(Hook::Fail(_)) => Err(std::io::Error::other("injected census I/O failure")),
        #[cfg(unix)]
        Some(Hook::Mutate(_, change)) => {
            change(_path);
            Ok(())
        }
        None => Ok(()),
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::services::tui_o::shadow::capture::file_identity;
    use serde_json::json;
    use std::fs;

    struct Case {
        source: SourceId,
        _dir: tempfile::TempDir,
    }

    impl Case {
        fn new(bytes: &[u8]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("native.jsonl");
            fs::write(&path, bytes).unwrap();
            let (dev, ino) = file_identity(&fs::metadata(&path).unwrap());
            Self {
                source: SourceId {
                    session_id: "native".into(),
                    path,
                    dev,
                    ino,
                },
                _dir: dir,
            }
        }

        fn read(&self, cache: &mut Cache) -> (Option<Fingerprint>, Option<Arc<Vec<Taken>>>) {
            let bytes = fs::read(&self.source.path).unwrap();
            cache.read(&self.source, bytes.len() as u64, &prefix(&bytes))
        }
    }

    fn user(uuid: &str) -> Vec<u8> {
        let mut bytes = serde_json::to_vec(&json!({
            "type":"user", "uuid":uuid, "message":{"content":"direct prompt"}
        }))
        .unwrap();
        bytes.push(b'\n');
        bytes
    }

    fn prefix(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    fn reads() -> usize {
        CENSUS_READS.with(|count| count.get())
    }

    fn reset_reads() {
        CENSUS_READS.with(|count| count.set(0));
    }

    fn ids(rows: &Option<Arc<Vec<Taken>>>) -> Vec<&str> {
        rows.as_ref()
            .unwrap()
            .iter()
            .map(|row| row.uuid.as_str())
            .collect()
    }

    #[test]
    fn native_census_cache_reuses_valid_rows_with_positive_scan_control() {
        let c = Case::new(&user("direct"));
        let mut cache = Cache::default();
        reset_reads();
        let (first_fp, first) = c.read(&mut cache);
        assert!(first_fp.is_some());
        assert_eq!(ids(&first), ["direct"]);
        assert_eq!(reads(), 1, "positive control actually scanned the source");
        let (next_fp, next) = c.read(&mut cache);
        assert_eq!(next_fp, first_fp);
        assert!(Arc::ptr_eq(first.as_ref().unwrap(), next.as_ref().unwrap()));
        assert_eq!(reads(), 1, "the same source snapshot must not scan twice");
        let mut cold = Cache::default();
        let (_, reopened) = c.read(&mut cold);
        assert_eq!(ids(&reopened), ids(&first));
        assert_eq!(reads(), 2, "a cold cache recomputes the same answer");
    }

    #[test]
    fn native_census_cache_reuses_each_stable_invalid_snapshot() {
        let malformed = b"not-json\n".to_vec();
        let missing_uuid = b"{\"type\":\"user\",\"message\":{\"content\":\"direct\"}}\n".to_vec();
        let mut unterminated = user("no-newline");
        unterminated.pop();
        for bytes in [
            malformed,
            missing_uuid,
            unterminated,
            user("prefix-mismatch"),
        ] {
            let c = Case::new(&bytes);
            let expected = if bytes == user("prefix-mismatch") {
                "0".repeat(64)
            } else {
                prefix(&bytes)
            };
            let before = fingerprint(&c.source).unwrap();
            let mut cache = Cache::default();
            reset_reads();
            let (first_fp, first) = cache.read(&c.source, bytes.len() as u64, &expected);
            assert_eq!(first_fp.as_ref(), Some(&before));
            assert!(first.is_none());
            assert_eq!(reads(), 1, "invalid input still performs one real scan");
            let (next_fp, next) = cache.read(&c.source, bytes.len() as u64, &expected);
            assert_eq!(next_fp.as_ref(), Some(&before));
            assert!(next.is_none());
            assert_eq!(reads(), 1, "deterministic None must be reusable");
        }
    }

    #[test]
    fn native_census_transient_failures_retry_without_any_key_or_file_change() {
        for stage in [Stage::Open, Stage::Metadata, Stage::Read] {
            let bytes = user("direct");
            let c = Case::new(&bytes);
            let before = fingerprint(&c.source).unwrap();
            let source = c.source.clone();
            let through = bytes.len() as u64;
            let hash = prefix(&bytes);
            let mut cache = Cache::default();
            reset_reads();
            let injection = fail_once(stage);
            let (_, failed) = cache.read(&source, through, &hash);
            assert!(failed.is_none(), "{stage:?} must be unavailable");
            assert_eq!(reads(), 1, "failed scan is the positive control");
            drop(injection);
            assert_eq!(fingerprint(&source), Some(before.clone()));
            assert_eq!(fs::read(&source.path).unwrap(), bytes);
            let (_, recovered) = cache.read(&source, through, &hash);
            assert_eq!(ids(&recovered), ["direct"], "{stage:?} must retry");
            assert_eq!(reads(), 2, "transient failure must never be memoized");
            let (_, reused) = cache.read(&source, through, &hash);
            assert_eq!(ids(&reused), ["direct"]);
            assert_eq!(reads(), 2);
            assert_eq!(fingerprint(&source), Some(before));
        }
    }

    #[test]
    fn native_census_append_refreshes_same_source_then_reuses_new_cursor() {
        use std::io::Write;
        let c = Case::new(&user("first"));
        let initial = fingerprint(&c.source).unwrap();
        let mut cache = Cache::default();
        reset_reads();
        assert_eq!(ids(&c.read(&mut cache).1), ["first"]);
        assert_eq!(reads(), 1);
        fs::OpenOptions::new()
            .append(true)
            .open(&c.source.path)
            .unwrap()
            .write_all(&user("second"))
            .unwrap();
        assert_ne!(fingerprint(&c.source), Some(initial));
        assert_eq!(ids(&c.read(&mut cache).1), ["first", "second"]);
        assert_eq!(reads(), 2);
        assert_eq!(ids(&c.read(&mut cache).1), ["first", "second"]);
        assert_eq!(reads(), 2);
    }

    #[test]
    fn native_census_prefix_key_change_invalidates_without_fingerprint_change() {
        let bytes = user("direct");
        let c = Case::new(&bytes);
        let before = fingerprint(&c.source);
        let mut cache = Cache::default();
        reset_reads();
        assert!(
            cache
                .read(&c.source, bytes.len() as u64, &prefix(&bytes))
                .1
                .is_some()
        );
        assert_eq!(reads(), 1);
        assert!(
            cache
                .read(&c.source, bytes.len() as u64, &"0".repeat(64))
                .1
                .is_none()
        );
        assert_eq!(reads(), 2);
        assert!(
            cache
                .read(&c.source, bytes.len() as u64, &"0".repeat(64))
                .1
                .is_none()
        );
        assert_eq!(reads(), 2);
        assert_eq!(
            ids(&cache.read(&c.source, bytes.len() as u64, &prefix(&bytes)).1),
            ["direct"]
        );
        assert_eq!(reads(), 3);
        assert_eq!(fingerprint(&c.source), before);
    }

    #[test]
    fn native_census_through_key_change_invalidates_without_fingerprint_change() {
        let first = user("first");
        let bytes = [first.clone(), user("second")].concat();
        let c = Case::new(&bytes);
        let before = fingerprint(&c.source);
        let hash = prefix(&first);
        let mut cache = Cache::default();
        reset_reads();
        assert_eq!(
            ids(&cache.read(&c.source, first.len() as u64, &hash).1),
            ["first"]
        );
        assert_eq!(reads(), 1);
        assert!(cache.read(&c.source, bytes.len() as u64, &hash).1.is_none());
        assert_eq!(reads(), 2);
        assert!(cache.read(&c.source, bytes.len() as u64, &hash).1.is_none());
        assert_eq!(reads(), 2);
        assert_eq!(
            ids(&cache.read(&c.source, bytes.len() as u64, &prefix(&bytes)).1),
            ["first", "second"]
        );
        assert_eq!(reads(), 3);
        assert_eq!(fingerprint(&c.source), before);
    }

    #[test]
    fn native_census_source_identity_and_same_path_incarnation_are_distinct() {
        let bytes = user("first");
        let c = Case::new(&bytes);
        let mut cache = Cache::default();
        reset_reads();
        assert_eq!(ids(&c.read(&mut cache).1), ["first"]);
        assert_eq!(reads(), 1);
        let mut renamed_session = c.source.clone();
        renamed_session.session_id = "other-native".into();
        assert_eq!(
            ids(&cache
                .read(&renamed_session, bytes.len() as u64, &prefix(&bytes))
                .1),
            ["first"]
        );
        assert_eq!(reads(), 2, "the complete SourceId belongs in the key");
        let replacement = c.source.path.with_extension("replacement");
        let next = user("other");
        fs::write(&replacement, &next).unwrap();
        fs::rename(&replacement, &c.source.path).unwrap();
        assert!(
            c.read(&mut cache).1.is_none(),
            "an old inode cannot authorize the replacement"
        );
        let (dev, ino) = file_identity(&fs::metadata(&c.source.path).unwrap());
        let updated = SourceId {
            dev,
            ino,
            ..c.source.clone()
        };
        assert_eq!(
            ids(&cache.read(&updated, next.len() as u64, &prefix(&next)).1),
            ["other"]
        );
    }

    #[test]
    fn native_census_same_length_rewrite_with_restored_mtime_uses_ctime() {
        let before_bytes = user("first");
        let after_bytes = user("other");
        assert_eq!(before_bytes.len(), after_bytes.len());
        let c = Case::new(&before_bytes);
        let before = fingerprint(&c.source).unwrap();
        let mut cache = Cache::default();
        reset_reads();
        assert_eq!(ids(&c.read(&mut cache).1), ["first"]);
        assert_eq!(reads(), 1);
        fs::write(&c.source.path, &after_bytes).unwrap();
        fs::File::options()
            .write(true)
            .open(&c.source.path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(before.modified))
            .unwrap();
        let after = fingerprint(&c.source).unwrap();
        assert_eq!(
            (after.len, after.dev, after.ino, after.modified),
            (before.len, before.dev, before.ino, before.modified)
        );
        assert_ne!(after.change, before.change, "ctime is the rewrite witness");
        assert!(
            cache
                .read(&c.source, before_bytes.len() as u64, &prefix(&before_bytes))
                .1
                .is_none()
        );
        assert_eq!(reads(), 2, "old proof must not hit after overwrite");
        assert_eq!(
            ids(&cache
                .read(&c.source, after_bytes.len() as u64, &prefix(&after_bytes))
                .1),
            ["other"]
        );
        assert_eq!(reads(), 3);
    }

    #[test]
    fn native_census_mutation_after_scan_is_not_returned_or_cached() {
        for initial in [user("first"), b"not-json\n".to_vec()] {
            let c = Case::new(&initial);
            let replacement = user("other");
            let next = replacement.clone();
            let mut cache = Cache::default();
            reset_reads();
            let _mutation = mutate_once(Stage::AfterScan, move |path| {
                fs::write(path, &next).unwrap()
            });
            let (stamp, rows) = cache.read(&c.source, initial.len() as u64, &prefix(&initial));
            assert!(
                stamp.is_none(),
                "changed FD/path must invalidate even deterministic None"
            );
            assert!(rows.is_none());
            assert_eq!(reads(), 1);
            let (_, recovered) = c.read(&mut cache);
            assert_eq!(ids(&recovered), ["other"]);
            assert_eq!(reads(), 2);
            assert_eq!(ids(&c.read(&mut cache).1), ["other"]);
            assert_eq!(reads(), 2);
        }
    }

    #[test]
    fn native_census_source_cap_evicts_oldest_and_keeps_recent_hit() {
        let bytes = user("direct");
        let c = Case::new(&bytes);
        let mut cache = Cache::default();
        reset_reads();
        for i in 0..=MAX_SOURCES {
            let mut source = c.source.clone();
            source.session_id = format!("source-{i}");
            assert_eq!(
                ids(&cache.read(&source, bytes.len() as u64, &prefix(&bytes)).1),
                ["direct"]
            );
        }
        assert_eq!(reads(), MAX_SOURCES + 1);
        assert_eq!(cache.entries.len(), MAX_SOURCES);
        let mut last = c.source.clone();
        last.session_id = format!("source-{MAX_SOURCES}");
        assert!(
            cache
                .read(&last, bytes.len() as u64, &prefix(&bytes))
                .1
                .is_some()
        );
        assert_eq!(reads(), MAX_SOURCES + 1, "recent source remains reusable");
        let mut oldest = c.source.clone();
        oldest.session_id = "source-0".into();
        assert!(
            cache
                .read(&oldest, bytes.len() as u64, &prefix(&bytes))
                .1
                .is_some()
        );
        assert_eq!(
            reads(),
            MAX_SOURCES + 2,
            "eviction only causes recomputation"
        );
        assert_eq!(cache.entries.len(), MAX_SOURCES);
    }

    #[test]
    fn native_census_byte_cap_evicts_results_and_oversize_falls_back() {
        let cases: Vec<_> = (0..3)
            .map(|i| Case::new(&user(&format!("{i}{}", "u".repeat(3 * 1024 * 1024)))))
            .collect();
        let mut cache = Cache::default();
        reset_reads();
        for c in &cases {
            assert!(c.read(&mut cache).1.is_some());
            assert!(cache.bytes <= MAX_BYTES);
        }
        assert_eq!(reads(), 3);
        assert!(cases[2].read(&mut cache).1.is_some());
        assert_eq!(reads(), 3);
        assert!(cases[0].read(&mut cache).1.is_some());
        assert_eq!(reads(), 4);
        assert!(cache.bytes <= MAX_BYTES);
        let oversize = Case::new(&user(&"u".repeat(MAX_BYTES + 1)));
        assert!(
            oversize.read(&mut cache).1.is_some(),
            "uncached answer remains usable"
        );
        assert_eq!(reads(), 5);
        assert!(oversize.read(&mut cache).1.is_some());
        assert_eq!(reads(), 6, "oversize has no persistent cache entry");
        assert!(cache.bytes <= MAX_BYTES);
    }
}

#[cfg(not(unix))]
#[test]
fn native_census_without_unix_change_witness_scans_each_time() {
    use serde_json::json;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("native.jsonl");
    let mut raw =
        serde_json::to_vec(&json!({"type":"user","uuid":"direct","message":{"content":"prompt"}}))
            .unwrap();
    raw.push(b'\n');
    std::fs::write(&path, &raw).unwrap();
    let source = SourceId {
        session_id: "native".into(),
        path,
        dev: 0,
        ino: 0,
    };
    let hash = hex::encode(Sha256::digest(&raw));
    let mut cache = Cache::default();
    CENSUS_READS.with(|count| count.set(0));
    assert!(cache.read(&source, raw.len() as u64, &hash).1.is_some());
    assert_eq!(CENSUS_READS.with(|count| count.get()), 1);
    assert!(cache.read(&source, raw.len() as u64, &hash).1.is_some());
    assert_eq!(CENSUS_READS.with(|count| count.get()), 2);
    let injection = fail_once(Stage::Read);
    assert!(cache.read(&source, raw.len() as u64, &hash).1.is_none());
    assert_eq!(CENSUS_READS.with(|count| count.get()), 3);
    drop(injection);
    assert!(cache.read(&source, raw.len() as u64, &hash).1.is_some());
    assert_eq!(CENSUS_READS.with(|count| count.get()), 4);
}
