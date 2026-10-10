use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use poise::serenity_prelude as serenity;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use super::evidence::{EvidenceScope, NotFoundEvidence, RunScope};
use super::matcher::{
    Attribution, AttributionSnapshot, ObservedMessage, RecoveryKind, match_observations,
};
use super::*;
use crate::services::discord::outbound::o_writer_io::GatewayPort;
use crate::services::tui_o::repost::config::{RepostConfig, RepostSwitch};
use crate::services::tui_o::repost::identity::{marker, payload_sha256};
use crate::services::tui_o::repost::io::RepostHttp;
use crate::services::tui_o::repost::o_piece_attempts::{AttemptKind, AttemptResult, AttemptRow};
use crate::services::tui_o::repost::o_piece_delivery::{
    AdmittedBy, DeliveryRow, PieceKey, ReceiptMethod,
};
use crate::services::tui_o::repost::send::{RepostEnvelope, RepostIds};
use crate::services::tui_o::shadow::tap::TuiOConfig;
use crate::services::tui_o::shadow::{ShadowProvider, UnitKey, UnitKind};
use crate::services::tui_o::writer::{DiscordPort, PostOutcome, SeenMessage, confirm};

pub(crate) const CHANNEL: u64 = 63250401;
pub(crate) const BOT: u64 = 42;
const OTHER: u64 = 77;
const ANCHOR: u64 = 100;
const PAYLOAD: &str = "조각 본문 ```code```";

pub(crate) fn key(native_key: &str) -> PieceKey {
    let unit = UnitKey {
        channel_id: CHANNEL,
        provider: ShadowProvider::Claude,
        native_key: native_key.into(),
        kind: UnitKind::Body,
    };
    PieceKey::new(unit, 0).unwrap()
}

fn row(revision: i64) -> DeliveryRow {
    DeliveryRow {
        key: key("f4"),
        payload: PAYLOAD.into(),
        payload_sha256: payload_sha256(PAYLOAD),
        identity_version: 1,
        split_version: 1,
        original_anchor: ANCHOR,
        sender_id: BOT,
        admitted_by: AdmittedBy::Uncertain,
        origin_serial: 3,
        origin_node: "node-a".into(),
        failure: None,
        conflict_sha256: None,
        revision,
    }
}

fn spent(slots: &[(u8, Option<AttemptResult>)]) -> Vec<AttemptRow> {
    let kind = |slot| match slot {
        0 => AttemptKind::Original,
        _ => AttemptKind::AutoReconfirm,
    };
    let rows = slots.iter().map(|(slot, result)| AttemptRow {
        slot: *slot,
        kind: kind(*slot),
        result: *result,
    });
    rows.collect()
}

pub(crate) fn run(name: &str) -> RunScope {
    RunScope {
        holder: "node-a".into(),
        run: name.into(),
        credentials: "creds".into(),
        evidence_generation: 1,
    }
}

fn scope(slots: &[u8]) -> EvidenceScope {
    let settled: Vec<_> = slots
        .iter()
        .map(|slot| (*slot, Some(AttemptResult::Uncertain)))
        .collect();
    EvidenceScope::of(&row(3), &spent(&settled), &run("run-1")).unwrap()
}

pub(crate) fn message(id: u64, author: u64, content: &str) -> ObservedMessage {
    ObservedMessage {
        id,
        channel_id: CHANNEL,
        author_id: author,
        content: content.into(),
        rich_embeds: 0,
        footers: Vec::new(),
        nonce: None,
    }
}

fn known() -> AttributionSnapshot {
    AttributionSnapshot::Known {
        receipts: BTreeMap::new(),
        same_payload: Vec::new(),
    }
}

/// The footer an additional send of `key` carries, as the real envelope builds it.
fn repost_footer(key: &PieceKey) -> String {
    let ids = RepostIds::for_piece(&marker(key)).unwrap();
    let body = RepostEnvelope::additional(CHANNEL, PAYLOAD.into(), ids).message();
    let body = serde_json::to_value(body).unwrap();
    body["embeds"][0]["footer"]["text"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// An in-memory channel. Reads take `delay` each on the tokio clock.
#[derive(Default)]
pub(crate) struct Fake {
    pub(crate) channel: StdMutex<BTreeMap<u64, ObservedMessage>>,
    /// Single reads answered differently from the channel, `None` for a 404.
    singles: BTreeMap<u64, Option<ObservedMessage>>,
    delay: Duration,
    /// Answers every page as if `before` were absent.
    stuck: bool,
    failing: std::sync::atomic::AtomicBool,
    reads: AtomicUsize,
    befores: StdMutex<Vec<Option<u64>>>,
}

impl Fake {
    pub(crate) fn with(messages: impl IntoIterator<Item = ObservedMessage>) -> Self {
        let channel = messages.into_iter().map(|m| (m.id, m)).collect();
        Self {
            channel: StdMutex::new(channel),
            ..Self::default()
        }
    }

    fn add(&self, message: ObservedMessage) {
        self.channel.lock().unwrap().insert(message.id, message);
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::SeqCst)
    }
}

impl ProbeRead for Fake {
    fn credentials(&self) -> String {
        "creds".into()
    }

    fn message(
        &self,
        _channel: u64,
        id: u64,
    ) -> impl Future<Output = Result<Option<ObservedMessage>, String>> + Send {
        self.reads.fetch_add(1, Ordering::SeqCst);
        let found = self.singles.get(&id).cloned();
        let found = found.unwrap_or_else(|| self.channel.lock().unwrap().get(&id).cloned());
        let delay = self.delay;
        async move {
            tokio::time::sleep(delay).await;
            Ok(found)
        }
    }

    fn history(
        &self,
        _channel: u64,
        before: Option<u64>,
        limit: u8,
    ) -> impl Future<Output = Result<Vec<ObservedMessage>, String>> + Send {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.befores.lock().unwrap().push(before);
        let before = if self.stuck { None } else { before };
        let page: Vec<_> = self
            .channel
            .lock()
            .unwrap()
            .values()
            .rev()
            .filter(|m| before.is_none_or(|before| m.id < before))
            .take(usize::from(limit))
            .cloned()
            .collect();
        let (delay, failing) = (self.delay, self.failing.load(Ordering::SeqCst));
        async move {
            tokio::time::sleep(delay).await;
            if failing {
                return Err("HTTP 500".into());
            }
            Ok(page)
        }
    }
}

/// `count` messages of another author above the anchor, ids from `first` up.
fn filler(first: u64, count: u64) -> Vec<ObservedMessage> {
    (first..first + count)
        .map(|id| message(id, OTHER, "chatter"))
        .collect()
}

/// Runs both passes of `scope` over a channel holding only the proof message, on a paused clock.
pub(crate) fn absent_evidence(scope: EvidenceScope, run: RunScope) -> NotFoundEvidence {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .start_paused(true)
            .build()
            .unwrap();
        runtime.block_on(async move {
            let proof = ObservedMessage {
                channel_id: scope.key.unit().channel_id,
                ..message(900, scope.sender_id, "earlier")
            };
            let reader = Fake::with([proof]);
            let mut session = ProbeSession::new(scope, vec![900], Instant::now());
            loop {
                match session.advance(&reader, &run, &known()).await {
                    Progress::Waiting { until } => tokio::time::sleep_until(until).await,
                    Progress::FirstPassDone { .. } => {}
                    Progress::Absent(evidence) => return *evidence,
                    other => panic!("an empty channel: {other:?}"),
                }
            }
        })
    })
    .join()
    .unwrap()
}

