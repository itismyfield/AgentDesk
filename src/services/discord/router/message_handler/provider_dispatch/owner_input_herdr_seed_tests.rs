//! A restart greeting that a Herdr user stop closes after its reserve and before any key is
//! cancelled by the owner; the person's input behind it keeps its original and ticket.
use super::*;
use crate::services::discord::queue_io::input_owner::drive::{FrontClaim, claim_turn};
use crate::services::provider::herdr_before_start::stop_at_input_boundary;
use crate::services::turn_orchestrator::{
    RESTART_SEED_SOURCE, SeedStatus, assert_original_and_ticket_kept,
};

/// Queues a restart greeting ahead of the owner's person input; returns that person's front and
/// the greeting.
fn admit_seed(owner: &HerdrOwner) -> (Front, PendingQueueItem) {
    let front = (owner.mailbox).v2_request(|s| Ok(s.as_ref().unwrap().front().unwrap()));
    let person = owner.runtime.block_on(front).unwrap();
    let seed: PendingQueueItem = serde_json::from_value(json!({"author_id":1,"author_is_bot":true,
        "message_id":8_399_651,"channel_id":owner.channel,"source_message_ids":[8_399_651],
        "queued_generation":1,"text":"greeting"}))
    .unwrap();
    let context = json!({"origin":"headless","source":RESTART_SEED_SOURCE,"request_owner":1,
        "metadata":{"human_input":{"origin_id":"seed-op"},
            "restart_seed":{"operation":"seed-op","target_nonce":"old","dispatch":null}}});
    let admitted =
        (owner.mailbox).v2_admit_ticket_with_context(seed.clone(), "seed-op".into(), context);
    owner.runtime.block_on(admitted).unwrap();
    (person, seed)
}

/// One wake's grant and turn claim on the owner's current front.
fn claim_front(owner: &HerdrOwner) -> FrontClaim {
    owner.runtime.block_on(async {
        let granted = owner.mailbox.v2_request(|slot| {
            let v2 = slot.as_mut().ok_or("V2 not adopted")?;
            let front = v2.front().ok_or("no front")?;
            v2.grant_front(&front).map(|_| front)
        });
        let front = granted.await.unwrap();
        let claimed = claim_turn(&owner.mailbox, front.1.message_id, &front).await;
        claimed.unwrap().expect("the granted front is claimed")
    })
}

fn owed_receipts(owner: &HerdrOwner) -> Vec<Value> {
    let snapshot = owner.snapshot();
    snapshot["owed_drop_receipts"].as_array().cloned().unwrap()
}

#[test]
fn restart_seed_herdr_stop_after_reserve_cancels_seed_keeps_user_input() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = HerdrOwner::start("claude", 7_272_651);
    let _registry = owner.rig.registry_on_this_thread();
    owner.rig.answer("pane.read", claude_ready());
    let (person, _) = admit_seed(&owner);
    let (pool, record) = (
        unused_pool(&owner),
        owner.record("claude", HostedState::Bound),
    );
    let run = |cancel: Option<Arc<CancelToken>>, sid: &str| {
        let mut turn = owner.claude_turn(&pool);
        turn.cancel = cancel;
        let transcript = owner._dir.path().join(format!("{sid}.jsonl"));
        crate::services::claude_tui::readiness::tests::pre_source(&owner.logical, &transcript);
        let _waiting = owner.wait_for("claude", Lineage::Created);
        let _entered = owner.runtime.enter();
        let prompt = crate::services::claude::herdr_turn::owned_prompt_for_tests;
        prompt(&turn, &record, (sid, transcript), Lineage::Created)
    };
    let token = Arc::new(CancelToken::new());
    token.prepare_herdr_interrupt(ProviderKind::Claude, &record.owner);
    stop_at_input_boundary(&owner.logical, true);
    let stopped = run(Some(token), "seed-sid");
    stop_at_input_boundary(&owner.logical, false);
    assert!(
        stopped.as_ref().unwrap_err().starts_with("NotSent("),
        "{stopped:?}"
    );
    assert!(owner.rig.sends().is_empty(), "the greeting wrote nothing");
    let status = owner.mailbox.v2_seed_status("seed-op".into());
    assert_eq!(owner.runtime.block_on(status), Ok(SeedStatus::Gone));
    let owed = owed_receipts(&owner);
    assert_eq!(owed[0]["outcome"], "cancelled", "{owed:?}");
    assert_eq!(owed[0]["origins"], json!(["seed-op"]), "{owed:?}");
    let kept = owner.mailbox.v2_request(move |slot| {
        assert_original_and_ticket_kept(slot.as_ref().unwrap(), &person);
        Ok(())
    });
    owner.runtime.block_on(kept).unwrap();
    assert_eq!(run(None, "person-sid"), Ok(()));
    owner.delivered_once(PROMPT);
}

