use serde::Serialize;

use poise::serenity_prelude::ChannelId;

use crate::services::discord::SharedData;
use crate::services::discord::relay_health::{
    FrontierProvenanceReport, RelayHealthSnapshot, RelayStallState,
};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::ChannelMailboxSnapshot;
use crate::services::turn_orchestrator::registry_purge::MailboxPurgeOutcome;

use super::HealthRegistry;
// #5071 T4-B6: `health::reachability` is `#[cfg(unix)]`, so every item this file
// takes from it — the `RelayVerdictReport` import and the field typed by it — is
// gated the same way. Windows keeps the pre-B6 entry, which had no such field.
#[cfg(unix)]
use super::reachability::composite::RelayVerdictReport;
use super::recovery::ProviderMailboxState;
use super::stall_verdict::StallVerdict;

#[derive(Debug, Serialize)]
pub(super) struct MailboxHealthSnapshot {
    pub(super) provider: String,
    pub(super) channel_id: u64,
    pub(super) has_cancel_token: bool,
    pub(super) queue_depth: usize,
    pub(super) recovery_started: bool,
    pub(super) active_request_owner: Option<u64>,
    pub(super) active_user_message_id: Option<u64>,
    pub(super) agent_turn_status: &'static str,
    pub(super) watcher_attached: bool,
    pub(super) inflight_state_present: bool,
    pub(super) tmux_present: bool,
    pub(super) process_present: bool,
    pub(super) active_dispatch_present: bool,
    pub(super) stall_shadow_verdict: Option<StallVerdict>,
    /// #5071 T4-B6 (4987 §4.4): the composed relay verdict for this channel.
    /// Published in both `RelayVerdictSource` modes; its
    /// `governs_health_polarity` field says whether it also decided anything.
    #[cfg(unix)]
    pub(super) reachability: RelayVerdictReport,
    /// #5071 relay-tail S1 (I-4): which witnesses produced this channel's
    /// frontier reading, and which E2 hypothesis the pair is consistent with.
    /// Published beside the relay-health snapshot rather than inside it: this
    /// entry is a serialization surface, while `RelayHealthSnapshot` is an
    /// input to `RelayStallClassifier` and to the recovery decisions, and an
    /// observation-only field has no business within their reach.
    pub(super) frontier_provenance: FrontierProvenanceReport,
    pub(super) relay_stall_state: RelayStallState,
    pub(super) relay_health: RelayHealthSnapshot,
}

/// How a guarded-finish residue is sitting on this channel's mailbox, as the
/// finalizer reconciler's own predicates answer it (#5068 r3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::services::discord) enum ResidualOccupancy {
    /// No residue is anchored to the owner the mailbox currently reports.
    None,
    /// A residue is anchored but the reconciler has NOT been handed release
    /// authority. Nothing resolves this on its own: it takes a user cancel or a
    /// newer episode taking the channel. Reporting it as `active` — which is
    /// what this surface did before r3 — makes a permanently stranded mailbox
    /// indistinguishable from a turn that is simply running.
    Held,
    /// A residue is anchored AND authorized. The reconciler releases it as soon
    /// as its I/O evidence gates clear, so this state is transient by
    /// construction.
    Releasable,
}

pub(in crate::services::discord) const fn mailbox_agent_turn_status(
    has_cancel_token: bool,
    residual: ResidualOccupancy,
) -> &'static str {
    match residual {
        ResidualOccupancy::Releasable => "residual",
        ResidualOccupancy::Held => "residual_held",
        ResidualOccupancy::None if has_cancel_token => "active",
        ResidualOccupancy::None => "idle",
    }
}

/// #5068: health REPORTS the question the finalizer reconciler DECIDES, by
/// calling its predicates instead of restating them — an observer that says
/// `residual` where the decider says "held, evidence insufficient" is the #5052
/// split-brain shape.
///
/// r3 splits the answer in two rather than collapsing it. Both arms come from
/// the SAME two predicates the reconciler branches on
/// (`matches_observed_owner`, `release_authorized`), so there is still exactly
/// one definition of release authority in the codebase; what changed is only
/// that health stopped throwing away the second bit. Health deliberately does
/// NOT consult the I/O evidence gates (`inflight_state_present`,
/// `tui_structurally_idle`) — those decide WHEN the reconciler acts, not whether
/// a residue exists, and a residue waiting on them is still one an operator
/// wants to see. That keeps this a projection of the decider's state, never a
/// second opinion about it.
pub(in crate::services::discord) fn residual_occupancy(
    shared: &super::super::SharedData,
    channel: ChannelId,
    snapshot: &ChannelMailboxSnapshot,
) -> ResidualOccupancy {
    shared
        .turn_finalizer
        .guarded_finish_residues()
        .get(&channel)
        .filter(|residue| residue.matches_observed_owner(snapshot))
        .map_or(ResidualOccupancy::None, |residue| {
            if residue.release_authorized(snapshot) {
                ResidualOccupancy::Releasable
            } else {
                ResidualOccupancy::Held
            }
        })
}

