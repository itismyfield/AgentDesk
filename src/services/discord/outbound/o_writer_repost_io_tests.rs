use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::*;
use crate::services::tui_o::repost::send::{DispatchReport, RepostIds, send_within};

const CHANNEL: u64 = 63250101;
const MARKER: &str = "o:63250101:claude:msg_01:body:0";

/// Each request the mock answered: its path and body.
type Seen = Arc<StdMutex<Vec<(String, Vec<u8>)>>>;
/// Outcome kind, wire, counted and throttled.
type Summary = (&'static str, u32, u32, u32);

enum Reply {
    Raw(String),
    /// Closes the connection without answering.
    Drop,
    /// Holds the connection open without answering.
    Stall,
}

fn reply(status: &str, headers: &[(&str, &str)], body: &str) -> Reply {
    let mut raw = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        raw.push_str(&format!("{name}: {value}\r\n"));
    }
    raw.push_str("\r\n");
    raw.push_str(body);
    Reply::Raw(raw)
}

fn created_with(footer: Option<&str>, headers: &[(&str, &str)]) -> Reply {
    let embeds = footer.map_or_else(|| json!([]), |text| json!([{"footer": {"text": text}}]));
    let body = json!({"id": "9001", "author": {"id": "42"}, "content": "x", "embeds": embeds});
    reply("200 OK", headers, &body.to_string())
}

fn created(footer: Option<&str>) -> Reply {
    created_with(footer, &[])
}

fn route_429(retry_after: &str) -> Reply {
    reply(
        "429 Too Many Requests",
        &[
            ("retry-after", retry_after),
            ("x-ratelimit-limit", "5"),
            ("x-ratelimit-remaining", "0"),
            ("x-ratelimit-reset-after", retry_after),
        ],
        r#"{"message":"You are being rate limited.","retry_after":0.01,"global":false}"#,
    )
}

struct Mock {
    base: String,
    seen: Seen,
}

impl Mock {
    async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen: Seen = Arc::default();
        let mut replies = VecDeque::from(replies);
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let Some(request) = read_request(&mut socket).await else {
                    continue;
                };
                log.lock().unwrap().push(request);
                match replies
                    .pop_front()
                    .unwrap_or_else(|| reply("500 X", &[], ""))
                {
                    Reply::Raw(raw) => {
                        let _ = socket.write_all(raw.as_bytes()).await;
                    }
                    Reply::Drop => drop(socket),
                    Reply::Stall => {
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_secs(30)).await;
                            drop(socket);
                        });
                    }
                }
            }
        });
        Self { base, seen }
    }

    fn requests(&self) -> Vec<(String, Vec<u8>)> {
        self.seen.lock().unwrap().clone()
    }

    fn port(&self) -> RepostHttp {
        RepostHttp::at(
            Arc::new(serenity::Http::new("test-token")),
            self.base.clone(),
        )
        .unwrap()
    }
}

async fn read_request(socket: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = socket.read(&mut chunk).await.ok().filter(|n| *n > 0)?;
        buf.extend_from_slice(&chunk[..n]);
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let path = head.split_whitespace().nth(1)?.to_owned();
    let len = head
        .lines()
        .find_map(|line| {
            let line = line.to_ascii_lowercase();
            line.strip_prefix("content-length:")?.trim().parse().ok()
        })
        .unwrap_or(0);
    while buf.len() < head_end + len {
        let n = socket.read(&mut chunk).await.ok().filter(|n| *n > 0)?;
        buf.extend_from_slice(&chunk[..n]);
    }
    Some((path, buf[head_end..head_end + len].to_vec()))
}

fn envelope() -> RepostEnvelope {
    let ids = RepostIds::for_piece(MARKER).unwrap();
    RepostEnvelope::additional(CHANNEL, "본문 ```code```".into(), ids)
}

fn always() -> Arc<dyn Fn() -> bool + Send + Sync> {
    Arc::new(|| true)
}

async fn dispatch(port: &RepostHttp, live: Arc<dyn Fn() -> bool + Send + Sync>) -> DispatchReport {
    send_within(port, &envelope(), live, Duration::from_secs(10)).await
}

fn summary(report: &DispatchReport) -> Summary {
    let kind = match &report.outcome {
        WireOutcome::Created(_) => "created",
        WireOutcome::Refused(_) => "refused",
        WireOutcome::Throttled => "throttled",
        WireOutcome::Uncertain(_) => "uncertain",
        WireOutcome::Unsent(_) => "unsent",
        WireOutcome::TimedOut => "timed_out",
    };
    (kind, report.wire, report.counted, report.throttled)
}

