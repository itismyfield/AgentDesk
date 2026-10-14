//! Typed custody over the existing mailbox and scan authority.

use super::{IntakeAdmission, IntakeBlockedReason, IntakeOrigin, IntakeSubmission};
use crate::services::discord::input_runtime::fence::Failure;
use crate::services::discord::input_runtime::{intake, supervisor as runtime};
use crate::services::provider::ProviderKind;
use crate::services::tui_input::rows::receipt_identity::Responsibility;
use intake::{Classification, PreparedSource};
use runtime::command::ScanCommit;
use runtime::ordering::ScanCapability;
use runtime::receipt::{Deferred, DurableReceipt, Receipt, ResponsibilityLookup};
#[cfg(test)]
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
        f.write_str(match &self.0 {
            IntakeAdmission::LedgerReceived(_) => "received ledger admission retains custody",
            IntakeAdmission::LedgerDeferred(value) => match value.recovery {
                Recovery::Rescan => "ledger admission retains responsibility for rescan",
                Recovery::RestoreOriginal => {
                    "ledger admission retains responsibility for original restore"
                }
            },
            _ => "ledger admission retains responsibility",
        })
    }
}
impl std::error::Error for LedgerOutcome {}

pub(in crate::services::discord) fn retained(
    admission: IntakeAdmission,
) -> crate::services::discord::Error {
    Box::new(LedgerOutcome(admission))
}

pub(super) fn is_ledger(admission: &IntakeAdmission) -> bool {
    matches!(
        admission,
        IntakeAdmission::LedgerReceived(_) | IntakeAdmission::LedgerDeferred(_)
    )
}

pub(in crate::services::discord) fn deferred(
    original: PreparedSource,
    reason: DeferReason,
    recovery: Recovery,
) -> IntakeAdmission {
    IntakeAdmission::LedgerDeferred(Box::new(LedgerDeferred {
        original,
        reason,
        recovery,
    }))
}

