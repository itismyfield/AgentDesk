//! Boot custody notice: one message per TUI-direct turn in custody, from the channel's routing bot.
//! An unknown post result is settled by the bot's nonce inside its retry window, else by history.

use crate::services::discord::http::send_channel_message_with_nonce;
use crate::services::discord::settings::DiscordBotLaunchConfig;
use crate::services::discord::settings::{self, validate_bot_channel_routing};
use crate::services::discord::task_notification_delivery::nonce_retry_allowed;
use crate::services::discord::{DiscordBotSettings, ProviderKind};
use crate::services::discord::{SharedData, rate_limit_wait, runtime_store};
use chrono::{DateTime, Utc};
use poise::serenity_prelude as serenity;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const STATE_FILE: &str = "notice.json";
const HISTORY_PAGES: usize = 10;
const FAILED: &str =
    "보존 실패: 일부 사본을 남기지 못했습니다. 경로의 manifest에 원본 위치와 오류가 있습니다.\n";
/// The wait before each pass in this process; what the last pass leaves waits for the next boot.
const PASS_DELAYS: [u64; 4] = [0, 30, 120, 600];

/// Durable notice progress in the episode directory; a missing file means pending. `bot` is the
/// Discord user that makes the POST, and only it may send the nonce again.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum NoticeState {
    Posting {
        nonce: String,
        post_started_at: DateTime<Utc>,
        bot: u64,
    },
    Sent {
        message_id: u64,
    },
}

/// The provider's bots, each by its token hash with its settings, in token hash order.
pub(super) type Bots = [(String, DiscordBotSettings)];

/// One bot's notice pass: its token hash, the provider's bots, and the clock read before a POST.
pub(super) struct NoticePass<'a> {
    pub(super) http: &'a serenity::Http,
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) provider: &'a ProviderKind,
    pub(super) bot: &'a str,
    pub(super) bots: &'a Bots,
    pub(super) clock: &'a (dyn Fn() -> DateTime<Utc> + Sync),
}

/// Spawns the notice passes once per bot; a runtime without a bot token is skipped.
pub(in crate::services::discord) fn spawn_boot_custody_notice(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) {
    static STARTED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        return;
    };
    let mut started = STARTED.lock().unwrap_or_else(|poison| poison.into_inner());
    if !started.insert(shared.token_hash.clone()) {
        return;
    }
    let (shared, provider) = (shared.clone(), provider.clone());
    crate::services::discord::task_supervisor::spawn_observed("boot_custody_notice", async move {
        let bot = shared.token_hash.as_str();
        // Every bot reads the same configured settings, so all agree on each channel's sender.
        let configs = settings::load_discord_bot_launch_configs().into_iter();
        let configs = configs.filter(|config| config.provider == provider);
        let load = |c: DiscordBotLaunchConfig| (c.hash_key, settings::load_bot_settings(&c.token));
        let mut bots: Vec<_> = configs.map(load).collect();
        if bots.iter().all(|(hash, _)| hash != bot) {
            bots.push((bot.to_string(), shared.settings.read().await.clone()));
        }
        bots.sort_by(|left, right| left.0.cmp(&right.0));
        let pass = NoticePass {
            http: &http,
            shared: &shared,
            provider: &provider,
            bot,
            bots: &bots,
            clock: &Utc::now,
        };
        notify_with_retries(&pass, &PASS_DELAYS.map(Duration::from_secs)).await;
    });
}

/// Passes after each delay until one leaves nothing unsettled; no retry outlives the process.
pub(super) async fn notify_with_retries(pass: &NoticePass<'_>, delays: &[Duration]) {
    for delay in delays {
        tokio::time::sleep(*delay).await;
        if notify_custody_episodes(pass).await {
            return;
        }
    }
    let provider = pass.provider.as_str();
    tracing::warn!(%provider, "unsettled boot custody notices wait for the next boot");
}

