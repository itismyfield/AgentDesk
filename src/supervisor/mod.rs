use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Row as SqlxRow};

use crate::engine::{PolicyEngine, PolicyEngineHandle};
use crate::error::{AppError, ErrorCode};

const SUPERVISOR_ACTOR: &str = "runtime_supervisor";
const ORPHAN_CONFIRM_KEY_PREFIX: &str = "runtime_supervisor:orphan_confirm:";

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

    fn runtime_supervisor(&self) -> Result<RuntimeSupervisor, String> {
        let engine = self.upgrade_engine()?;
        Ok(RuntimeSupervisor::new(engine.pg_pool().cloned(), engine))
    }
}

#[derive(Clone)]
pub struct RuntimeSupervisor {
    pg_pool: Option<PgPool>,
    engine: PolicyEngine,
}

#[derive(Clone)]
struct OrphanCandidate {
    card_id: String,
    card_status: String,
    title: String,
    assigned_agent_id: Option<String>,
    repo_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct OrphanConfirmMarker {
    card_id: String,
    card_status: String,
    assigned_agent_id: Option<String>,
}

impl RuntimeSupervisor {
    pub fn new(pg_pool: Option<PgPool>, engine: PolicyEngine) -> Self {
        Self { pg_pool, engine }
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
            if !self.confirm_orphan_candidate(&dispatch_id, &candidate)? {
                note = Some("orphan candidate awaiting confirm".to_string());
            } else {
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
                    let Some(pool) = self.pg_pool.as_ref() else {
                        return Err(
                            "runtime supervisor postgres backend is unavailable".to_string()
                        );
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
                            Ok::<String, String>(
                                effective
                                    .dispatchable_states()
                                    .first()
                                    .map(|s| s.to_string())
                                    .unwrap_or_else(|| "ready".to_string()),
                            )
                        },
                        |error| error,
                    )?;

                    let current = self.current_card_head(&candidate.card_id)?;
                    if current
                        .as_ref()
                        .is_some_and(|(status, latest_dispatch_id)| {
                            status == &candidate.card_status
                                && latest_dispatch_id.as_deref() == Some(dispatch_id.as_str())
                        })
                    {
                        let engine = self.engine.clone();
                        let card_id = candidate.card_id.clone();
                        let transition_target = ready_target.clone();
                        let transition_result = crate::utils::async_bridge::block_on_pg_result(
                            pool,
                            move |bridge_pool| async move {
                                crate::kanban::transition_status_with_opts_pg(
                                    None,
                                    &bridge_pool,
                                    &engine,
                                    &card_id,
                                    &transition_target,
                                    SUPERVISOR_ACTOR,
                                    crate::engine::transition::ForceIntent::SystemRecovery,
                                )
                                .await
                                .map(|_| ())
                                .map_err(|error| error.to_string())
                            },
                            |error| error,
                        );
                        match transition_result {
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
                let payload = result.to_string();
                let changed = sqlx::query(
                    "UPDATE task_dispatches
                     SET status = 'completed',
                         result = $1,
                         updated_at = NOW(),
                         completed_at = COALESCE(completed_at, NOW())
                     WHERE id = $2
                       AND status IN ('pending', 'dispatched')",
                )
                .bind(&payload)
                .bind(&dispatch_id)
                .execute(&bridge_pool)
                .await
                .map_err(|error| format!("mark dispatch completed {dispatch_id}: {error}"))?
                .rows_affected() as usize;
                Ok(changed)
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
                let payload = result.to_string();
                let current = sqlx::query(
                    "SELECT status, kanban_card_id, dispatch_type
                     FROM task_dispatches
                     WHERE id = $1",
                )
                .bind(&dispatch_id)
                .fetch_optional(&bridge_pool)
                .await
                .map_err(|error| format!("load dispatch {dispatch_id}: {error}"))?;
                let Some(current) = current else {
                    return Ok(0);
                };
                let status = current
                    .try_get::<Option<String>, _>("status")
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                if !matches!(status.as_str(), "pending" | "dispatched") {
                    return Ok(0);
                }
                let changed = sqlx::query(
                    "UPDATE task_dispatches
                     SET status = 'failed',
                         result = $1,
                         updated_at = NOW()
                     WHERE id = $2
                       AND status = $3",
                )
                .bind(&payload)
                .bind(&dispatch_id)
                .bind(&status)
                .execute(&bridge_pool)
                .await
                .map_err(|error| format!("mark dispatch failed {dispatch_id}: {error}"))?
                .rows_affected() as usize;
                if changed == 0 {
                    return Ok(0);
                }
                let _ = sqlx::query(
                    "INSERT INTO dispatch_events (
                        dispatch_id,
                        kanban_card_id,
                        dispatch_type,
                        from_status,
                        to_status,
                        transition_source,
                        payload_json
                     ) VALUES ($1, $2, $3, $4, 'failed', 'orphan_recovery_rollback', $5)",
                )
                .bind(&dispatch_id)
                .bind(
                    current
                        .try_get::<Option<String>, _>("kanban_card_id")
                        .ok()
                        .flatten(),
                )
                .bind(
                    current
                        .try_get::<Option<String>, _>("dispatch_type")
                        .ok()
                        .flatten(),
                )
                .bind(&status)
                .bind(&payload)
                .execute(&bridge_pool)
                .await;
                let _ = sqlx::query(
                    "INSERT INTO dispatch_outbox (dispatch_id, action)
                     SELECT $1, 'status_reaction'
                     WHERE NOT EXISTS (
                         SELECT 1
                         FROM dispatch_outbox
                         WHERE dispatch_id = $1
                           AND action = 'status_reaction'
                           AND status IN ('pending', 'processing')
                     )",
                )
                .bind(&dispatch_id)
                .execute(&bridge_pool)
                .await;
                Ok(changed)
            },
            |error| error,
        )
    }

    fn current_card_head(&self, card_id: &str) -> Result<Option<(String, Option<String>)>, String> {
        if let Some(pool) = self.pg_pool.as_ref() {
            let card_id = card_id.to_string();
            return crate::utils::async_bridge::block_on_pg_result(
                pool,
                move |bridge_pool| async move {
                    let row = sqlx::query(
                        "SELECT status, latest_dispatch_id FROM kanban_cards WHERE id = $1",
                    )
                    .bind(&card_id)
                    .fetch_optional(&bridge_pool)
                    .await
                    .map_err(|error| format!("current card head {card_id}: {error}"))?;
                    Ok(row.map(|row| {
                        (
                            row.try_get::<String, _>("status").unwrap_or_default(),
                            row.try_get::<Option<String>, _>("latest_dispatch_id")
                                .ok()
                                .flatten(),
                        )
                    }))
                },
                |error| error,
            );
        }
        Err("runtime supervisor postgres backend is unavailable".to_string())
    }

    fn record_decision(
        &self,
        signal: SupervisorSignal,
        evidence: &Value,
        chosen_action: SupervisorAction,
        session_key: Option<&str>,
        dispatch_id: Option<&str>,
    ) -> Result<(), String> {
        if let Some(pool) = self.pg_pool.as_ref() {
            let evidence_json = evidence.to_string();
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

    #[cfg(test)]
    fn apply_orphan_fault_injection(&self, dispatch_id: &str, card_id: &str) {
        let _ = (dispatch_id, card_id);
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

    #[test]
    fn emit_signal_json_returns_unified_policy_error_for_invalid_signal() {
        let bridge = BridgeHandle::new();

        let value: Value =
            serde_json::from_str(&emit_signal_json(&bridge, "Nope", r#"{}"#)).unwrap();

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
