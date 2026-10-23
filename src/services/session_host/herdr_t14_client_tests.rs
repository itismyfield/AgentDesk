//! Pinned scratch-only Herdr server and native terminal client for the ignored T14 scenarios.
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::session_hosts::BootSessionHosts;
use crate::services::session_host::herdr::{observe, pane_probe::start_text, provenance};
use crate::services::session_host::herdr_registry::{self, HerdrRegistry};

const PINNED_SHA256: &str = "5173a3e0ae42d5d1ab7ebfa5d5e6329f7c3d23f8e1a3677c7ce3231da2884157";
const FAKE_TEMPLATE: &str = r####"#!/usr/bin/python3
"""Offline native-stdin provider for the isolated T14 client acceptance fixture."""
import base64
import datetime
import hashlib
import json
import os
import pathlib
import select
import sys
import termios
import time
import traceback
import tty
import urllib.parse
import urllib.request
import uuid

ROOT = pathlib.Path(__ROOT__).resolve()
CASE = pathlib.Path(__CASE__).resolve()
PROVIDER = __PROVIDER__
SCRIPT_PROVIDER = pathlib.Path(sys.argv[0]).name

def append(path, value):
    data = (json.dumps(value, separators=(",", ":")) + "\n").encode()
    with open(path, "ab", buffering=0) as f:
        f.write(data)
        os.fsync(f.fileno())

def atomic(path, value):
    temp = pathlib.Path(str(path) + ".tmp")
    with open(temp, "w") as f:
        json.dump(value, f)
        f.flush()
        os.fsync(f.fileno())
    os.replace(temp, path)

def provider_error(kind, value, trace):
    atomic(CASE / "provider-error.json", {"type": kind.__name__, "message": str(value),
        "traceback": "".join(traceback.format_exception(kind, value, trace))})
    sys.__excepthook__(kind, value, trace)

sys.excepthook = provider_error

def wait_file(path):
    end = time.monotonic() + 90
    while not pathlib.Path(path).is_file():
        if time.monotonic() >= end:
            raise RuntimeError("T14 missing owned control/release: " + str(path))
        time.sleep(0.025)

def stamp():
    return datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z")

if sys.argv[1:] == ["--version"]:
    print("codex-cli 0.160.0" if SCRIPT_PROVIDER == "codex" else "2.1.81 (Claude Code)")
    raise SystemExit(0)
if SCRIPT_PROVIDER == "codex" and sys.argv[1:] == ["resume", "--help"]:
    print("--dangerously-bypass-hook-trust --no-daemon")
    raise SystemExit(0)
if SCRIPT_PROVIDER != PROVIDER:
    append(CASE / "unexpected-provider.jsonl", {"pid": os.getpid(), "argv": sys.argv})
    raise SystemExit(97)
expected_isolation = {
    "PATH": str(ROOT / "bin") + ":/usr/bin:/bin",
    "HOME": str(CASE / "home"),
    "CODEX_HOME": str(CASE / ("home/.adk/profiles/codex/t14-empty" if PROVIDER == "codex" else "codex-home")),
    "CLAUDE_CONFIG_DIR": str(CASE / "claude-config"),
    "XDG_CONFIG_HOME": str(CASE / "home/xdg"),
    "XDG_STATE_HOME": str(CASE / "state"),
    "XDG_DATA_HOME": str(CASE / "data"),
    "XDG_CACHE_HOME": str(CASE / "cache"),
    "XDG_RUNTIME_DIR": str(CASE / "run"),
    "TMPDIR": str(CASE / "tmp"),
    "SHELL": "/bin/bash",
}
isolation = {key: os.environ.get(key) for key in expected_isolation}
atomic(CASE / "provider-isolation.json", {"pid": os.getpid(), "expected": expected_isolation,
    "isolation": isolation, "matches": isolation == expected_isolation})
assert isolation == expected_isolation, "T14 actual provider exec environment must remain sealed"
context = pathlib.Path(os.environ["AGENTDESK_BINDING_CONTEXT"]).resolve(strict=True)
assert context.is_relative_to(ROOT), "T14 context must be in the owned scratch"
herdr_all_env = {k: v for k, v in os.environ.items() if k.startswith("HERDR_")}
herdr_env = {k: os.environ[k] for k in ("HERDR_ENV", "HERDR_PANE_ID", "HERDR_BIN_PATH", "HERDR_SOCKET_PATH", "HERDR_TAB_ID", "HERDR_WORKSPACE_ID", "HERDR_CONFIG_PATH") if k in os.environ}
assert not herdr_env, "T14 ADK provider must have Herdr injection variables removed"
assert not herdr_all_env, "T14 provider must not inherit server-only diagnostics"
seen = {"pid": os.getpid(), "pgid": os.getpgrp(), "ppid": os.getppid(),
        "provider": PROVIDER, "script": str(pathlib.Path(sys.argv[0]).resolve()),
        "argv": sys.argv, "context": str(context), "context_sha256": hashlib.sha256(context.read_bytes()).hexdigest(),
        "herdr_env": herdr_env, "herdr_all_env": herdr_all_env, "interpreter": sys.executable,
        "isolation": isolation}
atomic(CASE / "context-seen.json", seen)
append(CASE / "provider-starts.jsonl", seen)
wait_file(CASE / "control.json")
control = json.loads((CASE / "control.json").read_text())
assert control["provider"] == PROVIDER
assert len(control["prompts"]) == len(control["answers"]) == 3
assert hashlib.sha256(context.read_bytes()).hexdigest() == seen["context_sha256"]
context_data = json.loads(context.read_text())
assert context_data["provider"] == PROVIDER
# BindingContext.host identifies the execution node; the actual pane proves its backend.
assert context_data["host"] == control["execution_node"]
assert context_data["launch_mode"] == "fresh"
args = sys.argv[1:]
assert not any(arg in ("resume", "continue") or arg.startswith(("--resume", "--continue")) for arg in args)
if PROVIDER == "claude":
    at = args.index("--session-id")
    assert args[at + 1] == control["session_id"], "T14 native session came from the actual prepared argv"
