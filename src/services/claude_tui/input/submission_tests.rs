use super::*;
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
echo "$*" >> "$d/argv"
case "$2" in
display-message) echo 1 ;;
capture-pane) cat "$d/screen" ;;
load-buffer|paste-buffer|send-keys)
  n=$(cat "$d/sent" 2>/dev/null || echo 0); echo $((n + 1)) > "$d/sent"
  for last do :; done
  [ "$2" != load-buffer ] || cp "$last" "$d/body"
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

    /// The text of the last loaded paste buffer.
    pub(crate) fn body(&self) -> String {
        fs::read_to_string(self.dir.path().join("body")).unwrap_or_default()
    }

    /// The subcommands run against `session`'s pane, in order.
    pub(crate) fn pane_calls(&self, session: &str) -> Vec<String> {
        let target = format!("={session}:");
        events_in(&self.dir.path().join("argv"))
            .into_iter()
            .filter(|argv| argv.split(' ').any(|arg| arg == target))
            .filter_map(|argv| argv.split(' ').nth(1).map(str::to_string))
            .collect()
    }

    pub(crate) fn clear(&self) {
        for file in ["argv", "sends", "sent", "body"] {
            let _ = fs::remove_file(self.dir.path().join(file));
        }
    }
}

fn events_in(file: &Path) -> Vec<String> {
    let text = fs::read_to_string(file).unwrap_or_default();
    text.lines().map(str::to_string).collect()
}

fn request(session: &str) -> Request<'_> {
    Request {
        session,
        prompt: PROMPT,
        text: PROMPT,
        cancel: None,
        timing: &FAST,
    }
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
        ("own", view(prompt, !matches!(mode, "fresh" | "swallow"))),
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
    chunk = os.read(0, 65536)
    with (root/'received').open('ab') as out: out.write(chunk)
    pending += chunk
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
    if b'\x15' in pending:
        event('clear'); render('empty'); pending=pending.replace(b'\x15', b'')
    if b'\r' in pending:
        event('Enter')
        if mode == 'swallow' and not (root/'swallowed').exists():
            (root/'swallowed').write_text('1'); pending=b''; continue
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
/// A composer showing `body` at the prompt, as the frame renderer draws it.
pub(crate) fn screen(body: &str) -> String {
    format!(
        "{}\n❯\u{a0}{}\n{}\n  ⏵⏵ bypass permissions on (shift+tab to cycle)\n",
        "─".repeat(160),
        body.replace('\n', "\n  "),
        "─".repeat(160)
    )
}

