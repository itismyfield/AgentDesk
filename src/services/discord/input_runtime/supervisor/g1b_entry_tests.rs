//! The real loop: a merged receipt deferred by an append-time failure retries as the
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

async fn lookup(driven: &mut Driven, rig: &Rig, alias: u64) -> Responsibility {
    let identity = ReceiptIdentity::new(alias, vec![alias], 7, rig.channel, rig.channel).unwrap();
    let (reply, response) = oneshot::channel();
    let command = SupervisorCmd::LookupResponsibility { identity, reply };
    driven.supervisor.input_command(command, true).await;
    response.await.unwrap()
}

#[tokio::test]
async fn g1b_snapshot_conflict_holds_aliases_and_actor() {
    for (index, conflict) in [false, true].into_iter().enumerate() {
        let rig = Rig::new(6_326_190 + index as u64);
        let a = idle_transcript(&rig, "a", false);
        push(&rig, "n1", source(None, &a));
        let screen = Arc::new(Screen::default());
        let mut supervisor = admitted(&rig, &[]).await;
        let ledger = supervisor.slot().get().unwrap();
        let identity = ReceiptIdentity::new(41, vec![41, 42], 7, rig.channel, rig.channel);
        let input = json!({"text":"input 41","receipt_identity":identity.unwrap()});
        let received = Entry::Received { key: 41, input };
        let received_seq = ledger.append_entry(&received, &[]).unwrap();
        if conflict {
            // Both identities are complete, but the retained one names 43 instead of 42.
            let mut state = serde_json::to_value(ledger.rows().unwrap()).unwrap();
            let other = ReceiptIdentity::new(41, vec![41, 43], 7, rig.channel, rig.channel);
            state["receipts"] = json!({"41": other.unwrap()});
            ledger.checkpoint(state).unwrap();
            assert!(
                ledger.rows().is_err(),
                "the ledger opens but its rows do not fold"
            );
        }
        let mut driven = Driven::start(supervisor, &screen).await;
        driven.idle(4).await;
        let (aliases, sent) = (
            [
                lookup(&mut driven, &rig, 42).await,
                lookup(&mut driven, &rig, 43).await,
            ],
            driven.sent(),
        );
        if conflict {
            assert_eq!(aliases, [Responsibility::Unknown; 2]);
            assert_eq!(sent, 0, "no actor submits from a conflicting snapshot");
            assert_eq!(rig.gate.mode(), Mode::Held);
        } else {
            let known = Responsibility::Known {
                key: 41,
                received_seq,
            };
            assert_eq!(aliases, [known, Responsibility::Absent]);
            assert_eq!(sent, 1, "the control without the conflict submits");
        }
        assert!(driven.release());
    }
}
