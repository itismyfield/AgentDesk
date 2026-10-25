use super::*;
use crate::services::claude_tui::host_input::LegacyTmuxGate;
use crate::services::claude_tui::submission_fence::{
    PreSubmitPersistenceRefused, SubmissionFence, with_scope,
};
use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;
use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) const PROMPT: &str = "[User: test]\nprepared question";
pub(crate) const FAST: Timing = Timing {
    lock_retries: &[],
    settle: Duration::from_millis(30),
    confirm_window: Duration::from_secs(1),
    confirm_poll: Duration::from_millis(10),
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
pub(crate) fn wait(phase: &str, mut ready: impl FnMut() -> bool) {
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

/// A scripted `tmux` first on PATH for the managed idle path. Every capture shows `screen`;
/// each payload write (load, paste, key) is recorded with the durable managed phase it met.
pub(crate) struct ScriptedTmux {
    dir: tempfile::TempDir,
    _path: crate::config::TestEnvVarGuard,
}

impl ScriptedTmux {
    /// The caller holds the shared env lock, as a `TestRuntimeRootGuard` does.
    pub(crate) fn new(screen: &str) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = r#"#!/bin/sh
d='@D@'
case "$2" in
display-message) echo 1 ;;
capture-pane) cat "$d/screen" ;;
load-buffer|paste-buffer|send-keys)
  n=$(cat "$d/sent" 2>/dev/null || echo 0); echo $((n + 1)) > "$d/sent"
  for last do :; done
  case "$2" in load-buffer) key=load ;; paste-buffer) key=paste ;; *) key=$last ;; esac
  echo "$key" >> "$d/sends"
  if [ -f "$d/row" ]; then
    python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["managed_submission"]["phase"])' "$(cat "$d/row")" >> "$d/phases"
  fi
  [ "$(cat "$d/fail" 2>/dev/null)" != "$n" ] || exit 1
  if [ "$key" = Enter ] && [ -f "$d/on_enter" ]; then cat "$d/on_enter" >> "$(cat "$d/on_enter_target")"; fi ;;
esac
exit 0
"#
        .replace("@D@", &dir.path().display().to_string());
        let program = dir.path().join("tmux");
        fs::write(&program, script).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let path =
            crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(dir.path());
        let tmux = Self { dir, _path: path };
        tmux.show(screen);
        tmux
    }

    pub(crate) fn show(&self, screen: &str) {
        fs::write(self.dir.path().join("screen"), screen).unwrap();
    }

    /// Fails the `n`-th payload write, counted from 0.
    pub(crate) fn fail_send(&self, n: usize) {
        fs::write(self.dir.path().join("fail"), n.to_string()).unwrap();
    }

    /// Notes the managed phase of the row file `row` at every payload write.
    pub(crate) fn witness(&self, row: &Path) {
        fs::write(self.dir.path().join("row"), row.display().to_string()).unwrap();
    }

    /// Appends `line` to `file` when Enter arrives, as Claude records the submitted prompt.
    pub(crate) fn on_enter(&self, file: &Path, line: &str) {
        fs::write(self.dir.path().join("on_enter"), format!("{line}\n")).unwrap();
        let target = self.dir.path().join("on_enter_target");
        fs::write(target, file.display().to_string()).unwrap();
    }

    pub(crate) fn sends(&self) -> Vec<String> {
        events_in(&self.dir.path().join("sends"))
    }

    pub(crate) fn phases(&self) -> Vec<String> {
        events_in(&self.dir.path().join("phases"))
    }
}

fn events_in(file: &Path) -> Vec<String> {
    let text = fs::read_to_string(file).unwrap_or_default();
    text.lines().map(str::to_string).collect()
}

