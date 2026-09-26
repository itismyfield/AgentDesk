//! Custody notice contracts: episodes come from the real boot custody pass, and every notice
//! crosses serenity's HTTP client to a loopback Discord.

use super::custody_notice::{Bots, NoticePass, notify_with_retries};
use super::nondestructive_loader_tests::{CLAUDE, Env, row};
use super::*;
use crate::services::discord::{DiscordBotSettings, SharedData};
use axum::body::Body;
use axum::extract::State;
use axum::http::{Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use poise::serenity_prelude as serenity;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const BOT: u64 = 5_998_900;

/// How the loopback Discord treats the next message it has not seen the nonce of.
#[derive(Clone, Copy)]
enum Post {
    Answer,
    LoseResponse,
    LoseRequest,
    Hang,
}

/// A created message: id, channel, nonce, content, author and creation time.
#[derive(Clone)]
struct Msg(u64, u64, String, String, u64, DateTime<Utc>);

/// Created messages, each request's method, nonce and bot, the plan for the next new messages,
/// and the clock both sides read with the seconds each POST takes on it.
#[derive(Clone, Default)]
struct Wire {
    messages: Arc<Mutex<Vec<Msg>>>,
    requests: Arc<Mutex<Vec<(Method, Option<String>, u64)>>>,
    plan: Arc<Mutex<VecDeque<Post>>>,
    clock: Arc<Mutex<(DateTime<Utc>, i64)>>,
}

/// The bot user a token names: `BOT + 1` for a token ending in `-b`, `BOT` otherwise.
fn author(bot: u64) -> Value {
    json!({ "id": bot.to_string(), "username": "custody-bot", "discriminator": "0001",
        "avatar": null, "bot": true, "public_flags": 0 })
}

fn message_json(m: &Msg) -> Value {
    json!({
        "id": m.0.to_string(), "channel_id": m.1.to_string(), "author": author(m.4),
        "content": m.3, "timestamp": "2026-09-26T00:00:00.000000+00:00",
        "edited_timestamp": null, "tts": false, "mention_everyone": false,
        "mentions": [], "mention_roles": [], "mention_channels": [], "attachments": [],
        "embeds": [], "reactions": [], "nonce": null, "pinned": false, "type": 0,
        "flags": 0, "components": [], "sticker_items": [], "message_snapshots": []
    })
}

async fn discord(State(wire): State<Wire>, request: Request<Body>) -> Response {
    let (method, uri) = (request.method().clone(), request.uri().clone());
    let token = request.headers().get("authorization").cloned();
    let bot = BOT + u64::from(token.is_some_and(|token| token.as_bytes().ends_with(b"-b")));
    let body = axum::body::to_bytes(request.into_body(), 1 << 20).await;
    let payload: Option<Value> = serde_json::from_slice(&body.unwrap()).ok();
    let nonce = payload.as_ref().and_then(|p| p["nonce"].as_str());
    let nonce = nonce.map(str::to_string);
    let request = (method.clone(), nonce.clone(), bot);
    wire.requests.lock().unwrap().push(request);
    if uri.path() == "/api/v10/users/@me" {
        return Json(author(bot)).into_response();
    }
    let channel = uri.path().strip_prefix("/api/v10/channels/");
    let channel = channel.and_then(|rest| rest.strip_suffix("/messages")?.parse::<u64>().ok());
    let Some(channel) = channel else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let messages = wire.messages.lock().unwrap().clone();
    let ours = messages.iter().filter(|message| message.1 == channel);
    if method == Method::GET {
        let query = uri.query().unwrap_or_default().split('&');
        let before = query.filter_map(|pair| pair.strip_prefix("before=")?.parse::<u64>().ok());
        let before = before.last();
        let page = ours.rev().filter(|m| before.is_none_or(|id| m.0 < id));
        let page: Vec<Value> = page.take(100).map(message_json).collect();
        return Json(page).into_response();
    }
    // enforce_nonce: a nonce this bot used here in the last 120 s returns the message it created.
    let (nonce, (now, lag)) = (nonce.unwrap_or_default(), *wire.clock.lock().unwrap());
    wire.clock.lock().unwrap().0 = now + chrono::Duration::seconds(lag);
    let fresh = |m: &&Msg| m.2 == nonce && m.4 == bot && (now - m.5).num_seconds() < 120;
    if let Some(known) = ours.clone().find(fresh) {
        return Json(message_json(known)).into_response();
    }
    let plan = wire.plan.lock().unwrap().pop_front();
    let plan = plan.unwrap_or(Post::Answer);
    if matches!(plan, Post::LoseRequest) {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let content = payload.as_ref().and_then(|p| p["content"].as_str());
    let content = content.unwrap_or_default().to_string();
    let created = {
        let mut messages = wire.messages.lock().unwrap();
        let id = 6_000_000 + messages.len() as u64;
        messages.push(Msg(id, channel, nonce, content, bot, now));
        message_json(messages.last().unwrap())
    };
    match plan {
        Post::Hang => std::future::pending().await,
        Post::LoseResponse => StatusCode::BAD_GATEWAY.into_response(),
        _ => Json(created).into_response(),
    }
}

struct Harness {
    env: Env,
    wire: Wire,
    proxy: String,
    shared: Arc<SharedData>,
}

async fn harness(plan: &[Post]) -> Harness {
    let env = Env::new();
    let wire = Wire::default();
    wire.plan.lock().unwrap().extend(plan);
    let app = Router::new().fallback(discord).with_state(wire.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let shared = crate::services::discord::make_shared_data_for_tests();
    Harness {
        env,
        wire,
        proxy,
        shared,
    }
}

/// One unrestricted bot, named by its token.
fn anyone() -> [(String, DiscordBotSettings); 1] {
    [("test-token".to_string(), DiscordBotSettings::default())]
}

impl Harness {
    /// One boot with one unrestricted bot and one notice pass at `now`.
    async fn boot(&self, now: DateTime<Utc>) {
        self.boot_as(now, &anyone(), 1).await;
    }

    /// One boot at `now`: the real custody pass, then `passes` notice passes of each bot, named by
    /// its token; `bots` is in token order.
    async fn boot_as(&self, now: DateTime<Utc>, bots: &Bots, passes: usize) {
        self.wire.clock.lock().unwrap().0 = now;
        reap_inflight_rows_at_boot_with_guard(&BootReapOnce::default(), &CLAUDE).await;
        let clock = || self.wire.clock.lock().unwrap().0;
        for (bot, _) in bots {
            let http = serenity::HttpBuilder::new(bot).proxy(&self.proxy);
            let http = http.ratelimiter_disabled(true).build();
            let pass = NoticePass {
                http: &http,
                shared: &self.shared,
                provider: &CLAUDE,
                bot,
                bots,
                clock: &clock,
            };
            notify_with_retries(&pass, &vec![Duration::ZERO; passes]).await;
        }
    }

    fn requests(&self) -> usize {
        self.wire.requests.lock().unwrap().len()
    }

    /// Each POST's nonce and bot.
    fn posts(&self) -> Vec<(String, u64)> {
        let requests = self.wire.requests.lock().unwrap();
        let posts = requests.iter().filter(|r| r.0 == Method::POST);
        let posts = posts.map(|r| (r.1.clone().unwrap_or_default(), r.2));
        posts.collect()
    }

    fn contents(&self) -> Vec<String> {
        let messages = self.wire.messages.lock().unwrap();
        messages.iter().map(|message| message.3.clone()).collect()
    }

    fn custody(&self) -> PathBuf {
        let root = self.env.dir().with_file_name("discord_custody");
        root.join("claude")
    }

    fn episode_dirs(&self) -> Vec<PathBuf> {
        let dirs = fs::read_dir(self.custody()).into_iter().flatten().flatten();
        dirs.map(|dir| dir.path()).collect()
    }
}

/// Seeds an owner-1 ExternalInput row whose transcript turn exists or is missing.
fn tui_direct(env: &Env, channel_id: u64, transcript: bool) -> InflightTurnState {
    let mut state = row(channel_id, Some("AgentDesk-claude-notice"));
    (state.request_owner_user_id, state.turn_source) = (1, TurnSource::ExternalInput);
    let out = env.dir().with_file_name(format!("{channel_id}.jsonl"));
    fs::create_dir_all(env.dir()).unwrap();
    if transcript {
        fs::write(&out, "turn\n").unwrap();
    }
    (state.output_path, state.turn_start_offset) = (Some(out.display().to_string()), Some(0));
    env.seed(&state, 0);
    state
}

fn after(now: DateTime<Utc>, seconds: i64) -> DateTime<Utc> {
    now + chrono::Duration::seconds(seconds)
}

// Contract: a TUI-direct custody episode is announced once in its channel, naming the
// episode and its custody path, and a later boot sends nothing more.
#[tokio::test]
async fn a_tui_direct_episode_is_announced_once_across_boots() {
    let h = harness(&[]).await;
    tui_direct(&h.env, 5_998_001, true);
    let now = Utc::now();
    h.boot(now).await;
    let requests = h.requests();
    h.boot(after(now, 300)).await;

    let [dir] = h.episode_dirs().try_into().unwrap();
    let [text] = h.contents().try_into().unwrap();
    let episode = dir.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        text.contains("재시작으로 이 턴 출력 일부가 전달되지 않았을 수 있음"),
        "{text}"
    );
    assert!(
        text.contains(&episode) && text.contains(&dir.display().to_string()),
        "{text}"
    );
    assert!(!text.contains("보존 실패"), "{text}");
    assert_eq!(h.requests(), requests, "a settled episode makes no request");
}

// Contract: a POST whose response was lost is retried inside the nonce window with the same
// nonce, and Discord's nonce check leaves one message.
#[tokio::test]
async fn a_lost_response_inside_the_nonce_window_is_retried_with_the_same_nonce() {
    let h = harness(&[Post::LoseResponse]).await;
    tui_direct(&h.env, 5_998_011, true);
    let now = Utc::now();
    h.boot(now).await;
    h.boot(after(now, 30)).await;

    let [first, second] = h.posts().try_into().unwrap();
    assert_eq!(first, second);
    assert_eq!(h.contents().len(), 1);
}

// Contract: outside the nonce window, a notice whose token is already in the channel
// history is settled as sent with no second POST.
#[tokio::test]
async fn outside_the_window_a_notice_found_in_history_is_settled_without_a_post() {
    let h = harness(&[Post::LoseResponse]).await;
    tui_direct(&h.env, 5_998_021, true);
    let now = Utc::now();
    h.boot(now).await;
    h.boot(after(now, 121)).await;
    let requests = h.requests();
    h.boot(after(now, 400)).await;

    assert_eq!((h.posts().len(), h.contents().len()), (1, 1));
    assert_eq!(
        h.requests(),
        requests,
        "the history match settled the episode"
    );
}

// Contract: outside the nonce window, a notice missing from the channel history is posted
// again with a new nonce.
#[tokio::test]
async fn outside_the_window_a_notice_missing_from_history_is_posted_again() {
    let h = harness(&[Post::LoseRequest]).await;
    tui_direct(&h.env, 5_998_031, true);
    let now = Utc::now();
    h.boot(now).await;
    h.boot(after(now, 121)).await;

    let [first, second] = h.posts().try_into().unwrap();
    assert_ne!(first, second);
    assert_eq!(h.contents().len(), 1);
}

// Contract: the posting record is durable before the POST, so a boot that dies mid-POST
// leaves the next boot the same nonce and Discord keeps one message.
#[tokio::test]
async fn a_crash_after_the_posting_record_is_settled_by_the_next_boot() {
    let h = harness(&[Post::Hang]).await;
    tui_direct(&h.env, 5_998_041, true);
    let now = Utc::now();
    let crashed = tokio::time::timeout(Duration::from_secs(5), h.boot(now)).await;
    assert!(crashed.is_err(), "the first POST must still be in flight");
    h.boot(after(now, 30)).await;

    let [first, second] = h.posts().try_into().unwrap();
    assert_eq!(first, second);
    assert_eq!(h.contents().len(), 1);
}

// Contract: an episode that holds no TUI-direct turn is preserved but never announced.
#[tokio::test]
async fn an_episode_without_a_tui_direct_turn_is_not_announced() {
    let h = harness(&[]).await;
    h.env.seed(&row(5_998_051, None), 0);
    h.boot(Utc::now()).await;

    assert_eq!(h.episode_dirs().len(), 1);
    assert_eq!(h.requests(), 0);
}

// Contract: an episode whose custody copy failed is announced as a preservation failure.
#[tokio::test]
async fn an_episode_whose_copy_failed_is_announced_as_a_preservation_failure() {
    let h = harness(&[]).await;
    tui_direct(&h.env, 5_998_061, false);
    h.boot(Utc::now()).await;

    let [text] = h.contents().try_into().unwrap();
    assert!(text.contains("보존 실패"), "{text}");
}

// Contract: an anchorless turn is announced once while an earlier build's episode keyed by its
// start time and offset holds the same turn, and still once after its offset is rewritten.
#[tokio::test]
async fn an_anchorless_turn_is_announced_once_beside_an_earlier_builds_episode() {
    let h = harness(&[]).await;
    let mut turn = tui_direct(&h.env, 5_998_071, true);
    turn.user_msg_id = 0;
    let old = h.custody().join("0".repeat(64));
    let marker = json!({ "episode": { "provider": "claude", "channel_id": 5_998_071,
        "anchor_id": 0, "anchorless": [turn.started_at, 0] }, "tui_direct": true });
    fs::create_dir_all(old.join("rev-0000")).unwrap();
    fs::write(old.join("episode.json"), marker.to_string()).unwrap();
    let (rev, row) = (old.join("rev-0000"), serde_json::to_vec(&turn).unwrap());
    fs::write(rev.join("0-row.json"), row).unwrap();
    fs::write(rev.join("manifest.json"), r#"{"complete":true}"#).unwrap();
    for offset in [0, 1] {
        turn.turn_start_offset = Some(offset);
        h.env.seed(&turn, 0);
        h.boot(Utc::now()).await;
    }
    assert_eq!((h.episode_dirs().len(), h.contents().len()), (2, 1));
}

// Contract: two anchorless turns the real constructor starts in one second with the same start
// inputs share a finalizer turn id, yet each is its own episode with its own notice.
#[tokio::test]
async fn anchorless_turns_started_in_the_same_second_are_announced_apart() {
    let h = harness(&[]).await;
    let seeded = tui_direct(&h.env, 5_998_081, true);
    let (tmux, out) = (seeded.tmux_session_name, seeded.output_path);
    let start = || {
        let (text, tmux, out) = (String::new(), tmux.clone(), out.clone());
        let mut turn = InflightTurnState::new(
            CLAUDE, 5_998_081, None, 1, 0, 0, text, None, tmux, out, None, 0,
        );
        turn.turn_source = TurnSource::ExternalInput;
        turn
    };
    let (first, second) = loop {
        let pair = (start(), start());
        if pair.0.started_at == pair.1.started_at {
            break pair;
        }
    };
    assert_eq!(first.finalizer_turn_id, second.finalizer_turn_id);
    for turn in [first, second] {
        h.env.seed(&turn, 0);
        h.boot(Utc::now()).await;
    }
    assert_eq!(h.contents().len(), 2);
}

// Contract: a row written without a finalizer turn id or turn nonce, as older builds wrote it,
// stays one episode with one notice across boots and after the id is written back.
#[tokio::test]
async fn a_row_without_persisted_turn_ids_is_one_episode_across_boots_and_backfill() {
    let h = harness(&[]).await;
    let mut turn = tui_direct(&h.env, 5_998_091, true);
    turn.user_msg_id = 0;
    let path = h.env.seed(&turn, 0);
    let mut raw = serde_json::to_value(&turn).unwrap();
    for field in ["finalizer_turn_id", "turn_nonce"] {
        raw.as_object_mut().unwrap().remove(field);
    }
    for backfill in [false, false, true] {
        fs::write(&path, raw.to_string()).unwrap();
        if backfill {
            backfill_finalizer_turn_id_under_lock(&h.env.dir(), &path, &CLAUDE).unwrap();
            let text = fs::read_to_string(&path).unwrap();
            assert!(text.contains("finalizer_turn_id"), "{text}");
        }
        h.boot(Utc::now()).await;
    }
    assert_eq!((h.episode_dirs().len(), h.contents().len()), (1, 1));
}

// Contract: only the bot whose settings route the channel sends its notice, and a bot that takes
// the channel over settles the other bot's posting from history instead of reusing its nonce.
#[tokio::test]
async fn the_routing_bot_sends_and_a_new_sender_settles_another_bots_posting() {
    let h = harness(&[Post::LoseResponse]).await;
    tui_direct(&h.env, 5_998_101, true);
    let bot = |token: &str, channel: u64| {
        let allowed_channel_ids = vec![channel];
        let settings = DiscordBotSettings {
            allowed_channel_ids,
            ..Default::default()
        };
        (token.to_string(), settings)
    };
    let now = Utc::now();
    let bots = [bot("bot-a", 1), bot("bot-b", 5_998_101)];
    h.boot_as(now, &bots, 1).await;
    let bots = [bot("bot-a", 5_998_101), bot("bot-b", 1)];
    h.boot_as(after(now, 30), &bots, 1).await;

    let authors: Vec<u64> = h.posts().into_iter().map(|(_, bot)| bot).collect();
    assert_eq!((authors, h.contents().len()), (vec![BOT + 1], 1));
}

// Contract: a POST that failed before reaching Discord is retried in the same process, and one
// that keeps failing stops after the last pass instead of retrying without end.
#[tokio::test]
async fn a_failed_post_is_retried_in_process_a_bounded_number_of_times() {
    let h = harness(&[Post::LoseRequest; 4]).await;
    tui_direct(&h.env, 5_998_121, true);
    h.boot_as(Utc::now(), &anyone(), 3).await;
    assert_eq!((h.posts().len(), h.contents().len()), (3, 0));
    h.boot_as(Utc::now(), &anyone(), 3).await;
    assert_eq!((h.posts().len(), h.contents().len()), (5, 1));
}

// Contract: the nonce window is judged when each notice is posted, so a notice whose window ran
// out while earlier notices were sent is settled from history, not posted again.
#[tokio::test]
async fn the_nonce_window_is_judged_when_the_notice_is_posted() {
    let h = harness(&[Post::LoseResponse, Post::LoseResponse]).await;
    for channel in [5_998_111, 5_998_112] {
        tui_direct(&h.env, channel, true);
    }
    let now = Utc::now();
    h.boot(now).await;
    h.wire.clock.lock().unwrap().1 = 100;
    h.boot(after(now, 30)).await;
    assert_eq!(h.contents().len(), 2);
}
