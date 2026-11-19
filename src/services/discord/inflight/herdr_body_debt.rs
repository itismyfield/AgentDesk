//! Immutable old-episode body snapshots, separate from the channel's active inflight row.

use super::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

mod reconcile;
mod store;
pub(in crate::services::discord) use reconcile::reconcile;

#[derive(Serialize, Deserialize)]
struct Record {
    version: u32,
    episode_key: String,
    provider: String,
    channel_id: u64,
    row_json: String,
    evidence: Value,
    recovery_notice: String,
    disposition: String,
}

/// Only a verified file and directory flush can create this clear witness.
pub(in crate::services::discord) struct Archived {
    path: PathBuf,
    row_sha256: String,
}

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn episode_key(row: &InflightTurnState) -> String {
    let key = serde_json::json!([
        row.provider,
        row.channel_id,
        row.request_owner_user_id,
        row.user_msg_id,
        row.turn_nonce,
        row.born_generation,
        row.started_at
    ]);
    sha(key.to_string().as_bytes())
}

fn directory(provider: &ProviderKind) -> Result<PathBuf, String> {
    super::super::runtime_store::runtime_root()
        .map(|root| root.join("discord_herdr_body_debt").join(provider.as_str()))
        .ok_or_else(|| "body debt runtime root unavailable".into())
}

impl Record {
    fn path(&self, directory: &Path, encoded: &str) -> PathBuf {
        directory.join(format!(
            "{}-{}-{}.json",
            self.channel_id,
            self.episode_key,
            sha(encoded.as_bytes())
        ))
    }

    fn row(&self) -> Result<InflightTurnState, String> {
        let row: InflightTurnState =
            serde_json::from_str(&self.row_json).map_err(|e| e.to_string())?;
        if self.version != 1
            || self.disposition != "retained"
            || row.provider != self.provider
            || row.channel_id != self.channel_id
            || row.turn_nonce.as_deref().is_none_or(str::is_empty)
            || episode_key(&row) != self.episode_key
        {
            return Err("body debt identity or format changed".into());
        }
        Ok(row)
    }
}

/// Caller holds the active row lock and, as `_held`, the adoption its eligibility was read from;
/// different snapshots of an episode never overwrite each other.
pub(super) fn archive(
    provider: &ProviderKind,
    row: &InflightTurnState,
    (row_json, evidence, recovery_notice): (&str, Value, String),
    _held: &crate::services::tui_o::cutover::Observed<'_>,
) -> Result<Archived, String> {
    let record = Record {
        version: 1,
        episode_key: episode_key(row),
        provider: provider.as_str().into(),
        channel_id: row.channel_id,
        row_json: row_json.into(),
        evidence,
        recovery_notice,
        disposition: "retained".into(),
    };
    record.row()?;
    let encoded = serde_json::to_string(&record).map_err(|e| e.to_string())?;
    let path = record.path(&directory(provider)?, &encoded);
    store::publish(&path, &encoded)?;
    store::read(&path, provider)?;
    Ok(Archived {
        path,
        row_sha256: sha(row_json.as_bytes()),
    })
}

/// A retained snapshot of a channel whose adoption was refused; its file goes only after the
/// reason intent and the debt deletion commit.
pub(in crate::services::discord) struct Retained {
    path: PathBuf,
    pub(in crate::services::discord) episode: String,
    pub(in crate::services::discord) row: InflightTurnState,
}

/// `channel`'s retained snapshots; an unreadable one stays for a later read.
pub(in crate::services::discord) fn retained(
    provider: &ProviderKind,
    channel: u64,
) -> Vec<Retained> {
    let Ok(entries) = directory(provider).and_then(|d| fs::read_dir(d).map_err(|e| e.to_string()))
    else {
        return Vec::new();
    };
    let mut kept = Vec::new();
    for path in entries.flatten().map(|entry| entry.path()) {
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        match store::read(&path, provider).and_then(|record| Ok((record.row()?, record))) {
            Ok((row, record)) if record.channel_id == channel => kept.push(Retained {
                path,
                episode: record.episode_key,
                row,
            }),
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "retained body debt needs a later read")
            }
        }
    }
    kept
}

impl Retained {
    /// Removes the snapshot; an already missing file counts as removed.
    pub(in crate::services::discord) fn remove(self) -> Result<(), String> {
        match fs::remove_file(&self.path) {
            Ok(()) => super::super::runtime_store::sync_removed(&self.path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        Ok(())
    }
}

impl Archived {
    pub(super) fn verify(&self, provider: &ProviderKind, row_json: &str) -> Result<(), String> {
        if sha(row_json.as_bytes()) != self.row_sha256 {
            return Err("active row advanced after body archive".into());
        }
        let record = store::read(&self.path, provider)?;
        if record.row_json != row_json {
            return Err("archived body no longer matches the active row".into());
        }
        Ok(())
    }
}
