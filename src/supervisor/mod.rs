use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Row as SqlxRow};

use crate::engine::{PolicyEngine, PolicyEngineHandle};
use crate::error::{AppError, ErrorCode};

const SUPERVISOR_ACTOR: &str = "runtime_supervisor";
const ORPHAN_CONFIRM_KEY_PREFIX: &str = "runtime_supervisor:orphan_confirm:";
const ACTIVE_DISPATCH_STATUSES: &[&str] = &["pending", "dispatched"];
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

#[derive(Clone)]
struct OrphanCandidate {
    card_id: String,
    card_status: String,
    title: String,
    assigned_agent_id: Option<String>,
    repo_id: Option<String>,
}

struct ReadyTarget {
    status: String,
    clock: Option<crate::pipeline::ClockConfig>,
}

enum OrphanReadyTransition {
    Transitioned,
    CardMoved {
        status: String,
        latest_dispatch_id: Option<String>,
    },
    CardMissing,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct OrphanConfirmMarker {
    card_id: String,
    card_status: String,
    assigned_agent_id: Option<String>,
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

    fn handle_orphan_candidate(&self, evidence: Value) -> Result<SupervisorDecision, String> {
        let dispatch_id = extract_required_str(&evidence, "dispatch_id")?;
        let session_key = extract_str(&evidence, "session_key");
        let mut note: Option<String> = None;
        let mut chosen_action = SupervisorAction::Probe;
        let mut executed = false;
        let candidate = self.load_orphan_candidate(&dispatch_id)?;

        if let Some(candidate) = candidate {
            if !self.confirm_orphan_candidate(&dispatch_id, &candidate)? {
                note = Some("orphan candidate awaiting confirm".to_string());
            } else {
                chosen_action = SupervisorAction::Resume;

                let Some(pool) = self.pg_pool.as_ref() else {
                    return Err("runtime supervisor postgres backend is unavailable".to_string());
                };
                let repo_id = candidate.repo_id.clone();
                let agent_id = candidate.assigned_agent_id.clone();
                let ready_target = crate::utils::async_bridge::block_on_pg_result(
                    pool,
                    move |bridge_pool| async move {
                        crate::pipeline::ensure_loaded();
                        let effective = crate::pipeline::resolve_for_card_pg(
                            &bridge_pool,
                            repo_id.as_deref(),
                            agent_id.as_deref(),
                        )
                        .await;
                        let status = effective
                            .dispatchable_states()
                            .first()
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| "ready".to_string());
                        Ok::<ReadyTarget, String>(ReadyTarget {
                            clock: effective.clock_for_state(&status).cloned(),
                            status,
                        })
                    },
                    |error| error,
                )?;

                // Orphan recovery: fail the dispatch and return card to ready.
                // The dispatch had no active session, so no work was done.
                // Completing it would falsely advance the card through review → done.
                let fail_result = self.mark_dispatch_failed(&dispatch_id)?;
                if fail_result == 0 {
                    note = Some("dispatch already terminal or missing".to_string());
                    chosen_action = SupervisorAction::Probe;
                } else {
                    executed = true;
                    // Return card to ready for re-dispatch instead of advancing to review
                    match self.return_card_to_ready_if_orphan_head(
                        &candidate,
                        &dispatch_id,
                        &ready_target.status,
                        ready_target.clock.as_ref(),
                    ) {
                        Ok(OrphanReadyTransition::Transitioned) => {
                            self.notify_orphan_recovery(&candidate, &ready_target.status);
                        }
                        Ok(OrphanReadyTransition::CardMoved {
                            status,
                            latest_dispatch_id,
                        }) => {
                            note = Some(format!(
                                "dispatch failed; card moved to status={} latest_dispatch_id={}",
                                status,
                                latest_dispatch_id.unwrap_or_else(|| "null".to_string())
                            ));
                        }
                        Ok(OrphanReadyTransition::CardMissing) => {
                            note = Some(
                                "dispatch failed; card disappeared before transition".to_string(),
                            );
                        }
                        Err(e) => {
                            note = Some(format!(
                                "dispatch failed; resume transition status unknown: {e}"
                            ));
                        }
                    }
                }
            }
        } else {
            self.clear_orphan_confirmation(&dispatch_id);
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
            executed,
        )?;

        Ok(SupervisorDecision {
            signal: SupervisorSignal::OrphanCandidate,
            support_state: SupervisorSignal::OrphanCandidate.support_state(),
            chosen_action,
            actor: SUPERVISOR_ACTOR,
            session_key,
            dispatch_id: Some(dispatch_id),
            executed,
            note,
        })
    }

