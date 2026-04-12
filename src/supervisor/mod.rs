use std::sync::{Arc, Mutex};

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::db::Db;
use crate::engine::{PolicyEngine, PolicyEngineHandle};
use crate::error::{AppError, ErrorCode};

const SUPERVISOR_ACTOR: &str = "runtime_supervisor";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SupervisorSignal {
    DeadlockCandidate,
    OrphanCandidate,
    ResumeCandidate,
    StaleInflight,
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

    fn runtime_supervisor(&self, db: Db) -> Result<RuntimeSupervisor, String> {
        let engine = self.upgrade_engine()?;
        Ok(RuntimeSupervisor::new(db, engine))
    }
}

#[derive(Clone)]
pub struct RuntimeSupervisor {
    db: Db,
    engine: PolicyEngine,
}

struct OrphanCandidate {
    card_id: String,
    card_status: String,
    title: String,
    assigned_agent_id: Option<String>,
    repo_id: Option<String>,
}

impl RuntimeSupervisor {
    pub fn new(db: Db, engine: PolicyEngine) -> Self {
        Self { db, engine }
    }

    pub fn emit_signal(
        &self,
        signal: SupervisorSignal,
        evidence: Value,
    ) -> Result<SupervisorDecision, String> {
        match signal {
            SupervisorSignal::OrphanCandidate => self.handle_orphan_candidate(evidence),
            other => {
                let audit = wrap_audit_evidence(
                    evidence,
                    Some("signal not implemented yet".to_string()),
                    None,
                );
                let session_key = extract_str(&audit, "session_key");
                let dispatch_id = extract_str(&audit, "dispatch_id");
                self.record_decision(
                    other,
                    &audit,
                    SupervisorAction::Escalate,
                    session_key.as_deref(),
                    dispatch_id.as_deref(),
                )?;
                Ok(SupervisorDecision {
                    signal: other,
                    chosen_action: SupervisorAction::Escalate,
                    actor: SUPERVISOR_ACTOR,
                    session_key,
                    dispatch_id,
                    executed: false,
                    note: extract_str(&audit, "supervisor_note"),
                })
            }
        }
    }

    fn handle_orphan_candidate(&self, evidence: Value) -> Result<SupervisorDecision, String> {
        let dispatch_id = extract_required_str(&evidence, "dispatch_id")?;
        let session_key = extract_str(&evidence, "session_key");
        let mut note: Option<String> = None;
        let mut chosen_action = SupervisorAction::Probe;
        let mut executed = false;
        let candidate = self.load_orphan_candidate(&dispatch_id)?;

        if let Some(candidate) = candidate {
            chosen_action = SupervisorAction::Resume;

            // Orphan recovery: fail the dispatch and return card to ready.
            // The dispatch had no active session, so no work was done.
            // Completing it would falsely advance the card through review → done.
            let fail_result = self.mark_dispatch_failed(&dispatch_id)?;
            if fail_result == 0 {
                note = Some("dispatch already terminal or missing".to_string());
                chosen_action = SupervisorAction::Probe;
            } else {
                #[cfg(test)]
                self.apply_orphan_fault_injection(&dispatch_id, &candidate.card_id);

                // Return card to ready for re-dispatch instead of advancing to review
                let ready_target = {
                    let conn = self
                        .db
                        .separate_conn()
                        .map_err(|e| format!("db connection: {e}"))?;
                    crate::pipeline::ensure_loaded();
                    let effective = crate::pipeline::resolve_for_card(
                        &conn,
                        candidate.repo_id.as_deref(),
                        candidate.assigned_agent_id.as_deref(),
                    );
                    // Use the dispatchable state (ready) as target
                    effective
                        .dispatchable_states()
                        .first()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| "ready".to_string())
                };

                let current = self.current_card_head(&candidate.card_id)?;
                if current
                    .as_ref()
                    .is_some_and(|(status, latest_dispatch_id)| {
                        status == &candidate.card_status
                            && latest_dispatch_id.as_deref() == Some(dispatch_id.as_str())
                    })
                {
                    match crate::kanban::transition_status_with_opts(
                        &self.db,
                        &self.engine,
                        &candidate.card_id,
                        &ready_target,
                        SUPERVISOR_ACTOR,
                        true,
                    ) {
                        Ok(_) => {
                            executed = true;
                            self.notify_orphan_recovery(&candidate, &ready_target);
                        }
                        Err(e) => {
                            note = Some(format!("resume transition skipped: {e}"));
                        }
                    }
                } else {
                    let moved = current
                        .map(|(status, latest_dispatch_id)| {
                            format!(
                                "card moved to status={} latest_dispatch_id={}",
                                status,
                                latest_dispatch_id.unwrap_or_else(|| "null".to_string())
                            )
                        })
                        .unwrap_or_else(|| "card disappeared before transition".to_string());
                    note = Some(moved);
                }
            }
        } else {
            note = Some("stale or non-orphan candidate".to_string());
        }

