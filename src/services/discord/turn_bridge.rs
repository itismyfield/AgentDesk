use super::handoff::{HandoffRecord, save_handoff};
use super::restart_report::{RestartCompletionReport, clear_restart_report, save_restart_report};
use super::*;
use crate::config::local_api_url;
#[cfg(unix)]
use crate::services::tmux_common::tmux_exact_target;
#[cfg(unix)]
use crate::services::tmux_diagnostics::record_tmux_exit_reason;
use crate::utils::format::{safe_suffix, tail_with_ellipsis};

/// Auto-retry a failed resume by fetching recent Discord history,
/// storing it in kv_meta for the router to inject into the LLM prompt,
/// and re-sending the original message via announce bot.
/// Discord only sees a short notice — the full history is LLM-only.
async fn auto_retry_with_history(
    http: &serenity::Http,
    channel_id: ChannelId,
    user_text: &str,
    api_port: u16,
) {
    let ts = chrono::Local::now().format("%H:%M:%S");

    // Dedup guard: use a static set to prevent turn_bridge + watcher from
    // both firing auto-retry for the same channel simultaneously.
    use std::sync::LazyLock;
    static RETRY_PENDING: LazyLock<dashmap::DashSet<u64>> =
        LazyLock::new(|| dashmap::DashSet::new());
    if !RETRY_PENDING.insert(channel_id.get()) {
        eprintln!("  [{ts}] ⏭ auto-retry: skipped (dedup) for channel {channel_id}");
        return;
    }
    // Clean up guard after 30 seconds (allow future retries)
    let ch_id = channel_id.get();
    tokio::spawn(async move {
        tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
        RETRY_PENDING.remove(&ch_id);
    });

    eprintln!("  [{ts}] ↻ auto-retry: fetching last 10 messages for channel {channel_id}");

    // Fetch last 10 messages from Discord
    let history = match channel_id
        .messages(http, serenity::builder::GetMessages::new().limit(10))
        .await
    {
        Ok(msgs) => {
            let mut lines = Vec::new();
            for msg in msgs.iter().rev() {
                let author = &msg.author.name;
                let content = msg.content.chars().take(300).collect::<String>();
                if !content.trim().is_empty() {
                    lines.push(format!("{}: {}", author, content));
                }
            }
            if lines.is_empty() {
                None
            } else {
                Some(lines.join("\n"))
            }
        }
        Err(e) => {
            eprintln!("  [{ts}] ⚠ auto-retry: failed to fetch history: {e}");
            None
        }
    };

    // Store history in kv_meta for the router to inject into LLM prompt.
    // Key: session_retry_context:{channel_id} — consumed on next turn start.
    if let Some(ref hist) = history {
        let _ = reqwest::Client::new()
            .post(local_api_url(api_port, "/api/kv"))
            .json(&serde_json::json!({
                "key": format!("session_retry_context:{}", channel_id),
                "value": hist,
            }))
            .send()
            .await;
    }

    // Discord message: short notice only — history stays LLM-side
    let retry_content = format!(
        "[이전 대화 복원 — 세션이 만료되어 최근 대화를 컨텍스트로 제공합니다]\n\n{}",
        user_text
    );
    let retry_ch = channel_id.get().to_string();

    let _ = reqwest::Client::new()
        .post(local_api_url(api_port, "/api/send"))
        .json(&serde_json::json!({
            "target": format!("channel:{retry_ch}"),
            "content": retry_content,
            "source": "pipeline",
            "bot": "announce",
        }))
        .send()
        .await;
}

fn clear_local_session_state(
    new_session_id: &mut Option<String>,
    inflight_state: &mut InflightTurnState,
) {
    *new_session_id = None;
    inflight_state.session_id = None;
}

async fn reset_session_for_auto_retry(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    cancel_token: &Arc<CancelToken>,
    adk_session_key: Option<&str>,
    new_session_id: &mut Option<String>,
    inflight_state: &mut InflightTurnState,
    reason: &str,
) {
    clear_local_session_state(new_session_id, inflight_state);
    let _ = save_inflight_state(inflight_state);

    let stale_sid = {
        let mut data = shared.core.lock().await;
        let old = data
            .sessions
            .get(&channel_id)
            .and_then(|s| s.session_id.clone());
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.session_id = None;
        }
        old
    };

    if let Some(key) = adk_session_key {
        super::adk_session::clear_claude_session_id(key, shared.api_port).await;
    }

    if let Some(ref sid) = stale_sid {
        let port = shared.api_port;
        let sid_c = sid.clone();
        tokio::spawn(async move {
            let _ = reqwest::Client::new()
                .post(crate::config::local_api_url(
                    port,
                    "/api/dispatched-sessions/clear-stale-session-id",
                ))
                .json(&serde_json::json!({"claude_session_id": sid_c}))
                .send()
                .await;
        });
    }

    #[cfg(unix)]
    if let Some(name) = cancel_token
        .tmux_session
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        eprintln!("  [{ts}] ♻ auto-retry: killing tmux session {name} before retry ({reason})");
        record_tmux_exit_reason(
            &name,
            &format!("forcing fresh session before auto-retry: {reason}"),
        );
        crate::services::platform::tmux::kill_session(&name);
    }
}

/// Decide the final response text when a Done event arrives.
///
/// Returns the text that should be used as `full_response`.
/// - If streaming accumulated post-tool text, keep the streamed `full_response`.
/// - If streaming only accumulated pre-tool narration (tools used, no post-tool
///   text), replace with the authoritative `result` from the Done event.
/// - If streaming produced nothing, use `result` directly.
fn resolve_done_response(
    full_response: &str,
    result: &str,
    any_tool_used: bool,
    has_post_tool_text: bool,
) -> Option<String> {
    if result.is_empty() {
        return None;
    }
    if full_response.trim().is_empty() {
        return Some(result.to_string());
    }
    if any_tool_used && !has_post_tool_text {
        return Some(result.to_string());
    }
    None
}

pub(super) fn cancel_active_token(token: &Arc<CancelToken>, cleanup_tmux: bool, reason: &str) {
    token.cancelled.store(true, Ordering::Relaxed);

    let child_pid = token.child_pid.lock().ok().and_then(|guard| *guard);
    if let Some(pid) = child_pid {
        claude::kill_pid_tree(pid);
    }

    if cleanup_tmux {
        if child_pid.is_some() {
            if let Some(name) = token
                .tmux_session
                .lock()
                .ok()
                .and_then(|guard| guard.clone())
            {
                #[cfg(unix)]
                {
                    // #145: skip kill for unified-thread sessions with active runs
                    let is_unified =
                        crate::services::provider::parse_provider_and_channel_from_tmux_name(&name)
                            .map(|(_, ch)| {
                                crate::dispatch::is_unified_thread_channel_name_active(&ch)
                            })
                            .unwrap_or(false);
                    if !is_unified {
                        record_tmux_exit_reason(&name, &format!("explicit cleanup via {reason}"));
                        crate::services::platform::tmux::kill_session(&name);
                    }
                }
                #[cfg(not(unix))]
                {
                    let _ = &name;
                }
            }
        } else {
            #[cfg(unix)]
            if let Some(name) = token
                .tmux_session
                .lock()
                .ok()
                .and_then(|guard| guard.clone())
            {
                record_tmux_exit_reason(&name, &format!("explicit cleanup via {reason}"));
            }
            token.cancel_with_tmux_cleanup();
        }
    }
}

#[cfg(unix)]
pub(crate) fn tmux_runtime_paths(tmux_session_name: &str) -> (String, String) {
    use crate::services::tmux_common::session_temp_path;
    (
        session_temp_path(tmux_session_name, "jsonl"),
        session_temp_path(tmux_session_name, "input"),
    )
}

