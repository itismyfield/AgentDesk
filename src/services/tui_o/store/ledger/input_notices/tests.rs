use super::super::{LedgerEntry, LedgerState, LocalRetryMark};
use super::*;
use chrono::Utc;

fn evidence(uuid: &str, prompt: &str, start: u64) -> NoticeEvidence {
    let raw_record = serde_json::to_vec(&serde_json::json!({
        "type": "user", "uuid": uuid,
        "message": {"role": "user", "content": prompt}
    }))
    .unwrap();
    let record = SourceRange {
        source: SourceId {
            session_id: "native".into(),
            path: "/tmp/transcript.jsonl".into(),
            dev: 1,
            ino: 2,
        },
        start,
        end: start + raw_record.len() as u64 + 1,
    };
    NoticeEvidence {
        record: record.clone(),
        raw_record,
        prompt: Some(prompt.into()),
        prompt_sha256: Some(hex::encode(Sha256::digest(prompt.as_bytes()))),
        unit_key: Some(UnitKey {
            channel_id: 7,
            provider: ShadowProvider::Claude,
            native_key: uuid.into(),
            kind: UnitKind::InputNotice,
        }),
        payload: Some(format!("-# ⌨️ {prompt}")),
        origin: Some(Box::new(NoticeOrigin {
            record,
            logical_session: "pane".into(),
            binding_seq: 1,
            execution_nonce: "original".into(),
        })),
    }
}

fn snapshot(evidence: &NoticeEvidence) -> MigrationSnapshot {
    MigrationSnapshot {
        sources: vec![MigrationSource {
            source: evidence.record.source.clone(),
            lower: 0,
            upper: 20_000,
            prefix_hash: "0".repeat(64),
        }],
        binding_checkpoint: 4,
        history: MigrationHistory::Fixed {
            lower: 10,
            upper: 90,
        },
    }
}

fn apply(state: &mut LedgerState, entry: NoticeEntry) {
    state.apply(
        Utc::now(),
        LedgerEntry::InputNotice {
            entry: Box::new(entry),
        },
    );
}

#[test]
fn materialized_unresolved_input_replays_raw_evidence_without_a_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.jsonl");
    std::fs::File::create(&path).unwrap();
    let mut input = evidence("p", "same", 0);
    input.unit_key = None;
    input.origin = None;
    let entries = [
        NoticeEntry::MigrationStarted {
            snapshot: snapshot(&input),
        },
        NoticeEntry::Materialized {
            evidence: Box::new(input.clone()),
        },
        NoticeEntry::MigrationUnresolved {
            record: input.record.clone(),
            reason: "native attribution unavailable".into(),
            candidates: vec![40, 50],
        },
    ];
    let mut expected = LedgerState::starting_at(100);
    for entry in entries {
        let entry = LedgerEntry::InputNotice {
            entry: Box::new(entry),
        };
        let at = Utc::now();
        super::super::append(&path, at, &entry).unwrap();
        expected.apply(at, entry);
    }
    let replay = super::super::recover(&path, 100).unwrap();
    assert_eq!(replay, expected);
    assert!(replay.violation().is_none());
    assert_eq!(
        replay.input_notices().evidence.values().next(),
        Some(&input)
    );
    assert_eq!(replay.input_notices().unresolved.len(), 1);
    assert_eq!(
        replay.input_notices().reserved_ids,
        BTreeSet::from([40, 50])
    );
    assert!(replay.excluded.is_empty());
    assert_eq!((replay.anchor(), replay.next_serial()), (100, 0));
}

fn replay_materialized(inputs: &[NoticeEvidence]) -> LedgerState {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.jsonl");
    std::fs::File::create(&path).unwrap();
    for input in inputs {
        super::super::append(
            &path,
            Utc::now(),
            &LedgerEntry::InputNotice {
                entry: Box::new(NoticeEntry::Materialized {
                    evidence: Box::new(input.clone()),
                }),
            },
        )
        .unwrap();
    }
    super::super::recover(&path, 10).unwrap()
}

