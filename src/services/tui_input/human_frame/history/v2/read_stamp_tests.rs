use super::super::native::{Disposition, EvidenceRegistration, barrier::Barrier};
use super::super::native_tests::Fixture;
use super::*;
use std::io::Write;

const NS: &str = "native-fixture";
const FRAME: &str = "한국어 원본 입력";

fn scratch() -> (crate::config::TestRuntimeRootGuard, tempfile::TempDir) {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let root = runtime.path().join("runtime");
    std::fs::create_dir_all(&root).unwrap();
    let dir = tempfile::tempdir_in(root).unwrap();
    (runtime, dir)
}

fn later() -> SystemTime {
    SystemTime::now() + Duration::from_secs(10)
}

fn stat(path: &Path) -> Fingerprint {
    Fingerprint::of(&std::fs::metadata(path).unwrap()).unwrap()
}

/// A fixture history, the cache that keeps its stamp under a later clock, and its bytes.
fn kept(dir: &Path, channel: u64) -> (PathBuf, Cache, String) {
    let path = dir.join(format!("{channel}.json"));
    Fixture::at(path.clone(), channel, true);
    let mut cache = Cache::default();
    for _ in 0..2 {
        cache.refresh(&path, later(), fstat).unwrap();
    }
    assert_eq!(cache.reads, 1, "a settled stamp is kept");
    let raw = cache.raw().unwrap().to_string();
    (path, cache, raw)
}

/// Returns once a write gets a later change time than `path` has, whatever the clock tick.
fn tick_past(path: &Path) {
    let (probe, deadline) = (path.with_extension("tick"), SystemTime::now() + SETTLED);
    while {
        std::fs::write(&probe, "").unwrap();
        stat(&probe).change <= stat(path).change
    } {
        assert!(SystemTime::now() < deadline, "ctime never advanced");
    }
}

/// Rewrites the same inode with `new` and restores its mtime, leaving only length and ctime.
fn rewritten(path: &Path, new: &str) {
    let old = stat(path);
    let mut file = std::fs::File::options()
        .write(true)
        .truncate(true)
        .open(path)
        .unwrap();
    file.write_all(new.as_bytes()).unwrap();
    file.set_modified(old.modified).unwrap();
    let now = stat(path);
    assert_eq!((now.ino, now.modified), (old.ino, old.modified));
    assert_eq!(now.len == old.len, new.len() as u64 == old.len);
    assert_ne!(now.change, old.change);
}

/// Puts `new` under the path as another inode with the old length and mtime. `unlinked` removes
/// the old file first while a descriptor still holds its inode number.
fn swapped(path: &Path, new: &str, unlinked: bool) {
    let old = stat(path);
    let held = std::fs::File::open(path).unwrap();
    let next = match unlinked {
        true => path.to_path_buf(),
        false => path.with_extension("next"),
    };
    if unlinked {
        std::fs::remove_file(path).unwrap();
    }
    std::fs::write(&next, new).unwrap();
    let file = std::fs::File::options().write(true).open(&next).unwrap();
    file.set_modified(old.modified).unwrap();
    std::fs::rename(&next, path).unwrap();
    let now = stat(path);
    assert_eq!((now.len, now.modified), (old.len, old.modified));
    assert_ne!(now.ino, old.ino);
    drop(held);
}

/// Receipts the fixture's own row, keys it and stores a barrier behind that row.
fn bar(f: &Fixture) -> Barrier {
    let transcript = &f.ticket.source.as_ref().unwrap().transcript;
    let receipt = (transcript.as_path(), 0, "user", Some("own"), FRAME);
    assert_eq!(f.history.receipt(NS, &f.ticket.key, receipt), Some(true));
    f.history.note_native_any_key(NS, &f.ticket).unwrap();
    let mut seen = f.history.native_classification_registrations().unwrap();
    seen.retain(|r| r.ticket.channel_id == f.ticket.channel_id);
    let barrier: Barrier = serde_json::from_value(json!({"namespace":NS,"ticket":f.ticket,
        "membership_range":[0,4],"membership_row":"own","foreign_range":[4,8],"foreign_row":"stop",
        "foreign_hashes":["foreign"],"through":8,
        "registrations_fingerprint":native::registrations_fingerprint(&seen).unwrap()}))
    .unwrap();
    assert!(f.history.record_native_barrier(&barrier).unwrap());
    barrier
}