fn covers(receipt: &DurableReceipt, original: &PreparedSource) -> bool {
    #[cfg(test)]
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

pub(in crate::services::discord) fn export_lookup(
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

pub(in crate::services::discord) fn lookup_for_rescan(
    original: PreparedSource,
    lookup: Result<ResponsibilityLookup, Deferred>,
) -> IntakeAdmission {
    let mut admission = export_lookup(original, lookup);
    if let IntakeAdmission::LedgerDeferred(value) = &mut admission {
        value.recovery = Recovery::Rescan;
    }
    admission
}

pub(in crate::services::discord) fn settled(admission: &IntakeAdmission) -> bool {
    matches!(admission, IntakeAdmission::LedgerReceived(value) if value.settlement == Settlement::Settled)
}

pub(super) async fn admit_submission(submission: &IntakeSubmission) -> Option<IntakeAdmission> {
    #[cfg(test)]
    if runtime::mutant("c2c2_protected_admission_bypassed") {
        return None;
    }
    let request = &submission.request;
    let access = intake::client::lookup(
        runtime::runtime::registry(),
        &submission.provider,
        request.channel_id.get(),
    );
    if matches!(access, intake::client::Access::Legacy) {
        return None;
    }
    let mut sources = std::iter::once(request.user_msg_id.get())
        .chain(request.source_message_ids.iter().map(|id| id.get()))
        .collect::<Vec<_>>();
    sources.sort_unstable();
    sources.dedup();
    let origin = if matches!(submission.origin, IntakeOrigin::SlashSkill) {
        intake::Origin::SlashInteraction {
            interaction_id: request.user_msg_id.get(),
            author_id: request.request_owner.get(),
            original_channel_id: request.channel_id.get(),
            execution_channel_id: request.channel_id.get(),
        }
    } else {
        intake::Origin::Message {
            message_id: request.user_msg_id.get(),
            source_ids: sources,
            author_id: request.request_owner.get(),
            original_channel_id: request.channel_id.get(),
            execution_channel_id: request.channel_id.get(),
        }
    };
    let original = match intake::prepare_source(intake::SourceDraft {
        origin,
        author_is_bot: false,
        original_text: request.user_text.clone(),
        prompt: request.user_text.clone(),
        reply_context: request.reply_context.clone(),
        attachments: submission
            .attachments
            .iter()
            .map(|attachment| intake::StagedAttachment {
                filename: attachment.filename.clone(),
                url: attachment.url.clone(),
                staged: None,
            })
            .collect(),
        voice_announcement: submission.voice_announcement.clone(),
    }) {
        Ok(original) => original,
        Err(error) => {
            return Some(IntakeAdmission::Blocked {
                reason: IntakeBlockedReason::RoutingDependencyFailed {
                    detail: format!("ledger source preparation failed: {error}"),
                },
            });
        }
    };
    let lookup = match access {
        intake::client::Access::Ready(client) => {
            client
                .lookup_responsibility(original.identity().clone())
                .await
        }
        intake::client::Access::Deferred(reason) => Err(reason),
        intake::client::Access::Legacy => unreachable!("Legacy returned before source preparation"),
    };
    Some(export_lookup(original, lookup))
}

pub(in crate::services::discord) enum ScanAdmission {
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
            #[cfg(test)]
            let settlement = if runtime::mutant("c2c1_receipt_means_settled") {
                Settlement::Settled
            } else {
                settlement
            };
            #[cfg(test)]
            if runtime::mutant("c2c1_failed_settlement_loses_receipt")
                && matches!(settlement, Settlement::Rescan(_))
            {
                return ScanAdmission::Admission {
                    admission: deferred(
                        original,
                        DeferReason::Receipt(Deferred::Order),
                        Recovery::Rescan,
                    ),
                    capability: Some(capability),
                };
            }
            #[cfg(test)]
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

pub(in crate::services::discord) async fn consume_scan(
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
            #[cfg(test)]
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
    #[cfg(test)]
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
    #[cfg(test)]
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

#[cfg(test)]
thread_local! { static TEST_ADMISSION: RefCell<Option<IntakeAdmission>> = const { RefCell::new(None) }; }
#[cfg(test)]
pub(super) fn inject_test_admission(admission: IntakeAdmission) {
    TEST_ADMISSION.with(|slot| assert!(slot.replace(Some(admission)).is_none()));
}
#[cfg(test)]
pub(super) fn take_injected_admission() -> Option<IntakeAdmission> {
    TEST_ADMISSION.with(|slot| slot.borrow_mut().take())
}
#[cfg(test)]
pub(super) fn take_test_admission(fallback: IntakeAdmission) -> IntakeAdmission {
    take_injected_admission().unwrap_or(fallback)
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
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
#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
pub(in crate::services::discord::router) use payload_support::{
    classification_for_test, original_for_test, receipt_for_test,
};
#[cfg(test)]
pub(in crate::services::discord::router) use payload_support::{
    mutate_payload, mutate_result, observe_payload, test_payloads,
};

#[cfg(test)]
mod payload_support {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    pub(in crate::services::discord::router) enum PayloadSnapshot {
        Received {
            receipt: DurableReceipt,
            original: OriginalSnapshot,
            settlement: SettlementSnapshot,
        },
        Deferred {
            original: OriginalSnapshot,
            reason: ReasonSnapshot,
            recovery: Recovery,
        },
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(in crate::services::discord::router) struct OriginalSnapshot {
        key: u64,
        identity: crate::services::tui_input::rows::receipt_identity::ReceiptIdentity,
        input: serde_json::Value,
        attachments: Vec<intake::StagedAttachment>,
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(in crate::services::discord::router) enum SettlementSnapshot {
        LookupOnly,
        Settled,
        Rescan(Failure),
    }

    #[derive(Debug, PartialEq, Eq)]
    pub(in crate::services::discord::router) enum ReasonSnapshot {
        Receipt(Deferred),
        Lookup(Responsibility),
        Legacy(DurableReceipt),
        Mismatched(DurableReceipt),
        Classification(Classification),
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(in crate::services::discord::router) fn receipt_for_test(
        admission: &IntakeAdmission,
    ) -> Option<&DurableReceipt> {
        match admission {
            IntakeAdmission::LedgerReceived(value) => Some(&value.receipt),
            IntakeAdmission::LedgerDeferred(value) => match &value.reason {
                DeferReason::Legacy(receipt) | DeferReason::Mismatched(receipt) => Some(receipt),
                _ => None,
            },
            _ => None,
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(in crate::services::discord::router) fn original_for_test(
        admission: &IntakeAdmission,
    ) -> Option<&PreparedSource> {
        match admission {
            IntakeAdmission::LedgerReceived(value) => Some(&value.original),
            IntakeAdmission::LedgerDeferred(value) => Some(&value.original),
            _ => None,
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(in crate::services::discord::router) fn classification_for_test(
        admission: &IntakeAdmission,
    ) -> Option<Classification> {
        match admission {
            IntakeAdmission::LedgerDeferred(value) => match &value.reason {
                DeferReason::Classification(classification) => Some(*classification),
                _ => None,
            },
            _ => None,
        }
    }

    pub(in crate::services::discord::router) fn observe_payload(
        admission: &IntakeAdmission,
    ) -> PayloadSnapshot {
        let original = |source: &PreparedSource| OriginalSnapshot {
            key: source.key(),
            identity: source.identity().clone(),
            input: source.input().clone(),
            attachments: source.attachments().to_vec(),
        };
        match admission {
            IntakeAdmission::LedgerReceived(value) => PayloadSnapshot::Received {
                receipt: value.receipt.clone(),
                original: original(&value.original),
                settlement: match value.settlement {
                    Settlement::LookupOnly => SettlementSnapshot::LookupOnly,
                    Settlement::Settled => SettlementSnapshot::Settled,
                    Settlement::Rescan(reason) => SettlementSnapshot::Rescan(reason),
                },
            },
            IntakeAdmission::LedgerDeferred(value) => PayloadSnapshot::Deferred {
                original: original(&value.original),
                reason: match &value.reason {
                    DeferReason::Receipt(reason) => ReasonSnapshot::Receipt(*reason),
                    DeferReason::Lookup(reason) => ReasonSnapshot::Lookup(*reason),
                    DeferReason::Legacy(receipt) => ReasonSnapshot::Legacy(receipt.clone()),
                    DeferReason::Mismatched(receipt) => ReasonSnapshot::Mismatched(receipt.clone()),
                    DeferReason::Classification(value) => ReasonSnapshot::Classification(*value),
                },
                recovery: value.recovery,
            },
            other => panic!("expected a complete ledger payload: {other:?}"),
        }
    }

    pub(in crate::services::discord::router) fn mutate_payload(
        mut admission: IntakeAdmission,
        consumer: &str,
    ) -> IntakeAdmission {
        let enabled = |axis| runtime::mutant(&format!("c2c2_payload_{consumer}_{axis}"));
        let receipt = match &mut admission {
            IntakeAdmission::LedgerReceived(value) => Some(&mut value.receipt),
            IntakeAdmission::LedgerDeferred(value) => match &mut value.reason {
                DeferReason::Legacy(receipt) | DeferReason::Mismatched(receipt) => Some(receipt),
                _ => None,
            },
            _ => None,
        };
        if let Some(receipt) = receipt {
            if enabled("seq") {
                receipt.received_seq += 1;
                println!("C2C2_PAYLOAD_MUTATED={consumer}:seq");
            }
            if enabled("channel") {
                receipt.identity.execution_channel_id += 1;
                println!("C2C2_PAYLOAD_MUTATED={consumer}:channel");
            }
        }
        match &mut admission {
            IntakeAdmission::LedgerReceived(value)
                if enabled("settlement") && matches!(value.settlement, Settlement::Rescan(_)) =>
            {
                value.settlement = Settlement::Settled;
                println!("C2C2_PAYLOAD_MUTATED={consumer}:settlement");
            }
            IntakeAdmission::LedgerDeferred(value) if enabled("reason") => {
                value.reason = DeferReason::Receipt(Deferred::Closed);
                println!("C2C2_PAYLOAD_MUTATED={consumer}:reason");
            }
            _ => {}
        }
        admission
    }

    pub(in crate::services::discord::router) fn mutate_result(
        result: Result<(), crate::services::discord::Error>,
        consumer: &str,
    ) -> Result<(), crate::services::discord::Error> {
        match result {
            Err(error) => match error.downcast::<LedgerOutcome>() {
                Ok(outcome) => Err(Box::new(LedgerOutcome(mutate_payload(outcome.0, consumer)))),
                Err(error) => Err(error),
            },
            Ok(()) => Ok(()),
        }
    }

    pub(in crate::services::discord::router) fn test_payloads(
        channel: u64,
    ) -> [IntakeAdmission; 2] {
        use sha2::{Digest, Sha256};
        let original = intake::prepare_source(intake::SourceDraft {
            origin: intake::Origin::Message {
                message_id: 10,
                source_ids: vec![10, 11],
                author_id: 7,
                original_channel_id: channel,
                execution_channel_id: channel,
            },
            author_is_bot: false,
            original_text: "rich original".into(),
            prompt: "expanded prompt".into(),
            reply_context: Some("complete reply boundary".into()),
            attachments: vec![intake::StagedAttachment {
                filename: "original.txt".into(),
                url: "https://cdn.discordapp.com/attachments/original.txt".into(),
                staged: Some(
                    crate::services::cluster::attachment_transfer::AttachmentEntryV1 {
                        filename: "original.txt".into(),
                        sha256: hex::encode(Sha256::digest(b"original staged bytes")),
                        bytes: b"original staged bytes".to_vec(),
                    },
                ),
            }],
            voice_announcement: Some(
                serde_json::from_value(serde_json::json!({
                    "transcript": "spoken original", "user_id": "7", "utterance_id": "payload-10",
                    "language": "ko", "verbose_progress": true,
                    "started_at": "2026-10-14T00:00:00Z", "completed_at": "2026-10-14T00:00:01Z",
                    "samples_written": 240, "control_channel_id": channel,
                    "stt_mode": "fixture", "stt_latency_ms": 17
                }))
                .unwrap(),
            ),
        })
        .unwrap();
        let receipt = DurableReceipt {
            key: original.key(),
            received_seq: 41,
            identity: original.identity().clone(),
        };
        [
            IntakeAdmission::LedgerReceived(Box::new(LedgerReceived {
                receipt: receipt.clone(),
                original: original.clone(),
                settlement: Settlement::Rescan(Failure::StalePermit),
            })),
            deferred(
                original,
                DeferReason::Legacy(receipt),
                Recovery::RestoreOriginal,
            ),
        ]
    }
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
#[path = "ledger_tests.rs"]
mod tests;
