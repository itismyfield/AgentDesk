pub(crate) mod orphan_recovery;

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Row as SqlxRow};

use crate::engine::{PolicyEngine, PolicyEngineHandle};
use crate::error::{AppError, ErrorCode};

const SUPERVISOR_ACTOR: &str = "runtime_supervisor";
const AUDIT_ONLY_ACK_FIELD: &str = "supervisor_audit_only";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SupervisorSignal {
    /// Audit-only signal. Rust records an escalation decision only when callers
    /// explicitly set `supervisor_audit_only: true`; no recovery action exists.
    DeadlockCandidate,
    /// Implemented recovery signal. Rust probes/recovers orphan dispatches.
    OrphanCandidate,
    /// Audit-only signal. Rust records an escalation decision only when callers
    /// explicitly set `supervisor_audit_only: true`; no recovery action exists.
    ResumeCandidate,
    /// Audit-only signal. Rust records an escalation decision only when callers
    /// explicitly set `supervisor_audit_only: true`; no recovery action exists.
    StaleInflight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorSignalSupport {
    /// Rust owns and may execute corrective action semantics for this signal.
    ImplementedAction,
    /// Rust only records an escalation/probe audit row; callers must opt in.
    AuditOnly,
    /// Reserved names are not accepted at public emit boundaries.
    Reserved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SupervisorAction {
    Probe,
    Reconnect,
    Fail,
    Redispatch,
    Resume,
    Escalate,
}

#[derive(Debug, Clone, Serialize)]
pub struct SupervisorDecision {
    pub signal: SupervisorSignal,
    pub support_state: SupervisorSignalSupport,
    pub chosen_action: SupervisorAction,
    pub actor: &'static str,
    pub session_key: Option<String>,
    pub dispatch_id: Option<String>,
    pub executed: bool,
    pub note: Option<String>,
}

#[derive(Clone, Default)]
pub struct BridgeHandle {
    engine: Arc<Mutex<Option<PolicyEngineHandle>>>,
}

impl BridgeHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn attach_engine(&self, engine: &PolicyEngine) {
        if let Ok(mut slot) = self.engine.lock() {
            *slot = Some(engine.downgrade());
        }
    }

    pub fn upgrade_engine(&self) -> Result<PolicyEngine, String> {
        let handle = self
            .engine
            .lock()
            .map_err(|e| format!("supervisor bridge lock poisoned: {e}"))?
            .clone()
            .ok_or_else(|| "runtime supervisor is not attached".to_string())?;
        handle
            .upgrade()
            .ok_or_else(|| "policy engine is no longer available".to_string())
    }

    fn runtime_supervisor(&self) -> Result<RuntimeSupervisor, String> {
        let engine = self.upgrade_engine()?;
        Ok(RuntimeSupervisor::new(engine.pg_pool().cloned(), engine))
    }
}

#[derive(Clone)]
pub struct RuntimeSupervisor {
    pg_pool: Option<PgPool>,
}

impl RuntimeSupervisor {
    pub fn new(pg_pool: Option<PgPool>, _engine: PolicyEngine) -> Self {
        Self { pg_pool }
    }

    fn kv_get(&self, key: &str) -> Result<Option<String>, String> {
        if let Some(pool) = self.pg_pool.as_ref() {
            let key = key.to_string();
            return crate::utils::async_bridge::block_on_pg_result(
                pool,
                move |bridge_pool| async move {
                    sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1")
                        .bind(&key)
                        .fetch_optional(&bridge_pool)
                        .await
                        .map_err(|error| format!("load kv_meta {key}: {error}"))
                },
                |error| error,
            );
        }

        Err("runtime supervisor postgres backend is unavailable".to_string())
    }

    fn kv_set(&self, key: &str, value: &str) -> Result<(), String> {
        if let Some(pool) = self.pg_pool.as_ref() {
            let key = key.to_string();
            let value = value.to_string();
            return crate::utils::async_bridge::block_on_pg_result(
                pool,
                move |bridge_pool| async move {
                    sqlx::query(
                        "INSERT INTO kv_meta (key, value)
                         VALUES ($1, $2)
                         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
                    )
                    .bind(&key)
                    .bind(&value)
                    .execute(&bridge_pool)
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("store kv_meta {key}: {error}"))
                },
                |error| error,
            );
        }

        Err("runtime supervisor postgres backend is unavailable".to_string())
    }

    fn kv_delete(&self, key: &str) -> Result<(), String> {
        if let Some(pool) = self.pg_pool.as_ref() {
            let key = key.to_string();
            return crate::utils::async_bridge::block_on_pg_result(
                pool,
                move |bridge_pool| async move {
                    sqlx::query("DELETE FROM kv_meta WHERE key = $1")
                        .bind(&key)
                        .execute(&bridge_pool)
                        .await
                        .map(|_| ())
                        .map_err(|error| format!("delete kv_meta {key}: {error}"))
                },
                |error| error,
            );
        }

        Err("runtime supervisor postgres backend is unavailable".to_string())
    }

    pub fn emit_signal(
        &self,
        signal: SupervisorSignal,
        evidence: Value,
    ) -> Result<SupervisorDecision, String> {
        signal.validate_emit_evidence(&evidence)?;
        match signal {
            SupervisorSignal::OrphanCandidate => self.handle_orphan_candidate(evidence),
            other => {
                let (decision, audit) = build_audit_only_decision(other, evidence)?;
                let session_key = decision.session_key.clone();
                let dispatch_id = decision.dispatch_id.clone();
                self.record_decision(
                    other,
                    &audit,
                    SupervisorAction::Escalate,
                    session_key.as_deref(),
                    dispatch_id.as_deref(),
                    false,
                )?;
                Ok(decision)
            }
        }
    }

    fn record_decision(
        &self,
        signal: SupervisorSignal,
        evidence: &Value,
        chosen_action: SupervisorAction,
        session_key: Option<&str>,
        dispatch_id: Option<&str>,
        executed: bool,
    ) -> Result<(), String> {
        if let Some(pool) = self.pg_pool.as_ref() {
            let evidence_json = decision_log_evidence(signal, evidence, chosen_action, executed);
            let signal = signal.to_string();
            let chosen_action = chosen_action.to_string();
            let session_key = session_key.map(str::to_string);
            let dispatch_id = dispatch_id.map(str::to_string);
            return crate::utils::async_bridge::block_on_pg_result(
                pool,
                move |bridge_pool| async move {
                    sqlx::query(
                        "INSERT INTO runtime_decisions
                         (signal, evidence_json, chosen_action, actor, session_key, dispatch_id)
                         VALUES ($1, $2, $3, $4, $5, $6)",
                    )
                    .bind(&signal)
                    // evidence_json is JSONB: a text bind is rejected, so bind the object as JSON.
                    .bind(sqlx::types::Json(&evidence_json))
                    .bind(&chosen_action)
                    .bind(SUPERVISOR_ACTOR)
                    .bind(session_key.as_deref())
                    .bind(dispatch_id.as_deref())
                    .execute(&bridge_pool)
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("record runtime decision: {error}"))
                },
                |error| error,
            );
        }
        Err("runtime supervisor postgres backend is unavailable".to_string())
    }
}

