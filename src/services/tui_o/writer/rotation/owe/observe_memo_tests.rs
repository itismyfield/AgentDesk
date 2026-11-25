use super::*;
use crate::services::tui_o::shadow::capture::{Fingerprint, file_identity};
use crate::services::tui_o::shadow::{ShadowProvider, SourceId, SourceRange};
use crate::services::tui_o::store::spool::Cursor;
use crate::services::tui_o::writer::actor::native_input::Snapshot;
use crate::services::tui_o::writer::input_facts::native::terminal::Tail;
use crate::services::tui_o::writer::input_facts::native::tests::terminal_tests::{
    MARK, owned, stop, stored,
};
use crate::services::tui_o::writer::input_facts::native::tests::{Fixture, close, queue, user};
use crate::services::tui_o::writer::input_notices::NoticeOrigin;
use serde_json::json;
use sha2::Digest;

/// The seed authenticated over the transcript's first `through` bytes.
fn seed(f: &Fixture, through: usize) -> Seed {
    let bytes = std::fs::read(&f.binding.source.path).unwrap();
    let hash = hex::encode(sha2::Sha256::digest(&bytes[..through]));
    let reopened = Projection::reopen(f.binding.clone(), through as u64, &hash);
    Seed::new(reopened.unwrap().1)
}

fn whole(f: &Fixture) -> Seed {
    seed(
        f,
        std::fs::metadata(&f.binding.source.path).unwrap().len() as usize,
    )
}

fn fresh(seed: &Seed, registrations: &[Registration], raw: Option<&RawWitness>) -> Analyzed {
    let projection = seed.projection()?;
    let analysis = projection.analyze_closed(registrations, raw)?;
    let receipts = projection.receipts(registrations);
    Ok((analysis.evidence, analysis.barriers, receipts))
}

/// The memo answers as a fresh reduce does, and reduces only when `missed`.
fn same_as_fresh(
    seed: &Seed,
    registrations: &[Registration],
    raw: Option<&RawWitness>,
    missed: bool,
) {
    let before = seed.reduced();
    let kept = seed.analyzed(registrations, raw);
    assert_eq!(kept, fresh(seed, registrations, raw));
    assert_eq!(seed.reduced() - before, usize::from(missed));
}

#[test]
fn memo_equals_a_fresh_reduce_on_every_prefix_until_a_gap_ends_the_seed() {
    let f = Fixture::new();
    f.register("ours", "our input");
    f.register("other", "second input");
    f.append(&[
        queue("enqueue", "our input"),
        queue("dequeue", ""),
        user("own", "our input"),
        close(),
        queue("enqueue", "second input"),
        queue("remove", "second input"),
        close(),
    ]);
    let all = f.history.native_classification_registrations().unwrap();
    let mut flipped = all.clone();
    flipped[0].candidate = false;
    let reversed = all.iter().rev().cloned().collect();
    let variants = [all.clone(), reversed, flipped, Vec::new()];
    let bytes = std::fs::read(&f.binding.source.path).unwrap();
    let mut seed = seed(&f, 0);
    let (mut start, mut evidence) = (0, Vec::new());
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        let end = start + line.len() as u64;
        let line = line[..line.len() - 1].to_vec();
        seed.append(&CapturedRecord { start, end, line });
        start = end;
        for registrations in &variants {
            same_as_fresh(&seed, registrations, None, true);
            for _ in 0..4 {
                same_as_fresh(&seed, registrations, None, false);
            }
        }
        evidence.push(seed.analyzed(&all, None).unwrap().0.len());
    }
    assert_eq!((evidence[0], evidence[evidence.len() - 1]), (0, 2));
    let (start, end, line) = (start + 1, start + 3, b"{}".to_vec());
    seed.append(&CapturedRecord { start, end, line });
    assert!(seed.analyzed(&all, None).is_err() && seed.projection().is_err());
}

#[test]
fn a_registration_change_in_any_field_or_order_misses_the_memo() {
    let f = Fixture::new();
    let ticket = owned(&f, stop(json!([{"type":"text","text":MARK}])));
    f.register("later", "another request");
    stored(&f, &ticket, |_| {});
    let seed = whole(&f);
    let base = f.history.native_classification_registrations().unwrap();
    let digest = |registrations: &[Registration]| {
        let (evidence, ..) = seed.analyzed(registrations, None).unwrap();
        evidence[0].supersedes.clone().unwrap().barrier_digest
    };
    let stored_digest = digest(&base);
    let changes: [fn(&mut Vec<Registration>); 5] = [
        |r| r[0].candidate ^= true,
        |r| r[0].barrier = None,
        |r| r[0].exact_frame_hash.push('x'),
        |r| r.reverse(),
        // The same path to derived equality and the fingerprint, another barrier digest.
        |r| {
            let source = r[0]
                .barrier
                .as_mut()
                .unwrap()
                .ticket
                .source
                .as_mut()
                .unwrap();
            let spelled = source
                .transcript
                .to_str()
                .unwrap()
                .replace("/parent.", "//parent.");
            let spelled = std::path::PathBuf::from(spelled);
            assert!(
                spelled == source.transcript
                    && spelled.as_os_str() != source.transcript.as_os_str()
            );
            source.transcript = spelled;
        },
    ];
    for (at, change) in changes.into_iter().enumerate() {
        same_as_fresh(&seed, &base, None, at > 0);
        let mut changed = base.clone();
        change(&mut changed);
        same_as_fresh(&seed, &changed, None, true);
        if at == 4 {
            assert_ne!(digest(&changed), stored_digest);
        }
    }
}

