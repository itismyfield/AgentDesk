use super::*;

#[cfg(all(test, unix))]
use super::herdr_before_start_restart::{BeforeStartRestart, BeforeStartRestartReport};
#[cfg(all(test, unix))]
use crate::services::agent_protocol::NativeTerminalKind;
#[cfg(all(test, unix))]
use crate::services::cluster::channel_home_port::{
    RestoreFailure,
    scoped_restore::{
        InflightObservation, RestoreContext, RestoreScope, RestoreStatus, RestoreWitness,
        mutant as scoped_mutant,
    },
};

#[cfg(all(test, unix))]
pub(in crate::services::discord) struct InstalledInflight {
    scope: RestoreScope,
    generation: u64,
    state: Option<inflight::InflightTurnState>,
    actor: Option<crate::services::turn_orchestrator::installed_actor_handoff::Installation>,
    // True while this actor still owes a driver, including an inherited installation.
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

/// What the canonical row's own bytes say about a before-start policy receipt.
#[cfg(all(test, unix))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RawReceipt {
    Absent,
    Present,
    /// Unreadable bytes are never taken as absence.
    Indeterminate,
}

/// The exact episode and its raw receipt presence, both read under one canonical row lock.
#[cfg(all(test, unix))]
fn snapshot_receipt(
    provider: &ProviderKind,
    expected: &inflight::InflightTurnState,
) -> Result<(inflight::InflightTurnState, RawReceipt), String> {
    let pin = inflight::InflightEpisodePin::from_state(expected);
    let guard = inflight::lock_inflight_episode(provider, expected.channel_id, &pin)
        .map_err(|error| format!("retryable: inflight episode unavailable: {error:?}"))?;
    let state = guard.state().clone();
    if state.runtime_kind_unknown_on_disk || state.version > inflight::inflight_state_version() {
        return Err("unsupported inflight format/runtime".into());
    }
    let raw = match inflight::inflight_runtime_root().map(|root| {
        std::fs::read(inflight::inflight_state_path(
            &root,
            provider,
            expected.channel_id,
        ))
    }) {
        Some(Ok(bytes))
            if state.herdr_before_start_stop.is_some()
                || inflight::raw_before_start_receipt(&String::from_utf8_lossy(&bytes)) =>
        {
            RawReceipt::Present
        }
        Some(Ok(_)) => RawReceipt::Absent,
        _ => RawReceipt::Indeterminate,
    };
    drop(guard);
    Ok((state, raw))
}

#[cfg(all(test, unix))]
fn verify_snapshot(
    provider: &ProviderKind,
    snapshot: &inflight::InflightTurnState,
) -> Result<(), String> {
    verify_receipt(provider, snapshot, None)
}

/// Like [`verify_snapshot`], and also requires the same raw receipt presence when given.
#[cfg(all(test, unix))]
fn verify_receipt(
    provider: &ProviderKind,
    snapshot: &inflight::InflightTurnState,
    raw: Option<RawReceipt>,
) -> Result<(), String> {
    let (fresh, fresh_raw) = snapshot_receipt(provider, snapshot)?;
    if raw.is_some_and(|raw| raw != fresh_raw)
        || serde_json::to_value(&fresh).map_err(|e| e.to_string())?
            != serde_json::to_value(snapshot).map_err(|e| e.to_string())?
    {
        return Err("retryable: inflight progress changed during restore".into());
    }
    Ok(())
}

/// A receipt in the raw row takes the row off every actor, replay, native and provider path.
#[cfg(all(test, unix))]
fn policy_receipt(raw: RawReceipt, state: &inflight::InflightTurnState) -> bool {
    let unreadable = !state
        .herdr_before_start_stop
        .as_ref()
        .is_some_and(|receipt| receipt.supported());
    raw != RawReceipt::Absent
        && !scoped_mutant("b3b3-receipt-first")
        && !(unreadable && scoped_mutant("b3b3-raw-as-absent"))
}

/// Only a prior process's readable receipt with verified replay authority may be settled.
#[cfg(all(test, unix))]
fn classify_policy(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    raw: RawReceipt,
    lookup: &replay_hold::ReplayLookup,
) -> Result<BeforeStartRestart, RestoreFailure> {
    if raw == RawReceipt::Indeterminate {
        return Err(RestoreFailure::retryable(
            "policy receipt presence is indeterminate",
        ));
    }
    if let replay_hold::ReplayLookup::Unverified { reason, .. } = lookup
        && !scoped_mutant("b3b3-unverified-as-verified")
    {
        return Err(RestoreFailure::retryable(reason));
    }
    // The decoder holds a receipt of this process's generation even without an actor.
    match super::herdr_before_start_restart::decode(shared, provider, state) {
        Some(BeforeStartRestart::Held(why)) => Err(RestoreFailure::Rejected(why.into())),
        Some(settle) => Ok(settle),
        None => Err(RestoreFailure::Rejected(
            "a receipt this binary cannot read".into(),
        )),
    }
}

/// Settled only on the guarded clear of the exact row; any retained step stays held.
#[cfg(all(test, unix))]
fn policy_observation(report: BeforeStartRestartReport) -> InflightObservation {
    match report {
        BeforeStartRestartReport::Settled if scoped_mutant("b3b3-policy-native") => {
            InflightObservation::Settled {
                kind: NativeTerminalKind::Aborted,
            }
        }
        BeforeStartRestartReport::Retained(_) if scoped_mutant("b3b3-clear-is-success") => {
            InflightObservation::PolicyReceiptSettled
        }
        BeforeStartRestartReport::Settled => InflightObservation::PolicyReceiptSettled,
        BeforeStartRestartReport::Retained(why) => InflightObservation::PolicyReceiptHeld {
            reason: RestoreFailure::retryable(why),
        },
    }
}

#[cfg(all(test, unix))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RestoreEffect {
    PolicySettlement,
    NativeTerminalSettlement,
    ReplayDebtDelivery,
    ProviderExecution,
    NoEffect,
}

