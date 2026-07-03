use super::super::*;
use crate::utils::format::safe_suffix;

mod completion_context;
mod completion_postgres;

use completion_context::*;
use completion_postgres::*;

pub(crate) use completion_context::build_work_dispatch_completion_result;
pub(in crate::services::discord) use completion_postgres::{
    queue_dispatch_followup_with_handles, runtime_db_fallback_complete_with_result,
    streaming_final_complete_dispatch_with_result,
};

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
        && matches!(
            snapshot.dispatch_type.as_str(),
            // #3594 (T3): "plan"/"plan-review" are work-dispatches (they kick the
            // card into in_progress and are NOT side-paths). They must be
            // eligible for the idle/turn-end auto-completion safety net just like
            // implementation/rework — otherwise a plan agent that goes idle
            // WITHOUT explicitly PATCH-completing would stall forever
            // (OnDispatchCompleted never fires, the depth-gated lifecycle hangs).
            // The downstream completion is benign for them: they produce no
            // tracked changes, and their JS follow-up reads the plan dispatch
            // context (scope_depth) / plan-review result.verdict (missing →
            // cautious re-plan), not a commit SHA.
            "implementation" | "rework" | "plan" | "plan-review"
        )
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
        let mut hints = lookup_dispatch_completion_hints(shared.pg_pool.as_ref(), dispatch_id);
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
mod tests {
    use super::{DispatchSnapshot, should_complete_work_dispatch};

    fn snap(dispatch_type: &str, status: &str) -> DispatchSnapshot {
        DispatchSnapshot {
            dispatch_type: dispatch_type.to_string(),
            status: status.to_string(),
            kanban_card_id: None,
            context: None,
        }
    }

    #[test]
    fn idle_auto_completion_covers_work_dispatch_types_including_plan() {
        // #3594 (T3): plan / plan-review are work-dispatches and MUST be eligible
        // for the idle/turn-end auto-completion safety net, alongside the
        // pre-existing implementation / rework. Without this a plan agent that
        // goes idle without PATCHing would stall (OnDispatchCompleted never
        // fires).
        for dispatch_type in ["implementation", "rework", "plan", "plan-review"] {
            assert!(
                should_complete_work_dispatch(&snap(dispatch_type, "dispatched")),
                "{dispatch_type} must be eligible for idle auto-completion"
            );
            assert!(
                should_complete_work_dispatch(&snap(dispatch_type, "pending")),
                "{dispatch_type} (pending) must be eligible for idle auto-completion"
            );
        }
    }

    #[test]
    fn idle_auto_completion_excludes_side_paths_and_review_family() {
        // Side-paths (scope-assessment, consultation) and the review family must
        // NOT be auto-completed by the work-dispatch turn-end path — they have
        // their own lifecycles. Guards against the plan allowlist over-firing.
        for dispatch_type in [
            "scope-assessment",
            "consultation",
            "review",
            "review-decision",
            "create-pr",
        ] {
            assert!(
                !should_complete_work_dispatch(&snap(dispatch_type, "dispatched")),
                "{dispatch_type} must NOT be auto-completed by the work-dispatch path"
            );
        }
    }

    #[test]
    fn idle_auto_completion_requires_live_status() {
        // Only pending/dispatched are completable; a terminal status is a no-op.
        assert!(!should_complete_work_dispatch(&snap("plan", "completed")));
        assert!(!should_complete_work_dispatch(&snap("plan", "failed")));
    }
}
