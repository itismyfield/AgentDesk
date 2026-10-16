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

pub(crate) fn screen(body: &str) -> String {
    format!(
        "{}\n❯\u{a0}{}\n{}\n  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
        "─".repeat(160),
        body.replace('\n', "\n  "),
        "─".repeat(160)
    )
}

pub(crate) fn install_shim(f: &Fixture) {
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
if [ "$conditional" = 1 ] && {{ [ "$enter" = 1 ] || [ "$payload" = 1 ]; }}; then printf x >> {conditions}; fi
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

pub(crate) fn renderer(
    f: &Fixture,
    name: &str,
    frame: &str,
    before: &str,
    own: &str,
    after: &str,
) -> String {
    renderer_at_row(f, name, frame, before, own, after, 1)
}

fn steering_renderer(
    f: &Fixture,
    name: &str,
    frame: &str,
    before: &str,
    own: &str,
    after: &str,
) -> String {
    renderer_at_row(f, name, frame, before, own, after, 33)
}

fn renderer_at_row(
    f: &Fixture,
    name: &str,
    frame: &str,
    before: &str,
    own: &str,
    after: &str,
    row: usize,
) -> String {
    let root = f.root.path().join(name);
    std::fs::create_dir(&root).unwrap();
    let row = row.to_string();
    for (file, content) in [
        ("row", row.as_str()),
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
        r#"import json, os, pathlib, select, sys, tty
tty.setraw(0)
root = pathlib.Path(sys.argv[1])
def render(name):
    text = (root / name).read_text()
    os.write(1, ('\x1b[?2004h\x1b[2J' + '\x1b[' + (root / 'row').read_text() + ';1H' + text.replace('\n', '\r\n')).encode())
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
        expected = (root / 'expected').read_bytes()
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
            mode_path = root / 'steering-mode'
            if mode_path.exists():
                mode = mode_path.read_text()
                body = (root / 'frame').read_text()
                receipt = root / 'transcript.jsonl'
                if mode == 'replace':
                    receipt.rename(root / 'retired.jsonl')
                    receipt.write_text('')
                if mode != 'missing':
                    record = {'type': 'queue-operation', 'operation': 'enqueue', 'sessionId': root.name, 'content': body if mode != 'foreign' else body + ' foreign'}
                    with receipt.open('a') as out:
                        out.write(json.dumps(record) + ('' if mode == 'partial' else '\n'))
                    if mode in ('removed', 'consumed'):
                        with receipt.open('a') as out:
                            out.write(json.dumps({'type': 'queue-operation', 'operation': 'remove', 'sessionId': root.name, 'content': body}) + '\n')
                if mode == 'tail':
                    with receipt.open('a') as out:
                        out.write(json.dumps({'type': 'queue-operation', 'operation': 'remove', 'sessionId': root.name, 'content': body}))
                attachment = {'type': 'attachment', 'sessionId': root.name, 'attachment': {'type': 'queued_command', 'commandMode': 'prompt', 'prompt': body}}
                if mode not in ('removed', 'tail'):
                    with receipt.open('a') as out:
                        out.write(json.dumps(attachment) + '\n')
                    with (root / 'attachments.jsonl').open('a') as out:
                        out.write(json.dumps(attachment) + '\n')
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
        assert_eq!(
            selected(&name, frame, None, capture_request(&name).unwrap()),
            None
        );
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

#[test]
fn cg_b2c_post_fence_cancel_reaches_warm_owner_as_typed_stop() {
    use crate::services::claude_tui::hosting::FollowupHost;
    use crate::services::claude_tui::hosting::{
        ClaudeTuiWarmFollowupOutcome, try_claude_tui_warm_followup,
    };
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let name = "owner-fence-cancel";
    renderer(&f, name, "own", &screen(""), &screen("own"), &screen(""));
    let transcript = f.root.path().join("owner.jsonl");
    std::fs::write(&transcript, "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n").unwrap();
    let token = Arc::new(CancelToken::new());
    let fence = Arc::new(Fence {
        cancel: Some(Arc::clone(&token)),
        ..Fence::default()
    });
    let (tx, rx) = std::sync::mpsc::channel();
    let (outcome, stopped) =
        with_scope(Some(Arc::clone(&fence) as Arc<dyn SubmissionFence>), || {
            test_support::with_request(name, Rc::new(LegacyTmuxGate), || {
                crate::services::claude_tui::guarded_outcome::warm_owned(|| {
                    try_claude_tui_warm_followup(
                        "owner-session".into(),
                        transcript.clone(),
                        transcript.display().to_string(),
                        true,
                        f.root.path(),
                        "own",
                        tx,
                        Some(token),
                        &FollowupHost::legacy_tmux(name),
                        None,
                    )
                })
            })
        });
    assert!(matches!(
        stopped,
        Some(InputRun::Indeterminate { confirmed: 0, .. })
    ));
    assert_eq!(fence.calls.load(Ordering::SeqCst), 1);
    assert!(f.received(name).is_empty());
    assert!(rx.try_recv().is_err());
    assert!(
        !matches!(
            outcome,
            crate::services::claude_tui::guarded_outcome::OwnedWarmOutcome::Legacy(
                ClaudeTuiWarmFollowupOutcome::Terminal(_)
            )
        ),
        "post-fence hold must retain its typed owner disposition"
    );
}

#[test]
fn cg_b2c_post_fence_hold_compatibility_error_is_not_a_clean_cancel() {
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let name = "mapping-fence-cancel";
    renderer(&f, name, "own", &screen(""), &screen("own"), &screen(""));
    let token = Arc::new(CancelToken::new());
    let fence = Arc::new(Fence {
        cancel: Some(Arc::clone(&token)),
        ..Fence::default()
    });
    let (result, stopped) = with_scope(Some(fence), || {
        test_support::with_request(name, Rc::new(LegacyTmuxGate), || {
            input::send_fresh_prompt(name, "own", Some(&token))
        })
    });
    assert!(matches!(stopped, Some(InputRun::Indeterminate { .. })));
    let error = result.unwrap_err();
    assert!(!input::is_prompt_ready_cancelled_error(&error), "{error}");
    assert!(
        error.starts_with("claude tui guarded submission held:"),
        "{error}"
    );
}

#[test]
fn cg_b2c_selected_followup_waits_for_busy_to_become_idle() {
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let name = "wait-busy-idle";
    let pane = renderer(&f, name, "own", &screen(""), &screen("own"), &screen(""));
    let busy = format!("{}\nThinking...\n  esc to interrupt\n", f.tmux(&["display-message", "-p", "-t", &pane, "#{session_id} #{window_id} #{pane_id} #{pane_width} #{pane_height} #{window-size} #{window_panes} #{pane_dead} #{pid} #{pane_pid} #{pane_in_mode} #{pane_input_off} #{session_attached} #{window_linked}"]));
    std::fs::write(f.root.path().join("busy-capture"), busy).unwrap();
    let shim = f.root.path().join("tmux");
    let old = std::fs::read_to_string(&shim).unwrap();
    let prefix = format!(
        "#!/bin/sh\nfor arg in \"$@\"; do\n if [ \"$arg\" = capture-pane ] && [ -f {} ]; then cat {}; exit 0; fi\ndone\n",
        quote(&f.root.path().join("busy-capture")),
        quote(&f.root.path().join("busy-capture"))
    );
    std::fs::write(&shim, prefix + old.strip_prefix("#!/bin/sh\n").unwrap()).unwrap();
    assert!(
        f.tmux(&["capture-pane", "-p", "-t", &pane])
            .contains("bypass permissions")
    );
    let path = f.root.path().join("busy-capture");
    let worker = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(180));
        std::fs::remove_file(path).unwrap();
    });
    let (result, stopped) = entry(&f, Entry::Followup, name, "own", Arc::new(Fence::default()));
    worker.join().unwrap();
    assert_eq!(stopped, Some(InputRun::Applied), "{result:?}");
    assert!(result.is_ok());
    assert_receipt(&f, name, "own", "literal", 1);
}

