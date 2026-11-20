//! Session restore keeps the original policy and side-effect order.
use super::*;

pub(in crate::services::discord) struct Restore<'a> {
    pub session_id: &'a mut Option<String>,
    pub memento_context_loaded: &'a mut bool,
    pub reason: &'a mut &'static str,
    pub force_fresh: bool,
    pub cleared: bool,
    pub reset: bool,
    pub recreate: bool,
    pub session_key: Option<&'a str>,
    pub tmux: Option<&'a str>,
}

pub(in crate::services::discord) async fn restore(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    state: Restore<'_>,
) {
    if state.session_id.is_none() {
        if state.force_fresh {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Skipping DB provider session restore for channel {} due to /goal fresh session request",
                channel_id.get()
            );
        } else if state.cleared {
            *state.reason = "session_cleared_by_user";
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Skipping DB provider session restore for channel {} due to prior /clear",
                channel_id.get()
            );
        } else if state.reset || state.recreate {
            *state.reason = dispatch_reset_lifecycle_code(state.reset, state.recreate);
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Skipping DB provider session restore for dispatch reset_provider_state={} recreate_tmux={}",
                state.reset,
                state.recreate
            );
        } else if let Some(key) = state.session_key {
            let restored = super::super::super::super::adk_session::fetch_provider_session_id(
                key,
                provider,
                shared.api_port,
            )
            .await;
            if let Some(restored_session_id) = restored {
                *state.reason = "db_provider_session_restored";
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ↻ Restored provider session_id from DB for {}",
                    key
                );
                let mut data = shared.core.lock().await;
                if let Some(session) = data.sessions.get_mut(&channel_id) {
                    session.restore_provider_session(Some(restored_session_id.clone()));
                    *state.memento_context_loaded = session.memento_context_loaded;
                }
                *state.session_id = Some(restored_session_id);
            } else if let Some((recovered, recovered_memento_context_loaded)) =
                restore_live_tui_provider_session_from_binding(
                    shared,
                    channel_id,
                    provider,
                    state.tmux,
                    Some(key),
                )
                .await
            {
                *state.reason = "live_tui_runtime_binding_restored";
                *state.memento_context_loaded = recovered_memento_context_loaded;
                *state.session_id = Some(recovered);
            } else {
                *state.reason = "no_cached_provider_session";
                *state.session_id = None;
            }
        } else {
            *state.reason = "session_key_unavailable";
        }
    }
    // A pane rotation whose settle pass has not run yet still decides the session this turn resumes.
    #[cfg(unix)]
    if let Some(tmux) = state
        .tmux
        .filter(|_| matches!(provider, ProviderKind::Claude))
    {
        let follow = super::super::super::super::tui_prompt_relay::follow_pending_rotation_selector;
        if let Some(new) = follow(shared, channel_id, tmux).await
            && state.session_id.as_deref() != Some(new.as_str())
        {
            *state.session_id = Some(new);
            *state.memento_context_loaded = false;
            *state.reason = "claude_session_rotation_followed";
        }
    }
}

#[cfg(unix)]
use super::super::provider_dispatch::OwnerTurn;
#[cfg(unix)]
use crate::services::discord::session_transition::SessionTransitionLease;

#[cfg(unix)]
type OwnerRoute<'a> = (
    &'a ProviderKind,
    ChannelId,
    Option<&'a str>,
    Option<&'a str>,
);

/// The same front is prepared before Busy admission or idle dispatch. The staged path requires
/// a lease from the caller coordinating restart continuation and resume presentation.
#[cfg(unix)]
pub(in crate::services::discord) async fn prepare_owner(
    shared: &Arc<SharedData>,
    route: OwnerRoute<'_>,
    owner: Option<OwnerTurn>,
    (allows_continuation, lease): (bool, Option<&SessionTransitionLease<'_>>),
) -> Result<Option<OwnerTurn>, ()> {
    match (owner, lease.filter(|_| allows_continuation)) {
        (Some(owner), Some(lease)) => Box::pin(owner.prepare_restarted(shared, route, lease))
            .await
            .map(Some),
        (owner, _) => Ok(owner),
    }
}

#[cfg(unix)]
pub(super) async fn prepare_intake_owner(
    shared: &Arc<SharedData>,
    route: OwnerRoute<'_>,
    owner: Option<OwnerTurn>,
    (force_fresh, boundary): (bool, bool),
    lease: Option<&SessionTransitionLease<'_>>,
) -> Result<(Option<OwnerTurn>, bool), ()> {
    let owner = prepare_owner(shared, route, owner, (!(force_fresh || boundary), lease)).await?;
    if owner
        .as_ref()
        .is_some_and(OwnerTurn::awaiting_restarted_clear)
    {
        // Its own first prompt resolves the Pending. Requiring a bound source here would hold it.
        return Ok((owner, false));
    }
    let (provider, channel, logical, key) = route;
    OwnerTurn::early(shared, provider, channel, logical, key, owner, force_fresh).await
}

#[cfg(unix)]
pub(in crate::services::discord) async fn prepare_headless_owner(
    (shared, cancel): (
        &Arc<SharedData>,
        &Arc<crate::services::provider::CancelToken>,
    ),
    route: OwnerRoute<'_>,
    owner: Option<OwnerTurn>,
    (channel_name, clear_binding, cleared): (Option<&str>, bool, bool),
    lease: Option<&SessionTransitionLease<'_>>,
) -> Result<Option<OwnerTurn>, crate::services::discord::router::turn_start::HeadlessTurnStartError>
{
    let (provider, channel, logical, _) = route;
    let mut owner =
        match prepare_owner(shared, route, owner, (!(clear_binding || cleared), lease)).await {
            Ok(owner) => owner,
            Err(()) => {
                super::super::super::super::mailbox_finish::unwind_unstarted_turn(
                    shared, channel, cancel,
                )
                .await;
                return Err(
                    crate::services::discord::router::turn_start::HeadlessTurnStartError::Internal(
                        "herdr restart held before submit".into(),
                    ),
                );
            }
        };
    if let Some(owner) = owner.as_mut() {
        let channel_pane = channel_name.map(|name| provider.build_tmux_session_name(name));
        let own = channel_pane.is_some() && channel_pane.as_deref() == logical;
        owner.read_late(own, clear_binding);
    }
    Ok(owner)
}

#[cfg(test)]
#[path = "session_preparation/restore_tests.rs"]
mod tests;

#[cfg(test)]
pub(in crate::services::discord) async fn selected_reply(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    context: crate::services::discord::router::response_format::retry_prompt_context::RetryPromptContext,
) -> (
    crate::services::discord::router::response_format::retry_prompt_context::RetryPromptContext,
    Option<String>,
) {
    let prepared = super::retry_prompt::prepare(shared, provider, channel, context).await;
    (prepared.context, prepared.rendered)
}