#[cfg(not(unix))]
pub(crate) fn tmux_runtime_paths(tmux_session_name: &str) -> (String, String) {
    let tmp = std::env::temp_dir();
    (
        tmp.join(format!("agentdesk-{}.jsonl", tmux_session_name))
            .display()
            .to_string(),
        tmp.join(format!("agentdesk-{}.input", tmux_session_name))
            .display()
            .to_string(),
    )
}

pub(super) fn stale_inflight_message(saved_response: &str) -> String {
    let trimmed = saved_response.trim();
    if trimmed.is_empty() {
        "⚠️ AgentDesk가 재시작되어 진행 중이던 응답을 이어붙이지 못했습니다.".to_string()
    } else {
        let formatted = format_for_discord(trimmed);
        format!("{}\n\n[Interrupted by restart]", formatted)
    }
}

fn is_dcserver_restart_command(input: &str) -> bool {
    let lower = input.to_lowercase();

    if lower.contains("restart-dcserver") || lower.contains("restart_agentdesk.sh") {
        return true;
    }

    if lower.contains("agentdesk-discord-smoke.sh") && lower.contains("--deploy-live") {
        return true;
    }

    lower.contains("launchctl")
        && lower.contains("com.agentdesk.dcserver")
        && (lower.contains("kickstart") || lower.contains("bootstrap") || lower.contains("bootout"))
}

fn should_resume_watcher_after_turn(
    defer_watcher_resume: bool,
    has_local_queued_turns: bool,
    can_chain_locally: bool,
) -> bool {
    !defer_watcher_resume && !(has_local_queued_turns && can_chain_locally)
}

fn total_context_tokens(input_tokens: u64, output_tokens: u64) -> u64 {
    input_tokens.saturating_add(output_tokens)
}

fn persisted_context_tokens(input_tokens: u64, output_tokens: u64) -> Option<u64> {
    let total = total_context_tokens(input_tokens, output_tokens);
    (total > 0).then_some(total)
}

#[derive(Debug)]
struct DispatchSnapshot {
    dispatch_type: String,
    status: String,
    kanban_card_id: Option<String>,
}

