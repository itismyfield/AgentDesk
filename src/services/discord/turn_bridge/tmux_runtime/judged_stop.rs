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

/// The actor reply or inflight row went unobserved; closed receiver evidence belongs to that handle.
#[derive(Debug)]
pub(in crate::services::discord) struct StopUnobserved {
    closed_receiver: bool,
    reason: &'static str,
}

impl StopUnobserved {
    pub(in crate::services::discord) fn reason(&self) -> &'static str {
        self.reason
    }
}

/// A channel's judged stop; `Ok(None)` with no active turn.
pub(in crate::services::discord) type ChannelJudgement =
    Result<Option<ChannelStop>, StopUnobserved>;

/// Whether `judgement` keeps the turn: a refused host or a turn that could not be read.
pub(in crate::services::discord) fn keeps_turn(judgement: &ChannelJudgement) -> bool {
    match judgement {
        Ok(stop) => stop.as_ref().is_some_and(ChannelStop::refused),
        Err(_) => true,
    }
}

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
    /// Judges `channel`'s active turn. `approved` is a force-kill verdict's session; with
    /// `bind_unbound`, a token with no name is judged by its inflight row.
    pub(in crate::services::discord) async fn judge(
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        channel: ChannelId,
        approved: Option<Option<&str>>,
        bind_unbound: bool,
    ) -> ChannelJudgement {
        let Some(handle) = shared.mailbox_peek(channel) else {
            return Ok(None);
        };
        let token = match handle.cancel_token().await {
            Ok(token) => token,
            Err(_) => {
                let mut error = unobserved(channel, "mailbox actor unreachable".to_string());
                // Observe the receiver that failed, never a later registry incarnation.
                error.closed_receiver = handle.is_closed();
                error.reason = "actor_unreachable";
                return Err(error);
            }
        };
        let Some(token) = token else {
            return Ok(None);
        };
        let bound = token.tmux_session_name();
        let unbound = bound.is_none() && bind_unbound;
        let name = match unbound {
            true => inflight_name(provider, channel).map_err(|error| unobserved(channel, error))?,
            false => bound,
        };
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
                providers
                    .iter()
                    .find_map(|p| inflight_name(p, channel).ok().flatten())
            });
        Ok(Some(stop))
    }

    /// Offline consumers may finish a directly observed closed receiver; reply loss stays unknown.
    pub(in crate::services::discord) fn offline_if_closed(
        judged: ChannelJudgement,
    ) -> ChannelJudgement {
        match judged {
            Err(error) if error.closed_receiver => Ok(None),
            other => other,
        }
    }

    /// Judges `token` by `name` alone; [`Self::judge`] adds what the cancel binds and records.
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

    /// The judged token, which keys the finish that may follow the stop.
    pub(in crate::services::discord) fn token(&self) -> &Arc<CancelToken> {
        &self.token
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

/// The session `provider`'s row on `channel` names; a row that fails to read or parse is an
/// error, never "no name".
fn inflight_name(provider: &ProviderKind, channel: ChannelId) -> Result<Option<String>, String> {
    let row = inflight::load_inflight_state_read_only_result(provider, channel.get())?;
    Ok(row.and_then(|row| row.tmux_session_name))
}

fn unobserved(channel: ChannelId, error: String) -> StopUnobserved {
    let channel_id = channel.get();
    tracing::warn!(channel_id, %error, "stop keeps a turn it could not judge");
    StopUnobserved {
        closed_receiver: false,
        reason: "stop_unobserved",
    }
}

pub(in crate::services::discord) enum CommandStop {
    NoActiveTurn,
    /// The turn's host is not a confirmed legacy tmux, or could not be read: nothing was cancelled.
    HostRefused,
    AlreadyStopping,
    Stop(ChannelStop),
    Session(super::SessionStop),
    /// A Herdr turn's user stop: nothing was cancelled, and the provider still ends the turn.
    #[cfg(unix)]
    Herdr(super::codex_stop_delivery::HerdrStop),
}

/// A user stop: judged before any write, then cancelled only when the host is admitted.
pub(in crate::services::discord) async fn begin_command_stop(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    bind_unbound: bool,
) -> CommandStop {
    begin_stop(shared, provider, channel, bind_unbound, None).await
}

/// [`begin_command_stop`] for a stop naming its command: a Herdr turn takes the intent and
/// Escape path instead of any cancel.
pub(in crate::services::discord) async fn begin_user_stop(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    bind_unbound: bool,
    reason: &str,
) -> CommandStop {
    begin_stop(shared, provider, channel, bind_unbound, Some(reason)).await
}

async fn begin_stop(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    bind_unbound: bool,
    reason: Option<&str>,
) -> CommandStop {
    #[cfg(all(test, unix))]
    if crate::services::provider::cancel_token_claude_interrupt::herdr_interrupt_mutant("wire_stop")
        && crate::services::provider::cancel_token_claude_interrupt::herdr_cancel_enabled()
        && let Some(handle) = shared.mailbox_peek(channel)
        && let Ok(Some(token)) = handle.cancel_token().await
        && super::codex_stop_delivery::admit_herdr_command(
            &token,
            Some(&token),
            provider,
            channel.get(),
            &shared.token_hash,
            "/stop",
        )
        .is_ok()
        && let Some(pool) = shared.pg_pool.as_ref()
    {
        let _ = super::codex_stop_delivery::interrupt_herdr(pool, &token, provider).await;
    }
    let judgement = ChannelStop::judge(shared, provider, channel, None, bind_unbound).await;
    #[cfg(unix)]
    if let Some(reason) = reason
        && let Some(token) = herdr_turn(&judgement)
    {
        let stop = super::codex_stop_delivery::herdr_command_stop;
        return CommandStop::Herdr(stop(shared, provider, channel, token, reason).await);
    }
    #[cfg(not(unix))]
    let _ = reason;
    // A confirmed channel's Discord turn still holds its mailbox token and keeps the channel stop.
    if matches!(judgement, Ok(None))
        && crate::services::tui_o::turn_mode::transcript_turns(channel.get())
    {
        return super::SessionStop::judge(shared, provider, channel).await;
    }
    if keeps_turn(&judgement) {
        return CommandStop::HostRefused;
    }
    let Ok(Some(stop)) = judgement else {
        return CommandStop::NoActiveTurn;
    };
    let result = stop.cancel().await;
    match result.token {
        None => CommandStop::NoActiveTurn,
        Some(_) if result.already_stopping => CommandStop::AlreadyStopping,
        Some(_) => CommandStop::Stop(stop),
    }
}

/// The judged token when it is a Herdr turn's; settlement is checked first, so nothing else is
/// read while it is unavailable.
#[cfg(unix)]
fn herdr_turn(judgement: &ChannelJudgement) -> Option<&Arc<CancelToken>> {
    use crate::services::provider::cancel_token_claude_interrupt::herdr_stop_settlement_available;
    if !herdr_stop_settlement_available() {
        return None;
    }
    let Ok(Some(stop)) = judgement else {
        return None;
    };
    stop.token.herdr_interrupt_state().map(|_| &stop.token)
}

#[cfg(all(test, unix))]
#[path = "judged_stop_tests.rs"]
mod tests;
