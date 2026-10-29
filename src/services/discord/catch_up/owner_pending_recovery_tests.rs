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