async fn fetch_dispatch_snapshot(api_port: u16, dispatch_id: &str) -> Option<DispatchSnapshot> {
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

fn extract_review_decision(full_response: &str) -> Option<&'static str> {
    // Match explicit patterns like "DECISION: accept" or "결정: dismiss"
    let explicit =
        regex::Regex::new(r"(?im)^\s*(?:decision|결정)\s*:\s*\**\s*(accept|dispute|dismiss)\b")
            .ok()?;
    if let Some(caps) = explicit.captures(full_response) {
        let decision = caps.get(1)?.as_str().to_ascii_lowercase();
        return match decision.as_str() {
            "accept" => Some("accept"),
            "dispute" => Some("dispute"),
            "dismiss" => Some("dismiss"),
            _ => None,
        };
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

fn extract_explicit_review_verdict(full_response: &str) -> Option<&'static str> {
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

fn build_verdict_payload(
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

async fn guard_review_dispatch_completion(
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
pub(super) fn runtime_db_fallback_complete(dispatch_id: &str, source: &str) -> bool {
    let Some(root) = crate::cli::agentdesk_runtime_root() else {
        return false;
    };
    let db_path = root.join("data/agentdesk.sqlite");
    let Ok(conn) = rusqlite::Connection::open(&db_path) else {
        return false;
    };
    let result_json = format!("{{\"completion_source\":\"{source}\",\"needs_reconcile\":true}}");
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

/// Unlike review dispatches (which auto-complete on session idle), these types
/// require an explicit PATCH so the pipeline can advance deterministically.
/// Fail a dispatch with retry on PATCH failure.
async fn fail_dispatch_with_retry(api_port: u16, dispatch_id: Option<&str>, error_msg: &str) {
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
/// Retries finalize_dispatch 3× with 1-second backoff. On exhaustion, falls back
/// to direct DB UPDATE + reconciliation marker for onTick hook chain.
/// When Db/Engine are unavailable, retries API PATCH 3× then DB fallback.
async fn complete_work_dispatch_on_turn_end(
    shared: &Arc<super::SharedData>,
    dispatch_id: Option<&str>,
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

    // Direct finalize_dispatch with retry — single completion authority (#143)
    if let (Some(db), Some(engine)) = (&shared.db, &shared.engine) {
        for attempt in 1..=3u8 {
            match crate::dispatch::finalize_dispatch(
                db,
                engine,
                dispatch_id,
                "turn_bridge_explicit",
                None,
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
            let changed = conn.execute(
                "UPDATE task_dispatches SET status = 'completed', \
                 result = '{\"completion_source\":\"turn_bridge_db_fallback\",\"needs_reconcile\":true}', \
                 updated_at = datetime('now') WHERE id = ?1 AND status IN ('pending', 'dispatched')",
                [dispatch_id],
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
            let ok = runtime_db_fallback_complete(dispatch_id, "turn_bridge_db_fallback");
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
        let payload = serde_json::json!({
            "status": "completed",
            "result": { "completion_source": "turn_bridge_explicit" },
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
        if !runtime_db_fallback_complete(dispatch_id, "turn_bridge_db_fallback") {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!(
                "  [{ts}] 🔴 CRITICAL: all completion paths exhausted for dispatch {dispatch_id} — dispatch stranded"
            );
        }
    }
}

pub(super) struct TurnBridgeContext {
    pub(super) provider: ProviderKind,
    pub(super) channel_id: ChannelId,
    pub(super) user_msg_id: MessageId,
    pub(super) user_text_owned: String,
    pub(super) request_owner_name: String,
    pub(super) request_owner: Option<UserId>,
    pub(super) serenity_ctx: Option<serenity::Context>,
    pub(super) token: Option<String>,
    pub(super) role_binding: Option<RoleBinding>,
    pub(super) adk_session_key: Option<String>,
    pub(super) adk_session_name: Option<String>,
    pub(super) adk_session_info: Option<String>,
    pub(super) adk_cwd: Option<String>,
    pub(super) dispatch_id: Option<String>,
    pub(super) current_msg_id: MessageId,
    pub(super) response_sent_offset: usize,
    pub(super) full_response: String,
    pub(super) tmux_last_offset: Option<u64>,
    pub(super) new_session_id: Option<String>,
    pub(super) defer_watcher_resume: bool,
    pub(super) completion_tx: Option<tokio::sync::oneshot::Sender<()>>,
    pub(super) inflight_state: InflightTurnState,
}

pub(super) fn spawn_turn_bridge(
    http: Arc<serenity::Http>,
    shared_owned: Arc<SharedData>,
    cancel_token: Arc<CancelToken>,
    rx: mpsc::Receiver<StreamMessage>,
    bridge: TurnBridgeContext,
) {
    tokio::spawn(async move {
        const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let channel_id = bridge.channel_id;
        let provider = bridge.provider.clone();
        let user_msg_id = bridge.user_msg_id;
        let user_text_owned = bridge.user_text_owned.clone();
        let request_owner_name = bridge.request_owner_name.clone();
        let request_owner = bridge.request_owner;
        let serenity_ctx = bridge.serenity_ctx.clone();
        let token = bridge.token.clone();
        let role_binding = bridge.role_binding.clone();
        let adk_session_key = bridge.adk_session_key.clone();
        let adk_session_name = bridge.adk_session_name.clone();
        let adk_session_info = bridge.adk_session_info.clone();
        let adk_cwd = bridge.adk_cwd.clone();
        let dispatch_id = bridge.dispatch_id.clone();

        let mut full_response = bridge.full_response.clone();
        let mut last_edit_text = String::new();
        let mut done = false;
        let mut cancelled = false;
        let mut rx_disconnected = false;
        let mut current_tool_line: Option<String> = bridge.inflight_state.current_tool_line.clone();
        let mut last_tool_name: Option<String> = None;
        let mut last_tool_summary: Option<String> = None;
        let mut accumulated_input_tokens: u64 = 0;
        let mut accumulated_output_tokens: u64 = 0;
        let mut spin_idx: usize = 0;
        let mut restart_followup_pending = false;
        let mut any_tool_used = bridge.inflight_state.any_tool_used;
        let mut has_post_tool_text = bridge.inflight_state.has_post_tool_text;
        let mut tmux_handed_off = false;
        let mut transport_error = false;
        let mut last_adk_heartbeat = std::time::Instant::now();
        let current_msg_id = bridge.current_msg_id;
        let response_sent_offset = bridge.response_sent_offset;
        let mut tmux_last_offset = bridge.tmux_last_offset;
        let mut new_session_id = bridge.new_session_id.clone();
        let defer_watcher_resume = bridge.defer_watcher_resume;
        let completion_tx = bridge.completion_tx;
        // Guard: ensure completion_tx fires even if the task panics or
        // exits early, preventing the parent from hanging on completion_rx.
        struct CompletionGuard(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for CompletionGuard {
            fn drop(&mut self) {
                if let Some(tx) = self.0.take() {
                    let _ = tx.send(());
                }
            }
        }
        let _completion_guard = CompletionGuard(completion_tx);

        // Guard: ensure inflight state file is cleaned up even if the task
        // panics or exits early.  On the normal path we defuse the guard
        // after the explicit clear_inflight_state() call.
        struct InflightCleanupGuard {
            provider: Option<ProviderKind>,
            channel_id: u64,
        }
        impl Drop for InflightCleanupGuard {
            fn drop(&mut self) {
                if let Some(ref provider) = self.provider {
                    clear_inflight_state(provider, self.channel_id);
                }
            }
        }
        let mut inflight_guard = InflightCleanupGuard {
            provider: Some(provider.clone()),
            channel_id: channel_id.get(),
        };

        let mut inflight_state = bridge.inflight_state.clone();
        let mut last_status_edit = tokio::time::Instant::now();
        let status_interval = super::status_update_interval();
        let turn_start = std::time::Instant::now();

        let _ = save_inflight_state(&inflight_state);

        while !done {
            let mut state_dirty = false;

            if cancel_token.cancelled.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }

            tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;

            if cancel_token.cancelled.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }

            loop {
                match rx.try_recv() {
                    Ok(msg) => match msg {
                        StreamMessage::Init { session_id: sid } => {
                            new_session_id = Some(sid.clone());
                            inflight_state.session_id = Some(sid);
                            state_dirty = true;
                        }
                        StreamMessage::Text { content } => {
                            full_response.push_str(&content);
                            if any_tool_used {
                                has_post_tool_text = true;
                                inflight_state.has_post_tool_text = true;
                            }
                            current_tool_line = None;
                            last_tool_name = None;
                            last_tool_summary = None;
                            inflight_state.full_response = full_response.clone();
                            state_dirty = true;
                        }
                        StreamMessage::Thinking { summary } => {
                            let display = if let Some(ref s) = summary {
                                format!("💭 {s}")
                            } else {
                                "💭 Thinking...".to_string()
                            };
                            current_tool_line = Some(display);
                            last_tool_name = None;
                            last_tool_summary = None;
                        }
                        StreamMessage::ToolUse { name, input } => {
                            any_tool_used = true;
                            has_post_tool_text = false;
                            inflight_state.any_tool_used = true;
                            inflight_state.has_post_tool_text = false;
                            let summary = format_tool_input(&name, &input);
                            let display_summary = if summary.trim().is_empty() {
                                "…".to_string()
                            } else {
                                truncate_str(&summary, 120).to_string()
                            };
                            current_tool_line = Some(format!("⚙ {}: {}", name, display_summary));
                            last_tool_name = Some(name.clone());
                            last_tool_summary = Some(display_summary);
                            if !restart_followup_pending && is_dcserver_restart_command(&input) {
                                let mut report = RestartCompletionReport::new(
                                    provider.clone(),
                                    channel_id.get(),
                                    "pending",
                                    format!(
                                        "dcserver restart requested by `{}`; 새 프로세스가 후속 보고를 이어받을 예정입니다.",
                                        request_owner_name
                                    ),
                                );
                                report.current_msg_id = Some(current_msg_id.get());
                                report.channel_name = adk_session_name.clone();
                                if save_restart_report(&report).is_ok() {
                                    restart_followup_pending = true;

                                    // Save durable handoff for post-restart follow-up
                                    let handoff = HandoffRecord::new(
                                        &provider,
                                        channel_id.get(),
                                        adk_session_name.clone(),
                                        "재시작 후 수정 내용 확인 및 후속 작업 이어서 진행",
                                        format!(
                                            "재시작 전 사용자 요청: {}\n\n이전 턴의 응답 요약: {}",
                                            user_text_owned,
                                            tail_with_ellipsis(&full_response, 500),
                                        ),
                                        adk_cwd.clone(),
                                        Some(user_msg_id.get()),
                                    );
                                    if let Err(e) = save_handoff(&handoff) {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        println!("  [{ts}] ⚠ failed to save handoff: {e}");
                                    }

                                    let handoff_text = "♻️ dcserver 재시작 중...\n\n새 dcserver가 이 메시지를 이어받는 중입니다.";
                                    rate_limit_wait(&shared_owned, channel_id).await;
                                    let _ = channel_id
                                        .edit_message(
                                            &http,
                                            current_msg_id,
                                            EditMessage::new().content(handoff_text),
                                        )
                                        .await;
                                    last_edit_text = handoff_text.to_string();
                                    inflight_state.current_msg_id = current_msg_id.get();
                                    inflight_state.current_msg_len = handoff_text.len();
                                    state_dirty = true;
                                }
                            }
                            if !full_response.is_empty() {
                                let trimmed = full_response.trim_end();
                                full_response.truncate(trimmed.len());
                                full_response.push_str("\n\n");
                                inflight_state.full_response = full_response.clone();
                                state_dirty = true;
                            }
                        }
                        StreamMessage::ToolResult { content, is_error } => {
                            if let Some(ref tn) = last_tool_name {
                                let status = if is_error { "✗" } else { "✓" };
                                let detail = last_tool_summary
                                    .as_deref()
                                    .filter(|s| !s.is_empty() && *s != "…")
                                    .map(|s| format!("{} {}: {}", status, tn, s))
                                    .unwrap_or_else(|| format!("{} {}", status, tn));
                                current_tool_line = Some(detail);
                            }
                            let _ = content;
                        }
                        StreamMessage::TaskNotification { summary, .. } => {
                            if !summary.is_empty() {
                                full_response.push_str(&format!("\n[Task: {}]\n", summary));
                                inflight_state.full_response = full_response.clone();
                                state_dirty = true;
                            }
                        }
                        StreamMessage::Done {
                            result,
                            session_id: sid,
                        } => {
                            if let Some(resolved) = resolve_done_response(
                                &full_response,
                                &result,
                                any_tool_used,
                                has_post_tool_text,
                            ) {
                                full_response = resolved;
                                inflight_state.full_response = full_response.clone();
                            }
                            if let Some(s) = sid {
                                new_session_id = Some(s.clone());
                                inflight_state.session_id = Some(s);
                            }
                            state_dirty = true;
                            done = true;
                        }
                        StreamMessage::Error {
                            message, stderr, ..
                        } => {
                            transport_error = true;
                            let combined = format!("{} {}", message, stderr).to_lowercase();
                            if combined.contains("prompt is too long")
                                || combined.contains("prompt too long")
                                || combined.contains("context_length_exceeded")
                                || combined.contains("max_tokens")
                                || combined.contains("context window")
                                || combined.contains("token limit")
                            {
                                // Prompt too long is not a terminal failure — user can retry
                                // with a shorter message or /compact. Don't mark as transport error.
                                transport_error = false;
                                full_response = "⚠️ __prompt too long__".to_string();
                            } else if !stderr.is_empty() {
                                full_response = format!(
                                    "Error: {}\nstderr: {}",
                                    message,
                                    truncate_str(&stderr, 500)
                                );
                            } else {
                                full_response = format!("Error: {}", message);
                            }
                            inflight_state.full_response = full_response.clone();
                            state_dirty = true;
                            done = true;
                        }
                        StreamMessage::StatusUpdate {
                            input_tokens,
                            output_tokens,
                            ..
                        } => {
                            // Use latest value (not cumulative) — each StatusUpdate
                            // from claude.rs already includes cumulative cache tokens,
                            // representing the current context window occupancy.
                            if let Some(it) = input_tokens {
                                accumulated_input_tokens = it;
                            }
                            if let Some(ot) = output_tokens {
                                accumulated_output_tokens = ot;
                            }
                        }
                        StreamMessage::TmuxReady {
                            output_path,
                            input_fifo_path,
                            tmux_session_name,
                            last_offset,
                        } => {
                            tmux_handed_off = true;
                            tmux_last_offset = Some(last_offset);
                            inflight_state.tmux_session_name = Some(tmux_session_name.clone());
                            inflight_state.output_path = Some(output_path.clone());
                            inflight_state.input_fifo_path = Some(input_fifo_path);
                            inflight_state.last_offset = last_offset;

                            let already_watching =
                                shared_owned.tmux_watchers.contains_key(&channel_id);
                            if !already_watching {
                                let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
                                let paused = Arc::new(std::sync::atomic::AtomicBool::new(true));
                                let resume_offset = Arc::new(std::sync::Mutex::new(None::<u64>));
                                let pause_epoch = Arc::new(std::sync::atomic::AtomicU64::new(1));
                                let turn_delivered =
                                    Arc::new(std::sync::atomic::AtomicBool::new(false));
                                let handle = TmuxWatcherHandle {
                                    paused: paused.clone(),
                                    resume_offset: resume_offset.clone(),
                                    cancel: cancel.clone(),
                                    pause_epoch: pause_epoch.clone(),
                                    turn_delivered: turn_delivered.clone(),
                                };
                                shared_owned.tmux_watchers.insert(channel_id, handle);
                                #[cfg(unix)]
                                {
                                    let http_bg = http.clone();
                                    let shared_bg = shared_owned.clone();
                                    tokio::spawn(tmux_output_watcher(
                                        channel_id,
                                        http_bg,
                                        shared_bg,
                                        output_path,
                                        tmux_session_name,
                                        last_offset,
                                        cancel,
                                        paused,
                                        resume_offset,
                                        pause_epoch,
                                        turn_delivered,
                                    ));
                                }
                            }
                            state_dirty = true;
                        }
                        StreamMessage::ProcessReady {
                            output_path,
                            session_name,
                            last_offset,
                        } => {
                            // ProcessBackend completed first turn.
                            // No tmux watcher needed — process sessions are monitored
                            // inline via SessionProbe::process during read_output_file_until_result.
                            // Do NOT set tmux_handed_off: ProcessBackend has no watcher,
                            // so the handoff cleanup path would delete the placeholder
                            // with no one to send the final response.
                            tmux_last_offset = Some(last_offset);
                            inflight_state.tmux_session_name = Some(session_name);
                            inflight_state.output_path = Some(output_path);
                            inflight_state.last_offset = last_offset;
                            state_dirty = true;
                        }
                        StreamMessage::OutputOffset { offset } => {
                            tmux_last_offset = Some(offset);
                            inflight_state.last_offset = offset;
                            state_dirty = true;
                        }
                    },
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        rx_disconnected = true;
                        done = true;
                        break;
                    }
                }
            }

            let indicator = SPINNER[spin_idx % SPINNER.len()];
            spin_idx += 1;

            let raw_tool_status = super::formatting::resolve_raw_tool_status(
                current_tool_line.as_deref(),
                &full_response,
            );
            let tool_status = super::formatting::humanize_tool_status(raw_tool_status);
            let current_portion = if response_sent_offset < full_response.len() {
                &full_response[response_sent_offset..]
            } else {
                ""
            };
            let footer = format!("\n\n{} {}", indicator, tool_status);
            let body_budget = DISCORD_MSG_LIMIT.saturating_sub(footer.len() + 10);
            let normalized = normalize_empty_lines(current_portion);
            let stable_display_text = if current_portion.is_empty() {
                format!("{} {}", indicator, tool_status)
            } else {
                let body = tail_with_ellipsis(&normalized, body_budget.max(1));
                format!("{}{}", body, footer)
            };

            if stable_display_text != last_edit_text
                && !done
                && last_status_edit.elapsed() >= status_interval
            {
                rate_limit_wait(&shared_owned, channel_id).await;
                let _ = channel_id
                    .edit_message(
                        &http,
                        current_msg_id,
                        EditMessage::new().content(&stable_display_text),
                    )
                    .await;
                last_edit_text = stable_display_text;
                last_status_edit = tokio::time::Instant::now();
                inflight_state.current_msg_id = current_msg_id.get();
                inflight_state.current_msg_len = last_edit_text.len();
                inflight_state.response_sent_offset = response_sent_offset;
                inflight_state.full_response = full_response.clone();
                state_dirty = true;
            }

            if state_dirty || inflight_state.current_tool_line != current_tool_line {
                inflight_state.current_tool_line = current_tool_line.clone();
                let _ = save_inflight_state(&inflight_state);
            }

            if last_adk_heartbeat.elapsed() >= std::time::Duration::from_secs(30) {
                post_adk_session_status(
                    adk_session_key.as_deref(),
                    adk_session_name.as_deref(),
                    Some(provider.as_str()),
                    "working",
                    &provider,
                    adk_session_info.as_deref(),
                    None,
                    adk_cwd.as_deref(),
                    dispatch_id.as_deref(),
                    shared_owned.api_port,
                )
                .await;
                last_adk_heartbeat = std::time::Instant::now();
            }
        }

        let is_prompt_too_long = full_response.contains("__prompt too long__");
        let review_dispatch_warning = if !cancelled && !is_prompt_too_long {
            guard_review_dispatch_completion(
                shared_owned.api_port,
                dispatch_id.as_deref(),
                &full_response,
                provider.as_str(),
            )
            .await
        } else {
            None
        };

        // Explicitly complete implementation/rework dispatches before sending idle.
        // These types are NOT auto-completed by the session idle hook — they require
        // this explicit PATCH call so the pipeline can advance.
        // Skip if: cancelled, prompt too long, or transport error.
        // transport_error is set by StreamMessage::Error — not substring matching.
        if !cancelled && !is_prompt_too_long && !transport_error {
            complete_work_dispatch_on_turn_end(&shared_owned, dispatch_id.as_deref()).await;
        } else if transport_error && !cancelled {
            // Transport error — fail the dispatch instead of completing
            fail_dispatch_with_retry(
                shared_owned.api_port,
                dispatch_id.as_deref(),
                &full_response,
            )
            .await;
        }

        post_adk_session_status(
            adk_session_key.as_deref(),
            adk_session_name.as_deref(),
            Some(provider.as_str()),
            "idle",
            &provider,
            adk_session_info.as_deref(),
            persisted_context_tokens(accumulated_input_tokens, accumulated_output_tokens),
            adk_cwd.as_deref(),
            dispatch_id.as_deref(),
            shared_owned.api_port,
        )
        .await;

        // ─── Auto-compact: send /compact if context window usage exceeds threshold ───
        // Only for non-dispatch (main channel) sessions with a live tmux session.
        #[cfg(unix)]
        if dispatch_id.is_none() && !is_prompt_too_long {
            let total_tokens =
                total_context_tokens(accumulated_input_tokens, accumulated_output_tokens);
            let ctx_cfg = super::adk_session::fetch_context_thresholds(shared_owned.api_port).await;
            let pct = (total_tokens * 100) / ctx_cfg.context_window.max(1);
            if pct >= ctx_cfg.compact_pct {
                if let Some(ref tmux_name) = inflight_state.tmux_session_name {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] ⚡ Auto-compact: {tmux_name} at {pct}% ({total_tokens} tokens)"
                    );
                    let name = tmux_name.to_string();
                    let _ = tokio::task::spawn_blocking(move || {
                        crate::services::platform::tmux::send_keys(&name, &["/compact", "Enter"])
                    })
                    .await;
                }
            }
        }

        let can_chain_locally =
            serenity_ctx.is_some() && request_owner.is_some() && token.is_some();
        // Mark this turn as finalizing — deferred restart must wait until we finish
        // sending the Discord response and cleaning up state.
        shared_owned
            .finalizing_turns
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        shared_owned
            .global_finalizing
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let has_queued_turns = {
            let mut data = shared_owned.core.lock().await;
            if let Some(removed_token) = data.cancel_tokens.remove(&channel_id) {
                // Mark the token as cancelled so any lingering watchdog timer exits cleanly
                // instead of mistakenly firing on a newer turn's token.
                removed_token
                    .cancelled
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                shared_owned
                    .global_active
                    .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
            // Clean up any pending watchdog deadline override for this channel
            super::clear_watchdog_deadline_override(channel_id.get());
            data.active_request_owner.remove(&channel_id);
            // Clean up dispatch-thread parent mapping when the thread turn ends.
            // Iterate and remove entries whose thread matches this channel_id.
            shared_owned
                .dispatch_thread_parents
                .retain(|_, thread| *thread != channel_id);
            // Clean up cross-channel role override for this thread.
            shared_owned.dispatch_role_overrides.remove(&channel_id);
            let mut remove_queue = false;
            let has_pending = if let Some(queue) = data.intervention_queue.get_mut(&channel_id) {
                let has_pending = super::has_soft_intervention(queue);
                remove_queue = queue.is_empty();
                has_pending
            } else {
                false
            };
            if remove_queue {
                data.intervention_queue.remove(&channel_id);
            }
            drop(data);
            has_pending
        };

        // Remove ⏳ only if NOT handing off to tmux watcher.
        // When tmux watcher is handling the response, it will do ⏳→✅ after delivery.
        let tmux_handoff_path = rx_disconnected && tmux_handed_off && full_response.is_empty();
        if !tmux_handoff_path {
            remove_reaction_raw(&http, channel_id, user_msg_id, '⏳').await;
        }

        // Recovery auto-retry: session died during restart recovery
        let recovery_retry = full_response.contains("__session_died_retry__");
        if recovery_retry {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!(
                "  [{ts}] ↻ Recovery session died — triggering auto-retry with history (channel {})",
                channel_id
            );
            reset_session_for_auto_retry(
                &shared_owned,
                channel_id,
                &cancel_token,
                adk_session_key.as_deref(),
                &mut new_session_id,
                &mut inflight_state,
                "recovery session died",
            )
            .await;
            // Auto-retry with Discord history
            let http_c = http.clone();
            let retry_text = user_text_owned.clone();
            let retry_port = shared_owned.api_port;
            tokio::spawn(async move {
                auto_retry_with_history(&http_c, channel_id, &retry_text, retry_port).await;
            });
            // Replace placeholder with recovery notice (don't delete — avoids visual gap)
            let _ = channel_id
                .edit_message(
                    &http,
                    current_msg_id,
                    serenity::EditMessage::new()
                        .content("↻ 세션 복구 중... 잠시 후 자동으로 이어갑니다."),
                )
                .await;
            full_response = String::new();
        }

        if cancelled {
            if let Ok(guard) = cancel_token.child_pid.lock() {
                if let Some(pid) = *guard {
                    claude::kill_pid_tree(pid);
                }
            }

            full_response = if full_response.trim().is_empty() {
                "[Stopped]".to_string()
            } else {
                let formatted = format_for_discord(&full_response);
                format!("{}\n\n[Stopped]", formatted)
            };

            rate_limit_wait(&shared_owned, channel_id).await;
            let _ = super::formatting::replace_long_message_raw(
                &http,
                channel_id,
                current_msg_id,
                &full_response,
                &shared_owned,
            )
            .await;

            add_reaction_raw(&http, channel_id, user_msg_id, '🛑').await;

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ■ Stopped");
        } else if is_prompt_too_long {
            let mention = request_owner
                .map(|uid| format!("<@{}>", uid.get()))
                .unwrap_or_default();
            full_response = format!(
                "{} ⚠️ 프롬프트가 너무 깁니다. 대화 컨텍스트가 모델 한도를 초과했습니다.\n\n\
                 다음 메시지를 보내면 자동으로 새 턴이 시작됩니다.\n\
                 컨텍스트를 줄이려면 `/compact` 또는 `/clear`를 사용해 주세요.",
                mention
            );
            rate_limit_wait(&shared_owned, channel_id).await;
            let _ = super::formatting::replace_long_message_raw(
                &http,
                channel_id,
                current_msg_id,
                &full_response,
                &shared_owned,
            )
            .await;

            add_reaction_raw(&http, channel_id, user_msg_id, '⚠').await;

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ⚠ Prompt too long (channel {})", channel_id);
        } else if rx_disconnected && tmux_handed_off && full_response.is_empty() {
            // Tmux watcher is handling response delivery — this is normal.
            // Don't delete placeholder — update it so the user sees the turn is still active.
            // The tmux watcher will replace this content when output arrives.
            let _ = channel_id
                .edit_message(
                    &http,
                    current_msg_id,
                    serenity::builder::EditMessage::new().content("⏳ 처리 중..."),
                )
                .await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!(
                "  [{ts}] ✓ tmux handoff complete, placeholder cleaned up, watcher handles response (channel {})",
                channel_id
            );
        } else {
            // Check for resume failure BEFORE any other response handling.
            // This must be outside the is_empty() block because StreamMessage::Error
            // sets full_response to "Error: No conversation found..." which is non-empty.
            let resume_error_in_response = full_response.contains("No conversation found")
                || full_response.contains("Error: No conversation");
            if resume_error_in_response {
                let ts = chrono::Local::now().format("%H:%M:%S");
                eprintln!(
                    "  [{ts}] ⚠ Resume failed (error in response), clearing session_id (channel {})",
                    channel_id
                );
                reset_session_for_auto_retry(
                    &shared_owned,
                    channel_id,
                    &cancel_token,
                    adk_session_key.as_deref(),
                    &mut new_session_id,
                    &mut inflight_state,
                    "resume failed in response output",
                )
                .await;
                // Auto-retry with Discord history context
                let http_c = http.clone();
                let retry_text = user_text_owned.clone();
                let retry_port = shared_owned.api_port;
                tokio::spawn(async move {
                    auto_retry_with_history(&http_c, channel_id, &retry_text, retry_port).await;
                });
                full_response = String::new(); // Suppress error message to user
            } else if full_response.is_empty() {
                // Fallback: try to extract response from tmux output file
                if let Some(ref path) = inflight_state.output_path {
                    let recovered = super::recovery::extract_response_from_output_pub(
                        path,
                        inflight_state.last_offset,
                    );
                    if !recovered.trim().is_empty() {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        eprintln!(
                            "  [{ts}] ↻ Recovered {} chars from output file for channel {}",
                            recovered.len(),
                            channel_id
                        );
                        full_response = recovered;
                    }
                }

                // Check for resume failure in recovered output
                let resume_error_in_recovered = full_response.contains("No conversation found")
                    || full_response.contains("Error: No conversation");
                if resume_error_in_recovered {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    eprintln!(
                        "  [{ts}] ⚠ Resume failed (stale session_id in recovered output), auto-retrying (channel {})",
                        channel_id
                    );
                    reset_session_for_auto_retry(
                        &shared_owned,
                        channel_id,
                        &cancel_token,
                        adk_session_key.as_deref(),
                        &mut new_session_id,
                        &mut inflight_state,
                        "stale session_id in recovered output",
                    )
                    .await;
                    let http_c = http.clone();
                    let retry_text = user_text_owned.clone();
                    let retry_port = shared_owned.api_port;
                    tokio::spawn(async move {
                        auto_retry_with_history(&http_c, channel_id, &retry_text, retry_port).await;
                    });
                    full_response = String::new();
                } else {
                    // Check for resume failure via other methods
                    let mut resume_failed = false;
                    let quick_exit = turn_start.elapsed().as_secs() < 10;
                    // Method 1: check tmux output file
                    if let Some(ref path) = inflight_state.output_path {
                        if let Ok(content) = std::fs::read_to_string(path) {
                            if content.contains("No conversation found")
                                || content.contains("Error: No conversation")
                            {
                                resume_failed = true;
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                eprintln!(
                                    "  [{ts}] ⚠ Resume failed (stale session_id in output file), auto-retrying (channel {})",
                                    channel_id
                                );
                                reset_session_for_auto_retry(
                                    &shared_owned,
                                    channel_id,
                                    &cancel_token,
                                    adk_session_key.as_deref(),
                                    &mut new_session_id,
                                    &mut inflight_state,
                                    "stale session_id in output file",
                                )
                                .await;
                                let http_c = http.clone();
                                let retry_text = user_text_owned.clone();
                                let retry_port = shared_owned.api_port;
                                tokio::spawn(async move {
                                    auto_retry_with_history(
                                        &http_c,
                                        channel_id,
                                        &retry_text,
                                        retry_port,
                                    )
                                    .await;
                                });
                                full_response = String::new();
                            }
                        }
                    }
                    // Method 2: quick exit (<10s) + empty response + had a session_id to resume
                    if !resume_failed && quick_exit && rx_disconnected {
                        let attempted_resume = {
                            let data = shared_owned.core.lock().await;
                            data.sessions
                                .get(&channel_id)
                                .and_then(|s| s.session_id.as_ref())
                                .is_some()
                        };
                        if attempted_resume {
                            resume_failed = true;
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            eprintln!(
                                "  [{ts}] ⚠ Quick exit with empty response — auto-retrying with fresh session (channel {})",
                                channel_id
                            );
                            reset_session_for_auto_retry(
                                &shared_owned,
                                channel_id,
                                &cancel_token,
                                adk_session_key.as_deref(),
                                &mut new_session_id,
                                &mut inflight_state,
                                "quick exit with empty response",
                            )
                            .await;
                            let http_c = http.clone();
                            let retry_text = user_text_owned.clone();
                            let retry_port = shared_owned.api_port;
                            tokio::spawn(async move {
                                auto_retry_with_history(
                                    &http_c,
                                    channel_id,
                                    &retry_text,
                                    retry_port,
                                )
                                .await;
                            });
                            full_response = String::new();
                        }
                    }
                    if !resume_failed {
                        if rx_disconnected {
                            full_response =
                                "(No response — 프로세스가 응답 없이 종료됨)".to_string();
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            eprintln!(
                                "  [{ts}] ⚠ Empty response: rx disconnected before any text \
                                 (channel {}, output_path={:?}, last_offset={})",
                                channel_id, inflight_state.output_path, inflight_state.last_offset
                            );
                        } else {
                            full_response = "(No response)".to_string();
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            eprintln!(
                                "  [{ts}] ⚠ Empty response: done without text (channel {})",
                                channel_id
                            );
                        }
                    }
                }
            }

            // If response is empty (e.g. auto-retry on stale session), show
            // recovery notice instead of deleting — avoids visual gap.
            if full_response.trim().is_empty() {
                let _ = channel_id
                    .edit_message(
                        &http,
                        current_msg_id,
                        serenity::EditMessage::new()
                            .content("↻ 세션 복구 중... 잠시 후 자동으로 이어갑니다."),
                    )
                    .await;
            } else {
                full_response = format_for_discord(&full_response);
                let _ = super::formatting::replace_long_message_raw(
                    &http,
                    channel_id,
                    current_msg_id,
                    &full_response,
                    &shared_owned,
                )
                .await;
            }

            // Signal the watcher that this turn's response was already delivered.
            // Prevents the watcher from relaying the same response when it resumes.
            if let Some(watcher) = shared_owned.tmux_watchers.get(&channel_id) {
                watcher.turn_delivered.store(true, Ordering::Relaxed);
            }

            if !full_response.trim().is_empty() {
                add_reaction_raw(&http, channel_id, user_msg_id, '✅').await;
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ▶ Response sent");
            if let Ok(mut last) = shared_owned.last_turn_at.lock() {
                *last = Some(chrono::Local::now().to_rfc3339());
            }

            if let Some(warning) = review_dispatch_warning.as_deref() {
                // Send via announce bot so the agent sees this as an external
                // message and re-triggers a turn to handle the pending review.
                // Using the provider bot (claude/codex) would be ignored as
                // the agent treats its own bot's messages as self-messages.
                let _ = reqwest::Client::new()
                    .post(crate::config::local_api_url(
                        shared_owned.api_port,
                        "/api/send",
                    ))
                    .json(&serde_json::json!({
                        "target": format!("channel:{}", channel_id),
                        "content": warning,
                        "source": "pipeline",
                        "bot": "announce",
                    }))
                    .send()
                    .await;
            }

            // Record turn metrics
            {
                let duration = shared_owned
                    .turn_start_times
                    .remove(&channel_id)
                    .map(|(_, start)| start.elapsed().as_secs_f64())
                    .unwrap_or(0.0);
                let provider_name = {
                    let settings = shared_owned.settings.read().await;
                    settings.provider.as_str().to_string()
                };
                super::metrics::record_turn(&super::metrics::TurnMetric {
                    channel_id: channel_id.get(),
                    provider: provider_name,
                    timestamp: chrono::Local::now().to_rfc3339(),
                    duration_secs: duration,
                    model: None, // model info from StatusUpdate not yet accumulated in turn_bridge
                    input_tokens: if accumulated_input_tokens > 0 {
                        Some(accumulated_input_tokens)
                    } else {
                        None
                    },
                    output_tokens: if accumulated_output_tokens > 0 {
                        Some(accumulated_output_tokens)
                    } else {
                        None
                    },
                });
            }
        }

        if should_resume_watcher_after_turn(
            defer_watcher_resume,
            has_queued_turns,
            can_chain_locally,
        ) {
            if let Some(offset) = tmux_last_offset {
                if let Some(watcher) = shared_owned.tmux_watchers.get(&channel_id) {
                    if let Ok(mut guard) = watcher.resume_offset.lock() {
                        *guard = Some(offset);
                    }
                    // NOTE: turn_delivered is NOT cleared here — the watcher clears it
                    // when it consumes resume_offset, ensuring the flag stays active
                    // until the watcher actually starts reading from the new offset.
                    watcher.paused.store(false, Ordering::Relaxed);
                }
            }
        }

        // Update in-memory session under lock.
        let session_id_to_persist = {
            let mut data = shared_owned.core.lock().await;
            if let Some(session) = data.sessions.get_mut(&channel_id) {
                if !session.cleared && !is_prompt_too_long {
                    if let Some(sid) = new_session_id {
                        session.session_id = Some(sid);
                    }
                    session.history.push(HistoryItem {
                        item_type: HistoryType::User,
                        content: user_text_owned.clone(),
                    });
                    session.history.push(HistoryItem {
                        item_type: HistoryType::Assistant,
                        content: full_response.clone(),
                    });
                    session.session_id.clone()
                } else {
                    None
                }
            } else {
                None
            }
        };

        // Persist provider session_id to DB so it survives dcserver restarts.
        if let (Some(ref session_key), Some(ref persisted_sid)) =
            (adk_session_key, session_id_to_persist)
        {
            super::adk_session::save_provider_session_id(
                session_key,
                persisted_sid,
                shared_owned.api_port,
            )
            .await;
        }

        // Clear restart report BEFORE clearing inflight state (which removes
        // the cancel token) to prevent the flush loop from processing the
        // report in the gap between cancel token removal and report deletion.
        if restart_followup_pending {
            clear_restart_report(&provider, channel_id.get());
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ✓ Cleared restart report for channel {} (turn completed normally)",
                channel_id
            );
        }

        clear_inflight_state(&provider, channel_id.get());
        // Defuse the guard — cleanup already done above.
        inflight_guard.provider.take();
        shared_owned.recovering_channels.remove(&channel_id);

        // For dispatch-based turns (threads), kill the tmux session after
        // finalization. Thread sessions are one-shot — keeping claude alive
        // in "Ready for input" blocks idle detection and the auto-complete pipeline.
        //
        // Exception (#145): unified-thread auto-queue runs reuse the same thread
        // session across multiple entries. Skip kill if the run is still active.
        #[cfg(unix)]
        if dispatch_id.is_some() {
            let should_kill = if let Some(ref did) = dispatch_id {
                !crate::dispatch::is_unified_thread_active(did)
            } else {
                true
            };
            if should_kill {
                if let Some(ref name) = cancel_token
                    .tmux_session
                    .lock()
                    .ok()
                    .and_then(|g| g.clone())
                {
                    record_tmux_exit_reason(
                        name,
                        "dispatch turn completed — killing thread session",
                    );
                    let name_c = name.to_string();
                    let kill_result = tokio::task::spawn_blocking(move || {
                        crate::services::platform::tmux::kill_session_output(&name_c)
                    })
                    .await;
                    let kill_ok = matches!(&kill_result, Ok(Ok(o)) if o.status.success());
                    if !kill_ok {
                        match &kill_result {
                            Ok(Ok(o)) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                eprintln!(
                                    "  [{ts}] ⚠ tmux kill-session failed for {}: {}",
                                    name,
                                    String::from_utf8_lossy(&o.stderr)
                                );
                            }
                            Ok(Err(e)) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                eprintln!("  [{ts}] ⚠ tmux kill-session error for {name}: {e}");
                            }
                            Err(e) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                eprintln!(
                                    "  [{ts}] ⚠ tmux kill-session spawn error for {name}: {e}"
                                );
                            }
                            _ => {}
                        }
                    }

                    // Only delete the DB session row if tmux kill succeeded.
                    // If kill failed, leave the row so the periodic reaper can retry.
                    if kill_ok {
                        if let Some(session_key) = super::adk_session::build_adk_session_key(
                            &shared_owned,
                            channel_id,
                            &provider,
                        )
                        .await
                        {
                            super::adk_session::delete_adk_session(
                                &session_key,
                                shared_owned.api_port,
                            )
                            .await;
                        }
                    }
                }
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] ♻ Skipping tmux kill for unified-thread dispatch {:?} — run still active",
                    dispatch_id
                );
            }
        }

        // Finalization complete — decrement counters
        shared_owned
            .finalizing_turns
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        shared_owned
            .global_finalizing
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        // Note: deferred restart exit is handled by the 5-second poll loop in mod.rs,
        // which saves pending queues before calling check_deferred_restart.
        // Calling it here would risk exiting before other providers save their queues.

        if has_queued_turns {
            // Drain mode: if restart is pending, don't start new turns from queue.
            // The queued messages will be saved to disk and processed after restart.
            if shared_owned
                .restart_pending
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] ⏸ DRAIN: skipping queued turn dequeue for channel {} (restart pending)",
                    channel_id
                );
            } else if let (Some(ctx), Some(owner), Some(tok)) =
                (serenity_ctx.as_ref(), request_owner, token.as_deref())
            {
                let (next_intervention, has_more_queued_turns) = {
                    let mut data = shared_owned.core.lock().await;
                    let mut remove_queue = false;
                    let next = if let Some(queue) = data.intervention_queue.get_mut(&channel_id) {
                        let next = super::dequeue_next_soft_intervention(queue);
                        let has_more = super::has_soft_intervention(queue);
                        remove_queue = queue.is_empty();
                        (next, has_more)
                    } else {
                        (None, false)
                    };
                    // Write-through: update disk after dequeue
                    if next.0.is_some() {
                        if remove_queue {
                            super::save_channel_queue(&provider, channel_id, &[]);
                        } else if let Some(q) = data.intervention_queue.get(&channel_id) {
                            super::save_channel_queue(&provider, channel_id, q);
                        }
                    }
                    if remove_queue {
                        data.intervention_queue.remove(&channel_id);
                    }
                    next
                };

                if let Some(intervention) = next_intervention {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] 📋 Processing next queued command");
                    // Remove 📬 (queued) reaction before processing
                    remove_reaction_raw(&http, channel_id, intervention.message_id, '📬').await;
                    if let Err(e) = handle_text_message(
                        ctx,
                        channel_id,
                        intervention.message_id,
                        owner,
                        &request_owner_name,
                        &intervention.text,
                        &shared_owned,
                        tok,
                        true,
                        has_more_queued_turns,
                        true,
                        None,
                    )
                    .await
                    {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts}]   ⚠ queued command failed: {e}");
                        let mut data = shared_owned.core.lock().await;
                        let queue = data.intervention_queue.entry(channel_id).or_default();
                        super::requeue_intervention_front(queue, intervention);
                    }
                }
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] 📦 preserving queued command(s): missing live Discord context — scheduling deferred drain"
                );
                if let Some(offset) = tmux_last_offset {
                    if let Some(watcher) = shared_owned.tmux_watchers.get(&channel_id) {
                        if let Ok(mut guard) = watcher.resume_offset.lock() {
                            *guard = Some(offset);
                        }
                        watcher.paused.store(false, Ordering::Relaxed);
                    }
                }
                // Deferred drain: wait briefly then kickoff idle queues using cached context
                let shared_for_drain = shared_owned.clone();
                let provider_for_drain = provider.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    if let (Some(ctx), Some(tok)) = (
                        shared_for_drain.cached_serenity_ctx.get(),
                        shared_for_drain.cached_bot_token.get(),
                    ) {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts}] 🚀 Deferred drain: kicking off idle queues");
                        super::kickoff_idle_queues(
                            ctx,
                            &shared_for_drain,
                            tok,
                            &provider_for_drain,
                        )
                        .await;
                    } else {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!(
                            "  [{ts}] ⚠ Deferred drain: still no cached context, queued messages remain pending"
                        );
                    }
                });
            }
        }

        // completion_tx is sent automatically by CompletionGuard on drop
    });
}

