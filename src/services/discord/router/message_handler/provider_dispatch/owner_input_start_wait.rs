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
    let Some(endpoint) = crate::config::session_hosts::herdr_endpoint(channel.get()) else {
        let host = crate::services::turn_host::for_turn(
            shared.pg_pool.as_ref(),
            provider,
            channel.get(),
            key.as_deref(),
        )
        .await;
        return match host {
            TurnHost::Tmux | TurnHost::Return(_) => None,
            _ => Some(Screen::Unknown),
        };
    };
    if crate::config::session_hosts::local_node().as_deref()
        != Some(endpoint.execution_node.as_str())
    {
        return Some(Screen::Unknown);
    }
    let (Some(pool), Some(key)) = (shared.pg_pool.as_ref(), key) else {
        return Some(Screen::Unknown);
    };
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedLookup, HostedLookupKey, HostedRecord, HostedState, load_hosted_execution_pg,
    };
    let row = match load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(&key)).await {
        HostedLookup::Found(row) => Some(row),
        HostedLookup::Missing => None,
        _ => return Some(Screen::Unknown),
    };
    // Observation must not wait on execution admission or output-writer readiness.
    let plan = Box::new(crate::services::turn_host::HerdrTurnPlan {
        endpoint: crate::services::turn_host::launch_endpoint(endpoint),
        row,
    });
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
    let target = if plan.row.as_ref().is_some_and(|row| {
        matches!(&row.record, HostedRecord::Known(record) if record.state == HostedState::Bound)
    }) {
        match start_target(shared, channel, context).await {
            Ok(Some(target)) => Some(target),
            _ => return Some(Screen::Unknown),
        }
    } else {
        None
    };
    // The reader judges absent/retired rows before needing a pane name.
    let logical = target.as_ref().map_or("", |target| target.tmux.as_str());
    let host = TurnHost::Herdr(plan);
    let observation =
        claude_handoff::observe_busy(channel.get(), logical, &host, &shared.token_hash);
    Some(match observation.state() {
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
        screen(shared, channel, context).await
    }
}