#[tokio::test(start_paused = true)]
async fn f4_two_passes_wait_after_completion() {
    let t0 = Instant::now();
    let mut reader = Fake::with([message(900, BOT, "earlier")]);
    // Proof read 3 s plus one history page 4 s: the first pass ends 7 s after it starts.
    reader.delay = Duration::from_millis(3500);
    let run = run("run-1");
    let mut session = ProbeSession::new(scope(&[0]), vec![900], t0);
    let first_due = t0 + Duration::from_secs(10);
    tokio::time::sleep_until(first_due - Duration::from_millis(1)).await;
    let early = session.advance(&reader, &run, &known()).await;
    assert_eq!(early, Progress::Waiting { until: first_due });
    assert_eq!(reader.reads(), 0, "no read before t=10");

    tokio::time::sleep_until(first_due).await;
    let first = session.advance(&reader, &run, &known()).await;
    let second_due = t0 + Duration::from_secs(37);
    assert_eq!(
        first,
        Progress::FirstPassDone {
            second_from: second_due
        }
    );
    let after_first = reader.reads();
    tokio::time::sleep_until(t0 + Duration::from_secs(30)).await;
    let fixed_timer = session.advance(&reader, &run, &known()).await;
    assert_eq!(fixed_timer, Progress::Waiting { until: second_due });
    assert_eq!(reader.reads(), after_first, "no second-pass read at t=30");

    tokio::time::sleep_until(second_due).await;
    let Progress::Absent(evidence) = session.advance(&reader, &run, &known()).await else {
        panic!("two clean passes at the boundaries");
    };
    let [first, second] = evidence.passes();
    assert_eq!(first.started_at(), first_due);
    assert_eq!(second.started_at(), second_due);
    assert_eq!(first.proof().message_id(), 900);
}

#[tokio::test(start_paused = true)]
async fn f4_partial_resumes_only_in_its_run() {
    let mut messages = filler(1000, 1100);
    messages.push(message(900, BOT, "earlier"));
    let reader = Fake::with(messages);
    let settled = Instant::now();
    tokio::time::advance(Duration::from_secs(60)).await;
    let run_1 = run("run-1");

    let mut same_run = ProbeSession::new(scope(&[0]), vec![900], settled);
    let partial = same_run.advance(&reader, &run_1, &known()).await;
    assert_eq!(partial, Progress::Partial);
    assert!(!partial.covered());
    let resumed = same_run.advance(&reader, &run_1, &known()).await;
    assert!(
        matches!(resumed, Progress::FirstPassDone { .. }),
        "{resumed:?}"
    );

    let mut handed_over = ProbeSession::new(scope(&[0]), vec![900], settled);
    assert_eq!(
        handed_over.advance(&reader, &run_1, &known()).await,
        Progress::Partial
    );
    let run_2 = run("run-2");
    for _ in 0..3 {
        let progress = handed_over.advance(&reader, &run_2, &known()).await;
        assert!(matches!(progress, Progress::Incomplete(_)), "{progress:?}");
    }
    // Back under the first run, the dropped pass starts over from the newest page.
    let reads = reader.befores.lock().unwrap().len();
    let restarted = handed_over.advance(&reader, &run_1, &known()).await;
    assert_eq!(restarted, Progress::Partial);
    assert_eq!(reader.befores.lock().unwrap()[reads], None);
}

#[tokio::test]
async fn f4_slot_two_requires_new_closed_attempt_evidence() {
    let run_1 = run("run-1");
    let slot_0 = spent(&[(0, Some(AttemptResult::Uncertain))]);
    let slot_1_open = spent(&[(0, Some(AttemptResult::Uncertain)), (1, None)]);
    let slot_1_settled = spent(&[
        (0, Some(AttemptResult::Uncertain)),
        (1, Some(AttemptResult::Uncertain)),
    ]);
    let before = EvidenceScope::of(&row(3), &slot_0, &run_1).unwrap();
    let evidence = absent_evidence(before, run_1.clone());
    assert!(evidence.validate_current(&row(3), &slot_0, &run_1));
    assert!(!evidence.validate_current(&row(3), &slot_0, &run("run-2")));

    // Slot 1 was granted (revision 4) and settled (revision 5).
    assert_eq!(EvidenceScope::of(&row(4), &slot_1_open, &run_1), Err(1));
    assert!(!evidence.validate_current(&row(4), &slot_1_open, &run_1));
    assert!(!evidence.validate_current(&row(5), &slot_1_settled, &run_1));
    let after = EvidenceScope::of(&row(5), &slot_1_settled, &run_1).unwrap();
    assert_eq!(after.checked_slots, [0, 1]);
    let fresh = absent_evidence(after, run_1.clone());
    assert!(fresh.validate_current(&row(5), &slot_1_settled, &run_1));
}

/// A mock Discord serving one channel's messages to GETs; records method and path.
struct Discord {
    base: String,
    seen: Arc<StdMutex<Vec<(String, String)>>>,
}

