//! One reserved unit carries several queued originals, each under its own ticket.
use super::source_admission::{Custody, TurnBind};
use super::*;
use crate::config::TestRuntimeRootGuard;
use crate::services::provider::ProviderKind;
use poise::serenity_prelude::ChannelId;
use serde_json::json;

const BASE: u64 = 1_490_000_000_000_000_000;

fn item(n: u64, text: &str) -> PendingQueueItem {
    by(n, 70 + n, text)
}

fn by(n: u64, author: u64, text: &str) -> PendingQueueItem {
    serde_json::from_value(
        json!({"author_id":author,"message_id":BASE + n,"channel_id":7272,
        "source_message_ids":[BASE + n],"queued_generation":5,"text":text}),
    )
    .unwrap()
}

fn ticket(n: u64) -> Ticket {
    Ticket::new(&format!("unit-{n}")).unwrap()
}

fn attempt(nonce: &str) -> AttemptIdentity {
    AttemptIdentity {
        nonce: nonce.into(),
        provider: "claude".into(),
        target: TargetIdentity {
            host: "tmux".into(),
            owner: format!("unit-{nonce}"),
            root: "/test".into(),
            execution_nonce: "execution".into(),
        },
        source: AttemptSource::PreSource {
            expected_session_id: None,
            expected_transcript: None,
            binding_watermark: None,
        },
    }
}

fn path(channel: u64) -> PathBuf {
    let provider = ProviderKind::Claude;
    super::super::pending_queue_file_path(&provider, "v2-unit", ChannelId::new(channel)).unwrap()
}

fn owner(channel: u64) -> Owner {
    Owner::adopt(path(channel), "unit".into()).unwrap()
}

fn admit(owner: &mut Owner, n: u64, text: &str) -> PendingQueueItem {
    admit_item(owner, n, item(n, text))
}

fn admit_item(owner: &mut Owner, n: u64, original: PendingQueueItem) -> PendingQueueItem {
    Custody::with_ticket(&original, ticket(n))
        .advance(owner)
        .unwrap();
    original
}

fn named(item: &PendingQueueItem) -> Option<(String, String)> {
    Some((format!("user{}", item.author_id), item.text.clone()))
}

fn turn(owner: &Owner, n: u64, original: &PendingQueueItem) -> TurnBind {
    owner.exact_turn(&ticket(n), original).unwrap()
}

#[test]
fn a_unit_freezes_the_prepared_front_in_order_and_binds_every_ticket_at_once() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner(1);
    let q1 = admit_item(&mut owner, 1, by(1, 71, "first queued"));
    let q2 = admit_item(&mut owner, 2, by(2, 71, "second queued"));
    let new = admit_item(&mut owner, 3, by(3, 73, "new arrival"));
    let prepared: Vec<u64> = owner.snapshot.items.iter().map(|i| i.message_id).collect();
    let late = admit(&mut owner, 4, "arrived after the unit was prepared");
    let key = owner
        .reserve_unit(attempt("unit-a"), |item| {
            prepared
                .contains(&item.message_id)
                .then(|| named(item))
                .flatten()
        })
        .unwrap();
    let head = owner.snapshot.head_injection.clone().unwrap();
    assert_eq!(
        head.members().map(|m| m.message_id).collect::<Vec<_>>(),
        [q1.message_id, q2.message_id, new.message_id]
    );
    let frame = &head.payload.as_ref().unwrap().frame;
    assert_eq!(
        frame, "first queued\n\nsecond queued\n\n[User: user73 (ID: 73)]\nnew arrival",
        "no header for the first author, one author line where it changes, none for the same author"
    );
    assert!(
        !frame.contains("arrived after"),
        "an original the caller did not prepare waits"
    );
    drop(owner.arm(&key, 1).unwrap());
    for (n, original) in [(1, &q1), (2, &q2), (3, &new)] {
        assert!(
            matches!(turn(&owner, n, original), TurnBind::Armed { attempt, .. } if attempt == "unit-a"),
            "ticket {n}"
        );
    }
    assert_eq!(turn(&owner, 4, &late), TurnBind::NotArmed);
}

