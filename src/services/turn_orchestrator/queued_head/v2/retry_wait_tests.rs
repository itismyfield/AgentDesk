use super::retry_wait::{RetryCause, RetryWait};
use super::source_admission::Custody;
use super::*;
use crate::config::TestRuntimeRootGuard;
use serde_json::json;

fn item(id: u64) -> PendingQueueItem {
    serde_json::from_value(
        json!({"author_id":7,"message_id":id,"source_message_ids":[id],
        "channel_id":7788002,"text":format!("original {id}"),"queued_generation":1}),
    )
    .unwrap()
}

fn queue_dir() -> tempfile::TempDir {
    let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    std::fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}

fn adopt(dir: &tempfile::TempDir) -> Owner {
    Owner::adopt(dir.path().join("q.json"), "retry-wait".into()).unwrap()
}

/// Admits `id` under its own ticket, as a person's input arrives.
fn admit(owner: &mut Owner, id: u64) {
    let ticket = Ticket::new(&format!("retry-{id}")).unwrap();
    Custody::with_ticket(&item(id), ticket)
        .advance(owner)
        .unwrap();
}

fn claim(owner: &mut Owner) -> (Front, u64) {
    let front = owner.front().unwrap();
    assert!(owner.grant_front(&front).unwrap());
    owner.claim_turn(front.1.message_id, &front).unwrap();
    (front, owner.claim_number())
}

/// The current claim's refusal as `cause` at `now`; returns the wait number it was given.
fn refuse(owner: &mut Owner, cause: RetryCause, now: u64) -> u64 {
    let (front, number) = claim(owner);
    let entered = owner.enter_retry_wait_claimed((&front, number), cause, now);
    entered.unwrap().expect("the current claim enters the wait")
}

fn unstarted(nonce: &str) -> RetryCause {
    RetryCause::PendingUnstarted {
        nonce: nonce.into(),
    }
}

fn wait(owner: &Owner) -> RetryWait {
    owner.snapshot.retry.wait.clone().expect("a retry wait")
}

/// The waiting original keeps its slot, body and ticket, and nothing was retained or refused.
pub(crate) fn assert_original_and_ticket_kept(owner: &Owner, front: &Front) {
    let now = owner.front().expect("the original still leads");
    assert!(now.0.is_some() && now.0 == front.0, "ticket {:?}", now.0);
    assert!(same(&now.1, &front.1), "original changed");
    assert!(owner.snapshot.retained_inputs.is_empty());
    assert!(owner.snapshot.refused_fronts.is_empty());
}

#[test]
fn attempts_count_consumed_dues_first_due_enables_b_close() {
    let _root = TestRuntimeRootGuard::new();
    let dir = queue_dir();
    let mut owner = adopt(&dir);
    admit(&mut owner, 1);
    let front = owner.front().unwrap();
    refuse(&mut owner, unstarted("n1"), 1_000);
    owner.tick(30_999).unwrap();
    assert!(
        !owner.never_started_close_qualified(&front, "n1"),
        "before the first due"
    );
    assert!(
        !owner.grant_front(&front).unwrap(),
        "no handoff before the due"
    );
    owner.tick(31_000).unwrap();
    assert_eq!((wait(&owner).attempts, wait(&owner).armed), (1, false));
    assert!(owner.never_started_close_qualified(&front, "n1"));
    assert!(!owner.never_started_close_qualified(&front, "n2"));
    assert_original_and_ticket_kept(&owner, &front);
    assert!(
        owner.grant_front(&front).unwrap(),
        "the consumed due grants again"
    );
}

#[test]
fn own_launch_timeout_then_pending_unstarted_keeps_attempts() {
    let _root = TestRuntimeRootGuard::new();
    let dir = queue_dir();
    let mut owner = adopt(&dir);
    admit(&mut owner, 2);
    let front = owner.front().unwrap();
    let timeout = RetryCause::OwnLaunchTimeout { nonce: "n".into() };
    refuse(&mut owner, timeout, 0);
    owner.tick(30_000).unwrap();
    refuse(&mut owner, unstarted("n"), 40_000);
    let kept = wait(&owner);
    assert_eq!(
        (kept.attempts, kept.due_at_ms, kept.armed),
        (1, 100_000, true)
    );
    assert!(owner.never_started_close_qualified(&front, "n"));
}

#[test]
fn new_nonce_after_retire_starts_attempts_at_zero() {
    let _root = TestRuntimeRootGuard::new();
    let dir = queue_dir();
    let mut owner = adopt(&dir);
    admit(&mut owner, 3);
    let front = owner.front().unwrap();
    refuse(&mut owner, unstarted("old"), 0);
    owner.tick(30_000).unwrap();
    refuse(&mut owner, unstarted("new"), 50_000);
    assert_eq!((wait(&owner).attempts, wait(&owner).due_at_ms), (0, 80_000));
    assert!(!owner.never_started_close_qualified(&front, "new"));
}

