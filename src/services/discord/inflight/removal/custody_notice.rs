//! Boot custody notice: one channel message per TUI-direct custody episode. A post whose result
//! is unknown is settled by its nonce inside Discord's retry window, by the body token outside it.

use crate::services::discord::task_notification_delivery::nonce_retry_allowed;
use crate::services::discord::{DiscordBotSettings, ProviderKind};
use crate::services::discord::{SharedData, rate_limit_wait, runtime_store};
use chrono::{DateTime, Utc};
use poise::serenity_prelude as serenity;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const STATE_FILE: &str = "notice.json";
const HISTORY_PAGES: usize = 10;

/// Durable notice progress in the episode directory; a missing file means pending.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
enum NoticeState {
    Posting {
        nonce: String,
        post_started_at: DateTime<Utc>,
    },
    Sent {
        message_id: u64,
    },
}

/// The provider's bots, each by its token hash with its settings, in token hash order.
pub(super) type Bots = [(String, DiscordBotSettings)];

/// One bot's notice pass: its token hash, the provider's bots, and the clock.
pub(super) struct NoticePass<'a> {
    pub(super) http: &'a serenity::Http,
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) provider: &'a ProviderKind,
    pub(super) bot: &'a str,
    pub(super) bots: &'a Bots,
    pub(super) clock: &'a (dyn Fn() -> DateTime<Utc> + Sync),
}

struct Notice {
    channel: serenity::ChannelId,
    text: String,
    token: String,
}

/// Spawns the notice pass once per provider; a runtime without a bot token is skipped.
pub(in crate::services::discord) fn spawn_boot_custody_notice(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) {
    static STARTED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        return;
    };
    let mut started = STARTED.lock().unwrap_or_else(|poison| poison.into_inner());
    if !started.insert(provider.as_str().to_string()) {
        return;
    }
    let (shared, provider) = (shared.clone(), provider.clone());
    crate::services::discord::task_supervisor::spawn_observed("boot_custody_notice", async move {
        let (http, shared, provider) = (&http, &shared, &provider);
        let (bot, bots, clock) = ("", &[], &Utc::now);
        let pass = NoticePass {
            http,
            shared,
            provider,
            bot,
            bots,
            clock,
        };
        notify_with_retries(&pass, &[Duration::ZERO]).await;
    });
}

pub(super) async fn notify_with_retries(pass: &NoticePass<'_>, _delays: &[Duration]) {
    let (http, shared, provider) = (pass.http, pass.shared, pass.provider);
    notify_custody_episodes(http, shared, provider, (pass.clock)()).await;
}

/// One pass over the provider's custody episodes; an unsettled notice waits for the next boot.
async fn notify_custody_episodes(
    http: &serenity::Http,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    now: DateTime<Utc>,
) {
    let Some(root) = runtime_store::runtime_root() else {
        return;
    };
    let custody = root.join("discord_custody").join(provider.as_str());
    let entries = match fs::read_dir(&custody) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            let provider = provider.as_str();
            return tracing::warn!(provider, %error, "boot custody notice could not list custody");
        }
    };
    let mut dirs: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    dirs.sort();
    for dir in dirs {
        let Some(notice) = notice_for(&dir) else {
            continue;
        };
        if let Err(error) = notify_episode(http, shared, &dir, &notice, now).await {
            let dir = dir.display();
            let provider = provider.as_str();
            tracing::warn!(provider, %dir, %error, "boot custody notice unsettled; the next boot retries");
        }
    }
}

/// The notice for a TUI-direct episode; other episodes hold row bytes only and stay silent.
fn notice_for(dir: &Path) -> Option<Notice> {
    let marker = fs::read(dir.join("episode.json")).ok()?;
    let marker: Value = serde_json::from_slice(&marker).ok()?;
    (marker["tui_direct"] == true).then_some(())?;
    let channel = marker["episode"]["channel_id"]
        .as_u64()
        .filter(|id| *id != 0)?;
    let episode = dir.file_name()?.to_string_lossy().into_owned();
    let token = format!("adk-custody-notice:{}", episode.get(..16)?);
    let failed = match preserved_completely(dir) {
        true => "",
        false => {
            "보존 실패: 일부 사본을 남기지 못했습니다. 경로의 manifest에 원본 위치와 오류가 있습니다.\n"
        }
    };
    let text = format!(
        "⚠️ 재시작으로 이 턴 출력 일부가 전달되지 않았을 수 있음.\n보존본: `{episode}`\n경로: `{}`\n{failed}`{token}`",
        dir.display()
    );
    let channel = serenity::ChannelId::new(channel);
    Some(Notice {
        channel,
        text,
        token,
    })
}

