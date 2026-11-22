use super::*;
use crate::services::turn_orchestrator::TurnAdmissionOrder;
use serde_json::json;

fn item(id: u64, author: u64) -> PendingQueueItem {
    serde_json::from_value(
        json!({"author_id":author,"message_id":id,"source_message_ids":[id],
        "channel_id":7788001,"text":format!("body {id}"),"queued_generation":1,"merge_consecutive":true}),
    )
    .unwrap()
}
fn owner(dir: &tempfile::TempDir, items: &[(u64, u64)]) -> Owner {
    let mut owner = Owner::adopt(dir.path().join("q.json"), "readiness".into()).unwrap();
    for (id, author) in items {
        owner.enqueue_fixture(&item(*id, *author)).unwrap();
    }
    owner
}
fn queue_dir() -> tempfile::TempDir {
    let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    std::fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}
fn claim(owner: &mut Owner) -> (Front, u64) {
    let front = owner.front().unwrap();
    owner.grant_front(&front).unwrap();
    owner.claim_turn(front.1.message_id, &front).unwrap();
    (front, owner.claim_number())
}
/// The reserve a leaf makes before its first key; a refused front's claim can no longer make it.
fn reserve(owner: &mut Owner, (front, number): &(Front, u64)) -> Result<AttemptRef, String> {
    let attempt = serde_json::from_value(json!({"nonce":"n","provider":"claude",
        "target":{"host":"herdr","owner":"o","root":"/r","execution_nonce":"e"},
        "source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}}))
    .unwrap();
    let followers = owner.followers();
    let payload = PreparedPayload {
        frame: "f".into(),
        digest: "d".into(),
        attribution: json!({}),
        followers,
    };
    owner.reserve_claimed((front, *number), attempt, payload)
}

#[test]
fn deadline_first_refuses_the_unit_without_body_and_a_late_reserve_sends_nothing() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = queue_dir();
    let mut owner = owner(&dir, &[(1, 7), (2, 7)]);
    let claimed = claim(&mut owner);
    let hold = owner
        .hold_readiness(&claimed.0, "missing_launch", 1_000)
        .unwrap();
    assert!(
        !owner.refuse_readiness_front(&hold, 10_999).unwrap(),
        "before due"
    );
    assert!(owner.refuse_readiness_front(&hold, 11_000).unwrap());
    assert!(
        reserve(&mut owner, &claimed).is_err(),
        "no attempt, so no key"
    );
    assert!(owner.snapshot.items.is_empty() && owner.snapshot.head_injection.is_none());
    assert_eq!(owner.snapshot.refused_fronts[0].members.len(), 2);
    assert!(
        owner.blocks_claim(2, TurnAdmissionOrder::Immediate),
        "sources stay blocked"
    );
    let kept = owner.json().unwrap().to_string();
    assert!(
        !kept.contains("body 1") && !kept.contains("body 2"),
        "{kept}"
    );
}

#[test]
fn a_reserve_first_leaves_the_deadline_a_noop() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = queue_dir();
    let mut owner = owner(&dir, &[(3, 7)]);
    let claimed = claim(&mut owner);
    let hold = owner
        .hold_readiness(&claimed.0, "missing_launch", 1_000)
        .unwrap();
    reserve(&mut owner, &claimed).unwrap();
    assert!(!owner.refuse_readiness_front(&hold, 11_000).unwrap());
    owner.tick(11_000).unwrap();
    assert!(owner.snapshot.refused_fronts.is_empty());
    assert!(
        owner.snapshot.head_injection.is_some(),
        "the leaf settles its own attempt"
    );
}

#[test]
fn an_old_deadline_never_refuses_a_new_front_or_generation() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = queue_dir();
    let mut owner = owner(&dir, &[(4, 7), (5, 8)]);
    let hold = owner.hold_readiness(&owner.front().unwrap(), "missing_launch", 1_000);
    let hold = hold.unwrap();
    let mut regenerated = hold.clone();
    regenerated.identity.queued_generation += 1;
    assert!(!owner.refuse_readiness_front(&regenerated, 11_000).unwrap());
    let claimed = claim(&mut owner);
    let key = reserve(&mut owner, &claimed).unwrap();
    drop(owner.arm(&key, 1).unwrap());
    owner.settle(&key, true, 2).unwrap();
    assert!(!owner.refuse_readiness_front(&hold, 11_000).unwrap());
    owner.tick(11_000).unwrap();
    assert_eq!(owner.snapshot.items[0].message_id, 5);
    assert!(owner.snapshot.refused_fronts.is_empty() && owner.snapshot.readiness_holds.is_empty());
}

