//! Where a watcher's session runs, judged at watcher start from the `.host_kind` marker, the
//! Herdr admission map and the sessions row; only a Herdr judgement leaves main's tmux path.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use poise::serenity_prelude::ChannelId;

use crate::db::dispatched_sessions::hosted_execution::{HostedLookup, HostedRecord, HostedState};
use crate::services::discord::SharedData;
use crate::services::discord::host_key_derivation::derive_hosted_lookup;
use crate::services::provider::ProviderKind;
use crate::services::session_host::HostKind;
use crate::services::tmux_common::host_marker::{HostKindMarker, read_host_kind_marker};

/// How long one sessions-row read may hold a watcher start or a death.
const ROW_BUDGET: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WatchHost {
    /// A legacy row, no row, a retired record or no database: main's tmux path.
    Legacy,
    /// A row that could not be read or decoded: main's path, read once more before a death.
    Unverified,
    /// A Herdr marker, a listed Herdr pane or a pending or bound Herdr record.
    Herdr,
}

/// The host of `name` on the runtime's own key: local evidence first, then the sessions row.
async fn watch_host(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: u64,
    name: &str,
) -> WatchHost {
    if local_herdr(name) {
        return WatchHost::Herdr;
    }
    stored_host(shared, provider, channel_id, name).await
}

fn local_herdr(name: &str) -> bool {
    read_host_kind_marker(name) == HostKindMarker::Known(HostKind::Herdr)
        || crate::services::tui_prompt_dedupe::herdr_execution_listed(name)
}

async fn stored_host(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: u64,
    name: &str,
) -> WatchHost {
    let Some(pool) = shared.pg_pool.as_ref() else {
        return WatchHost::Legacy;
    };
    let hashes = [shared.token_hash.clone()];
    let lookup = derive_hosted_lookup(pool, &hashes, provider, channel_id, name);
    match tokio::time::timeout(ROW_BUDGET, lookup).await {
        Ok(lookup) => host_of(&lookup),
        Err(_) => WatchHost::Unverified,
    }
}

fn host_of(lookup: &HostedLookup) -> WatchHost {
    match lookup {
        HostedLookup::Found(found) => match &found.record {
            HostedRecord::Legacy => WatchHost::Legacy,
            HostedRecord::Known(record) if record.state == HostedState::Retired => {
                WatchHost::Legacy
            }
            HostedRecord::Known(_) => WatchHost::Herdr,
            HostedRecord::Unknown(_) => WatchHost::Unverified,
        },
        HostedLookup::Missing => WatchHost::Legacy,
        HostedLookup::Unknown(_) | HostedLookup::Conflict(_) => WatchHost::Unverified,
    }
}

/// One watcher's host, held by its task only; it changes only toward Herdr, or from
/// unverified to what a later row read says.
pub(crate) struct HostSnapshot(Mutex<WatchHost>);

impl HostSnapshot {
    pub(crate) fn new(host: WatchHost) -> Self {
        Self(Mutex::new(host))
    }

    /// The snapshot a watcher starts with for `name` on `channel_id`.
    pub(crate) async fn read(
        shared: &SharedData,
        provider: &ProviderKind,
        channel_id: ChannelId,
        name: &str,
    ) -> Arc<Self> {
        let host = watch_host(shared, provider, channel_id.get(), name).await;
        Arc::new(Self::new(host))
    }

    /// `read` of the pane on tmux; `None` off tmux, where the pane is unknown and never read.
    pub(crate) fn tmux_only<T>(&self, name: &str, read: impl FnOnce() -> T) -> Option<T> {
        if self.refresh_sync(name) == WatchHost::Herdr {
            tracing::debug!(name, "pane capture skipped: host is Herdr");
            return None;
        }
        Some(read())
    }

    fn get(&self) -> WatchHost {
        *self.0.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    /// Raises the snapshot to Herdr when a launch has since marked or listed `name` there.
    /// Reads the marker and the admission flag only; no lock is held while reading them.
    pub(crate) fn refresh_sync(&self, name: &str) -> WatchHost {
        if self.get() == WatchHost::Herdr || !local_herdr(name) {
            return self.get();
        }
        let mut host = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        *host = WatchHost::Herdr;
        *host
    }

    /// Whether a death tmux reported stands: an unverified host reads its row once more,
    /// and only a Herdr answer keeps the session.
    pub(crate) async fn death_stands(
        &self,
        shared: &SharedData,
        provider: &ProviderKind,
        channel_id: u64,
        name: &str,
    ) -> bool {
        if self.get() != WatchHost::Unverified {
            return self.get() != WatchHost::Herdr;
        }
        let read = stored_host(shared, provider, channel_id, name).await;
        let mut host = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if *host == WatchHost::Unverified {
            *host = read;
        }
        *host != WatchHost::Herdr
    }
}

#[cfg(test)]
#[path = "watch_host_tests.rs"]
mod tests;
