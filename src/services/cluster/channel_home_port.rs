//! The drain's reads of a delegated channel in this process: the mailbox's turn, what the O actor
//! last published that it owes and the writer's running POSTs. It keeps no state; not built yet.
#![cfg_attr(not(test), allow(dead_code))]

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use poise::serenity_prelude::ChannelId;

use super::channel_home_drain::{DrainPort, Owed, ResetRefused};
use crate::services::tui_o::writer::actor::POLL_INTERVAL;
use crate::services::tui_o::writer::deliver;
use crate::services::tui_o::writer::host::{OwedView, Readiness};
use crate::services::turn_orchestrator::ChannelMailboxRegistry;

/// An owed read waits this long for the actor's next poll; none by then reads as unknown.
const FRESH_WITHIN: Duration = Duration::from_secs(5 * POLL_INTERVAL.as_secs());

pub(crate) struct ChannelHomePort {
    channel: u64,
    readiness: Arc<Readiness>,
}

impl ChannelHomePort {
    /// `readiness` is the writer host's map the channel's actor publishes to.
    pub(crate) fn new(channel: u64, readiness: Arc<Readiness>) -> Self {
        Self { channel, readiness }
    }
}

impl DrainPort for ChannelHomePort {
    /// A turn running or queued in the channel's mailbox; no mailbox in this process runs none.
    fn turn_running(&self) -> impl Future<Output = Option<bool>> + Send {
        let channel = (self.channel != 0).then(|| ChannelId::new(self.channel));
        async move {
            let Some(mailbox) = ChannelMailboxRegistry::global_handle(channel?) else {
                return Some(false);
            };
            let mailbox = mailbox.try_snapshot().await.ok()?;
            Some(
                mailbox.cancel_token.is_some()
                    || !mailbox.intervention_queue.is_empty()
                    || mailbox.pending_user_dispatch.is_some(),
            )
        }
    }

    /// What the actor publishes after the read began, so a check after the final close sees a
    /// poll that ran after it; an ended, halted or silent actor is `None`.
    fn owed(&self) -> impl Future<Output = Option<Owed>> + Send {
        let view = self.readiness.undelivered(self.channel);
        async move {
            let OwedView {
                mut published,
                demand,
            } = view?;
            let _wanting = demand.want();
            published.borrow_and_update();
            let next = tokio::time::timeout(FRESH_WITHIN, published.changed()).await;
            next.ok()?.ok()?;
            let undelivered = *published.borrow();
            // An actor that ended after its last send may have sent it while ending: unknown.
            published.has_changed().ok()?;
            let undelivered = undelivered?;
            Some(Owed {
                owed: undelivered.owed,
                prepared: undelivered.prepared,
                unsealed: undelivered.unsealed,
                uncaptured: undelivered.uncaptured,
                binding_pending: undelivered.binding_pending,
            })
        }
    }

    fn posts_in_flight(&self) -> impl Future<Output = Option<usize>> + Send {
        std::future::ready(deliver::posts_in_flight(self.channel))
    }

    /// The gateway's session reset is reachable only from the Discord runtime, which does not
    /// hand it to the drain yet, so a releasing drain waits here.
    fn reset_legacy_source(&self) -> impl Future<Output = Result<(), ResetRefused>> + Send {
        std::future::ready(Err(ResetRefused::NotWired))
    }
}

#[cfg(test)]
#[path = "channel_home_port_tests.rs"]
mod tests;