if context_data.get("expected_native_session_id") is not None:
    assert context_data["expected_native_session_id"] == control["session_id"]
uuid.UUID(control["session_id"])
envelope = control["binding_envelope"]
decoded = json.loads(base64.urlsafe_b64decode(envelope + "=" * (-len(envelope) % 4)))
assert decoded["context"]["Captured"] == context_data, "T14 hook header captures this actual exec context"
source = pathlib.Path(control["transcript_path"]).resolve()
assert source.is_relative_to(ROOT)
source.parent.mkdir(parents=True, exist_ok=True)
assert not source.exists(), "T14 provider alone creates its native source"
parsed = urllib.parse.urlparse(control["hook_url"])
assert parsed.scheme == "http" and parsed.hostname == "127.0.0.1" and parsed.port != 8791
assert parsed.path in ("", "/") and not parsed.query and not parsed.fragment
session = control["session_id"]
logical = json.loads(context.read_text())["tmux_session"]

def hook(event, extra):
    payload = {"hook_event_name": event, "session_id": session, "cwd": os.getcwd(), "transcript_path": str(source)}
    payload.update(extra)
    url = control["hook_url"].rstrip("/") + "/hooks/" + PROVIDER + "/" + event + "?session_id=" + urllib.parse.quote(logical)
    request = urllib.request.Request(url, json.dumps(payload).encode(),
        {"content-type": "application/json", "x-agentdesk-binding-context": control["binding_envelope"]}, method="POST")
    # Empty proxies make the actual HTTP request local even if a caller's environment is unusual.
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    with opener.open(request, timeout=10) as response:
        assert response.status == 202, "T14 actual hook ingress must accept"
        response.read()
    append(CASE / "hook-receipts.jsonl", {"event": event, "url": url, "payload": payload, "status": 202})

def startup():
    if PROVIDER == "codex":
        append(source, {"timestamp": stamp(), "type": "session_meta", "payload": {
            "id": session, "session_id": session, "cwd": os.getcwd(), "originator": "codex-tui",
            "cli_version": "0.160.0", "source": "cli"}})
    else:
        with open(source, "xb") as f:
            f.flush()
            os.fsync(f.fileno())
    hook("SessionStart", {"source": "startup"})

def ready(answer=""):
    if PROVIDER == "codex":
        text = answer + "\r\n\r\n›\r\n\r\n  gpt-5.5 xhigh · " + os.getcwd()
    else:
        line = "────────────────────────────────────────────────────"
        text = answer + "\r\n" + line + "\r\n❯ \r\n" + line + "\r\n  🤖 Opus(H) │ 7% │ MCP: 2"
    os.write(1, ("\x1b[2J\x1b[H\x1b[?2004h" + text).encode())

original = termios.tcgetattr(0)
tty.setraw(0)
try:
    if PROVIDER == "claude":
        startup()
    ready("earlier output")
    atomic(CASE / "provider-ready.json", {"pid": os.getpid(), "provider": PROVIDER})
    raw = bytearray()
    seq = 0
    bracket = False
    while seq < 3:
        chunk = os.read(0, 1)
        if not chunk:
            raise RuntimeError("T14 stdin ended before its three real inputs")
        raw.extend(chunk)
        if raw.endswith(b"\x1b[200~"):
            bracket = True
        elif raw.endswith(b"\x1b[201~"):
            bracket = False
        if chunk != b"\r" or bracket:
            continue
        seq += 1
        prompt = bytes(raw[:-1]).replace(b"\x1b[200~", b"").replace(b"\x1b[201~", b"").decode("utf-8")
        assert control["prompts"][seq - 1] in prompt, "T14 actual stdin carries the expected prompt"
        append(CASE / "inputs.jsonl", {"seq": seq, "raw_bytes": list(raw), "prompt": prompt})
        raw.clear()
        if PROVIDER == "codex" and seq == 1:
            startup()
        turn = str(uuid.uuid4())
        if PROVIDER == "claude":
            append(source, {"timestamp": stamp(), "sessionId": session, "type": "user", "uuid": turn,
                "message": {"role": "user", "content": prompt}})
            direct = {"prompt": prompt, "prompt_id": turn}
        else:
            for record in [
                {"type": "event_msg", "payload": {"type": "task_started", "turn_id": turn}},
                {"type": "turn_context", "payload": {"turn_id": turn}},
                {"timestamp": stamp(), "type": "response_item", "payload": {"id": str(uuid.uuid4()),
                    "type": "message", "role": "user", "content": [{"type": "input_text", "text": prompt}],
                    "internal_chat_message_metadata_passthrough": {"turn_id": turn, "create_time": time.time(), "content_item_kinds": ["user.text"]}}},
                {"type": "event_msg", "payload": {"type": "item_completed", "turn_id": turn, "item": {"type": "UserMessage"}}},
            ]:
                append(source, record)
            direct = {"prompt": prompt, "turn_id": turn}
        if seq == 2:
            hook("UserPromptSubmit", direct)
        wait_file(CASE / ("answer-" + str(seq) + ".release"))
        if seq == 2:
            # Same actual hook identity replays after the live claim, before its answer is complete.
            hook("UserPromptSubmit", direct)
        answer = control["answers"][seq - 1]
        if PROVIDER == "claude":
            for record in [
                {"type": "assistant", "uuid": "t14-" + str(uuid.uuid4()), "apiBlockIndex": 0,
                    "message": {"id": str(uuid.uuid4()), "role": "assistant", "stop_reason": "end_turn", "content": [{"type": "text", "text": answer}]}},
                {"type": "system", "subtype": "stop_hook_summary", "hookCount": 1, "hasOutput": True},
                {"type": "system", "subtype": "turn_duration"},
            ]:
                record.update({"timestamp": stamp(), "sessionId": session})
                append(source, record)
        else:
            append(source, {"timestamp": stamp(), "type": "response_item", "payload": {
                "id": str(uuid.uuid4()), "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": answer}]}})
            append(source, {"type": "event_msg", "payload": {"type": "task_complete", "turn_id": turn, "last_agent_message": answer}})
        ready(answer)
    # Keep the actual provider and pane identity alive through all post-roundtrip assertions.
    while os.read(0, 1):
        pass
