//! Reservation, durable attempt metadata, and provider launch for agent-backed routines.
use super::*;

impl RoutineAgentExecutor {
    pub(super) async fn start_turn(
        &self,
        store: &RoutineStore,
        start: start_deferral::PendingAgentStart<'_>,
    ) -> Result<StartedAgentTurn> {
        #[cfg(test)]
        if let Some(result) = super::owner_input_tests::inject(self, store, &start).await {
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
        let Some(registry) = self.health_registry.as_deref() else {
            return Err(anyhow!(
                "routine agent action requires discord runtime health registry"
            ));
        };

        let bindings = crate::db::agents::load_agent_channel_bindings_pg(&self.pool, agent_id)
            .await
            .map_err(|error| anyhow!("load agent bindings for {agent_id}: {error}"))?
            .ok_or_else(|| anyhow!("agent {agent_id} not found"))?;
        let provider = bindings
            .resolved_primary_provider_kind()
            .ok_or_else(|| anyhow!("agent {agent_id} primary provider is not configured"))?;
        execution_mode.ensure_provider(&provider)?;
        start_deferral::ensure_provider_registered(registry, &provider).await?;
        let primary_channel = bindings
            .primary_channel()
            .ok_or_else(|| anyhow!("agent {agent_id} primary channel is not configured"))?;
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

        let (turn_channel_id, discord_thread_id, delivery_bot, reservation) =
            if let Some(dm_user_id_num) = dm_user_id_num {
                let (dm_channel_id, reservation) = reserve_headless_agent_turn_in_dm(
                    registry,
                    channel_id,
                    dm_user_id_num,
                    &provider,
                )
                .await
                .map_err(|error| {
                    start_deferral::dm_reservation_error(agent_id, dm_user_id_num, error)
                })?;
                (dm_channel_id, None, "dm".to_string(), reservation)
            } else {
                let routine_channel = self
                    .resolve_or_create_routine_thread(
                        store,
                        registry,
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
                    reserve_headless_agent_turn(turn_channel_id),
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
        let metadata = Some(json!({
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
        }));
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
            start_reserved_headless_agent_turn_in_dm(
                registry,
                channel_id,
                turn_channel_id,
                dm_user_id_num,
                provider.clone(),
                prompt.to_string(),
                Some("routine".to_string()),
                metadata,
                tmux_session_label,
                reservation,
            )
            .await
        } else {
            // #5: the agent's REAL primary channel/alias is the workspace hint so
            // `resolve_workspace` resolves alias/by-name routine agents; the
            // synthetic session name rides the tmux label (#3463 name-first).
            start_reserved_headless_agent_turn_with_owner_channel(
                registry,
                channel_id,
                turn_channel_id,
                provider.clone(),
                prompt.to_string(),
                Some("routine".to_string()),
                metadata,
                channel_name_hint,
                tmux_session_label,
                reservation,
            )
            .await
        }
        .map_err(|error| start_deferral::headless_start_error(agent_id, error))?;

        if outcome.turn_id != turn_id {
            return Err(anyhow!(
                "reserved routine agent turn id mismatch: expected {} but started {}",
                turn_id,
                outcome.turn_id
            ));
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
