//! Boot restores whose queue or marker merge failed: their placeholder rows are installed under
//! the channel's persist lock before any other save, and a retry settles them.

use super::*;
use crate::services::turn_orchestrator::HydratePendingQueueResult;

#[cfg(test)]
#[path = "queued_recovery_held_tests.rs"]
mod tests;

/// Why a boot merge left a channel unsettled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum HoldReason {
    /// The channel's pending queue file could not be read or parsed.
    QueueRead,
    /// A write, a refusing actor, or anything else.
    Other(String),
}

impl HoldReason {
    pub(super) fn of(error: &str) -> Self {
        match crate::services::turn_orchestrator::is_queue_unread(error) {
            true => Self::QueueRead,
            false => Self::Other(error.to_owned()),
        }
    }
}

/// A channel whose boot merge failed: the candidates still to merge and the rows it holds.
pub(super) struct HeldRestore {
    pub(super) channel: ChannelId,
    pub(super) reason: HoldReason,
    pub(super) queue: Option<Vec<Intervention>>,
    pub(super) marker: Option<(Intervention, Option<ChannelId>)>,
    pub(super) rows: Vec<(MessageId, MessageId)>,
}

/// The rows a failed merge installed and the ones the installer refused.
pub(super) type Installed = (
    Vec<(MessageId, MessageId)>,
    Vec<(ChannelId, MessageId, MessageId)>,
);

/// Runs `merge` under the channel's persist lock. When it fails and the channel is not held yet,
/// the rows read under that lock are installed before the lock is released.
pub(super) async fn guarded_merge(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    already_held: bool,
    merge: impl std::future::Future<Output = HydratePendingQueueResult>,
) -> (HydratePendingQueueResult, Option<Installed>) {
    let lock = shared.queued_placeholders_persist_lock(channel);
    let guard = lock.lock_owned().await;
    let token = shared.token_hash.as_str();
    let load = super::super::queued_placeholders_store::load_channel_queued_placeholders_checked;
    let rows = match already_held {
        true => Default::default(),
        false => load(provider, token, channel).unwrap_or_else(|error| {
            tracing::warn!(channel_id = channel.get(), %error, "held restore rows unreadable");
            Default::default()
        }),
    };
    let result = merge.await;
    if result.persistence_error.is_none() || already_held {
        return (result, None);
    }
    let rows: Vec<_> = rows
        .into_iter()
        .map(|((_, owner), card)| (owner, card))
        .collect();
    let install = super::super::queued_placeholders::install_channel_rows_locked;
    let installed = install(shared, channel, rows, false).await;
    drop(guard);
    (result, Some(installed))
}

/// Records a failed merge: the first failure holds the channel, its installed rows and reason;
/// refused rows join the boot's stale cards.
pub(super) fn hold<'a>(
    held: &'a mut std::collections::HashMap<ChannelId, HeldRestore>,
    stale: &mut Vec<(ChannelId, MessageId, MessageId)>,
    channel: ChannelId,
    error: &str,
    installed: Option<Installed>,
) -> &'a mut HeldRestore {
    let entry = held.entry(channel).or_insert_with(|| HeldRestore {
        channel,
        reason: HoldReason::of(error),
        queue: None,
        marker: None,
        rows: Vec::new(),
    });
    if let Some((rows, refused)) = installed {
        entry.rows.extend(rows);
        stale.extend(refused);
    }
    entry
}

/// Seconds before each retry; the last repeats.
const BACKOFF_SECS: [u64; 5] = [5, 15, 60, 300, 900];

pub(super) fn backoff(attempt: usize) -> std::time::Duration {
    let secs = BACKOFF_SECS[attempt.min(BACKOFF_SECS.len() - 1)];
    std::time::Duration::from_secs(secs)
}

