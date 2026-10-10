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

impl ChannelMailboxRegistry {
    pub(crate) async fn snapshot_all(&self) -> HashMap<ChannelId, ChannelMailboxSnapshot> {
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
}