impl Discord {
    async fn start(messages: Vec<Value>, stall: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<StdMutex<Vec<(String, String)>>> = Arc::default();
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let Some((method, path)) = read_head(&mut socket).await else {
                    continue;
                };
                log.lock().unwrap().push((method, path.clone()));
                if stall {
                    tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        drop(socket);
                    });
                    continue;
                }
                let (status, body) = answer(&messages, &path);
                let raw = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(raw.as_bytes()).await;
            }
        });
        Self { base, seen }
    }

    fn port(&self) -> RepostHttp {
        let http = Arc::new(serenity::Http::new("test-token"));
        RepostHttp::at(http, self.base.clone()).unwrap()
    }

    /// Asserts every request was a GET and returns how many there were.
    fn gets(&self) -> usize {
        let seen = self.seen.lock().unwrap();
        let posts = seen.iter().filter(|(method, _)| method != "GET").count();
        assert_eq!(posts, 0, "a probe never POSTs: {seen:?}");
        seen.len()
    }
}

async fn read_head(socket: &mut TcpStream) -> Option<(String, String)> {
    let (mut buf, mut chunk) = (Vec::new(), [0u8; 4096]);
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = socket.read(&mut chunk).await.ok().filter(|n| *n > 0)?;
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&buf).into_owned();
    let mut words = head.split_whitespace();
    Some((words.next()?.to_owned(), words.next()?.to_owned()))
}

fn answer(messages: &[Value], path: &str) -> (&'static str, String) {
    let id = |m: &Value| m["id"].as_str().unwrap().parse::<u64>().unwrap();
    let (route, query) = path.split_once('?').unwrap_or((path, ""));
    if let Some(single) = route.rsplit_once("/messages/").map(|(_, id)| id) {
        let found = messages.iter().find(|m| m["id"] == json!(single));
        return found.map_or(("404 Not Found", "{}".into()), |m| {
            ("200 OK", m.to_string())
        });
    }
    let param = |name: &str| {
        let prefix = format!("{name}=");
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix(prefix.as_str())?.parse::<u64>().ok())
    };
    let mut page: Vec<&Value> = messages
        .iter()
        .filter(|m| param("before").is_none_or(|before| id(m) < before))
        .collect();
    page.sort_by_key(|m| std::cmp::Reverse(id(m)));
    page.truncate(param("limit").unwrap_or(50) as usize);
    ("200 OK", json!(page).to_string())
}

fn wire(id: u64, author: u64, content: &str, footer: Option<&str>, nonce: Option<&str>) -> Value {
    let embeds = footer.map_or_else(|| json!([]), |text| json!([{"footer": {"text": text}}]));
    json!({
        "id": id.to_string(), "channel_id": CHANNEL.to_string(), "author": {"id": author.to_string()},
        "content": content, "embeds": embeds, "nonce": nonce,
    })
}

/// One first pass through the real adapter, the previous send settled long ago.
async fn first_pass(discord: &Discord, mut scope: EvidenceScope) -> (Progress, Attribution) {
    let port = discord.port();
    scope.run.credentials = port.credentials();
    let run = scope.run.clone();
    let settled = Instant::now().checked_sub(Duration::from_secs(60)).unwrap();
    let mut session = ProbeSession::new(scope, vec![900], settled);
    let progress = session.advance(&port, &run, &known()).await;
    (progress, session.attribution().clone())
}

#[tokio::test]
async fn f4_history_keeps_stable_ids_for_both_additional_slots() {
    let piece = key("f4");
    let ids = RepostIds::for_piece(&marker(&piece)).unwrap();
    let slot_1 = RepostEnvelope::additional(CHANNEL, PAYLOAD.into(), ids.clone()).message();
    let rebuilt = RepostIds::for_piece(&marker(&piece)).unwrap();
    let slot_2 = RepostEnvelope::additional(CHANNEL, PAYLOAD.into(), rebuilt).message();
    let (slot_1, slot_2) = (
        serde_json::to_value(slot_1).unwrap(),
        serde_json::to_value(slot_2).unwrap(),
    );
    assert_eq!(slot_1, slot_2, "both slots send the same identifiers");
    let footer = slot_1["embeds"][0]["footer"]["text"].as_str().unwrap();
    let nonce = slot_1["nonce"].as_str().unwrap();

    let discord = Discord::start(
        vec![
            wire(900, BOT, "earlier", None, None),
            wire(301, BOT, PAYLOAD, Some(footer), Some(nonce)),
            wire(302, BOT, "변형된 본문", Some(footer), None),
        ],
        false,
    )
    .await;
    let (progress, seen) = first_pass(&discord, scope(&[0, 1])).await;
    assert_eq!(progress, Progress::Present);
    let found: Vec<_> = seen.found.keys().copied().collect();
    assert_eq!(found, [301, 302], "the marker names the piece");
    for receipt in seen.found.values() {
        assert_eq!(receipt.receipt.method, ReceiptMethod::Marker);
        assert_eq!(receipt.receipt.slot, None);
        assert_eq!(
            receipt.observed_marker.as_deref(),
            Some(marker(&piece).as_str())
        );
    }
    assert_eq!(seen.found[&301].observed_nonce.as_deref(), Some(nonce));
    assert!(discord.gets() >= 2);
}

#[tokio::test]
async fn f4_damaged_identifier_is_not_exact_match_fallback() {
    let footer = repost_footer(&key("f4"));
    let truncated = &footer[..footer.len() - 3];
    let missing = footer.rsplit_once(' ').unwrap().0;
    let other = repost_footer(&key("f4-other"));
    let discord = Discord::start(
        vec![
            wire(900, BOT, "earlier", None, None),
            wire(401, BOT, PAYLOAD, Some(missing), None),
            wire(402, BOT, PAYLOAD, Some(truncated), None),
            wire(403, BOT, PAYLOAD, Some(&other), None),
            wire(404, BOT, PAYLOAD, Some(&footer), None),
        ],
        false,
    )
    .await;
    let (progress, seen) = first_pass(&discord, scope(&[0, 1])).await;
    assert_eq!(progress, Progress::Present);
    assert_eq!(seen.found.keys().copied().collect::<Vec<_>>(), [404]);
    assert_eq!(
        seen.damaged.iter().copied().collect::<Vec<_>>(),
        [401, 402, 403]
    );
    assert!(seen.unattributed.is_empty());
    discord.gets();
}

