use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashSet;

use super::AppState;

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct GenerateBody {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub mode: Option<String>, // "priority-sort" (default) or "dependency-aware"
}

#[derive(Debug, Deserialize)]
pub struct ActivateBody {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
    pub unified_thread: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct StatusQuery {
    pub repo: Option<String>,
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ReorderBody {
    #[serde(rename = "orderedIds")]
    pub ordered_ids: Vec<String>,
    #[serde(rename = "agentId")]
    pub agent_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateRunBody {
    pub status: Option<String>,
    pub unified_thread: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct EnqueueBody {
    pub repo: String,
    pub issue_number: i64,
    pub agent_id: Option<String>,
}

// ── Helpers ──────────────────────────────────────────────────────────────────

fn ensure_tables(conn: &rusqlite::Connection) {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS auto_queue_runs (
            id          TEXT PRIMARY KEY,
            repo        TEXT,
            agent_id    TEXT,
            status      TEXT DEFAULT 'active',
            ai_model    TEXT,
            ai_rationale TEXT,
            timeout_minutes INTEGER DEFAULT 120,
            unified_thread  INTEGER DEFAULT 0,
            unified_thread_id TEXT,
            unified_thread_channel_id TEXT,
            created_at  DATETIME DEFAULT CURRENT_TIMESTAMP,
            completed_at DATETIME
        );
        CREATE TABLE IF NOT EXISTS auto_queue_entries (
            id              TEXT PRIMARY KEY,
            run_id          TEXT REFERENCES auto_queue_runs(id),
            kanban_card_id  TEXT REFERENCES kanban_cards(id),
            agent_id        TEXT,
            priority_rank   INTEGER DEFAULT 0,
            reason          TEXT,
            status          TEXT DEFAULT 'pending',
            dispatch_id     TEXT,
            created_at      DATETIME DEFAULT CURRENT_TIMESTAMP,
            dispatched_at   DATETIME,
            completed_at    DATETIME
        );",
    )
    .ok();
    // #137: upgrade path for existing DBs
    let has_unified: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('auto_queue_runs') WHERE name = 'unified_thread'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if !has_unified {
        conn.execute_batch(
            "ALTER TABLE auto_queue_runs ADD COLUMN unified_thread INTEGER DEFAULT 0;
             ALTER TABLE auto_queue_runs ADD COLUMN unified_thread_id TEXT;
             ALTER TABLE auto_queue_runs ADD COLUMN unified_thread_channel_id TEXT;",
        )
        .ok();
    }
    // #145: dispatch_id on entries — direct dispatch→run association
    let has_dispatch_id: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('auto_queue_entries') WHERE name = 'dispatch_id'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if !has_dispatch_id {
        conn.execute_batch(
            "ALTER TABLE auto_queue_entries ADD COLUMN dispatch_id TEXT;",
        )
        .ok();
    }
}

fn entry_to_json(conn: &rusqlite::Connection, entry_id: &str) -> serde_json::Value {
    conn.query_row(
        "SELECT e.id, e.agent_id, e.kanban_card_id, e.priority_rank, e.reason, e.status,
                CAST(strftime('%s', e.created_at) AS INTEGER) * 1000,
                CASE WHEN e.dispatched_at IS NOT NULL THEN CAST(strftime('%s', e.dispatched_at) AS INTEGER) * 1000 END,
                CASE WHEN e.completed_at IS NOT NULL THEN CAST(strftime('%s', e.completed_at) AS INTEGER) * 1000 END,
                kc.title, kc.github_issue_number, kc.github_issue_url
         FROM auto_queue_entries e
         LEFT JOIN kanban_cards kc ON e.kanban_card_id = kc.id
         WHERE e.id = ?1",
        [entry_id],
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "agent_id": row.get::<_, String>(1)?,
                "card_id": row.get::<_, String>(2)?,
                "priority_rank": row.get::<_, i64>(3)?,
                "reason": row.get::<_, Option<String>>(4)?,
                "status": row.get::<_, String>(5)?,
                "created_at": row.get::<_, Option<i64>>(6)?.unwrap_or(0),
                "dispatched_at": row.get::<_, Option<i64>>(7)?,
                "completed_at": row.get::<_, Option<i64>>(8)?,
                "card_title": row.get::<_, Option<String>>(9)?,
                "github_issue_number": row.get::<_, Option<i64>>(10)?,
                "github_repo": row.get::<_, Option<String>>(11)?,
            }))
        },
    )
    .unwrap_or(json!(null))
}

