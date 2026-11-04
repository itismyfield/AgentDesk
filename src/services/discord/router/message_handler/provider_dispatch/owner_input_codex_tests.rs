//! The owner's busy Codex tmux branch from `provider_dispatch::execute`: a real pane drawing a
//! captured busy Codex screen under a traced tmux, the owner's actor, and the pane's own record.
use super::tests::{AUTHOR, NAME, PROMPT, Rig, owned};
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::claude_tui::input::submission::tests::wait;
use crate::services::discord::queue_io::input_owner::admission::TurnBind;
use crate::services::provider::ProviderKind;
use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;
use std::fs;
use std::path::Path;

/// The open model turn of the captured rollout's first eleven records.
const T: &str = "01a11d88-ef31-7860-9d48-88926c4b1809";
const ROWS: usize = 40;

fn fixture(name: &str) -> String {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex_busy_inject");
    fs::read_to_string(dir.join(name)).unwrap()
}

/// A pane that draws the bottom of a captured busy, or idle, Codex screen with the cursor in its
/// empty composer; a paste is recorded and drawn in the composer, and each Enter is recorded.
pub(super) fn codex_renderer(f: &Fixture, name: &str, idle: bool) -> std::path::PathBuf {
    let root = f.root.path().join(name);
    fs::create_dir(&root).unwrap();
    // An idle pane is a captured idle Codex composer at 80 columns, drawn at the pane's bottom.
    let (shown, width) = if idle {
        let idle = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tui_input/codex-status-sol-80-idle.ansi");
        let pad = "\n".repeat(ROWS);
        (pad + &fs::read_to_string(idle).unwrap(), "80")
    } else {
        (fixture("busy_empty.ansi"), "160")
    };
    let lines: Vec<&str> = shown.lines().collect();
    let rows = &lines[lines.len() - ROWS..];
    let plain = |row: &str| crate::services::codex_tui::input::strip_ansi_escape_sequences(row);
    let prompt = rows.iter().rposition(|row| plain(row).starts_with('›'));
    fs::write(root.join("screen"), rows.join("\n")).unwrap();
    fs::write(root.join("prompt-row"), prompt.unwrap().to_string()).unwrap();
    let rollout: String = fixture("model_turn.jsonl")
        .lines()
        .take(11)
        .map(|line| format!("{line}\n"))
        .collect();
    fs::write(root.join("transcript"), rollout).unwrap();
    let script = f.root.path().join("codex-busy-renderer.py");
    fs::write(
        &script,
        r#"import os, pathlib, sys, tty
tty.setraw(0)
root = pathlib.Path(sys.argv[1])
rows = (root / 'screen').read_text().split('\n')
at = int((root / 'prompt-row').read_text())
def render(draft):
    shown = list(rows)
    if draft: shown[at] = '› ' + draft
    out = '\x1b[?2004h\x1b[2J' + ''.join('\x1b[%d;1H%s' % (i + 1, row) for i, row in enumerate(shown))
    os.write(1, (out + '\x1b[%d;%dH' % (at + 1, 3 + len(draft))).encode())
def event(name):
    with (root / 'events').open('a') as out: out.write(name + '\n')
render('')
(root / 'ready').write_text('ready')
pending = b''
while True:
    pending += os.read(0, 65536)
    if pending.startswith(b'\x1b[200~') and b'\x1b[201~' in pending:
        body, pending = pending[6:].split(b'\x1b[201~', 1)
        (root / 'body').write_bytes(body); event('paste')
        render(body.decode().split('\n')[0])
    if b'\r' in pending:
        event('Enter'); render(''); pending = b''
"#,
    )
    .unwrap();
    let command = format!("python3 '{}' '{}'", script.display(), root.display());
    let height = ROWS.to_string();
    let pane = f.tmux(&[
        "new-session",
        "-d",
        "-P",
        "-F",
        "#{pane_id}",
        "-s",
        name,
        "-x",
        width,
        "-y",
        &height,
        &command,
    ]);
    f.tmux(&["set-window-option", "-t", &pane, "window-size", "manual"]);
    f.tmux(&["resize-window", "-t", &pane, "-x", width, "-y", &height]);
    wait("codex-renderer", || root.join("ready").exists());
    wait("codex-renderer-capture", || {
        f.tmux(&["capture-pane", "-p", "-t", &pane])
            .contains("Ask Codex")
    });
    root
}

fn codex_rig(name: &str, channel: u64) -> Rig {
    codex_rig_on(name, channel, false)
}

fn codex_rig_on(name: &str, channel: u64, idle: bool) -> Rig {
    let f = Fixture::after_shared_test_env_lock();
    let renderer = codex_renderer(&f, name, idle);
    owned(f, name, channel, renderer, "codex")
}

impl Rig {
    fn codex(&self) -> Result<(), String> {
        self.dispatch(ProviderKind::Codex, target::Phase::Busy, &self.transcript())
    }

    /// The pane's tmux runs `args` on the real server once, after the wrapper's `step`.
    fn after(&self, step: &str, args: &[&str]) {
        let line = format!(
            "'{}' -L '{}' -f /dev/null {}",
            self.f.real.display(),
            self.f.socket,
            args.join(" ")
        );
        self.mark(&format!("after-{step}"), &line);
    }

    fn buffered(&self) -> bool {
        let buffers = self.f.tmux(&["list-buffers", "-F", "#{buffer_name}"]);
        buffers.contains(&format!("agentdesk-busy-inject-{}", self.nonce))
    }

    fn pasted(&self) -> String {
        fs::read_to_string(self.renderer.join("body")).unwrap_or_default()
    }

    fn frame(&self) -> String {
        busy_inject::frame("discord", AUTHOR, NAME, &self.nonce, PROMPT)
    }
}

