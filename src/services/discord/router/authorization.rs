use crate::services::cluster::intake_turn::TurnKind;

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
