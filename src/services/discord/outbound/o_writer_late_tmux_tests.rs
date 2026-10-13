//! Real pane probes on a private tmux server, including the late Snapshot recheck.
//! PATH and runtime-root changes exist only in an isolated child process.

use super::*;
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::writer::adoption::{self, Hold};
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvent, BindingEvents, BindingEvidence, BindingRecord, BindingTarget,
};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CHILD: &str = "ADK_LATE_TMUX_ISOLATED_CHILD";
const REAL: &str = "ADK_LATE_TMUX_REAL_BINARY";
const SOCKET: &str = "ADK_LATE_TMUX_SOCKET";
const SESSION: &str = "ADK_LATE_TMUX_SESSION";
const FAIL: &str = "ADK_LATE_TMUX_FAIL_FILE";
const CALLS: &str = "ADK_LATE_TMUX_CALLS_FILE";

struct PrivateServer {
    binary: PathBuf,
    socket: String,
    socket_root: PathBuf,
}

impl Drop for PrivateServer {
    fn drop(&mut self) {
        // This cleanup always names the private socket, even after a child assertion fails.
        let _ = Command::new(&self.binary)
            .args(["-L", &self.socket, "kill-server"])
            .env("TMUX_TMPDIR", &self.socket_root)
            .env_remove("TMUX")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

fn real_tmux() -> PathBuf {
    let fixed = PathBuf::from("/opt/homebrew/bin/tmux");
    let binary = fixed.is_file().then_some(fixed).or_else(|| {
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("tmux"))
                .find(|file| file.is_file())
        })
    });
    std::fs::canonicalize(binary.expect("real tmux is required; this test must not soft-skip"))
        .unwrap()
}

fn child_case(name: &str) -> bool {
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return true;
    }
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::Builder::new()
        .prefix("o6325-")
        .tempdir_in("/tmp")
        .unwrap();
    let bin = root.path().join("bin");
    let sockets = root.path().join("sockets");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&sockets).unwrap();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let socket = format!("o6325-{}", &nonce[..12]);
    let session = format!("late6325_{}", &nonce[..12]);
    let server = PrivateServer {
        binary: real_tmux(),
        socket,
        socket_root: sockets,
    };
    let wrapper = bin.join("tmux");
    std::fs::write(
        &wrapper,
        concat!(
            "#!/bin/sh\n",
            "printf '%s\\n' \"-L $ADK_LATE_TMUX_SOCKET $*\" >> \"$ADK_LATE_TMUX_CALLS_FILE\"\n",
            "if [ -e \"$ADK_LATE_TMUX_FAIL_FILE\" ]; then\n",
            "  printf '%s\\n' 'injected private tmux probe failure' >&2\n",
            "  exit 70\n",
            "fi\n",
            "exec \"$ADK_LATE_TMUX_REAL_BINARY\" -L \"$ADK_LATE_TMUX_SOCKET\" \"$@\"\n",
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![bin];
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    let qualified = name.split_once("::").unwrap().1;
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", qualified, "--nocapture"])
        .env(CHILD, name)
        .env(REAL, &server.binary)
        .env(SOCKET, &server.socket)
        .env(SESSION, session)
        .env(FAIL, root.path().join("probe-fails"))
        .env(CALLS, root.path().join("calls"))
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("TMUX_TMPDIR", &server.socket_root)
        .env("AGENTDESK_ROOT_DIR", root.path().join("runtime"))
        .env_remove("TMUX")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let output = child.wait_with_output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                status.success(),
                "private tmux child failed:\n{stdout}\n{stderr}"
            );
            assert!(
                stdout.contains("ADK_LATE_TMUX_REAL_ORACLE_CHECKED"),
                "actual child oracle must run"
            );
            assert!(
                stdout.contains("1 passed; 0 failed; 0 ignored;"),
                "{stdout}"
            );
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("private tmux source/Snapshot test timed out");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(server);
    false
}