pub fn emit_signal_json(bridge: &BridgeHandle, signal_name: &str, evidence_json: &str) -> String {
    let signal = match SupervisorSignal::try_from(signal_name) {
        Ok(signal) => signal,
        Err(err) => {
            return AppError::bad_request(err)
                .with_code(ErrorCode::Policy)
                .with_operation("emit_signal_json.parse_signal")
                .with_context("signal_name", signal_name)
                .into_policy_json_string();
        }
    };
    let evidence: Value = match serde_json::from_str(evidence_json) {
        Ok(value) => value,
        Err(err) => {
            return AppError::bad_request(format!("invalid evidence_json: {err}"))
                .with_code(ErrorCode::Policy)
                .with_operation("emit_signal_json.parse_evidence")
                .with_context("signal_name", signal_name)
                .into_policy_json_string();
        }
    };
    if let Err(err) = signal.validate_emit_evidence(&evidence) {
        return AppError::bad_request(err)
            .with_code(ErrorCode::Policy)
            .with_operation("emit_signal_json.signal_support")
            .with_context("signal_name", signal_name)
            .into_policy_json_string();
    }
    let supervisor = match bridge.runtime_supervisor() {
        Ok(supervisor) => supervisor,
        Err(err) => {
            return AppError::internal(err)
                .with_code(ErrorCode::Policy)
                .with_operation("emit_signal_json.runtime_supervisor")
                .with_context("signal_name", signal_name)
                .into_policy_json_string();
        }
    };
    match supervisor.emit_signal(signal, evidence) {
        Ok(decision) => serde_json::to_string(&decision).unwrap_or_else(|err| {
            AppError::internal(format!("serialize decision: {err}"))
                .with_code(ErrorCode::Policy)
                .with_operation("emit_signal_json.serialize_decision")
                .with_context("signal_name", signal_name)
                .into_policy_json_string()
        }),
        Err(err) => AppError::internal(err)
            .with_code(ErrorCode::Policy)
            .with_operation("emit_signal_json.emit_signal")
            .with_context("signal_name", signal_name)
            .into_policy_json_string(),
    }
}