#[tokio::test(start_paused = true)]
async fn f4_scan_uses_original_anchor_and_frozen_upper() {
    let mut messages = filler(1000, 1100);
    messages.push(message(900, BOT, "the latest proof"));
    messages.push(message(150, BOT, PAYLOAD));
    // The anchor itself is outside the range even when it carries the payload.
    messages.push(message(ANCHOR, BOT, PAYLOAD));
    let mut reader = Fake::with(messages);
    reader.singles.insert(ANCHOR, None);
    let run_1 = run("run-1");
    let settled = Instant::now();
    tokio::time::advance(Duration::from_secs(60)).await;
    let mut session = ProbeSession::new(scope(&[0]), vec![ANCHOR, 900], settled);
    assert_eq!(
        session.advance(&reader, &run_1, &known()).await,
        Progress::Partial
    );
    // A message newer than the pass's first page, even the exact payload, waits for the next pass.
    reader.add(message(5000, BOT, PAYLOAD));
    let progress = session.advance(&reader, &run_1, &known()).await;
    assert_eq!(progress, Progress::Present);
    let found: Vec<_> = session.attribution().found.keys().copied().collect();
    assert_eq!(found, [150]);
    let befores = reader.befores.lock().unwrap();
    assert_eq!(befores[0], None);
    assert!(
        befores[1..]
            .iter()
            .all(|before| before.is_some_and(|b| b <= 2099))
    );
}

#[tokio::test(start_paused = true)]
async fn f4_permission_needs_expected_bot_get() {
    let wrong_channel = ObservedMessage {
        channel_id: CHANNEL + 1,
        ..message(900, BOT, "earlier")
    };
    let cases: Vec<(&str, Vec<u64>, Option<ObservedMessage>)> = vec![
        ("no known message", vec![], None),
        (
            "wrong author",
            vec![900],
            Some(message(900, OTHER, "earlier")),
        ),
        ("wrong channel", vec![900], Some(wrong_channel)),
        ("another id", vec![900], Some(message(901, BOT, "earlier"))),
        ("deleted anchor, no alternative", vec![ANCHOR], None),
    ];
    let run_1 = run("run-1");
    for (name, proof_ids, answer) in cases {
        // Empty 200 pages: nothing but the permission proof decides.
        let mut reader = Fake::default();
        reader.singles.insert(900, answer);
        reader.singles.insert(ANCHOR, None);
        let mut session = ProbeSession::new(scope(&[0]), proof_ids, Instant::now());
        for _ in 0..4 {
            tokio::time::advance(Duration::from_secs(30)).await;
            let progress = session.advance(&reader, &run_1, &known()).await;
            assert!(
                matches!(progress, Progress::Incomplete(_)),
                "{name}: {progress:?}"
            );
        }
    }
    let reader = Fake::with([message(900, BOT, "earlier")]);
    let mut session = ProbeSession::new(scope(&[0]), vec![ANCHOR, 900], Instant::now());
    let mut outcomes = Vec::new();
    for _ in 0..3 {
        tokio::time::advance(Duration::from_secs(30)).await;
        outcomes.push(session.advance(&reader, &run_1, &known()).await);
    }
    assert!(matches!(outcomes[0], Progress::FirstPassDone { .. }));
    assert!(matches!(outcomes[1], Progress::Absent(_)), "{outcomes:?}");
}

#[tokio::test]
async fn f4_timeout_budget_and_nonprogress_remain_incomplete() {
    let stalled = Discord::start(vec![], true).await;
    let started = std::time::Instant::now();
    let (progress, _) = first_pass(&stalled, scope(&[0])).await;
    assert!(matches!(progress, Progress::Incomplete(_)), "{progress:?}");
    let waited = started.elapsed();
    assert!(
        waited >= READ_TIMEOUT && waited < READ_TIMEOUT * 2,
        "{waited:?}"
    );
    assert_eq!(stalled.gets(), 1);

    let run_1 = run("run-1");
    let settled = Instant::now().checked_sub(Duration::from_secs(60)).unwrap();
    let mut proof_and_more = filler(1000, 1100);
    proof_and_more.push(message(900, BOT, "earlier"));
    let budget = Fake::with(proof_and_more.clone());
    let mut session = ProbeSession::new(scope(&[0]), vec![900], settled);
    assert_eq!(
        session.advance(&budget, &run_1, &known()).await,
        Progress::Partial
    );
    assert_eq!(budget.befores.lock().unwrap().len(), PAGES_PER_STEP);

    let mut repeating = Fake::with(proof_and_more);
    repeating.stuck = true;
    let mut session = ProbeSession::new(scope(&[0]), vec![900], settled);
    let progress = session.advance(&repeating, &run_1, &known()).await;
    assert!(matches!(progress, Progress::Incomplete(_)), "{progress:?}");

    // The anchor shows up on the tenth page: that pass is complete.
    let mut exactly_ten = filler(1000, 998);
    exactly_ten.extend([message(900, BOT, "earlier"), message(ANCHOR, BOT, "anchor")]);
    let bounded = Fake::with(exactly_ten);
    let mut session = ProbeSession::new(scope(&[0]), vec![900], settled);
    let progress = session.advance(&bounded, &run_1, &known()).await;
    assert!(
        matches!(progress, Progress::FirstPassDone { .. }),
        "{progress:?}"
    );
    assert_eq!(bounded.befores.lock().unwrap().len(), PAGES_PER_STEP);
}

#[tokio::test]
async fn f4_known_original_nonce_returns_original_recovered() {
    let ids = RepostIds::for_piece(&marker(&key("f4"))).unwrap();
    let discord = Discord::start(
        vec![
            wire(900, BOT, "earlier", None, None),
            wire(201, BOT, "Discord가 바꾼 본문", None, Some(ids.nonce())),
            wire(202, BOT, PAYLOAD, None, None),
            wire(203, OTHER, PAYLOAD, None, Some(ids.nonce())),
        ],
        false,
    )
    .await;
    let (_, only_original) = first_pass(&discord, scope(&[0])).await;
    let found: Vec<_> = only_original.found.keys().copied().collect();
    assert_eq!(found, [201, 202]);
    let recovered = &only_original.found[&201];
    assert_eq!(recovered.recovery, RecoveryKind::OriginalRecovered);
    assert_eq!(
        (recovered.receipt.slot, recovered.receipt.method),
        (Some(0), ReceiptMethod::Nonce)
    );
    assert_eq!(recovered.observed_nonce.as_deref(), Some(ids.nonce()));
    // An exact match without a nonce is the piece, but not provably its original.
    let exact = &only_original.found[&202];
    assert_eq!(exact.recovery, RecoveryKind::OriginUnknown);
    assert_eq!(exact.receipt.slot, None);

    let (_, after_a_repost) = first_pass(&discord, scope(&[0, 1])).await;
    let shared = &after_a_repost.found[&201];
    assert_eq!(shared.recovery, RecoveryKind::OriginUnknown);
    assert_eq!(
        shared.receipt.slot, None,
        "the nonce is shared by every send"
    );
    discord.gets();
}

