//! The probe against an in-process socket fixture whose pong speaks an older protocol, so the
//! check refuses before it reads or hashes any file.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use serde_json::{Value, json};

use super::super::{HerdrArgs, run as herdr_run};
use crate::config::TestEnvVarGuard;
use crate::config::test_env_lock::acquire_shared_test_env_lock;

/// Every request line the fixture read, one per connection that sent one.
struct Fixture {
    requests: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Fixture {
    fn serve(socket: &Path) -> Self {
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let (requests, stop) = (
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(AtomicBool::new(false)),
        );
        let (seen, stopped) = (requests.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                let Ok((stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                let mut writer = stream.try_clone().unwrap();
                let mut line = String::new();
                let _ = BufReader::new(stream).read_line(&mut line);
                let Ok(request) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let pong = json!({"id": request["id"], "result":
                    {"type": "pong", "version": "0.9.3", "protocol": 21}});
                let _ = writer.write_all(format!("{pong}\n").as_bytes());
                seen.lock().unwrap().push(request);
            }
        });
        Self {
            requests,
            stop,
            thread: Some(thread),
        }
    }

    fn methods(&self) -> Vec<String> {
        let requests = self.requests.lock().unwrap();
        requests
            .iter()
            .map(|request| request["method"].as_str().unwrap_or("?").to_owned())
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Everything under `dir`, so a probe that wrote anything shows up.
fn tree(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            found.extend(tree(&path));
        }
        found.push(path);
    }
    found.sort();
    found
}

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: Top,
}

#[derive(clap::Subcommand)]
enum Top {
    Herdr(HerdrArgs),
}

fn parse(args: &[&str]) -> HerdrArgs {
    let Top::Herdr(args) = Cli::try_parse_from(["agentdesk", "herdr"].iter().chain(args))
        .unwrap()
        .command;
    args
}

#[test]
fn probe_user_default_reports_the_reason_without_config_database_or_writes() {
    let _lock = acquire_shared_test_env_lock();
    // Short enough for a Unix socket path on every runner.
    let dir = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
    let root = dir.path().join("agentdesk-root");
    std::fs::create_dir(&root).unwrap();
    let missing_config = dir.path().join("absent.yaml");
    let _guards = [
        TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", &root),
        TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_CONFIG", &missing_config),
    ];
    let home = dir.path().join("home");
    let socket = home.join(".config/herdr/herdr.sock");
    let server = Fixture::serve(&socket);
    let before = tree(dir.path());

    let socket_arg = socket.to_str().unwrap();
    assert_eq!(
        herdr_run(parse(&["probe-user-default", "--socket", socket_arg])),
        Ok(()),
        "probe_cli_runs_without_config_or_database_contract"
    );
    assert!(
        server.methods().iter().any(|method| method == "ping"),
        "probe_cli_reaches_the_named_socket_contract: {:?}",
        server.methods()
    );

    let mut printed = Vec::new();
    super::run(None, Some(home.clone().into()), &mut printed).unwrap();
    let report: Value = serde_json::from_slice(&printed).unwrap();
    assert_eq!(
        (
            &report["next_restart_ready"],
            &report["reason"],
            &report["socket"]
        ),
        (
            &json!(false),
            &json!("next_restart_policy_unverified:Peer"),
            &json!(socket_arg)
        ),
        "probe_default_socket_reports_the_policy_reason_contract: {report}"
    );
    assert!(
        report["action"]
            .as_str()
            .is_some_and(|action| action.contains("start bare herdr")),
        "probe_reports_the_same_action_as_health_contract: {report}"
    );

    assert!(super::run(Some("relative.sock".into()), None, &mut Vec::new()).is_err());
    assert!(
        server.methods().iter().all(|method| method == "ping"),
        "probe_sends_only_read_only_pings_contract: {:?}",
        server.methods()
    );
    assert_eq!(
        tree(dir.path()),
        before,
        "probe_writes_no_config_runtime_or_herdr_file_contract"
    );
    assert!(!missing_config.exists());
}