/// A receipt-free row keeps its existing class; anything unclassified admits no effect.
#[cfg(all(test, unix))]
fn restore_effect(
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    policy: bool,
) -> RestoreEffect {
    if policy || scoped_mutant("b3b3-n-as-policy") {
        RestoreEffect::PolicySettlement
    } else if state.replay_rerun_blocked() {
        RestoreEffect::ReplayDebtDelivery
    } else if super::herdr_admitted_restart::admitted(provider, state).is_some() {
        RestoreEffect::NativeTerminalSettlement
    } else if herdr_turn_held(provider, state)
        || state.rebind_origin
        || recovery_terminal_delivery_already_committed(state)
        || state.tui_terminal_kind.is_some()
        || !state.full_response.is_empty()
        || state.request_owner_user_id == 0
    {
        RestoreEffect::NoEffect
    } else {
        RestoreEffect::ProviderExecution
    }
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
        return Err(probe
            .failure
            .unwrap_or_else(|| "inflight probe incomplete".into())
            .into());
    }
    let installed = |state, observation| InstalledInflight {
        scope: context.scope.clone(),
        generation: context.generation,
        state,
        actor: None,
        activated: false,
        effects_started: std::sync::atomic::AtomicBool::new(false),
        observation,
    };
    let Some(expected) = probe.states.into_iter().next() else {
        return Ok(installed(None, InflightObservation::Empty));
    };
    let (snapshot, raw) = snapshot_receipt(provider, &expected)?;
    let mut state = snapshot.clone();
    if state.runtime_kind_unknown_on_disk || state.version > inflight::inflight_state_version() {
        return Err("unsupported inflight format/runtime".into());
    }
    b3a2_tests::pause("install-hydrate").await;
    let lookup =
        replay_hold::hydrate_replay_hold_checked(shared.pg_pool.as_ref(), &mut state).await;
    let receipt = policy_receipt(raw, &state);
    if let replay_hold::ReplayLookup::Unverified { reason, .. } = &lookup
        && !receipt
        && !scoped_mutant("receipt-complete")
    {
        return Err(reason.clone());
    }
    verify_receipt(provider, &snapshot, Some(raw))?;
    check_restore_context(context, provider)?;
    // A receipt row installs no actor and consults no other owner; only resume may settle it.
    if receipt && !scoped_mutant("b3b3-actor-first") {
        let reason = classify_policy(shared, provider, &state, raw, &lookup)
            .err()
            .unwrap_or_else(|| RestoreFailure::retryable("policy receipt awaits recovery"));
        return Ok(installed(
            Some(state),
            InflightObservation::PolicyReceiptHeld { reason },
        ));
    }
    let intake = if state.replay_rerun_blocked() {
        None
    } else {
        let intake = crate::services::agent_recovery::channel_recovery_intake(
            provider,
            &state.channel_id.to_string(),
        )
        .await;
        verify_snapshot(provider, &snapshot)?;
        check_restore_context(context, provider)?;
        intake
    };
    let mut actor = None;
    let mut activated = false;
    let observation = if state.replay_rerun_blocked() {
        InflightObservation::ReplayHeld {
            receipt: state.replay_receipt_id,
            reasons: state.replay_hold_reasons.clone(),
            delivery: None,
        }
    } else if matches!(
        intake,
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
        verify_snapshot(provider, &snapshot)?;
        check_restore_context(context, provider)?;
        if recovery_terminal_delivery_already_committed(&state) || state.request_owner_user_id == 0
        {
            InflightObservation::Deferred("canonical row awaits authorized recovery".into())
        } else {
            let channel = ChannelId::new(state.channel_id);
            b3a2_tests::pause("install-mailbox").await;
            let before = shared
                .mailbox(channel)
                .try_snapshot()
                .await
                .map_err(|e| format!("mailbox unavailable: {e:?}"))?;
            verify_snapshot(provider, &snapshot)?;
            check_restore_context(context, provider)?;
            if before.cancel_token.is_some()
                && ((state.turn_nonce.as_deref().is_none_or(str::is_empty)
                    && !before.cancel_token.as_ref().is_some_and(|token| {
                        crate::services::turn_orchestrator::installed_actor_handoff::belongs(
                            channel,
                            token,
                            &birth_key(&state),
                        )
                    }))
                    || before.active_request_owner
                        != Some(UserId::new(state.request_owner_user_id))
                    || before.active_turn_nonce != state.turn_nonce
                    || before.active_user_message_id
                        != inflight::opt_message_id(state.effective_finalizer_turn_id()))
            {
                return Err("mailbox belongs to another episode".into());
            }
            if before.cancel_token.is_none() {
                b3a2_tests::pause("install-admission").await;
                let _admission = crate::services::agent_recovery::admission::admit(
                    &state.channel_id.to_string(),
                    provider,
                )
                .await
                .map_err(|e| format!("recovery installation admission refused: {e}"))?;
                verify_snapshot(provider, &snapshot)?;
                check_restore_context(context, provider)?;
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
                        token.clone(),
                        UserId::new(state.request_owner_user_id),
                        inflight::opt_message_id(state.effective_finalizer_turn_id())
                            .ok_or("invalid finalizer id")?,
                        crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent,
                        state.turn_nonce.clone(),
                        queue_persistence_context(shared, provider, channel),
                    )
                    .await;
                verify_snapshot(provider, &snapshot)?;
                check_restore_context(context, provider)?;
                if !claim.started || claim.persistence_error.is_some() {
                    return Err("active claim or queue persistence refused".into());
                }
                let proof = actor_proof(context, &state, token)?;
                if scoped_mutant("install-feedback") {
                    apply_queue_exit_feedback(shared, channel, &claim.queue_exit_events).await;
                    verify_snapshot(provider, &snapshot)?;
                }
                activated = crate::services::turn_orchestrator::installed_actor_handoff::remember(
                    &proof,
                    true,
                    claim.queue_exit_events,
                )?;
                actor = Some(proof);
            }
            // The existing helper now sees our actor; it only reseeds the ledger and binding.
            if !super::reregister_active_turn_from_inflight_under_episode_guard(shared, &state)
                .await
            {
                return Err("active inflight installation refused".into());
            }
            verify_snapshot(provider, &snapshot)?;
            check_restore_context(context, provider)?;
            let after = shared
                .mailbox(channel)
                .try_snapshot()
                .await
                .map_err(|e| format!("mailbox unavailable: {e:?}"))?;
            verify_snapshot(provider, &snapshot)?;
            check_restore_context(context, provider)?;
            if after.cancel_token.is_none()
                || after.active_request_owner != Some(UserId::new(state.request_owner_user_id))
                || after.active_turn_nonce != state.turn_nonce
                || after.active_user_message_id
                    != inflight::opt_message_id(state.effective_finalizer_turn_id())
            {
                return Err("active inflight installation unconfirmed".into());
            }
            if actor.is_none() {
                let proof = actor_proof(
                    context,
                    &state,
                    after.cancel_token.clone().expect("confirmed actor"),
                )?;
                activated = crate::services::turn_orchestrator::installed_actor_handoff::remember(
                    &proof,
                    false,
                    Vec::new(),
                )?;
                actor = Some(proof);
            }
            InflightObservation::Active
        }
    };
    check_restore_context(context, provider)?;
    let mut result = installed(Some(state), observation);
    result.actor = actor;
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
    let (snapshot, raw) = snapshot_receipt(provider, expected)?;
    let mut state = snapshot.clone();
    b3a2_tests::pause("resume-hydrate").await;
    let lookup = replay_hold::hydrate_replay_hold_checked(Some(pool), &mut state).await;
    let receipt = policy_receipt(raw, &state);
    if let replay_hold::ReplayLookup::Unverified { reason, .. } = &lookup
        && !receipt
    {
        return Err(reason.clone());
    }
    b3a2_tests::pause("resume-receipt").await;
    verify_receipt(provider, &snapshot, Some(raw))?;
    check_restore_context(context, provider)?;
    let mut policy = None;
    if receipt {
        // Settlement takes only a receipt that installation held without an actor.
        let held = installed.actor.is_none()
            && matches!(
                installed.observation,
                InflightObservation::PolicyReceiptHeld { .. }
            );
        match classify_policy(shared, provider, &state, raw, &lookup) {
            Ok(settle) if held => policy = Some(settle),
            Ok(_) => {
                return Ok(InflightObservation::PolicyReceiptHeld {
                    reason: RestoreFailure::Rejected("receipt follows an actor".into()),
                });
            }
            Err(reason) => return Ok(InflightObservation::PolicyReceiptHeld { reason }),
        }
    } else if !state.replay_rerun_blocked() {
        let intake = crate::services::agent_recovery::channel_recovery_intake(
            provider,
            &state.channel_id.to_string(),
        )
        .await;
        verify_snapshot(provider, &snapshot)?;
        check_restore_context(context, provider)?;
        if matches!(
            intake,
            Some(crate::services::agent_recovery::RecoveryIntake::Skip)
        ) || state.rebind_origin
        {
            return Ok(InflightObservation::Deferred(
                "existing recovery owner retained the row".into(),
            ));
        }
    }
    // Receipt lookup may yield; the full home identity is read again immediately before admission.
    let home = crate::db::o_channel_homes::read_home(pool, &context.scope.channel.to_string())
        .await
        .map_err(|e| format!("home lookup failed: {e:?}"))?
        .ok_or("home identity absent")?;
    verify_receipt(provider, &snapshot, Some(raw))?;
    check_restore_context(context, provider)?;
    if !context.scope.matches(&home) && !scoped_mutant("b3b3-home-stale") {
        return Err("home identity changed after receipt lookup".into());
    }
    let effect = restore_effect(provider, &state, policy.is_some());
    if effect == RestoreEffect::NoEffect {
        return Ok(if herdr_turn_held(provider, &state) {
            InflightObservation::HerdrHeld
        } else {
            InflightObservation::Deferred(
                "completion evidence or recovery effect classification unavailable".into(),
            )
        });
    }
    // Settlement and existing output need only recovery admission; a provider run needs a command.
    let recovery = effect != RestoreEffect::ProviderExecution
        && !(effect == RestoreEffect::PolicySettlement && scoped_mutant("b3b3-policy-command"));
    let permit = gate
        .admit_at_epoch(provider.as_str(), context.scope.epoch, recovery)
        .ok_or("home execution admission refused")?;
    if installed
        .effects_started
        .swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        return Err("inflight effects already started for this installation".into());
    }
    channel_home::command_scope(Some(permit), async {
        // A policy receipt settles before any handoff, replay, native or provider path.
        if effect == RestoreEffect::PolicySettlement {
            let settle = policy.unwrap_or(BeforeStartRestart::Held("no policy receipt to settle"));
            return Ok(policy_observation(settle.settle(http, shared, &state).await));
        }
        if installed.activated {
            let proof = installed.actor.clone().ok_or("installed actor proof absent")?;
            match checked_handoff(shared, provider, &state, proof, false).await {
                crate::services::turn_orchestrator::installed_actor_handoff::HandoffOutcome::Ready(events) => {
                    apply_queue_exit_feedback(shared, ChannelId::new(state.channel_id), &events).await;
                }
                crate::services::turn_orchestrator::installed_actor_handoff::HandoffOutcome::AlreadyDriven => return Ok(InflightObservation::Active),
                crate::services::turn_orchestrator::installed_actor_handoff::HandoffOutcome::Refused(reason) => return Err(reason.into()),
            }
        }
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
        if effect != RestoreEffect::ProviderExecution {
            return Ok(InflightObservation::Deferred("output-only recovery retained the row".into()));
        }
        // An actor this installation merely observed retains its existing driver.
        if installed.observation == InflightObservation::Active && !installed.activated {
            return Ok(InflightObservation::Active);
        }
        let restore = Box::pin(restore_inflight_states(http, shared, provider, vec![state], installed.activated));
        if let Some(proof) = installed.actor.clone() {
            let report = Arc::new(std::sync::Mutex::new(None));
            RESUME_REPORT.scope(report.clone(), RESUME_ACTOR.scope(proof, restore)).await;
            if let Some(outcome) = report.lock().unwrap().take() {
                outcome.map_err(String::from)?;
                return Ok(InflightObservation::Active);
            }
        } else {
            restore.await;
        }
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
    #[cfg(all(test, unix))]
    if !crate::services::turn_orchestrator::installed_actor_handoff::mutant("legacy-token")
        && let Ok(snapshot) = shared.mailbox(channel_id).try_snapshot().await
        && snapshot.active_user_message_id
            == inflight::opt_message_id(state.effective_finalizer_turn_id())
        && let Some(token) = snapshot.cancel_token
        && crate::services::turn_orchestrator::installed_actor_handoff::belongs(
            channel_id,
            &token,
            &birth_key(state),
        )
    {
        return token;
    }
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

#[cfg(all(test, unix))]
type ResumeReport = Arc<
    std::sync::Mutex<
        Option<Result<bool, crate::services::cluster::channel_home_port::RestoreFailure>>,
    >,
>;
#[cfg(all(test, unix))]
tokio::task_local! {
    static RESUME_ACTOR: crate::services::turn_orchestrator::installed_actor_handoff::Installation;
    static RESUME_REPORT: ResumeReport;
}
#[cfg(all(test, unix))]
fn episode_key(state: &inflight::InflightTurnState) -> String {
    format!("{:?}", inflight::InflightEpisodePin::from_state(state))
}
#[cfg(all(test, unix))]
fn birth_key(state: &inflight::InflightTurnState) -> String {
    format!(
        "{:?}",
        (
            &state.provider,
            state.channel_id,
            state.request_owner_user_id,
            state.user_msg_id,
            &state.turn_nonce,
            state.born_generation,
            &state.started_at
        )
    )
}
#[cfg(all(test, unix))]
fn actor_proof(
    context: &RestoreContext,
    state: &inflight::InflightTurnState,
    token: Arc<CancelToken>,
) -> Result<crate::services::turn_orchestrator::installed_actor_handoff::Installation, String> {
    Ok(
        crate::services::turn_orchestrator::installed_actor_handoff::Installation {
            channel: ChannelId::new(state.channel_id),
            token,
            episode: episode_key(state),
            birth: birth_key(state),
            owner: UserId::new(state.request_owner_user_id),
            message: inflight::opt_message_id(state.effective_finalizer_turn_id())
                .ok_or("invalid actor message")?,
            generation: context.generation,
            current: context.current.clone(),
        },
    )
}
#[cfg(all(test, unix))]
async fn checked_handoff(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    proof: crate::services::turn_orchestrator::installed_actor_handoff::Installation,
    drive: bool,
) -> crate::services::turn_orchestrator::installed_actor_handoff::HandoffOutcome {
    use crate::services::turn_orchestrator::installed_actor_handoff::HandoffOutcome;
    if proof.episode != episode_key(state)
        || proof.channel.get() != state.channel_id
        || provider.as_str() != state.provider
    {
        return HandoffOutcome::Refused("handoff proof scope changed".into());
    }
    let Ok(_admission) =
        crate::services::agent_recovery::admission::admit(&state.channel_id.to_string(), provider)
            .await
    else {
        return HandoffOutcome::Refused("handoff recovery admission refused".into());
    };
    let pin = inflight::InflightEpisodePin::from_state(state);
    let provider = provider.clone();
    let channel = proof.channel;
    shared
        .mailbox(channel)
        .handoff_installed(proof, drive, move || {
            if crate::services::discord::input_runtime::fence::lookup(&provider, channel.get())
                .is_some()
            {
                return Err("handoff input protected".into());
            }
            let path = inflight::inflight_state_path(
                &inflight::inflight_runtime_root().ok_or("handoff root absent")?,
                &provider,
                channel.get(),
            );
            let guard = inflight::try_lock_inflight_state_path(&path)
                .map_err(|error| format!("retryable: {error}"))?;
            let fresh: inflight::InflightTurnState = serde_json::from_slice(
                &std::fs::read(&path).map_err(|error| format!("retryable: {error}"))?,
            )
            .map_err(|error| error.to_string())?;
            if fresh.runtime_kind_unknown_on_disk
                || fresh.version > inflight::inflight_state_version()
            {
                return Err("handoff format/runtime changed".into());
            }
            if !pin.matches_state(&fresh) {
                return Err("handoff canonical episode changed".into());
            }
            Ok(Box::new(guard) as Box<dyn Send>)
        })
        .await
}

pub(super) async fn checked_kickoff(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    token: Arc<CancelToken>,
    state: &inflight::InflightTurnState,
) -> crate::services::turn_orchestrator::RecoveryKickoffResult {
    use crate::services::turn_orchestrator::RecoveryKickoffResult;
    if state.provider != provider.as_str() || state.channel_id != channel.get() {
        return RecoveryKickoffResult::Unavailable;
    }
    #[cfg(all(test, unix))]
    {
        if crate::services::turn_orchestrator::installed_actor_handoff::mutant("generic-kickoff") {
            return mailbox_recovery_kickoff(
                shared,
                channel,
                token,
                UserId::new(state.request_owner_user_id),
                inflight::opt_message_id(state.user_msg_id),
            )
            .await;
        }
        let Ok(proof) = RESUME_ACTOR.try_with(Clone::clone) else {
            return RecoveryKickoffResult::Unavailable;
        };
        if !Arc::ptr_eq(&proof.token, &token) {
            let _ = RESUME_REPORT.try_with(|report| {
                *report.lock().unwrap() = Some(Err("handoff token changed".into()))
            });
            return RecoveryKickoffResult::Unavailable;
        }
        match checked_handoff(shared, provider, state, proof, true).await {
            crate::services::turn_orchestrator::installed_actor_handoff::HandoffOutcome::Ready(events) => {
                apply_queue_exit_feedback(shared, channel, &events).await;
                let _ = RESUME_REPORT.try_with(|report| *report.lock().unwrap() = Some(Ok(true)));
                RecoveryKickoffResult::Activated
            }
            crate::services::turn_orchestrator::installed_actor_handoff::HandoffOutcome::AlreadyDriven => {
                let _ = RESUME_REPORT.try_with(|report| *report.lock().unwrap() = Some(Ok(false)));
                RecoveryKickoffResult::Unavailable
            }
            crate::services::turn_orchestrator::installed_actor_handoff::HandoffOutcome::Refused(reason) => {
                let _ = RESUME_REPORT.try_with(|report| *report.lock().unwrap() = Some(Err(reason)));
                RecoveryKickoffResult::Unavailable
            }
        }
    }
    #[cfg(not(all(test, unix)))]
    mailbox_recovery_kickoff(
        shared,
        channel,
        token,
        UserId::new(state.request_owner_user_id),
        inflight::opt_message_id(state.user_msg_id),
    )
    .await
}

#[cfg(all(test, unix))]
#[cfg(test)]
mod b3a1_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(super) fn context(
        channel: u64,
        generation: u64,
        current: Arc<AtomicU64>,
    ) -> RestoreContext {
        RestoreContext {
            scope: RestoreScope {
                channel,
                provider: "claude".into(),
                epoch: 1,
                state: crate::db::o_channel_homes::HomeState::Worker,
                holder: Some("mini".into()),
                target: None,
                local: "mini".into(),
            },
            generation,
            current,
        }
    }
    pub(super) fn row(channel: u64, nonce: Option<&str>) -> inflight::InflightTurnState {
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Claude,
            channel,
            None,
            7,
            71,
            72,
            "request".into(),
            None,
            None,
            None,
            None,
            0,
        );
        state.turn_nonce = nonce.map(str::to_owned);
        state.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
        state.born_generation = 0;
        state
    }
    pub(super) async fn fixture() -> (
        crate::db::auto_queue::test_support::TestPostgresDb,
        sqlx::PgPool,
        Arc<SharedData>,
    ) {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let shared =
            crate::services::discord::host_teardown_gate::test_support::shared_on(&pool).await;
        (db, pool, shared)
    }

    #[tokio::test]
    async fn s3act_b3a1_pending_driver_survives_ticket_drop_and_generation_pg() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let (db, pool, shared) = fixture().await;
        let state = row(5340401, Some("pending"));
        inflight::save_inflight_state_create_new(&state).unwrap();
        queued(&shared, &state).await;
        let current = Arc::new(AtomicU64::new(1));
        let first = context(state.channel_id, 1, current.clone());
        let http = Arc::new(serenity::Http::new("test"));
        let installed = install_inflight_scoped(&http, &shared, &ProviderKind::Claude, &first)
            .await
            .unwrap();
        assert!(installed.activated);
        let actor = shared
            .mailbox(ChannelId::new(state.channel_id))
            .snapshot()
            .await
            .cancel_token
            .unwrap();
        drop(installed);
        current.store(2, Ordering::Release);
        let second = context(state.channel_id, 2, current);
        let installed = install_inflight_scoped(&http, &shared, &ProviderKind::Claude, &second)
            .await
            .unwrap();
        let outcome = drive(&shared, &installed).await;
        assert!(
            !shared.queued.queued_placeholders.contains_key(&(
                ChannelId::new(state.channel_id),
                MessageId::new(state.user_msg_id)
            )),
            "feedback from the dropped ticket must be handed over exactly once"
        );
        assert!(
            shared
                .mailbox(ChannelId::new(state.channel_id))
                .snapshot()
                .await
                .recovery_started_at
                .is_some()
        );
        assert_eq!(
            outcome,
            crate::services::turn_orchestrator::RecoveryKickoffResult::Activated
        );
        assert!(
            installed.activated,
            "a merely installed actor still needs a driver after re-entry"
        );
        let after = shared
            .mailbox(ChannelId::new(state.channel_id))
            .snapshot()
            .await
            .cancel_token
            .unwrap();
        assert!(Arc::ptr_eq(&actor, &after));
        assert!(
            install_inflight_scoped(&http, &shared, &ProviderKind::Claude, &first)
                .await
                .is_err()
        );
        pool.close().await;
        db.drop().await;
    }

    #[tokio::test]
    async fn s3act_b3a1_legacy_resume_keeps_the_installed_arc_pg() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let (db, pool, shared) = fixture().await;
        let state = row(5340402, None);
        inflight::save_inflight_state_create_new(&state).unwrap();
        let current = Arc::new(AtomicU64::new(1));
        let ctx = context(state.channel_id, 1, current);
        let http = Arc::new(serenity::Http::new("test"));
        let installed = install_inflight_scoped(&http, &shared, &ProviderKind::Claude, &ctx)
            .await
            .unwrap();
        let channel = ChannelId::new(state.channel_id);
        let actor = shared
            .mailbox(channel)
            .snapshot()
            .await
            .cancel_token
            .unwrap();
        let candidate = Arc::new(CancelToken::from_persisted_turn_nonce(None));
        let resumed = installed_token(&shared, channel, &state, candidate).await;
        assert_eq!(
            shared
                .mailbox(channel)
                .recovery_kickoff(actor.clone(), UserId::new(7), Some(MessageId::new(71)))
                .await,
            crate::services::turn_orchestrator::RecoveryKickoffResult::OccupiedDifferentEpisode
        );
        assert!(
            shared
                .mailbox(channel)
                .snapshot()
                .await
                .recovery_started_at
                .is_none()
        );
        let outcome = RESUME_ACTOR
            .scope(
                installed.actor.clone().unwrap(),
                checked_kickoff(
                    &shared,
                    &ProviderKind::Claude,
                    channel,
                    resumed.clone(),
                    installed.state.as_ref().unwrap(),
                ),
            )
            .await;

        assert!(
            shared
                .mailbox(channel)
                .snapshot()
                .await
                .recovery_started_at
                .is_some()
        );
        assert_eq!(
            outcome,
            crate::services::turn_orchestrator::RecoveryKickoffResult::Activated
        );
        assert!(
            Arc::ptr_eq(&actor, &resumed),
            "legacy resume must carry the installation's actual actor"
        );
        pool.close().await;
        db.drop().await;
    }

    #[tokio::test]
    async fn s3act_b3a1_receipt_lookup_failure_is_retryable_pg() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let (db, pool, _) = fixture().await;
        let mut state = row(5340403, Some("receipt"));
        state.restart_generation = Some(0);
        inflight::save_inflight_state_create_new(&state).unwrap();
        let path = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &ProviderKind::Claude,
            state.channel_id,
        );
        let state: inflight::InflightTurnState =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        pool.close().await;
        let error = inflight::invalidate_stale_generation_scoped(
            Some(&pool),
            &ProviderKind::Claude,
            7,
            &state,
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().starts_with("retryable:"),
            "receipt error must retain retry semantics: {error}"
        );
        db.drop().await;
    }

    #[tokio::test]
    async fn s3act_b3a1_lock_contention_is_retryable() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let mut state = row(5340404, Some("lock"));
        state.restart_generation = Some(0);
        inflight::save_inflight_state_create_new(&state).unwrap();
        let path = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &ProviderKind::Claude,
            state.channel_id,
        );
        let _held = inflight::try_lock_inflight_state_path(&path).unwrap();
        let error =
            inflight::invalidate_stale_generation_scoped(None, &ProviderKind::Claude, 7, &state)
                .await
                .unwrap_err();
        assert!(
            error.to_string().starts_with("retryable:"),
            "lock error must retain retry semantics: {error}"
        );
    }
    #[tokio::test]
    async fn s3act_b3a1_parent_fsync_failure_is_retryable_pg() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let (db, pool, shared) = fixture().await;
        let state = row(5340405, Some("fsync"));
        inflight::save_inflight_state_create_new(&state).unwrap();
        let path = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &ProviderKind::Claude,
            state.channel_id,
        );
        let mut json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        json.as_object_mut().unwrap().remove("finalizer_turn_id");
        std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        let _fault = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "ADK_S3ACT_B1_MUTANT",
            "fsync-failure".as_ref(),
        );
        let ctx = context(state.channel_id, 1, Arc::new(AtomicU64::new(1)));
        let http = Arc::new(serenity::Http::new("test"));
        let error = match install_inflight_scoped(&http, &shared, &ProviderKind::Claude, &ctx).await
        {
            Err(error) => error,
            Ok(_) => panic!("required parent sync cannot acknowledge installation"),
        };
        assert!(
            error.starts_with("retryable:"),
            "parent fsync failure must remain retryable: {error}"
        );
        assert!(
            shared
                .mailbox(ChannelId::new(state.channel_id))
                .snapshot()
                .await
                .cancel_token
                .is_none()
        );
        pool.close().await;
        db.drop().await;
    }
    async fn queued(shared: &Arc<SharedData>, state: &inflight::InflightTurnState) {
        use crate::services::turn_orchestrator::{Intervention, InterventionMode};
        let channel = ChannelId::new(state.channel_id);
        let id = MessageId::new(state.user_msg_id);
        let item = Intervention {
            author_id: UserId::new(7),
            author_is_bot: false,
            message_id: id,
            queued_generation: 1,
            source_message_ids: vec![id],
            source_message_queued_generations: Vec::new(),
            source_text_segments: Vec::new(),
            text: "queued original".into(),
            mode: InterventionMode::Soft,
            created_at: std::time::Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: false,
            pending_uploads: Vec::new(),
            voice_announcement: None,
        };
        shared
            .mailbox(channel)
            .merge_restored_queue_items(
                vec![item],
                queue_persistence_context(shared, &ProviderKind::Claude, channel),
            )
            .await;
        shared
            .queued
            .queued_placeholders
            .insert((channel, id), MessageId::new(state.channel_id + 3));
    }
    async fn drive(
        shared: &Arc<SharedData>,
        installed: &InstalledInflight,
    ) -> crate::services::turn_orchestrator::RecoveryKickoffResult {
        let state = installed.state.as_ref().unwrap();
        let proof = installed.actor.clone().unwrap();
        let token = proof.token.clone();
        RESUME_ACTOR
            .scope(
                proof,
                checked_kickoff(
                    shared,
                    &ProviderKind::Claude,
                    ChannelId::new(state.channel_id),
                    token,
                    state,
                ),
            )
            .await
    }
    #[tokio::test]
    async fn s3act_b3a1_handoff_checks_actual_actor_before_feedback_pg() {
        use crate::services::turn_orchestrator::installed_actor_handoff::HandoffOutcome;
        let _root = crate::config::TestRuntimeRootGuard::new();
        let (db, pool, shared) = fixture().await;
        let state = row(5340410, Some("same-nonce"));
        inflight::save_inflight_state_create_new(&state).unwrap();
        queued(&shared, &state).await;
        let ctx = context(state.channel_id, 1, Arc::new(AtomicU64::new(1)));
        let discord =
            crate::services::discord::recovery_engine::o_cut_recorder::start(state.channel_id)
                .await;
        let installed =
            install_inflight_scoped(&discord.http, &shared, &ProviderKind::Claude, &ctx)
                .await
                .unwrap();
        let fresh = installed.state.as_ref().unwrap();
        let proof = installed.actor.clone().unwrap();
        let successor = Arc::new(CancelToken::from_persisted_turn_nonce(
            state.turn_nonce.clone(),
        ));
        let channel = ChannelId::new(state.channel_id);
        shared
            .mailbox(channel)
            .restore_active_turn_kinded(
                successor.clone(),
                UserId::new(7),
                MessageId::new(fresh.effective_finalizer_turn_id()),
                crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent,
            )
            .await;
        let outcome =
            checked_handoff(&shared, &ProviderKind::Claude, fresh, proof.clone(), false).await;
        if let HandoffOutcome::Ready(events) = &outcome {
            apply_queue_exit_feedback(&shared, channel, events).await;
        }
        let snap = shared.mailbox(channel).snapshot().await;
        assert!(Arc::ptr_eq(snap.cancel_token.as_ref().unwrap(), &successor));
        assert!(snap.recovery_started_at.is_none());
        assert!(
            shared
                .queued
                .queued_placeholders
                .contains_key(&(channel, MessageId::new(state.user_msg_id)))
        );
        assert!(discord.calls().is_empty());
        assert!(matches!(outcome, HandoffOutcome::Refused(_)));
        shared
            .mailbox(channel)
            .restore_active_turn_kinded(
                proof.token.clone(),
                UserId::new(7),
                MessageId::new(fresh.effective_finalizer_turn_id()),
                crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent,
            )
            .await;
        let HandoffOutcome::Ready(events) =
            checked_handoff(&shared, &ProviderKind::Claude, fresh, proof, false).await
        else {
            panic!("original actor must retain pending feedback")
        };
        assert_eq!(events.len(), 1);
        apply_queue_exit_feedback(&shared, channel, &events).await;
        assert!(
            !shared
                .queued
                .queued_placeholders
                .contains_key(&(channel, MessageId::new(state.user_msg_id)))
        );
        assert!(
            shared
                .mailbox(channel)
                .snapshot()
                .await
                .recovery_started_at
                .is_none()
        );
        assert_eq!(
            drive(&shared, &installed).await,
            crate::services::turn_orchestrator::RecoveryKickoffResult::Activated
        );
        assert!(
            shared
                .mailbox(channel)
                .snapshot()
                .await
                .recovery_started_at
                .is_some()
        );
        pool.close().await;
        db.drop().await;
    }
    #[tokio::test]
    async fn s3act_b3a1_handoff_rechecks_generation_before_feedback_pg() {
        stale_handoff("generation", 5340411).await;
    }
    #[tokio::test]
    async fn s3act_b3a1_handoff_rechecks_exact_row_before_feedback_pg() {
        stale_handoff("row", 5340412).await;
    }
    async fn stale_handoff(change: &str, channel: u64) {
        use crate::services::turn_orchestrator::installed_actor_handoff::HandoffOutcome;
        let _root = crate::config::TestRuntimeRootGuard::new();
        let (db, pool, shared) = fixture().await;
        let state = row(channel, Some("pending"));
        inflight::save_inflight_state_create_new(&state).unwrap();
        queued(&shared, &state).await;
        let current = Arc::new(AtomicU64::new(1));
        let ctx = context(channel, 1, current.clone());
        let http = Arc::new(serenity::Http::new("test"));
        let installed = install_inflight_scoped(&http, &shared, &ProviderKind::Claude, &ctx)
            .await
            .unwrap();
        let path = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &ProviderKind::Claude,
            channel,
        );
        let bytes = std::fs::read(&path).unwrap();
        if change == "generation" {
            current.store(2, Ordering::Release);
        } else {
            let mut successor = installed.state.clone().unwrap();
            successor.started_at.push_str("-successor");
            std::fs::write(&path, serde_json::to_vec(&successor).unwrap()).unwrap();
        }
        let outcome = checked_handoff(
            &shared,
            &ProviderKind::Claude,
            installed.state.as_ref().unwrap(),
            installed.actor.clone().unwrap(),
            false,
        )
        .await;
        let channel = ChannelId::new(channel);
        if let HandoffOutcome::Ready(events) = &outcome {
            apply_queue_exit_feedback(&shared, channel, events).await;
        }
        assert!(
            shared
                .mailbox(channel)
                .snapshot()
                .await
                .recovery_started_at
                .is_none()
        );
        assert!(
            shared
                .queued
                .queued_placeholders
                .contains_key(&(channel, MessageId::new(state.user_msg_id)))
        );
        assert!(
            matches!(outcome, HandoffOutcome::Refused(_)),
            "stale {change} must not consume feedback"
        );
        std::fs::write(&path, bytes).unwrap();
        current.store(1, Ordering::Release);
        let HandoffOutcome::Ready(events) = checked_handoff(
            &shared,
            &ProviderKind::Claude,
            installed.state.as_ref().unwrap(),
            installed.actor.clone().unwrap(),
            false,
        )
        .await
        else {
            panic!("refusal must preserve responsibility")
        };
        assert_eq!(events.len(), 1);
        assert_eq!(
            drive(&shared, &installed).await,
            crate::services::turn_orchestrator::RecoveryKickoffResult::Activated
        );
        pool.close().await;
        db.drop().await;
    }
    #[tokio::test]
    async fn s3act_b3a1_fresh_progress_can_transfer_only_pending_responsibility_pg() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let (db, pool, shared) = fixture().await;
        let state = row(5340413, None);
        inflight::save_inflight_state_create_new(&state).unwrap();
        let current = Arc::new(AtomicU64::new(1));
        let first = context(state.channel_id, 1, current.clone());
        let http = Arc::new(serenity::Http::new("test"));
        let installed = install_inflight_scoped(&http, &shared, &ProviderKind::Claude, &first)
            .await
            .unwrap();
        let mut progress = installed.state.clone().unwrap();
        progress.current_msg_id += 1;
        let path = inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &ProviderKind::Claude,
            state.channel_id,
        );
        std::fs::write(path, serde_json::to_vec(&progress).unwrap()).unwrap();
        current.store(2, Ordering::Release);
        let second = context(state.channel_id, 2, current.clone());
        let inherited = install_inflight_scoped(&http, &shared, &ProviderKind::Claude, &second)
            .await
            .unwrap();
        assert!(inherited.activated);
        assert_eq!(
            drive(&shared, &installed).await,
            crate::services::turn_orchestrator::RecoveryKickoffResult::Unavailable
        );
        assert!(
            shared
                .mailbox(ChannelId::new(state.channel_id))
                .snapshot()
                .await
                .recovery_started_at
                .is_none()
        );
        assert_eq!(
            drive(&shared, &inherited).await,
            crate::services::turn_orchestrator::RecoveryKickoffResult::Activated
        );
        let marker = shared
            .mailbox(ChannelId::new(state.channel_id))
            .snapshot()
            .await
            .recovery_started_at
            .unwrap();
        current.store(3, Ordering::Release);
        let observed = install_inflight_scoped(
            &http,
            &shared,
            &ProviderKind::Claude,
            &context(state.channel_id, 3, current),
        )
        .await
        .unwrap();
        assert!(
            !observed.activated,
            "a handed-off actor must retain its existing driver"
        );
        assert_eq!(
            shared
                .mailbox(ChannelId::new(state.channel_id))
                .snapshot()
                .await
                .recovery_started_at,
            Some(marker)
        );
        pool.close().await;
        db.drop().await;
    }
}

