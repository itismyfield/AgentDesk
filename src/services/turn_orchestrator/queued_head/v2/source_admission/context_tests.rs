// Windows has no directory fsync, so the queued V2 owner these tests adopt refuses every commit.
#![cfg(unix)]

use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::discord::runtime_store::{discord_pending_queue_root, durable_probe};
use crate::services::turn_orchestrator::ChannelMailboxRegistry;
use poise::serenity_prelude::ChannelId;
use serde_json::json;

fn original() -> PendingQueueItem {
    serde_json::from_value(json!({"author_id":1,"message_id":8000000000000001772u64,
        "channel_id":7272901,"queued_generation":5,"text":"unchanged routine prompt"}))
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
    assert!(
        handle
            .v2_exact_turn(original(), "settled".into())
            .await
            .unwrap()
            .settled()
    );
    assert_eq!(
        handle
            .v2_admission_context(original(), "settled".into())
            .await
            .unwrap(),
        Some(context)
    );
}

/// Two originals with their own execution contexts, reserved as one unit and ungrouped by a
/// restart, each keep their context through the rebind, a new reserve and its settle.
#[tokio::test]
async fn an_ungrouped_unit_keeps_each_members_execution_context() {
    let _root = TestRuntimeRootGuard::new();
    let item = |n: u64, text: &str| -> PendingQueueItem {
        serde_json::from_value(
            json!({"author_id":1,"message_id":8000000000000001780u64 + n,
            "channel_id":7272911,"queued_generation":5,"text":text}),
        )
        .unwrap()
    };
    let (a, b) = (item(1, "routine a"), item(2, "routine b"));
    let (ca, cb) = (
        json!({"run_id":"run-a"}),
        json!({"run_id":"run-b","checkpoint":{"n":2}}),
    );
    let registry = ChannelMailboxRegistry::default();
    let (handle, path) = owner(&registry, 7272911).await;
    for (original, ticket, context) in [(&a, "ticket-a", &ca), (&b, "ticket-b", &cb)] {
        let admitted =
            handle.v2_admit_ticket_with_context(original.clone(), ticket.into(), context.clone());
        admitted.await.unwrap();
    }
    let attempt = |nonce: &str| {
        json!({"nonce":nonce,"provider":"claude",
            "target":{"host":"tmux","owner":format!("context-{nonce}"),"root":"/test","execution_nonce":"e"},
            "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}})
    };
    let first = attempt("unit");
    handle
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            let attempt = serde_json::from_value(first).map_err(|e| e.to_string())?;
            owner
                .reserve_unit(attempt, |i| Some((String::new(), i.text.clone())))
                .map(drop)
        })
        .await
        .unwrap();
    let mut stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    stored["items"][1]["created_at_wall_time_ms"] = json!(123);
    std::fs::write(&path, stored.to_string()).unwrap();
    let restarted = ChannelMailboxRegistry::default();
    let (restored, _) = owner(&restarted, 7272911).await;
    let contexts = || async {
        let a = restored
            .v2_admission_context(a.clone(), "ticket-a".into())
            .await
            .unwrap();
        (
            a,
            restored
                .v2_admission_context(b.clone(), "ticket-b".into())
                .await
                .unwrap(),
        )
    };
    assert_eq!(
        contexts().await,
        (Some(ca.clone()), Some(cb.clone())),
        "after the rebind"
    );
    let second = attempt("again");
    restored
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            let attempt = serde_json::from_value(second).map_err(|e| e.to_string())?;
            let key = owner.reserve_unit(attempt, |i| Some((String::new(), i.text.clone())))?;
            drop(owner.arm(&key, 1)?);
            owner.settle(&key, true, 2)
        })
        .await
        .unwrap();
    assert_eq!(
        contexts().await,
        (Some(ca), Some(cb)),
        "after a new reserve and its settle"
    );
}

#[test]
fn person_selection_moves_items_and_ticket_binds_atomically_across_restart() {
    let _root = TestRuntimeRootGuard::new();
    let path = discord_pending_queue_root()
        .unwrap()
        .join("claude/person-context/789641.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut owner = Owner::adopt(path.clone(), "people".into()).unwrap();
    let by = |n, author, text: &str| {
        let mut item = original();
        item.message_id += n;
        item.author_id = author;
        item.text = text.to_string();
        item.merge_consecutive = true;
        item
    };
    let ticket = |n| Ticket::new(&format!("person-{n}")).unwrap();
    let turn = |owner: &Owner, n, original: &PendingQueueItem| {
        owner.exact_turn(&ticket(n), original).unwrap()
    };
    let machine = by(1, 7, "기계 시작");
    owner
        .admit_with_context(
            &machine,
            ticket(1),
            json!({"origin":"headless", "source":"http"}),
        )
        .unwrap();
    let first = by(2, 7, "첫 사람");
    owner
        .admit_with_context(
            &first,
            ticket(2),
            json!({"origin":"headless", "source":"imessage"}),
        )
        .unwrap();
    let second = by(3, 7, "둘째 사람");
    Custody::with_ticket(&second, ticket(3))
        .advance(&mut owner)
        .unwrap();
    let front = owner.front().unwrap();
    owner.grant_front(&front).unwrap();
    assert!(
        owner.followers().is_empty(),
        "machines never merge people or other starts"
    );
    let before = std::fs::read(&path).unwrap();
    owner.fault = super::super::Fault::Before;
    assert!(owner.prefer_person(&front).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(matches!(turn(&owner, 1, &machine), TurnBind::NotArmed));
    owner.fault = super::super::Fault::None;
    assert!(owner.prefer_person(&front).unwrap());
    drop(owner);
    let mut owner = Owner::adopt(path.clone(), "people".into()).unwrap();
    assert_eq!(
        owner
            .snapshot
            .items
            .iter()
            .map(|i| i.message_id)
            .collect::<Vec<_>>(),
        [first.message_id, machine.message_id, second.message_id]
    );
    for (n, original) in [(1, &machine), (2, &first), (3, &second)] {
        assert!(matches!(turn(&owner, n, original), TurnBind::NotArmed));
    }
    assert!(
        owner.followers().is_empty(),
        "people cannot carry a machine follower"
    );
    let key = owner.reserve_unit(serde_json::from_value(json!({"nonce":"first-person", "provider":"claude",
        "target":{"host":"tmux","owner":"person-context","root":"/test","execution_nonce":"e"},
        "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}})).unwrap(), |item| (item.message_id == first.message_id).then(|| ("person".into(), item.text.clone()))).unwrap();
    drop(owner.arm(&key, 100).unwrap());
    owner.settle(&key, true, 101).unwrap();
    assert!(matches!(turn(&owner, 2, &first), TurnBind::Settled(_)));
    let mut retry = first.clone();
    retry.queued_generation = 99;
    let resumed = owner.retry_original("person-2", &retry).unwrap();
    assert_eq!(resumed.queued_generation, 5);
    assert_eq!(
        owner
            .admit_with_context(
                &resumed,
                ticket(2),
                json!({"origin":"headless", "source":"imessage"})
            )
            .unwrap(),
        Admitted::Duplicate
    );
    retry.text = "changed retry".into();
    assert!(owner.retry_original("person-2", &retry).is_err());
    assert!(matches!(turn(&owner, 1, &machine), TurnBind::NotArmed));
    assert!(owner.prefer_person(&owner.front().unwrap()).unwrap());
    assert_eq!(owner.front().unwrap().1.message_id, second.message_id);
    assert!(matches!(turn(&owner, 3, &second), TurnBind::NotArmed));
    assert!(matches!(turn(&owner, 1, &machine), TurnBind::NotArmed));
}
