use super::*;

#[path = "operator_resume_delivery_tests.rs"]
mod delivery;

#[tokio::test]
async fn operator_resume_latest_legacy_posted_does_not_reblock_old_rejection() {
    let harness = Harness::new();
    harness.gate.acquired();
    let mut channel = harness.channel();
    for entry in [
        LedgerEntry::Prepared {
            serial: 0,
            unit_key: unit("old"),
            piece_index: 0,
            payload: "original".into(),
            anchor_id: 100,
            epoch: 1,
        },
        LedgerEntry::Rejected {
            serial: 0,
            status: 403,
        },
        LedgerEntry::Prepared {
            serial: 1,
            unit_key: unit("old"),
            piece_index: 0,
            payload: "original".into(),
            anchor_id: 100,
            epoch: 1,
        },
        LedgerEntry::Posted {
            serial: 1,
            msg_id: 200,
        },
    ] {
        channel.append_ledger(entry).unwrap();
    }
    drop(channel);
    let mut writer = harness.writer();
    assert!(!writer.is_stopped());
    assert_eq!(
        writer.deliver(&piece("later", "following")).await,
        Step::Done
    );
    assert_eq!(harness.port.posts(), ["following"]);
    assert_eq!(writer.store().ledger().violation(), None);
}