#[test]
fn a_stamp_settles_on_both_times_and_stands_only_on_every_field() {
    let at = |secs: u64| SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
    // (mtime, ctime, kept) at second 102: a backdated mtime under a fresh ctime is not trusted.
    for (modified, change, settles) in [(100, 100, true), (50, 101, false), (101, 50, false)] {
        let stamp = Fingerprint {
            len: 9,
            dev: 1,
            ino: 2,
            modified: at(modified),
            change: (change, 0),
        };
        assert_eq!(settled(&stamp, at(102)), settles, "{modified} {change}");
    }
    let (_runtime, dir) = scratch();
    let (path, mut cache, _) = kept(dir.path(), 83850001);
    let fields: [fn(&mut Fingerprint); 5] = [
        |stamp| stamp.len += 1,
        |stamp| stamp.dev += 1,
        |stamp| stamp.ino += 1,
        |stamp| stamp.modified += Duration::from_nanos(1),
        |stamp| stamp.change.1 += 1,
    ];
    for (at, differ) in fields.into_iter().enumerate() {
        differ(cache.kept.as_mut().unwrap().stamp.as_mut().unwrap());
        cache.refresh(&path, later(), fstat).unwrap();
        assert_eq!(cache.reads, 2 + at, "a stamp differing in field {at} alone");
    }
}

#[test]
fn history_is_read_until_it_settles_and_then_not_again() {
    let (_runtime, dir) = scratch();
    let path = dir.path().join("frames.json");
    Fixture::at(path.clone(), 83850002, true);
    let written = std::fs::metadata(&path).unwrap().modified().unwrap();
    let mut cache = Cache::default();
    for _ in 0..3 {
        cache.refresh(&path, written, fstat).unwrap();
    }
    assert_eq!(cache.reads, 3, "a write this fresh could repeat its stamp");
    cache.refresh(&path, later(), fstat).unwrap();
    for _ in 0..100 {
        cache.refresh(&path, written, fstat).unwrap();
        assert_eq!(cache.evidence(true, None).unwrap().len(), 1);
        assert_eq!(cache.evidence(false, None).unwrap().len(), 1);
        assert!(cache.state().is_some());
    }
    assert_eq!((cache.reads, cache.derived), (4, 2));
}

#[test]
fn every_change_under_a_kept_stamp_is_read_again() {
    type Change = fn(&Path, &str);
    let cases: [(&str, &str, Change); 4] = [
        ("truncated and rewritten, mtime restored", "nf", rewritten),
        (
            "same length in place, mtime restored",
            "native-fixturX",
            rewritten,
        ),
        ("renamed over", "native-fixturX", |path, new| {
            swapped(path, new, false)
        }),
        (
            "removed and created again",
            "native-fixturX",
            |path, new| swapped(path, new, true),
        ),
    ];
    let (_runtime, dir) = scratch();
    for (at, (case, namespace, change)) in cases.into_iter().enumerate() {
        let (path, mut cache, raw) = kept(dir.path(), 83850010 + at as u64);
        assert_eq!(cache.evidence(true, None).unwrap()[0].namespace, NS);
        let new = raw.replace(NS, namespace);
        tick_past(&path);
        change(&path, &new);
        cache.refresh(&path, later(), fstat).unwrap();
        assert_eq!(
            (cache.reads, cache.raw()),
            (2, Some(new.as_str())),
            "{case}"
        );
        assert_eq!(
            cache.evidence(true, None).unwrap()[0].namespace,
            namespace,
            "{case}"
        );
    }
}

#[test]
fn a_change_during_the_read_is_not_kept_under_the_new_stamp() {
    type Change = fn(&Path, &str);
    let cases: [(&str, Change); 2] = [
        ("renamed over", |path, new| swapped(path, new, false)),
        ("rewritten in place, mtime restored", |path, new| {
            tick_past(path);
            rewritten(path, new)
        }),
    ];
    let (_runtime, dir) = scratch();
    for (at, (case, change)) in cases.into_iter().enumerate() {
        let (path, _, raw) = kept(dir.path(), 83850030 + at as u64);
        let new = raw.replace(NS, "native-fixturX");
        let (mut cache, mut stamps) = (Cache::default(), 0);
        let changing = |file: &std::fs::File| {
            stamps += 1;
            if stamps == 2 {
                change(&path, &new);
            }
            fstat(file)
        };
        cache.refresh(&path, later(), changing).unwrap();
        assert_eq!(cache.raw(), Some(raw.as_str()), "{case}: the old bytes");
        cache.refresh(&path, later(), fstat).unwrap();
        assert_eq!(cache.raw(), Some(new.as_str()), "{case}");
    }
}

