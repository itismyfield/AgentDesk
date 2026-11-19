//! A running routine controls only its claimed execution, fenced against another claim.
use super::*;
use crate::services::routines::execution_mode::ProviderChoice;
use crate::services::routines::provider_choice::AgentProviderChannels;
use crate::services::routines::store::RoutineStore;

impl RoutineSessionController {
    pub(super) async fn lock_control_run(
        &self,
        routine: &RoutineRecord,
    ) -> Result<Option<sqlx::Transaction<'static, sqlx::Postgres>>> {
        let Some(run) = routine.in_flight_run_id.as_deref() else {
            return Ok(None);
        };
        let mut tx = self.pool.begin().await?;
        // Finish and the next claim wait for physical control; release this before interrupting.
        let found: Option<String> = sqlx::query_scalar(
            "SELECT id FROM routines WHERE id=$1 AND in_flight_run_id=$2 FOR KEY SHARE",
        )
        .bind(&routine.id)
        .bind(run)
        .fetch_optional(&mut *tx)
        .await?;
        if found.is_none() {
            return Err(anyhow!(
                "invalid routine run: changed before session control"
            ));
        }
        Ok(Some(tx))
    }

    pub(super) async fn resolve_control_target(
        &self,
        live: &RoutineRecord,
    ) -> Result<(RoutineSessionTarget, Option<String>)> {
        let Some(run) = live.in_flight_run_id.as_deref() else {
            ensure_persistent_routine(live)?;
            return Ok((self.resolve_target(live).await?, None));
        };
        let store =
            RoutineStore::new_with_timezone_and_checkpoint_limit(self.pool.clone(), "UTC", 4096);
        let claimed = store
            .routine_as_claimed(&live.id, run)
            .await?
            .ok_or_else(|| anyhow!("routine {} vanished before control", live.id))?;
        ensure_persistent_routine(&claimed)?;
        let row = sqlx::query("SELECT status,turn_id,result_json,attempts,run_spec FROM routine_runs WHERE id=$1 AND routine_id=$2")
            .bind(run).bind(&live.id).fetch_optional(&*self.pool).await?
            .ok_or_else(|| anyhow!("routine run {run} not found for session control"))?;
        let turn: Option<String> = row.try_get("turn_id")?;
        if row.try_get::<String, _>("status")? != "running" || turn.is_none() {
            return Err(anyhow!(
                "invalid routine run {run} has no current started agent turn"
            ));
        }
        let result: Option<Value> = row.try_get("result_json")?;
        let attempts: Value = row.try_get("attempts")?;
        let spec: Option<Value> = row.try_get("run_spec")?;
        let attempt = attempts.as_array().and_then(|all| {
            all.iter().rev().find(|a| {
                matches!(a["event"].as_str(), Some("started" | "input_pending"))
                    && a["turn_id"].as_str() == turn.as_deref()
            })
        });
        let result = result
            .as_ref()
            .filter(|v| v["turn_id"].as_str() == turn.as_deref());
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
        let key = read("session_key");
        let actual_thread = read("discord_thread_id")
            .and_then(parse_discord_channel_id)
            .or_else(|| read("channel_id").and_then(parse_discord_channel_id));
        let thread = actual_thread.or_else(|| {
            claimed
                .discord_thread_id
                .as_deref()
                .and_then(parse_discord_channel_id)
        });
        let session = if let Some(key) = key {
            let tmux = tmux_name_from_session_key(key)
                .ok_or_else(|| anyhow!("invalid run session key is not namespace-exact"))?;
            self.load_owned_session(agent, &route.provider, key, tmux)
                .await?
        } else if let Some(thread) = thread.filter(|c| *c != parent) {
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
            Some(run.to_string()),
        ))
    }
}
