//! A live arrival the input owner left Pending is recovered by the real catch-up sweep under the
//! same ticket and the same stored original, though catch-up renders the message differently.
use super::*;
use crate::services::discord::queue_io::input_ingress::{self, Intake};
use crate::services::discord::runtime_store::durable_probe::{Plan, Step, Watch};
use crate::services::discord::{IntakeRequest, TurnKind};

fn live_request(
    channel_id: ChannelId,
    message_id: MessageId,
    reply: Option<&str>,
) -> IntakeRequest {
    IntakeRequest {
        intake_outbox_id: None,
        channel_id,
        user_msg_id: message_id,
        source_message_ids: vec![],
        busy_followup_retry_user_msg_id: message_id,
        request_owner: serenity::UserId::new(HUMAN_ID),
        request_owner_name: "human".into(),
        user_text: format!("request {}", message_id.get()),
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        merge_consecutive: true,
        reply_context: reply.map(str::to_string),
        has_reply_boundary: reply.is_some(),
        dm_hint: None,
        turn_kind: TurnKind::Foreground,
        preserve_on_cancel: true,
    }
}

async fn snapshot(fx: &Fixture, channel_id: ChannelId) -> serde_json::Value {
    let handle = fx.shared.mailbox(channel_id);
    let read = handle.v2_request(|slot| slot.as_ref().ok_or("no owner")?.json());
    read.await.unwrap()
}

async fn recover(channel_seed: u64, reply: Option<&str>, uploads: Vec<&str>) {
    let fx = Fixture::new().await;
    let generation = crate::services::discord::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
    std::fs::write(generation, "7").unwrap();
    let channel_id = ChannelId::new(channel_seed);
    let (checkpoint, q) = (id(1, 600), id(2, 150));
    fx.seed_checkpoint(channel_id, checkpoint);
    let path = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join(&fx.shared.token_hash)
        .join(format!("{channel_id}.json"));
    let handle = fx.shared.mailbox(channel_id);
    handle
        .v2_adopt(path.clone(), fx.shared.token_hash.clone())
        .await
        .unwrap();
    let request = live_request(channel_id, q, reply);
    let expected_uploads = uploads.len();
    let uploads = uploads.into_iter().map(Into::into).collect();
    let watch = Watch::new(
        &path,
        Plan {
            fail: vec![Step::ParentSync],
            ..Default::default()
        },
    );
    let intake = input_ingress::intake(
        &fx.shared,
        &fx.provider,
        &request,
        uploads,
        None,
        false,
        false,
    );
    assert!(
        matches!(intake.await, Some(Intake::Pending(_))),
        "the live arrival is Pending"
    );
    drop(watch);
    let stored = snapshot(&fx, channel_id).await;
    assert!(
        stored["items"].as_array().unwrap().is_empty(),
        "nothing is committed yet"
    );

    let history = vec![own_reply(channel_id, checkpoint), human(channel_id, q)];
    let api = StrictApi::new(&fx.shared).with_history(channel_id, history);
    fx.sweep(&api).await;
    assert_eq!(
        api.enqueue_log(),
        vec![(q.get(), true, None)],
        "catch-up recovers it once"
    );
    let state = snapshot(&fx, channel_id).await;
    let items = state["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(state["admission_sequence"], 1);
    assert_eq!(state["admission_receipts"].as_array().unwrap().len(), 1);
    let envelope = items[0]["reply_context"].as_str().unwrap_or_default();
    assert!(
        envelope.starts_with('\u{1e}'),
        "the live original, not catch-up's copy: {envelope:?}"
    );
    if let Some(reply) = reply {
        assert!(envelope.contains(reply), "its reply context survives");
    }
    assert_eq!(
        items[0]["pending_uploads"].as_array().map_or(0, Vec::len),
        expected_uploads
    );
    fx.sweep(&api).await;
    assert_eq!(
        snapshot(&fx, channel_id).await["items"],
        state["items"],
        "a later sweep adds nothing"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_plain_live_pending_arrival_is_recovered_by_catch_up_as_its_stored_original() {
    recover(7_272_501, None, vec![]).await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_live_pending_reply_with_an_upload_keeps_both_through_catch_up() {
    recover(7_272_502, Some("quoted line"), vec!["local asset ref"]).await;
}

/// A V2 queue file as a forward binary leaves it, holding `held` queued for a V2-capable binary.
fn write_v2_queue(fx: &Fixture, channel_id: ChannelId, held: MessageId) -> std::path::PathBuf {
    let path = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join(&fx.shared.token_hash)
        .join(format!("{channel_id}.json"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let item = serde_json::json!({"author_id":HUMAN_ID,"message_id":held.get(),"channel_id":channel_id.get(),
        "source_message_ids":[held.get()],"queued_generation":7,"text":format!("request {}", held.get())});
    let snapshot = serde_json::json!({"version":2,"items":[item],"head_injection":null,
        "retained_inputs":[],"terminal_identities":[],"namespace":fx.shared.token_hash});
    std::fs::write(&path, snapshot.to_string()).unwrap();
    path
}

/// On a binary that cannot write V2, the production claim never starts a legacy turn on a V2
/// channel, and the file stays byte for byte.
#[tokio::test(flavor = "current_thread")]
async fn a_v2_queue_channel_refuses_every_legacy_claim_and_keeps_its_file() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(7_272_503);
    let path = write_v2_queue(&fx, channel_id, id(2, 150));
    let before = std::fs::read(&path).unwrap();
    let token = Arc::new(crate::services::provider::CancelToken::new());
    let owner = serenity::UserId::new(HUMAN_ID);
    let start = crate::services::discord::queue_io::mailbox_try_start_turn_behind_queue;
    assert!(!start(&fx.shared, channel_id, token, owner, id(3, 100), None).await);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

/// A message a V2 queue holds is known to the real catch-up sweep: it is not taken again, and the
/// frontier stays before it, as for any queued input this binary did not run.
#[tokio::test(flavor = "current_thread")]
async fn catch_up_never_retakes_a_message_a_v2_queue_holds() {
    let fx = Fixture::new().await;
    let channel_id = ChannelId::new(7_272_504);
    let (checkpoint, q) = (id(1, 600), id(2, 150));
    fx.seed_checkpoint(channel_id, checkpoint);
    let path = write_v2_queue(&fx, channel_id, q);
    let before = std::fs::read(&path).unwrap();
    let history = vec![own_reply(channel_id, checkpoint), human(channel_id, q)];
    let api = StrictApi::new(&fx.shared).with_history(channel_id, history);
    fx.sweep(&api).await;
    assert_eq!(
        api.enqueue_log(),
        Vec::<EnqueueRecord>::new(),
        "the V2 file keeps it"
    );
    assert_eq!(
        fx.surfaces(channel_id),
        (Some(checkpoint.get()), Some(checkpoint.get()))
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