finally:
    termios.tcsetattr(0, termios.TCSANOW, original)
"####;
const PTY_DRIVER: &str = r####"#!/usr/bin/python3
"""Native Herdr terminal attach driven through a real OS PTY, never socket input RPC."""
import fcntl
import json
import os
import pathlib
import pty
import select
import struct
import sys
import termios
import time

binary, terminal, screen_path, receipt_path = sys.argv[1:]
assert pathlib.Path(binary).is_absolute()
assert all(pathlib.Path(p).is_absolute() for p in (screen_path, receipt_path))

def atomic(value):
    temp = receipt_path + ".tmp"
    with open(temp, "w") as f:
        json.dump(value, f)
        f.flush()
        os.fsync(f.fileno())
    os.replace(temp, receipt_path)

pid, master = pty.fork()
if pid == 0:
    os.execve(binary, [binary, "terminal", "attach", terminal], os.environ.copy())
fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 36, 140, 0, 0))
receipt = {"driver_pid": os.getpid(), "pid": pid, "pgid": os.getpgid(pid), "terminal_id": terminal,
           "binary": binary, "argv": [binary, "terminal", "attach", terminal]}
atomic(receipt)
with open(screen_path, "ab", buffering=0) as screen:
    try:
        running = True
        commands = bytearray()
        while running:
            readable, _, _ = select.select([master, 0], [], [], 0.25)
            if master in readable:
                try:
                    data = os.read(master, 65536)
                except OSError:
                    data = b""
                if not data:
                    break
                screen.write(data)
                os.fsync(screen.fileno())
            if 0 in readable:
                data = os.read(0, 65536)
                if not data:
                    raise RuntimeError("T14 driver control closed without native detach")
                commands.extend(data)
                while b"\n" in commands:
                    line, _, remaining = commands.partition(b"\n")
                    commands = bytearray(remaining)
                    command = json.loads(line)
                    if command["op"] == "send":
                        text = command["prompt"]
                        assert "\r" not in text and "\x1b" not in text
                        os.write(master, text.encode("utf-8") + b"\r")
                    elif command["op"] == "detach":
                        os.write(master, b"\x02q")
                    else:
                        raise RuntimeError("T14 unknown native client command")
            observed, status = os.waitpid(pid, os.WNOHANG)
            if observed:
                receipt["wait_status"] = status
                atomic(receipt)
                running = False
        if "wait_status" not in receipt:
            _, receipt["wait_status"] = os.waitpid(pid, 0)
            atomic(receipt)
    finally:
        os.close(master)
"####;

fn sha(path: &Path) -> String {
    format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()))
}

