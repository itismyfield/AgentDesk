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
type Want = (
    &'static [&'static str],
    &'static [&'static str],
    &'static str,
    &'static [(u64, u64)],
);

// Contract: each ledger shape folds to the state the append-only single-generation model gives
// it, and none of them reads as complete: change evidence, unresolved ledger, gap, bad range.
#[test]
fn ledger_shapes_fold_to_their_obligation_state() {
    let rows: [(&str, fn(&Fx), Want); 11] = [
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
            "entries not a list",
            |fx| {
                fx.write(0, "manifest.json", b"{\"entries\":5}");
            },
            (&[HISTORY], &[], "", &[]),
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
            "copy cap from the first missing byte",
            |fx| {
                fx.held(0, 0, (0, 10));
                truncate(&fx.source(), (64 << 20) + 11);
            },
            (&[], &[], "over_cap from 10", &[(10, (64 << 20) + 11)]),
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

// Contract: intent and outcome records keep what an attempt saw: the EOF seen after a copy
// catches a later shrink, an unfinished attempt keeps its lower requirement, and a deferred
// attempt is reported as the last attempt while `current` still reads the source.
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
    assert_eq!(
        (source.max_eof, source.missing),
        (28, vec![(2, 4), (20, 28)])
    );
    assert_eq!(
        source.last_attempt.unwrap(),
        "rev-0001: incomplete (no outcome)"
    );

    let deferred = json!({ "result": "deferred_budget", "from": 2, "to": 28 });
    fx.record(1, "outcome.json", deferred);
    let source = fx.status().sources.remove(0);
    assert_eq!(source.current, "missing readable from 2");
    assert_eq!(source.last_attempt.unwrap(), "rev-0001: deferred_budget");

    truncate(&fx.source(), 24);
    assert_eq!(fx.status().sources[0].current, "source_changed_now");
}

// Contract: `current` checks the head and the last preserved 4 KiB and reads no more than
// that; a rewrite inside those windows is reported, one outside them is not detected.
#[test]
fn current_reads_only_the_head_and_the_last_preserved_window() {
    let bytes: Vec<u8> = (0..80 << 10).map(|i| b'a' + (i % 26) as u8).collect();
    for (at, current) in [
        ((80 << 10) - 10, "source_changed_now"),
        (70 << 10, "complete_to_eof"),
    ] {
        let fx = Fx::new(&bytes);
        fx.held(0, 0, (0, 80 << 10));
        let mut rewritten = bytes.clone();
        rewritten[at] = b'#';
        fs::write(fx.source(), &rewritten).unwrap();
        let status = fx.status();
        let got = (
            status.sources[0].current.as_str(),
            status.sources[0].verify_read,
        );
        assert_eq!(got, (current, (64 << 10) + (8 << 10)));
        assert_eq!(status.complete(), current == "complete_to_eof");
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
    let mut files = Vec::new();
    for path in fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
    {
        match path.is_dir() {
            true => files.extend(tree(&path)),
            false => files.push((path.clone(), fs::read(&path).unwrap())),
        }
    }
    files.sort();
    files
}
