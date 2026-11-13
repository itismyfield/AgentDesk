//! Headless failure streak events, reserved in the transaction that stores the failed run so a
//! re-logged outcome, a restart or a concurrent poll cannot add or lose one.
use super::*;
use crate::services::message_outbox::{
    ACTIONABLE_OPS_ALERT_BOT, OutboxMessage, enqueue_outbox_pg_on_tx_with_ttl,
    enqueue_outbox_pg_returning_id_with_persistent_dedupe_on_tx,
};

pub(crate) const HEADLESS_FAILURE_REASON_CODE: &str = "routine_headless_failure";
pub(crate) const FAILURE_INCIDENT_REASON_CODE: &str = "routine_failure_incident";
const NOTICE_TTL_SECS: i64 = 24 * 60 * 60;
/// Failure classes a notice may name; anything else is reported generically.
const SAFE_FAILURE_CLASSES: [(&str, &str); 4] = [
    ("timeout", "시간 초과"),
    ("provider_error", "provider 오류"),
    ("primary_busy", "본 채널 사용 중"),
    ("deferred", "시작 대기 소진"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HeadlessFailureEvent {
    /// Streak 1..=N: a non-turn notice per failed run.
    Notice,
    /// Streak N+1: one command-bot message per streak, keyed by its first failed run.
    Incident,
    /// Streak > N+1: notices again, merged per streak for 24h.
    AfterIncident,
}

pub(crate) fn headless_failure_event(streak: usize, limit: usize) -> Option<HeadlessFailureEvent> {
    match streak {
        0 => None,
        k if k <= limit => Some(HeadlessFailureEvent::Notice),
        k if k == limit + 1 => Some(HeadlessFailureEvent::Incident),
        _ => Some(HeadlessFailureEvent::AfterIncident),
    }
}

#[derive(sqlx::FromRow)]
struct FailedRun {
    id: String,
    run_spec: Option<Value>,
    result_json: Option<Value>,
}

fn is_headless(spec: Option<&Value>) -> bool {
    spec.and_then(|spec| spec.get("execution_mode"))
        .and_then(Value::as_str)
        == Some("headless")
}

/// The pinned N of the streak's first failed run; runs without one use the default.
fn pinned_limit(spec: Option<&Value>) -> usize {
    spec.and_then(|spec| spec.get("headless_failure_notice_limit"))
        .and_then(Value::as_u64)
        .and_then(|limit| usize::try_from(limit).ok())
        .filter(|limit| *limit >= 1)
        .unwrap_or(DEFAULT_HEADLESS_FAILURE_NOTICE_LIMIT as usize)
}

fn failure_class(result: Option<&Value>) -> &'static str {
    let status = result
        .and_then(|result| result.get("status"))
        .and_then(Value::as_str);
    SAFE_FAILURE_CLASSES
        .iter()
        .find(|(class, _)| Some(*class) == status)
        .map_or("실행 실패", |(_, label)| label)
}

impl RoutineStore {
    pub(crate) async fn run_is_headless(&self, run_id: &str) -> Result<bool> {
        let spec: Option<Option<Value>> =
            sqlx::query_scalar("SELECT run_spec FROM routine_runs WHERE id = $1")
                .bind(run_id)
                .fetch_optional(&*self.pool)
                .await
                .map_err(|e| anyhow!("read routine run {run_id} run_spec: {e}"))?;
        Ok(is_headless(spec.flatten().as_ref()))
    }

    /// Called once per run, after `run_id` was stored as failed inside `tx`.
    pub(super) async fn reserve_headless_failure_event(
        tx: &mut Transaction<'_, Postgres>,
        routine_id: &str,
        run_id: &str,
        paused: bool,
    ) -> Result<()> {
        // The streak is every failed run after the latest nonfailed terminal; interrupted is neutral.
        let streak: Vec<FailedRun> = sqlx::query_as(
            r#"
            SELECT f.id, f.run_spec, f.result_json
            FROM routine_runs f
            WHERE f.routine_id = $1
              AND f.status = 'failed'
              AND NOT EXISTS (
                  SELECT 1
                  FROM routine_runs b
                  WHERE b.routine_id = $1
                    AND b.status NOT IN ('running', 'interrupted', 'failed')
                    AND (b.created_at, b.id) > (f.created_at, f.id)
              )
            ORDER BY f.created_at, f.id
            "#,
        )
        .bind(routine_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(|e| anyhow!("routine {routine_id}: read failure streak: {e}"))?;
        let (Some(first), Some(current)) = (streak.first(), streak.last()) else {
            return Ok(());
        };
        if current.id != run_id || !is_headless(current.run_spec.as_ref()) {
            return Ok(());
        }
        let Some(agent_id) = current
            .run_spec
            .as_ref()
            .and_then(|spec| spec.get("agent_id"))
            .and_then(Value::as_str)
            .filter(|agent| !agent.trim().is_empty())
        else {
            return Ok(());
        };
        let limit = pinned_limit(first.run_spec.as_ref());
        let Some(event) = headless_failure_event(streak.len(), limit) else {
            return Ok(());
        };
        let name: String = sqlx::query_scalar("SELECT name FROM routines WHERE id = $1")
            .bind(routine_id)
            .fetch_one(&mut **tx)
            .await
            .map_err(|e| anyhow!("routine {routine_id}: read name: {e}"))?;
        let name = name.chars().take(80).collect::<String>();
        let class = failure_class(current.result_json.as_ref());
        let paused = if paused { " · 일시정지됨" } else { "" };
        let target = format!("agent:{agent_id}");
        let k = streak.len();
        let enqueued = match event {
            HeadlessFailureEvent::Incident => {
                let content = format!(
                    "[실행 보기](/routines) · {name} · {class} · {k}회 연속 실패(N+1){paused}\n\
                     원인을 진단하고 조치해 주세요. 실패한 루틴을 다시 실행하지는 마세요."
                );
                enqueue_outbox_pg_returning_id_with_persistent_dedupe_on_tx(
                    tx,
                    OutboxMessage {
                        target: &target,
                        content: &content,
                        bot: crate::domain::bot_role::UtilityBotRole::Announce.alias(),
                        source: "routine-runtime",
                        reason_code: Some(FAILURE_INCIDENT_REASON_CODE),
                        session_key: Some(&format!(
                            "routine:{routine_id}:failure_incident:{}",
                            first.id
                        )),
                    },
                )
                .await
                .map(|_| ())
            }
            HeadlessFailureEvent::Notice | HeadlessFailureEvent::AfterIncident => {
                let content = format!(
                    "루틴 {name} · {class} · {k}회 연속 실패{paused}\n[실행 이력](/routines)"
                );
                let session_key = if event == HeadlessFailureEvent::Notice {
                    format!("routine:{routine_id}:headless_failure:{run_id}")
                } else {
                    format!("routine:{routine_id}:after_incident:{}", first.id)
                };
                enqueue_outbox_pg_on_tx_with_ttl(
                    tx,
                    OutboxMessage {
                        target: &target,
                        content: &content,
                        bot: ACTIONABLE_OPS_ALERT_BOT,
                        source: "routine-runtime",
                        reason_code: Some(HEADLESS_FAILURE_REASON_CODE),
                        session_key: Some(&session_key),
                    },
                    NOTICE_TTL_SECS,
                )
                .await
                .map(|_| ())
            }
        };
        enqueued
            .map_err(|e| anyhow!("routine {routine_id}: reserve headless failure {event:?}: {e:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FAILURE_INCIDENT_REASON_CODE, HEADLESS_FAILURE_REASON_CODE, HeadlessFailureEvent,
        headless_failure_event,
    };
    use crate::services::message_outbox::{is_actionable_ops_alert, is_non_turn_operational_alert};

    #[test]
    fn notices_are_non_turn_alerts_and_the_incident_is_ordinary_announce_input() {
        let source = "routine-runtime";
        assert!(is_non_turn_operational_alert(
            source,
            Some(HEADLESS_FAILURE_REASON_CODE)
        ));
        assert!(is_actionable_ops_alert(
            source,
            Some(HEADLESS_FAILURE_REASON_CODE)
        ));
        assert!(!is_non_turn_operational_alert(
            source,
            Some(FAILURE_INCIDENT_REASON_CODE)
        ));
        assert!(!is_actionable_ops_alert(
            source,
            Some(FAILURE_INCIDENT_REASON_CODE)
        ));
    }

    #[test]
    fn notices_until_n_one_incident_at_n_plus_one_then_notices_again() {
        use HeadlessFailureEvent::{AfterIncident, Incident, Notice};
        let n3: Vec<_> = (0..=6).map(|k| headless_failure_event(k, 3)).collect();
        assert_eq!(
            n3,
            [
                None,
                Some(Notice),
                Some(Notice),
                Some(Notice),
                Some(Incident),
                Some(AfterIncident),
                Some(AfterIncident)
            ]
        );
        let n1: Vec<_> = (0..=3).map(|k| headless_failure_event(k, 1)).collect();
        assert_eq!(
            n1,
            [None, Some(Notice), Some(Incident), Some(AfterIncident)]
        );
    }
}