#[test]
fn a_local_submit_settles_every_member_and_leaves_the_late_arrival_queued() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner(2);
    let members = [
        admit(&mut owner, 1, "a"),
        admit(&mut owner, 2, "b"),
        admit(&mut owner, 3, "c"),
    ];
    let key = owner.reserve_unit(attempt("unit-b"), named).unwrap();
    let late = admit(&mut owner, 4, "late");
    drop(owner.arm(&key, 1).unwrap());
    owner.settle(&key, true, 2).unwrap();
    assert!(owner.snapshot.head_injection.is_none());
    assert_eq!(
        owner
            .snapshot
            .items
            .iter()
            .map(|i| i.message_id)
            .collect::<Vec<_>>(),
        [late.message_id]
    );
    let terminals: Vec<u64> = owner
        .snapshot
        .terminal_identities
        .iter()
        .map(|t| t.identity.primary)
        .collect();
    assert_eq!(
        terminals,
        members.iter().map(|m| m.message_id).collect::<Vec<_>>()
    );
    for (n, original) in (1..).zip(&members) {
        assert!(turn(&owner, n, original).settled(), "ticket {n}");
    }
    assert_eq!(turn(&owner, 4, &late), TurnBind::NotArmed);
    for (n, original) in (1..).zip(&members) {
        assert_eq!(
            Custody::with_ticket(original, ticket(n)).advance(&mut owner),
            Ok(source_admission::Admitted::Duplicate)
        );
    }
    assert_eq!(owner.snapshot.items.len(), 1, "no member is admitted again");
}

#[test]
fn a_reserved_unit_restores_whole_and_a_changed_member_refuses_the_file() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner(3);
    let members = [
        admit(&mut owner, 1, "a"),
        admit(&mut owner, 2, "b"),
        admit(&mut owner, 3, "c"),
    ];
    owner.reserve_unit(attempt("unit-c"), named).unwrap();
    let raw = std::fs::read_to_string(path(3)).unwrap();
    drop(owner);
    let restored = self::owner(3);
    assert!(
        restored.snapshot.head_injection.is_none(),
        "an unarmed reserve is released"
    );
    assert_eq!(restored.snapshot.items.len(), 3);
    for (n, original) in (1..).zip(&members) {
        assert_eq!(
            turn(&restored, n, original),
            TurnBind::NotArmed,
            "ticket {n} returns to its slot"
        );
    }
    let mut changed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    changed["items"][2]["text"] = json!("not what the unit froze");
    assert_eq!(
        sources::restore(&changed.to_string(), "unit")
            .err()
            .as_deref(),
        Some("a reserved member differs from its queue copy"),
        "a body conflict is held, never guessed"
    );
}

/// Rewrites the stored snapshot of `channel` and reports what boot adoption plans for it.
fn rewrite_and_plan(channel: u64, edit: impl FnOnce(&mut serde_json::Value)) -> Vec<String> {
    let mut stored: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path(channel)).unwrap()).unwrap();
    edit(&mut stored);
    std::fs::write(path(channel), stored.to_string()).unwrap();
    let persistence =
        super::super::super::QueuePersistenceContext::new(&ProviderKind::Claude, "v2-unit", None);
    let steps = projection::plan(&persistence, "unit").unwrap();
    let ours = steps
        .into_iter()
        .filter(|step| format!("{step:?}").contains(&format!("({channel})")));
    ours.map(|step| format!("{step:?}")).collect()
}

/// A reserved unit that only differs from the queue in recomputed fields goes back to its single
/// originals: boot adopts the channel, and each original and ticket is there once, unattempted.
#[test]
fn an_unmatched_reserved_unit_returns_to_its_single_originals() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner(8);
    let members = [
        admit(&mut owner, 1, "a"),
        admit(&mut owner, 2, "b"),
        admit(&mut owner, 3, "c"),
    ];
    let tail = admit(&mut owner, 4, "tail");
    let unit =
        |item: &PendingQueueItem| (item.message_id != BASE + 4).then(|| named(item)).flatten();
    owner.reserve_unit(attempt("unit-g"), unit).unwrap();
    drop(owner);
    let steps = rewrite_and_plan(8, |stored| {
        stored["items"][2]["created_at_wall_time_ms"] = json!(123)
    });
    assert!(
        steps.iter().any(|s| s.starts_with("Adopt"))
            && !steps.iter().any(|s| s.starts_with("Hold")),
        "{steps:?}"
    );
    let restored = self::owner(8);
    assert!(restored.snapshot.head_injection.is_none());
    let ids: Vec<u64> = restored
        .snapshot
        .items
        .iter()
        .map(|i| i.message_id)
        .collect();
    assert_eq!(ids, [BASE + 1, BASE + 2, BASE + 3, BASE + 4]);
    assert_eq!(restored.snapshot.admission_receipts.len(), 4);
    for (n, original) in (1..).zip(members.iter().chain([&tail])) {
        assert_eq!(
            turn(&restored, n, original),
            TurnBind::NotArmed,
            "ticket {n}"
        );
    }
}

