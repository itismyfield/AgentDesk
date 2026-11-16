//! Immutable C1 snapshots become shared plans; neither reading nor registration grants POST.
use super::pieces::PieceWork;
use crate::services::tui_o::repost::identity::SPLIT_VERSION;
use crate::services::tui_o::shadow::{IDENTITY_VERSION, ShadowProvider, SourceRange};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
mod replay;
#[cfg(test)]
#[path = "herdr_body_debt/debt_tests.rs"]
pub(crate) mod tests;

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn provider_name(provider: ShadowProvider) -> &'static str {
    match provider {
        ShadowProvider::Claude => "claude",
        ShadowProvider::Codex => "codex",
    }
}
fn directory(root: &Path, provider: ShadowProvider) -> PathBuf {
    root.join("runtime")
        .join("discord_herdr_body_debt")
        .join(provider_name(provider))
}
#[derive(Deserialize)]
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
struct Snapshot {
    sha: String,
    bytes: String,
    record: Record,
    row: Value,
}
impl Snapshot {
    fn read(path: &Path, provider: ShadowProvider) -> Result<Self, String> {
        let bytes = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        let record: Record = serde_json::from_str(&bytes).map_err(|e| e.to_string())?;
        let row: Value = serde_json::from_str(&record.row_json).map_err(|e| e.to_string())?;
        let episode = json!([
            row["provider"],
            row["channel_id"],
            row["request_owner_user_id"],
            row["user_msg_id"],
            row["turn_nonce"],
            row.get("born_generation").unwrap_or(&json!(0)),
            row["started_at"]
        ]);
        let digest = sha(bytes.as_bytes());
        let expected = format!(
            "{}-{}-{}.json",
            record.channel_id, record.episode_key, digest
        );
        let runtime = match provider {
            ShadowProvider::Claude => "claude_tui",
            ShadowProvider::Codex => "codex_tui",
        };
        if record.version != 1
            || record.disposition != "retained"
            || record.channel_id == 0
            || record.provider != provider_name(provider)
            || row["provider"] != record.provider
            || row["channel_id"] != record.channel_id
            || path.file_name().and_then(|n| n.to_str()) != Some(&expected)
            || record.episode_key != sha(episode.to_string().as_bytes())
            || row["turn_nonce"]
                .as_str()
                .is_none_or(|s| s.trim().is_empty())
            || row["started_at"]
                .as_str()
                .is_none_or(|s| s.trim().is_empty())
            || row["request_owner_user_id"].as_u64().is_none()
            || row["user_msg_id"].as_u64().is_none()
            || row["full_response"].as_str().is_none()
            || row["response_sent_offset"].as_u64().is_none()
            || row["runtime_kind"] != runtime
            || row.pointer("/host_locator/host_kind") != Some(&json!("herdr"))
            || ["host_session_id", "pane"].iter().any(|key| {
                row["host_locator"][key]
                    .as_str()
                    .is_none_or(|s| s.trim().is_empty())
            })
        {
            return Err("invalid C1 body debt identity or immutable digest".into());
        }
        if row
            .get("born_generation")
            .is_some_and(|v| v.as_u64().is_none())
            || row
                .get("intake_outbox_id")
                .is_some_and(|v| !v.is_null() && v.as_i64().is_none_or(|id| id <= 0))
            || row.get("source_message_ids").is_some_and(|v| {
                v.as_array()
                    .is_none_or(|ids| ids.iter().any(|id| id.as_u64().is_none()))
            })
            || row.get("input_origin_ids").is_some_and(|v| {
                v.as_array().is_none_or(|ids| {
                    ids.iter()
                        .any(|id| id.as_str().is_none_or(|s| s.trim().is_empty()))
                })
            })
        {
            return Err("invalid original input identity".into());
        }
        Ok(Self {
            sha: digest,
            bytes,
            record,
            row,
        })
    }
}

