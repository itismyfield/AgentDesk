#![cfg(test)]
//! Dormant custody adapters for the existing mailbox and scan authority.

use super::IntakeAdmission;
use crate::services::discord::input_runtime::fence::Failure;
use crate::services::discord::input_runtime::{intake, supervisor as runtime};
use crate::services::provider::ProviderKind;
use crate::services::tui_input::rows::receipt_identity::Responsibility;
use intake::{Classification, PreparedSource};
use runtime::command::ScanCommit;
use runtime::ordering::ScanCapability;
use runtime::receipt::{Deferred, DurableReceipt, Receipt, ResponsibilityLookup};
use std::cell::RefCell;
use std::fmt;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Settlement {
    LookupOnly,
    Settled,
    Rescan(Failure),
}

pub(crate) struct LedgerReceived {
    pub(super) receipt: DurableReceipt,
    pub(super) original: PreparedSource,
    pub(super) settlement: Settlement,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DeferReason {
    Receipt(Deferred),
    Lookup(Responsibility),
    Legacy(DurableReceipt),
    Mismatched(DurableReceipt),
    Classification(Classification),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Recovery {
    Rescan,
    RestoreOriginal,
}

pub(crate) struct LedgerDeferred {
    pub(super) original: PreparedSource,
    pub(super) reason: DeferReason,
    pub(super) recovery: Recovery,
}

impl fmt::Debug for LedgerReceived {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LedgerReceived")
            .field("receipt", &self.receipt)
            .field("original", self.original.identity())
            .field("settlement", &self.settlement)
            .finish()
    }
}
impl fmt::Debug for LedgerDeferred {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LedgerDeferred")
            .field("original", self.original.identity())
            .field("reason", &self.reason)
            .field("recovery", &self.recovery)
            .finish()
    }
}

#[derive(Debug)]
pub(crate) struct LedgerOutcome(pub(super) IntakeAdmission);
impl fmt::Display for LedgerOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ledger admission retains responsibility for a dedicated custody consumer")
    }
}
impl std::error::Error for LedgerOutcome {}

pub(super) fn is_ledger(admission: &IntakeAdmission) -> bool {
    matches!(
        admission,
        IntakeAdmission::LedgerReceived(_) | IntakeAdmission::LedgerDeferred(_)
    )
}

fn deferred(original: PreparedSource, reason: DeferReason, recovery: Recovery) -> IntakeAdmission {
    IntakeAdmission::LedgerDeferred(Box::new(LedgerDeferred {
        original,
        reason,
        recovery,
    }))
}

fn covers(receipt: &DurableReceipt, original: &PreparedSource) -> bool {
    if runtime::mutant("c2c1_export_ignores_original_binding") {
        return true;
    }
    let canonical = &receipt.identity;
    let requested = original.identity();
    canonical.validates(receipt.key)
        && canonical.author_id == requested.author_id
        && canonical.original_channel_id == requested.original_channel_id
        && canonical.execution_channel_id == requested.execution_channel_id
        && requested
            .source_ids
            .iter()
            .all(|id| canonical.source_ids.contains(id))
}

pub(super) fn export_lookup(
    original: PreparedSource,
    lookup: Result<ResponsibilityLookup, Deferred>,
) -> IntakeAdmission {
    let lookup = match lookup {
        Ok(value) => value,
        Err(reason) => {
            return deferred(
                original,
                DeferReason::Receipt(reason),
                Recovery::RestoreOriginal,
            );
        }
    };
    match (lookup.responsibility, lookup.receipt) {
        (Responsibility::Known { key, received_seq }, Some(receipt))
            if receipt.key == key
                && receipt.received_seq == received_seq
                && covers(&receipt, &original) =>
        {
            IntakeAdmission::LedgerReceived(Box::new(LedgerReceived {
                receipt,
                original,
                settlement: Settlement::LookupOnly,
            }))
        }
        (Responsibility::Legacy { .. }, Some(receipt)) => deferred(
            original,
            DeferReason::Legacy(receipt),
            Recovery::RestoreOriginal,
        ),
        (_, Some(receipt)) => deferred(
            original,
            DeferReason::Mismatched(receipt),
            Recovery::RestoreOriginal,
        ),
        (responsibility, _) => deferred(
            original,
            DeferReason::Lookup(responsibility),
            Recovery::RestoreOriginal,
        ),
    }
}

pub(crate) enum ScanAdmission {
    Legacy {
        original: PreparedSource,
        capability: ScanCapability,
    },
    Admission {
        admission: IntakeAdmission,
        capability: Option<ScanCapability>,
    },
    NonTarget(ScanCapability),
}