/// One pass over the channels this bot sends for; true when none of their notices is unsettled.
async fn notify_custody_episodes(pass: &NoticePass<'_>) -> bool {
    let Some(root) = runtime_store::runtime_root() else {
        return true;
    };
    let provider = pass.provider.as_str();
    let entries = match fs::read_dir(root.join("discord_custody").join(provider)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return true,
        Err(error) => {
            tracing::warn!(provider, %error, "boot custody notice could not list custody");
            return false;
        }
    };
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    dirs.sort();
    // Episodes holding one anchorless turn, as earlier builds keyed it too, share one notice.
    let mut turns: BTreeMap<(u64, String), Vec<PathBuf>> = BTreeMap::new();
    for dir in dirs {
        if let Some((channel, turn)) = episode_turn(&dir) {
            let turn = turn.unwrap_or_else(|| dir.display().to_string());
            turns.entry((channel, turn)).or_default().push(dir);
        }
    }
    let (mut me, mut settled) = (None, true);
    for ((channel, _), dirs) in turns {
        let channel = serenity::ChannelId::new(channel);
        if sender(pass, channel) != Some(pass.bot) {
            continue;
        }
        if let Err(error) = notify_turn(pass, channel, &dirs, &mut me).await {
            let dir = dirs[0].display();
            tracing::warn!(provider, %dir, %error, "boot custody notice unsettled");
            settled = false;
        }
    }
    settled
}

/// A TUI-direct episode's channel and, when it is anchorless, the turn nonce of its first row
/// copy; the nonce survives offset rewrites and is the same under every key build.
fn episode_turn(dir: &Path) -> Option<(u64, Option<String>)> {
    let marker = fs::read(dir.join("episode.json")).ok()?;
    let marker: Value = serde_json::from_slice(&marker).ok()?;
    (marker["tui_direct"] == true).then_some(())?;
    let channel = marker["episode"]["channel_id"].as_u64();
    let channel = channel.filter(|id| *id != 0)?;
    let anchorless = !marker["episode"]["anchorless"].is_null();
    let revisions = fs::read_dir(dir).into_iter().flatten().flatten();
    let copies = revisions.flat_map(|rev| fs::read_dir(rev.path()).into_iter().flatten().flatten());
    let is_row = |copy: &PathBuf| anchorless && copy.to_string_lossy().ends_with("-row.json");
    let rows = copies.map(|copy| copy.path()).filter(is_row);
    let row = rows.min().and_then(|row| fs::read(row).ok());
    let row = row.and_then(|row| serde_json::from_slice::<Value>(&row).ok());
    let turn = row.and_then(|row| Some(row["turn_nonce"].as_str()?.to_string()));
    Some((channel, turn))
}

/// The channel's sender: the first bot, by token hash, whose settings route the channel, else the
/// first bot, so a channel no bot routes any more is still tried.
fn sender<'a>(pass: &NoticePass<'a>, channel: serenity::ChannelId) -> Option<&'a str> {
    let routes = |settings: &DiscordBotSettings| {
        let routing = validate_bot_channel_routing(settings, pass.provider, channel, None, false);
        routing.map_or_else(|reason| !reason.is_expected_cross_bot_skip(), |()| true)
    };
    let bot = pass.bots.iter().find(|(_, settings)| routes(settings));
    bot.or(pass.bots.first()).map(|(hash, _)| hash.as_str())
}

/// The text and token of the notice naming every episode of the turn; the token names the one
/// that holds the state.
fn notice_for(home: &Path, dirs: &[PathBuf]) -> Option<(String, String)> {
    let name = |dir: &Path| Some(dir.file_name()?.to_string_lossy().into_owned());
    let token = format!("adk-custody-notice:{}", name(home)?.get(..16)?);
    let episodes: Option<Vec<String>> = dirs.iter().map(|dir| name(dir)).collect();
    let paths: Vec<String> = dirs.iter().map(|dir| dir.display().to_string()).collect();
    let complete = dirs.iter().all(|dir| preserved_completely(dir));
    let failed = (!complete).then_some(FAILED).unwrap_or_default();
    let text = format!(
        "⚠️ 재시작으로 이 턴 출력 일부가 전달되지 않았을 수 있음.\n보존본: `{}`\n경로: `{}`\n{failed}`{token}`",
        episodes?.join("`, `"),
        paths.join("`, `")
    );
    Some((text, token))
}

