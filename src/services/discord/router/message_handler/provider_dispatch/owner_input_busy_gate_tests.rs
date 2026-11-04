//! An armed owner start that finds the pane busy: its busy paste consumes the same first-key gate.
use super::*;
use crate::services::provider::cancel_token_claude_interrupt::StopCancel;

/// Holds the pane's first `step` call until `go` exists, once, and says when it is held.
fn hold_first(rig: &Rig, step: &str) -> (std::path::PathBuf, std::path::PathBuf, String) {
    let dir = rig.f.root.path().join(format!("busy-gate-{step}"));
    fs::create_dir_all(&dir).unwrap();
    let (held, go, shim) = (
        dir.join("held"),
        dir.join("go"),
        rig.f.root.path().join("tmux"),
    );
    let original = fs::read_to_string(&shim).unwrap();
    let hook = format!(
        "case \"$*\" in *{step}*) if [ ! -f '{h}' ]; then touch '{h}'; n=0; while [ ! -f '{g}' ] && [ $n -lt 1000 ]; do sleep 0.01; n=$((n+1)); done; fi ;; esac\n",
        h = held.display(),
        g = go.display()
    );
    let hooked = original.replacen("#!/bin/sh\n", &format!("#!/bin/sh\n{hook}"), 1);
    fs::write(&shim, hooked).unwrap();
    (held, go, original)
}

/// A stop that takes the gate just before the busy paste's first key leaves the pane keyless and
/// the attempt unconfirmed, never resent; a paste that keyed first leaves the gate MayHaveWritten.
#[test]
fn an_armed_start_on_a_busy_pane_pastes_only_through_the_first_key_gate() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (n, (case, stop_first)) in [("stop-before-paste", true), ("writer-first", false)]
        .into_iter()
        .enumerate()
    {
        let rig = rig(
            &format!("oi-busy-gate-{n}"),
            7_754_320 + n as u64,
            "normal",
            view("", false),
            BUSY,
        );
        let session = uuid::Uuid::new_v4().to_string();
        let elsewhere = tempfile::tempdir().unwrap();
        rehydrate(&rig, &session);
        let step = if stop_first {
            "load-buffer"
        } else {
            "paste-buffer"
        };
        let (held, go, original) = hold_first(&rig, step);
        let cancel = fresh();
        let sink = cancel.native_stop.receipt_sink();
        sink.arm_first_key();
        let (stopper, transcript) = (cancel.clone(), rig.transcript());
        let (renderer, gate) = (rig.renderer.clone(), sink.clone());
        let other = std::thread::spawn(move || {
            wait("the busy writer reaches its first key", || held.exists());
            let keyed = gate.may_have_written();
            if stop_first {
                let stop = StopCancel::decide(Some(stopper), "!stop".into());
                assert!(matches!(stop, StopCancel::Published(_)), "main cancels");
            }
            fs::write(go, "").unwrap();
            if !stop_first {
                // The pasted input's AI turn ends, so its observer returns.
                wait("Enter", || events(&renderer).iter().any(|e| e == "Enter"));
                let mut file = fs::OpenOptions::new()
                    .append(true)
                    .open(&transcript)
                    .unwrap();
                writeln!(file, "{}", IDLE.trim_end()).unwrap();
            }
            keyed
        });
        let (settled, _) = tokio::sync::oneshot::channel();
        let at = (session.as_str(), elsewhere.path());
        let (result, _) = idle_turn(&rig, at, cancel, (rig.granted_front(), settled), |_| None);
        let keyed_at_hold = other.join().unwrap();
        fs::write(rig.f.root.path().join("tmux"), &original).unwrap();
        let snapshot = rig.snapshot();
        if stop_first {
            assert!(result.is_err(), "{case}: {result:?}");
            assert!(rig.events().is_empty(), "{case}: no key {:?}", rig.events());
            assert!(sink.stopped_before_key(), "{case}");
            assert_eq!(snapshot["retained_inputs"], json!([]), "{case}: {snapshot}");
            assert!(
                !snapshot["head_injection"].is_null(),
                "{case}: the attempt stays unconfirmed: {snapshot}"
            );
        } else {
            assert!(keyed_at_hold, "{case}: the gate is taken before the paste");
            assert!(sink.may_have_written(), "{case}");
            assert_eq!(rig.events(), ["paste", "Enter"], "{case}: {result:?}");
        }
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&rig.name);
    }
}
