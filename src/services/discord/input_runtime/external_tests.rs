use super::*;
use crate::services::tui_input::input_key::{EXTERNAL_KEY_BASE, EXTERNAL_KEY_END, is_external_key};
use crate::services::tui_input::ledger::LedgerSlot;
use crate::services::tui_input::rows::{AbandonReason, DoneReason, Entry, Rows};
use std::path::{Path, PathBuf};

const CHANNEL: u64 = 101;
const AUTHOR: u64 = 7;

fn sandbox() -> tempfile::TempDir {
    let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
    std::fs::create_dir_all(&parent).unwrap();
    tempfile::tempdir_in(parent).unwrap()
}

fn input(text: &str, origin_id: &str) -> Source {
    source(CHANNEL, AUTHOR, text, Some("imessage"), origin_id).unwrap()
}

// A row at `key` whose stored input says `http_origin`; Source::new keeps provenance honest.
fn planted(key: u64, author: u64, http_origin: Value) -> Source {
    let identity = ReceiptIdentity::new(key, vec![key], author, CHANNEL, CHANNEL).unwrap();
    let mut input = json!({"text": "planted", "source_message_ids": [key]});
    if !http_origin.is_null() {
        input["http_origin"] = http_origin;
    }
    Source::new(key, identity, input, Vec::new()).unwrap()
}

// Appends a planted row through the generic receipt path, bypassing the origin checks.
fn append(lease: &mut LedgerLease, source: Source) -> DurableReceipt {
    match receipt::commit(lease, source, true) {
        Receipt::Accepted(receipt) => receipt,
        other => panic!("plant refused: {other:?}"),
    }
}

fn origin_of(source: &str, origin_id: &str) -> Value {
    json!({"version": 1, "source": source, "origin_id": origin_id})
}

fn seq(lease: &mut LedgerLease) -> u64 {
    lease.get().unwrap().rows().unwrap().folded_seq()
}

fn received(result: ExternalReceipt) -> (DurableReceipt, bool, RowState) {
    match result {
        ExternalReceipt::Received {
            receipt,
            duplicate,
            state,
        } => (receipt, duplicate, state),
        other => panic!("not received: {other:?}"),
    }
}

#[test]
fn external_source_is_origin_keyed_trimmed_and_bounded() {
    let source = source(CHANNEL, AUTHOR, "hi", Some(" imessage "), " guid-1 ").unwrap();
    assert_eq!(source.key(), external_key_v1(CHANNEL, "imessage", "guid-1"));
    assert_eq!(source.identity().source_ids, [source.key()]);
    assert_eq!(
        source.input()["http_origin"],
        origin_of("imessage", "guid-1")
    );
    for ns in [None, Some("  ")] {
        let key = super::source(CHANNEL, AUTHOR, "hi", ns, "guid-1")
            .unwrap()
            .key();
        assert_eq!(key, external_key_v1(CHANNEL, "external", "guid-1"));
    }
    let long_source = "s".repeat(65);
    let long_origin = "o".repeat(257);
    for (ns, origin) in [
        ("imessage", ""),
        ("imessage", "   "),
        ("imessage", long_origin.as_str()),
        (long_source.as_str(), "guid-1"),
    ] {
        assert!(super::source(CHANNEL, AUTHOR, "hi", Some(ns), origin).is_err());
    }
    assert!(
        super::source(
            CHANNEL,
            AUTHOR,
            "hi",
            Some(&"s".repeat(64)),
            &"o".repeat(256)
        )
        .is_ok()
    );
}

#[test]
fn external_collision_precedes_generic_known_receipt() {
    let dir = sandbox();
    let mut slot = LedgerSlot::new(dir.path(), CHANNEL);
    let mut lease = slot.lend().unwrap();
    let key = input("first", "guid-a").key();
    // Same key and provenance, another origin: only the origin check can tell them apart.
    let other = planted(key, AUTHOR, origin_of("imessage", "guid-b"));
    let first = append(&mut lease, other);
    let before = seq(&mut lease);
    assert_eq!(
        submit(&mut lease, input("second", "guid-a"), true),
        ExternalReceipt::Collision
    );
    assert_eq!(seq(&mut lease), before);
    let rows = lease.get().unwrap().rows().unwrap();
    assert_eq!(rows.row(key).unwrap().input["text"], "planted");
    assert_eq!(
        rows.row(key).unwrap().received_seq,
        Some(first.received_seq)
    );
    assert_eq!(rows.open_rows().count(), 1);
}

type Plant = Box<dyn Fn(&mut LedgerLease)>;