fn run_to_json(conn: &rusqlite::Connection, run_id: &str) -> serde_json::Value {
    conn.query_row(
        "SELECT id, repo, agent_id, status, timeout_minutes,
                ai_model, ai_rationale,
                CAST(strftime('%s', created_at) AS INTEGER) * 1000,
                CASE WHEN completed_at IS NOT NULL THEN CAST(strftime('%s', completed_at) AS INTEGER) * 1000 END,
                unified_thread, unified_thread_id
         FROM auto_queue_runs WHERE id = ?1",
        [run_id],
        |row| {
            Ok(json!({
                "id": row.get::<_, String>(0)?,
                "repo": row.get::<_, Option<String>>(1)?,
                "agent_id": row.get::<_, Option<String>>(2)?,
                "status": row.get::<_, String>(3)?,
                "timeout_minutes": row.get::<_, i64>(4)?,
                "ai_model": row.get::<_, Option<String>>(5)?,
                "ai_rationale": row.get::<_, Option<String>>(6)?,
                "created_at": row.get::<_, Option<i64>>(7)?.unwrap_or(0),
                "completed_at": row.get::<_, Option<i64>>(8)?,
                "unified_thread": row.get::<_, i64>(9).unwrap_or(0) != 0,
                "unified_thread_id": row.get::<_, Option<String>>(10)?,
            }))
        },
    )
    .unwrap_or(json!(null))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueueEntryOrder {
    id: String,
    status: String,
    agent_id: String,
}

fn reorder_entry_ids(
    entries: &[QueueEntryOrder],
    ordered_ids: &[String],
    agent_id: Option<&str>,
) -> Result<Vec<String>, String> {
    if ordered_ids.is_empty() {
        return Err("orderedIds cannot be empty".to_string());
    }

    let scope_ids: Vec<String> = entries
        .iter()
        .filter(|entry| {
            entry.status == "pending"
                && agent_id
                    .map(|target| entry.agent_id == target)
                    .unwrap_or(true)
        })
        .map(|entry| entry.id.clone())
        .collect();
    if scope_ids.is_empty() {
        return Err("no pending entries found for reorder scope".to_string());
    }

    let scope_set: HashSet<&str> = scope_ids.iter().map(String::as_str).collect();
    let mut seen = HashSet::new();
    let mut replacement_ids = Vec::new();
    for id in ordered_ids {
        let id_str = id.as_str();
        if scope_set.contains(id_str) && seen.insert(id_str) {
            replacement_ids.push(id.clone());
        }
    }
    if replacement_ids.is_empty() {
        return Err("orderedIds do not match any pending entries in scope".to_string());
    }

    for id in &scope_ids {
        if !seen.contains(id.as_str()) {
            replacement_ids.push(id.clone());
        }
    }

    let mut replacement_iter = replacement_ids.into_iter();
    let mut reordered = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.status == "pending"
            && agent_id
                .map(|target| entry.agent_id == target)
                .unwrap_or(true)
        {
            let next_id = replacement_iter
                .next()
                .ok_or_else(|| "replacement sequence exhausted".to_string())?;
            reordered.push(next_id);
        } else {
            reordered.push(entry.id.clone());
        }
    }

    if replacement_iter.next().is_some() {
        return Err("replacement sequence was not fully consumed".to_string());
    }

    Ok(reordered)
}

// ── Endpoints ────────────────────────────────────────────────────────────────

