//! A Claude Pending execution whose attach found its own logged start hands its first prompt over
//! on the route the owner waits on, and the owner's claim and reserve write it once.
use super::*;
use crate::services::discord::queue_io::input_owner::drive::{self, FrontClaim};

/// The owner's hook on `route` for `provider`, reserving under `front`, for the turn of `key`.
fn on_route(
    owner: &HerdrOwner,
    (provider, route): (&str, target::Route),
    front: Option<FrontClaim>,
    key: &str,
) -> scope::LaunchGuard {
    let wanted = handed_over(route, provider).expect("a route whose driver hands over");
    let registration = owner.registration(provider, "herdr");
    let handover = (wanted, Some(key.to_owned()));
    let hook = created(
        registration,
        PROMPT.into(),
        handover,
        (front, reply_once(None)),
    );
    scope::install_launch(hook)
}

/// One wake's grant and turn claim on the owner's front.
fn claim(owner: &HerdrOwner) -> FrontClaim {
    owner.runtime.block_on(async {
        let granted = owner.mailbox.v2_request(|slot| {
            let v2 = slot.as_mut().ok_or("V2 not adopted")?;
            let front = v2.front().ok_or("no front")?;
            v2.grant_front(&front).map(|_| front)
        });
        let front = granted.await.unwrap();
        let claimed = drive::claim_turn(&owner.mailbox, front.1.message_id, &front).await;
        claimed.unwrap().expect("the granted front is claimed")
    })
}

/// The Claude Pending turn's hand-over on this thread: it read `record`, its attach returned
/// `attached`, and the hook names `target`.
fn hand_over(
    owner: &HerdrOwner,
    executions: (&HostedExecution, &HostedExecution, &HostedExecution),
) -> Result<(), String> {
    let _registry = owner.rig.registry_on_this_thread();
    let pool = unused_pool(owner);
    let turn = owner.claude_turn(&pool);
    let transcript = owner._dir.path().join("attached-sid.jsonl");
    crate::services::claude_tui::readiness::tests::pre_source(&owner.logical, &transcript);
    let _entered = owner.runtime.enter();
    let session = ("attached-sid", transcript);
    crate::services::claude::herdr_turn::first_input::owned_pending_prompt_for_tests(
        &turn, executions, session,
    )
}

fn started(channel: u64) -> HerdrOwner {
    let owner = HerdrOwner::start("claude", channel);
    owner.rig.answer("pane.read", claude_ready());
    owner
}

fn renonced(record: &HostedExecution, nonce: &str) -> HostedExecution {
    let mut record = record.clone();
    record.execution_nonce = nonce.into();
    record.source_ref.execution_nonce = nonce.into();
    record
}

#[test]
fn attached_pending_front_is_delivered_once_on_cold_route() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = started(7_283_991);
    let record = owner.record("claude", HostedState::Pending);
    let route = ("claude", target::Route::Cold);
    let _waiting = on_route(&owner, route, Some(claim(&owner)), &owner.logical);
    assert_eq!(hand_over(&owner, (&record, &record, &record)), Ok(()));
    owner.delivered_once(PROMPT);
}

#[test]
fn attached_pending_on_herdr_idle_route_delivers_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = started(7_283_992);
    let record = owner.record("claude", HostedState::Pending);
    let route = ("claude", target::Route::HerdrIdle);
    let _waiting = on_route(&owner, route, Some(claim(&owner)), &owner.logical);
    assert_eq!(hand_over(&owner, (&record, &record, &record)), Ok(()));
    owner.delivered_once(PROMPT);
}

