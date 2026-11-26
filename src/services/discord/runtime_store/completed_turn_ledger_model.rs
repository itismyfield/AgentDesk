//! The completed-turn ledger's file shape, path and read; the engine queue settles against it.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::services::discord::runtime_store;
use crate::services::provider::ProviderKind;

/// Sidecar subtree name — a sibling of `discord_delivery_records/`.
const COMPLETED_TURN_LEDGER_DIR: &str = "discord_completed_turn_ledger";

/// One completed turn: the inbound `user_msg_id` and when its terminal delivery
/// committed. `committed_at_epoch_ms` drives the retention prune.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletedTurnEntry {
    pub user_msg_id: u64,
    pub committed_at_epoch_ms: u64,
    /// The delivered episode, only when the delivery proved it; `None` never certifies an alias.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_nonce: Option<String>,
}

/// Episode `turn_nonce` of merged head `primary` absorbed `absorbed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergedAlias {
    pub primary: u64,
    pub turn_nonce: String,
    pub absorbed: Vec<u64>,
    pub claimed_at_epoch_ms: u64,
}

/// The durable per-channel ledger — a bounded ring of completed turns.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletedTurnLedger {
    #[serde(default)]
    pub entries: Vec<CompletedTurnEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merged_aliases: Vec<MergedAlias>,
}

/// The one alias join: `entry` is episode `alias.turn_nonce` of `alias.primary`; `None` never joins.
fn entry_backs(entry: &CompletedTurnEntry, alias: &MergedAlias) -> bool {
    entry.user_msg_id == alias.primary
        && entry.turn_nonce.as_deref() == Some(alias.turn_nonce.as_str())
}

impl CompletedTurnLedger {
    pub fn backs(&self, alias: &MergedAlias) -> bool {
        self.entries.iter().any(|entry| entry_backs(entry, alias))
    }

    /// `entries.ids ∪ { a ∈ alias.absorbed | entry(alias.primary, Some(alias.turn_nonce)) }`,
    /// from this one read.
    pub fn settled_ids(&self) -> HashSet<u64> {
        self.settled_commit_ms().into_keys().collect()
    }

    /// Commit time of each [`Self::settled_ids`] id: a direct id keeps its last-appended entry's
    /// time; an absorbed id takes its backing entry's, or the later of that and its own entry's.
    pub(in crate::services::discord) fn settled_commit_ms(&self) -> HashMap<u64, u64> {
        let mut commits: HashMap<u64, u64> = self
            .entries
            .iter()
            .map(|entry| (entry.user_msg_id, entry.committed_at_epoch_ms))
            .collect();
        for entry in &self.entries {
            let backed = self
                .merged_aliases
                .iter()
                .filter(|alias| entry_backs(entry, alias));
            for id in backed.flat_map(|alias| alias.absorbed.iter().copied()) {
                let commit = commits.entry(id).or_insert(entry.committed_at_epoch_ms);
                *commit = (*commit).max(entry.committed_at_epoch_ms);
            }
        }
        commits
    }

    /// The ids episode `turn_nonce` of `primary` absorbed, per its durable alias.
    pub fn absorbed_by_episode(&self, primary: u64, turn_nonce: &str) -> Vec<u64> {
        self.merged_aliases
            .iter()
            .filter(|alias| alias.primary == primary && alias.turn_nonce == turn_nonce)
            .flat_map(|alias| alias.absorbed.iter().copied())
            .collect()
    }
}

fn ledger_root() -> Option<PathBuf> {
    runtime_store::runtime_root().map(|root| root.join(COMPLETED_TURN_LEDGER_DIR))
}

/// `<runtime_root>/discord_completed_turn_ledger/<provider>/<channel_id>.json`.
pub fn ledger_path(provider: &ProviderKind, channel_id: u64) -> Option<PathBuf> {
    ledger_root().map(|root| {
        root.join(provider.as_str())
            .join(format!("{channel_id}.json"))
    })
}

/// Conservative read: missing OR malformed → `None`, never an error a caller
/// might misread as "settled". A torn/garbage file reads as no completed turns.
pub fn read_ledger_at(path: &Path) -> Option<CompletedTurnLedger> {
    let content = fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Read the durable ledger for `(provider, channel_id)`. `None` when the runtime
/// root is unavailable, the file is missing, or the content is malformed.
pub fn read_ledger(provider: &ProviderKind, channel_id: u64) -> Option<CompletedTurnLedger> {
    read_ledger_at(&ledger_path(provider, channel_id)?)
}

/// Commit time (Unix ms) of each settled inbound `user_msg_id`, merged-head aliases
/// included; empty when the ledger is absent or malformed, exactly like `settled_user_msg_ids`.
pub fn settled_commit_ms_by_user_msg_id(
    provider: &ProviderKind,
    channel_id: u64,
) -> HashMap<u64, u64> {
    read_ledger(provider, channel_id)
        .map(|ledger| ledger.settled_commit_ms())
        .unwrap_or_default()
}
