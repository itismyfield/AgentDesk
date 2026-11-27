//! Exact host observation for a queued start and its held-front watch.
use super::{
    ChannelId, NativeTarget, ProviderKind, Screen, Shared, TurnHost, Value, claude_handoff,
    start_target,
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

/// Whether a start of `origin` runs under the channel's key; `None`: a routine's own context says.
/// An effort continuation resumes the channel's own session.
pub(super) fn keyed_to_channel(origin: super::InputOrigin) -> Option<bool> {
    use super::InputOrigin::{EffortContinue, HttpStart, Routine};
    match origin {
        Routine => None,
        origin => Some(origin.is_person() || origin == HttpStart || origin == EffortContinue),
    }
}

/// A Herdr start under the channel's key reads its current execution; a watch never reuses an old
/// host proof.
pub(super) async fn observe(shared: &Shared, channel: ChannelId, context: &Value) -> Observed {
    use crate::services::claude_tui::host_input::busy::BusyState;
    let origin = super::InputOrigin::from_context(Some(context));
    // A start apart from the channel's key keeps its native wait and existing typed refusal order.
    let channel_turn = match keyed_to_channel(origin) {
        Some(keyed) => keyed,
        None => {
            super::super::super::headless_turn::owner_start_takes_channel(shared, context).await
        }
    };
    if !channel_turn {
        return Observed::Legacy;
    }
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

pub(super) fn claude_native_read(target: &NativeTarget) -> Screen {
    use crate::services::tui_turn_state::{TuiTurnState, observe_claude_jsonl_turn_state};
    let resolve = super::super::super::tui_followup::claude_followup_transcript;
    let (dir, session_id) = (target.dir.as_deref(), target.session_id.as_deref());
    let Some(path) = resolve(dir, session_id, Some(&target.tmux)) else {
        return Screen::Unknown;
    };
    match observe_claude_jsonl_turn_state(&path) {
        TuiTurnState::Idle => Screen::Ready,
        TuiTurnState::Streaming | TuiTurnState::UserSubmitted => Screen::Blocked,
        TuiTurnState::Unknown if target.host_evidence_only => {
            if crate::services::claude_tui::readiness::observe(&target.tmux, Some(&path)).is_some()
            {
                Screen::Ready
            } else {
                Screen::Unknown
            }
        }
        // Session markers with no tool waiting read idle once the composer would take a send.
        TuiTurnState::Unknown => {
            use crate::services::claude_tui::input::{
                final_prompt_ready, prompt_readiness_snapshot,
            };
            match final_prompt_ready(&prompt_readiness_snapshot(&target.tmux)) {
                true => Screen::Ready,
                false => Screen::Unknown,
            }
        }
    }
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