#[test]
fn stale_callback_double_fire_does_not_bump_attempts() {
    let _root = TestRuntimeRootGuard::new();
    let dir = queue_dir();
    let mut owner = adopt(&dir);
    admit(&mut owner, 4);
    let first = refuse(&mut owner, unstarted("n"), 0);
    for at in [30_000, 30_000, 900_000] {
        owner.tick(at).unwrap();
    }
    assert_eq!(wait(&owner).attempts, 1, "one due, consumed once");
    assert!(
        !owner.release_retry_wait(4, first).unwrap(),
        "already disarmed"
    );
    let second = refuse(&mut owner, unstarted("n"), 1_000_000);
    assert!(second > first);
    assert!(!owner.release_retry_wait(4, first).unwrap());
    assert_eq!((wait(&owner).attempts, wait(&owner).armed), (1, true));
}

#[test]
fn repeated_waits_on_same_front_issue_new_wait_seq_and_old_callbacks_noop() {
    let _root = TestRuntimeRootGuard::new();
    let dir = queue_dir();
    let mut owner = adopt(&dir);
    admit(&mut owner, 5);
    let mut issued: Vec<u64> = Vec::new();
    for round in 0..3u64 {
        let now = round * 40_000;
        let seq = refuse(&mut owner, unstarted("n"), now);
        assert!(issued.iter().all(|old| *old < seq), "{issued:?} then {seq}");
        for old in &issued {
            assert!(
                !owner.release_retry_wait(5, *old).unwrap(),
                "old watch {old}"
            );
        }
        // The previous wait's timer fires before this wait's due: nothing is consumed.
        owner.tick(now.saturating_sub(10_000)).unwrap();
        assert_eq!((wait(&owner).attempts, wait(&owner).armed), (0, true));
        assert!(
            owner.release_retry_wait(5, seq).unwrap(),
            "the current watch"
        );
        assert_eq!((wait(&owner).attempts, wait(&owner).armed), (0, false));
        issued.push(seq);
    }
}

#[test]
fn old_timer_or_watch_after_clear_new_front_new_wait_is_inert() {
    let _root = TestRuntimeRootGuard::new();
    let dir = queue_dir();
    let mut owner = adopt(&dir);
    admit(&mut owner, 6);
    let old = refuse(&mut owner, unstarted("n"), 0);
    owner.clear_waiting().unwrap();
    assert!(
        owner.snapshot.retry.wait.is_none(),
        "the wait left with its front"
    );
    admit(&mut owner, 7);
    let front = owner.front().unwrap();
    let new = refuse(&mut owner, unstarted("n"), 20_000);
    assert!(new > old);
    assert!(!owner.release_retry_wait(6, old).unwrap());
    assert!(!owner.release_retry_wait(7, old).unwrap());
    owner.tick(30_000).unwrap();
    let current = wait(&owner);
    assert_eq!(
        (current.wait_seq, current.attempts, current.armed),
        (new, 0, true)
    );
    assert!(!owner.grant_front(&front).unwrap());
}

#[test]
fn unchanged_cause_watch_keeps_backoff_and_attempts_grow() {
    let _root = TestRuntimeRootGuard::new();
    let dir = queue_dir();
    let mut owner = adopt(&dir);
    admit(&mut owner, 8);
    let mut now = 0;
    for (attempts, backoff) in [
        (0, 30_000),
        (1, 60_000),
        (2, 120_000),
        (3, 240_000),
        (4, 300_000),
    ] {
        refuse(&mut owner, unstarted("n"), now);
        let held = wait(&owner);
        assert_eq!((held.attempts, held.due_at_ms - now), (attempts, backoff));
        assert_eq!(owner.settle_due(), Some(held.due_at_ms));
        now = held.due_at_ms;
        owner.tick(now).unwrap();
    }
}

#[test]
fn retry_wait_due_fires_after_real_owner_drop_and_adopt_with_claim_number_reset() {
    let _root = TestRuntimeRootGuard::new();
    let dir = queue_dir();
    let mut owner = adopt(&dir);
    admit(&mut owner, 9);
    let front = owner.front().unwrap();
    let seq = refuse(&mut owner, unstarted("n"), 0);
    drop(owner);
    let mut owner = adopt(&dir);
    assert_eq!(owner.claim_number(), 0);
    assert_eq!((wait(&owner).wait_seq, wait(&owner).armed), (seq, true));
    assert!(
        !owner.grant_front(&front).unwrap(),
        "the restored wait still holds"
    );
    owner.tick(30_000).unwrap();
    assert_original_and_ticket_kept(&owner, &front);
    let (claimed, number) = claim(&mut owner);
    assert!(is_front(&claimed, &front) && number > 0);
}

#[test]
fn restored_wait_keeps_attempts_and_consumes_overdue_once() {
    let _root = TestRuntimeRootGuard::new();
    let dir = queue_dir();
    let mut owner = adopt(&dir);
    admit(&mut owner, 10);
    refuse(&mut owner, unstarted("n"), 0);
    owner.tick(30_000).unwrap();
    refuse(&mut owner, unstarted("n"), 40_000);
    drop(owner);
    let mut owner = adopt(&dir);
    assert_eq!(wait(&owner).attempts, 1);
    owner.tick(5_000_000).unwrap();
    owner.tick(5_000_001).unwrap();
    assert_eq!((wait(&owner).attempts, wait(&owner).armed), (2, false));
}

