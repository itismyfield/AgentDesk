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

/// Caller holds the active row lock; different snapshots of an episode never overwrite each other.
pub(super) fn archive(
    provider: &ProviderKind,
    row: &InflightTurnState,
    row_json: &str,
    evidence: Value,
    recovery_notice: String,
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
