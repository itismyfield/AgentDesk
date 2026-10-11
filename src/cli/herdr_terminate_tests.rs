//! `agentdesk herdr terminate` against loopback HTTP mocks: one POST to a checked loopback base
//! with no redirect, no database and no retry, and exit 0 only for the server's 200 `retired`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use clap::Parser;
use serde_json::{Value, json};

use super::{HerdrArgs, HerdrCommand, run};
use crate::cli::client::{LocalPostError, post_json_local_at, post_local};
use crate::config::TestEnvVarGuard;
use crate::config::test_env_lock::acquire_shared_test_env_lock;

const PATH: &str = "/api/herdr/terminate";

/// One request a mock received.
#[derive(Debug, Clone)]
struct Seen {
    line: String,
    headers: Vec<(String, String)>,
    body: String,
}

#[derive(Clone)]
enum Reply {
    Json(u16, Value),
    Redirect(u16, String),
    /// Reads the request and never answers.
    Stall,
}

/// A loopback HTTP server that records every connection and answers each with `reply`.
struct Mock {
    addr: SocketAddr,
    connections: Arc<AtomicUsize>,
    seen: Arc<Mutex<Vec<Seen>>>,
    reply: Arc<Mutex<Reply>>,
    stop: Arc<AtomicBool>,
}

impl Mock {
    fn start(reply: Reply) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mock = Self {
            addr: listener.local_addr().unwrap(),
            connections: Arc::default(),
            seen: Arc::default(),
            reply: Arc::new(Mutex::new(reply)),
            stop: Arc::default(),
        };
        let (connections, seen, reply, stop) = (
            mock.connections.clone(),
            mock.seen.clone(),
            mock.reply.clone(),
            mock.stop.clone(),
        );
        thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let Ok((stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(2));
                    continue;
                };
                connections.fetch_add(1, Ordering::SeqCst);
                let (seen, reply, stop) =
                    (seen.clone(), reply.lock().unwrap().clone(), stop.clone());
                thread::spawn(move || answer(stream, &seen, reply, &stop));
            }
        });
        mock
    }

    fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn set(&self, reply: Reply) {
        *self.reply.lock().unwrap() = reply;
    }

    fn take(&self) -> Vec<Seen> {
        std::mem::take(&mut *self.seen.lock().unwrap())
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

fn answer(stream: TcpStream, seen: &Mutex<Vec<Seen>>, reply: Reply, stop: &AtomicBool) {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    let _ = reader.read_line(&mut line);
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim().is_empty() {
            break;
        }
        let (name, value) = header.split_once(':').unwrap_or((header.as_str(), ""));
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    let length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0; length];
    let _ = reader.read_exact(&mut body);
    let body = String::from_utf8_lossy(&body).into_owned();
    seen.lock().unwrap().push(Seen {
        line: line.trim().to_string(),
        headers,
        body,
    });
    let (status, extra, payload) = match reply {
        Reply::Json(status, value) => (status, String::new(), value.to_string()),
        Reply::Redirect(status, location) => {
            (status, format!("location: {location}\r\n"), String::new())
        }
        Reply::Stall => {
            while !stop.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(20));
            }
            return;
        }
    };
    let response = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{payload}",
        payload.len()
    );
    let mut stream = stream;
    let _ = stream.write_all(response.as_bytes());
}

/// This test's environment: the shared env lock, a config file (with `token`, a database
/// nothing may open) and, when given, the API base the CLI resolves.
struct Env {
    _guards: Vec<TestEnvVarGuard>,
    _root: tempfile::TempDir,
    _lock: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

fn env(token: Option<&str>, api: Option<&str>) -> Env {
    let lock = acquire_shared_test_env_lock();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("config").join("agentdesk.yaml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let token = token.map_or(String::new(), |token| {
        format!("  auth_token: \"{token}\"\n")
    });
    let yaml = format!(
        "server:\n  port: 1\n{token}database:\n  enabled: true\n  host: 127.0.0.1\n  port: 1\n"
    );
    std::fs::write(&path, yaml).unwrap();
    let mut guards = vec![TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &path,
    )];
    let api = api.map(std::ffi::OsStr::new);
    guards.push(match api {
        Some(api) => {
            TestEnvVarGuard::set_value_after_shared_test_env_lock("AGENTDESK_API_URL", api)
        }
        None => TestEnvVarGuard::capture_after_shared_test_env_lock("AGENTDESK_API_URL"),
    });
    if api.is_none() {
        unsafe { std::env::remove_var("AGENTDESK_API_URL") };
    }
    Env {
        _guards: guards,
        _root: root,
        _lock: lock,
    }
}

fn body() -> Value {
    json!({"session_key": "claude/h/mac-mini:AgentDesk-claude-act", "execution_nonce": "n", "reason": "r"})
}

fn cli() -> Result<(), String> {
    run(HerdrArgs {
        command: HerdrCommand::Terminate {
            session_key: "claude/h/mac-mini:AgentDesk-claude-act".into(),
            execution_nonce: "n".into(),
            reason: "r".into(),
        },
    })
}

