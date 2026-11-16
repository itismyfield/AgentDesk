//! Exact host observation for a queued start and its held-front watch.
use super::{
    ChannelId, ProviderKind, Screen, Shared, TurnHost, Value, claude_handoff, start_target,
};

pub(super) enum Observed {
    Legacy,
    Refused,
    Native(Screen),
}

impl Observed {
    pub(super) fn native(self) -> Option<Screen> {
        match self {
            Self::Native(screen) => Some(screen),
            Self::Legacy | Self::Refused => None,
        }
    }
}

/// A Herdr start reads its current exact execution; a watch never reuses an old host proof.
pub(super) async fn observe(shared: &Shared, channel: ChannelId, context: &Value) -> Observed {
    use crate::services::claude_tui::host_input::busy::BusyState;
    let provider = &shared.provider;
    let key =
        crate::services::discord::adk_session::turn_session_key(shared, channel, provider).await;
    use crate::db::dispatched_sessions::hosted_execution::{HostedRecord, HostedState};
    let host = crate::services::turn_host::for_observation(
        shared.pg_pool.as_ref(),
        provider,
        channel.get(),
        key.as_deref(),
    )
    .await;
    let plan = match host {
        TurnHost::Tmux | TurnHost::Return(_) => return Observed::Legacy,
        TurnHost::Herdr(plan) => plan,
        TurnHost::Refused(crate::services::turn_host::HerdrRefusal::HostedRowUnreadable {
            ..
        }) => {
            return Observed::Native(Screen::Unknown);
        }
        TurnHost::Refused(_) => return Observed::Refused,
    };
    if *provider != ProviderKind::Claude {
        use crate::services::discord::turn_presence::activity::{Activity, activity_now};
        return Observed::Native(
            match activity_now(shared, provider, channel).await.activity {
                Activity::Idle => Screen::Ready,
                Activity::Busy => Screen::Blocked,
                Activity::Unknown => Screen::Unknown,
            },
        );
    }
    let target = if plan.row.as_ref().is_some_and(|row| {
        matches!(&row.record, HostedRecord::Known(record) if record.state == HostedState::Bound)
    }) {
        match start_target(shared, channel, context).await {
            Ok(Some(target)) => Some(target),
            _ => return Observed::Native(Screen::Unknown),
        }
    } else {
        None
    };
    // The reader judges absent/retired rows before needing a pane name.
    let logical = target.as_ref().map_or("", |target| target.tmux.as_str());
    let host = TurnHost::Herdr(plan);
    let observation =
        claude_handoff::observe_busy(channel.get(), logical, &host, &shared.token_hash);
    Observed::Native(match observation.state() {
        BusyState::Idle => Screen::Ready,
        BusyState::Busy => Screen::Blocked,
        BusyState::Held | BusyState::Legacy => Screen::Unknown,
    })
}

#[cfg(test)]
impl super::OwnerTurn {
    pub(in crate::services::discord) async fn start_wait_screen_for_test(
        shared: &Shared,
        channel: ChannelId,
        context: &Value,
    ) -> Option<Screen> {
        observe(shared, channel, context).await.native()
    }
}
