use super::*;

#[cfg(all(test, unix))]
use crate::services::agent_protocol::NativeTerminalKind;
#[cfg(all(test, unix))]
use crate::services::cluster::channel_home_port::scoped_restore::{
    InflightObservation, RestoreContext, RestoreScope, RestoreStatus, RestoreWitness,
    mutant as scoped_mutant,
};

#[cfg(all(test, unix))]
pub(in crate::services::discord) struct InstalledInflight {
    scope: RestoreScope,
    generation: u64,
    state: Option<inflight::InflightTurnState>,
    queue_exit_events: Vec<crate::services::turn_orchestrator::QueueExitEvent>,
    activated: bool,
    effects_started: std::sync::atomic::AtomicBool,
    pub(in crate::services::discord) observation: InflightObservation,
}

#[cfg(all(test, unix))]
fn check_restore_context(context: &RestoreContext, provider: &ProviderKind) -> Result<(), String> {
    context.check()?;
    if context.scope.provider != provider.as_str()
        || context.scope.channel == 0
        || crate::services::discord::input_runtime::fence::lookup(provider, context.scope.channel)
            .is_some()
    {
        return Err("restore scope/input mismatch".into());
    }
    Ok(())
}

/// Installs or observes exactly one canonical episode; delivery and provider work are separate.
#[cfg(all(test, unix))]
pub(in crate::services::discord) async fn install_inflight_scoped(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    context: &RestoreContext,
) -> Result<InstalledInflight, String> {
    check_restore_context(context, provider)?;
    if scoped_mutant("install-effects") {
        restore_inflight_turns(http, shared, provider).await;
    }
    let probe = inflight::load_inflight_probe_scoped(provider, context.scope.channel);
    if !probe.complete || probe.states.len() > 1 {
        return Err("inflight probe incomplete".into());
    }
    let installed = |state, observation| InstalledInflight {
        scope: context.scope.clone(),
        generation: context.generation,
        state,
        queue_exit_events: Vec::new(),
        activated: false,
        effects_started: std::sync::atomic::AtomicBool::new(false),
        observation,
    };
    let Some(expected) = probe.states.into_iter().next() else {
        return Ok(installed(None, InflightObservation::Empty));
    };
    let pin = inflight::InflightEpisodePin::from_state(&expected);
    let guard = inflight::lock_inflight_episode(provider, context.scope.channel, &pin)
        .map_err(|error| format!("inflight episode unavailable: {error:?}"))?;
    let mut state = guard.state().clone();
    if state.runtime_kind_unknown_on_disk || state.version > inflight::inflight_state_version() {
        return Err("unsupported inflight format/runtime".into());
    }
    let lookup =
        replay_hold::hydrate_replay_hold_checked(shared.pg_pool.as_ref(), &mut state).await;
    if let replay_hold::ReplayLookup::Unverified { reason, .. } = lookup
        && !scoped_mutant("receipt-complete")
    {
        return Err(reason);
    }
    check_restore_context(context, provider)?;
    let mut queue_exit_events = Vec::new();
    let mut activated = false;
    let observation = if state.replay_rerun_blocked() {
        InflightObservation::ReplayHeld {
            receipt: state.replay_receipt_id,
            reasons: state.replay_hold_reasons.clone(),
            delivery: None,
        }
    } else if matches!(
        crate::services::agent_recovery::channel_recovery_intake(
            provider,
            &state.channel_id.to_string()
        )
        .await,
        Some(crate::services::agent_recovery::RecoveryIntake::Skip)
    ) {
        InflightObservation::Deferred("recovery intake owns the row".into())
    } else if state.rebind_origin {
        InflightObservation::Deferred("rebind owns the row".into())
    } else if herdr_turn_held(provider, &state) {
        InflightObservation::HerdrHeld
    } else if super::herdr_admitted_restart::admitted(provider, &state).is_some() {
        InflightObservation::Admitted {
            kind: state.tui_terminal_kind.expect("admitted kind"),
        }
    } else if !context.scope.held_locally() && !scoped_mutant("target-recovery") {
        InflightObservation::Deferred("released target preparation awaits adoption".into())
    } else {
        // Ordinary invalidation rechecks the locked snapshot and canonical replay authority.
        // Held and admitted rows never enter this destructive branch.
        drop(guard);
        if inflight::invalidate_stale_generation_scoped(
            shared.pg_pool.as_ref(),
            provider,
            shared.restart.current_generation,
            &expected,
        )
        .await?
        {
            return Ok(installed(None, InflightObservation::Empty));
        }
        let guard = inflight::lock_inflight_episode(provider, context.scope.channel, &pin)
            .map_err(|error| format!("inflight episode changed: {error:?}"))?;
        state = guard.state().clone();
        check_restore_context(context, provider)?;
        if recovery_terminal_delivery_already_committed(&state) || state.request_owner_user_id == 0
        {
            InflightObservation::Deferred("canonical row awaits authorized recovery".into())
        } else {
            let channel = ChannelId::new(state.channel_id);
            let before = shared
                .mailbox(channel)
                .try_snapshot()
                .await
                .map_err(|e| format!("mailbox unavailable: {e:?}"))?;
            if before.cancel_token.is_some()
                && (state.turn_nonce.as_deref().is_none_or(str::is_empty)
                    || before.active_turn_nonce != state.turn_nonce
                    || before.active_user_message_id
                        != inflight::opt_message_id(state.effective_finalizer_turn_id()))
            {
                return Err("mailbox belongs to another episode".into());
            }
            if before.cancel_token.is_none() {
                let _admission = crate::services::agent_recovery::admission::admit(
                    &state.channel_id.to_string(),
                    provider,
                )
                .await
                .map_err(|e| format!("recovery installation admission refused: {e}"))?;
                let token = Arc::new(CancelToken::from_persisted_turn_nonce(
                    state.turn_nonce.clone(),
                ));
                super::ensure_cancel_token_bound_from_inflight_state(
                    provider,
                    &state,
                    &token,
                    "scoped installation",
                );
                let claim = shared
                    .mailbox(channel)
                    .try_start_turn_adopting(
                        token,
                        UserId::new(state.request_owner_user_id),
                        inflight::opt_message_id(state.effective_finalizer_turn_id())
                            .ok_or("invalid finalizer id")?,
                        crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent,
                        state.turn_nonce.clone(),
                        queue_persistence_context(shared, provider, channel),
                    )
                    .await;
                if !claim.started || claim.persistence_error.is_some() {
                    return Err("active claim or queue persistence refused".into());
                }
                activated = true;
                queue_exit_events = claim.queue_exit_events;
                if scoped_mutant("install-feedback") {
                    apply_queue_exit_feedback(shared, channel, &queue_exit_events).await;
                }
            }
            // The existing helper now sees our actor; it only reseeds the ledger and binding.
            if !super::reregister_active_turn_from_inflight_under_episode_guard(shared, &state)
                .await
            {
                return Err("active inflight installation refused".into());
            }
            let after = shared
                .mailbox(channel)
                .try_snapshot()
                .await
                .map_err(|e| format!("mailbox unavailable: {e:?}"))?;
            if after.cancel_token.is_none()
                || after.active_turn_nonce != state.turn_nonce
                || after.active_user_message_id
                    != inflight::opt_message_id(state.effective_finalizer_turn_id())
            {
                return Err("active inflight installation unconfirmed".into());
            }
            InflightObservation::Active
        }
    };
    check_restore_context(context, provider)?;
    let mut result = installed(Some(state), observation);
    result.queue_exit_events = queue_exit_events;
    result.activated = activated;
    Ok(result)
}

