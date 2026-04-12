use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::runtime_store::{atomic_write, discord_handoff_root};
use crate::services::provider::ProviderKind;

const HANDOFF_VERSION: u32 = 1;

/// Maximum age for a handoff file before it is considered stale and removed.
const HANDOFF_MAX_AGE_SECS: u64 = 600; // 10 minutes

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct HandoffRecord {
    pub version: u32,
    pub provider: String,
    pub channel_id: u64,
    #[serde(default)]
    pub channel_name: Option<String>,
    /// What the agent intended to do after restart (e.g. "verify code changes").
    pub intent: String,
    /// Context summary carried from the pre-restart turn.
    pub context: String,
    /// Dedupe key to prevent the same handoff from executing twice.
    pub dedupe_key: String,
    /// Generation at which this handoff was created.
    pub born_generation: u64,
    /// Lifecycle: created → executing → completed | skipped | failed
    pub state: String,
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    /// The user message ID from the original turn (for reaction management).
    #[serde(default)]
    pub user_msg_id: Option<u64>,
    /// The current working directory for the follow-up turn.
    #[serde(default)]
    pub current_path: Option<String>,
}

impl HandoffRecord {
    pub fn new(
        provider: &ProviderKind,
        channel_id: u64,
        channel_name: Option<String>,
        intent: impl Into<String>,
        context: impl Into<String>,
        current_path: Option<String>,
        user_msg_id: Option<u64>,
    ) -> Self {
        let now = chrono::Local::now();
        let generation = super::runtime_store::load_generation();
        Self {
            version: HANDOFF_VERSION,
            provider: provider.as_str().to_string(),
            channel_id,
            channel_name,
            intent: intent.into(),
            context: context.into(),
            dedupe_key: format!("{}-{}-{}", provider.as_str(), channel_id, generation),
            born_generation: generation,
            state: "created".to_string(),
            created_at: now.format("%Y-%m-%d %H:%M:%S").to_string(),
            updated_at: now.format("%Y-%m-%d %H:%M:%S").to_string(),
            user_msg_id,
            current_path,
        }
    }

    pub fn provider_kind(&self) -> Option<ProviderKind> {
        ProviderKind::from_str(&self.provider)
    }
}

// ─── File operations ─────────────────────────────────────────────────────────

fn handoff_provider_dir(root: &Path, provider: &ProviderKind) -> PathBuf {
    root.join(provider.as_str())
}

fn handoff_path(root: &Path, provider: &ProviderKind, channel_id: u64) -> PathBuf {
    handoff_provider_dir(root, provider).join(format!("{channel_id}.json"))
}

pub(super) fn save_handoff(record: &HandoffRecord) -> Result<(), String> {
    let Some(root) = discord_handoff_root() else {
        return Err("Home directory not found".to_string());
    };
    let Some(provider) = record.provider_kind() else {
        return Err(format!("Unknown provider '{}'", record.provider));
    };
    let path = handoff_path(&root, &provider, record.channel_id);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut updated = record.clone();
    updated.updated_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let json = serde_json::to_string_pretty(&updated).map_err(|e| e.to_string())?;
    atomic_write(&path, &json)?;
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 📎 Saved handoff for channel {} (intent: {})",
        record.channel_id,
        truncate(&record.intent, 60)
    );
    Ok(())
}

pub(super) fn load_handoffs(provider: &ProviderKind) -> Vec<HandoffRecord> {
    let Some(root) = discord_handoff_root() else {
        return Vec::new();
    };
    let dir = handoff_provider_dir(&root, provider);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut records = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // Remove stale handoff files
        if let Ok(meta) = fs::metadata(&path) {
            if let Ok(modified) = meta.modified() {
                if let Ok(age) = modified.elapsed() {
                    if age > Duration::from_secs(HANDOFF_MAX_AGE_SECS) {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 🗑 Removing stale handoff file: {} (age={:.0}s)",
                            path.display(),
                            age.as_secs_f64()
                        );
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                }
            }
        }
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(record) = serde_json::from_str::<HandoffRecord>(&content) else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ Removing malformed handoff file: {}",
                path.display()
            );
            let _ = fs::remove_file(&path);
            continue;
        };
        if record.provider_kind().as_ref() != Some(provider) {
            continue;
        }
        records.push(record);
    }
    records
}

pub(super) fn clear_handoff(provider: &ProviderKind, channel_id: u64) {
    let Some(root) = discord_handoff_root() else {
        return;
    };
    let path = handoff_path(&root, provider, channel_id);
    let _ = fs::remove_file(path);
}

pub(super) fn update_handoff_state(
    provider: &ProviderKind,
    channel_id: u64,
    new_state: &str,
) -> Result<(), String> {
    let Some(root) = discord_handoff_root() else {
        return Err("Home directory not found".to_string());
    };
    let path = handoff_path(&root, provider, channel_id);
    let content = fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut record: HandoffRecord = serde_json::from_str(&content).map_err(|e| e.to_string())?;
    record.state = new_state.to_string();
    record.updated_at = chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let json = serde_json::to_string_pretty(&record).map_err(|e| e.to_string())?;
    atomic_write(&path, &json)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_save_and_load_handoff() {
        let temp = TempDir::new().unwrap();
        let provider = ProviderKind::Claude;
        let record = HandoffRecord {
            version: 1,
            provider: "claude".to_string(),
            channel_id: 123,
            channel_name: Some("adk-cc".to_string()),
            intent: "verify code changes".to_string(),
            context: "modified src/main.rs".to_string(),
            dedupe_key: "claude-123-5".to_string(),
            born_generation: 5,
            state: "created".to_string(),
            created_at: "2026-03-15 10:00:00".to_string(),
            updated_at: "2026-03-15 10:00:00".to_string(),
            user_msg_id: Some(999),
            current_path: Some("/Users/test/repo".to_string()),
        };

        let path = handoff_path(temp.path(), &provider, 123);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let json = serde_json::to_string_pretty(&record).unwrap();
        fs::write(&path, &json).unwrap();

        let loaded = load_handoffs_from_root(temp.path(), &provider);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].channel_id, 123);
        assert_eq!(loaded[0].intent, "verify code changes");
        assert_eq!(loaded[0].state, "created");
    }

    fn load_handoffs_from_root(root: &Path, provider: &ProviderKind) -> Vec<HandoffRecord> {
        let dir = handoff_provider_dir(root, provider);
        let Ok(entries) = fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut records = Vec::new();
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(record) = serde_json::from_str::<HandoffRecord>(&content) else {
                continue;
            };
            records.push(record);
        }
        records
    }
}
