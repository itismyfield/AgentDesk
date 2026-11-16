// Windows has no directory fsync, so the queued V2 owner these tests adopt refuses every commit.
#![cfg(unix)]

use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{
    ChannelMailboxHandle, ChannelMailboxRegistry, QueuePersistenceContext,
};
use serde_json::json;

fn item(channel: u64, id: u64) -> PendingQueueItem {
    serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":channel,
        "source_message_ids":[id],"queued_generation":5,"text":format!("original {id}")}))
    .unwrap()
}

fn ticket(id: u64) -> String {
    format!("caller-{id}")
}

fn attempt(nonce: &str) -> AttemptIdentity {
    serde_json::from_value(json!({"nonce":nonce,"provider":"claude",
        "target":{"host":"tmux","owner":"requeue-pane","root":"/test","execution_nonce":"e"},
        "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,
            "binding_watermark":null}}))
    .unwrap()
}

/// An adopted owner on `channel`'s queue file, as a boot finds it.
async fn adopt(channel: u64) -> (ChannelMailboxRegistry, ChannelMailboxHandle) {
    let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    let file = root
        .join("claude/v2-requeue")
        .join(format!("{channel}.json"));
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(ChannelId::new(channel));
    handle.v2_adopt(file, "v2-requeue".into()).await.unwrap();
    (registry, handle)
}

/// Admits each id under its own ticket with a caller context naming it.
async fn admit(handle: &ChannelMailboxHandle, channel: u64, ids: &[u64]) {
    for id in ids {
        let context = json!({"caller": id});
        let admitted =
            handle.v2_admit_ticket_with_context(item(channel, *id), ticket(*id), context);
        assert!(admitted.await.is_ok(), "{id}");
    }
}

/// What an older binary left: the unit reserved over `unit` refused as proven unsent and retained
/// with its resend ask `told` or not, and the queue then holding `queued` with only their own binds.
async fn retain(handle: &ChannelMailboxHandle, unit: &'static [u64], queued: Vec<u64>, told: bool) {
    let channel_items = move |owner: &Owner, id: u64| {
        let found = owner
            .snapshot
            .items
            .iter()
            .find(|i| i.message_id == id)
            .cloned();
        found.ok_or_else(|| format!("{id} not queued"))
    };
    handle
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("no owner")?;
            let copies = (queued.iter().map(|id| channel_items(owner, *id)))
                .collect::<Result<Vec<_>, _>>()?;
            let prepared = |i: &PendingQueueItem| {
                unit.contains(&i.message_id)
                    .then(|| (String::new(), i.text.clone()))
            };
            owner.reserve_unit(attempt(&format!("old-{told}")), prepared)?;
            let mut next = owner.snapshot.clone();
            let input = next.head_injection.take().ok_or("no head")?;
            let queued_binds: Vec<AdmissionBind> = (next.admission_binds.iter())
                .filter(|b| matches!(b.at, BindAt::Queued { .. }))
                .cloned()
                .collect();
            next.admission_binds
                .retain(|b| !matches!(b.at, BindAt::Queued { .. }));
            next.items = copies;
            for (index, item) in next.items.clone().iter().enumerate() {
                let digest = next.queued_digest(index).ok_or("no digest")?;
                let own = queued_binds.iter().find(|b| {
                    matches!(&b.at, BindAt::Queued { item: d, .. } if *d == digest)
                        && !unit.contains(&item.message_id)
                });
                if let Some(own) = own {
                    let mut bind = own.clone();
                    bind.at = BindAt::Queued {
                        index,
                        item: digest,
                    };
                    next.admission_binds.push(bind);
                }
            }
            next.retained_inputs.push(RetainedInput {
                input,
                reason: RetainedReason::Rejected {
                    reason: "known_not_sent: vetoed: NotBusy".into(),
                    start_failure: None,
                },
                disposition: Some(RetainedDisposition::KnownNotSent),
                notice: NoticeIntent {
                    kind: "retained_retry_required".into(),
                    enqueued: told,
                },
            });
            owner.revoke_front();
            owner.commit(next)
        })
        .await
        .unwrap();
}

