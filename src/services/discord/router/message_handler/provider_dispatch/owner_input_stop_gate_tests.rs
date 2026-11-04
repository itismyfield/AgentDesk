//! An owner idle submit a stop took before its first key: no key, no resend, no AI-turn wait.
use super::*;
use crate::services::provider::cancel_token_claude_interrupt::StopCancel;
use std::sync::atomic::{AtomicBool, Ordering};

/// A stop landing while the writer reads the pane, before its first key, leaves the pane keyless,
/// the attempt unconfirmed rather than resent, and no AI turn awaited.
#[test]
fn an_owner_idle_submit_a_stop_took_before_its_first_key_is_never_keyed_resent_or_awaited() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("oi-stop-gate", 7_754_300, "normal", view("", false), IDLE);
    let path = rig.transcript();
    // The writer's pane read, its last look before the first key, waits for the stop's decision.
    let dir = rig.f.root.path().join("stop-gate");
    fs::create_dir_all(&dir).unwrap();
    let (reading, go, shim) = (
        dir.join("reading"),
        dir.join("go"),
        rig.f.root.path().join("tmux"),
    );
    let original = fs::read_to_string(&shim).unwrap();
    let hook = format!(
        "case \"$*\" in *pane_dead*) if [ ! -f '{r}' ]; then touch '{r}'; while [ ! -f '{g}' ]; do sleep 0.01; done; fi ;; esac\n",
        r = reading.display(),
        g = go.display()
    );
    fs::write(
        &shim,
        original.replacen("#!/bin/sh\n", &format!("#!/bin/sh\n{hook}"), 1),
    )
    .unwrap();
    let claim = rig.claim(&rig.granted_front()).unwrap();
    let mut owner = rig.registration(
        &ProviderKind::Claude,
        target::Phase::Idle,
        &path,
        Execution::LocalTui,
    );
    let awaited = Arc::new(AtomicBool::new(false));
    let seen = awaited.clone();
    owner.until_done = Some(Box::new(move || {
        seen.store(true, Ordering::SeqCst);
        Ok(())
    }));
    let (settled, _) = tokio::sync::oneshot::channel();
    let mut token = None;
    let result = rig.with_turn(&ProviderKind::Claude, Some(&rig.name), |turn, sender| {
        let cancel = turn.cancel.clone();
        cancel.native_stop.receipt_sink().arm_first_key();
        token = Some(cancel.clone());
        let stopper = std::thread::spawn(move || {
            wait("writer reads the pane", || reading.exists());
            let stop = StopCancel::decide(Some(cancel), "!stop".into());
            fs::write(go, "").unwrap();
            matches!(stop, StopCancel::Published(_))
        });
        let result = execute_front(turn, sender, claim, owner, settled);
        assert!(stopper.join().unwrap(), "the stop published main's cancel");
        result
    });
    fs::write(&shim, &original).unwrap();
    let token = token.unwrap();
    let sink = token.native_stop.receipt_sink();
    assert!(result.is_err(), "{result:?}");
    assert!(sink.stopped_before_key());
    assert!(rig.events().is_empty(), "no key reached the pane");
    assert!(!awaited.load(Ordering::SeqCst), "no AI turn is awaited");
    let snapshot = rig.snapshot();
    eprintln!("stop gate snapshot={snapshot}");
    assert_eq!(
        snapshot["retained_inputs"],
        json!([]),
        "nothing held to resend: {snapshot}"
    );
    assert!(
        !snapshot["head_injection"].is_null(),
        "the attempt stays unconfirmed: {snapshot}"
    );
}

