use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use poise::serenity_prelude::{self as serenity, ChannelId, MessageId};

use crate::services::discord::{self as discord};
use crate::services::provider::ProviderKind;

/// #2860 - pure decision: the unrelayed byte range of `full_response` that a
/// completed-stale leak recovery should deliver. `start` is `response_sent_offset`
/// snapped down to a UTF-8 char boundary (Korean/multibyte safety) and clamped to
/// len; `end` is the full length. Returns `None` when nothing is unrelayed
/// (`start >= len`), making a repeated watchdog pass an idempotent no-op once the
/// offset has been advanced to len.
///
/// `last_watcher_relayed_offset` is deliberately NOT mixed into this range:
/// it is a tmux output-buffer coordinate, not a `full_response` byte index, so
/// max()-ing it against `response_sent_offset` could both over- and under-skip.
/// The authoritative delivered/not-delivered decision is the live-message probe,
/// not this offset; this range only bounds WHAT to send once the probe confirms
/// the message is still an undelivered placeholder.
pub(super) fn leak_recovery_unrelayed_range(
    full_response: &str,
    response_sent_offset: usize,
) -> Option<(usize, usize)> {
    let len = full_response.len();
    let mut start = response_sent_offset.min(len);
    while start > 0 && !full_response.is_char_boundary(start) {
        start -= 1;
    }
    if start >= len {
        None
    } else {
        Some((start, len))
    }
}

/// #2860 - pure render: format the unrelayed tail exactly as the bridge's
/// terminal-replace path would (strip TUI chrome, then status-panel or provider
/// formatting selected by the same flag). Returns `None` when the tail strips or
/// formats to empty - recovery must never post a placeholder or an empty notice,
/// only real leaked content.
pub(super) fn render_leak_recovery_delivery(
    full_response: &str,
    start: usize,
    status_panel_v2_enabled: bool,
    provider: &ProviderKind,
) -> Option<String> {
    let raw_tail = full_response.get(start..)?;
    let stripped = discord::response_sanitizer::strip_leading_tui_response_chrome(raw_tail);
    // Mirror terminal_delivery_response_after_offset: if the raw tail had content
    // but it was all chrome, there is nothing real to deliver.
    if !raw_tail.trim().is_empty() && stripped.trim().is_empty() {
        return None;
    }
    let rendered = if status_panel_v2_enabled {
        discord::formatting::format_for_discord_with_status_panel(&stripped, provider)
    } else {
        discord::formatting::format_for_discord_with_provider(&stripped, provider)
    };
    if rendered.trim().is_empty() {
        None
    } else {
        Some(rendered)
    }
}

pub(super) fn leak_recovery_chunk_fingerprints(chunks: &[String]) -> Vec<String> {
    chunks
        .iter()
        .enumerate()
        .map(|(index, chunk)| {
            let hash = blake3::hash(chunk.as_bytes());
            format!("{index}:{}", hash.to_hex())
        })
        .collect()
}

const LEAK_RECOVERY_CONTINUATION_SCAN_LIMIT: u8 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LeakRecoveryLedgerIdentity {
    pub(super) provider: String,
    pub(super) channel_id: u64,
    pub(super) current_msg_id: u64,
    pub(super) user_msg_id: u64,
    pub(super) byte_start: usize,
    pub(super) byte_end: usize,
    pub(super) chunk_fingerprints: Vec<String>,
}

