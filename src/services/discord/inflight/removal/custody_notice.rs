//! Boot custody notice: one message_outbox row per TUI-direct custody episode, deduped for good
//! by what its marker records; what PostgreSQL does not take waits for the next boot.

use super::boot_custody::sha;
use crate::services::discord::bot_role::UtilityBotRole;
use crate::services::discord::outbound::DISCORD_SAFE_LIMIT_CHARS;
use crate::services::discord::{ProviderKind, runtime_store};
use crate::services::message_outbox::{
    OutboxMessage, delivery_bot_for_target_session,
    enqueue_outbox_pg_returning_id_with_persistent_dedupe as enqueue,
};
use serde_json::Value;
use sqlx::PgPool;
use std::fs;
use std::path::{Path, PathBuf};

const FAILED: &str =
    "보존 실패: 일부 사본을 남기지 못했습니다. 보존본의 manifest에 원본 위치와 오류가 있습니다.";

/// One custody episode's notice: its outbox target, session key, sending bot and text.
#[derive(Debug)]
pub(super) struct Notice {
    pub(super) target: String,
    pub(super) session: String,
    pub(super) bot: String,
    pub(super) text: String,
    pub(super) dir: PathBuf,
}

/// Starts the notice pass off the boot path over the provider's custody.
pub(super) fn spawn_boot_custody_notice(provider: &ProviderKind, pool: Option<PgPool>) {
    let Some(root) = runtime_store::runtime_root() else {
        return;
    };
    let custody = root.join("discord_custody").join(provider.as_str());
    let provider = provider.clone();
    crate::services::discord::task_supervisor::spawn_observed("boot_custody_notice", async move {
        enqueue_custody_notices(&custody, &provider, pool.as_ref()).await;
    });
}

/// Enqueues every notice in `custody` and returns how many PostgreSQL holds; failures only log.
pub(super) async fn enqueue_custody_notices(
    custody: &Path,
    provider: &ProviderKind,
    pool: Option<&PgPool>,
) -> usize {
    let (notices, provider) = (notices(custody, provider), provider.as_str());
    let Some(pool) = pool else {
        if !notices.is_empty() {
            tracing::warn!(
                provider,
                turns = notices.len(),
                "custody notices wait for PostgreSQL"
            );
        }
        return 0;
    };
    let mut enqueued = 0;
    for notice in &notices {
        let (session, dir) = (notice.session.as_str(), notice.dir.display());
        let message = OutboxMessage {
            target: &notice.target,
            content: &notice.text,
            bot: &notice.bot,
            source: "boot_custody_notice",
            reason_code: Some("boot_custody.notice"),
            session_key: Some(session),
        };
        // The text names the episode only; its full path is logged here.
        match enqueue(pool, message).await {
            Ok(_) => enqueued += 1,
            Err(error) => {
                tracing::warn!(provider, %session, %dir, %error, "custody notice not enqueued");
                continue;
            }
        }
        tracing::info!(provider, %session, %dir, "custody notice held by the outbox");
    }
    enqueued
}

/// The notice of each TUI-direct episode in custody; an episode whose marker fails is logged
/// with its path and skipped.
pub(super) fn notices(custody: &Path, provider: &ProviderKind) -> Vec<Notice> {
    let entries = match fs::read_dir(custody) {
        Ok(entries) => entries.flatten().map(|entry| entry.path()),
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(provider = provider.as_str(), %error, "custody not listed");
            }
            return Vec::new();
        }
    };
    let mut dirs: Vec<PathBuf> = entries.collect();
    dirs.sort();
    let mut notices = Vec::new();
    for dir in dirs {
        match episode_notice(&dir, provider) {
            Ok(notice) => notices.extend(notice),
            Err((step, cause)) => {
                let (provider, dir) = (provider.as_str(), dir.display());
                tracing::warn!(provider, %dir, step, %cause, "custody episode skipped");
            }
        }
    }
    notices
}

/// A TUI-direct episode's notice. Its key comes from the marker alone: the turn nonce a current
/// anchorless marker records, else the digest of the marker's episode key.
fn episode_notice(
    dir: &Path,
    provider: &ProviderKind,
) -> Result<Option<Notice>, (&'static str, String)> {
    let marker = fs::read(dir.join("episode.json"));
    let marker = marker.map_err(|error| ("read episode.json", error.to_string()))?;
    let marker = serde_json::from_slice::<Value>(&marker);
    let marker = marker.map_err(|error| ("parse episode.json", error.to_string()))?;
    if marker["tui_direct"] != true {
        return Ok(None);
    }
    let episode = &marker["episode"];
    let channel = episode["channel_id"].as_u64().filter(|id| *id != 0);
    let channel = channel.ok_or(("find channel_id", "episode.json names none".to_string()))?;
    let nonce = episode["anchorless"].as_str().filter(|n| !n.is_empty());
    let turn = nonce.map_or_else(|| sha(episode.to_string().as_bytes()), str::to_string);
    let (target, tmux) = (format!("channel:{channel}"), copied_tmux(dir));
    let bot =
        delivery_bot_for_target_session(&target, UtilityBotRole::Notify.alias(), tmux.as_deref());
    let session = format!("boot_custody/{}/{turn}", provider.as_str());
    let (bot, dir) = (bot.into_owned(), dir.to_path_buf());
    let text = notice_text(provider.as_str(), &dir);
    Ok(Some(Notice {
        target,
        session,
        bot,
        text,
        dir,
    }))
}

/// The tmux session of the episode's first row or pending-start copy that names one; it only
/// picks the sending bot.
fn copied_tmux(dir: &Path) -> Option<String> {
    let revisions = fs::read_dir(dir).into_iter().flatten().flatten();
    let copies = revisions.flat_map(|rev| fs::read_dir(rev.path()).into_iter().flatten().flatten());
    let mut copies: Vec<PathBuf> = copies.map(|copy| copy.path()).collect();
    let routed = |name: &str| name.ends_with("-row.json") || name.ends_with("-pending_start.json");
    copies.retain(|copy| routed(&copy.to_string_lossy()));
    copies.sort();
    copies.iter().find_map(|copy| {
        let copy: Value = serde_json::from_slice(&fs::read(copy).ok()?).ok()?;
        let tmux = copy["tmux_session_name"].as_str()?;
        (!tmux.is_empty()).then(|| tmux.to_string())
    })
}

/// The fixed line, the episode under the runtime root and a failed-copy line, capped to one
/// Discord message.
fn notice_text(provider: &str, dir: &Path) -> String {
    let episode = dir.file_name().unwrap_or_default().to_string_lossy();
    let mut lines = vec![
        "⚠️ 재시작으로 이 턴 출력 일부가 전달되지 않았을 수 있음.".to_string(),
        format!("보존본: `discord_custody/{provider}/{episode}`"),
    ];
    if !preserved_completely(dir) {
        lines.push(FAILED.to_string());
    }
    let text = lines.join("\n");
    text.chars().take(DISCORD_SAFE_LIMIT_CHARS).collect()
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

/// The notice text for one episode directory.
#[cfg(test)]
pub(in crate::services::discord) fn custody_notice_text(dir: &Path, provider: &str) -> String {
    notice_text(provider, dir)
}
