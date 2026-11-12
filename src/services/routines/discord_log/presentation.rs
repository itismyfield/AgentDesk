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
        if super::super::legacy_body_census::applies(&routine.script_ref) {
            return self.log_census_outcome(store, routine, outcome).await;
        }
        let status: String = match sqlx::query_scalar(
            "SELECT status FROM routine_runs WHERE id = $1 AND routine_id = $2",
        )
        .bind(&outcome.run_id)
        .bind(&routine.id)
        .fetch_one(&*self.pool)
        .await
        {
            Ok(status) => status,
            Err(error) => return RoutineDiscordLogStatus::failed(error),
        };
        let summary = match status.as_str() {
            "failed" if routine.status == "paused" => "실행 실패 · 일시정지됨",
            "failed" => "실행 실패 · 점검이 필요합니다",
            "paused" => "일시정지됨 · 실행 이력을 확인해 주세요",
            _ => return RoutineDiscordLogStatus::skipped(),
        };
        self.log_to_routine_target(
            Some(store),
            Some(&routine.id),
            Some(&routine.name),
            routine.agent_id.as_deref(),
            routine.discord_thread_id.as_deref(),
            RUN_ATTENTION_REASON,
            &format!("routine:{}:run:{}:attention", routine.id, outcome.run_id),
            &attention_message(routine, summary),
        )
        .await
    }
}