#[cfg(all(test, unix))]
#[cfg(test)]
mod b3a2_tests {
    use super::b3a1_tests::{context, fixture, row};
    use super::*;
    use crate::db::o_channel_homes::HomeState;
    use crate::services::cluster::channel_home;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(super) struct Barrier {
        stage: &'static str,
        pub(super) entered: tokio::sync::Notify,
        pub(super) release: tokio::sync::Notify,
    }
    tokio::task_local! { pub(super) static BARRIER: Arc<Barrier>; }
    pub(super) async fn pause(stage: &str) {
        if let Ok(barrier) = BARRIER.try_with(Clone::clone)
            && stage == barrier.stage
        {
            barrier.entered.notify_one();
            barrier.release.notified().await;
        }
    }
    pub(super) fn barrier(stage: &'static str) -> Arc<Barrier> {
        Arc::new(Barrier {
            stage,
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        })
    }
    fn path(state: &inflight::InflightTurnState) -> std::path::PathBuf {
        inflight::inflight_state_path(
            &inflight::inflight_runtime_root().unwrap(),
            &ProviderKind::Claude,
            state.channel_id,
        )
    }
    async fn seed_home(pool: &sqlx::PgPool, channel: u64) {
        sqlx::query("INSERT INTO o_channel_homes(channel_id,provider,state,holder,epoch,renewed_at) VALUES($1,'claude','worker','mini',1,NOW())")
            .bind(channel.to_string()).execute(pool).await.unwrap();
    }
    fn witness(context: &RestoreContext, ticket: &InstalledInflight) -> RestoreWitness {
        RestoreWitness {
            scope: Some(context.scope.clone()),
            generation: context.generation,
            status: RestoreStatus::Restored,
            inflight: Some(ticket.observation.clone()),
        }
    }
    fn admitted_row(channel: u64) -> inflight::InflightTurnState {
        let mut state = row(channel, Some("admitted"));
        let name = ProviderKind::Claude.build_tmux_session_name(&format!("b3a2-{channel}"));
        let marker = crate::services::tmux_common::session_temp_path(&name, "host_kind");
        std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(marker, "herdr").unwrap();
        state.tmux_session_name = Some(name);
        state.tui_terminal_kind = Some(NativeTerminalKind::Aborted);
        state.full_response = "captured result".into();
        state
    }
    fn prior_generation() {
        let generation = crate::services::discord::runtime_store::generation_path().unwrap();
        std::fs::create_dir_all(generation.parent().unwrap()).unwrap();
        std::fs::write(generation, "7").unwrap();
    }
    async fn install_contention(stage: &'static str, channel: u64, mutate: bool) {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let (db, pool, shared) = fixture().await;
        let state = row(channel, None);
        inflight::save_inflight_state_create_new(&state).unwrap();
        let context = context(channel, 1, Arc::new(AtomicU64::new(1)));
        let http = Arc::new(serenity::Http::new("test"));
        let barrier = barrier(stage);
        let restore = BARRIER.scope(
            barrier.clone(),
            install_inflight_scoped(&http, &shared, &ProviderKind::Claude, &context),
        );
        let probe = async {
            barrier.entered.notified().await;
            let first = inflight::try_lock_inflight_state_path(&path(&state));
            let released = first.is_ok();
            let contended =
                released && inflight::try_lock_inflight_state_path(&path(&state)).is_err();
            if mutate && released {
                let mut changed = state.clone();
                changed.full_response = "new canonical progress".into();
                std::fs::write(path(&state), serde_json::to_vec(&changed).unwrap()).unwrap();
            }
            drop(first);
            let mailbox = shared
                .mailbox(ChannelId::new(channel))
                .try_snapshot()
                .await
                .is_ok();
            barrier.release.notify_one();
            (released, contended, mailbox)
        };
        let (result, (released, contended, mailbox)) = tokio::join!(restore, probe);
        assert!(
            released,
            "scoped await must release the actual episode flock: {stage}"
        );
        assert!(
            contended,
            "the test must create actual competing flock ownership"
        );
        assert!(mailbox, "mailbox must progress while restore awaits");
        if mutate {
            assert!(
                result.is_err(),
                "changed mutable snapshot must refuse installation"
            );
            assert!(
                shared
                    .mailbox(ChannelId::new(channel))
                    .snapshot()
                    .await
                    .cancel_token
                    .is_none()
            );
            assert_eq!(
                inflight::load_inflight_probe_scoped(&ProviderKind::Claude, channel).states[0]
                    .full_response,
                "new canonical progress"
            );
        } else {
            assert!(
                result.is_ok(),
                "uncontended restoration must progress after release"
            );
        }
        pool.close().await;
        db.drop().await;
    }
    #[tokio::test]
    async fn s3act_b3a2_hydrate_releases_lock_pg() {
        install_contention("install-hydrate", 5340501, false).await;
    }
    #[tokio::test]
    async fn s3act_b3a2_admission_releases_lock_pg() {
        install_contention("install-admission", 5340502, false).await;
    }
    #[tokio::test]
    async fn s3act_b3a2_mailbox_snapshot_releases_lock_pg() {
        install_contention("install-mailbox", 5340503, false).await;
    }
    #[tokio::test]
    async fn s3act_b3a2_changed_snapshot_refuses_install_pg() {
        install_contention("install-hydrate", 5340504, true).await;
    }

