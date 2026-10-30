use super::*;
use crate::services::tui_o::shadow::{SourceId, SourceRange};
use crate::services::tui_o::store::fault::{self, Keep, Step as At};
use crate::services::tui_o::store::ledger::input_notices::{
    MigrationHistory, MigrationSnapshot, NoticeEntry, NoticeEvidence,
};
use crate::services::tui_o::writer::input_notices::{
    NoticeOrigin,
    admission::tests::{BindingRoot, bind},
};
use sha2::{Digest, Sha256};

pub(super) fn materialized_notice(
    harness: &Harness,
    native_key: &str,
    prompt: &str,
) -> (BindingRoot, PieceWork) {
    let raw = serde_json::to_vec(&serde_json::json!({
        "type":"user", "uuid":native_key,
        "message":{"role":"user", "content":prompt},
    }))
    .unwrap();
    let path = harness._runtime.path().join(format!("{native_key}.jsonl"));
    let mut bytes = raw.clone();
    bytes.push(b'\n');
    std::fs::write(&path, bytes).unwrap();
    let (dev, ino) =
        crate::services::tui_o::shadow::capture::file_identity(&std::fs::metadata(&path).unwrap());
    let source = SourceId {
        session_id: "native-session".into(),
        path,
        dev,
        ino,
    };
    let logical = format!("notice-transport-{native_key}");
    let nonce = "73320000000000000000000000000120";
    let bindings = bind(harness._runtime.path(), CHANNEL, &source, &logical, nonce);
    let events = crate::services::tui_prompt_dedupe::binding_events::records_strict(CHANNEL)
        .unwrap()
        .unwrap();
    let origin = Box::new(NoticeOrigin {
        record: SourceRange {
            source,
            start: 0,
            end: raw.len() as u64 + 1,
        },
        logical_session: logical,
        binding_seq: events.last().unwrap().seq,
        execution_nonce: nonce.into(),
    });
    let item = PieceWork {
        output_origin: None,
        unit_key: UnitKey {
            kind: UnitKind::InputNotice,
            ..unit(native_key)
        },
        index: 0,
        payload: crate::services::discord::input_notice_content(prompt).unwrap(),
        notice_origin: Some(origin.clone()),
    };
    let mut store = harness.channel();
    if store.ledger().input_notices().migration.is_none() {
        let anchor = store.init().initial_anchor;
        store
            .append_ledger(LedgerEntry::InputNotice {
                entry: Box::new(NoticeEntry::MigrationStarted {
                    snapshot: MigrationSnapshot {
                        sources: Vec::new(),
                        binding_checkpoint: events.last().unwrap().seq,
                        history: MigrationHistory::Fixed {
                            lower: anchor,
                            upper: anchor,
                        },
                    },
                }),
            })
            .unwrap();
    }
    store
        .append_ledger(LedgerEntry::InputNotice {
            entry: Box::new(NoticeEntry::Materialized {
                evidence: Box::new(NoticeEvidence {
                    record: origin.record.clone(),
                    raw_record: raw,
                    prompt: Some(prompt.into()),
                    prompt_sha256: Some(hex::encode(Sha256::digest(prompt.as_bytes()))),
                    unit_key: Some(item.unit_key.clone()),
                    payload: Some(item.payload.clone()),
                    origin: Some(origin),
                }),
            }),
        })
        .unwrap();
    assert!(store.ledger().violation().is_none());
    (bindings, item)
}

#[tokio::test(start_paused = true)]
async fn input_notice_posted_before_outcome_failure_recovers_exact_receipt_without_reposting() {
    let _guard = crate::config::TestRuntimeRootGuard::new();
    let harness = Harness::new();
    harness.gate.acquired();
    let (_bindings, item) = materialized_notice(&harness, "crash-input", "terminal before crash");
    let origin = item.notice_origin.clone();
    let ledger = ledger_path(&harness);
    let planted = Arc::new(Mutex::new(None));
    let planting = Arc::clone(&planted);
    *harness.port.on_post.lock().unwrap() = Some(Box::new(move || {
        *planting.lock().unwrap() = Some(fault::plant(
            &ledger,
            At::Append(Keep::Nothing),
            std::io::ErrorKind::StorageFull,
            Some(1),
        ));
    }));
    harness
        .port
        .replies
        .lock()
        .unwrap()
        .push_back(Reply::Created);
    let mut writer = harness.writer();
    assert_eq!(
        writer.deliver(&Derived::Piece(item.clone())).await,
        Step::Stopped
    );
    *harness.port.on_post.lock().unwrap() = None;
    drop(planted.lock().unwrap().take());
    assert_eq!(
        harness.port.posts().as_slice(),
        std::slice::from_ref(&item.payload)
    );
    let message = harness.port.history.lock().unwrap()[0].clone();
    assert_eq!(message.author_id, BOT);
    assert_eq!(message.content, item.payload);
    let (serial, prepared) = writer.store().ledger().unresolved().unwrap();
    assert_eq!(serial, 0);
    assert_eq!(prepared.notice_origin, origin);
    assert_eq!(prepared.outcome, None);
    drop(writer);

    let mut reopened = harness.writer();
    assert_eq!(
        reopened.store().ledger().piece(0).unwrap().notice_origin,
        origin
    );
    assert_eq!(
        reopened.deliver(&Derived::Piece(item.clone())).await,
        Step::Done
    );
    assert!(harness.port.history_reads.load(Ordering::SeqCst) > 0);
    let ledger = reopened.store().ledger();
    assert!(ledger.unresolved().is_none());
    assert_eq!(ledger.next_serial(), 1);
    assert_eq!(
        ledger.piece(0).unwrap().outcome,
        Some(PieceOutcome::Posted(message.id))
    );
    assert_eq!(ledger.piece(0).unwrap().notice_origin, origin);
    drop(reopened);
    let mut reopened = harness.writer();
    assert_eq!(
        reopened.deliver(&Derived::Piece(item.clone())).await,
        Step::Done
    );
    assert_eq!(harness.port.posts(), [item.payload]);
    assert_eq!(reopened.store().ledger().next_serial(), 1);
}

