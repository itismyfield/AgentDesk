// Windows has no directory fsync, so the queued V2 owner these tests adopt refuses every commit.
#![cfg(unix)]

use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::discord::runtime_store::discord_pending_queue_root;
use crate::services::turn_orchestrator::checked_purge::ExpectedQueue;
use crate::services::turn_orchestrator::queued_head::v2_types::{
    AttemptIdentity, AttemptSource, PreparedPayload, TargetIdentity,
};
use crate::services::turn_orchestrator::{ChannelMailboxHandle, ChannelMailboxRegistry};
use poise::serenity_prelude::{ChannelId, MessageId};
use serde_json::{Value, json};
use std::path::PathBuf;

fn original(channel: u64, id: u64) -> crate::services::turn_orchestrator::PendingQueueItem {
    serde_json::from_value(
        json!({"author_id":1,"message_id":id,"source_message_ids":[id],
        "channel_id":channel,"queued_generation":5,"text":format!("input {id}")}),
    )
    .unwrap()
}

/// The provenance a headless start keeps, naming `origin` when it has one.
fn provenance(origin: Option<&str>) -> Value {
    let metadata = origin.map(|o| json!({"human_input": {"origin_id": o}}));
    json!({"origin":"headless","metadata":metadata})
}

async fn owner(registry: &ChannelMailboxRegistry, channel: u64) -> (ChannelMailboxHandle, PathBuf) {
    let dir = discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join("owed");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{channel}.json"));
    let handle = registry.handle(ChannelId::new(channel));
    handle.v2_adopt(path.clone(), "owed".into()).await.unwrap();
    (handle, path)
}

async fn admit(handle: &ChannelMailboxHandle, channel: u64, id: u64, origin: Option<&str>) {
    let ticket = format!("ticket-{id}");
    let admitted =
        handle.v2_admit_ticket_with_context(original(channel, id), ticket, provenance(origin));
    admitted.await.unwrap();
}

