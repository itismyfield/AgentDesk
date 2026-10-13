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
mod tests {
    use super::*;
    use crate::services::discord::input_runtime::fence::{Gate, test_health};
    use crate::services::discord::router::message_handler::{IntakeDeps, IntakeRequest};
    use crate::services::discord::{SharedData, make_shared_data_for_tests};
    use crate::services::tui_input::rows::receipt_identity::ReceiptIdentity;
    use futures::FutureExt;
    use poise::serenity_prelude as serenity;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn assert_persisted(evidence: &runtime::ScanFixtureEvidence) {
        assert!(
            evidence.persisted_row.is_some(),
            "scan consumer must append a real WAL receipt"
        );
        assert_eq!(evidence.open_rows, 1);
        assert_eq!(
            evidence.persisted_identity.as_ref(),
            Some(&evidence.original_identity)
        );
        assert_eq!(
            evidence.persisted_row.as_ref().unwrap().input,
            evidence.original_input
        );
    }

    fn received(admission: IntakeAdmission) -> LedgerReceived {
        assert!(
            matches!(admission, IntakeAdmission::LedgerReceived(_)),
            "actual receipt must remain a typed ledger receipt: {admission:?}"
        );
        let IntakeAdmission::LedgerReceived(received) = admission else {
            unreachable!()
        };
        *received
    }

    fn split(outcome: ScanAdmission) -> (IntakeAdmission, ScanCapability) {
        let ScanAdmission::Admission {
            admission,
            capability,
        } = outcome
        else {
            panic!("scan must return its typed admission and capability")
        };
        assert!(
            capability.is_some(),
            "an answered scan preserves the actual capability"
        );
        (admission, capability.unwrap())
    }

