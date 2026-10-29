use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::discord::runtime_store::durable_probe::{Plan, Step, Watch};
use crate::services::discord::{IntakeRequest, TurnKind, make_shared_data_for_tests};
use poise::serenity_prelude::{MessageId, UserId};


/// A booted process has a settled queue generation; a fresh test root has none.
fn seed_generation() {
    let path = crate::services::discord::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "7").unwrap();
}

fn request(channel: u64, message: u64) -> IntakeRequest {
    IntakeRequest {
        intake_outbox_id: None, channel_id: ChannelId::new(channel), user_msg_id: MessageId::new(message),
        source_message_ids: vec![], busy_followup_retry_user_msg_id: MessageId::new(message),
        request_owner: UserId::new(7), request_owner_name: "author".into(), user_text: "raw\r\n_ WIP literal".into(),
        reply_to_user_message: false, defer_watcher_resume: false, wait_for_completion: false,
        merge_consecutive: false, reply_context: Some("raw quote".into()), has_reply_boundary: true,
        dm_hint: None, turn_kind: TurnKind::Foreground, preserve_on_cancel: true,
    }
}

async fn adopt(shared: &SharedData, channel: u64) -> (ChannelMailboxHandle, std::path::PathBuf) {
    let path = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap()
        .join("claude").join(&shared.token_hash).join(format!("{channel}.json"));
    let handle = shared.mailbox(ChannelId::new(channel));
    handle.v2_adopt(path.clone(), shared.token_hash.clone()).await.unwrap();
    (handle, path)
}

async fn snapshot(handle: &ChannelMailboxHandle) -> serde_json::Value {
    handle.v2_request(|slot| slot.as_ref().ok_or_else(|| "no owner".to_string())?.json()).await.unwrap()
}

#[tokio::test]
async fn real_enqueue_keeps_pending_raw_and_reuses_the_source_after_restart() {
    let _root = TestRuntimeRootGuard::new();
    seed_generation();
    let shared = make_shared_data_for_tests();
    let channel = 727_451;
    let (handle, path) = adopt(&shared, channel).await;
    let req = request(channel, 727_451_001);
    let raw = crate::services::discord::router::input_payload::owned_original(
        &req, false, vec!["local asset ref".into()], None, false,
    );
    let watch = Watch::new(&path, Plan { fail: vec![Step::ParentSync], ..Default::default() });
    let enter = || crate::services::discord::queue_io::mailbox_enqueue_observed_intervention(
        &shared, &ProviderKind::Claude, req.channel_id, raw.clone(), None,
    );
    let pending = enter().await;
    assert!(!pending.enqueued && pending.persistence_error.is_some(), "durable before ACK");
    drop(watch);
    handle.v2_adopt(path.clone(), shared.token_hash.clone()).await.unwrap();
    let mut replay = raw.clone();
    replay.queued_generation += 1;
    for source in &mut replay.source_message_queued_generations { source.queued_generation += 1; }
    let recovered = crate::services::discord::queue_io::mailbox_enqueue_observed_intervention(
        &shared, &ProviderKind::Claude, req.channel_id, replay, None,
    ).await;
    assert!(recovered.enqueued && recovered.persistence_error.is_none());
    let state = snapshot(&handle).await;
    assert_eq!(state["items"].as_array().unwrap().len(), 1);
    assert_eq!(state["items"][0]["text"], req.user_text);
    assert_eq!(state["items"][0]["pending_uploads"], json!(["local asset ref"]));
    assert_eq!(state["items"][0]["source_message_queued_generations"][0]["preserve_on_cancel"], true);
    assert_eq!(state["admission_sequence"], 1);
    assert_eq!(state["admission_receipts"].as_array().unwrap().len(), 1);
    assert!(enter().await.enqueued);
    assert_eq!(snapshot(&handle).await["items"], state["items"]);
}

#[tokio::test]
async fn two_identical_synthetic_originals_keep_their_own_tickets_through_pending() {
    let _root = TestRuntimeRootGuard::new();
    seed_generation();
    let shared = make_shared_data_for_tests();
    let (handle, path) = adopt(&shared, 727_452).await;
    let item: PendingQueueItem = serde_json::from_value(json!({
        "author_id":1,"message_id":8_000_000_000_000_001_452u64,"channel_id":727452,
        "queued_generation":5,"text":"identical synthetic raw"
    })).unwrap();
    let first = Original::with_ticket(item.clone(), AdmissionTicket::new());
    let second = Original::with_ticket(item, AdmissionTicket::new());
    let watch = Watch::new(&path, Plan { fail: vec![Step::ParentSync], ..Default::default() });
    assert!(matches!(first.admit(&handle).await, Admission::Pending(_)));
    drop(watch);
    handle.v2_adopt(path, shared.token_hash.clone()).await.unwrap();
    assert!(matches!(second.admit(&handle).await, Admission::Accepted(_)));
    assert!(matches!(first.admit(&handle).await, Admission::Accepted(_)));
    let state = snapshot(&handle).await;
    assert_eq!(state["items"].as_array().unwrap().len(), 2);
    assert_eq!(state["admission_sequence"], 2);
    assert_ne!(first.ticket, second.ticket);
    assert_eq!(state["items"][0]["text"], state["items"][1]["text"]);
}

#[tokio::test]
async fn cancelled_source_is_not_reenqueued_by_its_committed_ticket() {
    let _root = TestRuntimeRootGuard::new();
    seed_generation();
    let shared = make_shared_data_for_tests();
    let (handle, path) = adopt(&shared, 727_453).await;
    let req = request(727_453, 727_453_001);
    let raw = crate::services::discord::router::input_payload::owned_original(&req, false, vec![], None, false);
    let admitted = original(&ProviderKind::Claude, req.channel_id, None, &raw).unwrap();
    assert!(matches!(admitted.admit(&handle).await, Admission::Accepted(_)));
    let removed = crate::services::discord::queue_io::mailbox_cancel_queued_primary_message(
        &shared, &ProviderKind::Claude, req.channel_id, req.user_msg_id,
    ).await;
    assert!(removed.is_some());
    handle.v2_adopt(path, shared.token_hash.clone()).await.unwrap();
    assert!(matches!(admitted.admit(&handle).await, Admission::Accepted(_)));
    assert!(snapshot(&handle).await["items"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn original_bot_and_human_rights_survive_the_real_queue_entry() {
    let _root = TestRuntimeRootGuard::new();
    seed_generation();
    let shared = make_shared_data_for_tests();
    let (handle, _) = adopt(&shared, 727_454).await;
    for (offset, bot) in [(0, false), (1, true)] {
        let mut req = request(727_454, 727_454_001 + offset);
        req.preserve_on_cancel = !bot;
        let raw = crate::services::discord::router::input_payload::owned_original(&req, bot, vec![], None, false);
        assert!(crate::services::discord::queue_io::mailbox_enqueue_observed_intervention(
            &shared, &ProviderKind::Claude, req.channel_id, raw, None,
        ).await.enqueued);
    }
    let state = snapshot(&handle).await;
    for (at, bot) in [(0, false), (1, true)] {
        assert_eq!(state["items"][at]["author_is_bot"], bot);
        assert_eq!(state["items"][at]["source_message_queued_generations"][0]["preserve_on_cancel"].as_bool().unwrap_or(false), !bot);
    }
}
