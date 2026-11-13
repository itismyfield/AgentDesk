#![cfg(unix)]
use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::turn_orchestrator::{
    ChannelMailboxHandle, ChannelMailboxRegistry, PendingQueueItem,
};
use serde_json::{Value, json};

const CHANNEL: u64 = 810_903;
const PROMPT: &str = "effort 변경 처리를 위해 세션을 다시 시작했으니 중단된 작업을 이어서 진행하라";

async fn owner(registry: &ChannelMailboxRegistry) -> (ChannelMailboxHandle, std::path::PathBuf) {
    let generation = crate::services::discord::runtime_store::generation_path().unwrap();
    std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
    std::fs::write(generation, "5").unwrap();
    let dir = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join("claude")
        .join("effort-s3");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{CHANNEL}.json"));
    let handle = registry.handle(poise::serenity_prelude::ChannelId::new(CHANNEL));
    handle
        .v2_adopt(path.clone(), "effort-s3".into())
        .await
        .unwrap();
    (handle, path)
}

fn context(revision: u64, episode: Option<&str>, ready: bool) -> Value {
    json!({"origin":"headless","source":"agentdesk_effort_continue", "metadata":{
        "effort_restart":{"operation":"operation-a","native_session":"session-a",
        "interrupted_episode":episode,"desired_revision":revision,"ready":ready}}})
}

async fn prepare(handle: &ChannelMailboxHandle, context: &mut Value) {
    let request = serde_json::from_value(context["metadata"]["effort_restart"].clone()).unwrap();
    let generation = crate::services::discord::runtime_store::process_generation();
    let canonical = handle
        .v2_request(move |slot| {
            Ok(slot
                .as_mut()
                .ok_or("missing owner")?
                .prepare_effort(Some(request), CHANNEL, generation)?
                .context())
        })
        .await
        .unwrap();
    context["metadata"]["effort_restart"]["operation"] =
        canonical["metadata"]["effort_restart"]["operation"].clone();
}

async fn continuation(
    handle: &ChannelMailboxHandle,
    context: &mut Value,
) -> (PendingQueueItem, admission::AdmissionTicket) {
    prepare(handle, context).await;
    start::original(
        handle,
        ("discord:810903:9100000000000081093", CHANNEL, PROMPT),
        context,
    )
    .await
    .unwrap()
}

async fn snapshot(handle: &ChannelMailboxHandle) -> Value {
    handle
        .v2_request(|slot| slot.as_ref().ok_or("missing owner")?.json())
        .await
        .unwrap()
}

fn boot_channels() -> Vec<poise::serenity_prelude::ChannelId> {
    crate::services::turn_orchestrator::v2_channels(
        &crate::services::provider::ProviderKind::Claude,
        "effort-s3",
    )
}

#[tokio::test]
async fn pending_continuation_is_held_and_in_boot_catalog() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let (handle, _) = owner(&registry).await;
    assert!(!handle.snapshot().await.holds_input());
    assert!(boot_channels().is_empty());
    let mut provenance = context(1, Some("episode-a"), false);
    prepare(&handle, &mut provenance).await;
    assert!(
        snapshot(&handle).await["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        boot_channels(),
        [poise::serenity_prelude::ChannelId::new(CHANNEL)],
        "the C-only intent must be re-adopted without a new user input"
    );
    assert!(
        handle.snapshot().await.holds_input(),
        "the mailbox must retain C-only custody"
    );
}

