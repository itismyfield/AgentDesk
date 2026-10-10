//! Receipt evidence is produced only after a synced WAL append or a positive replay lookup.

use super::source::Source;
use crate::services::tui_input::ledger::{Ledger, LedgerLease};
use crate::services::tui_input::rows::Rows;
use crate::services::tui_input::rows::receipt_identity::{ReceiptIdentity, Responsibility};

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
    LegacyResponsibility(DurableReceipt),
    Deferred(Deferred),
}

fn known(rows: &Rows, key: u64, received_seq: u64) -> Option<DurableReceipt> {
    Some(DurableReceipt {
        key,
        received_seq,
        identity: rows.receipt_identity(key)?,
    })
}

pub(super) fn commit(lease: &mut LedgerLease, source: Source, permits_new: bool) -> Receipt {
    let result = (|| {
        #[cfg(test)]
        if super::mutant("second_open") {
            lease.reopen()?;
        }
        let ledger = lease.get()?;
        let rows = ledger.rows()?;
        #[cfg(test)]
        let permits_new = permits_new || super::mutant("skip_receipt_permit");
        match rows.responsibility(source.identity()) {
            Responsibility::Known { key, received_seq } => {
                return Ok(known(&rows, key, received_seq)
                    .map(Receipt::DuplicateQueued)
                    .unwrap_or(Receipt::Deferred(Deferred::Unknown)));
            }
            Responsibility::Legacy { key, received_seq } => {
                return Ok(known(&rows, key, received_seq)
                    .map(Receipt::LegacyResponsibility)
                    .unwrap_or(Receipt::Deferred(Deferred::Unknown)));
            }
            Responsibility::Unknown => {
                return Ok(Receipt::Deferred(if permits_new {
                    Deferred::Unknown
                } else {
                    Deferred::Order
                }));
            }
            Responsibility::Conflict => {
                return Ok(Receipt::Deferred(if permits_new {
                    Deferred::Conflict
                } else {
                    Deferred::Order
                }));
            }
            Responsibility::Absent => {}
        }

        if !permits_new {
            return Ok(Receipt::Deferred(Deferred::Order));
        }
        #[cfg(test)]
        if super::mutant("ack_before_sync") {
            return Ok(Receipt::Accepted(DurableReceipt {
                key: source.key(),
                received_seq: rows.folded_seq() + 1,
                identity: source.identity().clone(),
            }));
        }
        append_active(ledger, source).map(Receipt::Accepted)
    })();
    finish(lease, result)
}

fn append_active(ledger: &mut Ledger, source: Source) -> std::io::Result<DurableReceipt> {
    let key = source.key();
    let identity = source.identity().clone();
    let (entry, pins) = source.into_entry();
    let received_seq = ledger.append_entry(&entry, &pins)?;
    #[cfg(test)]
    let verify = !super::mutant("skip_activation_check");
    #[cfg(not(test))]
    let verify = true;
    if verify
        && !ledger
            .rows()?
            .row(key)
            .is_some_and(|row| row.since_seq == received_seq)
    {
        return Err(std::io::Error::other(
            "synced receipt did not activate its row",
        ));
    }
    Ok(DurableReceipt {
        key,
        received_seq,
        identity,
    })
}

fn finish(lease: &mut LedgerLease, result: std::io::Result<Receipt>) -> Receipt {
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
