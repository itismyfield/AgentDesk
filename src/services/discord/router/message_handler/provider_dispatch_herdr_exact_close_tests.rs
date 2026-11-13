//! A private real tmux exercises the production exact-pane close protocol.
use super::*;
use crate::services::claude::session_handoff::{EarlierExecution, ExitSend, Liveness};
use crate::services::tui_prompt_dedupe::binding_context::{
    PreparedIncarnation, SpawnNonceMarker, execution_context, observe_spawn_nonce_marker,
};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};

// The environment guard stays on the test thread while the pane handle crosses executor threads.
thread_local! {
    static PATH_GUARDS: std::cell::RefCell<std::collections::HashMap<PathBuf, Guard>> = std::cell::RefCell::new(std::collections::HashMap::new());
}

pub(crate) struct OldTmux {
    root: tempfile::TempDir,
    real: PathBuf,
    logical: String,
    channel: u64,
    native: Option<String>,
    installed: Mutex<bool>,
    panes: Mutex<Option<(String, String)>>,
}

impl OldTmux {
    pub(crate) fn new(logical: &str, native: Option<String>) -> Self {
        Self::new_for_channel(logical, native, CHANNEL)
    }

    pub(crate) fn new_for_channel(logical: &str, native: Option<String>, channel: u64) -> Self {
        let real = [
            "/opt/homebrew/bin/tmux",
            "/usr/bin/tmux",
            "/usr/local/bin/tmux",
        ]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .expect("a real tmux binary is required");
        Self {
            root: tempfile::tempdir().unwrap(),
            real,
            logical: logical.into(),
            channel,
            native,
            installed: Mutex::new(false),
            panes: Mutex::new(None),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.real);
        command.args([
            "-S",
            self.root.path().join("s").to_str().unwrap(),
            "-f",
            "/dev/null",
        ]);
        command.env_remove("TMUX").env_remove("TMUX_PANE");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command().args(args).output().unwrap()
    }

