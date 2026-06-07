//! In-memory monitoring store for per-channel activity banners.
//!
//! This is pure in-memory state (no axum / no route dependency): it tracks the
//! set of active monitoring entries per Discord channel plus the rendered
//! message id and render-version bookkeeping used to coalesce banner updates.
//! It previously lived in `crate::server::state` (re-exported as
//! `crate::server::routes::state`); per #3037 bucket 3 it now lives beside the
//! rest of the service-layer infra. Route-layer callers keep the
//! `crate::server::routes::state::*` path through a re-export facade.

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

    /// Snapshot of every channel that currently has at least one monitoring
    /// entry. Used by `disk_monitor::run_disk_monitor_tick_once` to fan a
    /// disk-space banner out to all already-active channels (idle channels
    /// stay quiet and rely on `/api/health` for the same signal).
    pub fn tracked_channel_ids(&self) -> Vec<u64> {
        self.entries.keys().copied().collect()
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