    fn terminal(channel: u64) -> PreparedSource {
        intake::prepare_source(intake::SourceDraft {
            origin: intake::Origin::Message {
                message_id: 12,
                source_ids: vec![12],
                author_id: 7,
                original_channel_id: channel,
                execution_channel_id: channel,
            },
            author_is_bot: false,
            original_text: "terminal".into(),
            prompt: "terminal".into(),
            reply_context: None,
            attachments: vec![],
            voice_announcement: None,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn c2c1_scan_consumer_commits_target_settles_nontarget_and_preserves_capability() {
        let (admission, evidence) = runtime::run_scan_fixture(
            963_259_850_001,
            false,
            |registry, client, original, capability| async move {
                let (admission, capability) = split(
                    consume_scan(
                        registry,
                        &ProviderKind::Claude,
                        original.clone(),
                        Classification::Target,
                        capability,
                    )
                    .await,
                );
                let outcome = consume_scan(
                    registry,
                    &ProviderKind::Claude,
                    terminal(original.identity().execution_channel_id),
                    Classification::NonTarget,
                    capability,
                )
                .await;
                assert!(
                    matches!(outcome, ScanAdmission::NonTarget(_)),
                    "NonTarget must settle through the existing scan command"
                );
                let ScanAdmission::NonTarget(capability) = outcome else {
                    unreachable!()
                };
                assert!(
                    client.complete_scan(capability).await.is_ok(),
                    "the original capability completes its exact prefix"
                );
                admission
            },
        )
        .await;
        assert_persisted(&evidence);
        let received = received(admission);
        assert_eq!(received.settlement, Settlement::Settled);
        assert_eq!(received.receipt.identity, evidence.original_identity);
        assert_eq!(
            received.receipt.received_seq,
            evidence.persisted_row.unwrap().since_seq
        );
        assert_eq!(received.original.input(), &evidence.original_input);
        assert!(evidence.pending.sources.is_empty());
    }

    #[tokio::test]
    async fn c2c1_received_survives_actual_wal_then_failed_settlement() {
        let (admission, evidence) = runtime::run_scan_fixture(
            963_259_850_002,
            true,
            |_, client, original, capability| async move {
                let applied = intake::client::apply_from_scan(
                    &client,
                    Classification::Target,
                    original.clone(),
                    capability,
                )
                .await
                .unwrap();
                let intake::client::Applied::Receipt(commit) = applied else {
                    panic!("actual commit expected")
                };
                let (admission, _) = split(export_scan(original, commit));
                admission
            },
        )
        .await;
        assert_persisted(&evidence);
        let received = received(admission);
        assert!(
            matches!(received.settlement, Settlement::Rescan(_)),
            "WAL success must not invent scan settlement success"
        );
        assert_eq!(received.receipt.identity, evidence.original_identity);
        assert_eq!(received.original.input(), &evidence.original_input);
        assert_eq!(evidence.pending.sources, vec![10, 11]);
    }

    #[tokio::test]
    async fn c2c1_rescan_reuses_the_received_row_and_refuses_the_stale_capability() {
        let ((first, second, stale_refused), evidence) = runtime::run_scan_fixture(
            963_259_850_003,
            true,
            |registry, client, original, capability| async move {
                let (first, stale) = split(
                    consume_scan(
                        registry,
                        &ProviderKind::Claude,
                        original.clone(),
                        Classification::Target,
                        capability,
                    )
                    .await,
                );
                let stale_refused = client.complete_scan(stale).await.is_err();
                let ticket = client.fetch_ticket().await.unwrap();
                let capability = client
                    .begin_scan(ticket, vec![10, 11, 12], 12, true)
                    .await
                    .unwrap();
                let (second, _) = split(
                    consume_scan(
                        registry,
                        &ProviderKind::Claude,
                        original,
                        Classification::Target,
                        capability,
                    )
                    .await,
                );
                (first, second, stale_refused)
            },
        )
        .await;
        assert_persisted(&evidence);
        assert!(stale_refused);
        let first = received(first);
        let second = received(second);
        assert!(matches!(first.settlement, Settlement::Rescan(_)));
        assert_eq!(second.settlement, Settlement::Settled);
        assert_eq!(
            first.receipt, second.receipt,
            "retry must reuse the one persisted row"
        );
        assert_eq!(first.original.input(), second.original.input());
        assert!(evidence.pending.sources.is_empty());
    }

    #[tokio::test]
    async fn c2c1_lookup_export_keeps_canonical_superset_and_lookup_only_status() {
        let ((admission, requested, mismatches), evidence) = runtime::run_scan_fixture(
            963_259_850_004,
            false,
            |_, client, original, capability| async move {
                intake::client::apply_from_scan(
                    &client,
                    Classification::Target,
                    original,
                    capability,
                )
                .await
                .unwrap();
                let requested =
                    ReceiptIdentity::new(11, vec![11], 7, 963_259_850_004, 963_259_850_004)
                        .unwrap();
                let original = intake::prepare_source(intake::SourceDraft {
                    origin: intake::Origin::Message {
                        message_id: 11,
                        source_ids: vec![11],
                        author_id: 7,
                        original_channel_id: 963_259_850_004,
                        execution_channel_id: 963_259_850_004,
                    },
                    author_is_bot: false,
                    original_text: "alias".into(),
                    prompt: "alias".into(),
                    reply_context: None,
                    attachments: vec![],
                    voice_announcement: None,
                })
                .unwrap();
                let lookup = client
                    .lookup_responsibility(requested.clone())
                    .await
                    .unwrap();
                let mismatches = [
                    (vec![10, 11], 8, 963_259_850_004, 963_259_850_004),
                    (vec![10, 11], 7, 963_259_850_005, 963_259_850_004),
                    (vec![10, 11], 7, 963_259_850_004, 963_259_850_005),
                    (vec![10, 13], 7, 963_259_850_004, 963_259_850_004),
                ]
                .into_iter()
                .map(
                    |(source_ids, author_id, original_channel_id, execution_channel_id)| {
                        let original = intake::prepare_source(intake::SourceDraft {
                            origin: intake::Origin::Message {
                                message_id: 10,
                                source_ids,
                                author_id,
                                original_channel_id,
                                execution_channel_id,
                            },
                            author_is_bot: false,
                            original_text: "changed original".into(),
                            prompt: "changed original".into(),
                            reply_context: Some("kept reply".into()),
                            attachments: vec![],
                            voice_announcement: None,
                        })
                        .unwrap();
                        let input = original.input().clone();
                        (export_lookup(original, Ok(lookup.clone())), input)
                    },
                )
                .collect::<Vec<_>>();
                (export_lookup(original, Ok(lookup)), requested, mismatches)
            },
        )
        .await;
        assert_persisted(&evidence);
        let received = received(admission);
        assert_eq!(received.settlement, Settlement::LookupOnly);
        assert_eq!(received.receipt.identity, evidence.original_identity);
        assert_eq!(received.original.identity(), &requested);
        assert_ne!(received.receipt.identity, requested);
        for (admission, input) in mismatches {
            assert!(
                matches!(admission, IntakeAdmission::LedgerDeferred(_)),
                "a canonical reply cannot take responsibility for a replaced original"
            );
            let IntakeAdmission::LedgerDeferred(deferred) = admission else {
                unreachable!()
            };
            assert!(matches!(deferred.reason, DeferReason::Mismatched(_)));
            assert_eq!(deferred.original.input(), &input);
        }
    }

    #[test]
    fn c2c1_lookup_unknown_legacy_and_transport_errors_keep_the_entire_original() {
        for responsibility in [
            Responsibility::Unknown,
            Responsibility::Absent,
            Responsibility::Conflict,
        ] {
            let original = test_original(963_259_850_005);
            let value = original.input().clone();
            let identity = original.identity().clone();
            let lookup = ResponsibilityLookup {
                responsibility,
                receipt: None,
            };
            let result = export_lookup(original, Ok(lookup));
            let IntakeAdmission::LedgerDeferred(deferred) = result else {
                panic!("unproven responsibility never grants ledger custody")
            };
            assert_eq!(deferred.original.input(), &value);
            assert_eq!(deferred.original.identity(), &identity);
            assert_eq!(deferred.reason, DeferReason::Lookup(responsibility));
            assert_eq!(deferred.recovery, Recovery::RestoreOriginal);
        }
        for reason in [
            Deferred::Closed,
            Deferred::Order,
            Deferred::Unknown,
            Deferred::Conflict,
            Deferred::Persistence,
            Deferred::SupervisorLost,
        ] {
            let original = test_original(963_259_850_005);
            let value = original.input().clone();
            let result = export_lookup(original, Err(reason));
            let IntakeAdmission::LedgerDeferred(deferred) = result else {
                panic!("transport refusal must retain the original")
            };
            assert_eq!(deferred.original.input(), &value);
            assert_eq!(deferred.reason, DeferReason::Receipt(reason));
        }
        let original = test_original(963_259_850_005);
        let proof = DurableReceipt {
            key: 10,
            received_seq: 3,
            identity: original.identity().clone(),
        };
        let lookup = ResponsibilityLookup {
            responsibility: Responsibility::Legacy {
                key: 10,
                received_seq: 3,
            },
            receipt: Some(proof.clone()),
        };
        let result = export_lookup(original.clone(), Ok(lookup));
        let IntakeAdmission::LedgerDeferred(deferred) = result else {
            panic!("old receipt is not current C custody")
        };
        assert_eq!(deferred.reason, DeferReason::Legacy(proof));
        assert_eq!(deferred.original.input(), original.input());
    }

    #[tokio::test]
    async fn c2c1_scan_off_is_ready_on_first_poll_without_mailbox_or_io() {
        let ((original, _capability), evidence) = runtime::run_scan_fixture(
            963_259_850_006,
            false,
            |registry, _, _, capability| async move {
                let original = test_original(963_259_899_006);
                let identity = original.identity().clone();
                let value = original.input().clone();
                let population = registry.population_for_test();
                let opens = crate::services::tui_input::ledger::OPENS.with(|opens| opens.get());
                let ready = consume_scan(
                    registry,
                    &ProviderKind::Claude,
                    original,
                    Classification::Target,
                    capability,
                )
                .now_or_never();
                assert!(ready.is_some(), "OFF helper must finish on its first poll");
                let ScanAdmission::Legacy {
                    original,
                    capability,
                } = ready.unwrap()
                else {
                    panic!("OFF remains Legacy")
                };
                assert_eq!(original.identity(), &identity);
                assert_eq!(original.input(), &value);
                assert_eq!(registry.population_for_test(), population);
                assert_eq!(
                    crate::services::tui_input::ledger::OPENS.with(|opens| opens.get()),
                    opens
                );
                (original, capability)
            },
        )
        .await;
        assert!(evidence.persisted_row.is_none());
        assert_eq!(original.identity().execution_channel_id, 963_259_899_006);
        assert_eq!(evidence.open_rows, 0);
        assert_eq!(evidence.pending.sources, vec![10, 11]);
    }

    fn submission(channel: u64) -> super::super::IntakeSubmission {
        let channel_id = serenity::ChannelId::new(channel);
        super::super::IntakeSubmission {
            provider: ProviderKind::Claude,
            request: IntakeRequest {
                intake_outbox_id: None,
                channel_id,
                user_msg_id: serenity::MessageId::new(10),
                source_message_ids: vec![serenity::MessageId::new(11)],
                busy_followup_retry_user_msg_id: serenity::MessageId::new(10),
                request_owner: serenity::UserId::new(7),
                request_owner_name: "owner".into(),
                user_text: "original".into(),
                reply_to_user_message: true,
                defer_watcher_resume: false,
                wait_for_completion: false,
                merge_consecutive: true,
                reply_context: None,
                has_reply_boundary: false,
                dm_hint: None,
                turn_kind: crate::services::discord::router::TurnKind::Foreground,
                preserve_on_cancel: false,
            },
            origin: super::super::IntakeOrigin::LiveMessage,
            preserve_on_cancel: false,
            has_nonportable_uploads: false,
            attachments: vec![],
            preloaded_uploads: vec![],
            voice_announcement: None,
        }
    }

    fn deps<'a>(http: &'a Arc<serenity::Http>, shared: &'a Arc<SharedData>) -> IntakeDeps<'a> {
        IntakeDeps {
            http,
            cache: None,
            ctx_for_chained_dispatch: None,
            shared,
            token: "ledger-consumer-test",
        }
    }

    #[tokio::test]
    async fn c2c1_central_results_preserve_proof_before_generic_legacy_guard() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let channel = 963_259_850_007;
        let gate = Gate::protect(ProviderKind::Claude, channel).unwrap();
        let _health = test_health::Clear::new(&gate);
        let _closing = gate.close().unwrap();
        let http = Arc::new(
            serenity::HttpBuilder::new("consumer-test")
                .proxy("http://127.0.0.1:1")
                .ratelimiter_disabled(true)
                .build(),
        );
        let shared = make_shared_data_for_tests();
        for admission in [test_received(channel), test_deferred(channel)] {
            let result = super::super::finish_text_intake_admission(
                &deps(&http, &shared),
                admission,
                submission(channel),
            )
            .await;
            assert!(result.is_err());
            let error = result.unwrap_err();
            assert!(
                error.is::<LedgerOutcome>(),
                "C evidence must survive the generic guard: {error:?}"
            );
            let outcome = error.downcast::<LedgerOutcome>().unwrap();
            assert!(is_ledger(&outcome.0));
        }
        assert!(shared.core.lock().await.sessions.is_empty());
    }

    #[tokio::test]
    async fn c2c1_live_c_results_never_checkpoint_or_report_execution() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let _mode = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "ADK_INTAKE_ROUTING_MODE",
            "disabled".as_ref(),
        );
        let channel = 963_259_850_008;
        let http = Arc::new(
            serenity::HttpBuilder::new("consumer-test")
                .proxy("http://127.0.0.1:1")
                .ratelimiter_disabled(true)
                .build(),
        );
        let shared = make_shared_data_for_tests();
        for admission in [test_received(channel), test_deferred(channel)] {
            inject_test_admission(admission);
            let checkpoints = AtomicUsize::new(0);
            let result = super::super::dispatch_live_text_intake(
                &deps(&http, &shared),
                submission(channel),
                || {
                    checkpoints.fetch_add(1, Ordering::SeqCst);
                },
            )
            .await;
            assert_eq!(
                checkpoints.load(Ordering::SeqCst),
                0,
                "ledger receipt is not a checkpoint authority"
            );
            assert!(result.unwrap_err().is::<LedgerOutcome>());
        }
        assert!(shared.core.lock().await.sessions.is_empty());
    }

    #[test]
    fn c2c1_attachment_returns_both_ledger_payloads_intact() {
        for admission in [
            test_received(963_259_850_009),
            test_deferred(963_259_850_009),
        ] {
            let kind = std::mem::discriminant(&admission);
            let result = super::super::attachment::resolve_attachment_admission(admission);
            assert!(result.is_err());
            let admission = result.unwrap_err();
            assert_eq!(std::mem::discriminant(&admission), kind);
            match admission {
                IntakeAdmission::LedgerReceived(value) => assert_eq!(
                    value.original.input(),
                    test_original(963_259_850_009).input()
                ),
                IntakeAdmission::LedgerDeferred(value) => assert_eq!(
                    value.original.input(),
                    test_original(963_259_850_009).input()
                ),
                _ => panic!("attachment must not fold C results into Legacy results"),
            }
        }
    }
}
