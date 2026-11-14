#![cfg(unix)]
//! An unsubmitted continuation leaves the owner only together with its intent: a removal that
//! drops it, or a start that fails before any key, closes the barrier in the same commit.
use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{
    ChannelMailboxHandle, ChannelMailboxRegistry, PendingQueueItem, QueuePersistenceContext,
};
use poise::serenity_prelude::{ChannelId, MessageId};
use serde_json::{Value, json};

const PROMPT: &str = "effort 변경 처리를 위해 세션을 다시 시작했으니 중단된 작업을 이어서 진행하라";

/// A ready continuation queued ahead of waiting user 101, on an adopted owner of `channel`.
async fn waiting(
    registry: &ChannelMailboxRegistry,
    channel: u64,
) -> (ChannelMailboxHandle, std::path::PathBuf, PendingQueueItem) {
    let generation = crate::services::discord::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
    std::fs::write(generation, "5").unwrap();
    let dir = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join("effort-close");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{channel}.json"));
    let handle = registry.handle(ChannelId::new(channel));
    handle
        .v2_adopt(path.clone(), "effort-close".into())
        .await
        .unwrap();
    user(&handle, channel, 101).await;
    let c = continuation(&handle, channel).await.unwrap();
    assert_eq!(front(&handle).await, Some(c.message_id), "C runs first");
    (handle, path, c)
}

async fn user(handle: &ChannelMailboxHandle, channel: u64, id: u64) {
    let user = serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":channel,
        "source_message_ids":[id],"queued_generation":7,"text":format!("user-{id}")}))
    .unwrap();
    handle
        .v2_admit_ticket(user, format!("user-{id}"))
        .await
        .unwrap();
}

/// Records the ready intent of a cut episode and admits its continuation, as a restart does.
async fn continuation(
    handle: &ChannelMailboxHandle,
    channel: u64,
) -> Result<PendingQueueItem, String> {
    let mut context = json!({"origin":"headless","source":"agentdesk_effort_continue",
        "metadata":{"effort_restart":{"operation":"operation-close","native_session":"session-close",
        "interrupted_episode":"episode-close","desired_revision":1,"ready":true}}});
    let request = serde_json::from_value(context["metadata"]["effort_restart"].clone()).unwrap();
    let generation = crate::services::discord::runtime_store::process_generation();
    handle
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("missing owner")?;
            owner
                .prepare_effort(Some(request), channel, generation)
                .map(drop)
        })
        .await?;
    let turn = format!("discord:{channel}:9100000000000081094");
    let (c, ticket) =
        start::original(handle, (turn.as_str(), channel, PROMPT), &mut context).await?;
    match admission::admit_with_context(handle, &c, ticket, context).await {
        admission::Admission::Accepted(_) => Ok(c),
        refused => Err(format!("{refused:?}")),
    }
}

async fn snapshot(handle: &ChannelMailboxHandle) -> Value {
    handle
        .v2_request(|slot| slot.as_ref().ok_or("missing owner")?.json())
        .await
        .unwrap()
}

async fn front(handle: &ChannelMailboxHandle) -> Option<u64> {
    let read = handle.v2_request(|slot| {
        let owner = slot.as_ref().ok_or("missing owner")?;
        Ok(owner.front().map(|front| front.1.message_id))
    });
    read.await.unwrap()
}

/// The queued message ids and whether the intent is closed, as committed.
async fn state(handle: &ChannelMailboxHandle) -> (Vec<u64>, bool) {
    let json = snapshot(handle).await;
    let ids = json["items"].as_array().unwrap().iter();
    let ids = ids
        .map(|item| item["message_id"].as_u64().unwrap())
        .collect();
    (ids, json["effort_restart"]["cancelled"] == true)
}

/// The owner a restarted server adopts from the same queue file.
async fn readopted(
    path: &std::path::Path,
    channel: u64,
) -> (ChannelMailboxRegistry, ChannelMailboxHandle) {
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(ChannelId::new(channel));
    let adopt = handle.v2_adopt(path.to_path_buf(), "effort-close".into());
    adopt.await.unwrap();
    (registry, handle)
}

fn ctx() -> QueuePersistenceContext {
    QueuePersistenceContext::new(&ProviderKind::Claude, "effort-close", None)
}