#[test]
fn cg_b2c_invalid_payload_action_is_not_silently_dropped() {
    let payload = [
        input::TuiInputAction::Literal("own".into()),
        input::TuiInputAction::Escape,
    ];
    assert_eq!(
        payload_frame(&payload),
        Err(Refusal::InvalidPrompt),
        "a non-text action must refuse the whole plan before the fence"
    );
}

#[test]
fn cg_b2c_unknown_selected_session_cannot_fall_back_to_legacy() {
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    renderer(
        &f,
        "wrong-selected-session",
        "own",
        &screen(""),
        &screen("own"),
        &screen(""),
    );
    let (result, _) =
        test_support::with_request("expected-selected-session", Rc::new(LegacyTmuxGate), || {
            input::send_fresh_prompt("wrong-selected-session", "own", Some(&CancelToken::new()))
        });
    assert!(
        result.is_err(),
        "a selected identity mismatch must hold before all Legacy effects"
    );
    assert!(f.received("wrong-selected-session").is_empty());
}

#[test]
fn cg_b2c_payload_hold_blocks_the_next_cleanup_under_the_same_lock() {
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let name = "held-next-cleanup";
    renderer(&f, name, "own", &screen(""), &screen("own"), &screen(""));
    let fence = Arc::new(Fence {
        arm: Some((f.root.path().join("exec-mode"), "drop_payload")),
        ..Fence::default()
    });
    let (result, stopped) = entry(&f, Entry::ProvenWarm, name, "own", Arc::clone(&fence));
    assert!(result.is_err());
    assert!(matches!(stopped, Some(InputRun::Indeterminate { .. })));
    assert_receipt(&f, name, "own", "literal", 0);
    let cleanups = AtomicUsize::new(0);
    let cleanup =
        input::with_composer_cleanup_lock(name, || cleanups.fetch_add(1, Ordering::SeqCst));
    assert!(
        cleanup.is_none(),
        "a post-fence draft cannot use stranded draft cleanup"
    );
    assert_eq!(cleanups.load(Ordering::SeqCst), 0);
    use crate::services::claude_tui::hosting::{FollowupHost, try_claude_tui_warm_followup};
    let transcript = f.root.path().join(format!("{name}.jsonl"));
    let captures = std::fs::read(f.root.path().join("exec-captures")).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let outcome = crate::services::claude_tui::guarded_outcome::warm_owned(|| {
        try_claude_tui_warm_followup(
            "next-warm".into(),
            transcript.clone(),
            transcript.display().to_string(),
            true,
            f.root.path(),
            "own",
            tx,
            None,
            &FollowupHost::legacy_tmux(name),
            None,
        )
    });
    assert!(
        matches!(
            outcome,
            crate::services::claude_tui::guarded_outcome::OwnedWarmOutcome::Stopped(
                InputRun::Indeterminate { .. }
            )
        ),
        "next warm recovery must remain typed"
    );
    assert!(rx.try_recv().is_err());
    assert_eq!(
        std::fs::read(f.root.path().join("exec-captures")).unwrap(),
        captures,
        "no warm recovery IO or resend"
    );
    let (again, _) = entry(&f, Entry::Fresh, name, "own", Arc::clone(&fence));
    assert!(again.is_err());
    assert_eq!(
        fence.calls.load(Ordering::SeqCst),
        1,
        "no fresh retry or second fence"
    );
    assert_receipt(&f, name, "own", "literal", 0);
}

#[test]
fn cg_b2c_selected_steering_is_an_explicit_hold() {
    let (result, stopped) =
        test_support::with_request("steering-held", Rc::new(LegacyTmuxGate), || {
            input::inject_steering_prompt("steering-held", "own")
        });
    assert!(matches!(
        stopped,
        Some(InputRun::Refused(InputRefusal::Composer(Refusal::NotReady)))
    ));
    assert!(
        result
            .unwrap_err()
            .starts_with("claude tui guarded submission held:")
    );
}

