use super::*;
use crate::services::tui_input::durability_tests::supported::Recording;
use crate::services::tui_input::ledger::{LedgerSlot, OPENS, SlotError};
use crate::services::tui_input::rows::{AbandonReason, Entry, RowState};
use serde_json::json;

fn sandbox() -> tempfile::TempDir {
    let parent = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
    std::fs::create_dir_all(&parent).unwrap();
    tempfile::tempdir_in(parent).unwrap()
}

fn source(key: u64, ids: &[u64], text: &str) -> Source {
    Source::new(
        key,
        ReceiptIdentity::new(key, ids.to_vec(), 7, 8, 9).unwrap(),
        json!({"text":text,"source_message_ids":ids,"reply_context":text}),
        vec![],
    )
    .unwrap()
}

fn prepared_source(key: u64, fetched: bool) -> Source {
    use crate::services::cluster::attachment_transfer::AttachmentEntryV1;
    use crate::services::discord::input_runtime::intake::{
        Origin, SourceDraft, StagedAttachment, prepare_source,
    };
    use sha2::{Digest, Sha256};

    let attachments = [b"first".as_slice(), b"second".as_slice()]
        .into_iter()
        .enumerate()
        .map(|(index, bytes)| {
            let filename = format!("{index}.txt");
            StagedAttachment {
                filename: filename.clone(),
                url: format!("https://fixture.invalid/{index}"),
                staged: fetched.then(|| AttachmentEntryV1 {
                    filename,
                    sha256: hex::encode(Sha256::digest(bytes)),
                    bytes: bytes.to_vec(),
                }),
            }
        })
        .collect();
    let prepared = prepare_source(SourceDraft {
        origin: Origin::Message {
            message_id: key,
            source_ids: vec![key + 1, key],
            author_id: 7,
            original_channel_id: 8,
            execution_channel_id: 9,
        },
        author_is_bot: false,
        original_text: "!skill original".into(),
        prompt: "expanded prompt".into(),
        reply_context: Some("reply to original 5".into()),
        attachments,
        voice_announcement: Some(
            serde_json::from_value(json!({
                "transcript":"spoken words", "user_id":"7", "utterance_id":"utt-10",
                "language":"ko", "verbose_progress":true,
                "started_at":"2026-10-12T12:00:00Z", "completed_at":"2026-10-12T12:00:01Z",
                "samples_written":240, "control_channel_id":12,
                "stt_mode":"fixture", "stt_latency_ms":17
            }))
            .unwrap(),
        ),
    })
    .unwrap();
    Source::from_prepared(prepared).unwrap()
}

#[test]
fn c2_staged_receipt_pins_every_attachment_before_received_and_reopens_full_source() {
    let dir = sandbox();
    let mut slot = LedgerSlot::new(dir.path(), 9);
    let mut lease = slot.lend().unwrap();
    lease.get().unwrap();
    let original = prepared_source(10, true);
    let expected = original.input().clone();
    let identity = original.identity().clone();
    let before = OPENS.with(|opens| opens.get());
    let recording = Recording::start(dir.path());
    recording.arm(None);
    let Receipt::Accepted(receipt) = commit(&mut lease, original, true) else {
        panic!("staged receipt was not accepted");
    };
    assert_eq!(receipt.identity, identity);
    assert_eq!(OPENS.with(|opens| opens.get()) - before, 0);
    let events = recording.events();
    let wal_write = events
        .iter()
        .position(|event| event.starts_with("write:") && event.contains("/wal."))
        .unwrap();
    let blob_syncs: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| event.starts_with("file_sync:") && event.contains("/.blob-"))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(blob_syncs.len(), 2, "both staged blobs must be synced");
    assert!(blob_syncs.iter().all(|index| *index < wal_write));
    let pin_dir = format!(
        "dir_sync:{}/input_ledger/9/blobs/att/10",
        dir.path().display()
    );
    let pin_syncs: Vec<_> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| *event == &pin_dir)
        .map(|(index, _)| index)
        .collect();
    assert!(
        pin_syncs
            .iter()
            .any(|index| { *index > *blob_syncs.last().unwrap() && *index < wal_write })
    );
    drop(recording);
    slot.restore(lease);
    let ledger = slot.reopen().unwrap();
    let rows = ledger.rows().unwrap();
    assert_eq!(rows.folded_seq(), 1);
    let row = rows.row(10).unwrap();
    let pins: Vec<crate::services::tui_input::blob::BlobPin> =
        serde_json::from_value(row.input["blob_pins"].clone()).unwrap();
    assert_eq!(pins.len(), 2);
    for (pin, bytes) in pins.iter().zip([b"first".as_slice(), b"second".as_slice()]) {
        assert_eq!(ledger.read_blob(pin).unwrap(), bytes);
    }
    let mut canonical = row.input.clone();
    canonical.as_object_mut().unwrap().remove("blob_pins");
    assert_eq!(canonical, expected);
    assert_eq!(rows.receipt_identity(10), Some(identity));
}

