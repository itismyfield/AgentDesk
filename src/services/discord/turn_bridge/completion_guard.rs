use super::super::*;
use crate::services::git::GitCommand;
use crate::utils::format::safe_suffix;
use sqlx::Row;

#[derive(Debug)]
pub(super) struct DispatchSnapshot {
    pub(super) dispatch_type: String,
    pub(super) status: String,
    pub(super) kanban_card_id: Option<String>,
    pub(super) context: Option<serde_json::Value>,
}

pub(super) async fn fetch_dispatch_snapshot(
    _api_port: u16,
    dispatch_id: &str,
) -> Option<DispatchSnapshot> {
    let body = crate::services::discord::internal_api::fetch_dispatch(dispatch_id)
        .await
        .ok()?;
    let dispatch = body.get("dispatch")?;
    Some(DispatchSnapshot {
        dispatch_type: dispatch.get("dispatch_type")?.as_str()?.to_string(),
        status: dispatch.get("status")?.as_str()?.to_string(),
        kanban_card_id: dispatch
            .get("kanban_card_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        context: dispatch.get("context").and_then(|value| match value {
            serde_json::Value::Object(_) => Some(value.clone()),
            serde_json::Value::String(raw) => serde_json::from_str(raw).ok(),
            _ => None,
        }),
    })
}

fn should_complete_work_dispatch(snapshot: &DispatchSnapshot) -> bool {
    matches!(snapshot.status.as_str(), "pending" | "dispatched")
        && matches!(snapshot.dispatch_type.as_str(), "implementation" | "rework")
}

fn normalize_review_decision_text(text: &str) -> String {
    text.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_review_decision_meta_discussion(normalized: &str) -> bool {
    [
        "인식",
        "안먹",
        "안 먹",
        "원인",
        "버그",
        "로그",
        "테스트",
        "디버그",
        "debug",
        "parser",
        "파서",
    ]
    .iter()
    .any(|term| normalized.contains(term))
}

fn has_review_decision_negation(normalized: &str) -> bool {
    [
        "하지 마",
        "하지마",
        "하면 안",
        "안 돼",
        "안돼",
        "안 됩니다",
        "안됩니다",
        "안됨",
        "못 하게",
        "못하게",
        "막아",
        "막아줘",
        "보류",
        "금지",
        "불가",
        "불가능",
    ]
    .iter()
    .any(|term| normalized.contains(term))
}

fn classify_review_decision_phrase(text: &str) -> Option<&'static str> {
    let normalized = normalize_review_decision_text(text);
    if normalized.is_empty()
        || is_review_decision_meta_discussion(&normalized)
        || has_review_decision_negation(&normalized)
    {
        return None;
    }

    if normalized.starts_with("accept") {
        return Some("accept");
    }
    if normalized.starts_with("dispute") {
        return Some("dispute");
    }
    if normalized.starts_with("dismiss") {
        return Some("dismiss");
    }

    None
}

pub(in crate::services::discord) fn extract_review_decision(
    full_response: &str,
) -> Option<&'static str> {
    // Match explicit patterns like "DECISION: accept" or "결정: dismiss"
    let explicit =
        regex::Regex::new(r"(?im)^\s*(?:decision|결정)\s*:\s*\**\s*([^\n\r]+?)\s*\**\s*$").ok()?;
    if let Some(caps) = explicit.captures(full_response) {
        return classify_review_decision_phrase(caps.get(1)?.as_str());
    }
    // Fallback: scan for standalone keywords in the last ~500 bytes (char-boundary safe)
    let tail = safe_suffix(full_response, 500);
    let keyword_re = regex::Regex::new(r"(?im)\b(accept|dispute|dismiss)\b").ok()?;
    let mut found: Option<&'static str> = None;
    for caps in keyword_re.captures_iter(tail) {
        let kw = caps.get(1)?.as_str().to_ascii_lowercase();
        let candidate = match kw.as_str() {
            "accept" => "accept",
            "dispute" => "dispute",
            "dismiss" => "dismiss",
            _ => continue,
        };
        if found.is_some() && found != Some(candidate) {
            // Ambiguous — multiple different keywords found
            return None;
        }
        found = Some(candidate);
    }
    found
}

pub(in crate::services::discord) fn extract_review_decision_out_of_scope(
    full_response: &str,
    decision: &str,
) -> bool {
    if decision != "dispute" {
        return false;
    }
    let Ok(explicit) = regex::Regex::new(
        r"(?im)^\s*(?:out[_ -]?of[_ -]?scope|scope[_ -]?mismatch|범위\s*외|스코프\s*외)\s*:\s*(?:true|yes|y|1|맞음|예|네)\s*$",
    ) else {
        return false;
    };
    explicit.is_match(full_response)
}

pub(in crate::services::discord::turn_bridge) fn extract_review_decision_commit_sha(
    full_response: &str,
) -> Option<String> {
    let keyed_commit = regex::Regex::new(
        r#"(?im)\b(?:completed_commit|commit_sha|current_commit|head_sha|commit)\b\s*[:=]\s*`?([0-9a-f]{7,64})`?"#,
    )
    .ok()?;
    keyed_commit
        .captures_iter(full_response)
        .filter_map(|captures| {
            captures
                .get(1)
                .map(|value| value.as_str().to_ascii_lowercase())
        })
        .last()
}

async fn submit_review_decision_fallback(
    _api_port: u16,
    card_id: &str,
    dispatch_id: &str,
    decision: &str,
    full_response: &str,
) -> Result<(), String> {
    let comment = truncate_str(full_response.trim(), 4000).to_string();
    let commit_sha = (decision == "accept")
        .then(|| extract_review_decision_commit_sha(full_response))
        .flatten();
    let out_of_scope = extract_review_decision_out_of_scope(full_response, decision);
    crate::services::discord::internal_api::submit_review_decision(
        crate::services::review_decision::ReviewDecisionBody {
            card_id: card_id.to_string(),
            dispatch_id: Some(dispatch_id.to_string()),
            decision: decision.to_string(),
            comment: Some(comment),
            commit_sha,
            out_of_scope: out_of_scope.then_some(true),
        },
    )
    .await
    .map(|_| ())
}

pub(in crate::services::discord) fn extract_explicit_review_verdict(
    full_response: &str,
) -> Option<&'static str> {
    let pattern = regex::Regex::new(
        r"(?im)^\s*(?:final\s+)?(?:verdict|overall)\s*:\s*\**\s*(pass|improve|reject|rework|approved)\b",
    )
    .ok()?;
    let verdict = pattern
        .captures(full_response)?
        .get(1)?
        .as_str()
        .to_ascii_lowercase();
    match verdict.as_str() {
        "pass" => Some("pass"),
        "improve" => Some("improve"),
        "reject" => Some("reject"),
        "rework" => Some("rework"),
        "approved" => Some("approved"),
        _ => None,
    }
}