pub(super) fn spawn_retries(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    held: Vec<HeldRestore>,
) {
    for mut restore in held {
        let (shared, provider) = (Arc::clone(shared), provider.clone());
        let deleter = super::super::queued_placeholders::SerenityStalePlaceholderDeleter {
            http: Arc::clone(http),
        };
        tokio::spawn(async move {
            for attempt in 0.. {
                tokio::time::sleep(backoff(attempt)).await;
                let report = settle_once(&shared, &provider, &deleter, &mut restore).await;
                if report.settled {
                    break;
                }
                let channel_id = restore.channel.get();
                tracing::warn!(channel_id, attempt, reason = ?restore.reason, "held restore unsettled");
            }
        });
    }
}

/// What one settle attempt did.
#[derive(Debug, Default)]
pub(super) struct SettleReport {
    pub(super) settled: bool,
    pub(super) stale: Vec<(ChannelId, MessageId, MessageId)>,
    pub(super) rewatched: std::collections::HashSet<u64>,
    pub(super) kick: Option<IdleQueueKickoffChannelOutcome>,
    pub(super) backstop: bool,
}

/// Retries the failed merges, then lets the actor's probe keep live rows, leave undecided ones and
/// clean the rest like boot's stale rows; the queue is then re-watched and kicked.
pub(super) async fn settle_once(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    deleter: &impl super::super::queued_placeholders::StalePlaceholderDeleter,
    restore: &mut HeldRestore,
) -> SettleReport {
    let channel = restore.channel;
    let mut report = SettleReport::default();
    if let Some(items) = restore.queue.clone() {
        let merged = mailbox_merge_restored_queue_items(shared, provider, channel, items).await;
        if merged.persistence_error.is_some() {
            return report;
        }
        restore.queue = None;
    }
    if let Some((marker, alt)) = restore.marker.clone() {
        let merge = mailbox_merge_restored_dispatch_marker(shared, provider, channel, marker, alt);
        if merge.await.persistence_error.is_some() {
            return report;
        }
        restore.marker = None;
    }
    let persistence =
        crate::services::discord::queue_persistence_context(shared, provider, channel);
    let lock = shared.queued_placeholders_persist_lock(channel);
    let guard = lock.lock_owned().await;
    let probe = match shared.mailbox(channel).held_settle_probe(persistence).await {
        Ok(probe) if probe.error.is_none() => probe,
        _ => return report,
    };
    let map = &shared.queued.queued_placeholders;
    let mut undecided = Vec::new();
    for (owner, card) in std::mem::take(&mut restore.rows) {
        if map.get(&(channel, owner)).map(|entry| *entry) != Some(card)
            || probe.live.contains(&owner)
        {
            continue;
        }
        if probe.undecided.contains(&owner) {
            undecided.push((owner, card));
            continue;
        }
        shared.remove_queued_placeholder_locked(channel, owner);
        report.stale.push((channel, owner, card));
    }
    restore.rows = undecided;
    drop(guard);
    let delete = super::super::queued_placeholders::delete_stale_queued_placeholder_cards_with;
    delete(deleter, shared, &report.stale).await;
    report.rewatched = probe.queued.iter().map(|id| id.get()).collect();
    let watch = std::collections::HashMap::from([(channel, report.rewatched.clone())]);
    super::super::turn_bridge::followup_requeue::rewatch_draft_holds(shared, provider, &watch);
    let reason = "boot_restore_settled";
    let kick = super::super::queue_io::kick_idle_queue_channel_if_context_available;
    let kick = kick(shared, provider, channel, reason);
    let outcome = kick.await;
    report.kick = Some(outcome);
    let arm = super::super::queue_io::arm_event_backstop_after_no_start_if_queue_nonempty;
    report.backstop = arm(shared, provider, channel, outcome, reason).await;
    report.settled = restore.rows.is_empty();
    report
}

/// Test-only pause between the boot's queue merges and the rest of its restore.
#[cfg(test)]
pub(super) static PAUSE_AFTER_QUEUE_MERGES: std::sync::Mutex<
    Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(super) async fn pause_after_queue_merges() {
    let taken = PAUSE_AFTER_QUEUE_MERGES.lock().unwrap().take();
    if let Some((reached, release)) = taken {
        let _ = reached.send(());
        let _ = release.await;
    }
}
