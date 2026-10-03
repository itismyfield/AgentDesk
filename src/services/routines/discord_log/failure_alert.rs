//! Consecutive-failure alert: one actionable notice when a routine's streak of failed runs
//! reaches the configured threshold, independent of the pause and stale-paused knobs.
use super::*;

pub(crate) const CONSECUTIVE_FAILURES_REASON_CODE: &str = "routine_consecutive_failures";
const CONSECUTIVE_FAILURES_ALERT_TTL_SECS: i64 = 24 * 60 * 60;

/// Leading `failed` runs among the routine's latest terminal runs, read up to `limit` rows.
/// `interrupted` runs (restart recovery) neither extend nor reset the streak.
async fn consecutive_failed_runs(pool: &PgPool, routine_id: &str, limit: i64) -> Result<usize> {
    let statuses: Vec<String> = sqlx::query_scalar(
        r#"
        SELECT status
        FROM routine_runs
        WHERE routine_id = $1
          AND status NOT IN ('running', 'interrupted')
        ORDER BY created_at DESC, id DESC
        LIMIT $2
        "#,
    )
    .bind(routine_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|error| anyhow!("count consecutive failed routine runs {routine_id}: {error}"))?;
    Ok(statuses
        .iter()
        .take_while(|status| *status == "failed")
        .count())
}

impl RoutineDiscordLogger {
    /// Alerts once when `outcome` brings the routine's consecutive failures to exactly
    /// `threshold`; later failures in the same streak stay quiet. 0 disables the alert.
    pub async fn alert_consecutive_failures(
        &self,
        store: &RoutineStore,
        outcome: &RoutineRunOutcome,
        threshold: u32,
    ) -> RoutineDiscordLogStatus {
        if threshold == 0 || outcome.status != "failed" {
            return RoutineDiscordLogStatus::skipped();
        }
        let streak = match consecutive_failed_runs(
            &self.pool,
            &outcome.routine_id,
            i64::from(threshold) + 1,
        )
        .await
        {
            Ok(streak) => streak,
            Err(error) => return RoutineDiscordLogStatus::failed(error),
        };
        if streak != threshold as usize {
            return RoutineDiscordLogStatus::skipped();
        }
        let routine = match store.get_routine(&outcome.routine_id).await {
            Ok(Some(routine)) => routine,
            Ok(None) => return RoutineDiscordLogStatus::skipped(),
            Err(error) => return RoutineDiscordLogStatus::failed(error),
        };
        let message = consecutive_failures_message(&routine, threshold, outcome.error.as_deref());
        // Keyed by the run that completed the streak, so re-logging that outcome cannot repeat it.
        let session_key = format!(
            "routine:{}:consecutive_failures:{}",
            routine.id, outcome.run_id
        );
        let status = self
            .log_actionable_to_routine_target_with_ttl(
                Some(store),
                Some(&routine.id),
                Some(&routine.name),
                routine.agent_id.as_deref(),
                routine.discord_thread_id.as_deref(),
                CONSECUTIVE_FAILURES_REASON_CODE,
                &session_key,
                &message,
                CONSECUTIVE_FAILURES_ALERT_TTL_SECS,
            )
            .await;
        if status.status != "ok" {
            tracing::warn!(
                routine_id = %routine.id,
                routine = %routine.name,
                consecutive_failures = threshold,
                error = ?outcome.error,
                "routine consecutive-failure alert could not be enqueued"
            );
        }
        status
    }
}

fn consecutive_failures_message(
    routine: &RoutineRecord,
    threshold: u32,
    last_error: Option<&str>,
) -> String {
    let next_slot = routine
        .next_due_at
        .map(|value| value.to_rfc3339())
        .unwrap_or_else(|| "없음".to_string());
    routine_log_block_message(
        "루틴 연속 실패",
        vec![(
            "기본",
            vec![
                field_line(
                    "reason",
                    format!("{threshold}회 연속 실패 - 점검이 필요합니다"),
                ),
                field_line("routine", compact(&routine.name, 80)),
                field_line("id", &routine.id),
                field_line("script", script_ref_for_message(&routine.script_ref, 120)),
                field_line("last_error", compact(last_error.unwrap_or("없음"), 160)),
                field_line("next_slot", next_slot),
            ],
        )],
    )
}