/// POST /api/auto-queue/generate
/// Creates a queue run from ready cards, ordered by priority.
pub async fn generate(
    State(state): State<AppState>,
    Json(body): Json<GenerateBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);

    // Build filter — pipeline-driven dispatchable states
    crate::pipeline::ensure_loaded();
    let dispatchable = crate::pipeline::try_get()
        .map(|p| {
            p.dispatchable_states()
                .iter()
                .map(|s| format!("'{}'", s))
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_else(|| "'ready'".to_string());
    let mut conditions = vec![format!("kc.status IN ({})", dispatchable)];
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(ref repo) = body.repo {
        conditions.push(format!("kc.repo_id = ?{}", params.len() + 1));
        params.push(Box::new(repo.clone()));
    }
    if let Some(ref agent_id) = body.agent_id {
        conditions.push(format!("kc.assigned_agent_id = ?{}", params.len() + 1));
        params.push(Box::new(agent_id.clone()));
    }

    let where_clause = conditions.join(" AND ");
    let sql = format!(
        "SELECT kc.id, kc.assigned_agent_id, kc.priority, kc.title
         FROM kanban_cards kc
         WHERE {where_clause}
         ORDER BY
           CASE kc.priority
             WHEN 'urgent' THEN 0
             WHEN 'high' THEN 1
             WHEN 'medium' THEN 2
             WHEN 'low' THEN 3
             ELSE 4
           END,
           kc.created_at ASC"
    );

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let mut stmt = match conn.prepare(&sql) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let cards: Vec<(String, String, String)> = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                row.get::<_, Option<String>>(2)?
                    .unwrap_or_else(|| "medium".to_string()),
            ))
        })
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    if cards.is_empty() {
        // Provide context: how many cards are in backlog vs other statuses
        // Uses the same repo + agent_id filters as the main ready query
        let count_with_filters = |status_val: &str| -> i64 {
            let mut sql = format!(
                "SELECT COUNT(*) FROM kanban_cards WHERE status = '{}'",
                status_val
            );
            let mut count_params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            if let Some(ref repo) = body.repo {
                count_params.push(Box::new(repo.clone()));
                sql.push_str(&format!(" AND repo_id = ?{}", count_params.len()));
            }
            if let Some(ref agent_id) = body.agent_id {
                count_params.push(Box::new(agent_id.clone()));
                sql.push_str(&format!(" AND assigned_agent_id = ?{}", count_params.len()));
            }
            let refs: Vec<&dyn rusqlite::types::ToSql> =
                count_params.iter().map(|p| p.as_ref()).collect();
            conn.query_row(&sql, refs.as_slice(), |row| row.get(0))
                .unwrap_or(0)
        };
        // Pipeline-driven counts: report all non-terminal states
        let mut counts_map = serde_json::Map::new();
        if let Some(pipeline) = crate::pipeline::try_get() {
            for state in &pipeline.states {
                if !state.terminal {
                    let c: i64 = count_with_filters(&state.id);
                    counts_map.insert(state.id.clone(), serde_json::json!(c));
                }
            }
        }
        return (
            StatusCode::OK,
            Json(json!({
                "run": null,
                "entries": [],
                "message": "No dispatchable cards found",
                "hint": "Move cards to a dispatchable state before generating a queue.",
                "counts": counts_map,
            })),
        );
    }

    let mode = body.mode.as_deref().unwrap_or("priority-sort");

    // PM-assisted mode: send card list to PMD for async analysis
    if mode == "pm-assisted" {
        // Create pending run
        let run_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id, status, ai_model, ai_rationale) VALUES (?1, ?2, ?3, 'pending', 'pm-assisted', ?4)",
            rusqlite::params![run_id, body.repo, body.agent_id, format!("PMD 분석 대기 중 — {}개 카드 제출", cards.len())],
        ).ok();

        // Collect card info for PMD request
        let mut card_summaries = Vec::new();
        for (card_id, agent_id, priority) in &cards {
            let (title, issue_num): (String, Option<i64>) = conn
                .query_row(
                    "SELECT title, github_issue_number FROM kanban_cards WHERE id = ?1",
                    [card_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap_or_default();
            card_summaries.push(format!(
                "- #{} {} (priority: {}, agent: {})",
                issue_num.unwrap_or(0),
                title,
                priority,
                agent_id
            ));
        }

        let run_id_for_spawn = run_id.clone();
        let card_list_text = card_summaries.join("\n");
        let repo_name = body.repo.clone().unwrap_or_else(|| "all".to_string());
        drop(stmt);
        let run = run_to_json(&conn, &run_id);
        drop(conn);

        // Async: send PMD request via announce bot
        tokio::spawn(async move {
            let token = match crate::credential::read_bot_token("announce") {
                Some(t) => t,
                None => return,
            };

            // Kanban manager channel from config (kv_meta)
            let km_channel: Option<String> = {
                let conn = state.db.separate_conn().ok();
                conn.and_then(|c| {
                    c.query_row(
                        "SELECT value FROM kv_meta WHERE key = 'kanban_manager_channel_id'",
                        [],
                        |row| row.get(0),
                    )
                    .ok()
                })
            };
            let Some(km_channel) = km_channel else {
                tracing::warn!(
                    "[auto-queue] No kanban_manager_channel_id configured, skipping PM request"
                );
                return;
            };

            // Resolve channel name to ID if needed
            let km_channel_num: u64 = match km_channel.parse() {
                Ok(n) => n,
                Err(_) => {
                    match crate::server::routes::dispatches::resolve_channel_alias_pub(&km_channel)
                    {
                        Some(n) => n,
                        None => return,
                    }
                }
            };

            let message = format!(
                "[칸반매니저] 자동큐 순서 분석 요청\n\n\
                 repo: {}\n\
                 run_id: {}\n\n\
                 아래 일감들의 실행 순서를 분석해주세요.\n\
                 의존관계, 긴급도, 작업 내용을 고려하여 순서를 결정하고,\n\
                 `POST /api/auto-queue/runs/{}/order`에 결과를 전달해주세요.\n\n\
                 {}",
                repo_name, run_id_for_spawn, run_id_for_spawn, card_list_text
            );

            let client = reqwest::Client::new();
            let _ = client
                .post(format!(
                    "https://discord.com/api/v10/channels/{km_channel_num}/messages"
                ))
                .header("Authorization", format!("Bot {}", token))
                .json(&serde_json::json!({"content": message}))
                .send()
                .await;
        });

        return (
            StatusCode::OK,
            Json(json!({
                "run": run,
                "entries": [],
                "message": "PMD 분석 요청 전송됨. 응답 대기 중.",
            })),
        );
    }

    // Dependency-aware mode: filter out cards with incomplete dependencies
    let (filtered_cards, excluded_count) = if mode == "dependency-aware" {
        let mut filtered = Vec::new();
        let mut excluded = 0usize;
        for (card_id, agent_id, priority) in &cards {
            // Get GitHub issue body to parse dependencies
            let issue_body: Option<String> = conn
                .query_row(
                    "SELECT github_issue_url FROM kanban_cards WHERE id = ?1",
                    [card_id],
                    |row| row.get(0),
                )
                .ok()
                .flatten();

            // Parse dependency issue numbers from ## 의존성 section
            let dep_numbers = if let Some(ref url) = issue_body {
                // Get issue number from this card
                let issue_num: Option<i64> = conn
                    .query_row(
                        "SELECT github_issue_number FROM kanban_cards WHERE id = ?1",
                        [card_id],
                        |row| row.get(0),
                    )
                    .ok()
                    .flatten();

                // Look for dependencies in kv_meta or card metadata
                // Parse from GitHub issue body if available via sync
                let mut deps = Vec::new();
                if let Some(_num) = issue_num {
                    // Check if card has metadata with dependencies
                    let metadata: Option<String> = conn
                        .query_row(
                            "SELECT metadata FROM kanban_cards WHERE id = ?1",
                            [card_id],
                            |row| row.get(0),
                        )
                        .ok()
                        .flatten();
                    if let Some(meta) = metadata {
                        // Parse #N references from metadata
                        for cap in regex::Regex::new(r"#(\d+)").unwrap().captures_iter(&meta) {
                            if let Ok(n) = cap[1].parse::<i64>() {
                                deps.push(n);
                            }
                        }
                    }
                }
                deps
            } else {
                Vec::new()
            };

            // Check if all dependencies are done
            let mut all_deps_done = true;
            for dep_num in &dep_numbers {
                let dep_status: Option<String> = conn
                    .query_row(
                        "SELECT status FROM kanban_cards WHERE github_issue_number = ?1",
                        [dep_num],
                        |row| row.get(0),
                    )
                    .ok();
                if dep_status.as_deref() != Some("done") {
                    all_deps_done = false;
                    break;
                }
            }

            if all_deps_done {
                filtered.push((card_id.clone(), agent_id.clone(), priority.clone()));
            } else {
                excluded += 1;
            }
        }
        (filtered, excluded)
    } else {
        (cards.clone(), 0)
    };

    if filtered_cards.is_empty() {
        return (
            StatusCode::OK,
            Json(json!({
                "run": null,
                "entries": [],
                "message": format!("No cards available ({}개 의존성 미충족으로 제외)", excluded_count)
            })),
        );
    }

    // Create run
    let run_id = uuid::Uuid::new_v4().to_string();
    let (ai_model, ai_rationale) = if mode == "dependency-aware" {
        (
            "dependency-aware-sort",
            format!(
                "의존관계 기반 필터링 + 우선순위 정렬. {}개 큐잉, {}개 의존성 미충족 제외",
                filtered_cards.len(),
                excluded_count
            ),
        )
    } else {
        (
            "priority-sort",
            format!(
                "우선순위 기반 정렬 (urgent > high > medium > low), {}개 카드 큐잉",
                filtered_cards.len()
            ),
        )
    };
    let ai_model_str = ai_model.to_string();
    conn.execute(
        "INSERT INTO auto_queue_runs (id, repo, agent_id, ai_model, ai_rationale) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![run_id, body.repo, body.agent_id, ai_model_str, ai_rationale],
    )
    .ok();

    // Create entries
    let mut entries = Vec::new();
    for (rank, (card_id, agent_id, _priority)) in filtered_cards.iter().enumerate() {
        let entry_id = uuid::Uuid::new_v4().to_string();
        let agent = if agent_id.is_empty() {
            body.agent_id.as_deref().unwrap_or("")
        } else {
            agent_id
        };
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, priority_rank)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![entry_id, run_id, card_id, agent, rank as i64],
        )
        .ok();
        entries.push(entry_to_json(&conn, &entry_id));
    }

    let run = run_to_json(&conn, &run_id);

    (
        StatusCode::OK,
        Json(json!({ "run": run, "entries": entries })),
    )
}

