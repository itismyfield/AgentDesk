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
        ("empty", view("", false)),
        ("stashed", view("", true)),
        ("own", view(prompt, mode != "fresh")),
        (
            "after",
            view(
                if matches!(mode, "never" | "fresh") {
                    ""
                } else {
                    "js"
                },
                mode == "never",
            ),
        ),
        ("transcript", transcript.into()),
        ("mode", mode.into()),
    ] {
        fs::write(root.join(file), bytes).unwrap();
    }
    let script = f.root.path().join("idle-renderer.py");
    fs::write(&script, r#"import json, os, pathlib, sys, time, tty
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
if mode == 'fresh':
    while not (root/'release').exists(): time.sleep(0.01)
    render('empty')
    (root/'composer-ready').write_text('ready')
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
        if (root/'draft-bytes').exists(): (root/'draft-bytes').rename(root/'draft-stash')
        if (root/'draft-chip').exists(): (root/'draft-chip').rename(root/'draft-stash-chip')
        event('C-s'); pending=pending[1:]; render('stashed')
    if pending.startswith(b'\x1b[200~') and b'\x1b[201~' in pending:
        body, pending = pending[6:].split(b'\x1b[201~',1)
        (root/'body').write_bytes(body); event('paste')
        render('own' if mode != 'own_race' else 'before')
    if b'\r' in pending:
        event('Enter')
        body=(root/'body').read_text()
        if mode == 'foreign': record('foreign before expected')
        if mode == 'wrapped': record('\n\n<pasted_content id="a17f">\n'+body+'\n</pasted_content id="a17f">\n')
        elif mode == 'mixed': record([{'type':'text','text':body},{'type':'tool_result','content':'x'}])
        else: record(body)
        if (root/'draft-stash').exists(): (root/'draft-stash').rename(root/'draft-bytes')
        if (root/'draft-stash-chip').exists(): (root/'draft-stash-chip').rename(root/'draft-chip')
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
    if mode != "fresh" {
        wait("renderer-capture", || {
            f.tmux(&["capture-pane", "-p", "-t", &pane]).contains("❯")
        });
    }
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
fn fresh_pane_waits_before_one_submission_on_listed_and_unlisted_channels() {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::claude_tui::busy_inject::StashChannelGuard;
    use crate::services::claude_tui::submission_fence::IdleStashContext;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Attempt {
        session: String,
        channel: u64,
        root: PathBuf,
        person: bool,
        fenced: AtomicUsize,
        outcomes: Mutex<Vec<InputRun>>,
    }
    impl SubmissionFence for Attempt {
        fn before_first_payload(&self) -> Result<(), PreSubmitPersistenceRefused> {
            assert!(
                self.person || self.root.join("composer-ready").exists(),
                "payload preceded readiness"
            );
            self.fenced.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn refusal(&self) -> Option<PreSubmitPersistenceRefused> {
            None
        }
        fn idle_stash_context(&self) -> Option<IdleStashContext> {
            Some(IdleStashContext {
                channel: self.channel,
                target: self.session.clone(),
                available: true,
            })
        }
        fn idle_stash_admits(&self, target: &str) -> bool {
            target == self.session
        }
        fn claim_idle_stash(&self, _: &str) -> Result<(), PreSubmitPersistenceRefused> {
            if self.person {
                return self.before_first_payload();
            }
            panic!("no person draft was observed, so no stash may claim this attempt")
        }
    }
    impl guarded_outcome::Owner for Attempt {
        fn started(&self) {}
        fn finished(&self, run: &InputRun) {
            self.outcomes.lock().unwrap().push(run.clone());
        }
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    let shim = f.root.path().join("tmux");
    let original = fs::read_to_string(&shim).unwrap();
    for listed in [true, false] {
        for (i, fault) in [
            "scrollback",
            "capture",
            "pin",
            "unpainted",
            "partial",
            "banner",
            "person_no_viewport",
            "continuation",
            "viewport_read",
            "viewport_resize",
        ]
        .into_iter()
        .enumerate()
        {
            if !listed
                && matches!(
                    fault,
                    "continuation"
                        | "scrollback"
                        | "viewport_read"
                        | "viewport_resize"
                        | "person_no_viewport"
                )
            {
                continue;
            }
            let channel = 6_899_950 + i as u64 + if listed { 0 } else { 10 };
            let _listed = StashChannelGuard::new(if listed { channel } else { channel + 100 });
            let name = format!("fresh-{listed}-{fault}");
            let root = renderer_with(
                &f,
                &name,
                if fault == "person_no_viewport" {
                    "normal"
                } else if fault == "continuation" {
                    "person"
                } else {
                    "fresh"
                },
                match fault {
                    "scrollback" | "viewport_read" | "viewport_resize" => format!(
                        "{}{}\x1b[2J\x1b[HClaude is starting…\n",
                        view("[User: previous] old prompt", false),
                        (0..55)
                            .map(|i| format!("startup output {i}\n"))
                            .collect::<String>()
                    ),
                    "partial" => "❯ \n".into(),
                    "banner" => "Ready for input (type message + Enter)\n".into(),
                    "continuation" => "❯ human text\n  ❯\n".into(),
                    "person_no_viewport" => view("js", false),
                    _ => "Claude is starting…\n".into(),
                },
                "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
                PROMPT,
            );
            if matches!(fault, "scrollback" | "viewport_read" | "viewport_resize") {
                let target = format!("={name}:");
                let history = f.tmux(&["capture-pane", "-p", "-J", "-t", &target, "-S", "-80"]);
                let viewport = f.tmux(&["capture-pane", "-p", "-t", &target]);
                assert!(history.contains("❯\u{a0}[User: previous] old prompt"));
                assert_eq!(
                    busy_inject::composer_owner(&history),
                    busy_inject::ComposerOwner::Unread
                );
                assert!(viewport.contains("Claude is starting…"));
                assert!(
                    !viewport.contains('❯'),
                    "old composer is outside the viewport"
                );
            }
            if matches!(fault, "unpainted" | "person_no_viewport") {
                let target = format!("={name}:");
                let history = f.tmux(&["capture-pane", "-p", "-J", "-t", &target, "-S", "-80"]);
                if fault == "unpainted" {
                    assert!(!history.contains('❯'));
                    assert!(!f.tmux(&["capture-pane", "-p", "-t", &target]).contains('❯'));
                } else {
                    assert_eq!(
                        busy_inject::composer_owner(
                            &SubmissionTmux
                                .pin_guarded(&name)
                                .unwrap()
                                .capture_draft()
                                .unwrap()
                        ),
                        busy_inject::ComposerOwner::Person
                    );
                }
            }
            let hook = format!(
                "pin=0; capture=0; viewport=0\nfor arg in \"$@\"; do\n case \"$arg\" in *'#{{window_panes}}'*) pin=1;; capture-pane) capture=1;; 0) viewport=1;; esac\ndone\n\
                 if [ \"$pin\" = 1 ] && [ ! -f {failed} ]; then\n\
                  if [ '{fault}' = pin ] || {{ [ '{fault}' = capture ] && [ \"$capture\" = 1 ]; }}; then touch {failed}; exit 1; fi\nfi\n\
                 if [ \"$pin\" = 1 ] && [ \"$capture\" = 1 ] && [ \"$viewport\" = 1 ]; then\n touch {viewport_seen}\n\
                  case '{fault}' in\n\
                   unpainted|person_no_viewport) exit 1;;\n\
                   viewport_read) touch {failed}; exit 1;;\n\
                   viewport_resize) {real} -L {socket} -f /dev/null \"$@\"\n\
                    {real} -L {socket} resize-window -t ={name}: -x 161 -y 40\n touch {failed}\n exit 0;;\n\
                  esac\nfi\n\
                 if [ \"$capture\" = 1 ] && [ \"$pin\" = 0 ] && [ ! -f {release} ]; then\n\
                  {real} -L {socket} -f /dev/null \"$@\"\n touch {release}\n\
                  readiness_polls=0\n while [ ! -f {ready} ]; do\n\
                   readiness_polls=$((readiness_polls + 1))\n [ \"$readiness_polls\" -lt 400 ] || exit 1\n sleep 0.01\n done\n exit 0\nfi\n",
                failed = quote(&root.join("read-failed")),
                viewport_seen = quote(&root.join("viewport-seen")),
                release = quote(&root.join("release")),
                ready = quote(&root.join("composer-ready")),
                real = quote(&f.real),
                socket = f.socket,
            );
            fs::write(
                &shim,
                original.replacen("#!/bin/sh\n", &format!("#!/bin/sh\n{hook}"), 1),
            )
            .unwrap();
            let transcript = root.join("transcript");
            let attempt = Arc::new(Attempt {
                session: name.clone(),
                channel,
                root: root.clone(),
                person: fault == "person_no_viewport",
                fenced: AtomicUsize::new(0),
                outcomes: Mutex::new(Vec::new()),
            });
            crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
                &name,
                crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                    runtime_kind: RuntimeHandoffKind::ClaudeTui,
                    output_path: transcript.display().to_string(),
                    relay_output_path: None,
                    input_fifo_path: None,
                    session_id: None,
                    last_offset: 0,
                    relay_last_offset: None,
                },
            );
            let token = Arc::new(CancelToken::new());
            let (stop, stopped) = std::sync::mpsc::channel();
            let timer_token = token.clone();
            let timer = std::thread::spawn(move || {
                if stopped.recv_timeout(Duration::from_secs(4)).is_err() {
                    timer_token
                        .cancelled
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
            });
            let result = guarded_outcome::with_owner(Some(attempt.clone()), || {
                with_scope(Some(attempt.clone()), || {
                    input::send_prompt_with_readiness(
                        &name,
                        PROMPT,
                        input::PromptReadinessKind::FreshTurn,
                        Some(&token),
                    )
                })
            });
            let _ = stop.send(());
            timer.join().unwrap();
            eprintln!(
                "fresh entry listed={listed} fault={fault} effects={:?} result={result:?}",
                events(&root)
            );
            assert_eq!(
                root.join("viewport-seen").exists(),
                listed
                    && matches!(
                        fault,
                        "scrollback" | "continuation" | "viewport_read" | "viewport_resize"
                    ),
                "listed={listed}, fault={fault}: unexpected viewport observation"
            );
            if matches!(fault, "continuation" | "viewport_read" | "viewport_resize") {
                assert!(
                    matches!(result, Err(guarded_outcome::SubmissionError::Held(ref hold))
                        if hold.reason == "composer_not_ready" && hold.may_requeue()),
                    "not-ready stays a typed pre-payload hold: {result:?}"
                );
                assert!(events(&root).is_empty());
                assert_eq!(attempt.fenced.load(Ordering::SeqCst), 0);
                if fault == "continuation" {
                    let visible = f.tmux(&["capture-pane", "-p", "-t", &format!("={name}:")]);
                    assert!(visible.contains("❯ human text"));
                    assert!(visible.lines().any(|row| row.starts_with("  ❯")));
                } else {
                    assert!(root.join("read-failed").exists());
                    assert!(!root.join("release").exists());
                }
                continue;
            }
            assert!(result.is_ok(), "listed={listed}, fault={fault}: {result:?}");
            assert!(
                attempt
                    .outcomes
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|run| *run == InputRun::Applied),
                "fresh pane must not report a held owner"
            );
            assert_eq!(attempt.fenced.load(Ordering::SeqCst), 1);
            wait("fresh-submit", || {
                events(&root).contains(&"Enter".to_string())
            });
            let expected = if attempt.person {
                vec!["C-s", "paste", "Enter"]
            } else {
                vec!["paste", "Enter"]
            };
            assert_eq!(events(&root), expected, "{name}");
            assert_eq!(fs::read_to_string(root.join("body")).unwrap(), PROMPT);
            let users: Vec<serde_json::Value> = fs::read_to_string(&transcript)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                .filter(|record| record["type"] == "user")
                .map(|record| record["message"]["content"].clone())
                .collect();
            assert_eq!(users, [serde_json::Value::String(PROMPT.into())]);
            if attempt.person {
                let restored = f.tmux(&["capture-pane", "-p", "-t", &format!("={name}:")]);
                assert!(restored.contains("❯\u{a0}js"));
                assert_eq!(
                    busy_inject::composer_owner(
                        &SubmissionTmux
                            .pin_guarded(&name)
                            .unwrap()
                            .capture_draft()
                            .unwrap()
                    ),
                    busy_inject::ComposerOwner::Person
                );
            } else {
                assert!(
                    root.join("composer-ready").exists(),
                    "payload preceded readiness"
                );
            }
            fs::write(&shim, &original).unwrap();
        }
    }
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
fn managed_admission_rejects_busy_stash_modal_and_agentdesk_without_fence() {
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
    for kind in ["busy", "stash", "modal", "agentdesk"] {
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
        let prompt = PROMPT;
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

fn effort_view(text: &str, stashed: bool) -> String {
    let status = if stashed {
        "◉ xhigh · /effort · › stashed"
    } else {
        "◉ xhigh · /effort"
    };
    let shown = view(text, stashed);
    let (_, composer) = shown.split_once('\n').unwrap();
    format!("{status:>158}\n{composer}")
}

#[test]
fn managed_multiline_stash_restores_plain_and_chip_bytes_with_measured_effort_status() {
    let f = Fixture::new();
    let prompt = format!(
        "[User: test]\n{}",
        (0..14)
            .map(|n| format!("outgoing {n}: {}", "긴 메시지 ".repeat(12)))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let chip = format!("[Pasted text #2 +{} lines]", prompt.matches('\n').count());
    for human_chip in [false, true] {
        let name = format!("multiline-effort-{human_chip}");
        let original = if human_chip {
            "원래 붙여넣기\n보존할 둘째 줄  \n마지막 줄"
        } else {
            "사람 초안\n둘째 줄  \n마지막 줄"
        };
        let human_shown = if human_chip {
            "[Pasted text #1 +2 lines]"
        } else {
            original
        };
        let root = renderer_with(
            &f,
            &name,
            "wrapped",
            effort_view(human_shown, false),
            "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
            &prompt,
        );
        fs::write(root.join("draft-bytes"), original.as_bytes()).unwrap();
        if human_chip {
            fs::write(root.join("draft-chip"), human_shown).unwrap();
        }
        fs::write(root.join("stashed"), effort_view("", true)).unwrap();
        fs::write(
            root.join("own"),
            effort_view(&chip, true).replace(
                "  ⏵⏵ bypass permissions on (shift+tab to cycle)",
                "  paste again to expand",
            ),
        )
        .unwrap();
        fs::write(root.join("after"), effort_view(human_shown, false)).unwrap();
        let authority = Authority {
            identity: SubmissionTmux.pin_guarded(&name).unwrap().identity(),
            admitted: Cell::new(true),
        };
        let target = InputTarget::legacy_tmux(&name);
        let transcript = root.join("transcript");
        let request = Request {
            target: &target,
            gate: &LegacyTmuxGate,
            prompt: &prompt,
            transcript: &transcript,
            cancel: None,
            timing: &FAST,
        };
        let got = run(Some(&authority), true, &request).unwrap();
        assert_eq!(got.delivery, Delivery::Observed, "human_chip={human_chip}");
        assert_eq!(got.draft, DraftState::RestoredObserved);
        assert_eq!(events(&root), ["C-s", "paste", "Enter"]);
        assert_eq!(fs::read(root.join("body")).unwrap(), prompt.as_bytes());
        assert_eq!(
            fs::read(root.join("draft-bytes")).unwrap(),
            original.as_bytes()
        );
        assert!(!root.join("draft-stash").exists());
        assert!(!root.join("draft-stash-chip").exists());
        assert_eq!(
            fs::read_to_string(root.join("draft-chip")).ok(),
            human_chip.then(|| human_shown.to_string())
        );
        assert!(composer_lock::draft_guarded(&name));
        let after = SubmissionTmux
            .pin_guarded(&name)
            .unwrap()
            .capture_draft()
            .unwrap();
        assert_eq!(
            screen::stashable(&after, PROMPT, Some((160, 40))),
            Some(
                human_shown
                    .split('\n')
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            )
        );
    }
}

#[test]
fn managed_idle_literal_modal_text_submits_but_identical_chrome_blocks_all_keys() {
    let f = Fixture::new();
    let literal = "Enter to confirm · esc to cancel";
    let prompt = format!("[User: test]\n{literal}");
    for in_chrome in [false, true] {
        let name = format!("idle-literal-modal-{in_chrome}");
        let before = if in_chrome {
            view("js", false).replace(
                "  ⏵⏵ bypass permissions on",
                &format!("  {literal}\n  ⏵⏵ bypass permissions on"),
            )
        } else {
            view("js", false)
        };
        assert_eq!(
            judge_pane(ShadowProvider::Claude, &before) == PaneVerdict::Modal,
            in_chrome
        );
        assert_ne!(
            judge_pane(ShadowProvider::Claude, &view(&prompt, true)),
            PaneVerdict::Modal,
            "literal composer content is not an idle modal"
        );
        let root = renderer_with(
            &f,
            &name,
            "normal",
            before,
            "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
            &prompt,
        );
        let authority = Authority {
            identity: SubmissionTmux.pin_guarded(&name).unwrap().identity(),
            admitted: Cell::new(true),
        };
        let target = InputTarget::legacy_tmux(&name);
        let transcript = root.join("transcript");
        let request = Request {
            target: &target,
            gate: &LegacyTmuxGate,
            prompt: &prompt,
            transcript: &transcript,
            cancel: None,
            timing: &FAST,
        };
        let got = run(Some(&authority), true, &request).unwrap();
        if in_chrome {
            assert_eq!(got.delivery, Delivery::NotAttempted);
            assert_eq!(got.draft, DraftState::Unchanged);
            assert!(events(&root).is_empty());
        } else {
            assert_eq!(got.delivery, Delivery::Observed);
            assert_eq!(got.draft, DraftState::RestoredObserved);
            assert_eq!(events(&root), ["C-s", "paste", "Enter"]);
            assert_eq!(fs::read_to_string(root.join("body")).unwrap(), prompt);
        }
    }
}

#[test]
fn managed_idle_stash_modal_after_stash_or_paste_never_enters() {
    let f = Fixture::new();
    let modal =
        "  Do you want to proceed?\n  1. Allow\n  2. Deny\n  Enter to confirm · esc to cancel\n";
    for phase in ["stashed", "own"] {
        let name = format!("idle-modal-{phase}");
        let root = renderer(&f, &name, "normal");
        let state = root.join(phase);
        let shown = fs::read_to_string(&state).unwrap();
        let overlay = shown.replace(
            "  ⏵⏵ bypass permissions on",
            &format!("{modal}  ⏵⏵ bypass permissions on"),
        );
        fs::write(&state, &overlay).unwrap();
        assert_eq!(
            judge_pane(ShadowProvider::Claude, &overlay),
            PaneVerdict::Modal,
            "the modal fixture must reach the idle modal check"
        );
        assert!(
            if phase == "stashed" {
                screen::stashed(&overlay)
            } else {
                screen::owns(&overlay, PROMPT)
            },
            "the modal must veto a readable, otherwise owned composer"
        );
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
        assert_eq!(
            got.delivery,
            Delivery::AttemptedUnconfirmed,
            "phase={phase}"
        );
        assert_eq!(got.draft, DraftState::Unknown);
        assert_eq!(
            events(&root),
            if phase == "stashed" {
                vec!["C-s"]
            } else {
                vec!["C-s", "paste"]
            },
            "phase={phase}"
        );
        assert!(composer_lock::draft_guarded(&name));
    }
}

#[test]
fn managed_idle_receipt_accepts_only_exact_measured_paste_wrappers() {
    use receipt::Receipt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("transcript");
    let wrapped =
        |body: &str| format!("<pasted_content id=\"a17f\">\n{body}\n</pasted_content id=\"a17f\">");
    for (case, body, accepted) in [
        ("plain", PROMPT.to_string(), true),
        ("wrapped", wrapped(PROMPT), true),
        (
            "outer-separators",
            format!("\n\n{}\n", wrapped(PROMPT)),
            true,
        ),
        ("prefix", format!("foreign {}", wrapped(PROMPT)), false),
        ("suffix", format!("{} foreign", wrapped(PROMPT)), false),
        (
            "inner-foreign",
            wrapped(&format!("{PROMPT} foreign")),
            false,
        ),
        (
            "wrong-close",
            wrapped(PROMPT).replace(
                "</pasted_content id=\"a17f\">",
                "</pasted_content id=\"b17f\">",
            ),
            false,
        ),
        ("short-token", wrapped(PROMPT).replace("a17f", "a17"), false),
        (
            "upper-token",
            wrapped(PROMPT).replace("a17f", "A17F"),
            false,
        ),
        ("partial-outer", format!("\n{}\n", wrapped(PROMPT)), false),
    ] {
        for blocks in [false, true] {
            fs::write(&path, "{\"type\":\"system\"}\n").unwrap();
            let receipt = Receipt::before(&path).unwrap();
            let content = if blocks {
                serde_json::json!([{"type":"text", "text":body}])
            } else {
                serde_json::json!(body)
            };
            append(
                &path,
                serde_json::json!({"type":"user","message":{"content":content}}),
            );
            assert_eq!(
                receipt.scan(PROMPT),
                Some(accepted),
                "case={case}, blocks={blocks}"
            );
        }
    }
}

#[test]
fn owner_idle_stash_commits_payload_after_c_s_and_restores_exact_draft() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    let name = "owner-stash-window";
    let original = "사람 초안\n둘째 줄  \n마지막 줄";
    let prompt = format!(
        "[User: test]\n{}",
        (0..14)
            .map(|i| format!("outgoing {i}: {}", "긴 메시지 ".repeat(12)))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let root = renderer_with(
        &f,
        name,
        "wrapped",
        effort_view(original, false),
        "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
        &prompt,
    );
    fs::write(root.join("draft-bytes"), original.as_bytes()).unwrap();
    fs::write(root.join("stashed"), effort_view("", true)).unwrap();
    let chip = format!("[Pasted text #2 +{} lines]", prompt.matches('\n').count());
    fs::write(
        root.join("own"),
        effort_view(&chip, true).replace(
            "  ⏵⏵ bypass permissions on (shift+tab to cycle)",
            "  paste again to expand",
        ),
    )
    .unwrap();
    fs::write(root.join("after"), effort_view(original, false)).unwrap();
    let nonce = "00112233445566778899aabbccddee13";
    let owner_root = crate::services::tmux_common::current_tmux_owner_marker();
    let context = serde_json::json!({"schema":1,"provider":"claude","created_at":chrono::Utc::now(),"execution_nonce":nonce,"tmux_session":name,"channel_id":727422,"owner_runtime_root":owner_root,"host":"epoch-test","expected_native_session_id":null,"launch_mode":"fresh","provider_root":null});
    let _prepared =
        crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(
            serde_json::from_value(context).unwrap(),
        )
        .unwrap();
    let marker = crate::services::tmux_common::session_temp_path(name, "spawn_nonce");
    fs::create_dir_all(marker.parent().unwrap()).unwrap();
    fs::write(marker, nonce).unwrap();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let registry = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
    let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(727422));
    runtime.block_on(mailbox.v2_cold_fixture(f.root.path().join("stash-owner.json"),serde_json::json!({"nonce":"stash-window","provider":"claude","target":{"host":"tmux","owner":name,"root":owner_root,"execution_nonce":nonce},"source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}}),prompt.clone(),727422)).unwrap();
    let check = root.clone();
    let ticks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = ticks.clone();
    let clock = Arc::new(move || {
        assert_eq!(
            events(&check),
            ["C-s"],
            "payload deadline starts after stash, before paste"
        );
        observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        90_000
    });
    let input_root = root.clone();
    let sent = prompt.clone();
    let (_, outcome) = runtime
        .block_on(mailbox.v2_cold_bootstrap(1000, clock, move |_| {
            assert!(
                crate::services::tui_input::human_frame::history::v2::scoped_source().is_some()
            );
            let authority = Authority {
                identity: SubmissionTmux.pin_guarded(name).unwrap().identity(),
                admitted: Cell::new(true),
            };
            let target = InputTarget::legacy_tmux(name);
            let request = Request {
                target: &target,
                gate: &LegacyTmuxGate,
                prompt: &sent,
                transcript: &input_root.join("transcript"),
                cancel: None,
                timing: &FAST,
            };
            let report = run(Some(&authority), true, &request).ok_or("stash unavailable")?;
            Ok((report.delivery, report.draft))
        }))
        .unwrap();
    assert_eq!(
        outcome.unwrap(),
        (Delivery::Observed, DraftState::RestoredObserved)
    );
    assert_eq!(events(&root), ["C-s", "paste", "Enter"]);
    assert_eq!(fs::read(root.join("body")).unwrap(), prompt.as_bytes());
    assert_eq!(
        fs::read(root.join("draft-bytes")).unwrap(),
        original.as_bytes()
    );
    assert_eq!(ticks.load(std::sync::atomic::Ordering::SeqCst), 1);
    let snapshot = runtime
        .block_on(mailbox.v2_request(|s| s.as_ref().unwrap().json()))
        .unwrap();
    assert_eq!(
        snapshot["head_injection"]["confirmation_deadline_ms"],
        120_000
    );
    assert_eq!(snapshot["head_injection"]["phase"], "may_have_sent");
}