// A continuation whose launch fails twice before any key stays retained and closes its barrier:
// B, then D, run, and C is not queued again, also after a restart.
#[tokio::test]
async fn a_continuation_that_fails_to_start_twice_is_kept_and_frees_the_queue() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let channel = 810_911;
    let (handle, path, c) = waiting(&registry, channel).await;
    for _ in 0..2 {
        let failed = handle.v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("missing owner")?;
            let front = owner.front().ok_or("missing front")?;
            owner.grant_front(&front)?;
            owner.claim_turn(front.1.message_id, &front);
            let number = owner.claim_number();
            let members = std::slice::from_ref(&front.1);
            let kind = serde_json::from_value(json!("opaque")).map_err(|e| e.to_string())?;
            owner.start_failed(&front, number, members, None, kind, "cli missing")
        });
        failed.await.unwrap();
    }
    let json = snapshot(&handle).await;
    let retained = json["retained_inputs"].as_array().unwrap();
    assert_eq!(retained.len(), 1, "{json}");
    assert_eq!(retained[0]["input"]["original"]["message_id"], c.message_id);
    assert!(
        retained[0]["input"]["attempt"].is_null(),
        "kept, never tried"
    );
    assert_eq!(state(&handle).await, (vec![101], true), "{json}");
    assert_eq!(front(&handle).await, Some(101), "B runs");
    user(&handle, channel, 102).await;
    assert_eq!(state(&handle).await.0, [101, 102], "then D");
    assert!(
        continuation(&handle, channel).await.is_err(),
        "a closed continuation is not admitted again"
    );
    assert_eq!(state(&handle).await.0, [101, 102]);

    let (_registry, restarted) = readopted(&path, channel).await;
    assert_eq!(state(&restarted).await, (vec![101, 102], true));
    assert_eq!(front(&restarted).await, Some(101));
    let kept = snapshot(&restarted).await["retained_inputs"].clone();
    assert_eq!(kept.as_array().unwrap().len(), 1, "C stays retained");
}

// `/clear`, cancel-all and the single cancel of C each close the intent in the commit that drops
// C, so the next input claims the front, also after a restart.
#[tokio::test]
async fn dropping_the_waiting_continuation_closes_its_barrier() {
    let _root = TestRuntimeRootGuard::new();
    for (n, how) in ["clear", "cancel-all", "cancel-c"].into_iter().enumerate() {
        let registry = ChannelMailboxRegistry::default();
        let channel = 810_920 + n as u64;
        let (handle, path, c) = waiting(&registry, channel).await;
        match how {
            "clear" => drop(handle.clear(ctx()).await),
            "cancel-all" => {
                let all = handle.v2_request(|slot| {
                    slot.as_mut()
                        .ok_or("missing owner")?
                        .clear_waiting()
                        .map(drop)
                });
                all.await.unwrap();
            }
            _ => {
                let id = MessageId::new(c.message_id);
                let removed = handle.cancel_queued_primary_message(id, ctx()).await;
                assert!(removed.removed.is_some(), "{how}: C is cancelled");
            }
        }
        let kept: &[u64] = if how == "cancel-c" { &[101] } else { &[] };
        assert_eq!(state(&handle).await, (kept.to_vec(), true), "{how}");
        user(&handle, channel, 102).await;
        let first = if how == "cancel-c" { 101 } else { 102 };
        assert_eq!(front(&handle).await, Some(first), "{how}: no barrier");
        let (_registry, restarted) = readopted(&path, channel).await;
        assert_eq!(
            front(&restarted).await,
            Some(first),
            "{how}: after a restart"
        );
    }
}

// A continuation already handed to the provider is not the clear's to drop: it keeps its intent
// and barrier until its delivery settles.
#[tokio::test]
async fn an_attempted_continuation_survives_clear() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let channel = 810_930;
    let (handle, _, _) = waiting(&registry, channel).await;
    handle
        .v2_request(|slot| {
            let owner = slot.as_mut().ok_or("missing owner")?;
            let front = owner.front().ok_or("missing front")?;
            owner.grant_front(&front)?;
            owner.claim_turn(front.1.message_id, &front);
            let number = owner.claim_number();
            let attempt =
                serde_json::from_value(json!({"nonce":"effort-close-c","provider":"claude",
                "target":{"host":"tmux","owner":"fixture-effort-close","root":"/fixture",
                "execution_nonce":"new"},"source":{"kind":"pre_source","expected_session_id":null,
                "expected_transcript":null,"binding_watermark":null}}))
                .map_err(|e| e.to_string())?;
            let payload =
                serde_json::from_value(json!({"frame":PROMPT,"digest":"c","attribution":{}}))
                    .map_err(|e| e.to_string())?;
            owner
                .reserve_claimed((&front, number), attempt, payload)
                .map(drop)
        })
        .await
        .unwrap();
    handle.clear(ctx()).await;
    let json = snapshot(&handle).await;
    assert!(json["head_injection"].is_object(), "{json}");
    assert_eq!(json["effort_restart"]["cancelled"], Value::Null, "{json}");
    assert_eq!(json["effort_restart"]["continuation"]["submitted"], false);
}
