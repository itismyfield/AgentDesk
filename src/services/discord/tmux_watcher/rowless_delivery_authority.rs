//! #5464 T5 C1 rowless delivery authority: the two T5 AC1 operands, the
//! cohort that gates them, and the candidacy predicate they feed.
//!
//! Split out of `terminal_relay_plan.rs` to keep that module inside the
//! `src/services/discord/tmux_watcher/**` namespace size cap.

use super::*;
use crate::services::discord::LeaseSnapshot;

/// #5464 T5 C1: delivery authority for a terminal frame whose durable inflight
/// row is GONE, read from the two sources T5 AC1 names — the ledger's output
/// obligation and the delivery lease — plus the rollout cohort that gates them.
///
/// Three named operands, not one fused bool, so the flight recorder's
/// `soft_terminal_denial` stays attributable: surviving on a ledger obligation,
/// on a lease, and being outside the cohort are different operational stories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct RowlessDeliveryAuthority {
    pub(super) cohort_admits: bool,
    pub(super) ledger_obligation_open: bool,
    pub(super) delivery_lease_present: bool,
}

impl RowlessDeliveryAuthority {
    /// A structural signal alone never ends delivery (T5 AC1), but it takes a
    /// POSITIVE operand to keep the frame alive: inside the enforcement cohort a
    /// rowless frame the ledger has settled and no lease covers is still refused.
    pub(super) fn retains_delivery_candidacy(self) -> bool {
        self.cohort_admits && (self.ledger_obligation_open || self.delivery_lease_present)
    }
}

/// Does the ledger still OWE output for `consumed_end`? Pure, so a dropped `!`
/// fails a behavioural test, not only a source grep that would stay green.
///
/// FAIL-CLOSED on `None`. The frontier lookup reports several DISTINCT "unknown"
/// states — absent/malformed record, prior-generation frontier, no transcript EOF,
/// or a frontier END beyond EOF, which is what an in-place `/compact` or rotation
/// produces. Collapsing them onto `0` made every one an OPEN obligation, restoring
/// the historical rowless frame #5175 refused as a delivery candidate on this
/// operand alone. `Some(delivered)` keeps `consumed_end > delivered` unchanged.
pub(super) fn ledger_owes_output(consumed_end: u64, delivered_end: Option<u64>) -> bool {
    let Some(delivered_end) = delivered_end else {
        return false;
    };
    consumed_end > 0 && !dr::range_already_committed(consumed_end, delivered_end)
}

/// Is a LIVE delivery holder present? Only `Leased` counts (#5464 T5 C1).
///
/// `Committed` is a FINISHED delivery that `reclaim_if_expired` never returns to
/// `Unleased`, so a holder dying between `commit()` and `release()` strands the cell
/// there indefinitely. Counting it would hand a rowless soft terminal `denial=None`,
/// switching OFF the #5175 WARN and `record_relay_terminal_authority_denied`.
pub(super) fn lease_has_live_holder(snapshot: &LeaseSnapshot) -> bool {
    matches!(snapshot, LeaseSnapshot::Leased { .. })
}

/// Read the three operands for this frame: the rollout cohort, the DURABLE ledger
/// obligation (against the generation-guarded (#1270) and EOF-guarded (#4188)
/// `resolved_delivered_frontier_end_current_generation`, deliberately NOT the in-memory
/// watermark-fusing `committed_floor_for_resend_dedup`), and the live delivery
/// lease. This watcher has not acquired its own lease at this seam
/// (`try_acquire_watcher_delivery_lease` runs after the plan returns), so the
/// downstream B2 acquire, not this predicate, decides who actually sends.
pub(super) fn read_rowless_delivery_authority(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    tmux_session_name: &str,
    output_path: &str,
    consumed_end: u64,
) -> RowlessDeliveryAuthority {
    let transcript_eof = std::fs::metadata(output_path).ok().map(|meta| meta.len());
    let delivered_end = dr::resolved_delivered_frontier_end_current_generation(
        provider,
        channel_id,
        tmux_session_name,
        transcript_eof,
    );
    RowlessDeliveryAuthority {
        cohort_admits: crate::services::discord::relay_recovery::cohort::enforcement_admits(
            channel_id.get(),
        ),
        ledger_obligation_open: ledger_owes_output(consumed_end, delivered_end),
        delivery_lease_present: lease_has_live_holder(&shared.delivery_lease(channel_id).read()),
    }
}