/// The owner's own handoff of a claimed greeting installs its successor record: the launch's
/// record lands in the greeting's context, and nothing records once the launch guard drops.
#[test]
fn seed_successor_recorded_through_take_owned() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = HerdrOwner::start("claude", 7_272_652);
    let (_, seed) = admit_seed(&owner);
    let claim = claim_front(&owner);
    let mut registration = owner.registration("claude", "herdr");
    registration.origin = InputOrigin::RestartSeed;
    let host = TurnHost::Herdr(Box::new(crate::services::turn_host::HerdrTurnPlan {
        endpoint: owner.endpoint(),
        row: None,
        bootstrap: false,
    }));
    let provider = ProviderKind::Claude;
    let turn = busy_turn::build(
        None,
        None,
        &provider,
        PROMPT,
        &owner.logical,
        &host,
        owner.channel,
    );
    let _entered = owner.runtime.enter();
    let recorded = || {
        let read = owner
            .mailbox
            .v2_admission_context(seed.clone(), "seed-op".into());
        let context = owner.runtime.block_on(read).unwrap().unwrap();
        restart_seed::successor_nonce(&context).map(str::to_owned)
    };
    assert_eq!(scope::record_successor("before"), Ok(()));
    assert_eq!(recorded(), None, "no record before the owner's handoff");
    let Some(Taken::Launch(guard)) = take_owned(&turn, Some(claim), registration, reply_once(None))
    else {
        panic!("a cold greeting hands its launch over");
    };
    assert_eq!(scope::record_successor("successor"), Ok(()));
    assert_eq!(recorded().as_deref(), Some("successor"));
    drop(guard);
    assert_eq!(scope::record_successor("after"), Ok(()));
    assert_eq!(
        recorded().as_deref(),
        Some("successor"),
        "the record left with the guard"
    );
}

/// A Herdr user stop before the reserve settles the claimed greeting inside the turn's own
/// failure finish; the same finish keeps a person's front with its original and ticket.
#[test]
fn failure_finish_settles_herdr_seed_stop_before_reserve() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = HerdrOwner::start("claude", 7_272_653);
    let (person, _) = admit_seed(&owner);
    let token = Arc::new(CancelToken::new());
    let record = owner.record("claude", HostedState::Pending);
    let stop = token.prepare_herdr_interrupt(ProviderKind::Claude, &record.owner);
    stop.user_stop
        .store(true, std::sync::atomic::Ordering::Release);
    let provider = ProviderKind::Claude;
    let finish = |claim: FrontClaim| {
        let mut turn = busy_turn::build(
            None,
            None,
            &provider,
            PROMPT,
            &owner.logical,
            &TurnHost::Tmux,
            owner.channel,
        );
        turn.cancel = token.clone();
        let _entered = owner.runtime.enter();
        let timeout: Result<(), String> =
            Err("NotSent(\"pending_unattached:n1:own_timeout\")".into());
        let failure = super::super::super::start_failure::Failure::claimed(&owner.mailbox, &claim);
        assert_eq!(failure.finish(&turn, Ok(None), timeout.clone()), timeout);
    };
    finish(claim_front(&owner));
    let status = owner.mailbox.v2_seed_status("seed-op".into());
    assert_eq!(owner.runtime.block_on(status), Ok(SeedStatus::Gone));
    let owed = owed_receipts(&owner);
    assert_eq!(owed.len(), 1, "{owed:?}");
    assert_eq!(owed[0]["outcome"], "cancelled", "{owed:?}");
    finish(claim_front(&owner));
    assert_eq!(
        owed_receipts(&owner).len(),
        1,
        "a person's front is not cancelled"
    );
    let kept = owner.mailbox.v2_request(move |slot| {
        assert_original_and_ticket_kept(slot.as_ref().unwrap(), &person);
        Ok(())
    });
    owner.runtime.block_on(kept).unwrap();
}
