//! A Herdr user stop before a restart greeting's reserve cancels that greeting through its exact
//! claim; the person's input behind it keeps its original and ticket.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
use crate::services::discord::queue_io::input_owner::drive::claim_turn;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::{RESTART_SEED_SOURCE, assert_original_and_ticket_kept};
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_seed_herdr_stop_before_reserve_cancels_seed_keeps_user_input() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = poise::serenity_prelude::ChannelId::new(8_399_301);
    let mailbox = shared.mailbox(channel);
    let queue = crate::config::runtime_root()
        .unwrap()
        .join("runtime/seed-stop.json");
    mailbox.v2_adopt(queue, "seed-stop".into()).await.unwrap();
    let item = |id: u64, bot: bool| -> PendingQueueItem {
        serde_json::from_value(json!({"author_id":7,"author_is_bot":bot,"message_id":id,
            "channel_id":channel.get(),"source_message_ids":[id],"queued_generation":1,
            "text":format!("input {id}")}))
        .unwrap()
    };
    mailbox
        .v2_admit_ticket(item(1, false), "person-1".into())
        .await
        .unwrap();
    let person = mailbox.v2_request(|s| Ok(s.as_ref().unwrap().front().unwrap()));
    let person = person.await.unwrap();
    let context = json!({"origin":"headless","source":RESTART_SEED_SOURCE,"request_owner":1,
        "metadata":{"human_input":{"origin_id":"seed-stop"},
            "restart_seed":{"operation":"seed-stop","target_nonce":"old","dispatch":null}}});
    let admitted = mailbox.v2_admit_ticket_with_context(item(2, true), "seed-stop".into(), context);
    admitted.await.unwrap();
    let front = mailbox.v2_request(|slot| {
        let owner = slot.as_mut().unwrap();
        let front = owner.front().unwrap();
        owner.grant_front(&front)?;
        Ok(front)
    });
    let front = front.await.unwrap();
    let claim = claim_turn(&mailbox, 2, &front).await.unwrap().unwrap();
    let token = Arc::new(CancelToken::new());
    let owner = HostedOwner {
        provider: "claude".into(),
        discord_token_hash: "seed-stop".into(),
        channel_id: channel.get().to_string(),
        logical_key: "AgentDesk-claude-seed-stop".into(),
        owner_node: "node".into(),
        runtime_root: "root".into(),
    };
    let state = token.prepare_herdr_interrupt(ProviderKind::Claude, &owner);
    let failure = Failure::claimed(&mailbox, &claim);
    let stopped = |failure: Failure, token: Arc<CancelToken>| {
        tokio::task::spawn_blocking(move || failure.herdr_seed_stopped(&token))
    };
    assert!(
        !stopped(failure.clone(), token.clone()).await.unwrap(),
        "no stop yet"
    );
    state
        .user_stop
        .store(true, std::sync::atomic::Ordering::Release);
    assert!(stopped(failure.clone(), token.clone()).await.unwrap());
    assert!(
        !stopped(failure, token).await.unwrap(),
        "a stale claim settles nothing again"
    );
    let status = mailbox.v2_seed_status("seed-stop".into()).await;
    assert_eq!(
        status,
        Ok(crate::services::turn_orchestrator::SeedStatus::Gone)
    );
    let snapshot = mailbox
        .v2_request(|s| s.as_ref().unwrap().json())
        .await
        .unwrap();
    let owed = &snapshot["owed_drop_receipts"];
    assert_eq!(owed[0]["outcome"], "cancelled", "{snapshot}");
    assert_eq!(owed.as_array().map(Vec::len), Some(1), "{snapshot}");
    let kept = mailbox.v2_request(move |slot| {
        assert_original_and_ticket_kept(slot.as_ref().unwrap(), &person);
        Ok(())
    });
    kept.await.unwrap();
}
