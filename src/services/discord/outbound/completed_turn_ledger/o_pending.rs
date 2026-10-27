//! Captured O input context survives row retirement; only whole-episode proof completes it.

use super::super::{
    o_delivery_evidence,
    o_delivery_manifest::{self as manifest, Manifest, Query},
    o_writer_io::GatewayPort,
};
use super::*;
use crate::services::discord::{SharedData, inflight::InflightTurnState};
use crate::services::tui_o::{cutover, store::delivery_evidence};
use tokio::time::Instant;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Pending {
    version: u8,
    id: u64,
    nonce: String,
    session: Option<String>,
    path: Option<PathBuf>,
    file: Option<(u64, u64)>,
    start: Option<u64>,
    input: String,
    body: String,
    captured_ms: u64,
    manifest: Option<Manifest>,
}

fn path(provider: &ProviderKind, channel: u64) -> Option<PathBuf> {
    Some(
        runtime_store::runtime_root()?
            .join("discord_pending_o_deliveries")
            .join(provider.as_str())
            .join(format!("{channel}.json")),
    )
}

fn read(path: &Path) -> Option<Vec<Pending>> {
    if !path.exists() {
        return Some(Vec::new());
    }
    let pending: Vec<Pending> = delivery_evidence::json(path, 8 * 1024 * 1024)?;
    pending.iter().all(|p| p.version == 1).then_some(pending)
}

fn save(path: &Path, pending: &[Pending]) -> Option<()> {
    let data = serde_json::to_string(pending).ok()?;
    if data.len() > 8 * 1024 * 1024 {
        return None;
    }
    runtime_store::atomic_write(path, &data).ok()?;
    runtime_store::fsync_parent_and_ancestors(path).ok()?;
    Some(())
}

fn build(
    provider: &ProviderKind,
    channel: u64,
    captured: &Pending,
    deadline: Instant,
) -> Option<Manifest> {
    let root = crate::config::runtime_root()?;
    let path = captured.path.as_ref()?;
    let end = std::fs::metadata(path).ok()?.len();
    let plan = manifest::build(
        &root,
        Query {
            provider,
            channel,
            path,
            file: captured.file?,
            range: (captured.start?, end),
        },
        deadline,
    )?;
    if !plan.completed
        || plan.source.session_id != *captured.session.as_ref()?
        || plan.input.as_deref() != Some(captured.input.as_str())
        || (!captured.body.is_empty() && !plan.body.starts_with(&captured.body))
    {
        return None;
    }
    Some(plan)
}

pub(in crate::services::discord) fn capture(provider: &ProviderKind, row: &InflightTurnState) {
    if row.user_msg_id == 0
        || row.silent_turn
        || crate::services::discord::is_synthetic_headless_message_id_raw(row.user_msg_id)
        || row.tui_terminal_kind
            == Some(crate::services::agent_protocol::NativeTerminalKind::Aborted)
        || cutover::peek_o_owns_tui_output_for_channel(row.channel_id, row.runtime_kind) != Ok(true)
    {
        return;
    }
    let Some(nonce) = row.turn_nonce.as_ref().filter(|n| !n.is_empty()) else {
        return;
    };
    let Some(path) = path(provider, row.channel_id) else {
        return;
    };
    let file = row.tui_terminal_source_file_identity.or_else(|| {
        let file = std::fs::File::open(row.output_path.as_ref()?).ok()?;
        Some(crate::services::tui_o::shadow::capture::file_identity(
            &file.metadata().ok()?,
        ))
    });
    let mut captured = Pending {
        version: 1,
        id: row.user_msg_id,
        nonce: nonce.clone(),
        session: row.session_id.clone(),
        path: row.output_path.as_ref().map(PathBuf::from),
        file,
        start: row.turn_start_offset,
        input: row.user_text.clone(),
        body: row.full_response.clone(),
        captured_ms: now_epoch_ms(),
        manifest: None,
    };
    captured.manifest = build(
        provider,
        row.channel_id,
        &captured,
        Instant::now() + std::time::Duration::from_millis(250),
    );
    let publish = || -> Option<()> {
        fs::create_dir_all(path.parent()?).ok()?;
        let _lock = delivery_record::lock_record_path(&path).ok()?;
        let mut pending = read(&path)?;
        pending.retain(|p| now_epoch_ms().saturating_sub(p.captured_ms) <= LEDGER_RETENTION_MS);
        if let Some(old) = pending
            .iter_mut()
            .find(|p| p.id == captured.id && p.nonce == captured.nonce)
        {
            if old.manifest.is_none()
                && old.path == captured.path
                && old.file == captured.file
                && old.start == captured.start
                && old.input == captured.input
            {
                old.manifest = captured.manifest;
            }
        } else {
            pending.push(captured);
        }
        if pending.len() > LEDGER_ENTRY_CAP {
            pending.drain(..pending.len() - LEDGER_ENTRY_CAP);
        }
        save(&path, &pending)
    };
    if publish().is_none() {
        tracing::warn!(
            channel_id = row.channel_id,
            "O input context capture unconfirmed"
        );
    }
}