/// Only the validated reader can construct this input to shared registration.
#[derive(Debug)]
pub struct Plan {
    provider: ShadowProvider,
    channel: u64,
    episode: String,
    input_key: String,
    plan_sha: String,
    snapshots: BTreeMap<String, Value>,
    pieces: Vec<PieceWork>,
    native: Vec<(PieceWork, SourceRange)>,
    held: Option<&'static str>,
}
impl Plan {
    fn from(
        snapshot: Snapshot,
        provider: ShadowProvider,
        ledger: Option<&crate::services::tui_o::store::ledger::LedgerState>,
    ) -> Self {
        let input = json!([
            snapshot.row["request_owner_user_id"],
            snapshot.row["user_msg_id"],
            snapshot.row["turn_nonce"]
        ]);
        let (pieces, native, held) = replay::plan(&snapshot, provider, ledger);
        let plan_sha = sha(json!([
            IDENTITY_VERSION,
            SPLIT_VERSION,
            pieces
                .iter()
                .map(|p| json!([p.unit_key, p.index, p.payload]))
                .collect::<Vec<_>>()
        ])
        .to_string()
        .as_bytes());
        let mut plan = Self {
            provider,
            channel: snapshot.record.channel_id,
            episode: snapshot.record.episode_key.clone(),
            input_key: sha(input.to_string().as_bytes()),
            plan_sha,
            snapshots: BTreeMap::new(),
            pieces,
            native,
            held,
        };
        plan.include(snapshot);
        plan
    }
    fn include(&mut self, snapshot: Snapshot) {
        self.snapshots.insert(
            snapshot.sha,
            json!({"bytes": snapshot.bytes,
            "row_json": snapshot.record.row_json, "evidence": snapshot.record.evidence,
            "recovery_notice": snapshot.record.recovery_notice}),
        );
    }
    fn merge(&mut self, other: Self) {
        // A changed plan or contradictory original intake is not another send budget.
        let admissions = |snapshots: &BTreeMap<String, Value>| -> Vec<Value> {
            snapshots
                .values()
                .filter_map(|s| serde_json::from_str::<Value>(s["row_json"].as_str()?).ok())
                .filter_map(|row| {
                    row.get("intake_outbox_id")
                        .filter(|v| !v.is_null())
                        .cloned()
                })
                .collect()
        };
        let mut ids = admissions(&self.snapshots);
        ids.extend(admissions(&other.snapshots));
        if self.input_key != other.input_key
            || self.plan_sha != other.plan_sha
            || self.held.is_some()
            || other.held.is_some()
            || ids.windows(2).any(|ids| ids[0] != ids[1])
        {
            self.held = Some("ambiguous snapshot/input overlap");
        }
        self.snapshots.extend(other.snapshots);
        for alias in other.native {
            if !self.native.contains(&alias) {
                self.native.push(alias);
            }
        }
    }
    pub(crate) fn channel(&self) -> u64 {
        self.channel
    }
    pub(crate) fn registration(&self, bot: u64) -> Result<Value, String> {
        if let Some(reason) = self.held {
            return Err(reason.into());
        }
        if bot == 0 || self.pieces.is_empty() {
            return Err("body debt has no verified sender or body plan".into());
        }
        Ok(
            json!({"version": 1, "provider": provider_name(self.provider), "channel": self.channel.to_string(),
            "episode": self.episode, "input_key": self.input_key, "plan_sha": self.plan_sha,
            "snapshots": self.snapshots, "native": self.native.iter().map(|(p,r)| json!({"unit_key":p.unit_key,"piece_index":p.index,"payload":p.payload,"payload_sha":sha(p.payload.as_bytes()),"output_origin":p.output_origin,"coordinate":r})).collect::<Vec<_>>(),
            "effects": self.pieces.iter().map(|p| json!({"coverage": format!("body:v{IDENTITY_VERSION}:s{SPLIT_VERSION}:{}",p.index),
                "payload": p.payload, "payload_sha": sha(p.payload.as_bytes()), "unit_key": p.unit_key,
                "piece_index": p.index, "bot": bot})).collect::<Vec<_>>() }),
        )
    }
}

/// No active-row lookup: C1's retired row is retained inside the exact snapshot bytes.
pub(crate) fn read(
    root: &Path,
    provider: ShadowProvider,
    channel: u64,
    ledger: Option<&crate::services::tui_o::store::ledger::LedgerState>,
) -> Result<Vec<Plan>, String> {
    let mut episodes: BTreeMap<String, Plan> = BTreeMap::new();
    let entries = match std::fs::read_dir(directory(root, provider)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    for entry in entries {
        let path = entry.map_err(|e| e.to_string())?.path();
        let prefix = format!("{channel}-");
        if path.extension().is_none_or(|ext| ext != "json")
            || !path
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.starts_with(&prefix))
        {
            continue;
        }
        let snapshot = Snapshot::read(&path, provider)?;
        let plan = Plan::from(snapshot, provider, ledger);
        match episodes.get_mut(&plan.episode) {
            Some(old) => old.merge(plan),
            None => {
                episodes.insert(plan.episode.clone(), plan);
            }
        }
    }
    Ok(episodes.into_values().collect())
}

/// Boot wake has no effect on unrelated channels and does not imply a body receipt.
pub(crate) fn wake_channels(root: &Path, provider: ShadowProvider, mut wake: impl FnMut(u64)) {
    let Ok(entries) = std::fs::read_dir(directory(root, provider)) else {
        return;
    };
    let mut channels = std::collections::BTreeSet::new();
    for entry in entries.flatten() {
        if entry.path().extension().is_some_and(|ext| ext == "json") {
            match Snapshot::read(&entry.path(), provider) {
                Ok(snapshot) => {
                    channels.insert(snapshot.record.channel_id);
                }
                Err(error) => tracing::warn!(%error, "retained herdr debt needs a later read"),
            }
        }
    }
    for channel in channels {
        wake(channel);
    }
}