/// A Codex writer never consults the gate, so a stop after its paste still leaves the AI turn awaited.
#[test]
fn a_codex_idle_write_a_stop_found_ungated_still_awaits_its_ai_turn() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = super::super::codex_tests::codex_rig_on("oi-stop-codex", 7_754_310, true);
    // The pane is replaced after the paste so the Enter fails, and the Enter waits for the stop.
    rig.mark("swap-after", "paste");
    let dir = rig.f.root.path().join("stop-codex");
    fs::create_dir_all(&dir).unwrap();
    let (entering, go, shim) = (
        dir.join("entering"),
        dir.join("go"),
        rig.f.root.path().join("tmux"),
    );
    let original = fs::read_to_string(&shim).unwrap();
    let hook = format!(
        "case \"$*\" in *Enter*) if [ ! -f '{e}' ]; then touch '{e}'; n=0; while [ ! -f '{g}' ] && [ $n -lt 1000 ]; do sleep 0.01; n=$((n+1)); done; fi ;; esac\n",
        e = entering.display(),
        g = go.display()
    );
    fs::write(
        &shim,
        original.replacen("#!/bin/sh\n", &format!("#!/bin/sh\n{hook}"), 1),
    )
    .unwrap();
    let claim = rig.claim(&rig.granted_front()).unwrap();
    let path = rig.transcript();
    let mut owner = rig.registration(
        &ProviderKind::Codex,
        target::Phase::Idle,
        &path,
        Execution::LocalTui,
    );
    let awaited = Arc::new(AtomicBool::new(false));
    let seen = awaited.clone();
    owner.until_done = Some(Box::new(move || {
        seen.store(true, Ordering::SeqCst);
        Ok(())
    }));
    let (settled, _) = tokio::sync::oneshot::channel();
    let result = rig.with_turn(&ProviderKind::Codex, Some(&rig.name), |turn, sender| {
        let cancel = turn.cancel.clone();
        cancel.native_stop.receipt_sink().arm_first_key();
        let stopper = std::thread::spawn(move || {
            wait("the writer reaches its Enter", || entering.exists());
            let stop = StopCancel::decide(Some(cancel), "!stop".into());
            fs::write(go, "").unwrap();
            matches!(stop, StopCancel::Published(_))
        });
        let result = execute_front(turn, sender, claim, owner, settled);
        assert!(stopper.join().unwrap(), "the stop published main's cancel");
        result
    });
    fs::write(&shim, &original).unwrap();
    assert!(
        result.as_ref().is_err_and(|e| !effect::surely_unsent(e)),
        "{result:?}"
    );
    assert!(
        rig.events().contains(&"paste".to_string()),
        "{:?}",
        rig.events()
    );
    assert!(
        awaited.load(Ordering::SeqCst),
        "the AI turn the paste may start is awaited"
    );
}

/// A stop that took the idle writer's gate outranks the hold the writer meets after it, a pane
/// read that fails: unconfirmed, never held to resend once the pane reads again.
#[test]
fn an_owner_idle_submit_held_after_a_stop_took_its_gate_is_never_kept_to_resend() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("oi-stop-held", 7_754_340, "normal", view("", false), IDLE);
    let path = rig.transcript();
    let dir = rig.f.root.path().join("stop-held");
    fs::create_dir_all(&dir).unwrap();
    let (reading, go, unread, shim) = (
        dir.join("reading"),
        dir.join("go"),
        dir.join("unread"),
        rig.f.root.path().join("tmux"),
    );
    let original = fs::read_to_string(&shim).unwrap();
    let hook = format!(
        "case \"$*\" in *pane_dead*) if [ ! -f '{r}' ]; then touch '{r}'; while [ ! -f '{g}' ]; do sleep 0.01; done; [ -f '{u}' ] && exit 1; fi ;; esac\n",
        r = reading.display(),
        g = go.display(),
        u = unread.display()
    );
    fs::write(
        &shim,
        original.replacen("#!/bin/sh\n", &format!("#!/bin/sh\n{hook}"), 1),
    )
    .unwrap();
    let claim = rig.claim(&rig.granted_front()).unwrap();
    let owner = rig.registration(
        &ProviderKind::Claude,
        target::Phase::Idle,
        &path,
        Execution::LocalTui,
    );
    let (settled, _) = tokio::sync::oneshot::channel();
    let result = rig.with_turn(&ProviderKind::Claude, Some(&rig.name), |turn, sender| {
        let cancel = turn.cancel.clone();
        cancel.native_stop.receipt_sink().arm_first_key();
        let stopper = std::thread::spawn(move || {
            wait("writer reads the pane", || reading.exists());
            let stop = StopCancel::decide(Some(cancel), "!stop".into());
            // The held pane read then fails, so the writer holds before its first key.
            fs::write(unread, "").unwrap();
            fs::write(go, "").unwrap();
            matches!(stop, StopCancel::Published(_))
        });
        let result = execute_front(turn, sender, claim, owner, settled);
        assert!(stopper.join().unwrap(), "the stop published main's cancel");
        result
    });
    fs::write(&shim, &original).unwrap();
    assert!(
        result.as_ref().is_err_and(|e| !effect::surely_unsent(e)),
        "unconfirmed: {result:?}"
    );
    assert!(rig.events().is_empty(), "no key reached the pane");
    let snapshot = rig.snapshot();
    assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
    assert!(!snapshot["head_injection"].is_null(), "{snapshot}");
    let requeue = rig
        .mailbox
        .v2_request(|slot| slot.as_mut().unwrap().requeue_known_not_sent());
    assert_eq!(
        rig.runtime.block_on(requeue).unwrap().len(),
        0,
        "nothing goes back in line"
    );
}