#[test]
fn rerendered_materialized_replay_keeps_first_payload_and_enriches_missing_fields() {
    for reverse in [false, true] {
        for missing_payload in [false, true] {
            let mut first = evidence("p", "first\n\nhttps://example.com/a_b", 0);
            let mut next = first.clone();
            next.payload = Some("another renderer's payload".into());
            if reverse {
                std::mem::swap(&mut first.payload, &mut next.payload);
            }
            if missing_payload {
                first.payload = None;
            }
            first.origin = None;
            let state = replay_materialized(&[first.clone(), next.clone()]);
            assert_eq!(state.violation(), None);
            let mut expected = first.clone();
            expected.payload = first.payload.or(next.payload);
            expected.origin = next.origin;
            assert_eq!(
                state.input_notices().evidence.values().next(),
                Some(&expected)
            );
        }
    }
    let first = evidence("p", "first", 0);
    let mut partial = first.clone();
    partial.payload = None;
    partial.origin = None;
    let state = replay_materialized(&[first.clone(), partial]);
    assert_eq!(state.violation(), None);
    assert_eq!(state.input_notices().evidence.values().next(), Some(&first));
}

#[test]
fn materialized_prompt_must_match_raw_even_without_digest_or_native_key() {
    let mut first = evidence("p", "first", 0);
    first.prompt_sha256 = None;
    first.unit_key = None;
    let mut changed = first.clone();
    changed.prompt = Some("other".into());
    for inputs in [vec![changed.clone()], vec![first.clone(), changed]] {
        let state = replay_materialized(&inputs);
        assert_eq!(
            state.violation(),
            Some("input notice prompt or native key mismatches its raw record")
        );
        assert_eq!(state.input_notices().evidence.len(), inputs.len() - 1);
        if inputs.len() == 2 {
            assert_eq!(state.input_notices().evidence.values().next(), Some(&first));
        }
    }
}

#[test]
fn rerendered_materialized_replay_still_rejects_source_field_conflicts() {
    let first = evidence("p", "first", 0);
    let mut changed_origin = first.clone();
    changed_origin.origin.as_mut().unwrap().execution_nonce = "other".into();
    let mut changed_key = first.clone();
    changed_key.unit_key.as_mut().unwrap().channel_id += 1;
    let mut changed_prompt = first.clone();
    changed_prompt.prompt = Some("other".into());
    changed_prompt.prompt_sha256 = Some(hex::encode(Sha256::digest(b"other")));
    let mut changed_digest = first.clone();
    changed_digest.prompt_sha256 = Some("f".repeat(64));
    let mut changed_raw = first.clone();
    let bytes = String::from_utf8(changed_raw.raw_record).unwrap();
    changed_raw.raw_record = bytes
        .replace("\"role\":\"user\"", "\"role\":\"USER\"")
        .into_bytes();
    assert_ne!(changed_raw.raw_record, first.raw_record);
    for mut changed in [
        changed_origin,
        changed_key,
        changed_prompt,
        changed_digest,
        changed_raw,
    ] {
        changed.payload = Some("another renderer's payload".into());
        let state = replay_materialized(&[first.clone(), changed]);
        assert!(state.violation().is_some());
        assert_eq!(state.input_notices().evidence.values().next(), Some(&first));
    }
}

