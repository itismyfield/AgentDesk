//! `agentdesk o transition inspect` against loopback mocks: one POST to a loopback base carrying
//! the base's Origin and no token, no redirect or environment proxy followed, and an answer it
//! cannot prove harmless never shown as one.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use serde_json::json;

use super::http::{PostError, READ, post};
use super::{TransitionCommand, render};
use crate::config::TestEnvVarGuard;
use crate::config::test_env_lock::acquire_shared_test_env_lock;

/// A loopback server answering every connection with `reply`, keeping each request's head.
struct Mock {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Mock {
    fn start(reply: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let (addr, seen) = (
            listener.local_addr().unwrap(),
            Arc::<Mutex<Vec<String>>>::default(),
        );
        let (kept, reply) = (Arc::clone(&seen), reply.to_owned());
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let (mut head, mut line, mut length) = (String::new(), String::new(), 0);
                while reader.read_line(&mut line).unwrap_or(0) > 0 && line != "\r\n" {
                    let lower = line.to_ascii_lowercase();
                    if let Some(value) = lower.strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap_or(0);
                    }
                    head.push_str(&std::mem::take(&mut line));
                }
                let _ = reader.read_exact(&mut vec![0; length]);
                kept.lock().unwrap().push(head);
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        Self { addr, seen }
    }

    fn base(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

fn answer(status: &str, body: &str) -> String {
    let length = body.len();
    format!("HTTP/1.1 {status}\r\ncontent-length: {length}\r\nconnection: close\r\n\r\n{body}")
}

const INSPECTED: &str = r#"{"ok":true,"result":"inspected"}"#;

// The one request goes to the transition path with the base's own Origin and no Authorization; a
// base that is not loopback is refused before any dial.
#[test]
fn transition_cli_posts_its_bases_origin_and_no_token() {
    let server = Mock::start(&answer("200 OK", INSPECTED));
    let (status, body) = post(&server.base(), &json!({"operation": "inspect"}), READ).unwrap();
    assert_eq!((status, body["result"].as_str()), (200, Some("inspected")));
    let seen = server.seen();
    assert_eq!(seen.len(), 1, "[C1:one_request] {seen:?}");
    let head = seen[0].to_ascii_lowercase();
    assert!(
        head.starts_with("post /api/o/transition "),
        "[C1:path] {head}"
    );
    let origin = format!("\r\norigin: {}\r\n", server.base());
    assert!(head.contains(&origin), "[C1:origin] {head}");
    assert!(!head.contains("\r\nauthorization:"), "[C1:no_token] {head}");
    let remote = post("http://192.0.2.1:8791", &json!({}), READ);
    assert!(
        matches!(remote, Err(PostError::NotLoopback)),
        "[C1:remote] {remote:?}"
    );
}

// A redirect is a failure answered by the first server alone; its target never hears of it. A 303
// is one an HTTP client follows for a POST by default.
#[test]
fn transition_cli_never_follows_a_redirect() {
    for code in [303, 307] {
        let target = Mock::start(&answer("200 OK", INSPECTED));
        let location = format!("location: {}/api/o/transition\r\n", target.base());
        let first = Mock::start(&format!(
            "HTTP/1.1 {code} X\r\n{location}content-length: 0\r\nconnection: close\r\n\r\n"
        ));
        let answered = post(&first.base(), &json!({}), READ);
        assert!(
            matches!(answered, Err(PostError::Redirect(status)) if status == code),
            "[C2:{code}] {answered:?}"
        );
        assert_eq!(first.seen().len(), 1, "[C2:{code}:first]");
        assert!(target.seen().is_empty(), "[C2:{code}:target_untouched]");
    }
}

// A proxy named in the environment is never dialled; the request goes straight to the base.
#[test]
fn transition_cli_ignores_an_environment_proxy() {
    let _lock = acquire_shared_test_env_lock();
    let (server, proxy) = (Mock::start(&answer("200 OK", INSPECTED)), Mock::start(""));
    let named = std::ffi::OsString::from(proxy.base());
    let _proxies = ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"]
        .map(|key| TestEnvVarGuard::set_value_after_shared_test_env_lock(key, &named));
    let answered = post(&server.base(), &json!({}), READ);
    assert!(answered.is_ok(), "[C3] {answered:?}");
    assert!(proxy.seen().is_empty(), "[C3:proxy_untouched]");
    assert_eq!(server.seen().len(), 1, "[C3:direct]");
}

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    command: TransitionCommand,
}

// The command takes no token: the server admits the local operator by peer and Origin alone.
#[test]
fn transition_cli_has_no_token_argument() {
    let args = [
        "o",
        "inspect",
        "--channel",
        "1",
        "--session-key",
        "k",
        "--execution-nonce",
        "n",
        "--cutoff",
        "2",
        "--operator",
        "op",
        "--reason",
        "r",
    ];
    assert!(Cli::try_parse_from(args).is_ok(), "[C4:parses]");
    let token = args.iter().copied().chain(["--token", "t"]);
    let refused = Cli::try_parse_from(token).err().map(|error| error.kind());
    assert_eq!(
        refused,
        Some(clap::error::ErrorKind::UnknownArgument),
        "[C4:no_token]"
    );
}

// Busy, InProgress, a lost reply and no answer at all keep their own notice and are never shown as
// a request the actor did not get.
#[test]
fn transition_cli_never_shows_an_unproven_answer_as_unsent() {
    let refused = |refused: &str| json!({"result": "actor_refused", "reply": {"refused": refused}});
    let cases = [
        (Ok((409, refused("Busy"))), "다른 job이 실행 중입니다"),
        (Ok((409, refused("InProgress"))), "완료 영수증이 아닙니다"),
        (
            Ok((502, json!({"result": "reply_lost"}))),
            "쓰기가 없었다고 보지 마세요",
        ),
        (
            Err(PostError::Transport("timed out".into())),
            "쓰기가 없었다고 보지 마세요",
        ),
    ];
    for (answer, notice) in cases {
        let shown = render(&answer);
        assert!(shown.contains(notice), "[C5:notice] {shown}");
        assert!(
            !shown.contains("actor에 보내지 않았습니다"),
            "[C5:not_unsent] {shown}"
        );
    }
    let unsent = render(&Ok((
        403,
        json!({"result": "forbidden", "detail": "local operator only"}),
    )));
    assert!(
        unsent.contains("actor에 보내지 않았습니다"),
        "[C5:unsent] {unsent}"
    );
}
