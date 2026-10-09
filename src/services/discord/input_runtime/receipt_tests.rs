use super::*;
use crate::services::tui_input::durability_tests::supported::Recording;
use crate::services::tui_input::ledger::{LedgerSlot, OPENS, SlotError};
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

#[test]
fn g1a_receipt_sync_and_enriched_alias_duplicate_keep_the_first_canonical_bytes() {
    let dir = sandbox();
    let mut slot = LedgerSlot::new(dir.path(), 9);
    let mut lease = slot.lend().unwrap();
    let accepted = commit(&mut lease, source(10, &[10, 11], "first"));
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
    let duplicate = commit(&mut lease, source(11, &[11], "new reply/upload enrichment"));
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
            commit(&mut lease, source(10, &[10], "first")),
            Receipt::Deferred(Deferred::Persistence)
        );
        drop(recording);
        // A write may have reached the WAL despite its failed completion; no second Received follows.
        let retry = commit(&mut lease, source(10, &[10], "enriched"));
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
            commit(&mut lease, source(10, &[10], "first")),
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
                commit(&mut lease, source(10, &[10], "retry")),
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
                commit(&mut lease, source(10, &[10], "retry")),
                Receipt::Deferred(Deferred::Persistence)
            );
            drop(recording);
            assert!(matches!(
                commit(&mut lease, source(10, &[10], "retry")),
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
        commit(&mut lease, source(10, &[10], "first")),
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