fn steering_source(f: &Fixture, name: &str, mode: &str) -> PathBuf {
    let root = f.root.path().join(name);
    let path = root.join("transcript.jsonl");
    std::fs::write(&path, "").unwrap();
    std::fs::write(root.join("steering-mode"), mode).unwrap();
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        name,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(name.to_string()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    assert_eq!(
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(name)
            .unwrap()
            .output_path,
        path.display().to_string()
    );
    path
}

fn steering_entry(
    fence: Arc<Fence>,
    name: &str,
    frame: &str,
) -> (
    crate::services::tui_steering::SteeringOutcome,
    Option<InputRun>,
) {
    steering_entry_with_gate(fence, name, frame, Rc::new(LegacyTmuxGate))
}

fn steering_entry_with_gate(
    fence: Arc<Fence>,
    name: &str,
    frame: &str,
    gate: Rc<dyn MutationGate>,
) -> (
    crate::services::tui_steering::SteeringOutcome,
    Option<InputRun>,
) {
    use crate::services::provider_hosting::{ProviderSessionDriver, ProviderSessionSelection};
    let selection = ProviderSessionSelection {
        provider_id: "claude".into(),
        requested_tui_hosting: true,
        driver: ProviderSessionDriver::TuiHosting,
        fallback_reason: None,
    };
    with_scope(Some(fence), || {
        test_support::with_steering_request(name, gate, || {
            crate::services::tui_steering::inject_with_bounded_retry(
                &crate::services::provider::ProviderKind::Claude,
                &selection,
                name,
                frame,
            )
        })
    })
}

#[test]
fn cg_b3_busy_steering_queues_literal_and_folded_frames_through_the_real_entry() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    control(&f);
    for (name, frame, body, kind) in [
        (
            "b3-literal",
            "steer  a 😀".to_string(),
            "steer  a 😀".to_string(),
            "literal",
        ),
        (
            "b3-folded",
            "😀".repeat(401),
            "[Pasted text #7]".to_string(),
            "paste",
        ),
        (
            "b3-consumed",
            "consumed".to_string(),
            "consumed".to_string(),
            "literal",
        ),
    ] {
        steering_renderer(
            &f,
            name,
            &frame,
            &format!("✳ Architecting…\n{}", screen("")),
            &format!("✳ Architecting…\n{}", screen(&body)),
            &format!("✳ Architecting…\nMessage queued\n{}", screen("")),
        );
        let source = steering_source(
            &f,
            name,
            if name == "b3-consumed" {
                "consumed"
            } else {
                "enqueue"
            },
        );
        let fence = Arc::new(Fence::default());
        let (outcome, run) = steering_entry(Arc::clone(&fence), name, &frame);
        assert_eq!(
            outcome,
            crate::services::tui_steering::SteeringOutcome::Injected,
            "typed={run:?}, actual payloads={}, actual enters={}",
            count(&f, name, "payloads"),
            count(&f, name, "enters"),
        );
        assert_eq!(run, Some(InputRun::Applied));
        assert_eq!(fence.calls.load(Ordering::SeqCst), 1);
        assert_receipt(&f, name, &frame, kind, 1);
        assert_eq!(
            std::fs::read(f.root.path().join("exec-enters"))
                .unwrap()
                .len(),
            1
        );
        let records: Vec<serde_json::Value> = std::fs::read_to_string(source)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records[0]["type"], "queue-operation");
        assert_eq!(records[0]["operation"], "enqueue");
        assert_eq!(records[0]["content"], frame);
        let attachment = records.iter().find(|r| r["type"] == "attachment").unwrap();
        assert_eq!(attachment["attachment"]["type"], "queued_command");
        assert_eq!(attachment["attachment"]["prompt"], frame);
    }
}

#[test]
fn cg_b3_ambiguous_queued_ui_retains_typed_hold_without_another_enter_or_retry() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    control(&f);
    for mode in [
        "missing", "foreign", "stale", "partial", "replace", "draft", "removed", "tail",
    ] {
        let name = format!("b3-{mode}");
        let after = screen(if mode == "draft" { "own" } else { "" });
        steering_renderer(
            &f,
            &name,
            "own",
            &format!("✳ Architecting…\n{}", screen("")),
            &format!("✳ Architecting…\n{}", screen("own")),
            &format!("✳ Architecting…\nMessage queued\n{after}"),
        );
        let source = steering_source(
            &f,
            &name,
            match mode {
                "stale" => "missing",
                "draft" => "enqueue",
                other => other,
            },
        );
        if mode == "stale" {
            std::fs::write(&source, format!("{}\n", serde_json::json!({
                "type":"queue-operation", "operation":"enqueue", "sessionId":name, "content":"own"
            }))).unwrap();
        }
        let fence = Arc::new(Fence::default());
        let (outcome, run) = steering_entry(Arc::clone(&fence), &name, "own");
        assert!(
            matches!(
                outcome,
                crate::services::tui_steering::SteeringOutcome::Failed(_)
            ),
            "{mode}: outcome={outcome:?}, typed={run:?}, payloads={}, enters={}",
            count(&f, &name, "payloads"),
            count(&f, &name, "enters")
        );
        assert!(
            matches!(run, Some(InputRun::Indeterminate { confirmed: 2, .. })),
            "{mode}: {run:?}"
        );
        assert_receipt(&f, &name, "own", "literal", 1);
        let captures = std::fs::read(f.root.path().join("exec-captures")).unwrap();
        let (again, stopped) =
            with_scope(Some(Arc::clone(&fence) as Arc<dyn SubmissionFence>), || {
                test_support::with_steering_request(&name, Rc::new(LegacyTmuxGate), || {
                    input::inject_steering_prompt(&name, "own")
                })
            });
        assert!(
            again
                .unwrap_err()
                .starts_with("claude tui guarded submission held:")
        );
        assert_eq!(stopped, run);
        let (idle, _) = entry(&f, Entry::Fresh, &name, "next", Arc::clone(&fence));
        assert!(idle.is_err());
        let cleanups = AtomicUsize::new(0);
        assert!(
            input::with_composer_cleanup_lock(&name, || {
                cleanups.fetch_add(1, Ordering::SeqCst);
            })
            .is_none()
        );
        assert_eq!(cleanups.load(Ordering::SeqCst), 0);
        let (tx, rx) = std::sync::mpsc::channel();
        let warm = crate::services::claude_tui::guarded_outcome::warm_owned(|| {
            crate::services::claude_tui::hosting::try_claude_tui_warm_followup(
                "next-warm".into(),
                source.clone(),
                source.display().to_string(),
                true,
                f.root.path(),
                "own",
                tx,
                None,
                &crate::services::claude_tui::hosting::FollowupHost::legacy_tmux(&name),
                None,
            )
        });
        assert!(matches!(
            warm,
            crate::services::claude_tui::guarded_outcome::OwnedWarmOutcome::Stopped(
                InputRun::Indeterminate { confirmed: 2, .. }
            )
        ));
        assert!(rx.try_recv().is_err());
        assert_eq!(fence.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            std::fs::read(f.root.path().join("exec-captures")).unwrap(),
            captures
        );
        assert_eq!(
            std::fs::read(f.root.path().join("exec-enters"))
                .unwrap()
                .len(),
            1
        );
        assert_receipt(&f, &name, "own", "literal", 1);
    }
}

