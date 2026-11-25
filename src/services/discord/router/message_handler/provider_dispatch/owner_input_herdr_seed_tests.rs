//! A restart greeting that a Herdr user stop closes after its reserve and before any key is
//! cancelled by the owner; the person's input behind it keeps its original and ticket.
use super::*;
use crate::services::provider::herdr_before_start::stop_at_input_boundary;
use crate::services::turn_orchestrator::{
    RESTART_SEED_SOURCE, SeedStatus, assert_original_and_ticket_kept,
};

#[test]
fn restart_seed_herdr_stop_after_reserve_cancels_seed_keeps_user_input() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let owner = HerdrOwner::start("claude", 7_272_651);
    let _registry = owner.rig.registry_on_this_thread();
    owner.rig.answer("pane.read", claude_ready());
    let front = (owner.mailbox).v2_request(|s| Ok(s.as_ref().unwrap().front().unwrap()));
    let person = owner.runtime.block_on(front).unwrap();
    let seed: PendingQueueItem = serde_json::from_value(json!({"author_id":1,"author_is_bot":true,
        "message_id":8_399_651,"channel_id":owner.channel,"source_message_ids":[8_399_651],
        "queued_generation":1,"text":"greeting"}))
    .unwrap();
    let context = json!({"origin":"headless","source":RESTART_SEED_SOURCE,"request_owner":1,
        "metadata":{"human_input":{"origin_id":"seed-op"},
            "restart_seed":{"operation":"seed-op","target_nonce":"old","dispatch":null}}});
    let admitted = (owner.mailbox).v2_admit_ticket_with_context(seed, "seed-op".into(), context);
    owner.runtime.block_on(admitted).unwrap();
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
    let snapshot = owner.snapshot();
    let owed = &snapshot["owed_drop_receipts"];
    assert_eq!(owed[0]["outcome"], "cancelled", "{snapshot}");
    assert_eq!(owed[0]["origins"], json!(["seed-op"]), "{snapshot}");
    let kept = owner.mailbox.v2_request(move |slot| {
        assert_original_and_ticket_kept(slot.as_ref().unwrap(), &person);
        Ok(())
    });
    owner.runtime.block_on(kept).unwrap();
    assert_eq!(run(None, "person-sid"), Ok(()));
    owner.delivered_once(PROMPT);
}
