//! The thread an idle end keeps for its owner's next launch, stored apart from the owner's runtime
//! so an end with no runtime keeps it too; only a launch from that exact retired execution adopts it.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{EffortResume, live_source};
use crate::db::dispatched_sessions::hosted_execution::{HostedExecution, HostedOwner};

#[derive(Serialize, Deserialize)]
struct Kept {
    owner: HostedOwner,
    retired_nonce: String,
    native_session: String,
    operation: String,
    /// The end that stored it retired nothing, so no launch adopts it.
    cancelled: bool,
}

/// One record per channel: a later end on the channel replaces it, and a clear that knows only the
/// channel finds it.
fn path(provider: &str, token_hash: &str, channel_id: &str) -> Option<PathBuf> {
    let root = crate::config::runtime_root()?;
    let digest = Sha256::digest(serde_json::to_vec(&(provider, token_hash, channel_id)).ok()?);
    let name = format!("{}.json", hex::encode(&digest[..16]));
    Some(root.join("herdr_idle_resume").join(name))
}

fn path_of(owner: &HostedOwner) -> Option<PathBuf> {
    path(
        &owner.provider,
        &owner.discord_token_hash,
        &owner.channel_id,
    )
}

fn write(kept: &Kept) -> Result<(), String> {
    let path = path_of(&kept.owner).ok_or("no runtime root")?;
    std::fs::create_dir_all(path.parent().ok_or("no parent")?).map_err(|e| e.to_string())?;
    let text = serde_json::to_string(kept).map_err(|e| e.to_string())?;
    crate::services::discord::runtime_store::atomic_write(&path, &text)
}

fn read_at(path: &std::path::Path) -> Option<Kept> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn read(owner: &HostedOwner) -> Option<Kept> {
    read_at(&path_of(owner)?).filter(|kept| kept.owner == *owner)
}

/// Stores the thread the channel's live O cursor reads as `record`'s, which the end is about to
/// retire; answers the operation that names this record.
pub fn keep(record: &HostedExecution) -> Result<String, String> {
    let channel = record
        .owner
        .channel_id
        .parse::<u64>()
        .map_err(|e| e.to_string())?;
    let (_, source) = live_source(channel)?;
    let operation = uuid::Uuid::new_v4().simple().to_string();
    let kept = Kept {
        owner: record.owner.clone(),
        retired_nonce: record.execution_nonce.clone(),
        native_session: source.session_id,
        operation: operation.clone(),
        cancelled: false,
    };
    write(&kept)?;
    Ok(operation)
}

/// Cancels the record `operation` stored when its end retired nothing; another end's record stays.
pub fn forget(owner: &HostedOwner, operation: &str) {
    let Some(mut kept) = read(owner).filter(|kept| kept.operation == operation) else {
        return;
    };
    kept.cancelled = true;
    if let Err(error) = write(&kept) {
        tracing::warn!(%error, "herdr idle resume not cancelled");
    }
}

/// Cancels whatever thread an idle end kept for `owner`'s retired execution `nonce`, so a clear
/// leaves nothing for the next launch to resume; `Err` when that cancel did not land.
pub fn forbid(owner: &HostedOwner, nonce: &str) -> Result<(), String> {
    match read(owner).filter(|kept| !kept.cancelled && kept.retired_nonce == nonce) {
        Some(mut kept) => {
            kept.cancelled = true;
            write(&kept)
        }
        None => Ok(()),
    }
}

/// Cancels whatever thread an idle end kept on this channel, for any owner or execution, so a clear
/// that cannot name its execution still leaves the next launch fresh.
pub fn forbid_channel(provider: &str, token_hash: &str, channel_id: &str) -> Result<(), String> {
    let Some(path) = path(provider, token_hash, channel_id) else {
        return Ok(());
    };
    match read_at(&path).filter(|kept| !kept.cancelled) {
        Some(mut kept) => {
            kept.cancelled = true;
            write(&kept)
        }
        None => Ok(()),
    }
}

/// The resume an idle end kept for `owner`'s retired execution `nonce`.
pub(super) fn kept(owner: &HostedOwner, nonce: &str) -> Option<EffortResume> {
    let kept = read(owner).filter(|kept| !kept.cancelled && kept.retired_nonce == nonce)?;
    Some(EffortResume {
        operation: kept.operation,
        native_session: kept.native_session,
        retired_nonce: kept.retired_nonce,
        required: false,
    })
}