pub fn validate_signal_json(signal_name: &str, evidence_json: &str) -> String {
    let signal = match SupervisorSignal::try_from(signal_name) {
        Ok(signal) => signal,
        Err(err) => {
            return AppError::bad_request(err)
                .with_code(ErrorCode::Policy)
                .with_operation("validate_signal_json.parse_signal")
                .with_context("signal_name", signal_name)
                .into_policy_json_string();
        }
    };
    let evidence: Value = match serde_json::from_str(evidence_json) {
        Ok(value) => value,
        Err(err) => {
            return AppError::bad_request(format!("invalid evidence_json: {err}"))
                .with_code(ErrorCode::Policy)
                .with_operation("validate_signal_json.parse_evidence")
                .with_context("signal_name", signal_name)
                .into_policy_json_string();
        }
    };
    if let Err(err) = signal.validate_emit_evidence(&evidence) {
        return AppError::bad_request(err)
            .with_code(ErrorCode::Policy)
            .with_operation("validate_signal_json.signal_support")
            .with_context("signal_name", signal_name)
            .into_policy_json_string();
    }

    json!({
        "ok": true,
        "signal": signal.to_string(),
        "support_state": signal.support_state(),
    })
    .to_string()
}

fn wrap_audit_evidence(
    evidence: Value,
    note: Option<String>,
    review_target: Option<String>,
) -> Value {
    let mut payload = if evidence.is_object() {
        evidence
    } else {
        json!({ "input": evidence })
    };
    if let Some(obj) = payload.as_object_mut() {
        if let Some(note) = note {
            obj.insert("supervisor_note".to_string(), json!(note));
        }
        if let Some(review_target) = review_target {
            obj.insert("review_target".to_string(), json!(review_target));
        }
    }
    payload
}

fn decision_log_evidence(
    signal: SupervisorSignal,
    evidence: &Value,
    chosen_action: SupervisorAction,
    executed: bool,
) -> Value {
    let mut payload = if evidence.is_object() {
        evidence.clone()
    } else {
        json!({ "input": evidence })
    };
    if let Some(obj) = payload.as_object_mut() {
        obj.insert(
            "supervisor_signal_support".to_string(),
            json!(signal.support_state()),
        );
        obj.insert("supervisor_action_executed".to_string(), json!(executed));
        obj.insert(
            "supervisor_chosen_action".to_string(),
            json!(chosen_action.to_string()),
        );
    }
    payload
}

fn build_audit_only_decision(
    signal: SupervisorSignal,
    evidence: Value,
) -> Result<(SupervisorDecision, Value), String> {
    signal.validate_emit_evidence(&evidence)?;
    let note = Some(format!(
        "{signal} is audit-only; no runtime recovery action executed"
    ));
    let audit = wrap_audit_evidence(evidence, note.clone(), None);
    let session_key = extract_str(&audit, "session_key");
    let dispatch_id = extract_str(&audit, "dispatch_id");
    Ok((
        SupervisorDecision {
            signal,
            support_state: signal.support_state(),
            chosen_action: SupervisorAction::Escalate,
            actor: SUPERVISOR_ACTOR,
            session_key,
            dispatch_id,
            executed: false,
            note,
        },
        audit,
    ))
}

fn extract_required_str(evidence: &Value, key: &str) -> Result<String, String> {
    evidence
        .get(key)
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("missing required evidence field: {key}"))
}

fn extract_str(evidence: &Value, key: &str) -> Option<String> {
    evidence
        .get(key)
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned)
}

