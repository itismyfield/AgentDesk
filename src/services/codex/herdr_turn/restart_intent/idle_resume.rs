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
}

/// One record per owner and channel: a later end of the same owner replaces it.
fn path(owner: &HostedOwner) -> Option<PathBuf> {
    let root = crate::config::runtime_root()?;
    let digest = Sha256::digest(serde_json::to_vec(owner).ok()?);
    let name = format!("{}.json", hex::encode(&digest[..16]));
    Some(root.join("herdr_idle_resume").join(name))
}

fn read(owner: &HostedOwner) -> Option<Kept> {
    let kept: Kept = serde_json::from_slice(&std::fs::read(path(owner)?).ok()?).ok()?;
    (kept.owner == *owner).then_some(kept)
}

/// Stores the thread the channel's live O cursor reads as `record`'s, which the end is about to
/// retire; answers the operation that names this record.
pub(crate) fn keep(record: &HostedExecution) -> Result<String, String> {
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
    };
    let path = path(&record.owner).ok_or("no runtime root")?;
    std::fs::create_dir_all(path.parent().ok_or("no parent")?).map_err(|e| e.to_string())?;
    let text = serde_json::to_string(&kept).map_err(|e| e.to_string())?;
    crate::services::discord::runtime_store::atomic_write(&path, &text)?;
    Ok(operation)
}

/// Drops the record `operation` stored when its end retired nothing; another end's record stays.
pub(crate) fn forget(owner: &HostedOwner, operation: &str) {
    if read(owner).is_some_and(|kept| kept.operation == operation)
        && let Some(path) = path(owner)
        && let Err(error) = std::fs::remove_file(path)
    {
        tracing::warn!(%error, "herdr idle resume not dropped");
    }
}

/// The resume an idle end kept for `owner`'s retired execution `nonce`.
pub(super) fn kept(owner: &HostedOwner, nonce: &str) -> Option<EffortResume> {
    let kept = read(owner).filter(|kept| kept.retired_nonce == nonce)?;
    Some(EffortResume {
        operation: kept.operation,
        native_session: kept.native_session,
        retired_nonce: kept.retired_nonce,
        required: false,
    })
}
