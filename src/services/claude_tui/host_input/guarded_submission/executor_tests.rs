#![cfg(unix)]

use super::*;
use crate::services::claude_tui::input;
use crate::services::claude_tui::submission_fence::{
    PreSubmitPersistenceRefused, SubmissionFence, with_scope,
};
use crate::services::provider::CancelToken;
use crate::services::tui_input::submission::Refusal;
use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn wait(mut ready: impl FnMut() -> bool) {
    let until = Instant::now() + Duration::from_secs(4);
    while !ready() {
        assert!(
            Instant::now() < until,
            "executor fixture state did not arrive"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn screen(body: &str) -> String {
    format!(
        "{}\n❯\u{a0}{}\n{}\n  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
        "─".repeat(160),
        body.replace('\n', "\n  "),
        "─".repeat(160)
    )
}

fn install_shim(f: &Fixture) {
    std::fs::write(
        f.root.path().join("tmux"),
        format!(
            r#"#!/bin/sh
capture=0
enter=0
joined=0
conditional=0
payload=0
for arg in "$@"; do
  [ "$arg" != capture-pane ] || capture=1
  [ "$arg" != -J ] || joined=1
  [ "$arg" != if-shell ] || conditional=1
  case "$arg" in *CG_ENTER_APPLIED*) enter=1;; esac
  case "$arg" in *CG_PAYLOAD_APPLIED*) payload=1;; esac
done
if [ "$capture" = 1 ]; then printf x >> {captures}; fi
if [ "$enter" = 1 ]; then printf x >> {enters}; fi
if [ "$capture" = 1 ] && [ "$joined" = 1 ]; then printf x >> {joined}; fi
if [ "$conditional" = 1 ]; then printf x >> {conditions}; fi
mode=$(cat {mode} 2>/dev/null)
pane=$(cat {pane} 2>/dev/null)
n=$(wc -c < {captures} 2>/dev/null || printf 0)
if [ "$capture" = 1 ] && [ "$mode" = post_resize ] && [ "$n" -ge 2 ]; then
  {real} -L {socket} resize-window -t "$pane" -x 80 -y 24
fi
if [ "$capture" = 1 ] && [ "$mode" = passive_fail ] && [ "$n" -ge 3 ]; then exit 1; fi
if [ "$payload" = 1 ] && [ "$mode" = drop_payload ]; then
  {real} -L {socket} -f /dev/null "$@" >/dev/null; exit 0
fi
if [ "$enter" = 1 ]; then
  case "$mode" in
    enter_resize) {real} -L {socket} resize-window -t "$pane" -x 80 -y 24;;
    enter_copy) {real} -L {socket} copy-mode -t "$pane";;
    enter_off) {real} -L {socket} select-pane -d -t "$pane";;
    drop_enter) {real} -L {socket} -f /dev/null "$@" >/dev/null; exit 0;;
    swallow_copy) {real} -L {socket} -f /dev/null copy-mode -t "$pane" \; send-keys -t "$pane" Enter \; send-keys -X -t "$pane" cancel \; display-message -p CG_ENTER_APPLIED; exit $?;;
    swallow_off) {real} -L {socket} -f /dev/null select-pane -d -t "$pane" \; send-keys -t "$pane" Enter \; select-pane -e -t "$pane" \; display-message -p CG_ENTER_APPLIED; exit $?;;
  esac
fi
exec {real} -L {socket} -f /dev/null "$@"
"#,
            captures = quote(&f.root.path().join("exec-captures")),
            enters = quote(&f.root.path().join("exec-enters")),
            joined = quote(&f.root.path().join("exec-joined")),
            conditions = quote(&f.root.path().join("exec-conditions")),
            mode = quote(&f.root.path().join("exec-mode")),
            pane = quote(&f.root.path().join("exec-pane")),
            real = quote(&f.real),
            socket = f.socket
        ),
    )
    .unwrap();
}