impl TryFrom<&str> for SupervisorSignal {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "DeadlockCandidate" => Ok(Self::DeadlockCandidate),
            "OrphanCandidate" => Ok(Self::OrphanCandidate),
            "ResumeCandidate" => Ok(Self::ResumeCandidate),
            "StaleInflight" => Ok(Self::StaleInflight),
            other => Err(format!("unknown supervisor signal: {other}")),
        }
    }
}

impl SupervisorSignal {
    pub const fn support_state(self) -> SupervisorSignalSupport {
        match self {
            Self::OrphanCandidate => SupervisorSignalSupport::ImplementedAction,
            Self::DeadlockCandidate | Self::ResumeCandidate | Self::StaleInflight => {
                SupervisorSignalSupport::AuditOnly
            }
        }
    }

    pub fn validate_emit_evidence(self, evidence: &Value) -> Result<(), String> {
        match self.support_state() {
            SupervisorSignalSupport::ImplementedAction => Ok(()),
            SupervisorSignalSupport::AuditOnly => {
                if evidence
                    .get(AUDIT_ONLY_ACK_FIELD)
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    Ok(())
                } else {
                    Err(format!(
                        "supervisor signal {self} is audit-only; set {AUDIT_ONLY_ACK_FIELD}=true to record Escalate without runtime recovery"
                    ))
                }
            }
            SupervisorSignalSupport::Reserved => Err(format!(
                "supervisor signal {self} is reserved and cannot be emitted"
            )),
        }
    }
}

impl std::fmt::Display for SupervisorSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeadlockCandidate => write!(f, "DeadlockCandidate"),
            Self::OrphanCandidate => write!(f, "OrphanCandidate"),
            Self::ResumeCandidate => write!(f, "ResumeCandidate"),
            Self::StaleInflight => write!(f, "StaleInflight"),
        }
    }
}

impl std::fmt::Display for SupervisorAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Probe => write!(f, "Probe"),
            Self::Reconnect => write!(f, "Reconnect"),
            Self::Fail => write!(f, "Fail"),
            Self::Redispatch => write!(f, "Redispatch"),
            Self::Resume => write!(f, "Resume"),
            Self::Escalate => write!(f, "Escalate"),
        }
    }
}

#[cfg(test)]
mod decision_log_pg_tests;