fn phase_gate_pass_verdict(snapshot: &DispatchSnapshot) -> String {
    snapshot
        .context
        .as_ref()
        .and_then(|context| context.get("phase_gate"))
        .and_then(|phase_gate| phase_gate.get("pass_verdict"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("phase_gate_passed")
        .to_string()
}

pub(in crate::services::discord) fn extract_phase_gate_pass(full_response: &str) -> bool {
    let explicit_verdict = regex::Regex::new(
        r"(?im)^\s*(?:verdict|overall|result)\s*:\s*\**\s*(phase_gate_passed|pass|passed)\b",
    )
    .ok();
    if explicit_verdict
        .as_ref()
        .is_some_and(|pattern| pattern.is_match(full_response))
    {
        return true;
    }

    let phase_gate_line = regex::Regex::new(
        r"(?im)^\s*phase\s*gate(?:\s+P?\d+)?\s*:\s*\**\s*(phase_gate_passed|pass|passed)\b",
    )
    .ok();
    phase_gate_line
        .as_ref()
        .is_some_and(|pattern| pattern.is_match(full_response))
}

async fn submit_phase_gate_completion_fallback(
    dispatch_id: &str,
    pass_verdict: &str,
    full_response: &str,
) -> Result<(), String> {
    let payload = crate::services::dispatches::UpdateDispatchBody {
        status: Some("completed".to_string()),
        result: Some(serde_json::json!({
            "verdict": pass_verdict,
            "summary": truncate_str(full_response.trim(), 4000),
            "completion_source": "turn_bridge_phase_gate_pass_fallback",
        })),
        allowed_from: Some(
            ["pending", "dispatched", "failed"]
                .iter()
                .map(|status| (*status).to_string())
                .collect(),
        ),
    };
    match crate::services::discord::internal_api::update_dispatch(dispatch_id, payload).await? {
        crate::services::discord::internal_api::DispatchUpdateOutcome::Updated(_) => Ok(()),
        crate::services::discord::internal_api::DispatchUpdateOutcome::Conflict { body } => {
            Err(format!("phase-gate dispatch already terminal: {body}"))
        }
    }
}

fn decode_pg_completion_hint_field<T>(
    decoded: Result<Option<T>, sqlx::Error>,
    dispatch_id: &str,
    column: &'static str,
) -> Option<T> {
    match decoded {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(
                dispatch_id = %dispatch_id,
                column,
                "failed to decode postgres completion hint field: {error}"
            );
            None
        }
    }
}

pub(in crate::services::discord) fn extract_explicit_work_outcome(
    full_response: &str,
) -> Option<&'static str> {
    let pattern = regex::Regex::new(r"(?im)^\s*(?:outcome|결과)\s*:\s*\**\s*(noop)\b").ok()?;
    let outcome = pattern
        .captures(full_response)?
        .get(1)?
        .as_str()
        .to_ascii_lowercase();
    match outcome.as_str() {
        "noop" => Some("noop"),
        _ => None,
    }
}

async fn submit_review_verdict_fallback(
    _api_port: u16,
    dispatch_id: &str,
    verdict: &str,
    full_response: &str,
    provider: &str,
) -> Result<(), String> {
    crate::services::discord::internal_api::submit_review_verdict(
        crate::services::review_decision::SubmitVerdictBody {
            dispatch_id: dispatch_id.to_string(),
            overall: verdict.to_string(),
            items: None,
            notes: None,
            feedback: Some(truncate_str(full_response.trim(), 4000).to_string()),
            commit: None,
            provider: Some(provider.to_string()),
        },
    )
    .await
    .map(|_| ())
}

pub(in crate::services::discord) async fn guard_review_dispatch_completion(
    api_port: u16,
    dispatch_id: Option<&str>,
    full_response: &str,
    provider: &str,
) -> Option<String> {
    let dispatch_id = dispatch_id?;
    let snapshot = fetch_dispatch_snapshot(api_port, dispatch_id).await?;
    if !matches!(
        snapshot.status.as_str(),
        "pending" | "dispatched" | "failed"
    ) {
        return None;
    }

    match snapshot.dispatch_type.as_str() {
        "review" => {
            if let Some(verdict) = extract_explicit_review_verdict(full_response) {
                match submit_review_verdict_fallback(
                    api_port,
                    dispatch_id,
                    verdict,
                    full_response,
                    provider,
                )
                .await
                {
                    Ok(()) => return None,
                    Err(err) => {
                        return Some(format!(
                            "⚠️ review verdict 자동 제출 실패: {err}\n`review-verdict` API를 다시 호출해야 파이프라인이 진행됩니다."
                        ));
                    }
                }
            }
            Some(
                "⚠️ review dispatch가 아직 pending입니다. 응답 첫 줄에 `VERDICT: pass|improve|reject|rework`를 적고 `review-verdict` API를 호출해야 완료됩니다."
                    .to_string(),
            )
        }
        "review-decision" => {
            if let Some(decision) = extract_review_decision(full_response) {
                if let Some(card_id) = snapshot.kanban_card_id.as_deref() {
                    match submit_review_decision_fallback(
                        api_port,
                        card_id,
                        dispatch_id,
                        decision,
                        full_response,
                    )
                    .await
                    {
                        Ok(()) => return None,
                        Err(err) => {
                            return Some(format!(
                                "⚠️ review-decision 자동 제출 실패: {err}\n`review-decision` API를 다시 호출해야 파이프라인이 진행됩니다."
                            ));
                        }
                    }
                }
            }
            Some(
                "⚠️ review-decision dispatch가 아직 pending입니다. `review-decision` API를 호출해야 카드가 다음 단계로 이동합니다."
                    .to_string(),
            )
        }
        "phase-gate" => {
            if extract_phase_gate_pass(full_response) {
                let pass_verdict = phase_gate_pass_verdict(&snapshot);
                match submit_phase_gate_completion_fallback(
                    dispatch_id,
                    &pass_verdict,
                    full_response,
                )
                .await
                {
                    Ok(()) => return None,
                    Err(err) => {
                        return Some(format!(
                            "⚠️ phase-gate PASS 자동 완료 실패: {err}\n`PATCH /api/dispatches/{dispatch_id}`를 다시 호출해야 큐가 진행됩니다."
                        ));
                    }
                }
            }
            if matches!(snapshot.status.as_str(), "pending" | "dispatched") {
                Some(
                    "⚠️ phase-gate dispatch가 아직 열려 있습니다. `Phase Gate Pn: PASS`와 함께 `PATCH /api/dispatches/{id}`로 완료해야 큐가 진행됩니다."
                        .to_string(),
                )
            } else {
                None
            }
        }
        _ => None,
    }
}

fn transition_source_uses_live_command_bot(transition_source: &str) -> bool {
    let source = transition_source.trim();
    source.starts_with("turn_bridge") || source.starts_with("watcher")
}

fn with_runtime_postgres_result<T, F>(operation: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(
            sqlx::PgPool,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, String>> + Send>>
        + Send
        + 'static,
{
    let config = crate::config::load().map_err(|error| format!("load runtime config: {error}"))?;
    crate::utils::async_bridge::block_on_result(
        async move {
            let Some(pool) = crate::db::postgres::connect(&config).await? else {
                return Err("postgres is not configured".to_string());
            };
            operation(pool).await
        },
        |error| error,
    )
}

fn runtime_postgres_reconcile_key(dispatch_id: &str) -> String {
    format!("reconcile_dispatch:{dispatch_id}")
}

fn should_sync_runtime_auto_queue_terminal_entry(
    dispatch_type: Option<&str>,
    _result: &serde_json::Value,
    auto_queue_review_disabled: bool,
) -> bool {
    match dispatch_type {
        Some("consultation") => false,
        Some("implementation" | "rework") => auto_queue_review_disabled,
        _ => true,
    }
}

async fn auto_queue_review_disabled_for_runtime_dispatch_pg(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    dispatch_id: &str,
) -> Result<bool, String> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(
            SELECT 1
            FROM auto_queue_entries e
            JOIN auto_queue_runs r ON r.id = e.run_id
            WHERE e.dispatch_id = $1
              AND e.status = 'dispatched'
              AND r.status IN ('active', 'paused')
              AND COALESCE(r.review_mode, 'enabled') = 'disabled'
        )",
    )
    .bind(dispatch_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|error| {
        format!("load auto-queue review_mode for runtime dispatch {dispatch_id}: {error}")
    })
}

