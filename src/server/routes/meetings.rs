use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;

// ── Body types ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct IssueRepoBody {
    pub repo: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StartMeetingBody {
    pub agenda: Option<String>,
    pub channel_id: Option<String>,
    pub primary_provider: Option<String>,
}

// ── Handlers ───────────────────────────────────────────────────

/// GET /api/round-table-meetings
pub async fn list_meetings(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    let mut stmt = match conn.prepare(
        "SELECT id, channel_id, title, status, effective_rounds, started_at, completed_at, summary
         FROM meetings
         ORDER BY started_at DESC",
    ) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("prepare: {e}")})),
            );
        }
    };

    let rows = stmt.query_map([], |row| meeting_row_to_json(row)).ok();

    let mut meetings: Vec<serde_json::Value> = match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    };

    // Attach transcripts to each meeting (+ entries alias for frontend)
    for meeting in meetings.iter_mut() {
        if let Some(meeting_id) = meeting.get("id").and_then(|v| v.as_str()) {
            let transcripts = load_transcripts(&conn, meeting_id);
            let obj = meeting.as_object_mut().unwrap();
            obj.insert("transcripts".to_string(), json!(&transcripts));
            obj.insert("entries".to_string(), json!(transcripts));
        }
    }

    (StatusCode::OK, Json(json!({"meetings": meetings})))
}

/// GET /api/round-table-meetings/:id
pub async fn get_meeting(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    match conn.query_row(
        "SELECT id, channel_id, title, status, effective_rounds, started_at, completed_at, summary
         FROM meetings WHERE id = ?1",
        [&id],
        |row| meeting_row_to_json(row),
    ) {
        Ok(mut meeting) => {
            let transcripts = load_transcripts(&conn, &id);
            let obj = meeting.as_object_mut().unwrap();
            obj.insert("transcripts".to_string(), json!(&transcripts));
            obj.insert("entries".to_string(), json!(transcripts));
            (StatusCode::OK, Json(json!({"meeting": meeting})))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "meeting not found"})),
        ),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// DELETE /api/round-table-meetings/:id
pub async fn delete_meeting(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // Delete transcripts first
    let _ = conn.execute(
        "DELETE FROM meeting_transcripts WHERE meeting_id = ?1",
        [&id],
    );

    match conn.execute("DELETE FROM meetings WHERE id = ?1", [&id]) {
        Ok(0) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "meeting not found"})),
        ),
        Ok(_) => (StatusCode::OK, Json(json!({"ok": true}))),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// PATCH /api/round-table-meetings/:id/issue-repo
pub async fn update_issue_repo(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<IssueRepoBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    // Check meeting exists
    let exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM meetings WHERE id = ?1",
            [&id],
            |row| row.get::<_, i64>(0),
        )
        .map(|c| c > 0)
        .unwrap_or(false);

    if !exists {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "meeting not found"})),
        );
    }

    // Store issue_repo in kv_meta (meetings table doesn't have issue_repo column)
    let key = format!("meeting_issue_repo:{}", id);
    let value = body.repo.as_deref().unwrap_or("");

    if let Err(e) = conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
        rusqlite::params![key, value],
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        );
    }

    // Read back meeting
    match conn.query_row(
        "SELECT id, channel_id, title, status, effective_rounds, started_at, completed_at, summary
         FROM meetings WHERE id = ?1",
        [&id],
        |row| meeting_row_to_json(row),
    ) {
        Ok(mut meeting) => {
            meeting
                .as_object_mut()
                .unwrap()
                .insert("issue_repo".to_string(), json!(body.repo));
            (
                StatusCode::OK,
                Json(json!({"ok": true, "meeting": meeting})),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        ),
    }
}

/// POST /api/round-table-meetings/:id/issues
/// Extract action items from meeting summary and create GitHub issues.
#[derive(Debug, Deserialize)]
pub struct CreateIssuesBody {
    pub repo: Option<String>,
}

