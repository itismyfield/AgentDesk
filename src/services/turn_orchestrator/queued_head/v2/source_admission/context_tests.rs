use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::discord::runtime_store::{discord_pending_queue_root, durable_probe};
use crate::services::turn_orchestrator::ChannelMailboxRegistry;
use poise::serenity_prelude::ChannelId;
use serde_json::json;

fn original() -> PendingQueueItem {
    serde_json::from_value(json!({"author_id":1,"message_id":8000000000000001772u64,
        "channel_id":7272901,"text":"unchanged routine prompt"}))
    .unwrap()
}

async fn owner(
    registry: &ChannelMailboxRegistry,
    channel: u64,
) -> (super::super::super::super::ChannelMailboxHandle, PathBuf) {
    let dir = discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join("context");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{channel}.json"));
    let handle = registry.handle(ChannelId::new(channel));
    handle
        .v2_adopt(path.clone(), "context".into())
        .await
        .unwrap();
    (handle, path)
}

#[tokio::test]
async fn context_and_original_share_pending_commit_and_survive_restart() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let (handle, path) = owner(&registry, 7272901).await;
    let context = json!({"run_id":"run","attempt_kind":"retry","checkpoint":{"n":7}});
    let watch = durable_probe::Watch::new(
        &path,
        durable_probe::Plan {
            fail: vec![durable_probe::Step::ParentSync],
            ..Default::default()
        },
    );
    let result = handle
        .v2_admit_ticket_with_context(original(), "ticket-run".into(), context.clone())
        .await;
    assert!(matches!(result, Err(AdmitError::Pending)));
    drop(watch);
    handle
        .v2_admit_ticket_with_context(original(), "ticket-run".into(), context.clone())
        .await
        .unwrap();
    let restarted = ChannelMailboxRegistry::default();
    let (restored, _) = owner(&restarted, 7272901).await;
    assert_eq!(
        restored
            .v2_admission_context(original(), "ticket-run".into())
            .await
            .unwrap(),
        Some(context.clone())
    );
    let before = std::fs::read(&path).unwrap();
    restored
        .v2_admit_ticket_with_context(original(), "ticket-run".into(), context)
        .await
        .unwrap();
    assert_eq!(
        before,
        std::fs::read(&path).unwrap(),
        "same ticket does not republish"
    );
    let snapshot: serde_json::Value = serde_json::from_slice(&before).unwrap();
    assert_eq!(snapshot["items"].as_array().unwrap().len(), 1);
    assert_eq!(snapshot["items"][0]["text"], "unchanged routine prompt");
}

#[tokio::test]
async fn context_mismatch_and_legacy_admission_cannot_acquire_a_new_run() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let (handle, path) = owner(&registry, 7272902).await;
    let context = json!({"run_id":"first"});
    handle
        .v2_admit_ticket_with_context(original(), "ticket-run".into(), context.clone())
        .await
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let mut changed = original();
    changed.reply_context = Some("another prelude".into());
    for (input, value) in [(original(), json!({"run_id":"second"})), (changed, context)] {
        assert!(matches!(
            handle
                .v2_admit_ticket_with_context(input, "ticket-run".into(), value)
                .await,
            Err(AdmitError::Conflict(_))
        ));
    }
    assert_eq!(before, std::fs::read(&path).unwrap());
    let (legacy, _) = owner(&registry, 7272903).await;
    legacy
        .v2_admit_ticket(original(), "legacy".into())
        .await
        .unwrap();
    assert_eq!(
        legacy
            .v2_admission_context(original(), "legacy".into())
            .await
            .unwrap(),
        None
    );
    assert!(
        legacy
            .v2_admit_ticket_with_context(original(), "legacy".into(), json!({"run_id":"new"}))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn committed_after_local_settle_keeps_context_without_an_exact_live_turn() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let (handle, _) = owner(&registry, 7272904).await;
    let context = json!({"run_id":"settled"});
    handle
        .v2_admit_ticket_with_context(original(), "settled".into(), context.clone())
        .await
        .unwrap();
    handle
        .v2_arm_head_for_tests("settled-attempt")
        .await
        .unwrap();
    handle
        .v2_request(|slot| {
            let owner = slot.as_mut().ok_or("owner absent")?;
            let key = owner.active_attempt()?;
            owner.settle(&key, true, 1000)
        })
        .await
        .unwrap();
    assert!(
        handle
            .v2_admission_status("settled".into())
            .await
            .unwrap()
            .committed()
    );
    assert_eq!(
        handle
            .v2_exact_turn(original(), "settled".into())
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        handle
            .v2_admission_context(original(), "settled".into())
            .await
            .unwrap(),
        Some(context)
    );
}