#[test]
fn the_first_due_survives_rehold_restart_and_a_clock_set_back_then_tells_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = queue_dir();
    let mut owner = owner(&dir, &[(6, 7)]);
    let front = owner.front().unwrap();
    let hold = owner
        .hold_readiness(&front, "missing_launch", 1_000)
        .unwrap();
    assert_eq!(
        owner.hold_readiness(&front, "composer_unavailable", 9_000),
        Ok(hold.clone())
    );
    drop(owner);
    let mut owner = Owner::adopt(dir.path().join("q.json"), "readiness".into()).unwrap();
    assert_eq!(
        (owner.front_hold(), owner.settle_due()),
        (Some(&hold), Some(11_000))
    );
    owner.tick(10_999).unwrap();
    assert_eq!(owner.snapshot.items.len(), 1);
    owner.tick(999).unwrap();
    assert!(
        owner.snapshot.items.is_empty(),
        "a clock set back never renews the due"
    );
    let identity = owner.snapshot.refused_fronts[0].input.identity.clone();
    owner.attemptless_notice_enqueued(identity.clone()).unwrap();
    owner.attemptless_notice_enqueued(identity).unwrap();
    assert!(owner.snapshot.refused_fronts[0].notice.enqueued);
}

#[test]
fn a_reserved_leaf_starts_its_first_key_only_before_the_original_due() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (start, started) in [(11_000, false), (10_999, true)] {
        let dir = queue_dir();
        let mut owner = owner(&dir, &[(7, 7)]);
        let claimed = claim(&mut owner);
        owner
            .hold_readiness(&claimed.0, "missing_launch", 1_000)
            .unwrap();
        let key = reserve(&mut owner, &claimed).unwrap();
        drop(owner.arm_bootstrap(&key, 2_000).unwrap());
        assert_eq!(
            owner.payload_started(&key, start).is_ok(),
            started,
            "at {start}"
        );
        owner.tick(11_000).unwrap();
        assert!(
            owner.snapshot.refused_fronts.is_empty(),
            "the leaf owns its attempt"
        );
    }
}

#[test]
fn identity_hold_survives_adoption_and_allows_a_new_readiness_deadline() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = queue_dir();
    let mut owner = owner(&dir, &[(5, 7)]);
    let front = owner.front().unwrap();
    let now = super::super::origin::now_ms();
    let old = owner
        .hold_readiness(&front, "busy_readiness_unproven", now)
        .unwrap();
    let mut stale = front.clone();
    stale.1.queued_generation += 1;
    assert!(owner.hold_identity(&stale, "identity", now + 5).is_err());
    assert_eq!(owner.settle_due(), Some(old.due_at_ms));

    let identity = owner.hold_identity(&front, "identity", now + 5).unwrap();
    assert_eq!(identity.identity_held_at_ms, Some(now + 5));
    assert_eq!(identity.due_at_ms, now + 5 + 10_000);
    let again = owner.hold_identity(&front, "identity", now + 9).unwrap();
    assert_eq!(again, identity, "a later identity hold keeps its due");
    let repeated = owner
        .hold_readiness(&front, "composer_unavailable", now + 9)
        .unwrap();
    assert_eq!(repeated, identity);
    assert!(!owner.refuse_readiness_front(&old, old.due_at_ms).unwrap());
    drop(owner);

    let mut owner = Owner::adopt(dir.path().join("q.json"), "readiness".into()).unwrap();
    assert_eq!(owner.settle_due(), Some(identity.due_at_ms));
    owner.tick(old.due_at_ms).unwrap();
    assert!(is_front(&owner.front().unwrap(), &front));
    assert!(owner.snapshot.refused_fronts.is_empty());

    owner.finish_identity_hold(&front).unwrap();
    let next = owner
        .hold_readiness(&front, "composer_unavailable", old.due_at_ms)
        .unwrap();
    assert!(next.identity_held_at_ms.is_none());
    assert_eq!(owner.settle_due(), Some(old.due_at_ms + 10_000));
    assert_eq!(next.cause, "composer_unavailable");
}

#[test]
fn an_identity_hold_no_ready_read_ends_is_refused_unsent_at_its_due() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = queue_dir();
    let mut owner = owner(&dir, &[(6, 7)]);
    let front = owner.front().unwrap();
    let hold = owner.hold_identity(&front, "identity", 1_000).unwrap();
    assert_eq!(owner.settle_due(), Some(11_000));
    owner.tick(10_999).unwrap();
    assert!(is_front(&owner.front().unwrap(), &front));
    owner.tick(11_000).unwrap();
    assert!(owner.front().is_none());
    let refused = &owner.snapshot.refused_fronts;
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].input.identity, hold.identity);
    assert_eq!(refused[0].reason, "readiness_deadline: identity");
    assert!(owner.snapshot.readiness_holds.is_empty());
}

#[test]
fn a_legacy_readiness_hold_without_members_can_only_refuse_its_front() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let dir = queue_dir();
    let mut owner = owner(&dir, &[(8, 7), (9, 7)]);
    owner
        .hold_readiness(&owner.front().unwrap(), "idle_evidence_pending", 1_000)
        .unwrap();
    let mut old = owner.json().unwrap();
    old["readiness_holds"][0]
        .as_object_mut()
        .unwrap()
        .remove("members")
        .unwrap();
    owner.commit(serde_json::from_value(old).unwrap()).unwrap();
    drop(owner);

    let mut recovered = Owner::adopt(dir.path().join("q.json"), "readiness".into()).unwrap();
    assert_eq!(recovered.settle_due(), Some(11_000));
    assert!(recovered.followers().is_empty());
    recovered.tick(11_000).unwrap();
    assert_eq!(recovered.front().unwrap().1.message_id, 9);
    assert_eq!(recovered.snapshot.refused_fronts[0].members.len(), 1);
}