    fn checked(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "private tmux {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    fn install(&self) {
        let mut installed = self.installed.lock().unwrap();
        if *installed {
            return;
        }
        let marker = crate::services::tmux_common::session_temp_path(&self.logical, "spawn_nonce");
        let script = format!(
            r#"#!/usr/bin/env python3
import json, os, select, subprocess, sys
real, socket, log, arm, marker = {real}, {socket}, {log}, {arm}, {marker}
args = sys.argv[1:]
def record(text):
    with open(log, 'a') as out:
        out.write(text + '\n')
def exit_failure(pid, reason):
    try:
        probe = subprocess.run(['ps', '-p', str(pid), '-o', 'pid=,ppid=,stat=,lstart=,command='], capture_output=True, text=True, timeout=2)
        detail = 'ps_rc=%s stdout=%r stderr=%r' % (probe.returncode, probe.stdout, probe.stderr)
    except Exception as error:
        detail = 'ps_read_error=%r' % error
    text = 'PROVIDER_EXIT failed pid=%s reason=%s %s' % (pid, reason, detail)
    record(text)
    print(text, file=sys.stderr, flush=True)
def arm_exit(pane):
    pid = int(subprocess.check_output([real, '-S', socket, '-f', '/dev/null', 'display-message', '-p', '-t', pane, '#{{pane_pid}}'], text=True, timeout=2).strip())
    if pid <= 0:
        raise RuntimeError('private provider pid is not positive')
    fd, watch = None, None
    try:
        if hasattr(os, 'pidfd_open'):
            fd = os.pidfd_open(pid)
            watch = select.poll()
            watch.register(fd, select.POLLIN)
        else:
            watch = select.kqueue()
            event = select.kevent(pid, filter=select.KQ_FILTER_PROC, flags=select.KQ_EV_ADD | select.KQ_EV_ONESHOT, fflags=select.KQ_NOTE_EXIT)
            watch.control([event], 0, 0)
    except ProcessLookupError:
        return pid, fd, watch, True
    except Exception as error:
        exit_failure(pid, repr(error))
        raise
    record('PROVIDER_EXIT armed pane=%s pid=%s' % (pane, pid))
    return pid, fd, watch, False
def observed_exit(captured):
    pid, fd, watch, absent = captured
    try:
        if absent:
            return True
        if fd is not None:
            return any(event_fd == fd and flags & select.POLLIN for event_fd, flags in watch.poll(4000))
        return any(event.ident == pid and event.fflags & select.KQ_NOTE_EXIT and not event.flags & select.KQ_EV_ERROR for event in watch.control(None, 1, 4))
    finally:
        if fd is not None:
            os.close(fd)
        elif watch is not None:
            watch.close()
record('ARGV ' + json.dumps(args))
command = [real, '-S', socket, '-f', '/dev/null'] + args
if '-C' not in args:
    os.execv(real, command)
child = subprocess.Popen(command, stdin=subprocess.PIPE)
exit_failed = False
for line in sys.stdin.buffer:
    text = line.decode().rstrip('\n')
    record('CTL ' + text)
    if text.startswith('display-message -p -t %') and os.path.exists(arm):
        os.unlink(arm)
        with open(marker, 'w') as out:
            out.write('fedcba9876543210fedcba9876543210')
        record('RACE changed spawn nonce before metadata recheck')
    closing = arm_exit(text.split()[-1]) if text.startswith('kill-pane -t %') else None
    try:
        child.stdin.write(line)
        child.stdin.flush()
    except BrokenPipeError:
        break
    if closing is not None:
        try:
            observed = observed_exit(closing)
        except Exception as error:
            exit_failure(closing[0], repr(error))
            observed = False
        if not observed:
            exit_failure(closing[0], 'kernel exit event unobserved')
            exit_failed = True
            break
        record('PROVIDER_EXIT observed pid=%s' % closing[0])
child.stdin.close()
status = child.wait()
sys.exit(1 if exit_failed else status)
"#,
            real = serde_json::to_string(&self.real).unwrap(),
            socket = serde_json::to_string(&self.root.path().join("s")).unwrap(),
            log = serde_json::to_string(&self.root.path().join("control.log")).unwrap(),
            arm = serde_json::to_string(&self.root.path().join("revoke")).unwrap(),
            marker = serde_json::to_string(&marker).unwrap(),
        );
        std::fs::write(self.root.path().join("control.log"), b"").unwrap();
        let wrapper = self.root.path().join("tmux");
        std::fs::write(&wrapper, script).unwrap();
        std::fs::set_permissions(wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        PATH_GUARDS.with_borrow_mut(|guards| {
            guards.insert(
                self.root.path().to_path_buf(),
                Guard::prepend_path_after_shared_test_env_lock(self.root.path()),
            );
        });
        *installed = true;
    }

    fn start_with(&self, command: &str) {
        self.install();
        assert!(
            self.panes.lock().unwrap().is_none(),
            "private old pane already started"
        );
        let reusable = match observe_spawn_nonce_marker(&self.logical) {
            SpawnNonceMarker::Known(nonce) => {
                execution_context("claude", &nonce).is_ok_and(|context| {
                    context.tmux_session == self.logical && context.channel_id == Some(self.channel)
                })
            }
            _ => false,
        };
        if !reusable {
            let prepared = PreparedIncarnation::prepare(
                "claude",
                &self.logical,
                Some(self.channel),
                self.native.as_deref(),
                false,
            )
            .unwrap();
            crate::services::discord::stamp_spawn_markers(&self.logical, Some(&prepared)).unwrap();
        }
        let primary = self.checked(&[
            "new-session",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-s",
            &self.logical,
            "-n",
            "old",
            command,
        ]);
        let peer = self.checked(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            &format!("={}", self.logical),
            "-n",
            "peer",
            "exec sleep 300",
        ]);
        self.checked(&["select-window", "-t", &format!("={}:old", self.logical)]);
        let metadata = self.checked(&[
            "display-message",
            "-p",
            "-t",
            &primary,
            "#{pid} #{session_id} #{window_id} #{pane_id} #{pane_pid}",
        ]);
        eprintln!(
            "PRIVATE_OLD socket={} primary={primary} peer={peer} metadata={metadata} nonce={:?}",
            self.root.path().join("s").display(),
            observe_spawn_nonce_marker(&self.logical)
        );
        *self.panes.lock().unwrap() = Some((primary, peer));
    }

    pub(crate) fn start(&self) {
        self.start_with("exec sleep 300");
    }

