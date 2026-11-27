//! Reservation, durable attempt metadata, and provider launch for agent-backed routines.
use super::*;
use crate::services::routines::provider_choice::AgentProviderChannels;

impl RoutineAgentExecutor {
    pub(super) async fn start_turn(
        &self,
        store: &RoutineStore,
        start: start_deferral::PendingAgentStart<'_>,
    ) -> Result<StartedAgentTurn> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(result) = super::owner_hook_tests::scripted_start(self, store, &start).await {
            return result;
        }
        let start_deferral::PendingAgentStart {
            claimed,
            agent_id,
            attempt_kind,
            prompt,
            dm_user_id,
            checkpoint,
            next_due_at,
            execution_mode,
        } = start;
        let (Some(registry), Some(bot_http)) =
            (self.health_registry.as_deref(), self.bot_http.as_deref())
        else {
            return Err(anyhow!(
                "routine agent action requires discord runtime health registry"
            ));
        };

        // The claim pins the explicit choice; an override never falls back to another provider.
        let choice = store.run_provider_choice(&claimed.run_id).await?;
        let route = AgentProviderChannels::load(&self.pool, agent_id)
            .await?
            .route(agent_id, &choice)?;
        let provider = route.provider.clone();
        let headless = surface::headless(store, &claimed.run_id).await?;
        let execution_mode = match headless {
            true => surface::headless_backend(&provider, dm_user_id)?,
            false => execution_mode.for_provider(&provider),
        };
        execution_mode.ensure_provider(&provider)?;
        start_deferral::ensure_provider_registered(registry, &provider).await?;
        let primary_channel = route.channel.clone();
        let Some(channel_id_num) =
            crate::services::dispatches::outbox_route::resolve_channel_alias_pub(&primary_channel)
                .or_else(|| primary_channel.parse::<u64>().ok())
        else {
            return Err(anyhow!(
                "agent {agent_id} primary channel is invalid: {primary_channel}"
            ));
        };
        let channel_id = poise::serenity_prelude::ChannelId::new(channel_id_num);
        let dm_user_id_num = match dm_user_id {
            Some(value) => Some(value.parse::<u64>().map_err(|error| {
                anyhow!("RoutineAction.agent.dmUserId must be a Discord snowflake string: {error}")
            })?),
            None => None,
        };

        let (turn_channel_id, discord_thread_id, delivery_bot, reservation) = if headless {
            // No routine thread is resolved or created; the turn posts nothing of its own.
            let bot = UtilityBotRole::Notify.alias().to_string();
            (
                channel_id,
                None,
                bot,
                registry.reserve_headless_agent_turn(channel_id),
            )
        } else if let Some(dm_user_id_num) = dm_user_id_num {
            let (dm_channel_id, reservation) = registry
                .reserve_headless_agent_turn_in_dm(channel_id, dm_user_id_num, &provider)
                .await
                .map_err(|error| {
                    start_deferral::dm_reservation_error(agent_id, dm_user_id_num, error)
                })?;
            (dm_channel_id, None, "dm".to_string(), reservation)
        } else {
            let routine_channel = self
                .resolve_or_create_routine_thread(
                    store,
                    bot_http,
                    claimed,
                    agent_id,
                    provider.as_str(),
                    channel_id,
                )
                .await;
            let (turn_channel_id, discord_thread_id, delivery_bot) = match routine_channel {
                Ok(target) => (
                    target.channel_id,
                    target.discord_thread_id,
                    target.delivery_bot,
                ),
                Err(error) => {
                    let warning = error.to_string();
                    let _ = store
                        .record_discord_log_result(&claimed.run_id, "failed", Some(&warning))
                        .await;
                    // The claim decides: a run pinned so never runs on the primary instead.
                    if store.run_forbids_primary_fallback(&claimed.run_id).await? {
                        return Err(error);
                    }
                    tracing::warn!(
                        routine_id = claimed.routine_id,
                        run_id = claimed.run_id,
                        error = %warning,
                        "routine thread setup failed; falling back to agent primary channel"
                    );
                    (channel_id, None, UtilityBotRole::Notify.alias().to_string())
                }
            };
            (
                turn_channel_id,
                discord_thread_id,
                delivery_bot,
                registry.reserve_headless_agent_turn(turn_channel_id),
            )
        };
        let turn_id = reservation.turn_id().to_string();