#[test]
fn a_raw_witness_change_misses_the_memo() {
    let f = Fixture::new();
    let ticket = f.register("ours", "our request");
    f.append(&[user("own", "our request")]);
    f.receipt(&ticket, "own", "our request");
    f.history.note_native_any_key("native-b1", &ticket).unwrap();
    let seed = whole(&f);
    let registrations = f.history.native_classification_registrations().unwrap();
    let through = seed.projection().unwrap().through();
    let fingerprint = Fingerprint::of(&std::fs::metadata(&f.binding.source.path).unwrap()).unwrap();
    let witness = |aborted: bool, nonce: &str, digest: &str, tail: u64| RawWitness {
        version: 1,
        tail: Tail {
            through,
            prefix_hash: "prefix".into(),
            fingerprint: Fingerprint {
                len: through + tail,
                ..fingerprint.clone()
            },
            digest: digest.into(),
        },
        aborted,
        execution_nonce: nonce.into(),
        registrations_fingerprint: "stamp".into(),
    };
    let base = witness(false, "birth-1", "tail", 9);
    let (closed, ..) = seed.analyzed(&registrations, Some(&base)).unwrap();
    assert_eq!(closed[0].raw.as_ref(), Some(&base));
    let inputs = [
        None,
        Some(base),
        Some(witness(false, "birth-1", "tail", 10)),
        Some(witness(true, "birth-1", "tail", 10)),
        // The same range under another execution nonce, then another tail digest.
        Some(witness(true, "birth-2", "tail", 10)),
        Some(witness(true, "birth-2", "other", 10)),
        None,
    ];
    for raw in &inputs {
        same_as_fresh(&seed, &registrations, raw.as_ref(), true);
        same_as_fresh(&seed, &registrations, raw.as_ref(), false);
    }
}

#[test]
fn a_reopened_or_replaced_source_never_answers_from_an_older_memo() {
    let f = Fixture::new();
    f.register("ours", "our input");
    let rows = [
        queue("enqueue", "our input"),
        queue("dequeue", ""),
        user("own", "our input"),
        close(),
    ];
    f.append(&rows);
    let (channel, path) = (f.binding.channel_id, &f.binding.source.path);
    let open = |native: &mut Option<super::super::NativeSources>, source: &SourceId| {
        let bytes = std::fs::read(path).unwrap();
        let cursor = Cursor {
            source: source.clone(),
            captured_through: bytes.len() as u64,
            prefix_hash: hex::encode(sha2::Sha256::digest(&bytes)),
            retired: false,
            transition_cleanup: None,
        };
        super::super::open_native(native, channel, ShadowProvider::Claude, &cursor).unwrap();
        assert_eq!(native.as_ref().unwrap().seeds[source].reduced(), 0);
    };
    let observe = |native: &Option<super::super::NativeSources>, source: &SourceId| {
        let record = SourceRange {
            source: source.clone(),
            start: 0,
            end: 0,
        };
        let origin = NoticeOrigin {
            record,
            logical_session: "fixture".into(),
            binding_seq: 1,
            execution_nonce: "birth-1".into(),
        };
        let snapshot = Snapshot {
            generation: 1,
            registrations: f.history.native_classification_registrations().unwrap(),
        };
        let native = native.as_ref().unwrap();
        let observed = native.observe(channel, source, &origin, snapshot, None);
        (observed.unwrap().0.evidence, native.seeds[source].reduced())
    };
    let old = f.binding.source.clone();
    let mut native = Some(super::super::NativeSources::default());
    open(&mut native, &old);
    let (first, _) = observe(&native, &old);
    assert_eq!((first.len(), observe(&native, &old).1), (1, 1));
    // The same prefix authenticated again starts over, before any new record.
    open(&mut native, &old);
    assert_eq!(observe(&native, &old), (first.clone(), 1));
    // A second link keeps the old inode allocated, so no replacement can be given its number.
    std::fs::hard_link(path, path.with_extension("held")).unwrap();
    for grown in [false, true] {
        let next = path.with_extension("next");
        let mut body = std::fs::read(path).unwrap();
        body.extend(format!("{}\n", close()).bytes().filter(|_| grown));
        std::fs::write(&next, body).unwrap();
        std::fs::rename(&next, path).unwrap();
        let (dev, ino) = file_identity(&std::fs::metadata(path).unwrap());
        let new = SourceId {
            dev,
            ino,
            ..old.clone()
        };
        assert_ne!((new.dev, new.ino), (old.dev, old.ino), "grown={grown}");
        open(&mut native, &new);
        assert_eq!(observe(&native, &new), (Vec::new(), 1), "grown={grown}");
        assert_eq!(observe(&native, &old), (first.clone(), 1), "grown={grown}");
    }
}