/// Queued ids in order, and each id's ticket standing and caller context as callers read them.
async fn read(handle: &ChannelMailboxHandle, channel: u64) -> (Vec<u64>, Vec<String>) {
    let ids: Vec<u64> = (handle.v2_request(|slot| {
        let owner = slot.as_ref().ok_or("no owner")?;
        Ok(owner.snapshot.items.iter().map(|i| i.message_id).collect())
    }))
    .await
    .unwrap();
    let mut seen = Vec::new();
    for id in &ids {
        let original = item(channel, *id);
        let (ticket, lookup) = (Ticket::new(&ticket(*id)).unwrap(), original.clone());
        let bind = handle.v2_request(move |slot| {
            Ok(format!(
                "{:?}",
                slot.as_ref()
                    .ok_or("no owner")?
                    .exact_turn(&ticket, &lookup)?
            ))
        });
        let context = handle.v2_admission_context(original, ticket_name(*id));
        let (bind, context) = (bind.await.unwrap(), context.await.unwrap());
        seen.push(format!("{id}:{bind}:{}", context.unwrap_or_default()));
    }
    (ids, seen)
}

fn ticket_name(id: u64) -> String {
    ticket(id)
}

fn standing(ids: &[u64]) -> Vec<String> {
    ids.iter()
        .map(|id| format!("{id}:NotArmed:{}", json!({"caller": id})))
        .collect()
}

async fn requeue(handle: &ChannelMailboxHandle) -> usize {
    let moved = handle.v2_request(|slot| {
        let owner = slot.as_mut().ok_or("no owner")?;
        owner.requeue_known_not_sent().map(|moved| moved.len())
    });
    moved.await.unwrap()
}

/// A retained input whose resend ask already went out stays retained through every boot and is
/// never moved; one whose ask was never posted requeues once with its tickets and caller contexts.
#[tokio::test(flavor = "current_thread")]
async fn only_an_untold_proven_unsent_input_requeues_and_a_told_one_stays() {
    let _root = TestRuntimeRootGuard::new();
    for (channel, told) in [(767_630, true), (767_631, false)] {
        let (registry, handle) = adopt(channel).await;
        admit(&handle, channel, &[10, 11, 20]).await;
        retain(&handle, &[10, 11], vec![20], told).await;
        drop((handle, registry));
        for boot in 0..2 {
            let (registry, handle) = adopt(channel).await;
            let moved = requeue(&handle).await;
            let (ids, seen) = read(&handle, channel).await;
            let retained = (handle
                .v2_request(|s| Ok(s.as_ref().unwrap().snapshot.retained_inputs.len())))
            .await
            .unwrap();
            match (told, boot) {
                (true, _) => {
                    assert_eq!((moved, retained), (0, 1), "told stays: boot {boot}");
                    assert_eq!(ids, [20], "boot {boot}");
                }
                (false, 0) => assert_eq!((moved, retained), (1, 0)),
                (false, _) => assert_eq!((moved, retained), (0, 0), "once"),
            }
            if !told {
                assert_eq!(ids, [10, 11, 20], "boot {boot}");
                assert_eq!(seen, standing(&[10, 11, 20]), "boot {boot}");
            }
            drop((handle, registry));
        }
    }
}

