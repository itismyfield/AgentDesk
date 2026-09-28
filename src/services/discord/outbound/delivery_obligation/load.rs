use std::path::Path;

use super::super::delivery_record::lock_record_path;
use super::fence::LedgerFence;
use super::reader::{FenceState, LedgerDocument, LedgerLoad, LoadedLedger, read_optional};
use super::state::{BlockedReason, SourceIdentityState, SourceObs};
use super::{LEDGER_SCHEMA, validation};

/// Read under the existing record flock; report repairs without publishing them.
pub(in crate::services::discord) fn load_ledger(
    path: &Path,
    fence_path: &Path,
    source: SourceObs,
) -> LedgerLoad {
    let result = (|| {
        let _lock = lock_record_path(path).map_err(|_| BlockedReason::Unavailable)?;
        let fence = read_optional(fence_path)?;
        if let Some(bytes) = &fence {
            serde_json::from_slice::<LedgerFence>(bytes).map_err(|_| BlockedReason::Corrupt)?;
        }
        let Some(bytes) = read_optional(path)? else {
            return if fence.is_some() {
                Err(BlockedReason::Unavailable)
            } else {
                Ok(LoadedLedger {
                    document: LedgerDocument::default(),
                    identity: SourceIdentityState::LegacyUnbound,
                    fence: FenceState::Absent,
                })
            };
        };
        let record: LedgerDocument =
            serde_json::from_slice(&bytes).map_err(|_| BlockedReason::Corrupt)?;
        if record
            .ledger_schema
            .is_some_and(|schema| schema != LEDGER_SCHEMA)
        {
            return Err(BlockedReason::UnknownSchema);
        }
        if record.ledger_protocol.is_some_and(|protocol| protocol != 1) {
            return Err(BlockedReason::UnknownProtocol);
        }
        if record.obligation_ledger.is_some()
            && (record.ledger_schema.is_none() || record.ledger_protocol.is_none())
        {
            return Err(BlockedReason::Incompatible);
        }
        let identity = record
            .delivered_frontier
            .as_ref()
            .map_or(Ok(SourceIdentityState::LegacyUnbound), |frontier| {
                validation::source_identity(frontier, source)
            })?;
        if identity == SourceIdentityState::SourceUnavailable {
            return Err(BlockedReason::SourceUnavailable);
        }
        let fence_state = match &record.obligation_ledger {
            None if fence.is_some() => return Err(BlockedReason::Incompatible),
            None => FenceState::Absent,
            Some(ledger) => {
                validation::publication(ledger, record.delivered_frontier.as_ref(), source)?;
                let active = !ledger.open.is_empty()
                    || !ledger.held.is_empty()
                    || !ledger.intents.is_empty();
                match (active, fence.is_some()) {
                    (true, false) => FenceState::RecreateRequired,
                    (false, true) => FenceState::IncompleteClear,
                    (_, true) => FenceState::Present,
                    (_, false) => FenceState::Absent,
                }
            }
        };
        Ok(LoadedLedger {
            document: record,
            identity,
            fence: fence_state,
        })
    })();
    match result {
        Ok(loaded) => LedgerLoad::Loaded(Box::new(loaded)),
        Err(reason) => LedgerLoad::Blocked(reason),
    }
}