#[test]
fn f4_same_payload_claimants_are_not_arbitrarily_assigned() {
    let own = scope(&[0]);
    let rival = key("f4-rival");
    let exact = [message(301, BOT, PAYLOAD)];
    let attribute = |snapshot: AttributionSnapshot, seen: &[ObservedMessage]| {
        let mut into = Attribution::default();
        match_observations(&own, &snapshot, seen, &mut into);
        into
    };
    let contested = AttributionSnapshot::Known {
        receipts: BTreeMap::new(),
        same_payload: vec![rival.clone()],
    };
    let seen = attribute(contested.clone(), &exact);
    assert!(seen.found.is_empty());
    assert_eq!(seen.unattributed.iter().copied().collect::<Vec<_>>(), [301]);
    let seen = attribute(AttributionSnapshot::Unknown("PG down".into()), &exact);
    assert!(seen.found.is_empty());
    assert_eq!(seen.unattributed.len(), 1);

    let theirs = AttributionSnapshot::Known {
        receipts: BTreeMap::from([(301, rival.clone())]),
        same_payload: Vec::new(),
    };
    assert!(attribute(theirs, &exact).is_clear());
    let rival_marked = ObservedMessage {
        footers: vec![repost_footer(&rival)],
        ..message(302, BOT, PAYLOAD)
    };
    assert!(attribute(contested, &[rival_marked]).is_clear());

    let seen = attribute(known(), &exact);
    assert_eq!(seen.found[&301].receipt.method, ReceiptMethod::ExactMatch);
    assert_eq!(seen.found[&301].receipt.key, own.key);
}

#[tokio::test(start_paused = true)]
async fn f4_late_original_adds_a_second_distinct_id() {
    let own = key("f4");
    let reposted = ObservedMessage {
        footers: vec![repost_footer(&own)],
        ..message(500, BOT, PAYLOAD)
    };
    let reader = Fake::with([
        message(900, BOT, "earlier"),
        reposted,
        message(450, BOT, PAYLOAD),
    ]);
    let recorded = AttributionSnapshot::Known {
        receipts: BTreeMap::from([(500, own)]),
        same_payload: Vec::new(),
    };
    let run_1 = run("run-1");
    let mut session = ProbeSession::new(scope(&[0, 1]), vec![900], Instant::now());
    tokio::time::advance(Duration::from_secs(10)).await;
    assert_eq!(
        session.advance(&reader, &run_1, &recorded).await,
        Progress::Present
    );
    let seen = session.attribution();
    assert_eq!(seen.recorded.iter().copied().collect::<Vec<_>>(), [500]);
    assert_eq!(seen.found.keys().copied().collect::<Vec<_>>(), [450]);
    assert_eq!(seen.distinct_ids(), 2);
    // Seeing the same two again adds nothing.
    tokio::time::advance(Duration::from_secs(10)).await;
    session.advance(&reader, &run_1, &recorded).await;
    assert_eq!(session.attribution().distinct_ids(), 2);
}

#[tokio::test(start_paused = true)]
async fn f4_partial_audit_preserves_known_ids_and_unknown_coverage() {
    let mut messages = filler(1000, 1100);
    messages.push(message(900, BOT, "earlier"));
    messages.push(ObservedMessage {
        footers: vec![repost_footer(&key("f4"))],
        ..message(2050, BOT, PAYLOAD)
    });
    let reader = Fake::with(messages);
    let run_1 = run("run-1");
    let mut session = ProbeSession::new(scope(&[0, 1]), vec![900], Instant::now());
    tokio::time::advance(Duration::from_secs(10)).await;
    let partial = session.advance(&reader, &run_1, &known()).await;
    assert_eq!(partial, Progress::Partial);
    assert!(!partial.covered(), "a partial audit is no zero");
    assert_eq!(session.attribution().distinct_ids(), 1);

    reader.failing.store(true, Ordering::SeqCst);
    let failed = session.advance(&reader, &run_1, &known()).await;
    assert!(matches!(failed, Progress::Incomplete(_)) && !failed.covered());
    assert_eq!(
        session
            .attribution()
            .found
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        [2050]
    );
}

fn switch(enabled: bool) -> RepostSwitch {
    let boot = TuiOConfig {
        repost: RepostConfig { enabled },
        ..TuiOConfig::default()
    };
    RepostSwitch::new(Some(&boot), watch::channel(None).1)
}

#[tokio::test(start_paused = true)]
async fn f4_default_off_does_not_construct_a_probe() {
    assert!(!RepostConfig::default().enabled);
    let reader = Fake::with([message(900, BOT, "earlier")]);
    let built = AtomicUsize::new(0);
    let build = |_| {
        built.fetch_add(1, Ordering::SeqCst);
        ProbeSession::new(scope(&[0]), vec![900], Instant::now())
    };
    let default_off = RepostSwitch::new(None, watch::channel(None).1);
    for mut off in [default_off, switch(false)] {
        assert!(off.when_enabled(build).is_none());
    }
    assert_eq!((built.load(Ordering::SeqCst), reader.reads()), (0, 0));

    let mut session = switch(true).when_enabled(build).expect("on builds one");
    tokio::time::advance(Duration::from_secs(10)).await;
    session.advance(&reader, &run("run-1"), &known()).await;
    assert_eq!(built.load(Ordering::SeqCst), 1);
    assert!(reader.reads() > 0, "the on control really reads");
}

/// Answers like the legacy writer's gateway: empty history, permission as the adapter says.
struct Legacy(GatewayPort);

impl DiscordPort for Legacy {
    fn bot_id(&self) -> u64 {
        BOT
    }