/// A busy Codex pane takes main's frame as one paste and one Enter under the owner scope, and the
/// head settles without a receipt.
#[test]
fn owner_busy_codex_pastes_and_enters_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = codex_rig("oc-busy", 7_272_521);
    assert_eq!(rig.codex(), Ok(()));
    assert_eq!(rig.events(), ["paste", "Enter"]);
    assert_eq!(rig.pasted(), rig.frame());
    rig.delivered();
}

#[test]
fn owner_drive_codex_delivers_draft_busy_and_unknown_turn_once() {
    use crate::services::discord::queue_io::input_owner::drive::{Drive, Handoff, wake};
    use std::sync::Arc;
    use std::time::Duration;

    let _root = crate::config::TestRuntimeRootGuard::new();
    let ready =
        include_str!("../../../../../../tests/fixtures/tui_input/codex-status-sol-80-idle.ansi");
    let draft = ready.replace("\x1b[2mAsk Codex to do anything\x1b[0m", "a person's draft");
    let busy = format!("• Working (12s • esc to interrupt)\n{ready}");
    let control_words = ready.replace(
        "\x1b[2mAsk Codex to do anything\x1b[0m",
        "Approval required: my draft text",
    );
    let banner = include_str!(
        "../../../../../../tests/fixtures/tui_input/codex-0.160.0-security-banner.ansi"
    );
    for (n, phase, screen, rollout) in [
        (0, target::Phase::Busy, draft.as_str(), Some("")),
        (1, target::Phase::Busy, busy.as_str(), Some("{not json}\n")),
        (2, target::Phase::Busy, draft.as_str(), None),
        (3, target::Phase::Idle, draft.as_str(), None),
        (4, target::Phase::Idle, busy.as_str(), None),
        (5, target::Phase::Idle, banner, None),
        (
            6,
            target::Phase::Idle,
            "an unfamiliar composer layout",
            None,
        ),
        (7, target::Phase::Idle, control_words.as_str(), None),
        (8, target::Phase::Busy, ready, None),
        (
            9,
            target::Phase::Idle,
            "\n› /a person's draft\n\n  100% context left",
            None,
        ),
    ] {
        let name = format!("oc-deliver-{n}");
        let rig = codex_rig_on(&name, 7_676_100 + n, true);
        if let Some(rollout) = rollout {
            fs::write(rig.transcript(), rollout).unwrap();
        }
        // All effect threads read this capture through the existing tmux fixture.
        let shim = rig.f.root.path().join("tmux");
        let inner = rig.f.root.path().join("tmux-inner");
        let capture = rig.f.root.path().join("delivery-capture");
        fs::write(&capture, screen).unwrap();
        fs::rename(&shim, &inner).unwrap();
        let counter = rig.f.root.path().join("cursor-reads");
        let cursor = if n == 8 {
            format!(
                "case \"$*\" in *cursor_x*) n=$(cat '{}' 2>/dev/null || echo 0); n=$((n+1)); echo \"$n\" > '{}'; '{}' \"$@\" | awk -F '|' -v n=\"$n\" 'BEGIN {{OFS=\"|\"}} {{$5=($5+n)%$3; print}}'; exit 0 ;; esac\n",
                counter.display(),
                counter.display(),
                inner.display()
            )
        } else {
            String::new()
        };
        fs::write(&shim, format!("#!/bin/sh\n{cursor}case \"$*\" in *capture-pane*) cat '{}'; exit 0 ;; esac\nexec '{}' \"$@\"\n", capture.display(), inner.display())).unwrap();
        fs::set_permissions(&shim, fs::metadata(&inner).unwrap().permissions()).unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let handoff: Handoff = Arc::new(move |front| {
            let (reply, settled) = tokio::sync::oneshot::channel();
            send.send((front, reply)).unwrap();
            Box::pin(async move { settled.await.unwrap() })
        });
        let (settle_send, settle_receive) = std::sync::mpsc::channel();
        let drive = Arc::new(Drive {
            mailbox: rig.mailbox.clone(),
            channel: rig.channel,
            handoff,
            settled: Some(Arc::new(move || {
                settle_send.send(()).unwrap();
                Box::pin(async {})
            })),
        });
        {
            let _entered = rig.runtime.enter();
            wake(drive.clone());
        }
        let (front, reply) = receive
            .recv_timeout(Duration::from_secs(5))
            .expect("owner drive handoff");
        let result = rig.handoff(front, ProviderKind::Codex, phase, &rig.transcript());
        reply.send(result.clone()).unwrap();
        assert_eq!(result, Ok(()), "case {n}");
        assert_eq!(rig.events(), ["paste", "Enter"], "case {n}");
        let expected = if phase == target::Phase::Busy {
            rig.frame()
        } else {
            codex_herdr_frame(PROMPT).unwrap()
        };
        assert_eq!(rig.pasted(), expected, "case {n}");
        rig.delivered();
        settle_receive
            .recv_timeout(Duration::from_secs(5))
            .expect("drive settled after delivery");
        {
            let _entered = rig.runtime.enter();
            wake(drive.clone());
            wake(drive);
        }
        settle_receive
            .recv_timeout(Duration::from_secs(5))
            .expect("duplicate wake exhausted the queue");
        assert!(
            receive.try_recv().is_err(),
            "original submitted again in case {n}"
        );
        assert_eq!(rig.events(), ["paste", "Enter"], "case {n}");
    }
}