#[cfg(test)]
mod tests {
    use super::{
        build_verdict_payload, clear_local_session_state, extract_explicit_review_verdict,
        extract_review_decision, persisted_context_tokens, resolve_done_response,
        should_resume_watcher_after_turn, total_context_tokens,
    };
    use crate::services::discord::InflightTurnState;
    use crate::services::provider::ProviderKind;

    #[test]
    fn chained_batch_mid_turn_keeps_watcher_paused() {
        assert!(!should_resume_watcher_after_turn(true, false, false));
    }

    #[test]
    fn locally_chainable_queue_keeps_watcher_paused() {
        assert!(!should_resume_watcher_after_turn(false, true, true));
    }

    #[test]
    fn final_turn_without_remaining_queue_resumes_watcher() {
        assert!(should_resume_watcher_after_turn(false, false, true));
    }

    #[test]
    fn persisted_context_tokens_counts_input_and_output() {
        assert_eq!(persisted_context_tokens(610_000, 90_000), Some(700_000));
        assert_eq!(persisted_context_tokens(0, 0), None);
    }

    #[test]
    fn total_context_tokens_saturates_on_overflow() {
        assert_eq!(total_context_tokens(u64::MAX, 1), u64::MAX);
    }

    #[test]
    fn clear_local_session_state_drops_stale_resume_id_everywhere() {
        let mut new_session_id = Some("stale-session".to_string());
        let mut inflight_state = InflightTurnState::new(
            ProviderKind::Claude,
            1479671298497183835,
            Some("adk-cc".to_string()),
            343742347365974026,
            1,
            2,
            "resume me".to_string(),
            Some("stale-session".to_string()),
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        );

        clear_local_session_state(&mut new_session_id, &mut inflight_state);

        assert_eq!(new_session_id, None);
        assert_eq!(inflight_state.session_id, None);
    }