#[test]
fn cg_b3_busy_to_idle_keeps_the_composer_permit_and_one_enter() {
    struct LateGate {
        calls: std::cell::Cell<usize>,
        blocked: Arc<AtomicBool>,
    }
    impl MutationGate for LateGate {
        fn admit(&self, session: &str) -> Result<(), InputRefusal> {
            LegacyTmuxGate.admit(session)?;
            self.calls.set(self.calls.get() + 1);
            if self.calls.get() == 5 {
                let session = session.to_string();
                self.blocked.store(std::thread::spawn(move || {
                    (0..2).all(|_| crate::services::claude_tui::composer_lock::try_with_composer_mutation_lock(
                        &session, || (),
                    ).is_none())
                }).join().unwrap(), Ordering::SeqCst);
            }
            Ok(())
        }
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    control(&f);
    let name = "b3-busy-to-idle";
    steering_renderer(
        &f,
        name,
        "own",
        &format!("✳ Architecting…\n{}", screen("")),
        &screen("own"),
        &screen(""),
    );
    steering_source(&f, name, "enqueue");
    let blocked = Arc::new(AtomicBool::new(false));
    let fence = Arc::new(Fence {
        lock_probe: Some((name.to_string(), Arc::clone(&blocked))),
        ..Fence::default()
    });
    let late_blocked = Arc::new(AtomicBool::new(false));
    let (outcome, typed) = steering_entry_with_gate(
        fence,
        name,
        "own",
        Rc::new(LateGate {
            calls: std::cell::Cell::new(0),
            blocked: Arc::clone(&late_blocked),
        }),
    );
    assert_eq!(
        outcome,
        crate::services::tui_steering::SteeringOutcome::Injected
    );
    assert_eq!(typed, Some(InputRun::Applied));
    assert!(blocked.load(Ordering::SeqCst));
    assert!(
        late_blocked.load(Ordering::SeqCst),
        "other writers remain excluded after payload"
    );
    assert_receipt(&f, name, "own", "literal", 1);
}

#[test]
fn cg_b3_codex_production_submission_keeps_busy_veto_after_payload() {
    let f = Fixture::new();
    install_shim(&f);
    let before = include_str!("../../../../../tests/fixtures/tui_input/codex-idle-dim.ansi");
    let own = "• Working (1s • esc to interrupt)\n  bypass permissions\n› own\n\n  gpt-5.5 xhigh · Fast off · /fixture/workspace · Context 100% left";
    let name = "b3-codex-busy";
    renderer(
        &f,
        name,
        "own",
        &format!("  bypass permissions\n{before}"),
        own,
        own,
    );
    use crate::services::codex_tui::{host_input as codex_host, input as codex_input};
    let plan = [
        codex_input::TuiInputAction::Literal("own".into()),
        codex_input::TuiInputAction::Enter,
    ];
    let run = codex_host::run_prompt_submission_legacy(name, &plan, Some(before), None);
    assert!(matches!(
        run.run,
        codex_host::InputRun::Indeterminate { confirmed: 1, .. }
    ));
    assert!(run.composer_mutated);
    assert!(!run.enter_attempted);
    assert_receipt(&f, name, "own", "literal", 0);
    let after = f.tmux(&["capture-pane", "-p", "-t", &format!("={name}:")]);
    assert!(crate::services::tui_input::actor::gate::own_draft(
        ShadowProvider::Codex,
        &after,
        "own",
        true
    ));
    assert!(submission_busy(ShadowProvider::Codex, &after));
}

#[test]
fn cg_b2c_warm_owner_preserves_cancellation_after_payload_and_after_enter() {
    use crate::services::claude_tui::hosting::{
        ClaudeTuiWarmFollowupOutcome, FollowupHost, try_claude_tui_warm_followup,
    };
    let f = Fixture::new();
    install_shim(&f);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let mut terminals = 0;
    for after_enter in [false, true] {
        let name = format!("owner-cancel-after-enter-{after_enter}");
        renderer(&f, &name, "own", &screen(""), &screen("own"), &screen(""));
        let transcript = f.root.path().join(format!("{name}.jsonl"));
        std::fs::write(
            &transcript,
            "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
        )
        .unwrap();
        let token = Arc::new(CancelToken::new());
        let gate = Rc::new(CancellingGate {
            at: if after_enter { usize::MAX } else { 5 },
            calls: AtomicUsize::new(0),
            token: Arc::clone(&token),
        });
        let worker = if after_enter {
            let token = Arc::clone(&token);
            let path = f.root.path().join(&name).join("enters");
            Some(std::thread::spawn(move || {
                wait(|| path.exists());
                token.cancelled.store(true, Ordering::SeqCst);
            }))
        } else {
            None
        };
        let fence = Arc::new(Fence::default());
        let (tx, rx) = std::sync::mpsc::channel();
        let (outcome, stopped) =
            with_scope(Some(Arc::clone(&fence) as Arc<dyn SubmissionFence>), || {
                test_support::with_request(&name, gate, || {
                    crate::services::claude_tui::guarded_outcome::warm_owned(|| {
                        try_claude_tui_warm_followup(
                            "owner-session".into(),
                            transcript.clone(),
                            transcript.display().to_string(),
                            true,
                            f.root.path(),
                            "own",
                            tx,
                            Some(token),
                            &FollowupHost::legacy_tmux(&name),
                            None,
                        )
                    })
                })
            });
        if let Some(worker) = worker {
            worker.join().unwrap();
        }
        let confirmed = if after_enter { 2 } else { 1 };
        assert!(
            matches!(stopped, Some(InputRun::Indeterminate { confirmed: actual, .. }) if actual == confirmed),
            "{stopped:?}"
        );
        assert_receipt(&f, &name, "own", "literal", usize::from(after_enter));
        assert_eq!(fence.calls.load(Ordering::SeqCst), 1);
        assert!(rx.try_recv().is_err());
        terminals += usize::from(matches!(
            outcome,
            crate::services::claude_tui::guarded_outcome::OwnedWarmOutcome::Legacy(
                ClaudeTuiWarmFollowupOutcome::Terminal(_)
            )
        ));
    }
    assert_eq!(
        terminals, 0,
        "cancellations after payload and Enter must stay typed"
    );
}

pub(crate) fn d1_fixture_config(
    f: &Fixture,
    ids: &[u64],
) -> (Vec<crate::config::TestEnvVarGuard>, crate::config::Config) {
    use crate::config::TestEnvVarGuard as Env;
    use std::os::unix::fs::PermissionsExt;
    let path = f.root.path().join("cg-d1-fixture.yaml");
    let bindings = ids.iter().enumerate().map(|(index, id)| format!("  - id: cg-d1-{index}\n    name: cg-d1-{index}\n    channels:\n      claude: {{id: '{id}', runtime: tui}}\n")).collect::<String>();
    let data = serde_json::to_string(&f.root.path().join("data")).unwrap();
    let yaml = format!(
        "server: {{}}\ndata: {{dir: {data}}}\ncluster: {{instance_id: cg-d1}}\nproviders:\n  claude: {{runtime: tui}}\nagents:\n{bindings}session_hosts: {{claude_guard_channels: {ids:?}}}\n"
    );
    std::fs::write(&path, &yaml).unwrap();
    let fake = f.root.path().join("claude-fixture");
    std::fs::write(&fake, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let env = vec![
        Env::set_path_after_shared_test_env_lock("AGENTDESK_CONFIG", &path),
        Env::set_path_after_shared_test_env_lock("CLAUDE_CONFIG_DIR", f.root.path()),
        Env::set_path_after_shared_test_env_lock("AGENTDESK_CLAUDE_PATH", &fake),
    ];
    (env, serde_yaml::from_str(&yaml).unwrap())
}

pub(crate) fn d1_execution_proof(
    name: &str,
    channel: u64,
) -> crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation {
    use crate::services::tmux_common as tc;
    use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;
    std::fs::create_dir_all(
        Path::new(&tc::session_temp_path(name, "proof"))
            .parent()
            .unwrap(),
    )
    .unwrap();
    tc::write_tmux_owner_marker(name).unwrap();
    tc::host_marker::record_tmux_host_marker(name);
    tc::write_tmux_runtime_kind_marker(
        name,
        crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
    )
    .unwrap();
    let sid = uuid::Uuid::new_v4().to_string();
    let prepared =
        PreparedIncarnation::prepare("claude", name, Some(channel), Some(&sid), false).unwrap();
    prepared
        .finish_spawn(crate::services::discord::stamp_spawn_markers(
            name,
            Some(&prepared),
        ))
        .unwrap();
    prepared
}

fn d1_submit(
    name: &str,
    frame: &str,
    entry: Entry,
    fence: Arc<Fence>,
) -> (Result<(), String>, Option<InputRun>) {
    crate::services::claude_tui::submit_canary::with_channel(41, || {
        with_scope(Some(fence), || {
            crate::services::claude_tui::guarded_outcome::capture(|| {
                let token = CancelToken::new();
                match entry {
                    Entry::Fresh => input::send_fresh_prompt(name, frame, Some(&token)),
                    Entry::Followup => input::send_followup_prompt(name, frame, Some(&token)),
                    Entry::ProvenWarm => {
                        let root = tempfile::tempdir().unwrap();
                        let transcript = root.path().join("session.jsonl");
                        std::fs::write(&transcript, "{\"type\":\"result\"}\n").unwrap();
                        crate::services::claude_tui::guarded_outcome::send_followup_prompt_or_idle_transcript(name, frame, Some(&token), &transcript)
                    }
                }
            })
        })
    })
}

#[test]
fn cg_d1_selected_input_boundaries_and_other_legacy_use_actual_tmux() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    install_shim(&f);
    let (_env, _) = d1_fixture_config(&f, &[41]);
    let _boot =
        crate::config::session_hosts::force_with_claude_guard_for_test(Some("cg-d1"), &[], &[41]);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    for (index, frame) in [
        "한글  원문".to_string(),
        " 한글  원문".to_string(),
        "x".repeat(532),
        "x".repeat(533),
        "x".repeat(799),
        "x".repeat(800),
        "😀".repeat(401),
        "a\n  b\nc".into(),
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("d1-boundary-{index}");
        let folded =
            crate::services::tui_input::actor::gate::claude_guarded::claude_should_fold(&frame);
        let own = if folded {
            let lf = frame.bytes().filter(|byte| *byte == b'\n').count();
            if lf == 0 {
                "[Pasted text #7]".to_string()
            } else {
                format!("[Pasted text #7 +{lf} lines]")
            }
        } else {
            frame.clone()
        };
        renderer(&f, &name, &frame, &screen(""), &screen(&own), &screen(""));
        d1_execution_proof(&name, 41);
        let entry = match index % 3 {
            0 => Entry::Fresh,
            1 => Entry::Followup,
            _ => Entry::ProvenWarm,
        };
        let fence = Arc::new(Fence::default());
        let (result, observed) = d1_submit(&name, &frame, entry, Arc::clone(&fence));
        assert_eq!(
            observed,
            Some(InputRun::Applied),
            "selected input must consume the guarded result: {result:?}"
        );
        assert_eq!(result, Ok(()));
        assert_eq!(fence.calls.load(Ordering::SeqCst), 1);
        assert_receipt(
            &f,
            &name,
            &frame,
            if folded || frame.contains('\n') {
                "paste"
            } else {
                "literal"
            },
            1,
        );
        assert_eq!(
            std::fs::read(f.root.path().join("exec-enters"))
                .unwrap()
                .len(),
            1
        );
    }
    let name = "d1-other-normal";
    renderer(&f, name, "own", &screen(""), &screen("own"), &screen(""));
    let (result, observed) = crate::services::claude_tui::submit_canary::with_channel(44, || {
        crate::services::claude_tui::guarded_outcome::capture(|| {
            input::send_fresh_prompt(name, "own", None)
        })
    });
    assert_eq!(result, Ok(()));
    assert_eq!(observed, None);
    assert_receipt(&f, name, "own", "literal", 1);
    assert!(
        std::fs::read(f.root.path().join("exec-conditions"))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn cg_d1_selected_unknown_mismatch_and_host_conflict_never_downgrade() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    install_shim(&f);
    let (_env, _) = d1_fixture_config(&f, &[41]);
    let _boot =
        crate::config::session_hosts::force_with_claude_guard_for_test(Some("cg-d1"), &[], &[41]);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    for (index, case) in [
        "missing",
        "channel",
        "tmux",
        "root",
        "host",
        "nonce",
        "unknown-host",
        "conflict-host",
        "provider",
        "profile",
    ]
    .iter()
    .enumerate()
    {
        let name = format!("d1-identity-{index}");
        renderer(&f, &name, "own", &screen(""), &screen("own"), &screen(""));
        if *case != "missing" {
            let proof = d1_execution_proof(&name, 41);
            let mut ctx = serde_json::to_value(&proof.context).unwrap();
            match *case {
                "channel" => ctx["channel_id"] = 44.into(),
                "tmux" => ctx["tmux_session"] = "another".into(),
                "root" => ctx["owner_runtime_root"] = "/another".into(),
                "host" => ctx["host"] = "another".into(),
                "provider" => ctx["provider"] = "codex".into(),
                "profile" => ctx["launch_mode"] = "unsupported".into(),
                "nonce" => {
                    std::fs::write(
                        crate::services::tmux_common::session_temp_path(&name, "spawn_nonce"),
                        "f".repeat(32),
                    )
                    .unwrap();
                }
                "unknown-host" => {
                    std::fs::remove_file(crate::services::tmux_common::session_temp_path(
                        &name,
                        "host_kind",
                    ))
                    .unwrap();
                }
                "conflict-host" => {
                    std::fs::write(
                        crate::services::tmux_common::session_temp_path(&name, "host_kind"),
                        "herdr",
                    )
                    .unwrap();
                }
                _ => unreachable!(),
            }
            std::fs::write(&proof.path, serde_json::to_vec(&ctx).unwrap()).unwrap();
        }
        let fence = Arc::new(Fence::default());
        let (result, _) = d1_submit(&name, "own", Entry::Fresh, Arc::clone(&fence));
        assert!(
            result
                .as_ref()
                .is_err_and(|error| error.starts_with("claude tui guarded submission held:")),
            "{case}: selected evidence must hold, not become Legacy: {result:?}"
        );
        assert!(
            f.received(&name).is_empty(),
            "{case}: no payload or fallback"
        );
        assert_eq!(
            fence.calls.load(Ordering::SeqCst),
            0,
            "{case}: before durable fence"
        );
    }
    let name = "d1-unscoped-selected";
    renderer(&f, name, "own", &screen(""), &screen("own"), &screen(""));
    d1_execution_proof(name, 41);
    let fence = Arc::new(Fence::default());
    let (result, observed) =
        with_scope(Some(Arc::clone(&fence) as Arc<dyn SubmissionFence>), || {
            guarded_outcome::capture(|| {
                crate::services::claude_tui::input::send_fresh_prompt(name, "own", None)
            })
        });
    assert!(
        result.is_err() && matches!(observed, Some(InputRun::Refused(InputRefusal::Unknown))),
        "missing creator context must not infer a channel from execution files: {result:?} {observed:?}"
    );
    assert_eq!(fence.calls.load(Ordering::SeqCst), 0);
    assert!(f.received(name).is_empty());
}

#[test]
fn cg_d1_production_hold_survives_off_and_blocks_warm_fresh_resend() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    install_shim(&f);
    let (_env, _) = d1_fixture_config(&f, &[41]);
    let boot =
        crate::config::session_hosts::force_with_claude_guard_for_test(Some("cg-d1"), &[], &[41]);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let name = "d1-post-payload-off";
    renderer(&f, name, "own", &screen(""), &screen("own"), &screen(""));
    d1_execution_proof(name, 41);
    let fence = Arc::new(Fence {
        arm: Some((f.root.path().join("exec-mode"), "drop_payload")),
        ..Fence::default()
    });
    let (result, observed) = d1_submit(name, "own", Entry::ProvenWarm, Arc::clone(&fence));
    assert!(
        matches!(observed, Some(InputRun::Indeterminate { confirmed: 0, .. })),
        "{result:?} {observed:?}"
    );
    assert_receipt(&f, name, "own", "literal", 0);
    drop(boot);
    let _off = crate::config::session_hosts::force_with_claude_guard_for_test(None, &[], &[]);
    let original = f.received(name);
    let cleans = AtomicUsize::new(0);
    assert!(
        input::with_composer_cleanup_lock(name, || cleans.fetch_add(1, Ordering::SeqCst)).is_none()
    );
    assert_eq!(cleans.load(Ordering::SeqCst), 0);
    for entry in [Entry::Fresh, Entry::Followup, Entry::ProvenWarm] {
        let (again, observed) = d1_submit(name, "own", entry, Arc::clone(&fence));
        assert!(again.is_err());
        assert!(observed.is_none_or(|run| matches!(run, InputRun::Indeterminate { .. })));
    }
    assert_eq!(fence.calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.received(name), original);
    crate::services::claude_tui::guarded_outcome::forget_test_hold(name);
}

#[test]
fn cg_d1_empty_boot_preserves_legacy_actions_and_steering_selected_is_hold() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    install_shim(&f);
    let (_env, _) = d1_fixture_config(&f, &[41]);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let off = crate::config::session_hosts::force_with_claude_guard_for_test(None, &[], &[]);
    for (index, channel) in [Some(41), Some(44), None].into_iter().enumerate() {
        let name = format!("d1-off-{index}");
        renderer(&f, &name, "own", &screen(""), &screen("own"), &screen(""));
        let run = || {
            crate::services::claude_tui::guarded_outcome::capture(|| {
                input::send_fresh_prompt(&name, "own", None)
            })
        };
        let (result, observed) = channel.map_or_else(run, |channel| {
            crate::services::claude_tui::submit_canary::with_channel(channel, run)
        });
        assert_eq!(result, Ok(()));
        assert_eq!(observed, None);
        assert_receipt(&f, &name, "own", "literal", 1);
        assert!(
            std::fs::read(f.root.path().join("exec-conditions"))
                .unwrap()
                .is_empty()
        );
    }
    drop(off);
    let _on =
        crate::config::session_hosts::force_with_claude_guard_for_test(Some("cg-d1"), &[], &[41]);
    let name = "d1-first-profile-steer";
    renderer(&f, name, "own", &screen(""), &screen("own"), &screen(""));
    d1_execution_proof(name, 41);
    let (result, observed) = crate::services::claude_tui::submit_canary::with_channel(41, || {
        crate::services::claude_tui::guarded_outcome::capture(|| {
            input::inject_steering_prompt(name, "own")
        })
    });
    assert!(result.is_err());
    assert!(matches!(
        observed,
        Some(InputRun::Refused(InputRefusal::Composer(Refusal::NotReady)))
    ));
    assert!(f.received(name).is_empty());
}

