//! Exact host observation for a queued start and its held-front watch.
use super::{
    ChannelId, ChannelMailboxHandle, FrontClaim, ProviderKind, Screen, Shared, TurnHost, Value,
    claude_handoff, owned_busy::dead_bound, start_target,
};

pub(super) enum Observed {
    Legacy,
    Refused,
    Native(Screen),
    /// The dead-Bound recovery left the execution's row for a later look, for this reason.
    Unrecovered(String),
    /// This owner can never recover the channel's Bound execution.
    Reject(String),
}

impl Observed {
    pub(super) fn native(self) -> Option<Screen> {
        match self {
            Self::Native(screen) => Some(screen),
            Self::Unrecovered(_) => Some(Screen::Unknown),
            Self::Legacy | Self::Refused | Self::Reject(_) => None,
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
/// host proof. An unclear read of a Bound execution asks the shared dead-Bound recovery.
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
    let bound = plan.row.as_ref().is_some_and(|row| {
        matches!(&row.record, HostedRecord::Known(record) if record.state == HostedState::Bound)
    });
    let host = TurnHost::Herdr(plan);
    let (screen, logical) = if *provider != ProviderKind::Claude {
        use crate::services::discord::turn_presence::activity::{Activity, activity_now};
        let screen = match activity_now(shared, provider, channel).await.activity {
            Activity::Idle => Screen::Ready,
            Activity::Busy => Screen::Blocked,
            Activity::Unknown => Screen::Unknown,
        };
        let alive = match screen {
            Screen::Ready | Screen::Blocked if bound => execution_alive(&host).await,
            _ => true,
        };
        (live_rollout(screen, bound, alive), None)
    } else {
        let logical = match bound {
            true => match start_target(shared, channel, context).await {
                Ok(Some(target)) => Some(target.tmux),
                _ => return Observed::Native(Screen::Unknown),
            },
            false => None,
        };
        // The reader judges absent/retired rows before needing a pane name.
        let named = logical.as_deref().unwrap_or("");
        let observation =
            claude_handoff::observe_busy(channel.get(), named, &host, &shared.token_hash);
        let screen = match observation.state() {
            BusyState::Idle => Screen::Ready,
            BusyState::Busy => Screen::Blocked,
            BusyState::Held | BusyState::Legacy => Screen::Unknown,
        };
        (screen, logical)
    };
    // Ready or Busy is a live execution's read here (Claude's reader checks it first); anything
    // else on a Bound row may be a dead one.
    if screen != Screen::Unknown || !bound {
        return Observed::Native(screen);
    }
    let logical = match logical {
        Some(logical) => logical,
        None => match start_target(shared, channel, context).await {
            Ok(Some(target)) => target.tmux,
            _ => return Observed::Native(Screen::Unknown),
        },
    };
    let mailbox = shared.mailbox(channel);
    let at = (provider, channel, key.as_deref(), logical.as_str());
    let lease = dead_bound::LeaseRef::Acquire;
    match dead_bound::recover(shared, at, &host, &mailbox, lease, || {}).await {
        Ok(()) => Observed::Native(Screen::Ready),
        Err(dead_bound::DeadBound::Refused(reason)) => Observed::Reject(reason),
        Err(dead_bound::DeadBound::Reobserve(reason) | dead_bound::DeadBound::Retry(reason)) => {
            Observed::Unrecovered(reason)
        }
    }
}

/// A rollout's Idle or Busy reads its last turn, never that the execution still runs: on a Bound
/// row it counts only while that execution is alive.
fn live_rollout(screen: Screen, bound: bool, alive: bool) -> Screen {
    match screen {
        Screen::Ready | Screen::Blocked if bound && !alive => Screen::Unknown,
        screen => screen,
    }
}

/// Whether the host's Bound execution still runs at its exact endpoint; an unread one does not.
async fn execution_alive(host: &TurnHost) -> bool {
    use crate::db::dispatched_sessions::hosted_execution::{HostedRecord, HostedState};
    use crate::services::session_host::{PaneProvider, PaneReading, herdr_endpoints};

    let TurnHost::Herdr(plan) = host else {
        return false;
    };
    let Some(HostedRecord::Known(record)) = plan.row.as_ref().map(|row| &row.record) else {
        return false;
    };
    let Some(expected) = record
        .expected
        .as_ref()
        .filter(|_| record.state == HostedState::Bound)
    else {
        return false;
    };
    let Some(view) = herdr_endpoints().view(record) else {
        return false;
    };
    let expected = expected.clone();
    tokio::task::spawn_blocking(move || {
        matches!(
            view.read_execution(),
            PaneReading::Present { root, provider: PaneProvider::Execution(provider) }
                if root == expected.root && provider == expected.provider_process
        )
    })
    .await
    .unwrap_or(false)
}

/// How a start that read its channel's execution waits: Ready goes on, a refusal is settled after
/// claim, and anything else is watched by `observe` itself, so each look asks the recovery again.
pub(super) fn wait(
    observed: Observed,
    shared: &Shared,
    channel: ChannelId,
    context: &Value,
) -> super::Wait {
    let mut said = None;
    match observed {
        Observed::Native(Screen::Ready) => return super::Wait::Go,
        Observed::Reject(reason) => return super::Wait::Reject(reason),
        Observed::Unrecovered(reason) => note(channel, &reason, &mut said),
        _ => {}
    }
    let (shared, context) = (shared.clone(), context.clone());
    super::Wait::Watch(
        "screen",
        Box::new(move || {
            tokio::runtime::Handle::current().block_on(async {
                let observed = observe(&shared, channel, &context).await;
                if let Observed::Unrecovered(reason) = &observed {
                    note(channel, reason, &mut said);
                }
                observed.native().unwrap_or(Screen::Ready)
            })
        }),
    )
}

/// A waiting recovery's reason reaches info once per change; a repeated one stays at debug.
fn note(channel: ChannelId, reason: &str, said: &mut Option<String>) {
    if said.as_deref() == Some(reason) {
        tracing::debug!(
            channel_id = channel.get(),
            reason,
            "start dead-Bound recovery waits"
        );
        return;
    }
    tracing::info!(
        channel_id = channel.get(),
        reason,
        "start dead-Bound recovery waits"
    );
    *said = Some(reason.to_string());
}

/// A claimed start whose Bound execution this owner can never recover: its originals go with a
/// `Failed` receipt and a posted (or outboxed) reason, and the start settles as a non-send.
pub(super) async fn reject(
    shared: &Shared,
    channel: ChannelId,
    (mailbox, claim): (&ChannelMailboxHandle, &FrontClaim),
    reason: String,
) -> Result<(), String> {
    use super::super::start_failure::{Failure, RECOVERY_REFUSED};
    tracing::info!(channel_id = channel.get(), %reason, "start refused: Bound execution unrecoverable");
    let failure = Failure::claimed(mailbox, claim);
    failure
        .fail(Some(shared.clone()), channel, reason)
        .await
        .map_err(super::effect::unsent)?;
    Err(RECOVERY_REFUSED.into())
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

#[cfg(test)]
#[path = "owner_input_start_wait_tests.rs"]
mod tests;
