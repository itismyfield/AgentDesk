//! Intake turn contract shared by the cluster worker and the Discord router.
use poise::serenity_prelude::{ChannelId, MessageId, UserId};

/// Classifies how a turn was triggered. Drives the race-handler delete-on-loss
/// behavior — background-trigger turns must never have their placeholder
/// deleted, because the placeholder may already carry information the user
/// needs (e.g. "🟢 main CI 통과!" relayed from a `Bash run_in_background`
/// completion). See #796.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnKind {
    /// Triggered by a human user message. Race-handler may delete the
    /// placeholder when this turn loses to another active turn — the user
    /// still sees their own message and can be told "queued for later".
    Foreground,
    /// Triggered by a background-task notification (notify bot post or other
    /// agent self-emitted info-only delivery). The placeholder content is the
    /// only visible record of the notification and MUST be preserved when
    /// this turn loses a race.
    BackgroundTrigger,
}

impl TurnKind {
    pub(crate) fn is_background_trigger(self) -> bool {
        matches!(self, TurnKind::BackgroundTrigger)
    }
}

/// Execution and replay custody remain separate outcomes through queued and worker settlement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IntakeExecution {
    Executed,
    Deferred,
    ConsumedProtected,
}

/// Per-message inputs of `handle_text_message` bundled into a single
/// owned struct. Phase 2-pre.3 of intake-node-routing: lets worker-side
/// callers (`execute_intake_turn_core`) accept a single deserialized
/// row from `intake_outbox` instead of a long positional parameter list.
///
/// Payload fields mirror the `intake_outbox` columns (see
/// migrations/postgres/0052_intake_node_routing.sql) and the per-message
/// parameters of the legacy `handle_text_message` signature; the row primary
/// key is carried separately as `intake_outbox_id`.
/// Adding a column to `intake_outbox` means adding a field here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IntakeRequest {
    /// Worker rows carry their claimed `intake_outbox` primary key. Every other
    /// producer carries `None`: a leader-local request is admitted only after any
    /// stale pending route is retired, so no live outbox row remains to own the
    /// turn (the `None` is correct, but the reason is the retirement, not an
    /// absence of any row); and a worker request that loses the mailbox or
    /// session-transition race is re-queued as an `Intervention`, which does not
    /// carry this id, so its later queued-drain reconstruction is `None`. The id
    /// is therefore sealed only along the direct worker path
    /// (`InflightTurnState` into the headless delivery argument struct); the
    /// requeue path is outside that seal. This is harmless today because delivery
    /// does not yet consume the id (it is parked). Binding the requeue /
    /// `Intervention` path, and sealing the `pub` `IntakeRequest` producer seam
    /// itself, are later slices.
    pub intake_outbox_id: Option<i64>,
    pub channel_id: ChannelId,
    pub user_msg_id: MessageId,
    pub source_message_ids: Vec<MessageId>,
    pub busy_followup_retry_user_msg_id: MessageId,
    pub request_owner: UserId,
    pub request_owner_name: String,
    pub user_text: String,
    pub reply_to_user_message: bool,
    pub defer_watcher_resume: bool,
    pub wait_for_completion: bool,
    pub merge_consecutive: bool,
    pub reply_context: Option<String>,
    pub has_reply_boundary: bool,
    pub dm_hint: Option<bool>,
    pub turn_kind: TurnKind,
    pub preserve_on_cancel: bool,
}
