//! Session control targets the execution the routine actually started.
use super::*;
use crate::services::routines::execution_mode::ProviderChoice;
use crate::services::routines::provider_choice::AgentProviderChannels;
use crate::services::routines::store::RoutineStore;

impl RoutineSessionController {
    pub(super) async fn lock_control_run(
        &self,
        routine: &RoutineRecord,
    ) -> Result<sqlx::Transaction<'static, sqlx::Postgres>> {
        let mut tx = self.pool.begin().await?;
        // Fence physical control against another claim or completion. Release before interrupting.
        let found: Option<String> = sqlx::query_scalar(
            "SELECT id FROM routines WHERE id=$1 AND in_flight_run_id IS NOT DISTINCT FROM $2
             AND ($2::text IS NOT NULL OR execution_strategy=$3) FOR NO KEY UPDATE",
        )
        .bind(&routine.id)
        .bind(&routine.in_flight_run_id)
        .bind(&routine.execution_strategy)
        .fetch_optional(&mut *tx)
        .await?;
        if found.is_none() {
            return Err(anyhow!(
                "invalid routine run: changed before session control"
            ));
        }
        Ok(tx)
    }

    pub(super) async fn resolve_control_target(
        &self,
        live: &RoutineRecord,
    ) -> Result<(RoutineSessionTarget, Option<String>)> {
        let active_run = live.in_flight_run_id.as_deref();
        if active_run.is_none() {
            ensure_persistent_routine(live)?;
        }
        let row = sqlx::query(
            r#"
            SELECT id, status, turn_id, result_json, attempts, run_spec
            FROM routine_runs
            WHERE routine_id = $1
              AND (id = $2 OR (
                $2::text IS NULL AND action = 'agent'
                AND (
                  attempts @> '[{"event":"started"}]'::jsonb
                  OR (turn_id IS NOT NULL AND result_json->>'status' IN ('started', 'completed'))
                )
              ))
            ORDER BY started_at DESC, id DESC
            LIMIT 1
            "#,
        )
        .bind(&live.id)
        .bind(active_run)
        .fetch_optional(&*self.pool)
        .await?
        .ok_or_else(|| anyhow!("invalid routine run: no started execution for session control"))?;
        let run: String = row.try_get("id")?;
        let store =
            RoutineStore::new_with_timezone_and_checkpoint_limit(self.pool.clone(), "UTC", 4096);
        let claimed = store
            .routine_as_claimed(&live.id, &run)
            .await?
            .ok_or_else(|| anyhow!("routine {} vanished before control", live.id))?;
        ensure_persistent_routine(&claimed)?;

        let current_turn: Option<String> = row.try_get("turn_id")?;
        let running = active_run.is_some();
        if running && (row.try_get::<String, _>("status")? != "running" || current_turn.is_none()) {
            return Err(anyhow!(
                "invalid routine run {run} has no current started agent turn"
            ));
        }
        let result: Option<Value> = row.try_get("result_json")?;
        let attempts: Value = row.try_get("attempts")?;
        let spec: Option<Value> = row.try_get("run_spec")?;
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
            running.then_some(run),
        ))
    }
}

#[cfg(test)]
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
