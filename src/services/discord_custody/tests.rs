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

    fn custody(&self) -> PathBuf {
        self.0.path().join("custody")
    }

    fn episode(&self) -> PathBuf {
        self.custody().join("claude").join("e1")
    }

    fn source(&self) -> PathBuf {
        self.0.path().join("t.jsonl")
    }

    /// The source as a copier observes it now.
    fn seen(&self) -> Value {
        let bytes = fs::read(self.source()).unwrap();
        let (dev, ino) = identity(&fs::metadata(self.source()).unwrap());
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

    /// A one-entry manifest shaped like the boot copier's, with `patch` over a plain attempt.
    fn legacy(&self, rev: u32, offset: u64, patch: Value) {
        let mut entry = self.seen();
        (entry["kind"], entry["offset"]) = (json!("transcript"), json!(offset));
        entry["source"] = json!(self.source());
        for (key, value) in patch.as_object().unwrap() {
            entry[key] = value.clone();
        }
        let manifest = json!({ "entries": [entry] }).to_string();
        self.write(rev, "manifest.json", manifest.as_bytes());
    }

    /// A successful copy of source bytes `[from, to)` for a turn starting at `offset`.
    fn held(&self, rev: u32, offset: u64, (from, to): (u64, u64)) {
        let bytes = fs::read(self.source()).unwrap()[from as usize..to as usize].to_vec();
        self.write(rev, "c", &bytes);
        self.legacy(rev, offset, json!({ "copy": "c", "from": from, "to": to }));
    }

    fn status(&self) -> EpisodeStatus {
        let mut episodes = provider_status(&self.custody().join("claude")).unwrap();
        episodes.remove(0)
    }
}

const BYTES: &[u8; 28] = b"aaaabbbbccccddddeeeeffffgggg";
const INVENTORY: &str = "inventory_unresolved";
const HISTORY: &str = "history_unresolved";