    fn load_orphan_candidate(&self, dispatch_id: &str) -> Result<Option<OrphanCandidate>, String> {
        if let Some(pool) = self.pg_pool.as_ref() {
            let dispatch_id = dispatch_id.to_string();
            return crate::utils::async_bridge::block_on_pg_result(
                pool,
                move |bridge_pool| async move {
                    let row = sqlx::query(
                        "SELECT td.kanban_card_id,
                                kc.status,
                                kc.title,
                                kc.assigned_agent_id,
                                kc.repo_id
                         FROM task_dispatches td
                         JOIN kanban_cards kc ON kc.id = td.kanban_card_id
                         WHERE td.id = $1
                           AND td.status = 'pending'
                           AND kc.latest_dispatch_id = td.id
                           AND td.dispatch_type IN ('implementation', 'rework')
                           AND td.created_at < NOW() - INTERVAL '5 minutes'
                           AND NOT EXISTS (
                             SELECT 1 FROM sessions s
                             WHERE s.active_dispatch_id = td.id AND s.status IN ('turn_active', 'working')
                           )",
                    )
                    .bind(&dispatch_id)
                    .fetch_optional(&bridge_pool)
                    .await
                    .map_err(|error| format!("load orphan candidate {dispatch_id}: {error}"))?;
                    Ok(row.map(|row| OrphanCandidate {
                        card_id: row.try_get("kanban_card_id").unwrap_or_default(),
                        card_status: row.try_get("status").unwrap_or_default(),
                        title: row.try_get("title").unwrap_or_default(),
                        assigned_agent_id: row.try_get("assigned_agent_id").ok().flatten(),
                        repo_id: row.try_get("repo_id").ok().flatten(),
                    }))
                },
                |error| error,
            );
        }
        Err("runtime supervisor postgres backend is unavailable".to_string())
    }

    fn clear_orphan_confirmation(&self, dispatch_id: &str) {
        let key = format!("{ORPHAN_CONFIRM_KEY_PREFIX}{dispatch_id}");
        let _ = self.kv_delete(&key);
    }

    fn load_orphan_confirmation(&self, dispatch_id: &str) -> Option<OrphanConfirmMarker> {
        let key = format!("{ORPHAN_CONFIRM_KEY_PREFIX}{dispatch_id}");
        match self.kv_get(&key) {
            Ok(raw) => {
                raw.and_then(|value| serde_json::from_str::<OrphanConfirmMarker>(&value).ok())
            }
            Err(error) => {
                tracing::warn!("failed to load orphan confirmation: {error}");
                None
            }
        }
    }

    fn confirm_orphan_candidate(
        &self,
        dispatch_id: &str,
        candidate: &OrphanCandidate,
    ) -> Result<bool, String> {
        let marker = OrphanConfirmMarker {
            card_id: candidate.card_id.clone(),
            card_status: candidate.card_status.clone(),
            assigned_agent_id: candidate.assigned_agent_id.clone(),
        };

        if self.load_orphan_confirmation(dispatch_id).as_ref() == Some(&marker) {
            self.clear_orphan_confirmation(dispatch_id);
            return Ok(true);
        }

        let key = format!("{ORPHAN_CONFIRM_KEY_PREFIX}{dispatch_id}");
        let marker_json =
            serde_json::to_string(&marker).map_err(|e| format!("serialize orphan marker: {e}"))?;
        self.kv_set(&key, &marker_json)
            .map_err(|error| format!("store orphan marker {dispatch_id}: {error}"))?;
        Ok(false)
    }

