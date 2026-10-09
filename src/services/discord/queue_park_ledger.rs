use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use super::{ChannelId, SharedData, health::HealthRegistry};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{ChannelMailboxSnapshot, QueueExitEvent};
use tokio::time::Instant;

const QUEUE_PARK_ERROR_SECS: u64 = 600;
const HEALTH_SOURCE_DISPLAY_LIMIT: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Origin {
    PostCancelPreserved,
    CancelledAnchorObserved,
}

#[derive(Clone, Debug)]
struct TrackedSource {
    first_seen: Instant,
    origin: Origin,
    escalated: bool,
}

#[derive(Default)]
pub(super) struct QueueParkLedger {
    channels: Mutex<BTreeMap<ChannelId, BTreeMap<u64, TrackedSource>>>,
    #[cfg(test)]
    evaluations: std::sync::atomic::AtomicUsize,
}

#[derive(Default)]
pub(super) struct ParkProjection {
    pub(super) reason: Option<String>,
    pub(super) owner: Option<&'static str>,
    pub(super) oldest_tracked_secs: Option<u64>,
    pub(super) tracked_source_ids: Vec<u64>,
    pub(super) tracked_source_count: usize,
    pub(super) ids_truncated: bool,
    pub(super) inflight_row_kind: Option<&'static str>,
    pub(super) recovery_state: Option<&'static str>,
}

fn queued_sources(snapshot: &ChannelMailboxSnapshot) -> BTreeSet<u64> {
    snapshot
        .intervention_queue
        .iter()
        .flat_map(|item| {
            std::iter::once(item.message_id.get())
                .chain(item.source_message_ids.iter().map(|id| id.get()))
        })
        .collect()
}

fn foreground(snapshot: &ChannelMailboxSnapshot) -> bool {
    snapshot.cancel_token.is_some() && !snapshot.active_turn_kind.is_background()
}

fn cancelled(snapshot: &ChannelMailboxSnapshot) -> bool {
    foreground(snapshot)
        && snapshot
            .cancel_token
            .as_ref()
            .is_some_and(|token| token.cancelled.load(std::sync::atomic::Ordering::Relaxed))
}

fn classify(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
    snapshot: &ChannelMailboxSnapshot,
) -> ParkProjection {
    let mut result = ParkProjection::default();
    let row = super::inflight::load_inflight_state_read_only_result(provider, channel.get());
    if let Ok(Some(row)) = &row {
        result.inflight_row_kind = Some(if row.tmux_session_name.is_none() {
            "no_tmux_identity"
        } else if row.terminal_delivery_committed {
            "terminal_committed"
        } else {
            match row.effective_relay_owner_kind() {
                super::inflight::RelayOwnerKind::SessionBoundRelay => "session_bound",
                _ => "non_session_bound",
            }
        });
        // Structure alone cannot prove death or recovery eligibility.
        result.recovery_state = Some(if row.tmux_session_name.is_none() {
            "no_periodic_caller"
        } else {
            "unknown"
        });
    } else if row.is_err() {
        result.recovery_state = Some("unknown");
    }
    let residue = shared
        .turn_finalizer
        .guarded_finish_residues()
        .get(&channel)
        .is_some_and(|residue| residue.matches_observed_owner(snapshot));
    if row.is_err() && cancelled(snapshot) {
        return result;
    }
    let (reason, owner) = if foreground(snapshot) && !cancelled(snapshot) {
        ("live_turn_active".to_string(), "active_turn_completion")
    } else if residue {
        ("residual_held".to_string(), "turn_finalizer_reconcile")
    } else if super::input_runtime::fence::lookup(provider, channel.get())
        .is_some_and(|gate| gate.mode() != super::input_runtime::fence::Mode::LegacyOpen)
    {
        ("fenced".to_string(), "none")
    } else if cancelled(snapshot) {
        let evidence = super::zombie_foreground_release::collect_zombie_foreground_evidence(
            provider,
            channel,
            snapshot.cancel_token.as_ref(),
        );
        let verdict = super::zombie_foreground_release::classify_zombie_foreground(evidence);
        (
            format!("cancelled_anchor_held:{}", verdict.as_str()),
            "idle_queue_backstop",
        )
    } else if snapshot.recovery_started_at.is_some() {
        ("recovery_started".to_string(), "none")
    } else if super::cleanup_retry_inflight_blocks_idle_kickoff(shared, provider, channel) {
        (
            "cleanup_retry_inflight".to_string(),
            "turn_finalizer_reconcile",
        )
    } else if matches!(
        super::automatic_queue_progression(shared, provider, channel, snapshot),
        super::AutomaticQueueProgression::BlockedByCappedRetries
    ) {
        ("capped_retries".to_string(), "none")
    } else if super::tui_direct_pending_start::pending_synthetic_start_blocks_idle_kickoff(
        provider.as_str(),
        channel.get(),
    ) {
        ("pending_synthetic_start".to_string(), "none")
    } else {
        ("idle_no_start".to_string(), "none")
    };
    result.reason = Some(reason);
    result.owner = Some(owner);
    result
}

impl QueueParkLedger {
    pub(super) fn register(
        &self,
        channel: ChannelId,
        snapshot: &ChannelMailboxSnapshot,
        origin: Origin,
    ) {
        let sources = queued_sources(snapshot);
        if sources.is_empty() {
            return;
        }
        let mut channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        let tracked = channels.entry(channel).or_default();
        for source in sources {
            tracked.entry(source).or_insert_with(|| TrackedSource {
                first_seen: Instant::now(),
                origin,
                escalated: false,
            });
        }
    }