fn renderer(f: &Fixture, name: &str, frame: &str, before: &str, own: &str, after: &str) -> String {
    let root = f.root.path().join(name);
    std::fs::create_dir(&root).unwrap();
    for (file, content) in [
        ("expected", frame),
        ("before", before),
        ("own", own),
        ("after", after),
    ] {
        std::fs::write(root.join(file), content).unwrap();
    }
    let script = f.root.path().join("executor-renderer.py");
    std::fs::write(
        &script,
        r#"import os, pathlib, select, sys, tty
tty.setraw(0)
root = pathlib.Path(sys.argv[1])
def render(name):
    text = (root / name).read_text()
    os.write(1, ('\x1b[?2004h\x1b[2J\x1b[H' + text.replace('\n', '\r\n')).encode())
render('before')
(root / 'ready').write_text('ready')
expected = (root / 'expected').read_bytes()
pending = b''
payload = False
while True:
    if not select.select([0], [], [], .01)[0]:
        continue
    chunk = os.read(0, 65536)
    with (root / 'received').open('ab') as out:
        out.write(chunk)
    pending += chunk
    if not payload:
        pasted = pending.startswith(b'\x1b[200~')
        size = len(expected) + (12 if pasted else 0)
        if len(pending) < size:
            continue
        sent, pending = pending[:size], pending[size:]
        (root / 'frame').write_bytes(sent[6:-6] if pasted else sent)
        (root / 'kind').write_text('paste' if pasted else 'literal')
        with (root / 'payloads').open('ab') as out:
            out.write(b'x')
        payload = True
        render('own')
    for byte in pending:
        if byte == 13:
            with (root / 'enters').open('ab') as out:
                out.write(b'x')
            render('after')
    pending = b''
"#,
    )
    .unwrap();
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
    f.wait_ready(name);
    wait(|| {
        f.tmux(&["capture-pane", "-p", "-t", &pane])
            .contains("bypass permissions")
    });
    std::fs::write(f.root.path().join("exec-pane"), &pane).unwrap();
    std::fs::write(f.root.path().join("exec-captures"), "").unwrap();
    std::fs::write(f.root.path().join("exec-enters"), "").unwrap();
    std::fs::write(f.root.path().join("exec-joined"), "").unwrap();
    std::fs::write(f.root.path().join("exec-conditions"), "").unwrap();
    std::fs::write(f.root.path().join("exec-mode"), "").unwrap();
    pane
}

fn count(f: &Fixture, name: &str, file: &str) -> usize {
    std::fs::read(f.root.path().join(name).join(file))
        .unwrap_or_default()
        .len()
}

fn assert_receipt(f: &Fixture, name: &str, frame: &str, kind: &str, enters: usize) {
    wait(|| count(f, name, "payloads") == 1 && count(f, name, "enters") >= enters);
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        std::fs::read(f.root.path().join(name).join("frame")).unwrap(),
        frame.as_bytes()
    );
    assert_eq!(
        std::fs::read_to_string(f.root.path().join(name).join("kind")).unwrap(),
        kind
    );
    assert_eq!(count(f, name, "payloads"), 1, "one actual payload receipt");
    assert_eq!(count(f, name, "enters"), enters, "actual Enter receipt");
    let received = f.received(name);
    let expected = if kind == "paste" {
        [
            b"\x1b[200~".as_slice(),
            frame.as_bytes(),
            b"\x1b[201~",
            &vec![b'\r'; enters],
        ]
        .concat()
    } else {
        [frame.as_bytes(), &vec![b'\r'; enters]].concat()
    };
    assert_eq!(received, expected, "no duplicate payload or cleanup key");
}

#[derive(Default)]
struct Fence {
    calls: AtomicUsize,
    persisted: AtomicBool,
    refuse: bool,
    command: Option<(PathBuf, Vec<String>)>,
    arm: Option<(PathBuf, &'static str)>,
    lock_probe: Option<(String, Arc<AtomicBool>)>,
    cancel: Option<Arc<CancelToken>>,
}

impl SubmissionFence for Fence {
    fn before_first_payload(&self) -> Result<(), PreSubmitPersistenceRefused> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.refuse {
            return Err(PreSubmitPersistenceRefused {
                reason: "injected durable refusal".into(),
            });
        }
        self.persisted.store(true, Ordering::SeqCst);
        if let Some(cancel) = &self.cancel {
            cancel.cancelled.store(true, Ordering::SeqCst);
        }
        if let Some((session, blocked)) = &self.lock_probe {
            let session = session.clone();
            blocked.store(
                std::thread::spawn(move || {
                    crate::services::claude_tui::composer_lock::try_with_composer_mutation_lock(
                        &session,
                        || (),
                    )
                    .is_none()
                })
                .join()
                .unwrap(),
                Ordering::SeqCst,
            );
        }
        if let Some((program, args)) = &self.command {
            assert!(Command::new(program).args(args).status().unwrap().success());
        }
        if let Some((path, mode)) = &self.arm {
            std::fs::write(path.parent().unwrap().join("exec-captures"), "x").unwrap();
            std::fs::write(path, mode).unwrap();
        }
        Ok(())
    }
    fn refusal(&self) -> Option<PreSubmitPersistenceRefused> {
        (self.refuse && self.calls.load(Ordering::SeqCst) > 0).then(|| {
            PreSubmitPersistenceRefused {
                reason: "injected durable refusal".into(),
            }
        })
    }
}