    #[allow(dead_code)]
    fn mark_dispatch_completed(&self, dispatch_id: &str) -> Result<usize, String> {
        let result = json!({
            "auto_completed": true,
            "completion_source": "orphan_recovery"
        });
        let Some(pool) = self.pg_pool.as_ref() else {
            return Err("runtime supervisor postgres backend is unavailable".to_string());
        };
        let dispatch_id = dispatch_id.to_string();
        crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |bridge_pool| async move {
                crate::dispatch::set_dispatch_status_on_pg_async(
                    &bridge_pool,
                    &dispatch_id,
                    "completed",
                    Some(&result),
                    "orphan_recovery",
                    Some(ACTIVE_DISPATCH_STATUSES),
                    true,
                )
                .await
                .map_err(|error| format!("mark dispatch completed {dispatch_id}: {error}"))
            },
            |error| error,
        )
    }

    fn mark_dispatch_failed(&self, dispatch_id: &str) -> Result<usize, String> {
        let result = json!({
            "orphan_failed": true,
            "completion_source": "orphan_recovery_rollback"
        });
        let Some(pool) = self.pg_pool.as_ref() else {
            return Err("runtime supervisor postgres backend is unavailable".to_string());
        };
        let dispatch_id = dispatch_id.to_string();
        crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |bridge_pool| async move {
                crate::dispatch::set_dispatch_status_on_pg_async(
                    &bridge_pool,
                    &dispatch_id,
                    "failed",
                    Some(&result),
                    "orphan_recovery_rollback",
                    Some(ACTIVE_DISPATCH_STATUSES),
                    false,
                )
                .await
                .map_err(|error| format!("mark dispatch failed {dispatch_id}: {error}"))
            },
            |error| error,
        )
    }

    fn return_card_to_ready_if_orphan_head(
        &self,
        candidate: &OrphanCandidate,
        dispatch_id: &str,
        ready_target: &str,
        ready_clock: Option<&crate::pipeline::ClockConfig>,
    ) -> Result<OrphanReadyTransition, String> {
        let Some(pool) = self.pg_pool.as_ref() else {
            return Err("runtime supervisor postgres backend is unavailable".to_string());
        };
        let card_id = candidate.card_id.clone();
        let expected_status = candidate.card_status.clone();
        let dispatch_id = dispatch_id.to_string();
        let ready_target = ready_target.to_string();
        let clock_assignment = ready_clock
            .map(orphan_ready_clock_assignment_sql)
            .unwrap_or_default();
        crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |bridge_pool| async move {
                let mut tx = bridge_pool
                    .begin()
                    .await
                    .map_err(|error| format!("begin orphan ready transition tx: {error}"))?;
                let update_sql = format!(
                    "UPDATE kanban_cards
                     SET status = $1, updated_at = NOW(){clock_assignment}
                     WHERE id = $2
                       AND status = $3
                       AND latest_dispatch_id = $4"
                );
                let update = sqlx::query(&update_sql)
                    .bind(&ready_target)
                    .bind(&card_id)
                    .bind(&expected_status)
                    .bind(&dispatch_id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|error| {
                        format!("conditionally return orphan card {card_id}: {error}")
                    })?;

                if update.rows_affected() == 1 {
                    sqlx::query(
                        "INSERT INTO kanban_audit_logs (
                            card_id, from_status, to_status, source, result
                         )
                         VALUES ($1, $2, $3, $4, $5)",
                    )
                    .bind(&card_id)
                    .bind(&expected_status)
                    .bind(&ready_target)
                    .bind(SUPERVISOR_ACTOR)
                    .bind(format!(
                        "orphan recovery failed dispatch {dispatch_id}; returned card to {ready_target}"
                    ))
                    .execute(&mut *tx)
                    .await
                    .map_err(|error| format!("audit orphan ready transition {card_id}: {error}"))?;
                    tx.commit()
                        .await
                        .map_err(|error| format!("commit orphan ready transition tx: {error}"))?;
                    return Ok(OrphanReadyTransition::Transitioned);
                }

                let row = sqlx::query(
                    "SELECT status, latest_dispatch_id
                     FROM kanban_cards
                     WHERE id = $1",
                )
                .bind(&card_id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|error| format!("reload orphan card head {card_id}: {error}"))?;
                tx.commit()
                    .await
                    .map_err(|error| format!("commit orphan ready skip tx: {error}"))?;

                Ok(row
                    .map(|row| OrphanReadyTransition::CardMoved {
                        status: row.try_get("status").unwrap_or_default(),
                        latest_dispatch_id: row
                            .try_get::<Option<String>, _>("latest_dispatch_id")
                            .ok()
                            .flatten(),
                    })
                    .unwrap_or(OrphanReadyTransition::CardMissing))
            },
            |error| error,
        )
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
            let evidence_json =
                decision_log_evidence(signal, evidence, chosen_action, executed).to_string();
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
                    .bind(&evidence_json)
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

    fn notify_orphan_recovery(&self, candidate: &OrphanCandidate, review_target: &str) {
        let channel_id: Option<String> = match self.kv_get("kanban_manager_channel_id") {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!("failed to load orphan recovery channel id: {error}");
                None
            }
        };
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
        if let Some(pool) = self.pg_pool.as_ref() {
            let channel_id = channel_id.clone();
            let content = content.clone();
            let card_id = candidate.card_id.clone();
            let _ = crate::utils::async_bridge::block_on_pg_result(
                pool,
                move |bridge_pool| async move {
                    crate::services::message_outbox::enqueue_lifecycle_notification_pg(
                        &bridge_pool,
                        &format!("channel:{channel_id}"),
                        Some(&card_id),
                        "lifecycle.orphan_recovery",
                        &content,
                    )
                    .await
                    .map(|_| ())
                    .map_err(|error| format!("enqueue orphan recovery notification: {error}"))
                },
                |error| error,
            );
            return;
        }
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

fn orphan_ready_clock_assignment_sql(clock: &crate::pipeline::ClockConfig) -> String {
    if clock.mode.as_deref() == Some("coalesce") {
        format!(", {field} = COALESCE({field}, NOW())", field = clock.set)
    } else {
        format!(", {} = NOW()", clock.set)
    }
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
mod tests {
    use super::*;
    use serde_json::json;

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
