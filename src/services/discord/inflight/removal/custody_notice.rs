//! Boot custody notice: one message_outbox row per TUI-direct turn in custody, deduped for good by
//! the turn's episode identity; what PostgreSQL does not take waits for the next boot.

use crate::services::discord::bot_role::UtilityBotRole;
use crate::services::discord::{ProviderKind, runtime_store};
use crate::services::message_outbox::{
    OutboxMessage, enqueue_outbox_pg_returning_id_with_persistent_dedupe as enqueue,
};
use serde_json::Value;
use sqlx::PgPool;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const FAILED: &str =
    "보존 실패: 일부 사본을 남기지 못했습니다. 경로의 manifest에 원본 위치와 오류가 있습니다.";

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
    for (target, session, content) in &notices {
        let message = OutboxMessage {
            target,
            content,
            bot: UtilityBotRole::Notify.alias(),
            source: "boot_custody_notice",
            reason_code: Some("boot_custody.notice"),
            session_key: Some(session),
        };
        match enqueue(pool, message).await {
            Ok(_) => enqueued += 1,
            Err(error) => tracing::warn!(provider, %session, %error, "custody notice not enqueued"),
        }
    }
    enqueued
}

/// Target, session key and text of each TUI-direct turn in custody. The key names the turn: an
/// anchorless turn's nonce, which every key build shares, else its episode directory.
pub(super) fn notices(custody: &Path, provider: &ProviderKind) -> Vec<(String, String, String)> {
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
    let mut turns: BTreeMap<(u64, String), (Option<String>, Vec<PathBuf>)> = BTreeMap::new();
    for dir in dirs {
        let Some(((channel, nonce, tmux), name)) = episode_turn(&dir).zip(dir_name(&dir)) else {
            continue;
        };
        let turn = turns.entry((channel, nonce.unwrap_or(name)));
        turn.or_insert((tmux, Vec::new())).1.push(dir);
    }
    let mut notices = Vec::new();
    for ((channel, turn), (tmux, dirs)) in turns {
        // A trailing tmux session lets the outbox send a DM session's notice from its own bot.
        let session = format!("boot_custody/{}/{turn}", provider.as_str());
        let session = match tmux {
            Some(tmux) => format!("{session}:{tmux}"),
            None => session,
        };
        let text = notice_text(&dirs);
        notices.extend(text.map(|text| (format!("channel:{channel}"), session, text)));
    }
    notices
}

fn dir_name(dir: &Path) -> Option<String> {
    Some(dir.file_name()?.to_string_lossy().into_owned())
}

/// A TUI-direct episode's channel, the nonce of its first row copy when it is anchorless, and
/// that copy's tmux session.
fn episode_turn(dir: &Path) -> Option<(u64, Option<String>, Option<String>)> {
    let marker = fs::read(dir.join("episode.json")).ok()?;
    let marker: Value = serde_json::from_slice(&marker).ok()?;
    (marker["tui_direct"] == true).then_some(())?;
    let channel = marker["episode"]["channel_id"].as_u64();
    let channel = channel.filter(|id| *id != 0)?;
    let anchorless = !marker["episode"]["anchorless"].is_null();
    let revisions = fs::read_dir(dir).into_iter().flatten().flatten();
    let copies = revisions.flat_map(|rev| fs::read_dir(rev.path()).into_iter().flatten().flatten());
    let rows = copies.map(|copy| copy.path());
    let row = rows
        .filter(|copy| copy.to_string_lossy().ends_with("-row.json"))
        .min();
    let row = row.and_then(|row| serde_json::from_slice::<Value>(&fs::read(row).ok()?).ok());
    let field = |name: &str| {
        let value = row.as_ref()?[name].as_str()?;
        (!value.is_empty()).then(|| value.to_string())
    };
    Some((
        channel,
        field("turn_nonce").filter(|_| anchorless),
        field("tmux_session_name"),
    ))
}

/// One message naming the turn's first episode and its path; other episodes are only counted,
/// so the text stays within one Discord message.
fn notice_text(dirs: &[PathBuf]) -> Option<String> {
    let home = dirs.first()?;
    let mut lines = vec![
        "⚠️ 재시작으로 이 턴 출력 일부가 전달되지 않았을 수 있음.".to_string(),
        format!("보존본: `{}`", dir_name(home)?),
        format!("경로: `{}`", home.display()),
    ];
    if dirs.len() > 1 {
        lines.push(format!(
            "같은 턴의 보존본 {}개가 같은 폴더에 더 있음.",
            dirs.len() - 1
        ));
    }
    if !dirs.iter().all(|dir| preserved_completely(dir)) {
        lines.push(FAILED.to_string());
    }
    Some(lines.join("\n"))
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