#[test]
fn legacy_import_is_one_atomic_receipt_without_moving_body_frontiers() {
    let first = evidence("p", "same", 0);
    let second = evidence("q", "same", 1_000);
    let mut state = LedgerState::starting_at(100);
    apply(
        &mut state,
        NoticeEntry::MigrationStarted {
            snapshot: snapshot(&first),
        },
    );
    for input in [&first, &second] {
        apply(
            &mut state,
            NoticeEntry::Materialized {
                evidence: Box::new(input.clone()),
            },
        );
    }
    let key = first.unit_key.clone().unwrap();
    apply(
        &mut state,
        NoticeEntry::LegacyImported {
            input_key: key.clone(),
            msg_id: 40,
        },
    );
    assert!(state.violation().is_none());
    assert_eq!(state.excluded(&key), Some("input_notice_legacy_imported"));
    assert_eq!(state.input_notices().imported.get(&key), Some(&40));
    assert_eq!((state.anchor(), state.next_serial()), (100, 0));
    let imported = state.clone();
    apply(
        &mut state,
        NoticeEntry::LegacyImported {
            input_key: key,
            msg_id: 40,
        },
    );
    assert_eq!(state, imported);
    apply(
        &mut state,
        NoticeEntry::LegacyImported {
            input_key: second.unit_key.clone().unwrap(),
            msg_id: 40,
        },
    );
    assert!(state.violation().is_some());
    assert!(state.excluded(second.unit_key.as_ref().unwrap()).is_none());
    assert_eq!(state.input_notices().imported.len(), 1);
}

#[test]
fn frozen_cohort_does_not_expand_or_import_outside_evidence_and_history() {
    let input = evidence("p", "same", 0);
    let initial = snapshot(&input);
    let mut state = LedgerState::default();
    apply(
        &mut state,
        NoticeEntry::MigrationStarted {
            snapshot: initial.clone(),
        },
    );
    apply(
        &mut state,
        NoticeEntry::Materialized {
            evidence: Box::new(input.clone()),
        },
    );
    for id in [0, 10, 91] {
        let mut invalid = state.clone();
        apply(
            &mut invalid,
            NoticeEntry::LegacyImported {
                input_key: input.unit_key.clone().unwrap(),
                msg_id: id,
            },
        );
        assert!(invalid.violation().is_some());
        assert!(invalid.excluded(input.unit_key.as_ref().unwrap()).is_none());
    }
    let mut changed = initial.clone();
    changed.sources[0].upper += 1;
    apply(
        &mut state,
        NoticeEntry::MigrationStarted { snapshot: changed },
    );
    assert!(state.violation().is_some());
    assert_eq!(state.input_notices().migration.as_ref(), Some(&initial));
    let mut straddling = initial;
    straddling.sources[0].upper = 1;
    assert!(straddling.contains(&input.record));
}

#[test]
fn incomplete_evidence_cannot_import_and_unresolved_candidates_stay_reserved() {
    let first = evidence("p", "same", 0);
    let mut second = evidence("q", "same", 1_000);
    let mut state = LedgerState::default();
    apply(
        &mut state,
        NoticeEntry::MigrationStarted {
            snapshot: snapshot(&first),
        },
    );
    apply(
        &mut state,
        NoticeEntry::Materialized {
            evidence: Box::new(first.clone()),
        },
    );
    apply(
        &mut state,
        NoticeEntry::MigrationUnresolved {
            record: first.record.clone(),
            reason: "ambiguous legacy match".into(),
            candidates: vec![40],
        },
    );
    second.origin = None;
    apply(
        &mut state,
        NoticeEntry::Materialized {
            evidence: Box::new(second.clone()),
        },
    );
    assert!(state.violation().is_none());
    let mut incomplete = state.clone();
    apply(
        &mut incomplete,
        NoticeEntry::LegacyImported {
            input_key: second.unit_key.clone().unwrap(),
            msg_id: 50,
        },
    );
    assert!(incomplete.violation().is_some());
    second.origin = evidence("q", "same", 1_000).origin;
    apply(
        &mut state,
        NoticeEntry::Materialized {
            evidence: Box::new(second.clone()),
        },
    );
    assert!(state.violation().is_none());
    apply(
        &mut state,
        NoticeEntry::LegacyImported {
            input_key: second.unit_key.unwrap(),
            msg_id: 40,
        },
    );
    assert!(state.violation().is_some());
    assert!(state.excluded.is_empty());
}