/// POST /api/auto-queue/activate
/// Dispatches the next pending entry in the active run.
pub async fn activate(
    State(state): State<AppState>,
    Json(body): Json<ActivateBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);

    // Find active run
    let mut run_filter = "status = 'active'".to_string();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(ref repo) = body.repo {
        run_filter.push_str(&format!(
            " AND (repo = ?{} OR repo IS NULL OR repo = '')",
            params.len() + 1
        ));
        params.push(Box::new(repo.clone()));
    }
    if let Some(ref agent_id) = body.agent_id {
        run_filter.push_str(&format!(
            " AND (agent_id = ?{} OR agent_id IS NULL OR agent_id = '')",
            params.len() + 1
        ));
        params.push(Box::new(agent_id.clone()));
    }

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let run_id: Option<String> = conn
        .query_row(
            &format!(
                "SELECT id FROM auto_queue_runs WHERE {run_filter} ORDER BY created_at DESC LIMIT 1"
            ),
            param_refs.as_slice(),
            |row| row.get(0),
        )
        .ok();

    let Some(run_id) = run_id else {
        return (
            StatusCode::OK,
            Json(json!({ "dispatched": [], "count": 0, "message": "No active run" })),
        );
    };

    // #137: Apply unified_thread toggle if provided
    if let Some(unified) = body.unified_thread {
        conn.execute(
            "UPDATE auto_queue_runs SET unified_thread = ?1 WHERE id = ?2",
            rusqlite::params![unified as i32, run_id],
        )
        .ok();
    }

    // Stale empty run cleanup: after generate()/enqueue() fixes, normal paths never
    // leave an active run with 0 entries.  Any such run is legacy corruption — complete
    // it immediately instead of auto-populating with unrelated ready cards (#85).
    let entry_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries WHERE run_id = ?1",
            [&run_id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    if entry_count == 0 {
        conn.execute(
            "UPDATE auto_queue_runs SET status = 'completed', completed_at = datetime('now') WHERE id = ?1",
            [&run_id],
        ).ok();
        tracing::info!(
            "[auto-queue] Completed stale empty run {run_id} — no entries, skipping fallback populate (#85)"
        );
        return (
            StatusCode::OK,
            Json(
                json!({ "dispatched": [], "count": 0, "message": "Stale empty run completed — no entries to dispatch" }),
            ),
        );
    }

    // Get first pending entry only (sequential dispatch — one at a time)
    let mut stmt = conn
        .prepare(
            "SELECT e.id, e.kanban_card_id, e.agent_id
             FROM auto_queue_entries e
             WHERE e.run_id = ?1 AND e.status = 'pending'
             ORDER BY e.priority_rank ASC
             LIMIT 1",
        )
        .unwrap();

    let pending: Vec<(String, String, String)> = stmt
        .query_map([&run_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default();

    drop(stmt);

    let mut dispatched = Vec::new();
    for (entry_id, card_id, agent_id) in &pending {
        // Busy-agent guard (#110): skip dispatch if agent already has active cards.
        // Prevents manual /api/auto-queue/activate from bypassing serialization.
        let busy: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM kanban_cards \
                 WHERE assigned_agent_id = ?1 AND status IN ('requested', 'in_progress', 'review')",
                [agent_id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if busy {
            tracing::info!("[auto-queue] Skipping activate for {agent_id}: agent has active cards");
            drop(conn);
            conn = state.db.separate_conn().unwrap();
            break;
        }

        // Get card title for dispatch creation
        let title: String = conn
            .query_row(
                "SELECT title FROM kanban_cards WHERE id = ?1",
                [card_id],
                |row| row.get(0),
            )
            .unwrap_or_else(|_| "Dispatch".to_string());

        drop(conn);

        // Create dispatch — use block_in_place to allow tokio to schedule
        // other tasks while fire_hook executes blocking QuickJS
        let dispatch_result = tokio::task::block_in_place(|| {
            crate::dispatch::create_dispatch(
                &state.db,
                &state.engine,
                card_id,
                agent_id,
                "implementation",
                &title,
                &json!({"auto_queue": true, "entry_id": entry_id}),
            )
        });

        let conn_reacquired = state.db.separate_conn().unwrap();
        if dispatch_result.is_err() {
            tracing::error!(
                "[auto-queue] create_dispatch failed for entry {entry_id}, leaving as pending for retry"
            );
            drop(conn_reacquired);
            conn = state.db.separate_conn().unwrap();
            continue;
        }

        // Dispatch succeeded — now mark entry with dispatch_id for direct run association (#145)
        let dispatch_id = dispatch_result.as_ref().unwrap()["id"]
            .as_str()
            .unwrap_or("")
            .to_string();
        conn_reacquired.execute(
            "UPDATE auto_queue_entries SET status = 'dispatched', dispatch_id = ?1, dispatched_at = datetime('now') WHERE id = ?2",
            rusqlite::params![dispatch_id, entry_id],
        )
        .ok();
        drop(conn_reacquired);
        let db_clone = state.db.clone();
        let card_id_c = card_id.clone();
        let agent_id_c = agent_id.clone();
        let title_c = title.clone();
        tokio::spawn(async move {
            super::dispatches::send_dispatch_to_discord(
                &db_clone,
                &agent_id_c,
                &title_c,
                &card_id_c,
                &dispatch_id,
            )
            .await;
        });

        let conn_inner = state.db.separate_conn().unwrap();
        dispatched.push(entry_to_json(&conn_inner, entry_id));
        drop(conn_inner);

        break; // Dispatch one at a time — next one starts when this one completes
    }

    // Check if all entries are done
    let conn = state.db.separate_conn().unwrap();
    let remaining: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM auto_queue_entries WHERE run_id = ?1 AND status = 'pending'",
            [&run_id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    if remaining == 0 {
        conn.execute(
            "UPDATE auto_queue_runs SET status = 'completed', completed_at = datetime('now') WHERE id = ?1",
            [&run_id],
        )
        .ok();
    }

    (
        StatusCode::OK,
        Json(json!({ "dispatched": dispatched, "count": dispatched.len() })),
    )
}

/// GET /api/auto-queue/status
pub async fn status(
    State(state): State<AppState>,
    Query(query): Query<StatusQuery>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);

    // Find latest run (NULL agent_id/repo matches any filter)
    let mut run_filter = "1=1".to_string();
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(ref repo) = query.repo {
        run_filter.push_str(&format!(
            " AND (repo = ?{} OR repo IS NULL OR repo = '')",
            params.len() + 1
        ));
        params.push(Box::new(repo.clone()));
    }
    if let Some(ref agent_id) = query.agent_id {
        run_filter.push_str(&format!(
            " AND (agent_id = ?{} OR agent_id IS NULL OR agent_id = '')",
            params.len() + 1
        ));
        params.push(Box::new(agent_id.clone()));
    }

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let run_id: Option<String> = conn
        .query_row(
            &format!(
                "SELECT id FROM auto_queue_runs WHERE {run_filter} ORDER BY created_at DESC LIMIT 1"
            ),
            param_refs.as_slice(),
            |row| row.get(0),
        )
        .ok();

    let Some(run_id) = run_id else {
        return (
            StatusCode::OK,
            Json(json!({ "run": null, "entries": [], "agents": {} })),
        );
    };

    let run = run_to_json(&conn, &run_id);

    // Get entries (filtered by agent_id and repo if specified)
    let entry_ids: Vec<String> = {
        let mut entry_sql = String::from(
            "SELECT e.id FROM auto_queue_entries e \
             LEFT JOIN kanban_cards kc ON e.kanban_card_id = kc.id \
             WHERE e.run_id = ?1",
        );
        let mut entry_params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(run_id.clone())];
        if let Some(ref agent_id) = query.agent_id {
            entry_sql.push_str(&format!(" AND e.agent_id = ?{}", entry_params.len() + 1));
            entry_params.push(Box::new(agent_id.clone()));
        }
        if let Some(ref repo) = query.repo {
            entry_sql.push_str(&format!(" AND kc.repo_id = ?{}", entry_params.len() + 1));
            entry_params.push(Box::new(repo.clone()));
        }
        entry_sql.push_str(" ORDER BY e.priority_rank ASC");

        let entry_refs: Vec<&dyn rusqlite::types::ToSql> =
            entry_params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn.prepare(&entry_sql).unwrap();
        stmt.query_map(entry_refs.as_slice(), |row| row.get::<_, String>(0))
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    };

    let entries: Vec<serde_json::Value> = entry_ids
        .iter()
        .map(|id| entry_to_json(&conn, id))
        .collect();

    // Agent summary
    let mut agents: std::collections::HashMap<String, serde_json::Value> =
        std::collections::HashMap::new();
    for entry in &entries {
        let agent = entry["agent_id"].as_str().unwrap_or("unknown").to_string();
        let status = entry["status"].as_str().unwrap_or("pending");
        let counter = agents
            .entry(agent)
            .or_insert_with(|| json!({"pending": 0, "dispatched": 0, "done": 0, "skipped": 0}));
        if let Some(obj) = counter.as_object_mut() {
            if let Some(val) = obj.get_mut(status) {
                *val = json!(val.as_i64().unwrap_or(0) + 1);
            }
        }
    }

    (
        StatusCode::OK,
        Json(json!({ "run": run, "entries": entries, "agents": agents })),
    )
}

/// PATCH /api/auto-queue/entries/{id}/skip
pub async fn skip_entry(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);

    let changed = conn
        .execute(
            "UPDATE auto_queue_entries SET status = 'skipped', completed_at = datetime('now') WHERE id = ?1 AND status = 'pending'",
            [&id],
        )
        .unwrap_or(0);

    if changed == 0 {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "entry not found or not pending"})),
        );
    }

    (StatusCode::OK, Json(json!({ "ok": true })))
}