        let mut result_json = json!({
            "status": "started",
            "execution_mode": execution_mode.as_str(),
            "turn_id": turn_id.clone(),
            "agent_id": agent_id,
            "provider": provider.as_str(),
            "channel_id": turn_channel_id.get().to_string(),
            "parent_channel_id": channel_id_num.to_string(),
            "discord_thread_id": discord_thread_id,
            "dm_user_id": dm_user_id,
            "is_dm": dm_user_id.is_some(),
            "routine_id": claimed.routine_id,
            "run_id": claimed.run_id,
            "script_ref": claimed.script_ref,
            "attempt_kind": attempt_kind,
            "prompt": prompt,
            "completion_evidence": "session_transcripts",
            "fresh_context_guaranteed":
                fresh_context_guaranteed(&claimed.execution_strategy, false),
            "checkpoint": checkpoint,
            "next_due_at": next_due_at.map(|value| value.to_rfc3339()),
        });
        let mut metadata = json!({
            "agent_id": agent_id,
            "delivery_bot": delivery_bot,
            "routine_id": claimed.routine_id,
            "routine_run_id": claimed.run_id,
            "script_ref": claimed.script_ref,
            "execution_strategy": claimed.execution_strategy,
            "execution_mode": execution_mode.as_str(),
            "fresh_context_guaranteed":
                fresh_context_guaranteed(&claimed.execution_strategy, false),
            "turn_id": turn_id.clone(),
            "parent_channel_id": channel_id_num.to_string(),
            "discord_thread_id": discord_thread_id,
            "dm_user_id": dm_user_id,
            "is_dm": dm_user_id.is_some(),
        });
        if headless {
            surface::mark_headless(&mut metadata);
        }
        if let Some(model) = route.model.as_deref() {
            metadata["model"] = json!(model);
            result_json["model"] = json!(model);
        }
        let metadata = Some(metadata);
        let process = execution_mode == execution::AgentExecutionMode::Process;
        let channel_name_hint = dm_user_id_num.is_none().then(|| primary_channel.clone());
        let tmux_session_label = (dm_user_id_num.is_none() || process)
            .then(|| execution_mode.session_label(claimed, agent_id, &turn_id));
        // The owner keeps what the legacy start resolves here, so its turn resolves the same way.
        let mut planned = result_json.clone();
        planned["metadata"] = json!(metadata);
        // A DM start is given the user's `dm-<user>` channel as its hint, as the DM start below is.
        let dm_hint = dm_user_id_num.map(|user| format!("dm-{user}"));
        planned["channel_name_hint"] = json!(dm_hint.or(channel_name_hint.clone()));
        planned["synthetic_label"] = json!(tmux_session_label);
        planned["source"] = json!("routine");
        if let Some(started) = owner_input::custody::owned(self, store, &start, &planned).await? {
            return Ok(started);
        }
        let updated = store
            .mark_agent_turn_started(
                &claimed.run_id,
                &turn_id,
                Some(result_json.clone()),
                agent_id,
                attempt_kind,
            )
            .await?;
        if !updated {
            return Err(anyhow!(
                "routine agent run {} vanished before turn_id could be stored",
                claimed.run_id
            ));
        }

        let outcome = if let Some(dm_user_id_num) = dm_user_id_num {
            let start = HeadlessDmTurnStart {
                owner_channel_id: channel_id,
                dm_channel_id: turn_channel_id,
                dm_user_id: dm_user_id_num,
                owner_provider: provider.clone(),
                prompt: prompt.to_string(),
                source: Some("routine".to_string()),
                metadata,
                tmux_session_label,
            };
            registry
                .start_reserved_headless_agent_turn_in_dm(start, reservation)
                .await
        } else {
            // The selected provider channel resolves the workspace; the synthetic label
            // keeps this routine's session separate.
            let start = HeadlessTurnStart {
                owner_channel_id: channel_id,
                turn_channel_id,
                owner_provider: provider.clone(),
                prompt: prompt.to_string(),
                source: Some("routine".to_string()),
                metadata,
                channel_name_hint,
                tmux_session_label,
            };
            registry
                .start_reserved_headless_agent_turn_with_owner_channel(start, reservation)
                .await
        }
        .map_err(|error| start_deferral::headless_start_error(agent_id, error))?;

        if outcome.turn_id != turn_id {
            return Err(run_policy::SubmittedStartError(format!(
                "reserved routine agent turn id mismatch: expected {} but started {}",
                turn_id, outcome.turn_id
            ))
            .into());
        }
        let disposition = owner_input::disposition(outcome.status);
        let started = disposition == AgentStartDisposition::Started;
        let provider_fresh_context = fresh_context_guaranteed(&claimed.execution_strategy, started);
        if !started && let Some(object) = result_json.as_object_mut() {
            object.insert(
                "status".to_string(),
                Value::String(outcome.status.as_str().to_string()),
            );
            object.insert(
                "completion_evidence".to_string(),
                Value::String("headless_start_outcome".to_string()),
            );
        }
        let durable_confirmation_persisted = if provider_fresh_context {
            let confirmed_result = result_with_fresh_context_guarantee(result_json.clone(), true);
            match store
                .confirm_agent_turn_started(&claimed.run_id, &turn_id)
                .await
            {
                Ok(true) => {
                    result_json = confirmed_result;
                    true
                }
                Ok(false) => {
                    tracing::warn!(
                        routine_id = %claimed.routine_id,
                        run_id = %claimed.run_id,
                        turn_id = %turn_id,
                        "fresh provider start confirmation found no matching routine agent turn"
                    );
                    false
                }
                Err(error) => {
                    tracing::warn!(
                        routine_id = %claimed.routine_id,
                        run_id = %claimed.run_id,
                        turn_id = %turn_id,
                        error = %error,
                        "failed to persist fresh provider start confirmation"
                    );
                    false
                }
            }
        } else {
            false
        };
        let fresh_context_guaranteed = confirmed_fresh_context_guarantee(
            provider_fresh_context,
            durable_confirmation_persisted,
        );
        result_json = result_with_fresh_context_guarantee(result_json, fresh_context_guaranteed);

        // #3022: persist run -> fresh-session ownership now that the session is
        // up, so boot recovery can reap this exact session if a dcserver restart
        // orphans it. Best-effort: a failure here only loses the boot-recovery
        // backstop (the in-line completion path still tears the session down),
        // so it must never fail the started turn.
        if started && execution_mode != execution::AgentExecutionMode::Process {
            self.record_owned_fresh_session(
                store,
                claimed,
                &result_json,
                outcome.owned_session_key.as_deref(),
            )
            .await;
        }

        Ok(StartedAgentTurn {
            result_json,
            disposition,
            fresh_context_guaranteed,
        })
    }
}