fn tmux(args: &[&str]) {
    let output = Command::new("tmux").args(args).output().unwrap();
    assert!(
        output.status.success(),
        "private tmux {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct RealPane {
    frontier: u64,
}

impl LegacyView for RealPane {
    fn started(&self) -> bool {
        true
    }
    fn cursor(&self, tmux: &str) -> LegacyCursor {
        late_source_cursor(tmux).unwrap_or(LegacyCursor::Unbound)
    }
    fn late_cursor(&self, tmux: &str) -> Result<LegacyCursor, String> {
        late_source_cursor(tmux)
    }
    fn frontier(&self, _: u64, _: &str, _: u64) -> Option<u64> {
        Some(self.frontier)
    }
    fn tail_running(&self, _: &str) -> bool {
        false
    }
}

struct Log(Vec<BindingEvent>);

impl BindingEvents for Log {
    fn binding_events_since(&self, channel: u64, after: u64) -> Result<Vec<BindingEvent>, String> {
        Ok(self
            .0
            .iter()
            .filter(|event| event.channel_id == channel && event.seq > after)
            .cloned()
            .collect())
    }
    fn subscribe(&self, _: u64) -> tokio::sync::watch::Receiver<u64> {
        tokio::sync::watch::channel(1).1
    }
}

fn log(path: &Path, session: &str, channel: u64) -> Log {
    Log(vec![BindingEvent {
        seq: 1,
        channel_id: channel,
        provider: ShadowProvider::Claude,
        tmux_session: session.into(),
        execution_nonce: "late-tmux-test".into(),
        record: BindingRecord::Bound {
            old: None,
            new: BindingTarget::Source(source_id_for("late-tmux", path).unwrap()),
            cause: BindingCause::Startup,
            parent_hint: None,
            evidence: BindingEvidence {
                hook_event: "session_start".into(),
                received_at: chrono::Utc::now(),
                reclaims: false,
            },
        },
        committed_at: chrono::Utc::now(),
    }])
}

#[test]
fn late_adopt_real_private_tmux_live_absent_error_and_snapshot_recheck() {
    if !child_case(concat!(
        module_path!(),
        "::late_adopt_real_private_tmux_live_absent_error_and_snapshot_recheck"
    )) {
        return;
    }
    let session = std::env::var(SESSION).unwrap();
    assert!(!session.contains("agentdesk"));
    let anchor = format!("{session}_anchor");
    tmux(&[
        "-f",
        "/dev/null",
        "new-session",
        "-d",
        "-s",
        &anchor,
        "/bin/sleep 120",
    ]);
    tmux(&["new-session", "-d", "-s", &session, "/bin/sleep 120"]);
    assert_eq!(late_source_cursor(&session).unwrap(), LegacyCursor::Unbound);

    let transcript = tempfile::NamedTempFile::new().unwrap();
    let rows = [
        serde_json::json!({"type":"user","uuid":"late-tmux-q","message":{"content":"asked"}}),
        serde_json::json!({"type":"assistant","uuid":"late-tmux-a","apiBlockIndex":0,"message":{"id":"late-tmux-old","content":[{"type":"text","text":"old"}]}}),
        serde_json::json!({"type":"system","subtype":"turn_duration","durationMs":5}),
    ].iter().map(|row| format!("{row}\n")).collect::<String>();
    std::fs::write(transcript.path(), &rows).unwrap();
    let channel = 6_325_912;
    let events = log(transcript.path(), &session, channel);
    let legacy = RealPane {
        frontier: rows.len() as u64,
    };
    let refused = adoption::pin_late(&legacy, &events.0, channel).unwrap_err();
    assert_eq!(
        refused.hold,
        Hold::Retry,
        "real live pane lacks a published source cursor"
    );

    tmux(&["kill-session", "-t", &session]);
    assert_eq!(late_source_cursor(&session).unwrap(), LegacyCursor::NoPane);
    let snapshot = adoption::pin_late(&legacy, &events.0, channel).unwrap();
    assert_eq!(snapshot.start(), rows.len() as u64);
    assert!(snapshot.owed().is_none() && snapshot.abandoned().is_none());
    assert!(snapshot.recheck(&legacy, &events, channel).is_ok());

    crate::services::tmux_common::with_tmux_source_authority(&session, |_| {
        let candidate = crate::services::tui_o::channel_policy::Candidate::reserved_late();
        let _adoption = candidate.lock();
        let started = Instant::now();
        let refused = snapshot.recheck(&legacy, &events, channel).unwrap_err();
        assert_eq!(
            refused.hold,
            Hold::Retry,
            "locked final recheck retries source contention"
        );
        assert!(refused.to_string().contains("source authority is busy"));
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "final recheck must not block on SourceAuthority"
        );
        let other = format!("{session}_other_absent");
        assert_eq!(
            late_source_cursor(&other).unwrap(),
            LegacyCursor::NoPane,
            "another source lookup progresses while this source and Candidate stay locked"
        );
    });

    tmux(&["new-session", "-d", "-s", &session, "/bin/sleep 120"]);
    assert_eq!(late_source_cursor(&session).unwrap(), LegacyCursor::Unbound);
    assert!(
        snapshot.recheck(&legacy, &events, channel).is_err(),
        "actual pane reappearance invalidates NoPane pin"
    );
    tmux(&["kill-session", "-t", &session]);
    let snapshot = adoption::pin_late(&legacy, &events.0, channel).unwrap();
    let fail = PathBuf::from(std::env::var_os(FAIL).unwrap());
    std::fs::write(&fail, b"fail only private wrapper probes").unwrap();
    assert!(
        late_source_cursor(&session).is_err(),
        "actual failed command is never NoPane"
    );
    assert!(
        snapshot.recheck(&legacy, &events, channel).is_err(),
        "ProbeError refuses late publication"
    );
    std::fs::remove_file(fail).unwrap();

    let calls = std::fs::read_to_string(std::env::var_os(CALLS).unwrap()).unwrap();
    let prefix = format!("-L {} ", std::env::var(SOCKET).unwrap());
    assert!(
        calls.lines().all(|line| line.starts_with(&prefix)),
        "every tmux call carries the unique socket"
    );
    assert!(
        calls.contains("has-session") && calls.contains("list-panes"),
        "production probe traversed the private wrapper"
    );
    println!("ADK_LATE_TMUX_REAL_ORACLE_CHECKED");
}