fn set(flags: &[&'static str]) -> BTreeSet<&'static str> {
    flags.iter().copied().collect()
}

fn truncate(path: &Path, len: u64) {
    let file = fs::File::options().write(true).open(path).unwrap();
    file.set_len(len).unwrap();
}

/// Episode flags, then the first source's flags, `current` and missing ranges.
type Flags = &'static [&'static str];
type Want = (Flags, Flags, &'static str, &'static [(u64, u64)]);

// Contract: each ledger shape folds to the state the append-only single-generation model gives
// it, and none of them reads as complete: change evidence, unresolved ledger, gap, bad range.
#[test]
fn ledger_shapes_fold_to_their_obligation_state() {
    let rows: [(&str, fn(&Fx), Want); 12] = [
        (
            "shrink evidence is sticky",
            |fx| {
                fx.held(0, 0, (0, 28));
                fx.legacy(1, 0, json!({ "size": 20, "source_changed": true }));
            },
            (&[], &[CHANGED], CHANGED, &[]),
        ),
        (
            "a failed post-copy check is a change and its copy is not held",
            |fx| {
                (fx.held(0, 0, (0, 20)), fx.write(1, "c", &BYTES[20..]));
                let mut failed =
                    json!({ "result": "verify_failed", "copy": "c", "from": 20, "to": 28 });
                failed["post"] = fx.seen();
                fx.record(1, "outcome.json", failed);
                let intent = json!({ "required_from": 0, "pre": fx.seen() });
                fx.record(1, "intent.json", intent);
            },
            (&[], &[CHANGED], CHANGED, &[(20, 28)]),
        ),
        (
            "copied after an identity-less failure",
            |fx| {
                fx.legacy(0, 4, json!({ "dev": null, "error": "NotFound" }));
                fx.held(1, 4, (4, 28));
            },
            (&[], &["first_seen_after_failure"], "complete_to_eof", &[]),
        ),
        (
            "unpublished revision after a published one",
            |fx| {
                fx.held(0, 0, (0, 28));
                fx.write(1, "c", b"partial");
            },
            (&[INVENTORY], &[], "complete_to_eof", &[]),
        ),
        (
            "marker with no revision",
            |_| {},
            (&[INVENTORY], &[], "", &[]),
        ),
        (
            "manifest not JSON",
            |fx| fx.write(0, "manifest.json", b"{torn"),
            (&[HISTORY], &[], "", &[]),
        ),
        (
            "outcome not JSON",
            |fx| {
                fx.held(0, 0, (0, 28));
                fx.write(0, "outcome.json", b"{torn");
            },
            (&[HISTORY, "fairness_lost"], &[], "complete_to_eof", &[]),
        ),
        (
            "short copy leaves an internal gap",
            |fx| {
                (fx.held(0, 4, (4, 12)), fx.held(2, 4, (20, 28)));
                fx.write(1, "c", b"bad");
                fx.legacy(1, 4, json!({ "copy": "c", "from": 12, "to": 20 }));
            },
            (&[], &[], "missing readable from 12", &[(12, 20)]),
        ),
        (
            "requirement past EOF",
            |fx| {
                fx.legacy(0, 50, json!({ "error": "turn start is past EOF" }));
            },
            (&[], &["required_past_eof"], "complete_to_eof", &[]),
        ),
        (
            "copy cap from the first missing byte, not the missing total",
            |fx| {
                fx.held(0, 0, (0, 10));
                truncate(&fx.source(), (64 << 20) + 11);
                fx.write(1, "d", b"");
                truncate(&fx.episode().join("rev-0001/d"), (64 << 20) - 9);
                let far = json!({ "copy": "d", "from": 20, "to": (64 << 20) + 11 });
                fx.legacy(1, 0, far);
            },
            (&[], &[], "over_cap from 10", &[(10, 20)]),
        ),
        (
            "a copy whose attempt failed is not held",
            |fx| {
                (fx.held(0, 0, (0, 20)), fx.write(1, "c", &BYTES[20..]));
                let failed = json!({ "result": "copy_failed", "copy": "c", "from": 20, "to": 28 });
                fx.record(1, "outcome.json", failed);
                let intent = json!({ "required_from": 0, "pre": fx.seen() });
                fx.record(1, "intent.json", intent);
            },
            (&[], &[], "missing readable from 20", &[(20, 28)]),
        ),
        (
            "a head rewrite an attempt saw is sticky",
            |fx| {
                fx.held(0, 0, (0, 28));
                let mut pre = fx.seen();
                pre["g_prefix_sha"] = json!("0".repeat(64));
                fx.record(1, "intent.json", json!({ "required_from": 0, "pre": pre }));
            },
            (&[], &[CHANGED], CHANGED, &[]),
        ),
    ];
    for (name, setup, (episode, flags, current, missing)) in rows {
        let fx = Fx::new(&BYTES[..if name.contains("copy cap") { 10 } else { 28 }]);
        setup(&fx);
        let status = fx.status();
        assert!(!status.complete(), "{name}: {status:?}");
        assert_eq!(status.flags, set(episode), "{name}");
        let Some(got) = status.sources.first() else {
            assert!(current.is_empty(), "{name}: {status:?}");
            continue;
        };
        let want = (set(flags), current, missing.to_vec());
        let got = (got.flags.clone(), got.current.as_str(), got.missing.clone());
        assert_eq!(got, want, "{name}");
    }
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
    let source = fx.status().sources.remove(0);
    let got = (source.max_eof, source.missing, source.last_attempt.unwrap());
    let incomplete = "rev-0001: incomplete (no outcome)".to_string();
    assert_eq!(got, (28, vec![(2, 4), (20, 28)], incomplete));

    let deferred = json!({ "result": "deferred_budget", "from": 2, "to": 28 });
    fx.record(1, "outcome.json", deferred);
    let source = fx.status().sources.remove(0);
    assert_eq!(source.current, "missing readable from 2");
    assert_eq!(source.last_attempt.unwrap(), "rev-0001: deferred_budget");

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

// Contract: the status report shows the current state apart from the last attempt, names the
// file now at the source path, and leaves every custody file as it found it.
#[test]
fn the_status_report_separates_current_from_last_attempt_and_writes_nothing() {
    let fx = Fx::new(BYTES);
    fx.held(0, 4, (4, 12));
    fx.legacy(1, 4, json!({ "error": "boot copy budget exhausted" }));
    let before = tree(&fx.custody());
    let report = status_report(&fx.custody(), Some("claude"), None).unwrap();
    let (dev, ino) = identity(&fs::metadata(fx.source()).unwrap());
    for line in [
        "current: missing readable from 12".to_string(),
        "last_attempt: rev-0001: boot copy budget exhausted".to_string(),
        format!("now: (dev, ino)=({dev}, {ino}) size=28"),
    ] {
        assert!(report.contains(&line), "{report}");
    }
    assert_eq!(tree(&fx.custody()), before);
}

fn tree(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let (mut files, entries) = (Vec::new(), fs::read_dir(dir).unwrap().flatten());
    for path in entries.map(|entry| entry.path()) {
        match path.is_dir() {
            true => files.extend(tree(&path)),
            false => files.push((path.clone(), fs::read(&path).unwrap())),
        }
    }
    files.sort();
    files
}
