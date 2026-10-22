//! A pinned offline scratch server restores AgentDesk providers as shells only.
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::*;
use crate::services::herdr_launch::{HERDR_PANE_ENV, unset_herdr_env_before_exec};
use crate::services::session_host::herdr::model::{HerdrProcessInfo, HerdrResult};
use crate::services::session_host::herdr::{pane_probe::start_text, provenance};

const PINNED_SHA256: &str = "5173a3e0ae42d5d1ab7ebfa5d5e6329f7c3d23f8e1a3677c7ce3231da2884157";

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

fn wait_for(mut ready: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ready() {
        assert!(Instant::now() < deadline, "T19 never observed {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

struct T19Server {
    root: PathBuf,
    bin: PathBuf,
    dir: PathBuf,
    ledger: PathBuf,
    child: Option<Child>,
    owned: Vec<(u32, String)>,
    generation: u32,
}

impl T19Server {
    fn new() -> Self {
        let read = |key| PathBuf::from(std::env::var_os(key).unwrap_or_else(|| panic!("{key}")));
        let root = read("ADK_TEST_HERDR_ROOT").canonicalize().unwrap();
        assert_eq!(root.parent(), Some(Path::new("/private/tmp")));
        let owner = root
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .strip_prefix("adk-e-t19-")
            .unwrap();
        assert!(
            !owner.is_empty() && owner.bytes().all(|byte| byte.is_ascii_digit()),
            "T19 scratch root records its owner PID"
        );
        let bin = read("ADK_TEST_HERDR_BIN").canonicalize().unwrap();
        assert_eq!(
            bin,
            root.join("bin/herdr"),
            "T19 executes only the scratch copy"
        );
        assert_eq!(sha(&bin), PINNED_SHA256, "T19 pinned 0.9.3 bytes");
        let ledger = read("ADK_TEST_HERDR_PIDS");
        assert_eq!(ledger.parent().unwrap().canonicalize().unwrap(), root);
        let dir = root.join(format!("run-{}", std::process::id()));
        assert!(
            !dir.exists(),
            "T19 never reuses a previous server directory"
        );
        for child in [
            "home/xdg", "state", "data", "cache", "run", "tmp", "work", "pathbin",
        ] {
            std::fs::create_dir_all(dir.join(child)).unwrap();
        }
        for (tool, target) in [("git", "/usr/bin/git"), ("sh", "/bin/sh")] {
            std::os::unix::fs::symlink(target, dir.join("pathbin").join(tool)).unwrap();
        }
        std::fs::write(dir.join("home/config.toml"), observe::CANONICAL_CONFIG).unwrap();
        std::fs::File::options()
            .write(true)
            .open(dir.join("home/config.toml"))
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(60))
            .unwrap();
        Self {
            root,
            bin,
            dir,
            ledger,
            child: None,
            owned: Vec::new(),
            generation: 0,
        }
    }

    fn append(&self, event: Value) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.ledger)
            .unwrap();
        writeln!(file, "{event}").unwrap();
        file.sync_all().unwrap();
        println!("T19_EVENT {event}");
    }

    fn socket(&self) -> PathBuf {
        let socket = self.dir.join("run/h.sock");
        assert!(socket.is_absolute());
        assert!(
            socket
                .parent()
                .unwrap()
                .canonicalize()
                .unwrap()
                .starts_with(&self.root)
        );
        if socket.exists() {
            assert!(socket.canonicalize().unwrap().starts_with(&self.root));
        }
        socket
    }

    fn endpoint(&self) -> HerdrEndpoint {
        HerdrEndpoint::new("t19-node", "t19", &self.socket(), "t19")
            .unwrap()
            .with_herdr_home(&self.dir.join("home"))
            .unwrap()
    }

    fn transport(&self) -> HerdrSocketTransport {
        HerdrSocketTransport::new(
            &self.endpoint(),
            HerdrSocketConfig::default(),
            LineJsonFraming,
        )
    }

    fn witness(&self) -> ServerWitness {
        let witness = self.transport().server_witness().unwrap();
        assert_eq!(witness.socket, self.socket());
        assert_eq!(
            witness.pid,
            self.child.as_ref().unwrap().id(),
            "T19 socket peer is the owned child"
        );
        let exe = PathBuf::from(provenance::process_exec_path(witness.pid).unwrap())
            .canonicalize()
            .unwrap();
        assert_eq!(exe, self.bin, "T19 W3 socket peer runs the scratch copy");
        assert_eq!(sha(&exe), PINNED_SHA256);
        witness
    }

    fn record(&mut self, pid: u32, role: &str) {
        let start = start_text(provenance::process_start(pid).unwrap().identity);
        let pgid = ps(pid, "pgid=").parse::<u32>().unwrap();
        self.append(
            json!({"event":"process", "generation":self.generation, "pid":pid,
            "pgid":pgid, "start":start, "role":role}),
        );
        self.owned.push((pid, start));
    }

    fn start(&mut self) {
        assert!(self.child.is_none());
        self.generation += 1;
        let out = std::fs::File::create(self.dir.join(format!("server-{}.out", self.generation)))
            .unwrap();
        let at = |sub| self.dir.join(sub);
        let child = Command::new(&self.bin)
            .arg("server")
            .env_clear()
            .env("PATH", at("pathbin"))
            .env("HOME", at("home"))
            .env("XDG_CONFIG_HOME", at("home/xdg"))
            .env("XDG_STATE_HOME", at("state"))
            .env("XDG_DATA_HOME", at("data"))
            .env("XDG_CACHE_HOME", at("cache"))
            .env("XDG_RUNTIME_DIR", at("run"))
            .env("TMPDIR", at("tmp"))
            .env("HERDR_CONFIG_PATH", at("home/config.toml"))
            .env("HERDR_SOCKET_PATH", self.socket())
            .env("TERM", "xterm-256color")
            .env("LANG", "en_US.UTF-8")
            .env("SHELL", "/bin/sh")
            .current_dir(at("work"))
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(out.try_clone().unwrap())
            .stderr(out)
            .spawn()
            .unwrap();
        let pid = child.id();
        self.child = Some(child);
        self.record(pid, "server");
        wait_for(
            || UnixStream::connect(self.socket()).is_ok(),
            "scratch socket",
        );
        let witness = self.witness();
        assert_eq!(
            ps(pid, "pgid="),
            pid.to_string(),
            "T19 owns the server process group"
        );
        self.append(
            json!({"event":"server", "generation":self.generation, "pid":pid,
            "pgid":pid, "start":start_text(witness.start), "socket":self.socket(),
            "exe":self.bin, "sha256":sha(&self.bin), "dir":self.dir}),
        );
        std::thread::sleep(Duration::from_millis(1_200));
    }

    fn rpc(&self, id: &str, request: HerdrRequest) -> HerdrResult {
        let witness = self.witness();
        let call = HerdrCall {
            id: id.into(),
            request,
        };
        let transport = self.transport();
        let reply = if call.request.is_read_only() {
            let (reply, observed) = transport.call(&call);
            assert_eq!(observed.unwrap(), witness);
            reply
        } else {
            transport.call_with_witness(&call, &witness)
        }
        .unwrap();
        assert_eq!(reply.id, id);
        let result = reply.body.unwrap();
        self.append(
            json!({"event":"reply", "generation":self.generation, "id":id,
            "pid":witness.pid, "start":start_text(witness.start), "socket":witness.socket}),
        );
        result
    }

    fn process(&self, pane: &str) -> HerdrProcessInfo {
        match self.rpc(
            "t19-process",
            HerdrRequest::PaneProcessInfo {
                pane_id: pane.into(),
            },
        ) {
            HerdrResult::PaneProcessInfo { process_info } => process_info,
            other => panic!("T19 process reply {other:?}"),
        }
    }

    fn confirm_write(&self, id: &str, method: &str) {
        let path = self.dir.join("home/xdg/herdr/herdr-server.log");
        wait_for(
            || {
                std::fs::read_to_string(&path).is_ok_and(|log| {
                    log.lines().any(|line| {
                        line.contains("event=\"api.request.start\"")
                            && line.contains(&format!(" request_id=\"{id}\""))
                    })
                })
            },
            "the real server's request log",
        );
        let log = std::fs::read_to_string(path).unwrap();
        let taken: Vec<_> = log
            .lines()
            .filter(|line| {
                line.contains("event=\"api.request.start\"")
                    && line.contains(&format!(" request_id=\"{id}\""))
            })
            .collect();
        assert_eq!(
            taken.len(),
            1,
            "T19 W3 exactly one real server request: {id}"
        );
        assert!(
            taken[0].contains(&format!(" method=\"{method}\"")),
            "T19 W3 server took the intended mutation"
        );
        let witness = self.witness();
        self.append(
            json!({"event":"write", "id":id, "method":method, "pid":witness.pid,
            "start":start_text(witness.start), "socket":witness.socket, "exe":self.bin,
            "sha256":sha(&self.bin)}),
        );
    }

    fn confirm_launch(&self, ordinal: usize) {
        for (id, method) in [
            (ordinal * 2 + 1, "workspace.create"),
            (ordinal * 2 + 2, "pane.send_input"),
        ] {
            self.confirm_write(&format!("adk-launch-{id}"), method);
        }
    }

    fn raw_rpc(&self, id: &str, method: &str, params: Value) -> Value {
        let mut stream = UnixStream::connect(self.socket()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (pid, start) = provenance::socket_peer(&stream).unwrap();
        let witness = self.witness();
        assert_eq!(
            pid, witness.pid,
            "T19 raw mutation peer is the owned scratch child"
        );
        assert_eq!(start.identity, witness.start);
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
        assert!(reply["error"].is_null(), "T19 control API error: {reply}");
        self.confirm_write(id, method);
        reply["result"].clone()
    }

    fn stop(&mut self) -> bool {
        let Some(mut child) = self.child.take() else {
            return true;
        };
        let pid = child.id();
        assert!(self.owned.iter().any(|(owned, _)| *owned == pid));
        let _ = Command::new("/bin/kill")
            .args(["-TERM", &pid.to_string()])
            .status();
        let deadline = Instant::now() + Duration::from_secs(15);
        while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        let forced = child.try_wait().ok().flatten().is_none();
        if forced {
            let _ = child.kill();
        }
        let _ = child.wait();
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.owned.iter().any(|(pid, start)| {
            provenance::process_start(*pid).is_ok_and(|now| start_text(now.identity) == *start)
        }) && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(100));
        }
        let remaining: Vec<_> = self
            .owned
            .iter()
            .filter(|(pid, start)| {
                provenance::process_start(*pid).is_ok_and(|now| start_text(now.identity) == *start)
            })
            .map(|(pid, _)| *pid)
            .collect();
        self.append(
            json!({"event":"stopped", "generation":self.generation, "pid":pid, "forced":forced, "remaining":remaining}),
        );
        !forced && remaining.is_empty()
    }
}