struct Authority {
    admitted: Cell<bool>,
}
impl StashAuthority for Authority {
    fn admits(&self, _: &str) -> bool {
        self.admitted.get()
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
            !f.tmux(&["capture-pane", "-p", "-t", &pane])
                .trim()
                .is_empty()
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

/// A fresh pane gets no key, and no attempt is claimed, before its input box is drawn; then the
/// managed attempt pastes and enters once, whether or not the channel may stash.
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
        claimed: AtomicUsize,
        outcomes: Mutex<Vec<InputRun>>,
    }
    impl SubmissionFence for Attempt {
        fn before_first_payload(&self) -> Result<(), PreSubmitPersistenceRefused> {
            panic!("the managed attempt claims through its idle fence")
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
            assert!(
                self.root.join("composer-ready").exists(),
                "payload preceded readiness"
            );
            self.claimed.fetch_add(1, Ordering::SeqCst);
            Ok(())
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
        for (i, fault) in ["unpainted", "partial", "banner"].into_iter().enumerate() {
            let channel = 6_899_950 + i as u64 + if listed { 0 } else { 10 };
            let _listed = StashChannelGuard::new(if listed { channel } else { channel + 100 });
            let name = format!("fresh-{listed}-{fault}");
            let before = match fault {
                "partial" => "❯ \n",
                "banner" => "Ready for input (type message + Enter)\n",
                _ => "Claude is starting…\n",
            };
            let root = renderer_with(
                &f,
                &name,
                "fresh",
                before.into(),
                "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
                PROMPT,
            );
            // The first plain capture, a readiness probe, lets the renderer draw its input box.
            let hook = format!(
                "capture=0; escapes=0\nfor arg in \"$@\"; do\n case \"$arg\" in capture-pane) capture=1;; -e) escapes=1;; esac\ndone\n\
                 if [ \"$capture\" = 1 ] && [ \"$escapes\" = 0 ] && [ ! -f {release} ]; then\n\
                  {real} -L {socket} -f /dev/null \"$@\"\n touch {release}\n\
                  polls=0\n while [ ! -f {ready} ]; do\n\
                   polls=$((polls + 1))\n [ \"$polls\" -lt 400 ] || exit 1\n sleep 0.01\n done\n exit 0\nfi\n",
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
                claimed: AtomicUsize::new(0),
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
            assert!(result.is_ok(), "listed={listed}, fault={fault}: {result:?}");
            assert_eq!(*attempt.outcomes.lock().unwrap(), [InputRun::Applied]);
            assert_eq!(attempt.claimed.load(Ordering::SeqCst), 1);
            wait("fresh-submit", || {
                events(&root).contains(&"Enter".to_string())
            });
            assert_eq!(events(&root), ["paste", "Enter"], "{name}");
            assert_eq!(fs::read_to_string(root.join("body")).unwrap(), PROMPT);
            fs::write(&shim, &original).unwrap();
        }
    }
}

#[test]
fn idle_sends_the_same_sequence_without_receipt_or_restore_verdicts() {
    let f = Fixture::new();
    for mode in ["normal", "never", "foreign", "mixed", "own_race"] {
        let name = format!("idle-{mode}");
        let root = renderer(&f, &name, mode);
        let authority = Authority {
            admitted: Cell::new(true),
        };
        let target = InputTarget::legacy_tmux(&name);
        let request = Request {
            target: &target,
            gate: &LegacyTmuxGate,
            prompt: PROMPT,
            cancel: None,
            timing: &FAST,
        };
        assert_eq!(
            run(Some(&authority), true, &request),
            Some(InputRun::Applied)
        );
        wait("one Enter", || events(&root).len() == 3);
        assert_eq!(events(&root), ["C-s", "paste", "Enter"]);
        assert_eq!(fs::read_to_string(root.join("body")).unwrap(), PROMPT);
        assert!(!composer_lock::draft_guarded(&name));
        assert_eq!(guarded_outcome::held(&name), None);
    }
}

#[test]
fn idle_menu_blocks_every_key_and_empty_or_unread_never_stashes() {
    let f = Fixture::new();
    for (name, before, allowed, expected) in [
        ("idle-empty", view("", false), true, vec!["paste", "Enter"]),
        (
            "idle-unread",
            "unread composer".into(),
            true,
            vec!["paste", "Enter"],
        ),
        ("idle-off", view("js", false), false, vec!["paste", "Enter"]),
        (
            "idle-modal",
            format!("Run this tool? allow / deny\n{}", view("js", false)),
            true,
            vec![],
        ),
    ] {
        let root = renderer_with(
            &f,
            name,
            "normal",
            before,
            "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
            PROMPT,
        );
        let authority = Authority {
            admitted: Cell::new(true),
        };
        let target = InputTarget::legacy_tmux(name);
        let request = Request {
            target: &target,
            gate: &LegacyTmuxGate,
            prompt: PROMPT,
            cancel: None,
            timing: &FAST,
        };
        let got = run(Some(&authority), allowed, &request);
        if expected.is_empty() {
            assert!(matches!(got, Some(InputRun::Refused(_))));
        } else {
            assert_eq!(got, Some(InputRun::Applied));
            wait("idle Enter", || events(&root).len() == expected.len());
        }
        assert_eq!(events(&root), expected);
    }
}

#[test]
fn idle_fence_and_revoked_authority_precede_every_key() {
    struct Fence;
    impl SubmissionFence for Fence {
        fn before_first_payload(&self) -> Result<(), PreSubmitPersistenceRefused> {
            Err(PreSubmitPersistenceRefused {
                reason: "fixture refusal".into(),
            })
        }
        fn refusal(&self) -> Option<PreSubmitPersistenceRefused> {
            None
        }
    }
    let f = Fixture::new();
    let root = renderer(&f, "idle-fence", "normal");
    let target = InputTarget::legacy_tmux("idle-fence");
    let request = Request {
        target: &target,
        gate: &LegacyTmuxGate,
        prompt: PROMPT,
        cancel: None,
        timing: &FAST,
    };
    let authority = Authority {
        admitted: Cell::new(false),
    };
    assert!(matches!(
        run(Some(&authority), true, &request),
        Some(InputRun::Refused(_))
    ));
    authority.admitted.set(true);
    let got = with_scope(Some(Arc::new(Fence)), || {
        run(Some(&authority), true, &request)
    });
    assert_eq!(got, Some(refused(Refusal::SubmissionFence)));
    assert!(events(&root).is_empty());
    assert_eq!(run(None, true, &request), None);
}

#[test]
fn scripted_idle_pre_capture_selects_stash_once_without_later_io_checks() {
    use std::os::unix::fs::PermissionsExt;
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let _path = crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(dir.path());
    let script = r#"#!/bin/sh
d='@D@'
echo "$2" >> "$d/calls"
case "$2" in
capture-pane) cat "$d/before" ;;
load-buffer) for last do :; done; cp "$last" "$d/body" ;;
paste-buffer) echo paste >> "$d/events" ;;
send-keys) for last do :; done; echo "$last" >> "$d/events" ;;
esac
"#
    .replace("@D@", &dir.path().display().to_string());
    let program = dir.path().join("tmux");
    fs::write(&program, script).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    let transcript = dir.path().join("transcript");
    fs::write(
        &transcript,
        "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
    )
    .unwrap();
    let target = InputTarget::legacy_tmux("scripted-idle");
    let request = Request {
        target: &target,
        gate: &LegacyTmuxGate,
        prompt: PROMPT,
        cancel: None,
        timing: &FAST,
    };
    let authority = Authority {
        admitted: Cell::new(true),
    };
    for (before, allowed, expected) in [
        (view("draft", false), true, vec!["C-s", "paste", "Enter"]),
        (view("", false), true, vec!["paste", "Enter"]),
        ("unread".into(), true, vec!["paste", "Enter"]),
        (view("draft", false), false, vec!["paste", "Enter"]),
        (
            format!("Run this tool? allow / deny\n{}", view("draft", false)),
            true,
            vec![],
        ),
    ] {
        fs::write(dir.path().join("before"), before).unwrap();
        fs::write(dir.path().join("calls"), "").unwrap();
        fs::write(dir.path().join("events"), "").unwrap();
        let result = run(Some(&authority), allowed, &request);
        if expected.is_empty() {
            assert!(matches!(result, Some(InputRun::Refused(_))));
        } else {
            assert_eq!(result, Some(InputRun::Applied));
        }
        assert_eq!(events(dir.path()), expected);
        assert_eq!(
            fs::read_to_string(dir.path().join("calls"))
                .unwrap()
                .lines()
                .filter(|call| *call == "capture-pane")
                .count(),
            1
        );
        assert!(!composer_lock::draft_guarded("scripted-idle"));
    }
    fs::write(dir.path().join("before"), view("", false)).unwrap();
    fs::write(dir.path().join("events"), "").unwrap();
    fs::write(&transcript, "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"go\"}}\n{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"working\"}]}}\n").unwrap();
    assert!(observe_claude_jsonl_turn_state(&transcript).is_busy());
    assert_eq!(
        run(Some(&authority), true, &request),
        Some(InputRun::Applied)
    );
    assert_eq!(events(dir.path()), ["paste", "Enter"]);

    fs::write(dir.path().join("calls"), "").unwrap();
    fs::write(dir.path().join("events"), "").unwrap();
    let contended = composer_lock::with_composer_mutation_lock("scripted-idle", || {
        run(Some(&authority), true, &request)
    });
    assert_eq!(contended, Some(refused(Refusal::WriterContended)));
    assert_eq!(fs::read_to_string(dir.path().join("calls")).unwrap(), "");
    assert!(events(dir.path()).is_empty());
    assert!(!composer_lock::draft_guarded("scripted-idle"));
}