fn direct(fence: Arc<Fence>, name: &str, frame: &str) -> InputRun {
    with_scope(Some(fence), || {
        run(
            &InputTarget::legacy_tmux(name),
            &LegacyTmuxGate,
            frame,
            None,
        )
    })
}

fn control(f: &Fixture) {
    renderer(
        f,
        "control",
        "control",
        &screen(""),
        &screen("control"),
        &screen(""),
    );
    assert_eq!(
        direct(Arc::new(Fence::default()), "control", "control"),
        InputRun::Applied
    );
    assert_receipt(f, "control", "control", "literal", 1);
}

#[derive(Clone, Copy, Debug)]
enum Entry {
    Fresh,
    Followup,
    ProvenWarm,
}

fn entry(
    f: &Fixture,
    which: Entry,
    name: &str,
    frame: &str,
    fence: Arc<Fence>,
) -> (Result<(), String>, Option<InputRun>) {
    let token = CancelToken::new();
    let transcript = f.root.path().join(format!("{name}.jsonl"));
    std::fs::write(&transcript, "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n").unwrap();
    with_scope(Some(fence), || {
        test_support::with_request(name, Rc::new(LegacyTmuxGate), || match which {
            Entry::Fresh => input::send_fresh_prompt(name, frame, Some(&token)),
            Entry::Followup => input::send_followup_prompt(name, frame, Some(&token)),
            Entry::ProvenWarm => input::send_followup_prompt_or_idle_transcript(
                name,
                frame,
                Some(&token),
                &transcript,
            ),
        })
    })
}