#[test]
fn owner_codex_holds_modal_missing_capture_and_copy_mode_without_keys() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let ready =
        include_str!("../../../../../../tests/fixtures/tui_input/codex-status-sol-80-idle.ansi");
    let modal = format!("Approval required?\n1. Yes\n2. No\n{}", ready.trim_start());
    let menu = include_str!(
        "../../../../../../tests/fixtures/tui_input/codex-0.160.0-security-banner.ansi"
    )
    .replace(
        "Press a number to choose · esc to dismiss · type to continue",
        "Press a number to choose",
    );
    let confirmation = menu.replace(
        "Press a number to choose",
        "Press enter to confirm or esc to cancel",
    );
    for (n, capture, mode) in [
        (0, Some(modal.as_str()), false),
        (1, None, false),
        (2, Some(ready), true),
        (3, Some(menu.as_str()), false),
        (4, Some(confirmation.as_str()), false),
        // Codex 0.166.0's slash_popup_footer_wide and default_unified_mention_popup snapshots.
        (
            5,
            Some(
                "  /model     choose what model and reasoning effort to use\n› /memories  configure memory use and generation\n  /mention   mention a file\n  /mcp       list MCP tools\n\n› /m\n\n  model · high · fast",
            ),
            false,
        ),
        (
            6,
            Some(
                "  Mentions\n› Sample Plugin   Plugin with skills and an MCP server\n  enter/tab insert · esc close · ↑/↓ select · ←/→ filter\n\n› @sa\n\n  100% context left",
            ),
            false,
        ),
    ] {
        let name = format!("oc-hold-{n}");
        let rig = codex_rig_on(&name, 7_676_120 + n, true);
        let shim = rig.f.root.path().join("tmux");
        let inner = rig.f.root.path().join("tmux-inner");
        let shown = rig.f.root.path().join("held-capture");
        if let Some(capture) = capture {
            fs::write(&shown, capture).unwrap();
        }
        fs::rename(&shim, &inner).unwrap();
        fs::write(
            &shim,
            format!(
                "#!/bin/sh\ncase \"$*\" in *capture-pane*) {} ;; esac\nexec '{}' \"$@\"\n",
                if capture.is_some() {
                    format!("cat '{}'; exit 0", shown.display())
                } else {
                    "exit 1".into()
                },
                inner.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&shim, fs::metadata(&inner).unwrap().permissions()).unwrap();
        if mode {
            rig.f.tmux(&["copy-mode", "-t", &format!("={name}:")]);
        }
        let result = rig.dispatch(ProviderKind::Codex, target::Phase::Idle, &rig.transcript());
        assert!(result.is_err(), "case {n}: {result:?}");
        assert!(rig.events().is_empty(), "case {n}: {:?}", rig.events());
        let snapshot = rig.snapshot();
        assert!(snapshot["head_injection"].is_null(), "{snapshot}");
        assert_eq!(
            snapshot["items"].as_array().unwrap().len()
                + snapshot["retained_inputs"].as_array().unwrap().len(),
            1,
            "{snapshot}"
        );
    }
}

/// A Codex modal on an idle pane holds the front and leaves the composer read with the held watch:
/// nothing is handed on while it stays, a ready composer wakes the drive to deliver once.
#[test]
fn a_codex_modal_that_closes_wakes_the_drive_to_deliver_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let ready =
        include_str!("../../../../../../tests/fixtures/tui_input/codex-status-sol-80-idle.ansi");
    let modal = format!("Approval required?\n1. Yes\n2. No\n{}", ready.trim_start());
    let rig = codex_rig_on("oc-watch", 7_676_140, true);
    let (shim, inner) = (
        rig.f.root.path().join("tmux"),
        rig.f.root.path().join("tmux-inner"),
    );
    let shown = rig.f.root.path().join("held-capture");
    fs::write(&shown, &modal).unwrap();
    fs::rename(&shim, &inner).unwrap();
    let script = format!(
        "#!/bin/sh\ncase \"$*\" in *capture-pane*) cat '{}'; exit 0 ;; esac\nexec '{}' \"$@\"\n",
        shown.display(),
        inner.display()
    );
    fs::write(&shim, script).unwrap();
    fs::set_permissions(&shim, fs::metadata(&inner).unwrap().permissions()).unwrap();
    let (drive, inbox) = rig.drive();
    rig.wake(&drive);
    let idle = (ProviderKind::Codex, target::Phase::Idle);
    let held = rig.serve(&inbox, &idle, 10);
    assert_eq!(held, Some(Err("NotSent(\"Modal\")".into())));
    assert_eq!(
        rig.serve(&inbox, &idle, 5),
        None,
        "an open modal hands nothing on"
    );
    fs::write(&shown, ready).unwrap();
    assert_eq!(rig.serve(&inbox, &idle, 15), Some(Ok(())));
    assert_eq!(rig.events(), ["paste", "Enter"]);
    rig.delivered();
}

#[test]
fn owner_codex_changed_source_and_late_copy_mode_never_receive_payload() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    for (n, copy_mode) in [(0, false), (1, true)] {
        let name = format!("oc-late-hold-{n}");
        let rig = codex_rig(&name, 7_676_130 + n);
        if copy_mode {
            rig.mark(
                "before-paste",
                &format!(
                    "'{}' -L '{}' -f /dev/null copy-mode -t '={name}:'",
                    rig.f.real.display(),
                    rig.f.socket
                ),
            );
        } else {
            rig.mark(
                "after-load",
                &format!("truncate -s 0 '{}'", rig.transcript().display()),
            );
        }
        let result = rig.codex();
        assert!(result.is_err(), "case {n}: {result:?}");
        assert!(rig.events().is_empty(), "case {n}: {:?}", rig.events());
    }
}

/// An owner refusal after the buffer is loaded and before the paste proves the same attempt was
/// not sent: the original stays at the queue front unarmed, with no key, buffer or ledger row.
#[test]
fn owner_ready_refusal_before_the_codex_paste_is_known_not_sent() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = codex_rig("oc-ready", 7_272_522);
    // Another incarnation's marker: the owner's epoch refuses the pane, the composer still reads.
    let marker = crate::services::tmux_common::session_temp_path(&rig.name, "spawn_nonce");
    rig.mark("after-load", &format!("printf other > '{marker}'"));
    let result = rig.codex();
    assert_eq!(result, Err("NotSent(\"PaneUnavailable\")".into()));
    rig.waits_not_sent();
    assert_eq!(rig.bind(), TurnBind::NotArmed);
    assert!(!rig.buffered(), "the loaded buffer is dropped");
    let register = crate::services::tui_prompt_dedupe::register_injected_steer;
    assert!(
        register("codex", &rig.name, &rig.nonce, T, None),
        "the legacy ledger row was withdrawn"
    );
    crate::services::tui_prompt_dedupe::withdraw_injected_steer("codex", &rig.name, &rig.nonce);
}

