use std::sync::Arc;

use crate::services::provider::ProviderKind;

use super::{FinalizeContext, TerminalEvent, TurnKey};
use crate::services::discord::SharedData;

/// Late `AlreadyFinalized` losers still perform guarded active-state cleanup.
/// This is intentionally narrower than `do_finalize`: only the same real turn id
/// can lose mailbox/inflight state, so a newer active turn is preserved.
pub(super) async fn already_finalized_active_state(
    key: TurnKey,
    provider: &ProviderKind,
    event: &TerminalEvent,
    ctx: FinalizeContext,
    shared: &Arc<SharedData>,
) {
    if key.user_msg_id == 0 {
        return;
    }

    let _ = crate::services::discord::inflight::clear_inflight_state_if_matches(
        provider,
        key.channel_id.get(),
        key.user_msg_id,
    );

    let finish = super::super::mailbox_finish_turn_if_matches(
        shared,
        provider,
        key.channel_id,
        serenity::model::id::MessageId::new(key.user_msg_id),
    )
    .await;
    let Some(token) = finish.removed_token.as_ref() else {
        return;
    };

    if ctx.allow_completion_cleanup && !matches!(event, TerminalEvent::Cancel) {
        token.mark_completion_cleanup();
    }
    token
        .cancelled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    super::super::saturating_decrement_global_active(shared);
    super::super::clear_watchdog_deadline_override(key.channel_id.get()).await;
    shared
        .dispatch_thread_parents
        .retain(|_, thread| *thread != key.channel_id);
    if !finish.has_pending {
        shared.dispatch_role_overrides.remove(&key.channel_id);
    }
}