    fn post(&self, _: u64, _: String) -> impl Future<Output = PostOutcome> + Send + 'static {
        std::future::pending()
    }

    async fn history_after(&self, _: u64, _: u64) -> Result<Vec<SeenMessage>, String> {
        Ok(Vec::new())
    }

    fn history_readable(&self, channel: u64) -> bool {
        self.0.history_readable(channel)
    }
}

/// The probe's own definition files; everything else is checked line by line.
const PROBE_FILES: [&str; 3] = [
    "src/services/tui_o/repost/probe.rs",
    "src/services/tui_o/repost/evidence.rs",
    "src/services/tui_o/repost/matcher.rs",
];
const ADAPTER: &str = "src/services/discord/outbound/o_writer_repost_io.rs";
/// The adapter may only declare the probe and implement its reads.
const ADAPTER_LINES: [&str; 4] = [
    "use probe::ProbeRead;",
    "use probe::matcher::ObservedMessage;",
    "pub(crate) mod probe;",
    "impl ProbeRead for RepostHttp {",
];

/// Whether a line names a probe entry point or reaches into the re-post `io` module.
fn names_the_probe(path: &str, line: &str) -> bool {
    let entries = [
        "ProbeSession",
        "ProbeRead",
        "match_observations",
        "NotFoundEvidence",
        "EvidenceScope",
        "AttributionSnapshot",
        "repost::io",
        "io::probe",
    ];
    let words = line.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':'));
    entries.iter().any(|entry| line.contains(entry))
        || words.clone().any(|word| word.starts_with("probe::"))
        || (path.contains("tui_o/repost/")
            && words.into_iter().any(|word| word.starts_with("super::io")))
}

/// `path:line` of every production line under `root/src` that names the probe outside what is
/// allowed. Unrecognised forms are reported, never skipped.
fn probe_edges(root: &std::path::Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut dirs = vec![root.join("src")];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            if !name.ends_with(".rs")
                || name.ends_with("_tests.rs")
                || PROBE_FILES.contains(&relative.as_str())
            {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            for (number, line) in text.lines().enumerate() {
                let allowed = relative == ADAPTER && ADAPTER_LINES.contains(&line.trim());
                if names_the_probe(&relative, line) && !allowed {
                    found.push(format!("{relative}:{}", number + 1));
                }
            }
        }
    }
    found.sort();
    found
}