    async fn resume_contention(channel: u64, mutate: bool) {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let (db, pool, shared) = fixture().await;
        prior_generation();
        seed_home(&pool, channel).await;
        let gate = channel_home::register_for_test(channel, Some(HomeState::Worker));
        let state = admitted_row(channel);
        inflight::save_inflight_state_create_new(&state).unwrap();
        let context = context(channel, 1, Arc::new(AtomicU64::new(1)));
        let discord =
            crate::services::discord::recovery_engine::o_cut_recorder::start(channel).await;
        let installed =
            install_inflight_scoped(&discord.http, &shared, &ProviderKind::Claude, &context)
                .await
                .unwrap();
        let witness = witness(&context, &installed);
        let barrier = barrier("resume-hydrate");
        let resume = BARRIER.scope(
            barrier.clone(),
            resume_inflight_scoped(
                &discord.http,
                &shared,
                &ProviderKind::Claude,
                &context,
                &witness,
                &installed,
            ),
        );
        let compete = async {
            barrier.entered.notified().await;
            let guard = inflight::try_lock_inflight_state_path(&path(&state));
            let released = guard.is_ok();
            let contended =
                released && inflight::try_lock_inflight_state_path(&path(&state)).is_err();
            if mutate && released {
                let mut changed = state.clone();
                changed.full_response = "new resume progress".into();
                std::fs::write(path(&state), serde_json::to_vec(&changed).unwrap()).unwrap();
            }
            drop(guard);
            barrier.release.notify_one();
            (released, contended)
        };
        let (result, (released, contended)) = tokio::join!(resume, compete);
        assert!(
            released && contended,
            "resume receipt await must release actual episode flock and permit competing ownership"
        );
        if mutate {
            assert!(
                result.is_err(),
                "resume must reject a changed mutable canonical snapshot"
            );
            assert!(discord.calls().is_empty());
            assert!(!installed.effects_started.load(Ordering::Acquire));
            assert_eq!(
                inflight::load_inflight_probe_scoped(&ProviderKind::Claude, channel).states[0]
                    .full_response,
                "new resume progress"
            );
        } else {
            assert!(matches!(
                result,
                Ok(InflightObservation::Settled {
                    kind: NativeTerminalKind::Aborted
                })
            ));
        }
        channel_home::unregister_if_same(&gate);
        pool.close().await;
        db.drop().await;
    }
    #[tokio::test]
    async fn s3act_b3a2_resume_hydrate_releases_lock_pg() {
        resume_contention(5340505, false).await;
    }
    #[tokio::test]
    async fn s3act_b3a2_resume_snapshot_change_is_preserved_pg() {
        resume_contention(5340506, true).await;
    }