fn ps(pid: u32, field: &str) -> String {
    let output = Command::new("/bin/ps")
        .args(["-o", field, "-p", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn ps_proves_absence(status: Option<i32>, stdout: &[u8], stderr: &[u8]) -> bool {
    status == Some(1)
        && stdout.iter().all(u8::is_ascii_whitespace)
        && stderr.iter().all(u8::is_ascii_whitespace)
}

fn confirmed_absent(pid: u32) -> bool {
    Command::new("/bin/ps")
        .args(["-o", "pid=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|output| ps_proves_absence(output.status.code(), &output.stdout, &output.stderr))
}

fn wait_for(mut ready: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(45);
    while !ready() {
        assert!(Instant::now() < deadline, "T14 never observed {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn json_file(path: &Path) -> Option<Value> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

fn json_lines(path: &Path) -> Vec<Value> {
    let bytes = std::fs::read(path).unwrap_or_default();
    // A writer appends one fsynced line; do not interpret its incomplete final write as a record.
    bytes
        .split_inclusive(|byte| *byte == b'\n')
        .filter(|line| line.last() == Some(&b'\n'))
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect()
}

fn atomic_json(path: &Path, value: &Value) {
    let temp = path.with_extension("tmp");
    let mut file = std::fs::File::create(&temp).unwrap();
    serde_json::to_writer(&mut file, value).unwrap();
    file.sync_all().unwrap();
    std::fs::rename(&temp, path).unwrap();
}

fn executable(path: &Path, bytes: &[u8]) {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        assert!(meta.is_file() && !meta.file_type().is_symlink());
    }
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[derive(Clone)]
struct Ledger {
    path: PathBuf,
    owned: Arc<Mutex<Vec<(u32, String)>>>,
}

impl Ledger {
    fn append(&self, value: Value) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .unwrap();
        writeln!(file, "{value}").unwrap();
        file.sync_all().unwrap();
        println!("T14_EVENT {value}");
    }

    fn record(&self, pid: u32, role: &str) {
        let start = start_text(provenance::process_start(pid).unwrap().identity);
        let pgid = ps(pid, "pgid=").parse::<u32>().unwrap();
        let exe = provenance::process_exec_path(pid).map(PathBuf::from);
        self.append(json!({"event":"process", "pid":pid, "pgid":pgid, "start":start,
            "role":role, "exe":exe, "exe_sha256":exe.as_deref().filter(|path|path.is_file()).map(sha)}));
        let mut owned = self.owned.lock().unwrap();
        if !owned
            .iter()
            .any(|(seen, stamp)| *seen == pid && stamp == &start)
        {
            owned.push((pid, start));
        }
    }

    fn remaining(&self) -> Vec<u32> {
        self.owned
            .lock()
            .unwrap()
            .iter()
            .filter(|(pid, start)| match provenance::process_start(*pid) {
                Ok(now) => start_text(now.identity) == *start,
                Err(_) => !confirmed_absent(*pid),
            })
            .map(|(pid, _)| *pid)
            .collect()
    }

    fn terminate_recorded(&self, pid: u32) {
        let expected = self
            .owned
            .lock()
            .unwrap()
            .iter()
            .find(|(owned, _)| *owned == pid)
            .map(|(_, stamp)| stamp.clone());
        if expected.is_some_and(|expected| {
            provenance::process_start(pid).is_ok_and(|now| start_text(now.identity) == expected)
        }) {
            let _ = Command::new("/bin/kill")
                .args(["-TERM", &pid.to_string()])
                .status();
        }
    }
}

pub(crate) struct RealServer {
    root: PathBuf,
    bin: PathBuf,
    dir: PathBuf,
    home: PathBuf,
    provider: &'static str,
    fake: PathBuf,
    context_seen: PathBuf,
    input_receipts: PathBuf,
    stopped: bool,
    ledger: Ledger,
    child: Option<Child>,
    lease: PathBuf,
    lease_bytes: Vec<u8>,
}

impl RealServer {
    pub(crate) fn new(provider: &'static str) -> Self {
        assert!(matches!(provider, "claude" | "codex"));
        let read = |key| PathBuf::from(std::env::var_os(key).unwrap_or_else(|| panic!("{key}")));
        let root = read("ADK_TEST_HERDR_ROOT").canonicalize().unwrap();
        assert_eq!(root.parent(), Some(Path::new("/private/tmp")));
        let owner = root
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .strip_prefix("adk-e-t14-")
            .unwrap();
        assert!(!owner.is_empty() && owner.bytes().all(|byte| byte.is_ascii_digit()));
        let bin = read("ADK_TEST_HERDR_BIN").canonicalize().unwrap();
        assert_eq!(
            bin,
            root.join("bin/herdr"),
            "T14 executes the approved scratch copy only"
        );
        assert_eq!(sha(&bin), PINNED_SHA256, "T14 pinned 0.9.3 image");
        let path = read("ADK_TEST_HERDR_PIDS");
        assert_eq!(path.parent().unwrap().canonicalize().unwrap(), root);
        for file in std::fs::read_dir(&root).unwrap() {
            let path = file.unwrap().path();
            if path.to_string_lossy().ends_with(".pids.jsonl") {
                for event in json_lines(&path) {
                    if event["event"] == "process" {
                        let pid = u32::try_from(event["pid"].as_u64().unwrap()).unwrap();
                        assert!(
                            ps(pid, "pid=").is_empty(),
                            "T14 prior scratch process still lives: {pid}"
                        );
                    }
                }
            }
        }
        let dir = root.join(format!("run-{provider}-{}", std::process::id()));
        assert!(!dir.exists(), "T14 never reuses a server directory");
        for child in [
            "home/xdg",
            "codex-home",
            "claude-config",
            "state",
            "data",
            "cache",
            "run",
            "tmp",
            "work",
        ] {
            std::fs::create_dir_all(dir.join(child)).unwrap();
        }
        let lease = root.join("provider-fixture.lock");
        let lease_bytes = serde_json::to_vec(&json!({"pid":std::process::id(),"dir":dir})).unwrap();
        let mut lock = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lease)
            .unwrap();
        lock.write_all(&lease_bytes).unwrap();
        lock.sync_all().unwrap();
        let home = dir.join("home");
        let sealed = format!("{}:/usr/bin:/bin", root.join("bin").display());
        let rc = format!(
            "export PATH='{sealed}'\nexport CODEX_HOME='{}'\nexport CLAUDE_CONFIG_DIR='{}'\nunset HERDR_LOG\n",
            dir.join("codex-home").display(),
            dir.join("claude-config").display()
        );
        for file in [".zshrc", ".zprofile", ".bash_profile", ".profile"] {
            std::fs::write(home.join(file), &rc).unwrap();
        }
        std::fs::write(dir.join("network.sb"), "(version 1)\n(allow default)\n(deny network-outbound)\n(allow network-outbound (remote ip \"localhost:*\") (remote unix-socket))\n").unwrap();
        std::fs::write(home.join("config.toml"), observe::CANONICAL_CONFIG).unwrap();
        std::fs::File::options()
            .write(true)
            .open(home.join("config.toml"))
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(60))
            .unwrap();
        let script = FAKE_TEMPLATE
            .replace("__ROOT__", &serde_json::to_string(&root).unwrap())
            .replace("__CASE__", &serde_json::to_string(&dir).unwrap())
            .replace("__PROVIDER__", &serde_json::to_string(provider).unwrap());
        for program in ["claude", "codex"] {
            executable(&root.join("bin").join(program), script.as_bytes());
        }
        executable(&root.join("bin/pty_driver.py"), PTY_DRIVER.as_bytes());
        let fake = root.join("bin").join(provider);
        let context_seen = dir.join("context-seen.json");
        let input_receipts = dir.join("inputs.jsonl");
        Self {
            root,
            bin,
            dir,
            home,
            provider,
            fake,
            context_seen,
            input_receipts,
            stopped: false,
            ledger: Ledger {
                path,
                owned: Arc::new(Mutex::new(Vec::new())),
            },
            child: None,
            lease,
            lease_bytes,
        }
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }
    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }
    pub(crate) fn home(&self) -> &Path {
        &self.home
    }
    pub(crate) fn fake_provider(&self) -> &Path {
        &self.fake
    }
    pub(crate) fn context_seen_path(&self) -> &Path {
        &self.context_seen
    }
    pub(crate) fn input_receipts_path(&self) -> &Path {
        &self.input_receipts
    }
    pub(crate) fn session(&self) -> &str {
        "agentdesk-t14"
    }

    pub(crate) fn socket(&self) -> PathBuf {
        let path = self.dir.join("run/h.sock");
        assert!(
            path.parent()
                .unwrap()
                .canonicalize()
                .unwrap()
                .starts_with(&self.root)
        );
        if path.exists() {
            assert!(path.canonicalize().unwrap().starts_with(&self.root));
        }
        path
    }

    fn sandboxed(&self) -> Command {
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command
            .arg("-f")
            .arg(self.dir.join("network.sb"))
            .env_clear()
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.join("bin").display()),
            )
            .env("HOME", &self.home)
            .env("CODEX_HOME", self.dir.join("codex-home"))
            .env("CLAUDE_CONFIG_DIR", self.dir.join("claude-config"))
            .env("XDG_CONFIG_HOME", self.home.join("xdg"))
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env("XDG_DATA_HOME", self.dir.join("data"))
            .env("XDG_CACHE_HOME", self.dir.join("cache"))
            .env("XDG_RUNTIME_DIR", self.dir.join("run"))
            .env("TMPDIR", self.dir.join("tmp"))
            .env("HERDR_CONFIG_PATH", self.home.join("config.toml"))
            .env("HERDR_SOCKET_PATH", self.socket())
            .env("HERDR_LOG", "herdr=debug")
            .env("TERM", "xterm-256color")
            .env("LANG", "en_US.UTF-8")
            .env("SHELL", "/bin/bash")
            .current_dir(self.dir.join("work"))
            .process_group(0);
        command
    }

    fn preflight(&self) {
        let out = self.dir.join("preflight.stdout");
        let err = self.dir.join("preflight.stderr");
        let curl_out = self.dir.join("preflight.curl.stdout");
        let curl_err = self.dir.join("preflight.curl.stderr");
        let path = format!("{}:/usr/bin:/bin", self.root.join("bin").display());
        let script = format!(
            "IFS= read -r token; [ \"$token\" = t14-preflight ] || exit 90\ncodex=$(command -v codex); claude=$(command -v claude); helper=$(command -v env)\nprintf 'codex=%s\\nclaude=%s\\nenv=%s\\nPATH=%s\\nHOME=%s\\nCODEX_HOME=%s\\nCLAUDE_CONFIG_DIR=%s\\n' \"$codex\" \"$claude\" \"$helper\" \"$PATH\" \"$HOME\" \"$CODEX_HOME\" \"$CLAUDE_CONFIG_DIR\"\n[ \"$codex\" = '{}' ] && [ \"$claude\" = '{}' ] && [ \"$helper\" = /usr/bin/env ] && [ \"$PATH\" = '{path}' ] || exit 91\n/usr/bin/curl -m 3 -sS https://example.com > '{}' 2> '{}'\nresult=$?; printf 'curl_status=%s\\n' \"$result\"; [ \"$result\" -ne 0 ]\n",
            self.root.join("bin/codex").display(),
            self.root.join("bin/claude").display(),
            curl_out.display(),
            curl_err.display()
        );
        let mut child = self
            .sandboxed()
            .args(["/bin/bash", "-l", "-c", &script])
            .stdin(Stdio::piped())
            .stdout(std::fs::File::create(&out).unwrap())
            .stderr(std::fs::File::create(&err).unwrap())
            .spawn()
            .unwrap();
        self.ledger.record(child.id(), "sandbox-login-preflight");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"t14-preflight\n")
            .unwrap();
        let status = child.wait().unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        let observed = |key: &str| text.lines().find_map(|line| line.strip_prefix(key));
        let curl_status = observed("curl_status=").and_then(|code| code.parse::<i32>().ok());
        let profile = self.dir.join("network.sb");
        self.ledger.append(json!({"event":"isolation_preflight","pid":child.id(),"sandbox_profile":profile,
            "sandbox_profile_sha256":sha(&profile),"path":observed("PATH="),"codex":observed("codex="),
            "claude":observed("claude="),"env":observed("env="),"curl_status":curl_status,"status":status.code(),
            "stdout":out,"stdout_sha256":sha(&out),"stderr":err,"stderr_sha256":sha(&err),"curl_stdout":curl_out,"curl_stderr":curl_err}));
        assert!(
            status.success() && curl_status.is_some_and(|code| code != 0),
            "T14 sandbox preflight failed: {text}"
        );
        let expected = format!(
            "codex={}\nclaude={}\nenv=/usr/bin/env\nPATH={path}\nHOME={}\nCODEX_HOME={}\nCLAUDE_CONFIG_DIR={}\ncurl_status={}\n",
            self.root.join("bin/codex").display(),
            self.root.join("bin/claude").display(),
            self.home.display(),
            self.dir.join("codex-home").display(),
            self.dir.join("claude-config").display(),
            curl_status.unwrap()
        );
        assert_eq!(text, expected, "T14 exact sealed login environment");
    }

    pub(crate) fn start(&mut self) {
        assert!(self.child.is_none());
        self.preflight();
        let out = std::fs::File::create(self.dir.join("server.out")).unwrap();
        let child = self
            .sandboxed()
            .arg(&self.bin)
            .arg("server")
            .stdin(Stdio::null())
            .stdout(out.try_clone().unwrap())
            .stderr(out)
            .spawn()
            .unwrap();
        let pid = child.id();
        self.child = Some(child);
        self.ledger.record(pid, "server");
        wait_for(
            || UnixStream::connect(self.socket()).is_ok(),
            "owned server socket",
        );
        let (pid, start) = self.witness();
        self.ledger.append(
            json!({"event":"server","pid":pid,"pgid":ps(pid,"pgid=").parse::<u32>().unwrap(),"start":start,
            "socket":self.socket(),"exe":self.bin,"sha256":sha(&self.bin),"dir":self.dir}),
        );
        // E7 compares config mtime with the server's second-resolution OS start identity.
        std::thread::sleep(Duration::from_millis(1200));
    }

    fn witness(&self) -> (u32, String) {
        let socket = self.socket();
        let stream = UnixStream::connect(&socket).unwrap();
        let (pid, start) = provenance::socket_peer(&stream).unwrap();
        assert_eq!(pid, self.child.as_ref().unwrap().id());
        let exe = PathBuf::from(provenance::process_exec_path(pid).unwrap())
            .canonicalize()
            .unwrap();
        assert_eq!(exe, self.bin, "T14 W3 peer is the copied image");
        assert_eq!(sha(&exe), PINNED_SHA256);
        (pid, start_text(start.identity))
    }

    fn read_rpc(&self, method: &str, params: Value) -> Value {
        assert!(matches!(
            method,
            "session.snapshot" | "pane.get" | "pane.process_info"
        ));
        let socket = self.socket();
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (pid, stamp) = provenance::socket_peer(&stream).unwrap();
        let (expected, start) = self.witness();
        assert_eq!((pid, start_text(stamp.identity)), (expected, start.clone()));
        let id = format!("t14-read-{method}");
        serde_json::to_writer(
            &mut stream,
            &json!({"id":id,"method":method,"params":params}),
        )
        .unwrap();
        stream.write_all(b"\n").unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["id"], id);
        assert!(reply["error"].is_null(), "T14 read-only RPC: {reply}");
        self.ledger.append(
            json!({"event":"read","method":method,"id":id,"pid":pid,"start":start,
            "socket":socket,"exe":self.bin,"sha256":sha(&self.bin),"result":reply["result"]}),
        );
        reply["result"].clone()
    }

    pub(crate) fn installer(&self, boot: BootSessionHosts) -> impl Fn() + Send + Sync + 'static {
        let root = self.root.clone();
        move || {
            std::mem::forget(crate::services::codex::isolated_herdr_path_for_test(&root));
            std::mem::forget(herdr_registry::force_for_test(HerdrRegistry::build(&boot)));
        }
    }

    pub(crate) fn provider_ready(&mut self) -> Value {
        let path = self.dir.join("context-seen.json");
        wait_for(
            || json_file(&path).is_some(),
            "fake provider exec-time context",
        );
        let seen = json_file(&path).unwrap();
        assert_eq!(seen["provider"], self.provider);
        assert_eq!(seen["script"], self.fake.display().to_string());
        assert_eq!(seen["herdr_env"], json!({}));
        let context = PathBuf::from(seen["context"].as_str().unwrap())
            .canonicalize()
            .unwrap();
        assert!(context.starts_with(&self.root));
        assert_eq!(seen["context_sha256"], sha(&context));
        let pid = u32::try_from(seen["pid"].as_u64().unwrap()).unwrap();
        let ppid = u32::try_from(seen["ppid"].as_u64().unwrap()).unwrap();
        self.ledger.record(ppid, "provider-parent-shell");
        self.ledger.record(pid, "provider");
        let exe = PathBuf::from(provenance::process_exec_path(pid).unwrap())
            .canonicalize()
            .unwrap();
        self.ledger.append(json!({"event":"provider","pid":pid,"pgid":ps(pid,"pgid=").parse::<u32>().unwrap(),
            "start":start_text(provenance::process_start(pid).unwrap().identity),"exe":exe,"exe_sha256":sha(&exe),
            "script":self.fake,"script_sha256":sha(&self.fake),"provider":self.provider,"context":context,"receipt":seen}));
        seen
    }

    pub(crate) fn write_control(&self, control: &Value) {
        validate_control(&self.root, self.provider, control).unwrap();
        atomic_json(&self.dir.join("control.json"), control);
    }

    pub(crate) fn release_answer(&self, seq: usize) {
        assert!((1..=3).contains(&seq));
        atomic_json(
            &self.dir.join(format!("answer-{seq}.release")),
            &json!({"seq":seq}),
        );
    }

    pub(crate) fn inputs(&self) -> Vec<Value> {
        json_lines(&self.dir.join("inputs.jsonl"))
    }

    pub(crate) fn snapshot(&self) -> Value {
        let result = self.read_rpc("session.snapshot", json!({}));
        assert_eq!(result["type"], "session_snapshot");
        result["snapshot"].clone()
    }

    /// Counts real API mutations; native terminal bytes never use these API methods.
    pub(crate) fn input_requests(&self) -> Vec<String> {
        let server_log = self.home.join("xdg/herdr/herdr-server.log");
        let log = std::fs::read_to_string(&server_log).unwrap();
        let fields = regex::Regex::new(
            r#"event="api\.request\.(start|complete)".* outcome="([^"]+)" request_id="([^"]+)" method="([^"]+)""#,
        ).unwrap();
        let mut starts = Vec::new();
        let mut completes = Vec::new();
        let mut requests = Vec::new();
        for line in log.lines() {
            let input = ["pane.send_input", "pane.send_text", "pane.send_keys"]
                .iter()
                .any(|method| line.contains(&format!(" method=\"{method}\"")));
            if !input
                || !(line.contains("event=\"api.request.start\"")
                    || line.contains("event=\"api.request.complete\""))
            {
                continue;
            }
            let found = fields
                .captures(line)
                .expect("T14 actual API input log format");
            let (phase, outcome, id, method) = (&found[1], &found[2], &found[3], &found[4]);
            assert!(
                id.starts_with("adk-"),
                "T14 API input belongs to AgentDesk: {line}"
            );
            let entry = (id.to_owned(), method.to_owned());
            match phase {
                "start" => {
                    assert_eq!(outcome, "started", "T14 input API starts normally");
                    starts.push(entry);
                    requests.push(line.to_owned());
                }
                "complete" => {
                    assert_eq!(outcome, "ok", "T14 input API completes normally: {line}");
                    completes.push(entry);
                }
                _ => unreachable!(),
            }
        }
        assert_eq!(
            starts, completes,
            "T14 every real API input completes once in order"
        );
        let methods: Vec<_> = starts.iter().map(|(_, method)| method.as_str()).collect();
        assert!(
            methods == ["pane.send_input", "pane.send_text", "pane.send_keys"]
                || methods
                    == [
                        "pane.send_input",
                        "pane.send_text",
                        "pane.send_keys",
                        "pane.send_text",
                        "pane.send_keys"
                    ],
            "T14 bootstrap command then one or two paste/Enter pairs: {methods:?}"
        );
        self.ledger.append(
            json!({"event":"input_check", "methods":methods, "requests":requests,
            "server_log":server_log, "raw_server_log":log}),
        );
        requests
    }

    pub(crate) fn assert_input_bytes(&self, direct: &str) {
        let inputs = self.inputs();
        assert_eq!(inputs.len(), 3, "T14 three actual provider inputs");
        for (index, input) in inputs.iter().enumerate() {
            assert_eq!(input["seq"].as_u64(), Some((index + 1) as u64));
            let raw: Vec<u8> = input["raw_bytes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|byte| u8::try_from(byte.as_u64().unwrap()).unwrap())
                .collect();
            assert_eq!(raw.last(), Some(&b'\r'), "T14 actual input ends in Enter");
            if index == 1 {
                assert_eq!(
                    raw,
                    format!("{direct}\r").as_bytes(),
                    "T14 native PTY input is the direct prompt and Enter only"
                );
            } else {
                for marker in [b"\x1b[200~".as_slice(), b"\x1b[201~".as_slice()] {
                    assert_eq!(
                        raw.windows(marker.len())
                            .filter(|part| *part == marker)
                            .count(),
                        1,
                        "T14 each Discord input is one bracketed paste then Enter"
                    );
                }
                assert!(raw.starts_with(b"\x1b[200~") && raw.ends_with(b"\x1b[201~\r"));
            }
        }
        self.ledger
            .append(json!({"event":"input_bytes_check", "inputs":inputs}));
    }

    pub(crate) fn assert_single_execution(&self, pane: &str, provider_pid: u32) {
        let snapshot = self.snapshot();
        let panes = snapshot["panes"].as_array().unwrap();
        assert_eq!(panes.len(), 1, "T14 one real execution pane");
        assert_eq!(panes[0]["pane_id"], pane);
        let starts = json_lines(&self.dir.join("provider-starts.jsonl"));
        assert_eq!(starts.len(), 1, "T14 fake provider was launched once");
        assert_eq!(starts[0]["pid"], provider_pid);
        let process = self.read_rpc("pane.process_info", json!({"pane_id":pane}));
        assert!(
            process["process_info"]["foreground_processes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["pid"] == provider_pid),
            "T14 same provider remains foreground"
        );
        let server_log = self.home.join("xdg/herdr/herdr-server.log");
        let log = std::fs::read_to_string(&server_log).unwrap();
        let creates: Vec<_> = log
            .lines()
            .filter(|line| {
                line.contains("event=\"api.request.start\"")
                    && line.contains(" method=\"workspace.create\"")
            })
            .collect();
        assert_eq!(
            creates.len(),
            1,
            "T14 production created one real workspace"
        );
        self.ledger.append(json!({"event":"single_execution","pane":pane,"provider_pid":provider_pid,"workspace_creates":creates,"server_log":server_log,
            "snapshot":snapshot,"process_info":process,"provider_starts":starts.len()}));
    }

    pub(crate) fn attach(&mut self, pane: &str) -> RealClient {
        let info = self.read_rpc("pane.get", json!({"pane_id":pane}));
        let terminal = info["pane"]["terminal_id"]
            .as_str()
            .expect("T14 real pane has a terminal ID");
        let receipt = self.dir.join("client.json");
        let screen = self.dir.join("client-screen.bin");
        let driver = self.root.join("bin/pty_driver.py");
        let out = std::fs::File::create(self.dir.join("client-driver.out")).unwrap();
        let mut child = self
            .sandboxed()
            .arg("/usr/bin/python3")
            .arg(&driver)
            .arg(&self.bin)
            .arg(terminal)
            .arg(&screen)
            .arg(&receipt)
            .stdin(Stdio::piped())
            .stdout(out.try_clone().unwrap())
            .stderr(out)
            .spawn()
            .unwrap();
        self.ledger.record(child.id(), "pty-driver");
        wait_for(
            || json_file(&receipt).is_some(),
            "native client PID receipt",
        );
        let observed = json_file(&receipt).unwrap();
        let pid = u32::try_from(observed["pid"].as_u64().unwrap()).unwrap();
        wait_for(
            || {
                provenance::process_exec_path(pid).is_some_and(|path| {
                    PathBuf::from(path).canonicalize().ok().as_ref() == Some(&self.bin)
                })
            },
            "native client copied image",
        );
        self.ledger.record(pid, "native-client");
        self.ledger.append(json!({"event":"client","pid":pid,"pgid":ps(pid,"pgid=").parse::<u32>().unwrap(),
            "start":start_text(provenance::process_start(pid).unwrap().identity),"exe":self.bin,"sha256":sha(&self.bin),
            "terminal_id":terminal,"pane":pane,"driver_pid":child.id(),"driver_script":driver,"driver_script_sha256":sha(&driver)}));
        wait_for(
            || {
                std::fs::read(&screen).is_ok_and(|bytes| {
                    bytes
                        .windows(b"\x1b[?7l".len())
                        .any(|part| part == b"\x1b[?7l")
                })
            },
            "native client terminal setup screen",
        );
        let stdin = child.stdin.take().unwrap();
        RealClient {
            child: Some(child),
            stdin: Some(stdin),
            ledger: self.ledger.clone(),
            screen,
            receipt,
        }
    }

    pub(crate) fn stop(&mut self) -> bool {
        if self.stopped {
            return self.ledger.remaining().is_empty();
        }
        let ids = self.ledger.remaining();
        for pid in ids.iter().rev() {
            self.ledger.terminate_recorded(*pid);
        }
        if let Some(mut child) = self.child.take() {
            let deadline = Instant::now() + Duration::from_secs(15);
            while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            if child.try_wait().ok().flatten().is_none() {
                self.ledger.append(json!({"event":"cleanup_failed","pid":child.id(),"reason":"owned server did not end after TERM"}));
                return false;
            }
            let _ = child.wait();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.ledger.remaining().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let remaining = self.ledger.remaining();
        self.stopped = true;
        self.ledger.append(json!({"event":"cleanup","recorded":self.ledger.owned.lock().unwrap().clone(),"remaining":remaining,"signalled_remaining":remaining}));
        if remaining.is_empty()
            && std::fs::read(&self.lease).is_ok_and(|bytes| bytes == self.lease_bytes)
        {
            std::fs::remove_file(&self.lease).unwrap();
        }
        remaining.is_empty()
    }
}

impl Drop for RealServer {
    fn drop(&mut self) {
        self.stop();
    }
}

pub(crate) struct RealClient {
    child: Option<Child>,
    stdin: Option<std::process::ChildStdin>,
    ledger: Ledger,
    screen: PathBuf,
    receipt: PathBuf,
}

impl RealClient {
    pub(crate) fn screen_path(&self) -> &Path {
        &self.screen
    }
    pub(crate) fn receipt_path(&self) -> &Path {
        &self.receipt
    }
    pub(crate) fn send_prompt(&mut self, prompt: &str) {
        assert!(!prompt.contains(['\r', '\u{1b}']));
        writeln!(
            self.stdin.as_mut().unwrap(),
            "{}",
            json!({"op":"send","prompt":prompt})
        )
        .unwrap();
        self.stdin.as_mut().unwrap().flush().unwrap();
    }
    pub(crate) fn detach(&mut self) -> bool {
        let Some(mut child) = self.child.take() else {
            return true;
        };
        if let Some(mut stdin) = self.stdin.take() {
            let _ = writeln!(stdin, "{}", json!({"op":"detach"}));
            let _ = stdin.flush();
            // The native detach command must run before closing the driver input pipe.
            let deadline = Instant::now() + Duration::from_secs(15);
            while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let ended = child.try_wait().ok().flatten();
        let clean = ended.is_some_and(|status| status.success())
            && json_file(&self.receipt).is_some_and(|v| v["wait_status"] == 0);
        if ended.is_none() {
            self.ledger.terminate_recorded(child.id());
        }
        if ended.is_some() {
            let _ = child.wait();
        }
        self.ledger.append(json!({"event":"client_detach","driver_pid":child.id(),"clean":clean,"receipt":json_file(&self.receipt)}));
        clean
    }
}
impl Drop for RealClient {
    fn drop(&mut self) {
        self.detach();
    }
}

fn validate_control(root: &Path, provider: &str, value: &Value) -> Result<(), String> {
    if value["provider"] != provider {
        return Err("provider mismatch".into());
    }
    for key in ["prompts", "answers"] {
        if !value[key]
            .as_array()
            .is_some_and(|v| v.len() == 3 && v.iter().all(Value::is_string))
        {
            return Err(format!("invalid {key}"));
        }
    }
    if !value["session_id"].as_str().is_some_and(|s| !s.is_empty())
        || !value["binding_envelope"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
    {
        return Err("missing native session or binding envelope".into());
    }
    let path = Path::new(value["transcript_path"].as_str().ok_or("source missing")?);
    if !path.is_absolute()
        || !path.starts_with(root)
        || path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err("source escaped owned root".into());
    }
    let mut existing = path;
    while !existing.exists() {
        existing = existing.parent().ok_or("source has no owned parent")?;
    }
    if !existing
        .canonicalize()
        .map_err(|e| e.to_string())?
        .starts_with(root)
    {
        return Err("source symlink escaped owned root".into());
    }
    let url = value["hook_url"].as_str().ok_or("hook origin missing")?;
    let authority = url
        .strip_prefix("http://127.0.0.1:")
        .ok_or("hook must use numeric loopback HTTP")?
        .trim_end_matches('/');
    let port = authority.parse::<u16>().map_err(|e| e.to_string())?;
    if matches!(port, 0 | 8791 | 15432) {
        return Err("hook port is reserved".into());
    }
    Ok(())
}

#[test]
fn t14_client_control_rejects_external_hooks_and_source_escape() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let mut control = json!({"provider":"claude","session_id":"native","binding_envelope":"captured",
        "transcript_path":root.join("source/events.jsonl"),"hook_url":"http://127.0.0.1:45678",
        "prompts":["first","direct","followup"],"answers":["one","two","three"]});
    assert!(validate_control(&root, "claude", &control).is_ok());
    control["hook_url"] = json!("https://example.com");
    assert!(validate_control(&root, "claude", &control).is_err());
    control["hook_url"] = json!("http://127.0.0.1:45678");
    control["transcript_path"] = json!(root.join("../escaped.jsonl"));
    assert!(validate_control(&root, "claude", &control).is_err());
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
    control["transcript_path"] = json!(root.join("link/events.jsonl"));
    assert!(validate_control(&root, "claude", &control).is_err());
}

#[test]
fn t14_cleanup_requires_positive_process_absence() {
    assert!(ps_proves_absence(Some(1), b"", b""));
    assert!(!ps_proves_absence(Some(0), b" 123\n", b""));
    assert!(!ps_proves_absence(Some(1), b"123\n", b""));
    assert!(!ps_proves_absence(Some(1), b"", b"ps unavailable\n"));
    assert!(!ps_proves_absence(Some(2), b"", b""));
    assert!(!ps_proves_absence(None, b"", b""));
}
