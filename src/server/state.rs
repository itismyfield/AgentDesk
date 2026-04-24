use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use tokio::sync::Mutex;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct MonitoringEntry {
    pub key: String,
    pub description: String,
    pub started_at: DateTime<Utc>,
    pub last_refresh: DateTime<Utc>,
}

#[derive(Debug, Default)]
pub struct MonitoringStore {
    entries: HashMap<u64, Vec<MonitoringEntry>>,
    rendered: HashMap<u64, u64>,
    render_versions: HashMap<u64, u64>,
}

impl MonitoringStore {
    pub fn upsert(&mut self, channel_id: u64, key: String, description: String) -> usize {
        let now = Utc::now();
        let entries = self.entries.entry(channel_id).or_default();
        if let Some(entry) = entries.iter_mut().find(|entry| entry.key == key) {
            entry.description = description;
            entry.last_refresh = now;
        } else {
            entries.push(MonitoringEntry {
                key,
                description,
                started_at: now,
                last_refresh: now,
            });
        }
        entries.len()
    }

    pub fn remove(&mut self, channel_id: u64, key: &str) -> usize {
        let Some(entries) = self.entries.get_mut(&channel_id) else {
            return 0;
        };
        entries.retain(|entry| entry.key != key);
        let active_count = entries.len();
        if active_count == 0 {
            self.entries.remove(&channel_id);
        }
        active_count
    }

    pub fn list(&self, channel_id: u64) -> Vec<MonitoringEntry> {
        match self.entries.get(&channel_id) {
            Some(entries) => entries.clone(),
            None => Vec::new(),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn sweep_expired(&mut self, ttl: Duration) -> Vec<u64> {
        self.sweep_expired_inner(ttl).emptied_channels
    }

    pub(crate) fn sweep_expired_affected(&mut self, ttl: Duration) -> Vec<u64> {
        self.sweep_expired_inner(ttl).affected_channels
    }

    pub fn set_rendered_msg(&mut self, channel_id: u64, msg_id: Option<u64>) {
        match msg_id {
            Some(msg_id) => {
                self.rendered.insert(channel_id, msg_id);
            }
            None => {
                self.rendered.remove(&channel_id);
            }
        }
    }

    pub fn get_rendered_msg(&self, channel_id: u64) -> Option<u64> {
        self.rendered.get(&channel_id).copied()
    }

    pub(crate) fn next_render_version(&mut self, channel_id: u64) -> u64 {
        let version = self.render_versions.entry(channel_id).or_insert(0);
        *version = version.saturating_add(1);
        *version
    }

    pub(crate) fn is_latest_render_version(&self, channel_id: u64, version: u64) -> bool {
        self.render_versions
            .get(&channel_id)
            .copied()
            .map_or(0, |value| value)
            == version
    }

    fn sweep_expired_inner(&mut self, ttl: Duration) -> SweepResult {
        let now = Utc::now();
        let mut affected_channels = Vec::new();
        let mut emptied_channels = Vec::new();
        let channel_ids = self.entries.keys().copied().collect::<Vec<_>>();

        for channel_id in channel_ids {
            let Some(entries) = self.entries.get_mut(&channel_id) else {
                continue;
            };
            let before = entries.len();
            entries.retain(|entry| now.signed_duration_since(entry.last_refresh) <= ttl);
            if entries.len() == before {
                continue;
            }

            affected_channels.push(channel_id);
            if entries.is_empty() {
                self.entries.remove(&channel_id);
                emptied_channels.push(channel_id);
            }
        }

        SweepResult {
            affected_channels,
            emptied_channels,
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
struct SweepResult {
    affected_channels: Vec<u64>,
    emptied_channels: Vec<u64>,
}

static GLOBAL_MONITORING_STORE: OnceLock<Arc<Mutex<MonitoringStore>>> = OnceLock::new();

pub fn global_monitoring_store() -> Arc<Mutex<MonitoringStore>> {
    GLOBAL_MONITORING_STORE
        .get_or_init(|| Arc::new(Mutex::new(MonitoringStore::default())))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_adds_and_refreshes_entries() {
        let mut store = MonitoringStore::default();

        assert_eq!(
            store.upsert(10, "one".to_string(), "waiting".to_string()),
            1
        );
        let first = store.list(10).remove(0);

        assert_eq!(
            store.upsert(10, "one".to_string(), "updated".to_string()),
            1
        );
        let updated = store.list(10).remove(0);

        assert_eq!(updated.description, "updated");
        assert_eq!(updated.started_at, first.started_at);
        assert!(updated.last_refresh >= first.last_refresh);
    }

    #[test]
    fn remove_clears_entries_without_losing_rendered_message() {
        let mut store = MonitoringStore::default();
        store.upsert(10, "one".to_string(), "waiting".to_string());
        store.set_rendered_msg(10, Some(99));

        assert_eq!(store.remove(10, "one"), 0);
        assert!(store.list(10).is_empty());
        assert_eq!(store.get_rendered_msg(10), Some(99));
    }

    #[test]
    fn rendered_message_can_be_set_and_cleared() {
        let mut store = MonitoringStore::default();

        assert_eq!(store.get_rendered_msg(10), None);
        store.set_rendered_msg(10, Some(42));
        assert_eq!(store.get_rendered_msg(10), Some(42));
        store.set_rendered_msg(10, None);
        assert_eq!(store.get_rendered_msg(10), None);
    }

    #[test]
    fn sweep_expired_removes_old_entries_and_reports_empty_channels() -> Result<(), String> {
        let mut store = MonitoringStore::default();
        store.upsert(10, "old".to_string(), "old wait".to_string());
        store.upsert(11, "fresh".to_string(), "fresh wait".to_string());
        let entries = store
            .entries
            .get_mut(&10)
            .ok_or_else(|| "missing channel 10 entry".to_string())?;
        entries[0].last_refresh = Utc::now() - Duration::minutes(11);

        let emptied = store.sweep_expired(Duration::minutes(10));

        assert_eq!(emptied, vec![10]);
        assert!(store.list(10).is_empty());
        assert_eq!(store.list(11).len(), 1);
        Ok(())
    }

    #[test]
    fn sweep_expired_affected_reports_partial_channel_updates() -> Result<(), String> {
        let mut store = MonitoringStore::default();
        store.upsert(10, "old".to_string(), "old wait".to_string());
        store.upsert(10, "fresh".to_string(), "fresh wait".to_string());
        let entries = store
            .entries
            .get_mut(&10)
            .ok_or_else(|| "missing channel 10 entries".to_string())?;
        entries[0].last_refresh = Utc::now() - Duration::minutes(11);

        let affected = store.sweep_expired_affected(Duration::minutes(10));

        assert_eq!(affected, vec![10]);
        assert_eq!(store.list(10).len(), 1);
        assert_eq!(store.list(10)[0].key, "fresh");
        Ok(())
    }
}
