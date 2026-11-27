//! Source evidence creates a result; live or restored custody alone grants its cleanup rights.
use super::*;
use crate::services::tui_input::human_frame::history::v2::{
    Registry,
    native::{Outcome, Ticket},
};
use crate::services::tui_o::writer::{
    actor::native_input::{EvidencePort, NativePort, Snapshot},
    binding::{BindingEvents, BindingRecord, BindingTarget},
    input_facts::native::Observation,
    input_notices::NoticeOrigin,
};
use std::sync::Mutex;
#[path = "native_abstain.rs"]
pub(in crate::services::discord::turn_finalizer) mod native_abstain;
#[path = "native_completion.rs"]
pub(in crate::services::discord::turn_finalizer) mod native_completion;
#[path = "native_terminal.rs"]
pub(in crate::services::discord) mod native_terminal;
#[path = "native_binding.rs"]
pub(in crate::services::discord) mod source_binding;

pub(in crate::services::discord::turn_finalizer) type Custody = (
    Vec<NativePrepared>,
    Vec<super::super::native_prepare::RestoredInput>,
);
#[derive(Clone)]
enum Job {
    Abstain(Box<native_abstain::Work>),
    Live(Box<NativeCleanup>, Registry),
    Boot(
        Box<super::super::native_prepare::RestoredInput>,
        Registry,
        Option<(Outcome, String)>,
    ),
}
struct Port {
    channel: u64,
    shared: Arc<SharedData>,
    bindings: Arc<dyn BindingEvents>,
    pending: Mutex<HashMap<String, Job>>,
}

pub(in crate::services::discord) fn port(
    channel: u64,
    shared: Arc<SharedData>,
    bindings: Arc<dyn BindingEvents>,
) -> NativePort {
    NativePort {
        inner: Arc::new(Port {
            channel,
            shared,
            bindings,
            pending: Mutex::new(HashMap::new()),
        }),
    }
}
fn input_id(namespace: &str, ticket: &Ticket) -> Result<String, String> {
    serde_json::to_string(&(namespace, &ticket.key)).map_err(|e| e.to_string())
}

