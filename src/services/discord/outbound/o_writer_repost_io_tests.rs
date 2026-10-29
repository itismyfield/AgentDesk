use std::collections::VecDeque;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Instant;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::*;
use crate::services::tui_o::repost::send::{DispatchReport, RepostIds, hand_over, send_within};

const CHANNEL: u64 = 63250101;
const MARKER: &str = "o:63250101:claude:msg_01:body:0";

/// Each request the mock answered: its path and body.
type Seen = Arc<StdMutex<Vec<(String, Vec<u8>)>>>;
/// Outcome kind, wire, counted and throttled.
type Summary = (&'static str, u32, u32, u32);

impl RepostHttp {
    /// Keeps the factory's client and rate authority while directing test requests to loopback.
    pub(crate) fn at_base(mut self, base: String) -> Self {
        self.base = Some(base);
        self
    }
}

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
    probes: Arc<AtomicUsize>,
}

impl Mock {
    async fn start(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen: Seen = Arc::default();
        let mut replies = VecDeque::from(replies);
        let log = Arc::clone(&seen);
        let probes = Arc::new(AtomicUsize::new(0));
        let ignored = Arc::clone(&probes);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let Some(request) = read_request(&mut socket).await else {
                    continue;
                };
                // Local port probes must not consume scripted Discord replies.
                if request.0 == "/" {
                    ignored.fetch_add(1, Ordering::SeqCst);
                    let _ = socket.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
                    continue;
                }
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
        Self { base, seen, probes }
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
        WireOutcome::Withheld(_) => "withheld",
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
async fn f6_gateway_and_delegated_transport_share_rate_authority() {
    use crate::services::discord::outbound::o_writer_io::GatewayPort;

    let gateway = Arc::new(serenity::Http::new("gateway-token"));
    let delegated = Arc::new(serenity::Http::new("rest-token"));
    let gateway_routes = gateway.ratelimiter.as_ref().unwrap().routes();
    let delegated_routes = delegated.ratelimiter.as_ref().unwrap().routes();
    assert!(!Arc::ptr_eq(&gateway_routes, &delegated_routes));
    let bucket_key = Route::ChannelMessages {
        channel_id: ChannelId::new(CHANNEL),
    }
    .ratelimiting_bucket();
    for (name, http) in [("gateway", gateway), ("delegated REST", delegated)] {
        let routes = http.ratelimiter.as_ref().unwrap().routes();
        let authority = Arc::clone(routes.write().await.entry(bucket_key).or_default());
        let mock = Mock::start(vec![
            created_with(
                None,
                &[
                    ("x-ratelimit-limit", "1"),
                    ("x-ratelimit-remaining", "0"),
                    ("x-ratelimit-reset-after", "0.30"),
                ],
            ),
            created(None),
        ])
        .await;
        let port = GatewayPort::new(Arc::clone(&http), 42);
        let transport = port.repost_http().unwrap().at_base(mock.base.clone());
        assert_eq!(
            summary(&dispatch(&transport, always()).await),
            ("created", 1, 1, 0),
            "{name}"
        );
        assert_eq!(
            mock.requests().len(),
            1,
            "{name}: first POST reached loopback"
        );
        assert!(
            Arc::ptr_eq(routes.read().await.get(&bucket_key).unwrap(), &authority),
            "{name}"
        );
        let bucket = authority.lock().await;
        assert_eq!(
            (bucket.limit(), bucket.remaining()),
            (1, 0),
            "{name}: the port's own bucket was charged"
        );
        assert!(bucket.reset().is_some(), "{name}");
        drop(bucket);

        let started = Instant::now();
        assert_eq!(
            summary(&dispatch(&transport, always()).await),
            ("created", 1, 1, 0),
            "{name}"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(200),
            "{name}: shared route must actually pre-wait"
        );
        let requests = mock.requests();
        assert_eq!(
            requests.len(),
            2,
            "{name}: positive control reached loopback twice"
        );
        assert!(
            requests
                .iter()
                .all(|(path, _)| path == &format!("/api/v10/channels/{CHANNEL}/messages"))
        );
    }
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
    let legacy_message = serenity::CreateMessage::new()
        .content(content.clone())
        .allowed_mentions(crate::services::discord::http::relay_allowed_mentions());
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

/// A live check that counts how often the guard consulted it.
fn counted_live(
    calls: &Arc<std::sync::atomic::AtomicUsize>,
    answer: bool,
) -> Arc<dyn Fn() -> bool + Send + Sync> {
    let calls = Arc::clone(calls);
    Arc::new(move || {
        calls.fetch_add(1, Ordering::SeqCst);
        answer
    })
}

#[tokio::test]
async fn f1r_a_handed_over_request_carries_its_guard_and_counts_like_send_within() {
    let replies = || vec![route_429("0.01"), created(None)];
    let mock = Mock::start(replies()).await;
    let calls = Arc::default();
    let (request, counts) = hand_over(&mock.port(), &envelope(), counted_live(&calls, true));
    assert_eq!(
        summary(&counts.report(WireOutcome::TimedOut)),
        ("timed_out", 0, 0, 0)
    );
    assert_eq!(
        (mock.requests().len(), calls.load(Ordering::SeqCst)),
        (0, 0),
        "not polled yet"
    );

    // The first poll may run anywhere, e.g. under the gate; the rest runs in its own task.
    let mut request = Box::pin(request);
    let first = std::future::poll_fn(|cx| std::task::Poll::Ready(request.as_mut().poll(cx))).await;
    assert!(first.is_pending());
    let outcome = tokio::spawn(request).await.unwrap();
    let handed = counts.report(outcome);
    assert_eq!(summary(&handed), ("created", 2, 1, 1));
    assert_eq!(mock.requests().len(), 2);

    let reference = Mock::start(replies()).await;
    let sent = dispatch(&reference.port(), always()).await;
    assert_eq!(
        summary(&sent),
        summary(&handed),
        "the same counts as send_within"
    );
}

#[tokio::test]
async fn f1r_an_unpolled_request_dropped_sends_and_counts_nothing() {
    let mock = Mock::start(vec![created(None)]).await;
    let calls = Arc::default();
    let (request, counts) = hand_over(&mock.port(), &envelope(), counted_live(&calls, true));
    drop(request);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        summary(&counts.report(WireOutcome::Unsent("dropped".into()))),
        ("unsent", 0, 0, 0)
    );
    assert_eq!(
        (mock.requests().len(), calls.load(Ordering::SeqCst)),
        (0, 0)
    );

    // A withdrawn guard stops the polled request before it leaves.
    let (request, counts) = hand_over(&mock.port(), &envelope(), counted_live(&calls, false));
    let outcome = request.await;
    assert_eq!(summary(&counts.report(outcome)), ("unsent", 0, 0, 0));
    assert_eq!(
        (mock.requests().len(), calls.load(Ordering::SeqCst)),
        (0, 1)
    );
}

/// A success whose rate header is finite but far too large for the bucket's clock stays a
/// success with one POST: it never reaches the hook, is never reclassified and never sent again.
#[tokio::test]
async fn f6_hookable_overflow_on_success_never_reclassifies_or_reposts() {
    for huge in ["1e19", "18446744073709551615", "9e15"] {
        let headers = [
            ("x-ratelimit-limit", "5"),
            ("x-ratelimit-remaining", "4"),
            ("x-ratelimit-reset-after", huge),
            ("retry-after", huge),
        ];
        let mock = Mock::start(vec![created_with(Some(MARKER), &headers)]).await;
        let report = dispatch(&mock.port(), always()).await;
        assert_eq!(summary(&report), ("created", 1, 1, 0), "{huge}");
        assert_eq!(mock.requests().len(), 1, "{huge}");
    }
}

/// The writer settling a POST whose reply was lost, through Serenity's client to loopback.
#[cfg(unix)]
mod settle_over_http {
    use chrono::Utc;

    use super::*;
    use crate::services::discord::outbound::o_writer_io::GatewayPort;
    use crate::services::tui_o::ownership::OwnershipGate;
    use crate::services::tui_o::shadow::{ShadowProvider, UnitKey, UnitKind};
    use crate::services::tui_o::store::ledger::{LedgerEntry, PieceOutcome};
    use crate::services::tui_o::store::{Initialized, OStore, StoreConfig};
    use crate::services::tui_o::writer::confirm::LAST_LOOK;
    use crate::services::tui_o::writer::deliver::{ChannelWriter, Step};
    use crate::services::tui_o::writer::pieces::{Derived, PieceWork};
    use crate::services::tui_o::writer::{AlarmSink, DeliveryLease, WriterAlarm};

    pub(super) const BOT: u64 = 42;
    pub(super) const OTHER: u64 = 77;
    pub(super) const PAYLOAD: &str = "transcript unit";
    const READ_TIMEOUT: Duration = Duration::from_secs(5);

    pub(super) struct Lease;

    impl DeliveryLease for Lease {
        type Held = ();

        fn try_acquire(&self, _: u64, _: u64) -> Option<()> {
            Some(())
        }
    }

    #[derive(Clone, Default)]
    pub(super) struct Alarms(Arc<StdMutex<Vec<WriterAlarm>>>);

    impl AlarmSink for Alarms {
        fn raise(&self, _: u64, alarm: WriterAlarm) {
            self.0.lock().unwrap().push(alarm);
        }
    }

    /// A message as Discord returns it, complete enough for Serenity to decode.
    pub(super) fn message(channel: u64, id: u64, author: u64, content: &str) -> Value {
        json!({
            "id": id.to_string(), "channel_id": channel.to_string(), "content": content,
            "author": {"id": author.to_string(), "username": "bot", "discriminator": "0",
                "global_name": null, "avatar": null, "bot": true},
            "timestamp": "2026-10-15T00:00:00.000000+00:00", "edited_timestamp": null,
            "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
            "attachments": [], "embeds": [], "pinned": false, "type": 0, "flags": 0
        })
    }

    pub(super) fn ok(body: Value) -> Reply {
        reply("200 OK", &[], &body.to_string())
    }

    pub(super) fn status(line: &str) -> Reply {
        reply(line, &[], r#"{"message":"refused","code":0}"#)
    }

    pub(super) fn unit(channel: u64, native: u64) -> UnitKey {
        UnitKey {
            channel_id: channel,
            provider: ShadowProvider::Claude,
            native_key: format!("msg_{native}"),
            kind: UnitKind::Body,
        }
    }

    #[derive(Debug, PartialEq)]
    enum Expect {
        NotFound,
        Unresolved(&'static str),
    }

    struct Row {
        name: &'static str,
        /// Ids the ledger already records as this bot's posts, oldest first.
        posted: Vec<u64>,
        /// Replies after the lost POST's, in request order.
        replies: Vec<Reply>,
        expect: Expect,
        /// History pages and reads by id the settlement made.
        lists: usize,
        proofs: Vec<u64>,
    }

    struct Settled {
        outcome: Option<PieceOutcome>,
        alarms: Vec<WriterAlarm>,
        requests: Vec<(String, Vec<u8>)>,
        elapsed: Duration,
    }

    /// Delivers one piece whose POST connection drops, on a channel whose replayed ledger already
    /// holds `posted`.
    async fn settle_lost_post(channel: u64, posted: &[u64], replies: Vec<Reply>) -> Settled {
        let mock = Mock::start(std::iter::once(Reply::Drop).chain(replies).collect()).await;
        let runtime = tempfile::tempdir().unwrap();
        let config = StoreConfig { enabled: true };
        let store = OStore::open_if_enabled(&config, runtime.path())
            .unwrap()
            .unwrap();
        let init = |channel| {
            let (sources, build_digest, at) = (Vec::new(), "b".to_owned(), Utc::now());
            let initial_anchor = 100;
            Ok(Initialized {
                channel,
                sources,
                initial_anchor,
                build_digest,
                at,
            })
        };
        let era = store.begin_era(&[channel], Utc::now(), init).unwrap();
        let mut recorded = store.open_channel(&era, channel).unwrap().unwrap();
        for (serial, &msg_id) in (0u64..).zip(posted) {
            let anchor_id = recorded.ledger().anchor();
            let (unit_key, payload) = (unit(channel, serial), format!("earlier {serial}"));
            let prepared = LedgerEntry::Prepared {
                notice_origin: None,
                local_retry: None,
                serial,
                unit_key,
                piece_index: 0,
                payload,
                anchor_id,
                epoch: 1,
            };
            recorded.append_ledger(prepared).unwrap();
            recorded
                .append_ledger(LedgerEntry::Posted { serial, msg_id })
                .unwrap();
        }
        drop(recorded);
        let gate = Arc::new(OwnershipGate::default());
        gate.acquired();
        let http = serenity::HttpBuilder::new("test-token")
            .proxy(mock.base.clone())
            .ratelimiter_disabled(true)
            .build();
        let port = Arc::new(GatewayPort::new(Arc::new(http), BOT));
        let alarms = Alarms::default();
        let replayed = store.open_channel(&era, channel).unwrap().unwrap();
        let mut writer = ChannelWriter::new(replayed, gate, port, Lease, alarms.clone());
        let unit_key = unit(channel, 99);
        let payload = PAYLOAD.to_owned();
        let piece = Derived::Piece(PieceWork {
            output_origin: None,
            unit_key: unit_key.clone(),
            index: 0,
            payload,
            notice_origin: None,
        });
        let started = Instant::now();
        assert_eq!(writer.deliver(&piece).await, Step::Done);
        let elapsed = started.elapsed();
        let ledger = writer.store().ledger();
        let outcome = ledger
            .latest_piece(&unit_key, 0)
            .and_then(|(_, piece)| piece.outcome.clone());
        let alarms = alarms.0.lock().unwrap().clone();
        let requests = mock.requests();
        Settled {
            outcome,
            alarms,
            requests,
            elapsed,
        }
    }

    /// Every way `settled` departs from `row`, empty when it matches.
    fn mismatches(row: &Row, channel: u64, settled: &Settled) -> Vec<String> {
        let serial = row.posted.len() as u64;
        let mut wrong = Vec::new();
        match (&row.expect, &settled.outcome, settled.alarms.as_slice()) {
            (
                Expect::NotFound,
                Some(PieceOutcome::NotFound),
                [WriterAlarm::NotFound { serial: s }],
            ) if *s == serial => {}
            (
                Expect::Unresolved(why),
                Some(PieceOutcome::Unresolved(reason)),
                [WriterAlarm::Unresolved { serial: s, .. }],
            ) if *s == serial && reason.contains(why) => {}
            (expect, outcome, alarms) => {
                wrong.push(format!(
                    "expected {expect:?}, got {outcome:?} with {alarms:?}"
                ));
            }
        }
        let messages = format!("/api/v10/channels/{channel}/messages");
        let posts = (settled.requests.iter())
            .filter(|(path, body)| path == &messages && !body.is_empty())
            .count();
        let lists = (settled.requests.iter())
            .filter(|(path, _)| path.starts_with(&format!("{messages}?")))
            .count();
        let proofs: Vec<u64> = (settled.requests.iter())
            .filter_map(|(path, _)| path.strip_prefix(&format!("{messages}/"))?.parse().ok())
            .collect();
        let requests = settled.requests.len();
        let expected = 1 + row.lists + row.proofs.len();
        if (posts, lists, &proofs, requests) != (1, row.lists, &row.proofs, expected) {
            wrong.push(format!(
                "POSTs {posts}, pages {lists}, reads by id {proofs:?}, requests {requests}"
            ));
        }
        // Two looks of one stalled read each stay well inside the settlement.
        let bound = LAST_LOOK + READ_TIMEOUT * 2 + Duration::from_secs(3);
        if settled.elapsed >= bound {
            wrong.push(format!("took {:?}", settled.elapsed));
        }
        wrong
    }

    /// Absence needs a history read Discord could not have answered without permission: a
    /// non-empty page, or an empty one with a known own message read back by id.
    #[tokio::test]
    async fn a_lost_post_is_absent_only_when_a_real_read_proves_history_readable() {
        let proof = |channel| ok(message(channel, 5000, BOT, "earlier 0"));
        let other = |channel| ok(json!([message(channel, 6000, OTHER, "chatter")]));
        let altered = |channel| ok(json!([message(channel, 6000, BOT, "Discord가 바꾼 본문")]));
        let empty = || ok(json!([]));
        let base = 63_250_200;
        let rows = |channel: u64| -> Vec<Row> {
            vec![
                Row {
                    name: "empty pages, proof read refused",
                    posted: vec![5000],
                    replies: vec![empty(), empty(), status("403 Forbidden")],
                    expect: Expect::Unresolved("permission proof read failed"),
                    lists: 2,
                    proofs: vec![5000],
                },
                Row {
                    name: "empty pages, proof read back",
                    posted: vec![5000],
                    replies: vec![empty(), empty(), proof(channel)],
                    expect: Expect::NotFound,
                    lists: 2,
                    proofs: vec![5000],
                },
                Row {
                    name: "empty pages, no own post to prove with",
                    posted: vec![],
                    replies: vec![empty(), empty()],
                    expect: Expect::Unresolved("without read permission proof"),
                    lists: 2,
                    proofs: vec![],
                },
                Row {
                    name: "first page 500, last empty with proof",
                    posted: vec![5000],
                    replies: vec![status("500 Internal Server Error"), empty(), proof(channel)],
                    expect: Expect::NotFound,
                    lists: 2,
                    proofs: vec![5000],
                },
                Row {
                    name: "only another author listed",
                    posted: vec![],
                    replies: vec![other(channel), other(channel)],
                    expect: Expect::NotFound,
                    lists: 2,
                    proofs: vec![],
                },
                Row {
                    name: "newest proof id 404, the next proves",
                    posted: vec![5000, 5001],
                    replies: vec![empty(), empty(), status("404 Not Found"), proof(channel)],
                    expect: Expect::NotFound,
                    lists: 2,
                    proofs: vec![5001, 5000],
                },
                Row {
                    name: "history reads stall",
                    posted: vec![5000],
                    replies: vec![Reply::Stall, Reply::Stall],
                    expect: Expect::Unresolved("history read failed"),
                    lists: 2,
                    proofs: vec![],
                },
                Row {
                    name: "this bot's message with other content",
                    posted: vec![],
                    replies: vec![altered(channel), altered(channel)],
                    expect: Expect::Unresolved("same-author non-exact message observed"),
                    lists: 2,
                    proofs: vec![],
                },
            ]
        };
        let count = rows(base).len() as u64;
        let runs = (0..count).map(|index| async move {
            let channel = base + index;
            let mut row = rows(channel).swap_remove(index as usize);
            let replies = std::mem::take(&mut row.replies);
            let settled = settle_lost_post(channel, &row.posted, replies).await;
            (row, channel, settled)
        });
        let settled = futures::future::join_all(runs).await;
        let wrong: Vec<String> = (settled.iter())
            .flat_map(|(row, channel, settled)| {
                let found = mismatches(row, *channel, settled);
                found
                    .into_iter()
                    .map(|wrong| format!("{}: {wrong}", row.name))
            })
            .collect();
        assert!(wrong.is_empty(), "{wrong:#?}");
    }
}

#[cfg(unix)]
#[path = "local_retry_io_tests.rs"]
mod local_retry_io_tests;

#[path = "o_writer_repost_io_tests/owner_tests.rs"]
mod owner_tests;