#[test]
fn a_stamp_that_cannot_be_taken_still_yields_the_history_and_is_not_kept() {
    let (_runtime, dir) = scratch();
    let (path, _, raw) = kept(dir.path(), 83850040);
    for failing in [1, 2] {
        let (mut cache, mut stamps) = (Cache::default(), 0);
        let failed = |file: &std::fs::File| {
            stamps += 1;
            match stamps == failing {
                true => Err(std::io::Error::other("no stamp")),
                false => fstat(file),
            }
        };
        cache.refresh(&path, later(), failed).unwrap();
        assert_eq!((cache.reads, cache.raw()), (1, Some(raw.as_str())));
        for _ in 0..2 {
            cache.refresh(&path, later(), fstat).unwrap();
        }
        assert_eq!(cache.reads, 2, "stamp {failing} failed: read once more");
    }
}

#[test]
fn warm_views_follow_receipt_and_barrier_writes() {
    let (_runtime, dir) = scratch();
    let f = Fixture::at(dir.path().join("frames.json"), 83850004, true);
    f.history.note_native_any_key(NS, &f.ticket).unwrap();
    let view = || {
        let mut seen = f.history.native_classification_registrations().unwrap();
        seen.remove(0)
    };
    assert!(view().receipts.is_empty() && view().candidate);
    let stamp = native::registrations_fingerprint(&[view()]).unwrap();
    f.history
        .try_native_receipt(
            NS,
            &f.ticket,
            ("user", "row", (0, 4), 4, &hash(FRAME)),
            &stamp,
        )
        .unwrap();
    assert_eq!(view().receipts, ["row"], "a control transaction's write");
    let barrier = bar(&f);
    let barred = view();
    assert!(!barred.candidate && barred.barrier == Some(barrier));
}

#[test]
fn a_stored_barrier_is_validated_on_every_call_beside_kept_registrations() {
    let (_runtime, dir) = scratch();
    let path = dir.path().join("frames.json");
    let barred = Fixture::at(path.clone(), 83850005, true);
    bar(&barred);
    Fixture::at(path, 83850006, true);
    let transcript = barred.ticket.source.as_ref().unwrap().transcript.clone();
    let aside = transcript.with_extension("aside");
    let derived = || barred.history.cached.lock().unwrap().derived;
    let mut seen = Vec::new();
    for gone in [false, false, true, false] {
        if gone {
            std::fs::rename(&transcript, &aside).unwrap();
        }
        let before = derived();
        let view = barred.history.native_classification_registrations();
        seen.push((view.ok().map(|view| view.len()), derived() - before));
        if gone {
            std::fs::rename(&aside, &transcript).unwrap();
        }
    }
    // The barred registration is derived on every call; the other one only on the first.
    assert_eq!(seen, [(Some(2), 2), (Some(2), 1), (None, 1), (Some(2), 1)]);
}

#[test]
fn kept_views_equal_fresh_results_in_both_modes_and_orders() {
    let (_runtime, dir) = scratch();
    let path = dir.path().join("frames.json");
    let barred = Fixture::at(path.clone(), 83850007, true);
    bar(&barred);
    let settled = Fixture::at(path.clone(), 83850008, false);
    let outcome = Fixture::outcome(Disposition::Executed);
    let history = &settled.history;
    assert!(
        history
            .record_native_outcome(NS, &settled.ticket, &outcome)
            .unwrap()
    );
    let open = Fixture::at(path.clone(), 83850009, true);
    let spell = |seen: Result<Vec<EvidenceRegistration>, String>| {
        seen.map(|seen| {
            let all = |r: &EvidenceRegistration| {
                json!([r, r.exact_frame_hash, r.candidate, r.not_sent, r.barrier])
            };
            seen.iter().map(all).collect::<Vec<_>>()
        })
    };
    let check = |lens: Option<[usize; 2]>| {
        for order in [[true, false], [false, true]] {
            let history = Registry::at(path.clone());
            for classify in order.into_iter().chain(order) {
                let kept = spell(match classify {
                    true => history.native_classification_registrations(),
                    false => history.native_evidence_registrations(),
                });
                let fresh = history.inspect(|state| evidence_registrations(state, classify));
                assert_eq!(kept, spell(fresh), "classify={classify} order={order:?}");
                let len = kept.ok().map(|kept| kept.len());
                assert_eq!(len, lens.map(|lens| lens[usize::from(classify)]));
            }
        }
    };
    check(Some([2, 3]));
    let mut broken = open.history.json().unwrap();
    broken["live"][2]["registration"]["native_settlement"]["ticket"]["channel_id"] = json!(0);
    std::fs::write(&path, broken.to_string()).unwrap();
    check(None);
    std::fs::remove_file(&barred.ticket.source.as_ref().unwrap().transcript).unwrap();
    check(None);
}

