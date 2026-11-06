use super::*;
use crate::services::tui_o::shadow::{ShadowProvider, SourceRange, UnitKind};
use crate::services::tui_o::store::ledger::LedgerEntry;
use crate::services::tui_o::store::ledger::input_notices::{NoticeEntry, NoticeEvidence};
use crate::services::tui_o::writer::input_notices::NoticeOrigin;
use sha2::{Digest, Sha256};

mod streaming_tests;

fn large_notice(runtime: &Path) -> LedgerEntry {
    let prompt = "x".repeat(210 * 1024);
    let raw_record = serde_json::to_vec(&serde_json::json!({
        "type": "user", "uuid": "native-notice",
        "message": {"role": "user", "content": prompt}
    }))
    .unwrap();
    let record = SourceRange {
        source: SourceId {
            session_id: "native".into(),
            path: runtime.join("transcript.jsonl"),
            dev: 1,
            ino: 2,
        },
        start: 0,
        end: raw_record.len() as u64 + 1,
    };
    LedgerEntry::InputNotice {
        entry: Box::new(NoticeEntry::Materialized {
            evidence: Box::new(NoticeEvidence {
                record: record.clone(),
                raw_record,
                prompt_sha256: Some(hex::encode(Sha256::digest(prompt.as_bytes()))),
                prompt: Some(prompt),
                unit_key: Some(UnitKey {
                    channel_id: 7,
                    provider: ShadowProvider::Claude,
                    native_key: "native-notice".into(),
                    kind: UnitKind::InputNotice,
                }),
                payload: Some("-# ⌨️ large input".into()),
                origin: Some(Box::new(NoticeOrigin {
                    record,
                    logical_session: "pane".into(),
                    binding_seq: 1,
                    execution_nonce: "original".into(),
                })),
            }),
        }),
    }
}

fn large_notice_does_not_hide_body_receipt(notice_first: bool) {
    let runtime = tempfile::tempdir().unwrap();
    let store = super::super::tests::enabled(runtime.path());
    let era = super::super::tests::sealed(&store, &[7]);
    let mut channel = store.open_channel(&era, 7).unwrap().unwrap();
    let body = UnitKey {
        channel_id: 7,
        provider: ShadowProvider::Claude,
        native_key: "body".into(),
        kind: UnitKind::Body,
    };
    if notice_first {
        channel.append_ledger(large_notice(runtime.path())).unwrap();
    }
    channel
        .append_ledger(LedgerEntry::Prepared {
            serial: 0,
            unit_key: body.clone(),
            piece_index: 0,
            payload: "normal body".into(),
            anchor_id: 100,
            epoch: 3,
            local_retry: None,
            notice_origin: None,
        })
        .unwrap();
    channel
        .append_ledger(LedgerEntry::Posted {
            serial: 0,
            msg_id: 200,
            confirmation: None,
        })
        .unwrap();
    if !notice_first {
        channel.append_ledger(large_notice(runtime.path())).unwrap();
    }
    assert!(channel.ledger().violation().is_none());
    let expected = channel
        .ledger()
        .latest_piece(&body, 0)
        .map(|(serial, record)| (serial, record.clone()))
        .unwrap();
    drop(channel);
    let ledger = runtime.path().join("o_store/7/ledger.jsonl");
    let before = std::fs::read(&ledger).unwrap();
    assert!(
        before
            .split(|byte| *byte == b'\n')
            .any(|line| line.len() > 1024 * 1024),
        "the fixture must exceed the observer's existing per-line cap"
    );
    let snapshot = local(
        runtime.path(),
        7,
        &[(body.clone(), 0)],
        Instant::now() + std::time::Duration::from_secs(10),
    )
    .expect("large notice evidence must not hide a normal Body receipt");
    let actual = snapshot.latest_piece(&body, 0).unwrap();
    assert_eq!(actual.0, expected.0);
    assert_eq!(actual.1, &expected.1);
    assert_eq!(std::fs::read(&ledger).unwrap(), before);
    assert!(snapshot.max_line_bytes <= 1024 * 1024);
}

