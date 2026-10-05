//! Channels whose input moved to the ledger stop Legacy supervision and cleanup.
//! The set starts empty every boot and only grows; unmarked, every gate runs its Legacy body.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::Duration;

use super::transcript_turn::Presence;
use crate::services::provider::ProviderKind;

type RetiredSet = RwLock<HashSet<(String, u64)>>;

static RETIRED: OnceLock<RetiredSet> = OnceLock::new();
static SKIPPED: OnceLock<Mutex<BTreeMap<&'static str, u64>>> = OnceLock::new();
const ROW_STAT_BUDGET: Duration = Duration::from_secs(2);

/// True when `channel_id` is retired for `provider`; the caller then skips its Legacy
/// effect and the skip is logged and counted under `site`. Never locks an empty set.
pub(in crate::services::discord) fn legacy_retired(
    provider: &str,
    channel_id: u64,
    site: &'static str,
) -> bool {
    if !retired_in(&RETIRED, provider, channel_id) {
        return false;
    }
    *SKIPPED
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(site)
        .or_default() += 1;
    tracing::info!(
        provider,
        channel_id,
        site,
        "legacy supervision skipped a retired channel"
    );
    true
}

/// True when the channel is retired; unlike [`legacy_retired`] nothing is counted.
pub(in crate::services::discord) fn is_retired(provider: &str, channel_id: u64) -> bool {
    retired_in(&RETIRED, provider, channel_id)
}

/// Retired `(provider, channel)` pairs in order; an empty boot returns without locking.
pub(in crate::services::discord) fn retired_channels() -> Vec<(String, u64)> {
    channels_in(&RETIRED)
}

fn channels_in(cell: &OnceLock<RetiredSet>) -> Vec<(String, u64)> {
    let Some(set) = cell.get() else {
        return Vec::new();
    };
    let mut channels: Vec<_> = set
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect();
    #[cfg(test)]
    channels.retain(test_owner::visible);
    channels.sort();
    channels
}

/// Skipped Legacy effects counted by gate site since boot.
pub(in crate::services::discord) fn skipped_effects() -> BTreeMap<&'static str, u64> {
    SKIPPED.get().map_or_else(BTreeMap::new, |skipped| {
        skipped.lock().unwrap_or_else(|e| e.into_inner()).clone()
    })
}

/// Stats each channel's Legacy row in one blocking batch, read-only; a batch that misses
/// its budget leaves every row unknown.
pub(in crate::services::discord) async fn observe_rows(
    channels: &[(String, u64)],
) -> Vec<Presence> {
    let batch = channels.to_vec();
    let stat = tokio::task::spawn_blocking(move || {
        let root = super::super::runtime_store::discord_inflight_root();
        batch
            .iter()
            .map(|(provider, channel_id)| row_presence(root.as_deref(), provider, *channel_id))
            .collect::<Vec<_>>()
    });
    match tokio::time::timeout(ROW_STAT_BUDGET, stat).await {
        Ok(Ok(rows)) => rows,
        _ => vec![Presence::Unknown("row_stat_timeout"); channels.len()],
    }
}

fn row_presence(root: Option<&std::path::Path>, provider: &str, channel_id: u64) -> Presence {
    let Some(root) = root else {
        return Presence::Unknown("no_runtime_root");
    };
    let provider = ProviderKind::from_str_or_unsupported(provider);
    let path = super::super::inflight::inflight_state_path(root, &provider, channel_id);
    match std::fs::metadata(path) {
        Ok(_) => Presence::Present(1),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Presence::Absent,
        Err(_) => Presence::Unknown("row_stat_failed"),
    }
}

fn retired_in(cell: &OnceLock<RetiredSet>, provider: &str, channel_id: u64) -> bool {
    cell.get().is_some_and(|set| {
        set.read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&(provider.to_ascii_lowercase(), channel_id))
    })
}

/// Retires a channel for the guard's lifetime; production has no marking caller yet.
#[cfg(test)]
pub(in crate::services::discord) struct RetiredForTest(String, u64);

#[cfg(test)]
impl RetiredForTest {
    pub(in crate::services::discord) fn new(provider: &str, channel_id: u64) -> Self {
        let key = (provider.to_ascii_lowercase(), channel_id);
        RETIRED
            .get_or_init(RwLock::default)
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone());
        test_owner::claim(&key);
        Self(key.0, key.1)
    }
}

#[cfg(test)]
impl Drop for RetiredForTest {
    fn drop(&mut self) {
        test_owner::release(&(self.0.clone(), self.1));
        RETIRED
            .get()
            .unwrap()
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(std::mem::take(&mut self.0), self.1));
    }
}

/// Observers enumerate only the channels the current test thread retired, so a concurrent
/// fixture's channel never reaches another test's health snapshot.
#[cfg(test)]
mod test_owner {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};
    use std::thread::ThreadId;

    static OWNERS: LazyLock<Mutex<HashMap<(String, u64), ThreadId>>> =
        LazyLock::new(Mutex::default);

    pub(super) fn claim(key: &(String, u64)) {
        let owner = std::thread::current().id();
        OWNERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone(), owner);
    }

    pub(super) fn release(key: &(String, u64)) {
        OWNERS.lock().unwrap_or_else(|e| e.into_inner()).remove(key);
    }

    pub(super) fn visible(key: &(String, u64)) -> bool {
        OWNERS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .is_none_or(|owner| *owner == std::thread::current().id())
    }
}

#[cfg(test)]
pub(in crate::services::discord) fn skipped_for_test(site: &'static str) -> u64 {
    SKIPPED.get().map_or(0, |skipped| {
        skipped
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(site)
            .copied()
            .unwrap_or(0)
    })
}

#[cfg(test)]
pub(in crate::services::discord) mod test_support;

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty boot leaves the cell uninitialized, so a gate read takes no lock.
    #[test]
    fn empty_set_reads_without_initializing_or_locking() {
        let cell: OnceLock<RetiredSet> = OnceLock::new();
        assert!(!retired_in(&cell, "codex", 63_254_001));
        assert!(cell.get().is_none(), "a gate read must not create the set");
        assert!(channels_in(&cell).is_empty());
        assert!(
            cell.get().is_none(),
            "an observer read must not create the set"
        );
    }

    #[test]
    fn retired_channel_is_scoped_by_provider_and_counted() {
        let channel = 63_254_002;
        assert!(!legacy_retired("codex", channel, "test_site_scope"));
        let _retired = RetiredForTest::new("codex", channel);
        assert!(legacy_retired("Codex", channel, "test_site_scope"));
        assert!(!legacy_retired("claude", channel, "test_site_scope"));
        assert!(!legacy_retired("codex", channel + 1, "test_site_scope"));
        assert_eq!(skipped_for_test("test_site_scope"), 1);
    }

    /// Confirming a transcript turn mode (N1) does not retire Legacy supervision.
    #[test]
    fn transcript_confirmed_channel_is_not_retired() {
        let channel = 63_254_003;
        let _confirmed = crate::services::tui_o::turn_mode::TestConfirmation::new(channel);
        assert!(!legacy_retired("codex", channel, "test_site_confirmed"));
    }
}