impl Drop for T19Server {
    fn drop(&mut self) {
        self.stop();
        let mut left = Vec::new();
        for (pid, start) in &self.owned {
            if provenance::process_start(*pid).is_ok_and(|now| start_text(now.identity) == *start) {
                // Only recorded child identities may receive a cleanup signal.
                let _ = Command::new("/bin/kill")
                    .args(["-TERM", &pid.to_string()])
                    .status();
                left.push(*pid);
            }
        }
        self.append(json!({"event":"cleanup", "recorded":self.owned, "signalled_remaining":left}));
    }
}

fn provider_script(dir: &Path, provider: &str) -> String {
    let fresh = dir.join(provider);
    let control = dir.join("codex-control");
    let env = HERDR_PANE_ENV
        .iter()
        .map(|key| format!("printf '%s\\n' \"{key}=${{{key}-}}\"\n"))
        .collect::<String>();
    format!(
        "#!/bin/sh\nbase='{}'\nif [ \"${{1-}}\" = --version ]; then printf 'codex-cli 0.9.3\\n'; exit 0; fi\nif [ \"${{1-}}\" = resume ]; then base='{}'; fi\nprintf '%s\\n' \"$$\" > \"$base.pid\"\nprintf '%s\\n' \"$@\" > \"$base.argv\"\nprintf 'start\\n' >> \"$base.starts\"\ncase \"$*\" in *resume*|*continue*) printf 'RESUMED\\n' >> \"$base.resume\" ;; esac\n{{\n{env}}} > \"$base.env\"\nwhile IFS= read -r line; do :; done\n",
        fresh.display(),
        control.display()
    )
}

