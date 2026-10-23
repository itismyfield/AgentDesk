//! The drain's reads of a delegated channel in this process: the channel's turn, what the O actor
//! last published that it owes and the writer's running POSTs, or that O never held the channel
//! here. It keeps no state.
#![cfg_attr(not(test), allow(dead_code))]

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use poise::serenity_prelude::ChannelId;

#[cfg(test)]
fn writer_mutant(name: &str) -> bool {
    std::env::var("ADK_S3B_MUTANT").ok().as_deref() == Some(name)
}
use super::channel_home_drain::{DrainPort, Owed, ResetRefused};
use crate::services::tui_o::store;
use crate::services::tui_o::writer::actor::POLL_INTERVAL;
use crate::services::tui_o::writer::binding::{BindingEvents, BindingLog};
use crate::services::tui_o::writer::deliver;
use crate::services::tui_o::writer::host::{OwedView, Readiness};
use crate::services::turn_orchestrator::ChannelMailboxRegistry;

/// An owed read waits this long for the actor's next poll; none by then reads as unknown.
const FRESH_WITHIN: Duration = Duration::from_secs(5 * POLL_INTERVAL.as_secs());

/// Whether the channel still has persisted input on disk; `None` when it cannot be read.
pub(crate) type PersistedInput = Arc<dyn Fn() -> Option<bool> + Send + Sync>;

pub(crate) struct ChannelHomePort {
    channel: u64,
    readiness: Arc<Readiness>,
    restored: Arc<AtomicBool>,
    persisted: Option<PersistedInput>,
    store_root: Option<PathBuf>,
}

impl ChannelHomePort {
    /// `readiness` is the writer host's map the channel's actor publishes to; `restored` must turn
    /// true only once this role restored its persisted turns, which not every role's marker means.
    pub(crate) fn new(channel: u64, readiness: Arc<Readiness>, restored: Arc<AtomicBool>) -> Self {
        Self {
            channel,
            readiness,
            restored,
            persisted: None,
            store_root: None,
        }
    }

    /// Reads a missing actor or POST count as nothing owed when `runtime_root` (the writer host's
    /// own) holds no store of the channel and this node's P5 log no event of it.
    pub(crate) fn stored_under(mut self, runtime_root: Option<PathBuf>) -> Self {
        self.store_root = runtime_root;
        self
    }

    /// Whether O never held the channel here. Read after the actor or count was found missing:
    /// a store is laid out before either exists, and none is laid out once the final close ran.
    fn never_held(&self, skip_store: bool) -> bool {
        let Some(root) = &self.store_root else {
            return false;
        };
        let unbound = || {
            #[cfg(test)]
            if writer_mutant("binding_ignored") {
                return true;
            }
            let events = BindingLog.binding_events_since(self.channel, 0);
            #[cfg(test)]
            if writer_mutant("p5_err_as_empty") {
                return events.unwrap_or_default().is_empty();
            }
            events.is_ok_and(|events| events.is_empty())
        };
        (skip_store || store::no_channel_store(root, self.channel)) && unbound()
    }
}

impl DrainPort for ChannelHomePort {
    /// A turn running or queued in the channel's mailbox, persisted input, or one only its inflight
    /// row still names; unknown until this process restored its persisted turns.
    fn turn_running(&self) -> impl Future<Output = Option<bool>> + Send {
        let channel = (self.channel != 0).then(|| ChannelId::new(self.channel));
        let restored = self.restored.load(Ordering::Acquire);
        let persisted = self.persisted.clone();
        async move {
            let channel = channel.filter(|_| restored)?;
            if let Some(persisted) = persisted
                && persisted()?
            {
                return Some(true);
            }
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
    /// close sees a poll that ran after it; an ended, halted or silent actor is `None`. With no
    /// actor, nothing is owed only where O never held the channel here.
    fn owed(&self) -> impl Future<Output = Option<Owed>> + Send {
        let view = self.readiness.undelivered(self.channel);
        #[cfg(test)]
        let skip_store = writer_mutant("owed_ignores_store");
        #[cfg(not(test))]
        let skip_store = false;
        let never_held = view.is_none() && self.never_held(skip_store);
        async move {
            if never_held {
                return Some(Owed::default());
            }
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

    /// A count only a writer of the channel makes; without one, zero only where O never held it.
    fn posts_in_flight(&self) -> impl Future<Output = Option<usize>> + Send {
        let count = deliver::posts_in_flight(self.channel);
        #[cfg(test)]
        if count.is_none() && writer_mutant("posts_ignore_store") {
            return std::future::ready(Some(0));
        }
        let count = count.or_else(|| self.never_held(false).then_some(0));
        std::future::ready(count)
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

/// Recovery errors preserve whether a fresh attempt can resolve the failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RestoreFailure {
    Retryable(String),
    Rejected(String),
}
impl RestoreFailure {
    pub(crate) fn retryable(reason: impl ToString) -> Self {
        #[cfg(test)]
        if std::env::var("ADK_S3ACT_B3A1_MUTANT").ok().as_deref() == Some("retryable") {
            return Self::Rejected(reason.to_string());
        }
        Self::Retryable(reason.to_string())
    }
}
// Escape reserved tags at the String boundary so rejection text cannot become retryable.
fn encode_restore_failure(failure: &RestoreFailure) -> String {
    match failure {
        RestoreFailure::Retryable(reason) => format!("retryable: {reason}"),
        RestoreFailure::Rejected(reason)
            if reason.starts_with("retryable: ") || reason.starts_with("rejected: ") =>
        {
            format!("rejected: {reason}")
        }
        RestoreFailure::Rejected(reason) => reason.clone(),
    }
}
fn decode_restore_failure(reason: String) -> RestoreFailure {
    if let Some(payload) = reason.strip_prefix("rejected: ") {
        RestoreFailure::Rejected(payload.into())
    } else if let Some(payload) = reason.strip_prefix("retryable: ") {
        RestoreFailure::retryable(payload)
    } else {
        RestoreFailure::Rejected(reason)
    }
}
impl std::fmt::Display for RestoreFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&encode_restore_failure(self))
    }
}
impl From<String> for RestoreFailure {
    fn from(reason: String) -> Self {
        decode_restore_failure(reason)
    }
}
impl From<&str> for RestoreFailure {
    fn from(reason: &str) -> Self {
        reason.to_string().into()
    }
}
impl From<RestoreFailure> for String {
    fn from(reason: RestoreFailure) -> Self {
        reason.to_string()
    }
}

#[cfg(test)]
mod b3a2_failure_tests {
    use super::RestoreFailure;
    #[test]
    fn s3act_b3a2_failure_string_roundtrip_property() {
        let fragments = [
            "",
            "ordinary",
            "retryable: ",
            "retryable: X",
            "rejected: ",
            "rejected: retryable: X",
            "한글\n\0\\",
        ];
        for a in fragments {
            for b in fragments {
                for original in [
                    RestoreFailure::Retryable(format!("{a}{b}")),
                    RestoreFailure::Rejected(format!("{a}{b}")),
                ] {
                    assert_eq!(
                        RestoreFailure::from(original.to_string()),
                        original,
                        "failure variant and payload must roundtrip"
                    );
                    assert_eq!(
                        RestoreFailure::from(String::from(original.clone())),
                        original
                    );
                }
            }
        }
    }
}
