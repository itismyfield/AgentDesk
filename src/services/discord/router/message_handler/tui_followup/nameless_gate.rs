//! A channel without a core session has no session name, so its own checks decide promotion.

use super::*;

/// The canary keeps its verified hold here too, so an unretired pin parks the input instead of
/// letting the launch take it and wait; otherwise the nameless host check decides.
pub(super) async fn nameless_promote_blocked(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) -> bool {
    #[cfg(unix)]
    {
        use crate::services::codex_tui::canary::{CANARY_CHANNEL, CANARY_TMUX};
        if matches!(provider, ProviderKind::Codex)
            && channel_id.get() == CANARY_CHANNEL
            && named_blocked(provider, channel_id, CANARY_TMUX)
        {
            return true;
        }
    }
    let nameless = crate::services::discord::host_defer_gate::nameless_channel_deferred;
    let held = nameless(shared, provider, channel_id.get()).await;
    if held && Box::pin(returns_to_tmux(shared, provider, channel_id)).await {
        return false;
    }
    #[cfg(unix)]
    if held && crate::services::tui_o::turn_mode::transcript_turns(channel_id.get()) {
        if Box::pin(local_legacy_start_ready(shared, provider, channel_id, None)).await {
            return false;
        }
    }
    held
}

#[cfg(unix)]
pub(super) fn named_blocked(
    provider: &ProviderKind,
    channel: serenity::ChannelId,
    tmux: &str,
) -> bool {
    use crate::services::tui_prompt_dedupe::clear_relaunch;
    if !matches!(provider, ProviderKind::Codex) {
        return false;
    }
    let Some(observation) = crate::services::discord::codex_absence::readiness(channel.get(), tmux)
    else {
        return clear_relaunch::launch_blocked(tmux);
    };
    if clear_relaunch::absent_capture(tmux, &observation).is_some()
        && !clear_relaunch::relaunch_open(tmux)
        && !crate::services::codex::successor_hooks_ready()
    {
        tracing::info!(
            tmux_session = tmux,
            reason = "successor_hooks_unavailable",
            "Codex fresh successor is waiting for hook eligibility"
        );
        return true;
    }
    clear_relaunch::launch_blocked_observed(tmux, &observation)
}

/// Probe hosted readiness before teardown; host-kept sessions not moving back to tmux, verified
/// Codex holds and busy panes defer. The post-claim path keeps message preservation and defer UX.
pub(in crate::services::discord) async fn hosted_tui_promote_readiness_blocked(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) -> bool {
    #[cfg(test)]
    if let Some(busy) = *HOSTED_TUI_PROMOTE_BUSY_HOOK_FOR_TESTS
        .lock()
        .expect("hosted tui promote busy hook lock")
    {
        return busy;
    }
    // A Herdr-configured channel's queued turn is judged by its turn host, not this tmux probe.
    if crate::config::session_hosts::herdr_endpoint(channel_id.get()).is_some() {
        return false;
    }

    let (tmux_session_name, current_path, session_id) = {
        let data = shared.core.lock().await;
        let Some(session) = data.sessions.get(&channel_id) else {
            drop(data);
            return nameless_promote_blocked(shared, provider, channel_id).await;
        };
        let tmux_session_name = if provider.uses_managed_tmux_backend() {
            session
                .channel_name
                .as_ref()
                .map(|name| provider.build_tmux_session_name(name))
        } else {
            None
        };
        (
            tmux_session_name,
            session.current_path.clone(),
            session.session_id.clone(),
        )
    };
    let Some(tmux_session_name) = tmux_session_name else {
        return nameless_promote_blocked(shared, provider, channel_id).await;
    };
    // Verified incarnations wait for source and permission before queue teardown.
    #[cfg(unix)]
    if named_blocked(provider, channel_id, &tmux_session_name) {
        return true;
    }
    // ADK_TEST_CODEX_PROMOTE_READY isolates source permission from host readiness in tests.
    #[cfg(test)]
    if std::env::var_os("ADK_TEST_CODEX_PROMOTE_READY").is_some() {
        return false;
    }
    let host = crate::services::discord::host_defer_gate::channel_session_deferred;
    if host(shared, provider, channel_id.get(), &tmux_session_name).await {
        if Box::pin(returns_to_tmux(shared, provider, channel_id)).await {
            return false;
        }
        #[cfg(unix)]
        if crate::services::tui_o::turn_mode::transcript_turns(channel_id.get()) {
            if Box::pin(local_legacy_start_ready(
                shared,
                provider,
                channel_id,
                Some(&tmux_session_name),
            ))
            .await
            {
                return false;
            }
        }
        return true;
    }
    // Platforms without tmux retain the ordinary dispatch path.
    #[cfg(unix)]
    let blocked = tui_busy_followup_diagnostic(
        shared,
        provider,
        channel_id,
        Some(tmux_session_name.as_str()),
        current_path.as_deref(),
        session_id.as_deref(),
    )
    .is_some();
    #[cfg(not(unix))]
    let blocked = {
        let _ = (
            shared,
            provider,
            &tmux_session_name,
            &current_path,
            &session_id,
        );
        false
    };
    if blocked {
        tracing::info!(
            channel_id = channel_id.get(),
            provider = provider.as_str(),
            tmux_session_name = %tmux_session_name,
            "#4270 promote gate: hosted TUI busy before queued-turn teardown; deferring promotion"
        );
    }
    blocked
}

/// Whether the channel's next turn first moves its Herdr execution back to tmux, as its turn host
/// judges it; that turn starts, while any other kept Herdr row still holds the queue.
async fn returns_to_tmux(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) -> bool {
    let build = crate::services::discord::adk_session::build_adk_session_key;
    let key = build(shared, channel_id, provider, None).await;
    let pool = shared.pg_pool.as_ref();
    let host =
        crate::services::turn_host::for_turn(pool, provider, channel_id.get(), key.as_deref());
    matches!(host.await, crate::services::turn_host::TurnHost::Return(_))
}

#[cfg(unix)]
// The caller boxes this witness so nested queue admission keeps its observation state off-stack.
async fn local_legacy_start_ready(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    name: Option<&str>,
) -> bool {
    // A start acts on this judgment, so the periodic observer must not supersede it meanwhile.
    let _effect =
        crate::services::discord::turn_presence::activity::effect_reads::begin(channel_id.get());
    let reading = crate::services::discord::turn_presence::activity::reading_now(
        shared, provider, channel_id,
    )
    .await;
    reading.local_legacy_absence_for_start(name).await
}
