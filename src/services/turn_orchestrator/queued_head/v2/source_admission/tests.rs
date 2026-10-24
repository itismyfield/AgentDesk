use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::discord::runtime_store::durable_probe::{Plan, Step, Watch};
use crate::services::turn_orchestrator::ChannelMailboxRegistry;
use poise::serenity_prelude::ChannelId;
use serde_json::json;

fn original(id: u64, text: &str) -> PendingQueueItem {
    serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":7272,
        "source_message_ids":[id],"queued_generation":5,"text":text}))
    .unwrap()
}

fn fixture_path(channel: u64) -> std::path::PathBuf {
    crate::services::turn_orchestrator::pending_queue_persistence::pending_queue_file_path(
        &crate::services::provider::ProviderKind::Claude,
        "s3b",
        ChannelId::new(channel),
    )
    .unwrap()
}

fn owner() -> Owner {
    Owner::adopt(fixture_path(727370), "s3b".into()).unwrap()
}

#[test]
fn pending_synthetic_admission_recovers_the_same_candidate_without_reenqueue() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner();
    let original = original(
        8_000_000_000_000_001_000,
        "synthetic original\n_ WIP literal",
    );
    let mut custody = Custody::new(&original);
    let watch = Watch::new(
        &owner.path,
        Plan {
            fail: vec![Step::ParentSync],
            ..Default::default()
        },
    );
    assert_eq!(
        custody.advance(&mut owner),
        Err(AdmitError::Pending),
        "uncertain publish must not acknowledge admission"
    );
    assert_eq!(
        serde_json::to_value(&custody.original).unwrap(),
        serde_json::to_value(&original).unwrap()
    );
    assert!(
        watch
            .events()
            .iter()
            .any(|event| event.step == Step::ParentSync && !event.ok)
    );
    drop(watch);
    owner.recover().unwrap();
    let ack = custody.advance(&mut owner).unwrap();
    assert_eq!(
        owner.snapshot.items.len(),
        1,
        "synthetic retry must reuse the published candidate, never enqueue twice"
    );
    assert_eq!(ack, Admitted::Queued);
    assert_eq!(
        custody.advance(&mut owner),
        Ok(Admitted::Duplicate),
        "accepted custody acknowledges exactly once"
    );
}

#[test]
fn not_committed_full_and_duplicate_keep_distinct_custody() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner();
    let original = original(100, "raw body");
    let mut custody = Custody::new(&original);
    owner.fault = Fault::Before;
    assert!(
        matches!(
            custody.advance(&mut owner),
            Err(AdmitError::NotCommitted(_))
        ),
        "failed publication cannot be accepted"
    );
    assert_eq!(owner.snapshot.items.len(), 0);
    owner.fault = Fault::None;
    assert_eq!(custody.advance(&mut owner), Ok(Admitted::Queued));
    assert_eq!(
        Custody::new(&original).advance(&mut owner),
        Ok(Admitted::Duplicate)
    );
    for id in 101..130 {
        owner.enqueue(&self::original(id, "other")).unwrap();
    }
    let mut full = Custody::new(&self::original(200, "not dropped"));
    assert_eq!(full.advance(&mut owner), Err(AdmitError::Full));
    assert_eq!(full.original.text, "not dropped");
}

#[test]
fn mixed_sources_partition_only_explicit_text_and_retain_opaque_originals() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner();
    owner.enqueue(&original(100, "known A")).unwrap();
    let mut mixed = original(100, "known A\nfresh B");
    mixed.source_message_ids = vec![100, 200];
    mixed.source_text_segments = serde_json::from_value(json!([
        {"message_id":100,"text":"known A"},{"message_id":200,"text":"fresh B"}]))
    .unwrap();
    let before = serde_json::to_value(&mixed).unwrap();
    assert_eq!(
        Custody::new(&mixed).advance(&mut owner),
        Ok(Admitted::Queued),
        "known source must not veto fresh B"
    );
    assert_eq!(owner.snapshot.items[1].message_id, 200);
    assert_eq!(
        owner.snapshot.items[1].text, "fresh B",
        "known source must not veto fresh B"
    );
    assert_eq!(serde_json::to_value(&mixed).unwrap(), before);
    mixed.source_message_ids = vec![100, 300];
    mixed.reply_context = Some("legacy rendered reply with no source provenance".into());
    assert_eq!(
        Custody::new(&mixed).advance(&mut owner),
        Ok(Admitted::RetainedOpaque)
    );
    assert_eq!(owner.snapshot.items.len(), 2);
    assert_eq!(
        serde_json::to_value(&owner.snapshot.retained_inputs[0].input.original).unwrap(),
        serde_json::to_value(mixed).unwrap()
    );
}