/// PATCH /api/auto-queue/runs/{id}
pub async fn update_run(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateRunBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);

    let mut changed = 0usize;

    if let Some(ref status) = body.status {
        let completed_at = if status == "completed" {
            "datetime('now')"
        } else {
            "NULL"
        };
        changed += conn
            .execute(
                &format!(
                    "UPDATE auto_queue_runs SET status = ?1, completed_at = {completed_at} WHERE id = ?2"
                ),
                rusqlite::params![status, id],
            )
            .unwrap_or(0);
    }

    if let Some(unified) = body.unified_thread {
        changed += conn
            .execute(
                "UPDATE auto_queue_runs SET unified_thread = ?1 WHERE id = ?2",
                rusqlite::params![unified as i32, id],
            )
            .unwrap_or(0);
    }

    if changed == 0 && body.status.is_none() && body.unified_thread.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no fields to update"})),
        );
    }

    (StatusCode::OK, Json(json!({ "ok": true })))
}

/// POST /api/auto-queue/reset
/// Clear all entries and complete all active runs.
pub async fn reset(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);

    let deleted_entries = conn
        .execute("DELETE FROM auto_queue_entries", [])
        .unwrap_or(0);
    let completed_runs = conn
        .execute(
            "UPDATE auto_queue_runs SET status = 'completed', completed_at = datetime('now') WHERE status IN ('active', 'paused')",
            [],
        )
        .unwrap_or(0);

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "deleted_entries": deleted_entries,
            "completed_runs": completed_runs,
        })),
    )
}