    #[test]
    fn explicit_review_verdict_parser_accepts_structured_marker() {
        assert_eq!(
            extract_explicit_review_verdict("VERDICT: pass\nNo findings."),
            Some("pass")
        );
        assert_eq!(
            extract_explicit_review_verdict("overall: improve\nNeeds work."),
            Some("improve")
        );
    }

    #[test]
    fn explicit_review_verdict_parser_ignores_unstructured_text() {
        assert_eq!(
            extract_explicit_review_verdict("검토 완료. 전반적으로 좋아 보입니다."),
            None
        );
    }

    #[test]
    fn review_decision_parser_accepts_explicit_marker() {
        assert_eq!(
            extract_review_decision("DECISION: accept\n리뷰 반영하겠습니다."),
            Some("accept")
        );
        assert_eq!(
            extract_review_decision("결정: dismiss\n이 리뷰는 무시합니다."),
            Some("dismiss")
        );
        assert_eq!(
            extract_review_decision("Decision: dispute\n반론을 제기합니다."),
            Some("dispute")
        );
    }

    #[test]
    fn review_decision_parser_accepts_keyword_in_tail() {
        assert_eq!(
            extract_review_decision("리뷰 내용을 검토한 결과 수정이 필요합니다.\n\naccept"),
            Some("accept")
        );
        assert_eq!(
            extract_review_decision("불필요한 변경이므로 dismiss 합니다."),
            Some("dismiss")
        );
    }

