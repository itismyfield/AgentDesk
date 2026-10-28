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

type Handle = crate::services::turn_orchestrator::ChannelMailboxHandle;

/// An actor that adopted `channel`'s fixture file; the registry keeps it alive.
async fn ticket_owner(channel: u64, namespace: &str) -> (ChannelMailboxRegistry, Handle) {
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(ChannelId::new(channel));
    handle
        .v2_adopt(fixture_path(channel), namespace.into())
        .await
        .unwrap();
    (registry, handle)
}

/// The owner's queued texts and admission receipts.
async fn held(handle: &Handle) -> (Vec<String>, Vec<String>) {
    handle
        .v2_request(|slot| {
            let owner = slot.as_ref().ok_or("no owner")?;
            let texts = owner
                .snapshot
                .items
                .iter()
                .map(|i| i.text.clone())
                .collect();
            Ok((texts, owner.snapshot.admission_receipts.clone()))
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn stable_ticket_readmission_is_duplicate() {
    let _root = TestRuntimeRootGuard::new();
    let (_registry, handle) = ticket_owner(727_381, "tickets").await;
    let input = original(8_000_000_000_000_001_810, "routine prompt");
    let ticket = || "run-7272-a".to_string();
    assert_eq!(
        handle.v2_admit_ticket(input.clone(), ticket()).await,
        Ok(Admitted::Queued)
    );
    assert_eq!(
        handle.v2_admit_ticket(input, ticket()).await,
        Ok(Admitted::Duplicate),
        "the same ticket is acknowledged once"
    );
    let (texts, receipts) = held(&handle).await;
    assert_eq!(texts, ["routine prompt"]);
    assert_eq!(receipts.len(), 1);
    assert!(receipts[0].starts_with("tk:run-7272-a:"), "{receipts:?}");
    assert_eq!(
        handle.v2_admission_status(ticket()).await,
        Ok(TicketStatus::Committed)
    );
}

#[tokio::test]
async fn pending_ticket_resumes_after_parent_sync_fault_and_restart() {
    let _root = TestRuntimeRootGuard::new();
    let channel = ChannelId::new(727_382);
    let path = fixture_path(channel.get());
    let (first, handle) = ticket_owner(channel.get(), "tickets").await;
    let input = original(8_000_000_000_000_001_820, "routine prompt");
    let ticket = || "run-7272-b".to_string();
    let watch = Watch::new(
        &path,
        Plan {
            fail: vec![Step::ParentSync],
            ..Default::default()
        },
    );
    assert_eq!(
        handle.v2_admit_ticket(input.clone(), ticket()).await,
        Err(AdmitError::Pending)
    );
    assert_eq!(
        handle.v2_admission_status(ticket()).await,
        Ok(TicketStatus::Pending)
    );
    drop(watch);
    assert_eq!(
        handle.v2_admit_ticket(input.clone(), ticket()).await,
        Ok(Admitted::Queued),
        "the retry resolves the same receipt and is acknowledged once"
    );

    // A restart re-adopts the owner from disk; the ticket resumes instead of admitting again.
    drop((handle, first));
    let registry = ChannelMailboxRegistry::default();
    let persistence = crate::services::turn_orchestrator::QueuePersistenceContext::new(
        &crate::services::provider::ProviderKind::Claude,
        "s3b",
        None,
    );
    let adopted = super::super::projection::readopt(&registry, &persistence, "tickets")
        .await
        .unwrap();
    assert!(adopted.contains(&Ok(channel)), "{adopted:?}");
    let restarted = registry.handle(channel);
    assert_eq!(
        restarted.v2_admission_status(ticket()).await,
        Ok(TicketStatus::Committed)
    );
    assert_eq!(
        restarted.v2_admit_ticket(input, ticket()).await,
        Ok(Admitted::Duplicate)
    );
    let (texts, receipts) = held(&restarted).await;
    assert_eq!(texts, ["routine prompt"]);
    assert_eq!(receipts.len(), 1);
}

#[tokio::test]
async fn admission_status_reads_without_admitting() {
    let _root = TestRuntimeRootGuard::new();
    let path = fixture_path(727_383);
    let (_registry, handle) = ticket_owner(727_383, "tickets").await;
    let watch = Watch::new(
        &path,
        Plan {
            fail: vec![Step::ParentSync],
            ..Default::default()
        },
    );
    let input = original(8_000_000_000_000_001_830, "routine prompt");
    let ticket = || "run-7272-c".to_string();
    assert_eq!(
        handle.v2_admit_ticket(input, ticket()).await,
        Err(AdmitError::Pending)
    );
    let state = || async {
        handle
            .v2_request(|slot| {
                let owner = slot.as_ref().ok_or("no owner")?;
                let snapshot = &owner.snapshot;
                let pending = owner.pending.is_some();
                Ok((
                    snapshot.admission_sequence,
                    snapshot.admission_receipts.clone(),
                    pending,
                ))
            })
            .await
            .unwrap()
    };
    // With the fault gone a recovery would now publish; a lookup still must not run one.
    drop(watch);
    let before = state().await;
    for _ in 0..2 {
        assert_eq!(
            handle.v2_admission_status(ticket()).await,
            Ok(TicketStatus::Pending)
        );
        assert_eq!(
            handle.v2_admission_status("run-7272-unknown".into()).await,
            Ok(TicketStatus::Absent)
        );
    }
    assert_eq!(
        state().await,
        before,
        "a lookup neither recovers nor commits"
    );
    assert!(before.2);
}

#[test]
fn legacy_seq_uuid_receipts_keep_their_verdicts_beside_tickets() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner();
    let legacy = original(100, "legacy body");
    assert_eq!(
        Custody::new(&legacy).advance(&mut owner),
        Ok(Admitted::Queued)
    );
    // The owner is read back from what a pre-ticket writer left on disk.
    let mut owner = Owner::adopt(fixture_path(727370), "s3b".into()).unwrap();
    let old = owner.snapshot.admission_receipts.clone();
    assert!(old.len() == 1 && !old[0].starts_with("tk:"), "{old:?}");
    let ticket = Ticket::new("run-b").unwrap();
    let ticketed = original(200, "ticketed body");
    assert_eq!(
        Custody::with_ticket(&ticketed, ticket.clone()).advance(&mut owner),
        Ok(Admitted::Queued)
    );
    assert_eq!(owner.admission_status(&ticket), TicketStatus::Committed);
    let late = Ticket::new("run-c").unwrap();
    assert_eq!(
        Custody::with_ticket(&legacy, late.clone()).advance(&mut owner),
        Err(AdmitError::SourceAlreadyKnown),
        "an input admitted before tickets is neither copied under the ticket nor a conflict"
    );
    assert_eq!(owner.admission_status(&late), TicketStatus::Absent);
    assert_eq!(
        Custody::new(&legacy).advance(&mut owner),
        Ok(Admitted::Duplicate)
    );
    let texts: Vec<_> = owner
        .snapshot
        .items
        .iter()
        .map(|i| i.text.as_str())
        .collect();
    assert_eq!(texts, ["legacy body", "ticketed body"]);
    assert_eq!(owner.snapshot.admission_receipts[0], old[0]);
}

/// The owner's snapshot, unconfirmed candidate, sequence and receipts, compared whole.
async fn whole(handle: &Handle) -> (serde_json::Value, Option<serde_json::Value>) {
    handle
        .v2_request(|slot| {
            let owner = slot.as_ref().ok_or("no owner")?;
            let candidate =
                (owner.pending.as_ref()).map(|(_, next)| serde_json::to_value(next).unwrap());
            Ok((owner.json()?, candidate))
        })
        .await
        .unwrap()
}

/// A ticket is accepted only once it names its original: a source admitted earlier without the
/// ticket is refused under it, so a later original can still take the ticket, and then only that one.
#[tokio::test]
async fn a_source_admitted_without_the_ticket_is_not_accepted_under_it() {
    let _root = TestRuntimeRootGuard::new();
    let (_registry, handle) = ticket_owner(727_384, "tickets").await;
    let ticket = || "run-7272-d".to_string();
    let legacy = original(727_384_001, "legacy original");
    let custody = Arc::new(Mutex::new(Custody::new(&legacy)));
    assert_eq!(handle.v2_admit(custody).await, Ok(Admitted::Queued));
    assert_eq!(
        handle.v2_admit_ticket(legacy, ticket()).await,
        Err(AdmitError::SourceAlreadyKnown)
    );
    assert_eq!(
        handle.v2_admission_status(ticket()).await,
        Ok(TicketStatus::Absent)
    );
    let fresh = original(727_384_002, "ticketed original");
    assert_eq!(
        handle.v2_admit_ticket(fresh, ticket()).await,
        Ok(Admitted::Queued)
    );
    assert_eq!(
        handle.v2_admission_status(ticket()).await,
        Ok(TicketStatus::Committed)
    );
    let other = original(727_384_003, "a third original");
    assert!(matches!(
        handle.v2_admit_ticket(other, ticket()).await,
        Err(AdmitError::Conflict(_))
    ));
    let (texts, receipts) = held(&handle).await;
    assert_eq!(texts, ["legacy original", "ticketed original"]);
    assert_eq!(receipts.len(), 2);
}

/// Inputs that share a synthetic identity, even with one body, are told apart by the ticket each
/// was admitted under, as it moves from the queue to the head.
#[tokio::test]
async fn an_exact_turn_follows_its_ticket_not_a_shared_identity() {
    let _root = TestRuntimeRootGuard::new();
    let (_registry, handle) = ticket_owner(727_385, "tickets").await;
    let id = 8_000_000_000_000_001_850;
    let (a, b) = (original(id, "A"), original(id, "B"));
    for (input, ticket) in [(&a, "run-a"), (&b, "run-b"), (&a, "run-a-again")] {
        assert_eq!(
            handle.v2_admit_ticket(input.clone(), ticket.into()).await,
            Ok(Admitted::Queued)
        );
    }
    handle.v2_arm_head_for_tests("attempt-a").await.unwrap();
    let turn =
        |input: &PendingQueueItem, ticket: &str| handle.v2_exact_turn(input.clone(), ticket.into());
    assert_eq!(
        turn(&a, "run-a").await,
        Ok(TurnBind::Armed {
            attempt: "attempt-a".into(),
            source: None
        })
    );
    assert_eq!(turn(&b, "run-b").await, Ok(TurnBind::NotArmed));
    assert_eq!(turn(&a, "run-a-again").await, Ok(TurnBind::NotArmed));
    assert!(turn(&b, "run-a").await.is_err(), "the ticket names A");
}

/// A ticket that names another original is refused before any recovery, with the fault still in
/// place and after it is gone, and the refusal changes neither the snapshot nor the candidate.
#[tokio::test]
async fn a_conflicting_ticket_is_refused_before_recovery() {
    let _root = TestRuntimeRootGuard::new();
    let path = fixture_path(727_386);
    let (_registry, handle) = ticket_owner(727_386, "tickets").await;
    let ticket = || "run-7272-e".to_string();
    let watch = Watch::new(
        &path,
        Plan {
            fail: vec![Step::ParentSync],
            ..Default::default()
        },
    );
    let first = original(8_000_000_000_000_001_860, "first original");
    assert_eq!(
        handle.v2_admit_ticket(first, ticket()).await,
        Err(AdmitError::Pending)
    );
    let before = whole(&handle).await;
    assert!(before.1.is_some());
    let other = original(8_000_000_000_000_001_860, "another original");
    let refused = || async {
        assert!(matches!(
            handle.v2_admit_ticket(other.clone(), ticket()).await,
            Err(AdmitError::Conflict(_))
        ));
        assert_eq!(whole(&handle).await, before);
    };
    refused().await;
    drop(watch);
    refused().await;
}

/// The receipt names what runs: reply, attachments and a source's route or origin each make another
/// original, while a retry that differs only in recomputed times and diagnostics is the same one.
#[tokio::test]
async fn the_receipt_names_every_field_that_changes_what_runs() {
    let _root = TestRuntimeRootGuard::new();
    let (_registry, handle) = ticket_owner(727_387, "tickets").await;
    let id = 8_000_000_000_000_001_870_u64;
    let with = |source: serde_json::Value, extra: serde_json::Value| {
        let mut value = json!({"author_id":7,"message_id":id,"channel_id":7272,
            "source_message_ids":[id],"queued_generation":5,"text":"run the report",
            "source_message_queued_generations":[source]});
        let map = value.as_object_mut().unwrap();
        map.extend(extra.as_object().unwrap().clone());
        serde_json::from_value::<PendingQueueItem>(value).unwrap()
    };
    let source = json!({"message_id":id,"queued_generation":5,"origin":"routine"});
    let ticket = || "run-7272-f".to_string();
    assert_eq!(
        handle
            .v2_admit_ticket(with(source.clone(), json!({})), ticket())
            .await,
        Ok(Admitted::Queued)
    );
    let before = whole(&handle).await;
    let route = json!({"target":7272,"parent":null,"role":null});
    for (field, variant) in [
        (
            "reply",
            with(source.clone(), json!({"reply_context":"another reply"})),
        ),
        (
            "attachment",
            with(source.clone(), json!({"pending_uploads":["/tmp/a.png"]})),
        ),
        (
            "route",
            with(
                json!({"message_id":id,"queued_generation":5,"origin":"routine",
            "route":route}),
                json!({}),
            ),
        ),
        (
            "origin",
            with(
                json!({"message_id":id,"queued_generation":5,"origin":"http"}),
                json!({}),
            ),
        ),
    ] {
        let refused = handle.v2_admit_ticket(variant, ticket()).await;
        assert!(
            matches!(refused, Err(AdmitError::Conflict(_))),
            "{field}: {refused:?}"
        );
        assert_eq!(whole(&handle).await, before, "{field}");
    }
    let retried = with(
        json!({"message_id":id,"queued_generation":5,"origin":"routine",
            "enqueued_at_epoch_us":1_234}),
        json!({"created_at_wall_time_ms":99,"channel_name":"renamed"}),
    );
    assert_eq!(
        handle.v2_admit_ticket(retried, ticket()).await,
        Ok(Admitted::Duplicate)
    );
    let unsettled = with(source, json!({"queued_generation":0}));
    let refused = handle.v2_admit_ticket(unsettled, "run-7272-g".into()).await;
    assert!(
        matches!(&refused, Err(AdmitError::NotCommitted(why)) if why.contains("generation")),
        "{refused:?}"
    );
    assert_eq!(whole(&handle).await, before);
}

/// A restart right after Pending, with no retry or recovery in between, reads the ticket from the
/// published snapshot alone: committed, acknowledged as a duplicate, held once.
#[tokio::test]
async fn a_restart_right_after_pending_reads_the_published_ticket() {
    let _root = TestRuntimeRootGuard::new();
    let channel = ChannelId::new(727_388);
    let path = fixture_path(channel.get());
    let (first, handle) = ticket_owner(channel.get(), "tickets").await;
    let input = original(8_000_000_000_000_001_880, "routine prompt");
    let ticket = || "run-7272-h".to_string();
    let watch = Watch::new(
        &path,
        Plan {
            fail: vec![Step::ParentSync],
            ..Default::default()
        },
    );
    assert_eq!(
        handle.v2_admit_ticket(input.clone(), ticket()).await,
        Err(AdmitError::Pending)
    );
    let (committed, candidate) = handle
        .v2_request(|slot| {
            let owner = slot.as_ref().ok_or("no owner")?;
            let named =
                |receipts: &[String]| receipts.iter().any(|r| r.starts_with("tk:run-7272-h:"));
            let candidate = owner
                .pending
                .as_ref()
                .map(|(_, next)| named(&next.admission_receipts));
            Ok((named(&owner.snapshot.admission_receipts), candidate))
        })
        .await
        .unwrap();
    assert_eq!((committed, candidate), (false, Some(true)));
    drop((handle, first, watch));

    let registry = ChannelMailboxRegistry::default();
    let persistence = crate::services::turn_orchestrator::QueuePersistenceContext::new(
        &crate::services::provider::ProviderKind::Claude,
        "s3b",
        None,
    );
    let adopted = super::super::projection::readopt(&registry, &persistence, "tickets")
        .await
        .unwrap();
    assert!(adopted.contains(&Ok(channel)), "{adopted:?}");
    let restarted = registry.handle(channel);
    assert_eq!(
        restarted.v2_admission_status(ticket()).await,
        Ok(TicketStatus::Committed)
    );
    assert_eq!(
        restarted.v2_admit_ticket(input, ticket()).await,
        Ok(Admitted::Duplicate)
    );
    let (texts, receipts) = held(&restarted).await;
    assert_eq!(texts, ["routine prompt"]);
    assert_eq!(receipts.len(), 1);
}