#[axum::debug_handler]
pub async fn create_issues(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<CreateIssuesBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            )
        }
    };

    // Verify meeting exists
    let meeting_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM meetings WHERE id = ?1",
            [&id],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if !meeting_exists {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "meeting not found"})),
        );
    }

    // Get issue repo from kv_meta or request body
    let repo: Option<String> = body.repo.clone().or_else(|| {
            conn.query_row(
                "SELECT value FROM kv_meta WHERE key = ?1",
                [&format!("meeting:{id}:issue_repo")],
                |row| row.get(0),
            )
            .ok()
        });

    let Some(repo) = repo else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "no repo configured for this meeting — set issue_repo first"})),
        );
    };

    // Get summary transcripts (action items)
    let summaries: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT content FROM meeting_transcripts
                 WHERE meeting_id = ?1 AND is_summary = 1
                 ORDER BY seq ASC",
            )
            .unwrap();
        stmt.query_map([&id], |row| row.get::<_, String>(0))
            .ok()
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
    };

    if summaries.is_empty() {
        return (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "skipped": true,
                "results": [],
                "summary": {"total": 0, "created": 0, "failed": 0, "discarded": 0, "pending": 0, "all_created": true, "all_resolved": true}
            })),
        );
    }

    drop(conn);

    // Create issues from summaries using gh CLI
    let mut results = Vec::new();
    let mut created = 0i64;
    let mut failed = 0i64;

    for (i, summary) in summaries.iter().enumerate() {
        let key = format!("item-{i}");
        // Check if already discarded
        let discarded = {
            let conn = state.db.lock().unwrap();
            conn.query_row(
                "SELECT value FROM kv_meta WHERE key = ?1",
                [&format!("meeting:{id}:issue:{key}:discarded")],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .map(|v| v == "true")
            .unwrap_or(false)
        };
        if discarded {
            results.push(json!({"key": key, "title": summary.lines().next().unwrap_or(""), "assignee": "", "ok": true, "discarded": true, "attempted_at": 0}));
            continue;
        }

        // Check if already created
        let already_url: Option<String> = {
            let conn = state.db.lock().unwrap();
            conn.query_row(
                "SELECT value FROM kv_meta WHERE key = ?1",
                [&format!("meeting:{id}:issue:{key}:url")],
                |row| row.get(0),
            )
            .ok()
        };
        if let Some(url) = already_url {
            results.push(json!({"key": key, "title": summary.lines().next().unwrap_or(""), "assignee": "", "ok": true, "issue_url": url, "attempted_at": 0}));
            created += 1;
            continue;
        }

        // Extract first line as title
        let title = summary.lines().next().unwrap_or("Meeting action item").trim();
        let body_text = if summary.lines().count() > 1 {
            summary.lines().skip(1).collect::<Vec<_>>().join("\n")
        } else {
            String::new()
        };

        // Create GitHub issue
        let output = std::process::Command::new("gh")
            .args([
                "issue",
                "create",
                "--repo",
                &repo,
                "--title",
                title,
                "--body",
                &body_text,
            ])
            .output();

        match output {
            Ok(out) if out.status.success() => {
                let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
                // Store result
                let conn = state.db.lock().unwrap();
                conn.execute(
                    "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                    rusqlite::params![format!("meeting:{id}:issue:{key}:url"), url],
                )
                .ok();
                drop(conn);
                results.push(json!({"key": key, "title": title, "assignee": "", "ok": true, "issue_url": url, "attempted_at": chrono::Utc::now().timestamp()}));
                created += 1;
            }
            Ok(out) => {
                let err = String::from_utf8_lossy(&out.stderr).to_string();
                results.push(json!({"key": key, "title": title, "assignee": "", "ok": false, "error": err, "attempted_at": chrono::Utc::now().timestamp()}));
                failed += 1;
            }
            Err(e) => {
                results.push(json!({"key": key, "title": title, "assignee": "", "ok": false, "error": format!("{e}"), "attempted_at": chrono::Utc::now().timestamp()}));
                failed += 1;
            }
        }
    }

    let total = results.len() as i64;
    let discarded = results.iter().filter(|r| r["discarded"].as_bool().unwrap_or(false)).count() as i64;
    let pending = total - created - failed - discarded;

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "results": results,
            "summary": {
                "total": total,
                "created": created,
                "failed": failed,
                "discarded": discarded,
                "pending": pending,
                "all_created": pending == 0 && failed == 0,
                "all_resolved": pending == 0 && failed == 0,
            }
        })),
    )
}

/// POST /api/round-table-meetings/:id/issues/discard
#[derive(Debug, Deserialize)]
pub struct DiscardIssueBody {
    pub key: Option<String>,
}

pub async fn discard_issue(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<DiscardIssueBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let key = body.key.as_deref().unwrap_or("");
    if key.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "key is required"})),
        );
    }

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            )
        }
    };

    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, 'true')",
        [&format!("meeting:{id}:issue:{key}:discarded")],
    )
    .ok();

    // Return meeting + summary for UI refresh
    let meeting = conn
        .query_row(
            "SELECT id, channel_id, title, status, effective_rounds, started_at, completed_at, summary FROM meetings WHERE id = ?1",
            [&id],
            |row| meeting_row_to_json(row),
        )
        .unwrap_or(json!(null));

    (
        StatusCode::OK,
        Json(json!({"ok": true, "meeting": meeting, "summary": {"total": 0, "created": 0, "failed": 0, "discarded": 1, "pending": 0, "all_created": false, "all_resolved": false}})),
    )
}