fn runtime_pg_complete_dispatch_with_result(
    dispatch_id: &str,
    result: &serde_json::Value,
    transition_source: &str,
) -> bool {
    let dispatch_id = dispatch_id.to_string();
    let result_json = result.to_string();
    let result_value = result.clone();
    let transition_source = transition_source.to_string();
    with_runtime_postgres_result(move |pool| {
        Box::pin(async move {
            let mut tx = pool
                .begin()
                .await
                .map_err(|error| format!("begin postgres completion via {transition_source} for {dispatch_id}: {error}"))?;

            let current = sqlx::query(
                "SELECT status, kanban_card_id, dispatch_type
                 FROM task_dispatches
                 WHERE id = $1",
            )
            .bind(&dispatch_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| format!("load postgres dispatch {dispatch_id}: {error}"))?;
            let Some(current) = current else {
                return Ok(false);
            };

            let current_status = current
                .try_get::<Option<String>, _>("status")
                .ok()
                .flatten()
                .unwrap_or_default();
            if !matches!(current_status.as_str(), "pending" | "dispatched") {
                return Ok(false);
            }

            let changed = sqlx::query(
                "UPDATE task_dispatches
                 SET status = 'completed',
                     result = CAST($1 AS jsonb),
                     updated_at = NOW(),
                     completed_at = COALESCE(completed_at, NOW())
                 WHERE id = $2
                   AND status = $3",
            )
            .bind(&result_json)
            .bind(&dispatch_id)
            .bind(&current_status)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("update postgres dispatch {dispatch_id} to completed: {error}"))?
            .rows_affected();
            if changed == 0 {
                return Ok(false);
            }

            let kanban_card_id = current
                .try_get::<Option<String>, _>("kanban_card_id")
                .ok()
                .flatten();
            let dispatch_type = current
                .try_get::<Option<String>, _>("dispatch_type")
                .ok()
                .flatten();
            let auto_queue_review_disabled =
                if matches!(dispatch_type.as_deref(), Some("implementation" | "rework")) {
                    auto_queue_review_disabled_for_runtime_dispatch_pg(&mut tx, &dispatch_id)
                        .await?
                } else {
                    false
                };

            sqlx::query(
                "INSERT INTO dispatch_events (
                    dispatch_id,
                    kanban_card_id,
                    dispatch_type,
                    from_status,
                    to_status,
                    transition_source,
                    payload_json
                ) VALUES ($1, $2, $3, $4, 'completed', $5, CAST($6 AS jsonb))",
            )
            .bind(&dispatch_id)
            .bind(kanban_card_id)
            .bind(dispatch_type.clone())
            .bind(&current_status)
            .bind(&transition_source)
            .bind(&result_json)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("record postgres dispatch event for {dispatch_id}: {error}"))?;

            if should_sync_runtime_auto_queue_terminal_entry(
                dispatch_type.as_deref(),
                &result_value,
                auto_queue_review_disabled,
            ) {
                crate::db::auto_queue::finalize_completed_dispatch_terminal_entry_on_pg_tx(
                    &mut tx,
                    &dispatch_id,
                    &transition_source,
                    true,
                )
                .await
                .map_err(|error| {
                    format!(
                        "sync auto_queue_entries on runtime dispatch completion {dispatch_id}: {error}"
                    )
                })?;
            }

            sqlx::query(
                "INSERT INTO kv_meta (key, value)
                 VALUES ($1, $2)
                 ON CONFLICT (key) DO UPDATE
                     SET value = EXCLUDED.value",
            )
            .bind(runtime_postgres_reconcile_key(&dispatch_id))
            .bind(&dispatch_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("set postgres reconcile marker for {dispatch_id}: {error}"))?;

            if !transition_source_uses_live_command_bot(&transition_source) {
                sqlx::query(
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
                .execute(&mut *tx)
                .await
                .map_err(|error| format!("enqueue postgres status reaction for {dispatch_id}: {error}"))?;
            }

            tx.commit()
                .await
                .map_err(|error| format!("commit postgres completion via {transition_source} for {dispatch_id}: {error}"))?;
            Ok(true)
        })
    })
    .unwrap_or(false)
}

fn runtime_pg_reset_linked_auto_queue_entries(dispatch_id: &str) -> bool {
    let dispatch_id = dispatch_id.to_string();
    with_runtime_postgres_result(move |pool| {
        Box::pin(async move {
            let changed = sqlx::query(
                "UPDATE auto_queue_entries
                 SET status = 'pending',
                     dispatch_id = NULL,
                     slot_index = NULL,
                     dispatched_at = NULL,
                     completed_at = NULL
                 WHERE dispatch_id = $1
                   AND status IN ('pending', 'dispatched', 'failed')",
            )
            .bind(&dispatch_id)
            .execute(&pool)
            .await
            .map_err(|error| {
                format!("reset postgres auto_queue_entries for {dispatch_id}: {error}")
            })?
            .rows_affected();
            Ok(changed > 0)
        })
    })
    .unwrap_or(false)
}

fn runtime_pg_fail_linked_auto_queue_entries(dispatch_id: &str) -> bool {
    let dispatch_id = dispatch_id.to_string();
    with_runtime_postgres_result(move |pool| {
        Box::pin(async move {
            let changed = sqlx::query(
                "UPDATE auto_queue_entries
                 SET status = 'failed',
                     dispatch_id = NULL,
                     slot_index = NULL,
                     dispatched_at = NULL,
                     completed_at = NOW()
                 WHERE dispatch_id = $1
                   AND status IN ('pending', 'dispatched')",
            )
            .bind(&dispatch_id)
            .execute(&pool)
            .await
            .map_err(|error| {
                format!("mark postgres auto_queue_entries failed for {dispatch_id}: {error}")
            })?
            .rows_affected();
            Ok(changed > 0)
        })
    })
    .unwrap_or(false)
}

fn dispatch_failure_result(error_msg: &str, error_code: Option<&str>) -> serde_json::Value {
    let message = error_msg.chars().take(500).collect::<String>();
    match error_code {
        Some(code) => serde_json::json!({
            "error": code,
            "message": message,
        }),
        None => serde_json::json!({
            "error": message,
        }),
    }
}

