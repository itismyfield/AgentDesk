//! Public results and attention notices exclude the run's diagnostic payload.
use super::*;

pub(super) const RUN_ATTENTION_REASON: &str = "routine_run_attention";

pub(super) fn attention_message(routine: &RoutineRecord, summary: &str) -> String {
    format!(
        "루틴 {} · {summary}\n[실행 이력](/routines)",
        compact(&routine.name, 80)
    )
}

impl RoutineDiscordLogger {
    pub(super) async fn log_public_run_outcome(
        &self,
        store: &RoutineStore,
        routine: &RoutineRecord,
        outcome: &RoutineRunOutcome,
    ) -> RoutineDiscordLogStatus {
        let snapshot: Result<(String, bool, bool), _> = sqlx::query_as(
            "SELECT status, COALESCE(run_spec ->> 'execution_mode' = 'headless', FALSE),
                    COALESCE(result_json ->> 'thread_failure' = 'create_unconfirmed', FALSE)
             FROM routine_runs WHERE id = $1 AND routine_id = $2",
        )
        .bind(&outcome.run_id)
        .bind(&routine.id)
        .fetch_one(&*self.pool)
        .await;
        let (status, headless, create_unconfirmed) = match snapshot {
            Ok(status) => status,
            Err(error) => return RoutineDiscordLogStatus::failed(error),
        };
        // A run whose thread create went unanswered gets no second thread from its own log.
        let thread_store = (!create_unconfirmed).then_some(store);
        // The census verdict goes to the routine's thread, which a headless run does not have.
        if !headless && super::super::legacy_body_census::applies(&routine.script_ref) {
            return self
                .log_census_outcome(store, thread_store, routine, outcome)
                .await;
        }
        // Headless failures are reported at close; a headless pause goes to the primary channel.
        if headless {
            return self.log_headless_pause(routine, outcome, &status).await;
        }
        let summary = match status.as_str() {
            "failed" if routine.status == "paused" => "실행 실패 · 일시정지됨",
            "failed" => "실행 실패 · 점검이 필요합니다",
            "paused" => "일시정지됨 · 실행 이력을 확인해 주세요",
            _ => return RoutineDiscordLogStatus::skipped(),
        };
        let target = match self
            .resolve_routine_log_target(
                thread_store,
                Some(&routine.id),
                Some(&routine.name),
                routine.agent_id.as_deref(),
                routine.discord_thread_id.as_deref(),
            )
            .await
        {
            Ok(Some(target)) => target,
            Ok(None) => return RoutineDiscordLogStatus::skipped(),
            Err(error) => return RoutineDiscordLogStatus::failed(error),
        };
        match crate::services::message_outbox::enqueue_outbox_pg_returning_id_with_persistent_dedupe(
            &self.pool,
            crate::services::message_outbox::OutboxMessage {
                target: &target.target, content: &attention_message(routine, summary), bot: &target.bot,
                source: "routine-runtime", reason_code: Some(RUN_ATTENTION_REASON),
                session_key: Some(&format!("routine:{}:run:{}:attention", routine.id, outcome.run_id)),
            },
        ).await {
            Ok(_) => RoutineDiscordLogStatus::ok(),
            Err(error) => RoutineDiscordLogStatus::failed(format!("failed to enqueue routine discord log: {error}")),
        }
    }
}

impl RoutineDiscordLogger {
    /// Whether the routine's stored definition is headless; a failed read is an error, not "no".
    pub(super) async fn headless_routine(
        &self,
        store: &RoutineStore,
        routine: &RoutineRecord,
    ) -> Result<bool, String> {
        let definitions = store
            .load_execution_definitions(std::slice::from_ref(&routine.id))
            .await
            .map_err(|error| error.to_string())?;
        let mode = definitions.get(&routine.id);
        Ok(mode.and_then(|row| row.execution_mode.as_deref()) == Some("headless"))
    }

    /// `agent:<id>` for a routine whose stored definition is headless.
    pub(super) async fn headless_primary_target(
        &self,
        store: &RoutineStore,
        routine: &RoutineRecord,
    ) -> Option<String> {
        let agent_id = routine.agent_id.as_deref()?;
        let definitions = store
            .load_execution_definitions(std::slice::from_ref(&routine.id))
            .await
            .ok()?;
        let headless = definitions.get(&routine.id)?.execution_mode.as_deref() == Some("headless");
        headless.then(|| format!("agent:{agent_id}"))
    }

    /// `None` unless the routine is headless; the agent's own channel, not an operator fallback.
    pub(super) async fn log_headless_stale_to_primary(
        &self,
        store: &RoutineStore,
        routine: &RoutineRecord,
        session_key: &str,
        message: &str,
        dedupe_ttl_secs: i64,
    ) -> Option<RoutineDiscordLogStatus> {
        let target = self.headless_primary_target(store, routine).await?;
        Some(
            self.log_to_target_with_ttl(
                &target,
                crate::services::message_outbox::ACTIONABLE_OPS_ALERT_BOT,
                super::STALE_PAUSED_REASON_CODE,
                session_key,
                message,
                dedupe_ttl_secs,
            )
            .await,
        )
    }

    async fn log_headless_pause(
        &self,
        routine: &RoutineRecord,
        outcome: &RoutineRunOutcome,
        status: &str,
    ) -> RoutineDiscordLogStatus {
        let Some(agent_id) = routine.agent_id.as_deref().filter(|_| status == "paused") else {
            return RoutineDiscordLogStatus::skipped();
        };
        let session_key = format!("routine:{}:run:{}:attention", routine.id, outcome.run_id);
        match crate::services::message_outbox::enqueue_outbox_pg_returning_id_with_persistent_dedupe(
            &self.pool,
            crate::services::message_outbox::OutboxMessage {
                target: &format!("agent:{agent_id}"),
                content: &attention_message(routine, "일시정지됨 · 실행 이력을 확인해 주세요"),
                bot: crate::services::message_outbox::ACTIONABLE_OPS_ALERT_BOT,
                source: "routine-runtime",
                reason_code: Some(RUN_ATTENTION_REASON),
                session_key: Some(&session_key),
            },
        )
        .await
        {
            Ok(_) => RoutineDiscordLogStatus::ok(),
            Err(error) => RoutineDiscordLogStatus::failed(format!(
                "failed to enqueue routine discord log: {error}"
            )),
        }
    }
}
