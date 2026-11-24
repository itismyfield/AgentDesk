//! Session control targets the execution the routine actually started.
use super::{RuntimeResetOutcome, *};
use crate::services::routines::execution_mode::ProviderChoice;
use crate::services::routines::provider_choice::AgentProviderChannels;
use crate::services::routines::store::RoutineStore;

#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlPause {
    BeforeEffect,
    AfterCommit,
}

#[cfg(any(test, feature = "test-support"))]
tokio::task_local! {
    pub(crate) static CONTROL_TEST_PAUSE: (ControlPause, Arc<tokio::sync::Barrier>);
}

#[cfg(any(test, feature = "test-support"))]
pub(super) async fn pause_control_for_test(stage: ControlPause) {
    let barrier = CONTROL_TEST_PAUSE
        .try_with(|(at, barrier)| (*at == stage).then(|| barrier.clone()))
        .ok()
        .flatten();
    if let Some(barrier) = barrier {
        barrier.wait().await;
        barrier.wait().await;
    }
}

fn ensure_persistent_routine(routine_id: &str, strategy: &str) -> Result<()> {
    if strategy == "persistent" {
        Ok(())
    } else {
        Err(anyhow!(
            "routine {} requires execution_strategy=persistent for session control",
            routine_id
        ))
    }
}

struct ControlledTurn {
    run_id: String,
    turn_id: String,
}

fn control_lock_error(error: sqlx::Error) -> anyhow::Error {
    if error
        .as_database_error()
        .and_then(|db| db.code())
        .as_deref()
        == Some("55P03")
    {
        anyhow!("invalid routine run: session control is busy; retry")
    } else {
        error.into()
    }
}

impl RoutineSessionController {
    pub(crate) async fn control_persistent_session(
        &self,
        routine: &RoutineRecord,
        command: RoutineSessionCommand,
        reason: &str,
        runtime_reset: Option<&dyn RoutineRuntimeReset>,
    ) -> Result<RoutineSessionControlResult> {
        let mut control_guard = self.lock_control_run(routine).await?;
        let (target, controlled_turn) = self
            .resolve_control_target(routine, &mut control_guard)
            .await?;
        #[cfg(any(test, feature = "test-support"))]
        pause_control_for_test(ControlPause::BeforeEffect).await;
        let provider_clear_behavior = provider_clear_behavior(&target.provider);

        let mut runtime_cleared = false;
        let mut tmux_killed = false;
        let mut inflight_cleared = false;
        let mut lifecycle_path = "registry-unavailable";
        let mut queued_remaining = None;
        let mut queue_preserved = None;
        let mut disconnected_sessions = 0;

        match command {
            RoutineSessionCommand::Reset => {
                if let Some(reset) = runtime_reset {
                    lifecycle_path = match reset
                        .reset_runtime(
                            target.provider.as_str(),
                            target.channel_id,
                            target.session_key.as_deref(),
                        )
                        .await
                    {
                        RuntimeResetOutcome::Applied => {
                            runtime_cleared = true;
                            "runtime-clear"
                        }
                        RuntimeResetOutcome::Refused => "runtime-clear-refused",
                        RuntimeResetOutcome::Unavailable => "runtime-clear-unavailable",
                    };
                }
            }
            RoutineSessionCommand::Kill => {
                let lifecycle = self
                    .force_kill(&target, reason, "routine_session_kill")
                    .await;
                tmux_killed = lifecycle.tmux_killed;
                inflight_cleared = lifecycle.inflight_cleared;
                lifecycle_path = lifecycle.lifecycle_path;
                queued_remaining = lifecycle.queue_depth;
                queue_preserved = lifecycle.queue_preserved;
                // A kill the host guard keeps changed nothing, so the session rows stay too.
                if !lifecycle.host_guard_kept() {
                    disconnected_sessions = if controlled_turn.is_some() {
                        Self::disconnect_matching_sessions(&target, &mut *control_guard).await?
                    } else {
                        Self::disconnect_matching_sessions(&target, &*self.pool).await?
                    };
                }
            }
        }

        let changed =
            runtime_cleared || tmux_killed || inflight_cleared || disconnected_sessions > 0;
        let interrupted_run_id = if let Some(turn) = controlled_turn.as_ref().filter(|_| changed) {
            let result = serde_json::json!({
                "status": "interrupted_by_session_control",
                "routine_id": routine.id,
                "action": command.as_str(),
                "provider": target.provider.as_str(),
                "channel_id": target.channel_id.get().to_string(),
                "session_key": target.session_key,
                "tmux_session": target.tmux_session,
            });
            let interrupted = RoutineStore::interrupt_controlled_turn(
                &mut control_guard,
                &routine.id,
                &turn.run_id,
                &turn.turn_id,
                reason,
                Some(result),
            )
            .await?;
            if interrupted.is_none() {
                return Err(anyhow!(
                    "invalid routine run: turn changed before interruption"
                ));
            }
            interrupted
        } else {
            None
        };
        control_guard.commit().await?;
        #[cfg(any(test, feature = "test-support"))]
        pause_control_for_test(ControlPause::AfterCommit).await;
        Ok(RoutineSessionControlResult {
            controlled_run_id: controlled_turn.map(|turn| turn.run_id),
            interrupted_run_id,
            action: command.as_str(),
            routine_id: routine.id.clone(),
            agent_id: target.agent_id,
            provider: target.provider.as_str().to_string(),
            channel_id: target.channel_id.get().to_string(),
            session_key: target.session_key,
            tmux_session: target.tmux_session,
            provider_clear_behavior,
            runtime_cleared,
            tmux_killed,
            inflight_cleared,
            lifecycle_path,
            queued_remaining,
            queue_preserved,
            disconnected_sessions,
        })
    }

