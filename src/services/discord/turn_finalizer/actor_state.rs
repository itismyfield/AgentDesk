use super::completion_admission::CompletionAdmission;
use super::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Phase {
    Pending,
    Finalizing,
    Finalized,
}

pub(super) struct LedgerEntry {
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
}

pub(super) struct PendingCompletionAdmission {
    pub(super) turn_key: TurnKey,
    pub(super) completion_admission: CompletionAdmission,
    pub(super) updated_at: Instant,
}

pub(super) enum FinalizeMsg {
    Start {
        key: TurnKey,
        provider: ProviderKind,
        relay_owner: RelayOwnerKind,
        completion_admission_plan: CompletionAdmissionPlan,
        shared: std::sync::Weak<SharedData>,
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
        claim_snapshot: Option<SyntheticClaimSnapshot>,
        shared: Arc<SharedData>,
        ack: oneshot::Sender<FinalizeOutcome>,
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
