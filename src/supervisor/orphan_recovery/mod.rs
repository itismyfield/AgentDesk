//! Owns boot notification recovery and the policy's confirmed orphan rollback.
//! The entrypoints retain their different clocks, identity guards and effects.
use super::*;

mod boot;
mod policy;

pub(crate) use boot::recover_on_boot;
#[cfg(test)]
pub(crate) use boot::recover_once;
pub(crate) use policy::maintain_policy;

const ORPHAN_CONFIRM_KEY_PREFIX: &str = "runtime_supervisor:orphan_confirm:";
const ACTIVE_DISPATCH_STATUSES: &[&str] = &["pending", "dispatched"];

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
    pub(super) fn handle_orphan_candidate(
        &self,
        evidence: Value,
    ) -> Result<SupervisorDecision, String> {
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
                let update = sqlx::query(sqlx::AssertSqlSafe(update_sql))
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

pub(super) fn orphan_ready_clock_assignment_sql(clock: &crate::pipeline::ClockConfig) -> String {
    if clock.mode.as_deref() == Some("coalesce") {
        format!(", {field} = COALESCE({field}, NOW())", field = clock.set)
    } else {
        format!(", {} = NOW()", clock.set)
    }
}