/// After the paste nothing is read before Enter: a resized pane still gets its one Enter, and a
/// replaced pane process gets none and stays an unconfirmed write that is never resent.
#[test]
fn after_the_codex_paste_only_a_replaced_pane_stops_the_enter() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let resized = codex_rig("oc-resize", 7_272_523);
    let target = format!("={}:", resized.name);
    resized.after(
        "paste",
        &["resize-window", "-t", &target, "-x", "150", "-y", "38"],
    );
    assert_eq!(resized.codex(), Ok(()));
    assert_eq!(resized.events(), ["paste", "Enter"]);
    resized.delivered();
    let swapped = codex_rig("oc-swap", 7_272_524);
    swapped.mark("swap-after", "paste");
    let result = swapped.codex();
    assert_eq!(result, Err("WriteUncertain(\"enter_failed\")".into()));
    wait("swapped-pane-ready", || {
        swapped.renderer.join("ready").exists()
    });
    assert_eq!(swapped.events(), ["paste"]);
    swapped.uncertain();
    assert!(matches!(swapped.bind(), TurnBind::Armed(_)));
}

/// An idle Codex pane takes the prompt's typed text as one pinned paste and one Enter after a
/// single composer read; nothing reads the pane after the keys, and the head settles unreceipted.
/// An idle Codex composer held by another writer's lock or a failed buffer load gets no key and keeps
/// the original at the front with its ticket; only that cause clearing wakes the drive to paste once.
#[test]
fn an_idle_codex_lock_or_load_refusal_holds_the_front_until_it_clears() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    use crate::services::codex_tui::input::try_with_composer_mutation_lock;
    for (n, locked) in [(0_u64, true), (1, false)] {
        let rig = codex_rig_on(&format!("oc-wait-{n}"), 7_676_130 + n, true);
        let (release, released) = std::sync::mpsc::channel::<()>();
        let holder = match locked {
            true => {
                let (name, (taken, took)) = (rig.name.clone(), std::sync::mpsc::channel());
                let holder = std::thread::spawn(move || {
                    try_with_composer_mutation_lock(&name, || {
                        taken.send(()).unwrap();
                        released.recv().ok()
                    })
                });
                took.recv().unwrap();
                Some(holder)
            }
            false => {
                rig.mark("fail-load", "");
                None
            }
        };
        let reason = if locked {
            "LockContended"
        } else {
            "LoadFailed"
        };
        let (drive, inbox) = rig.drive();
        rig.wake(&drive);
        let idle = (ProviderKind::Codex, target::Phase::Idle);
        let held = rig.serve(&inbox, &idle, 10);
        assert_eq!(held, Some(Err(format!("NotSent({reason:?})"))));
        rig.waits_not_sent();
        assert_eq!(rig.bind(), TurnBind::NotArmed, "{reason}");
        let stays = rig.serve(&inbox, &idle, 6);
        assert_eq!(
            stays, None,
            "{reason}: nothing is handed on while the cause stays"
        );
        match holder {
            Some(holder) => {
                release.send(()).unwrap();
                holder.join().unwrap();
            }
            None => {
                let dir = rig.f.root.path().join(format!("{}-owner", rig.name));
                fs::remove_file(dir.join("fail-load")).unwrap();
            }
        }
        assert_eq!(rig.serve(&inbox, &idle, 15), Some(Ok(())), "{reason}");
        assert_eq!(rig.events(), ["paste", "Enter"], "{reason}");
        rig.delivered();
    }
}

/// A buffer load that fails once and loads again before the hold commits is still the hold's
/// cause clearing: the watch's first look wakes the drive, which pastes and enters once.
#[test]
fn an_idle_codex_load_that_fails_once_is_pasted_once_without_another_wake() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = codex_rig_on("oc-load-once", 7_676_150, true);
    rig.mark("fail-load", "once");
    let (drive, inbox) = rig.drive();
    rig.wake(&drive);
    let idle = (ProviderKind::Codex, target::Phase::Idle);
    let held = rig.serve(&inbox, &idle, 10);
    assert_eq!(held, Some(Err("NotSent(\"LoadFailed\")".into())));
    assert_eq!(rig.serve(&inbox, &idle, 15), Some(Ok(())));
    assert_eq!(rig.events(), ["paste", "Enter"]);
    rig.delivered();
}

/// A load watch whose pane is gone ends: no read runs at 2, 5, 30 or 60 s and nothing is handed on.
#[test]
fn a_cause_watch_ends_when_its_pane_is_gone() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = codex_rig_on("oc-cause-gone", 7_676_151, true);
    rig.mark("fail-load", "");
    let (drive, inbox) = rig.drive();
    rig.wake(&drive);
    let idle = (ProviderKind::Codex, target::Phase::Idle);
    let held = rig.serve(&inbox, &idle, 10);
    assert_eq!(held, Some(Err("NotSent(\"LoadFailed\")".into())));
    rig.f
        .tmux(&["kill-session", "-t", &format!("={}", rig.name)]);
    let trace = rig.f.root.path().join(format!("{}-owner/trace", rig.name));
    let loads = || {
        let trace = fs::read_to_string(&trace).unwrap_or_default();
        trace.lines().filter(|l| l.contains("load-buffer")).count()
    };
    // One look after the kill sees the pane gone.
    std::thread::sleep(std::time::Duration::from_millis(4_500));
    let (ended, mut waited) = (loads(), 0);
    for at in [2, 5, 30, 60] {
        std::thread::sleep(std::time::Duration::from_secs(at - waited));
        waited = at;
        assert_eq!(loads(), ended, "a read at {at}s after the watch ended");
    }
    assert!(inbox.try_recv().is_err(), "nothing is handed on");
    assert!(rig.events().is_empty(), "{:?}", rig.events());
}