#[tokio::test]
async fn bounded_create_counts_one_post_and_sends_again_only_after_a_confirmed_429() {
    let messages = format!("/api/v10/channels/{CHANNEL}/messages");
    let moved = [("location", "/api/v10/channels/63250101/moved")];
    let global_429 = reply(
        "429 Too Many Requests",
        &[("x-ratelimit-global", "true"), ("retry-after", "0.01")],
        r#"{"global":true}"#,
    );
    let bad_header = [("x-ratelimit-limit", "not-a-number")];
    let global_header = [("x-ratelimit-global", "true"), ("retry-after", "0.01")];
    let cases: Vec<(&str, Vec<Reply>, Summary)> = vec![
        (
            "route 429 twice then 200",
            vec![route_429("0.01"), route_429("0.01"), created(None)],
            ("created", 3, 1, 2),
        ),
        (
            "global 429 then 200",
            vec![global_429, created(None)],
            ("created", 2, 1, 1),
        ),
        (
            "200 with a malformed rate header",
            vec![created_with(None, &bad_header)],
            ("created", 1, 1, 0),
        ),
        (
            "403 with a malformed rate header",
            vec![reply("403 Forbidden", &bad_header, "{}")],
            ("refused", 1, 1, 0),
        ),
        (
            "500 with a malformed rate header",
            vec![reply("500 Internal Server Error", &bad_header, "{}")],
            ("uncertain", 1, 1, 0),
        ),
        (
            "200 carrying the global header",
            vec![created_with(None, &global_header)],
            ("created", 1, 1, 0),
        ),
        (
            "307 redirect",
            vec![reply("307 Temporary Redirect", &moved, ""), created(None)],
            ("uncertain", 1, 1, 0),
        ),
        (
            "308 redirect",
            vec![reply("308 Permanent Redirect", &moved, ""), created(None)],
            ("uncertain", 1, 1, 0),
        ),
        (
            "connection dropped",
            vec![Reply::Drop],
            ("uncertain", 1, 1, 0),
        ),
        (
            "200 with a negative reset-after",
            vec![created_with(None, &[("x-ratelimit-reset-after", "-1")])],
            ("created", 1, 1, 0),
        ),
        (
            "429 with a negative retry time",
            vec![reply(
                "429 Too Many Requests",
                &[("retry-after", "-1")],
                "{}",
            )],
            ("throttled", 1, 0, 1),
        ),
        (
            "429 without a retry time",
            vec![reply("429 Too Many Requests", &[], "{}")],
            ("throttled", 1, 0, 1),
        ),
    ];
    for (name, replies, expected) in cases {
        let mock = Mock::start(replies).await;
        let report = dispatch(&mock.port(), always()).await;
        assert_eq!(summary(&report), expected, "{name}: {report:?}");
        let requests = mock.requests();
        assert_eq!(requests.len() as u32, report.wire, "{name}: wire count");
        assert!(requests.iter().all(|(path, _)| *path == messages), "{name}");
        assert!(report.counted <= 1, "{name}");
    }
}

#[tokio::test]
async fn bounded_create_shares_the_gateway_route_bucket_and_refuses_a_lone_limiter() {
    let headers = [
        ("x-ratelimit-limit", "5"),
        ("x-ratelimit-remaining", "0"),
        ("x-ratelimit-reset-after", "30"),
    ];
    let body = json!({"id": "9001", "author": {"id": "42"}, "content": "x"});
    let mock = Mock::start(vec![reply("200 OK", &headers, &body.to_string())]).await;
    let http = Arc::new(serenity::Http::new("test-token"));
    let port = RepostHttp::at(Arc::clone(&http), mock.base.clone()).unwrap();
    assert_eq!(
        summary(&dispatch(&port, always()).await),
        ("created", 1, 1, 0)
    );

    let routes = http.ratelimiter.as_ref().unwrap().routes();
    let channel_id = ChannelId::new(CHANNEL);
    let bucket = Route::ChannelMessages { channel_id }.ratelimiting_bucket();
    let bucket = Arc::clone(routes.read().await.get(&bucket).expect("the shared bucket"));
    let bucket = bucket.lock().await;
    assert_eq!((bucket.limit(), bucket.remaining()), (5, 0));
    assert!(
        bucket.reset().is_some(),
        "Legacy now pre-waits on this route"
    );

    let lone = serenity::HttpBuilder::new("test-token")
        .ratelimiter_disabled(true)
        .build();
    assert_eq!(
        RepostHttp::new(Arc::new(lone)).err(),
        Some(Unsupported::NoRatelimiter)
    );
    let proxied = serenity::HttpBuilder::new("test-token")
        .proxy("http://127.0.0.1:9")
        .build();
    assert_eq!(
        RepostHttp::new(Arc::new(proxied)).err(),
        Some(Unsupported::Proxy)
    );
    assert!(RepostHttp::new(Arc::new(serenity::Http::new("test-token"))).is_ok());
}