#[test]
fn c2_exact_duplicate_returns_before_pin_or_unfetched_validation() {
    let dir = sandbox();
    let mut slot = LedgerSlot::new(dir.path(), 9);
    let mut lease = slot.lend().unwrap();
    let Receipt::Accepted(first) = commit(&mut lease, prepared_source(10, true), true) else {
        panic!("initial staged receipt was not accepted");
    };
    let saved = lease
        .get()
        .unwrap()
        .rows()
        .unwrap()
        .row(10)
        .unwrap()
        .input
        .clone();
    for fetched in [true, false] {
        let recording = Recording::start(dir.path());
        recording.arm(Some(1));
        assert_eq!(
            commit(&mut lease, prepared_source(10, fetched), false),
            Receipt::DuplicateQueued(first.clone())
        );
        assert!(recording.events().is_empty(), "duplicate must not repin");
        let rows = lease.get().unwrap().rows().unwrap();
        assert_eq!(rows.folded_seq(), 1);
        assert_eq!(rows.row(10).unwrap().input, saved);
    }
}

#[test]
fn c2_unfetched_or_corrupt_staging_never_appends_received() {
    for fetched in [false, true] {
        let dir = sandbox();
        let mut slot = LedgerSlot::new(dir.path(), 9);
        let mut lease = slot.lend().unwrap();
        lease.get().unwrap();
        if fetched {
            let target = dir.path().join("input_ledger/9/blobs/att/10");
            std::fs::create_dir_all(&target).unwrap();
            std::fs::write(target.join("0_0.txt"), b"corrupt existing pin").unwrap();
        }
        assert_eq!(
            commit(&mut lease, prepared_source(10, fetched), true),
            Receipt::Deferred(Deferred::Persistence)
        );
        assert!(lease.needs_reopen);
        assert_eq!(lease.get().unwrap().rows().unwrap().folded_seq(), 0);
    }
}

#[test]
fn c2_every_pin_failure_retains_no_receipt_and_retry_preserves_all_bytes() {
    let probe = sandbox();
    let mut probe_slot = LedgerSlot::new(probe.path(), 9);
    let mut probe_lease = probe_slot.lend().unwrap();
    probe_lease.get().unwrap();
    let recording = Recording::start(probe.path());
    recording.arm(None);
    assert!(matches!(
        commit(&mut probe_lease, prepared_source(10, true), true),
        Receipt::Accepted(_)
    ));
    let wal_write = recording
        .events()
        .iter()
        .position(|event| event.starts_with("write:") && event.contains("/wal."))
        .unwrap();
    assert!(wal_write > 0, "pin IO must precede the receipt");
    drop(recording);
    for fail_at in 1..=wal_write {
        let dir = sandbox();
        let mut slot = LedgerSlot::new(dir.path(), 9);
        let mut lease = slot.lend().unwrap();
        lease.get().unwrap();
        let recording = Recording::start(dir.path());
        recording.arm(Some(fail_at));
        assert_eq!(
            commit(&mut lease, prepared_source(10, true), true),
            Receipt::Deferred(Deferred::Persistence),
            "pin cut {fail_at}"
        );
        drop(recording);
        assert_eq!(lease.get().unwrap().rows().unwrap().folded_seq(), 0);
        assert!(matches!(
            commit(&mut lease, prepared_source(10, true), true),
            Receipt::Accepted(_)
        ));
        let ledger = lease.get().unwrap();
        let rows = ledger.rows().unwrap();
        assert_eq!(rows.folded_seq(), 1);
        let pins: Vec<crate::services::tui_input::blob::BlobPin> =
            serde_json::from_value(rows.row(10).unwrap().input["blob_pins"].clone()).unwrap();
        assert_eq!(pins.len(), 2);
        for (pin, bytes) in pins.iter().zip([b"first".as_slice(), b"second".as_slice()]) {
            assert_eq!(ledger.read_blob(pin).unwrap(), bytes);
        }
    }
}