fn on_disk(path: &PathBuf) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn owed(snapshot: &Value) -> Vec<(u64, String, String)> {
    let list = snapshot["owed_drop_receipts"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    (list.iter())
        .map(|o| {
            (
                o["message_id"].as_u64().unwrap(),
                o["origins"][0].to_string(),
                o["outcome"].to_string(),
            )
        })
        .collect()
}

fn waiting(channel: u64, origin: &str) -> Option<Waiting> {
    waiting_origin("claude", channel, origin).unwrap()
}

/// A cancel or clear owes each removed origin in its removing commit, never an origin-less input;
/// only an exact ack clears it, and an empty set keeps the file in the shape an older build reads.
#[tokio::test]
async fn removed_origins_are_owed_by_the_removing_commit_until_acked() {
    let _root = TestRuntimeRootGuard::new();
    let (registry, channel) = (ChannelMailboxRegistry::default(), 7_340_301);
    let (handle, path) = owner(&registry, channel).await;
    admit(&handle, channel, 11, Some("o-11")).await;
    admit(&handle, channel, 12, Some("o-12")).await;
    admit(&handle, channel, 13, None).await;
    assert_eq!(waiting(channel, "o-11"), Some(Waiting::Accepted));
    assert_eq!(waiting(channel, "o-unknown"), None);

    let cancel = handle.cancel_queued_primary_message(MessageId::new(11), ctx());
    assert!(cancel.await.removed.is_some());
    let disk = on_disk(&path);
    assert_eq!(
        owed(&disk),
        [(11, "\"o-11\"".into(), "\"cancelled\"".into())]
    );
    let Some(Waiting::Owed(first)) = waiting(channel, "o-11") else {
        panic!("a cancelled origin is owed in the file");
    };
    assert_eq!(waiting(channel, "o-12"), Some(Waiting::Accepted));

    let cleared = handle.clear(ctx()).await;
    assert_eq!(cleared.discarded_message_ids.len(), 2);
    let disk = on_disk(&path);
    let ids: Vec<u64> = owed(&disk).iter().map(|o| o.0).collect();
    assert_eq!(ids, [11, 12], "the origin-less input owes nothing");
    assert!(disk["items"].as_array().unwrap().is_empty());

    let ack =
        handle.v2_request(move |slot| slot.as_mut().ok_or("no owner")?.ack_owed_drops(&[first]));
    ack.await.unwrap();
    assert_eq!(waiting(channel, "o-11"), None);
    assert!(matches!(waiting(channel, "o-12"), Some(Waiting::Owed(_))));
    let rest = handle.v2_request(|slot| Ok(slot.as_ref().ok_or("no owner")?.owed_drops()));
    let rest = rest.await.unwrap();
    let ack = handle.v2_request(move |slot| slot.as_mut().ok_or("no owner")?.ack_owed_drops(&rest));
    ack.await.unwrap();
    assert!(on_disk(&path).get("owed_drop_receipts").is_none());
}

fn attempt(nonce: &str) -> AttemptIdentity {
    AttemptIdentity {
        nonce: nonce.into(),
        provider: "claude".into(),
        target: TargetIdentity {
            host: "tmux".into(),
            owner: "owed-pane".into(),
            root: "/test".into(),
            execution_nonce: "execution".into(),
        },
        source: AttemptSource::Existing {
            session_id: "native".into(),
            transcript: "/test/rollout".into(),
            file: [1, 2],
            offset: 0,
        },
    }
}

/// Reserves the head under `nonce` and returns the owner's refusal of it as proven unsent.
async fn refuse_head(
    handle: &ChannelMailboxHandle,
    channel: u64,
    nonce: &str,
    veto: &str,
) -> Value {
    let attempt = attempt(nonce);
    let key = handle.v2_request(move |slot| {
        let owner = slot.as_mut().ok_or("no owner")?;
        let payload = PreparedPayload {
            frame: "frame".into(),
            digest: "digest".into(),
            attribution: json!({}),
            followers: Vec::new(),
        };
        let key = owner.reserve(attempt, payload)?;
        drop(owner.arm(&key, 0)?);
        serde_json::to_value(&key).map_err(|e| e.to_string())
    });
    let key = key.await.unwrap();
    let veto = veto.to_string();
    let target = json!({"host":"none","_attempt":key.clone()});
    let (_, proof) = crate::services::claude_tui::owner_input::capture_scope(
        "claude".into(),
        target,
        channel,
        std::sync::Arc::new(|_| Ok(())),
        std::sync::Arc::new(|_, _| Ok(())),
        std::sync::Arc::new(|_| Ok(())),
        || crate::services::claude_tui::owner_input::vetoed(&veto),
    );
    let proof = proof.expect("a veto before any key proves not-sent");
    let retain_key = key.clone();
    let retained = handle.v2_request(move |slot| {
        let key = serde_json::from_value(retain_key).map_err(|e| e.to_string())?;
        slot.as_mut()
            .ok_or("no owner")?
            .retain_not_sent(&key, proof)
    });
    retained.await.unwrap();
    key
}

/// A permanent refusal owes a failed receipt as it is retained; a retryable one owes nothing
/// until the person is told, after which it is never resubmitted.
#[tokio::test]
async fn a_refused_input_owes_failed_once_nothing_will_resubmit_it() {
    let _root = TestRuntimeRootGuard::new();
    let (registry, channel) = (ChannelMailboxRegistry::default(), 7_340_302);
    let (handle, path) = owner(&registry, channel).await;
    admit(&handle, channel, 21, Some("o-21")).await;
    refuse_head(&handle, channel, "nonce-21", "InvalidInput").await;
    let disk = on_disk(&path);
    assert_eq!(owed(&disk), [(21, "\"o-21\"".into(), "\"failed\"".into())]);

    admit(&handle, channel, 22, Some("o-22")).await;
    let key = refuse_head(&handle, channel, "nonce-22", "pane_busy").await;
    assert_eq!(
        owed(&on_disk(&path)).len(),
        1,
        "a retryable refusal owes nothing yet"
    );
    assert_eq!(waiting(channel, "o-22"), Some(Waiting::Accepted));
    let told = handle.v2_request(move |slot| {
        let key = serde_json::from_value(key).map_err(|e| e.to_string())?;
        slot.as_mut().ok_or("no owner")?.notice_enqueued(&key)
    });
    told.await.unwrap();
    let ids: Vec<(u64, String)> = owed(&on_disk(&path))
        .into_iter()
        .map(|o| (o.0, o.2))
        .collect();
    assert_eq!(ids, [(21, "\"failed\"".into()), (22, "\"failed\"".into())]);
}

/// The purge a forced cancel runs owes what it drains, while the attempted head stays unowed.
#[tokio::test]
async fn a_purge_owes_untried_origins_and_leaves_the_attempted_head() {
    let _root = TestRuntimeRootGuard::new();
    let (registry, channel) = (ChannelMailboxRegistry::default(), 7_340_303);
    let (handle, path) = owner(&registry, channel).await;
    admit(&handle, channel, 31, Some("o-31")).await;
    admit(&handle, channel, 32, Some("o-32")).await;
    let reserve = handle.v2_request(move |slot| {
        let owner = slot.as_mut().ok_or("no owner")?;
        let payload = PreparedPayload {
            frame: "frame".into(),
            digest: "digest".into(),
            attribution: json!({}),
            followers: Vec::new(),
        };
        let key = owner.reserve(attempt("nonce-31"), payload)?;
        drop(owner.arm(&key, 0)?);
        Ok(())
    });
    reserve.await.unwrap();
    let fingerprint = handle.v2_request(|slot| slot.as_ref().ok_or("no owner")?.fingerprint());
    let fingerprint = fingerprint.await.unwrap();
    let expected = ExpectedQueue::of(&handle.snapshot().await).with_v2(fingerprint);
    let purged = handle
        .try_purge_queue_if_unchanged(ctx(), expected)
        .await
        .unwrap();
    let drained: Vec<u64> = purged.receipt.iter().map(|i| i.message_id.get()).collect();
    assert_eq!(drained, [32]);
    assert_eq!(
        owed(&on_disk(&path)),
        [(32, "\"o-32\"".into(), "\"cancelled\"".into())]
    );
    assert_eq!(waiting(channel, "o-31"), Some(Waiting::Accepted));
}

fn ctx() -> crate::services::turn_orchestrator::QueuePersistenceContext {
    let provider = crate::services::provider::ProviderKind::Claude;
    crate::services::turn_orchestrator::QueuePersistenceContext::new(&provider, "owed", None)
}

/// A file the reader cannot parse is an error, never an origin no queue holds.
#[test]
fn an_unreadable_queue_file_is_an_error_not_an_absent_origin() {
    let _root = TestRuntimeRootGuard::new();
    let dir = discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join("torn");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("7340304.json"), b"{\"version\":2,\"items\":[").unwrap();
    assert!(waiting_origin("claude", 7_340_304, "o-1").is_err());
    assert_eq!(waiting_origin("claude", 7_340_305, "o-1"), Ok(None));
}