#[test]
fn external_unknown_identity_never_becomes_absent() {
    let key = input("x", "guid-a").key();
    let cases: Vec<(&str, Plant, ExternalReceipt)> = vec![
        (
            "origin missing",
            Box::new(move |lease| {
                append(lease, planted(key, AUTHOR, Value::Null));
            }),
            ExternalReceipt::Deferred(Deferred::Unknown),
        ),
        (
            "other origin version",
            Box::new(move |lease| {
                let origin = json!({"version": 2, "source": "imessage", "origin_id": "guid-a"});
                append(lease, planted(key, AUTHOR, origin));
            }),
            ExternalReceipt::Deferred(Deferred::Unknown),
        ),
        (
            "index without its row",
            Box::new(move |lease| {
                let identity = ReceiptIdentity::new(key, vec![key], AUTHOR, CHANNEL, CHANNEL);
                let mut state = serde_json::to_value(Rows::default()).unwrap();
                state["receipts"] = json!({ key.to_string(): identity.unwrap() });
                lease.get().unwrap().checkpoint(state).unwrap();
            }),
            ExternalReceipt::Deferred(Deferred::Unknown),
        ),
        (
            "handed to Legacy",
            Box::new(move |lease| {
                received(submit(lease, input("x", "guid-a"), true));
                let handback = RowState::Abandoned(AbandonReason::Handback);
                let entry = Entry::Transition {
                    key,
                    state: handback,
                    attempt: None,
                };
                lease.get().unwrap().append_entry(&entry, &[]).unwrap();
            }),
            ExternalReceipt::Deferred(Deferred::Conflict),
        ),
        (
            "another author",
            Box::new(move |lease| {
                let origin = origin_of("imessage", "guid-a");
                append(lease, planted(key, AUTHOR + 1, origin));
            }),
            ExternalReceipt::AuthorMismatch,
        ),
    ];
    for (case, plant, expected) in cases {
        let dir = sandbox();
        let mut slot = LedgerSlot::new(dir.path(), CHANNEL);
        let mut lease = slot.lend().unwrap();
        plant(&mut lease);
        let before = seq(&mut lease);
        let rows = lease.get().unwrap().rows().unwrap();
        let row = rows.row(key).map(|row| (row.state, row.input.clone()));
        assert_eq!(
            submit(&mut lease, input("x", "guid-a"), true),
            expected,
            "{case}"
        );
        assert_eq!(seq(&mut lease), before, "{case}");
        let rows = lease.get().unwrap().rows().unwrap();
        let after = rows.row(key).map(|row| (row.state, row.input.clone()));
        assert_eq!(after, row, "{case}");
    }
}

#[test]
fn closed_admission_refuses_only_new_origins() {
    let dir = sandbox();
    let mut slot = LedgerSlot::new(dir.path(), CHANNEL);
    let mut lease = slot.lend().unwrap();
    let (first, ..) = received(submit(&mut lease, input("first", "guid-a"), true));
    let before = seq(&mut lease);
    let (again, duplicate, state) = received(submit(&mut lease, input("again", "guid-a"), false));
    assert_eq!((again, duplicate, state), (first, true, RowState::Received));
    assert_eq!(
        submit(&mut lease, input("new", "guid-b"), false),
        ExternalReceipt::Deferred(Deferred::Closed)
    );
    assert_eq!(seq(&mut lease), before);
}

#[test]
fn external_terminal_origin_survives_compact_reopen() {
    for terminal in [
        RowState::Done(DoneReason::Completed),
        RowState::Abandoned(AbandonReason::UserClear),
    ] {
        let dir = sandbox();
        let mut slot = LedgerSlot::new(dir.path(), CHANNEL);
        let mut lease = slot.lend().unwrap();
        let (first, ..) = received(submit(&mut lease, input("first", "guid-a"), true));
        let key = first.key;
        let ledger = lease.get().unwrap();
        let entry = Entry::Transition {
            key,
            state: terminal,
            attempt: None,
        };
        ledger.append_entry(&entry, &[]).unwrap();
        ledger.checkpoint_rows().unwrap();
        let tombstone = ledger.rows().unwrap().row(key).unwrap().input.clone();
        let expected = json!({
            "receipt_identity": first.identity,
            "http_origin": origin_of("imessage", "guid-a"),
        });
        assert_eq!(tombstone, expected);
        ledger.checkpoint_rows().unwrap();
        assert_eq!(ledger.rows().unwrap().row(key).unwrap().input, tombstone);
        slot.restore(lease);
        slot.reopen().unwrap();
        let mut lease = slot.lend().unwrap();
        let before = seq(&mut lease);
        let (again, duplicate, state) =
            received(submit(&mut lease, input("retry", "guid-a"), true));
        assert_eq!((again, duplicate, state), (first, true, terminal));
        let other = planted(key, AUTHOR, origin_of("imessage", "guid-b"));
        assert_eq!(submit(&mut lease, other, true), ExternalReceipt::Collision);
        assert_eq!(seq(&mut lease), before);
        let rows = lease.get().unwrap().rows().unwrap();
        assert_eq!(rows.row(key).unwrap().state, terminal);
        assert_eq!(rows.open_rows().count(), 0);
    }
}

