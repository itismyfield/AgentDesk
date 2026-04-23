use super::super::*;
use crate::utils::format::safe_suffix;
use sqlx::Row;

#[derive(Debug)]
pub(super) struct DispatchSnapshot {
    pub(super) dispatch_type: String,
    pub(super) status: String,
    pub(super) kanban_card_id: Option<String>,
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

async fn submit_review_decision_fallback(
    _api_port: u16,
    card_id: &str,
    dispatch_id: &str,
    decision: &str,
    full_response: &str,
) -> Result<(), String> {
    let comment = truncate_str(full_response.trim(), 4000).to_string();
    crate::services::discord::internal_api::submit_review_decision(
        crate::server::routes::review_verdict::ReviewDecisionBody {
            card_id: card_id.to_string(),
            dispatch_id: Some(dispatch_id.to_string()),
            decision: decision.to_string(),
            comment: Some(comment),
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

pub(super) fn build_verdict_payload(
    dispatch_id: &str,
    verdict: &str,
    full_response: &str,
    provider: &str,
) -> serde_json::Value {
    let feedback = truncate_str(full_response.trim(), 4000).to_string();
    serde_json::json!({
        "dispatch_id": dispatch_id,
        "overall": verdict,
        "feedback": feedback,
        "provider": provider,
    })
}

async fn submit_review_verdict_fallback(
    _api_port: u16,
    dispatch_id: &str,
    verdict: &str,
    full_response: &str,
    provider: &str,
) -> Result<(), String> {
    crate::services::discord::internal_api::submit_review_verdict(
        crate::server::routes::review_verdict::SubmitVerdictBody {
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
    if snapshot.status != "pending" {
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
        _ => None,
    }
}

fn transition_source_uses_live_command_bot(transition_source: &str) -> bool {
    let source = transition_source.trim();
    source.starts_with("turn_bridge") || source.starts_with("watcher")
}

fn reset_linked_auto_queue_entries_on_db(
    sqlite: &crate::db::Db,
    dispatch_id: &str,
) -> Result<usize, String> {
    let conn = sqlite
        .separate_conn()
        .map_err(|error| format!("open sqlite auto_queue_entries connection: {error}"))?;
    conn.execute(
        "UPDATE auto_queue_entries
         SET status = 'pending',
             dispatch_id = NULL,
             slot_index = NULL,
             dispatched_at = NULL,
             completed_at = NULL
         WHERE dispatch_id = ?1
           AND status IN ('pending', 'dispatched')",
        [dispatch_id],
    )
    .map_err(|error| format!("reset sqlite auto_queue_entries for {dispatch_id}: {error}"))
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

fn runtime_pg_complete_dispatch_with_result(dispatch_id: &str, result: &serde_json::Value) -> bool {
    let dispatch_id = dispatch_id.to_string();
    let result_json = result.to_string();
    with_runtime_postgres_result(move |pool| {
        Box::pin(async move {
            let mut tx = pool
                .begin()
                .await
                .map_err(|error| format!("begin postgres completion fallback for {dispatch_id}: {error}"))?;

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

            sqlx::query(
                "INSERT INTO dispatch_events (
                    dispatch_id,
                    kanban_card_id,
                    dispatch_type,
                    from_status,
                    to_status,
                    transition_source,
                    payload_json
                ) VALUES ($1, $2, $3, $4, 'completed', 'turn_bridge_runtime_db_fallback', CAST($5 AS jsonb))",
            )
            .bind(&dispatch_id)
            .bind(kanban_card_id)
            .bind(dispatch_type)
            .bind(&current_status)
            .bind(&result_json)
            .execute(&mut *tx)
            .await
            .map_err(|error| format!("record postgres dispatch event for {dispatch_id}: {error}"))?;

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

            if !transition_source_uses_live_command_bot("turn_bridge_runtime_db_fallback") {
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
                .map_err(|error| format!("commit postgres completion fallback for {dispatch_id}: {error}"))?;
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
                   AND status IN ('pending', 'dispatched')",
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

fn runtime_pg_fail_dispatch_with_result(dispatch_id: &str, error_msg: &str) -> bool {
    let dispatch_id = dispatch_id.to_string();
    let fallback_result = serde_json::json!({
        "error": error_msg.chars().take(500).collect::<String>(),
        "fallback": true,
    })
    .to_string();
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
                     updated_at = NOW()
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
    runtime_pg_complete_dispatch_with_result(dispatch_id, result)
}

pub(in crate::services::discord) async fn queue_dispatch_followup_with_handles(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    dispatch_id: &str,
    source: &str,
) -> bool {
    if let Some(pool) = pg_pool {
        if let Err(error) =
            crate::server::routes::dispatches::queue_dispatch_followup_pg(pool, dispatch_id).await
        {
            tracing::warn!(
                "[{source}] failed to enqueue postgres dispatch followup for {dispatch_id}: {error}"
            );
            return false;
        }
        return true;
    }

    if let Some(db) = db {
        crate::server::routes::dispatches::queue_dispatch_followup(db, dispatch_id);
        return true;
    }

    tracing::warn!(
        "[{source}] no database handle available to enqueue dispatch followup for {dispatch_id}"
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

    if let Some(db) = db {
        match db.separate_conn() {
            Ok(conn) => {
                if let Err(error) = conn.execute(
                    "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                    [reconcile_key.as_str(), dispatch_id],
                ) {
                    tracing::warn!(
                        "[{source}] failed to persist sqlite reconcile marker for {dispatch_id}: {error}"
                    );
                    return false;
                }
                return true;
            }
            Err(error) => {
                tracing::warn!(
                    "[{source}] no sqlite connection available to persist reconcile marker for {dispatch_id}: {error}"
                );
            }
        }
    }

    false
}

#[allow(dead_code)]
pub(in crate::services::discord) fn runtime_db_fallback_complete(
    dispatch_id: &str,
    source: &str,
) -> bool {
    runtime_db_fallback_complete_with_result(
        dispatch_id,
        &serde_json::json!({
            "completion_source": source,
            "needs_reconcile": true,
        }),
    )
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
    std::process::Command::new("git")
        .args(["rev-parse", short_sha])
        .current_dir(cwd)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// Context needed to resolve the correct completed commit for a dispatch.
struct DispatchCompletionHints {
    issue_number: Option<i64>,
    dispatch_created_at: Option<String>,
    target_repo: Option<String>,
    /// Commit SHA extracted directly from agent output (most reliable).
    output_commit: Option<String>,
    output_commit_repo_dir: Option<String>,
}

fn resolve_completion_target_repo(
    context_raw: Option<&str>,
    fallback_repo: Option<String>,
) -> Option<String> {
    context_raw
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .as_ref()
        .and_then(|value| value.get("target_repo"))
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .or(fallback_repo)
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
                    (
                        issue_number,
                        dispatch_created_at,
                        resolve_completion_target_repo(context_raw.as_deref(), fallback_repo),
                    )
                }))
            },
            |error| error,
        ) {
            Ok(Some((issue_number, dispatch_created_at, target_repo))) => {
                return DispatchCompletionHints {
                    issue_number,
                    dispatch_created_at,
                    target_repo,
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

    let conn = db.and_then(|db| db.separate_conn().ok());
    let (issue_number, dispatch_created_at, target_repo) = conn
        .as_ref()
        .and_then(|conn| {
            conn.query_row(
                "SELECT kc.github_issue_number, td.created_at, td.context, kc.repo_id
                 FROM task_dispatches td
                 LEFT JOIN kanban_cards kc ON kc.id = td.kanban_card_id
                 WHERE td.id = ?1",
                [dispatch_id],
                |row| {
                    let context_raw: Option<String> = row.get(2)?;
                    let target_repo = resolve_completion_target_repo(
                        context_raw.as_deref(),
                        row.get::<_, Option<String>>(3).ok().flatten(),
                    );
                    Ok((
                        row.get::<_, Option<i64>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        target_repo,
                    ))
                },
            )
            .ok()
        })
        .unwrap_or((None, None, None));
    DispatchCompletionHints {
        issue_number,
        dispatch_created_at,
        target_repo,
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

fn work_dispatch_completion_context(
    adk_cwd: Option<&str>,
    hints: &DispatchCompletionHints,
) -> Option<serde_json::Value> {
    let repo_dirs = completion_repo_dirs(adk_cwd, hints);
    // Commit resolution priority:
    // 1) Agent's own commit extracted from turn output (most reliable — direct evidence)
    // 2) Time-scoped: newest commit since dispatch start, preferring issue-number match
    // 3) Issue grep: recent commits matching (#issue_number)
    let (cwd, completed_commit) = if let Some(commit) = hints.output_commit.clone() {
        let repo_dir = hints
            .output_commit_repo_dir
            .clone()
            .or_else(|| repo_dirs.first().cloned())?;
        (repo_dir, commit)
    } else if let Some((repo_dir, commit)) =
        hints.dispatch_created_at.as_deref().and_then(|since| {
            repo_dirs.iter().find_map(|repo_dir| {
                crate::services::platform::shell::git_best_commit_for_dispatch(
                    repo_dir,
                    since,
                    hints.issue_number,
                )
                .map(|commit| (repo_dir.clone(), commit))
            })
        })
    {
        (repo_dir, commit)
    } else {
        let issue_number = hints.issue_number?;
        repo_dirs.iter().find_map(|repo_dir| {
            crate::services::platform::shell::git_latest_commit_for_issue(repo_dir, issue_number)
                .map(|commit| (repo_dir.clone(), commit))
        })?
    };
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

pub(in crate::services::discord) fn build_work_dispatch_completion_result(
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
    let Some(dispatch_id) = dispatch_id else {
        return;
    };
    let dispatch_span =
        crate::logging::dispatch_span("fail_dispatch_with_retry", Some(dispatch_id), None, None);
    let _guard = dispatch_span.enter();
    let payload = crate::server::routes::dispatches::UpdateDispatchBody {
        status: Some("failed".to_string()),
        result: Some(serde_json::json!({
            "error": error_msg.chars().take(500).collect::<String>()
        })),
    };
    for attempt in 1..=3 {
        match crate::services::discord::internal_api::update_dispatch(dispatch_id, payload.clone())
            .await
        {
            Ok(_) => {
                tracing::warn!("marked dispatch as failed after transport error");
                if !runtime_pg_reset_linked_auto_queue_entries(dispatch_id) {
                    tracing::warn!("failed dispatch auto-queue reset skipped or affected no rows");
                }
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
    if !runtime_pg_fail_dispatch_with_result(dispatch_id, error_msg) {
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
    if let Some(engine) = &shared.engine {
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
            shared.sqlite.as_ref(),
            shared.pg_pool.as_ref(),
            dispatch_id,
        );
        let repo_dirs = completion_repo_dirs(adk_cwd, &hints);
        if explicit_work_outcome != Some("noop") {
            if let Some((repo_dir, output_commit)) = turn_output
                .and_then(|output| extract_output_commit_from_repo_dirs(output, &repo_dirs))
            {
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
                shared.sqlite.as_ref(),
                engine,
                dispatch_id,
                completion_source,
                completion_context.as_ref(),
            ) {
                Ok(_) => {
                    tracing::info!(dispatch_type = %snapshot.dispatch_type, "explicitly completed dispatch");
                    let _ = queue_dispatch_followup_with_handles(
                        shared.sqlite.as_ref(),
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
                shared.sqlite.as_ref(),
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
                    shared.sqlite.as_ref(),
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
                shared.sqlite.as_ref(),
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
                    shared.sqlite.as_ref(),
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
                    output_commit: None,
                    output_commit_repo_dir: None,
                },
            )
        };
        let payload = crate::server::routes::dispatches::UpdateDispatchBody {
            status: Some("completed".to_string()),
            result: Some(update_result),
        };
        for attempt in 1..=3u8 {
            match crate::services::discord::internal_api::update_dispatch(
                dispatch_id,
                payload.clone(),
            )
            .await
            {
                Ok(_) => {
                    tracing::info!(dispatch_type = %snapshot.dispatch_type, "explicitly completed dispatch via API");
                    let _ = queue_dispatch_followup_with_handles(
                        shared.sqlite.as_ref(),
                        shared.pg_pool.as_ref(),
                        dispatch_id,
                        "turn_bridge_explicit_api",
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
                shared.sqlite.as_ref(),
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
    use super::*;
    use std::io::{self, Write};
    use std::path::Path;
    use std::process::Command;
    use std::sync::{Arc, Mutex};

    fn run_git(repo_dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(repo_dir)
            .output()
            .unwrap_or_else(|err| panic!("git {:?} failed to start: {err}", args));
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_repo_with_initial_commit() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        let repo_dir = repo.path();
        run_git(repo_dir, &["init"]);
        run_git(repo_dir, &["config", "user.name", "AgentDesk Test"]);
        run_git(repo_dir, &["config", "user.email", "agentdesk@example.com"]);
        std::fs::write(repo_dir.join("tracked.txt"), "v1\n").unwrap();
        run_git(repo_dir, &["add", "tracked.txt"]);
        run_git(repo_dir, &["commit", "-m", "initial"]);
        repo
    }

    #[derive(Clone)]
    struct TestLogWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for TestLogWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn capture_logs<T>(run: impl FnOnce() -> T) -> (T, String) {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let log_buffer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .with_writer(move || TestLogWriter {
                buffer: log_buffer.clone(),
            })
            .finish();

        let result = tracing::subscriber::with_default(subscriber, run);
        let captured = buffer.lock().unwrap().clone();
        (result, String::from_utf8_lossy(&captured).to_string())
    }

    #[test]
    fn summarize_tracked_change_paths_limits_preview() {
        let summary = summarize_tracked_change_paths(&[
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
            "e".to_string(),
            "f".to_string(),
        ]);
        assert_eq!(summary.as_deref(), Some("a, b, c, d, e (+1 more)"));
    }

    #[test]
    fn decode_pg_completion_hint_field_logs_decode_errors() {
        let (value, logs) = capture_logs(|| {
            decode_pg_completion_hint_field::<String>(
                Err(sqlx::Error::ColumnNotFound("context".to_string())),
                "dispatch-1",
                "context",
            )
        });

        assert!(value.is_none());
        assert!(logs.contains("failed to decode postgres completion hint field"));
        assert!(logs.contains("dispatch_id=dispatch-1"));
        assert!(logs.contains("context"));
    }

    #[test]
    fn tracked_change_summary_reports_only_tracked_modifications() {
        let repo = init_repo_with_initial_commit();
        let repo_dir = repo.path();
        std::fs::write(repo_dir.join("tracked.txt"), "v2\n").unwrap();
        std::fs::write(repo_dir.join("scratch.txt"), "untracked\n").unwrap();

        let summary = tracked_change_summary(repo_dir.to_str());
        assert_eq!(summary.as_deref(), Some("tracked.txt"));
    }

    #[test]
    fn work_dispatch_completion_context_requires_attributable_commit() {
        let repo = init_repo_with_initial_commit();
        let repo_dir = repo.path().to_str().unwrap();

        let context = work_dispatch_completion_context(
            Some(repo_dir),
            &DispatchCompletionHints {
                issue_number: None,
                dispatch_created_at: None,
                target_repo: None,
                output_commit: None,
                output_commit_repo_dir: None,
            },
        );

        assert!(context.is_none());
    }

    #[test]
    fn work_dispatch_completion_context_uses_output_commit_when_available() {
        let repo = init_repo_with_initial_commit();
        let repo_dir = repo.path();
        let head = run_git(repo_dir, &["rev-parse", "HEAD"]);

        let context = work_dispatch_completion_context(
            repo_dir.to_str(),
            &DispatchCompletionHints {
                issue_number: None,
                dispatch_created_at: None,
                target_repo: None,
                output_commit: Some(head.clone()),
                output_commit_repo_dir: repo_dir.to_str().map(str::to_string),
            },
        )
        .unwrap();

        assert_eq!(context["completed_commit"].as_str(), Some(head.as_str()));
        assert_eq!(
            context["completed_worktree_path"].as_str(),
            repo_dir.to_str()
        );
    }

    #[test]
    fn work_dispatch_completion_context_searches_target_repo_when_cwd_has_no_commit() {
        let default_repo = init_repo_with_initial_commit();
        let target_repo = init_repo_with_initial_commit();
        let target_repo_dir = target_repo.path();

        std::fs::write(target_repo_dir.join("feature.txt"), "external\n").unwrap();
        run_git(target_repo_dir, &["add", "feature.txt"]);
        run_git(
            target_repo_dir,
            &["commit", "-m", "fix: external repo (#627)"],
        );
        let target_head = run_git(target_repo_dir, &["rev-parse", "HEAD"]);
        let expected_target_repo = std::fs::canonicalize(target_repo_dir)
            .unwrap()
            .to_string_lossy()
            .into_owned();

        let context = work_dispatch_completion_context(
            default_repo.path().to_str(),
            &DispatchCompletionHints {
                issue_number: Some(627),
                dispatch_created_at: None,
                target_repo: Some(target_repo_dir.to_string_lossy().to_string()),
                output_commit: None,
                output_commit_repo_dir: None,
            },
        )
        .expect("target repo issue commit should be detected");
        let actual_completed_worktree =
            std::fs::canonicalize(context["completed_worktree_path"].as_str().unwrap())
                .unwrap()
                .to_string_lossy()
                .into_owned();
        let actual_target_repo = std::fs::canonicalize(context["target_repo"].as_str().unwrap())
            .unwrap()
            .to_string_lossy()
            .into_owned();

        assert_eq!(
            context["completed_commit"].as_str(),
            Some(target_head.as_str())
        );
        assert_eq!(actual_completed_worktree, expected_target_repo);
        assert_eq!(actual_target_repo, expected_target_repo);
    }

    #[test]
    fn build_work_dispatch_completion_result_includes_branch_and_source() {
        let repo = init_repo_with_initial_commit();
        let repo_dir = repo.path();
        let short_head = run_git(repo_dir, &["rev-parse", "--short", "HEAD"]);

        let result = build_work_dispatch_completion_result(
            None,
            None,
            "dispatch-1",
            "watcher_completed",
            true,
            repo_dir.to_str(),
            Some(&format!("[main {short_head}] test commit")),
        );

        assert_eq!(
            result["completion_source"].as_str(),
            Some("watcher_completed")
        );
        assert_eq!(result["needs_reconcile"].as_bool(), Some(true));
        assert_eq!(
            result["completed_branch"].as_str(),
            crate::services::platform::shell::git_branch_name(repo_dir.to_str().unwrap())
                .as_deref()
        );
        assert_eq!(
            result["completed_worktree_path"].as_str(),
            repo_dir.to_str()
        );
    }

    #[test]
    fn should_complete_work_dispatch_accepts_pending_implementation() {
        let snapshot = DispatchSnapshot {
            dispatch_type: "implementation".to_string(),
            status: "pending".to_string(),
            kanban_card_id: None,
        };

        assert!(should_complete_work_dispatch(&snapshot));
    }

    #[test]
    fn should_complete_work_dispatch_accepts_dispatched_rework() {
        let snapshot = DispatchSnapshot {
            dispatch_type: "rework".to_string(),
            status: "dispatched".to_string(),
            kanban_card_id: None,
        };

        assert!(should_complete_work_dispatch(&snapshot));
    }

    #[test]
    fn should_complete_work_dispatch_rejects_non_work_statuses() {
        let completed_work = DispatchSnapshot {
            dispatch_type: "implementation".to_string(),
            status: "completed".to_string(),
            kanban_card_id: None,
        };
        let dispatched_review = DispatchSnapshot {
            dispatch_type: "review".to_string(),
            status: "dispatched".to_string(),
            kanban_card_id: None,
        };

        assert!(!should_complete_work_dispatch(&completed_work));
        assert!(!should_complete_work_dispatch(&dispatched_review));
    }

    #[test]
    fn noop_completion_context_targets_ready_without_changes() {
        let result = noop_completion_context(None, Some("OUTCOME: noop\nalready satisfied"));

        assert_eq!(result["work_outcome"], "noop");
        assert_eq!(result["completed_without_changes"], true);
        assert_eq!(result["card_status_target"], "ready");
        assert_eq!(result["notes"], "OUTCOME: noop\nalready satisfied");
    }

    #[test]
    fn reset_linked_auto_queue_entries_on_conn_resets_pending_and_dispatched_rows() {
        let db = crate::db::test_db();
        let conn = db.lock().expect("db lock");
        conn.execute_batch(
            "DROP TABLE IF EXISTS auto_queue_entries;
             CREATE TABLE auto_queue_entries (
                id TEXT PRIMARY KEY,
                run_id TEXT,
                kanban_card_id TEXT,
                agent_id TEXT,
                status TEXT,
                dispatch_id TEXT,
                slot_index INTEGER,
                thread_group INTEGER DEFAULT 0,
                batch_phase INTEGER DEFAULT 0,
                created_at DATETIME DEFAULT CURRENT_TIMESTAMP,
                dispatched_at DATETIME,
                completed_at DATETIME
            );",
        )
        .expect("schema");
        conn.execute(
            "INSERT INTO auto_queue_entries (
                id, run_id, kanban_card_id, agent_id, status, dispatch_id, slot_index, thread_group, batch_phase, dispatched_at, completed_at
             ) VALUES
                ('entry-pending', 'run-1', 'card-1', 'agent-1', 'pending', 'dispatch-1', 7, 0, 0, datetime('now'), datetime('now')),
                ('entry-dispatched', 'run-1', 'card-2', 'agent-1', 'dispatched', 'dispatch-1', 8, 0, 0, datetime('now'), NULL),
                ('entry-done', 'run-1', 'card-3', 'agent-1', 'done', 'dispatch-1', 9, 0, 0, datetime('now'), datetime('now'))",
            [],
        )
        .expect("seed entries");
        drop(conn);

        let changed = reset_linked_auto_queue_entries_on_db(&db, "dispatch-1").expect("reset");
        assert_eq!(changed, 2);

        let pending: (
            String,
            Option<String>,
            Option<i64>,
            Option<String>,
            Option<String>,
        ) = db
            .read_conn()
            .expect("read conn")
            .query_row(
                "SELECT status, dispatch_id, slot_index, dispatched_at, completed_at
                 FROM auto_queue_entries
                 WHERE id = 'entry-pending'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("pending row");
        assert_eq!(pending.0, "pending");
        assert!(pending.1.is_none());
        assert!(pending.2.is_none());
        assert!(pending.3.is_none());
        assert!(pending.4.is_none());

        let dispatched: (
            String,
            Option<String>,
            Option<i64>,
            Option<String>,
            Option<String>,
        ) = db
            .read_conn()
            .expect("read conn")
            .query_row(
                "SELECT status, dispatch_id, slot_index, dispatched_at, completed_at
                 FROM auto_queue_entries
                 WHERE id = 'entry-dispatched'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("dispatched row");
        assert_eq!(dispatched.0, "pending");
        assert!(dispatched.1.is_none());
        assert!(dispatched.2.is_none());
        assert!(dispatched.3.is_none());
        assert!(dispatched.4.is_none());

        let done: (
            String,
            Option<String>,
            Option<i64>,
            Option<String>,
            Option<String>,
        ) = db
            .read_conn()
            .expect("read conn")
            .query_row(
                "SELECT status, dispatch_id, slot_index, dispatched_at, completed_at
                 FROM auto_queue_entries
                 WHERE id = 'entry-done'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("done row");
        assert_eq!(done.0, "done");
        assert_eq!(done.1.as_deref(), Some("dispatch-1"));
        assert_eq!(done.2, Some(9));
        assert!(done.3.is_some());
        assert!(done.4.is_some());
    }
}