// A non-loopback base, or a name that merely looks loopback, is refused before any dial; the
// CLI takes the same path from `AGENTDESK_API_URL`.
#[test]
fn act_cli_terminate_refuses_remote_api_url() {
    let mock = Mock::start(Reply::Json(200, json!({"result": "retired"})));
    let port = mock.addr.port();
    let _env = env(None, Some("http://100.64.0.5:8791"));
    for base in [
        "http://100.64.0.5:9".to_string(),
        format!("http://127.0.0.1.nip.io:{port}"),
        format!("http://[::ffff:127.0.0.1]:{port}"),
    ] {
        assert_eq!(
            post_json_local_at(&base, PATH, &body()),
            Err(LocalPostError::NotLoopback),
            "{base}"
        );
    }
    let refused = cli().unwrap_err();
    assert!(refused.contains("NotLoopback"), "{refused}");
    thread::sleep(Duration::from_millis(200));
    assert_eq!(mock.connections.load(Ordering::SeqCst), 0);
}

// The CLI sends one POST with the three fields, the configured Bearer and no Origin, and never
// touches the database; only a 200 whose result is `retired` exits 0.
#[test]
fn act_cli_terminate_never_opens_postgres() {
    let mock = Mock::start(Reply::Json(200, json!({"result": "retired"})));
    for token in [Some("tok-cl2"), None] {
        let _env = env(token, Some(&mock.base()));
        assert_eq!(cli(), Ok(()), "token {token:?}");
        let seen = mock.take();
        assert_eq!(seen.len(), 1, "{seen:?}");
        assert_eq!(seen[0].line, format!("POST {PATH} HTTP/1.1"));
        let sent: Value = serde_json::from_str(&seen[0].body).unwrap();
        assert_eq!(sent, body());
        let header = |name: &str| {
            let found = seen[0].headers.iter().filter(|(n, _)| n == name);
            found.map(|(_, value)| value.clone()).collect::<Vec<_>>()
        };
        let bearer = token.map(|token| format!("Bearer {token}"));
        assert_eq!(
            header("authorization"),
            bearer.into_iter().collect::<Vec<_>>()
        );
        assert!(header("origin").is_empty());
    }
    let _env = env(None, Some(&mock.base()));
    for (status, result) in [(500, "retired"), (409, "retired"), (200, "refused")] {
        mock.set(Reply::Json(status, json!({"result": result})));
        assert!(cli().is_err(), "HTTP {status} {result} is not success");
    }
    assert_eq!(mock.take().len(), 3, "one request each, no retry");
}

#[derive(Parser)]
struct Command {
    #[command(subcommand)]
    command: HerdrCommand,
}

#[test]
fn act_cli_terminate_requires_all_fields() {
    let all = [
        "--session-key",
        "k",
        "--execution-nonce",
        "n",
        "--reason",
        "r",
    ];
    let parse = |skip: Option<usize>| {
        let args = all
            .chunks(2)
            .enumerate()
            .filter(|(at, _)| Some(*at) != skip);
        let args = args.flat_map(|(_, pair)| pair.iter().copied());
        Command::try_parse_from(["herdr", "terminate"].into_iter().chain(args))
    };
    assert!(matches!(
        parse(None).map(|parsed| parsed.command),
        Ok(HerdrCommand::Terminate { .. })
    ));
    for missing in 0..3 {
        assert!(parse(Some(missing)).is_err(), "argument {missing} missing");
    }
}

// Every redirect status is a failure answered by the first server only; its target is never
// dialled, directly or through the CLI.
#[test]
fn act_cli_terminate_never_follows_redirect() {
    let target = Mock::start(Reply::Json(200, json!({"result": "retired"})));
    let location = format!("{}{PATH}", target.base());
    let first = Mock::start(Reply::Json(500, json!({})));
    let _env = env(None, Some(&first.base()));
    for status in [301, 302, 303, 307, 308] {
        first.set(Reply::Redirect(status, location.clone()));
        assert_eq!(
            post_json_local_at(&first.base(), PATH, &body()),
            Err(LocalPostError::Redirect(status))
        );
        let failed = cli().unwrap_err();
        assert!(failed.contains(&format!("Redirect({status})")), "{failed}");
    }
    thread::sleep(Duration::from_millis(500));
    assert_eq!(target.connections.load(Ordering::SeqCst), 0);
    assert_eq!(first.take().len(), 10);
}

// A server that stops answering ends the wait with an error, and the request is never resent.
#[test]
fn act_cli_terminate_stalled_server_times_out_once() {
    let stalled = Mock::start(Reply::Stall);
    let _env = env(None, None);
    let (done, waited) = std::sync::mpsc::channel();
    let base = stalled.base();
    thread::spawn(move || {
        let _ = done.send(post_local(&base, PATH, &body(), Duration::from_millis(300)));
    });
    let result = waited
        .recv_timeout(Duration::from_secs(10))
        .expect("the read timeout ends the wait");
    assert!(
        matches!(result, Err(LocalPostError::Transport(_))),
        "{result:?}"
    );
    thread::sleep(Duration::from_millis(500));
    assert_eq!(stalled.connections.load(Ordering::SeqCst), 1);
}
