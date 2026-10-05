//! Channels whose input moved to the ledger stop Legacy supervision and cleanup.
//! The set starts empty every boot and only grows; unmarked, every gate runs its Legacy body.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Mutex, OnceLock, RwLock};

type RetiredSet = RwLock<HashSet<(String, u64)>>;

static RETIRED: OnceLock<RetiredSet> = OnceLock::new();
static SKIPPED: OnceLock<Mutex<BTreeMap<&'static str, u64>>> = OnceLock::new();

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
        Self(key.0, key.1)
    }
}

#[cfg(test)]
impl Drop for RetiredForTest {
    fn drop(&mut self) {
        RETIRED
            .get()
            .unwrap()
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(std::mem::take(&mut self.0), self.1));
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