#[tokio::test]
async fn two_mailbox_requests_share_custody_and_ack_once() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(ChannelId::new(727_371));
    handle
        .v2_adopt(fixture_path(727371), "two".into())
        .await
        .unwrap();
    let custody = Arc::new(Mutex::new(Custody::new(&original(
        8_000_000_000_000_001_100,
        "same input",
    ))));
    let (a, b) = tokio::join!(handle.v2_admit(custody.clone()), handle.v2_admit(custody));
    let results = [a.unwrap(), b.unwrap()];
    assert_eq!(
        results.iter().filter(|r| **r == Admitted::Queued).count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|r| **r == Admitted::Duplicate)
            .count(),
        1
    );
    assert_eq!(
        handle
            .v2_request(|s| Ok(s.as_ref().unwrap().snapshot.items.len()))
            .await
            .unwrap(),
        1
    );
}

#[cfg(unix)]
mod pg_tests;

#[tokio::test]
async fn pending_a_and_fresh_b_keep_separate_originals_and_finish_once() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(ChannelId::new(727_375));
    let path = fixture_path(727375);
    handle
        .v2_adopt(path.clone(), "two-originals".into())
        .await
        .unwrap();
    let a = Arc::new(Mutex::new(Custody::new(&original(
        8_000_000_000_000_001_500,
        "A",
    ))));
    let b = Arc::new(Mutex::new(Custody::new(&original(
        8_000_000_000_000_001_500,
        "B",
    ))));
    let watch = Watch::new(
        &path,
        Plan {
            fail: vec![Step::ParentSync],
            ..Default::default()
        },
    );
    assert_eq!(handle.v2_admit(a.clone()).await, Err(AdmitError::Pending));
    assert!(matches!(
        handle.v2_admit(b.clone()).await,
        Err(AdmitError::NotCommitted(_))
    ));
    drop(watch);
    assert_eq!(handle.v2_admit(a).await, Ok(Admitted::Queued));
    assert_eq!(handle.v2_admit(b).await, Ok(Admitted::Queued));
    let texts = handle
        .v2_request(|s| {
            Ok(s.as_ref()
                .unwrap()
                .snapshot
                .items
                .iter()
                .map(|i| i.text.clone())
                .collect::<Vec<_>>())
        })
        .await
        .unwrap();
    assert_eq!(
        texts,
        vec!["A", "B"],
        "one uncertain input cannot acknowledge or discard the next original"
    );
}

#[test]
fn s3d_regression_admission_receipt_survives_later_owner_commits() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner();
    let mut custody = Custody::new(&original(8_000_000_000_000_001_310, "synthetic original"));
    let watch = Watch::new(
        &owner.path,
        Plan {
            fail: vec![Step::ParentSync],
            ..Default::default()
        },
    );
    assert_eq!(custody.advance(&mut owner), Err(AdmitError::Pending));
    drop(watch);
    owner.recover().unwrap();
    owner
        .enqueue_fixture(&original(8_000_000_000_000_001_311, "later original"))
        .unwrap();
    assert_eq!(
        custody.advance(&mut owner),
        Ok(Admitted::Queued),
        "committed admission must survive later snapshot changes"
    );
    assert_eq!(
        owner.snapshot.items.len(),
        2,
        "pending custody cannot reenqueue its synthetic original"
    );
    assert_eq!(custody.advance(&mut owner), Ok(Admitted::Duplicate));
}

#[test]
fn s3d_regression_opaque_custody_is_unsent_with_retry_notice() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner();
    owner.enqueue_fixture(&original(100, "known A")).unwrap();
    let mut opaque = original(100, "inseparable A and fresh B");
    opaque.source_message_ids = vec![100, 200];
    assert_eq!(
        Custody::new(&opaque).advance(&mut owner),
        Ok(Admitted::RetainedOpaque)
    );
    let entry = &owner.snapshot.retained_inputs[0];
    assert_eq!(
        entry.input.phase,
        HeadPhase::Reserved,
        "opaque original was never attempted"
    );
    assert_eq!(
        entry.notice.kind, "retained_retry_required",
        "unsent retention must not claim delivery uncertainty"
    );
    assert!(entry.input.attempt.is_none());
}