#[test]
fn preclaim_root_recovery_held_waits_without_claim_then_retries_at_due() {
    let _root = TestRuntimeRootGuard::new();
    let dir = queue_dir();
    let mut owner = adopt(&dir);
    admit(&mut owner, 11);
    let front = owner.front().unwrap();
    assert!(owner.grant_front(&front).unwrap());
    let grant = owner.grant_number_of(&front).unwrap();
    let held = RetryCause::RootRecoveryHeld;
    let stale = owner.enter_retry_wait_granted((&front, grant + 1), held.clone(), 0);
    assert_eq!(stale.unwrap(), None, "another grant enters nothing");
    let entered = owner.enter_retry_wait_granted((&front, grant), held, 0);
    assert!(entered.unwrap().is_some());
    assert!(
        !owner.front_outstanding(),
        "the grant was spent, no claim taken"
    );
    assert!(owner.claim_turn(11, &front).is_none());
    assert!(!owner.grant_front(&front).unwrap());
    owner.tick(30_000).unwrap();
    assert_original_and_ticket_kept(&owner, &front);
    claim(&mut owner);
}

#[test]
fn root_recovery_held_wait_ignores_bound_row_watch() {
    let _root = TestRuntimeRootGuard::new();
    let dir = queue_dir();
    let mut owner = adopt(&dir);
    admit(&mut owner, 12);
    let front = owner.front().unwrap();
    assert!(owner.grant_front(&front).unwrap());
    let grant = owner.grant_number_of(&front).unwrap();
    let held = owner.enter_retry_wait_granted((&front, grant), RetryCause::RootRecoveryHeld, 0);
    let seq = held.unwrap().unwrap();
    assert!(
        !owner.release_retry_wait(12, seq).unwrap(),
        "a row read never ends it"
    );
    assert!(
        owner.release_root_recovery_wait().unwrap(),
        "the reader's Retired kick does"
    );
    assert!(owner.grant_front(&front).unwrap());
    owner.claim_turn(12, &front).unwrap();
    refuse_claimed_pending(&mut owner, &front);
    assert!(
        !owner.release_root_recovery_wait().unwrap(),
        "a Pending wait is not its"
    );
}

fn refuse_claimed_pending(owner: &mut Owner, front: &Front) {
    let number = owner.claim_number();
    let entered = owner.enter_retry_wait_claimed((front, number), unstarted("n"), 0);
    assert!(entered.unwrap().is_some());
}

#[test]
fn own_launch_timeout_keeps_opaque_count_original_ticket_and_no_handoff_before_due() {
    let _root = TestRuntimeRootGuard::new();
    let dir = queue_dir();
    let mut owner = adopt(&dir);
    admit(&mut owner, 13);
    let front = owner.front().unwrap();
    let typed = RetryCause::OwnLaunchTimeout { nonce: "n".into() }.refusal("no SessionStart");
    assert!(typed.starts_with("NotSent("), "{typed}");
    let cause = RetryCause::from_refusal(&typed).expect("typed own timeout");
    assert_eq!(
        RetryCause::from_refusal("NotSent(\"front_not_granted\")"),
        None
    );
    let (claimed, number) = claim(&mut owner);
    let stale = owner.enter_retry_wait_claimed((&claimed, number + 1), cause.clone(), 0);
    assert_eq!(stale.unwrap(), None, "an earlier claim enters nothing");
    owner
        .enter_retry_wait_claimed((&claimed, number), cause, 0)
        .unwrap()
        .unwrap();
    assert!(
        owner.take_held(13),
        "the drive watches it rather than retrying"
    );
    assert!(!owner.front_outstanding());
    assert!(!owner.grant_front(&front).unwrap());
    assert_original_and_ticket_kept(&owner, &front);
}

#[test]
fn stale_claim_cannot_request_auto_terminate() {
    let _root = TestRuntimeRootGuard::new();
    let dir = queue_dir();
    let mut owner = adopt(&dir);
    admit(&mut owner, 21);
    refuse(&mut owner, unstarted("n1"), 0);
    owner.tick(30_000).unwrap();
    let (front, stale) = claim(&mut owner);
    let named = Some("n1".to_string());
    assert_eq!(owner.never_started_close_claimed((&front, stale)), named);
    owner.revoke_front();
    assert_eq!(owner.never_started_close_claimed((&front, stale)), None);
    let (front, current) = claim(&mut owner);
    assert_eq!(owner.never_started_close_claimed((&front, stale)), None);
    assert_eq!(owner.never_started_close_claimed((&front, current)), named);
    assert!(!owner.end_never_started_wait(&front, "n2").unwrap());
    assert!(owner.end_never_started_wait(&front, "n1").unwrap());
    assert_eq!(owner.never_started_close_claimed((&front, current)), None);
    assert!(owner.claimed(front.1.message_id, &front), "the claim stays");
    assert_original_and_ticket_kept(&owner, &front);
}