#[test]
fn g1a_receipt_sync_and_enriched_alias_duplicate_keep_the_first_canonical_bytes() {
    let dir = sandbox();
    let mut slot = LedgerSlot::new(dir.path(), 9);
    let mut lease = slot.lend().unwrap();
    let accepted = commit(&mut lease, source(10, &[10, 11], "first"), true);
    let Receipt::Accepted(first) = accepted else {
        panic!("receipt was not accepted")
    };
    let saved = lease
        .get()
        .unwrap()
        .rows()
        .unwrap()
        .row(10)
        .unwrap()
        .input
        .clone();
    let seq = lease.get().unwrap().rows().unwrap().folded_seq();
    let duplicate = commit(
        &mut lease,
        source(11, &[11], "new reply/upload enrichment"),
        true,
    );
    assert_eq!(duplicate, Receipt::DuplicateQueued(first));
    let rows = lease.get().unwrap().rows().unwrap();
    assert_eq!(rows.folded_seq(), seq);
    assert_eq!(rows.row(10).unwrap().input, saved);
    assert!(rows.row(11).is_none());
    slot.restore(lease);
    let restored = slot.reopen().unwrap().rows().unwrap();
    assert_eq!(restored.row(10).unwrap().input, saved);
    assert_eq!(restored.folded_seq(), 1);
}

#[test]
fn g1a_receipt_write_or_sync_failure_has_no_ack_and_retry_uses_replay_evidence() {
    for fail_at in [1, 2] {
        let dir = sandbox();
        let mut slot = LedgerSlot::new(dir.path(), 9);
        let mut lease = slot.lend().unwrap();
        lease.get().unwrap();
        let recording = Recording::start(dir.path());
        recording.arm(Some(fail_at));
        assert_eq!(
            commit(&mut lease, source(10, &[10], "first"), true),
            Receipt::Deferred(Deferred::Persistence)
        );
        drop(recording);
        // A write may have reached the WAL despite its failed completion; no second Received follows.
        let retry = commit(&mut lease, source(10, &[10], "enriched"), true);
        assert!(matches!(
            retry,
            Receipt::DuplicateQueued(DurableReceipt {
                key: 10,
                received_seq: 1,
                ..
            })
        ));
        assert_eq!(lease.get().unwrap().rows().unwrap().folded_seq(), 1);
    }
}

