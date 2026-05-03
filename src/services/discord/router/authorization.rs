/// Classifies how a turn was triggered. Drives the race-handler delete-on-loss
/// behavior — background-trigger turns must never have their placeholder
/// deleted, because the placeholder may already carry information the user
/// needs (e.g. "🟢 main CI 통과!" relayed from a `Bash run_in_background`
/// completion). See #796.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum TurnKind {
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
    pub(in crate::services::discord) fn is_background_trigger(self) -> bool {
        matches!(self, TurnKind::BackgroundTrigger)
    }
}

/// Returns `true` when a Discord message arriving on a channel was authored
/// by the dedicated notify bot (or the agent's own background-task self-emit
/// channel). Such turns are exempt from the race-handler delete-on-loss path
/// per #796.
///
/// **Phase 2 note**: today the intake gate at
/// `intake_gate.rs::is_allowed_turn_sender` early-returns for any bot-authored
/// message that is not in `allowed_bot_ids`, so a real notify-bot post is
/// dropped before this classifier runs. The race-handler exemption here is
/// scaffolding for the proper turn-origin propagation work (tracked as Phase 2
/// in `docs/background-task-pattern.md`). The agent-side convention to deliver
/// background results through `bot: notify` is what avoids the message-loss
/// bug today; this enum lets us evolve the runtime behavior without another
/// signature churn when Phase 2 lands.
pub(in crate::services::discord) fn classify_turn_kind_from_author(
    author_id: u64,
    notify_bot_user_id: Option<u64>,
) -> TurnKind {
    if notify_bot_user_id.is_some_and(|id| id == author_id) {
        TurnKind::BackgroundTrigger
    } else {
        TurnKind::Foreground
    }
}
