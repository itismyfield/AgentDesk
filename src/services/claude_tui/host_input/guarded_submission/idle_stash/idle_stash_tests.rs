use super::*;
use crate::services::claude_tui::host_input::LegacyTmuxGate;
use crate::services::claude_tui::submission_fence::{
    PreSubmitPersistenceRefused, SubmissionFence, with_scope,
};
use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;
use std::cell::Cell;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub(crate) const PROMPT: &str = "[User: test]\nprepared question";
pub(crate) const FAST: Timing = Timing {
    lock_retries: &[],
    settle: Duration::from_millis(30),
    rechecks: 8,
    recheck_interval: Duration::from_millis(20),
    confirm_window: Duration::from_secs(1),
    confirm_poll: Duration::from_millis(10),
    restore_window: Duration::from_millis(200),
};

pub(crate) fn view(text: &str, stashed: bool) -> String {
    format!(
        "{}\n{}\n❯\u{a0}{}\n{}\n  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
        if stashed { "› stashed" } else { "" },
        "─".repeat(160),
        text.replace('\n', "\n  "),
        "─".repeat(160)
    )
}
fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}
fn wait(phase: &str, mut ready: impl FnMut() -> bool) {
    eprintln!("fixture phase={phase} waiting");
    let deadline = Instant::now() + Duration::from_secs(4);
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "fixture phase={phase} did not become ready"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    eprintln!("fixture phase={phase} reached");
}

struct Authority {
    identity: crate::services::tui_input::submission_tmux::guarded::PaneIdentity,
    admitted: Cell<bool>,
}
impl StashAuthority for Authority {
    fn admits(&self, pane: &GuardedPane) -> bool {
        self.admitted.get() && pane.identity() == self.identity
    }
}

pub(crate) fn renderer(f: &Fixture, name: &str, mode: &str) -> PathBuf {
    renderer_with(
        f,
        name,
        mode,
        view("js", false),
        "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
        PROMPT,
    )
}