    pub(super) fn project(
        &self,
        shared: &SharedData,
        provider: &ProviderKind,
        channel: ChannelId,
        snapshot: &ChannelMailboxSnapshot,
    ) -> ParkProjection {
        let mut result = classify(shared, provider, channel, snapshot);
        let channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tracked) = channels.get(&channel) {
            result.oldest_tracked_secs = tracked
                .values()
                .map(|source| source.first_seen.elapsed().as_secs())
                .max();
            result.tracked_source_count = tracked.len();
            result.tracked_source_ids = tracked
                .keys()
                .take(HEALTH_SOURCE_DISPLAY_LIMIT)
                .copied()
                .collect();
            result.ids_truncated = result.tracked_source_count > result.tracked_source_ids.len();
        }
        result
    }

    pub(super) fn exit(&self, channel: ChannelId, events: &[QueueExitEvent]) {
        let mut channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        let Some(tracked) = channels.get_mut(&channel) else {
            return;
        };
        for event in events {
            for id in std::iter::once(&event.intervention.message_id)
                .chain(&event.intervention.source_message_ids)
            {
                if let Some(source) = tracked.remove(&id.get()) {
                    tracing::info!(target: "agentdesk::discord::queue_park", channel_id = channel.get(), source_id = id.get(), ?source.origin, age_secs = source.first_seen.elapsed().as_secs(), kind = ?event.kind, "cancel-preserved source explicitly_removed");
                }
            }
        }
        if tracked.is_empty() {
            channels.remove(&channel);
        }
    }

    #[cfg(test)]
    pub(super) fn evaluate(
        &self,
        shared: &SharedData,
        provider: &ProviderKind,
        channel: ChannelId,
        snapshot: &ChannelMailboxSnapshot,
    ) {
        self.evaluate_observed(shared, provider, channel, snapshot, None);
    }

    fn evaluate_observed(
        &self,
        shared: &SharedData,
        provider: &ProviderKind,
        channel: ChannelId,
        snapshot: &ChannelMailboxSnapshot,
        observed: Option<&BTreeSet<u64>>,
    ) {
        if cancelled(snapshot) {
            self.register(channel, snapshot, Origin::CancelledAnchorObserved);
        }
        let projection = classify(shared, provider, channel, snapshot);
        let active: BTreeSet<_> = snapshot
            .active_user_message_id
            .iter()
            .chain(&snapshot.active_absorbed_source_ids)
            .map(|id| id.get())
            .collect();
        let mut waiting = queued_sources(snapshot);
        waiting.extend(
            snapshot
                .pending_user_dispatch
                .iter()
                .chain(&snapshot.pending_user_dispatch_source_ids)
                .map(|id| id.get()),
        );
        let mut channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        let Some(tracked) = channels.get_mut(&channel) else {
            return;
        };
        tracked.retain(|id, source| {
            let age_secs = source.first_seen.elapsed().as_secs();
            if active.contains(id) {
                tracing::info!(target: "agentdesk::discord::queue_park", provider = provider.as_str(), channel_id = channel.get(), source_id = id, ?source.origin, age_secs, "cancel-preserved source resumed");
                return false;
            }
            if !waiting.contains(id) {
                if observed.is_some_and(|sources| !sources.contains(id)) { return true; }
                tracing::info!(target: "agentdesk::discord::queue_park", provider = provider.as_str(), channel_id = channel.get(), source_id = id, ?source.origin, age_secs, "tracked source left the queue without an observed claim: unknown");
                return false;
            }
            if age_secs >= QUEUE_PARK_ERROR_SECS && projection.reason.is_some() && projection.reason.as_deref() != Some("live_turn_active") && !source.escalated {
                tracing::error!(target: "agentdesk::discord::queue_park", provider = provider.as_str(), channel_id = channel.get(), source_id = id, ?source.origin, age_secs, park_reason = projection.reason.as_deref(), recovery_owner = projection.owner, inflight_row_kind = projection.inflight_row_kind, recovery_state = projection.recovery_state, "cancel-preserved source remains parked");
                source.escalated = true;
            }
            true
        });
        if tracked.is_empty() {
            channels.remove(&channel);
        }
    }
}

pub(super) async fn evaluate_provider(registry: &HealthRegistry, provider: &ProviderKind) {
    for shared in registry.all_shared_for_provider(provider).await {
        // A source registered during snapshot I/O has not yet been observed by this pass.
        let observed: BTreeMap<_, BTreeSet<_>> = shared
            .queue_park_ledger
            .channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(channel, sources)| (*channel, sources.keys().copied().collect()))
            .collect();
        let mut snapshots: BTreeMap<_, _> =
            shared.mailboxes.snapshot_all().await.into_iter().collect();
        for channel in observed.keys() {
            snapshots.entry(*channel).or_default();
        }
        for (channel, snapshot) in snapshots {
            let empty = BTreeSet::new();
            shared.queue_park_ledger.evaluate_observed(
                &shared,
                provider,
                channel,
                &snapshot,
                Some(observed.get(&channel).unwrap_or(&empty)),
            );
        }
        #[cfg(test)]
        shared
            .queue_park_ledger
            .evaluations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
#[path = "queue_park_ledger/cancel_park_tests.rs"]
mod tests;