#[tokio::test(start_paused = true)]
async fn original_retry_and_approved_notice_share_clear_first_without_preparing_or_posting() {
    let _guard = crate::config::TestRuntimeRootGuard::new();
    for mode in ["original", "retry", "approval"] {
        let harness = Harness::new();
        harness.gate.acquired();
        let (_bindings, item) = materialized_notice(&harness, mode, "terminal before clear");
        if mode != "original" {
            let mut store = harness.channel();
            store
                .append_ledger(LedgerEntry::Prepared {
                    serial: 0,
                    unit_key: item.unit_key.clone(),
                    piece_index: 0,
                    payload: item.payload.clone(),
                    anchor_id: store.init().initial_anchor,
                    epoch: 1,
                    local_retry: (mode == "retry")
                        .then_some(crate::services::tui_o::store::ledger::LocalRetryMark::Budget),
                    notice_origin: item.notice_origin.clone(),
                })
                .unwrap();
            if mode == "retry" {
                store
                    .append_ledger(LedgerEntry::NotFound { serial: 0 })
                    .unwrap();
            } else {
                store
                    .append_ledger(LedgerEntry::Rejected {
                        serial: 0,
                        status: 403,
                    })
                    .unwrap();
                let at = Utc::now();
                let entry = LedgerEntry::OperatorResume {
                    rejected_serial: 0,
                    approval_id: uuid::Uuid::new_v4(),
                    operator: "test".into(),
                    reason: "retry original input".into(),
                    at,
                };
                // Restore an old approval through the reader; live writes use their dedicated path.
                let mut line =
                    serde_json::to_vec(&serde_json::json!({"at":at,"entry":entry})).unwrap();
                line.push(b'\n');
                use std::io::Write;
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(ledger_path(&harness))
                    .unwrap()
                    .write_all(&line)
                    .unwrap();
            }
            assert!(store.ledger().violation().is_none());
        }
        let origin = item.notice_origin.as_ref().unwrap();
        let end = crate::services::tui_o::store::clear_end::ClearEnd {
            channel: CHANNEL,
            provider: "claude".into(),
            tmux_session: origin.logical_session.clone(),
            execution_nonce: origin.execution_nonce.clone(),
            source: origin.record.source.clone(),
            binding_seq: origin.binding_seq,
            host: None,
            ended_at: Utc::now(),
            basis: "confirmed_clear".into(),
        };
        assert_eq!(
            crate::services::tui_o::store::clear_end::append(harness._runtime.path(), end).unwrap(),
            crate::services::tui_o::store::clear_end::Appended::Durable
        );
        let mut writer = harness.writer();
        let before = writer.store().ledger().next_serial();
        let step = if mode == "retry" {
            writer
                .retry_local(Utc::now() + chrono::Duration::days(1))
                .await
        } else {
            writer.deliver(&Derived::Piece(item.clone())).await
        };
        assert!(matches!(step, Step::Done | Step::Withheld));
        assert!(!writer.is_stopped());
        assert_eq!(writer.store().ledger().next_serial(), before);
        assert!(writer.store().ledger().excluded(&item.unit_key).is_some());
        assert!(harness.port.posts().is_empty());
        if before != 0 {
            assert_eq!(
                writer.store().ledger().piece(0).unwrap().notice_origin,
                item.notice_origin
            );
        }
        drop(writer);
        let mut writer = harness.writer();
        assert_eq!(writer.deliver(&Derived::Piece(item)).await, Step::Done);
        assert_eq!(writer.store().ledger().next_serial(), before);
        assert!(harness.port.posts().is_empty());
        assert_eq!(
            writer
                .deliver(&piece("later-body", "body after clear"))
                .await,
            Step::Done
        );
        assert_eq!(harness.port.posts(), ["body after clear"]);
    }
}