#[cfg(test)]
mod tests {
    use super::orphan_recovery::orphan_ready_clock_assignment_sql;
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn orphan_recovery_returns_card_to_dispatchable_without_notification_pg() {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        sqlx::query("INSERT INTO kanban_cards (id, title, status, latest_dispatch_id) VALUES ('orphan-card', '고아 작업', 'in_progress', 'orphan-dispatch')")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO task_dispatches (id, kanban_card_id, dispatch_type, status, created_at) VALUES ('orphan-dispatch', 'orphan-card', 'implementation', 'pending', NOW() - INTERVAL '10 minutes')")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO kv_meta (key, value) VALUES ('kanban_manager_channel_id', '1234567890123456789')")
            .execute(&pool).await.unwrap();
        let supervisor = RuntimeSupervisor {
            pg_pool: Some(pool.clone()),
        };
        let evidence = json!({ "dispatch_id": "orphan-dispatch" });
        let probe = supervisor.emit_signal(SupervisorSignal::OrphanCandidate, evidence.clone());
        let status: String =
            sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = 'orphan-dispatch'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            status, "pending",
            "first signal must only confirm: {probe:?}"
        );
        let recovered = supervisor.emit_signal(SupervisorSignal::OrphanCandidate, evidence);
        let status: String =
            sqlx::query_scalar("SELECT status FROM task_dispatches WHERE id = 'orphan-dispatch'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            status, "failed",
            "recovery must fail the dispatch: {recovered:?}"
        );
        let card: (String, Option<String>) = sqlx::query_as(
            "SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = 'orphan-card'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(card, ("requested".into(), Some("orphan-dispatch".into())));
        let audit: (String, String) = sqlx::query_as("SELECT from_status, to_status FROM kanban_audit_logs WHERE card_id = 'orphan-card' AND source = 'runtime_supervisor'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(audit, ("in_progress".into(), "requested".into()));
        let notifications: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            notifications, 0,
            "orphan recovery must not enqueue notifications"
        );
        pool.close().await;
        db.drop().await;
    }

    #[test]
    fn supervisor_signal_support_states_are_explicit() {
        assert_eq!(
            SupervisorSignal::OrphanCandidate.support_state(),
            SupervisorSignalSupport::ImplementedAction
        );
        for signal in [
            SupervisorSignal::DeadlockCandidate,
            SupervisorSignal::ResumeCandidate,
            SupervisorSignal::StaleInflight,
        ] {
            assert_eq!(signal.support_state(), SupervisorSignalSupport::AuditOnly);
        }
    }

    #[test]
    fn audit_only_signals_require_explicit_acknowledgement() {
        for signal in [
            SupervisorSignal::DeadlockCandidate,
            SupervisorSignal::ResumeCandidate,
            SupervisorSignal::StaleInflight,
        ] {
            let err = signal
                .validate_emit_evidence(&json!({ "session_key": "s1" }))
                .expect_err("audit-only signal should require an explicit acknowledgement");
            assert!(err.contains("audit-only"));
            assert!(err.contains(AUDIT_ONLY_ACK_FIELD));

            signal
                .validate_emit_evidence(&json!({
                    "session_key": "s1",
                    "supervisor_audit_only": true,
                }))
                .expect("acknowledged audit-only signal should validate");
        }

        SupervisorSignal::OrphanCandidate
            .validate_emit_evidence(&json!({ "dispatch_id": "dispatch-1" }))
            .expect("implemented signal should not require audit-only acknowledgement");
    }

    #[test]
    fn audit_only_decisions_escalate_without_execution() {
        for signal in [
            SupervisorSignal::DeadlockCandidate,
            SupervisorSignal::ResumeCandidate,
            SupervisorSignal::StaleInflight,
        ] {
            let (decision, audit) = build_audit_only_decision(
                signal,
                json!({
                    "session_key": "session-1",
                    "dispatch_id": "dispatch-1",
                    "supervisor_audit_only": true,
                }),
            )
            .expect("audit-only decision should build");

            assert_eq!(decision.signal, signal);
            assert_eq!(decision.support_state, SupervisorSignalSupport::AuditOnly);
            assert_eq!(decision.chosen_action, SupervisorAction::Escalate);
            assert!(!decision.executed);
            assert_eq!(decision.session_key.as_deref(), Some("session-1"));
            assert_eq!(decision.dispatch_id.as_deref(), Some("dispatch-1"));
            assert!(
                decision
                    .note
                    .as_deref()
                    .is_some_and(|note| note.contains("no runtime recovery action executed"))
            );

            let persisted =
                decision_log_evidence(signal, &audit, decision.chosen_action, decision.executed);
            assert_eq!(persisted["supervisor_signal_support"], json!("audit_only"));
            assert_eq!(persisted["supervisor_action_executed"], json!(false));
            assert_eq!(persisted["supervisor_chosen_action"], json!("Escalate"));
            assert!(
                persisted["supervisor_note"]
                    .as_str()
                    .is_some_and(|note| note.contains("audit-only"))
            );
        }
    }

    #[test]
    fn emit_signal_json_rejects_audit_only_signal_before_bridge_lookup() {
        let bridge = BridgeHandle::new();
        let response = emit_signal_json(&bridge, "DeadlockCandidate", r#"{"session_key":"s1"}"#);

        assert!(response.contains("audit-only"));
        assert!(response.contains("emit_signal_json.signal_support"));
        assert!(!response.contains("runtime supervisor is not attached"));
    }

    #[test]
    fn orphan_recovery_clock_assignment_matches_transition_semantics() {
        let requested = crate::pipeline::ClockConfig {
            set: "requested_at".to_string(),
            mode: None,
        };
        assert_eq!(
            orphan_ready_clock_assignment_sql(&requested),
            ", requested_at = NOW()"
        );

        let started = crate::pipeline::ClockConfig {
            set: "started_at".to_string(),
            mode: Some("coalesce".to_string()),
        };
        assert_eq!(
            orphan_ready_clock_assignment_sql(&started),
            ", started_at = COALESCE(started_at, NOW())"
        );

        let review_gate = crate::pipeline::ClockConfig {
            set: "awaiting_dod_at".to_string(),
            mode: None,
        };
        assert_eq!(
            orphan_ready_clock_assignment_sql(&review_gate),
            ", awaiting_dod_at = NOW()"
        );
    }
}