#[test]
fn cg_d1_fixed_request_rejects_nonce_swap_at_the_payload_fence() {
    struct Swap {
        marker: PathBuf,
        nonce: String,
        calls: AtomicUsize,
    }
    impl SubmissionFence for Swap {
        fn before_first_payload(&self) -> Result<(), PreSubmitPersistenceRefused> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::fs::write(&self.marker, &self.nonce).unwrap();
            Ok(())
        }
        fn refusal(&self) -> Option<PreSubmitPersistenceRefused> {
            None
        }
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    install_shim(&f);
    let (_env, _) = d1_fixture_config(&f, &[41]);
    let _boot =
        crate::config::session_hosts::force_with_claude_guard_for_test(Some("cg-d1"), &[], &[41]);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let name = "d1-nonce-boundary";
    renderer(&f, name, "own", &screen(""), &screen("own"), &screen(""));
    let original = d1_execution_proof(name, 41);
    let replacement = d1_execution_proof(name, 41);
    let marker = PathBuf::from(crate::services::tmux_common::session_temp_path(
        name,
        "spawn_nonce",
    ));
    std::fs::write(&marker, &original.context.execution_nonce).unwrap();
    let fence = Arc::new(Swap {
        marker,
        nonce: replacement.context.execution_nonce,
        calls: AtomicUsize::new(0),
    });
    let (result, observed) = crate::services::claude_tui::submit_canary::with_channel(41, || {
        with_scope(Some(fence.clone()), || {
            crate::services::claude_tui::guarded_outcome::capture(|| {
                input::send_fresh_prompt(name, "own", None)
            })
        })
    });
    assert!(
        matches!(
            observed,
            Some(InputRun::Indeterminate {
                confirmed: 0,
                cause: StopCause::Refused(InputRefusal::IdentityMismatch)
            })
        ),
        "{result:?} {observed:?}"
    );
    assert_eq!(fence.calls.load(Ordering::SeqCst), 1);
    assert!(f.received(name).is_empty());
    assert!(
        std::fs::read(f.root.path().join("exec-enters"))
            .unwrap()
            .is_empty()
    );
    crate::services::claude_tui::guarded_outcome::forget_test_hold(name);
}