fn runtime_pg_fail_dispatch_with_result(
    dispatch_id: &str,
    error_msg: &str,
    error_code: Option<&str>,
    reset_auto_queue_entries: bool,
) -> bool {
    let dispatch_id = dispatch_id.to_string();
    let mut fallback_result = dispatch_failure_result(error_msg, error_code);
    fallback_result["fallback"] = serde_json::json!(true);
    let fallback_result = fallback_result.to_string();
    with_runtime_postgres_result(move |pool| {
        Box::pin(async move {
            let mut tx = pool
                .begin()
                .await
                .map_err(|error| format!("begin postgres failure fallback for {dispatch_id}: {error}"))?;

            let current = sqlx::query(
                "SELECT status, kanban_card_id, dispatch_type
                 FROM task_dispatches
                 WHERE id = $1",
            )
            .bind(&dispatch_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|error| format!("load postgres dispatch {dispatch_id}: {error}"))?;
            let Some(current) = current else {
                return Ok(false);
            };

            let current_status = current
                .try_get::<Option<String>, _>("status")
                .ok()
                .flatten()
                .unwrap_or_default();
            if !matches!(current_status.as_str(), "pending" | "dispatched") {
                return Ok(false);
            }

            let changed = sqlx::query(
                "UPDATE task_dispatches
                 SET status = 'failed',
                     result = CAST($1 AS jsonb),
                     updated_at = NOW(),
                     last_stuck_alert_at = NULL
                 WHERE id = $2
                   AND status = $3",
            )
            .bind(&fallback_result)
            .bind(&dispatch_id)
            .bind(&current_status)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("update postgres dispatch {dispatch_id} to failed: {error}"))?
            .rows_affected();
            if changed == 0 {
                return Ok(false);
            }

            let kanban_card_id = current
                .try_get::<Option<String>, _>("kanban_card_id")
                .ok()
                .flatten();
            let dispatch_type = current
                .try_get::<Option<String>, _>("dispatch_type")
                .ok()
                .flatten();

            sqlx::query(
                "INSERT INTO dispatch_events (
                    dispatch_id,
                    kanban_card_id,
                    dispatch_type,
                    from_status,
                    to_status,
                    transition_source,
                    payload_json
                ) VALUES ($1, $2, $3, $4, 'failed', 'turn_bridge_patch_failure_fallback', CAST($5 AS jsonb))",
            )
            .bind(&dispatch_id)
            .bind(kanban_card_id)
            .bind(dispatch_type)
            .bind(&current_status)
            .bind(&fallback_result)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("record postgres dispatch failure event for {dispatch_id}: {error}"))?;

            if reset_auto_queue_entries {
                sqlx::query(
                    "UPDATE auto_queue_entries
                     SET status = 'pending',
                         dispatch_id = NULL,
                         slot_index = NULL,
                         dispatched_at = NULL,
                         completed_at = NULL
                     WHERE dispatch_id = $1
                       AND status IN ('pending', 'dispatched')",
                )
                .bind(&dispatch_id)
                .execute(&mut *tx)
                .await
                .map_err(|error| format!("reset postgres auto_queue_entries for failed dispatch {dispatch_id}: {error}"))?;
            } else {
                sqlx::query(
                    "UPDATE auto_queue_entries
                     SET status = 'failed',
                         dispatch_id = NULL,
                         slot_index = NULL,
                         dispatched_at = NULL,
                         completed_at = NOW()
                     WHERE dispatch_id = $1
                       AND status IN ('pending', 'dispatched')",
                )
                .bind(&dispatch_id)
                .execute(&mut *tx)
                .await
                .map_err(|error| format!("mark postgres auto_queue_entries failed for dispatch {dispatch_id}: {error}"))?;
            }

            sqlx::query(
                "INSERT INTO kv_meta (key, value)
                 VALUES ($1, $2)
                 ON CONFLICT (key) DO UPDATE
                     SET value = EXCLUDED.value",
            )
            .bind(runtime_postgres_reconcile_key(&dispatch_id))
            .bind(&dispatch_id)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("set postgres reconcile marker for failed dispatch {dispatch_id}: {error}"))?;

            sqlx::query(
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
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("enqueue postgres failure status reaction for {dispatch_id}: {error}"))?;

            tx.commit()
                .await
                .map_err(|error| format!("commit postgres failure fallback for {dispatch_id}: {error}"))?;
            Ok(true)
        })
    })
    .unwrap_or(false)
}

/// Explicitly complete implementation/rework dispatches at turn end.
/// Last-resort dispatch completion via the canonical Postgres store.
pub(in crate::services::discord) fn runtime_db_fallback_complete_with_result(
    dispatch_id: &str,
    result: &serde_json::Value,
) -> bool {
    runtime_pg_complete_dispatch_with_result(dispatch_id, result, "turn_bridge_runtime_db_fallback")
}

pub(in crate::services::discord) fn streaming_final_complete_dispatch_with_result(
    dispatch_id: &str,
    result: &serde_json::Value,
) -> bool {
    runtime_pg_complete_dispatch_with_result(dispatch_id, result, "watcher_streaming_final")
}

pub(in crate::services::discord) async fn queue_dispatch_followup_with_handles(
    pg_pool: Option<&sqlx::PgPool>,
    dispatch_id: &str,
    source: &str,
) -> bool {
    if let Some(pool) = pg_pool {
        if let Err(error) =
            crate::services::dispatches_followup::queue_dispatch_followup_pg(pool, dispatch_id)
                .await
        {
            tracing::warn!(
                "[{source}] failed to enqueue postgres dispatch followup for {dispatch_id}: {error}"
            );
            return false;
        }
        return true;
    }

    tracing::warn!(
        "[{source}] no postgres pool available to enqueue dispatch followup for {dispatch_id}"
    );
    false
}

pub(in crate::services::discord) async fn store_reconcile_marker_with_handles(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    dispatch_id: &str,
    source: &str,
) -> bool {
    let reconcile_key = runtime_postgres_reconcile_key(dispatch_id);
    if super::super::internal_api::set_kv_value(&reconcile_key, dispatch_id).is_ok() {
        return true;
    }

    if let Some(pool) = pg_pool {
        if let Err(error) = sqlx::query(
            "INSERT INTO kv_meta (key, value)
             VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE
                 SET value = EXCLUDED.value",
        )
        .bind(&reconcile_key)
        .bind(dispatch_id)
        .execute(pool)
        .await
        {
            tracing::warn!(
                "[{source}] failed to persist postgres reconcile marker for {dispatch_id}: {error}"
            );
            return false;
        }
        return true;
    }

    let _ = db;

    false
}

