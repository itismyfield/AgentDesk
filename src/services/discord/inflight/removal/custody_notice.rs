//! Boot custody notice: one message_outbox row per TUI-direct custody episode, deduped for good
//! by what its marker records; what PostgreSQL does not take waits for the next boot.

use super::boot_custody::{revision_index, sha};
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

pub(super) mod retention;

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Preservation {
    Boot,
    #[cfg(unix)]
    InProcess,
}

#[derive(Clone, Copy)]
enum NoticeKind {
    BootOutput,
    Output,
    InputNotSent,
}

#[derive(Debug)]
struct TurnDelivery {
    committed: bool,
    nonce: Option<String>,
}

const FAILED: &str =
    "보존 실패: 일부 사본을 남기지 못했습니다. 보존본의 manifest에 원본 위치와 오류가 있습니다.";

/// The outbox address and notice eligibility of one retained custody episode.
#[derive(Debug)]
pub(super) struct Notice {
    pub(super) target: String,
    pub(super) session: String,
    pub(super) bot: String,
    pub(super) text: String,
    pub(super) dir: PathBuf,
    eligible: bool,
    delivery: Option<TurnDelivery>,
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

/// Recovers undelivered notices and retires expired delivered custody; failures only log.
pub(super) async fn enqueue_custody_notices(
    custody: &Path,
    provider: &ProviderKind,
    pool: Option<&PgPool>,
) -> usize {
    let _lock = match retention::lock(custody, false) {
        Ok(lock) => lock,
        Err(error) => {
            tracing::warn!(provider = provider.as_str(), %error, "custody notices wait for custody lock");
            return 0;
        }
    };
    let notices = notices(custody, provider);
    let Some(pool) = pool else {
        if !notices.is_empty() {
            tracing::warn!(
                provider = provider.as_str(),
                turns = notices.len(),
                "custody notices wait for PostgreSQL"
            );
        }
        return 0;
    };
    let (mut enqueued, mut removed, mut bytes) = (0, 0, 0u64);
    for notice in &notices {
        let (session, dir) = (notice.session.as_str(), notice.dir.display());
        match retention::terminal(pool, notice).await {
            Ok(Some(sent_at)) => {
                if let Some(sent_at) = sent_at {
                    match retention::remove_expired(
                        &notice.dir,
                        sent_at.into(),
                        std::time::SystemTime::now(),
                    ) {
                        Ok(Some(size)) => {
                            removed += 1;
                            bytes += size;
                        }
                        Ok(None) => {}
                        Err(error) => {
                            tracing::warn!(%dir, %error, "terminal custody retained after cleanup error")
                        }
                    }
                }
                continue;
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(%session, %error, "custody delivery state unknown");
            }
        }
        if !notice.eligible || turn_delivered(pool, notice).await {
            continue;
        }
        let message = OutboxMessage {
            target: &notice.target,
            content: &notice.text,
            bot: &notice.bot,
            source: "boot_custody_notice",
            reason_code: Some("boot_custody.notice"),
            session_key: Some(session),
        };
        // The text names the episode only; its full path is logged here.
        let id = match enqueue(pool, message).await {
            Ok(id) => id,
            Err(error) => {
                tracing::warn!(provider = provider.as_str(), %session, %dir, %error, "custody notice not enqueued");
                continue;
            }
        };
        enqueued += 1;
        tracing::info!(provider = provider.as_str(), %session, %dir, "custody notice held by the outbox");
        if notice.bot != UtilityBotRole::Notify.alias() {
            readdress(pool, id, &notice.bot).await;
        }
    }
    if removed != 0 {
        tracing::info!(
            provider = provider.as_str(),
            removed,
            bytes,
            "custody retention removed"
        );
    }
    enqueued
}

async fn turn_delivered(pool: &PgPool, notice: &Notice) -> bool {
    let Some(delivery) = &notice.delivery else {
        return false;
    };
    if delivery.committed {
        return true;
    }
    let Some(nonce) = &delivery.nonce else {
        return false;
    };
    // A sent headless row carries the terminal body and the nonce of its producer.
    match sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM message_outbox
         WHERE target = $1 AND producer_turn_nonce = $2 AND status = 'sent'
           AND source = 'headless_turn' AND reason_code = 'headless.delivery')",
    )
    .bind(&notice.target)
    .bind(nonce)
    .fetch_one(pool)
    .await
    {
        Ok(delivered) => delivered,
        Err(error) => {
            tracing::warn!(session = %notice.session, %error, "custody turn delivery unknown");
            false
        }
    }
}

/// Moves a row a copy-less pass staged for the notify bot to the provider bot while no worker has
/// claimed or retried it; a claimed row keeps its bot.
async fn readdress(pool: &PgPool, id: i64, bot: &str) {
    let updated = sqlx::query(
        "UPDATE message_outbox SET bot = $2
         WHERE id = $1 AND status = 'pending' AND retry_count = 0 AND bot = $3",
    );
    let notify = UtilityBotRole::Notify.alias();
    let updated = updated.bind(id).bind(bot).bind(notify).execute(pool).await;
    if let Err(error) = updated {
        tracing::warn!(id, %error, "custody notice bot not updated");
    }
}