fn assert_no_resume_metadata(value: &Value, control_cwd: &str) -> usize {
    match value {
        Value::Object(object) => {
            let pane = usize::from(
                object.contains_key("cwd")
                    && object.get("cwd").and_then(Value::as_str) != Some(control_cwd),
            );
            if pane == 1 {
                for key in ["launch_argv", "agent_session", "agent_resume"] {
                    assert!(
                        object.get(key).is_none_or(Value::is_null),
                        "T19 persisted {key} cannot resume a provider"
                    );
                }
            }
            pane + object
                .values()
                .map(|item| assert_no_resume_metadata(item, control_cwd))
                .sum::<usize>()
        }
        Value::Array(values) => values
            .iter()
            .map(|item| assert_no_resume_metadata(item, control_cwd))
            .sum(),
        _ => 0,
    }
}

fn assert_control_resume_metadata(value: &Value, control_cwd: &str) -> usize {
    match value {
        Value::Object(object) if object.get("cwd").and_then(Value::as_str) == Some(control_cwd) => {
            assert_eq!(
                object.get("agent_session"),
                Some(&json!({
                    "source":"herdr:codex", "agent":"codex", "kind":"id", "value":"t19-sentinel"
                })),
                "T19 positive control persisted the upstream exact Codex resume argv"
            );
            1
        }
        Value::Object(object) => object
            .values()
            .map(|item| assert_control_resume_metadata(item, control_cwd))
            .sum(),
        Value::Array(values) => values
            .iter()
            .map(|item| assert_control_resume_metadata(item, control_cwd))
            .sum(),
        _ => 0,
    }
}

