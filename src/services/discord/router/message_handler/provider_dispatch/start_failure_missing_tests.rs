//! A normalized start failure settles its exact members once, never retaining them for retry.
use super::*;
use crate::services::discord::queue_io::input_owner::drive::claim_turn;
use poise::serenity_prelude::ChannelId;
use serde_json::json;

#[tokio::test]
async fn a_repaired_start_failure_discards_only_its_claim_and_notices_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = ChannelId::new(8212001);
    let mailbox = shared.mailbox(channel);
    let queue = crate::config::runtime_root()
        .unwrap()
        .join("runtime/missing-failure.json");
    mailbox
        .v2_adopt(queue, "missing-failure".into())
        .await
        .unwrap();
    for id in [1, 2] {
        let item: PendingQueueItem = serde_json::from_value(json!({
            "author_id":7, "message_id":id, "channel_id":channel.get(),
            "queued_generation":1, "merge_consecutive":true, "text":format!("original {id}")
        }))
        .unwrap();
        mailbox
            .v2_admit_ticket(item, format!("missing-{id}"))
            .await
            .unwrap();
    }
    let front = mailbox
        .v2_request(|slot| {
            let owner = slot.as_mut().unwrap();
            let front = owner.front().unwrap();
            owner.grant_front(&front)?;
            Ok(front)
        })
        .await
        .unwrap();
    let claim = claim_turn(&mailbox, front.1.message_id, &front)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        claim.followers().len(),
        1,
        "the failure owns the merged originals"
    );
    let recovery = MissingRecovery::default();
    recovery.mark();
    let failure = Failure::claimed(&mailbox, &claim).with_recovery(recovery);
    let again = failure.clone();
    let api = crate::services::discord::admin_host_guard::tests::Recorder::start().await;
    let _http = crate::services::discord::shared_state::test_rest::install(api.http.clone());
    assert_eq!(
        failure
            .commit(
                Kind::Opaque,
                "new execution unavailable".into(),
                Ok(None),
                Some(shared.clone()),
                channel,
                None
            )
            .await
            .unwrap(),
        Some(true)
    );
    let later: PendingQueueItem = serde_json::from_value(json!({"author_id":7,
        "message_id":3,"channel_id":channel.get(),"queued_generation":1,"text":"later"}))
    .unwrap();
    mailbox
        .v2_admit_ticket(later.clone(), "later".into())
        .await
        .unwrap();
    assert_eq!(
        again
            .commit(
                Kind::Opaque,
                "same failure".into(),
                Ok(None),
                Some(shared),
                channel,
                None
            )
            .await
            .unwrap(),
        None
    );
    let state = mailbox
        .v2_request(|slot| slot.as_ref().unwrap().json())
        .await
        .unwrap();
    assert_eq!(state["items"], json!([later]));
    assert_eq!(state["retained_inputs"], json!([]));
    assert_eq!(
        api.take()
            .iter()
            .filter(|call| call.contains("이 입력을 폐기했습니다"))
            .count(),
        1
    );
}
