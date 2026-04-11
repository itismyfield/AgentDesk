use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::services::provider::ProviderKind;

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

#[derive(Debug, Deserialize)]
pub struct MeetingEntryBody {
    pub seq: Option<i64>,
    pub round: Option<i64>,
    pub speaker_role_id: Option<String>,
    pub speaker_name: Option<String>,
    pub content: Option<String>,
    pub is_summary: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct UpsertMeetingBody {
    pub id: String,
    pub agenda: Option<String>,
    pub summary: Option<String>,
    pub status: Option<String>,
    pub primary_provider: Option<String>,
    pub reviewer_provider: Option<String>,
    pub participant_names: Option<Vec<String>>,
    pub total_rounds: Option<i64>,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub thread_id: Option<String>,
    pub entries: Option<Vec<MeetingEntryBody>>,
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
        "SELECT id, channel_id, title, status, effective_rounds, started_at, completed_at, summary,
                primary_provider, reviewer_provider, participant_names, created_at
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

    // Attach transcripts + issue data to each meeting
    for meeting in meetings.iter_mut() {
        let meeting_id = meeting
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        if let Some(mid) = meeting_id {
            let transcripts = load_transcripts(&conn, &mid);
            let obj = meeting.as_object_mut().unwrap();
            obj.insert("transcripts".to_string(), json!(&transcripts));
            obj.insert("entries".to_string(), json!(transcripts));
            enrich_meeting_with_issue_data(&conn, &mid, obj);
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
        "SELECT id, channel_id, title, status, effective_rounds, started_at, completed_at, summary,
                primary_provider, reviewer_provider, participant_names, created_at
         FROM meetings WHERE id = ?1",
        [&id],
        |row| meeting_row_to_json(row),
    ) {
        Ok(mut meeting) => {
            let transcripts = load_transcripts(&conn, &id);
            let obj = meeting.as_object_mut().unwrap();
            obj.insert("transcripts".to_string(), json!(&transcripts));
            obj.insert("entries".to_string(), json!(transcripts));
            enrich_meeting_with_issue_data(&conn, &id, obj);
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
        "SELECT id, channel_id, title, status, effective_rounds, started_at, completed_at, summary,
                primary_provider, reviewer_provider, participant_names, created_at
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
    let (repo, summaries) = {
        let conn = match state.db.lock() {
            Ok(c) => c,
            Err(e) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
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
                [&format!("meeting_issue_repo:{id}")],
                |row| row.get(0),
            )
            .ok()
        });

        let Some(repo) = repo else {
            return (
                StatusCode::BAD_REQUEST,
                Json(
                    json!({"error": "no repo configured for this meeting — set issue_repo first"}),
                ),
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

        (repo, summaries)
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
        let title = summary
            .lines()
            .next()
            .unwrap_or("Meeting action item")
            .trim();
        let body_text = if summary.lines().count() > 1 {
            summary.lines().skip(1).collect::<Vec<_>>().join("\n")
        } else {
            String::new()
        };

        // Create GitHub issue
        match crate::github::create_issue(&repo, title, &body_text).await {
            Ok(created_issue) => {
                let url = created_issue.url;
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
            Err(error) => {
                results.push(json!({"key": key, "title": title, "assignee": "", "ok": false, "error": error, "attempted_at": chrono::Utc::now().timestamp()}));
                failed += 1;
            }
        }
    }

    let total = results.len() as i64;
    let discarded = results
        .iter()
        .filter(|r| r["discarded"].as_bool().unwrap_or(false))
        .count() as i64;
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
            );
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
            "SELECT id, channel_id, title, status, effective_rounds, started_at, completed_at, summary,
                    primary_provider, reviewer_provider, participant_names, created_at
             FROM meetings WHERE id = ?1",
            [&id],
            |row| meeting_row_to_json(row),
        )
        .unwrap_or(json!(null));

    (
        StatusCode::OK,
        Json(
            json!({"ok": true, "meeting": meeting, "summary": {"total": 0, "created": 0, "failed": 0, "discarded": 1, "pending": 0, "all_created": false, "all_resolved": false}}),
        ),
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
            );
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
            "SELECT id, channel_id, title, status, effective_rounds, started_at, completed_at, summary,
                    primary_provider, reviewer_provider, participant_names, created_at
             FROM meetings WHERE id = ?1",
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
    let primary_provider = match parse_meeting_provider(body.primary_provider.as_deref()) {
        Ok(provider) => provider,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error})));
        }
    };

    let message = build_meeting_start_command(agenda, primary_provider);
    let Some(registry) = state.health_registry.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"ok": false, "error": "Discord health registry unavailable"})),
        );
    };
    let (status, body) = crate::services::discord::health::send_message(
        registry,
        &state.db,
        &format!("channel:{channel_id}"),
        &message,
        "dashboard",
        "announce",
    )
    .await;
    if status == "200 OK" {
        (
            StatusCode::OK,
            Json(json!({"ok": true, "message": "Meeting start command sent"})),
        )
    } else {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": format!("Discord send failed: {status} {body}")})),
        )
    }
}