/// Extract the last git commit SHA from agent turn output.
///
/// Scans the output for `git commit` result lines like:
///   `[main abc1234] fix: some message`
///   `[wt/304-rework def5678] feat: add feature`
///
/// Returns the **last** (most recent) match, resolved to full SHA via
/// `git rev-parse` in the given CWD.  This is the most reliable commit
/// capture method because it reads what the agent actually committed.
fn extract_commit_sha_from_output(output: &str, cwd: &str) -> Option<String> {
    // Pattern: [branch_or_tag SHORT_SHA] message
    // Git commit output format: [main abc1234] commit message here
    let mut last_short_sha: Option<&str> = None;
    for line in output.lines().rev() {
        let trimmed = line.trim();
        // Fast pre-check before full parse
        if !trimmed.starts_with('[') {
            continue;
        }
        // Parse: [branch_name SHA] rest
        let after_bracket = match trimmed.strip_prefix('[') {
            Some(s) => s,
            None => continue,
        };
        let close_idx = match after_bracket.find(']') {
            Some(i) => i,
            None => continue,
        };
        let inside = &after_bracket[..close_idx];
        // Split into branch and SHA: "main abc1234" or "wt/304 def5678"
        let parts: Vec<&str> = inside.split_whitespace().collect();
        if parts.len() != 2 {
            continue;
        }
        let candidate_sha = parts[1];
        // Validate: 7-12 hex chars (short SHA from git commit output)
        if candidate_sha.len() >= 7
            && candidate_sha.len() <= 12
            && candidate_sha.chars().all(|c| c.is_ascii_hexdigit())
        {
            last_short_sha = Some(candidate_sha);
            break; // Scanning in reverse, first match is the last commit
        }
    }
    let short_sha = last_short_sha?;
    // Resolve short SHA to full SHA
    GitCommand::new()
        .args(["rev-parse", short_sha])
        .repo(cwd)
        .run_output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Context needed to resolve the correct completed commit for a dispatch.
struct DispatchCompletionHints {
    issue_number: Option<i64>,
    dispatch_created_at: Option<String>,
    target_repo: Option<String>,
    baseline_commit: Option<String>,
    /// Commit SHA extracted directly from agent output (most reliable).
    output_commit: Option<String>,
    output_commit_repo_dir: Option<String>,
}

#[derive(Default)]
struct ParsedCompletionHintContext {
    target_repo: Option<String>,
    baseline_commit: Option<String>,
}

fn parse_completion_hint_context(
    dispatch_id: &str,
    context_raw: Option<&str>,
    fallback_repo: Option<String>,
) -> ParsedCompletionHintContext {
    let parsed = context_raw.and_then(|raw| match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(value) => Some(value),
        Err(error) => {
            tracing::warn!(
                dispatch_id = %dispatch_id,
                error = %error,
                "failed to parse postgres completion hint context JSON"
            );
            None
        }
    });

    ParsedCompletionHintContext {
        target_repo: parsed
            .as_ref()
            .and_then(|value| value.get("target_repo"))
            .and_then(|value| value.as_str())
            .map(str::to_string)
            .or(fallback_repo),
        baseline_commit: parsed
            .as_ref()
            .and_then(|value| value.get("baseline_commit"))
            .and_then(|value| value.as_str())
            .map(str::to_string),
    }
}

type LegacyCompletionHintColumns = (Option<i64>, Option<String>, Option<String>, Option<String>);

// Production runs PostgreSQL-only (#3035 Phase 0): the legacy sqlite handle is
// always `None`, so the prod build has no DB fallback after the PG path.
fn lookup_dispatch_completion_hints_legacy_fallback(
    _db: Option<&crate::db::Db>,
    _dispatch_id: &str,
) -> LegacyCompletionHintColumns {
    (None, None, None, None)
}

fn lookup_dispatch_completion_hints(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    dispatch_id: &str,
) -> DispatchCompletionHints {
    if let Some(pool) = pg_pool {
        let dispatch_id_owned = dispatch_id.to_string();
        match crate::utils::async_bridge::block_on_pg_result(
            pool,
            move |bridge_pool| async move {
                let row = sqlx::query(
                    "SELECT kc.github_issue_number,
                            td.created_at::text AS created_at,
                            td.context,
                            kc.repo_id
                     FROM task_dispatches td
                     LEFT JOIN kanban_cards kc ON kc.id = td.kanban_card_id
                     WHERE td.id = $1",
                )
                .bind(&dispatch_id_owned)
                .fetch_optional(&bridge_pool)
                .await
                .map_err(|error| {
                    format!(
                        "load postgres completion hints for dispatch {dispatch_id_owned}: {error}"
                    )
                })?;
                Ok(row.map(|row| {
                    let issue_number = decode_pg_completion_hint_field(
                        row.try_get::<Option<i64>, _>("github_issue_number"),
                        &dispatch_id_owned,
                        "github_issue_number",
                    );
                    let dispatch_created_at = decode_pg_completion_hint_field(
                        row.try_get::<Option<String>, _>("created_at"),
                        &dispatch_id_owned,
                        "created_at",
                    );
                    let context_raw = decode_pg_completion_hint_field(
                        row.try_get::<Option<String>, _>("context"),
                        &dispatch_id_owned,
                        "context",
                    );
                    let fallback_repo = decode_pg_completion_hint_field(
                        row.try_get::<Option<String>, _>("repo_id"),
                        &dispatch_id_owned,
                        "repo_id",
                    );
                    let parsed_context = parse_completion_hint_context(
                        &dispatch_id_owned,
                        context_raw.as_deref(),
                        fallback_repo,
                    );
                    (
                        issue_number,
                        dispatch_created_at,
                        parsed_context.target_repo,
                        parsed_context.baseline_commit,
                    )
                }))
            },
            |error| error,
        ) {
            Ok(Some((issue_number, dispatch_created_at, target_repo, baseline_commit))) => {
                return DispatchCompletionHints {
                    issue_number,
                    dispatch_created_at,
                    target_repo,
                    baseline_commit,
                    output_commit: None,
                    output_commit_repo_dir: None,
                };
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    dispatch_id = %dispatch_id,
                    "failed to load postgres completion hints: {error}"
                );
            }
        }
    }

    let (issue_number, dispatch_created_at, target_repo, baseline_commit) =
        lookup_dispatch_completion_hints_legacy_fallback(db, dispatch_id);

    DispatchCompletionHints {
        issue_number,
        dispatch_created_at,
        target_repo,
        baseline_commit,
        output_commit: None,
        output_commit_repo_dir: None,
    }
}

fn completion_repo_dirs(adk_cwd: Option<&str>, hints: &DispatchCompletionHints) -> Vec<String> {
    let mut dirs = Vec::new();
    let mut push_dir = |candidate: Option<String>| {
        if let Some(path) = candidate.filter(|path| std::path::Path::new(path).is_dir()) {
            if !dirs.iter().any(|existing| existing == &path) {
                dirs.push(path);
            }
        }
    };

    push_dir(adk_cwd.map(str::to_string));
    push_dir(
        hints
            .target_repo
            .as_deref()
            .and_then(|value| {
                crate::services::platform::shell::resolve_repo_dir_for_target(Some(value)).ok()
            })
            .flatten(),
    );
    dirs
}

fn extract_output_commit_from_repo_dirs(
    output: &str,
    repo_dirs: &[String],
) -> Option<(String, String)> {
    repo_dirs.iter().find_map(|repo_dir| {
        extract_commit_sha_from_output(output, repo_dir).map(|sha| (repo_dir.clone(), sha))
    })
}

fn completion_main_repo_dir(
    adk_cwd: Option<&str>,
    repo_dirs: &[String],
    hints: &DispatchCompletionHints,
) -> Option<String> {
    crate::services::platform::shell::resolve_repo_dir_for_target(hints.target_repo.as_deref())
        .ok()
        .flatten()
        .or_else(|| adk_cwd.map(str::to_string))
        .or_else(|| repo_dirs.first().cloned())
}