/// POST /api/auto-queue/pause — pause all active runs
pub async fn pause(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);
    let paused = conn
        .execute(
            "UPDATE auto_queue_runs SET status = 'paused' WHERE status = 'active'",
            [],
        )
        .unwrap_or(0);
    (
        StatusCode::OK,
        Json(json!({"ok": true, "paused_runs": paused})),
    )
}

/// POST /api/auto-queue/resume — resume paused runs and dispatch next entry
pub async fn resume_run(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);
    let resumed = conn
        .execute(
            "UPDATE auto_queue_runs SET status = 'active' WHERE status = 'paused'",
            [],
        )
        .unwrap_or(0);
    drop(conn);

    // Trigger dispatch of next pending entry
    if resumed > 0 {
        let (status, body) = activate(
            State(state),
            Json(ActivateBody {
                repo: None,
                agent_id: None,
                unified_thread: None,
            }),
        )
        .await;
        let dispatched = body.0.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
        return (
            StatusCode::OK,
            Json(json!({"ok": true, "resumed_runs": resumed, "dispatched": dispatched})),
        );
    }

    (
        StatusCode::OK,
        Json(json!({"ok": true, "resumed_runs": 0, "message": "No paused runs"})),
    )
}

/// POST /api/auto-queue/cancel — cancel all active/paused runs and pending entries
pub async fn cancel(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);
    let cancelled_entries = conn
        .execute(
            "UPDATE auto_queue_entries SET status = 'skipped' WHERE status IN ('pending', 'dispatched')",
            [],
        )
        .unwrap_or(0);
    let cancelled_runs = conn
        .execute(
            "UPDATE auto_queue_runs SET status = 'cancelled', completed_at = datetime('now') WHERE status IN ('active', 'paused')",
            [],
        )
        .unwrap_or(0);
    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "cancelled_entries": cancelled_entries,
            "cancelled_runs": cancelled_runs,
        })),
    )
}

