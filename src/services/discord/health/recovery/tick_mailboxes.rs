//! One bounded mailbox enumeration per runtime per watchdog tick, shared by its consumers.
//! An actor that does not answer leaves its channel unknown for the tick, never idle.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use poise::serenity_prelude::ChannelId;

use super::HealthRegistry;
use super::snapshot::WatcherStateSnapshot;
use super::transcript_binding_stall::{Transition, TransitionLog};
use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{ChannelMailboxSnapshot, MailboxObservationFailure};

/// How long one mailbox read may wait; well under the ~31 s tick.
const MAILBOX_READ_DEADLINE: Duration = Duration::from_secs(10);
/// An unreachable entry no tick re-read for this long is dropped.
const UNREACHABLE_FORGET: Duration = Duration::from_secs(600);

/// Channels last read unreachable; a continuing one logs once across consumers and ticks.
static UNREACHABLE: LazyLock<TransitionLog> = LazyLock::new(TransitionLog::default);

type Enumerated = HashMap<ChannelId, Result<ChannelMailboxSnapshot, MailboxObservationFailure>>;

/// One tick's mailboxes; built per tick and dropped with it.
pub(in crate::services::discord::health) struct TickMailboxes {
    provider: ProviderKind,
    runtimes: Vec<Arc<SharedData>>,
    by_runtime: Vec<Enumerated>,
    unknown: Mutex<HashSet<ChannelId>>,
}

pub(in crate::services::discord::health) enum WatcherRead {
    Observed(Box<WatcherStateSnapshot>),
    /// No snapshot exists for the channel.
    Gone,
    /// Its actor did not answer by the deadline this tick.
    Unknown,
}

impl TickMailboxes {
    /// Reads every runtime's actors at once, each cut at one deadline.
    pub(in crate::services::discord::health) async fn enumerate(
        provider: &ProviderKind,
        runtimes: Vec<Arc<SharedData>>,
    ) -> Self {
        let now = tokio::time::Instant::now();
        UNREACHABLE.reasons(UNREACHABLE_FORGET, now);
        let deadline = now + MAILBOX_READ_DEADLINE;
        let by_runtime = futures::future::join_all(runtimes.iter().map(|shared| {
            #[cfg(all(test, unix))]
            ENUMERATIONS.with(|count| count.set(count.get() + 1));
            shared
                .mailboxes
                .try_snapshot_all_for_health(Vec::new(), deadline)
        }))
        .await;
        let tick = Self {
            provider: provider.clone(),
            runtimes,
            by_runtime,
            unknown: Mutex::default(),
        };
        let mut answered: HashMap<ChannelId, bool> = HashMap::new();
        for (channel, read) in tick.by_runtime.iter().flatten() {
            *answered.entry(*channel).or_insert(true) &= read.is_ok();
        }
        for (channel, answered) in answered {
            if answered {
                tick.answered(channel);
            } else {
                tick.mark_unknown(channel);
            }
        }
        tick
    }

    pub(in crate::services::discord::health) fn runtimes(&self) -> &[Arc<SharedData>] {
        &self.runtimes
    }

    /// Each runtime with the mailboxes this tick read from it.
    pub(in crate::services::discord::health) fn mailboxes(
        &self,
    ) -> impl Iterator<Item = (&Arc<SharedData>, &Enumerated)> {
        self.runtimes.iter().zip(&self.by_runtime)
    }

    pub(in crate::services::discord::health) fn is_unknown(&self, channel: ChannelId) -> bool {
        self.unknown
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&channel)
    }

    /// The channel's watcher state within the deadline; a channel already unknown is not read.
    pub(in crate::services::discord::health) async fn watcher_state(
        &self,
        registry: &HealthRegistry,
        shared: Arc<SharedData>,
        channel: ChannelId,
    ) -> WatcherRead {
        if self.is_unknown(channel) {
            return WatcherRead::Unknown;
        }
        let read =
            registry.snapshot_watcher_state_for_shared(&self.provider, shared, channel.get());
        match tokio::time::timeout(MAILBOX_READ_DEADLINE, read).await {
            Ok(Some(snapshot)) => WatcherRead::Observed(Box::new(snapshot)),
            Ok(None) => WatcherRead::Gone,
            Err(_) => {
                self.mark_unknown(channel);
                WatcherRead::Unknown
            }
        }
    }

    fn key(&self, channel: ChannelId) -> String {
        format!("{}:{}", self.provider.as_str(), channel.get())
    }

    fn mark_unknown(&self, channel: ChannelId) {
        self.unknown
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(channel);
        let now = tokio::time::Instant::now();
        let state = Some(("mailbox_unreachable", None));
        if UNREACHABLE.record(&self.key(channel), state, now).warns() {
            tracing::warn!(
                target: "agentdesk::services::discord::health::recovery",
                provider = self.provider.as_str(),
                channel_id = channel.get(),
                "STALL-WATCHDOG: mailbox_unreachable: the actor did not answer; the channel is unknown, not idle, and gets no idle repair"
            );
        }
    }

    fn answered(&self, channel: ChannelId) {
        let now = tokio::time::Instant::now();
        if let Transition::Ended(_) = UNREACHABLE.record(&self.key(channel), None, now) {
            tracing::info!(
                target: "agentdesk::services::discord::health::recovery",
                provider = self.provider.as_str(),
                channel_id = channel.get(),
                "STALL-WATCHDOG: mailbox actor answers again"
            );
        }
    }
}

#[cfg(all(test, unix))]
thread_local! {
    static ENUMERATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Runtime enumerations this thread has made so far.
#[cfg(all(test, unix))]
pub(in crate::services::discord::health) fn enumerations_for_tests() -> usize {
    ENUMERATIONS.with(std::cell::Cell::get)
}
