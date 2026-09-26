//! Custody status contracts over ledgers as the boot copier leaves them, read back from disk.

use super::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs;

/// A custody root holding one `claude` episode for a transcript beside it.
struct Fx(tempfile::TempDir);

impl Fx {
    fn new(transcript: &[u8]) -> Self {
        let fx = Self(tempfile::tempdir().unwrap());
        fs::create_dir_all(fx.episode()).unwrap();
        let marker = json!({ "episode": { "channel_id": 7 }, "tui_direct": true });
        fs::write(fx.episode().join("episode.json"), marker.to_string()).unwrap();
        fs::write(fx.source(), transcript).unwrap();
        fx
    }

    fn episode(&self) -> PathBuf {
        self.0.path().join("custody/claude/e1")
    }

    fn source(&self) -> PathBuf {
        self.0.path().join("t.jsonl")
    }

    /// The source as a copier observes it now.
    fn seen(&self) -> Value {
        let bytes = fs::read(self.source()).unwrap();
        let (dev, ino) = identity(&fs::metadata(self.source()).unwrap()).unwrap();
        let head = &bytes[..bytes.len().min(64 << 10)];
        let sha = format!("{:x}", Sha256::digest(head));
        json!({ "dev": dev, "ino": ino, "size": bytes.len(), "head_len": head.len(),
            "head_sha256": sha })
    }