/// Whether the newest published custody revision copied everything it attempted.
fn preserved_completely(dir: &Path) -> bool {
    let revisions = fs::read_dir(dir).into_iter().flatten().flatten();
    let mut manifests: Vec<PathBuf> = revisions.map(|r| r.path().join("manifest.json")).collect();
    manifests.retain(|manifest| manifest.is_file());
    manifests.sort();
    let newest = manifests.last().and_then(|m| fs::read(m).ok());
    let newest = newest.and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    newest.is_some_and(|manifest| manifest["complete"] == true)
}

/// pending → posting (recorded before the POST) → sent, in the first episode holding a state. The
/// posting bot resends its nonce inside the window judged at the POST; otherwise history decides.
async fn notify_turn(
    pass: &NoticePass<'_>,
    channel: serenity::ChannelId,
    dirs: &[PathBuf],
    me: &mut Option<u64>,
) -> Result<(), String> {
    let home = dirs.iter().find(|dir| dir.join(STATE_FILE).exists());
    let home = home.unwrap_or(&dirs[0]);
    let path = home.join(STATE_FILE);
    let state = match fs::read(&path) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes).ok()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.to_string()),
    };
    if let Some(Some(NoticeState::Sent { .. })) = state {
        return Ok(());
    }
    let (text, token) = notice_for(home, dirs).ok_or("the episode has no name")?;
    if me.is_none() {
        let user = pass.http.get_current_user().await;
        *me = Some(user.map_err(|error| error.to_string())?.id.get());
    }
    let me = me.unwrap_or_default();
    rate_limit_wait(pass.shared, channel).await;
    let nonce = match state {
        None => begin_posting(&path, (pass.clock)(), me)?,
        Some(Some(NoticeState::Posting {
            nonce,
            post_started_at,
            bot,
        })) if bot == me && nonce_retry_allowed(post_started_at, (pass.clock)()) => nonce,
        // Another bot's posting, one past the window, or a state that no longer parses.
        Some(state) => {
            let author = match state {
                Some(NoticeState::Posting { bot, .. }) => bot,
                _ => me,
            };
            if let Some(message_id) = find_token(pass, channel, &token, author).await? {
                return record(&path, &NoticeState::Sent { message_id });
            }
            rate_limit_wait(pass.shared, channel).await;
            begin_posting(&path, (pass.clock)(), me)?
        }
    };
    let sent = send_channel_message_with_nonce(pass.http, channel, &text, &nonce).await;
    let message_id = sent.map_err(|error| error.to_string())?.id.get();
    record(&path, &NoticeState::Sent { message_id })
}

/// Durably records a fresh nonce before the POST that uses it.
fn begin_posting(path: &Path, now: DateTime<Utc>, bot: u64) -> Result<String, String> {
    let nonce = format!("adkcn{}", &uuid::Uuid::new_v4().simple().to_string()[..20]);
    let posting = NoticeState::Posting {
        nonce: nonce.clone(),
        post_started_at: now,
        bot,
    };
    record(path, &posting)?;
    runtime_store::fsync_parent_dir(path).map_err(|error| error.to_string())?;
    Ok(nonce)
}

fn record(path: &Path, state: &NoticeState) -> Result<(), String> {
    let text = serde_json::to_string(state).map_err(|error| error.to_string())?;
    runtime_store::atomic_write(path, &text)
}

/// Newest-first search of the channel's latest messages, from any author, for the token posted
/// by `author`; a failed read is an error.
async fn find_token(
    pass: &NoticePass<'_>,
    channel: serenity::ChannelId,
    token: &str,
    author: u64,
) -> Result<Option<u64>, String> {
    let mut before = None;
    for _ in 0..HISTORY_PAGES {
        let mut query = serenity::GetMessages::new().limit(100);
        if let Some(message) = before {
            query = query.before(message);
        }
        rate_limit_wait(pass.shared, channel).await;
        let page = channel.messages(pass.http, query).await;
        let page = page.map_err(|error| error.to_string())?;
        let ours = |m: &&serenity::Message| m.author.id == author && m.content.contains(token);
        if let Some(found) = page.iter().find(ours) {
            return Ok(Some(found.id.get()));
        }
        match page.last() {
            Some(oldest) if page.len() == 100 => before = Some(oldest.id),
            _ => return Ok(None),
        }
    }
    Ok(None)
}
