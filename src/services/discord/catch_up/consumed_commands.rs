//! Live intake's record of the `!` text commands it consumed, by original message id.
//! Catch-up settles a recorded command instead of replaying its text as a provider prompt.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use dashmap::DashMap;
use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use super::super::{MailboxEnqueueOutcome, runtime_store};
use super::handled_command::{TextCommandEvidence, is_text_command, text_command_evidence};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::EnqueueRefusalReason;

/// Records kept per channel. Older ones are dropped with a warning and may replay.
pub(super) const MAX_CONSUMED_PER_CHANNEL: usize = 1024;

type ChannelLock = Arc<tokio::sync::Mutex<()>>;

/// One lock per channel record: a live record never lands between catch-up's
/// final re-read and its enqueue commit.
static CHANNEL_LOCKS: LazyLock<DashMap<(String, u64), ChannelLock>> = LazyLock::new(DashMap::new);

fn channel_lock(provider: &ProviderKind, channel_id: ChannelId) -> ChannelLock {
    let key = (provider.as_str().to_string(), channel_id.get());
    CHANNEL_LOCKS.entry(key).or_default().clone()
}

/// Beside the channel's checkpoint; the checkpoint scan and stale prune skip this name.
fn record_path(provider: &ProviderKind, channel_id: ChannelId) -> Option<PathBuf> {
    let file = format!("{}.consumed.json", channel_id.get());
    runtime_store::last_message_root().map(|root| root.join(provider.as_str()).join(file))
}

/// Recorded ids, oldest first. A missing, unreadable or malformed record is no evidence.
fn load(path: &Path) -> Vec<u64> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "consumed-command record unreadable");
            return Vec::new();
        }
    };
    serde_json::from_str(&raw).unwrap_or_else(|error| {
        tracing::warn!(path = %path.display(), %error, "consumed-command record malformed");
        Vec::new()
    })
}

/// Records that live intake consumed the command `message_id`. Call before its handler runs.
pub(in crate::services::discord) async fn record(
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
) {
    let Some(path) = record_path(provider, channel_id) else {
        return;
    };
    let lock = channel_lock(provider, channel_id);
    let _held = lock.lock().await;
    let mut ids = load(&path);
    if ids.contains(&message_id.get()) {
        return;
    }
    ids.push(message_id.get());
    let overflow = ids.len().saturating_sub(MAX_CONSUMED_PER_CHANNEL);
    if overflow > 0 {
        let dropped: Vec<u64> = ids.drain(..overflow).collect();
        tracing::warn!(
            channel_id = channel_id.get(),
            ?dropped,
            "consumed-command record full; oldest commands dropped and may replay"
        );
    }
    let raw = serde_json::to_string(&ids).expect("u64 ids serialize");
    if let Err(error) = runtime_store::atomic_write(&path, &raw) {
        tracing::warn!(path = %path.display(), %error, "consumed-command record not saved");
    }
}

/// One scan's view of a channel's record.
pub(super) struct ConsumedCommands {
    provider: ProviderKind,
    channel_id: ChannelId,
    ids: HashSet<u64>,
}

pub(super) fn read(provider: &ProviderKind, channel_id: ChannelId) -> ConsumedCommands {
    let ids = record_path(provider, channel_id).map_or_else(Vec::new, |path| load(&path));
    ConsumedCommands {
        provider: provider.clone(),
        channel_id,
        ids: ids.into_iter().collect(),
    }
}

impl ConsumedCommands {
    /// Whether this snapshot shows `text` as a command live intake consumed.
    pub(super) fn settles(&self, text: &str, message_id: u64) -> bool {
        is_text_command(text) && self.ids.contains(&message_id)
    }

    /// The live record first, then this bot's reply in the scanned history.
    pub(super) fn evidence(
        &self,
        text: &str,
        message_id: MessageId,
        scanned: &[serenity::Message],
        bot_user_id: Option<u64>,
    ) -> TextCommandEvidence {
        if self.settles(text, message_id.get()) {
            return TextCommandEvidence::Consumed;
        }
        text_command_evidence(text, message_id, scanned, bot_user_id)
    }

    /// Commits `enqueue` unless live intake consumed the command since this snapshot.
    /// The re-read and the commit share the record lock; a refusal re-classifies next scan.
    pub(super) async fn guard(
        &self,
        enqueue: impl Future<Output = MailboxEnqueueOutcome>,
        message_id: MessageId,
        text: &str,
    ) -> MailboxEnqueueOutcome {
        if !is_text_command(text) {
            return enqueue.await;
        }
        #[cfg(test)]
        test_gate::pause(test_gate::Stage::BeforeLock, message_id).await;
        let lock = channel_lock(&self.provider, self.channel_id);
        let _held = lock.lock().await;
        if read(&self.provider, self.channel_id).settles(text, message_id.get()) {
            return MailboxEnqueueOutcome {
                enqueued: false,
                merged: false,
                refusal_reason: Some(EnqueueRefusalReason::ClaimedSinceObservation),
                persistence_error: None,
            };
        }
        #[cfg(test)]
        test_gate::pause(test_gate::Stage::AfterRead, message_id).await;
        enqueue.await
    }
}

/// Parks catch-up's final check for one message so a scenario can act at a fixed point.
#[cfg(test)]
pub(in crate::services::discord) mod test_gate {
    use std::sync::{Arc, Mutex};

    use poise::serenity_prelude::MessageId;
    use tokio::sync::Notify;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(in crate::services::discord) enum Stage {
        /// Before the record lock is taken.
        BeforeLock,
        /// Under the lock, after the re-read found no record and before the commit.
        AfterRead,
    }

    struct Gate {
        stage: Stage,
        message_id: u64,
        held: Arc<Notify>,
        release: Arc<Notify>,
    }

    static GATES: Mutex<Vec<Gate>> = Mutex::new(Vec::new());

    /// Arms one hold; returns `(held, release)`. `held` is signalled when the check parks.
    pub(in crate::services::discord) fn hold(
        stage: Stage,
        message_id: u64,
    ) -> (Arc<Notify>, Arc<Notify>) {
        let (held, release) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
        GATES
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(Gate {
                stage,
                message_id,
                held: held.clone(),
                release: release.clone(),
            });
        (held, release)
    }

    pub(super) async fn pause(stage: Stage, message_id: MessageId) {
        let gate = {
            let mut gates = GATES.lock().unwrap_or_else(|poison| poison.into_inner());
            let index = gates
                .iter()
                .position(|gate| gate.stage == stage && gate.message_id == message_id.get());
            index.map(|index| gates.remove(index))
        };
        if let Some(gate) = gate {
            gate.held.notify_one();
            gate.release.notified().await;
        }
    }
}
