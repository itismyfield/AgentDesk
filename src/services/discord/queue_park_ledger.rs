use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use super::{ChannelId, SharedData, health::HealthRegistry};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{
    ChannelMailboxSnapshot, MailboxObservationFailure, QueueExitEvent,
};
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
struct LedgerState {
    sources: BTreeMap<ChannelId, BTreeMap<u64, TrackedSource>>,
    revisions: BTreeMap<ChannelId, u64>,
    unavailable: BTreeMap<ChannelId, MailboxObservationFailure>,
}

impl LedgerState {
    fn advance(&mut self, channel: ChannelId) {
        let revision = self.revisions.entry(channel).or_default();
        *revision = revision
            .checked_add(1)
            .expect("queue observation revision overflow");
    }
}

#[derive(Default)]
pub(super) struct QueueParkLedger {
    channels: Mutex<LedgerState>,
    #[cfg(test)]
    evaluations: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    observation_gate: Mutex<Option<ObservationGate>>,
}

#[cfg(test)]
type ObservationGate = (
    std::sync::Arc<tokio::sync::Barrier>,
    std::sync::Arc<tokio::sync::Barrier>,
);

#[derive(Debug, Default, serde::Serialize)]
pub(super) struct ParkProjection {
    #[serde(rename = "queue_park_reason")]
    pub(super) reason: Option<String>,
    #[serde(rename = "queue_park_owner")]
    pub(super) owner: Option<&'static str>,
    #[serde(rename = "queue_park_oldest_tracked_secs")]
    pub(super) oldest_tracked_secs: Option<u64>,
    #[serde(rename = "queue_park_tracked_source_ids")]
    pub(super) tracked_source_ids: Vec<u64>,
    #[serde(rename = "queue_park_tracked_source_count")]
    pub(super) tracked_source_count: usize,
    #[serde(rename = "queue_park_ids_truncated")]
    pub(super) ids_truncated: bool,
    #[serde(rename = "queue_park_inflight_row_kind")]
    pub(super) inflight_row_kind: Option<&'static str>,
    #[serde(rename = "queue_park_recovery_state")]
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
    } else if super::health::legacy_supervision::is_retired(provider.as_str(), channel.get()) {
        result.recovery_state = Some("no_periodic_caller");
        ("legacy_retired".to_string(), "none")
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
    } else if row
        .as_ref()
        .ok()
        .and_then(|row| row.as_ref())
        .is_some_and(|row| {
            super::inflight::opt_message_id(row.current_msg_id).is_some_and(|message| {
                shared
                    .ui
                    .placeholder_cleanup
                    .terminal_cleanup_retry_pending_read_only(provider, channel, message)
            })
        })
    {
        (
            "cleanup_retry_inflight".to_string(),
            "turn_finalizer_reconcile",
        )
    } else if matches!(
        super::automatic_queue_progression(shared, provider, channel, snapshot),
        super::AutomaticQueueProgression::BlockedByCappedRetries
    ) {
        ("capped_retries".to_string(), "none")
    } else if super::tui_direct_pending_start::pending_synthetic_start_present(
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
        channels.advance(channel);
        Self::register_sources(&mut channels, channel, sources, origin);
    }

    fn register_sources(
        channels: &mut LedgerState,
        channel: ChannelId,
        sources: BTreeSet<u64>,
        origin: Origin,
    ) {
        let tracked = channels.sources.entry(channel).or_default();
        for source in sources {
            tracked.entry(source).or_insert_with(|| TrackedSource {
                first_seen: Instant::now(),
                origin,
                escalated: false,
            });
        }
    }

    #[cfg(test)]
    pub(super) fn project(
        &self,
        shared: &SharedData,
        provider: &ProviderKind,
        channel: ChannelId,
        snapshot: &ChannelMailboxSnapshot,
    ) -> ParkProjection {
        self.project_observed(shared, provider, channel, Ok(snapshot))
    }

    pub(super) fn tracked_channels(&self) -> Vec<ChannelId> {
        self.channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sources
            .keys()
            .copied()
            .collect()
    }

    pub(super) fn project_observed(
        &self,
        shared: &SharedData,
        provider: &ProviderKind,
        channel: ChannelId,
        observation: Result<&ChannelMailboxSnapshot, MailboxObservationFailure>,
    ) -> ParkProjection {
        let mut result = observation
            .ok()
            .map(|snapshot| classify(shared, provider, channel, snapshot))
            .unwrap_or_default();
        let channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        if observation.is_err() {
            result.reason = Some("observation_unavailable".to_string());
            result.owner = Some("none");
            result.recovery_state = Some("unknown");
        }
        if let Some(tracked) = channels.sources.get(&channel) {
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
        if events.is_empty() {
            return;
        }
        let mut channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        // Keep the revision after the last source exits so an older snapshot cannot rediscover it.
        channels.advance(channel);
        let Some(tracked) = channels.sources.get_mut(&channel) else {
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
            channels.sources.remove(&channel);
            channels.unavailable.remove(&channel);
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
        self.evaluate_observed(shared, provider, channel, Ok(snapshot), None);
    }

    fn evaluate_observed(
        &self,
        shared: &SharedData,
        provider: &ProviderKind,
        channel: ChannelId,
        observation: Result<&ChannelMailboxSnapshot, MailboxObservationFailure>,
        expected_revision: Option<u64>,
    ) {
        let projection = observation
            .ok()
            .map(|snapshot| classify(shared, provider, channel, snapshot));
        let mut channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        if expected_revision.is_some_and(|revision| {
            revision
                != channels
                    .revisions
                    .get(&channel)
                    .copied()
                    .unwrap_or_default()
        }) {
            return;
        }
        channels.advance(channel);
        let snapshot = match observation {
            Ok(snapshot) => snapshot,
            Err(failure) => {
                if channels.sources.contains_key(&channel)
                    && channels.unavailable.insert(channel, failure) != Some(failure)
                {
                    tracing::warn!(target: "agentdesk::discord::queue_park", provider = provider.as_str(), channel_id = channel.get(), observation_failure = failure.as_str(), "cancel-preserved source observation_unavailable");
                }
                return;
            }
        };
        channels.unavailable.remove(&channel);
        let projection = projection.expect("successful observation classified");
        if cancelled(snapshot) {
            let sources = queued_sources(snapshot);
            if !sources.is_empty() {
                Self::register_sources(
                    &mut channels,
                    channel,
                    sources,
                    Origin::CancelledAnchorObserved,
                );
            }
        }
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
        let Some(tracked) = channels.sources.get_mut(&channel) else {
            return;
        };
        tracked.retain(|id, source| {
            let age_secs = source.first_seen.elapsed().as_secs();
            if active.contains(id) {
                tracing::info!(target: "agentdesk::discord::queue_park", provider = provider.as_str(), channel_id = channel.get(), source_id = id, ?source.origin, age_secs, "cancel-preserved source resumed");
                return false;
            }
            if !waiting.contains(id) {
                tracing::info!(target: "agentdesk::discord::queue_park", provider = provider.as_str(), channel_id = channel.get(), source_id = id, ?source.origin, age_secs, "tracked source left the queue without an observed claim: unknown");
                return false;
            }
            if age_secs >= QUEUE_PARK_ERROR_SECS && projection.reason.as_deref() != Some("live_turn_active") && !source.escalated {
                tracing::error!(target: "agentdesk::discord::queue_park", provider = provider.as_str(), channel_id = channel.get(), source_id = id, ?source.origin, age_secs, park_reason = projection.reason.as_deref(), recovery_owner = projection.owner, inflight_row_kind = projection.inflight_row_kind, recovery_state = projection.recovery_state, "cancel-preserved source remains parked");
                source.escalated = true;
            }
            true
        });
        if tracked.is_empty() {
            channels.sources.remove(&channel);
        }
    }
}

pub(super) async fn evaluate_provider(registry: &HealthRegistry, provider: &ProviderKind) {
    for shared in registry.all_shared_for_provider(provider).await {
        let (revisions, required) = {
            let state = shared
                .queue_park_ledger
                .channels
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            (
                state.revisions.clone(),
                state.sources.keys().copied().collect::<Vec<_>>(),
            )
        };
        let snapshots = shared.mailboxes.try_snapshot_all_observed(required).await;
        #[cfg(test)]
        {
            let gate = shared
                .queue_park_ledger
                .observation_gate
                .lock()
                .unwrap()
                .take();
            if let Some((observed, apply)) = gate {
                observed.wait().await;
                apply.wait().await;
            }
        }
        for (channel, observation) in snapshots {
            shared.queue_park_ledger.evaluate_observed(
                &shared,
                provider,
                channel,
                observation.as_ref().map_err(|failure| *failure),
                Some(revisions.get(&channel).copied().unwrap_or_default()),
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

#[cfg(all(test, unix))]
impl QueueParkLedger {
    pub(in crate::services::discord) fn age_sources_for_tests(
        &self,
        channel: ChannelId,
        age: std::time::Duration,
    ) {
        let mut channels = self
            .channels
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let sources = channels
            .sources
            .get_mut(&channel)
            .expect("already observed sources");
        for source in sources.values_mut() {
            source.first_seen = source
                .first_seen
                .checked_sub(age)
                .expect("fixture source age");
        }
    }
}
