//! `assess` against a real protocol-22 herdr in an isolated HOME under `/private/tmp`: it reports
//! presence and eligibility, and never starts, stops or touches a server.
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::*;

#[path = "autostart/spawn_real_tests.rs"]
mod spawn;

/// A started server with the environment and arguments that address it again.
type Server = (Child, Vec<(String, String)>, Vec<String>);

/// One isolated HOME with the scratch herdr installed where the user would have it.
pub(super) struct Home {
    dir: tempfile::TempDir,
    pub home: PathBuf,
    pub bin: PathBuf,
    servers: Vec<Server>,
    /// Other processes this test started (launchers, helpers); killed by pid on drop.
    pub others: Vec<Child>,
}

/// What an assessment or a refused start must leave as it was.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct State {
    config: Option<Vec<u8>>,
    socket: Option<u64>,
    pids: Vec<u32>,
}

impl Home {
    pub fn new(config: &str) -> Self {
        let source =
            PathBuf::from(std::env::var("ADK_TEST_HERDR_BIN").expect("ADK_TEST_HERDR_BIN"));
        let dir = tempfile::Builder::new()
            .prefix("h7638t")
            .tempdir_in("/private/tmp")
            .unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".config/herdr")).unwrap();
        std::fs::write(home.join(".config/herdr/config.toml"), config).unwrap();
        let bin = home.join(".local/bin/herdr");
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::copy(source, &bin).unwrap();
        Self {
            dir,
            home,
            bin,
            servers: Vec::new(),
            others: Vec::new(),
        }
    }

    pub fn socket(&self) -> PathBuf {
        self.home.join(".config/herdr/herdr.sock")
    }

    pub fn records(&self) -> PathBuf {
        self.dir.path().join("records")
    }

    pub fn command(&self, env: &[(String, String)], args: &[String]) -> Command {
        let mut command = Command::new(&self.bin);
        command
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .envs(env.iter().cloned())
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    /// Starts `herdr <args> server` and waits until `listens` accepts; returns the server pid.
    pub fn serve(&mut self, env: &[(&str, &str)], args: &[&str], listens: &Path) -> u32 {
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let child = self.command(&env, &args).spawn().unwrap();
        wait_for(listens);
        let pid = child.id();
        self.servers.push((child, env, args));
        pid
    }

    /// PATH comes before the known install dirs, so this HOME's copy is chosen.
    pub fn env(&self) -> Environment {
        Environment {
            home: Some(self.home.clone()),
            path: Some(format!("{}:/usr/bin:/bin", self.bin.parent().unwrap().display()).into()),
            excluded: vec![self.home.join(".adk")],
        }
    }

    async fn assess(&self) -> Value {
        assess(&self.socket(), self.env()).await
    }

    /// herdr processes whose HOME is this one. A scan error or a herdr whose environment cannot
    /// be read fails the test instead of looking like no process.
    pub fn herdr_pids(&self) -> Vec<u32> {
        let home = format!("HOME={}", self.home.display());
        let processes = presence::PresenceOs::herdr_processes(&presence::HostOs)
            .expect("the process table must be readable to judge a start");
        let mut pids: Vec<u32> = processes
            .into_iter()
            .filter(|p| {
                p.environ
                    .as_ref()
                    .unwrap_or_else(|| panic!("environment of herdr {} unreadable", p.pid))
                    .contains(&home)
            })
            .map(|p| p.pid)
            .collect();
        pids.sort_unstable();
        pids
    }

    pub fn state(&self) -> State {
        use std::os::unix::fs::MetadataExt;
        State {
            config: std::fs::read(self.home.join(".config/herdr/config.toml")).ok(),
            socket: std::fs::symlink_metadata(self.socket())
                .ok()
                .map(|m| m.ino()),
            pids: self.herdr_pids(),
        }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let servers: Vec<_> = self.servers.drain(..).collect();
        for (mut child, env, args) in servers {
            let mut stop: Vec<String> = args.iter().filter(|a| *a != "server").cloned().collect();
            stop.extend(["server".into(), "stop".into()]);
            let _ = self.command(&env, &stop).status();
            settle(&mut child);
        }
        // A server AgentDesk started here is not a child of this test; stop it by its socket.
        let _ = self
            .command(&[], &["server".into(), "stop".into()])
            .status();
        for mut child in self.others.drain(..) {
            let _ = child.kill();
            let _ = child.wait();
        }
        let home = format!("HOME={}", self.home.display());
        let leftover = presence::PresenceOs::herdr_processes(&presence::HostOs).unwrap_or_default();
        for pid in leftover
            .iter()
            .filter(|p| p.environ.as_ref().is_some_and(|env| env.contains(&home)))
            .map(|p| p.pid)
        {
            // SAFETY: kill only signals; every pid here runs with this test's private HOME.
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
    }
}

fn settle(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub(super) fn wait_for(listens: &Path) {
    let ready = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(listens).is_err() {
        assert!(Instant::now() < ready, "no server at {}", listens.display());
        std::thread::sleep(Duration::from_millis(50));
    }
}

pub(super) const FALSE: &str = "[session]\nresume_agents_on_restore = false\n";

#[tokio::test]
#[ignore = "needs ADK_TEST_HERDR_BIN: a protocol-22 herdr run in an isolated HOME"]
async fn t7638_absent_and_stale_sockets_are_eligible_without_a_start() {
    let mut home = Home::new(FALSE);
    let before = home.state();
    let report = home.assess().await;
    assert_eq!(report["presence"], "absent:no_file", "{report}");
    assert_eq!(
        (&report["eligible"], &report["reason"]),
        (&Value::Bool(true), &Value::from("eligible")),
        "{report}"
    );
    assert_eq!(report["binary"], home.bin.display().to_string(), "{report}");
    assert_eq!(report["version"], "0.9.5", "{report}");
    assert_eq!(home.state(), before, "a dry run starts and writes nothing");
    assert!(before.pids.is_empty() && before.socket.is_none());

    let socket = home.socket();
    let pid = home.serve(&[], &["server"], &socket);
    let (mut child, _, _) = home.servers.pop().unwrap();
    assert_eq!(child.id(), pid);
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(
        home.socket().exists(),
        "SIGKILL leaves the stale socket file"
    );
    let before = home.state();
    let report = home.assess().await;
    assert_eq!(report["presence"], "absent:refused", "{report}");
    assert_eq!(report["eligible"], true, "{report}");
    assert_eq!(home.state(), before, "a dry run starts and writes nothing");
}

#[tokio::test]
#[ignore = "needs ADK_TEST_HERDR_BIN: a protocol-22 herdr run in an isolated HOME"]
async fn t7638_a_live_server_is_present_and_untouched() {
    let mut home = Home::new(FALSE);
    let socket = home.socket();
    let pid = home.serve(&[], &["server"], &socket);
    let before = home.state();
    let report = home.assess().await;
    assert_eq!(report["presence"], "present", "{report}");
    assert_eq!(
        (&report["eligible"], &report["reason"]),
        (&Value::Bool(false), &Value::from("not_needed_present")),
        "{report}"
    );
    assert_eq!(home.state(), before);
    assert_eq!(before.pids, vec![pid]);
}

#[tokio::test]
#[ignore = "needs ADK_TEST_HERDR_BIN: a protocol-22 herdr run in an isolated HOME"]
async fn t7638_servers_of_other_sockets_do_not_block_the_default() {
    let mut home = Home::new(FALSE);
    let named = |name: &str| {
        home.home
            .join(format!(".config/herdr/sessions/{name}/herdr.sock"))
    };
    let (work, alt) = (named("work"), named("alt"));
    let default = home.socket().display().to_string();
    let first = home.serve(&[("HERDR_SESSION", "work")], &["server"], &work);
    // An explicit session wins over HERDR_SOCKET_PATH even when that names the default socket.
    let second = home.serve(
        &[("HERDR_SOCKET_PATH", &default)],
        &["--session", "alt", "server"],
        &alt,
    );
    let before = home.state();
    let report = home.assess().await;
    assert_eq!(report["presence"], "absent:no_file", "{report}");
    assert_eq!(report["eligible"], true, "{report}");
    let mut expected = vec![first, second];
    expected.sort_unstable();
    assert_eq!(before.pids, expected);
    assert_eq!(
        home.state(),
        before,
        "both named servers kept, nothing started"
    );
}

#[tokio::test]
#[ignore = "needs ADK_TEST_HERDR_BIN: a protocol-22 herdr run in an isolated HOME"]
async fn t7638_a_config_that_may_resume_is_not_eligible() {
    let home = Home::new("[session]\nresume_agents_on_restore = true\n");
    let before = home.state();
    let report = home.assess().await;
    assert_eq!(report["presence"], "absent:no_file", "{report}");
    assert_eq!(
        (&report["eligible"], &report["reason"]),
        (&Value::Bool(false), &Value::from("config_resume_not_off")),
        "{report}"
    );
    assert!(
        report["action"]
            .as_str()
            .is_some_and(|a| a.contains("resume_agents_on_restore = false")),
        "{report}"
    );
    assert_eq!(home.state(), before, "a refusal starts and writes nothing");
}

#[tokio::test]
#[ignore = "needs ADK_TEST_HERDR_BIN: a protocol-22 herdr run in an isolated HOME"]
async fn t7638_a_config_the_binary_rejects_is_not_eligible() {
    // `explicit_false` accepts this; only the candidate's own `config check` rejects the key.
    let home = Home::new("[session]\nresume_agents_on_restore = false\nbogus_key = 1\n");
    let before = home.state();
    let report = home.assess().await;
    assert_eq!(report["reason"], "config_check_failed", "{report}");
    assert_eq!(report["binary"], home.bin.display().to_string(), "{report}");
    assert_eq!(
        home.state(),
        before,
        "the check reads the config and changes nothing"
    );
}

#[tokio::test]
#[ignore = "needs ADK_TEST_HERDR_BIN: a protocol-22 herdr run in an isolated HOME"]
async fn t7638_the_host_scan_finds_a_server_whose_socket_file_is_gone() {
    let mut home = Home::new(FALSE);
    let socket = home.socket();
    let pid = home.serve(&[], &["server"], &socket);
    // The test's own server keeps running while its socket path is missing.
    std::fs::remove_file(&socket).unwrap();
    let report = home.assess().await;
    assert_eq!(
        report["presence"],
        format!("unknown:herdr_process_{pid}_server"),
        "{report}"
    );
    assert_eq!(report["reason"], "presence_unknown", "{report}");
}
