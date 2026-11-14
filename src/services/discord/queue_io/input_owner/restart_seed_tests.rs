//! The greeting from the restart command's side: one ticket per operation, read before it is
//! admitted, joined by a second request, and closed to every other ingress.
use super::super::drive::Drive;
use super::super::ingress::Intake;
use super::super::origin::{Execution, InputOrigin};
use super::super::start::admit_synthetic;
use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::turn_orchestrator::ChannelMailboxRegistry;
use serde_json::json;
use std::sync::Arc;

const BY: (&str, &str) = ("!restart", "restart-fixture");

async fn mailbox(registry: &ChannelMailboxRegistry, channel: u64) -> ChannelMailboxHandle {
    // A greeting is a queued original: it needs the process generation every boot allocates.
    assert_ne!(
        crate::services::discord::runtime_store::allocate_process_generation(),
        0
    );
    let dir = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude/restart-seed");
    std::fs::create_dir_all(&dir).unwrap();
    let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(channel));
    let path = dir.join(format!("{channel}.json"));
    mailbox.v2_adopt(path, "restart-seed".into()).await.unwrap();
    mailbox
}

fn operation(channel: u64, id: &str) -> Operation {
    Operation::new("claude", "token", channel, id).unwrap()
}

fn against(nonce: &'static str) -> Target<'static> {
    Target {
        nonce: Some(nonce),
        dispatch: Some("dispatch-1"),
    }
}

async fn queued(mailbox: &ChannelMailboxHandle) -> Vec<String> {
    let snapshot = mailbox.v2_request(|s| s.as_ref().ok_or("no owner".to_string())?.json());
    let items = snapshot.await.unwrap()["items"].clone();
    (items.as_array().unwrap().iter())
        .map(|item| item["text"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_seed_is_created_once_joined_by_a_second_request_and_read_by_ticket() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let channel = 8_128_711;
    let mailbox = mailbox(&registry, channel).await;
    let first = operation(channel, "message:1");
    assert_eq!(Seed::prior(&mailbox, &first).await, Ok(SeedStatus::Absent));
    let mut seed = Seed::of(&mailbox, &first).await.unwrap();
    let created = seed.admit(&mailbox, BY, against("old-nonce")).await;
    assert!(matches!(created, Admitted::Created));
    assert_eq!(queued(&mailbox).await, [PROMPT]);
    assert_eq!(Seed::prior(&mailbox, &first).await, Ok(SeedStatus::Queued));
    // The durable bind alone names the origin, the execution to end and its dispatch.
    let kept = mailbox
        .v2_admission_context(seed.original.clone(), seed.ticket.as_str().into())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        InputOrigin::from_context(Some(&kept)),
        InputOrigin::RestartSeed
    );
    assert!(!InputOrigin::from_context(Some(&kept)).is_person());
    let read = target(&kept);
    assert_eq!(
        (read.nonce, read.dispatch),
        (Some("old-nonce"), Some("dispatch-1"))
    );
    // The same operation against a row that moved on is a conflict, never a second greeting.
    let mut again = Seed::of(&mailbox, &first).await.unwrap();
    let conflict = again.admit(&mailbox, BY, against("another-nonce")).await;
    assert!(matches!(conflict, Admitted::Conflict));
    // Another request reuses the queued greeting and may not withdraw it.
    let second = operation(channel, "message:2");
    let mut other = Seed::of(&mailbox, &second).await.unwrap();
    let joined = other.admit(&mailbox, BY, against("old-nonce")).await;
    assert!(matches!(joined, Admitted::Joined));
    assert_eq!(queued(&mailbox).await, [PROMPT], "one greeting for both");
    assert_eq!(Seed::prior(&mailbox, &second).await, Ok(SeedStatus::Queued));
    seed.withdraw(&mailbox).await;
    assert!(queued(&mailbox).await.is_empty());
    for operation in [&first, &second] {
        assert_eq!(
            Seed::prior(&mailbox, operation).await,
            Ok(SeedStatus::Gone),
            "a withdrawn greeting is not absent: its request is never run again"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_seed_needs_an_adopted_owner_and_no_other_ingress_admits_its_source() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let channel = 8_128_712;
    let unowned = registry.handle(poise::serenity_prelude::ChannelId::new(channel + 1));
    let refused = Seed::of(&unowned, &operation(channel + 1, "message:1")).await;
    assert_eq!(refused.err().as_deref(), Some("V2 not adopted"));
    let mailbox = mailbox(&registry, channel).await;
    let handoff: super::super::drive::Handoff =
        Arc::new(|_| Box::pin(async { Err("left queued".into()) }));
    let drive = Arc::new(Drive {
        mailbox: mailbox.clone(),
        channel,
        handoff,
        settled: None,
    });
    let seed = Seed::of(&mailbox, &operation(channel, "message:1"))
        .await
        .unwrap();
    let forged = json!({"origin":"headless","source":RESTART_SEED_SOURCE,"request_owner":7,
        "metadata":{"restart_seed":{"operation":"forged","target_nonce":"old-nonce"}}});
    let admitted = admit_synthetic(
        drive,
        (Execution::LocalTui, InputOrigin::HttpStart),
        (&seed.original, AdmissionTicket::stored("forged-start")),
        forged,
        async { Ok(()) },
    );
    assert_eq!(
        admitted.await,
        Some(Intake::Rejected("restart_seed_source_reserved".into()))
    );
    assert!(queued(&mailbox).await.is_empty());
}
