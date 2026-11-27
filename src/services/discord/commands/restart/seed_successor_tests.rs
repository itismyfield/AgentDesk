//! The greeting's permit takes the execution its own launch recorded before creating it as its
//! own, after a reboot too, and still refuses any other live execution.
use super::*;
use crate::services::discord::queue_io::input_owner::drive::claim_turn;
use crate::services::turn_orchestrator::{
    ChannelMailboxRegistry, PendingQueueItem, RESTART_SEED_SOURCE,
};
use serde_json::json;

/// The greeting's stored context as a new process reads it, after its claimed launch recorded
/// `nonce` through the record the owner's turn installs.
async fn rebooted_context(channel: u64, nonce: &str) -> serde_json::Value {
    let path = crate::config::runtime_root().unwrap();
    let path = path.join(format!("runtime/successor-{channel}.json"));
    let (registry, rebooted) = (
        ChannelMailboxRegistry::default(),
        ChannelMailboxRegistry::default(),
    );
    let mailbox = registry.handle(ChannelId::new(channel));
    mailbox
        .v2_adopt(path.clone(), "successor".into())
        .await
        .unwrap();
    let greeting: PendingQueueItem = serde_json::from_value(json!({"author_id":1,
        "author_is_bot":true,"message_id":9,"channel_id":channel,"source_message_ids":[9],
        "queued_generation":1,"text":seed::PROMPT}))
    .unwrap();
    let context = json!({"origin":"headless","source":RESTART_SEED_SOURCE,"request_owner":1,
        "metadata":{"restart_seed":{"operation":"restart-s","target_nonce":"old","dispatch":null}}});
    let admit = mailbox.v2_admit_ticket_with_context(greeting.clone(), "restart-s".into(), context);
    admit.await.unwrap();
    let front = mailbox.v2_request(|slot| {
        let owner = slot.as_mut().unwrap();
        let front = owner.front().unwrap();
        owner.grant_front(&front)?;
        Ok(front)
    });
    let claim = claim_turn(&mailbox, 9, &front.await.unwrap()).await;
    let record = seed::successor(&mailbox, &claim.unwrap().unwrap());
    let nonce = nonce.to_owned();
    let recorded = tokio::task::spawn_blocking(move || record(&nonce));
    recorded.await.unwrap().unwrap();
    let rebooted = rebooted.handle(ChannelId::new(channel));
    rebooted.v2_adopt(path, "successor".into()).await.unwrap();
    let read = rebooted.v2_admission_context(greeting, "restart-s".into());
    read.await.unwrap().expect("the greeting keeps its context")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn seed_successor_recorded_before_install_survives_reboot_after_install() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let context = rebooted_context(8_399_201, "successor-nonce").await;
    assert!(other_execution_admits(&context, "successor-nonce", false));
    assert_eq!(
        seed::target(&context).nonce,
        Some("old"),
        "its target is unchanged"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn foreign_live_nonce_still_refused() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let context = rebooted_context(8_399_202, "successor-nonce").await;
    assert!(!other_execution_admits(&context, "foreign-nonce", false));
    assert!(
        other_execution_admits(&context, "foreign-nonce", true),
        "a retired one is vacant"
    );
}
