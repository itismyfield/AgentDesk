//! An armed owner start that finds the pane busy: its busy paste consumes the same first-key gate.
use super::*;
use crate::services::provider::cancel_token_claude_interrupt::StopCancel;
use crate::test_support::wait::{WAIT_CAP, join_within};

#[test]
fn a_running_owner_dispatch_records_its_user_row_as_the_native_root() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig(
        "oi-running-native-root",
        7_755_000,
        "normal",
        view("", false),
        BUSY,
    );
    let session = uuid::Uuid::new_v4().to_string();
    let elsewhere = tempfile::tempdir().unwrap();
    rehydrate(&rig, &session);
    fs::write(rig.renderer.join("native-sid"), &session).unwrap();
    fs::write(rig.renderer.join("native-uuid"), "running-owned-user").unwrap();
    let cancel = fresh();
    let observer = cancel.clone();
    let renderer = rig.renderer.clone();
    let reader = std::thread::spawn(move || {
        struct CancelAtExit(Arc<CancelToken>);
        impl Drop for CancelAtExit {
            fn drop(&mut self) {
                self.0
                    .cancelled
                    .store(true, std::sync::atomic::Ordering::Release);
            }
        }
        let _cancel = CancelAtExit(observer.clone());
        wait("the running paste's Enter", || {
            events(&renderer).iter().any(|e| e == "Enter")
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(4);
        loop {
            if let Some(pin) = observer.native_stop.observe_native_input() {
                return Some(pin);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    let (settled, _) = tokio::sync::oneshot::channel();
    let (result, _) = idle_turn(
        &rig,
        (session.as_str(), elsewhere.path()),
        cancel.clone(),
        (rig.granted_front(), settled),
        |_| None,
    );
    let pin = join_within(reader, WAIT_CAP)
        .expect("BusyClaude must capture a native root for the user row recorded after Enter")
        .expect("the running paste's native user root must validate");
    assert_eq!(result, Ok(()));
    assert_eq!(pin.uuid, "running-owned-user");
    assert_eq!(pin.session_id, session);
    assert!(!pin.direct, "the running branch pins the owner's user root");
    assert_eq!(rig.events(), ["paste", "Enter"]);
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&rig.name);
}

fn raw_busy_paste(rig: &Rig) -> busy_inject::Report {
    let transcript = rig.transcript();
    let request = busy_inject::Request {
        session: &rig.name,
        transcript: &transcript,
        source: "discord",
        author: AUTHOR,
        name: NAME,
        nonce: &rig.nonce,
        text: PROMPT,
    };
    busy_inject::inject_owned(
        &busy_inject::Pane::with_program(&rig.name, rig.program.clone()),
        &request,
        &OWNER_TIMING,
        false,
    )
}

#[derive(Default)]
struct BusyCallbacks {
    starts: std::sync::atomic::AtomicUsize,
    finishes: std::sync::Mutex<Vec<crate::services::claude_tui::host_input::InputRun>>,
}

impl crate::services::claude_tui::guarded_outcome::Owner for BusyCallbacks {
    fn started(&self) {
        self.starts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    fn finished(&self, run: &crate::services::claude_tui::host_input::InputRun) {
        self.finishes.lock().unwrap().push(run.clone());
    }
}

#[test]
fn a_busy_paste_stopped_before_its_first_key_never_starts_its_owner() {
    use crate::services::claude_tui::{guarded_outcome, host_input::InputRun};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig(
        "oi-busy-stop-before-begin",
        7_755_004,
        "normal",
        view("", false),
        BUSY,
    );
    rehydrate(&rig, "stopped-before-begin-session");
    let token = fresh();
    let sink = token.native_stop.receipt_sink();
    sink.arm_first_key();
    assert_eq!(sink.stop_before_key(), Some(true));
    let callbacks = Arc::new(BusyCallbacks::default());
    let report = guarded_outcome::with_receipt_sink(Some(&token), || {
        guarded_outcome::with_owner(Some(callbacks.clone()), || raw_busy_paste(&rig))
    });
    assert_eq!(
        report.outcome,
        busy_inject::Outcome::NotSent(busy_inject::Veto::LockContended)
    );
    assert!(rig.events().is_empty(), "the stopped writer sends no key");
    assert_eq!(
        callbacks.starts.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert!(matches!(
        callbacks.finishes.lock().unwrap().as_slice(),
        [InputRun::Refused(_)]
    ));
    assert!(!sink.may_have_written());
    assert!(!sink.retains_receipt());
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&rig.name);
}

#[test]
fn failed_busy_stash_paste_and_enter_finish_as_indeterminate() {
    use crate::services::claude_tui::{guarded_outcome, host_input::InputRun};
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (n, (failure, pattern)) in [
        (busy_inject::WriteFailure::Stash, "*send-keys*C-s*"),
        (busy_inject::WriteFailure::Paste, "*paste-buffer*"),
        (busy_inject::WriteFailure::Enter, "*send-keys*Enter*"),
    ]
    .into_iter()
    .enumerate()
    {
        let stash = failure == busy_inject::WriteFailure::Stash;
        let rig = rig(
            &format!("oi-busy-typed-failure-{n}"),
            7_755_010 + n as u64,
            "normal",
            view(if stash { "person draft" } else { "" }, false),
            BUSY,
        );
        rehydrate(&rig, &format!("typed-failure-session-{n}"));
        let source = fs::read_to_string(&rig.program).unwrap();
        fs::write(
            &rig.program,
            source.replacen(
                "#!/bin/sh\n",
                &format!("#!/bin/sh\ncase \"$*\" in {pattern}) exit 1 ;; esac\n"),
                1,
            ),
        )
        .unwrap();
        let token = fresh();
        token.native_stop.receipt_sink().arm_first_key();
        let callbacks = Arc::new(BusyCallbacks::default());
        let report = guarded_outcome::with_receipt_sink(Some(&token), || {
            guarded_outcome::with_owner(Some(callbacks.clone()), || raw_busy_paste(&rig))
        });
        assert_eq!(report.outcome, busy_inject::Outcome::Failed(failure));
        assert_eq!(
            callbacks.starts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "{failure:?}"
        );
        assert!(
            matches!(
                callbacks.finishes.lock().unwrap().as_slice(),
                [InputRun::Indeterminate { .. }]
            ),
            "{failure:?}: write failure keeps its uncertainty"
        );
        assert!(
            !token.native_stop.receipt_sink().retains_receipt(),
            "{failure:?}"
        );
        assert_eq!(
            rig.events(),
            if failure == busy_inject::WriteFailure::Enter {
                vec!["paste".to_string()]
            } else {
                vec![]
            },
            "{failure:?}"
        );
        crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&rig.name);
    }
}

#[test]
fn a_busy_paste_without_a_receipt_sink_does_not_touch_an_enclosing_token() {
    use crate::services::claude_tui::guarded_outcome;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig(
        "oi-busy-no-sink",
        7_755_002,
        "normal",
        view("", false),
        BUSY,
    );
    rehydrate(&rig, "no-sink-session");
    let unrelated = fresh();
    let report = guarded_outcome::with_receipt_sink(Some(&unrelated), || {
        guarded_outcome::with_receipt_sink(None, || raw_busy_paste(&rig))
    });
    assert_eq!(report.outcome, busy_inject::Outcome::Injected);
    assert_eq!(rig.events(), ["paste", "Enter"]);
    assert!(!unrelated.native_stop.receipt_sink().retains_receipt());
    assert_eq!(unrelated.native_stop.receipt_sink().observed(), None);
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&rig.name);
}

#[test]
fn a_failed_busy_write_never_applies_or_retains_a_native_receipt() {
    use crate::services::claude_tui::{guarded_outcome, host_input::InputRun};
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig(
        "oi-busy-failed-receipt",
        7_755_003,
        "normal",
        view("", false),
        BUSY,
    );
    rehydrate(&rig, "failed-receipt-session");
    rig.mark("fail-load", "");
    let token = fresh();
    let (report, finished) = guarded_outcome::with_receipt_sink(Some(&token), || {
        guarded_outcome::capture(|| raw_busy_paste(&rig))
    });
    assert_eq!(
        report.outcome,
        busy_inject::Outcome::NotSent(busy_inject::Veto::LoadFailed)
    );
    assert!(
        !matches!(finished, Some(InputRun::Applied)),
        "failed writes never report Applied"
    );
    assert!(!token.native_stop.receipt_sink().retains_receipt());
    assert!(
        token
            .native_stop
            .observe_native_input()
            .is_none_or(|pin| pin.is_err())
    );
    assert!(rig.events().is_empty());
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&rig.name);
}

/// Holds the pane's first `step` call until `go` exists, once, and says when it is held.
fn hold_first(rig: &Rig, step: &str) -> (std::path::PathBuf, std::path::PathBuf, String) {
    hold_first_then(rig, step, ":")
}

/// As `hold_first`; `after` runs once the hold ends, and a `{refuse}` file fails every later
/// pane identity read.
fn hold_first_then(
    rig: &Rig,
    step: &str,
    after: &str,
) -> (std::path::PathBuf, std::path::PathBuf, String) {
    let dir = rig.f.root.path().join(format!("busy-gate-{step}"));
    fs::create_dir_all(&dir).unwrap();
    let (held, go, shim) = (
        dir.join("held"),
        dir.join("go"),
        rig.f.root.path().join("tmux"),
    );
    let original = fs::read_to_string(&shim).unwrap();
    let refuse = dir.join("refuse");
    let hook = format!(
        "case \"$*\" in *{step}*) if [ ! -f '{h}' ]; then touch '{h}'; n=0; while [ ! -f '{g}' ] && [ $n -lt 1000 ]; do sleep 0.01; n=$((n+1)); done; {after}; fi ;; esac\nif [ -f '{r}' ]; then case \"$*\" in *session_id*) exit 1 ;; esac; fi\n",
        h = held.display(),
        g = go.display(),
        r = refuse.display(),
        after = after.replace("{refuse}", &refuse.display().to_string()),
    );
    let hooked = original.replacen("#!/bin/sh\n", &format!("#!/bin/sh\n{hook}"), 1);
    fs::write(&shim, hooked).unwrap();
    (held, go, original)
}

/// A failed peer must release both the shim and the turn before its panic can be joined.
struct ReleaseBusyGateOnPanic {
    cancel: Arc<CancelToken>,
    go: std::path::PathBuf,
}

impl Drop for ReleaseBusyGateOnPanic {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.cancel
                .cancelled
                .store(true, std::sync::atomic::Ordering::Release);
            let _ = fs::write(&self.go, "");
        }
    }
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
        let release = ReleaseBusyGateOnPanic {
            cancel: cancel.clone(),
            go: go.clone(),
        };
        let other = std::thread::spawn(move || {
            let _release = release;
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
                // Enter is recorded before the renderer writes the user row; its turn must end later.
                let body = fs::read_to_string(renderer.join("body")).unwrap();
                wait("the busy input's transcript row", || {
                    fs::read_to_string(&transcript)
                        .unwrap()
                        .lines()
                        .any(|line| {
                            serde_json::from_str::<Value>(line).is_ok_and(|row| {
                                row["type"] == "user" && row["message"]["content"] == body
                            })
                        })
                });
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
        let keyed_at_hold = join_within(other, WAIT_CAP);
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

/// A busy start held at its buffer load, a stop deciding there when `stop` and `after` run as it
/// ends: the result, the keys sent, the owner snapshot and how many inputs recovery requeues.
fn refused_busy_start(
    name: &str,
    channel: u64,
    stop: bool,
    after: &str,
) -> (Result<(), String>, Vec<String>, Value, usize) {
    let rig = rig(name, channel, "normal", view("", false), BUSY);
    let session = uuid::Uuid::new_v4().to_string();
    let elsewhere = tempfile::tempdir().unwrap();
    rehydrate(&rig, &session);
    let (held, go, original) = hold_first_then(&rig, "load-buffer", after);
    let cancel = fresh();
    cancel.native_stop.receipt_sink().arm_first_key();
    let stopper = cancel.clone();
    let release = ReleaseBusyGateOnPanic {
        cancel: cancel.clone(),
        go: go.clone(),
    };
    let other = std::thread::spawn(move || {
        let _release = release;
        wait("the busy writer loads its buffer", || held.exists());
        if stop {
            let decided = StopCancel::decide(Some(stopper), "!stop".into());
            assert!(matches!(decided, StopCancel::Published(_)), "main cancels");
        }
        fs::write(go, "").unwrap();
    });
    let (settled, _) = tokio::sync::oneshot::channel();
    let at = (session.as_str(), elsewhere.path());
    let (result, _) = idle_turn(&rig, at, cancel, (rig.granted_front(), settled), |_| None);
    join_within(other, WAIT_CAP);
    fs::write(rig.f.root.path().join("tmux"), &original).unwrap();
    let snapshot = rig.snapshot();
    let requeue = rig
        .mailbox
        .v2_request(|slot| slot.as_mut().unwrap().requeue_known_not_sent());
    let requeued = rig.runtime.block_on(requeue).unwrap().len();
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&rig.name);
    (result, rig.events(), snapshot, requeued)
}

/// What a stop that won before the busy paste leaves behind, whatever refusal followed it.
fn assert_stopped_unconfirmed(
    case: &str,
    outcome: (Result<(), String>, Vec<String>, Value, usize),
) {
    let (result, events, snapshot, requeued) = outcome;
    assert!(
        result.as_ref().is_err_and(|e| !effect::surely_unsent(e)),
        "{case}: unconfirmed, {result:?}"
    );
    assert!(events.is_empty(), "{case}: no key {events:?}");
    assert_eq!(snapshot["retained_inputs"], json!([]), "{case}: {snapshot}");
    assert!(
        !snapshot["head_injection"].is_null(),
        "{case}: the attempt stays Unknown: {snapshot}"
    );
    assert_eq!(requeued, 0, "{case}: recovery puts nothing back");
}

/// A buffer load that fails after a stop took the gate does not make the stopped input resendable.
#[test]
fn a_load_that_fails_after_a_stop_won_leaves_the_input_unconfirmed() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let outcome = refused_busy_start("oi-busy-refused-0", 7_754_330, true, "exit 1");
    assert_stopped_unconfirmed("load-fails-after-stop", outcome);
}

/// A ready check refused after a stop took the gate does not make the stopped input resendable.
#[test]
fn a_ready_refused_after_a_stop_won_leaves_the_input_unconfirmed() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let outcome = refused_busy_start("oi-busy-refused-1", 7_754_331, true, "touch '{refuse}'");
    assert_stopped_unconfirmed("ready-refused-after-stop", outcome);
}

/// Without a stop, a failed buffer load still keeps the input in line to send once it loads.
#[test]
fn a_load_that_fails_without_a_stop_keeps_the_input_in_line() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (result, events, snapshot, _) =
        refused_busy_start("oi-busy-refused-2", 7_754_332, false, "exit 1");
    assert!(
        result.as_ref().is_err_and(|e| effect::surely_unsent(e)),
        "{result:?}"
    );
    assert!(events.is_empty(), "{events:?}");
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    assert_eq!(snapshot["items"][0]["text"], PROMPT, "{snapshot}");
}
