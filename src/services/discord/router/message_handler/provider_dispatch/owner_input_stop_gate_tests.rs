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