    fn write(&self, rev: u32, file: &str, content: &[u8]) {
        let dir = self.episode().join(format!("rev-{rev:04}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(file), content).unwrap();
    }

    /// Writes `{"sources": [record]}` for this source, as intent and outcome files hold it.
    fn record(&self, rev: u32, file: &str, mut record: Value) {
        record["source"] = json!(self.source());
        let records = json!({ "sources": [record] }).to_string();
        self.write(rev, file, records.as_bytes());
    }

    fn manifest(&self, rev: u32, entries: Value) {
        let manifest = json!({ "entries": entries }).to_string();
        self.write(rev, "manifest.json", manifest.as_bytes());
    }

    /// A transcript entry shaped like the boot copier's, with `patch` over a plain attempt.
    fn entry(&self, offset: u64, patch: Value) -> Value {
        let mut entry = self.seen();
        (entry["kind"], entry["offset"]) = (json!("transcript"), json!(offset));
        entry["source"] = json!(self.source());
        for (key, value) in patch.as_object().unwrap() {
            entry[key] = value.clone();
        }
        entry
    }

    fn legacy(&self, rev: u32, offset: u64, patch: Value) {
        self.manifest(rev, json!([self.entry(offset, patch)]));
    }

    /// A copy of source bytes `[from, to)` for a turn at `offset`, beside its row's copy.
    fn held(&self, rev: u32, offset: u64, (from, to): (u64, u64)) {
        let bytes = fs::read(self.source()).unwrap()[from as usize..to as usize].to_vec();
        self.write(rev, "c", &bytes);
        self.write(rev, "0-row.json", b"{}");
        let sha = format!("{:x}", Sha256::digest(b"{}"));
        let row = json!({ "kind": "row", "source": "r", "sha256": sha, "copy": "0-row.json" });
        let transcript = self.entry(offset, json!({ "copy": "c", "from": from, "to": to }));
        self.manifest(rev, json!([row, transcript]));
    }

    fn status(&self) -> EpisodeStatus {
        episode_status(&self.episode())
    }
}

const BYTES: &[u8; 28] = b"aaaabbbbccccddddeeeeffffgggg";

fn truncate(path: &Path, len: u64) {
    let file = fs::File::options().write(true).open(path).unwrap();
    file.set_len(len).unwrap();
}

/// Episode flags, then the first source's flags (both sorted), `current` and missing ranges.
type Flags = &'static [&'static str];
type Want = (Flags, Flags, &'static str, &'static [(u64, u64)]);

/// Folds the ledger `setup` leaves beside the 28-byte transcript; no shape reads as complete.
fn check(name: &str, (episode, flags, current, missing): Want, setup: impl Fn(&Fx)) {
    let fx = Fx::new(BYTES);
    setup(&fx);
    let status = fx.status();
    assert!(!status.complete(), "{name}: {status:?}");
    assert!(status.flags.iter().eq(episode), "{name}: {status:?}");
    let Some(got) = status.sources.first() else {
        return assert!(current.is_empty(), "{name}: {status:?}");
    };
    assert!(got.flags.iter().eq(flags), "{name}: {got:?}");
    let got = (got.current.as_str(), &got.missing[..]);
    assert_eq!(got, (current, missing), "{name}");
}

// Contract: each ledger shape folds to the state the append-only single-generation model gives
// it, and none of them reads as complete: change evidence, unresolved ledger, gap, bad range.
#[test]
#[rustfmt::skip]
fn ledger_shapes_fold_to_their_obligation_state() {
    check("shrink beside a torn observation", (&[HISTORY], &[CHANGED], CHANGED, &[]), |fx| {
        fx.held(0, 0, (0, 28));
        fx.legacy(1, 0, json!({ "size": 20, "source_changed": true, "dev": null }));
    });
    // An attempt that copied `[20, 28)` and ended with `result`.
    let failed = |fx: &Fx, result: &str| {
        (fx.held(0, 0, (0, 20)), fx.write(1, "c", &BYTES[20..]));
        let rec = json!({ "result": result, "post": fx.seen(), "copy": "c", "from": 20, "to": 28 });
        fx.record(1, "outcome.json", rec);
        fx.record(1, "intent.json", json!({ "required_from": 0, "pre": fx.seen() }));
    };
    check("a failed post-copy check", (&[], &[CHANGED], CHANGED, &[(20, 28)]),
        |fx| failed(fx, "verify_failed"));
    // The manifest claims the copy its own attempt's outcome reports as failed.
    check("failed attempt copy", (&[HISTORY], &[], "missing readable from 20", &[(20, 28)]), |fx| {
        (failed(fx, "copy_failed"), fx.legacy(1, 0, json!({ "copy": "c", "from": 20, "to": 28 })));
    });
    check("an unreadable turn start", (&[], &[START], "missing readable from 0", &[(0, 28)]),
        |fx| fx.legacy(0, 0, json!({ "offset": null })));
    check("torn copy result", (&[HISTORY], &[], "missing readable from 0", &[(0, 28)]), |fx| {
        fx.write(0, "c", BYTES);
        fx.legacy(0, 0, json!({ "copy": "c", "from": 0, "to": 28, "error": 7 }));
    });
    for (name, row) in [("row copy gone", None), ("row copy altered", Some(b"{ }"))] {
        check(name, (&["bytes_copy_failed"], &[], "complete_to_eof", &[]), |fx| {
            let (_, copy) = (fx.held(0, 0, (0, 28)), fx.episode().join("rev-0000/0-row.json"));
            row.map_or_else(|| fs::remove_file(&copy), |row| fs::write(&copy, row)).unwrap();
        });
    }
    check("open failed first", (&[], &["first_seen_after_failure"], "complete_to_eof", &[]), |fx| {
        let failure = json!({ "kind": "transcript", "source": fx.source(), "offset": 4,
            "error": "NotFound" });
        (fx.manifest(0, json!([failure])), fx.held(1, 4, (4, 28)));
    });
    check("later unpublished rev", (&["inventory_unresolved"], &[], "complete_to_eof", &[]),
        |fx| _ = (fx.held(0, 0, (0, 28)), fx.write(1, "c", b"partial")));
    check("marker with no revision", (&["inventory_unresolved"], &[], "", &[]), |_| {});
    check("torn manifest", (&[HISTORY], &[], "", &[]), |fx| fx.write(0, "manifest.json", b"{"));
    // The copy of an attempt whose outcome is unreadable is not held.
    check("torn outcome", (&["fairness_lost", HISTORY], &[], "missing readable from 0", &[(0, 28)]),
        |fx| _ = (fx.held(0, 0, (0, 28)), fx.write(0, "outcome.json", b"{")));
    check("short copy leaves a gap", (&[], &[], "missing readable from 12", &[(12, 20)]), |fx| {
        (fx.held(0, 4, (4, 12)), fx.held(2, 4, (20, 28)), fx.write(1, "c", b"bad"));
        fx.legacy(1, 4, json!({ "copy": "c", "from": 12, "to": 20 }));
    });
    check("requirement past EOF", (&[], &["required_past_eof"], "complete_to_eof", &[]),
        |fx| fx.legacy(0, 50, json!({ "error": "turn start is past EOF" })));
    check("cap from the first missing byte", (&[], &[], "over_cap from 10", &[(10, 20)]), |fx| {
        (truncate(&fx.source(), 10), fx.held(0, 0, (0, 10)));
        (truncate(&fx.source(), (64 << 20) + 11), fx.write(1, "d", b""));
        truncate(&fx.episode().join("rev-0001/d"), (64 << 20) - 9);
        fx.legacy(1, 0, json!({ "copy": "d", "from": 20, "to": (64 << 20) + 11 }));
    });
    check("a head rewrite an attempt saw is sticky", (&[], &[CHANGED], CHANGED, &[]), |fx| {
        let (_, mut pre) = (fx.held(0, 0, (0, 28)), fx.seen());
        pre["g_prefix_sha"] = json!("0".repeat(64));
        fx.record(1, "intent.json", json!({ "required_from": 0, "pre": pre }));
    });
}

// Contract: intent and outcome records keep what an attempt saw (post-copy EOF, an unfinished
// attempt's lower requirement), and a deferred attempt is the last attempt beside `current`.
#[test]
fn attempt_records_keep_their_observations_across_a_reload() {
    let fx = Fx::new(BYTES);
    let mut pre = fx.seen();
    pre["size"] = json!(20);
    fx.record(0, "intent.json", json!({ "required_from": 4, "pre": pre }));
    fx.write(0, "c", &BYTES[4..20]);
    let ok = json!({ "result": "ok", "post": fx.seen(), "copy": "c", "from": 4, "to": 20 });
    fx.record(0, "outcome.json", ok);
    fx.record(1, "intent.json", json!({ "required_from": 2, "pre": null }));
    // The manifest copy of an attempt that has no outcome yet is not held.
    fx.write(1, "c", &BYTES[20..]);
    fx.legacy(1, 2, json!({ "copy": "c", "from": 20, "to": 28 }));
    let source = fx.status().sources.remove(0);
    let got = (source.max_eof, source.missing, source.last_attempt.unwrap());
    let incomplete = "rev-0001: incomplete (no outcome)".to_string();
    assert_eq!(got, (28, vec![(2, 4), (20, 28)], incomplete));

    let deferred = json!({ "result": "deferred_budget", "from": 2, "to": 28 });
    fx.record(2, "intent.json", json!({ "required_from": 2, "pre": null }));
    fx.record(2, "outcome.json", deferred);
    let source = fx.status().sources.remove(0);
    assert_eq!(source.current, "missing readable from 2");
    assert_eq!(source.last_attempt.unwrap(), "rev-0002: deferred_budget");

    truncate(&fx.source(), 24);
    assert_eq!(fx.status().sources[0].current, "source_changed_now");
}

// Contract: `current` checks the file's identity, size, head and last preserved 4 KiB, reading no
// more; a change there is reported and stays once recorded, one outside those windows is not.
#[test]
fn current_reads_only_the_head_and_the_last_preserved_window() {
    let bytes: Vec<u8> = (0..80 << 10).map(|i| b'a' + (i % 26) as u8).collect();
    for (at, current) in [
        (10, "source_changed_now"),
        ((80 << 10) - 10, "source_changed_now"),
        (70 << 10, "complete_to_eof"),
    ] {
        let fx = Fx::new(&bytes);
        fx.held(0, 0, (0, 80 << 10));
        let mut rewritten = bytes.clone();
        rewritten[at] = b'#';
        fs::write(fx.source(), &rewritten).unwrap();
        let status = fx.status();
        let source = &status.sources[0];
        let got = (source.current.as_str(), source.verify_read);
        assert_eq!(got, (current, (64 << 10) + (8 << 10)));
        assert_eq!(status.complete(), current == "complete_to_eof");
    }
    for replaced in [true, false] {
        let fx = Fx::new(&bytes);
        let other = fx.0.path().join("new");
        fx.held(0, 0, (0, 70 << 10));
        match replaced {
            true => fs::write(&other, &bytes).and_then(|()| fs::rename(&other, fx.source())),
            false => Ok(truncate(&fx.source(), 75 << 10)),
        }
        .unwrap();
        assert_eq!(fx.status().sources[0].current, "source_changed_now");
        fx.legacy(1, 0, json!({}));
        assert_eq!(fx.status().sources[0].current, CHANGED, "{replaced}");
    }
}

// Contract: the report shows the current state apart from the last attempt, names the file now
// at the source path, warns past a stray root file, and leaves the custody tree as it was.
#[test]
fn the_status_report_separates_current_from_last_attempt_and_writes_nothing() {
    let fx = Fx::new(BYTES);
    fx.held(0, 4, (4, 12));
    fx.legacy(1, 4, json!({ "error": "boot copy budget exhausted" }));
    let (custody, stray) = (
        fx.0.path().join("custody"),
        fx.0.path().join("custody/.DS_Store"),
    );
    fs::write(&stray, b"").unwrap();
    let before = tree(&custody);
    let report = status_report(&custody, None, None).unwrap();
    let (dev, ino) = identity(&fs::metadata(fx.source()).unwrap()).unwrap();
    for line in [
        format!("skipped non-directory {}", stray.display()),
        "current: missing readable from 12".to_string(),
        "last_attempt: rev-0001: boot copy budget exhausted".to_string(),
        format!("now: (dev, ino)=({dev}, {ino}) size=28"),
    ] {
        assert!(report.contains(&line), "{report}");
    }
    assert_eq!(tree(&custody), before);
}

/// Every path under `path`, empty directories included, with a file's bytes.
fn tree(path: &Path) -> std::collections::BTreeMap<PathBuf, Option<Vec<u8>>> {
    let mut tree = std::collections::BTreeMap::from([(path.into(), fs::read(path).ok())]);
    for entry in fs::read_dir(path).into_iter().flatten() {
        tree.append(&mut self::tree(&entry.unwrap().path()));
    }
    tree
}
