use super::TurnViewState;

const NONE: &[char] = &[];
const QUEUED: &[char] = &['📬'];
const QUEUED_MERGED: &[char] = &['➕'];
const QUEUED_RECONCILE: &[char] = &['🔄'];
const PENDING: &[char] = &['⏳'];
const COMPLETED: &[char] = &['✅'];
const FAILED: &[char] = &['⚠'];
const STOPPED: &[char] = &['🛑'];

pub(super) const fn for_state(state: TurnViewState) -> &'static [char] {
    match state {
        TurnViewState::Queued => QUEUED,
        TurnViewState::QueuedMerged => QUEUED_MERGED,
        TurnViewState::QueuedReconcile => QUEUED_RECONCILE,
        TurnViewState::Pending => PENDING,
        TurnViewState::Completed => COMPLETED,
        TurnViewState::Failed => FAILED,
        TurnViewState::Stopped => STOPPED,
        TurnViewState::None => NONE,
    }
}