pub(crate) fn renderer_with(
    f: &Fixture,
    name: &str,
    mode: &str,
    before: String,
    transcript: &str,
    prompt: &str,
) -> PathBuf {
    let root = f.root.path().join(name);
    fs::create_dir(&root).unwrap();
    for (file, bytes) in [
        ("before", before),
        ("stashed", view("", true)),
        ("own", view(prompt, true)),
        (
            "after",
            view(if mode == "never" { "" } else { "js" }, mode == "never"),
        ),
        ("transcript", transcript.into()),
        ("mode", mode.into()),
    ] {
        fs::write(root.join(file), bytes).unwrap();
    }
    let script = f.root.path().join("idle-renderer.py");
    fs::write(&script, r#"import json, os, pathlib, sys, tty
tty.setraw(0)
root = pathlib.Path(sys.argv[1])
mode = (root / 'mode').read_text()
def render(name):
    os.write(1, ('\x1b[?2004h\x1b[2J\x1b[32;1H' + (root/name).read_text().replace('\n','\r\n')).encode())
def event(name):
    with (root/'events').open('a') as out: out.write(name+'\n')
def record(content):
    with (root/'transcript').open('a') as out:
        out.write(json.dumps({'type':'user','message':{'content':content}})+'\n')
render('before')
(root/'ready').write_text('ready')
pending = b''
while True:
    pending += os.read(0, 65536)
    if pending.startswith(b'\x12'):
        if (root/'body').exists(): (root/'body').write_text((root/'body').read_text()+' changed by person')
        render('tampered'); pending=pending[1:]
    if pending.startswith(b'\x13'):
        if (root/'inflight').exists():
            state=json.loads(pathlib.Path((root/'inflight').read_text()).read_text())
            (root/'phase-at-stash').write_text(state['managed_submission']['phase'])
        event('C-s'); pending=pending[1:]; render('stashed')
    if pending.startswith(b'\x1b[200~') and b'\x1b[201~' in pending:
        body, pending = pending[6:].split(b'\x1b[201~',1)
        (root/'body').write_bytes(body); event('paste')
        render('own' if mode != 'own_race' else 'before')
    if b'\r' in pending:
        event('Enter')
        body=(root/'body').read_text()
        if mode == 'foreign': record('foreign before expected')
        if mode == 'mixed': record([{'type':'text','text':body},{'type':'tool_result','content':'x'}])
        else: record(body)
        render('after'); pending=b''
"#).unwrap();
    let command = format!("python3 {} {}", quote(&script), quote(&root));
    let pane = f.tmux(&[
        "new-session",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-s",
        name,
        "-x",
        "160",
        "-y",
        "40",
        &command,
    ]);
    f.tmux(&["set-window-option", "-t", &pane, "window-size", "manual"]);
    f.tmux(&["resize-window", "-t", &pane, "-x", "160", "-y", "40"]);
    wait("renderer-started", || root.join("ready").exists());
    wait("renderer-capture", || {
        f.tmux(&["capture-pane", "-p", "-t", &pane]).contains("❯")
    });
    root
}
pub(crate) fn events(root: &Path) -> Vec<String> {
    fs::read_to_string(root.join("events"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn missing_capability_and_off_do_no_preparation_or_io() {
    struct Never;
    impl StashAuthority for Never {
        fn admits(&self, _: &GuardedPane) -> bool {
            panic!("authority was queried");
        }
    }
    impl MutationGate for Never {
        fn admit(&self, _: &str) -> Result<(), InputRefusal> {
            panic!("gate was queried");
        }
    }
    let target = InputTarget::legacy_tmux("nonexistent-idle-stash-target");
    let invalid_request = Request {
        target: &target,
        gate: &Never,
        prompt: "\0invalid",
        transcript: Path::new("missing-transcript"),
        cancel: None,
        timing: &FAST,
    };
    assert_eq!(run(None, true, &invalid_request), None);
    assert!(!composer_lock::draft_guarded(
        "nonexistent-idle-stash-target"
    ));

    let f = Fixture::new();
    let root = renderer(&f, "idle-off", "normal");
    let authority = Authority {
        identity: SubmissionTmux.pin_guarded("idle-off").unwrap().identity(),
        admitted: Cell::new(true),
    };
    let target = InputTarget::legacy_tmux("idle-off");
    let transcript = root.join("transcript");
    let request = Request {
        target: &target,
        gate: &LegacyTmuxGate,
        prompt: PROMPT,
        transcript: &transcript,
        cancel: None,
        timing: &FAST,
    };
    let shim = f.root.path().join("tmux");
    let tape = root.join("io");
    let original = fs::read_to_string(&shim).unwrap();
    fs::write(
        &shim,
        original.replacen(
            "#!/bin/sh\n",
            &format!("#!/bin/sh\nprintf x >> {}\n", quote(&tape)),
            1,
        ),
    )
    .unwrap();
    assert_eq!(run(None, true, &request), None);
    assert_eq!(run(Some(&authority), false, &request), None);
    assert!(!tape.exists(), "OFF/None queried the pane");
    assert!(events(&root).is_empty());
    assert!(!composer_lock::draft_guarded("idle-off"));
    assert_eq!(run(Some(&Never), false, &invalid_request), None);
}

#[test]
fn pinned_idle_stash_preserves_delivery_and_protects_unrestored_draft() {
    let f = Fixture::new();
    for mode in ["normal", "never", "foreign", "mixed", "own_race"] {
        let name = format!("idle-{mode}");
        let root = renderer(&f, &name, mode);
        let authority = Authority {
            identity: SubmissionTmux.pin_guarded(&name).unwrap().identity(),
            admitted: Cell::new(true),
        };
        let target = InputTarget::legacy_tmux(&name);
        let transcript = root.join("transcript");
        let request = Request {
            target: &target,
            gate: &LegacyTmuxGate,
            prompt: PROMPT,
            transcript: &transcript,
            cancel: None,
            timing: &FAST,
        };
        let got = run(Some(&authority), true, &request).unwrap();
        let delivery = if ["normal", "never"].contains(&mode) {
            Delivery::Observed
        } else {
            Delivery::AttemptedUnconfirmed
        };
        assert_eq!(got.delivery, delivery, "mode={mode}");
        assert_eq!(
            got.draft,
            if ["never", "own_race"].contains(&mode) {
                DraftState::Unknown
            } else {
                DraftState::RestoredObserved
            },
            "mode={mode}"
        );
        let expected = if mode == "own_race" {
            vec!["C-s", "paste"]
        } else {
            vec!["C-s", "paste", "Enter"]
        };
        assert_eq!(events(&root), expected);
        assert_eq!(fs::read_to_string(root.join("body")).unwrap(), PROMPT);
        let pane = SubmissionTmux.pin_guarded(&name).unwrap();
        let admit = composer_lock::try_with_composer_permit(&name, |_| {
            composer_lock::admit_composer_write(&name, || pane.capture_draft())
        });
        assert_eq!(admit, Some(Err(composer_lock::DraftRecoveryHold)));
        let before = events(&root);
        assert_eq!(
            super::super::run(&target, &LegacyTmuxGate, "next question", None),
            if got.delivery == Delivery::Observed {
                refused(Refusal::NotReady)
            } else {
                got.input_run(None)
            }
        );
        assert_eq!(events(&root), before);
        guarded_outcome::forget_test_hold(&name);
    }
}

struct Fence {
    path: PathBuf,
    fail: bool,
}
impl SubmissionFence for Fence {
    fn before_first_payload(&self) -> Result<(), PreSubmitPersistenceRefused> {
        assert!(
            events(&self.path).is_empty(),
            "mutation preceded durable fence"
        );
        fs::write(self.path.join("fenced"), "fenced").unwrap();
        if self.fail {
            Err(PreSubmitPersistenceRefused {
                reason: "fixture persistence failure".into(),
            })
        } else {
            Ok(())
        }
    }
    fn refusal(&self) -> Option<PreSubmitPersistenceRefused> {
        None
    }
}

#[test]
fn fence_and_revoked_authority_precede_any_stash_effect() {
    let f = Fixture::new();
    for (name, revoked, fail) in [
        ("fence-fail", false, true),
        ("authority-revoked", true, false),
        ("fence-pass", false, false),
    ] {
        let root = renderer(&f, name, "normal");
        let authority = Authority {
            identity: SubmissionTmux.pin_guarded(name).unwrap().identity(),
            admitted: Cell::new(!revoked),
        };
        let target = InputTarget::legacy_tmux(name);
        let transcript = root.join("transcript");
        let request = Request {
            target: &target,
            gate: &LegacyTmuxGate,
            prompt: PROMPT,
            transcript: &transcript,
            cancel: None,
            timing: &FAST,
        };
        let got = with_scope(
            Some(Arc::new(Fence {
                path: root.clone(),
                fail,
            })),
            || run(Some(&authority), true, &request),
        )
        .unwrap();
        if revoked || fail {
            assert_eq!(got.delivery, Delivery::NotAttempted);
            assert_eq!(got.draft, DraftState::Unchanged);
            assert!(!got.entered);
            assert_eq!(
                got.refusal,
                revoked.then_some(InputRefusal::IdentityMismatch)
            );
            assert!(events(&root).is_empty());
        } else {
            assert_eq!(got.delivery, Delivery::Observed);
            assert_eq!(events(&root), ["C-s", "paste", "Enter"]);
        }
    }
}

fn append(path: &Path, record: serde_json::Value) {
    writeln!(
        OpenOptions::new().append(true).open(path).unwrap(),
        "{record}"
    )
    .unwrap();
}
#[test]
fn exact_receipt_rejects_first_foreign_mixed_partial_and_replaced_records() {
    use receipt::Receipt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript");
    let user = |body: &str| serde_json::json!({"type":"user","message":{"content":body}});
    for case in [
        "exact", "foreign", "mixed", "meta", "partial", "rotate", "rewrite",
    ] {
        fs::write(&path, "{\"type\":\"result\"}\n").unwrap();
        let receipt = Receipt::before(&path).unwrap();
        match case {
            "foreign" => append(&path, user("foreign")),
            "mixed" => append(
                &path,
                serde_json::json!({"type":"user","message":{"content":[{"type":"text","text":PROMPT},{"type":"tool_result","content":"x"}]}}),
            ),
            "meta" => append(
                &path,
                serde_json::json!({"type":"user","isMeta":true,"message":{"content":"metadata"}}),
            ),
            "partial" => {
                write!(
                    OpenOptions::new().append(true).open(&path).unwrap(),
                    "{}",
                    user(PROMPT)
                )
                .unwrap();
                assert_eq!(receipt.scan(PROMPT), None);
                writeln!(OpenOptions::new().append(true).open(&path).unwrap()).unwrap();
            }
            "rotate" => {
                fs::rename(&path, dir.path().join("retired")).unwrap();
                fs::write(&path, "").unwrap();
            }
            "rewrite" => {
                fs::write(&path, "{\"type\":\"system\"}\n").unwrap();
            }
            _ => {}
        }
        append(&path, user(PROMPT));
        assert_eq!(
            receipt.scan(PROMPT),
            Some(["exact", "meta", "partial"].contains(&case)),
            "case={case}"
        );
    }
    fs::write(&path, "{\"type\":\"result\"}").unwrap();
    assert!(Receipt::before(&path).is_none());
}

#[test]
fn pinned_stash_is_single_attempt_even_when_acknowledgement_is_lost() {
    let f = Fixture::new();
    for mode in ["normal", "drop"] {
        let name = format!("stash-once-{mode}");
        f.session(&name);
        if mode == "drop" {
            fs::write(f.root.path().join("mode"), mode).unwrap();
        }
        let mut pane = SubmissionTmux.pin_guarded(&name).unwrap();
        let first = pane.stash();
        if mode == "normal" {
            assert_eq!(first, EffectOutcome::Applied);
        } else {
            assert!(matches!(first, EffectOutcome::Unknown(_)));
        }
        assert_eq!(pane.stash(), EffectOutcome::Held);
        wait("stash-key-received", || f.received(&name).contains(&0x13));
        assert_eq!(f.received(&name).iter().filter(|b| **b == 0x13).count(), 1);
        fs::remove_file(f.root.path().join("mode")).ok();
    }
}

#[test]
fn each_lost_acknowledgement_stops_without_retry_or_cleanup() {
    for (stage, expected) in [
        ("C-s", vec!["C-s"]),
        ("paste-buffer", vec!["C-s", "paste"]),
        ("Enter", vec!["C-s", "paste", "Enter"]),
    ] {
        let f = Fixture::new();
        let root = renderer(&f, "lost-ack", "normal");
        let shim = f.root.path().join("tmux");
        let original = fs::read_to_string(&shim).unwrap();
        let hook = format!(
            "for effect in \"$@\"; do\n case \"$effect\" in *'{stage}'*) {} -L {} -f /dev/null \"$@\" >/dev/null; exit 0;; esac\ndone\n",
            quote(&f.real),
            f.socket
        );
        fs::write(
            &shim,
            original.replacen("#!/bin/sh\n", &format!("#!/bin/sh\n{hook}"), 1),
        )
        .unwrap();
        let authority = Authority {
            identity: SubmissionTmux.pin_guarded("lost-ack").unwrap().identity(),
            admitted: Cell::new(true),
        };
        let target = InputTarget::legacy_tmux("lost-ack");
        let transcript = root.join("transcript");
        let request = Request {
            target: &target,
            gate: &LegacyTmuxGate,
            prompt: PROMPT,
            transcript: &transcript,
            cancel: None,
            timing: &FAST,
        };
        let got = run(Some(&authority), true, &request).unwrap();
        assert_eq!(got.delivery, Delivery::AttemptedUnconfirmed, "{stage}");
        assert_eq!(
            got.draft,
            if stage == "Enter" {
                DraftState::RestoredObserved
            } else {
                DraftState::Unknown
            },
            "{stage}"
        );
        wait("lost-ack-effects", || events(&root).len() >= expected.len());
        assert_eq!(events(&root), expected);
        assert!(composer_lock::draft_guarded("lost-ack"));
    }
}

#[test]
fn cancel_and_gate_revocation_at_effect_boundaries_never_enter_or_fallback() {
    struct Gate {
        root: PathBuf,
        boundary: &'static str,
        token: Arc<CancelToken>,
        cancel: bool,
    }
    impl MutationGate for Gate {
        fn admit(&self, _: &str) -> Result<(), InputRefusal> {
            if self.boundary == "before" || events(&self.root).iter().any(|e| e == self.boundary) {
                if self.cancel {
                    self.token
                        .cancelled
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    return Ok(());
                }
                return Err(InputRefusal::IdentityMismatch);
            }
            Ok(())
        }
    }
    let f = Fixture::new();
    for (i, boundary) in ["before", "C-s", "paste"].into_iter().enumerate() {
        for cancel in [false, true] {
            let name = format!("boundary-{i}-{cancel}");
            let root = renderer(&f, &name, "normal");
            let token = Arc::new(CancelToken::new());
            let gate = Gate {
                root: root.clone(),
                boundary,
                token: token.clone(),
                cancel,
            };
            let authority = Authority {
                identity: SubmissionTmux.pin_guarded(&name).unwrap().identity(),
                admitted: Cell::new(true),
            };
            let target = InputTarget::legacy_tmux(&name);
            let transcript = root.join("transcript");
            let request = Request {
                target: &target,
                gate: &gate,
                prompt: PROMPT,
                transcript: &transcript,
                cancel: Some(&token),
                timing: &FAST,
            };
            let got = run(Some(&authority), true, &request).unwrap();
            assert_eq!(
                got.delivery,
                if i == 0 {
                    Delivery::NotAttempted
                } else {
                    Delivery::AttemptedUnconfirmed
                },
                "{boundary}/{cancel}"
            );
            assert_eq!(events(&root), ["C-s", "paste"][..i], "{boundary}/{cancel}");
        }
    }
}

#[test]
fn restore_observation_holds_same_pane_writer_while_other_pane_proceeds() {
    let f = Fixture::new();
    let root = renderer(&f, "restore-barrier", "normal");
    let shim = f.root.path().join("tmux");
    let original = fs::read_to_string(&shim).unwrap();
    let hook = format!(
        "for effect in \"$@\"; do\n if [ \"$effect\" = capture-pane ] && [ -f {} ] && grep -q Enter {}; then\n  touch {}\n  while [ ! -f {} ]; do sleep 0.01; done\n fi\ndone\n",
        quote(&root.join("events")),
        quote(&root.join("events")),
        quote(&root.join("observing")),
        quote(&root.join("release"))
    );
    fs::write(
        &shim,
        original.replacen("#!/bin/sh\n", &format!("#!/bin/sh\n{hook}"), 1),
    )
    .unwrap();
    let worker_root = root.clone();
    let (finished_tx, finished_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let target = InputTarget::legacy_tmux("restore-barrier");
        let authority = Authority {
            identity: SubmissionTmux
                .pin_guarded("restore-barrier")
                .unwrap()
                .identity(),
            admitted: Cell::new(true),
        };
        let transcript = worker_root.join("transcript");
        let request = Request {
            target: &target,
            gate: &LegacyTmuxGate,
            prompt: PROMPT,
            transcript: &transcript,
            cancel: None,
            timing: &FAST,
        };
        let got = run(Some(&authority), true, &request).unwrap();
        let _ = finished_tx.send(format!("{got:?}"));
        got
    });
    let while_observing = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wait("restore-observation", || {
            if let Ok(ended) = finished_rx.try_recv() {
                let probe = std::process::Command::new(&shim)
                    .args(["capture-pane", "-p", "-t", "=restore-barrier:"])
                    .output()
                    .unwrap();
                eprintln!(
                    "fixture capture status={} stderr={:?}",
                    probe.status,
                    String::from_utf8_lossy(&probe.stderr)
                );
                panic!(
                    "worker ended before restore barrier: {ended}; effects={:?}",
                    events(&root)
                );
            }
            root.join("observing").exists()
        });
        let got = super::super::run(
            &InputTarget::legacy_tmux("restore-barrier"),
            &LegacyTmuxGate,
            "competing input",
            None,
        );
        let calls = Cell::new(0);
        let same = composer_lock::try_with_composer_permit("restore-barrier", |_| {
            calls.set(calls.get() + 1);
            7
        });
        let other = composer_lock::try_with_composer_permit("other-idle-barrier-pane", |_| 7);
        (got, same, calls.get(), other)
    }));
    fs::write(root.join("release"), "release").unwrap();
    let ended = worker.join().unwrap();
    let (got, same, calls, other) = match while_observing {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    };
    assert_eq!(got, refused(Refusal::WriterContended));
    assert_eq!(same, None);
    assert_eq!(calls, 0);
    assert_eq!(other, Some(7));
    assert_eq!(ended.delivery, Delivery::Observed);
    assert_eq!(ended.draft, DraftState::RestoredObserved);
    assert_eq!(events(&root), ["C-s", "paste", "Enter"]);
}