#[tokio::test]
async fn repost_sends_keep_one_nonce_and_marker_outside_the_content() {
    let ids = RepostIds::for_piece(MARKER).unwrap();
    assert_eq!(ids.nonce().len(), 25);
    assert_eq!(RepostIds::for_piece(MARKER), Some(ids.clone()));
    for unusable in [String::new(), " ".into(), "x".repeat(2048)] {
        assert_eq!(
            RepostIds::for_piece(&unusable),
            None,
            "no marker, no re-post"
        );
    }

    let content = "첫 줄 😀\n```rust\nfn main() {}\n```".to_owned();
    let footer = format!("재확인 후 추가 전달 {MARKER}");
    let mock = Mock::start(vec![
        created(Some(&footer)),
        created(Some(&footer)),
        created(None),
        created(None),
    ])
    .await;
    let port = mock.port();
    let additional = RepostEnvelope::additional(CHANNEL, content.clone(), ids.clone());
    let original = RepostEnvelope::original(CHANNEL, content.clone(), ids.clone());
    let mut created_messages = Vec::new();
    for envelope in [&additional, &additional, &original] {
        let report = send_within(&port, envelope, always(), Duration::from_secs(10)).await;
        let WireOutcome::Created(message) = report.outcome else {
            panic!("{report:?}");
        };
        created_messages.push(message);
    }
    let legacy = serenity::HttpBuilder::new("test-token")
        .proxy(mock.base.clone())
        .ratelimiter_disabled(true)
        .build();
    let legacy_message = serenity::CreateMessage::new().content(content.clone());
    // The mock's reply is not a full Message, so only the request it recorded matters here.
    let _ = ChannelId::new(CHANNEL)
        .send_message(&legacy, legacy_message)
        .await;

    let bodies: Vec<Vec<u8>> = mock.requests().into_iter().map(|(_, body)| body).collect();
    assert_eq!(bodies.len(), 4);
    assert_eq!(
        bodies[0], bodies[1],
        "both re-post slots send the same bytes"
    );
    let repost: Value = serde_json::from_slice(&bodies[0]).unwrap();
    assert_eq!(
        repost["content"],
        json!(content),
        "content travels unchanged"
    );
    assert_eq!(repost["nonce"], json!(ids.nonce()));
    assert_eq!(repost["enforce_nonce"], json!(true));
    assert_eq!(repost["embeds"][0]["footer"]["text"], json!(footer));

    // The first post is Legacy's body plus the nonce, nothing else.
    let mut expected: Value = serde_json::from_slice(&bodies[3]).unwrap();
    expected["nonce"] = json!(ids.nonce());
    expected["enforce_nonce"] = json!(true);
    assert_eq!(
        serde_json::from_slice::<Value>(&bodies[2]).unwrap(),
        expected
    );

    assert!(additional.carries_marker(&created_messages[0]));
    assert!(!additional.carries_marker(&created_messages[2]));
    assert!(original.carries_marker(&created_messages[2]));
}

/// Marks whether the request future is still alive.
struct Probe<'a> {
    port: &'a RepostHttp,
    alive: Arc<AtomicBool>,
}

struct Alive(Arc<AtomicBool>);

impl Drop for Alive {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl BoundedTransport for Probe<'_> {
    fn create(
        &self,
        envelope: &RepostEnvelope,
        guard: AttemptGuard,
    ) -> impl Future<Output = WireOutcome> + Send + 'static {
        self.alive.store(true, Ordering::SeqCst);
        let (alive, request) = (
            Alive(Arc::clone(&self.alive)),
            self.port.create(envelope, guard),
        );
        async move {
            let _alive = alive;
            request.await
        }
    }
}

#[tokio::test]
async fn bounded_dispatch_times_out_with_the_request_gone_and_waits_429s_inside_the_timeout() {
    let timeout = Duration::from_millis(300);
    let mock = Mock::start(vec![Reply::Stall]).await;
    let (port, alive) = (mock.port(), Arc::new(AtomicBool::new(false)));
    let probe = Probe {
        port: &port,
        alive: Arc::clone(&alive),
    };
    let report = send_within(&probe, &envelope(), always(), timeout).await;
    assert_eq!(summary(&report), ("timed_out", 1, 1, 0));
    assert!(
        !alive.load(Ordering::SeqCst),
        "the timed-out request is gone"
    );

    let mock = Mock::start(vec![route_429("5")]).await;
    let started = Instant::now();
    let report = send_within(&mock.port(), &envelope(), always(), timeout).await;
    assert_eq!(summary(&report), ("timed_out", 1, 0, 1));
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the 429 wait counts"
    );

    // The guard is read again right before the retry leaves.
    let mock = Mock::start(vec![route_429("0.3"), created(None)]).await;
    let open = Arc::new(AtomicBool::new(true));
    let flag = Arc::clone(&open);
    let live: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(move || flag.load(Ordering::SeqCst));
    let closer = tokio::spawn({
        let open = Arc::clone(&open);
        async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            open.store(false, Ordering::SeqCst);
        }
    });
    let report = dispatch(&mock.port(), live).await;
    closer.await.unwrap();
    assert_eq!(summary(&report), ("unsent", 1, 0, 1));
    assert_eq!(mock.requests().len(), 1);

    let mock = Mock::start(vec![created(None)]).await;
    let report = dispatch(&mock.port(), Arc::new(|| false)).await;
    assert_eq!(summary(&report), ("unsent", 0, 0, 0));
    assert!(mock.requests().is_empty());
}