#[test]
fn cg_d1_runtime_env_change_does_not_change_the_selected_attempt() {
    struct Change;
    impl SubmissionFence for Change {
        fn before_first_payload(&self) -> Result<(), PreSubmitPersistenceRefused> {
            unsafe {
                std::env::set_var("AGENTDESK_CLAUDE_SUBMIT_GUARD_CHANNEL_IDS", "");
            }
            Ok(())
        }
        fn refusal(&self) -> Option<PreSubmitPersistenceRefused> {
            None
        }
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    install_shim(&f);
    let (_env, _) = d1_fixture_config(&f, &[41]);
    let _input = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_CLAUDE_SUBMIT_GUARD_CHANNEL_IDS",
        std::ffi::OsStr::new("41"),
    );
    let _boot =
        crate::config::session_hosts::force_with_claude_guard_for_test(Some("cg-d1"), &[], &[41]);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let name = "d1-env-boundary";
    renderer(&f, name, "own", &screen(""), &screen("own"), &screen(""));
    d1_execution_proof(name, 41);
    let (result, observed) = crate::services::claude_tui::submit_canary::with_channel(41, || {
        with_scope(Some(Arc::new(Change)), || {
            crate::services::claude_tui::guarded_outcome::capture(|| {
                input::send_fresh_prompt(name, "own", None)
            })
        })
    });
    assert_eq!(result, Ok(()));
    assert_eq!(observed, Some(InputRun::Applied));
    assert_receipt(&f, name, "own", "literal", 1);
}