/// Members the queue lost come back from the reserved unit, so no original is dropped.
#[test]
fn a_reserved_unit_missing_from_the_queue_comes_back_whole() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner(9);
    let members = [
        admit(&mut owner, 1, "a"),
        admit(&mut owner, 2, "b"),
        admit(&mut owner, 3, "c"),
    ];
    owner.reserve_unit(attempt("unit-h"), named).unwrap();
    drop(owner);
    let steps = rewrite_and_plan(9, |stored| {
        stored["items"].as_array_mut().unwrap().truncate(1)
    });
    assert!(!steps.iter().any(|s| s.starts_with("Hold")), "{steps:?}");
    let mut restored = self::owner(9);
    let ids: Vec<u64> = restored
        .snapshot
        .items
        .iter()
        .map(|i| i.message_id)
        .collect();
    assert_eq!(ids, [BASE + 1, BASE + 2, BASE + 3]);
    for (n, original) in (1..).zip(&members) {
        assert_eq!(
            turn(&restored, n, original),
            TurnBind::NotArmed,
            "ticket {n}"
        );
        assert_eq!(
            Custody::with_ticket(original, ticket(n)).advance(&mut restored),
            Ok(source_admission::Admitted::Duplicate),
            "ticket {n} is not admitted again"
        );
    }
    assert_eq!(restored.snapshot.items.len(), 3);
}

/// An armed unit that no longer matches the queue may have been sent: it stays held, never rerun.
#[test]
fn an_armed_unit_that_does_not_match_stays_held() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner(10);
    admit(&mut owner, 1, "a");
    admit(&mut owner, 2, "b");
    let key = owner.reserve_unit(attempt("unit-i"), named).unwrap();
    drop(owner.arm(&key, 1).unwrap());
    drop(owner);
    let steps = rewrite_and_plan(10, |stored| {
        stored["items"][1]["created_at_wall_time_ms"] = json!(123)
    });
    assert!(steps.iter().any(|s| s.starts_with("Hold")), "{steps:?}");
    assert!(Owner::adopt(path(10), "unit".into()).is_err());
}

#[test]
fn an_arrival_before_the_permit_is_handed_over_joins_the_reprepared_unit() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner(7);
    let queued = [admit(&mut owner, 1, "q1"), admit(&mut owner, 2, "q2")];
    let old = owner.reserve_unit(attempt("prepared"), named).unwrap();
    let new = admit(&mut owner, 3, "arrived while registering");
    let key = owner.reprepare(&old, "reprepared".into(), named).unwrap();
    let head = owner.snapshot.head_injection.clone().unwrap();
    assert_eq!(head.members().count(), 3);
    let frame = &head.payload.as_ref().unwrap().frame;
    assert!(
        frame.ends_with("arrived while registering"),
        "the frame names every member: {frame:?}"
    );
    assert!(owner.arm(&old, 1).is_err(), "the old key never arms");
    assert!(owner.settle(&old, true, 1).is_err() && owner.payload_started(&old, 1).is_err());
    drop(owner.arm(&key, 1).unwrap());
    assert!(
        owner.reprepare(&key, "late".into(), named).is_err(),
        "a handed-over unit is fixed"
    );
    let late = admit(&mut owner, 4, "after the handover");
    owner.settle(&key, true, 2).unwrap();
    for (n, original) in (1..).zip(queued.iter().chain([&new])) {
        assert!(
            matches!(turn(&owner, n, original), TurnBind::Settled { attempt, .. } if attempt == "reprepared")
        );
    }
    assert_eq!(
        owner
            .snapshot
            .items
            .iter()
            .map(|i| i.message_id)
            .collect::<Vec<_>>(),
        [late.message_id]
    );
}