// Production Rust sources: test files and `tests/` trees are fixtures, not producers.
fn production_sources() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut found = Vec::new();
    let mut dirs = vec![root.join("src")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path: PathBuf = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_dir() {
                if name != "tests" {
                    dirs.push(path);
                }
            } else if name.ends_with(".rs") && !name.ends_with("_tests.rs") && name != "tests.rs" {
                let relative = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                found.push((relative, std::fs::read_to_string(&path).unwrap()));
            }
        }
    }
    assert!(found.len() > 1000, "the scan must see the whole crate");
    found
}

#[test]
fn hm1_external_producer_census_is_zero() {
    // Every production mention of the external producer surface; a call site anywhere else
    // (a route, intake, voice or boot) is an unclassified producer.
    let surface = regex::Regex::new(
        r"\bexternal::(source|submit)\b|\bexternal::\{|\bSubmitExternal\b|\bexternal_key_v1\b",
    )
    .unwrap();
    let allowed = [
        (
            "src/services/tui_input/input_key.rs",
            "key codec definition",
        ),
        (
            "src/services/discord/input_runtime/external.rs",
            "source and submit definitions",
        ),
        (
            "src/services/discord/input_runtime/command.rs",
            "SubmitExternal definition",
        ),
        (
            "src/services/discord/input_runtime/supervisor.rs",
            "the one SubmitExternal consumer",
        ),
    ];
    let mut seen = std::collections::BTreeSet::new();
    for (path, text) in production_sources() {
        if surface.is_match(&text) {
            assert!(
                allowed.iter().any(|(file, _)| *file == path),
                "unclassified external producer in {path}"
            );
            seen.insert(path);
        }
    }
    let classified: std::collections::BTreeSet<_> =
        allowed.iter().map(|(file, _)| file.to_string()).collect();
    assert_eq!(
        seen, classified,
        "a classified file no longer holds its surface"
    );
}

#[test]
fn existing_generators_start_above_external_range() {
    let literal = regex::Regex::new(r"(?:^|[^\w.])(\d[\d_]{17,})(?:_?u64)?\b").unwrap();
    let parse = |code: &str| -> Vec<u64> {
        (literal.captures_iter(code))
            .filter_map(|capture| capture[1].replace('_', "").parse().ok())
            .collect()
    };
    // Seeds of every Legacy message-id generator, and the fixed ids that sit in the external range.
    let seeds = [
        (
            "src/services/discord/gateway.rs",
            "HEADLESS_MESSAGE_ID_SEQ: AtomicU64 = AtomicU64::new(",
        ),
        (
            "src/services/discord/voice_barge_in/utility.rs",
            "INTERNAL_VOICE_MESSAGE_ID_START: u64 =",
        ),
        (
            "src/services/discord/router/turn_start.rs",
            "const HEADLESS_TURN_MESSAGE_ID_BASE: u64 =",
        ),
    ];
    let in_range_allowed = [
        ("src/services/tui_input/input_key.rs", EXTERNAL_KEY_BASE),
        // A placeholder anchor for a failed recovery post, never an input-ledger key.
        (
            "src/services/discord/turn_bridge/headless_delivery.rs",
            EXTERNAL_KEY_BASE + 1,
        ),
    ];
    let sources: std::collections::BTreeMap<_, _> = production_sources().into_iter().collect();
    for (path, text) in &sources {
        for line in text.lines() {
            for value in parse(line.split("//").next().unwrap()) {
                assert!(
                    !is_external_key(value) || in_range_allowed.contains(&(path.as_str(), value)),
                    "{path}: {value} lies in the external key range"
                );
            }
        }
    }
    for (path, anchor) in seeds {
        let text = &sources[path];
        let mut read = 0;
        for (at, _) in text.match_indices(anchor) {
            let rest = &text[at + anchor.len()..];
            let values = parse(&rest[..rest.find(';').unwrap()]);
            // An initializer this census cannot read is a new generator shape: fail closed.
            assert_eq!(values.len(), 1, "{path}: unreadable seed {anchor}");
            assert!(
                values[0] >= EXTERNAL_KEY_END,
                "{path}: seed {} reaches the range",
                values[0]
            );
            read += 1;
        }
        assert!(read > 0, "{path}: generator seed {anchor} moved");
    }
    let voice = crate::services::discord::voice_barge_in::INTERNAL_VOICE_MESSAGE_ID_START;
    assert!(voice >= EXTERNAL_KEY_END);
}
