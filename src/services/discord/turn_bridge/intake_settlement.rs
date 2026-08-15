//! Receipt-backed settlement at the terminal bridge boundary.
//!
//! Honest boundary: settlement requires a `Ready` capability probe, so this
//! path cannot be production-verified while #5245 leaves the required schema
//! migrations unapplied. The done-writer gate is a lexical scan and cannot see
//! direct SQL, glob imports, or generated calls; this slice neither widens nor
//! narrows that declared limitation.

use super::InflightTurnState;
use crate::db::intake_outbox_delivery_proof::{
    IntakeSettlementSource, settle_intake_done_from_receipt,
};
use crate::services::discord::SharedData;
use crate::services::discord::runtime_bootstrap::intake_delivery_capability::SettlementCapabilities;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// A terminal bridge must not wait indefinitely behind an unrelated row lock.
/// This is a service-local safety ceiling, not a measured contention claim.
const SETTLEMENT_LOCK_TIMEOUT: &str = "1s";

/// Binds the pre-await stamp snapshot into the existing bridge inflight state.
/// See the four-part snapshot contract on [`SettlementCapabilities`].
pub(super) fn bind_bridge_turn_snapshot(
    shared: &std::sync::Arc<SharedData>,
    bridge: &mut super::TurnBridgeContext,
) {
    let snapshot = shared
        .intake_delivery_capabilities
        .take_bridge_turn_snapshot(bridge.inflight_state.intake_outbox_id());
    bridge
        .inflight_state
        .bind_intake_delivery_capabilities(snapshot);
}

/// The disposition of a bridge turn at its one normal terminal exit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::services::discord) enum BridgeTurnDisposition {
    /// A retry still owns the inflight row; leave the intake row open.
    PreservedForRetry,
    /// This bridge committed terminal delivery.
    Committed,
    /// A watcher or standby relay owns the terminal delivery.
    RelayOwnerHandoff,
    /// No body was delivered and no retry was retained.
    NoBodyNoRetry,
}

impl BridgeTurnDisposition {
    fn settlement_source(self) -> Option<IntakeSettlementSource> {
        match self {
            Self::PreservedForRetry => None,
            Self::Committed => Some(IntakeSettlementSource::Committed),
            Self::RelayOwnerHandoff => Some(IntakeSettlementSource::RelayOwnerHandoff),
            Self::NoBodyNoRetry => Some(IntakeSettlementSource::NoBodyNoRetry),
        }
    }
}

/// Applies the terminal-outcome precedence contract.
pub(super) fn classify(
    terminal_delivery_committed: bool,
    status_panel_terminal_committed: bool,
    preserve_inflight_for_cleanup_retry: bool,
    bridge_skip_holder_owns_inflight: bool,
    relay_owner_present: bool,
) -> BridgeTurnDisposition {
    if preserve_inflight_for_cleanup_retry || bridge_skip_holder_owns_inflight {
        BridgeTurnDisposition::PreservedForRetry
    } else if terminal_delivery_committed || status_panel_terminal_committed {
        BridgeTurnDisposition::Committed
    } else if relay_owner_present {
        BridgeTurnDisposition::RelayOwnerHandoff
    } else {
        BridgeTurnDisposition::NoBodyNoRetry
    }
}

// `Sweep` is reserved for the S-W3 caller and already owns its telemetry slot.
const SETTLEMENT_SOURCES: [IntakeSettlementSource; 4] = [
    IntakeSettlementSource::Committed,
    IntakeSettlementSource::RelayOwnerHandoff,
    IntakeSettlementSource::NoBodyNoRetry,
    IntakeSettlementSource::Sweep,
];
const SOURCE_COUNT: usize = SETTLEMENT_SOURCES.len();

struct SettlementCounters {
    cas_won: [AtomicU64; SOURCE_COUNT],
    cas_noop: [AtomicU64; SOURCE_COUNT],
    write_failed: [AtomicU64; SOURCE_COUNT],
}

impl Default for SettlementCounters {
    fn default() -> Self {
        Self {
            cas_won: std::array::from_fn(|_| AtomicU64::new(0)),
            cas_noop: std::array::from_fn(|_| AtomicU64::new(0)),
            write_failed: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

static SETTLEMENT_COUNTERS: OnceLock<SettlementCounters> = OnceLock::new();

fn counters() -> &'static SettlementCounters {
    SETTLEMENT_COUNTERS.get_or_init(SettlementCounters::default)
}

const fn source_index(source: IntakeSettlementSource) -> usize {
    match source {
        IntakeSettlementSource::Committed => 0,
        IntakeSettlementSource::RelayOwnerHandoff => 1,
        IntakeSettlementSource::NoBodyNoRetry => 2,
        IntakeSettlementSource::Sweep => 3,
    }
}

fn record_settlement_result(
    outbox_id: i64,
    source: IntakeSettlementSource,
    result: Result<bool, sqlx::Error>,
) {
    let index = source_index(source);
    match result {
        Ok(true) => {
            counters().cas_won[index].fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                counter = "intake_settlement_cas_won",
                source = source.as_str(),
                outbox_id,
                "intake settlement CAS reached done"
            );
        }
        Ok(false) => {
            counters().cas_noop[index].fetch_add(1, Ordering::Relaxed);
            tracing::info!(
                counter = "intake_settlement_cas_noop",
                source = source.as_str(),
                outbox_id,
                "intake settlement CAS was already terminal or won by another actor"
            );
        }
        Err(error) => {
            counters().write_failed[index].fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                counter = "intake_settlement_write_failed",
                source = source.as_str(),
                outbox_id,
                %error,
                "intake settlement SQL failed; leaving turn outcome unchanged"
            );
        }
    }
}

async fn settle_with_lock_timeout(
    pool: &sqlx::PgPool,
    outbox_id: i64,
    source: IntakeSettlementSource,
) -> Result<bool, sqlx::Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT set_config('lock_timeout', $1, true)")
        .bind(SETTLEMENT_LOCK_TIMEOUT)
        .execute(&mut *transaction)
        .await?;
    let won = settle_intake_done_from_receipt(&mut transaction, outbox_id, source).await?;
    transaction.commit().await?;
    Ok(won)
}

/// Settles the intake row associated with a bridge at the terminal exit.
///
/// The inflight identity is read here for terminal settlement. The headless
/// delivery argument assembler also reads it into a parked, no-effect seam.
/// The immutable turn snapshot follows the contract declared on
/// `SettlementCapabilities`: fresh Off/Observe turns return without database
/// access, while an Enforce turn remains authorized after a later downgrade.
/// SQL errors are counted and swallowed after the bridge classified the turn as
/// committed, handed off to a relay owner, or complete with no retained retry;
/// changing those outcomes here could create a duplicate retry.
pub(in crate::services::discord) async fn settle_intake_row_at_bridge_exit(
    shared: &std::sync::Arc<SharedData>,
    inflight_state: &InflightTurnState,
    disposition: BridgeTurnDisposition,
    caps: SettlementCapabilities,
) {
    let Some(source) = disposition.settlement_source() else {
        return;
    };
    let Some(outbox_id) = inflight_state.intake_outbox_id() else {
        return;
    };
    if !caps.settle_and_sweep {
        return;
    }
    let Some(pool) = shared.pg_pool.as_ref() else {
        return;
    };

    let result = settle_with_lock_timeout(pool, outbox_id, source).await;
    record_settlement_result(outbox_id, source, result);
}

#[cfg(test)]
mod tests;