fn export_scan(original: PreparedSource, commit: ScanCommit) -> ScanAdmission {
    let ScanCommit {
        receipt,
        settlement,
        capability,
    } = commit;
    let admission = match receipt {
        Receipt::Accepted(receipt) | Receipt::DuplicateQueued(receipt) => {
            if !covers(&receipt, &original) {
                return ScanAdmission::Admission {
                    admission: deferred(
                        original,
                        DeferReason::Mismatched(receipt),
                        Recovery::Rescan,
                    ),
                    capability: Some(capability),
                };
            }
            let settlement = match settlement {
                Ok(()) => Settlement::Settled,
                Err(reason) => Settlement::Rescan(reason),
            };
            let settlement = if runtime::mutant("c2c1_receipt_means_settled") {
                Settlement::Settled
            } else {
                settlement
            };
            if runtime::mutant("c2c1_failed_settlement_loses_receipt")
                && matches!(settlement, Settlement::Rescan(_))
            {
                deferred(
                    original,
                    DeferReason::Receipt(Deferred::Order),
                    Recovery::Rescan,
                )
            } else {
                if runtime::mutant("c2c1_receipt_folds_to_duplicate") {
                    return ScanAdmission::Admission {
                        admission: IntakeAdmission::SkippedDuplicate,
                        capability: Some(capability),
                    };
                }
                IntakeAdmission::LedgerReceived(Box::new(LedgerReceived {
                    receipt,
                    original,
                    settlement,
                }))
            }
        }
        Receipt::LegacyResponsibility(receipt) => {
            deferred(original, DeferReason::Legacy(receipt), Recovery::Rescan)
        }
        Receipt::Deferred(reason) => {
            deferred(original, DeferReason::Receipt(reason), Recovery::Rescan)
        }
    };
    ScanAdmission::Admission {
        admission,
        capability: Some(capability),
    }
}

pub(super) async fn consume_scan(
    registry: &'static runtime::Registry,
    provider: &ProviderKind,
    original: PreparedSource,
    classification: Classification,
    capability: ScanCapability,
) -> ScanAdmission {
    let client = match intake::client::lookup(
        registry,
        provider,
        original.identity().execution_channel_id,
    ) {
        intake::client::Access::Legacy => {
            if runtime::mutant("c2c1_off_scan_waits") {
                std::future::pending::<()>().await;
            }
            return ScanAdmission::Legacy {
                original,
                capability,
            };
        }
        intake::client::Access::Deferred(reason) => {
            return ScanAdmission::Admission {
                admission: deferred(original, DeferReason::Receipt(reason), Recovery::Rescan),
                capability: Some(capability),
            };
        }
        intake::client::Access::Ready(client) => client,
    };
    if runtime::mutant("c2c1_scan_bypasses_commit") {
        return ScanAdmission::Admission {
            admission: deferred(
                original,
                DeferReason::Receipt(Deferred::Order),
                Recovery::Rescan,
            ),
            capability: Some(capability),
        };
    }
    if runtime::mutant("c2c1_scan_adds_pending") {
        let _ = client.pending(original.identity().source_ids.clone()).await;
    }
    match intake::client::apply_from_scan(&client, classification, original.clone(), capability)
        .await
    {
        Ok(intake::client::Applied::Receipt(commit)) => export_scan(original, commit),
        Ok(intake::client::Applied::Settled(capability)) => ScanAdmission::NonTarget(capability),
        Ok(intake::client::Applied::Unsettled {
            classification,
            capability,
        }) => ScanAdmission::Admission {
            admission: deferred(
                original,
                DeferReason::Classification(classification),
                Recovery::Rescan,
            ),
            capability: Some(capability),
        },
        Err(reason) => ScanAdmission::Admission {
            admission: deferred(original, DeferReason::Receipt(reason), Recovery::Rescan),
            capability: None,
        },
    }
}

thread_local! { static TEST_ADMISSION: RefCell<Option<IntakeAdmission>> = const { RefCell::new(None) }; }
pub(super) fn inject_test_admission(admission: IntakeAdmission) {
    TEST_ADMISSION.with(|slot| assert!(slot.replace(Some(admission)).is_none()));
}
pub(super) fn take_test_admission(fallback: IntakeAdmission) -> IntakeAdmission {
    TEST_ADMISSION
        .with(|slot| slot.borrow_mut().take())
        .unwrap_or(fallback)
}

pub(super) fn test_original(channel: u64) -> PreparedSource {
    intake::prepare_source(intake::SourceDraft {
        origin: intake::Origin::Message {
            message_id: 10,
            source_ids: vec![10, 11],
            author_id: 7,
            original_channel_id: channel,
            execution_channel_id: channel,
        },
        author_is_bot: false,
        original_text: "original".into(),
        prompt: "transformed".into(),
        reply_context: Some("original reply".into()),
        attachments: vec![],
        voice_announcement: None,
    })
    .unwrap()
}
pub(super) fn test_received(channel: u64) -> IntakeAdmission {
    let original = test_original(channel);
    IntakeAdmission::LedgerReceived(Box::new(LedgerReceived {
        receipt: DurableReceipt {
            key: 10,
            received_seq: 1,
            identity: original.identity().clone(),
        },
        original,
        settlement: Settlement::LookupOnly,
    }))
}
pub(super) fn test_deferred(channel: u64) -> IntakeAdmission {
    deferred(
        test_original(channel),
        DeferReason::Receipt(Deferred::Unknown),
        Recovery::RestoreOriginal,
    )
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[path = "ledger_tests.rs"]
mod tests;
