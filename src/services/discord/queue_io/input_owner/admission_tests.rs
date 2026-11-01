use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::discord::runtime_store::{discord_pending_queue_root, durable_probe};
use crate::services::turn_orchestrator::ChannelMailboxRegistry;
use poise::serenity_prelude::ChannelId;
use std::path::PathBuf;

fn input(id: u64, text: &str) -> PendingQueueItem {
    serde_json::from_value(
        serde_json::json!({"author_id":7,"message_id":id,"channel_id":7272,
        "source_message_ids":[id],"queued_generation":5,"text":text}),
    )
    .unwrap()
}

/// An actor whose V2 owner adopted the channel's queue file.
async fn owner(registry: &ChannelMailboxRegistry, channel: u64) -> (ChannelMailboxHandle, PathBuf) {
    let dir = discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join("input-owner");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{channel}.json"));
    let handle = registry.handle(ChannelId::new(channel));
    handle
        .v2_adopt(path.clone(), "input-owner".into())
        .await
        .unwrap();
    (handle, path)
}

#[tokio::test]
async fn pending_is_not_accepted_or_consumed() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let (handle, path) = owner(&registry, 727_391).await;
    let original = input(8_000_000_000_000_001_910, "routine prompt");
    let ticket = AdmissionTicket::new();
    let watch = durable_probe::Watch::new(
        &path,
        durable_probe::Plan {
            fail: vec![durable_probe::Step::ParentSync],
            ..Default::default()
        },
    );
    assert_eq!(
        admit(&handle, &original, ticket.clone()).await,
        Admission::Pending(ticket.clone()),
        "an unconfirmed publish is neither accepted nor refused"
    );
    assert_eq!(status(&handle, &ticket).await, Ok(AdmissionState::Pending));
    drop(watch);
    let stored = AdmissionTicket::stored(ticket.as_str());
    assert_eq!(
        admit(&handle, &original, stored).await,
        Admission::Accepted(ticket.clone())
    );
    assert_eq!(
        status(&handle, &ticket).await,
        Ok(AdmissionState::Committed)
    );
}

#[tokio::test]
async fn a_ticket_never_names_a_second_original() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let (handle, _) = owner(&registry, 727_392).await;
    let ticket = AdmissionTicket::new();
    let first = input(8_000_000_000_000_001_920, "first body");
    assert_eq!(
        admit(&handle, &first, ticket.clone()).await,
        Admission::Accepted(ticket.clone())
    );
    // The same ticket with another body, and with another source, is refused without a write.
    for other in [
        input(8_000_000_000_000_001_920, "edited body"),
        input(8_000_000_000_000_001_921, "another input"),
    ] {
        let Admission::Rejected(reason) = admit(&handle, &other, ticket.clone()).await else {
            panic!("a reused ticket is refused");
        };
        assert!(reason.contains("another original"), "{reason}");
    }
    let queued = handle
        .v2_request(|slot| {
            let owner = slot.as_ref().ok_or("no owner")?;
            Ok((
                owner.snapshot.items.len(),
                owner.snapshot.admission_receipts.len(),
            ))
        })
        .await
        .unwrap();
    assert_eq!(queued, (1, 1));
    // Another channel's owner keeps its own receipts: the same ticket there is a separate admission.
    let (elsewhere, _) = owner(&registry, 727_393).await;
    assert_eq!(
        admit(&elsewhere, &first, ticket.clone()).await,
        Admission::Accepted(ticket)
    );
}

/// Only an armed attempt is the ticket's turn: before admission it is absent, and queued or
/// reserved it is held but not armed.
#[tokio::test]
async fn the_exact_turn_is_the_attempt_armed_for_the_ticket_original() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let (handle, _) = owner(&registry, 727_394).await;
    let ticket = AdmissionTicket::new();
    let original = input(8_000_000_000_000_001_940, "routine prompt");
    let state = || exact_turn(&handle, &ticket, &original);
    assert_eq!(state().await, Ok(TurnBind::Absent));
    assert_eq!(
        admit(&handle, &original, ticket.clone()).await,
        Admission::Accepted(ticket.clone())
    );
    assert_eq!(state().await, Ok(TurnBind::NotArmed));
    handle
        .v2_reserve_head_for_tests("attempt-1", false)
        .await
        .unwrap();
    assert_eq!(
        state().await,
        Ok(TurnBind::NotArmed),
        "reserved is not armed"
    );
    handle.v2_arm_head_for_tests("attempt-1").await.unwrap();
    let Ok(TurnBind::Armed(turn)) = state().await else {
        panic!("the armed attempt is the ticket's turn");
    };
    assert!(turn.is("attempt-1", None) && !turn.is("attempt-2", None));
    let other = AdmissionTicket::new();
    assert_eq!(
        exact_turn(&handle, &other, &original).await,
        Ok(TurnBind::Absent)
    );
    let edited = input(8_000_000_000_000_001_940, "edited prompt");
    assert!(exact_turn(&handle, &ticket, &edited).await.is_err());
}

/// A delivery keeps the ticket's exact turn for the terminal window; after it the owner keeps no
/// record and the committed ticket reads as Unknown, never as re-issuable.
#[tokio::test]
async fn a_settled_turn_keeps_its_bind_for_the_terminal_window() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let (handle, _) = owner(&registry, 727_395).await;
    let ticket = AdmissionTicket::new();
    let original = input(8_000_000_000_000_001_950, "routine prompt");
    let state = || exact_turn(&handle, &ticket, &original);
    assert_eq!(
        admit(&handle, &original, ticket.clone()).await,
        Admission::Accepted(ticket.clone())
    );
    handle.v2_arm_head_for_tests("attempt-9").await.unwrap();
    let advance = |now: u64, settle: bool| {
        handle.v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            if settle {
                let key = owner.active_attempt()?;
                owner.settle(&key, true, now)?;
            }
            owner.tick(now)
        })
    };
    advance(1_000, true).await.unwrap();
    let Ok(TurnBind::Settled(turn)) = state().await else {
        panic!("a delivered turn keeps its bind");
    };
    assert!(turn.is("attempt-9", None));
    advance(1_000 + 86_400_000 - 1, false).await.unwrap();
    assert!(matches!(state().await, Ok(TurnBind::Settled(_))));
    advance(1_000 + 86_400_000, false).await.unwrap();
    assert_eq!(state().await, Ok(TurnBind::Unknown));
    assert_eq!(
        status(&handle, &ticket).await,
        Ok(AdmissionState::Committed)
    );
}
