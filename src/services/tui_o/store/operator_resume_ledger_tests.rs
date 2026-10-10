use super::*;
use crate::services::tui_o::shadow::{ShadowProvider, UnitKind};

fn key() -> UnitKey {
    UnitKey {
        channel_id: 7,
        provider: ShadowProvider::Claude,
        native_key: "original".into(),
        kind: UnitKind::Body,
    }
}

fn prepared(serial: u64) -> LedgerEntry {
    LedgerEntry::Prepared {
        serial,
        unit_key: key(),
        piece_index: 0,
        payload: "original".into(),
        anchor_id: 100,
        epoch: 1,
    }
}

fn approval(serial: u64) -> LedgerEntry {
    LedgerEntry::OperatorResume {
        rejected_serial: serial,
        approval_id: uuid::Uuid::new_v4(),
        operator: "operator".into(),
        reason: "restored".into(),
        at: Utc::now(),
    }
}

fn rejected() -> LedgerState {
    let mut state = LedgerState {
        anchor: 100,
        ..LedgerState::default()
    };
    state.apply(Utc::now(), prepared(0));
    state.apply(
        Utc::now(),
        LedgerEntry::Rejected {
            serial: 0,
            status: 403,
        },
    );
    state
}

#[test]
fn operator_resume_exact_evidence_and_frontiers_survive_approval_and_read_only_replay() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.jsonl");
    File::create(&path).unwrap();
    let metadata = crate::services::tui_o::exact_episode::tests::fixture().remove(4);
    let now = Utc::now();
    for entry in [
        prepared(0),
        LedgerEntry::Rejected {
            serial: 0,
            status: 403,
        },
        LedgerEntry::ExactEvidence {
            metadata: Box::new(metadata.clone()),
        },
    ] {
        append(&path, now, &entry).unwrap();
    }
    let before = recover(&path, 100).unwrap();
    append(&path, now, &approval(0)).unwrap();
    let after = recover(&path, 100).unwrap();
    assert_eq!(after.exact_evidence, [metadata]);
    assert_eq!(
        (after.anchor, after.next_serial, after.open_serial),
        (before.anchor, before.next_serial, before.open_serial)
    );
    assert_eq!(
        (
            &after.latest,
            &after.pieces,
            &after.resolved,
            &after.gc,
            &after.excluded
        ),
        (
            &before.latest,
            &before.pieces,
            &before.resolved,
            &before.gc,
            &before.excluded
        )
    );
    let mut file = OpenOptions::new().read(true).open(path).unwrap();
    assert_eq!(read_only(&mut file, 100).unwrap(), after);
}

#[test]
fn operator_resume_first_prepared_connection_uses_row_order_and_never_rebinds() {
    let mut state = rejected();
    state.apply(Utc::now(), approval(0));
    let audit = state.approval(0).unwrap().clone();
    state.apply(Utc::now() - chrono::Duration::days(1), prepared(1));
    assert_eq!(state.approval(0).unwrap().consumed_serial, Some(1));
    state.apply(
        Utc::now(),
        LedgerEntry::Unresolved {
            serial: 1,
            reason: "unreadable history".into(),
        },
    );
    state.apply(Utc::now(), prepared(2));
    assert_eq!(state.approval(0).unwrap().consumed_serial, Some(1));
    state.apply(Utc::now(), approval(0));
    assert_eq!(state.approval(0).unwrap().approval_id, audit.approval_id);
    assert_eq!(state.violation(), None);
}

#[test]
fn operator_resume_invalid_and_duplicate_records_neither_grant_nor_violate() {
    for serial in [0, 9] {
        let mut state = rejected();
        if serial == 0 {
            state.apply(Utc::now(), LedgerEntry::NotFound { serial: 0 });
        }
        let before = state.clone();
        state.apply(Utc::now(), approval(serial));
        assert_eq!(state, before);
    }
    let mut state = rejected();
    state.apply(Utc::now(), approval(0));
    let before = state.clone();
    state.apply(Utc::now(), approval(0));
    assert_eq!(state, before);
}

#[test]
fn operator_resume_uncertain_latest_attempt_resolves_old_rejection_without_another_post() {
    for outcome in [
        LedgerEntry::NotFound { serial: 1 },
        LedgerEntry::Ambiguous {
            serial: 1,
            candidates: vec![201, 202],
        },
        LedgerEntry::Unresolved {
            serial: 1,
            reason: "history disabled".into(),
        },
    ] {
        let mut state = rejected();
        state.apply(Utc::now(), approval(0));
        state.apply(Utc::now(), prepared(1));
        assert_eq!(
            state.resume_pieces().count(),
            1,
            "open linked retry still owes settlement"
        );
        state.apply(Utc::now(), outcome);
        assert_eq!(state.disposition(&key(), 0), PieceDisposition::Settled);
        assert_eq!(state.blocked(), None);
        assert_eq!(state.resume_pieces().count(), 0);
    }
}

#[cfg(unix)]
#[test]
fn operator_resume_unsent_withdrawal_restores_only_local_approval_consumption() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.jsonl");
    File::create(&path).unwrap();
    for entry in [
        prepared(0),
        LedgerEntry::Rejected {
            serial: 0,
            status: 403,
        },
        approval(0),
        prepared(1),
    ] {
        append(&path, Utc::now(), &entry).unwrap();
    }
    let survived = recover(&path, 100).unwrap();
    assert_eq!(survived.approval(0).unwrap().consumed_serial, Some(1));
    let unsent = Unsent::new(prepared(1)).unwrap();
    let (restored, withdrew) = recover_withdrawing(&path, 100, Some(&unsent)).unwrap();
    assert!(withdrew);
    assert_eq!(restored.approval(0).unwrap().consumed_serial, None);
    assert_eq!(
        restored.disposition(&key(), 0),
        PieceDisposition::Authorized { rejected_serial: 0 }
    );
    assert_eq!(restored.next_serial(), 1);
}