#[test]
fn g1a_torn_retry_accepts_only_after_reopen_and_reopen_sync_failure_has_no_ack() {
    for torn in [true, false] {
        let dir = sandbox();
        let mut slot = LedgerSlot::new(dir.path(), 9);
        let mut lease = slot.lend().unwrap();
        lease.get().unwrap();
        let recording = Recording::start(dir.path());
        recording.arm(Some(1));
        assert_eq!(
            commit(&mut lease, source(10, &[10], "first"), true),
            Receipt::Deferred(Deferred::Persistence)
        );
        drop(recording);
        if torn {
            let wal = std::fs::read_dir(dir.path().join("input_ledger/9"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .find(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("wal.")
                })
                .unwrap();
            std::fs::OpenOptions::new()
                .write(true)
                .open(wal)
                .unwrap()
                .set_len(4)
                .unwrap();
            assert!(matches!(
                commit(&mut lease, source(10, &[10], "retry"), true),
                Receipt::Accepted(DurableReceipt {
                    received_seq: 1,
                    ..
                })
            ));
        } else {
            let recording = Recording::start(dir.path());
            lease.reopen().unwrap();
            let sync = recording
                .events()
                .iter()
                .position(|event| event.starts_with("file_sync:") && event.contains("/wal."))
                .unwrap();
            lease.needs_reopen = true;
            recording.arm(Some(sync + 1));
            assert_eq!(
                commit(&mut lease, source(10, &[10], "retry"), true),
                Receipt::Deferred(Deferred::Persistence)
            );
            drop(recording);
            assert!(matches!(
                commit(&mut lease, source(10, &[10], "retry"), true),
                Receipt::DuplicateQueued(DurableReceipt {
                    received_seq: 1,
                    ..
                })
            ));
        }
        assert_eq!(lease.get().unwrap().rows().unwrap().folded_seq(), 1);
    }
}

#[test]
fn g1a_receipt_uses_the_single_loan_without_opening_a_second_handle() {
    let dir = sandbox();
    let mut slot = LedgerSlot::new(dir.path(), 9);
    slot.get().unwrap();
    let mut lease = slot.lend().unwrap();
    assert!(matches!(slot.get(), Err(SlotError::Loaned)));
    assert!(matches!(slot.lend(), Err(SlotError::Loaned)));
    let before = OPENS.with(|opens| opens.get());
    assert!(matches!(
        commit(&mut lease, source(10, &[10], "first"), true),
        Receipt::Accepted(_)
    ));
    assert_eq!(OPENS.with(|opens| opens.get()) - before, 0);
    slot.restore(lease);
    assert_eq!(slot.get().unwrap().rows().unwrap().folded_seq(), 1);
}

#[test]
fn g1a_materialized_source_requires_exact_full_coverage_and_author() {
    let identity = ReceiptIdentity::new(10, vec![10, 11], 7, 8, 9).unwrap();
    for input in [
        json!({"text":"a"}),
        json!({"text":"a","source_message_ids":[10,12]}),
        json!({"text":"a","source_message_ids":[10,11],"author_id":6}),
        json!({"text":"a","source_message_ids":[10,11],"message_id":11}),
        json!({"text":"a","source_message_ids":[10,11],"receipt_identity":{"source_ids":[10]}}),
        json!({"source_message_ids":[10,11]}),
    ] {
        assert!(Source::new(10, identity.clone(), input, vec![]).is_err());
    }
    assert!(
        Source::new(
            10,
            identity.clone(),
            json!({"text":"a","source_message_ids":[11,10]}),
            vec![]
        )
        .is_ok()
    );
    assert!(
        Source::new(
            10,
            identity,
            json!({"text":"a","source_message_ids":[11,10],"receipt_identity":{
                "source_ids":[11,10],"author_id":7,"original_channel_id":8,"execution_channel_id":9
            }}),
            vec![]
        )
        .is_ok()
    );
}

#[test]
fn g1a_handback_rescan_retains_legacy_responsibility_and_appends_nothing() {
    for reason in [AbandonReason::Handback, AbandonReason::HandbackAmbiguous] {
        let dir = sandbox();
        let mut slot = LedgerSlot::new(dir.path(), 9);
        let mut lease = slot.lend().unwrap();
        let Receipt::Accepted(first) = commit(&mut lease, source(10, &[10, 11], "first"), true)
        else {
            panic!("initial receipt was not accepted")
        };
        lease
            .get()
            .unwrap()
            .append_entry(
                &Entry::Transition {
                    key: 10,
                    state: RowState::Abandoned(reason),
                    attempt: None,
                },
                &[],
            )
            .unwrap();
        let before = lease.get().unwrap().rows().unwrap().folded_seq();
        for permits_new in [true, false] {
            assert_eq!(
                commit(&mut lease, source(11, &[11], "alias retry"), permits_new),
                Receipt::LegacyResponsibility(first.clone())
            );
        }
        let rows = lease.get().unwrap().rows().unwrap();
        assert_eq!(rows.folded_seq(), before);
        assert!(rows.row(11).is_none());
    }
}

#[test]
fn g1a_receipt_requires_new_admission_but_positive_duplicates_do_not() {
    let dir = sandbox();
    let mut slot = LedgerSlot::new(dir.path(), 9);
    let mut lease = slot.lend().unwrap();
    assert_eq!(
        commit(&mut lease, source(10, &[10], "first"), false),
        Receipt::Deferred(Deferred::Order)
    );
    assert_eq!(lease.get().unwrap().rows().unwrap().folded_seq(), 0);
    let Receipt::Accepted(first) = commit(&mut lease, source(10, &[10], "first"), true) else {
        panic!("initial receipt was not accepted")
    };
    assert_eq!(
        commit(&mut lease, source(10, &[10], "enrichment"), false),
        Receipt::DuplicateQueued(first)
    );
    assert_eq!(lease.get().unwrap().rows().unwrap().folded_seq(), 1);
}

#[test]
fn g1a_ignored_received_on_tracked_handback_has_no_ack_and_requires_reopen() {
    use crate::services::tui_input::attempt::{AttemptMeta, Effect, Tracking, fresh_token};
    use crate::services::tui_input::rows::AttemptEvidence;
    use crate::services::tui_o::shadow::{ShadowProvider, SourceBinding, SourceId};

    let dir = sandbox();
    let mut slot = LedgerSlot::new(dir.path(), 9);
    let mut lease = slot.lend().unwrap();
    assert!(matches!(
        commit(&mut lease, source(10, &[10], "first"), true),
        Receipt::Accepted(_)
    ));
    let source_id = SourceId {
        session_id: "s".into(),
        path: "/nonexistent/s".into(),
        dev: 1,
        ino: 1,
    };
    let meta = AttemptMeta {
        generation: 1,
        token: fresh_token(),
        frame_digest: "ab".repeat(32),
        frame_profile: None,
        execution_nonce: "n1".into(),
        source: source_id.clone(),
        anchor: 0,
        effect: Effect::Intent,
        incarnation: None,
        queue_end: None,
    };
    lease
        .get()
        .unwrap()
        .append_tracked(
            10,
            RowState::Injecting,
            Some(AttemptEvidence {
                binding: SourceBinding {
                    channel_id: 9,
                    provider: ShadowProvider::Claude,
                    source: source_id,
                },
                execution_nonce: "n1".into(),
                eof: 0,
                rendered_prompt: "frame 10".into(),
                source_ids: vec![10],
                record_end: None,
                native_turn_id: None,
            }),
            &Tracking {
                attempt: Some(meta),
                ..Tracking::default()
            },
        )
        .unwrap();
    lease
        .get()
        .unwrap()
        .append_entry(
            &Entry::Transition {
                key: 10,
                state: RowState::Abandoned(AbandonReason::Handback),
                attempt: None,
            },
            &[],
        )
        .unwrap();
    assert!(
        lease
            .get()
            .unwrap()
            .rows()
            .unwrap()
            .keeps_tracked_history(10)
    );
    // Exercise the append guard independently of the earlier positive handback lookup.
    let result = append_active(lease.get().unwrap(), source(10, &[10], "forced Received"))
        .map(Receipt::Accepted);
    assert_eq!(
        finish(&mut lease, result),
        Receipt::Deferred(Deferred::Persistence)
    );
    assert!(lease.needs_reopen);
    let restored = lease.get().unwrap().rows().unwrap();
    assert_eq!(
        restored.row(10).unwrap().state,
        RowState::Abandoned(AbandonReason::Handback)
    );
    assert_eq!(restored.row(10).unwrap().since_seq, 1);
    assert_eq!(restored.ignored(), 1);
}

#[test]
fn g1a_old_null_receipt_holds_new_sources_and_has_no_ack() {
    let dir = sandbox();
    let mut slot = LedgerSlot::new(dir.path(), 9);
    let mut lease = slot.lend().unwrap();
    lease
        .get()
        .unwrap()
        .append_entry(
            &Entry::Received {
                key: 9,
                input: serde_json::Value::Null,
            },
            &[],
        )
        .unwrap();
    assert!(
        lease
            .get()
            .unwrap()
            .rows()
            .unwrap()
            .receipt_identity_missing()
    );
    assert_eq!(
        commit(&mut lease, source(10, &[10], "new"), true),
        Receipt::Deferred(Deferred::Unknown)
    );
    assert_eq!(lease.get().unwrap().rows().unwrap().folded_seq(), 1);
}