#[test]
fn not_sent_is_classified_without_opening_a_native_candidate() {
    let (_runtime, dir) = scratch();
    let path = dir.path().join("frames.json");
    let f = Fixture::at(path.clone(), 83850050, true);
    f.history.try_native_not_sent(NS, &f.ticket).unwrap();
    let restarted = Registry::at(path);
    assert!(
        restarted
            .native_evidence_registrations()
            .unwrap()
            .is_empty()
    );
    let classified = restarted.native_classification_registrations().unwrap();
    assert_eq!(classified.len(), 1);
    assert!(!classified[0].candidate && classified[0].barrier.is_none());
    assert!(restarted.note_native_any_key(NS, &f.ticket).is_err());
}

#[test]
fn rejected_native_check_keeps_a_twelve_megabyte_history_warm() {
    let (_runtime, dir) = scratch();
    let path = dir.path().join("frames.json");
    let f = Fixture::at(path.clone(), 83850051, true);
    let mut synthetic = f.history.json().unwrap();
    synthetic["live"][0]["registration"]["native_settlement"]["fixture_padding"] =
        json!("a".repeat(12_000_000));
    std::fs::write(&path, synthetic.to_string()).unwrap();
    let history = Registry::at(path.clone());
    history
        .cached
        .lock()
        .unwrap()
        .refresh(&path, later(), fstat)
        .unwrap();
    let reads = || history.cached.lock().unwrap().reads;
    let before = reads();
    let error = history
        .record_native_outcome_at_stamp(
            NS,
            &f.ticket,
            &Fixture::outcome(Disposition::WithdrawnNotExecuted),
            "stale",
        )
        .unwrap_err();
    assert_eq!(error, "native registrations superseded");
    assert!(
        history
            .native_settlement(NS, &f.ticket)
            .unwrap()
            .outcome
            .is_none()
    );
    let bytes = std::fs::metadata(&path).unwrap().len();
    let tick_reads = reads() - before;
    eprintln!("synthetic history bytes={bytes} tick_reads={tick_reads}");
    assert_eq!(tick_reads, 0);
}

#[test]
fn native_durability_read_refreshes_the_cache_after_external_replacement() {
    let (_runtime, dir) = scratch();
    let path = dir.path().join("frames.json");
    let f = Fixture::at(path.clone(), 83850052, true);
    let mut cache = f.history.cached.lock().unwrap();
    cache.refresh(&path, later(), fstat).unwrap();
    let raw = cache.raw().unwrap().replace(NS, "native-fixturX");
    let before = cache.reads;
    drop(cache);
    let saved = f.history.try_native_settlement(NS, &f.ticket).unwrap();
    assert!(saved.outcome.is_none());
    assert_eq!(f.history.cached.lock().unwrap().reads, before);
    swapped(&path, &raw, false);
    assert_eq!(
        f.history.try_native_settlement(NS, &f.ticket).unwrap_err(),
        "native registration missing"
    );
    assert_eq!(f.history.cached.lock().unwrap().reads, before + 1);
}

#[test]
fn channel_views_derive_only_the_requested_channel_and_track_new_bytes() {
    let (_runtime, dir) = scratch();
    let path = dir.path().join("frames.json");
    let first = Fixture::at(path.clone(), 83850053, true);
    let second = Fixture::at(path.clone(), 83850054, true);
    let history = Registry::at(path.clone());
    history
        .cached
        .lock()
        .unwrap()
        .refresh(&path, later(), fstat)
        .unwrap();
    let initial = history.native_classification_revision().unwrap();
    history.transaction(|_| Ok(())).unwrap();
    assert_eq!(history.native_classification_revision().unwrap(), initial);
    assert_eq!(history.cached.lock().unwrap().reads, 1);
    let view = history
        .native_classification_for_channel(second.ticket.channel_id)
        .unwrap();
    assert_eq!(view.registrations.len(), 1);
    assert_eq!(view.registrations[0].ticket, second.ticket);
    assert_eq!(history.cached.lock().unwrap().derived, 1);
    assert_eq!(
        history.native_classification_revision().unwrap(),
        view.revision
    );
    first
        .history
        .try_native_not_sent(NS, &first.ticket)
        .unwrap();
    let changed = history.native_classification_revision().unwrap();
    assert_ne!(changed, view.revision);
    let first_channel = history
        .native_classification_for_channel(first.ticket.channel_id)
        .unwrap();
    assert_eq!(first_channel.revision, changed);
    assert!(!first_channel.registrations[0].candidate);
    assert_eq!(history.cached.lock().unwrap().derived, 2);
}