/// PATCH /api/auto-queue/reorder
pub async fn reorder(
    State(state): State<AppState>,
    Json(body): Json<ReorderBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let mut conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);

    let run_id = body.ordered_ids.iter().find_map(|id| {
        conn.query_row(
            "SELECT run_id FROM auto_queue_entries WHERE id = ?1",
            [id],
            |row| row.get::<_, String>(0),
        )
        .ok()
    });
    let Some(run_id) = run_id else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no matching queue entries found"})),
        );
    };

    let current_entries: Vec<QueueEntryOrder> = {
        let mut stmt = match conn.prepare(
            "SELECT id, COALESCE(status, 'pending'), COALESCE(agent_id, '')
             FROM auto_queue_entries
             WHERE run_id = ?1
             ORDER BY priority_rank ASC, created_at ASC, id ASC",
        ) {
            Ok(stmt) => stmt,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        };
        stmt.query_map([&run_id], |row| {
            Ok(QueueEntryOrder {
                id: row.get(0)?,
                status: row.get(1)?,
                agent_id: row.get(2)?,
            })
        })
        .ok()
        .map(|rows| rows.filter_map(|row| row.ok()).collect())
        .unwrap_or_default()
    };

    let reordered_ids = match reorder_entry_ids(
        &current_entries,
        &body.ordered_ids,
        body.agent_id.as_deref(),
    ) {
        Ok(ids) => ids,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": error })));
        }
    };

    let tx = match conn.transaction() {
        Ok(tx) => tx,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    for (rank, id) in reordered_ids.iter().enumerate() {
        if let Err(e) = tx.execute(
            "UPDATE auto_queue_entries SET priority_rank = ?1 WHERE id = ?2",
            rusqlite::params![rank as i64, id],
        ) {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    }

    if let Err(e) = tx.commit() {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        );
    }

    (StatusCode::OK, Json(json!({ "ok": true })))
}

/// POST /api/auto-queue/enqueue
pub async fn enqueue(
    State(state): State<AppState>,
    Json(body): Json<EnqueueBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);

    // Resolve agent_id
    let agent_id = match body.agent_id {
        Some(ref id) if !id.is_empty() => id.clone(),
        _ => match conn.query_row(
            "SELECT default_agent_id FROM github_repos WHERE full_name = ?1",
            [&body.repo],
            |row| row.get::<_, Option<String>>(0),
        ) {
            Ok(Some(id)) if !id.is_empty() => id,
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "no agent_id provided and repo has no default_agent_id"})),
                );
            }
        },
    };

    // Find or create kanban card
    let card_id: Option<String> = conn
        .query_row(
            "SELECT id FROM kanban_cards WHERE github_issue_number = ?1 AND repo_id = ?2",
            rusqlite::params![body.issue_number, body.repo],
            |row| row.get(0),
        )
        .ok();

    let card_id = match card_id {
        Some(id) => id,
        None => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "kanban card not found for this issue"})),
            );
        }
    };

    let card_status: String = conn
        .query_row(
            "SELECT status FROM kanban_cards WHERE id = ?1",
            [&card_id],
            |row| row.get(0),
        )
        .unwrap_or_default();

    // Find existing active run (do NOT create yet — preserves idempotent retry)
    let existing_run_id: Option<String> = conn
        .query_row(
            "SELECT id FROM auto_queue_runs WHERE status = 'active' AND (repo = ?1 OR repo IS NULL) AND (agent_id = ?2 OR agent_id IS NULL) ORDER BY created_at DESC LIMIT 1",
            rusqlite::params![body.repo, agent_id],
            |row| row.get(0),
        )
        .ok();

    // Check if already in queue (idempotent retry) — must run BEFORE status validation
    if let Some(ref rid) = existing_run_id {
        let already: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM auto_queue_entries WHERE run_id = ?1 AND kanban_card_id = ?2 AND status = 'pending'",
                rusqlite::params![rid, card_id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0)
            > 0;

        if already {
            return (
                StatusCode::OK,
                Json(
                    json!({"ok": true, "card_id": card_id, "agent_id": agent_id, "already_queued": true}),
                ),
            );
        }
    }

    // Validate card is dispatchable AFTER duplicate check to preserve idempotent retry,
    // but BEFORE run creation to prevent empty active runs
    let dispatchable_states = crate::pipeline::try_get()
        .map(|p| {
            p.dispatchable_states()
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_else(|| vec!["ready".to_string()]);
    if !dispatchable_states.iter().any(|s| s == &card_status) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": format!("card status is '{}', only dispatchable cards can be enqueued", card_status),
                "card_id": card_id,
                "status": card_status,
            })),
        );
    }

    // Use existing run or create new one (only reached when card is ready)
    let run_id = existing_run_id.unwrap_or_else(|| {
        let id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO auto_queue_runs (id, repo, agent_id) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, body.repo, agent_id],
        )
        .ok();
        id
    });

    let entry_id = uuid::Uuid::new_v4().to_string();
    let max_rank: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(priority_rank), -1) FROM auto_queue_entries WHERE run_id = ?1",
            [&run_id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    conn.execute(
        "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, priority_rank)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![entry_id, run_id, card_id, agent_id, max_rank + 1],
    )
    .ok();

    (
        StatusCode::OK,
        Json(json!({"ok": true, "card_id": card_id, "agent_id": agent_id})),
    )
}

// ── PM-assisted callback ─────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct OrderBody {
    /// Ordered list of GitHub issue numbers (or card IDs)
    pub order: Vec<serde_json::Value>,
    pub rationale: Option<String>,
    /// Alias for rationale (compatibility)
    pub reasoning: Option<String>,
}