#[test]
fn materialized_prompt_digest_and_native_key_conflicts_preserve_first_evidence() {
    let input = evidence("p", "first", 0);
    let mut root = LedgerState::default();
    apply(
        &mut root,
        NoticeEntry::Materialized {
            evidence: Box::new(input.clone()),
        },
    );
    let mut invalid_digest = input.clone();
    invalid_digest.prompt_sha256 = Some("f".repeat(64));
    for changed in [invalid_digest, evidence("p", "changed", 1_000)] {
        let mut state = root.clone();
        apply(
            &mut state,
            NoticeEntry::Materialized {
                evidence: Box::new(changed),
            },
        );
        assert!(state.violation().is_some());
        assert_eq!(state.input_notices().evidence.values().next(), Some(&input));
    }
    let mut invalid = input.clone();
    invalid.unit_key.as_mut().unwrap().native_key = "other".into();
    apply(
        &mut root,
        NoticeEntry::Materialized {
            evidence: Box::new(invalid),
        },
    );
    assert!(root.violation().is_some());
    assert_eq!(root.input_notices().evidence.values().next(), Some(&input));
}

#[test]
fn enriching_an_unknown_key_still_checks_the_retained_full_prompt() {
    let original = evidence("p", "first", 0);
    let mut changed = evidence("p", "changed", 1_000);
    let key = changed.unit_key.take();
    let mut state = LedgerState::default();
    for input in [original, changed.clone()] {
        apply(
            &mut state,
            NoticeEntry::Materialized {
                evidence: Box::new(input),
            },
        );
    }
    assert!(state.violation().is_none());
    changed.unit_key = key;
    changed.prompt = None;
    changed.prompt_sha256 = None;
    apply(
        &mut state,
        NoticeEntry::Materialized {
            evidence: Box::new(changed),
        },
    );
    assert!(state.violation().is_some());
    assert!(
        state
            .input_notices()
            .evidence
            .values()
            .any(|kept| kept.unit_key.is_none())
    );
}

#[test]
fn prepared_legacy_default_and_notice_origin_roundtrip_preserve_retry_identity() {
    let input = evidence("p", "same", 0);
    let entry = LedgerEntry::Prepared {
        serial: 0,
        unit_key: input.unit_key.unwrap(),
        piece_index: 0,
        payload: input.payload.unwrap(),
        anchor_id: 0,
        epoch: 1,
        local_retry: Some(LocalRetryMark::Budget),
        notice_origin: input.origin,
    };
    let mut encoded = serde_json::to_value(&entry).unwrap();
    assert_eq!(
        serde_json::from_value::<LedgerEntry>(encoded.clone()).unwrap(),
        entry
    );
    encoded.as_object_mut().unwrap().remove("notice_origin");
    assert!(matches!(
        serde_json::from_value::<LedgerEntry>(encoded).unwrap(),
        LedgerEntry::Prepared {
            notice_origin: None,
            ..
        }
    ));
    let mut root = LedgerState::default();
    root.apply(Utc::now(), entry.clone());
    root.apply(Utc::now(), LedgerEntry::NotFound { serial: 0 });
    assert!(root.piece(0).unwrap().notice_origin.is_some());
    for retain_origin in [true, false] {
        let mut state = root.clone();
        let mut child = entry.clone();
        if let LedgerEntry::Prepared {
            serial,
            local_retry,
            notice_origin,
            ..
        } = &mut child
        {
            *serial = 1;
            *local_retry = Some(LocalRetryMark::Attempt { root_serial: 0 });
            if !retain_origin {
                *notice_origin = None;
            }
        }
        state.apply(Utc::now(), child);
        assert_eq!(state.violation().is_none(), retain_origin);
    }
}