/// An unverified Pending, a lineage naming another execution than the target, another owner's
/// turn and a Codex owner are each refused before any reservation.
#[test]
fn unverified_or_codex_pending_is_still_refused() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = started(7_283_993);
    let record = owner.record("claude", HostedState::Pending);
    let other = renonced(&record, &"e".repeat(32));
    let cold = ("claude", target::Route::Cold);
    let refused = |result: Result<(), String>| {
        let error = result.unwrap_err();
        assert!(error.starts_with("NotSent(\"lineage_refused: "), "{error}");
        owner.untouched();
    };
    let waiting = on_route(&owner, cold, None, &owner.logical);
    let pool = unused_pool(&owner);
    let unverified = {
        let _registry = owner.rig.registry_on_this_thread();
        let _entered = owner.runtime.enter();
        crate::services::claude::herdr_turn::owned_prompt_for_tests(
            &owner.claude_turn(&pool),
            &record,
            ("pending-sid", owner._dir.path().join("pending.jsonl")),
            Lineage::Pending,
        )
    };
    assert_eq!(
        unverified,
        Err("NotSent(\"lineage_refused: Pending\")".into())
    );
    owner.untouched();
    drop(waiting);
    let waiting = on_route(&owner, cold, None, &owner.logical);
    refused(hand_over(&owner, (&record, &other, &record)));
    drop(waiting);
    let waiting = on_route(&owner, cold, None, "AgentDesk-claude-another-owner");
    refused(hand_over(&owner, (&record, &record, &record)));
    drop(waiting);
    let _waiting = on_route(&owner, ("codex", target::Route::Cold), None, &owner.logical);
    refused(hand_over(&owner, (&record, &record, &record)));
}

/// A wake whose claim a later wake replaced reserves nothing, and leaves the current claim intact.
#[test]
fn stale_claim_cannot_reserve_attached_pending() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = started(7_283_994);
    let record = owner.record("claude", HostedState::Pending);
    let stale = claim(&owner);
    let current = claim(&owner);
    assert_ne!(stale.number(), current.number());
    let route = ("claude", target::Route::Cold);
    let _waiting = on_route(&owner, route, Some(stale), &owner.logical);
    let error = hand_over(&owner, (&record, &record, &record)).unwrap_err();
    assert!(error.starts_with("NotSent("), "{error}");
    owner.untouched();
    let front = current.front().clone();
    let claimed = owner
        .runtime
        .block_on(owner.mailbox.v2_request(move |slot| {
            let v2 = slot.as_ref().ok_or("V2 not adopted")?;
            Ok((v2.claimed(front.1.message_id, &front), v2.claim_number()))
        }));
    assert_eq!(claimed, Ok((true, current.number())));
}

/// Two wakes that each claimed the front hand the same attached Pending over at once: only the
/// current claim reserves, and the pane takes one paste and one Enter.
#[test]
fn concurrent_wakes_write_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = started(7_283_995);
    let record = owner.record("claude", HostedState::Pending);
    let claims = [claim(&owner), claim(&owner)];
    let barrier = std::sync::Barrier::new(2);
    let (done, results) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        for front in claims {
            let (owner, record, barrier, done) = (&owner, &record, &barrier, done.clone());
            scope.spawn(move || {
                let route = ("claude", target::Route::Cold);
                let _waiting = on_route(owner, route, Some(front), &owner.logical);
                barrier.wait();
                let _ = done.send(hand_over(owner, (record, record, record)));
            });
        }
    });
    let cap = crate::test_support::wait::WAIT_CAP;
    let results: Vec<Result<(), String>> = (0..2)
        .map(|_| crate::test_support::wait::recv_within(&results, cap))
        .collect();
    assert_eq!(
        results.iter().filter(|r| r.is_ok()).count(),
        1,
        "{results:?}"
    );
    owner.delivered_once(PROMPT);
}

/// A Pending turn whose attach returned another execution than the row it read (an unplaced
/// Pending retired for a fresh launch) hands over the execution it returned.
#[test]
fn relaunched_pending_hands_over_its_returned_nonce() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = started(7_283_996);
    let live = owner.record("claude", HostedState::Pending);
    let retired = renonced(&live, &"f".repeat(32));
    let route = ("claude", target::Route::Cold);
    let _waiting = on_route(&owner, route, Some(claim(&owner)), &owner.logical);
    assert_eq!(hand_over(&owner, (&retired, &live, &live)), Ok(()));
    owner.delivered_once(PROMPT);
}
