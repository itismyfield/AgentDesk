use super::super::*;
use crate::config::local_api_url;
use crate::utils::format::safe_suffix;

#[derive(Debug)]
pub(super) struct DispatchSnapshot {
    pub(super) dispatch_type: String,
    pub(super) status: String,
    pub(super) kanban_card_id: Option<String>,
}

pub(super) async fn fetch_dispatch_snapshot(
    api_port: u16,
    dispatch_id: &str,
) -> Option<DispatchSnapshot> {
    let url = local_api_url(api_port, &format!("/api/dispatches/{dispatch_id}"));
    let resp = reqwest::Client::new().get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = resp.json::<serde_json::Value>().await.ok()?;
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

    const DISMISS_PHRASES: &[&str] = &[
        "리뷰 우회",
        "리뷰 무시",
        "리뷰 스킵",
        "직접 머지",
        "직접 merge",
        "머지 가능하게",
        "머지가능하게",
        "merge 가능하게",
        "merge가능하게",
        "기여자가 직접 머지",
        "contributor can merge",
        "author can merge",
        "direct merge",
    ];
    if DISMISS_PHRASES
        .iter()
        .any(|phrase| normalized.contains(phrase))
    {
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
    found.or_else(|| classify_review_decision_phrase(tail))
}

async fn submit_review_decision_fallback(
    api_port: u16,
    card_id: &str,
    dispatch_id: &str,
    decision: &str,
    full_response: &str,
) -> Result<(), String> {
    let comment = truncate_str(full_response.trim(), 4000).to_string();
    let url = local_api_url(api_port, "/api/review-decision");
    // #109: Include dispatch_id so the server can atomically consume the
    // specific review-decision dispatch, preventing replay attacks.
    let resp = reqwest::Client::new()
        .post(url)
        .json(&serde_json::json!({
            "card_id": card_id,
            "dispatch_id": dispatch_id,
            "decision": decision,
            "comment": comment,
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(format!("HTTP {status}: {body}"))
    }
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
    api_port: u16,
    dispatch_id: &str,
    verdict: &str,
    full_response: &str,
    provider: &str,
) -> Result<(), String> {
    let payload = build_verdict_payload(dispatch_id, verdict, full_response, provider);
    let url = local_api_url(api_port, "/api/review-verdict");
    let resp = reqwest::Client::new()
        .post(url)
        .json(&payload)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if resp.status().is_success() {
        Ok(())
    } else {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        Err(format!("HTTP {status}: {body}"))
    }
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

/// Explicitly complete implementation/rework dispatches at turn end.
/// Last-resort dispatch completion via runtime-root SQLite file.
///
/// Opens a fresh connection to the on-disk DB (bypassing the Db pool) and writes
/// a status + reconciliation marker so onTick can run the hook chain later.
/// Returns `true` if the UPDATE affected at least one row.
fn runtime_db_fallback_complete_with_result(dispatch_id: &str, result: &serde_json::Value) -> bool {
    let Some(root) = crate::cli::agentdesk_runtime_root() else {
        return false;
    };
    let db_path = root.join("data/agentdesk.sqlite");
    let Ok(conn) = rusqlite::Connection::open(&db_path) else {
        return false;
    };
    let result_json = result.to_string();
    let changed = conn
        .execute(
            "UPDATE task_dispatches SET status = 'completed', result = ?1, \
             updated_at = datetime('now') WHERE id = ?2 AND status IN ('pending', 'dispatched')",
            rusqlite::params![result_json, dispatch_id],
        )
        .unwrap_or(0);
    if changed > 0 {
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
            rusqlite::params![format!("reconcile_dispatch:{dispatch_id}"), dispatch_id],
        )
        .ok();
    }
    changed > 0
}

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
    /// Commit SHA extracted directly from agent output (most reliable).
    output_commit: Option<String>,
}

fn lookup_dispatch_completion_hints(
    db: Option<&crate::db::Db>,
    dispatch_id: &str,
    card_id: Option<&str>,
) -> DispatchCompletionHints {
    let conn = db.and_then(|db| db.separate_conn().ok());
    let issue_number = conn.as_ref().and_then(|conn| {
        card_id.and_then(|cid| {
            conn.query_row(
                "SELECT github_issue_number FROM kanban_cards WHERE id = ?1",
                [cid],
                |row| row.get::<_, Option<i64>>(0),
            )
            .ok()
            .flatten()
        })
    });
    let dispatch_created_at = conn.as_ref().and_then(|conn| {
        conn.query_row(
            "SELECT created_at FROM task_dispatches WHERE id = ?1",
            [dispatch_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
    });
    DispatchCompletionHints {
        issue_number,
        dispatch_created_at,
        output_commit: None,
    }
}

fn work_dispatch_completion_context(
    adk_cwd: Option<&str>,
    hints: &DispatchCompletionHints,
) -> Option<serde_json::Value> {
    let cwd = adk_cwd.filter(|p| std::path::Path::new(p).is_dir())?;
    // Commit resolution priority:
    // 1) Agent's own commit extracted from turn output (most reliable — direct evidence)
    // 2) Time-scoped: newest commit since dispatch start, preferring issue-number match
    // 3) Issue grep: recent commits matching (#issue_number)
    let completed_commit = hints
        .output_commit
        .clone()
        .or_else(|| {
            hints.dispatch_created_at.as_deref().and_then(|since| {
                crate::services::platform::shell::git_best_commit_for_dispatch(
                    cwd,
                    since,
                    hints.issue_number,
                )
            })
        })
        .or_else(|| {
            hints
                .issue_number
                .and_then(|n| crate::services::platform::shell::git_latest_commit_for_issue(cwd, n))
        })?;
    let mut obj = serde_json::Map::new();
    obj.insert(
        "completed_worktree_path".to_string(),
        serde_json::Value::String(cwd.to_string()),
    );
    obj.insert(
        "completed_commit".to_string(),
        serde_json::Value::String(completed_commit),
    );
    if let Some(branch) = crate::services::platform::shell::git_branch_name(cwd) {
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
    api_port: u16,
    dispatch_id: Option<&str>,
    error_msg: &str,
) {
    let Some(dispatch_id) = dispatch_id else {
        return;
    };
    let url = local_api_url(api_port, &format!("/api/dispatches/{dispatch_id}"));
    let payload = serde_json::json!({
        "status": "failed",
        "result": {"error": error_msg.chars().take(500).collect::<String>()}
    });
    for attempt in 1..=3 {
        match reqwest::Client::new()
            .patch(&url)
            .json(&payload)
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                eprintln!("  [{ts}] ⚠ Dispatch {dispatch_id} failed (transport error)");
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
    let ts = chrono::Local::now().format("%H:%M:%S");
    eprintln!(
        "  [{ts}] ❌ PATCH failed after 3 retries, falling back to direct DB for {dispatch_id}"
    );
    if let Some(root) = crate::cli::agentdesk_runtime_root() {
        let db_path = root.join("data/agentdesk.sqlite");
        if let Ok(conn) = rusqlite::Connection::open(&db_path) {
            let result_json = serde_json::json!({"error": error_msg.chars().take(500).collect::<String>(), "fallback": true}).to_string();
            let _ = conn.execute(
                "UPDATE task_dispatches SET status = 'failed', result = ?1, updated_at = datetime('now') WHERE id = ?2 AND status = 'pending'",
                rusqlite::params![result_json, dispatch_id],
            );
            // Leave reconciliation marker for onTick to pick up and run hook chain
            let _ = conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                rusqlite::params![format!("reconcile_dispatch:{dispatch_id}"), dispatch_id],
            );
        }
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
    let Some(snapshot) = fetch_dispatch_snapshot(shared.api_port, dispatch_id).await else {
        fail_dispatch_with_retry(
            shared.api_port,
            Some(dispatch_id),
            "dispatch snapshot fetch failed",
        )
        .await;
        return;
    };
    if snapshot.status != "pending" {
        return;
    }
    match snapshot.dispatch_type.as_str() {
        "implementation" | "rework" => {}
        _ => return,
    }

    let explicit_work_outcome = turn_output.and_then(extract_explicit_work_outcome);
    let tracked_changes = tracked_change_summary(adk_cwd);

    // Direct finalize_dispatch with retry — single completion authority (#143)
    if let (Some(db), Some(engine)) = (&shared.db, &shared.engine) {
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
        let output_commit = if explicit_work_outcome == Some("noop") {
            None
        } else {
            turn_output.and_then(|output| {
                adk_cwd
                    .filter(|p| std::path::Path::new(p).is_dir())
                    .and_then(|cwd| extract_commit_sha_from_output(output, cwd))
            })
        };
        if let Some(ref sha) = output_commit {
            tracing::info!(
                "[turn_bridge] Extracted commit {} from agent output for dispatch {}",
                &sha[..8.min(sha.len())],
                dispatch_id,
            );
        }
        let mut hints = lookup_dispatch_completion_hints(
            Some(db),
            dispatch_id,
            snapshot.kanban_card_id.as_deref(),
        );
        hints.output_commit = output_commit;
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
            match crate::dispatch::finalize_dispatch(
                db,
                engine,
                dispatch_id,
                completion_source,
                completion_context.as_ref(),
            ) {
                Ok(_) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] ✅ Explicitly completed {dtype} dispatch {dispatch_id}",
                        dtype = snapshot.dispatch_type,
                    );
                    crate::server::routes::dispatches::queue_dispatch_followup(db, dispatch_id);
                    return;
                }
                Err(e) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    eprintln!(
                        "  [{ts}] ⚠ finalize_dispatch failed for {dispatch_id} (attempt {attempt}/3): {e}"
                    );
                    if attempt < 3 {
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    }
                }
            }
        }
        // All retries exhausted — DB fallback via pool, then runtime-root file
        let fallback_ok = db.separate_conn().ok().map_or(false, |conn| {
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
            let result_json = fallback_result.to_string();
            let changed = conn.execute(
                "UPDATE task_dispatches SET status = 'completed', \
                 result = ?1, \
                 updated_at = datetime('now') WHERE id = ?2 AND status IN ('pending', 'dispatched')",
                rusqlite::params![result_json, dispatch_id],
            ).unwrap_or(0);
            if changed > 0 {
                conn.execute(
                    "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                    rusqlite::params![format!("reconcile_dispatch:{dispatch_id}"), dispatch_id],
                ).ok();
            }
            changed > 0
        });
        if !fallback_ok {
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
            if !ok {
                let ts = chrono::Local::now().format("%H:%M:%S");
                eprintln!(
                    "  [{ts}] 🔴 CRITICAL: all completion paths exhausted for dispatch {dispatch_id} — dispatch stranded"
                );
            }
        }
    } else {
        // Db/Engine not available — fall back to API PATCH with retry
        let url = local_api_url(shared.api_port, &format!("/api/dispatches/{dispatch_id}"));
        let api_result = if explicit_work_outcome == Some("noop") {
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
                    output_commit: None,
                },
            )
        };
        let payload = serde_json::json!({
            "status": "completed",
            "result": api_result,
        });
        for attempt in 1..=3u8 {
            match reqwest::Client::new()
                .patch(&url)
                .json(&payload)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] ✅ Explicitly completed {dtype} dispatch {dispatch_id} (via API)",
                        dtype = snapshot.dispatch_type,
                    );
                    return;
                }
                Ok(resp) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    eprintln!(
                        "  [{ts}] ⚠ Explicit dispatch completion failed (attempt {attempt}/3): HTTP {}",
                        resp.status()
                    );
                }
                Err(e) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    eprintln!(
                        "  [{ts}] ⚠ Explicit dispatch completion error (attempt {attempt}/3): {e}"
                    );
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
        if !runtime_db_fallback_complete_with_result(dispatch_id, &runtime_result) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!(
                "  [{ts}] 🔴 CRITICAL: all completion paths exhausted for dispatch {dispatch_id} — dispatch stranded"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

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
                output_commit: None,
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
                output_commit: Some(head.clone()),
            },
        )
        .unwrap();

        assert_eq!(context["completed_commit"].as_str(), Some(head.as_str()));
        assert_eq!(
            context["completed_worktree_path"].as_str(),
            repo_dir.to_str()
        );
    }
}