fn work_dispatch_completion_context(
    adk_cwd: Option<&str>,
    hints: &DispatchCompletionHints,
) -> Option<serde_json::Value> {
    let repo_dirs = completion_repo_dirs(adk_cwd, hints);
    let main_repo_dir = completion_main_repo_dir(adk_cwd, &repo_dirs, hints);
    // Commit resolution priority:
    // 1) Agent's own commit extracted from turn output (most reliable — direct evidence)
    // 2) Time-scoped: newest commit since dispatch start, preferring issue-number match
    // 3) Mainline range scan from dispatch baseline with revert filtering
    // 4) Issue grep: recent commits matching (#issue_number)
    let output_commit = hints.output_commit.clone().and_then(|commit| {
        let repo_dir = hints
            .output_commit_repo_dir
            .clone()
            .or_else(|| repo_dirs.first().cloned())?;
        Some((repo_dir, commit))
    });
    let time_scoped_commit = hints.dispatch_created_at.as_deref().and_then(|since| {
        repo_dirs.iter().find_map(|repo_dir| {
            crate::services::platform::shell::git_best_commit_for_dispatch(
                repo_dir,
                since,
                hints.issue_number,
            )
            .map(|commit| (repo_dir.clone(), commit))
        })
    });
    let mainline_commit =
        if let (Some(issue_number), Some(repo_dir)) = (hints.issue_number, main_repo_dir) {
            let baseline_commit = hints
                .baseline_commit
                .clone()
                .or_else(|| crate::services::platform::shell::git_mainline_head_commit(&repo_dir));
            baseline_commit.and_then(|baseline_commit| {
                crate::services::platform::shell::git_mainline_commit_for_issue_since(
                    &repo_dir,
                    &baseline_commit,
                    issue_number,
                )
                .map(|commit| (repo_dir, commit))
            })
        } else {
            None
        };
    let issue_grep_commit = hints.issue_number.and_then(|issue_number| {
        repo_dirs.iter().find_map(|repo_dir| {
            crate::services::platform::shell::git_latest_commit_for_issue(repo_dir, issue_number)
                .map(|commit| (repo_dir.clone(), commit))
        })
    });

    let (cwd, completed_commit) = output_commit
        .or(time_scoped_commit)
        .or(mainline_commit)
        .or(issue_grep_commit)?;
    let mut obj = serde_json::Map::new();
    obj.insert(
        "completed_worktree_path".to_string(),
        serde_json::Value::String(cwd.clone()),
    );
    obj.insert(
        "completed_commit".to_string(),
        serde_json::Value::String(completed_commit),
    );
    if let Some(target_repo) = hints.target_repo.as_deref() {
        obj.insert(
            "target_repo".to_string(),
            serde_json::Value::String(target_repo.to_string()),
        );
    }
    if let Some(branch) = crate::services::platform::shell::git_branch_name(&cwd) {
        obj.insert(
            "completed_branch".to_string(),
            serde_json::Value::String(branch),
        );
    }
    Some(serde_json::Value::Object(obj))
}

fn completion_result_with_context(
    source: &str,
    needs_reconcile: bool,
    adk_cwd: Option<&str>,
    hints: &DispatchCompletionHints,
) -> serde_json::Value {
    let mut result = work_dispatch_completion_context(adk_cwd, hints)
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    if let Some(obj) = result.as_object_mut() {
        obj.insert(
            "completion_source".to_string(),
            serde_json::Value::String(source.to_string()),
        );
        if needs_reconcile {
            obj.insert("needs_reconcile".to_string(), serde_json::Value::Bool(true));
        }
    }
    result
}

pub(crate) fn build_work_dispatch_completion_result(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    dispatch_id: &str,
    source: &str,
    needs_reconcile: bool,
    adk_cwd: Option<&str>,
    turn_output: Option<&str>,
) -> serde_json::Value {
    let mut hints = lookup_dispatch_completion_hints(db, pg_pool, dispatch_id);
    let repo_dirs = completion_repo_dirs(adk_cwd, &hints);
    if let Some((repo_dir, output_commit)) =
        turn_output.and_then(|output| extract_output_commit_from_repo_dirs(output, &repo_dirs))
    {
        hints.output_commit_repo_dir = Some(repo_dir);
        hints.output_commit = Some(output_commit);
    }
    completion_result_with_context(source, needs_reconcile, adk_cwd, &hints)
}

fn summarize_tracked_change_paths(paths: &[String]) -> Option<String> {
    if paths.is_empty() {
        return None;
    }
    let preview = paths.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
    let remaining = paths.len().saturating_sub(5);
    Some(if remaining > 0 {
        format!("{preview} (+{remaining} more)")
    } else {
        preview
    })
}

fn tracked_change_summary(adk_cwd: Option<&str>) -> Option<String> {
    let cwd = adk_cwd.filter(|p| std::path::Path::new(p).is_dir())?;
    let paths = crate::services::platform::shell::git_tracked_change_paths(cwd)?;
    summarize_tracked_change_paths(&paths)
}

fn noop_completion_context(
    adk_cwd: Option<&str>,
    full_response: Option<&str>,
) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    obj.insert(
        "work_outcome".to_string(),
        serde_json::Value::String("noop".to_string()),
    );
    obj.insert(
        "completed_without_changes".to_string(),
        serde_json::Value::Bool(true),
    );
    obj.insert(
        "card_status_target".to_string(),
        serde_json::Value::String("ready".to_string()),
    );
    if let Some(response) = full_response {
        let trimmed = response.trim();
        if !trimmed.is_empty() {
            obj.insert(
                "notes".to_string(),
                serde_json::Value::String(truncate_str(trimmed, 4000).to_string()),
            );
        }
    }
    if let Some(cwd) = adk_cwd.filter(|p| std::path::Path::new(p).is_dir()) {
        obj.insert(
            "completed_worktree_path".to_string(),
            serde_json::Value::String(cwd.to_string()),
        );
        if let Some(branch) = crate::services::platform::shell::git_branch_name(cwd) {
            obj.insert(
                "completed_branch".to_string(),
                serde_json::Value::String(branch),
            );
        }
    }
    serde_json::Value::Object(obj)
}

/// Review and review-decision dispatches stay pending until their explicit
/// API submissions arrive. Work dispatches use explicit PATCH/finalize flows.
/// Fail a dispatch with retry on PATCH failure.
pub(in crate::services::discord) async fn fail_dispatch_with_retry(
    _api_port: u16,
    dispatch_id: Option<&str>,
    error_msg: &str,
) {
    fail_dispatch_with_policy(dispatch_id, error_msg, None, true, None).await;
}

pub(in crate::services::discord) async fn fail_dispatch_tmux_session_died(
    _api_port: u16,
    dispatch_id: Option<&str>,
    error_msg: &str,
) {
    fail_dispatch_with_policy(
        dispatch_id,
        error_msg,
        Some("tmux_session_died"),
        true,
        Some(&["pending", "dispatched"]),
    )
    .await;
}

pub(in crate::services::discord) async fn fail_dispatch_auth_expired(
    _api_port: u16,
    dispatch_id: Option<&str>,
    error_msg: &str,
) {
    fail_dispatch_with_policy(
        dispatch_id,
        error_msg,
        Some("auth_token_expired"),
        false,
        None,
    )
    .await;
}

