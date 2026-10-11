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
    let mut actor = None;
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
                        token.clone(),
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
                let proof = actor_proof(context, &state, token)?;
                if scoped_mutant("install-feedback") {
                    apply_queue_exit_feedback(shared, channel, &claim.queue_exit_events).await;
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
            let after = shared
                .mailbox(channel)
                .try_snapshot()
                .await
                .map_err(|e| format!("mailbox unavailable: {e:?}"))?;
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

    fn context(channel: u64, generation: u64, current: Arc<AtomicU64>) -> RestoreContext {
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
    fn row(channel: u64, nonce: Option<&str>) -> inflight::InflightTurnState {
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
    async fn fixture() -> (
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