    pub(crate) fn start_draft(&self) {
        let script = self.root.path().join("read-draft.sh");
        let submitted = self.root.path().join("submitted");
        std::fs::write(
            &script,
            format!(
                "#!/bin/bash\nIFS= read -r line\nprintf '%s' \"$line\" > {}\n",
                crate::services::process::shell_escape(&submitted.display().to_string())
            ),
        )
        .unwrap();
        self.start_with(&format!(
            "exec /bin/bash {}",
            crate::services::process::shell_escape(&script.display().to_string())
        ));
        let pane = self.panes.lock().unwrap().as_ref().unwrap().0.clone();
        self.checked(&[
            "send-keys",
            "-t",
            &pane,
            "-l",
            "PERSON DRAFT MUST NEVER RUN",
        ]);
        assert!(!submitted.exists());
        eprintln!(
            "PRIVATE_OLD_DRAFT seeded_without_enter submitted_file={}",
            submitted.display()
        );
    }

    pub(crate) fn revoke_nonce_on_recheck(&self) {
        std::fs::write(self.root.path().join("revoke"), b"armed").unwrap();
    }

    fn pane_exists(&self, peer: bool) -> bool {
        let panes = self.panes.lock().unwrap();
        let Some((primary, other)) = panes.as_ref() else {
            return false;
        };
        let pane = if peer { other } else { primary };
        let output = self.run(&["display-message", "-p", "-t", pane, "#{pane_id}"]);
        output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == pane
    }

    pub(crate) fn exists(&self) -> bool {
        self.pane_exists(false)
    }

    pub(crate) fn peer_exists(&self) -> bool {
        self.pane_exists(true)
    }

    fn log(&self) -> String {
        if !*self.installed.lock().unwrap() {
            return String::new();
        }
        std::fs::read_to_string(self.root.path().join("control.log")).unwrap()
    }

    pub(crate) fn closes(&self) -> usize {
        self.log()
            .lines()
            .filter(|line| line.starts_with("CTL kill-pane -t %"))
            .count()
    }

    pub(crate) fn assert_no_input(&self) {
        let log = self.log();
        eprintln!("PRIVATE_OLD_CONTROL\n{log}");
        assert_eq!(
            log.lines()
                .filter(|line| line.starts_with("PROVIDER_EXIT observed pid="))
                .count(),
            self.closes(),
            "each closed private pane needs its actual provider exit witness: {log}"
        );
        for forbidden in [
            "send-keys",
            "send-literal",
            "paste-buffer",
            "capture-pane",
            "/exit",
        ] {
            assert!(
                !log.contains(forbidden),
                "old pane operation {forbidden}: {log}"
            );
        }
    }

    pub(crate) fn view(self: &Arc<Self>) -> Box<dyn EarlierExecution> {
        Box::new(OldTmuxView(self.clone()))
    }
}

struct OldTmuxView(Arc<OldTmux>);
impl EarlierExecution for OldTmuxView {
    fn liveness(&mut self) -> Liveness {
        if self.0.exists() {
            Liveness::Running
        } else {
            Liveness::Ended
        }
    }
    fn send_exit(&mut self) -> ExitSend {
        panic!("forward handoff must not type into the old pane");
    }
}

impl Drop for OldTmux {
    fn drop(&mut self) {
        if self.panes.get_mut().unwrap().is_some() {
            let _ = self.command().args(["-N", "kill-server"]).status();
        }
        PATH_GUARDS.with_borrow_mut(|guards| {
            guards.remove(self.root.path());
        });
    }
}

#[test]
fn f3_a_person_draft_is_closed_without_reading_or_submitting_it_pg() {
    let root = crate::config::TestRuntimeRootGuard::new();
    let fx = Fixture::with_root("resume-f3-draft", None, root);
    let (session, path) = fx.seed_resumable(true);
    let launcher = Arc::new(Launcher::default());
    let ports = fx.ports(&launcher);
    ports.old_tmux.start_draft();
    let (result, _) = fx.turn(&HostedRecord::Legacy, &ports, || {
        if let Some(started) = fx.start_provider_as(&launcher, true, "resume") {
            assert_eq!((&started.session, &started.path), (&session, &path));
            fx.answer(&started);
        }
    });
    assert_eq!(result, Ok(()));
    assert_eq!(ports.old_tmux.closes(), 1);
    assert!(!ports.old_tmux.exists());
    assert!(ports.old_tmux.peer_exists());
    assert!(!ports.old_tmux.root.path().join("submitted").exists());
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
    assert_eq!(fx.row(), Some(HostedState::Bound));
    assert_eq!(fx.rig.sends(), prompt_sends());
    ports.old_tmux.assert_no_input();
}