/// A real pane that draws `before`, records the first payload of `frame`'s length, then draws
/// `own`; each Enter is counted and draws `after`. Returns the pane id.
pub(crate) fn frame_renderer(
    f: &Fixture,
    name: &str,
    frame: &str,
    before: &str,
    own: &str,
    after: &str,
) -> String {
    let root = f.root.path().join(name);
    fs::create_dir(&root).unwrap();
    for (file, content) in [
        ("expected", frame),
        ("before", before),
        ("own", own),
        ("after", after),
    ] {
        fs::write(root.join(file), content).unwrap();
    }
    let script = f.root.path().join("frame-renderer.py");
    fs::write(
        &script,
        r#"import os, pathlib, select, sys, tty
tty.setraw(0)
root = pathlib.Path(sys.argv[1])
def render(name):
    os.write(1, ('\x1b[?2004h\x1b[2J\x1b[1;1H' + (root / name).read_text().replace('\n', '\r\n')).encode())
render('before')
(root / 'ready').write_text('ready')
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
    wait("frame-renderer", || {
        f.tmux(&["capture-pane", "-p", "-t", &pane])
            .contains("bypass permissions")
    });
    pane
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
                    crate::services::claude_tui::input::send_prompt_with_readiness(
                        &name,
                        PROMPT,
                        PromptReadinessKind::FreshTurn,
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
        let request = request(&name);
        assert_eq!(run(&authority, true, &request), InputRun::Applied);
        wait("one Enter", || events(&root).len() == 3);
        assert_eq!(events(&root), ["C-s", "paste", "Enter"]);
        assert_eq!(fs::read_to_string(root.join("body")).unwrap(), PROMPT);
        assert!(!composer_lock::draft_guarded(&name));
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
        let request = request(&name);
        let got = run(&authority, allowed, &request);
        if expected.is_empty() {
            assert!(matches!(got, InputRun::Refused(_)));
        } else {
            assert_eq!(got, InputRun::Applied);
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
    let request = request("idle-fence");
    let authority = Authority {
        admitted: Cell::new(false),
    };
    assert!(matches!(
        run(&authority, true, &request),
        InputRun::Refused(_)
    ));
    authority.admitted.set(true);
    let got = with_scope(Some(Arc::new(Fence)), || run(&authority, true, &request));
    assert_eq!(got, refused(Refusal::SubmissionFence));
    assert!(events(&root).is_empty());
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
    let request = request("scripted-idle");
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
        let result = run(&authority, allowed, &request);
        if expected.is_empty() {
            assert!(matches!(result, InputRun::Refused(_)));
        } else {
            assert_eq!(result, InputRun::Applied);
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
    assert_eq!(run(&authority, true, &request), InputRun::Applied);
    assert_eq!(events(dir.path()), ["paste", "Enter"]);

    fs::write(dir.path().join("calls"), "").unwrap();
    fs::write(dir.path().join("events"), "").unwrap();
    let contended = composer_lock::with_composer_mutation_lock("scripted-idle", || {
        run(&authority, true, &request)
    });
    assert_eq!(contended, refused(Refusal::WriterContended));
    assert_eq!(fs::read_to_string(dir.path().join("calls")).unwrap(), "");
    assert!(events(dir.path()).is_empty());
    assert!(!composer_lock::draft_guarded("scripted-idle"));
}

/// Every Claude entry, managed or not, on a channel outside the stash list takes one pre-paste
/// read, a paste behind the draft and one Enter; nothing goes through the legacy transport.
#[test]
fn every_entry_on_an_unlisted_channel_takes_the_one_submission() {
    use crate::services::claude_tui::busy_inject::StashChannelGuard;
    use crate::services::claude_tui::host_input::{SpyGuard, SpyState};
    use crate::services::claude_tui::input as entry;
    use crate::services::claude_tui::submission_fence::IdleStashContext;
    const IDLE: &str = include_str!("../../../../tests/fixtures/tui_input/claude-2.1.293-idle.txt");
    const CHANNEL: u64 = 6_899_990;
    struct Attempt(String);
    impl SubmissionFence for Attempt {
        fn before_first_payload(&self) -> Result<(), PreSubmitPersistenceRefused> {
            panic!("the managed attempt claims through its idle fence")
        }
        fn refusal(&self) -> Option<PreSubmitPersistenceRefused> {
            None
        }
        fn idle_stash_context(&self) -> Option<IdleStashContext> {
            Some(IdleStashContext {
                channel: CHANNEL,
                target: self.0.clone(),
                available: true,
            })
        }
        fn idle_stash_admits(&self, target: &str) -> bool {
            target == self.0
        }
        fn claim_idle_stash(&self, _: &str) -> Result<(), PreSubmitPersistenceRefused> {
            Ok(())
        }
    }
    type Entry = fn(&str) -> Result<(), String>;
    let entries: [(&str, Entry); 4] = [
        ("fresh", |s| {
            entry::send_prompt_with_readiness(s, PROMPT, PromptReadinessKind::FreshTurn, None)
                .map_err(SubmissionError::into_legacy)
        }),
        ("followup", |s| {
            entry::send_prompt_with_readiness(s, PROMPT, PromptReadinessKind::Followup, None)
                .map_err(SubmissionError::into_legacy)
        }),
        ("warm", |s| {
            entry::send_warm_followup_owned(s, PROMPT, None).map_err(SubmissionError::into_legacy)
        }),
        ("steering", |s| entry::inject_steering_prompt(s, PROMPT)),
    ];
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _listed = StashChannelGuard::new(CHANNEL + 1);
    let tmux = ScriptedTmux::new(&view("draft", false));
    for managed in [false, true] {
        for (label, send) in entries {
            let session = format!("one-path-{label}-{managed}");
            tmux.clear();
            let spy = SpyGuard::install(SpyState {
                captures: std::iter::repeat_n(Some(IDLE.to_string()), 64).collect(),
                ..SpyState::default()
            });
            let result = if managed {
                with_scope(Some(Arc::new(Attempt(session.clone()))), || send(&session))
            } else {
                send(&session)
            };
            assert_eq!(result, Ok(()), "{label} managed={managed}");
            assert_eq!(
                tmux.sends(),
                ["load", "paste", "Enter"],
                "{label} {managed}"
            );
            assert_eq!(tmux.body(), PROMPT, "{label} managed={managed}");
            let pane = tmux.pane_calls(&session);
            let pre_paste = pane
                .iter()
                .position(|call| call == "display-message")
                .unwrap();
            assert_eq!(
                pane[pre_paste..],
                [
                    "display-message",
                    "capture-pane",
                    "paste-buffer",
                    "send-keys"
                ],
                "{label} managed={managed}: one pre-paste read, none after a key"
            );
            assert!(
                !spy.calls()
                    .iter()
                    .any(|call| ["literal:", "load:", "paste:", "keys:"]
                        .iter()
                        .any(|write| call.starts_with(write))),
                "{label} managed={managed}: legacy writes {:?}",
                spy.calls()
            );
            assert!(!composer_lock::draft_guarded(&session));
        }
    }
}

/// Codex keeps its own submission: a pane that turns busy after the payload gets no Enter, and the
/// typed text stays in the composer.
#[test]
fn cg_b3_codex_production_submission_keeps_busy_veto_after_payload() {
    use crate::services::codex_tui::{host_input as codex_host, input as codex_input};
    use crate::services::tui_input::actor::gate::{own_draft, submission_busy};
    use crate::services::tui_o::shadow::ShadowProvider;
    let f = Fixture::new();
    let before = include_str!("../../../../tests/fixtures/tui_input/codex-idle-dim.ansi");
    let own = "• Working (1s • esc to interrupt)\n  bypass permissions\n› own\n\n  gpt-5.5 xhigh · Fast off · /fixture/workspace · Context 100% left";
    let name = "b3-codex-busy";
    let before_screen = format!("  bypass permissions\n{before}");
    frame_renderer(&f, name, "own", &before_screen, own, own);
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
    let root = f.root.path().join(name);
    wait("codex payload", || root.join("payloads").exists());
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(fs::read_to_string(root.join("kind")).unwrap(), "literal");
    assert_eq!(f.received(name), b"own", "one literal payload and no Enter");
    let after = f.tmux(&["capture-pane", "-p", "-t", &format!("={name}:")]);
    assert!(own_draft(ShadowProvider::Codex, &after, "own", true));
    assert!(submission_busy(ShadowProvider::Codex, &after));
}

/// A fresh pane that swallows the first Enter keeps the pasted prompt and gets no second Enter:
/// the one submission reads nothing after a key, so it cannot notice this case.
#[test]
fn fresh_swallowed_enter_preserves_payload_and_never_retries() {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::claude_tui::submission_fence::IdleStashContext;
    struct Attempt(String);
    impl SubmissionFence for Attempt {
        fn before_first_payload(&self) -> Result<(), PreSubmitPersistenceRefused> {
            panic!("the managed attempt claims through its idle fence")
        }
        fn refusal(&self) -> Option<PreSubmitPersistenceRefused> {
            None
        }
        fn idle_stash_context(&self) -> Option<IdleStashContext> {
            Some(IdleStashContext {
                channel: 6_899_980,
                target: self.0.clone(),
                available: true,
            })
        }
        fn idle_stash_admits(&self, target: &str) -> bool {
            target == self.0
        }
        fn claim_idle_stash(&self, _: &str) -> Result<(), PreSubmitPersistenceRefused> {
            Ok(())
        }
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    for managed in [false, true] {
        let name = format!("fresh-swallow-{managed}");
        let root = renderer_with(
            &f,
            &name,
            "swallow",
            view("", false),
            "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
            PROMPT,
        );
        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
            &name,
            crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: root.join("transcript").display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: None,
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        let send = || {
            crate::services::claude_tui::input::send_prompt_with_readiness(
                &name,
                PROMPT,
                PromptReadinessKind::FreshTurn,
                None,
            )
        };
        let result = if managed {
            with_scope(Some(Arc::new(Attempt(name.clone()))), send)
        } else {
            send()
        };
        assert!(result.is_ok(), "managed={managed}: {result:?}");
        wait("swallowed-enter", || root.join("swallowed").exists());
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(events(&root), ["paste", "Enter"], "managed={managed}");
        assert_eq!(fs::read_to_string(root.join("body")).unwrap(), PROMPT);
        let sent = [b"\x1b[200~".as_slice(), PROMPT.as_bytes(), b"\x1b[201~\r"].concat();
        assert_eq!(
            fs::read(root.join("received")).unwrap(),
            sent,
            "managed={managed}: one paste and one Enter, no cleanup key"
        );
        let pane = f.tmux(&["capture-pane", "-p", "-t", &format!("={name}:")]);
        assert!(pane.contains("prepared question"), "{pane}");
        let transcript = fs::read_to_string(root.join("transcript")).unwrap();
        assert!(!transcript.contains("\"user\""), "{transcript}");
        assert!(!composer_lock::draft_guarded(&name));
    }
}