#[test]
fn cg_d1_readiness_wait_keeps_the_original_execution_request() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    install_shim(&f);
    let (_env, _) = d1_fixture_config(&f, &[41]);
    let _boot =
        crate::config::session_hosts::force_with_claude_guard_for_test(Some("cg-d1"), &[], &[41]);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let name = "d1-readiness-identity";
    renderer(&f, name, "own", &screen(""), &screen("own"), &screen(""));
    let original = d1_execution_proof(name, 41);
    let replacement = d1_execution_proof(name, 41);
    let marker = PathBuf::from(crate::services::tmux_common::session_temp_path(
        name,
        "spawn_nonce",
    ));
    std::fs::write(&marker, &original.context.execution_nonce).unwrap();
    let busy = f.root.path().join("d1-busy-capture");
    let seen = f.root.path().join("d1-readiness-seen");
    std::fs::write(&busy, "Thinking...\n  esc to interrupt\n").unwrap();
    let shim = f.root.path().join("tmux");
    let script = std::fs::read_to_string(&shim).unwrap();
    let prefix = format!(
        "#!/bin/sh\nfor arg in \"$@\"; do\n if [ \"$arg\" = capture-pane ] && [ -f {} ]; then touch {}; cat {}; exit 0; fi\ndone\n",
        quote(&busy),
        quote(&seen),
        quote(&busy)
    );
    std::fs::write(&shim, prefix + script.strip_prefix("#!/bin/sh\n").unwrap()).unwrap();
    let worker = std::thread::spawn(move || {
        wait(|| seen.exists());
        std::fs::write(marker, &replacement.context.execution_nonce).unwrap();
        std::fs::remove_file(busy).unwrap();
    });
    let fence = Arc::new(Fence::default());
    let (result, observed) = d1_submit(name, "own", Entry::Followup, Arc::clone(&fence));
    worker.join().unwrap();
    assert!(
        matches!(
            observed,
            Some(InputRun::Refused(InputRefusal::IdentityMismatch))
        ),
        "request captured before readiness must reject the replacement: {result:?} {observed:?}"
    );
    assert_eq!(fence.calls.load(Ordering::SeqCst), 0);
    assert!(f.received(name).is_empty());
}

