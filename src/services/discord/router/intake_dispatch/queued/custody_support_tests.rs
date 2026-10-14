use super::{
    Arc, IntakeAdmission, Intervention, QueuedAdmissionDisposition, QueuedLedgerOutcome, intake,
    runtime,
};

impl QueuedAdmissionDisposition {
    pub(in crate::services::discord) fn inject_ledger_for_test(channel: u64, received: bool) {
        let [receipt, deferred] = super::super::ledger::test_payloads(channel);
        super::super::ledger::inject_test_admission(if received { receipt } else { deferred });
    }

    pub(in crate::services::discord) fn inject_ledger_for_original_test(
        channel: u64,
        original: &Intervention,
        received_seq: u64,
        received: bool,
    ) {
        let prepared = intake::prepare_source(intake::SourceDraft {
            origin: intake::Origin::Message {
                message_id: original.message_id.get(),
                source_ids: original
                    .source_message_ids
                    .iter()
                    .map(|id| id.get())
                    .collect(),
                author_id: original.author_id.get(),
                original_channel_id: channel,
                execution_channel_id: channel,
            },
            author_is_bot: original.author_is_bot,
            original_text: original.text.clone(),
            prompt: original.text.clone(),
            reply_context: original.reply_context.clone(),
            attachments: Vec::new(),
            voice_announcement: original.voice_announcement.clone(),
        })
        .expect("test original must have complete provenance");
        let receipt = runtime::receipt::DurableReceipt {
            key: prepared.key(),
            received_seq,
            identity: prepared.identity().clone(),
        };
        let admission = if received {
            IntakeAdmission::LedgerReceived(Box::new(super::super::ledger::LedgerReceived {
                receipt,
                original: prepared,
                settlement: super::super::ledger::Settlement::Rescan(
                    crate::services::discord::input_runtime::fence::Failure::StalePermit,
                ),
            }))
        } else {
            super::super::ledger::deferred(
                prepared,
                super::super::ledger::DeferReason::Legacy(receipt),
                super::super::ledger::Recovery::RestoreOriginal,
            )
        };
        super::super::ledger::inject_test_admission(admission);
    }
}

tokio::task_local! {
    pub(super) static TEST_REGISTRY: &'static crate::services::discord::input_runtime::supervisor::Registry;
}

thread_local! {
    pub(super) static TEST_FALLBACK_ADMISSION: std::cell::RefCell<Option<IntakeAdmission>> = const { std::cell::RefCell::new(None) };
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(super) async fn with_registry_for_test<T>(
    registry: &'static crate::services::discord::input_runtime::supervisor::Registry,
    future: impl std::future::Future<Output = T>,
) -> T {
    TEST_REGISTRY.scope(registry, future).await
}

impl QueuedLedgerOutcome {
    pub(in crate::services::discord) fn proof_for_test(&self) -> Option<String> {
        self.admission
            .as_ref()
            .map(|proof| format!("{:?}", super::super::ledger::observe_payload(proof)))
    }
}

pub(super) fn guard(value: bool, mutant: &str) -> bool {
    value && !runtime::mutant(mutant)
}

pub(super) fn lookup_identity(
    prepared: &intake::PreparedSource,
) -> crate::services::tui_input::rows::receipt_identity::ReceiptIdentity {
    let mut identity = prepared.identity().clone();
    if runtime::mutant("c2c2_queued_primary_only_lookup") {
        identity.source_ids = vec![prepared.key()];
    }
    identity
}

pub(super) fn dispatch_lease(
    lease: &Arc<crate::services::turn_orchestrator::DispatchLease>,
) -> Arc<crate::services::turn_orchestrator::DispatchLease> {
    if runtime::mutant("c2c2_queued_invents_dispatch_lease") {
        Arc::new(crate::services::turn_orchestrator::DispatchLease)
    } else {
        lease.clone()
    }
}

pub(super) fn original_for_restore(original: &Intervention) -> Intervention {
    let mut original = original.clone();
    if runtime::mutant("c2c2_queued_text_only_restore") {
        original.pending_uploads.clear();
        original.reply_context = None;
        original.voice_announcement = None;
    }
    original
}

pub(super) async fn before_custody(
    deps: &crate::services::discord::router::IntakeDeps<'_>,
    outcome: &QueuedLedgerOutcome,
) {
    if runtime::mutant("c2c2_queued_ui_before_custody") {
        crate::services::discord::queue_dispatch::settle_received_queued_custody(
            deps.shared,
            deps.http,
            &outcome.provider,
            outcome.channel_id,
            &outcome.original,
            "queued ledger early custody mutant",
        )
        .await;
    }
}

pub(super) fn finish_ledger(
    mut outcome: Box<QueuedLedgerOutcome>,
) -> Result<crate::services::discord::router::IntakeExecution, crate::services::discord::Error> {
    if runtime::mutant("c2c1_queued_ledger_means_executed") {
        return Ok(crate::services::discord::router::IntakeExecution::Executed);
    }
    if runtime::mutant("c2c1_queued_ledger_drops_original") {
        outcome.original.pending_uploads.clear();
    }
    if runtime::mutant("c2c1_queued_ledger_drops_lease") {
        return Err(Box::new(super::super::ledger::LedgerOutcome(
            outcome.admission.unwrap(),
        )));
    }
    outcome.admission = outcome
        .admission
        .map(|admission| super::super::ledger::mutate_payload(admission, "queued"));
    Err(outcome)
}