#[test]
fn owner_idle_codex_pastes_and_enters_once_without_a_later_read() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = codex_rig_on("oc-idle", 7_272_525, true);
    // Every tmux call from here on, the composer snapshot's own included, lands in one log.
    let shim = rig.f.root.path().join("tmux");
    let inner = rig.f.root.path().join("tmux-inner");
    fs::rename(&shim, &inner).unwrap();
    let log = rig.f.root.path().join("oc-idle-calls");
    let wrapper = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
        log.display(),
        inner.display()
    );
    fs::write(&shim, wrapper).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o700)).unwrap();
    let result = rig.dispatch(ProviderKind::Codex, target::Phase::Idle, &rig.transcript());
    assert_eq!(result, Ok(()));
    assert_eq!(rig.events(), ["paste", "Enter"]);
    assert_eq!(rig.pasted(), codex_herdr_frame(PROMPT).unwrap());
    rig.delivered();
    let dir = rig.f.root.path().join(format!("{}-owner", rig.name));
    let trace = fs::read_to_string(dir.join("trace")).unwrap();
    let keys: Vec<_> = (trace.lines())
        .filter(|line| line.contains("paste-buffer") || line.contains("send-keys"))
        .collect();
    assert_eq!(keys.len(), 2, "{trace}");
    assert!(
        keys.iter().all(|line| line.contains("if-shell")),
        "pinned: {trace}"
    );
    let last = trace.lines().last().unwrap_or_default();
    assert!(last.contains("send-keys"), "no read after Enter: {trace}");
    let calls = fs::read_to_string(&log).unwrap();
    let calls: Vec<&str> = calls.lines().collect();
    let pasted = (calls.iter().position(|c| c.contains("paste-buffer"))).expect("one paste");
    let reads: Vec<usize> = (0..calls.len())
        .filter(|&i| calls[i].contains("capture-pane"))
        .collect();
    assert_eq!(reads.len(), 1, "one composer read: {calls:#?}");
    assert!(reads[0] < pasted, "read before the paste: {calls:#?}");
    let count = |key: &str| calls.iter().filter(|c| c.contains(key)).count();
    assert_eq!(
        (count("paste-buffer"), count("Enter")),
        (1, 1),
        "{calls:#?}"
    );
}

// A child must inherit the stable PATH, not another test's short-lived tmux shim.
fn capacity_child(test: &str) -> bool {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    crate::services::discord::admin_host_guard::tests::api_child(test)
}

/// An idle pane's owner effect needs the node's execution capacity first: a refused capacity sends
/// no key and reserves nothing, so the handoff never returns ahead of the capacity it runs under.
#[test]
fn an_idle_owner_front_needs_execution_capacity_before_any_key() {
    let path = module_path!().split_once("::").unwrap().1;
    let test = format!("{path}::an_idle_owner_front_needs_execution_capacity_before_any_key");
    if !capacity_child(&test) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let data = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.data.dir = data.path().to_path_buf();
    config.cluster.enabled = true;
    config.cluster.execution_slots = Some(1);
    crate::config_live_reload::install(config);
    let rig = codex_rig_on("oc-capacity", 7_272_527, true);
    let front = rig.granted_front();
    let result = rig.handoff(
        front,
        ProviderKind::Codex,
        target::Phase::Idle,
        &rig.transcript(),
    );
    assert_eq!(
        result,
        Err("NotSent(\"execution capacity requires PostgreSQL\")".into())
    );
    assert!(rig.events().is_empty(), "{:?}", rig.events());
    let snapshot = rig.snapshot();
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    assert_eq!(snapshot["items"][0]["text"], PROMPT, "{snapshot}");
}

/// A submitted idle turn holds its execution slot and lease until its AI turn ends, so another
/// channel finds the node full meanwhile; the slot returns once, and only then may another run.
#[test]
fn an_idle_owner_turn_holds_its_capacity_until_its_ai_turn_ends_pg() {
    let path = module_path!().split_once("::").unwrap().1;
    let test = format!("{path}::an_idle_owner_turn_holds_its_capacity_until_its_ai_turn_ends_pg");
    if capacity_child(&test) {
        holds_capacity_to_the_turn_end(Fault::None);
    }
}

/// A write that may have landed, its Enter failing after the paste, holds the slot the same way.
#[test]
fn an_uncertain_idle_write_holds_its_capacity_until_its_ai_turn_ends_pg() {
    let path = module_path!().split_once("::").unwrap().1;
    let test =
        format!("{path}::an_uncertain_idle_write_holds_its_capacity_until_its_ai_turn_ends_pg");
    if capacity_child(&test) {
        holds_capacity_to_the_turn_end(Fault::Enter);
    }
}

/// A turn whose Enter went but whose settlement could not be stored holds the slot the same way.
#[test]
fn a_sent_idle_turn_whose_settlement_fails_holds_its_capacity_until_its_ai_turn_ends_pg() {
    let path = module_path!().split_once("::").unwrap().1;
    let name =
        "a_sent_idle_turn_whose_settlement_fails_holds_its_capacity_until_its_ai_turn_ends_pg";
    if capacity_child(&format!("{path}::{name}")) {
        holds_capacity_to_the_turn_end(Fault::Settle);
    }
}

