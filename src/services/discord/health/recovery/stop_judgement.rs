//! A provider channel's turn stop, judged before its first write and carried out on that verdict.

use std::sync::Arc;

use serenity::ChannelId;

use super::{HardStopRuntimeResult, HealthRegistry, RuntimeTurnStopResult, shared_for_provider};
use crate::services::discord::MailboxLookup::{Create, Peek};
use crate::services::discord::turn_bridge::{ChannelJudgement, ChannelStop, keeps_turn};
use crate::services::discord::{self as discord, SharedData};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{FinishTurnResult, TokenFinish};
use poise::serenity_prelude as serenity;

#[cfg(test)]
pub(super) mod judged_finish_tests;
#[cfg(test)]
use judged_finish_tests::Seam;

/// The runtime a stop resolved for a provider channel and its turn's verdict; nothing written.
pub(crate) struct ProviderChannelStop(Option<JudgedChannel>);

struct JudgedChannel {
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel: ChannelId,
    stop: ChannelJudgement,
}

impl ProviderChannelStop {
    /// The channel's active turn runs on a host that is not a confirmed legacy tmux, or its
    /// turn could not be read.
    pub(crate) fn host_refused(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|judged| keeps_turn(&judged.stop))
    }

    /// The session the verdict judged.
    pub(crate) fn session(&self) -> Option<String> {
        self.judged_stop()?.session().map(str::to_string)
    }

    fn judged_stop(&self) -> Option<&ChannelStop> {
        self.0.as_ref()?.stop.as_ref().ok()?.as_ref()
    }
}

pub(crate) async fn judge_provider_channel_stop(
    registry: &HealthRegistry,
    provider_name: &str,
    channel: ChannelId,
) -> ProviderChannelStop {
    let judged = async {
        let provider = ProviderKind::from_str(provider_name)?;
        let shared = shared_for_provider(registry, &provider, channel).await?;
        let stop = ChannelStop::judge(&shared, &provider, channel, None, false).await;
        Some(JudgedChannel {
            shared,
            provider,
            channel,
            stop,
        })
    };
    ProviderChannelStop(judged.await)
}

/// Stops the judged turn on the runtime the verdict resolved; `None` with no runtime.
pub(crate) async fn stop_judged_provider_channel(
    judged: ProviderChannelStop,
    reason: &str,
    cleanup_policy: discord::TmuxCleanupPolicy,
) -> Option<RuntimeTurnStopResult> {
    let JudgedChannel {
        shared,
        provider,
        channel,
        stop,
    } = judged.0?;
    let stop = super::stop_judged_channel_runtime(
        &shared,
        &provider,
        channel,
        stop,
        reason,
        cleanup_policy,
        None,
    );
    Some(stop.await)
}

pub(crate) async fn stop_provider_channel_runtime_with_policy(
    registry: &HealthRegistry,
    provider_name: &str,
    channel_id: ChannelId,
    reason: &str,
    cleanup_policy: discord::TmuxCleanupPolicy,
) -> Option<RuntimeTurnStopResult> {
    let judged = judge_provider_channel_stop(registry, provider_name, channel_id).await;
    stop_judged_provider_channel(judged, reason, cleanup_policy).await
}

/// A turn stop on `shared`'s channel; a force-kill passes the session its verdict approved
/// (`Some(None)`: a process turn) so the stop never judges the host again.
pub(crate) async fn stop_channel_runtime(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    reason: &str,
    cleanup_policy: discord::TmuxCleanupPolicy,
    approved: Option<Option<&str>>,
) -> RuntimeTurnStopResult {
    #[cfg(test)]
    judged_finish_tests::seam(channel_id, Seam::BeforeJudge).await;
    let stop = ChannelStop::judge(shared, provider, channel_id, approved, false).await;
    #[cfg(test)]
    judged_finish_tests::seam(channel_id, Seam::AfterJudge).await;
    let policy = cleanup_policy;
    let run = super::stop_judged_channel_runtime;
    run(shared, provider, channel_id, stop, reason, policy, approved).await
}

/// The result of a stop the host guard kept before its first write.
pub(super) async fn host_guard_preserved(
    shared: &SharedData,
    channel: ChannelId,
) -> RuntimeTurnStopResult {
    preserved(
        shared,
        channel,
        RuntimeTurnStopResult::preserved_by_host_guard,
    )
    .await
}

/// The result of a stop that leaves the channel's turn in place, by `kept`, at the queue's depth.
pub(super) async fn preserved(
    shared: &SharedData,
    channel: ChannelId,
    kept: impl FnOnce(usize) -> RuntimeTurnStopResult,
) -> RuntimeTurnStopResult {
    let snapshot = discord::mailbox_snapshot(shared, channel).await;
    kept(snapshot.intervention_queue.len())
}

/// The result of a stop whose finish went unobserved: nothing was finished or cleared.
pub(super) async fn unobserved(shared: &SharedData, channel: ChannelId) -> RuntimeTurnStopResult {
    preserved(shared, channel, RuntimeTurnStopResult::finish_unobserved).await
}

/// Finishes the channel's turn only while it is still the one `judged` stopped, or no turn when
/// it judged none. `Err` is the stop's result when another turn holds the channel or the finish
/// went unobserved; nothing was finished or cleared then.
pub(super) async fn finish_judged_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    (stop, judged_termination): (Option<&ChannelStop>, bool),
) -> Result<FinishTurnResult, RuntimeTurnStopResult> {
    let key = stop.map(ChannelStop::token);
    let finish = discord::mailbox_finish_judged_turn(shared, provider, channel, key, Create);
    match finish.await {
        TokenFinish::Finished(finish) | TokenFinish::NoActiveTurn(finish) => Ok(finish),
        TokenFinish::TokenMismatch { .. } => {
            let superseded =
                |depth| RuntimeTurnStopResult::token_superseded(depth, judged_termination);
            Err(preserved(shared, channel, superseded).await)
        }
        TokenFinish::Unavailable | TokenFinish::NoMailbox => Err(unobserved(shared, channel).await),
    }
}

/// The name-lookup stop's finish of the turn it judged on the runtime it found; with no actor
/// registered there it finishes as offline. `Err` keeps the turn, as for [`finish_judged_turn`].
pub(super) async fn finish_found_turn(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    judged: &ChannelJudgement,
) -> Result<FinishTurnResult, HardStopRuntimeResult> {
    #[cfg(test)]
    judged_finish_tests::seam(channel, Seam::AfterJudge).await;
    let Ok(stop) = judged else {
        return Err(HardStopRuntimeResult::finish_unobserved());
    };
    let key = stop.as_ref().map(ChannelStop::token);
    match discord::mailbox_finish_judged_turn(shared, provider, channel, key, Peek).await {
        TokenFinish::Finished(finish) | TokenFinish::NoActiveTurn(finish) => Ok(finish),
        TokenFinish::NoMailbox => Ok(discord::unavailable_finish_turn_result()),
        TokenFinish::TokenMismatch { has_pending } => {
            Err(HardStopRuntimeResult::token_superseded(has_pending))
        }
        TokenFinish::Unavailable => Err(HardStopRuntimeResult::finish_unobserved()),
    }
}
