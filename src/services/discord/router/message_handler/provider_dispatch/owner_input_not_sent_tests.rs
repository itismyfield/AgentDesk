//! An owned start that provably sent nothing: the original is retained with one owed notice,
//! nothing is resent, and the turn's inflight birth stays the one its bridge captured.
use super::*;
use crate::services::discord::inflight::{
    GuardedSaveOutcome, InflightTurnState, load_inflight_state, save_inflight_state,
    save_inflight_state_if_identity_unchanged,
};

#[test]
fn an_owned_start_that_sent_nothing_is_retained_once_and_keeps_its_birth() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("ns-idle", 7_272_571, "normal", view("", false), IDLE);
    // The pane's tmux refuses the load, so no key of the prompt can reach the pane.
    let (shim, fail) = (
        rig.f.root.path().join("tmux"),
        rig.f.root.path().join("ns-fail"),
    );
    let refuse = format!(
        "case \"$*\" in *load-buffer*) [ ! -f '{}' ] || exit 1 ;; esac\n",
        fail.display()
    );
    let script = fs::read_to_string(&shim).unwrap();
    fs::write(&shim, script.replacen("for arg", &(refuse + "for arg"), 1)).unwrap();
    fs::write(&fail, "").unwrap();
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        rig.channel,
        None,
        7,
        1,
        2,
        PROMPT.into(),
        None,
        Some(rig.name.clone()),
        None,
        None,
        0,
    );
    row.turn_nonce = Some("owned-birth".into());
    save_inflight_state(&row).unwrap();

    let (idle, transcript) = (target::Phase::Idle, rig.transcript());
    let result = rig.handoff(rig.granted_front(), ProviderKind::Claude, idle, &transcript);
    assert!(
        matches!(&result, Err(e) if e.starts_with("NotSent")),
        "{result:?}"
    );
    rig.held_not_sent("known_not_sent");
    let snapshot = rig.snapshot();
    let owed = json!({"kind": "retained_retry_required", "enqueued": false});
    assert_eq!(snapshot["retained_inputs"][0]["notice"], owed, "{snapshot}");
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    let front = rig
        .mailbox
        .v2_request(|slot| Ok(slot.as_ref().unwrap().front()));
    assert!(
        rig.runtime.block_on(front).unwrap().is_none(),
        "nothing is left to resend"
    );
    let after = load_inflight_state(&ProviderKind::Claude, rig.channel).unwrap();
    assert!(
        after.owned_by_turn_nonce(Some("owned-birth")),
        "{:?}",
        after.turn_nonce
    );
    assert_eq!(
        save_inflight_state_if_identity_unchanged(&after, "owned_start_bridge"),
        GuardedSaveOutcome::Saved,
        "the turn's bridge still owns its row"
    );
}