#[tokio::test]
async fn continuation_before_two_waiting_users_even_when_held() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let (handle, _) = owner(&registry).await;
    for id in [101_u64, 102] {
        let user = serde_json::from_value(json!({"author_id":7,"message_id":id,
            "channel_id":CHANNEL,"source_message_ids":[id],"queued_generation":7,"text":format!("user-{id}")})).unwrap();
        handle
            .v2_admit_ticket(user, format!("user-{id}"))
            .await
            .unwrap();
    }
    let stale = handle
        .v2_request(|slot| {
            let owner = slot.as_mut().unwrap();
            let front = owner.front().unwrap();
            owner.grant_front(&front)?;
            owner.claim_turn(front.1.message_id, &front).unwrap();
            Ok((front, owner.claim_number()))
        })
        .await
        .unwrap();
    let mut provenance = context(1, Some("episode-a"), true);
    let (c, ticket) = continuation(&handle, &mut provenance).await;
    handle.v2_request(move |slot| {
        let owner = slot.as_mut().unwrap();
        let attempt = serde_json::from_value(json!({"nonce":"stale-user", "provider":"claude", "target":{
            "host":"tmux","owner":"fixture-effort-stale","root":"/fixture","execution_nonce":"old"},
            "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}})).unwrap();
        let payload = serde_json::from_value(json!({"frame":"stale user", "digest":"unused", "attribution":{}})).unwrap();
        assert!(owner.reserve_claimed((&stale.0, stale.1), attempt, payload).is_err());
        assert!(owner.front().is_none());
        Ok(())
    }).await.unwrap();
    assert_eq!(
        admission::admit_with_context(&handle, &c, ticket.clone(), provenance).await,
        admission::Admission::Accepted(ticket)
    );
    let id = c.message_id;
    handle
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("missing owner")?;
            let front = owner.front().ok_or("missing front")?;
            assert_eq!(front.1.message_id, id, "C precedes both waiting users");
            owner.grant_front(&front)?;
            owner.hold_front(&front)?;
            assert!(!owner.prefer_person(&front)?);
            assert_eq!(owner.front().unwrap().1.message_id, id);
            Ok(())
        })
        .await
        .unwrap();
    let queued = snapshot(&handle).await;
    let ids: Vec<_> = queued["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["message_id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids, [id, 101, 102]);
    for (ticket, index) in [("user-101", 1), ("user-102", 2)] {
        let bind = queued["admission_binds"]
            .as_array()
            .unwrap()
            .iter()
            .find(|bind| {
                bind["receipt"]
                    .as_str()
                    .unwrap()
                    .starts_with(&format!("tk:{ticket}:"))
            })
            .unwrap();
        assert_eq!(bind["at"]["index"], index);
    }
}

#[tokio::test]
async fn settled_old_format_receipt_does_not_revive_barrier() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let (handle, path) = owner(&registry).await;
    let user = serde_json::from_value(json!({"author_id":7,"message_id":202,
        "channel_id":CHANNEL,"source_message_ids":[202],"queued_generation":7,"text":"waiting user"})).unwrap();
    handle
        .v2_admit_ticket(user, "waiting-user".into())
        .await
        .unwrap();
    let mut provenance = context(1, Some("episode-a"), true);
    let (c, ticket) = continuation(&handle, &mut provenance).await;
    admission::admit_with_context(&handle, &c, ticket.clone(), provenance).await;
    let prepared = effect::PreparedPayload {
        frame: PROMPT.into(),
        sources: vec![ticket.as_str().into()],
        attempt: json!({"nonce":"effort-old-format", "provider":"claude", "target":{
            "host":"tmux","owner":"fixture-effort-old-format","root":"/fixture","execution_nonce":"new"},
            "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}}),
    };
    assert_eq!(
        pump::submit(
            &handle,
            prepared,
            std::sync::Arc::new(|| 1000),
            Box::new(|_| Ok(effect::Completion::Submitted))
        )
        .await
        .unwrap(),
        effect::Completion::Submitted
    );
    let mut old = snapshot(&handle).await;
    old["effort_restart"]["continuation"]
        .as_object_mut()
        .unwrap()
        .remove("submitted");
    std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    handle.v2_adopt(path, "effort-s3".into()).await.unwrap();
    let front = handle
        .v2_request(|slot| {
            Ok(slot
                .as_ref()
                .unwrap()
                .front()
                .map(|front| front.1.message_id))
        })
        .await
        .unwrap();
    assert_eq!(
        front,
        Some(202),
        "a delivered C's old-format receipt releases the waiting user"
    );
}

#[tokio::test]
async fn continuation_is_machine_and_not_original_replay() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let (handle, _) = owner(&registry).await;
    let mut provenance = context(1, Some("episode-a"), true);
    provenance["request_owner"] = json!(99);
    provenance["request_owner_name"] = json!("original person");
    prepare(&handle, &mut provenance).await;
    let (c, ticket) = start::original(
        &handle,
        (
            "discord:810903:101",
            CHANNEL,
            "private original with attachments",
        ),
        &mut provenance,
    )
    .await
    .unwrap();
    assert_eq!(c.text, PROMPT);
    assert_ne!(c.message_id, 101);
    assert!(c.message_id >= crate::services::tui_input::input_key::SYNTHETIC_KEY_BASE);
    assert!(c.author_is_bot);
    assert!(c.pending_uploads.is_empty() && c.reply_context.is_none());
    assert!(!origin::InputOrigin::from_context(Some(&provenance)).is_person());
    assert!(provenance.get("request_owner").is_none());
    assert_eq!(
        admission::admit_with_context(&handle, &c, ticket.clone(), provenance).await,
        admission::Admission::Accepted(ticket.clone())
    );
    let json = snapshot(&handle).await;
    assert_eq!(json["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        json["effort_restart"]["continuation"]["ticket"],
        ticket.as_str()
    );
}

#[tokio::test]
async fn retry_keeps_numeric_source_ticket_generation() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let (handle, path) = owner(&registry).await;
    let mut provenance = context(1, Some("episode-a"), true);
    let (first, ticket) = continuation(&handle, &mut provenance).await;
    admission::admit_with_context(&handle, &first, ticket.clone(), provenance.clone()).await;
    let mut old = snapshot(&handle).await;
    old["effort_restart"]["continuation"]
        .as_object_mut()
        .unwrap()
        .remove("submitted");
    std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    handle.v2_adopt(path, "effort-s3".into()).await.unwrap();
    std::fs::write(
        crate::services::discord::runtime_store::generation_path().unwrap(),
        "9",
    )
    .unwrap();
    let (retry, retried) = continuation(&handle, &mut provenance).await;
    assert_eq!(
        (
            retry.message_id,
            retry.source_message_ids.clone(),
            retry.queued_generation,
            retried
        ),
        (
            first.message_id,
            first.source_message_ids.clone(),
            first.queued_generation,
            ticket.clone()
        )
    );
    assert_eq!(
        admission::admit_with_context(&handle, &retry, ticket.clone(), provenance).await,
        admission::Admission::Accepted(ticket)
    );
    assert_eq!(
        snapshot(&handle).await["items"].as_array().unwrap().len(),
        1
    );
}