/// An equal copy already queued merges into the retained member's slot: the order is the retained
/// members' and every original ticket keeps its slot and caller context, across a restart too.
#[tokio::test(flavor = "current_thread")]
async fn an_equal_queued_copy_merges_in_retained_order_with_every_ticket() {
    let _root = TestRuntimeRootGuard::new();
    for (channel, queued) in [(767_632, vec![10, 20]), (767_633, vec![20, 11])] {
        let (registry, handle) = adopt(channel).await;
        admit(&handle, channel, &[10, 11, 20]).await;
        retain(&handle, &[10, 11], queued.clone(), false).await;
        drop((handle, registry));
        let (registry, handle) = adopt(channel).await;
        assert_eq!(requeue(&handle).await, 1, "{queued:?}");
        let (ids, seen) = read(&handle, channel).await;
        assert_eq!(ids, [10, 11, 20], "{queued:?}");
        assert_eq!(seen, standing(&[10, 11, 20]), "{queued:?}");
        drop((handle, registry));
        let (_registry, handle) = adopt(channel).await;
        assert_eq!(read(&handle, channel).await.1, standing(&[10, 11, 20]));
    }
}

/// A cancel takes only the original it names: an unrelated proven-unsent input stays retained and
/// a cancelled retained member takes its own input out, its follower queued with its ticket.
#[tokio::test(flavor = "current_thread")]
async fn a_cancel_moves_only_the_input_it_names() {
    let _root = TestRuntimeRootGuard::new();
    let channel = 767_634;
    let (_registry, handle) = adopt(channel).await;
    admit(&handle, channel, &[10, 11, 20]).await;
    retain(&handle, &[10, 11], vec![20], false).await;
    let retained = || {
        handle.v2_request(|s| {
            let owner = s.as_ref().unwrap();
            Ok(serde_json::to_value(&owner.snapshot.retained_inputs).unwrap())
        })
    };
    let before = retained().await.unwrap();
    let cancelled =
        handle.v2_request(|s| Ok(cancel(s.as_mut().unwrap(), MessageId::new(20), false)));
    assert_eq!(
        cancelled.await.unwrap().removed.map(|r| r.message_id.get()),
        Some(20)
    );
    assert_eq!(
        retained().await.unwrap(),
        before,
        "the unrelated input did not move"
    );
    assert_eq!(read(&handle, channel).await.0, Vec::<u64>::new());
    let cancelled =
        handle.v2_request(|s| Ok(cancel(s.as_mut().unwrap(), MessageId::new(10), false)));
    assert_eq!(
        cancelled.await.unwrap().removed.map(|r| r.message_id.get()),
        Some(10)
    );
    assert_eq!(retained().await.unwrap(), json!([]));
    let (ids, seen) = read(&handle, channel).await;
    assert_eq!((ids, seen), (vec![11], standing(&[11])));
    assert_eq!(requeue(&handle).await, 0, "nothing comes back");
}

/// An explicit clear discards queued and retained originals whether their notice was sent or not.
#[tokio::test(flavor = "current_thread")]
async fn a_clear_takes_untold_inputs_and_leaves_told_ones_retained() {
    let _root = TestRuntimeRootGuard::new();
    let channel = 767_635;
    let (_registry, handle) = adopt(channel).await;
    admit(&handle, channel, &[10, 11, 20, 30]).await;
    retain(&handle, &[10, 11], vec![20, 30], false).await;
    retain(&handle, &[20], vec![30], true).await;
    let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, "v2-requeue", None);
    let cleared = handle.clear(persistence).await;
    assert!(cleared.persistence.failed.is_empty());
    assert!(!cleared.persistence.count_unknown);
    let mut discarded: Vec<u64> = cleared
        .discarded_message_ids
        .iter()
        .map(|m| m.get())
        .collect();
    discarded.sort();
    assert_eq!(discarded, [10, 11, 20, 30]);
    let left = handle.v2_request(|s| {
        let owner = s.as_ref().unwrap();
        let retained = owner.snapshot.retained_inputs.iter();
        Ok((
            owner.snapshot.items.len(),
            retained
                .map(|r| r.input.original.message_id)
                .collect::<Vec<_>>(),
        ))
    });
    assert_eq!(left.await.unwrap(), (0, Vec::<u64>::new()));
    assert_eq!(requeue(&handle).await, 0);
}
