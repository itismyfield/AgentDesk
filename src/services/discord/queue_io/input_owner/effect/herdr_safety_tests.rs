use super::*;

fn no_replay(attempt: &SafetyAttempt) {
    let snapshot = &attempt.first.2;
    assert_eq!(snapshot["head_injection"]["phase"], "unknown", "{snapshot}");
    assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
    let recovered = attempt.restarted.as_ref().expect("owner was restarted");
    assert!(
        recovered["items"].as_array().unwrap().is_empty(),
        "{recovered}"
    );
    assert!(recovered["head_injection"].is_null(), "{recovered}");
    assert_eq!(
        recovered["retained_inputs"].as_array().unwrap().len(),
        1,
        "{recovered}"
    );
    assert_eq!(
        attempt.retry_leaves, 0,
        "restart and deadline never replay the input"
    );
}

#[test]
fn owner_herdr_busy_effect_enters_common_operation_once() {
    let (_, completion, snapshot, nonce, _) = attempt(8_114_701, true, None, busy_screen());
    assert_eq!(completion, Completion::Submitted, "{snapshot}");
    assert_eq!(
        crate::services::claude_tui::host_input::busy::observed(&nonce),
        1,
        "the compatibility leaf must enter the same operation the real owner uses"
    );
}

#[test]
fn owner_herdr_payload_start_refused_after_stash_is_unknown_without_content() {
    let attempted = attempt_case(
        8_114_702,
        false,
        None,
        draft_screen("half typed"),
        FaultCase::StartAfterStash,
    );
    let (rig, completion, snapshot, nonce, stashed) = &attempted.first;
    assert_eq!(
        completion,
        &Completion::Failed("stash_failed".into()),
        "{snapshot}"
    );
    let sends = rig.sends();
    assert_eq!(sends.len(), 1, "only C-s reaches the pane: {sends:?}");
    assert_eq!(sends[0]["keys"], json!(["ctrl+s"]));
    let at_stash = stashed
        .lock()
        .unwrap()
        .clone()
        .expect("C-s reached the pane");
    assert_eq!(
        at_stash["payload_window"]["kind"], "bootstrap",
        "{at_stash}"
    );
    assert_eq!(reads_after_first_key(rig), 0);
    assert_eq!(alerts(nonce), 1);
    no_replay(&attempted);
}

#[test]
fn owner_herdr_replaced_pin_after_paste_sends_no_enter_and_never_replays() {
    let attempted = attempt_case(
        8_114_703,
        false,
        None,
        busy_screen(),
        FaultCase::PinAfterPaste,
    );
    let (rig, completion, snapshot, nonce, _) = &attempted.first;
    assert_eq!(
        completion,
        &Completion::WriteUncertain("enter_failed".into()),
        "{snapshot}"
    );
    let sends = rig.sends();
    assert_eq!(
        sends.len(),
        1,
        "only the paste can precede replacement: {sends:?}"
    );
    assert!(
        sends[0]["text"]
            .as_str()
            .unwrap()
            .starts_with("\u{1b}[200~")
    );
    assert_eq!(reads_after_first_key(rig), 0);
    assert_eq!(alerts(nonce), 1);
    no_replay(&attempted);
}

#[test]
fn owner_herdr_enter_reply_lost_never_replays_after_restart() {
    let attempted = attempt_case(
        8_114_704,
        false,
        None,
        busy_screen(),
        FaultCase::EnterReplyLost,
    );
    let (rig, completion, snapshot, nonce, _) = &attempted.first;
    assert_eq!(
        completion,
        &Completion::WriteUncertain("enter_failed".into()),
        "{snapshot}"
    );
    let sends = rig.sends();
    assert_eq!(
        sends.len(),
        2,
        "one paste and one unacknowledged Enter: {sends:?}"
    );
    assert_eq!(sends[1]["keys"], json!(["enter"]));
    assert_eq!(reads_after_first_key(rig), 0);
    assert_eq!(alerts(nonce), 1);
    no_replay(&attempted);
}
