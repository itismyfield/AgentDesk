//! The drain's reads of a delegated channel in this process: the channel's turn, what the O actor
//! last published that it owes and the writer's running POSTs. It keeps no state.
#![cfg_attr(not(test), allow(dead_code))]

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
    restored: Arc<AtomicBool>,
}

impl ChannelHomePort {
    /// `readiness` is the writer host's map the channel's actor publishes to; `restored` must turn
    /// true only once this role restored its persisted turns, which not every role's marker means.
    pub(crate) fn new(channel: u64, readiness: Arc<Readiness>, restored: Arc<AtomicBool>) -> Self {
        Self {
            channel,
            readiness,
            restored,
        }
    }
}

impl DrainPort for ChannelHomePort {
    /// A turn running or queued in the channel's mailbox, or one only its inflight row still names;
    /// unknown until this process restored its persisted turns, since a mailbox may still be missing.
    fn turn_running(&self) -> impl Future<Output = Option<bool>> + Send {
        let channel = (self.channel != 0).then(|| ChannelId::new(self.channel));
        let restored = self.restored.load(Ordering::Acquire);
        async move {
            let channel = channel.filter(|_| restored)?;
            let queued = match ChannelMailboxRegistry::global_handle(channel) {
                Some(mailbox) => {
                    let mailbox = mailbox.try_snapshot().await.ok()?;
                    mailbox.cancel_token.is_some()
                        || !mailbox.intervention_queue.is_empty()
                        || mailbox.pending_user_dispatch.is_some()
                }
                None => false,
            };
            Some(queued || crate::services::discord::has_fresh_inflight_for_channel(channel.get()))
        }
    }

    /// What the actor publishes from a poll begun after the read asked, so a check after the final
    /// close sees a poll that ran after it; an ended, halted or silent actor is `None`.
    fn owed(&self) -> impl Future<Output = Option<Owed>> + Send {
        let view = self.readiness.undelivered(self.channel);
        async move {
            let OwedView {
                mut published,
                demand,
            } = view?;
            let wanting = demand.want();
            published.borrow_and_update();
            let answer = async {
                loop {
                    published.changed().await.ok()?;
                    let undelivered = *published.borrow_and_update();
                    // An actor that ended after its last send may have sent it while ending: unknown.
                    published.has_changed().ok()?;
                    // A poll begun before this read asked may predate what it waits on.
                    match undelivered {
                        Some(stale) if stale.asked < wanting.asked() => continue,
                        undelivered => return undelivered,
                    }
                }
            };
            let undelivered = tokio::time::timeout(FRESH_WITHIN, answer).await.ok()??;
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

    /// The gateway's session reset is reachable only from the Discord runtime, which supplies it
    /// beside this port at boot; through this port alone a releasing drain waits here.
    fn reset_legacy_source(&self) -> impl Future<Output = Result<(), ResetRefused>> + Send {
        std::future::ready(Err(ResetRefused::NotWired))
    }
}

#[cfg(test)]
#[path = "channel_home_port_tests.rs"]
mod tests;

#[cfg(all(test, unix))]
pub(crate) mod scoped_restore {
    use crate::db::o_channel_homes::{ChannelHome, HomeState};
    use crate::services::agent_protocol::NativeTerminalKind;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    /// Installation identity, distinct from a live output or intake permit.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) struct RestoreScope {
        pub(crate) channel: u64,
        pub(crate) provider: String,
        pub(crate) epoch: i64,
        pub(crate) state: HomeState,
        pub(crate) holder: Option<String>,
        pub(crate) target: Option<String>,
        pub(crate) local: String,
    }
    impl RestoreScope {
        pub(crate) fn new(row: &ChannelHome, provider: &str, local: &str) -> Result<Self, String> {
            let channel = row
                .channel_id
                .parse::<u64>()
                .ok()
                .filter(|id| *id != 0)
                .ok_or("invalid restore channel")?;
            let held = row.holder.as_deref() == Some(local)
                && matches!(
                    row.state,
                    HomeState::Worker | HomeState::Releasing | HomeState::Reclaiming
                );
            let preparing =
                row.state == HomeState::Released && row.target.as_deref() == Some(local);
            if local.is_empty()
                || row.epoch <= 0
                || row.provider != provider
                || !(held || preparing)
            {
                return Err("restore home identity is not local".into());
            }
            Ok(Self {
                channel,
                provider: provider.into(),
                epoch: row.epoch,
                state: row.state,
                holder: row.holder.clone(),
                target: row.target.clone(),
                local: local.into(),
            })
        }
        pub(crate) fn matches(&self, row: &ChannelHome) -> bool {
            row.channel_id == self.channel.to_string()
                && row.provider == self.provider
                && row.epoch == self.epoch
                && row.state == self.state
                && row.holder == self.holder
                && row.target == self.target
        }
        pub(crate) fn held_locally(&self) -> bool {
            self.state != HomeState::Released && self.holder.as_deref() == Some(self.local.as_str())
        }
    }
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) enum RestoreStatus {
        Pending,
        Restoring,
        Restored,
        Blocked(String),
    }
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) enum InflightObservation {
        Empty,
        Active,
        ReplayHeld {
            receipt: Option<i64>,
            reasons: Vec<String>,
            delivery: Option<Result<(), String>>,
        },
        HerdrHeld,
        Admitted {
            kind: NativeTerminalKind,
        },
        Retained {
            kind: NativeTerminalKind,
            reason: String,
        },
        Settled {
            kind: NativeTerminalKind,
        },
        Deferred(String),
    }
    /// Only installation metadata; mailbox and canonical rows remain the busy authorities.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) struct RestoreWitness {
        pub(crate) scope: Option<RestoreScope>,
        pub(crate) generation: u64,
        pub(crate) status: RestoreStatus,
        pub(crate) inflight: Option<InflightObservation>,
    }
    impl Default for RestoreWitness {
        fn default() -> Self {
            Self {
                scope: None,
                generation: 0,
                status: RestoreStatus::Pending,
                inflight: None,
            }
        }
    }
    pub(crate) struct RestoreContext {
        pub(crate) scope: RestoreScope,
        pub(crate) generation: u64,
        pub(crate) current: Arc<AtomicU64>,
    }
    impl RestoreContext {
        pub(crate) fn check(&self) -> Result<(), String> {
            if self.generation == 0
                || (self.current.load(Ordering::Acquire) != self.generation
                    && !mutant("old-generation"))
            {
                Err("restore runtime generation changed".into())
            } else {
                Ok(())
            }
        }
    }
    pub(crate) fn mutant(name: &str) -> bool {
        std::env::var("ADK_S3ACT_B2_MUTANT").ok().as_deref() == Some(name)
    }
}