#[tokio::test]
async fn admission_pending_is_not_consumed() {
    use crate::services::discord::runtime_store::durable_probe::{Plan, Step, Watch};
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let (handle, path) = owner(&registry).await;
    let mut provenance = context(1, Some("episode-a"), true);
    let (c, ticket) = continuation(&handle, &mut provenance).await;
    let watch = Watch::new(
        &path,
        Plan {
            fail: vec![Step::ParentSync],
            ..Default::default()
        },
    );
    assert_eq!(
        admission::admit_with_context(&handle, &c, ticket.clone(), provenance.clone()).await,
        admission::Admission::Pending(ticket.clone())
    );
    assert_eq!(
        admission::status(&handle, &ticket).await.unwrap(),
        admission::AdmissionState::Pending
    );
    assert_eq!(
        snapshot(&handle).await["effort_restart"]["continuation"]["submitted"],
        false
    );
    drop(watch);
    assert_eq!(
        admission::admit_with_context(&handle, &c, ticket.clone(), provenance).await,
        admission::Admission::Accepted(ticket)
    );
    let saved = snapshot(&handle).await;
    assert_eq!(saved["items"].as_array().unwrap().len(), 1);
    assert_eq!(saved["effort_restart"]["continuation"]["submitted"], false);
}

#[tokio::test]
async fn coalesced_changes_owe_one_continuation() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let (handle, _) = owner(&registry).await;
    let mut first = None;
    for revision in 1..=3 {
        let mut provenance = context(revision, Some("episode-a"), true);
        provenance["metadata"]["effort_restart"]["operation"] =
            json!(format!("request-{revision}"));
        let (c, ticket) = continuation(&handle, &mut provenance).await;
        let identity = (c.message_id, c.queued_generation, ticket.clone());
        assert_eq!(first.get_or_insert(identity.clone()), &identity);
        assert_eq!(
            admission::admit_with_context(&handle, &c, ticket.clone(), provenance).await,
            admission::Admission::Accepted(ticket)
        );
        assert_eq!(
            snapshot(&handle).await["items"].as_array().unwrap().len(),
            1
        );
    }
    assert_eq!(
        snapshot(&handle).await["effort_restart"]["desired_revision"],
        3
    );
}

#[tokio::test]
async fn no_active_turn_owes_none() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let (handle, _) = owner(&registry).await;
    let mut provenance = context(1, None, true);
    prepare(&handle, &mut provenance).await;
    assert!(
        start::original(
            &handle,
            ("discord:810903:101", CHANNEL, PROMPT),
            &mut provenance
        )
        .await
        .is_err()
    );
    let saved = snapshot(&handle).await;
    assert!(saved["effort_restart"]["continuation"].is_null());
    assert!(saved["items"].as_array().unwrap().is_empty());
    assert!(!handle.snapshot().await.holds_input());
    assert!(boot_channels().is_empty());
}