/// Reads each TUI-direct episode for notice delivery and retention.
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
        if !fs::symlink_metadata(&dir).is_ok_and(|meta| meta.is_dir()) {
            continue;
        }
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

/// A TUI-direct episode's notice, keyed by the turn nonce its marker records (or, for an earlier
/// build's anchorless marker, a copy names), else the digest of the marker's episode key.
fn episode_notice(
    dir: &Path,
    provider: &ProviderKind,
) -> Result<Option<Notice>, (&'static str, String)> {
    let marker = dir.join("episode.json");
    if !fs::symlink_metadata(&marker).is_ok_and(|meta| meta.is_file()) {
        return Err(("read episode.json", "not a regular file".to_string()));
    }
    let marker = fs::read(marker);
    let marker = marker.map_err(|error| ("read episode.json", error.to_string()))?;
    let marker = serde_json::from_slice::<Value>(&marker);
    let marker = marker.map_err(|error| ("parse episode.json", error.to_string()))?;
    if marker["tui_direct"] != true {
        return Ok(None);
    }
    let episode = &marker["episode"];
    let channel = episode["channel_id"].as_u64().filter(|id| *id != 0);
    let channel = channel.ok_or(("find channel_id", "episode.json names none".to_string()))?;
    let nonce = match &episode["anchorless"] {
        Value::String(nonce) => Some(nonce.clone()).filter(|n| !n.is_empty()),
        // An earlier build's start-time key can be two turns' key; a copied nonce parts them, and
        // an episode whose copies cannot be read then may split from its row.
        Value::Array(_) => copied(dir, "turn_nonce"),
        _ => None,
    };
    let turn = nonce.unwrap_or_else(|| sha(episode.to_string().as_bytes()));
    let target = format!("channel:{channel}");
    let tmux = copied(dir, "tmux_session_name");
    let bot =
        delivery_bot_for_target_session(&target, UtilityBotRole::Notify.alias(), tmux.as_deref());
    let session = format!("boot_custody/{}/{turn}", provider.as_str());
    let (bot, dir) = (bot.into_owned(), dir.to_path_buf());
    let evidence = episode_evidence(&dir, episode, provider);
    let kind = if evidence.output {
        Some(if evidence.restart_only {
            NoticeKind::BootOutput
        } else {
            NoticeKind::Output
        })
    } else {
        evidence
            .input_not_sent
            .is_some()
            .then_some(NoticeKind::InputNotSent)
    };
    let text = kind
        .map(|kind| notice_text(provider.as_str(), &dir, kind))
        .unwrap_or_default();
    Ok(Some(Notice {
        target,
        session,
        bot,
        text,
        dir,
        eligible: kind.is_some(),
        delivery: if evidence.output {
            evidence.delivery
        } else {
            evidence.input_not_sent
        },
    }))
}

/// `field` of the episode's first row or pending-start copy that names it.
fn copied(dir: &Path, field: &str) -> Option<String> {
    let revisions = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|rev| rev.file_type().is_ok_and(|kind| kind.is_dir()));
    let copies = revisions.flat_map(|rev| fs::read_dir(rev.path()).into_iter().flatten().flatten());
    let mut copies: Vec<PathBuf> = copies.map(|copy| copy.path()).collect();
    let routed = |name: &str| name.ends_with("-row.json") || name.ends_with("-pending_start.json");
    copies.retain(|copy| {
        routed(&copy.to_string_lossy())
            && fs::symlink_metadata(copy).is_ok_and(|meta| meta.is_file())
    });
    copies.sort();
    copies.iter().find_map(|copy| {
        let copy: Value = serde_json::from_slice(&fs::read(copy).ok()?).ok()?;
        let value = copy[field].as_str()?;
        (!value.is_empty()).then(|| value.to_string())
    })
}

struct Evidence {
    output: bool,
    restart_only: bool,
    input_not_sent: Option<TurnDelivery>,
    delivery: Option<TurnDelivery>,
}

impl Evidence {
    fn unknown(&mut self) {
        self.output = true;
        self.restart_only = false;
        self.delivery = None;
    }
}