/// POST /api/round-table-meetings
/// Persist completed/cancelled meeting payloads posted back from the Discord runtime.
pub async fn upsert_meeting(
    State(state): State<AppState>,
    Json(body): Json<UpsertMeetingBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    if body.id.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "meeting id is required"})),
        );
    }

    let primary_provider = match parse_meeting_provider(body.primary_provider.as_deref()) {
        Ok(provider) => provider,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error})));
        }
    };
    let reviewer_provider = match parse_meeting_provider(body.reviewer_provider.as_deref()) {
        Ok(provider) => provider.or_else(|| primary_provider.clone().map(|p| p.counterpart())),
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error})));
        }
    };

    let agenda_update = body
        .agenda
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let agenda = agenda_update.unwrap_or("General discussion");
    let status_update = body
        .status
        .as_deref()
        .filter(|value| !value.trim().is_empty());
    let status = status_update.unwrap_or("completed");
    let total_rounds_update = body.total_rounds;
    let total_rounds = total_rounds_update.unwrap_or(0);
    let started_at = body
        .started_at
        .unwrap_or_else(|| chrono::Utc::now().timestamp_millis());
    let participant_names = body.participant_names.clone().unwrap_or_default();
    let participant_names_json =
        serde_json::to_string(&participant_names).unwrap_or_else(|_| "[]".to_string());
    let participant_names_update_json = body
        .participant_names
        .as_ref()
        .map(|value| serde_json::to_string(value).unwrap_or_else(|_| "[]".to_string()));
    let summary = body
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let conn = match state.db.lock() {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    };

    if let Err(e) = conn.execute(
        "INSERT INTO meetings (
            id, channel_id, title, status, effective_rounds, started_at, completed_at, summary,
            primary_provider, reviewer_provider, participant_names, created_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
        ON CONFLICT(id) DO UPDATE SET
            channel_id = COALESCE(excluded.channel_id, meetings.channel_id),
            title = COALESCE(?13, meetings.title),
            status = COALESCE(?14, meetings.status),
            effective_rounds = COALESCE(?15, meetings.effective_rounds),
            started_at = COALESCE(meetings.started_at, excluded.started_at),
            completed_at = COALESCE(?16, meetings.completed_at),
            summary = COALESCE(?17, meetings.summary),
            primary_provider = COALESCE(?18, meetings.primary_provider),
            reviewer_provider = COALESCE(?19, meetings.reviewer_provider),
            participant_names = COALESCE(?20, meetings.participant_names),
            created_at = COALESCE(meetings.created_at, excluded.created_at)",
        rusqlite::params![
            body.id,
            body.thread_id,
            agenda,
            status,
            total_rounds,
            started_at,
            body.completed_at,
            summary,
            primary_provider.as_ref().map(ProviderKind::as_str),
            reviewer_provider.as_ref().map(ProviderKind::as_str),
            participant_names_json,
            started_at,
            agenda_update,
            status_update,
            total_rounds_update,
            body.completed_at,
            summary,
            primary_provider.as_ref().map(ProviderKind::as_str),
            reviewer_provider.as_ref().map(ProviderKind::as_str),
            participant_names_update_json,
        ],
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        );
    }

    let mut next_seq = conn
        .query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM meeting_transcripts WHERE meeting_id = ?1",
            [&body.id],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(1);
    let entries = body.entries;
    let replacing_entries = entries.is_some();

    if let Some(entries) = entries {
        let _ = conn.execute(
            "DELETE FROM meeting_transcripts WHERE meeting_id = ?1",
            [&body.id],
        );

        next_seq = 1;
        for (idx, entry) in entries.into_iter().enumerate() {
            let seq = entry.seq.unwrap_or((idx as i64) + 1);
            next_seq = next_seq.max(seq + 1);
            if let Err(e) = conn.execute(
                "INSERT INTO meeting_transcripts (
                    meeting_id, seq, round, speaker_agent_id, speaker_name, content, is_summary
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    body.id,
                    seq,
                    entry.round,
                    entry.speaker_role_id,
                    entry.speaker_name,
                    entry.content,
                    entry.is_summary.unwrap_or(false),
                ],
            ) {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": format!("{e}")})),
                );
            }
        }
    }

    if let Some(summary_text) = summary {
        let summary_round = conn
            .query_row(
                "SELECT effective_rounds FROM meetings WHERE id = ?1",
                [&body.id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .ok()
            .flatten()
            .unwrap_or(total_rounds);
        let existing_summary_id = if replacing_entries {
            None
        } else {
            conn.query_row(
                "SELECT id
                 FROM meeting_transcripts
                 WHERE meeting_id = ?1 AND is_summary = 1
                 ORDER BY seq DESC, id DESC
                 LIMIT 1",
                [&body.id],
                |row| row.get::<_, i64>(0),
            )
            .ok()
        };

        let summary_result = if let Some(summary_id) = existing_summary_id {
            conn.execute(
                "UPDATE meeting_transcripts
                 SET round = ?2,
                     speaker_agent_id = NULL,
                     speaker_name = ?3,
                     content = ?4,
                     is_summary = 1
                 WHERE id = ?1",
                rusqlite::params![
                    summary_id,
                    summary_round,
                    Some("Summary".to_string()),
                    summary_text,
                ],
            )
        } else {
            conn.execute(
                "INSERT INTO meeting_transcripts (
                    meeting_id, seq, round, speaker_agent_id, speaker_name, content, is_summary
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
                rusqlite::params![
                    body.id,
                    next_seq,
                    summary_round,
                    Option::<String>::None,
                    Some("Summary".to_string()),
                    summary_text,
                ],
            )
        };

        if let Err(e) = summary_result {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("{e}")})),
            );
        }
    }

    match conn.query_row(
        "SELECT id, channel_id, title, status, effective_rounds, started_at, completed_at, summary,
                primary_provider, reviewer_provider, participant_names, created_at
         FROM meetings WHERE id = ?1",
        [&body.id],
        |row| meeting_row_to_json(row),
    ) {
        Ok(mut meeting) => {
            let transcripts = load_transcripts(&conn, &body.id);
            let obj = meeting.as_object_mut().unwrap();
            obj.insert("transcripts".to_string(), json!(&transcripts));
            obj.insert("entries".to_string(), json!(transcripts));
            enrich_meeting_with_issue_data(&conn, &body.id, obj);
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

// ── Helpers ────────────────────────────────────────────────────

fn parse_meeting_provider(raw: Option<&str>) -> Result<Option<ProviderKind>, String> {
    let Some(value) = raw.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };

    ProviderKind::from_str(value)
        .map(Some)
        .ok_or_else(|| format!("invalid provider '{}'", value))
}

fn build_meeting_start_command(agenda: &str, primary_provider: Option<ProviderKind>) -> String {
    match primary_provider {
        Some(provider) => format!("/meeting start --primary {} {}", provider.as_str(), agenda),
        None => format!("/meeting start {agenda}"),
    }
}

fn row_optional_timestamp(row: &rusqlite::Row, idx: usize) -> Option<i64> {
    use rusqlite::types::ValueRef;

    match row.get_ref(idx).ok()? {
        ValueRef::Null => None,
        ValueRef::Integer(v) => Some(v),
        ValueRef::Real(v) => Some(v as i64),
        ValueRef::Text(bytes) => {
            let text = std::str::from_utf8(bytes).ok()?.trim();
            if text.is_empty() {
                None
            } else if let Ok(ts) = text.parse::<i64>() {
                Some(ts)
            } else if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(text) {
                Some(dt.timestamp_millis())
            } else if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S")
            {
                Some(dt.and_utc().timestamp_millis())
            } else {
                None
            }
        }
        ValueRef::Blob(_) => None,
    }
}

fn meeting_row_to_json(row: &rusqlite::Row) -> rusqlite::Result<serde_json::Value> {
    let title = row.get::<_, Option<String>>(2)?;
    let effective_rounds = row.get::<_, Option<i64>>(4)?.unwrap_or(0);
    let participant_names = row
        .get::<_, Option<String>>(10)?
        .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
        .unwrap_or_default();
    let started_at = row_optional_timestamp(row, 5).unwrap_or(0);
    let completed_at = row_optional_timestamp(row, 6);
    let created_at = row_optional_timestamp(row, 11).unwrap_or(started_at);
    Ok(json!({
        "id": row.get::<_, String>(0)?,
        "channel_id": row.get::<_, Option<String>>(1)?,
        "title": title,
        "status": row.get::<_, Option<String>>(3)?,
        "effective_rounds": effective_rounds,
        "started_at": started_at,
        "completed_at": completed_at,
        "summary": row.get::<_, Option<String>>(7)?,
        // alias fields for frontend compatibility
        "agenda": title,
        "total_rounds": effective_rounds,
        "primary_provider": row.get::<_, Option<String>>(8)?,
        "reviewer_provider": row.get::<_, Option<String>>(9)?,
        "participant_names": participant_names,
        "issues_created": 0,
        "proposed_issues": null,
        "issue_creation_results": null,
        "issue_repo": null,
        "created_at": created_at,
    }))
}

/// Enrich meeting JSON with issue_repo, issue_creation_results from kv_meta.
fn enrich_meeting_with_issue_data(
    conn: &rusqlite::Connection,
    meeting_id: &str,
    obj: &mut serde_json::Map<String, serde_json::Value>,
) {
    // issue_repo
    let issue_repo: Option<String> = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = ?1",
            [&format!("meeting_issue_repo:{meeting_id}")],
            |row| row.get(0),
        )
        .ok();
    if let Some(ref repo) = issue_repo {
        obj.insert("issue_repo".to_string(), json!(repo));
    }

    // Collect issue creation results from kv_meta
    let mut results = Vec::new();
    let mut i = 0;
    loop {
        let key = format!("meeting:{meeting_id}:issue:item-{i}");
        let url: Option<String> = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = ?1",
                [&format!("{key}:url")],
                |row| row.get(0),
            )
            .ok();
        let discarded: bool = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = ?1",
                [&format!("{key}:discarded")],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .map(|v| v == "true")
            .unwrap_or(false);

        if url.is_none() && !discarded && i > 0 {
            break; // No more items
        }
        if url.is_some() || discarded {
            results.push(json!({
                "key": format!("item-{i}"),
                "ok": url.is_some(),
                "discarded": discarded,
                "issue_url": url,
            }));
        }
        i += 1;
        if i > 100 {
            break;
        } // safety limit
    }

    if !results.is_empty() {
        let created_count = results
            .iter()
            .filter(|entry| entry.get("ok").and_then(|v| v.as_bool()).unwrap_or(false))
            .count();
        obj.insert("issue_creation_results".to_string(), json!(results));
        obj.insert("issues_created".to_string(), json!(created_count));
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::engine::PolicyEngine;
    use crate::services::provider::ProviderKind;
    use axum::{Json, extract::State};
    use std::path::PathBuf;

    fn test_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        crate::db::wrap_conn(conn)
    }

    fn test_engine(db: &Db) -> PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
        config.policies.hot_reload = false;
        PolicyEngine::new(&config, db.clone()).unwrap()
    }

    fn transcript_counts(conn: &rusqlite::Connection, meeting_id: &str) -> (i64, i64) {
        let total = conn
            .query_row(
                "SELECT COUNT(*) FROM meeting_transcripts WHERE meeting_id = ?1",
                [meeting_id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        let summary = conn
            .query_row(
                "SELECT COUNT(*) FROM meeting_transcripts WHERE meeting_id = ?1 AND is_summary = 1",
                [meeting_id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap();
        (total, summary)
    }

    fn meeting_entry(seq: i64, round: i64, speaker_name: &str, content: &str) -> MeetingEntryBody {
        MeetingEntryBody {
            seq: Some(seq),
            round: Some(round),
            speaker_role_id: Some(format!("agent-{seq}")),
            speaker_name: Some(speaker_name.to_string()),
            content: Some(content.to_string()),
            is_summary: Some(false),
        }
    }

    #[test]
    fn build_meeting_start_command_uses_primary_flag_for_qwen() {
        assert_eq!(
            build_meeting_start_command("신규 안건", Some(ProviderKind::Qwen)),
            "/meeting start --primary qwen 신규 안건"
        );
    }

    #[test]
    fn build_meeting_start_command_omits_flag_when_provider_missing() {
        assert_eq!(
            build_meeting_start_command("일반 안건", None),
            "/meeting start 일반 안건"
        );
    }

    #[tokio::test]
    async fn upsert_meeting_preserves_existing_metadata_when_optional_fields_omitted() {
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, _) = upsert_meeting(
            State(state.clone()),
            Json(UpsertMeetingBody {
                id: "meeting-meta".to_string(),
                agenda: Some("기존 안건".to_string()),
                summary: None,
                status: Some("in_progress".to_string()),
                primary_provider: Some("qwen".to_string()),
                reviewer_provider: Some("codex".to_string()),
                participant_names: Some(vec!["Alice".to_string(), "Bob".to_string()]),
                total_rounds: Some(7),
                started_at: Some(111),
                completed_at: None,
                thread_id: Some("thread-1".to_string()),
                entries: Some(vec![meeting_entry(1, 1, "Alice", "초기 기록")]),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = upsert_meeting(
            State(state),
            Json(UpsertMeetingBody {
                id: "meeting-meta".to_string(),
                agenda: None,
                summary: Some("요약 갱신".to_string()),
                status: None,
                primary_provider: None,
                reviewer_provider: None,
                participant_names: None,
                total_rounds: None,
                started_at: None,
                completed_at: Some(222),
                thread_id: None,
                entries: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let conn = db.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT title, status, effective_rounds, completed_at, summary, participant_names
                 FROM meetings WHERE id = ?1",
                ["meeting-meta"],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                },
            )
            .unwrap();

        assert_eq!(row.0.as_deref(), Some("기존 안건"));
        assert_eq!(row.1.as_deref(), Some("in_progress"));
        assert_eq!(row.2, Some(7));
        assert_eq!(row.3, Some(222));
        assert_eq!(row.4.as_deref(), Some("요약 갱신"));
        assert_eq!(row.5.as_deref(), Some("[\"Alice\",\"Bob\"]"));
    }

    #[tokio::test]
    async fn upsert_meeting_preserves_existing_transcripts_and_updates_summary_without_duplication()
    {
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, _) = upsert_meeting(
            State(state.clone()),
            Json(UpsertMeetingBody {
                id: "meeting-transcript".to_string(),
                agenda: Some("안건".to_string()),
                summary: Some("기존 요약".to_string()),
                status: Some("completed".to_string()),
                primary_provider: Some("qwen".to_string()),
                reviewer_provider: Some("codex".to_string()),
                participant_names: Some(vec!["Alice".to_string()]),
                total_rounds: Some(2),
                started_at: Some(100),
                completed_at: Some(150),
                thread_id: Some("thread-2".to_string()),
                entries: Some(vec![
                    meeting_entry(1, 1, "Alice", "첫 발언"),
                    meeting_entry(2, 2, "Bob", "두 번째 발언"),
                ]),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = upsert_meeting(
            State(state),
            Json(UpsertMeetingBody {
                id: "meeting-transcript".to_string(),
                agenda: None,
                summary: Some("새 요약".to_string()),
                status: Some("completed".to_string()),
                primary_provider: None,
                reviewer_provider: None,
                participant_names: None,
                total_rounds: None,
                started_at: None,
                completed_at: Some(200),
                thread_id: None,
                entries: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let conn = db.lock().unwrap();
        let (total, summary_count) = transcript_counts(&conn, "meeting-transcript");
        assert_eq!(total, 3);
        assert_eq!(summary_count, 1);

        let transcript_rows = load_transcripts(&conn, "meeting-transcript");
        let contents: Vec<&str> = transcript_rows
            .iter()
            .filter_map(|row| row.get("content").and_then(|value| value.as_str()))
            .collect();
        assert!(contents.contains(&"첫 발언"));
        assert!(contents.contains(&"두 번째 발언"));
        assert!(contents.contains(&"새 요약"));
    }
}
