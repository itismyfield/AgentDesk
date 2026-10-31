//! An offline scratch server restores AgentDesk providers as shells only.
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

// The macOS sh launcher execs bash, so select the native shell for an exact OS identity.
const T19_SHELL: &str = "/bin/bash";

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
    /// The scratch copy's bytes when the test began; the server must run exactly these.
    image_sha: String,
    dir: PathBuf,
    ledger: PathBuf,
    child: Option<Child>,
    owned: Vec<(u32, String)>,
    generation: u32,
    lease: PathBuf,
    lease_bytes: Vec<u8>,
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
        let image_sha = sha(&bin);
        let ledger = read("ADK_TEST_HERDR_PIDS");
        assert_eq!(ledger.parent().unwrap().canonicalize().unwrap(), root);
        for entry in std::fs::read_dir(&root).unwrap() {
            let path = entry.unwrap().path();
            if !path.to_string_lossy().ends_with(".pids.jsonl") {
                continue;
            }
            for line in std::fs::read_to_string(path).unwrap().lines() {
                let event: Value = serde_json::from_str(line).unwrap();
                if event["event"] != "process" {
                    continue;
                }
                let pid = u32::try_from(event["pid"].as_u64().unwrap()).unwrap();
                assert!(
                    ps(pid, "pid=").is_empty(),
                    "T19 does not overwrite fake providers while a recorded child is alive: {pid}"
                );
            }
        }
        let dir = root.join(format!("run-{}", std::process::id()));
        assert!(
            !dir.exists(),
            "T19 never reuses a previous server directory"
        );
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
        let lease_bytes =
            serde_json::to_vec(&json!({"pid":std::process::id(), "dir":dir})).unwrap();
        let mut lock = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lease)
            .expect("T19 requires exclusive ownership before replacing scratch fake providers");
        lock.write_all(&lease_bytes).unwrap();
        lock.sync_all().unwrap();
        let path = format!("{}:/usr/bin:/bin", root.join("bin").display());
        let rc = format!(
            "export PATH='{path}'\nexport CODEX_HOME='{}'\nexport CLAUDE_CONFIG_DIR='{}'\n",
            dir.join("codex-home").display(),
            dir.join("claude-config").display()
        );
        for name in [".zshrc", ".zprofile", ".bash_profile", ".profile"] {
            std::fs::write(dir.join("home").join(name), &rc).unwrap();
        }
        std::fs::write(
            dir.join("network.sb"),
            "(version 1)\n(allow default)\n(deny network-outbound)\n(allow network-outbound (remote ip \"localhost:*\") (remote unix-socket))\n",
        )
        .unwrap();
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
            image_sha,
            dir,
            ledger,
            child: None,
            owned: Vec::new(),
            generation: 0,
            lease,
            lease_bytes,
        }
    }

    fn path(&self) -> String {
        format!("{}:/usr/bin:/bin", self.root.join("bin").display())
    }

    fn fake(&self, provider: &str) -> PathBuf {
        assert!(matches!(provider, "codex" | "claude"));
        self.root.join("bin").join(provider)
    }

    fn sandboxed(&self) -> Command {
        let at = |sub| self.dir.join(sub);
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command
            .arg("-f")
            .arg(at("network.sb"))
            .env_clear()
            .env("PATH", self.path())
            .env("HOME", at("home"))
            .env("CODEX_HOME", at("codex-home"))
            .env("CLAUDE_CONFIG_DIR", at("claude-config"))
            .env("XDG_CONFIG_HOME", at("home/xdg"))
            .env("XDG_STATE_HOME", at("state"))
            .env("XDG_DATA_HOME", at("data"))
            .env("XDG_CACHE_HOME", at("cache"))
            .env("XDG_RUNTIME_DIR", at("run"))
            .env("TMPDIR", at("tmp"))
            .env("HERDR_CONFIG_PATH", at("home/config.toml"))
            .env("HERDR_SOCKET_PATH", self.socket())
            .env("HERDR_LOG", "herdr=debug")
            .env("TERM", "xterm-256color")
            .env("LANG", "en_US.UTF-8")
            .env("SHELL", T19_SHELL)
            .current_dir(at("work"))
            .process_group(0);
        command
    }

    fn preflight(&mut self) {
        let prefix = self.dir.join(format!("preflight-{}", self.generation));
        let stdout = prefix.with_extension("stdout");
        let stderr = prefix.with_extension("stderr");
        let curl_out = prefix.with_extension("curl.stdout");
        let curl_err = prefix.with_extension("curl.stderr");
        let script = format!(
            "IFS= read -r token; [ \"$token\" = t19-preflight ] || exit 90\n\
             codex=$(command -v codex); claude=$(command -v claude); helper=$(command -v env)\n\
             printf 'codex=%s\\nclaude=%s\\nenv=%s\\nPATH=%s\\nHOME=%s\\nCODEX_HOME=%s\\nCLAUDE_CONFIG_DIR=%s\\n' \"$codex\" \"$claude\" \"$helper\" \"$PATH\" \"$HOME\" \"$CODEX_HOME\" \"$CLAUDE_CONFIG_DIR\"\n\
             [ \"$codex\" = '{}' ] && [ \"$claude\" = '{}' ] && [ \"$helper\" = /usr/bin/env ] && [ \"$PATH\" = '{}' ] || exit 91\n\
             /usr/bin/curl -m 3 -sS https://example.com > '{}' 2> '{}'\n\
             result=$?; printf 'curl_status=%s\\n' \"$result\"; [ \"$result\" -ne 0 ]\n",
            self.fake("codex").display(),
            self.fake("claude").display(),
            self.path(),
            curl_out.display(),
            curl_err.display()
        );
        let mut child = self
            .sandboxed()
            .args([T19_SHELL, "-l", "-c", &script])
            .stdin(Stdio::piped())
            .stdout(std::fs::File::create(&stdout).unwrap())
            .stderr(std::fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap();
        let pid = child.id();
        self.record(pid, "sandbox-login-preflight");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"t19-preflight\n")
            .unwrap();
        let status = child.wait().unwrap();
        let text = std::fs::read_to_string(&stdout).unwrap();
        let observed = |key: &str| text.lines().find_map(|line| line.strip_prefix(key));
        let curl_status = text
            .lines()
            .find_map(|line| line.strip_prefix("curl_status="))
            .and_then(|code| code.parse::<i32>().ok());
        let profile = self.dir.join("network.sb");
        let event = json!({"event":"isolation_preflight", "generation":self.generation,
            "pid":pid, "sandbox_profile":profile, "sandbox_profile_sha256":sha(&profile),
            "path":observed("PATH="), "codex":observed("codex="), "claude":observed("claude="),
            "env":observed("env="), "curl_status":curl_status, "status":status.code(),
            "stdout":stdout, "stdout_sha256":sha(&stdout), "stderr":stderr,
            "stderr_sha256":sha(&stderr), "curl_stdout":curl_out, "curl_stderr":curl_err});
        std::fs::write(
            prefix.with_extension("json"),
            serde_json::to_vec_pretty(&event).unwrap(),
        )
        .unwrap();
        self.append(event);
        assert!(
            status.success(),
            "T19 sandbox login preflight failed: {text}"
        );
        assert!(
            curl_status.is_some_and(|code| code != 0),
            "T19 outbound curl must fail"
        );
        let expected = format!(
            "codex={}\nclaude={}\nenv=/usr/bin/env\nPATH={}\nHOME={}\nCODEX_HOME={}\nCLAUDE_CONFIG_DIR={}\ncurl_status={}\n",
            self.fake("codex").display(),
            self.fake("claude").display(),
            self.path(),
            self.dir.join("home").display(),
            self.dir.join("codex-home").display(),
            self.dir.join("claude-config").display(),
            curl_status.unwrap()
        );
        assert_eq!(
            text, expected,
            "T19 observes sealed login-shell paths before any server"
        );
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
        assert_eq!(sha(&exe), self.image_sha, "T19 scratch copy unchanged");
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
        self.preflight();
        let out = std::fs::File::create(self.dir.join(format!("server-{}.out", self.generation)))
            .unwrap();
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
        if left.is_empty()
            && std::fs::read(&self.lease).is_ok_and(|bytes| bytes == self.lease_bytes)
        {
            std::fs::remove_file(&self.lease).unwrap();
        }
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
        "#!/bin/sh\nbase='{}'\nif [ \"${{1-}}\" = --version ]; then printf 'codex-cli 0.9.3\\n'; exit 0; fi\nif [ \"${{1-}}\" = resume ]; then base='{}'; fi\nprintf '%s\\n' \"$$\" > \"$base.pid\"\nprintf '%s\\n' \"$0\" >> \"$base.exec\"\nprintf '%s\\n' \"$@\" > \"$base.argv\"\nprintf 'start\\n' >> \"$base.starts\"\nprintf 'PATH=%s\\nHOME=%s\\nCODEX_HOME=%s\\nCLAUDE_CONFIG_DIR=%s\\n' \"$PATH\" \"$HOME\" \"$CODEX_HOME\" \"$CLAUDE_CONFIG_DIR\" > \"$base.isolation\"\ncase \"$*\" in *resume*|*continue*) printf 'RESUMED\\n' >> \"$base.resume\" ;; esac\n{{\n{env}}} > \"$base.env\"\nwhile IFS= read -r line; do :; done\n",
        fresh.display(),
        control.display()
    )
}