impl Port {
    async fn custody(&self) -> Result<Custody, String> {
        let (ack, rx) = oneshot::channel();
        self.shared
            .turn_finalizer
            .tx
            .send(FinalizeMsg::NativeInputs {
                channel: ChannelId::new(self.channel),
                ack,
            })
            .map_err(|_| "native custody actor unavailable")?;
        rx.await
            .map_err(|_| "native custody acknowledgement lost".into())
    }
    fn origin(&self, origin: &NoticeOrigin) -> Result<(), String> {
        let events = self.bindings.binding_events_since(self.channel, 0)?;
        let event = events
            .iter()
            .find(|e| e.seq == origin.binding_seq)
            .ok_or("native binding missing")?;
        let source = match &event.record {
            BindingRecord::Bound {
                new: BindingTarget::Source(source),
                ..
            } => Some(source),
            BindingRecord::Bound {
                new: BindingTarget::Pending { .. },
                ..
            } => events.iter().find_map(|e| match &e.record {
                BindingRecord::Resolved {
                    resolves_seq,
                    source,
                } if *resolves_seq == event.seq => Some(source),
                _ => None,
            }),
            _ => None,
        };
        if event.channel_id != self.channel
            || event.provider != crate::services::tui_o::shadow::ShadowProvider::Claude
            || event.tmux_session != origin.logical_session
            || event.execution_nonce != origin.execution_nonce
            || origin.binding_seq == 0
            || origin.execution_nonce.is_empty()
            || source != Some(&origin.record.source)
        {
            return Err("native source binding superseded".into());
        }
        Ok(())
    }
    async fn registrations(
        &self,
    ) -> Result<Vec<crate::services::tui_o::writer::input_facts::native::Registration>, String>
    {
        let (live, boot) = self.custody().await?;
        let mut histories = Vec::new();
        for input in live {
            if !histories
                .iter()
                .any(|history: &Registry| history.same_control_registry(&input.history))
            {
                histories.push(input.history);
            }
        }
        if !boot.is_empty() {
            let history = Registry::runtime()?;
            if !histories.iter().any(|h| h.same_control_registry(&history)) {
                histories.push(history);
            }
        }
        let mut registrations = Vec::new();
        for history in histories {
            registrations.extend(history.native_classification_registrations()?);
        }
        Ok(registrations)
    }
    fn saved_live(&self, input: &NativePrepared) -> Result<Option<Job>, String> {
        if input.key.generation != self.shared.restart.current_generation {
            return Ok(None);
        }
        let ticket = input.bound_ticket(&input.history)?;
        let record = input
            .history
            .native_boot_record(&input.namespace, &ticket)?;
        let Some(outcome) = &record.settlement.outcome else {
            return Ok(None);
        };
        if record.settlement.cleanup_complete || outcome.extra.get("native_proof").is_none() {
            return Ok(None);
        }
        super::super::native_prepare::validate_saved_record(&record)?;
        let evidence = serde_json::from_value(outcome.extra["native_proof"].clone())
            .map_err(|e| e.to_string())?;
        let cleanup = input.actor.as_ref().map(|actor| (input.key, actor.clone()));
        let admission = NativeAdmission {
            evidence,
            registrations_fingerprint: String::new(),
            cleanup,
        };
        let work =
            NativeCleanup::capture(admission, input.row_witness.clone(), input.started_before)?;
        Ok(Some(Job::Live(Box::new(work), input.history.clone())))
    }
    async fn durable(history: Registry, namespace: String, ticket: Ticket) -> Result<(), String> {
        tokio::task::spawn_blocking(move || {
            history
                .try_native_settlement(&namespace, &ticket)
                .map(|_| ())
        })
        .await
        .map_err(|e| e.to_string())?
    }
    async fn receipt_input(
        &self,
        observed: Observation,
        current: Observation,
        index: usize,
        origin: NoticeOrigin,
    ) -> Result<(), String> {
        self.origin(&origin)?;
        if observed != current
            || current.binding.channel_id != self.channel
            || current.binding.source != origin.record.source
            || current.binding_seq != origin.binding_seq
            || current.generation != self.shared.restart.current_generation
        {
            return Err("native receipt observation superseded".into());
        }
        let receipt = current
            .receipts
            .get(index)
            .ok_or("native receipt offer missing")?
            .clone();
        let (namespace, ticket) = receipt.identity();
        let source = ticket
            .source
            .as_ref()
            .ok_or("native receipt source missing")?;
        check_target(&source.target, &origin)?;
        if !(crate::services::tui_o::writer::input_facts::native::Registration {
            namespace: namespace.into(),
            ticket: ticket.clone(),
            frame_hash: String::new(),
            exact_frame_hash: String::new(),
            receipts: Vec::new(),
            candidate: false,
            barrier: None,
        })
        .matches(&current.binding, current.proven)
        {
            return Err("native receipt source differs".into());
        }
        let (live, boot) = self.custody().await?;
        // Custody carries the queue owner's actual provider/token namespace, including old debt.
        let history = live
            .iter()
            .find(|p| p.namespace == namespace)
            .map(|p| p.history.clone())
            .or_else(|| {
                boot.iter()
                    .any(|p| p.identity().0 == namespace)
                    .then(Registry::runtime)
                    .and_then(Result::ok)
            })
            .ok_or("native receipt domain custody missing")?;
        tokio::task::spawn_blocking(move || {
            let (namespace, ticket) = receipt.identity();
            let (kind, row, range, hash) = receipt.record();
            history.try_native_receipt(
                namespace,
                ticket,
                (kind, row, range, current.through, hash),
                &current.registrations_fingerprint,
            )
        })
        .await
        .map_err(|e| e.to_string())?
    }
    async fn submit(&self, id: &str, job: Job) -> Result<bool, String> {
        let done = match job {
            Job::Abstain(_) => return Ok(false),
            Job::Live(work, history) => {
                let e = &work.admission.evidence;
                let settlement = history.native_settlement(&e.namespace, &e.ticket)?;
                if settlement.outcome.is_none() {
                    Self::durable(history.clone(), e.namespace.clone(), e.ticket.clone()).await?;
                }
                if settlement
                    .outcome
                    .as_ref()
                    .is_some_and(|o| work.outcome().as_ref() != Ok(o))
                {
                    true
                } else {
                    self.shared
                        .turn_finalizer
                        .submit_native(*work, history, self.shared.clone())
                        .await?
                }
            }
            Job::Boot(input, history, proof) => {
                let (namespace, ticket) = input.identity();
                if let Some((outcome, fingerprint)) = proof
                    && history
                        .native_settlement(namespace, ticket)?
                        .outcome
                        .is_none()
                {
                    Self::durable(history.clone(), namespace.into(), ticket.clone()).await?;
                    history.record_native_outcome_at_stamp(
                        namespace,
                        ticket,
                        &outcome,
                        &fingerprint,
                    )?;
                }
                input.settle_saved(&self.shared).await?
            }
        };
        if done {
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(id);
        }
        Ok(done)
    }
    async fn abstain(
        &self,
        observed: Observation,
        current: Observation,
        proofs: (
            Vec<crate::services::tui_o::writer::input_facts::native::Barrier>,
            Vec<crate::services::tui_o::writer::input_facts::native::Barrier>,
        ),
        idle: bool,
        origin: NoticeOrigin,
    ) -> Result<(), String> {
        self.origin(&origin)?;
        if observed != current
            || proofs.0 != proofs.1
            || current.binding.channel_id != self.channel
            || current.binding.source != origin.record.source
            || current.binding_seq != origin.binding_seq
            || current.generation != self.shared.restart.current_generation
        {
            return Err("abstain observation superseded".into());
        }
        let (live, boot) = self.custody().await?;
        for proof in &proofs.0 {
            let source = proof
                .ticket
                .source
                .as_ref()
                .ok_or("abstain source missing")?;
            check_target(&source.target, &origin)?;
            if !(crate::services::tui_o::writer::input_facts::native::Registration {
                namespace: proof.namespace.clone(),
                ticket: proof.ticket.clone(),
                frame_hash: String::new(),
                exact_frame_hash: String::new(),
                receipts: Vec::new(),
                candidate: false,
                barrier: None,
            })
            .matches(&current.binding, current.proven)
                || proof.through != current.through
            {
                return Err("abstain source mismatch".into());
            }
            let history = live
                .iter()
                .find(|p| p.namespace == proof.namespace && p.ticket.key == proof.ticket.key)
                .map(|p| p.history.clone())
                .or_else(|| {
                    boot.iter()
                        .find(|p| {
                            let (ns, t) = p.identity();
                            ns == proof.namespace && t.key == proof.ticket.key
                        })
                        .and_then(|_| Registry::runtime().ok())
                })
                .ok_or("abstain custody missing")?;
            if history
                .native_settlement(&proof.namespace, &proof.ticket)?
                .barrier(&proof.namespace)?
                .is_none()
            {
                Self::durable(
                    history.clone(),
                    proof.namespace.clone(),
                    proof.ticket.clone(),
                )
                .await?;
            }
            history.record_native_barrier(proof)?;
        }
        if !idle {
            return Ok(());
        }
        let events = self.bindings.binding_events_since(self.channel, 0)?;
        if events
            .iter()
            .rev()
            .find(|e| {
                e.channel_id == self.channel
                    && e.provider == current.binding.provider
                    && matches!(e.record, BindingRecord::Bound { .. })
            })
            .is_none_or(|e| e.seq != origin.binding_seq)
        {
            return Err("abstain current binding changed".into());
        }
        for input in live {
            let ticket = input.bound_ticket(&input.history)?;
            let saved = input.history.native_settlement(&input.namespace, &ticket)?;
            if saved.outcome.is_some() || saved.barrier(&input.namespace)?.is_none() {
                continue;
            }
            let id = input_id(&input.namespace, &ticket)?;
            let retained = self
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&id)
                .cloned();
            let work = match retained {
                Some(Job::Abstain(work)) => *work,
                _ => native_abstain::Work::capture(input, &self.shared)?,
            };
            self.pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id.clone(), Job::Abstain(Box::new(work.clone())));
            if work.drive(&self.shared).await? {
                self.pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
            }
        }
        if !boot.is_empty() {
            let history = Registry::runtime()?;
            for input in boot {
                let input = input.refreshed(&history)?;
                let (ns, ticket) = input.identity();
                let saved = history.native_settlement(ns, ticket)?;
                if saved.outcome.is_none() && saved.barrier(ns)?.is_some() && !input.settled {
                    input.abstain_idle(&self.shared, &history).await?;
                }
            }
        }
        Ok(())
    }

    async fn offer_input(
        &self,
        observed: Observation,
        current: Observation,
        index: usize,
        origin: NoticeOrigin,
    ) -> Result<bool, String> {
        self.origin(&origin)?;
        if observed.binding.channel_id != self.channel
            || observed.binding.source != origin.record.source
            || observed.binding_seq != origin.binding_seq
            || observed.generation != self.shared.restart.current_generation
        {
            return Err("native owner stamp superseded".into());
        }
        let evidence = observed.evidence.get(index).ok_or("native offer missing")?;
        let id = input_id(&evidence.namespace, &evidence.ticket)?;
        let (live, boot) = self.custody().await?;
        let job = if let Some(prepared) = live.into_iter().find(|p| {
            p.ticket.channel_id == evidence.ticket.channel_id
                && p.namespace == evidence.namespace
                && p.ticket.key == evidence.ticket.key
        }) {
            if prepared.key.generation != observed.generation {
                return Err("native offer cannot adopt old-generation debt".into());
            }
            let ticket = prepared.bound_ticket(&prepared.history)?;
            if ticket != evidence.ticket {
                return Err("native offer ticket changed".into());
            }
            let settlement = prepared
                .history
                .native_settlement(&prepared.namespace, &ticket)?;
            if settlement.outcome.is_some() {
                return Ok(settlement.cleanup_complete);
            }
            let target = &ticket.source.as_ref().ok_or("native source absent")?.target;
            check_target(target, &origin)?;
            let work = prepared
                .admit(
                    &observed,
                    &current,
                    index,
                    target,
                    &prepared.history,
                    &self.shared,
                )
                .await?;
            let (ack, rx) = oneshot::channel();
            self.shared
                .turn_finalizer
                .tx
                .send(FinalizeMsg::NativeRemember {
                    work: Box::new(work.clone()),
                    ack,
                })
                .map_err(|_| "native row custody unavailable")?;
            rx.await
                .map_err(|_| "native row custody acknowledgement lost")??;
            Job::Live(Box::new(work), prepared.history)
        } else {
            let input = boot
                .into_iter()
                .find(|p| {
                    let (ns, t) = p.identity();
                    ns == evidence.namespace && t.key == evidence.ticket.key
                })
                .ok_or("native offer has no prepared custody")?;
            let history = Registry::runtime()?;
            let input = input.refreshed(&history)?;
            let (namespace, ticket) = input.identity();
            if ticket != &evidence.ticket
                || ticket
                    .owning_episode
                    .as_ref()
                    .is_some_and(|e| e.generation >= observed.generation)
            {
                return Err("native boot offer identity changed".into());
            }
            let settlement = history.native_settlement(namespace, ticket)?;
            if settlement.outcome.is_some() {
                return input.settle_saved(&self.shared).await;
            }
            if settlement.extra.get("any_key") != Some(&serde_json::Value::Bool(true)) {
                return Err("native boot original has no key receipt".into());
            }
            let target = &ticket
                .source
                .as_ref()
                .ok_or("native boot source absent")?
                .target;
            check_target(target, &origin)?;
            let proof = source_evidence(&observed, &current, index, target)?;
            let outcome = Outcome {
                disposition: proof.disposition.clone(),
                range: Some([proof.range.0, proof.range.1]),
                extra: serde_json::Map::from_iter([(
                    "native_proof".into(),
                    serde_json::to_value(&proof).map_err(|e| e.to_string())?,
                )]),
            };
            Job::Boot(
                Box::new(input),
                history,
                Some((outcome, observed.registrations_fingerprint.clone())),
            )
        };
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), job.clone());
        self.submit(&id, job).await
    }
}
fn check_target(target: &serde_json::Value, origin: &NoticeOrigin) -> Result<(), String> {
    if target["owner"] != origin.logical_session
        || target["execution_nonce"] != origin.execution_nonce
    {
        return Err("native target differs from the source owner".into());
    }
    Ok(())
}
impl EvidencePort for Port {
    fn snapshot(
        &self,
        origin: NoticeOrigin,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Snapshot, String>> + Send + '_>>
    {
        Box::pin(async move {
            self.origin(&origin)?;
            Ok(Snapshot {
                generation: self.shared.restart.current_generation,
                registrations: self.registrations().await?,
            })
        })
    }
    fn offer(
        &self,
        observed: Observation,
        current: Observation,
        index: usize,
        origin: NoticeOrigin,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>> + Send + '_>>
    {
        Box::pin(self.offer_input(observed, current, index, origin))
    }
    fn abstain(
        &self,
        observed: Observation,
        current: Observation,
        proofs: (
            Vec<crate::services::tui_o::writer::input_facts::native::Barrier>,
            Vec<crate::services::tui_o::writer::input_facts::native::Barrier>,
        ),
        idle: bool,
        origin: NoticeOrigin,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(Port::abstain(self, observed, current, proofs, idle, origin))
    }
    fn receipt(
        &self,
        observed: Observation,
        current: Observation,
        index: usize,
        origin: NoticeOrigin,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(self.receipt_input(observed, current, index, origin))
    }
    fn terminal(
        &self,
        observed: Observation,
        ticket: crate::services::tui_o::writer::input_facts::native::Registration,
        tail: crate::services::tui_o::writer::input_facts::native::terminal::Tail,
        origin: NoticeOrigin,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = crate::services::tui_o::writer::actor::native_input::Terminal,
                > + Send
                + '_,
        >,
    > {
        Box::pin(self.terminal_proof(observed, ticket, tail, origin))
    }
    fn retry(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            self.retry_source_bindings().await;
            let mut jobs: HashMap<_, _> = self
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Ok((live, boot)) = self.custody().await {
                for input in live {
                    if let Ok(Some(job)) = self.saved_live(&input)
                        && let Ok(id) = input_id(&input.namespace, &input.ticket)
                    {
                        jobs.insert(id, job);
                    }
                }
                if let Ok(history) = Registry::runtime() {
                    for input in boot {
                        if let Ok(input) = input.refreshed(&history) {
                            let (ns, t) = input.identity();
                            if let Ok(saved) = history.native_settlement(ns, t)
                                && saved.outcome.is_some()
                                && !saved.cleanup_complete
                                && let Ok(id) = input_id(ns, t)
                            {
                                jobs.insert(id, Job::Boot(Box::new(input), history.clone(), None));
                            }
                        }
                    }
                }
            }
            for (id, job) in jobs {
                if let Err(error) = self.submit(&id, job).await {
                    tracing::debug!(%error,"native acknowledgement remains pending");
                }
            }
        })
    }
}

pub(in crate::services::discord::turn_finalizer) fn remember(
    ledger: &mut HashMap<LedgerKey, LedgerEntry>,
    work: &NativeCleanup,
) -> Result<(), String> {
    let evidence = &work.admission.evidence;
    let prepared = ledger
        .values_mut()
        .flat_map(|e| e.native_inputs.values_mut())
        .find(|p| {
            p.ticket.channel_id == evidence.ticket.channel_id
                && p.namespace == evidence.namespace
                && p.ticket.key == evidence.ticket.key
        })
        .ok_or("native prepared custody disappeared")?;
    if prepared.ticket.owning_episode != evidence.ticket.owning_episode
        || prepared.ticket.extra != evidence.ticket.extra
        || prepared
            .ticket
            .source
            .as_ref()
            .is_some_and(|s| Some(s) != evidence.ticket.source.as_ref())
        || match (&prepared.actor, &work.admission.cleanup) {
            (None, None) => false,
            (Some(actor), Some((key, current))) => {
                !Arc::ptr_eq(actor, current) || prepared.key.exact_key() != key.exact_key()
            }
            _ => true,
        }
    {
        return Err("native prepared custody changed".into());
    }
    if let (Some(old), Some(new)) = (&prepared.row_witness, &work.row)
        && (old.started_at != new.started_at || old.turn_start_offset != new.turn_start_offset)
    {
        return Err("native row custody changed".into());
    }
    if prepared.row_witness.is_none() {
        prepared.row_witness = work.row.clone();
    }
    Ok(())
}
pub(in crate::services::discord::turn_finalizer) fn finish_follower(
    ledger: &mut HashMap<LedgerKey, LedgerEntry>,
    work: &NativeCleanup,
) {
    if work.primary_key().is_some() {
        return;
    }
    let e = &work.admission.evidence;
    for entry in ledger.values_mut() {
        let before = entry.native_inputs.len();
        entry.native_inputs.retain(|_, p| {
            !(p.actor.is_none()
                && p.ticket.channel_id == e.ticket.channel_id
                && p.namespace == e.namespace
                && p.ticket.key == e.ticket.key)
        });
        if entry.native_inputs.len() < before
            && entry.native_inputs.is_empty()
            && entry.native_boot_inputs.is_empty()
            && entry.completion_admission.native_input.is_none()
            && entry.turn_key.channel_id.get() == e.ticket.channel_id
            && entry.turn_key.user_msg_id
                == e.ticket.key["identity"]["primary"].as_u64().unwrap_or(0)
        {
            entry.phase = Phase::Finalized;
            entry.finalized_at = Some(Instant::now());
        }
    }
}

pub(super) fn source_evidence(
    observed: &Observation,
    current: &Observation,
    index: usize,
    current_target: &serde_json::Value,
) -> Result<crate::services::tui_o::writer::input_facts::native::Evidence, String> {
    use crate::services::tui_input::human_frame::history::v2::native::Disposition;
    if observed.binding != current.binding
        || observed.generation != current.generation
        || observed.binding_seq != current.binding_seq
        || observed.revision != current.revision
        || observed.through != current.through
        || observed.registrations_fingerprint != current.registrations_fingerprint
        || observed.terminal != current.terminal
    {
        return Err("native observation superseded".into());
    }
    let evidence = observed
        .evidence
        .get(index)
        .ok_or("native candidate missing")?;
    let source = evidence
        .ticket
        .source
        .as_ref()
        .ok_or("native source missing")?;
    source.validate(&evidence.ticket.key)?;
    if observed.binding.provider != crate::services::tui_o::shadow::ShadowProvider::Claude
        || source.target != *current_target
        || evidence.namespace.is_empty()
        || evidence.ticket.channel_id != observed.binding.channel_id
        || source.provider != "claude"
        || source.session_id != observed.binding.source.session_id
        || source.transcript != observed.binding.source.path
        || !crate::services::tui_o::shadow::capture::names_source_file(
            &observed.binding.source,
            observed.proven,
            source.file,
        )
        || evidence.range.0 < source.offset
        || evidence.range.0 >= evidence.range.1
        || !crate::services::tui_o::writer::input_facts::native::terminal::within(
            observed, evidence,
        )
    {
        return Err("native proof source/range mismatch".into());
    }
    match evidence.disposition {
        Disposition::Executed | Disposition::Cancelled => {
            let membership = evidence
                .membership_range
                .ok_or("native membership missing")?;
            let closer = evidence.closer_range.ok_or("native closer missing")?;
            if evidence.run.is_none()
                || membership.0 < source.offset
                || membership.0 >= membership.1
                || membership.1 != evidence.range.0
                || closer.0 < membership.1
                || closer.0 >= closer.1
                || closer.1 != evidence.range.1
            {
                return Err("native run boundary mismatch".into());
            }
        }
        Disposition::WithdrawnNotExecuted | Disposition::TransferredUnverified => {
            if evidence.run.is_some()
                || evidence.membership_range.is_some()
                || evidence.closer_range.is_some()
                || evidence.membership_row.is_some()
            {
                return Err("native withdrawal carries run authority".into());
            }
        }
        Disposition::OperatorReleased => {
            return Err("native observation is not operator release".into());
        }
    }

    Ok(evidence.clone())
}