/// Consumes installation proof only after fresh home, input and replay admission.
#[cfg(all(test, unix))]
pub(in crate::services::discord) async fn resume_inflight_scoped(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    context: &RestoreContext,
    witness: &RestoreWitness,
    installed: &InstalledInflight,
) -> Result<InflightObservation, String> {
    use crate::services::cluster::channel_home::{self, HomeOwnership};
    check_restore_context(context, provider)?;
    if witness.status != RestoreStatus::Restored
        || witness.scope.as_ref() != Some(&context.scope)
        || witness.generation != context.generation
        || installed.scope != context.scope
        || installed.generation != context.generation
        || !context.scope.held_locally()
    {
        return Err("inflight resume lacks current installation proof".into());
    }
    let pool = shared
        .pg_pool
        .as_ref()
        .ok_or("home authority unavailable")?;
    let home = crate::db::o_channel_homes::read_home(pool, &context.scope.channel.to_string())
        .await
        .map_err(|e| format!("home lookup failed: {e:?}"))?
        .ok_or("home identity absent")?;
    if !context.scope.matches(&home) && !scoped_mutant("home-identity") {
        return Err("home identity changed".into());
    }
    let gate = channel_home::registered_channel(context.scope.channel).ok_or("home gate absent")?;
    if gate.holder() != context.scope.local
        || !matches!(gate.ownership(),
        HomeOwnership::Owned { home_epoch, .. } if home_epoch == context.scope.epoch)
    {
        return Err("home output authority lost".into());
    }
    let Some(expected) = &installed.state else {
        return Ok(InflightObservation::Empty);
    };
    let pin = inflight::InflightEpisodePin::from_state(expected);
    let guard = inflight::lock_inflight_episode(provider, context.scope.channel, &pin)
        .map_err(|e| format!("resume episode changed: {e:?}"))?;
    let mut state = guard.state().clone();
    if let replay_hold::ReplayLookup::Unverified { reason, .. } =
        replay_hold::hydrate_replay_hold_checked(Some(pool), &mut state).await
    {
        return Err(reason);
    }
    drop(guard); // Canonical delivery/clear APIs take this same lock themselves.
    check_restore_context(context, provider)?;
    let permit = if state.replay_rerun_blocked()
        || herdr_turn_held(provider, &state)
        || super::herdr_admitted_restart::admitted(provider, &state).is_some()
    {
        gate.admit_recovery(provider.as_str())
    } else {
        gate.admit_command(provider.as_str())
    }
    .ok_or("home execution admission refused")?;
    if installed
        .effects_started
        .swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        return Err("inflight effects already started for this installation".into());
    }
    channel_home::command_scope(Some(permit), async {
        apply_queue_exit_feedback(
            shared,
            ChannelId::new(state.channel_id),
            &installed.queue_exit_events,
        )
        .await;
        check_restore_context(context, provider)?;
        if state.replay_rerun_blocked() {
            let delivery =
                match replay_hold::deliver_held_debt_checked(http, shared, provider, &mut state)
                    .await
                {
                    replay_hold::ReplayDebtDelivery::Acknowledged => Ok(()),
                    replay_hold::ReplayDebtDelivery::Retained(reason) => Err(reason.into()),
                };
            return Ok(InflightObservation::ReplayHeld {
                receipt: state.replay_receipt_id,
                reasons: state.replay_hold_reasons,
                delivery: Some(delivery),
            });
        }
        if matches!(
            crate::services::agent_recovery::channel_recovery_intake(
                provider,
                &state.channel_id.to_string()
            )
            .await,
            Some(crate::services::agent_recovery::RecoveryIntake::Skip)
        ) || state.rebind_origin
        {
            return Ok(InflightObservation::Deferred(
                "existing recovery owner retained the row".into(),
            ));
        }
        if herdr_turn_held(provider, &state) {
            return Ok(InflightObservation::HerdrHeld);
        }
        if scoped_mutant("admitted-kind") && state.tui_terminal_kind.is_some() {
            state.tui_terminal_kind = Some(NativeTerminalKind::Completed);
        }
        if let Some(terminal) = super::herdr_admitted_restart::admitted(provider, &state) {
            let kind = state.tui_terminal_kind.expect("admitted kind");
            return Ok(match terminal.settle_checked(http, shared, &state).await {
                super::herdr_admitted_restart::AdmittedRestartReport::Settled => {
                    InflightObservation::Settled { kind }
                }
                super::herdr_admitted_restart::AdmittedRestartReport::Retained(reason) => {
                    InflightObservation::Retained {
                        kind,
                        reason: reason.into(),
                    }
                }
            });
        }
        // An actor this installation merely observed retains its existing driver.
        if installed.observation == InflightObservation::Active && !installed.activated {
            return Ok(InflightObservation::Active);
        }
        Box::pin(restore_inflight_states(
            http,
            shared,
            provider,
            vec![state],
            installed.activated,
        ))
        .await;
        Ok(InflightObservation::Deferred(
            "existing recovery path visited".into(),
        ))
    })
    .await
}

pub(super) async fn installed_token(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    state: &inflight::InflightTurnState,
    candidate: Arc<CancelToken>,
) -> Arc<CancelToken> {
    shared
        .mailbox(channel_id)
        .try_snapshot()
        .await
        .ok()
        .filter(|s| {
            state
                .turn_nonce
                .as_deref()
                .is_some_and(|nonce| !nonce.is_empty())
                && s.active_turn_nonce == state.turn_nonce
                && s.active_user_message_id == inflight::opt_message_id(state.user_msg_id)
        })
        .and_then(|s| s.cancel_token)
        .unwrap_or(candidate)
}
