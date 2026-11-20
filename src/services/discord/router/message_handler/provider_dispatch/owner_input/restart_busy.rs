//! Restore a running turn before the owner chooses its ordinary busy or idle path.
use super::*;
use crate::services::discord::inflight::{self, InflightTurnState};
use crate::services::discord::recovery_engine::herdr_reattach::continue_channel;
use crate::services::discord::session_transition::SessionTransitionLease;

pub(super) async fn recover_active(
    shared: Option<&Shared>,
    provider: &ProviderKind,
    channel: ChannelId,
) {
    let Some(shared) = shared else { return };
    let Some(row) = inflight::load_inflight_state_read_only(provider, channel.get()) else {
        return;
    };
    if crate::services::turn_host::herdr_restart_candidate(
        channel.get(),
        (
            row.hosted_execution_nonce.is_some(),
            row.runtime_kind,
            row.output_path.is_none(),
        ),
    ) && !inflight::row_is_current_generation(&row, shared.restart.current_generation)
        && crate::services::discord::mailbox_snapshot(shared, channel)
            .await
            .cancel_token
            .is_none()
        && let Ok(lease) = SessionTransitionLease::try_acquire(shared, channel)
    {
        let _ = continue_channel(shared, provider, channel, &lease).await;
    }
}

pub(super) async fn registration(
    (mailbox, pool, runtime): (
        &ChannelMailboxHandle,
        Option<&sqlx::PgPool>,
        Option<&Shared>,
    ),
    provider: &ProviderKind,
    channel: u64,
    front: &Front,
) -> Result<(InflightTurnState, String, TurnHost, Registration), String> {
    let row = crate::services::discord::inflight::load_inflight_state_read_only(provider, channel)
        .ok_or_else(unobserved)?;
    let session = row.tmux_session_name.clone().ok_or_else(unobserved)?;
    let key = row.session_key.as_deref();
    let host = crate::services::turn_host::for_turn(pool, provider, channel, key).await;
    let origin = front_origin(mailbox, front).await?;
    let observed = owned_busy::running(
        runtime,
        mailbox,
        (provider, origin),
        (channel, &session, &host),
        front,
    )
    .await?;
    // Claude's prelaunch row has no transcript until its observer hands it back. Its live pane
    // already has the binding that the idle observer used before submitting the first input.
    let binding = (*provider == ProviderKind::Claude
        && matches!(&host, TurnHost::Tmux)
        && row.output_path.is_none())
    .then(|| crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&session))
    .flatten()
    .filter(|binding| {
        binding.runtime_kind == crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui
    });
    let output = row
        .output_path
        .as_deref()
        .or_else(|| binding.as_ref().map(|binding| binding.output_path.as_str()));
    let native = binding
        .as_ref()
        .and_then(|binding| binding.session_id.as_deref())
        .or(row.session_id.as_deref());
    let live = output.map(|path| (path, native));
    let at = (&host, session.as_str(), live);
    let kind = (provider.as_str(), origin, true);
    let owner = observed
        .map(Ok)
        .unwrap_or_else(|| Registration::observed(mailbox.clone(), kind, at, &front.1, None))
        .map_err(|error| {
            if effect::surely_unsent(&error) {
                error
            } else {
                effect::unsent(format!("busy_source_unavailable: {error}"))
            }
        })?;
    if owner.phase != target::Phase::Busy {
        return Err(unobserved());
    }
    Ok((row, session, host, owner))
}

pub(super) async fn link_attempt(
    mailbox: &ChannelMailboxHandle,
    row: &InflightTurnState,
    attempt: Option<&str>,
) {
    // The running turn's token learns the attempt before its paste can land a row.
    let active = mailbox.cancel_token().await.ok().flatten();
    if let (Some(active), Some(attempt)) = (active, attempt)
        && active
            .turn_nonce()
            .is_some_and(|nonce| row.turn_nonce.as_deref() == Some(nonce))
        && !active.native_stop.is_completed()
    {
        crate::services::discord::inflight::record_native_busy_child(row, &active, attempt);
    }
}

impl OwnerTurn {
    pub(in crate::services::discord) async fn submit_restarted_busy(
        self,
        shared: &Shared,
        provider: &ProviderKind,
        channel: ChannelId,
    ) -> Result<Self, ()> {
        if !self.continues_herdr()
            || !self.origin.is_person()
            || *provider != ProviderKind::Codex
            || self.mailbox.has_active_turn().await != Ok(true)
        {
            return Ok(self);
        }
        let at = (&self.mailbox, shared.pg_pool.as_ref(), Some(shared));
        let Ok((row, session, host, mut registration)) =
            registration(at, provider, channel.get(), self.front()).await
        else {
            self.defer_before_submit("herdr_restart_busy_unready");
            return Err(());
        };
        link_attempt(&self.mailbox, &row, registration.attempt["nonce"].as_str()).await;
        registration.restarted = self.late.restarted;
        let _ = paste_busy(
            (
                shared.pg_pool.clone(),
                provider.clone(),
                Some(shared.clone()),
            ),
            (channel.get(), session, host),
            self.claim,
            registration,
            self.settled,
        )
        .await;
        Err(())
    }
}
