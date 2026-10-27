use std::collections::HashMap;

use super::{ChannelId, ChannelMailboxRegistry, ChannelMailboxSnapshot};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MailboxObservationFailure {
    Missing,
    Unreachable,
}

impl MailboxObservationFailure {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Unreachable => "unreachable",
        }
    }
}

#[cfg(all(test, unix))]
thread_local! {
    static ENUMERATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Counts a whole-registry read, so a test can tell how often one pass enumerated actors.
fn count_enumeration() {
    #[cfg(all(test, unix))]
    ENUMERATIONS.with(|count| count.set(count.get() + 1));
}

impl ChannelMailboxRegistry {
    /// Whole-registry reads this thread has made through `snapshot_all` or the health read.
    #[cfg(all(test, unix))]
    pub(crate) fn enumerations_for_tests() -> usize {
        ENUMERATIONS.with(std::cell::Cell::get)
    }

    pub(crate) async fn snapshot_all(&self) -> HashMap<ChannelId, ChannelMailboxSnapshot> {
        count_enumeration();
        let handles: Vec<_> = self
            .handles
            .iter()
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect();
        let mut snapshots = HashMap::new();
        for (channel_id, handle) in handles {
            snapshots.insert(channel_id, handle.snapshot().await);
        }
        snapshots
    }

    /// Observe existing actors and required channels without creating actors or defaulting failures.
    pub(crate) async fn try_snapshot_all_observed(
        &self,
        required_channels: impl IntoIterator<Item = ChannelId>,
    ) -> HashMap<ChannelId, Result<ChannelMailboxSnapshot, MailboxObservationFailure>> {
        let handles: HashMap<_, _> = self
            .handles
            .iter()
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect();
        let mut snapshots: HashMap<_, _> = required_channels
            .into_iter()
            .filter(|channel| !handles.contains_key(channel))
            .map(|channel| (channel, Err(MailboxObservationFailure::Missing)))
            .collect();
        for (channel, handle) in handles {
            snapshots.insert(
                channel,
                handle
                    .try_snapshot()
                    .await
                    .map_err(|_| MailboxObservationFailure::Unreachable),
            );
        }
        snapshots
    }

    /// `/api/health`'s read: every existing actor at once, each cut at `deadline`, without
    /// creating actors or defaulting failures.
    pub(crate) async fn try_snapshot_all_for_health(
        &self,
        required_channels: impl IntoIterator<Item = ChannelId>,
        deadline: tokio::time::Instant,
    ) -> HashMap<ChannelId, Result<ChannelMailboxSnapshot, MailboxObservationFailure>> {
        count_enumeration();
        let handles: HashMap<_, _> = self
            .handles
            .iter()
            .map(|entry| (*entry.key(), entry.value().clone()))
            .collect();
        let mut snapshots: HashMap<_, _> = required_channels
            .into_iter()
            .filter(|channel| !handles.contains_key(channel))
            .map(|channel| (channel, Err(MailboxObservationFailure::Missing)))
            .collect();
        let observed =
            futures::future::join_all(handles.into_iter().map(|(channel, handle)| async move {
                let result = tokio::time::timeout_at(deadline, handle.try_snapshot())
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .ok_or(MailboxObservationFailure::Unreachable);
                (channel, result)
            }))
            .await;
        snapshots.extend(observed);
        snapshots
    }
}
