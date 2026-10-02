//! A channel stop judged once, before its first write: the active token, its host verdict and
//! the names the cancel will bind and record are all read here, and nothing here writes.

use std::sync::Arc;

use poise::serenity_prelude::ChannelId;

use super::stop_host::{LegacyTmuxName, StopOutcome, StopTarget};
use super::{TmuxCleanupPolicy, bind_judged_legacy, stop_active_turn_on};
use crate::services::discord::{SharedData, inflight};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::turn_orchestrator::CancelActiveTurnResult;

/// The reason the channel cancel records on the token and its tombstone.
const CANCEL_REASON: &str = "mailbox_cancel_active_turn";

pub(in crate::services::discord) struct ChannelStop {
    shared: Arc<SharedData>,
    provider: ProviderKind,
    channel: ChannelId,
    token: Arc<CancelToken>,
    target: StopTarget,
    /// The unbound token's name, read from its inflight row; bound by the cancel.
    bind_name: Option<LegacyTmuxName>,
    tombstone_name: Option<String>,
}

impl ChannelStop {
    /// Judges `channel`'s active turn, or `None` with no turn. `approved` is a force-kill
    /// verdict's session; with `bind_unbound`, a token with no name is judged by its inflight row.
    pub(in crate::services::discord) async fn judge(
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel: ChannelId,
        approved: Option<Option<&str>>,
        bind_unbound: bool,
    ) -> Option<Self> {
        let handle = shared.mailbox_peek(channel)?;
        let token = handle.cancel_token().await.ok().flatten()?;
        let bound = token.tmux_session_name();
        let unbound = bound.is_none() && bind_unbound;
        let name = bound.or_else(|| unbound.then(|| inflight_name(provider, channel)).flatten());
        let mut stop = Self::judge_token(shared, provider, channel, token, approved, name);
        #[cfg(test)]
        if let Some(target) = NEXT_TARGET.with_borrow_mut(Option::take) {
            stop.target = target;
        }
        if unbound {
            stop.bind_name = stop.target.legacy_name().cloned();
        }
        let watcher = shared.tmux_watchers.channel_binding(&channel);
        stop.tombstone_name = watcher
            .map(|binding| binding.tmux_session_name)
            .or_else(|| {
                let providers = [
                    ProviderKind::Claude,
                    ProviderKind::Codex,
                    ProviderKind::Gemini,
                    ProviderKind::Qwen,
                ];
                providers.iter().find_map(|p| inflight_name(p, channel))
            });
        Some(stop)
    }

    /// Judges `token` by `name` alone, for a stop that neither cancels nor binds it.
    pub(in crate::services::discord) fn judge_token(
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel: ChannelId,
        token: Arc<CancelToken>,
        approved: Option<Option<&str>>,
        name: Option<String>,
    ) -> Self {
        Self {
            shared: shared.clone(),
            provider: provider.clone(),
            channel,
            token,
            target: StopTarget::judge(name, approved),
            bind_name: None,
            tombstone_name: None,
        }
    }

    pub(in crate::services::discord) fn refused(&self) -> bool {
        matches!(self.target, StopTarget::Refused { .. })
    }

    /// The judged session name, legacy or refused.
    pub(in crate::services::discord) fn session(&self) -> Option<&str> {
        match &self.target {
            StopTarget::Refused { name, .. } => Some(name),
            target => target.legacy_name().map(LegacyTmuxName::as_str),
        }
    }

    pub(in crate::services::discord) fn legacy_name(&self) -> Option<&str> {
        self.target.legacy_name().map(LegacyTmuxName::as_str)
    }

    pub(in crate::services::discord) fn is_token(&self, token: &Arc<CancelToken>) -> bool {
        Arc::ptr_eq(&self.token, token)
    }

    /// Cancels the judged token only while it is still the channel's turn, then records the
    /// tombstone under the name judged with it and binds the name the judge read.
    pub(in crate::services::discord) async fn cancel(&self) -> CancelActiveTurnResult {
        let mailbox = self.shared.mailbox(self.channel);
        let expected = self.token.clone();
        let reason = CANCEL_REASON.to_string();
        let result = mailbox
            .cancel_active_turn_if_current_with_reason(expected, reason)
            .await;
        if result.token.is_some() {
            let name = self.tombstone_name.as_deref();
            let record = crate::services::discord::record_turn_stop_tombstone;
            record(self.channel, name, CANCEL_REASON).await;
        }
        let cancelled_now = result.token.is_some() && !result.already_stopping;
        if let (true, Some(name)) = (cancelled_now, self.bind_name.as_ref()) {
            let reason = "text command stop mailbox lookup";
            bind_judged_legacy(&self.provider, &self.token, name, reason);
        }
        result
    }

    /// Stops the judged token on the judged target.
    pub(in crate::services::discord) async fn stop(
        &self,
        policy: TmuxCleanupPolicy,
        reason: &str,
    ) -> StopOutcome {
        let (target, provider) = (&self.target, &self.provider);
        stop_active_turn_on(target, provider, &self.token, policy, reason).await
    }

    /// The outcome of a judged turn the caller leaves unstopped.
    pub(in crate::services::discord) fn unstopped(&self) -> StopOutcome {
        StopOutcome {
            termination_recorded: false,
            settlement: self.target.settlement(false),
        }
    }
}

#[cfg(test)]
thread_local! {
    /// The verdict the next channel judge on this thread takes, as a Herdr turn will carry one.
    static NEXT_TARGET: std::cell::RefCell<Option<StopTarget>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn judge_next_as(target: StopTarget) {
    NEXT_TARGET.with_borrow_mut(|next| *next = Some(target));
}

fn inflight_name(provider: &ProviderKind, channel: ChannelId) -> Option<String> {
    let row = inflight::load_inflight_state_read_only_result(provider, channel.get());
    row.ok().flatten().and_then(|row| row.tmux_session_name)
}

pub(in crate::services::discord) enum CommandStop {
    NoActiveTurn,
    /// The turn's host is not a confirmed legacy tmux: nothing was cancelled.
    HostRefused,
    AlreadyStopping,
    Stop(ChannelStop),
}

/// A user stop: judged before any write, then cancelled only when the host is admitted.
pub(in crate::services::discord) async fn begin_command_stop(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    bind_unbound: bool,
) -> CommandStop {
    let Some(stop) = ChannelStop::judge(shared, provider, channel, None, bind_unbound).await else {
        return CommandStop::NoActiveTurn;
    };
    if stop.refused() {
        return CommandStop::HostRefused;
    }
    let result = stop.cancel().await;
    match result.token {
        None => CommandStop::NoActiveTurn,
        Some(_) if result.already_stopping => CommandStop::AlreadyStopping,
        Some(_) => CommandStop::Stop(stop),
    }
}

#[cfg(all(test, unix))]
#[path = "judged_stop_tests.rs"]
mod tests;