    async fn lock_control_run(
        &self,
        routine: &RoutineRecord,
    ) -> Result<sqlx::Transaction<'static, sqlx::Postgres>> {
        let mut tx = self.pool.begin().await?;
        let active = routine.in_flight_run_id.as_deref();
        if let Some(run) = active {
            // Match close_run's run-before-routine order. NOWAIT covers reverse-order thread writes.
            let locked: Option<String> = sqlx::query_scalar(
                "SELECT id FROM routine_runs WHERE id=$1 AND routine_id=$2 AND status='running'
                 FOR UPDATE NOWAIT",
            )
            .bind(run)
            .bind(&routine.id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(control_lock_error)?;
            if locked.is_none() {
                return Err(anyhow!(
                    "invalid routine run: changed before session control"
                ));
            }
        }
        let lock = if active.is_some() {
            "SELECT id FROM routines WHERE id=$1 AND in_flight_run_id IS NOT DISTINCT FROM $2
             AND ($2::text IS NOT NULL OR execution_strategy=$3) FOR NO KEY UPDATE NOWAIT"
        } else {
            "SELECT id FROM routines WHERE id=$1 AND in_flight_run_id IS NOT DISTINCT FROM $2
             AND ($2::text IS NOT NULL OR execution_strategy=$3) FOR NO KEY UPDATE"
        };
        let found: Option<String> = sqlx::query_scalar(lock)
            .bind(&routine.id)
            .bind(active)
            .bind(&routine.execution_strategy)
            .fetch_optional(&mut *tx)
            .await
            .map_err(control_lock_error)?;
        if found.is_none() {
            return Err(anyhow!(
                "invalid routine run: changed before session control"
            ));
        }
        Ok(tx)
    }

    async fn resolve_control_target(
        &self,
        live: &RoutineRecord,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ) -> Result<(RoutineSessionTarget, Option<ControlledTurn>)> {
        let active_run = live.in_flight_run_id.as_deref();
        if active_run.is_none() {
            ensure_persistent_routine(&live.id, &live.execution_strategy)?;
        }
        let row = sqlx::query(
            r#"
            SELECT rr.id, rr.status, rr.turn_id, rr.result_json, rr.attempts, rr.run_spec,
                   r.execution_strategy
            FROM routine_runs rr JOIN routines r ON r.id = rr.routine_id
            WHERE rr.routine_id = $1
              AND (rr.id = $2 OR (
                $2::text IS NULL AND rr.action = 'agent'
                AND (
                  rr.attempts @> '[{"event":"started"}]'::jsonb
                  OR (rr.turn_id IS NOT NULL AND rr.result_json->>'status' IN ('started', 'completed'))
                )
              ))
            ORDER BY rr.started_at DESC, rr.id DESC
            LIMIT 1
            "#,
        )
        .bind(&live.id)
        .bind(active_run)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| anyhow!("invalid routine run: no started execution for session control"))?;
        let run: String = row.try_get("id")?;
        let spec: Option<Value> = row.try_get("run_spec")?;
        let strategy = spec
            .as_ref()
            .and_then(|spec| spec["execution_strategy"].as_str())
            .unwrap_or(row.try_get::<&str, _>("execution_strategy")?);
        ensure_persistent_routine(&live.id, strategy)?;