    #[test]
    fn review_decision_parser_rejects_ambiguous_keywords() {
        // Multiple different keywords → ambiguous, return None
        assert_eq!(
            extract_review_decision("accept or dismiss 중 선택해야 합니다."),
            None
        );
    }

    #[test]
    fn review_decision_parser_ignores_unstructured_text() {
        assert_eq!(
            extract_review_decision("리뷰 피드백을 확인했습니다. 코드를 수정하겠습니다."),
            None
        );
    }

    #[test]
    fn review_decision_explicit_marker_takes_priority() {
        // Even with keywords in tail, explicit marker should be found first
        assert_eq!(
            extract_review_decision("DECISION: accept\n이 dismiss는 무시해도 됩니다."),
            Some("accept")
        );
    }

    #[test]
    fn review_decision_parser_handles_korean_text_over_500_bytes() {
        // Korean chars are 3 bytes each in UTF-8; build a response > 500 bytes
        // to exercise the safe_suffix path without panicking
        let padding = "가".repeat(200); // 600 bytes of Korean text
        let response = format!("{padding}\ndismiss");
        assert_eq!(extract_review_decision(&response), Some("dismiss"));
    }

    #[test]
    fn verdict_fallback_payload_includes_provider() {
        let payload = build_verdict_payload("d-123", "pass", "LGTM", "codex");
        assert_eq!(payload["dispatch_id"], "d-123");
        assert_eq!(payload["overall"], "pass");
        assert_eq!(payload["feedback"], "LGTM");
        assert_eq!(payload["provider"], "codex");
    }