#[test]
fn managed_admission_rejects_busy_stash_modal_agentdesk_and_nonflat_without_fence() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Count(AtomicUsize);
    impl SubmissionFence for Count {
        fn before_first_payload(&self) -> Result<(), PreSubmitPersistenceRefused> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn refusal(&self) -> Option<PreSubmitPersistenceRefused> {
            None
        }
    }
    let f = Fixture::new();
    for kind in ["busy", "stash", "modal", "agentdesk", "folded", "long"] {
        let name = format!("negative-{kind}");
        let mut before = view("js", kind == "stash");
        if kind == "modal" {
            before = format!(
                "  ❯ 1. Resume from summary (recommended)\n    2. Resume full session as-is\n  Enter to confirm · Esc to cancel\n{before}"
            );
        }
        if kind == "agentdesk" {
            before = view("[User: test]\nprevious", false);
            assert_eq!(
                busy_inject::composer_owner(&before),
                busy_inject::ComposerOwner::AgentDesk
            );
        }
        let transcript = if kind == "busy" {
            "{\"type\":\"user\",\"message\":{\"content\":\"previous\"}}\n"
        } else {
            "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n"
        };
        let long = "x".repeat(801);
        let prompt = match kind {
            "long" => long.as_str(),
            "folded" => "a\nb\nc\nd",
            _ => PROMPT,
        };
        let root = renderer_with(&f, &name, "normal", before, transcript, prompt);
        let authority = Authority {
            identity: SubmissionTmux.pin_guarded(&name).unwrap().identity(),
            admitted: Cell::new(true),
        };
        let target = InputTarget::legacy_tmux(&name);
        let path = root.join("transcript");
        let request = Request {
            target: &target,
            gate: &LegacyTmuxGate,
            prompt,
            transcript: &path,
            cancel: None,
            timing: &FAST,
        };
        let fence = Arc::new(Count(AtomicUsize::new(0)));
        let _result = with_scope(Some(fence.clone()), || {
            run(Some(&authority), true, &request)
        });
        assert!(events(&root).is_empty(), "{kind}: no C-s, paste or Enter");
        assert_eq!(fence.0.load(Ordering::SeqCst), 0, "{kind}: no fence entry");
    }
}