/// #3293 (c): probe a channel's mailbox state WITHOUT creating a registry
/// entry. `shared.mailbox()` (the pre-#3293 probe path) mints a permanent
/// mailbox actor for every channel id it is asked about, so health/repair
/// before+after probes against a non-existent channel id polluted the
/// registry forever. No entry simply reports the idle/empty state.
pub(super) async fn peeked_provider_mailbox_state(
    shared: &SharedData,
    channel_id: u64,
) -> ProviderMailboxState {
    let Some(handle) = shared.mailbox_peek(ChannelId::new(channel_id)) else {
        return ProviderMailboxState {
            channel_id,
            has_cancel_token: false,
            queue_depth: 0,
            recovery_started: false,
        };
    };
    let snapshot = handle.snapshot().await;
    ProviderMailboxState {
        channel_id,
        has_cancel_token: snapshot.cancel_token.is_some(),
        queue_depth: snapshot.intervention_queue.len(),
        recovery_started: snapshot.recovery_started_at.is_some(),
    }
}

/// Result of [`purge_idle_channel_mailbox_registry_entry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MailboxRegistryPurgeResult {
    /// At least one runtime's registry entry was unlinked.
    pub removed: bool,
    /// Why nothing (or not everything) was removed; `None` on full success.
    pub skipped_reason: Option<&'static str>,
}

/// #3293 (c): operator-gated purge of a channel's idle mailbox registry
/// entry. Called by the stale-mailbox repair endpoint AFTER its full gate
/// chain (CAS `expected_has_cancel_token` + `no_live_work_evidence`) passed
/// and the repair reported `applied`. Visits every registered runtime for
/// the provider filter (or all runtimes when unfiltered) because the bogus
/// entry may live in any instance registry; each removal re-verifies actor
/// idleness right before the unlink. In-memory only — no disk/DB mutation.
pub async fn purge_idle_channel_mailbox_registry_entry(
    registry: &HealthRegistry,
    provider_name: Option<&str>,
    channel_id: u64,
) -> MailboxRegistryPurgeResult {
    let runtimes = match provider_name {
        Some(name) => match ProviderKind::from_str(name) {
            Some(provider) => registry.all_shared_for_provider(&provider).await,
            None => Vec::new(),
        },
        None => registry.all_registered_shared().await,
    };
    if runtimes.is_empty() {
        return MailboxRegistryPurgeResult {
            removed: false,
            skipped_reason: Some("no_registered_runtime"),
        };
    }
    let channel = ChannelId::new(channel_id);
    let mut removed = false;
    let mut refused: Option<&'static str> = None;
    for shared in runtimes {
        match shared.mailboxes.remove_idle_entry(channel).await {
            MailboxPurgeOutcome::Removed => removed = true,
            MailboxPurgeOutcome::NoEntry => {}
            MailboxPurgeOutcome::RefusedLiveWork(reason) => refused = Some(reason),
        }
    }
    let skipped_reason = refused.or(if removed {
        None
    } else {
        Some("no_registry_entry")
    });
    MailboxRegistryPurgeResult {
        removed,
        skipped_reason,
    }
}

#[cfg(test)]
mod tests {
    use super::{ResidualOccupancy, mailbox_agent_turn_status};

    #[test]
    fn agent_turn_status_separates_releasable_and_permanently_held_residue() {
        assert_eq!(
            mailbox_agent_turn_status(true, ResidualOccupancy::Releasable),
            "residual"
        );
        assert_eq!(
            mailbox_agent_turn_status(true, ResidualOccupancy::Held),
            "residual_held",
            "a residue nothing will release on its own must never read as an ordinary active turn"
        );
        assert_eq!(
            mailbox_agent_turn_status(true, ResidualOccupancy::None),
            "active"
        );
        assert_eq!(
            mailbox_agent_turn_status(false, ResidualOccupancy::None),
            "idle"
        );
    }
}