#[test]
fn native_key_partial_evidence_cannot_bypass_conflicts_in_either_order() {
    let complete = evidence("p", "first", 0);
    let mut partial = evidence("p", "changed", 1_000);
    partial.prompt = None;
    partial.prompt_sha256 = None;
    let mut missing_digest = evidence("p", "first", 1_000);
    missing_digest.prompt_sha256 = None;
    for partial in [partial, missing_digest] {
        for partial_first in [false, true] {
            let mut state = NoticeState::default();
            if !partial_first {
                state
                    .apply(NoticeEntry::Materialized {
                        evidence: Box::new(complete.clone()),
                    })
                    .unwrap();
            }
            let before = state.clone();
            assert!(
                state
                    .apply(NoticeEntry::Materialized {
                        evidence: Box::new(partial.clone()),
                    })
                    .is_err(),
                "a native key must never enter storage without a canonical prompt digest"
            );
            assert_eq!(state, before);
            if partial_first {
                state
                    .apply(NoticeEntry::Materialized {
                        evidence: Box::new(complete.clone()),
                    })
                    .unwrap();
            }
            assert_eq!(state.evidence.len(), 1);
            assert_eq!(state.evidence.values().next(), Some(&complete));
            assert!(state.imported.is_empty());
            assert!(state.reserved_ids.is_empty());
        }
    }
}

#[test]
fn native_key_only_enrichment_requires_the_retained_canonical_prompt() {
    let complete = evidence("p", "first", 0);
    let mut raw_only = complete.clone();
    raw_only.unit_key = None;
    raw_only.prompt = None;
    raw_only.prompt_sha256 = None;
    raw_only.payload = None;
    raw_only.origin = None;
    let mut state = NoticeState::default();
    state
        .apply(NoticeEntry::Materialized {
            evidence: Box::new(raw_only.clone()),
        })
        .unwrap();
    let mut key_only = raw_only.clone();
    key_only.unit_key = complete.unit_key.clone();
    let before = state.clone();
    assert!(
        state
            .apply(NoticeEntry::Materialized {
                evidence: Box::new(key_only.clone()),
            })
            .is_err()
    );
    assert_eq!(state, before);
    raw_only.prompt = complete.prompt.clone();
    raw_only.prompt_sha256 = complete.prompt_sha256.clone();
    state
        .apply(NoticeEntry::Materialized {
            evidence: Box::new(raw_only),
        })
        .unwrap();
    state
        .apply(NoticeEntry::Materialized {
            evidence: Box::new(key_only),
        })
        .unwrap();
    let stored = state.evidence.values().next().unwrap();
    assert_eq!(stored.unit_key, complete.unit_key);
    assert_eq!(stored.prompt, complete.prompt);
    assert_eq!(stored.prompt_sha256, complete.prompt_sha256);
    assert!(!stored.complete());
}

#[test]
fn legacy_import_rejects_any_noncanonical_or_conflicting_same_key_evidence() {
    let complete = evidence("p", "first", 0);
    let key = complete.unit_key.clone().unwrap();
    let mut partial = evidence("p", "changed", 1_000);
    partial.prompt = None;
    partial.prompt_sha256 = None;
    let mut forged = evidence("p", "changed", 1_000);
    forged.prompt_sha256 = complete.prompt_sha256.clone();
    for invalid in [partial, evidence("p", "changed", 1_000), forged] {
        for already_imported in [false, true] {
            let mut state = NoticeState::default();
            state
                .apply(NoticeEntry::MigrationStarted {
                    snapshot: snapshot(&complete),
                })
                .unwrap();
            state
                .apply(NoticeEntry::Materialized {
                    evidence: Box::new(complete.clone()),
                })
                .unwrap();
            if already_imported {
                state
                    .apply(NoticeEntry::LegacyImported {
                        input_key: key.clone(),
                        msg_id: 40,
                    })
                    .unwrap();
            }
            state
                .evidence
                .insert(record_id(&invalid.record), invalid.clone());
            let before = state.clone();
            assert!(
                state
                    .apply(NoticeEntry::LegacyImported {
                        input_key: key.clone(),
                        msg_id: 40,
                    })
                    .is_err(),
                "one complete row cannot authorize a receipt beside conflicting native evidence"
            );
            assert_eq!(state, before);
        }
    }
}
