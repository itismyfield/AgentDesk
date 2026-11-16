//! Exact host observation for a queued start and its held-front watch.
use super::{
    ChannelId, ProviderKind, Screen, Shared, TurnHost, Value, claude_handoff, start_target,
};

/// A Herdr start reads its current exact execution; a watch never reuses an old host proof.
pub(super) async fn screen(shared: &Shared, channel: ChannelId, context: &Value) -> Option<Screen> {
    use crate::services::claude_tui::host_input::busy::BusyState;
    let provider = &shared.provider;
    let key =
        crate::services::discord::adk_session::turn_session_key(shared, channel, provider).await;
    let host = crate::services::turn_host::for_turn(
        shared.pg_pool.as_ref(),
        provider,
        channel.get(),
        key.as_deref(),
    )
    .await;
    match &host {
        TurnHost::Tmux | TurnHost::Return(_) => return None,
        TurnHost::Refused(_) => return Some(Screen::Unknown),
        TurnHost::Herdr(_) => {}
    }
    if *provider != ProviderKind::Claude {
        use crate::services::discord::turn_presence::activity::{Activity, activity_now};
        return Some(
            match activity_now(shared, provider, channel).await.activity {
                Activity::Idle => Screen::Ready,
                Activity::Busy => Screen::Blocked,
                Activity::Unknown => Screen::Unknown,
            },
        );
    }
    let target = match start_target(shared, channel, context).await {
        Ok(Some(target)) => target,
        _ => return Some(Screen::Unknown),
    };
    let observation =
        claude_handoff::observe_busy(channel.get(), &target.tmux, &host, &shared.token_hash);
    Some(match observation.state() {
        BusyState::Idle => Screen::Ready,
        BusyState::Busy => Screen::Blocked,
        BusyState::Held | BusyState::Legacy => Screen::Unknown,
    })
}