#[test]
fn one_member_unit_serializes_as_a_single_reserve() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner(4);
    admit(&mut owner, 1, "only");
    owner.reserve_unit(attempt("unit-d"), named).unwrap();
    let head = owner.json().unwrap()["head_injection"].clone();
    assert!(head["payload"].get("followers").is_none());
    assert_eq!(
        head["payload"]["attribution"],
        json!({"members":[BASE + 1]})
    );
}

#[test]
fn no_content_successor_keeps_the_members_and_tickets_and_retention_moves_the_whole_unit() {
    let _root = TestRuntimeRootGuard::new();
    let mut owner = owner(5);
    let members = [admit(&mut owner, 1, "a"), admit(&mut owner, 2, "b")];
    let key = owner.reserve_unit(attempt("unit-e"), named).unwrap();
    drop(owner.arm(&key, 1).unwrap());
    owner.tick(40_000).unwrap();
    assert!(
        owner.snapshot.items.is_empty(),
        "an unconfirmed unit leaves the queue together"
    );
    let retained = &owner.snapshot.retained_inputs[0].input;
    assert_eq!(retained.members().count(), 2);
    for (n, original) in (1..).zip(&members) {
        assert!(turn(&owner, n, original).armed(), "ticket {n}");
        assert!(
            owner.fenced(original),
            "a retained member is never admitted again"
        );
    }
    let tail = admit(&mut owner, 3, "tail");
    let next = owner.reserve_unit(attempt("unit-e2"), named).unwrap();
    assert_eq!(
        next.identity.primary, tail.message_id,
        "a retained unit never runs again"
    );
    for (n, original) in (1..).zip(&members) {
        assert!(
            turn(&owner, n, original).armed(),
            "ticket {n} keeps its bind beside a new reserve"
        );
    }
    drop(owner);
    let restored = self::owner(5);
    assert_eq!(
        restored.snapshot.retained_inputs[0].input.members().count(),
        2
    );
    assert_eq!(
        restored
            .snapshot
            .items
            .iter()
            .map(|i| i.message_id)
            .collect::<Vec<_>>(),
        [tail.message_id]
    );
}

#[tokio::test]
async fn cancel_and_clear_leave_every_reserved_member_in_custody() {
    use super::super::super::{ChannelMailboxRegistry, QueuePersistenceContext};
    let _root = TestRuntimeRootGuard::new();
    let ctx = || QueuePersistenceContext::new(&ProviderKind::Claude, "v2-unit", None);
    let registry = ChannelMailboxRegistry::default();
    let handle = registry.handle(ChannelId::new(727_206));
    handle.v2_adopt(path(6), "unit".into()).await.unwrap();
    handle
        .v2_request(|slot| {
            let owner = slot.as_mut().unwrap();
            for n in 1..=3 {
                admit(owner, n, "queued");
            }
            let unit = |item: &PendingQueueItem| {
                (item.message_id != BASE + 3).then(|| named(item)).flatten()
            };
            owner.reserve_unit(attempt("unit-f"), unit).map(drop)
        })
        .await
        .unwrap();
    let follower = handle
        .cancel_queued_primary_message(poise::serenity_prelude::MessageId::new(BASE + 2), ctx())
        .await;
    assert!(
        follower.removed.is_none(),
        "a reserved member is never cancelled"
    );
    let cleared = handle.clear(ctx()).await;
    let discarded: Vec<u64> = cleared
        .discarded_message_ids
        .iter()
        .map(|id| id.get())
        .collect();
    assert_eq!(
        discarded,
        [BASE + 3],
        "clear discards only what no unit holds"
    );
    let left = handle
        .v2_request(|slot| {
            Ok(slot
                .as_ref()
                .unwrap()
                .snapshot
                .items
                .iter()
                .map(|i| i.message_id)
                .collect::<Vec<_>>())
        })
        .await
        .unwrap();
    assert_eq!(left, [BASE + 1, BASE + 2]);
}