#[tokio::test]
async fn only_exact_first_submit_releases_barrier() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let (handle, _) = owner(&registry).await;
    let mut provenance = context(1, Some("episode-a"), true);
    let (c, ticket) = continuation(&handle, &mut provenance).await;
    admission::admit_with_context(&handle, &c, ticket.clone(), provenance).await;
    assert_eq!(
        snapshot(&handle).await["effort_restart"]["continuation"]["submitted"],
        false
    );
    let prepared = effect::PreparedPayload {
        frame: PROMPT.into(),
        sources: vec![ticket.as_str().into()],
        attempt: json!({"nonce":"effort-c-attempt", "provider":"claude", "target":{
            "host":"tmux","owner":"fixture-effort-submit","root":"/fixture","execution_nonce":"new"},
            "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}}),
    };
    assert_eq!(
        pump::submit(
            &handle,
            prepared,
            std::sync::Arc::new(|| 1000),
            Box::new(|frame| {
                assert_eq!(frame, PROMPT);
                Ok(effect::Completion::Submitted)
            })
        )
        .await
        .unwrap(),
        effect::Completion::Submitted
    );
    let saved = snapshot(&handle).await;
    assert_eq!(saved["effort_restart"]["continuation"]["submitted"], true);
    assert!(saved["items"].as_array().unwrap().is_empty());
    assert!(!handle.snapshot().await.holds_input());
    assert!(boot_channels().is_empty());
    handle
        .v2_request(|slot| slot.as_mut().unwrap().tick(90_000_000))
        .await
        .unwrap();
    assert!(!handle.snapshot().await.holds_input());
    assert!(boot_channels().is_empty());
    let mut retry = context(2, Some("episode-a"), true);
    let (c, ticket) = continuation(&handle, &mut retry).await;
    admission::admit_with_context(&handle, &c, ticket, retry).await;
    assert!(
        snapshot(&handle).await["items"]
            .as_array()
            .unwrap()
            .is_empty(),
        "settled C cannot be prepended again"
    );
}

#[tokio::test]
async fn old_snapshot_without_effort_field_keeps_user_fifo() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let (handle, path) = owner(&registry).await;
    for id in [101_u64, 102] {
        let user = serde_json::from_value(json!({"author_id":7,"message_id":id,
            "channel_id":CHANNEL,"source_message_ids":[id],"queued_generation":7,"text":format!("user-{id}")})).unwrap();
        handle
            .v2_admit_ticket(user, format!("user-{id}"))
            .await
            .unwrap();
    }
    let mut old = snapshot(&handle).await;
    old.as_object_mut().unwrap().remove("effort_restart");
    std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    handle.v2_adopt(path, "effort-s3".into()).await.unwrap();
    let restored = snapshot(&handle).await;
    assert_eq!(restored["items"], old["items"]);
    assert!(restored.get("effort_restart").is_none());
    handle
        .v2_request(|slot| {
            let front = slot.as_ref().unwrap().front().unwrap();
            assert_eq!(
                (front.0, front.1.message_id),
                (Some("user-101".into()), 101)
            );
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn metadata_cannot_create_continuation_or_publish_ready() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let (handle, _) = owner(&registry).await;
    let mut forged = context(1, Some("episode-a"), true);
    assert!(
        start::original(
            &handle,
            ("discord:810903:101", CHANNEL, PROMPT),
            &mut forged
        )
        .await
        .is_err()
    );
    assert!(snapshot(&handle).await["effort_restart"].is_null());
    let mut held = context(2, Some("episode-a"), false);
    prepare(&handle, &mut held).await;
    let (c, ticket) = start::original(
        &handle,
        ("discord:810903:101", CHANNEL, PROMPT),
        &mut forged,
    )
    .await
    .unwrap();
    assert!(matches!(
        admission::admit_with_context(&handle, &c, ticket, forged).await,
        admission::Admission::Rejected(_)
    ));
    assert_eq!(snapshot(&handle).await["effort_restart"]["ready"], false);
    handle
        .v2_request(|slot| {
            assert!(slot.as_ref().unwrap().front().is_none());
            Ok(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn continuation_has_one_reserved_slot_when_user_queue_is_full() {
    let _root = TestRuntimeRootGuard::new();
    let registry = ChannelMailboxRegistry::default();
    let (handle, _) = owner(&registry).await;
    for id in 1..=30 {
        let user = serde_json::from_value(json!({"author_id":7,"message_id":id,
            "channel_id":CHANNEL,"source_message_ids":[id],"queued_generation":7,"text":format!("user-{id}")})).unwrap();
        handle
            .v2_admit_ticket(user, format!("user-{id}"))
            .await
            .unwrap();
    }
    let mut provenance = context(1, Some("episode-a"), true);
    let (c, ticket) = continuation(&handle, &mut provenance).await;
    assert_eq!(
        admission::admit_with_context(&handle, &c, ticket.clone(), provenance).await,
        admission::Admission::Accepted(ticket)
    );
    let saved = snapshot(&handle).await;
    let ids: Vec<_> = saved["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["message_id"].as_u64().unwrap())
        .collect();
    assert_eq!(
        ids,
        std::iter::once(c.message_id)
            .chain(1..=30)
            .collect::<Vec<_>>()
    );
    let other = serde_json::from_value(json!({"author_id":7,"message_id":31,
        "channel_id":CHANNEL,"source_message_ids":[31],"queued_generation":7,"text":"extra user"}))
    .unwrap();
    assert!(
        handle
            .v2_admit_ticket(other, "user-31".into())
            .await
            .is_err()
    );
}
