//! Channels whose input moved to the ledger stop Legacy supervision and cleanup.
//! The set starts empty every boot; only the queue cancel backstop still reads it.

use std::collections::HashSet;
use std::sync::{OnceLock, RwLock};

type RetiredSet = RwLock<HashSet<(String, u64)>>;

static RETIRED: OnceLock<RetiredSet> = OnceLock::new();

/// True when `channel_id` is retired for `provider`; the caller then skips its Legacy
/// effect and the skip is logged under `site`. Never locks an empty set.
pub(in crate::services::discord) fn legacy_retired(
    provider: &str,
    channel_id: u64,
    site: &'static str,
) -> bool {
    if !retired_in(&RETIRED, provider, channel_id) {
        return false;
    }
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
        let key = (std::mem::take(&mut self.0), self.1);
        RETIRED
            .get()
            .unwrap()
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&key);
    }
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
    fn retired_channel_is_scoped_by_provider() {
        let channel = 63_254_002;
        assert!(!legacy_retired("codex", channel, "test_site_scope"));
        let _retired = RetiredForTest::new("codex", channel);
        assert!(legacy_retired("Codex", channel, "test_site_scope"));
        assert!(!legacy_retired("claude", channel, "test_site_scope"));
        assert!(!legacy_retired("codex", channel + 1, "test_site_scope"));
    }

    /// Confirming a transcript turn mode (N1) does not retire Legacy supervision.
    #[test]
    fn transcript_confirmed_channel_is_not_retired() {
        let channel = 63_254_003;
        let _confirmed = crate::services::tui_o::turn_mode::TestConfirmation::new(channel);
        assert!(!legacy_retired("codex", channel, "test_site_confirmed"));
    }
}