    #[tokio::test]
    async fn s3act_b3a2_receipt_then_full_home_revalidation_pg() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let (db, pool, shared) = fixture().await;
        prior_generation();
        for (i, column, value) in [
            (0, "target", "remote"),
            (1, "holder", "remote"),
            (2, "provider", "codex"),
            (3, "state", "releasing"),
        ] {
            let channel = 5340510 + i;
            seed_home(&pool, channel).await;
            sqlx::query("UPDATE o_channel_homes SET state='reclaiming',target='gateway' WHERE channel_id=$1")
                .bind(channel.to_string()).execute(&pool).await.unwrap();
            let gate = channel_home::register_for_test(channel, Some(HomeState::Reclaiming));
            let state = admitted_row(channel);
            inflight::save_inflight_state_create_new(&state).unwrap();
            let before = std::fs::read(path(&state)).unwrap();
            let mut context = context(channel, 1, Arc::new(AtomicU64::new(1)));
            context.scope.state = HomeState::Reclaiming;
            context.scope.target = Some("gateway".into());
            let discord =
                crate::services::discord::recovery_engine::o_cut_recorder::start(channel).await;
            let installed =
                install_inflight_scoped(&discord.http, &shared, &ProviderKind::Claude, &context)
                    .await
                    .unwrap();
            let witness = witness(&context, &installed);
            let barrier = barrier("resume-receipt");
            let resume = BARRIER.scope(
                barrier.clone(),
                resume_inflight_scoped(
                    &discord.http,
                    &shared,
                    &ProviderKind::Claude,
                    &context,
                    &witness,
                    &installed,
                ),
            );
            let change = async {
                barrier.entered.notified().await;
                sqlx::query(&format!(
                    "UPDATE o_channel_homes SET {column}=$1 WHERE channel_id=$2"
                ))
                .bind(value)
                .bind(channel.to_string())
                .execute(&pool)
                .await
                .unwrap();
                barrier.release.notify_one();
            };
            let (result, ()) = tokio::join!(resume, change);
            assert!(
                discord.calls().is_empty(),
                "full home identity must be rechecked after receipt: {column}; forbidden REST effects: {:?}",
                discord.calls()
            );
            assert!(
                !installed.effects_started.load(Ordering::Acquire),
                "changed home must not start an effect attempt"
            );
            assert_eq!(
                std::fs::read(path(&state)).ok(),
                Some(before),
                "changed home must preserve the canonical row"
            );
            assert!(
                result.is_err(),
                "full home identity must be rechecked after receipt: {column}; {result:?}"
            );
            assert_eq!(gate.commands_in_flight(), 0);
            channel_home::unregister_if_same(&gate);
        }
        pool.close().await;
        db.drop().await;
    }
    #[tokio::test]
    async fn s3act_b3a2_epoch_change_between_receipt_and_permit_pg() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let (db, pool, shared) = fixture().await;
        prior_generation();
        let channel = 5340515;
        seed_home(&pool, channel).await;
        let gate = channel_home::register_for_test(channel, Some(HomeState::Worker));
        let state = admitted_row(channel);
        inflight::save_inflight_state_create_new(&state).unwrap();
        let before = std::fs::read(path(&state)).unwrap();
        let context = context(channel, 1, Arc::new(AtomicU64::new(1)));
        let discord =
            crate::services::discord::recovery_engine::o_cut_recorder::start(channel).await;
        let installed =
            install_inflight_scoped(&discord.http, &shared, &ProviderKind::Claude, &context)
                .await
                .unwrap();
        let witness = witness(&context, &installed);
        let barrier = barrier("resume-receipt");
        let resume = BARRIER.scope(
            barrier.clone(),
            resume_inflight_scoped(
                &discord.http,
                &shared,
                &ProviderKind::Claude,
                &context,
                &witness,
                &installed,
            ),
        );
        let change = async {
            barrier.entered.notified().await;
            gate.confirm(
                &crate::db::o_channel_homes::HeldHome::for_test(
                    &channel.to_string(),
                    "mini",
                    2,
                    HomeState::Worker,
                ),
                tokio::time::Instant::now(),
            )
            .unwrap();
            barrier.release.notify_one();
        };
        let (result, ()) = tokio::join!(resume, change);
        assert!(
            discord.calls().is_empty(),
            "permit must belong to the epoch named by installation proof; forbidden REST effects: {:?}",
            discord.calls()
        );
        assert!(
            !installed.effects_started.load(Ordering::Acquire),
            "wrong epoch must not start an effect attempt"
        );
        assert_eq!(std::fs::read(path(&state)).ok(), Some(before));
        assert!(
            result.is_err(),
            "permit must belong to the epoch named by installation proof: {result:?}"
        );
        assert_eq!(gate.commands_in_flight(), 0);
        channel_home::unregister_if_same(&gate);
        pool.close().await;
        db.drop().await;
    }
    #[tokio::test]
    async fn s3act_b3a2_proven_output_settles_with_intake_closed_pg() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let (db, pool, shared) = fixture().await;
        prior_generation();
        let channel = 5340521;
        seed_home(&pool, channel).await;
        let gate = channel_home::register_for_test(channel, Some(HomeState::Worker));
        let state = admitted_row(channel);
        inflight::save_inflight_state_create_new(&state).unwrap();
        let context = context(channel, 1, Arc::new(AtomicU64::new(1)));
        let discord =
            crate::services::discord::recovery_engine::o_cut_recorder::start(channel).await;
        let installed =
            install_inflight_scoped(&discord.http, &shared, &ProviderKind::Claude, &context)
                .await
                .unwrap();
        gate.close_intake();
        let result = resume_inflight_scoped(
            &discord.http,
            &shared,
            &ProviderKind::Claude,
            &context,
            &witness(&context, &installed),
            &installed,
        )
        .await;
        assert!(
            matches!(
                result,
                Ok(InflightObservation::Settled {
                    kind: NativeTerminalKind::Aborted
                })
            ),
            "existing output must use recovery admission while new commands are closed: {result:?}"
        );
        assert!(!path(&state).exists());
        assert!(!discord.calls().is_empty());
        assert!(
            shared
                .mailbox(ChannelId::new(channel))
                .snapshot()
                .await
                .cancel_token
                .is_none()
        );
        assert_eq!(gate.commands_in_flight(), 0);
        channel_home::unregister_if_same(&gate);
        pool.close().await;
        db.drop().await;
    }
    #[tokio::test]
    async fn s3act_b3a2_uncertain_output_is_preserved_without_admission_pg() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let (db, pool, shared) = fixture().await;
        let channel = 5340520;
        seed_home(&pool, channel).await;
        let gate = channel_home::register_for_test(channel, Some(HomeState::Worker));
        let mut state = row(channel, None);
        state.full_response = "partial output without completion proof".into();
        inflight::save_inflight_state_create_new(&state).unwrap();
        let before = std::fs::read(path(&state)).unwrap();
        let context = context(channel, 1, Arc::new(AtomicU64::new(1)));
        let http = Arc::new(serenity::Http::new("test"));
        let installed = install_inflight_scoped(&http, &shared, &ProviderKind::Claude, &context)
            .await
            .unwrap();
        gate.close_intake();
        let result = resume_inflight_scoped(
            &http,
            &shared,
            &ProviderKind::Claude,
            &context,
            &witness(&context, &installed),
            &installed,
        )
        .await;
        assert!(
            matches!(result, Ok(InflightObservation::Deferred(_))),
            "uncertain output must be preserved and deferred before effect admission: {result:?}"
        );
        assert!(!installed.effects_started.load(Ordering::Acquire));
        assert_eq!(gate.commands_in_flight(), 0);
        assert_eq!(std::fs::read(path(&state)).unwrap(), before);
        channel_home::unregister_if_same(&gate);
        pool.close().await;
        db.drop().await;
    }
}

#[cfg(all(test, unix))]
#[path = "scoped_policy_receipt_tests.rs"]
mod policy_receipt_tests;
