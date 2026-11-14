use super::completion_admission::CompletionAdmission;
use super::*;

impl TurnFinalizer {
    /// #3018 — register a turn so the ledger knows it exists before any
    /// terminal can arrive. Idempotent: a second `Start` for a key already in
    /// the ledger only refreshes the relay owner. #3016 phase-5a: `shared` is
    /// downgraded to a `Weak` carried on the `Start` so the actor primes its
    /// `cached_shared` from the first register (see `FinalizeMsg::Start`).
    pub(in crate::services::discord) fn register_start(
        &self,
        key: TurnKey,
        provider: ProviderKind,
        relay_owner: RelayOwnerKind,
        shared: &Arc<SharedData>,
    ) {
        self.register_start_with_completion_admission(
            key,
            provider,
            relay_owner,
            CompletionAdmissionPlan::Immediate,
            shared,
        );
    }

    pub(in crate::services::discord) fn register_start_with_completion_admission(
        &self,
        key: TurnKey,
        provider: ProviderKind,
        relay_owner: RelayOwnerKind,
        completion_admission_plan: CompletionAdmissionPlan,
        shared: &Arc<SharedData>,
    ) {
        let _ = self.tx.send(FinalizeMsg::Start {
            key,
            recovery_lease: crate::services::agent_recovery::lease_for_provider(
                &key.channel_id.get().to_string(),
                &provider,
            ),
            provider,
            relay_owner,
            completion_admission_plan,
            shared: Arc::downgrade(shared),
        });
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Phase {
    Pending,
    Finalizing,
    Finalized,
}

pub(super) struct LedgerEntry {
    #[cfg(unix)]
    pub(super) native_inputs: HashMap<String, super::terminal_handler::NativePrepared>,
    #[cfg(unix)]
    pub(super) native_boot_inputs: HashMap<String, super::native_prepare::RestoredInput>,
    pub(super) recovery_lease: Option<crate::services::agent_recovery::RecoveryLease>,
    pub(super) phase: Phase,
    pub(super) relay_owner: RelayOwnerKind,
    pub(super) provider: ProviderKind,
    pub(super) turn_key: TurnKey,
    pub(super) terminal_deadline: Option<Instant>,
    pub(super) watcher_backstop_deadline: Option<Instant>,
    pub(super) watcher_backstop_probe_at: Option<Instant>,
    pub(super) watcher_backstop_terminal_streak: u8,
    pub(super) watcher_backstop_deadline_pulled: bool,
    pub(super) completion_admission: CompletionAdmission,
    pub(super) finalized_at: Option<Instant>,
    /// The exact policy close this entry owns until it finalizes; never shared with another key.
    pub(super) pending_policy_commit: Option<Arc<super::terminal_handler::PolicyCommit>>,
    /// Set only when this episode's own token was released by its exact nonce: a normal terminal
    /// that removed it, or this key's policy close whose tail finished.
    pub(super) exact_release_confirmed: bool,
}

impl LedgerEntry {
    pub(super) fn native_debt_pending(&self) -> bool {
        #[cfg(unix)]
        return self.completion_admission.native_input == Some(false)
            || super::native_prepare::boot_pending(self);
        #[cfg(not(unix))]
        false
    }

    /// A fresh `Pending` entry, from `register_start` or a terminal that arrived first.
    pub(super) fn registered(
        key: TurnKey,
        provider: ProviderKind,
        relay_owner: RelayOwnerKind,
        recovery_lease: Option<crate::services::agent_recovery::RecoveryLease>,
        plan: CompletionAdmissionPlan,
        watcher_backstop_deadline: Option<Instant>,
    ) -> Self {
        Self {
            #[cfg(unix)]
            native_inputs: HashMap::new(),
            #[cfg(unix)]
            native_boot_inputs: HashMap::new(),
            recovery_lease,
            phase: Phase::Pending,
            relay_owner,
            provider,
            turn_key: key,
            terminal_deadline: None,
            watcher_backstop_deadline,
            watcher_backstop_probe_at: None,
            watcher_backstop_terminal_streak: 0,
            watcher_backstop_deadline_pulled: false,
            completion_admission: CompletionAdmission::new(plan),
            finalized_at: None,
            pending_policy_commit: None,
            exact_release_confirmed: false,
        }
    }
}

pub(super) struct PendingCompletionAdmission {
    pub(super) turn_key: TurnKey,
    pub(super) completion_admission: CompletionAdmission,
    pub(super) updated_at: Instant,
}

pub(super) enum FinalizeMsg {
    #[cfg(unix)]
    NativeBoot {
        inputs: Vec<super::native_prepare::RestoredInput>,
        step: super::native_prepare::RestoreStep,
        shared: Arc<SharedData>,
        ack: oneshot::Sender<Result<(), String>>,
    },
    #[cfg(unix)]
    NativeBootReady {
        channel: ChannelId,
        ack: oneshot::Sender<bool>,
    },
    #[cfg(unix)]
    NativeRegister {
        key: TurnKey,
        actor: Option<Arc<CancelToken>>,
        history: crate::services::tui_input::human_frame::history::v2::Registry,
        namespace: String,
        ticket: Box<crate::services::tui_input::human_frame::history::v2::native::Ticket>,
        shared: Arc<SharedData>,
        ack: oneshot::Sender<Result<String, String>>,
    },
    #[cfg(unix)]
    NativeKeyed {
        key: TurnKey,
        input: String,
        keyed: bool,
        history: crate::services::tui_input::human_frame::history::v2::Registry,
        shared: Arc<SharedData>,
        ack: oneshot::Sender<Result<(), String>>,
    },
    #[cfg(unix)]
    NativeScope {
        key: TurnKey,
        actor: usize,
        ack: oneshot::Sender<bool>,
    },
    #[cfg(unix)]
    NativeSettled {
        key: TurnKey,
        actor: usize,
        ack: oneshot::Sender<bool>,
    },
    Start {
        key: TurnKey,
        recovery_lease: Option<crate::services::agent_recovery::RecoveryLease>,
        provider: ProviderKind,
        relay_owner: RelayOwnerKind,
        completion_admission_plan: CompletionAdmissionPlan,
        shared: std::sync::Weak<SharedData>,
    },
    #[cfg(all(test, unix))]
    Native {
        work: Box<super::terminal_handler::NativeCleanup>,
        history: crate::services::tui_input::human_frame::history::v2::Registry,
        shared: Arc<SharedData>,
        ack: oneshot::Sender<Result<bool, String>>,
    },
    MailboxReleased {
        key: TurnKey,
        shared: Arc<SharedData>,
    },
    TerminalProjectionSettled {
        key: TurnKey,
        allow_queue: bool,
        shared: Arc<SharedData>,
    },
    TerminalDispositionSettled {
        key: TurnKey,
        allow_queue: bool,
        shared: Arc<SharedData>,
    },
    Terminal {
        key: TurnKey,
        provider: ProviderKind,
        event: TerminalEvent,
        ctx: FinalizeContext,
        evidence: TerminalEvidence,
        shared: Arc<SharedData>,
        ack: oneshot::Sender<FinalizeOutcome>,
    },
    PolicyClose {
        intent: Box<super::episode::PolicyCloseIntent>,
        shared: Arc<SharedData>,
        ack: oneshot::Sender<PolicyCloseOutcome>,
    },
    #[allow(dead_code)]
    AcquireDelivery {
        key: DeliveryLeaseKey,
        lease: Arc<DeliveryLeaseCell>,
        holder: LeaseHolder,
        start: u64,
        end: u64,
        deadline_ms: u64,
        ack: oneshot::Sender<bool>,
    },
    #[allow(dead_code)]
    CommitDelivery {
        key: DeliveryLeaseKey,
        lease: Arc<DeliveryLeaseCell>,
        holder: LeaseHolder,
        start: u64,
        end: u64,
        outcome: LeaseOutcome,
        provider: ProviderKind,
        tmux_session_name: String,
        shared: Arc<SharedData>,
        ack: oneshot::Sender<bool>,
    },
    #[allow(dead_code)]
    ReleaseDelivery {
        key: DeliveryLeaseKey,
        lease: Arc<DeliveryLeaseCell>,
        holder: LeaseHolder,
        start: u64,
        end: u64,
        ack: oneshot::Sender<bool>,
    },
    QueryWatcherPending {
        channel_id: ChannelId,
        generation: u64,
        ack: oneshot::Sender<bool>,
    },
}

/// Registers the exact start and applies only its pending admission edges.
pub(super) fn handle_start(
    msg: FinalizeMsg,
    ledger: &mut HashMap<LedgerKey, LedgerEntry>,
    pending_admission: &mut HashMap<LedgerKey, PendingCompletionAdmission>,
    cached_shared: &mut Option<std::sync::Weak<SharedData>>,
) {
    let FinalizeMsg::Start {
        key,
        recovery_lease,
        provider,
        relay_owner,
        completion_admission_plan,
        shared,
    } = msg
    else {
        return;
    };
    // #3016 phase-5a: prime the reconcile cache from the very
    // FIRST register so a FRESH actor whose first watcher-owned
    // turn never submits its own terminal still gets the
    // far-backstop tick. `is_none()` guard: the `Terminal` arm
    // stays authoritative (always overwrites with the live Arc);
    // a dead `Weak` here just skips reconcile harmlessly until a
    // live submission re-primes it.
    if cached_shared.is_none() {
        *cached_shared = Some(shared.clone());
    }
    // #3016 phase-5a: a watcher-owned handoff arms the FAR
    // backstop so a never-terminated turn is visible to the
    // reconciler. `get_or_insert`: set ONCE — a repeat `Start`
    // must never push an armed deadline forward (the EPIC's
    // never-finalizing bug, mirrored from the gate-timeout arm).
    let arm_watcher_backstop = relay_owner == RelayOwnerKind::Watcher;
    // A `Start` consumes only pre-ledger edges for its exact
    // generation. Older generation authority remains isolated
    // until TTL GC instead of pre-settling the current turn.
    let exact_key = key.exact_key();
    let pending = take_exact_pending_completion_admission(pending_admission, exact_key);
    let deadline = arm_watcher_backstop.then(|| Instant::now() + WATCHER_REGISTER_BACKSTOP);
    let (lease, plan) = (recovery_lease, completion_admission_plan);
    let entry = ledger.entry(exact_key).or_insert_with(|| {
        LedgerEntry::registered(key, provider.clone(), relay_owner, lease, plan, deadline)
    });
    if entry.phase != Phase::Finalized {
        entry.relay_owner = relay_owner;
        entry.provider = provider;
        entry.turn_key = key;
        entry
            .completion_admission
            .update_plan(completion_admission_plan);
        apply_pending_completion_admission(entry, pending);
        if arm_watcher_backstop {
            entry
                .watcher_backstop_deadline
                .get_or_insert_with(|| Instant::now() + WATCHER_REGISTER_BACKSTOP);
        }
        if let Some(shared) = shared.upgrade() {
            publish_claimed_queue_eligible(&shared, entry);
        }
    } else if let Some(pending) = pending {
        apply_pending_completion_admission(entry, Some(pending));
        if let Some(shared) = shared.upgrade() {
            publish_claimed_queue_eligible(&shared, entry);
        }
    }
}

#[cfg(all(test, unix))]
impl TurnFinalizer {
    pub(super) async fn submit_native(
        &self,
        work: super::terminal_handler::NativeCleanup,
        history: crate::services::tui_input::human_frame::history::v2::Registry,
        shared: Arc<SharedData>,
    ) -> Result<bool, String> {
        contain_panic(work.prepare_notice(&history, &shared)).await??;
        let (ack, result) = oneshot::channel();
        self.tx
            .send(FinalizeMsg::Native {
                work: Box::new(work),
                history,
                shared,
                ack,
            })
            .map_err(|_| "native finalizer unavailable")?;
        result
            .await
            .map_err(|_| "native finalizer reply unavailable")?
    }
}

#[cfg(all(test, unix))]
pub(super) async fn handle_native(
    work: super::terminal_handler::NativeCleanup,
    history: crate::services::tui_input::human_frame::history::v2::Registry,
    shared: Arc<SharedData>,
    ack: oneshot::Sender<Result<bool, String>>,
    ledger: &mut HashMap<LedgerKey, LedgerEntry>,
) {
    let key = work.primary_key();
    let result = contain_panic(work.drive(&history, &shared))
        .await
        .and_then(|result| result);
    if result.as_ref() == Ok(&true)
        && let Some(entry) = key.and_then(|key| ledger.get_mut(&key.exact_key()))
    {
        entry.phase = Phase::Finalized;
        entry.finalized_at = Some(Instant::now());
        entry.completion_admission.native_input = Some(true);
        entry.completion_admission.note_mailbox_released();
        entry.completion_admission.queue_eligible_published = true;
    }
    let _ = ack.send(result);
}

pub(super) fn panic_payload_summary(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "non-string panic payload".to_string()
}

pub(super) async fn contain_panic<T>(
    future: impl std::future::Future<Output = T>,
) -> Result<T, String> {
    std::panic::AssertUnwindSafe(future).catch_unwind().await.map_err(|payload| {
        let message = panic_payload_summary(payload.as_ref());
        tracing::error!(panic = %message, "TurnFinalizer handler panicked; durable work retained");
        message
    })
}

/// Resolve a submission to the ledger entry it acts on. A real `user_msg_id`
/// uses its exact key (a stale terminal of a finalized turn matches the
/// retained `Finalized` entry → `AlreadyFinalized`); a channel-only id-0
/// terminal collapses per `resolve_channel_only` (never prematurely finalizing
/// a queued follow-up).
pub(super) fn resolve_ledger_key(
    ledger: &HashMap<LedgerKey, LedgerEntry>,
    key: TurnKey,
) -> LedgerKey {
    resolve_channel_only(
        key,
        ledger
            .iter()
            .map(|(lk, entry)| (lk, entry.phase == Phase::Finalized)),
    )
}