    #[test]
    fn verdict_fallback_payload_truncates_long_feedback() {
        let long_response = "x".repeat(5000);
        let payload = build_verdict_payload("d-456", "improve", &long_response, "claude");
        assert_eq!(payload["provider"], "claude");
        let feedback = payload["feedback"].as_str().unwrap();
        assert!(feedback.len() <= 4003); // 4000 + "..." ellipsis
    }

    // ========== resolve_done_response tests ==========

    #[test]
    fn done_replaces_stale_pre_tool_text_with_result() {
        // Text → ToolUse → Done(result): intermediate text should be replaced
        let res =
            resolve_done_response("이슈를 생성합니다.\n\n", "이슈 #90 생성 완료", true, false);
        assert_eq!(res, Some("이슈 #90 생성 완료".to_string()));
    }

    #[test]
    fn done_keeps_full_response_when_post_tool_text_exists() {
        // Text → ToolUse → Text → Done(result): streaming captured everything
        let res = resolve_done_response(
            "진행 중...\n\n이슈 #90 생성 완료",
            "이슈 #90 생성 완료",
            true,
            true,
        );
        assert_eq!(res, None); // keep full_response as-is
    }

    #[test]
    fn done_uses_result_when_full_response_empty() {
        let res = resolve_done_response("", "최종 응답", false, false);
        assert_eq!(res, Some("최종 응답".to_string()));
    }

    #[test]
    fn done_uses_result_when_full_response_whitespace_only() {
        let res = resolve_done_response("  \n\n  ", "최종 응답", true, false);
        assert_eq!(res, Some("최종 응답".to_string()));
    }

    #[test]
    fn done_keeps_full_response_when_no_tools_used() {
        // Pure text turn without tools — streaming text IS the final response
        let res = resolve_done_response(
            "여기 분석 결과입니다...",
            "여기 분석 결과입니다...",
            false,
            false,
        );
        assert_eq!(res, None);
    }

    #[test]
    fn done_noop_when_result_empty() {
        // Synthetic Done with empty result — nothing to replace with
        let res = resolve_done_response("중간 텍스트\n\n", "", true, false);
        assert_eq!(res, None);
    }
}
