use super::thread_reuse::clear_all_threads;

// ── Outbox worker trait ───────────────────────────────────────

/// Trait for outbox side-effects (Discord notifications, followups).
/// Extracted from `dispatch_outbox_loop` to allow mock injection in tests.
pub(crate) trait OutboxNotifier: Send + Sync {
    fn notify_dispatch(
        &self,
        db: crate::db::Db,
        agent_id: String,
        title: String,
        card_id: String,
        dispatch_id: String,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;

    fn handle_followup(
        &self,
        db: crate::db::Db,
        dispatch_id: String,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

/// Production notifier that calls the real Discord functions.
pub(crate) struct RealOutboxNotifier;

impl OutboxNotifier for RealOutboxNotifier {
    async fn notify_dispatch(
        &self,
        db: crate::db::Db,
        agent_id: String,
        title: String,
        card_id: String,
        dispatch_id: String,
    ) -> Result<(), String> {
        super::discord_delivery::send_dispatch_to_discord(
            &db,
            &agent_id,
            &title,
            &card_id,
            &dispatch_id,
        )
        .await
    }

    async fn handle_followup(&self, db: crate::db::Db, dispatch_id: String) -> Result<(), String> {
        handle_completed_dispatch_followups(&db, &dispatch_id).await
    }
}

/// Backoff delays for outbox retries: 1m → 5m → 15m → 1h
const RETRY_BACKOFF_SECS: [i64; 4] = [60, 300, 900, 3600];
/// Maximum number of retries before marking as permanent failure.
const MAX_RETRY_COUNT: i32 = 4;

/// Process one batch of pending outbox entries.
/// Returns the number of entries processed (0 if queue was empty).
///
/// Retry/backoff policy (#209):
/// - On notifier success: mark entry as 'done'
/// - On notifier failure (retry_count < MAX_RETRY_COUNT): increment retry_count,
///   set next_attempt_at with exponential backoff, revert to 'pending'
/// - On max retry exceeded: mark as 'failed' (permanent failure)
/// - For 'notify' actions: manages dispatch_notified reservation atomically
pub(crate) async fn process_outbox_batch<N: OutboxNotifier>(
    db: &crate::db::Db,
    notifier: &N,
) -> usize {
    let pending: Vec<(
        i64,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        i32,
    )> = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return 0,
        };
        let mut stmt = match conn.prepare(
            "SELECT id, dispatch_id, action, agent_id, card_id, title, retry_count \
             FROM dispatch_outbox \
             WHERE status = 'pending' \
               AND (next_attempt_at IS NULL OR next_attempt_at <= datetime('now')) \
             ORDER BY id ASC LIMIT 5",
        ) {
            Ok(s) => s,
            Err(_) => return 0,
        };
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    };

    let count = pending.len();
    for (id, dispatch_id, action, agent_id, card_id, title, retry_count) in pending {
        // Mark as processing
        if let Ok(conn) = db.lock() {
            conn.execute(
                "UPDATE dispatch_outbox SET status = 'processing' WHERE id = ?1",
                [id],
            )
            .ok();
        }

        let result = match action.as_str() {
            "notify" => {
                if let (Some(aid), Some(cid), Some(t)) =
                    (agent_id.clone(), card_id.clone(), title.clone())
                {
                    // Two-phase delivery guard (reservation + notified marker) is handled
                    // inside send_dispatch_to_discord, protecting all callers uniformly.
                    notifier
                        .notify_dispatch(db.clone(), aid, t, cid, dispatch_id.clone())
                        .await
                } else {
                    Err("missing agent_id, card_id, or title for notify action".into())
                }
            }
            "followup" => {
                notifier
                    .handle_followup(db.clone(), dispatch_id.clone())
                    .await
            }
            other => {
                tracing::warn!("[dispatch-outbox] Unknown action: {other}");
                Err(format!("unknown action: {other}"))
            }
        };

        match result {
            Ok(()) => {
                // Mark done
                if let Ok(conn) = db.lock() {
                    conn.execute(
                        "UPDATE dispatch_outbox SET status = 'done', processed_at = datetime('now') WHERE id = ?1",
                        [id],
                    )
                    .ok();
                }
            }
            Err(err) => {
                let new_count = retry_count + 1;
                if new_count > MAX_RETRY_COUNT {
                    // Permanent failure — exhausted all 4 retries (1m → 5m → 15m → 1h)
                    tracing::error!(
                        "[dispatch-outbox] Permanent failure for entry {id} (dispatch={dispatch_id}, action={action}): {err}"
                    );
                    if let Ok(conn) = db.lock() {
                        conn.execute(
                            "UPDATE dispatch_outbox SET status = 'failed', error = ?1, \
                             retry_count = ?2, processed_at = datetime('now') WHERE id = ?3",
                            rusqlite::params![err, new_count, id],
                        )
                        .ok();
                    }
                } else {
                    // Schedule retry with backoff (index = new_count - 1, since retry 1 uses BACKOFF[0])
                    let backoff_idx = (new_count - 1) as usize;
                    let backoff_secs = RETRY_BACKOFF_SECS.get(backoff_idx).copied().unwrap_or(3600);
                    tracing::warn!(
                        "[dispatch-outbox] Retry {new_count}/{MAX_RETRY_COUNT} for entry {id} (dispatch={dispatch_id}, action={action}) \
                         in {backoff_secs}s: {err}",
                    );
                    if let Ok(conn) = db.lock() {
                        conn.execute(
                            "UPDATE dispatch_outbox SET status = 'pending', error = ?1, \
                             retry_count = ?2, \
                             next_attempt_at = datetime('now', '+' || ?3 || ' seconds') \
                             WHERE id = ?4",
                            rusqlite::params![err, new_count, backoff_secs, id],
                        )
                        .ok();
                    }
                }
            }
        }
    }
    count
}

// ── Followup & verdict helpers ──────────────────────────────────

pub(super) fn extract_review_verdict(result_json: Option<&str>) -> String {
    result_json
        .and_then(|r| serde_json::from_str::<serde_json::Value>(r).ok())
        .and_then(|v| {
            v.get("verdict")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string())
                .or_else(|| {
                    v.get("decision")
                        .and_then(|s| s.as_str())
                        .map(|s| s.to_string())
                })
        })
        // NEVER default to "pass" — missing verdict means the review agent
        // did not submit a verdict (e.g. session idle auto-complete).
        // Returning "unknown" forces the followup path to request human/agent review.
        .unwrap_or_else(|| "unknown".to_string())
}