/// Whether the newest published custody revision copied everything it attempted.
fn preserved_completely(dir: &Path) -> bool {
    let revisions = fs::read_dir(dir).into_iter().flatten().flatten();
    let mut manifests: Vec<PathBuf> = revisions
        .map(|rev| rev.path().join("manifest.json"))
        .collect();
    manifests.retain(|manifest| manifest.is_file());
    manifests.sort();
    let newest = manifests
        .last()
        .and_then(|manifest| fs::read(manifest).ok());
    let newest = newest.and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    newest.is_some_and(|manifest| manifest["complete"] == true)
}

/// pending → posting (recorded before the POST) → sent. A posting inside the nonce window
/// retries its own nonce; outside it the channel history decides between sent and a new post.
async fn notify_episode(
    http: &serenity::Http,
    shared: &Arc<SharedData>,
    dir: &Path,
    notice: &Notice,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let path = dir.join(STATE_FILE);
    let state = match fs::read(&path) {
        Ok(bytes) => Some(serde_json::from_slice(&bytes).ok()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.to_string()),
    };
    let nonce = match state {
        None => begin_posting(&path, now)?,
        Some(Some(NoticeState::Sent { .. })) => return Ok(()),
        Some(Some(NoticeState::Posting {
            nonce,
            post_started_at,
        })) if nonce_retry_allowed(post_started_at, now) => nonce,
        // A posting past the window, or a state file that no longer parses: result unknown.
        Some(_) => {
            if let Some(message_id) = find_token(http, notice).await? {
                return record(&path, &NoticeState::Sent { message_id });
            }
            begin_posting(&path, now)?
        }
    };
    rate_limit_wait(shared, notice.channel).await;
    let sent = crate::services::discord::http::send_channel_message_with_nonce(
        http,
        notice.channel,
        &notice.text,
        &nonce,
    );
    let message = sent.await.map_err(|error| error.to_string())?;
    record(
        &path,
        &NoticeState::Sent {
            message_id: message.id.get(),
        },
    )
}

/// Durably records a fresh nonce before the POST that uses it.
fn begin_posting(path: &Path, now: DateTime<Utc>) -> Result<String, String> {
    let nonce = format!("adkcn{}", &uuid::Uuid::new_v4().simple().to_string()[..20]);
    let posting = NoticeState::Posting {
        nonce: nonce.clone(),
        post_started_at: now,
    };
    record(path, &posting)?;
    runtime_store::fsync_parent_dir(path).map_err(|error| error.to_string())?;
    Ok(nonce)
}

fn record(path: &Path, state: &NoticeState) -> Result<(), String> {
    let text = serde_json::to_string(state).map_err(|error| error.to_string())?;
    runtime_store::atomic_write(path, &text)
}

/// Newest-first search of this bot's recent messages for the token; a failed read is an error.
async fn find_token(http: &serenity::Http, notice: &Notice) -> Result<Option<u64>, String> {
    let bot = http
        .get_current_user()
        .await
        .map_err(|error| error.to_string())?
        .id;
    let mut before = None;
    for _ in 0..HISTORY_PAGES {
        let mut query = serenity::GetMessages::new().limit(100);
        if let Some(message) = before {
            query = query.before(message);
        }
        let page = notice.channel.messages(http, query).await;
        let page = page.map_err(|error| error.to_string())?;
        let ours = |message: &&serenity::Message| message.author.id == bot;
        let found = page
            .iter()
            .filter(ours)
            .find(|message| message.content.contains(&notice.token));
        if let Some(found) = found {
            return Ok(Some(found.id.get()));
        }
        match page.last() {
            Some(oldest) if page.len() == 100 => before = Some(oldest.id),
            _ => return Ok(None),
        }
    }
    Ok(None)
}