fn episode_evidence(dir: &Path, episode: &Value, provider: &ProviderKind) -> Evidence {
    let mut evidence = Evidence {
        output: false,
        restart_only: true,
        input_not_sent: None,
        delivery: None,
    };
    let Ok(revisions) = fs::read_dir(dir).and_then(Iterator::collect::<Result<Vec<_>, _>>) else {
        evidence.unknown();
        return evidence;
    };
    let mut revisions: Vec<_> = revisions
        .into_iter()
        .filter_map(|entry| Some((revision_index(&entry.file_name())?, entry)))
        .collect();
    revisions.sort_by_key(|(index, entry)| (*index, entry.file_name()));
    if revisions.is_empty() {
        evidence.unknown();
    }
    for (_, revision) in revisions {
        if !revision.file_type().is_ok_and(|kind| kind.is_dir()) {
            evidence.unknown();
            continue;
        }
        let path = revision.path();
        let Some(manifest) = read_json(&path.join("manifest.json")) else {
            evidence.unknown();
            continue;
        };
        let Some(entries) = manifest["entries"]
            .as_array()
            .filter(|items| !items.is_empty())
        else {
            evidence.unknown();
            continue;
        };
        // The original writer only published row, pending-start and transcript copies at boot.
        let at_boot = manifest["preservation"].is_null() || manifest["preservation"] == "boot";
        let mut has_identity_copy = false;
        let mut delivery = None;
        for item in entries {
            match item["kind"].as_str() {
                Some("native_input") => {
                    let input = custody_copy(&path, item, "-native_input.json")
                        .and_then(|copy| native_not_sent(&copy, episode, provider));
                    if input.is_some() {
                        evidence.input_not_sent = input;
                    }
                }
                Some(kind @ ("row" | "pending_start" | "transcript")) => {
                    evidence.output = true;
                    evidence.restart_only &= at_boot;
                    has_identity_copy |= kind != "transcript";
                    if kind == "row" {
                        delivery = custody_copy(&path, item, "-row.json")
                            .and_then(|copy| row_delivery(&copy, episode, provider));
                    }
                }
                _ => evidence.unknown(),
            }
        }
        if has_identity_copy {
            evidence.delivery = delivery;
        }
    }
    evidence
}

fn read_json(path: &Path) -> Option<Value> {
    if !fs::symlink_metadata(path).ok()?.is_file() {
        return None;
    }
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn custody_copy(revision: &Path, entry: &Value, suffix: &str) -> Option<Value> {
    let name = entry["copy"].as_str()?;
    let index = name.strip_suffix(suffix)?;
    if index.is_empty() || !index.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let path = revision.join(name);
    if !fs::symlink_metadata(&path).ok()?.is_file() {
        return None;
    }
    let bytes = fs::read(path).ok()?;
    if !entry["error"].is_null() || entry["sha256"].as_str()? != sha(&bytes) {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn native_not_sent(copy: &Value, episode: &Value, provider: &ProviderKind) -> Option<TurnDelivery> {
    let settlement = &copy["settlement"];
    let ticket = &settlement["ticket"];
    let owner = &ticket["owning_episode"];
    let anchor = owner["user_message_id"]
        .as_u64()
        .or_else(|| ticket["key"]["identity"]["primary"].as_u64())?;
    if settlement["not_sent"] != true
        || ticket["channel_id"] != episode["channel_id"]
        || ticket["key"]["attempt"]["provider"] != provider.as_str()
        || anchor == 0
        || episode["anchor_id"] != anchor
    {
        return None;
    }
    Some(TurnDelivery {
        committed: false,
        nonce: copied_nonce(owner),
    })
}

fn copied_nonce(value: &Value) -> Option<String> {
    value["turn_nonce"]
        .as_str()
        .filter(|nonce| !nonce.is_empty())
        .map(str::to_owned)
}

fn row_delivery(row: &Value, episode: &Value, provider: &ProviderKind) -> Option<TurnDelivery> {
    let anchor = episode["anchor_id"].as_u64()?;
    let nonce = copied_nonce(row);
    if row["provider"] != provider.as_str()
        || row["channel_id"] != episode["channel_id"]
        || row["user_msg_id"] != anchor
        || (anchor == 0 && nonce.as_deref() != Some(episode["anchorless"].as_str()?))
    {
        return None;
    }
    Some(TurnDelivery {
        committed: row["terminal_delivery_committed"] == true
            || row["session_bound_delivered"] == true
            || row["anchor_reposted"] == true,
        nonce,
    })
}

/// The warning, episode and failed-copy detail, capped to one Discord message.
fn notice_text(provider: &str, dir: &Path, kind: NoticeKind) -> String {
    let warning = match kind {
        NoticeKind::BootOutput => "⚠️ 재시작으로 이 턴 출력 일부가 전달되지 않았을 수 있음.",
        NoticeKind::Output => "⚠️ 이 턴 출력 일부가 전달되지 않았을 수 있음.",
        NoticeKind::InputNotSent => "⚠️ 이 턴의 입력이 전달되지 않았습니다.",
    };
    let episode = dir.file_name().unwrap_or_default().to_string_lossy();
    let mut lines = vec![
        warning.to_string(),
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
    let revisions = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|rev| rev.file_type().is_ok_and(|kind| kind.is_dir()));
    let mut manifests: Vec<PathBuf> = revisions.map(|r| r.path().join("manifest.json")).collect();
    manifests.retain(|manifest| fs::symlink_metadata(manifest).is_ok_and(|meta| meta.is_file()));
    manifests.sort();
    let newest = manifests.last().and_then(|m| fs::read(m).ok());
    let newest = newest.and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    newest.is_some_and(|manifest| manifest["complete"] == true)
}

/// The notice text for one episode directory.
#[cfg(test)]
pub(in crate::services::discord) fn custody_notice_text(dir: &Path, provider: &str) -> String {
    ProviderKind::from_str(provider)
        .and_then(|provider| episode_notice(dir, &provider).ok().flatten())
        .map_or_else(
            || notice_text(provider, dir, NoticeKind::BootOutput),
            |notice| notice.text,
        )
}
