//! `assess` against a real protocol-22 herdr in an isolated HOME under `/private/tmp`: it reports
//! presence and eligibility, and never starts, stops or touches a server.
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::*;

/// A started server with the environment and arguments that address it again.
type Server = (Child, Vec<(String, String)>, Vec<String>);

/// One isolated HOME with the scratch herdr installed where the user would have it.
struct Home {
    _dir: tempfile::TempDir,
    home: PathBuf,
    bin: PathBuf,
    servers: Vec<Server>,
}

impl Home {
    fn new(config: &str) -> Self {
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
            _dir: dir,
            home,
            bin,
            servers: Vec::new(),
        }
    }

    fn socket(&self) -> PathBuf {
        self.home.join(".config/herdr/herdr.sock")
    }

    fn command(&self, env: &[(String, String)], args: &[String]) -> Command {
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
    fn serve(&mut self, env: &[(&str, &str)], args: &[&str], listens: &Path) -> u32 {
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let child = self.command(&env, &args).spawn().unwrap();
        let ready = Instant::now() + Duration::from_secs(10);
        while UnixStream::connect(listens).is_err() {
            assert!(Instant::now() < ready, "no server at {}", listens.display());
            std::thread::sleep(Duration::from_millis(50));
        }
        let pid = child.id();
        self.servers.push((child, env, args));
        pid
    }

    async fn assess(&self) -> Value {
        assess(
            &self.socket(),
            Environment {
                home: Some(self.home.clone()),
                // PATH comes before the known install dirs, so this HOME's copy is chosen.
                path: Some(
                    format!("{}:/usr/bin:/bin", self.bin.parent().unwrap().display()).into(),
                ),
                excluded: vec![self.home.join(".adk")],
            },
        )
        .await
    }

    /// herdr processes whose HOME is this one: an assessment must leave this set unchanged.
    fn herdr_pids(&self) -> Vec<u32> {
        let home = format!("HOME={}", self.home.display());
        let mut pids: Vec<u32> = presence::PresenceOs::herdr_processes(&presence::HostOs)
            .unwrap_or_default()
            .into_iter()
            .filter(|p| p.environ.as_ref().is_some_and(|env| env.contains(&home)))
            .map(|p| p.pid)
            .collect();
        pids.sort_unstable();
        pids
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let servers: Vec<_> = self.servers.drain(..).collect();
        for (mut child, env, args) in servers {
            let mut stop: Vec<String> = args.iter().filter(|a| *a != "server").cloned().collect();
            stop.extend(["server".into(), "stop".into()]);
            let _ = self.command(&env, &stop).status();
            let deadline = Instant::now() + Duration::from_secs(5);
            while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

const FALSE: &str = "[session]\nresume_agents_on_restore = false\n";

fn inode(path: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path).unwrap().ino()
}

#[tokio::test]
#[ignore = "needs ADK_TEST_HERDR_BIN: a protocol-22 herdr run in an isolated HOME (#7638)"]
async fn t7638_absent_and_stale_sockets_are_eligible_without_a_start() {
    let mut home = Home::new(FALSE);
    let report = home.assess().await;
    assert_eq!(report["presence"], "absent:no_file", "{report}");
    assert_eq!(
        (&report["eligible"], &report["reason"]),
        (&Value::Bool(true), &Value::from("eligible")),
        "{report}"
    );
    assert_eq!(report["binary"], home.bin.display().to_string(), "{report}");
    assert_eq!(report["version"], "0.9.5", "{report}");
    assert!(
        !home.socket().exists() && home.herdr_pids().is_empty(),
        "a dry run starts nothing"
    );

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
    let report = home.assess().await;
    assert_eq!(report["presence"], "absent:refused", "{report}");
    assert_eq!(report["eligible"], true, "{report}");
    assert!(home.herdr_pids().is_empty(), "a dry run starts nothing");
}

#[tokio::test]
#[ignore = "needs ADK_TEST_HERDR_BIN: a protocol-22 herdr run in an isolated HOME (#7638)"]
async fn t7638_a_live_server_is_present_and_untouched() {
    let mut home = Home::new(FALSE);
    let socket = home.socket();
    let pid = home.serve(&[], &["server"], &socket);
    let before = (inode(&socket), home.herdr_pids());
    let report = home.assess().await;
    assert_eq!(report["presence"], "present", "{report}");
    assert_eq!(
        (&report["eligible"], &report["reason"]),
        (&Value::Bool(false), &Value::from("not_needed_present")),
        "{report}"
    );
    assert_eq!((inode(&socket), home.herdr_pids()), before);
    assert_eq!(before.1, vec![pid]);
}

#[tokio::test]
#[ignore = "needs ADK_TEST_HERDR_BIN: a protocol-22 herdr run in an isolated HOME (#7638)"]
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
    assert!(!home.socket().exists());
    let report = home.assess().await;
    assert_eq!(report["presence"], "absent:no_file", "{report}");
    assert_eq!(report["eligible"], true, "{report}");
    let mut expected = vec![first, second];
    expected.sort_unstable();
    assert_eq!(
        home.herdr_pids(),
        expected,
        "both named servers kept, nothing started"
    );
}

#[tokio::test]
#[ignore = "needs ADK_TEST_HERDR_BIN: a protocol-22 herdr run in an isolated HOME (#7638)"]
async fn t7638_a_config_that_may_resume_is_not_eligible() {
    let home = Home::new("[session]\nresume_agents_on_restore = true\n");
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
}
