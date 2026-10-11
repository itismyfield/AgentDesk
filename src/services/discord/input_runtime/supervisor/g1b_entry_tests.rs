//! G1b P2-1 on the real loop: a merged receipt deferred by an append-time failure retries as the
//! same merged prefix once the cause is gone, and the next source follows.
use super::*;

fn missing_blob(rig: &Rig, key: u64, ids: Vec<u64>) -> Source {
    let pin = crate::services::tui_input::blob::BlobPin {
        local_path: format!("blobs/att/{key}/0_missing.txt").into(),
        sha256: "0".repeat(64),
        pinned: true,
    };
    let identity = ReceiptIdentity::new(key, ids.clone(), 7, rig.channel, rig.channel).unwrap();
    Source::new(
        key,
        identity,
        json!({"text":"merged","source_message_ids":ids}),
        vec![pin],
    )
    .unwrap()
}

async fn merged_retry(channel: u64, primary: u64) {
    let rig = Rig::new(channel);
    let screen = Arc::new(Screen::default());
    let mut driven = Driven::start(admitted(&rig, &[]).await, &screen).await;
    let before = wal_seq(&mut driven);
    let cap = scan_page(&mut driven, vec![10, 11, 12], 12).await;
    let failed = commit_merged(&mut driven, missing_blob(&rig, primary, vec![10, 11]), cap).await;
    assert_eq!(failed.receipt, Receipt::Deferred(Deferred::Persistence));
    assert_eq!(
        failed.settlement,
        Ok(()),
        "the first deferral itself settles"
    );
    assert_eq!(
        wal_seq(&mut driven),
        before,
        "the blob check refuses before append"
    );
    let cap = scan_page(&mut driven, vec![10, 11, 12], 12).await;
    let retried = commit_merged(&mut driven, merged(&rig, primary, vec![10, 11]), cap).await;
    assert!(
        matches!(retried.receipt, Receipt::Accepted(_)),
        "{:?}",
        retried.receipt
    );
    assert_eq!(retried.settlement, Ok(()));
    assert_eq!(wal_seq(&mut driven), before + 1);
    let next = commit_source(&mut driven, &rig, 12, retried.capability).await;
    assert!(matches!(next.receipt, Receipt::Accepted(_)));
    assert_eq!(next.settlement, Ok(()));
    let (reply, response) = oneshot::channel();
    let complete = SupervisorCmd::CompleteScan {
        capability: next.capability,
        reply,
    };
    driven.supervisor.input_command(complete, true).await;
    assert!(response.await.unwrap().is_ok());
    assert!(driven.release());
}

#[tokio::test]
async fn g1b_merge_retry_primary_first() {
    merged_retry(6_326_180, 10).await;
}

#[tokio::test]
async fn g1b_merge_retry_primary_nonfirst() {
    merged_retry(6_326_181, 11).await;
}