#[test]
fn large_notice_before_body_keeps_the_body_receipt_available() {
    large_notice_does_not_hide_body_receipt(true);
}

#[test]
fn large_notice_after_body_keeps_the_body_receipt_available() {
    large_notice_does_not_hide_body_receipt(false);
}

#[test]
fn corrupt_large_notice_and_oversized_delivery_rows_stay_unavailable() {
    let runtime = tempfile::tempdir().unwrap();
    let store = super::super::tests::enabled(runtime.path());
    let era = super::super::tests::sealed(&store, &[7]);
    let mut channel = store.open_channel(&era, 7).unwrap().unwrap();
    let body = UnitKey {
        channel_id: 7,
        provider: ShadowProvider::Claude,
        native_key: "body".into(),
        kind: UnitKind::Body,
    };
    channel
        .append_ledger(LedgerEntry::Prepared {
            serial: 0,
            unit_key: body.clone(),
            piece_index: 0,
            payload: "body".into(),
            anchor_id: 100,
            epoch: 3,
            local_retry: None,
            notice_origin: None,
        })
        .unwrap();
    channel
        .append_ledger(LedgerEntry::Posted {
            serial: 0,
            msg_id: 200,
            confirmation: None,
        })
        .unwrap();
    channel.append_ledger(large_notice(runtime.path())).unwrap();
    drop(channel);
    let path = runtime.path().join("o_store/7/ledger.jsonl");
    let before = std::fs::read(&path).unwrap();
    let query = || {
        local(
            runtime.path(),
            7,
            &[(body.clone(), 0)],
            Instant::now() + std::time::Duration::from_secs(10),
        )
    };
    assert!(query().is_some());
    super::streaming::EXPIRE_AFTER_CHUNK.with(|expire| expire.set(true));
    assert!(query().is_none());
    assert!(!super::streaming::EXPIRE_AFTER_CHUNK.with(std::cell::Cell::get));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(query().is_some());
    let prompt = before
        .windows(10)
        .position(|part| part == b"\"prompt\":\"")
        .unwrap()
        + 10;
    let mut utf8 = before.clone();
    utf8[prompt] = 0xff;
    let mut surrogate = before.clone();
    surrogate[prompt..prompt + 6].copy_from_slice(br#"\uD800"#);
    let mut syntax = before.clone();
    syntax[prompt] = b'"';
    let mut torn = before.clone();
    assert_eq!(torn.pop(), Some(b'\n'));
    let mut suffix = torn.clone();
    suffix.extend_from_slice(b"false\n");
    let mut oversized = before.clone();
    oversized.extend_from_slice(
        &serde_json::to_vec(&ledger::LedgerLine {
            at: chrono::Utc::now(),
            entry: LedgerEntry::Prepared {
                serial: 1,
                unit_key: body.clone(),
                piece_index: 1,
                payload: "x".repeat(1024 * 1024),
                anchor_id: 200,
                epoch: 3,
                local_retry: None,
                notice_origin: None,
            },
        })
        .unwrap(),
    );
    oversized.push(b'\n');
    for (name, bytes) in [
        ("UTF-8", utf8),
        ("surrogate", surrogate),
        ("syntax", syntax),
        ("torn", torn),
        ("suffix", suffix),
        ("delivery cap", oversized),
    ] {
        std::fs::write(&path, &bytes).unwrap();
        assert!(query().is_none(), "observer accepted {name}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            bytes,
            "observer wrote during {name}"
        );
    }
    std::fs::write(&path, &before).unwrap();
    assert!(query().is_some());
}

/// A growing ledger is read only past the last observation, with the answer a full replay gives;
/// a withdrawn last line, a shorter file, an in-place rewrite and a replaced file read it whole.
#[test]
fn growing_ledger_is_read_past_the_last_observation_with_the_full_answer() {
    use std::io::Write;
    let runtime = tempfile::tempdir().unwrap();
    let store = super::super::tests::enabled(runtime.path());
    let era = super::super::tests::sealed(&store, &[7]);
    drop(store.open_channel(&era, 7).unwrap().unwrap());
    let path = runtime.path().join("o_store/7/ledger.jsonl");
    let key = |name: &str| UnitKey {
        channel_id: 7,
        provider: ShadowProvider::Claude,
        native_key: name.into(),
        kind: UnitKind::Body,
    };
    let target = key("target");
    let at: chrono::DateTime<chrono::Utc> = "2026-11-03T00:00:00Z".parse().unwrap();
    let line = |entry: LedgerEntry| {
        let mut bytes = serde_json::to_vec(&ledger::LedgerLine { at, entry }).unwrap();
        bytes.push(b'\n');
        bytes
    };
    let prepared = |serial: u64, unit_key: &UnitKey, payload: &str| {
        line(LedgerEntry::Prepared {
            serial,
            unit_key: unit_key.clone(),
            piece_index: 0,
            payload: payload.into(),
            anchor_id: 100,
            epoch: 3,
            local_retry: None,
            notice_origin: None,
        })
    };
    let append = |bytes: &[u8]| {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(bytes).unwrap();
    };
    let mut base = Vec::new();
    for serial in 0..20_000u64 {
        let unit = if serial == 10_000 {
            target.clone()
        } else {
            key(&format!("body-{serial}"))
        };
        base.extend(prepared(serial, &unit, &format!("body {serial}")));
        base.extend(line(LedgerEntry::Posted {
            serial,
            msg_id: 1_000_000 + serial,
            confirmation: None,
        }));
    }
    append(&base);
    let anchor = store.read_init(7).unwrap().unwrap().initial_anchor;
    let observe = |lines: usize, why: &str| {
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        let seen = local(runtime.path(), 7, &[(target.clone(), 0)], deadline).expect(why);
        let mut file = File::open(&path).unwrap();
        let replay = ledger::read_only(&mut file, anchor).unwrap();
        let expected = replay.latest_piece(&target, 0).map(|(s, r)| (s, r.clone()));
        let actual = seen.latest_piece(&target, 0).map(|(s, r)| (s, r.clone()));
        assert_eq!(actual, expected, "{why}");
        assert_eq!(seen.parsed_lines, lines, "{why}");
    };
    observe(40_000, "first observation");
    observe(0, "unchanged ledger");
    append(&prepared(20_000, &target, "first retry"));
    append(&line(LedgerEntry::NotFound { serial: 20_000 }));
    observe(2, "appended lines only");

    let withdrawn = prepared(20_001, &target, "withdrawn A");
    append(&withdrawn);
    observe(1, "the line that is withdrawn next");
    let len = std::fs::metadata(&path).unwrap().len();
    let cut = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    cut.set_len(len - withdrawn.len() as u64).unwrap();
    drop(cut);
    let replacement = prepared(20_001, &target, "withdrawn B");
    assert_eq!(replacement.len(), withdrawn.len());
    append(&replacement);
    append(&line(LedgerEntry::NotFound { serial: 20_001 }));
    observe(40_004, "a withdrawn last line");

    let shorter = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    shorter.set_len(base.len() as u64).unwrap();
    drop(shorter);
    observe(40_000, "a shorter file");

    let mut rewritten = std::fs::read(&path).unwrap();
    let (from, to) = (b"body 10000", b"body 1000X");
    let at = rewritten
        .windows(from.len())
        .position(|w| w == from)
        .unwrap();
    rewritten[at..at + to.len()].copy_from_slice(to);
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    std::fs::write(&path, &rewritten).unwrap();
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_modified(modified + std::time::Duration::from_secs(1))
        .unwrap();
    drop(file);
    observe(40_000, "an in-place rewrite of equal length");

    let copy = path.with_extension("copy");
    std::fs::write(&copy, &base).unwrap();
    std::fs::rename(&copy, &path).unwrap();
    observe(40_000, "a replaced file");
}