#[test]
fn cg_b2b_real_entries_preserve_one_payload_and_enter_with_strict_utf16_evidence() {
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    for (i, which) in [Entry::Fresh, Entry::Followup, Entry::ProvenWarm]
        .into_iter()
        .enumerate()
    {
        for (j, (frame, body, kind)) in [
            ("a  b".to_string(), "a  b".to_string(), "literal"),
            (
                format!("{}😀", "a".repeat(799)),
                "[Pasted text #7]".to_string(),
                "paste",
            ),
            (
                "a\nb\nc\nd".to_string(),
                "[Pasted text #7 +3 lines]".to_string(),
                "paste",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let name = format!("entry-{i}-{j}");
            renderer(&f, &name, &frame, &screen(""), &screen(&body), &screen(""));
            let fence = Arc::new(Fence::default());
            let (result, typed) = entry(&f, which, &name, &frame, Arc::clone(&fence));
            assert_eq!(result, Ok(()), "{which:?}: {typed:?}");
            assert_eq!(typed, Some(InputRun::Applied));
            assert_eq!(fence.calls.load(Ordering::SeqCst), 1);
            assert_receipt(&f, &name, &frame, kind, 1);
        }
    }
}

#[test]
fn cg_b2b_real_literal_532_533_799_800_softwrap_has_no_capacity_veto() {
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    for size in [532, 533, 799, 800] {
        for (variant, frame) in [
            "가".repeat(size),
            format!(
                "{}가{}",
                "가 ".repeat((size - 1) / 2),
                "가".repeat(usize::from(size % 2 == 0))
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let name = format!("literal-{size}-{variant}");
            assert_eq!(frame.encode_utf16().count(), size);
            renderer(&f, &name, &frame, &screen(""), &screen(&frame), &screen(""));
            assert_eq!(
                direct(Arc::new(Fence::default()), &name, &frame),
                InputRun::Applied,
                "UTF16 {size}, variant {variant}"
            );
            assert_receipt(&f, &name, &frame, "literal", 1);
        }
    }
}

#[test]
fn cg_b2b_pre_refusal_keeps_fence_waiting_and_sends_nothing() {
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    control(&f);
    for mode in ["small", "foreign", "modal"] {
        let before = if mode == "foreign" {
            screen("foreign draft")
        } else if mode == "modal" {
            format!(
                "{}\nDo you want to proceed?\n1. Yes\n2. No\nEnter to confirm · esc to cancel",
                screen("")
            )
        } else {
            screen("")
        };
        let pane = renderer(&f, mode, "own", &before, &screen("own"), &screen(""));
        if mode == "small" {
            f.tmux(&["resize-window", "-t", &pane, "-x", "80", "-y", "24"]);
        }
        let fence = Arc::new(Fence::default());
        assert!(
            matches!(
                direct(Arc::clone(&fence), mode, "own"),
                InputRun::Refused(_)
            ),
            "{mode}"
        );
        assert_eq!(
            fence.calls.load(Ordering::SeqCst),
            0,
            "PRE must precede durable boundary"
        );
        assert!(!fence.persisted.load(Ordering::SeqCst));
        assert!(f.received(mode).is_empty());
    }
}

#[test]
fn cg_b2b_resize_in_fence_keeps_payload_zero_and_post_fence_hold_monotone() {
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    control(&f);
    let pane = renderer(
        &f,
        "fence-resize",
        "own",
        &screen(""),
        &screen("own"),
        &screen(""),
    );
    let fence = Arc::new(Fence {
        command: Some((
            f.real.clone(),
            vec![
                "-L".into(),
                f.socket.clone(),
                "resize-window".into(),
                "-t".into(),
                pane,
                "-x".into(),
                "80".into(),
                "-y".into(),
                "24".into(),
            ],
        )),
        ..Fence::default()
    });
    let stopped = direct(Arc::clone(&fence), "fence-resize", "own");
    assert!(
        matches!(stopped, InputRun::Indeterminate { confirmed: 0, .. }),
        "{stopped:?}"
    );
    assert_eq!(fence.calls.load(Ordering::SeqCst), 1);
    assert!(
        fence.persisted.load(Ordering::SeqCst),
        "fence cannot roll back after conditional payload hold"
    );
    assert!(f.received("fence-resize").is_empty());
}

#[test]
fn cg_b2b_post_capture_and_commit_races_preserve_payload_and_never_enter() {
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    control(&f);
    for mode in ["post_resize", "enter_resize", "enter_copy", "enter_off"] {
        renderer(&f, mode, "own", &screen(""), &screen("own"), &screen(""));
        let fence = Arc::new(Fence {
            arm: Some((f.root.path().join("exec-mode"), mode)),
            ..Fence::default()
        });
        let stopped = direct(fence, mode, "own");
        assert!(
            matches!(stopped, InputRun::Indeterminate { confirmed: 1.., .. }),
            "{mode}: {stopped:?}"
        );
        assert_receipt(&f, mode, "own", "literal", 0);
        assert_eq!(
            std::fs::read(f.root.path().join("exec-enters"))
                .unwrap()
                .len(),
            usize::from(mode != "post_resize"),
            "POST hold must stop before conditional Enter"
        );
    }
}

#[test]
fn cg_b2b_enter_ack_loss_is_hold_with_actual_enter_one_and_retry_zero() {
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    control(&f);
    renderer(&f, "lost", "own", &screen(""), &screen("own"), &screen(""));
    let fence = Arc::new(Fence {
        arm: Some((f.root.path().join("exec-mode"), "drop_enter")),
        ..Fence::default()
    });
    let stopped = direct(fence, "lost", "own");
    assert!(
        matches!(
            stopped,
            InputRun::Indeterminate {
                confirmed: 1..,
                cause: StopCause::Send(_)
            }
        ),
        "{stopped:?}"
    );
    assert_receipt(&f, "lost", "own", "literal", 1);
    assert_eq!(
        std::fs::read(f.root.path().join("exec-enters"))
            .unwrap()
            .len(),
        1,
        "one conditional Enter call"
    );
}

#[test]
fn cg_b2b_passive_confirmation_failure_leaves_draft_and_provenance_without_cleanup() {
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    control(&f);
    for (name, mode, after) in [
        ("passive-unavailable", "passive_fail", screen("")),
        ("passive-draft", "", screen("own")),
        ("foreign-post", "", screen("")),
    ] {
        let own = if name == "foreign-post" {
            screen("foreign draft")
        } else {
            screen("own")
        };
        renderer(&f, name, "own", &screen(""), &own, &after);
        let fence = Arc::new(Fence {
            arm: Some((f.root.path().join("exec-mode"), mode)),
            ..Fence::default()
        });
        let (result, typed) = entry(&f, Entry::Fresh, name, "own", fence);
        assert!(result.is_err(), "{name}: {typed:?}");
        assert!(
            matches!(typed, Some(InputRun::Indeterminate { confirmed: 1.., .. })),
            "{name}: {typed:?}"
        );
        assert_receipt(
            &f,
            name,
            "own",
            "literal",
            usize::from(name != "foreign-post"),
        );
        assert_eq!(
            crate::services::tui_prompt_dedupe::observe_prompt_by_tmux("claude", name, "own"),
            crate::services::tui_prompt_dedupe::PromptObservation::SuppressedDiscordDuplicate,
            "post-fence provenance remains"
        );
    }
}

#[test]
fn cg_b2b_actual_composer_lock_excludes_other_writer_at_fence() {
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    renderer(
        &f,
        "locked",
        "own",
        &screen(""),
        &screen("own"),
        &screen(""),
    );
    let blocked = Arc::new(AtomicBool::new(false));
    let fence = Arc::new(Fence {
        lock_probe: Some(("locked".into(), Arc::clone(&blocked))),
        ..Fence::default()
    });
    assert_eq!(direct(fence, "locked", "own"), InputRun::Applied);
    assert!(
        blocked.load(Ordering::SeqCst),
        "another thread could obtain the composer lock during submit"
    );
    assert_receipt(&f, "locked", "own", "literal", 1);
}

struct RejectGate;
impl MutationGate for RejectGate {
    fn admit(&self, _: &str) -> Result<(), InputRefusal> {
        Err(InputRefusal::IdentityMismatch)
    }
}

#[test]
fn cg_b2b_gate_cancel_and_durable_refusal_prevent_payload() {
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    control(&f);
    for name in ["gate", "cancel", "durable"] {
        renderer(&f, name, "own", &screen(""), &screen("own"), &screen(""));
        let fence = Arc::new(Fence {
            refuse: name == "durable",
            ..Fence::default()
        });
        let target = InputTarget::legacy_tmux(name);
        let cancel = CancelToken::new();
        cancel.cancelled.store(name == "cancel", Ordering::SeqCst);
        let gate: &dyn MutationGate = if name == "gate" {
            &RejectGate
        } else {
            &LegacyTmuxGate
        };
        let stopped = with_scope(Some(Arc::clone(&fence) as Arc<dyn SubmissionFence>), || {
            run(&target, gate, "own", Some(&cancel))
        });
        assert_ne!(stopped, InputRun::Applied, "{name}");
        assert_eq!(
            fence.calls.load(Ordering::SeqCst),
            usize::from(name == "durable")
        );
        assert!(!fence.persisted.load(Ordering::SeqCst));
        assert!(
            f.received(name).is_empty(),
            "{name}: no key reaches application"
        );
    }
}

#[test]
fn cg_b2b_default_off_real_entries_keep_legacy_io_and_no_guarded_observer() {
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let _empty = crate::config::session_hosts::force_for_test(None, &[]);
    assert!(!crate::config::session_hosts::claude_guarded(Some(1)));
    for (i, which) in [Entry::Fresh, Entry::Followup, Entry::ProvenWarm]
        .into_iter()
        .enumerate()
    {
        let name = format!("off-{i}");
        let frame = "legacy exact  body";
        renderer(&f, &name, frame, &screen(""), &screen(frame), &screen(""));
        let fence = Arc::new(Fence::default());
        assert_eq!(selected(&name, frame, None), None);
        assert_eq!(count(&f, &name, "payloads"), 0);
        assert_eq!(
            std::fs::read(f.root.path().join("exec-captures"))
                .unwrap()
                .len(),
            0
        );
        let token = CancelToken::new();
        let transcript = f.root.path().join(format!("{name}.jsonl"));
        std::fs::write(&transcript, "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n").unwrap();
        let result = with_scope(
            Some(Arc::clone(&fence) as Arc<dyn SubmissionFence>),
            || match which {
                Entry::Fresh => input::send_fresh_prompt(&name, frame, Some(&token)),
                Entry::Followup => input::send_followup_prompt(&name, frame, Some(&token)),
                Entry::ProvenWarm => input::send_followup_prompt_or_idle_transcript(
                    &name,
                    frame,
                    Some(&token),
                    &transcript,
                ),
            },
        );
        assert_eq!(result, Ok(()), "{which:?}");
        assert_eq!(fence.calls.load(Ordering::SeqCst), 1);
        assert_receipt(&f, &name, frame, "literal", 1);
        for counter in ["exec-enters", "exec-joined", "exec-conditions"] {
            assert_eq!(
                std::fs::read(f.root.path().join(counter)).unwrap().len(),
                0,
                "OFF entered guarded transport: {counter}"
            );
        }
    }
}

#[test]
fn cg_b2b_selected_steering_holds_before_fence_without_legacy_fallback() {
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    control(&f);
    renderer(
        &f,
        "steering",
        "own",
        &format!("✳ Architecting…\n{}", screen("")),
        &screen("own"),
        &screen(""),
    );
    let fence = Arc::new(Fence::default());
    let (result, typed) = with_scope(Some(Arc::clone(&fence) as Arc<dyn SubmissionFence>), || {
        test_support::with_request("steering", Rc::new(LegacyTmuxGate), || {
            input::inject_steering_prompt("steering", "own")
        })
    });
    assert!(result.is_err());
    assert_eq!(
        typed,
        Some(InputRun::Refused(InputRefusal::Composer(Refusal::NotReady)))
    );
    assert_eq!(fence.calls.load(Ordering::SeqCst), 0);
    assert!(!fence.persisted.load(Ordering::SeqCst));
    assert!(f.received("steering").is_empty());
    assert_eq!(
        std::fs::read(f.root.path().join("exec-conditions"))
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn cg_b2b_applied_enter_ack_without_app_delivery_is_passive_confirmation_hold() {
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    control(&f);
    for mode in ["swallow_copy", "swallow_off"] {
        let pane = renderer(&f, mode, "own", &screen(""), &screen("own"), &screen(""));
        let fence = Arc::new(Fence {
            arm: Some((f.root.path().join("exec-mode"), mode)),
            ..Fence::default()
        });
        let (result, stopped) = entry(&f, Entry::Fresh, mode, "own", Arc::clone(&fence));
        assert!(result.is_err());
        assert_eq!(
            stopped,
            Some(InputRun::Indeterminate {
                confirmed: 2,
                cause: StopCause::Refused(InputRefusal::Composer(Refusal::Confirmation))
            })
        );
        assert_eq!(fence.calls.load(Ordering::SeqCst), 1);
        assert!(fence.persisted.load(Ordering::SeqCst));
        assert_receipt(&f, mode, "own", "literal", 0);
        assert_eq!(
            f.tmux(&[
                "display-message",
                "-p",
                "-t",
                &pane,
                "#{pane_in_mode} #{pane_input_off}"
            ]),
            "0 0"
        );
        assert!(f.tmux(&["capture-pane", "-p", "-t", &pane]).contains("own"));
        assert_eq!(
            std::fs::read(f.root.path().join("exec-enters"))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            crate::services::tui_prompt_dedupe::observe_prompt_by_tmux("claude", mode, "own"),
            crate::services::tui_prompt_dedupe::PromptObservation::SuppressedDiscordDuplicate
        );
    }
}

#[test]
fn cg_b2b_lf2_placeholder_is_not_strict_owned_draft_and_enter_stays_zero() {
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    control(&f);
    let frame = "a\nb\nc";
    renderer(
        &f,
        "lf2",
        frame,
        &screen(""),
        &screen("[Pasted text #7 +2 lines]"),
        &screen(""),
    );
    assert_eq!(
        direct(Arc::new(Fence::default()), "lf2", frame),
        InputRun::Indeterminate {
            confirmed: 1,
            cause: StopCause::Refused(InputRefusal::Composer(Refusal::OwnDraft))
        }
    );
    assert_receipt(&f, "lf2", frame, "paste", 0);
    assert_eq!(
        std::fs::read(f.root.path().join("exec-enters"))
            .unwrap()
            .len(),
        0
    );
}

struct CancellingGate {
    at: usize,
    calls: AtomicUsize,
    token: Arc<CancelToken>,
}

impl MutationGate for CancellingGate {
    fn admit(&self, _: &str) -> Result<(), InputRefusal> {
        if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == self.at {
            self.token.cancelled.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
}

#[test]
fn cg_b2b_cancellation_during_payload_and_enter_gate_blocks_the_next_effect() {
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    control(&f);
    for (at, confirmed) in [(4, 0), (5, 1)] {
        let name = format!("gate-cancel-{at}");
        renderer(&f, &name, "own", &screen(""), &screen("own"), &screen(""));
        let token = Arc::new(CancelToken::new());
        let gate = CancellingGate {
            at,
            calls: AtomicUsize::new(0),
            token: Arc::clone(&token),
        };
        let fence = Arc::new(Fence::default());
        let stopped = with_scope(Some(Arc::clone(&fence) as Arc<dyn SubmissionFence>), || {
            run(&InputTarget::legacy_tmux(&name), &gate, "own", Some(&token))
        });
        assert!(
            matches!(stopped, InputRun::Indeterminate { confirmed: seen, .. } if seen == confirmed),
            "gate {at}: {stopped:?}"
        );
        assert_eq!(gate.calls.load(Ordering::SeqCst), at);
        assert_eq!(fence.calls.load(Ordering::SeqCst), 1);
        assert!(fence.persisted.load(Ordering::SeqCst));
        if confirmed == 0 {
            assert!(f.received(&name).is_empty());
        } else {
            assert_receipt(&f, &name, "own", "literal", 0);
        }
    }
}

#[test]
fn cg_b2b_fence_success_then_cancellation_is_post_fence_hold_with_zero_payload() {
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    control(&f);
    renderer(
        &f,
        "fence-cancel",
        "own",
        &screen(""),
        &screen("own"),
        &screen(""),
    );
    let token = Arc::new(CancelToken::new());
    let fence = Arc::new(Fence {
        cancel: Some(Arc::clone(&token)),
        ..Fence::default()
    });
    let stopped = with_scope(Some(Arc::clone(&fence) as Arc<dyn SubmissionFence>), || {
        run(
            &InputTarget::legacy_tmux("fence-cancel"),
            &LegacyTmuxGate,
            "own",
            Some(&token),
        )
    });
    assert!(
        matches!(stopped, InputRun::Indeterminate { confirmed: 0, .. }),
        "{stopped:?}"
    );
    assert!(fence.persisted.load(Ordering::SeqCst));
    assert_eq!(fence.calls.load(Ordering::SeqCst), 1);
    assert!(f.received("fence-cancel").is_empty());
}

#[test]
fn cg_b2b_real_entries_hold_post_fence_zero_count_without_fallback_or_dedupe_removal() {
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    control(&f);
    for (i, which) in [Entry::Fresh, Entry::ProvenWarm].into_iter().enumerate() {
        for mode in ["resize", "drop_payload"] {
            let name = format!("entry-hold-{i}-{mode}");
            let pane = renderer(&f, &name, "own", &screen(""), &screen("own"), &screen(""));
            let fence = Arc::new(if mode == "resize" {
                Fence {
                    command: Some((
                        f.real.clone(),
                        vec![
                            "-L".into(),
                            f.socket.clone(),
                            "resize-window".into(),
                            "-t".into(),
                            pane,
                            "-x".into(),
                            "80".into(),
                            "-y".into(),
                            "24".into(),
                        ],
                    )),
                    ..Fence::default()
                }
            } else {
                Fence {
                    arm: Some((f.root.path().join("exec-mode"), "drop_payload")),
                    ..Fence::default()
                }
            });
            let (result, stopped) = entry(&f, which, &name, "own", Arc::clone(&fence));
            assert!(result.is_err());
            assert!(
                matches!(stopped, Some(InputRun::Indeterminate { confirmed: 0, .. })),
                "{which:?}/{mode}: {stopped:?}"
            );
            assert_eq!(fence.calls.load(Ordering::SeqCst), 1);
            assert!(fence.persisted.load(Ordering::SeqCst));
            if mode == "resize" {
                assert!(f.received(&name).is_empty());
            } else {
                assert_receipt(&f, &name, "own", "literal", 0);
            }
            assert_eq!(
                crate::services::tui_prompt_dedupe::observe_prompt_by_tmux("claude", &name, "own"),
                crate::services::tui_prompt_dedupe::PromptObservation::SuppressedDiscordDuplicate
            );
        }
    }
}

#[test]
fn cg_b2b_already_held_composer_lock_refuses_before_fence_and_payload() {
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    control(&f);
    let name = format!("contended-{}", uuid::Uuid::new_v4());
    renderer(&f, &name, "own", &screen(""), &screen("own"), &screen(""));
    let fence = Arc::new(Fence::default());
    let target = InputTarget::legacy_tmux(&name);
    let worker_fence = Arc::clone(&fence);
    let (worker, received) =
        crate::services::claude_tui::composer_lock::with_composer_mutation_lock(&name, || {
            let (tx, rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let stopped = with_scope(Some(worker_fence as Arc<dyn SubmissionFence>), || {
                    run(&target, &LegacyTmuxGate, "own", None)
                });
                let _ = tx.send(stopped);
            });
            (worker, rx.recv_timeout(Duration::from_millis(250)))
        });
    worker.join().unwrap();
    assert_eq!(
        received,
        Ok(InputRun::Refused(InputRefusal::Composer(
            Refusal::WriterContended
        )))
    );
    assert_eq!(fence.calls.load(Ordering::SeqCst), 0);
    assert!(!fence.persisted.load(Ordering::SeqCst));
    assert!(f.received(&name).is_empty());
}

struct LastPreResizeGate<'a> {
    fixture: &'a Fixture,
    pane: String,
    calls: AtomicUsize,
}

impl MutationGate for LastPreResizeGate<'_> {
    fn admit(&self, _: &str) -> Result<(), InputRefusal> {
        if self.calls.fetch_add(1, Ordering::SeqCst) + 1 == 3 {
            let output = Command::new(&self.fixture.real)
                .args([
                    "-L",
                    &self.fixture.socket,
                    "-f",
                    "/dev/null",
                    "resize-window",
                    "-t",
                    &self.pane,
                    "-x",
                    "80",
                    "-y",
                    "24",
                ])
                .output()
                .unwrap();
            assert!(output.status.success(), "last PRE resize: {output:?}");
            assert!(output.stderr.is_empty());
        }
        Ok(())
    }
}

#[test]
fn cg_b2b_last_pre_geometry_change_refuses_before_durable_fence() {
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    control(&f);
    let name = "last-pre-resize";
    let pane = renderer(&f, name, "own", &screen(""), &screen("own"), &screen(""));
    assert_eq!(
        f.tmux(&[
            "display-message",
            "-p",
            "-t",
            &pane,
            "#{pane_width} #{pane_height}"
        ]),
        "160 40"
    );
    let gate = LastPreResizeGate {
        fixture: &f,
        pane,
        calls: AtomicUsize::new(0),
    };
    let fence = Arc::new(Fence::default());
    let stopped = with_scope(Some(Arc::clone(&fence) as Arc<dyn SubmissionFence>), || {
        run(&InputTarget::legacy_tmux(name), &gate, "own", None)
    });
    assert_eq!(
        stopped,
        InputRun::Refused(InputRefusal::Composer(Refusal::GeometryChanged))
    );
    assert_eq!(gate.calls.load(Ordering::SeqCst), 3);
    assert_eq!(fence.calls.load(Ordering::SeqCst), 0);
    assert!(!fence.persisted.load(Ordering::SeqCst));
    assert!(f.received(name).is_empty());
    assert_eq!(
        std::fs::read(f.root.path().join("exec-conditions"))
            .unwrap()
            .len(),
        0
    );
}