#[test]
#[ignore = "needs ADK_TEST_HERDR_BIN: pinned isolated Herdr 0.9.3 scratch copy"]
fn herdr_t19_resume_true_restores_claude_and_codex_as_shells_only() {
    use crate::services::codex::validate_herdr_fresh_argv;

    assert!(
        validate_herdr_fresh_argv("fresh", None, &[]).is_ok(),
        "T19 fresh argv accepted"
    );
    assert!(
        validate_herdr_fresh_argv("fresh", None, &["resume".into(), "t19-sentinel".into()])
            .is_err(),
        "T19 resumable argv refused"
    );
    let mut server = T19Server::new();
    for provider in ["claude", "codex"] {
        let path = server.dir.join("pathbin").join(provider);
        std::fs::write(&path, provider_script(&server.dir, provider)).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    server.start();
    let mut launched = Vec::new();
    let host = SocketHerdrLaunchHost::new(vec![server.endpoint()], HerdrSocketConfig::default());
    for (ordinal, provider) in ["claude", "codex"].into_iter().enumerate() {
        let script = format!(
            "#!/bin/sh\nexec {}\n",
            server.dir.join("pathbin").join(provider).display()
        );
        let script = unset_herdr_env_before_exec(&script).unwrap();
        let path = server.dir.join(format!("launch-{provider}.sh"));
        std::fs::write(&path, script).unwrap();
        let endpoint = HerdrLaunchEndpoint {
            execution_node: "t19-node".into(),
            config_key: "t19".into(),
            socket_addr: server.socket().display().to_string(),
            herdr_session: "t19".into(),
        };
        let witness = host
            .restore_resume(&endpoint)
            .admitted_witness()
            .expect("T19 first launch canonical off");
        assert_eq!(witness, server.witness());
        let outcome = host.create(&HerdrCreateRequest {
            endpoint,
            label: format!("adk-t19-{provider}"),
            cwd: server.dir.join("work"),
            command: format!("sh {}", path.display()),
            restore_off_witness: witness,
        });
        let pane = match outcome {
            HerdrCreateOutcome::Created { pane_id } => pane_id,
            other => panic!("T19 actual launch {provider}: {other:?}"),
        };
        server.confirm_launch(ordinal);
        let provider_pid = server.dir.join(format!("{provider}.pid"));
        wait_for(|| provider_pid.exists(), "fake provider started");
        let pid = std::fs::read_to_string(provider_pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        server.record(pid, provider);
        let env_path = server.dir.join(format!("{provider}.env"));
        wait_for(
            || {
                std::fs::read_to_string(&env_path)
                    .is_ok_and(|text| text.lines().count() == HERDR_PANE_ENV.len())
            },
            "complete provider env witness",
        );
        let env = std::fs::read_to_string(env_path).unwrap();
        assert_eq!(
            env.lines().count(),
            HERDR_PANE_ENV.len(),
            "T19 env witness is complete"
        );
        for key in HERDR_PANE_ENV {
            assert!(
                env.lines().any(|line| line == format!("{key}=")),
                "T19 provider herdr env removed: {key}"
            );
        }
        assert_eq!(
            std::fs::read(server.dir.join(format!("{provider}.argv"))).unwrap(),
            b"\n",
            "T19 provider receives fresh argv without resume"
        );
        let info = server.process(&pane);
        server.record(info.shell_pid.unwrap(), "original-shell");
        assert!(
            info.foreground_processes
                .unwrap()
                .iter()
                .any(|p| p.pid == pid),
            "T19 fake provider is really foreground"
        );
        launched.push((provider, pane));
    }
    // The same fake Codex must really resume when 0.9.3 receives a resumable argv.
    let control_cwd = server.dir.join("work/control");
    std::fs::create_dir(&control_cwd).unwrap();
    let control_pane = match server.rpc(
        "t19-control-create",
        HerdrRequest::WorkspaceCreate {
            cwd: control_cwd.display().to_string(),
            label: "t19-positive-control".into(),
            focus: false,
        },
    ) {
        HerdrResult::WorkspaceCreated { root_pane } => root_pane.pane_id,
        other => panic!("T19 positive control workspace: {other:?}"),
    };
    server.confirm_write("t19-control-create", "workspace.create");
    let started = server.raw_rpc(
        "t19-control-start",
        "agent.start",
        json!({
            "name":"t19-positive-control", "kind":"codex", "pane_id":control_pane,
            "args":["resume","t19-sentinel"], "timeout_ms":300000
        }),
    );
    assert_eq!(started["type"], "agent_started");
    assert_eq!(started["argv"], json!(["codex", "resume", "t19-sentinel"]));
    let control_pid = server.dir.join("codex-control.pid");
    wait_for(|| control_pid.exists(), "positive control fake Codex");
    let control_provider: u32 = std::fs::read_to_string(&control_pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    server.record(control_provider, "positive-control-codex");
    let control_info = server.process(&control_pane);
    server.record(control_info.shell_pid.unwrap(), "positive-control-shell");
    assert!(
        control_info
            .foreground_processes
            .unwrap()
            .iter()
            .any(|process| process.pid == control_provider),
        "T19 positive control provider is foreground"
    );
    let control_env = server.dir.join("codex-control.env");
    wait_for(
        || {
            std::fs::read_to_string(&control_env)
                .is_ok_and(|text| text.lines().count() == HERDR_PANE_ENV.len())
        },
        "positive control env witness",
    );
    assert!(
        std::fs::read_to_string(control_env)
            .unwrap()
            .lines()
            .any(|line| line.starts_with("HERDR_PANE_ID=") && line != "HERDR_PANE_ID="),
        "T19 positive control keeps the real pane identity env"
    );
    assert_eq!(
        std::fs::read(server.dir.join("codex-control.argv")).unwrap(),
        b"resume\nt19-sentinel\n"
    );
    assert_eq!(
        std::fs::read(server.dir.join("codex-control.starts")).unwrap(),
        b"start\n"
    );
    assert_eq!(
        std::fs::read(server.dir.join("codex-control.resume")).unwrap(),
        b"RESUMED\n"
    );
    let session = server.dir.join("home/xdg/herdr/session.json");
    wait_for(|| session.exists(), "saved server session");
    wait_for(
        || std::fs::read_to_string(&session).is_ok_and(|text| text.contains("t19-sentinel")),
        "live server persisted the positive-control session ref",
    );
    std::fs::write(
        server.dir.join("home/config.toml"),
        "[session]\nresume_agents_on_restore = true\n",
    )
    .unwrap();
    assert!(server.stop(), "T19 graceful stop publishes the snapshot");
    let persisted: Value = serde_json::from_slice(&std::fs::read(&session).unwrap()).unwrap();
    assert!(
        assert_no_resume_metadata(&persisted, control_cwd.to_str().unwrap()) >= 2,
        "T19 actually saved both provider panes"
    );
    assert_eq!(
        assert_control_resume_metadata(&persisted, control_cwd.to_str().unwrap()),
        1,
        "T19 actual snapshot contains the positive resume control"
    );
    server.append(
        json!({"event":"saved", "session":session, "sha256":sha(&session), "snapshot":persisted}),
    );
    let starts: Vec<_> = launched
        .iter()
        .map(|(provider, _)| std::fs::read(server.dir.join(format!("{provider}.starts"))).unwrap())
        .collect();
    server.start();
    wait_for(
        || {
            std::fs::read(server.dir.join("codex-control.starts"))
                .is_ok_and(|text| text == b"start\nstart\n")
        },
        "actual positive-control Codex resume after restart",
    );
    assert_eq!(
        std::fs::read(server.dir.join("codex-control.resume")).unwrap(),
        b"RESUMED\nRESUMED\n",
        "T19 positive control really resumed, so shell-only is not a no-op restore"
    );
    let resumed_provider: u32 = std::fs::read_to_string(control_pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_ne!(resumed_provider, control_provider);
    server.record(resumed_provider, "resumed-positive-control-codex");
    let control_info = server.process(&control_pane);
    server.record(
        control_info.shell_pid.unwrap(),
        "resumed-positive-control-shell",
    );
    assert!(
        control_info
            .foreground_processes
            .unwrap()
            .iter()
            .any(|process| process.pid == resumed_provider),
        "T19 positive resumed provider is foreground"
    );
    server.append(json!({"event":"positive_control", "pane":control_pane, "provider_starts":2, "resumes":2, "resumed_provider":resumed_provider}));
    for (index, (provider, pane)) in launched.iter().enumerate() {
        let info = server.process(pane);
        let shell = info.shell_pid.expect("T19 restored pane has a shell");
        server.record(shell, "restored-shell");
        let foreground = info
            .foreground_processes
            .as_ref()
            .expect("T19 foreground observation exists");
        assert!(
            foreground.iter().all(|process| process.pid == shell),
            "T19 restored AgentDesk pane is shell only: {info:?}"
        );
        assert_eq!(
            ps(shell, "comm="),
            "/bin/sh",
            "T19 restored root is the configured shell"
        );
        assert_eq!(
            std::fs::read(server.dir.join(format!("{provider}.starts"))).unwrap(),
            starts[index],
            "T19 provider was not launched on restore"
        );
        assert!(
            !server.dir.join(format!("{provider}.resume")).exists(),
            "T19 resume sentinel stayed absent"
        );
        server.append(json!({"event":"restored", "provider":provider, "pane":pane, "shell":shell,
            "foreground":foreground.iter().map(|process| process.pid).collect::<Vec<_>>(), "resumes":0, "provider_starts":1}));
    }
    assert!(server.stop(), "T19 second server stops without escalation");
}
