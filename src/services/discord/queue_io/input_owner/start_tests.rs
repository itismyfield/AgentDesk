//! A synthetic start from its entry: a start the owner does not take is left untouched, a refused
//! preflight admits nothing, and an accepted original keeps its provenance and wakes the drive.
use super::super::routine::synthetic_original;
use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::turn_orchestrator::ChannelMailboxRegistry;
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn original(channel: u64) -> PendingQueueItem {
    let id = 8_000_000_000_000_008_000 + channel % 1000;
    serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":channel,
        "source_message_ids":[id],"queued_generation":1,"text":"synthetic start"}))
    .unwrap()
}

fn drive(mailbox: &ChannelMailboxHandle, channel: u64, woken: Arc<AtomicUsize>) -> Arc<Drive> {
    let handoff: super::super::drive::Handoff = Arc::new(move |_| {
        woken.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Err("left queued".into()) })
    });
    Arc::new(Drive {
        mailbox: mailbox.clone(),
        channel,
        handoff,
        recheck: Duration::from_secs(60),
    })
}

async fn items(mailbox: &ChannelMailboxHandle) -> usize {
    let snapshot = mailbox.v2_request(|s| s.as_ref().map_or(Ok(json!({})), |o| o.json()));
    snapshot.await.unwrap()["items"]
        .as_array()
        .map_or(0, Vec::len)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_synthetic_start_admits_after_its_preflight_and_wakes_the_drive() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let channel = 7_272_711;
    let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(channel));
    let woken = Arc::new(AtomicUsize::new(0));
    let start = |execution, ticket: &str, preflight: Result<(), String>| {
        let (drive, original) = (drive(&mailbox, channel, woken.clone()), original(channel));
        let ticket = AdmissionTicket::stored(ticket);
        let provenance = json!({"request_owner": 7, "synthetic_label": "http"});
        async move {
            let origin = (execution, InputOrigin::HttpStart);
            let preflight = async move { preflight };
            admit_synthetic(drive, origin, (&original, ticket), provenance, preflight).await
        }
    };
    assert_eq!(
        start(Execution::LocalTui, "start-1", Ok(())).await,
        None,
        "no owner yet"
    );
    assert!(
        !owner_will_take(&mailbox, Execution::LocalTui, InputOrigin::HttpStart)
            .await
            .unwrap()
    );
    let dir = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude/input-owner-start");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{channel}.json"));
    mailbox.v2_adopt(path, "input-owner".into()).await.unwrap();
    assert_eq!(
        start(Execution::Process, "start-1", Ok(())).await,
        None,
        "a process keeps its path"
    );
    let refused = start(Execution::LocalTui, "start-1", Err("replay refused".into())).await;
    assert_eq!(refused, Some(Intake::Rejected("replay refused".into())));
    assert_eq!(
        items(&mailbox).await,
        0,
        "a refused preflight admits nothing"
    );
    assert_eq!(woken.load(Ordering::SeqCst), 0);
    assert!(
        owner_will_take(&mailbox, Execution::LocalTui, InputOrigin::HttpStart)
            .await
            .unwrap()
    );
    let accepted = start(Execution::LocalTui, "start-1", Ok(())).await;
    assert_eq!(accepted, Some(Intake::Accepted));
    assert_eq!(items(&mailbox).await, 1);
    let context = mailbox.v2_admission_context(original(channel), "start-1".into());
    let expected = json!({"request_owner": 7, "synthetic_label": "http"});
    assert_eq!(
        context.await.unwrap(),
        Some(expected),
        "the provenance is kept"
    );
    for _ in 0..100 {
        if woken.load(Ordering::SeqCst) > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        woken.load(Ordering::SeqCst),
        1,
        "the accepted original woke the drive once"
    );
}

/// A headless start asked again for the same reservation names the same ticket and original, so
/// the owner holds it once with the provenance its caller resolved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repeated_headless_start_is_admitted_once_under_its_reservation() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let channel = 7_272_712;
    let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(channel));
    let dir = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude/input-owner-start");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{channel}.json"));
    mailbox.v2_adopt(path, "input-owner".into()).await.unwrap();
    let generation = crate::services::discord::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
    std::fs::write(generation, "5").unwrap();
    let turn_id = format!("discord:{channel}:9100000000000000042");
    let woken = Arc::new(AtomicUsize::new(0));
    let mut tickets = Vec::new();
    for _ in 0..2 {
        let (original, ticket) = synthetic_original(&turn_id, channel, "headless", 1).unwrap();
        tickets.push(ticket.clone());
        let provenance = HeadlessProvenance {
            owner: (1, "api"),
            source: Some("http"),
            metadata: Some(json!({"k":"v"})),
            channel_hint: Some("adk-cc"),
            label: None,
            dm: Some(false),
        }
        .json(&turn_id);
        let origin = (Execution::LocalTui, InputOrigin::HttpStart);
        let started = admit_synthetic(
            drive(&mailbox, channel, woken.clone()),
            origin,
            (&original, ticket),
            provenance,
            async { Ok(()) },
        );
        assert_eq!(started.await, Some(Intake::Accepted));
    }
    assert_eq!(tickets[0], tickets[1]);
    assert_eq!(
        items(&mailbox).await,
        1,
        "one admission for one reservation"
    );
    let (original, ticket) = synthetic_original(&turn_id, channel, "headless", 1).unwrap();
    assert_eq!(original.message_id, 9_100_000_000_000_000_042);
    let context = mailbox.v2_admission_context(original, ticket.as_str().into());
    let context = context.await.unwrap().unwrap();
    assert_eq!(context["request_owner_name"], "api");
    assert_eq!(context["channel_name_hint"], "adk-cc");
    assert_eq!(context["metadata"]["k"], "v");
    assert!(synthetic_original("discord:1:x", channel, "headless", 1).is_err());
}