/// Send Discord notifications for a completed dispatch (review verdicts, etc.).
/// Callers of `finalize_dispatch` should spawn this after the sync call returns.
pub(crate) async fn handle_completed_dispatch_followups(
    db: &crate::db::Db,
    dispatch_id: &str,
) -> Result<(), String> {
    let info: Option<(String, String, String, String, String, Option<String>)> = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return Err("db lock failed for dispatch lookup".into()),
        };
        conn.query_row(
            "SELECT td.dispatch_type, td.status, kc.id, COALESCE(kc.assigned_agent_id, ''), kc.title, td.result \
             FROM task_dispatches td \
             JOIN kanban_cards kc ON kc.id = td.kanban_card_id \
             WHERE td.id = ?1",
            [dispatch_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .ok()
    };

    let Some((dispatch_type, status, card_id, _agent_id, _title, result_json)) = info else {
        return Err(format!("dispatch {dispatch_id} not found"));
    };
    if status != "completed" {
        return Ok(()); // Not an error — dispatch not yet completed
    }

    if dispatch_type == "review" {
        let verdict = extract_review_verdict(result_json.as_deref());
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!(
            "  [{ts}] 🔍 REVIEW-FOLLOWUP: dispatch={dispatch_id} verdict={verdict} result={:?}",
            result_json.as_deref().unwrap_or("NULL")
        );
        // Skip Discord notification for auto-completed reviews without an explicit verdict.
        // The policy engine's onDispatchCompleted hook handles those (review-automation.js).
        // Only send_review_result_to_primary for explicit verdicts (pass/improve/reject)
        // submitted via the verdict API — these have a real "verdict" field in the result.
        if verdict != "unknown" {
            super::discord_delivery::send_review_result_to_primary(
                db,
                &card_id,
                dispatch_id,
                &verdict,
            )
            .await?;
        } else {
            println!(
                "  [{ts}] ⏭ REVIEW-FOLLOWUP: skipping send_review_result_to_primary (verdict=unknown)"
            );
        }
    }

    // Archive thread on dispatch completion — but only if the card is done.
    // When the card has an active lifecycle (not done), keep the thread open for reuse
    // by subsequent dispatches (rework, review-decision, etc.).
    let card_status: Option<String> = db.lock().ok().and_then(|conn| {
        conn.query_row(
            "SELECT status FROM kanban_cards WHERE id = ?1",
            [&card_id],
            |row| row.get(0),
        )
        .ok()
    });
    let should_archive = card_status.as_deref() == Some("done");

    if should_archive {
        let thread_id: Option<String> = {
            let conn = match db.lock() {
                Ok(c) => c,
                Err(_) => return Ok(()), // Best effort — archiving is not critical
            };
            conn.query_row(
                "SELECT COALESCE(thread_id, json_extract(context, '$.thread_id')) FROM task_dispatches WHERE id = ?1",
                [dispatch_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
        };
        if let Some(ref tid) = thread_id {
            if let Some(token) = crate::credential::read_bot_token("announce") {
                let archive_url = format!("https://discord.com/api/v10/channels/{}", tid);
                let client = reqwest::Client::new();
                let _ = client
                    .patch(&archive_url)
                    .header("Authorization", format!("Bot {}", token))
                    .json(&serde_json::json!({"archived": true}))
                    .send()
                    .await;
                tracing::info!(
                    "[dispatch] Archived thread {tid} for completed dispatch {dispatch_id} (card done)"
                );
            }
        }
        // Clear all thread mappings when card is done
        if let Ok(conn) = db.lock() {
            clear_all_threads(&conn, &card_id);
        }
    }

    // Generic resend removed — dispatch Discord notification is handled by:
    // 1. kanban.rs fire_transition_hooks → onCardTransition → send_dispatch_to_discord
    // 2. timeouts.js [I-0] recovery for unnotified dispatches
    // 3. dispatch_notified guard in process_outbox_batch prevents duplicates
    // Previously this generic resend caused 2-3x duplicate messages for every dispatch.
    Ok(())
}

// ── Channel helpers ─────────────────────────────────────────────

/// Resolve a channel name alias (e.g. "adk-cc") to a numeric channel ID.
/// Public wrapper around the shared resolve_channel_alias.
pub fn resolve_channel_alias_pub(alias: &str) -> Option<u64> {
    super::resolve_channel_alias(alias)
}

pub(crate) fn use_counter_model_channel(dispatch_type: Option<&str>) -> bool {
    // "review", "e2e-test" (#197), and "consultation" (#256) go to the counter-model channel.
    // "review-decision" is routed back to the original implementation provider
    // so it reuses the implementation-side thread rather than the reviewer channel.
    matches!(
        dispatch_type,
        Some("review") | Some("e2e-test") | Some("consultation")
    )
}

// ── Message formatting ──────────────────────────────────────────

pub(super) fn format_dispatch_message(
    dispatch_id: &str,
    title: &str,
    issue_url: Option<&str>,
    issue_number: Option<i64>,
    use_alt: bool,
    reviewed_commit: Option<&str>,
    target_provider: Option<&str>,
    review_branch: Option<&str>,
    dispatch_type: Option<&str>,
    dispatch_context: Option<&str>,
) -> String {
    let context_json = dispatch_context
        .and_then(|ctx| serde_json::from_str::<serde_json::Value>(ctx).ok())
        .unwrap_or_else(|| serde_json::json!({}));

    // Format issue link as markdown hyperlink with angle brackets to suppress embed
    let issue_link = match (issue_url, issue_number) {
        (Some(url), Some(num)) => format!("[{title} #{num}](<{url}>)"),
        (Some(url), None) => format!("[{title}](<{url}>)"),
        _ => String::new(),
    };

    // Build dispatch type label and reason line
    let type_label = match dispatch_type {
        Some("implementation") => "📋 구현",
        Some("review") => "🔍 리뷰",
        Some("rework") => "🔧 리워크",
        Some("review-decision") => "⚖️ 리뷰 검토",
        Some("pm-decision") => "🎯 PM 판단",
        Some("e2e-test") => "🧪 E2E 테스트",
        Some(other) => other,
        None => "dispatch",
    };

    // Extract reason from context JSON
    let reason = context_json
        .get("resumed_from")
        .and_then(|r| r.as_str())
        .map(|s| format!("resume from {s}"))
        .or_else(|| {
            if context_json
                .get("retry")
                .and_then(|r| r.as_bool())
                .unwrap_or(false)
            {
                Some("retry".to_string())
            } else if context_json
                .get("redispatch")
                .and_then(|r| r.as_bool())
                .unwrap_or(false)
            {
                Some("redispatch".to_string())
            } else if context_json
                .get("auto_queue")
                .and_then(|r| r.as_bool())
                .unwrap_or(false)
            {
                Some("auto-queue".to_string())
            } else if context_json
                .get("auto_accept")
                .and_then(|r| r.as_bool())
                .unwrap_or(false)
            {
                Some("auto-accept rework".to_string())
            } else {
                None
            }
        });

    let reason_suffix = reason.map(|r| format!(" ({r})")).unwrap_or_default();
    let review_verdict = context_json
        .get("verdict")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    if use_alt {
        let mut message = format!(
            "DISPATCH:{dispatch_id} [{type_label}] - {title}\n\
             ⚠️ 검토 전용 — 작업 착수 금지\n\
             코드 리뷰만 수행하고 GitHub 이슈에 코멘트로 피드백해주세요."
        );
        if !issue_link.is_empty() {
            message.push('\n');
            message.push_str(&issue_link);
        }
        // #193: Include branch info so reviewer inspects the correct code
        if let Some(branch) = review_branch {
            let short_commit = reviewed_commit.map(|c| &c[..8.min(c.len())]).unwrap_or("?");
            message.push_str(&format!(
                "\n\n리뷰 대상 브랜치: `{branch}` (commit: `{short_commit}`)\n\
                 반드시 해당 브랜치를 checkout하여 리뷰하세요. main 브랜치가 아닙니다."
            ));
        }
        // Append verdict API call instructions for the counter-model reviewer
        let commit_arg = reviewed_commit
            .map(|c| format!(r#","commit":"{}""#, c))
            .unwrap_or_default();
        let provider_arg = target_provider
            .map(|p| format!(r#","provider":"{}""#, p))
            .unwrap_or_default();
        let base_url = crate::config::local_api_url(crate::config::load_graceful().server.port, "");
        message.push_str(&format!(
            "\n---\n\
             응답 첫 줄에 반드시 `VERDICT: pass|improve|reject|rework` 중 하나를 적으세요.\n\
             verdict API가 200 OK로 호출되기 전까지 리뷰는 완료로 간주되지 않습니다.\n\
             `improve`/`reject`/`rework` 시 반드시 `notes`에 구체적 피드백을, `items`에 개별 지적 사항을 포함하세요.\n\
             리뷰 완료 후 verdict API를 호출하세요:\n\
             `curl -sf -X POST {base_url}/api/review-verdict \
             -H \"Content-Type: application/json\" \
             -d '{{\"dispatch_id\":\"{dispatch_id}\",\"overall\":\"pass|improve|reject|rework\",\
             \"notes\":\"리뷰 피드백 요약\",\
             \"items\":[{{\"category\":\"bug|style|perf|security|logic\",\"summary\":\"개별 지적 사항\"}}]\
             {commit_arg}{provider_arg}}}'`"
        ));
        message
    } else if dispatch_type == Some("review-decision") {
        let mut message = format!(
            "DISPATCH:{dispatch_id} [{type_label}] - {title}\n\
             ⛔ 코드 리뷰 금지 — 이미 완료된 리뷰 결과를 검토하는 단계입니다\n\
             📝 카운터모델 리뷰 결과: **{review_verdict}**\n\
             GitHub 이슈 코멘트에서 피드백을 확인하고 다음 중 하나를 선택하세요:\n\
             • **수용** → 피드백 반영 수정 후 review-decision API에 `accept` 호출\n\
             • **반론** → GitHub 코멘트로 이의 제기 후 review-decision API에 `dispute` 호출\n\
             • **무시** → review-decision API에 `dismiss` 호출"
        );
        if !issue_link.is_empty() {
            message.push('\n');
            message.push_str(&issue_link);
        }
        message
    } else if !issue_link.is_empty() {
        format!("DISPATCH:{dispatch_id} [{type_label}] - {title}{reason_suffix}\n{issue_link}")
    } else {
        format!("DISPATCH:{dispatch_id} [{type_label}] - {title}{reason_suffix}")
    }
}

pub(super) fn prefix_dispatch_message(dispatch_type: &str, message: &str) -> String {
    format!("── {} dispatch ──\n{}", dispatch_type, message)
}

// ── #144: Dispatch Notification Outbox ───────────────────────

/// Queue a dispatch completion followup for async processing.
///
/// Replaces `tokio::spawn(handle_completed_dispatch_followups(...))`.
pub(crate) fn queue_dispatch_followup(db: &crate::db::Db, dispatch_id: &str) {
    if let Ok(conn) = db.separate_conn() {
        // Dedup: skip if a followup entry already exists for this dispatch
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM dispatch_outbox WHERE dispatch_id = ?1 AND action = 'followup'",
                [dispatch_id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if exists {
            return;
        }
        conn.execute(
            "INSERT INTO dispatch_outbox (dispatch_id, action) VALUES (?1, 'followup')",
            [dispatch_id],
        )
        .ok();
    }
}

/// Worker loop that drains dispatch_outbox and executes Discord side-effects.
///
/// This is the SINGLE place where dispatch-related Discord HTTP calls originate.
/// All other code paths insert into the outbox table and return immediately.
pub(crate) async fn dispatch_outbox_loop(db: crate::db::Db) {
    use std::time::Duration;

    // Wait for server to be ready
    tokio::time::sleep(Duration::from_secs(3)).await;
    tracing::info!("[dispatch-outbox] Worker started (adaptive backoff 500ms-5s)");

    let notifier = RealOutboxNotifier;
    let mut poll_interval = Duration::from_millis(500);
    let max_interval = Duration::from_secs(5);

    loop {
        tokio::time::sleep(poll_interval).await;

        let processed = process_outbox_batch(&db, &notifier).await;
        if processed == 0 {
            poll_interval = (poll_interval.mul_f64(1.5)).min(max_interval);
        } else {
            poll_interval = Duration::from_millis(500);
        }
    }
}