impl LeakRecoveryLedgerIdentity {
    pub(super) fn new(
        provider: &ProviderKind,
        state: &discord::inflight::InflightTurnState,
        start: usize,
        end: usize,
        chunks: &[String],
    ) -> Self {
        Self {
            provider: provider.as_str().to_string(),
            channel_id: state.channel_id,
            current_msg_id: state.current_msg_id,
            user_msg_id: state.user_msg_id,
            byte_start: start,
            byte_end: end,
            chunk_fingerprints: leak_recovery_chunk_fingerprints(chunks),
        }
    }
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct LeakRecoveryChunkLedger {
    version: u32,
    provider: String,
    channel_id: u64,
    current_msg_id: u64,
    user_msg_id: u64,
    byte_start: usize,
    byte_end: usize,
    chunk_fingerprints: Vec<String>,
    confirmed_chunks: Vec<LeakRecoveryConfirmedChunk>,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
struct LeakRecoveryConfirmedChunk {
    index: usize,
    message_id: u64,
}

fn leak_recovery_chunk_ledger_root() -> Option<PathBuf> {
    crate::config::runtime_root().map(|root| {
        root.join("runtime")
            .join("discord_leak_recovery_chunk_ledgers")
    })
}

fn leak_recovery_chunk_ledger_path(identity: &LeakRecoveryLedgerIdentity) -> Option<PathBuf> {
    leak_recovery_chunk_ledger_root().map(|root| {
        root.join(&identity.provider)
            .join(identity.channel_id.to_string())
            .join(format!("{}.json", identity.current_msg_id))
    })
}

/// #3031(A) - stable, byte-boundary-INDEPENDENT identity gate.
///
/// The durable ledger's idempotency key is the turn coordinate
/// (`provider/channel_id/current_msg_id/user_msg_id`) plus `byte_start`, NOT the
/// *whole* response extent. We deliberately drop `byte_end` and the full
/// `chunk_fingerprints` array from this gate: a late post-terminal assistant
/// continuation grows `full_response`, which moves `byte_end` and appends chunk
/// fingerprints. Gating on those would invalidate the entire ledger and reset the
/// confirmed prefix to 0 (`.unwrap_or(0)`), risking a re-send of already-delivered
/// chunks. Per-chunk fingerprint equality is enforced separately and only for the
/// chunks the ledger actually claims as confirmed, so the confirmed prefix stays
/// immutable (monotonic) as the response grows.
fn leak_recovery_ledger_stable_identity_matches(
    ledger: &LeakRecoveryChunkLedger,
    identity: &LeakRecoveryLedgerIdentity,
) -> bool {
    ledger.version == 1
        && ledger.provider == identity.provider
        && ledger.channel_id == identity.channel_id
        && ledger.current_msg_id == identity.current_msg_id
        && ledger.user_msg_id == identity.user_msg_id
        && ledger.byte_start == identity.byte_start
}

/// #3031(A) - count the confirmed chunk prefix that is still valid against the
/// (possibly grown) live identity. A confirmed chunk only counts while its
/// ledger fingerprint still equals the current identity's fingerprint at the same
/// index; the first divergence (or a chunk index now beyond the live chunk count)
/// stops the prefix. This makes a longer `full_response` unable to LOWER the
/// confirmed prefix - appended tail chunks leave the confirmed prefix untouched,
/// while an actually-rewritten confirmed chunk still fails closed at that index.
fn leak_recovery_confirmed_prefix_against_identity(
    ledger: &LeakRecoveryChunkLedger,
    identity: &LeakRecoveryLedgerIdentity,
) -> usize {
    let mut expected = 0usize;
    for confirmed in &ledger.confirmed_chunks {
        if confirmed.index != expected || confirmed.message_id == 0 {
            break;
        }
        // The chunk must still exist in the live response and carry the same
        // fingerprint we recorded when we confirmed delivery. A grown response
        // keeps these prefix fingerprints byte-identical, so growth never trims
        // the prefix; a content rewrite at this index does.
        match (
            ledger.chunk_fingerprints.get(expected),
            identity.chunk_fingerprints.get(expected),
        ) {
            (Some(recorded), Some(live)) if recorded == live => {}
            _ => break,
        }
        expected += 1;
    }
    expected.min(identity.chunk_fingerprints.len())
}

pub(super) fn leak_recovery_confirmed_prefix_from_ledger(
    identity: &LeakRecoveryLedgerIdentity,
) -> Option<usize> {
    let path = leak_recovery_chunk_ledger_path(identity)?;
    let content = fs::read_to_string(path).ok()?;
    let ledger: LeakRecoveryChunkLedger = serde_json::from_str(&content).ok()?;
    if !leak_recovery_ledger_stable_identity_matches(&ledger, identity) {
        return None;
    }
    Some(leak_recovery_confirmed_prefix_against_identity(
        &ledger, identity,
    ))
}

pub(super) fn leak_recovery_record_confirmed_chunk(
    identity: &LeakRecoveryLedgerIdentity,
    chunk_index: usize,
    message_id: u64,
) -> Result<(), String> {
    if chunk_index >= identity.chunk_fingerprints.len() || message_id == 0 {
        return Err(format!(
            "invalid confirmed chunk index={chunk_index} message_id={message_id}"
        ));
    }
    let Some(path) = leak_recovery_chunk_ledger_path(identity) else {
        return Err("runtime root unavailable for leak recovery chunk ledger".to_string());
    };
    let mut confirmed_chunks = Vec::new();
    if let Ok(content) = fs::read_to_string(&path)
        && let Ok(existing) = serde_json::from_str::<LeakRecoveryChunkLedger>(&content)
        && leak_recovery_ledger_stable_identity_matches(&existing, identity)
    {
        // #3031(A): carry forward only the confirmed prefix that is STILL valid
        // against the current identity. A grown response keeps prefix fingerprints
        // identical (prefix preserved); a rewritten confirmed chunk trims the prefix
        // at the divergence so we never claim a stale delivery.
        let valid_prefix = leak_recovery_confirmed_prefix_against_identity(&existing, identity);
        confirmed_chunks = existing.confirmed_chunks;
        confirmed_chunks.retain(|chunk| chunk.index < valid_prefix);
    }

    confirmed_chunks.retain(|chunk| chunk.index < chunk_index);
    if confirmed_chunks.len() != chunk_index
        || confirmed_chunks
            .iter()
            .enumerate()
            .any(|(index, chunk)| chunk.index != index || chunk.message_id == 0)
    {
        return Err(format!(
            "cannot record non-contiguous leak recovery chunk {chunk_index}"
        ));
    }
    confirmed_chunks.push(LeakRecoveryConfirmedChunk {
        index: chunk_index,
        message_id,
    });
    let ledger = LeakRecoveryChunkLedger {
        version: 1,
        provider: identity.provider.clone(),
        channel_id: identity.channel_id,
        current_msg_id: identity.current_msg_id,
        user_msg_id: identity.user_msg_id,
        byte_start: identity.byte_start,
        byte_end: identity.byte_end,
        chunk_fingerprints: identity.chunk_fingerprints.clone(),
        confirmed_chunks,
    };
    let json = serde_json::to_string_pretty(&ledger)
        .map_err(|error| format!("serialize leak recovery chunk ledger: {error}"))?;
    discord::runtime_store::atomic_write(&path, &json)
}

pub(super) fn leak_recovery_clear_chunk_ledger(
    identity: &LeakRecoveryLedgerIdentity,
) -> Result<(), String> {
    let Some(path) = leak_recovery_chunk_ledger_path(identity) else {
        return Ok(());
    };
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "remove leak recovery chunk ledger {}: {error}",
            path.display()
        )),
    }
}

pub(super) fn leak_recovery_confirmed_chunk_count<'a>(
    current_message_content: &str,
    continuation_contents: impl IntoIterator<Item = &'a str>,
    chunks: &[String],
) -> Option<usize> {
    let first_chunk = chunks.first()?;
    if current_message_content != first_chunk {
        return None;
    }

    let mut confirmed = 1usize;
    for content in continuation_contents {
        if confirmed >= chunks.len() {
            break;
        }
        if content == chunks[confirmed] {
            confirmed += 1;
        }
    }
    Some(confirmed)
}

pub(super) async fn leak_recovery_fetch_continuation_contents(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    current_msg_id: MessageId,
    current_bot_user_id: u64,
) -> Option<Vec<(u64, String)>> {
    let messages = channel_id
        .messages(
            http.as_ref(),
            serenity::builder::GetMessages::new()
                .after(current_msg_id)
                .limit(LEAK_RECOVERY_CONTINUATION_SCAN_LIMIT),
        )
        .await
        .ok()?;

    Some(
        messages
            .into_iter()
            .filter(|msg| msg.author.id.get() == current_bot_user_id)
            .map(|msg| (msg.id.get(), msg.content))
            .collect(),
    )
}