/// POST /api/auto-queue/runs/:id/order
/// Callback from PMD: provides the ordered card list for a pending run.
pub async fn submit_order(
    State(state): State<AppState>,
    Path(run_id): Path<String>,
    Json(body): Json<OrderBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.separate_conn() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };
    ensure_tables(&conn);

    // Verify run exists and is pending, get repo for filtering
    let run_info: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT status, repo FROM auto_queue_runs WHERE id = ?1",
            [&run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();

    match run_info.as_ref().map(|(s, _)| s.as_str()) {
        Some("pending") => {}
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "run not found or not pending"})),
            );
        }
    }
    let run_repo = run_info.as_ref().and_then(|(_, r)| r.clone());

    // Create entries from the ordered list
    let mut created = 0;
    for (rank, item) in body.order.iter().enumerate() {
        // Item can be issue number (i64) or card_id (string)
        // When matching by issue number, filter by repo to prevent cross-repo collisions
        let card_id: Option<String> = if let Some(num) = item.as_i64() {
            if let Some(ref repo) = run_repo {
                conn.query_row(
                    "SELECT id FROM kanban_cards WHERE github_issue_number = ?1 AND repo_id = ?2",
                    rusqlite::params![num, repo],
                    |row| row.get(0),
                )
                .ok()
            } else {
                conn.query_row(
                    "SELECT id FROM kanban_cards WHERE github_issue_number = ?1",
                    [num],
                    |row| row.get(0),
                )
                .ok()
            }
        } else if let Some(id) = item.as_str() {
            Some(id.to_string())
        } else {
            None
        };

        let Some(card_id) = card_id else { continue };

        // Only enqueue cards in dispatchable states (pipeline-driven)
        let card_status: String = conn
            .query_row(
                "SELECT COALESCE(status, '') FROM kanban_cards WHERE id = ?1",
                [&card_id],
                |row| row.get(0),
            )
            .unwrap_or_default();
        let dispatchable_check = crate::pipeline::try_get()
            .map(|p| p.dispatchable_states().iter().any(|s| *s == card_status))
            .unwrap_or(card_status == "ready");
        if !dispatchable_check {
            tracing::info!(
                "[auto-queue] Skipping card {card_id} (status={card_status}, not dispatchable)"
            );
            continue;
        }

        let agent_id: String = conn
            .query_row(
                "SELECT COALESCE(assigned_agent_id, '') FROM kanban_cards WHERE id = ?1",
                [&card_id],
                |row| row.get(0),
            )
            .unwrap_or_default();

        let entry_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, priority_rank)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![entry_id, run_id, card_id, agent_id, rank as i64],
        )
        .ok();
        created += 1;
    }

    // Only activate if at least one card was enqueued; otherwise leave as pending
    // to prevent the activate() fallback from filling the run with unintended cards
    let rationale = body
        .rationale
        .as_deref()
        .or(body.reasoning.as_deref())
        .unwrap_or("PMD 분석 완료");
    if created > 0 {
        conn.execute(
            "UPDATE auto_queue_runs SET status = 'active', ai_rationale = ?1 WHERE id = ?2",
            rusqlite::params![rationale, run_id],
        )
        .ok();
    } else {
        tracing::warn!(
            "[auto-queue] submit_order: no ready cards enqueued, run {run_id} stays pending"
        );
        conn.execute(
            "UPDATE auto_queue_runs SET status = 'completed', ai_rationale = ?1 WHERE id = ?2",
            rusqlite::params![
                format!("{rationale} (no ready cards — auto-completed)"),
                run_id
            ],
        )
        .ok();
    }

    // Queue created and activated — dispatch is a separate step via POST /api/auto-queue/activate
    // This allows PMD to review/adjust the order before dispatching begins.
    drop(conn);

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "created": created,
            "run_id": run_id,
            "message": "Queue active. Call POST /api/auto-queue/activate to start dispatching.",
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::{QueueEntryOrder, reorder_entry_ids};

    fn entry(id: &str, status: &str, agent_id: &str) -> QueueEntryOrder {
        QueueEntryOrder {
            id: id.to_string(),
            status: status.to_string(),
            agent_id: agent_id.to_string(),
        }
    }

    #[test]
    fn reorder_entry_ids_reorders_only_pending_entries_in_scope() {
        let entries = vec![
            entry("done-a", "done", "agent-a"),
            entry("a-1", "pending", "agent-a"),
            entry("b-1", "pending", "agent-b"),
            entry("a-2", "pending", "agent-a"),
            entry("done-b", "done", "agent-b"),
        ];

        let reordered = reorder_entry_ids(
            &entries,
            &["a-2".to_string(), "a-1".to_string()],
            Some("agent-a"),
        )
        .expect("agent reorder should succeed");

        assert_eq!(
            reordered,
            vec![
                "done-a".to_string(),
                "a-2".to_string(),
                "b-1".to_string(),
                "a-1".to_string(),
                "done-b".to_string(),
            ]
        );
    }

    #[test]
    fn reorder_entry_ids_filters_non_pending_ids_from_legacy_payloads() {
        let entries = vec![
            entry("done-a", "done", "agent-a"),
            entry("p-1", "pending", "agent-a"),
            entry("p-2", "pending", "agent-a"),
            entry("done-b", "done", "agent-a"),
        ];

        let reordered = reorder_entry_ids(
            &entries,
            &[
                "done-a".to_string(),
                "p-2".to_string(),
                "p-1".to_string(),
                "done-b".to_string(),
            ],
            None,
        )
        .expect("legacy payload should still reorder pending entries");

        assert_eq!(
            reordered,
            vec![
                "done-a".to_string(),
                "p-2".to_string(),
                "p-1".to_string(),
                "done-b".to_string(),
            ]
        );
    }
}