#[test]
fn f4r2_the_dormancy_scan_reports_a_wrapper_in_the_adapter_and_its_caller() {
    let write = |root: &std::path::Path, relative: &str, text: &str| {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    };
    let declared = "#[path = \"../../tui_o/repost/probe.rs\"]\npub(crate) mod probe;\nuse probe::ProbeRead;\nimpl ProbeRead for RepostHttp {\n}\n";
    let clean = tempfile::tempdir().unwrap();
    write(clean.path(), ADAPTER, declared);
    write(
        clean.path(),
        PROBE_FILES[0],
        "pub(crate) struct ProbeSession;\n",
    );
    write(
        clean.path(),
        "src/services/tui_o/repost/mod.rs",
        "pub(crate) mod io;\n",
    );
    assert_eq!(probe_edges(clean.path()), Vec::<String>::new());

    let wired = tempfile::tempdir().unwrap();
    let wrapper = format!(
        "{declared}pub(crate) fn start_reader() {{\n    let _ = probe::ProbeSession::new;\n}}\n"
    );
    write(wired.path(), ADAPTER, &wrapper);
    let caller = "fn tick() {\n    crate::services::tui_o::repost::io::start_reader();\n}\n";
    write(wired.path(), "src/services/tui_o/writer/host.rs", caller);
    let sibling = "use super::io;\nfn go() { io::start_reader(); }\n";
    write(wired.path(), "src/services/tui_o/repost/runner.rs", sibling);
    assert_eq!(
        probe_edges(wired.path()),
        [
            "src/services/discord/outbound/o_writer_repost_io.rs:7",
            "src/services/tui_o/repost/runner.rs:1",
            "src/services/tui_o/writer/host.rs:2",
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn f4_has_no_operational_probe_edges_and_preserves_legacy_confirmation() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    assert_eq!(
        probe_edges(root),
        Vec::<String>::new(),
        "no production caller yet"
    );

    let gateway = GatewayPort::new(Arc::new(serenity::Http::new("test-token")), BOT);
    let legacy = Legacy(gateway);
    assert!(!legacy.history_readable(CHANNEL));
    let verdict = confirm::settle(&legacy, CHANNEL, ANCHOR, PAYLOAD, false).await;
    assert!(
        matches!(verdict, confirm::Verdict::Unresolved(_)),
        "{verdict:?}"
    );
}

fn receipts(of: &[(u64, PieceKey)]) -> AttributionSnapshot {
    AttributionSnapshot::Known {
        receipts: of.iter().cloned().collect(),
        same_payload: Vec::new(),
    }
}

/// Advances `session` with `snapshot` every 30 s, `times` times; returns every result.
async fn drive(
    session: &mut ProbeSession,
    reader: &Fake,
    snapshot: &AttributionSnapshot,
    times: usize,
) -> Vec<Progress> {
    let mut outcomes = Vec::new();
    for _ in 0..times {
        tokio::time::advance(Duration::from_secs(30)).await;
        outcomes.push(session.advance(reader, &run("run-1"), snapshot).await);
    }
    outcomes
}

#[tokio::test(start_paused = true)]
async fn f4r_an_unknown_receipt_index_never_yields_absence() {
    let own = key("f4");
    let unknown = AttributionSnapshot::Unknown("receipt lookup failed".into());
    // 450 is this piece only through its recorded receipt; Discord changed its content.
    let channels = [
        vec![message(900, BOT, "earlier")],
        vec![
            message(900, BOT, "earlier"),
            message(450, BOT, "변형된 본문"),
        ],
    ];
    for channel in channels {
        let reader = Fake::with(channel);
        let mut session = ProbeSession::new(scope(&[0]), vec![900], Instant::now());
        for progress in drive(&mut session, &reader, &unknown, 4).await {
            assert!(matches!(progress, Progress::Incomplete(_)), "{progress:?}");
        }
    }

    // A pass done while the index was known does not pair with one after an unknown read.
    let reader = Fake::with([message(900, BOT, "earlier")]);
    let mut session = ProbeSession::new(scope(&[0]), vec![900], Instant::now());
    let first = drive(&mut session, &reader, &known(), 1).await;
    assert!(matches!(first[0], Progress::FirstPassDone { .. }));
    drive(&mut session, &reader, &unknown, 1).await;
    let again = drive(&mut session, &reader, &known(), 2).await;
    assert!(
        matches!(again[0], Progress::FirstPassDone { .. }),
        "{again:?}"
    );
    assert!(matches!(again[1], Progress::Absent(_)), "the known control");

    let reader = Fake::with([
        message(900, BOT, "earlier"),
        message(450, BOT, "변형된 본문"),
    ]);
    let mut session = ProbeSession::new(scope(&[0]), vec![900], Instant::now());
    let recorded = receipts(&[(450, own)]);
    assert_eq!(
        drive(&mut session, &reader, &recorded, 1).await,
        [Progress::Present]
    );
}

fn with_embeds(mut message: Value, embeds: Value) -> Value {
    message["embeds"] = embeds;
    message
}

#[tokio::test]
async fn f4r_a_damaged_identifier_is_kept_apart_from_success_and_absence() {
    let footer = repost_footer(&key("f4"));
    let truncated = &footer[..footer.len() - 3];
    let ids = RepostIds::for_piece(&marker(&key("f4"))).unwrap();
    let no_footer = with_embeds(
        wire(601, BOT, PAYLOAD, None, None),
        json!([{"type": "rich"}]),
    );
    let nonce_damaged = wire(602, BOT, "변형된 본문", Some(truncated), Some(ids.nonce()));
    let link_preview = with_embeds(
        wire(
            604,
            BOT,
            "변형된 본문 https://example.com",
            None,
            Some(ids.nonce()),
        ),
        json!([{"type": "article", "footer": {"text": "example"}}]),
    );
    let discord = Discord::start(
        vec![
            wire(900, BOT, "earlier", None, None),
            no_footer.clone(),
            nonce_damaged.clone(),
            wire(603, BOT, PAYLOAD, Some(&footer), None),
            link_preview,
        ],
        false,
    )
    .await;
    let (progress, seen) = first_pass(&discord, scope(&[0])).await;
    assert_eq!(progress, Progress::Present);
    assert_eq!(seen.damaged.iter().copied().collect::<Vec<_>>(), [601, 602]);
    assert_eq!(seen.found.keys().copied().collect::<Vec<_>>(), [603, 604]);
    // A link preview is not a send's embed: the nonce still names the original.
    assert_eq!(seen.found[&604].recovery, RecoveryKind::OriginalRecovered);
    discord.gets();

    for alone in [no_footer, nonce_damaged] {
        let discord =
            Discord::start(vec![wire(900, BOT, "earlier", None, None), alone], false).await;
        let (progress, seen) = first_pass(&discord, scope(&[0, 1])).await;
        assert_eq!(
            progress,
            Progress::Present,
            "a damaged observation is no absence"
        );
        assert!(seen.found.is_empty(), "nor a success");
    }
}

#[tokio::test(start_paused = true)]
async fn f4r_the_latest_snapshot_rejudges_everything_the_session_read() {
    let rival = key("f4-rival");
    let mut messages = filler(1000, 1100);
    messages.push(message(900, BOT, "earlier"));
    messages.push(message(2050, BOT, PAYLOAD));
    let reader = Fake::with(messages);
    let mut session = ProbeSession::new(scope(&[0]), vec![900], Instant::now());
    let contested = AttributionSnapshot::Known {
        receipts: BTreeMap::new(),
        same_payload: vec![rival.clone()],
    };
    assert_eq!(
        drive(&mut session, &reader, &contested, 1).await,
        [Progress::Partial]
    );
    assert_eq!(session.attribution().unattributed.len(), 1);
    // B recorded 2050 before the resume, whose pages are all below it.
    let resumed = drive(&mut session, &reader, &receipts(&[(2050, rival)]), 1).await;
    assert!(
        matches!(resumed[0], Progress::FirstPassDone { .. }),
        "{resumed:?}"
    );
    assert!(session.attribution().is_clear());
    assert_eq!(session.attribution().distinct_ids(), 0);

    // An exact match seen again with this piece's nonce is promoted, and still counts once.
    let reader = Fake::with([message(900, BOT, "earlier"), message(450, BOT, PAYLOAD)]);
    let mut session = ProbeSession::new(scope(&[0]), vec![900], Instant::now());
    drive(&mut session, &reader, &known(), 1).await;
    assert_eq!(
        session.attribution().found[&450].receipt.method,
        ReceiptMethod::ExactMatch
    );
    let nonce = RepostIds::for_piece(&marker(&key("f4")))
        .unwrap()
        .nonce()
        .to_owned();
    reader.add(ObservedMessage {
        nonce: Some(nonce),
        ..message(450, BOT, PAYLOAD)
    });
    drive(&mut session, &reader, &known(), 1).await;
    let promoted = &session.attribution().found[&450];
    assert_eq!(promoted.recovery, RecoveryKind::OriginalRecovered);
    assert_eq!(session.attribution().distinct_ids(), 1);
}

#[tokio::test(start_paused = true)]
async fn f4r_a_proof_read_of_the_piece_is_an_observation() {
    let nonce = RepostIds::for_piece(&marker(&key("f4")))
        .unwrap()
        .nonce()
        .to_owned();
    let piece = ObservedMessage {
        nonce: Some(nonce),
        ..message(450, BOT, "Discord가 바꾼 본문")
    };
    // History never lists 450, though the read by id returned it.
    let mut reader = Fake::with([message(900, BOT, "earlier")]);
    reader.singles.insert(450, Some(piece.clone()));
    let mut session = ProbeSession::new(scope(&[0]), vec![450], Instant::now());
    for progress in drive(&mut session, &reader, &known(), 4).await {
        assert!(matches!(progress, Progress::Incomplete(_)), "{progress:?}");
    }
    let found = &session.attribution().found[&450];
    assert_eq!(found.recovery, RecoveryKind::OriginalRecovered);

    // The anchor itself stays outside the range, whatever it carries.
    let mut reader = Fake::with([message(900, BOT, "earlier")]);
    reader.singles.insert(
        ANCHOR,
        Some(ObservedMessage {
            id: ANCHOR,
            ..piece
        }),
    );
    let mut session = ProbeSession::new(scope(&[0]), vec![ANCHOR], Instant::now());
    let outcomes = drive(&mut session, &reader, &known(), 2).await;
    assert!(matches!(outcomes[1], Progress::Absent(_)), "{outcomes:?}");
    assert!(session.attribution().is_clear());
}

#[test]
fn f4r_the_marker_names_the_channel_and_is_the_same_on_every_node_and_slot() {
    use crate::services::tui_o::repost::identity::piece_of;
    use crate::services::tui_o::store::ledger::PieceRecord;
    let unit = |channel_id| UnitKey {
        channel_id,
        provider: ShadowProvider::Claude,
        native_key: "msg_01".into(),
        kind: UnitKind::Body,
    };
    let here = PieceKey::new(unit(CHANNEL), 2).unwrap();
    let there = PieceKey::new(unit(CHANNEL + 1), 2).unwrap();
    assert_ne!(marker(&here), marker(&there));
    assert!(marker(&here).contains(&CHANNEL.to_string()));
    let nonce = |key: &PieceKey| {
        RepostIds::for_piece(&marker(key))
            .unwrap()
            .nonce()
            .to_owned()
    };
    assert_ne!(nonce(&here), nonce(&there));

    // Two nodes derive the piece from their own ledger records: another epoch, anchor, time.
    let record = |epoch, anchor_id| PieceRecord {
        unit_key: unit(CHANNEL),
        piece_index: 2,
        payload: PAYLOAD.into(),
        anchor_id,
        epoch,
        prepared_at: chrono::Utc::now(),
        outcome: None,
    };
    let node_a = piece_of(&record(1, 40)).unwrap();
    let node_b = piece_of(&record(7, 990)).unwrap();
    assert_eq!(marker(&node_a), marker(&node_b));
    assert_eq!(marker(&node_a), marker(&here));
    let slot = |key: &PieceKey| {
        let ids = RepostIds::for_piece(&marker(key)).unwrap();
        serde_json::to_value(RepostEnvelope::additional(CHANNEL, PAYLOAD.into(), ids).message())
            .unwrap()
    };
    assert_eq!(
        slot(&node_a),
        slot(&node_b),
        "slot 1 and slot 2 on any node"
    );
}

#[tokio::test(start_paused = true)]
async fn f4r2_a_known_damage_outlives_a_later_read_without_the_embed() {
    // A: an exact payload under a sent embed is damaged; the embed vanishing later changes nothing.
    let damaged = ObservedMessage {
        rich_embeds: 1,
        ..message(450, BOT, PAYLOAD)
    };
    let reader = Fake::with([message(900, BOT, "earlier"), damaged]);
    let mut session = ProbeSession::new(scope(&[0, 1]), vec![900], Instant::now());
    assert_eq!(
        drive(&mut session, &reader, &known(), 1).await,
        [Progress::Present]
    );
    reader.add(message(450, BOT, PAYLOAD));
    assert_eq!(
        drive(&mut session, &reader, &known(), 1).await,
        [Progress::Present]
    );
    let seen = session.attribution();
    assert!(seen.found.is_empty(), "a known damage is never a success");
    assert_eq!(seen.damaged.iter().copied().collect::<Vec<_>>(), [450]);
}

#[tokio::test(start_paused = true)]
async fn f4r2_an_intact_marker_read_by_id_outlives_a_truncated_listing() {
    // B: the proof read shows the intact marker; history then lists the same id truncated.
    let footer = repost_footer(&key("f4"));
    let read_by_id = ObservedMessage {
        rich_embeds: 1,
        footers: vec![footer.clone()],
        ..message(450, BOT, "변형된 본문")
    };
    let listed = ObservedMessage {
        footers: vec![footer[..footer.len() - 3].to_owned()],
        ..read_by_id.clone()
    };
    let mut reader = Fake::with([message(900, BOT, "earlier"), listed]);
    reader.singles.insert(450, Some(read_by_id));
    let mut session = ProbeSession::new(scope(&[0, 1]), vec![450], Instant::now());
    for progress in drive(&mut session, &reader, &known(), 4).await {
        assert_eq!(progress, Progress::Present);
    }
    let found = &session.attribution().found[&450];
    assert_eq!(found.receipt.method, ReceiptMethod::Marker);
}

/// What F3/F4 derived for `key("f4")` before the marker named its channel, fixed as data.
const OLD_MARKER: &str = "o:claude:body:f4#0";
const OLD_NONCE: &str = "r983159e99d7b1b1c01cd382d";

#[tokio::test(start_paused = true)]
async fn f4r2_an_older_or_unreadable_identifier_is_never_absence() {
    let old_footer = format!("재확인 후 추가 전달 {OLD_MARKER}");
    let old_marked = ObservedMessage {
        rich_embeds: 1,
        footers: vec![old_footer.clone()],
        ..message(450, BOT, "변형된 본문")
    };
    let reader = Fake::with([message(900, BOT, "earlier"), old_marked.clone()]);
    let mut session = ProbeSession::new(scope(&[0, 1]), vec![900], Instant::now());
    for progress in drive(&mut session, &reader, &known(), 4).await {
        assert!(matches!(progress, Progress::Incomplete(_)), "{progress:?}");
    }
    assert_eq!(session.attribution().unreadable.len(), 1);

    let own = scope(&[0]);
    let attribute = |snapshot: &AttributionSnapshot, seen: &ObservedMessage| {
        let mut into = Attribution::default();
        match_observations(&own, snapshot, [seen], &mut into);
        into
    };
    let old_original = ObservedMessage {
        nonce: Some(OLD_NONCE.into()),
        ..message(451, BOT, PAYLOAD)
    };
    let seen = attribute(&known(), &old_original);
    assert!(seen.found.is_empty());
    assert_eq!(seen.unattributed.iter().copied().collect::<Vec<_>>(), [451]);

    // Controls: another author, another piece's receipt, a readable marker of another piece.
    let other_author = ObservedMessage {
        author_id: OTHER,
        ..old_marked.clone()
    };
    assert!(attribute(&known(), &other_author).is_clear());
    assert!(attribute(&receipts(&[(450, key("f4-other"))]), &old_marked).is_clear());
    let readable_other = ObservedMessage {
        footers: vec![repost_footer(&key("f4-other"))],
        ..old_marked
    };
    assert!(attribute(&known(), &readable_other).is_clear());
}