        let mut audit = wrap_audit_evidence(evidence, note.clone(), None);
        if let Some(dispatch_id_value) = audit.get("dispatch_id").and_then(|v| v.as_str()) {
            audit["dispatch_id"] = json!(dispatch_id_value);
        }
        self.record_decision(
            SupervisorSignal::OrphanCandidate,
            &audit,
            chosen_action,
            session_key.as_deref(),
            Some(dispatch_id.as_str()),
        )?;

        Ok(SupervisorDecision {
            signal: SupervisorSignal::OrphanCandidate,
            chosen_action,
            actor: SUPERVISOR_ACTOR,
            session_key,
            dispatch_id: Some(dispatch_id),
            executed,
            note,
        })
    }

    fn load_orphan_candidate(&self, dispatch_id: &str) -> Result<Option<OrphanCandidate>, String> {
        let conn = self
            .db
            .separate_conn()
            .map_err(|e| format!("db connection: {e}"))?;
        conn.query_row(
            "SELECT td.kanban_card_id,
                    kc.status,
                    kc.title,
                    kc.assigned_agent_id,
                    kc.repo_id
             FROM task_dispatches td
             JOIN kanban_cards kc ON kc.id = td.kanban_card_id
             WHERE td.id = ?1
               AND td.status = 'pending'
               AND kc.latest_dispatch_id = td.id
               AND td.dispatch_type IN ('implementation', 'rework')
               AND td.created_at < datetime('now', '-5 minutes')
               AND NOT EXISTS (
                 SELECT 1 FROM sessions s
                 WHERE s.active_dispatch_id = td.id AND s.status = 'working'
               )",
            [dispatch_id],
            |row| {
                Ok(OrphanCandidate {
                    card_id: row.get(0)?,
                    card_status: row.get(1)?,
                    title: row.get(2)?,
                    assigned_agent_id: row.get(3)?,
                    repo_id: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(|e| format!("load orphan candidate: {e}"))
    }

    #[allow(dead_code)]
    fn mark_dispatch_completed(&self, dispatch_id: &str) -> Result<usize, String> {
        let conn = self
            .db
            .separate_conn()
            .map_err(|e| format!("db connection: {e}"))?;
        crate::dispatch::set_dispatch_status_on_conn(
            &conn,
            dispatch_id,
            "completed",
            Some(&json!({
                "auto_completed": true,
                "completion_source": "orphan_recovery"
            })),
            "orphan_recovery",
            Some(&["pending", "dispatched"]),
            true,
        )
        .map_err(|e| format!("mark dispatch completed: {e}"))
    }

    fn mark_dispatch_failed(&self, dispatch_id: &str) -> Result<usize, String> {
        let conn = self
            .db
            .separate_conn()
            .map_err(|e| format!("db connection: {e}"))?;
        crate::dispatch::set_dispatch_status_on_conn(
            &conn,
            dispatch_id,
            "failed",
            Some(&json!({
                "orphan_failed": true,
                "completion_source": "orphan_recovery_rollback"
            })),
            "orphan_recovery_rollback",
            Some(&["pending", "dispatched"]),
            false,
        )
        .map_err(|e| format!("mark dispatch failed: {e}"))
    }

    fn current_card_head(&self, card_id: &str) -> Result<Option<(String, Option<String>)>, String> {
        let conn = self
            .db
            .separate_conn()
            .map_err(|e| format!("db connection: {e}"))?;
        conn.query_row(
            "SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = ?1",
            [card_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|e| format!("current card head: {e}"))
    }

    fn record_decision(
        &self,
        signal: SupervisorSignal,
        evidence: &Value,
        chosen_action: SupervisorAction,
        session_key: Option<&str>,
        dispatch_id: Option<&str>,
    ) -> Result<(), String> {
        let conn = self
            .db
            .separate_conn()
            .map_err(|e| format!("db connection: {e}"))?;
        conn.execute(
            "INSERT INTO runtime_decisions
             (signal, evidence_json, chosen_action, actor, session_key, dispatch_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                signal.to_string(),
                evidence.to_string(),
                chosen_action.to_string(),
                SUPERVISOR_ACTOR,
                session_key,
                dispatch_id,
            ],
        )
        .map_err(|e| format!("record runtime decision: {e}"))?;
        Ok(())
    }

    fn notify_orphan_recovery(&self, candidate: &OrphanCandidate, review_target: &str) {
        let Ok(conn) = self.db.separate_conn() else {
            return;
        };
        let channel_id: Option<String> = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'kanban_manager_channel_id'",
                [],
                |row| row.get(0),
            )
            .ok();
        let Some(channel_id) = channel_id.filter(|id| !id.trim().is_empty()) else {
            return;
        };
        let agent = candidate
            .assigned_agent_id
            .as_deref()
            .unwrap_or("?")
            .to_string();
        let content = format!(
            "🔄 [고아 디스패치 복구] {} — {}\n사유: pending 디스패치 5분 경과 + 활성 세션 없음 → {} 전이",
            agent, candidate.title, review_target
        );
        let _ = conn.execute(
            "INSERT INTO message_outbox (target, content, bot, source)
             VALUES (?1, ?2, 'announce', 'system')",
            rusqlite::params![format!("channel:{channel_id}"), content],
        );
    }

    #[cfg(test)]
    fn apply_orphan_fault_injection(&self, dispatch_id: &str, card_id: &str) {
        let Ok(conn) = self.db.separate_conn() else {
            return;
        };
        let key = format!("test:runtime_supervisor:orphan_post_complete_override:{dispatch_id}");
        let override_status: Option<String> = conn
            .query_row("SELECT value FROM kv_meta WHERE key = ?1", [&key], |row| {
                row.get(0)
            })
            .optional()
            .ok()
            .flatten();
        let Some(override_status) = override_status else {
            return;
        };
        conn.execute("DELETE FROM kv_meta WHERE key = ?1", [&key])
            .ok();
        conn.execute(
            "UPDATE kanban_cards SET status = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![override_status, card_id],
        )
        .ok();
    }
}

pub fn emit_signal_json(
    db: &Db,
    bridge: &BridgeHandle,
    signal_name: &str,
    evidence_json: &str,
) -> String {
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
    let supervisor = match bridge.runtime_supervisor(db.clone()) {
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
mod tests {
    use super::*;

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    #[test]
    fn emit_signal_json_returns_unified_policy_error_for_invalid_signal() {
        let db = test_db();
        let bridge = BridgeHandle::new();

        let value: Value =
            serde_json::from_str(&emit_signal_json(&db, &bridge, "Nope", r#"{}"#)).unwrap();

        assert_eq!(value["ok"], false);
        assert_eq!(value["code"], "policy");
        assert_eq!(
            value["context"]["operation"],
            "emit_signal_json.parse_signal"
        );
        assert!(
            value["error"]
                .as_str()
                .unwrap_or("")
                .contains("unknown supervisor signal")
        );
    }
}