/// A refusal before the first key whose not-sent custody cannot be stored either still proves
/// nothing went: no AI turn is awaited and the slot returns at once.
#[test]
fn an_unsent_idle_write_whose_custody_fails_returns_its_capacity_at_once_pg() {
    let path = module_path!().split_once("::").unwrap().1;
    let name = "an_unsent_idle_write_whose_custody_fails_returns_its_capacity_at_once_pg";
    if capacity_child(&format!("{path}::{name}")) {
        unsent_returns_its_capacity_at_once();
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Fault {
    None,
    Enter,
    Settle,
}

/// A one-slot node with a database for its leases, and an idle Codex owner on `channel`.
fn slot_rig(name: &str, channel: u64) -> (tempfile::TempDir, Rig, TestPostgresDb, sqlx::PgPool) {
    let data = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::default();
    config.data.dir = data.path().to_path_buf();
    config.cluster.enabled = true;
    config.cluster.execution_slots = Some(1);
    crate::config_live_reload::install(config);
    let rig = codex_rig_on(name, channel, true);
    let db = rig.runtime.block_on(TestPostgresDb::create());
    let pool = rig.runtime.block_on(db.connect_and_migrate());
    rig.pool.set(pool.clone()).unwrap();
    (data, rig, db, pool)
}

fn leases(rig: &Rig, pool: &sqlx::PgPool) -> i64 {
    let count = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM node_execution_leases");
    rig.runtime.block_on(count.fetch_one(pool)).unwrap()
}

type Capacity =
    Result<Option<crate::services::cluster::execution_capacity::ExecutionGuard>, String>;

fn other_run(rig: &Rig, pool: &sqlx::PgPool, channel: u64) -> Capacity {
    let _entered = rig.runtime.enter();
    let cancel = std::sync::Arc::new(crate::services::provider::CancelToken::new());
    crate::services::cluster::execution_capacity::acquire(Some(pool), "codex", channel, cancel)
}

/// A shell line that changes the owner's snapshot file behind it, so each later commit fails.
fn outside_write(channel: u64) -> String {
    let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    let path = root.join(format!("claude/input-owner-dispatch/{channel}.json"));
    format!("printf '\\n' >> '{}'", path.display())
}

fn holds_capacity_to_the_turn_end(fault: Fault) {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (_data, rig, db, pool) = slot_rig("oc-slot", 7_272_539);
    match fault {
        Fault::None => {}
        Fault::Enter => rig.mark("swap-after", "paste"),
        Fault::Settle => rig.mark("after-Enter", &outside_write(7_272_539)),
    }
    let (settled, answer) = tokio::sync::oneshot::channel();
    let claim = rig.claim(&rig.granted_front()).unwrap();
    let transcript = rig.transcript();
    std::thread::scope(|scope| {
        // Inside the scope, so a failed assertion drops it and ends the held AI turn.
        let (release, turn) = std::sync::mpsc::channel::<()>();
        let until_done: super::UntilDone = Box::new(move || {
            let _ = turn.recv();
            Ok(())
        });
        let first = scope.spawn(|| {
            let how = (
                ProviderKind::Codex,
                target::Phase::Idle,
                transcript.as_path(),
            );
            rig.execute_until(claim, how, settled, Some(until_done))
        });
        let wait = async { tokio::time::timeout(std::time::Duration::from_secs(20), answer).await };
        let answered = rig.runtime.block_on(wait);
        let (held, refused) = (leases(&rig, &pool), other_run(&rig, &pool, 7_272_540).err());
        let waiting = release.send(()).is_ok();
        let first = first.join().unwrap();
        assert!(
            waiting,
            "the capacity holder ended before the AI turn: {first:?}; tmux={}",
            rig.f.real.display()
        );
        match fault {
            Fault::None => assert_eq!(first, Ok(())),
            Fault::Enter => assert_eq!(first, Err("WriteUncertain(\"enter_failed\")".into())),
            // The store's refusal after the Enter, never read as a write that surely did not land.
            Fault::Settle => assert!(
                first.as_ref().is_err_and(|e| !effect::surely_unsent(e)),
                "{first:?}"
            ),
        }
        assert_eq!(answered.map(Result::ok), Ok(Some(first)));
        assert_eq!(held, 1, "the lease stays while the AI turn runs");
        assert!(
            refused.is_some_and(|e| e.contains("exhausted")),
            "the slot stays taken"
        );
    });
    if fault == Fault::Settle {
        assert_eq!(rig.events(), ["paste", "Enter"], "the turn was sent");
    }
    assert_eq!(leases(&rig, &pool), 0, "released once the turn ended");
    let next = other_run(&rig, &pool, 7_272_540).expect("the returned slot");
    assert!(next.is_some());
    assert!(
        other_run(&rig, &pool, 7_272_546).is_err(),
        "returned once: one slot, now taken"
    );
    drop(next);
    rig.runtime.block_on(db.drop());
}

fn unsent_returns_its_capacity_at_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (_data, rig, db, pool) = slot_rig("oc-unsent", 7_272_547);
    // After the buffer loads and before the payload commit, so no key ever follows.
    rig.mark("after-load", &outside_write(7_272_547));
    let observed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = observed.clone();
    let until_done: super::UntilDone = Box::new(move || {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    });
    let (settled, _answer) = tokio::sync::oneshot::channel();
    let claim = rig.claim(&rig.granted_front()).unwrap();
    let how = (ProviderKind::Codex, target::Phase::Idle, rig.transcript());
    let result = rig.execute_until(claim, (how.0, how.1, &how.2), settled, Some(until_done));
    let observed = observed.load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(observed, 0, "no AI turn is awaited: {result:?}");
    assert!(
        result.as_ref().is_err_and(|e| effect::surely_unsent(e)),
        "{result:?}"
    );
    assert!(!rig.events().contains(&"paste".to_string()), "no content");
    assert_eq!(leases(&rig, &pool), 0, "returned at once");
    let next = other_run(&rig, &pool, 7_272_540).expect("the returned slot");
    assert!(next.is_some());
    drop(next);
    rig.runtime.block_on(db.drop());
}

/// A first paste held by a pane replaced before it ran settles not sent with one notice; a pane
/// replaced after the paste holds only the Enter, a write that may have landed.
#[test]
fn an_idle_codex_paste_held_before_it_ran_settles_not_sent() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = codex_rig_on("oc-held", 7_272_528, true);
    let pane = rig.f.tmux(&[
        "display-message",
        "-p",
        "-t",
        &format!("={}:", rig.name),
        "#{pane_id}",
    ]);
    let swap = format!(
        "'{}' -L '{}' -f /dev/null respawn-pane -k -t '{pane}'\n",
        rig.f.real.display(),
        rig.f.socket
    );
    rig.mark("before-paste", &swap);
    let result = rig.dispatch(ProviderKind::Codex, target::Phase::Idle, &rig.transcript());
    assert_eq!(result, Err("NotSent(\"pane_moved\")".into()));
    wait("held-pane-ready", || rig.renderer.join("ready").exists());
    rig.held_not_sent("pane_moved");
    let snapshot = rig.snapshot();
    assert!(snapshot["head_injection"].is_null(), "{snapshot}");
    let notice = &snapshot["retained_inputs"][0]["notice"];
    assert_eq!(notice["kind"], "retained_retry_required", "{snapshot}");

    let after = codex_rig_on("oc-held-enter", 7_272_529, true);
    after.mark("swap-after", "paste");
    let result = after.dispatch(
        ProviderKind::Codex,
        target::Phase::Idle,
        &after.transcript(),
    );
    assert_eq!(result, Err("WriteUncertain(\"enter_failed\")".into()));
    wait("enter-pane-ready", || after.renderer.join("ready").exists());
    assert_eq!(after.events(), ["paste"]);
    after.uncertain();
}