#[test]
fn cg_d1_production_own_draft_requires_exact_spaces() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    install_shim(&f);
    let (_env, _) = d1_fixture_config(&f, &[41]);
    let _boot =
        crate::config::session_hosts::force_with_claude_guard_for_test(Some("cg-d1"), &[], &[41]);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let frame = "a  b";
    for (index, composer) in [frame, "a b", "a\tb", "a\nb"].iter().enumerate() {
        let name = format!("d1-exact-spaces-{index}");
        renderer(
            &f,
            &name,
            frame,
            &screen(""),
            &screen(composer),
            &screen(""),
        );
        d1_execution_proof(&name, 41);
        let fence = Arc::new(Fence::default());
        let (result, observed) = d1_submit(&name, frame, Entry::Fresh, Arc::clone(&fence));
        if index == 0 {
            assert_eq!(result, Ok(()));
            assert_eq!(observed, Some(InputRun::Applied));
        } else {
            assert!(
                matches!(
                    observed,
                    Some(InputRun::Indeterminate {
                        confirmed: 1,
                        cause: StopCause::Refused(InputRefusal::Composer(Refusal::OwnDraft))
                    })
                ),
                "foreign composer must hold after actual payload: {result:?} {observed:?}"
            );
        }
        assert_eq!(fence.calls.load(Ordering::SeqCst), 1);
        assert_receipt(&f, &name, frame, "literal", usize::from(index == 0));
        guarded_outcome::forget_test_hold(&name);
    }
}

#[test]
fn cg_d1_real_warm_terminal_does_not_recreate_or_resend_a_guarded_hold() {
    use crate::services::claude_tui::hosting::{
        ClaudeTuiWarmFollowupOutcome, FollowupHost, try_claude_tui_warm_followup,
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    install_shim(&f);
    let (_env, _) = d1_fixture_config(&f, &[41]);
    let _boot =
        crate::config::session_hosts::force_with_claude_guard_for_test(Some("cg-d1"), &[], &[41]);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let name = "d1-warm-terminal";
    renderer(&f, name, "own", &screen(""), &screen("own"), &screen(""));
    let proof = d1_execution_proof(name, 41);
    let transcript = f.root.path().join("d1-warm.jsonl");
    std::fs::write(&transcript, "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n").unwrap();
    let fence = Arc::new(Fence {
        arm: Some((f.root.path().join("exec-mode"), "drop_payload")),
        ..Fence::default()
    });
    let outcome = with_scope(Some(Arc::clone(&fence) as Arc<dyn SubmissionFence>), || {
        crate::services::claude_tui::submit_canary::with_channel(41, || {
            guarded_outcome::warm_owned(|| {
                let (tx, _) = std::sync::mpsc::channel();
                let raw = try_claude_tui_warm_followup(
                    proof.context.expected_native_session_id.clone().unwrap(),
                    transcript.clone(),
                    transcript.display().to_string(),
                    true,
                    f.root.path(),
                    "own",
                    tx,
                    None,
                    &FollowupHost::legacy_tmux(name),
                    Some(41),
                );
                assert!(
                    matches!(raw, ClaudeTuiWarmFollowupOutcome::Terminal(Err(_))),
                    "a guarded warm stop must not request fresh recreation"
                );
                raw
            })
        })
    });
    assert!(matches!(
        outcome,
        guarded_outcome::OwnedWarmOutcome::Stopped(InputRun::Indeterminate { confirmed: 0, .. })
    ));
    assert_receipt(&f, name, "own", "literal", 0);
    assert_eq!(fence.calls.load(Ordering::SeqCst), 1);
    let prior = f.received(name);
    for entry in [Entry::Fresh, Entry::Followup, Entry::ProvenWarm] {
        assert!(d1_submit(name, "own", entry, Arc::clone(&fence)).0.is_err());
    }
    assert_eq!(f.received(name), prior);
    assert_eq!(fence.calls.load(Ordering::SeqCst), 1);
    guarded_outcome::forget_test_hold(name);
}

#[test]
fn cg_d1_missing_host_identity_is_hold_and_known_host_is_a_live_control() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    install_shim(&f);
    let (_env, _) = d1_fixture_config(&f, &[41]);
    let config = f.root.path().join("cg-d1-fixture.yaml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, text.replace("cluster: {instance_id: cg-d1}\n", "")).unwrap();
    let _instance =
        crate::config::TestEnvVarGuard::capture_after_shared_test_env_lock("AGENTDESK_INSTANCE_ID");
    unsafe { std::env::remove_var("AGENTDESK_INSTANCE_ID") };
    let _boot = crate::config::session_hosts::force_with_claude_guard_for_test(None, &[], &[41]);
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    for known in [false, true] {
        let name = if known {
            "d1-host-known"
        } else {
            "d1-host-unknown"
        };
        if known {
            unsafe { std::env::set_var("AGENTDESK_INSTANCE_ID", "cg-d1") };
        }
        renderer(&f, name, "own", &screen(""), &screen("own"), &screen(""));
        let proof = d1_execution_proof(name, 41);
        assert_eq!(proof.context.host.is_some(), known);
        let fence = Arc::new(Fence::default());
        let (result, observed) = d1_submit(name, "own", Entry::Fresh, Arc::clone(&fence));
        if known {
            assert_eq!(result, Ok(()));
            assert_eq!(observed, Some(InputRun::Applied));
            assert_receipt(&f, name, "own", "literal", 1);
        } else {
            assert!(
                matches!(observed, Some(InputRun::Refused(_))),
                "unknown host cannot become a proven execution: {result:?} {observed:?}"
            );
            assert!(f.received(name).is_empty());
        }
        assert_eq!(fence.calls.load(Ordering::SeqCst), usize::from(known));
    }
}