async fn fail_dispatch_with_policy(
    dispatch_id: Option<&str>,
    error_msg: &str,
    error_code: Option<&str>,
    reset_auto_queue_entries: bool,
    allowed_from: Option<&[&str]>,
) {
    let Some(dispatch_id) = dispatch_id else {
        return;
    };
    let span_name = if reset_auto_queue_entries {
        "fail_dispatch_with_retry"
    } else {
        "fail_dispatch_terminal"
    };
    let dispatch_span = crate::logging::dispatch_span(span_name, Some(dispatch_id), None, None);
    let _guard = dispatch_span.enter();
    let payload = crate::services::dispatches::UpdateDispatchBody {
        status: Some("failed".to_string()),
        result: Some(dispatch_failure_result(error_msg, error_code)),
        allowed_from: allowed_from.map(|statuses| {
            statuses
                .iter()
                .map(|status| (*status).to_string())
                .collect()
        }),
    };
    use crate::services::discord::internal_api::DispatchUpdateOutcome;
    for attempt in 1..=3 {
        match crate::services::discord::internal_api::update_dispatch(dispatch_id, payload.clone())
            .await
        {
            Ok(DispatchUpdateOutcome::Updated(_)) => {
                tracing::warn!("marked dispatch as failed");
                if reset_auto_queue_entries {
                    if !runtime_pg_reset_linked_auto_queue_entries(dispatch_id) {
                        tracing::warn!(
                            "failed dispatch auto-queue retry reset skipped or affected no rows"
                        );
                    }
                } else if !runtime_pg_fail_linked_auto_queue_entries(dispatch_id) {
                    tracing::warn!(
                        "failed dispatch auto-queue terminal update skipped or affected no rows"
                    );
                }
                return;
            }
            Ok(DispatchUpdateOutcome::Conflict { body }) => {
                // #2194 follow-up: dispatch is already in a terminal status
                // (completed/cancelled/failed). Do NOT overwrite via DB fallback.
                // Trust the existing terminal state — reconciliation hooks own
                // the auto-queue transition.
                tracing::info!(
                    dispatch_id = %dispatch_id,
                    response = %body,
                    "fail_dispatch: dispatch already terminal (409 conflict); skipping DB fallback"
                );
                return;
            }
            _ => {
                if attempt < 3 {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    }
    // Fallback: direct DB update to prevent orphan dispatch.
    // Also leave a reconciliation marker so onTick can run the hook chain later.
    tracing::error!("dispatch PATCH failed after retries; falling back to direct DB");
    if !runtime_pg_fail_dispatch_with_result(
        dispatch_id,
        error_msg,
        error_code,
        reset_auto_queue_entries,
    ) {
        tracing::warn!("postgres failure fallback skipped or affected no rows");
    }
}

/// Complete an implementation/rework dispatch via finalize_dispatch (#143).
///
/// Retries finalize_dispatch 3x with 1-second backoff. On exhaustion, falls back
/// to direct DB UPDATE + reconciliation marker for onTick hook chain.
/// When Db/Engine are unavailable, retries API PATCH 3x then DB fallback.
pub(super) async fn complete_work_dispatch_on_turn_end(
    shared: &Arc<super::super::SharedData>,
    dispatch_id: Option<&str>,
    adk_cwd: Option<&str>,
    turn_output: Option<&str>,
) {
    let Some(dispatch_id) = dispatch_id else {
        return;
    };
    let turn_span = crate::logging::dispatch_span(
        "complete_work_dispatch_on_turn_end",
        Some(dispatch_id),
        None,
        None,
    );
    let _turn_guard = turn_span.enter();
    let Some(snapshot) = fetch_dispatch_snapshot(shared.api_port, dispatch_id).await else {
        fail_dispatch_with_retry(
            shared.api_port,
            Some(dispatch_id),
            "dispatch snapshot fetch failed",
        )
        .await;
        return;
    };
    if !should_complete_work_dispatch(&snapshot) {
        return;
    }
    let snapshot_span = crate::logging::dispatch_span(
        "complete_work_dispatch_snapshot",
        Some(dispatch_id),
        snapshot.kanban_card_id.as_deref(),
        None,
    );
    let _snapshot_guard = snapshot_span.enter();

    let explicit_work_outcome = turn_output.and_then(extract_explicit_work_outcome);
    let tracked_changes = tracked_change_summary(adk_cwd);

    // Direct finalize_dispatch with retry — single completion authority (#143)
    if let Some(engine) = &shared.policy.engine {
        if explicit_work_outcome == Some("noop") {
            if let Some(ref changes) = tracked_changes {
                let reason = format!(
                    "OUTCOME: noop rejected because tracked changes remain in the worktree: {changes}. Commit or discard them before completing the dispatch."
                );
                tracing::warn!(
                    "[turn_bridge] Rejecting noop completion for dispatch {}: {}",
                    dispatch_id,
                    reason
                );
                fail_dispatch_with_retry(shared.api_port, Some(dispatch_id), &reason).await;
                return;
            }
        }

        // Extract commit SHA directly from agent output (most reliable method)
        let mut hints = lookup_dispatch_completion_hints(
            None::<&crate::db::Db>,
            shared.pg_pool.as_ref(),
            dispatch_id,
        );
        let repo_dirs = completion_repo_dirs(adk_cwd, &hints);
        if explicit_work_outcome != Some("noop") {
            let turn_output = turn_output.map(str::to_string);
            let repo_dirs_for_lookup = repo_dirs.clone();
            let output_commit = tokio::task::spawn_blocking(move || {
                turn_output.as_deref().and_then(|output| {
                    extract_output_commit_from_repo_dirs(output, &repo_dirs_for_lookup)
                })
            })
            .await
            .ok()
            .flatten();
            if let Some((repo_dir, output_commit)) = output_commit {
                hints.output_commit_repo_dir = Some(repo_dir);
                hints.output_commit = Some(output_commit);
            }
        }
        if let Some(ref sha) = hints.output_commit {
            tracing::info!(
                "[turn_bridge] Extracted commit {} from agent output for dispatch {}",
                &sha[..8.min(sha.len())],
                dispatch_id,
            );
        }
        let completion_context = if explicit_work_outcome == Some("noop") {
            Some(noop_completion_context(adk_cwd, turn_output))
        } else {
            match work_dispatch_completion_context(adk_cwd, &hints) {
                Some(ctx) => Some(ctx),
                None => {
                    let reason = if let Some(ref changes) = tracked_changes {
                        format!(
                            "No attributable commit detected for {} dispatch {}. Tracked changes remain in the worktree: {}. Create a commit before finishing the dispatch.",
                            snapshot.dispatch_type, dispatch_id, changes
                        )
                    } else {
                        format!(
                            "No attributable commit detected for {} dispatch {}. Create a commit before finishing the dispatch.",
                            snapshot.dispatch_type, dispatch_id
                        )
                    };
                    tracing::warn!(
                        "[turn_bridge] Rejecting completion without commit for dispatch {}: {}",
                        dispatch_id,
                        reason
                    );
                    fail_dispatch_with_retry(shared.api_port, Some(dispatch_id), &reason).await;
                    return;
                }
            }
        };
        let completion_source = if explicit_work_outcome == Some("noop") {
            "turn_bridge_explicit_noop"
        } else {
            "turn_bridge_explicit"
        };
        for attempt in 1..=3u8 {
            match crate::dispatch::finalize_dispatch_with_backends(
                None::<&crate::db::Db>,
                engine,
                dispatch_id,
                completion_source,
                completion_context.as_ref(),
            ) {
                Ok(_) => {
                    tracing::info!(dispatch_type = %snapshot.dispatch_type, "explicitly completed dispatch");
                    let _ = queue_dispatch_followup_with_handles(
                        shared.pg_pool.as_ref(),
                        dispatch_id,
                        "turn_bridge_explicit",
                    )
                    .await;
                    return;
                }
                Err(e) => {
                    tracing::warn!(attempt, error = %e, "finalize_dispatch failed");
                    if attempt < 3 {
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
            }
        }
        // All retries exhausted — DB fallback via pool, then runtime-root file
        let fallback_ok = {
            let fallback_result = if explicit_work_outcome == Some("noop") {
                let mut result = noop_completion_context(adk_cwd, turn_output);
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "completion_source".to_string(),
                        serde_json::Value::String("turn_bridge_db_fallback_noop".to_string()),
                    );
                    obj.insert("needs_reconcile".to_string(), serde_json::Value::Bool(true));
                }
                result
            } else {
                completion_result_with_context("turn_bridge_db_fallback", true, adk_cwd, &hints)
            };
            let changed = crate::dispatch::set_dispatch_status_with_backends(
                None::<&crate::db::Db>,
                shared.pg_pool.as_ref(),
                dispatch_id,
                "completed",
                Some(&fallback_result),
                "turn_bridge_db_fallback",
                Some(&["pending", "dispatched"]),
                true,
            )
            .unwrap_or(0);
            if changed > 0 {
                let _ = store_reconcile_marker_with_handles(
                    None::<&crate::db::Db>,
                    shared.pg_pool.as_ref(),
                    dispatch_id,
                    "turn_bridge_db_fallback",
                )
                .await;
            }
            changed > 0
        };
        if fallback_ok {
            let _ = queue_dispatch_followup_with_handles(
                shared.pg_pool.as_ref(),
                dispatch_id,
                "turn_bridge_db_fallback",
            )
            .await;
        } else {
            let fallback_result = if explicit_work_outcome == Some("noop") {
                let mut result = noop_completion_context(adk_cwd, turn_output);
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "completion_source".to_string(),
                        serde_json::Value::String("turn_bridge_db_fallback_noop".to_string()),
                    );
                    obj.insert("needs_reconcile".to_string(), serde_json::Value::Bool(true));
                }
                result
            } else {
                serde_json::json!({
                    "completion_source": "turn_bridge_db_fallback",
                    "needs_reconcile": true,
                })
            };
            let ok = runtime_db_fallback_complete_with_result(dispatch_id, &fallback_result);
            if ok {
                let _ = queue_dispatch_followup_with_handles(
                    shared.pg_pool.as_ref(),
                    dispatch_id,
                    "turn_bridge_runtime_db_fallback",
                )
                .await;
            } else {
                tracing::error!("all completion paths exhausted; dispatch stranded");
            }
        }
    } else {
        // Db/Engine not available — fall back to direct dispatch update with retry
        let update_result = if explicit_work_outcome == Some("noop") {
            let mut result = noop_completion_context(adk_cwd, turn_output);
            if let Some(obj) = result.as_object_mut() {
                obj.insert(
                    "completion_source".to_string(),
                    serde_json::Value::String("turn_bridge_explicit_noop".to_string()),
                );
            }
            result
        } else {
            completion_result_with_context(
                "turn_bridge_explicit",
                false,
                adk_cwd,
                &DispatchCompletionHints {
                    issue_number: None,
                    dispatch_created_at: None,
                    target_repo: None,
                    baseline_commit: None,
                    output_commit: None,
                    output_commit_repo_dir: None,
                },
            )
        };
        let payload = crate::services::dispatches::UpdateDispatchBody {
            status: Some("completed".to_string()),
            result: Some(update_result),
            allowed_from: None,
        };
        use crate::services::discord::internal_api::DispatchUpdateOutcome;
        for attempt in 1..=3u8 {
            match crate::services::discord::internal_api::update_dispatch(
                dispatch_id,
                payload.clone(),
            )
            .await
            {
                Ok(DispatchUpdateOutcome::Updated(_)) => {
                    tracing::info!(dispatch_type = %snapshot.dispatch_type, "explicitly completed dispatch via API");
                    let _ = queue_dispatch_followup_with_handles(
                        shared.pg_pool.as_ref(),
                        dispatch_id,
                        "turn_bridge_explicit_api",
                    )
                    .await;
                    return;
                }
                Ok(DispatchUpdateOutcome::Conflict { body }) => {
                    // #2194 follow-up: dispatch already terminal. Skip DB
                    // fallback so we don't clobber the existing result.
                    tracing::info!(
                        dispatch_id = %dispatch_id,
                        dispatch_type = %snapshot.dispatch_type,
                        response = %body,
                        "explicit completion: dispatch already terminal (409); enqueueing followup only"
                    );
                    let _ = queue_dispatch_followup_with_handles(
                        shared.pg_pool.as_ref(),
                        dispatch_id,
                        "turn_bridge_explicit_api_conflict",
                    )
                    .await;
                    return;
                }
                Err(err) => {
                    tracing::warn!(attempt, error = %err, "explicit dispatch completion failed");
                }
            }
            if attempt < 3 {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
        // API retries exhausted — runtime-root DB fallback
        let runtime_result = if explicit_work_outcome == Some("noop") {
            let mut result = noop_completion_context(adk_cwd, turn_output);
            if let Some(obj) = result.as_object_mut() {
                obj.insert(
                    "completion_source".to_string(),
                    serde_json::Value::String("turn_bridge_db_fallback_noop".to_string()),
                );
                obj.insert("needs_reconcile".to_string(), serde_json::Value::Bool(true));
            }
            result
        } else {
            serde_json::json!({
                "completion_source": "turn_bridge_db_fallback",
                "needs_reconcile": true,
            })
        };
        if runtime_db_fallback_complete_with_result(dispatch_id, &runtime_result) {
            let _ = queue_dispatch_followup_with_handles(
                shared.pg_pool.as_ref(),
                dispatch_id,
                "turn_bridge_runtime_db_fallback",
            )
            .await;
        } else {
            tracing::error!("all completion paths exhausted; dispatch stranded");
        }
    }
}

#[cfg(test)]
mod failure_result_tests {
    use super::dispatch_failure_result;

    #[test]
    fn dispatch_failure_result_preserves_legacy_error_shape() {
        let result = dispatch_failure_result("plain transport failure", None);

        assert_eq!(result["error"], "plain transport failure");
        assert!(result.get("message").is_none());
    }

    #[test]
    fn dispatch_failure_result_uses_auth_token_expired_code() {
        let result = dispatch_failure_result(
            "authentication expired; re-authentication required",
            Some("auth_token_expired"),
        );

        assert_eq!(result["error"], "auth_token_expired");
        assert_eq!(
            result["message"],
            "authentication expired; re-authentication required"
        );
    }
}

#[cfg(test)]
mod runtime_completion_policy_tests {
    use super::should_sync_runtime_auto_queue_terminal_entry;

    #[test]
    fn runtime_auto_queue_terminal_sync_matches_dispatch_completion_policy() {
        let normal_result = serde_json::json!({"completion_source": "watcher_streaming_final"});
        let noop_result = serde_json::json!({
            "completion_source": "watcher_streaming_final",
            "work_outcome": "noop",
            "completed_without_changes": true
        });

        assert!(!should_sync_runtime_auto_queue_terminal_entry(
            Some("implementation"),
            &normal_result,
            false
        ));
        assert!(!should_sync_runtime_auto_queue_terminal_entry(
            Some("implementation"),
            &noop_result,
            false
        ));
        assert!(should_sync_runtime_auto_queue_terminal_entry(
            Some("rework"),
            &normal_result,
            true
        ));
        assert!(should_sync_runtime_auto_queue_terminal_entry(
            Some("implementation"),
            &noop_result,
            true
        ));
        assert!(!should_sync_runtime_auto_queue_terminal_entry(
            Some("consultation"),
            &normal_result,
            false
        ));
    }
}