fn assert_provider_isolation(server: &T19Server, record: &str, provider: &str, starts: usize) {
    let executable = server.fake(provider);
    assert_eq!(executable.canonicalize().unwrap(), executable);
    assert_eq!(
        std::fs::read_to_string(server.dir.join(format!("{record}.exec"))).unwrap(),
        format!("{}\n", executable.display()).repeat(starts),
        "T19 every provider invocation uses its absolute scratch fake path"
    );
    assert_eq!(
        std::fs::read_to_string(server.dir.join(format!("{record}.isolation"))).unwrap(),
        format!(
            "PATH={}\nHOME={}\nCODEX_HOME={}\nCLAUDE_CONFIG_DIR={}\n",
            server.path(),
            server.dir.join("home").display(),
            server.dir.join("codex-home").display(),
            server.dir.join("claude-config").display()
        ),
        "T19 actual provider inherits the sealed scratch environment"
    );
    server.append(
        json!({"event":"provider_isolation", "generation":server.generation,
        "record":record, "provider":executable, "starts":starts,
        "exec_sha256":sha(&server.dir.join(format!("{record}.exec"))),
        "isolation_sha256":sha(&server.dir.join(format!("{record}.isolation")))}),
    );
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

fn assert_control_resume_metadata(value: &Value, control_cwd: &str, fake: &Path) -> usize {
    match value {
        Value::Object(object) if object.get("cwd").and_then(Value::as_str) == Some(control_cwd) => {
            assert_eq!(
                object.get("agent_session"),
                Some(&json!({
                    "source":"herdr:codex", "agent":"codex", "kind":"id", "value":"t19-sentinel"
                })),
                "T19 positive control persisted the upstream Codex session report"
            );
            assert_eq!(
                object.get("agent_resume"),
                Some(&json!({"source":"herdr:codex", "agent":"codex",
                    "argv":["env", fake, "resume", "t19-sentinel"]})),
                "T19 reported resume preserves the absolute fake provider path"
            );
            1
        }
        Value::Object(object) => object
            .values()
            .map(|item| assert_control_resume_metadata(item, control_cwd, fake))
            .sum(),
        Value::Array(values) => values
            .iter()
            .map(|item| assert_control_resume_metadata(item, control_cwd, fake))
            .sum(),
        _ => 0,
    }
}

#[test]
#[ignore = "needs ADK_TEST_HERDR_BIN: isolated scratch copy of a protocol-22 Herdr"]
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
        let path = server.fake(provider);
        if let Ok(metadata) = std::fs::symlink_metadata(&path) {
            assert!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "T19 replaces only its regular fake provider files"
            );
        }
        std::fs::write(&path, provider_script(&server.dir, provider)).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    server.start();
    let mut launched = Vec::new();
    let host = SocketHerdrLaunchHost::new(vec![server.endpoint()], HerdrSocketConfig::default());
    for (ordinal, provider) in ["claude", "codex"].into_iter().enumerate() {
        let script = format!("#!/bin/sh\nexec {}\n", server.fake(provider).display());
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
            command: format!("/bin/sh {}", path.display()),
            restore_off_witness: witness,
        });
        let pane = match outcome {
            HerdrCreateOutcome::Created { pane_id } => pane_id,
            other => panic!("T19 actual launch {provider}: {other:?}"),
        };
        let provider_pid = server.dir.join(format!("{provider}.pid"));
        wait_for(|| provider_pid.exists(), "fake provider started");
        let pid = std::fs::read_to_string(provider_pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        server.record(pid, provider);
        server.confirm_launch(ordinal);
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
        assert_provider_isolation(&server, provider, provider, 1);
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
    // The supported report API persists a real resume plan without agent.start's bare provider command.
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
    let control_info = server.process(&control_pane);
    server.record(control_info.shell_pid.unwrap(), "positive-control-shell");
    let control_command = format!("{} resume t19-sentinel", server.fake("codex").display());
    let started = server.raw_rpc(
        "t19-control-start",
        "pane.send_input",
        json!({
            "pane_id":control_pane, "text":control_command, "keys":["enter"]
        }),
    );
    assert_eq!(started["type"], "ok");
    let control_pid = server.dir.join("codex-control.pid");
    wait_for(|| control_pid.exists(), "positive control fake Codex");
    let control_provider: u32 = std::fs::read_to_string(&control_pid)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    server.record(control_provider, "positive-control-codex");
    let control_info = server.process(&control_pane);
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
    assert_provider_isolation(&server, "codex-control", "codex", 1);
    wait_for(
        || {
            std::fs::read_to_string(server.dir.join("home/xdg/herdr/herdr-server.log")).is_ok_and(
                |log| {
                    log.lines().any(|line| {
                        line.contains(&format!("process=codex pgid=Some({control_provider})"))
                    })
                },
            )
        },
        "the actual upstream observes the fake Codex process",
    );
    // 0.9.3 accepts a plain helper name, and env executes the following absolute provider path.
    let reported = server.raw_rpc(
        "t19-control-report",
        "pane.report_agent",
        json!({"pane_id":control_pane, "source":"herdr:codex", "agent":"codex",
            "state":"working", "seq":1, "agent_session_id":"t19-sentinel",
            "resume_argv":["env", server.fake("codex"), "resume", "t19-sentinel"]}),
    );
    assert_eq!(
        reported["type"], "ok",
        "T19 upstream accepts the supported absolute-provider resume plan"
    );
    server.append(
        json!({"event":"positive_control_report", "method":"pane.report_agent",
        "provider":server.fake("codex"), "resume_helper":"/usr/bin/env", "session":"t19-sentinel"}),
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
        assert_control_resume_metadata(
            &persisted,
            control_cwd.to_str().unwrap(),
            &server.fake("codex")
        ),
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
    assert_provider_isolation(&server, "codex-control", "codex", 2);
    server.append(json!({"event":"positive_control", "pane":control_pane, "provider_starts":2, "resumes":2, "resumed_provider":resumed_provider,
        "contract":"reported_resume_argv", "provider":server.fake("codex"), "resume_helper":"/usr/bin/env"}));
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
        let shell_start = provenance::process_start(shell).unwrap().identity;
        let shell_exe = provenance::process_exec_path(shell).expect("T19 shell exec is readable");
        assert_eq!(
            shell_exe, T19_SHELL,
            "T19 restored root is the configured shell"
        );
        assert_eq!(
            provenance::process_start(shell).unwrap().identity,
            shell_start,
            "T19 shell identity is unchanged across the OS exec read"
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
            "foreground":foreground.iter().map(|process| process.pid).collect::<Vec<_>>(), "shell_exec":shell_exe,
            "shell_comm":ps(shell, "comm="), "resumes":0, "provider_starts":1}));
    }
    assert!(server.stop(), "T19 second server stops without escalation");
}