fn insert(ledger: &mut CompletedTurnLedger, pending: &Pending, posted_ms: u64) {
    if let Some(entry) = ledger.entries.iter_mut().find(|e| {
        e.user_msg_id == pending.id && e.turn_nonce.as_deref() == Some(pending.nonce.as_str())
    }) {
        entry.committed_at_epoch_ms = entry.committed_at_epoch_ms.max(posted_ms);
    } else {
        ledger.entries.push(CompletedTurnEntry {
            user_msg_id: pending.id,
            turn_nonce: Some(pending.nonce.clone()),
            committed_at_epoch_ms: posted_ms,
        });
    }
    // The consumer takes the last entry per id and the cap removes the oldest first.
    ledger
        .entries
        .sort_by_key(|entry| entry.committed_at_epoch_ms);
}

/// Unknowns stay durable until another eligible projection call; quiet sweeps need not call here.
pub(in crate::services::discord) async fn project(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: u64,
) {
    let Some(path) = path(provider, channel) else {
        return;
    };
    let Some(snapshot) = read(&path).filter(|p| !p.is_empty()) else {
        return;
    };
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        return;
    };
    let deadline = Instant::now() + std::time::Duration::from_secs(10);
    let Ok(Ok(bot)) = tokio::time::timeout_at(deadline, http.get_current_user()).await else {
        return;
    };
    let port = GatewayPort::new(http, bot.id.get());
    let Some(root) = crate::config::runtime_root() else {
        return;
    };
    for captured in snapshot {
        if Instant::now() >= deadline {
            break;
        }
        let plan = captured
            .manifest
            .clone()
            .or_else(|| build(provider, channel, &captured, deadline));
        let Some(plan) = plan else {
            continue;
        };
        let proof =
            o_delivery_evidence::episode(&root, shared.pg_pool.as_ref(), &port, &plan, deadline)
                .await;
        let publish = || -> Option<()> {
            let _lock = delivery_record::lock_record_path(&path).ok()?;
            let mut pending = read(&path)?;
            let position = pending.iter().position(|p| *p == captured)?;
            let Some(proof) = proof else {
                pending[position].manifest = Some(plan);
                return save(&path, &pending);
            };
            let ledger_path = ledger_path(provider, channel)?;
            let outcome = mutate_at(&ledger_path, now_epoch_ms(), |ledger| {
                insert(ledger, &captured, proof.posted_ms)
            });
            if !matches!(outcome, Ok(Publication::Published(ParentSync::Flushed))) {
                return None;
            }
            pending.remove(position);
            save(&path, &pending)
        };
        if publish().is_none() {
            tracing::debug!(
                channel_id = channel,
                "O completion projection retained for retry"
            );
        }
    }
}

#[cfg(test)]
#[path = "o_pending_tests.rs"]
mod tests;
