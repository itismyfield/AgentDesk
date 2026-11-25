//! A scoped terminate's last steps around its exclusion: the retired execution's runtime hint is
//! cleared while the channel is still excluded, and its owner is woken once it no longer is.
use std::sync::Arc;

use super::{ChannelId, RetiredExclusion, SharedData};
use crate::db::dispatched_sessions::hosted_execution::HostedState;
use crate::services::herdr_launch::runtime_marker;

/// Whether the terminate changed the channel, carried past its postlude to the one wake.
pub(super) struct RetiredWake {
    shared: Arc<SharedData>,
    channel: ChannelId,
    changed: bool,
}

impl RetiredWake {
    /// Clears the hint of `held`'s execution, retired by this run's own CAS when `written`.
    pub(super) async fn clear_under(
        shared: &Arc<SharedData>,
        held: &RetiredExclusion,
        written: bool,
    ) -> Self {
        let mut cleared = false;
        if let Some(record) = held.execution() {
            let mut retired = record.clone();
            retired.state = HostedState::Retired;
            match runtime_marker::clear_retired(&retired).await {
                Ok(removed) => cleared = removed,
                // A refused cleanup keeps the hint, and presence keeps holding, as before.
                Err(why) => tracing::warn!(
                    nonce = %retired.execution_nonce,
                    %why,
                    "herdr terminate: retired runtime hint kept"
                ),
            }
        }
        Self {
            shared: shared.clone(),
            channel: held.channel(),
            changed: written || cleared,
        }
    }

    /// Called once the exclusion is dropped: wakes the owner when this run changed the channel and
    /// input still waits, so a postlude that emptied the queue wakes nothing.
    pub(super) async fn after_release(self) {
        if !self.changed {
            return;
        }
        let Some(mailbox) = self.shared.mailbox_peek(self.channel) else {
            return;
        };
        let waiting = mailbox.v2_request(|slot| {
            Ok(slot
                .as_ref()
                .is_some_and(|owner| !owner.snapshot.items.is_empty()))
        });
        if waiting.await == Ok(true) {
            wake(&self.shared, self.channel);
        }
    }
}

#[cfg(unix)]
fn wake(shared: &Arc<SharedData>, channel: ChannelId) {
    (shared.router_ports.owner_wake)(shared, channel);
}

#[cfg(not(unix))]
fn wake(_: &Arc<SharedData>, _: ChannelId) {}