        let current_turn: Option<String> = row.try_get("turn_id")?;
        let running = active_run.is_some();
        let controlled_turn = match (
            running,
            row.try_get::<&str, _>("status")?,
            current_turn.as_ref(),
        ) {
            (false, _, _) => None,
            (true, "running", Some(turn_id)) => Some(ControlledTurn {
                run_id: run.clone(),
                turn_id: turn_id.clone(),
            }),
            _ => {
                return Err(anyhow!(
                    "invalid routine run {run} has no current started agent turn"
                ));
            }
        };
        let result: Option<Value> = row.try_get("result_json")?;
        let attempts: Value = row.try_get("attempts")?;
        let attempt = attempts.as_array().and_then(|all| {
            all.iter().rev().find(|a| {
                let event = a["event"].as_str();
                if running {
                    matches!(event, Some("started" | "input_pending"))
                        && a["turn_id"].as_str() == current_turn.as_deref()
                } else {
                    event == Some("started")
                }
            })
        });
        let turn = string_field(attempt, "turn_id")
            .or(current_turn.as_deref())
            .ok_or_else(|| anyhow!("invalid routine run {run} has no started turn ID"))?;
        let result = result
            .as_ref()
            .filter(|v| v["turn_id"].as_str() == Some(turn));
        let read = |key: &str| string_field(result, key).or_else(|| string_field(attempt, key));
        let agent = read("agent_id")
            .or_else(|| string_field(spec.as_ref(), "agent_id"))
            .ok_or_else(|| anyhow!("invalid routine run {run} has no claimed agent"))?;
        let provider = read("provider")
            .or_else(|| string_field(spec.as_ref(), "provider"))
            .ok_or_else(|| anyhow!("invalid routine run {run}: applied provider is unproved"))?;
        let route = AgentProviderChannels::load(&self.pool, agent)
            .await?
            .route(
                agent,
                &ProviderChoice {
                    provider: Some(provider.to_string()),
                    model: None,
                },
            )?;
        let parent =
            crate::services::dispatches::outbox_route::resolve_channel_alias_pub(&route.channel)
                .or_else(|| route.channel.parse::<u64>().ok())
                .ok_or_else(|| anyhow!("claimed provider channel is invalid"))?;
        let recorded_parent = read("parent_channel_id").and_then(parse_discord_channel_id);
        let key = read("session_key");
        let actual_thread = read("discord_thread_id")
            .and_then(parse_discord_channel_id)
            .or_else(|| read("channel_id").and_then(parse_discord_channel_id));
        let thread = actual_thread.or_else(|| {
            string_field(spec.as_ref(), "discord_thread_id").and_then(parse_discord_channel_id)
        });
        let session = if let Some(key) = key {
            let tmux = tmux_name_from_session_key(key)
                .ok_or_else(|| anyhow!("invalid run session key is not namespace-exact"))?;
            self.load_owned_session(agent, &route.provider, key, tmux)
                .await?
        } else if let Some(thread) = thread.filter(|c| *c != parent && Some(*c) != recorded_parent)
        {
            self.load_latest_thread_session(agent, &route.provider, thread)
                .await?
        } else {
            return Err(anyhow!(
                "invalid routine run {run} has no isolated session evidence; refusing primary session control"
            ));
        };
        let session =
            session.ok_or_else(|| anyhow!("invalid routine run {run} session is not provable"))?;
        let session_key = session
            .session_key
            .clone()
            .ok_or_else(|| anyhow!("invalid routine run {run} session has no exact key"))?;
        let tmux = tmux_name_from_session_key(&session_key)
            .ok_or_else(|| anyhow!("invalid routine run {run} session key is invalid"))?
            .to_string();
        let channel = actual_thread
            .or_else(|| {
                session
                    .thread_channel_id
                    .as_deref()
                    .and_then(parse_discord_channel_id)
            })
            .or(thread)
            .ok_or_else(|| anyhow!("invalid routine run {run} session channel is unproved"))?;
        Ok((
            RoutineSessionTarget {
                agent_id: agent.to_string(),
                provider: route.provider,
                channel_id: ChannelId::new(channel),
                session_id: Some(session.id),
                session_key: Some(session_key),
                tmux_session: tmux,
            },
            controlled_turn,
        ))
    }
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) async fn seed_started_control_session(
    pool: &PgPool,
    routine: &RoutineRecord,
    provider: &str,
    channel: &str,
    session_key: &str,
) {
    sqlx::query("INSERT INTO routines (id, agent_id, script_ref, name, status, execution_strategy, discord_thread_id)
        VALUES ($1,$2,$3,$4,'enabled','persistent',$5)")
        .bind(&routine.id)
        .bind(&routine.agent_id)
        .bind(&routine.script_ref)
        .bind(&routine.name)
        .bind(&routine.discord_thread_id)
        .execute(pool)
        .await
        .unwrap();
    let store =
        RoutineStore::new_with_timezone_and_checkpoint_limit(Arc::new(pool.clone()), "UTC", 4096);
    let run = store.claim_run_now(&routine.id).await.unwrap().unwrap();
    let turn = format!("fixture:{}", run.run_id);
    let agent = routine.agent_id.as_deref().unwrap();
    let mut result = serde_json::json!({
        "status": "started", "turn_id": turn, "agent_id": agent,
        "provider": provider, "channel_id": channel, "session_key": session_key,
    });
    assert!(
        store
            .mark_agent_turn_started(&run.run_id, &turn, Some(result.clone()), agent, "primary")
            .await
            .unwrap()
    );
    result["status"] = serde_json::json!("completed");
    assert!(
        store
            .complete_agent_run(
                &run.run_id,
                Some(result),
                None,
                None,
                crate::services::routines::store::NextDueAtUpdate::Preserve,
            )
            .await
            .unwrap()
    );
}