/// POST /api/round-table-meetings/:id/issues/discard-all
pub async fn discard_all_issues(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            )
        }
    };

    // Count summary items and mark all as discarded
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM meeting_transcripts WHERE meeting_id = ?1 AND is_summary = 1",
            [&id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    for i in 0..count {
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, 'true')",
            [&format!("meeting:{id}:issue:item-{i}:discarded")],
        )
        .ok();
    }

    let meeting = conn
        .query_row(
            "SELECT id, channel_id, title, status, effective_rounds, started_at, completed_at, summary FROM meetings WHERE id = ?1",
            [&id],
            |row| meeting_row_to_json(row),
        )
        .unwrap_or(json!(null));

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "meeting": meeting,
            "results": [],
            "summary": {"total": count, "created": 0, "failed": 0, "discarded": count, "pending": 0, "all_created": false, "all_resolved": true}
        })),
    )
}

/// POST /api/round-table-meetings/start
/// Send meeting start request to Discord channel via announce bot.
pub async fn start_meeting(
    State(state): State<AppState>,
    Json(body): Json<StartMeetingBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let channel_id = match &body.channel_id {
        Some(id) if !id.is_empty() => id.clone(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "channel_id is required"})),
            );
        }
    };

    let agenda = body.agenda.as_deref().unwrap_or("General discussion");

    // Send meeting start command to the channel via /api/send (health server)
    let health_port = crate::services::discord::health::resolve_port();

    let message = format!("/meeting start {agenda}");
    let client = reqwest::Client::new();
    match client
        .post(format!("http://127.0.0.1:{health_port}/api/send"))
        .json(&json!({
            "target": format!("channel:{channel_id}"),
            "content": message,
            "source": "dashboard",
        }))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => (
            StatusCode::OK,
            Json(json!({"ok": true, "message": "Meeting start command sent"})),
        ),
        Ok(resp) => {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"ok": false, "error": format!("Discord send failed: {status} {body}")})),
            )
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": format!("Request failed: {e}")})),
        ),
    }
}

// ── Helpers ────────────────────────────────────────────────────

fn meeting_row_to_json(row: &rusqlite::Row) -> rusqlite::Result<serde_json::Value> {
    let title = row.get::<_, Option<String>>(2)?;
    let effective_rounds = row.get::<_, Option<i64>>(4)?;
    Ok(json!({
        "id": row.get::<_, String>(0)?,
        "channel_id": row.get::<_, Option<String>>(1)?,
        "title": title,
        "status": row.get::<_, Option<String>>(3)?,
        "effective_rounds": effective_rounds,
        "started_at": row.get::<_, Option<String>>(5)?,
        "completed_at": row.get::<_, Option<String>>(6)?,
        "summary": row.get::<_, Option<String>>(7)?,
        // alias fields for frontend compatibility
        "agenda": title,
        "total_rounds": effective_rounds.unwrap_or(0),
        // additional fields expected by frontend (defaults)
        "primary_provider": null,
        "reviewer_provider": null,
        "participant_names": null,
        "issues_created": null,
        "proposed_issues": null,
        "issue_creation_results": null,
        "issue_repo": null,
        "created_at": null,
    }))
}

fn load_transcripts(conn: &rusqlite::Connection, meeting_id: &str) -> Vec<serde_json::Value> {
    let mut stmt = match conn.prepare(
        "SELECT id, meeting_id, seq, round, speaker_agent_id, speaker_name, content, is_summary
         FROM meeting_transcripts
         WHERE meeting_id = ?1
         ORDER BY seq ASC",
    ) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    let rows = stmt
        .query_map([meeting_id], |row| {
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "meeting_id": row.get::<_, String>(1)?,
                "seq": row.get::<_, Option<i64>>(2)?,
                "round": row.get::<_, Option<i64>>(3)?,
                "speaker_agent_id": row.get::<_, Option<String>>(4)?,
                "speaker_name": row.get::<_, Option<String>>(5)?,
                "content": row.get::<_, Option<String>>(6)?,
                "is_summary": row.get::<_, bool>(7).unwrap_or(false),
            }))
        })
        .ok();

    match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    }
}