/// The real Codex leaf passes ready, fails only its start commit, releases both temporary stores
/// and keeps A at the queue front, which the next attempt delivers once under a fresh nonce.
#[test]
fn a_codex_start_only_refusal_keeps_custody_and_releases_buffer_and_ledger() {
    use crate::services::discord::queue_io::input_owner::{
        effect::{BusyCodex, CodexInput, PreparedPayload},
        pump,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = codex_rig("oc-start-only", 7_272_548);
    let input = |nonce: &str, text: &str| CodexInput {
        session: rig.name.clone(),
        rollout: rig.transcript(),
        target_turn: T.into(),
        channel: rig.channel,
        message: None,
        source: "discord".into(),
        author: AUTHOR.into(),
        name: NAME.into(),
        nonce: nonce.into(),
        text: text.into(),
    };
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(rig.transcript()).unwrap();
    let payload = |nonce: &str, text: &str, source: u64| PreparedPayload {
        frame: crate::services::claude_tui::busy_inject::frame(
            "discord", AUTHOR, NAME, nonce, text,
        ),
        sources: vec![source.to_string()],
        attempt: serde_json::json!({"nonce":nonce,"provider":"codex","target":scope::created_tmux_target("codex", &rig.name).unwrap(),"source":{"kind":"existing","session_id":format!("sid-{}", rig.channel),"transcript":rig.transcript().display().to_string(),"file":[meta.dev(),meta.ino()],"offset":meta.len()}}),
    };
    let leaf = |input| {
        BusyCodex {
            pane: crate::services::claude_tui::busy_inject::Pane::with_program(
                &rig.name,
                rig.program.clone(),
            ),
            timing: &crate::services::codex_tui::busy_inject::TIMING,
            input,
        }
        .leaf()
    };
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = calls.clone();
    let mailbox = rig.mailbox.clone();
    let path = crate::services::discord::runtime_store::discord_pending_queue_root()
        .unwrap()
        .join(format!("claude/input-owner-dispatch/{}.json", rig.channel));
    let saved = Arc::new(std::sync::Mutex::new(None));
    let (at_start, store) = (saved.clone(), path.clone());
    let clock: effect::Clock = Arc::new(move || {
        if counted.fetch_add(1, Ordering::SeqCst) == 1 {
            tokio::runtime::Handle::current()
                .block_on(mailbox.v2_request(|slot| {
                    assert!(
                        slot.as_ref()
                            .unwrap()
                            .snapshot
                            .head_injection
                            .as_ref()
                            .unwrap()
                            .resolved_target
                            .is_some(),
                        "ready must pin before the start fault"
                    );
                    Ok(())
                }))
                .unwrap();
            let bytes = fs::read(&store).unwrap();
            fs::write(&store, [&bytes[..], b"\n"].concat()).unwrap();
            *at_start.lock().unwrap() = Some(bytes);
        }
        10_000
    });
    let actual = leaf(input(&rig.nonce, PROMPT));
    let failing: effect::Leaf = Box::new(move |frame| {
        let completion = actual(frame);
        if let Some(bytes) = saved.lock().unwrap().take() {
            fs::write(path, bytes).unwrap();
        }
        completion
    });
    let refused = rig.runtime.block_on(pump::submit(
        &rig.mailbox,
        payload(&rig.nonce, PROMPT, rig.original.message_id),
        clock,
        failing,
    ));
    assert!(rig.events().is_empty(), "start refusal must send zero keys");
    assert_eq!(
        refused,
        Ok(effect::Completion::NotSent("LockContended".into()))
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "only the arm and real start asked for time"
    );
    let held = rig.snapshot();
    assert!(held["head_injection"].is_null(), "{held}");
    assert_eq!(held["retained_inputs"], json!([]), "{held}");
    assert_eq!(
        held["items"][0],
        serde_json::to_value(&rig.original).unwrap()
    );
    assert!(
        matches!(rig.bind(), TurnBind::NotArmed),
        "the same admission remains in owner custody"
    );
    let buffers = rig.f.tmux(&["list-buffers", "-F", "#{buffer_name}"]);
    assert!(
        !buffers.contains(&format!("agentdesk-busy-inject-{}", rig.nonce)),
        "temporary buffer leaked: {buffers}"
    );
    let free = crate::services::tui_prompt_dedupe::register_injected_steer(
        "codex", &rig.name, &rig.nonce, T, None,
    );
    crate::services::tui_prompt_dedupe::withdraw_injected_steer("codex", &rig.name, &rig.nonce);
    assert!(free, "start refusal must withdraw the legacy ledger row");
    // The next attempt takes A from the front under a fresh nonce and delivers it once.
    let result = rig.runtime.block_on(pump::submit(
        &rig.mailbox,
        payload("retry0a", PROMPT, rig.original.message_id),
        Arc::new(|| 10_000),
        leaf(input("retry0a", PROMPT)),
    ));
    assert_eq!(result, Ok(effect::Completion::Submitted));
    assert_eq!(rig.events(), ["paste", "Enter"]);
    assert_eq!(
        fs::read_to_string(rig.renderer.join("body")).unwrap(),
        crate::services::claude_tui::busy_inject::frame("discord", AUTHOR, NAME, "retry0a", PROMPT)
    );
    let after = rig.snapshot();
    assert_eq!(after["retained_inputs"], json!([]), "{after}");
    assert_eq!(after["items"], json!([]), "{after}");
}

/// A start claimed while its Codex pane was idle meets a native turn opened before its execution:
/// that execution reads the same rollout, holds the start and writes nothing to the pane.
#[test]
fn a_start_claimed_before_a_native_codex_turn_opens_is_held_unwritten() {
    use crate::services::discord::router::message_handler::provider_dispatch::StreamingTurn;
    use crate::services::provider::CancelToken;
    use crate::services::turn_host::TurnHost;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = codex_rig_on("oc-late-busy", 7_340_812, true);
    let root = rig.f.root.path();
    let (workspace, home) = (root.join("late-workspace"), root.join("codex-home"));
    fs::create_dir(&workspace).unwrap();
    let guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let _home = guard("CODEX_HOME", &home);
    let sid = uuid::Uuid::new_v4().to_string();
    let rollout = home
        .join("sessions/2026/11/04")
        .join(format!("rollout-2026-11-04T00-00-00-{sid}.jsonl"));
    fs::create_dir_all(rollout.parent().unwrap()).unwrap();
    let line = |value: serde_json::Value| format!("{value}\n");
    let meta = serde_json::json!({"type":"session_meta","payload":{"id":sid,"cwd":workspace,
        "source":"cli"}});
    fs::write(&rollout, line(meta)).unwrap();
    let binding = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
        output_path: rollout.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(sid.clone()),
        last_offset: 0,
        relay_last_offset: None,
    };
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(&rig.name, binding);
    let claim = rig.claim(&rig.granted_front()).expect("granted front");
    // The native turn opens after the claim and before the start's execution reads the pane.
    let open = move |turn: &str, text: &str| {
        let user = serde_json::json!({"type":"response_item","payload":{"type":"message",
            "role":"user","content":[{"type":"input_text","text":text}]}});
        let started = serde_json::json!({"type":"event_msg","payload":{"type":"task_started",
            "turn_id":turn}});
        line(started) + &line(user)
    };
    let append = {
        let rollout = rollout.clone();
        move |rows: String| {
            let mut file = fs::OpenOptions::new().append(true).open(&rollout).unwrap();
            std::io::Write::write_all(&mut file, rows.as_bytes()).unwrap();
        }
    };
    append(open("native-turn", "native prompt"));
    // A pasted start would open its own turn; it is answered so no read waits on it.
    let (renderer, answers) = (rig.renderer.clone(), append.clone());
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopped = stop.clone();
    let answering = std::thread::spawn(move || {
        while !stopped.load(std::sync::atomic::Ordering::SeqCst) {
            if let Ok(body) = fs::read_to_string(renderer.join("body")) {
                std::thread::sleep(std::time::Duration::from_millis(500));
                let done = serde_json::json!({"type":"event_msg","payload":{"type":"task_complete",
                    "turn_id":"own-turn","last_agent_message":"own"}});
                answers(open("own-turn", &body) + &line(done));
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    });
    let (settled, _) = tokio::sync::oneshot::channel();
    let owner = OwnerTurn {
        mailbox: rig.mailbox.clone(),
        claim,
        settled,
        origin: InputOrigin::HttpStart,
    };
    let (host, provider) = (TurnHost::Tmux, ProviderKind::Codex);
    let turn = StreamingTurn {
        pool: None,
        shared: None,
        provider: &provider,
        prompt: PROMPT,
        session_id: Some(&sid),
        working_dir: workspace.to_str().unwrap(),
        system_prompt: None,
        allowed_tools: &[],
        cancel: std::sync::Arc::new(CancelToken::new()),
        codex_observation: None,
        tmux_session_name: Some(&rig.name),
        teardown: None,
        host: &host,
        channel_id: rig.channel,
        model: None,
        native_fast_mode: None,
        effort: None,
        codex_goals: None,
        compact_percent: None,
        compact_lower_bound_tokens: 0,
        compact_token_limit: None,
        cache_ttl_minutes: None,
        dispatch_type: None,
        force_fresh: false,
        submission: None,
    };
    let result = {
        let _entered = rig.runtime.enter();
        execute_owned(turn, std::sync::mpsc::channel().0, owner)
    };
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    answering.join().unwrap();
    let held = (result, rig.events(), rig.buffered());
    let expected = (
        Err("NotSent(\"synthetic_waits_idle\")".to_string()),
        Vec::<String>::new(),
        false,
    );
    assert_eq!(held, expected);
    let state = rig.snapshot();
    assert_eq!(state["items"].as_array().unwrap().len(), 1, "{state}");
    assert!(state["head_injection"].is_null(), "unattempted: {state}");
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&rig.name);
}
