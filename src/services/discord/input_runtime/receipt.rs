//! Receipt evidence is produced only after a synced WAL append or a positive replay lookup.

use super::source::Source;
use crate::services::tui_input::ledger::LedgerLease;
use crate::services::tui_input::receipt_identity::{ReceiptIdentity, Responsibility};
use crate::services::tui_input::rows::Rows;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DurableReceipt {
    pub(crate) key: u64,
    pub(crate) received_seq: u64,
    pub(crate) identity: ReceiptIdentity,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Deferred {
    Closed,
    Order,
    Unknown,
    Conflict,
    Persistence,
    SupervisorLost,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Receipt {
    Accepted(DurableReceipt),
    DuplicateQueued(DurableReceipt),
    Deferred(Deferred),
}

fn known(rows: &Rows, key: u64, received_seq: u64) -> Option<DurableReceipt> {
    Some(DurableReceipt {
        key,
        received_seq,
        identity: rows.row(key)?.receipt_identity.clone()?,
    })
}

pub(super) fn commit(lease: &mut LedgerLease, source: Source) -> Receipt {
    let result = (|| {
        #[cfg(test)]
        if super::mutant("second_open") {
            lease.reopen()?;
        }
        let ledger = lease.get()?;
        let rows = ledger.rows()?;
        match rows.responsibility(source.identity()) {
            Responsibility::Known { key, received_seq } => {
                return Ok(known(&rows, key, received_seq)
                    .map(Receipt::DuplicateQueued)
                    .unwrap_or(Receipt::Deferred(Deferred::Unknown)));
            }
            Responsibility::Unknown => return Ok(Receipt::Deferred(Deferred::Unknown)),
            Responsibility::Conflict => return Ok(Receipt::Deferred(Deferred::Conflict)),
            Responsibility::Absent => {}
        }
        let key = source.key();
        let identity = source.identity().clone();
        #[cfg(test)]
        if super::mutant("ack_before_sync") {
            return Ok(Receipt::Accepted(DurableReceipt {
                key,
                received_seq: rows.folded_seq() + 1,
                identity,
            }));
        }
        let (entry, pins) = source.into_entry();
        let received_seq = ledger.append_entry(&entry, &pins)?;
        Ok::<_, std::io::Error>(Receipt::Accepted(DurableReceipt {
            key,
            received_seq,
            identity,
        }))
    })();
    match result {
        Ok(receipt) => receipt,
        Err(_) => {
            lease.needs_reopen = true;
            Receipt::Deferred(Deferred::Persistence)
        }
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
#[path = "receipt_tests.rs"]
mod tests;