/// An owner cold first prompt on an armed token whose composer liveness read, after readiness, is
/// held and then fails; a stop takes the gate during the hold when `stop`.
fn cold_first_prompt_held(name: &str, channel: u64, stop: bool) -> (Result<(), String>, Rig) {
    let rig = rig(name, channel, "normal", view("", false), IDLE);
    let dir = rig.f.root.path().join("cold-held");
    fs::create_dir_all(&dir).unwrap();
    let (reading, go, shim) = (
        dir.join("reading"),
        dir.join("go"),
        rig.f.root.path().join("tmux"),
    );
    let original = fs::read_to_string(&shim).unwrap();
    // Only the busy-inject composer read passes `-u`; readiness reads pane liveness otherwise.
    let hook = format!(
        "case \"$*\" in \"-u display-message\"*pane_dead*) if [ ! -f '{r}' ]; then touch '{r}'; while [ ! -f '{g}' ]; do sleep 0.01; done; exit 1; fi ;; esac\n",
        r = reading.display(),
        g = go.display()
    );
    fs::write(
        &shim,
        original.replacen("#!/bin/sh\n", &format!("#!/bin/sh\n{hook}"), 1),
    )
    .unwrap();
    let token = Arc::new(CancelToken::new());
    token.native_stop.receipt_sink().arm_first_key();
    let stopper = token.clone();
    let other = std::thread::spawn(move || {
        wait("the cold writer reads the composer", || reading.exists());
        if stop {
            let decided = StopCancel::decide(Some(stopper), "!stop".into());
            assert!(matches!(decided, StopCancel::Published(_)), "main cancels");
        }
        fs::write(go, "").unwrap();
    });
    let target = scope::created_tmux_target("claude", &rig.name).unwrap();
    let source = json!({"kind":"pre_source","expected_session_id":null,
        "expected_transcript":null,"binding_watermark":null});
    let (transcript, pane) = (rig.transcript().display().to_string(), rig.name.clone());
    let submit: scope::CreatedSubmit = Box::new(move |frame| {
        crate::services::claude::first_input::submit(&transcript, 0, Some(&token), &pane, &frame)
            .map(drop)
    });
    let hook = created(
        prepared_before_launch(&rig),
        PROMPT.into(),
        scope::Lineage::Created,
        (None, reply_once(None)),
    );
    let result = {
        let _entered = rig.runtime.enter();
        hook(scope::Lineage::Created, target, source, None, submit)
    };
    other.join().unwrap();
    fs::write(&shim, &original).unwrap();
    (result, rig)
}

/// Whether the owner would hand its front to the next drain.
fn front_offered(rig: &Rig) -> bool {
    let front = rig
        .mailbox
        .v2_request(|slot| Ok::<_, String>(slot.as_mut().ok_or("no owner")?.front().is_some()));
    rig.runtime.block_on(front).unwrap()
}

/// A stop that took a cold first prompt's gate outranks the hold its failed composer read records:
/// unconfirmed, nothing retained, and the next drain is offered nothing to send again.
#[test]
fn a_cold_first_prompt_held_after_a_stop_took_its_gate_is_never_sent_again() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (result, rig) = cold_first_prompt_held("oc-stop-cold", 7_754_350, true);
    assert!(
        result.as_ref().is_err_and(|e| !effect::surely_unsent(e)),
        "unconfirmed: {result:?}"
    );
    assert!(rig.events().is_empty(), "no key: {:?}", rig.events());
    let snapshot = rig.snapshot();
    assert_eq!(snapshot["retained_inputs"], json!([]), "{snapshot}");
    assert!(!snapshot["head_injection"].is_null(), "Unknown: {snapshot}");
    assert!(!front_offered(&rig), "no drain resends it: {snapshot}");
}

/// Without a stop the same failed composer read keeps the cold first prompt held to send later.
#[test]
fn a_cold_first_prompt_held_without_a_stop_stays_in_line() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (result, rig) = cold_first_prompt_held("oc-held-cold", 7_754_351, false);
    assert!(
        result.as_ref().is_err_and(|e| effect::surely_unsent(e)),
        "{result:?}"
    );
    assert!(rig.events().is_empty(), "no key: {:?}", rig.events());
    let snapshot = rig.snapshot();
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    assert!(front_offered(&rig), "held to send later: {snapshot}");
}
