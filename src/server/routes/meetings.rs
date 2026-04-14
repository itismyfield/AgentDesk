use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
};
use poise::serenity_prelude::ChannelId;
use serde::Deserialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};

use super::AppState;
use crate::services::discord::{health, meeting, settings};
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
    pub reviewer_provider: Option<String>,
    pub fixed_participants: Option<Vec<String>>,
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
    pub channel_id: Option<String>,
    pub agenda: Option<String>,
    pub summary: Option<String>,
    pub selection_reason: Option<String>,
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

fn normalize_selection_reason(value: &str) -> Option<String> {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = compact.trim();
    if trimmed.is_empty() {
        return None;
    }

    let normalized = trimmed
        .strip_prefix("선정 사유:")
        .map(str::trim)
        .filter(|inner| !inner.is_empty())
        .unwrap_or(trimmed);

    Some(normalized.to_string())
}

fn selection_reason_needs_fallback(value: Option<&str>) -> bool {
    let Some(reason) = value.and_then(normalize_selection_reason) else {
        return true;
    };

    let compact = reason.split_whitespace().collect::<String>();
    compact.contains("안건적합도와후보메타데이터적합도")
        || compact.contains("고정전문에이전트조건을함께반영")
        || compact.contains("커버리지를우선해자동구성했어")
        || compact.contains("자동구성했어")
        || (reason.starts_with("안건의 ")
            && reason.contains("축을 기준으로")
            && reason.contains("조합으로 정리했어"))
}

fn tokenize_selection_reason_text(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();

    for ch in text.chars() {
        if ch.is_alphanumeric() {
            current.extend(ch.to_lowercase());
        } else if !current.is_empty() {
            if current.chars().count() >= 2 {
                tokens.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
    }

    if current.chars().count() >= 2 {
        tokens.push(current);
    }

    tokens
}

fn compact_reason_fragment(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn clean_reason_signal_fragment(value: &str) -> Option<String> {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut trimmed = compact.trim();
    if trimmed.is_empty() {
        return None;
    }

    for prefix in ["BLOCKED.", "BLOCKED:", "CONSENSUS:", "이견:", "합의:"] {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            trimmed = rest.trim();
        }
    }

    trimmed = trimmed.trim_matches(|ch: char| matches!(ch, '"' | '\'' | '`' | '“' | '”'));
    trimmed = trimmed.trim_start_matches(|ch: char| !ch.is_alphanumeric());
    trimmed = trimmed.trim_end_matches(|ch: char| !ch.is_alphanumeric());

    if trimmed.chars().count() < 3 {
        return None;
    }

    Some(trimmed.to_string())
}

fn transcript_signal_candidates_from_content(content: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    let mut push_value = |value: &str| {
        let Some(cleaned) = clean_reason_signal_fragment(value) else {
            return;
        };
        let key = cleaned.to_lowercase();
        if seen.insert(key) {
            candidates.push(cleaned);
        }
    };

    let mut in_backticks = false;
    let mut backtick_buffer = String::new();
    for ch in content.chars() {
        if ch == '`' {
            if in_backticks {
                push_value(&backtick_buffer);
                backtick_buffer.clear();
            }
            in_backticks = !in_backticks;
            continue;
        }
        if in_backticks {
            backtick_buffer.push(ch);
        }
    }

    for fragment in content.split(|ch| ['\n', '.', '!', '?', ',', ';', ':'].contains(&ch)) {
        push_value(fragment);
    }

    candidates
}

fn json_string_array(value: Option<&serde_json::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn selection_signal_candidates_from_expert(expert: &serde_json::Value) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();

    let mut push_value = |value: &str| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return;
        }
        let key = trimmed.to_lowercase();
        if seen.insert(key) {
            candidates.push(trimmed.to_string());
        }
    };

    for value in json_string_array(expert.get("task_types")) {
        push_value(&value);
    }
    for value in json_string_array(expert.get("strengths")) {
        push_value(&value);
    }
    for value in json_string_array(expert.get("keywords")) {
        push_value(&value);
    }
    if let Some(summary) = expert
        .get("domain_summary")
        .and_then(|value| value.as_str())
    {
        for fragment in summary.split(|ch| ['.', '\n', ',', ';', '·', '/', '|'].contains(&ch)) {
            push_value(fragment);
        }
    }

    candidates
}

fn score_signal_against_agenda(
    signal: &str,
    agenda_lower: &str,
    agenda_tokens: &HashSet<String>,
) -> usize {
    if agenda_lower.is_empty() {
        return 0;
    }

    let signal_lower = signal.trim().to_lowercase();
    if signal_lower.is_empty() {
        return 0;
    }

    let mut score = 0;
    if agenda_lower.contains(&signal_lower) || signal_lower.contains(agenda_lower) {
        score += 6;
    }

    let matched_tokens = tokenize_selection_reason_text(&signal_lower)
        .into_iter()
        .filter(|token| agenda_tokens.contains(token))
        .count();

    score + matched_tokens * 3
}

fn score_signal_for_reason(
    signal: &str,
    agenda_lower: &str,
    agenda_tokens: &HashSet<String>,
) -> usize {
    let trimmed = signal.trim();
    if trimmed.is_empty() {
        return 0;
    }

    let mut score = score_signal_against_agenda(trimmed, agenda_lower, agenda_tokens);
    let length = trimmed.chars().count();

    if (4..=36).contains(&length) {
        score += 2;
    }
    if trimmed.chars().any(|ch| ch.is_ascii_digit()) {
        score += 3;
    }
    if trimmed.chars().any(|ch| ch.is_ascii_uppercase()) {
        score += 1;
    }
    if trimmed.contains("Top") || trimmed.contains("TOP") {
        score += 2;
    }
    if trimmed.split_whitespace().count() <= 7 {
        score += 1;
    }

    score
}

fn build_expert_reason_clause(
    expert: Option<&serde_json::Value>,
    transcript_signals: &[String],
    agenda_lower: &str,
    agenda_tokens: &HashSet<String>,
    fallback_label: &str,
) -> (usize, String, Option<String>) {
    let display_name = expert
        .and_then(|value| value.get("display_name"))
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(fallback_label);

    let mut signal_candidates = Vec::new();
    let mut seen_signals = HashSet::new();
    let mut push_signal = |signal: String| {
        let trimmed = signal.trim();
        if trimmed.is_empty() {
            return;
        }
        let key = trimmed.to_lowercase();
        if seen_signals.insert(key) {
            signal_candidates.push(trimmed.to_string());
        }
    };

    if let Some(expert) = expert {
        for signal in selection_signal_candidates_from_expert(expert) {
            push_signal(signal);
        }
    }
    for signal in transcript_signals {
        push_signal(signal.clone());
    }

    let mut scored_signals: Vec<(usize, String)> = signal_candidates
        .iter()
        .cloned()
        .map(|signal| {
            (
                score_signal_for_reason(&signal, agenda_lower, agenda_tokens),
                signal,
            )
        })
        .collect();

    scored_signals.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.chars().count().cmp(&b.1.chars().count()))
    });

    let mut detail_parts = Vec::new();
    let mut detail_seen = HashSet::new();
    let mut best_score = 0;
    for (score, signal) in scored_signals {
        if score == 0 {
            continue;
        }
        if best_score == 0 {
            best_score = score;
        }
        let compact = compact_reason_fragment(&signal);
        let key = compact.to_lowercase();
        if detail_seen.insert(key) {
            detail_parts.push(compact);
        }
        if detail_parts.len() >= 2 {
            break;
        }
    }

    if detail_parts.is_empty() {
        if let Some(fallback) = signal_candidates.first().cloned() {
            detail_parts.push(compact_reason_fragment(&fallback));
        }
    }

    let focus = detail_parts.first().cloned();
    let clause = if detail_parts.is_empty() {
        display_name.to_string()
    } else {
        format!("{display_name}({})", detail_parts.join("·"))
    };

    (best_score, clause, focus)
}

fn derive_selection_reason_from_meeting_data(
    agenda: &str,
    participant_count: usize,
    transcripts: &[serde_json::Value],
    experts: &[serde_json::Value],
) -> Option<String> {
    let agenda_compact = compact_reason_fragment(agenda);
    let agenda_lower = agenda.trim().to_lowercase();
    let agenda_tokens: HashSet<String> =
        tokenize_selection_reason_text(agenda).into_iter().collect();

    let mut role_ids = Vec::new();
    let mut seen_role_ids = HashSet::new();
    let mut transcript_signals_by_role: HashMap<String, Vec<String>> = HashMap::new();
    for entry in transcripts {
        if entry
            .get("is_summary")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        let Some(role_id) = entry
            .get("speaker_agent_id")
            .or_else(|| entry.get("speaker_role_id"))
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let role_id = role_id.to_string();
        if seen_role_ids.insert(role_id.clone()) {
            role_ids.push(role_id.clone());
        }
        if let Some(content) = entry.get("content").and_then(|value| value.as_str()) {
            let signals = transcript_signals_by_role.entry(role_id).or_default();
            let mut seen_signals: HashSet<String> =
                signals.iter().map(|value| value.to_lowercase()).collect();
            for signal in transcript_signal_candidates_from_content(content) {
                let key = signal.to_lowercase();
                if seen_signals.insert(key) {
                    signals.push(signal);
                }
            }
        }
    }

    if role_ids.is_empty() {
        return None;
    }

    let experts_by_id: HashMap<&str, &serde_json::Value> = experts
        .iter()
        .filter_map(|expert| {
            expert
                .get("role_id")
                .and_then(|value| value.as_str())
                .map(|role_id| (role_id, expert))
        })
        .collect();

    let mut participant_clauses = Vec::new();
    let mut focus_labels = Vec::new();
    let mut seen_focus = HashSet::new();
    for role_id in &role_ids {
        let transcript_signals = transcript_signals_by_role
            .get(role_id)
            .cloned()
            .unwrap_or_default();
        let (score, clause, focus) = build_expert_reason_clause(
            experts_by_id.get(role_id.as_str()).copied(),
            &transcript_signals,
            &agenda_lower,
            &agenda_tokens,
            role_id,
        );
        if let Some(label) = focus {
            let key = label.to_lowercase();
            if seen_focus.insert(key) {
                focus_labels.push(label);
            }
        }
        participant_clauses.push((score, clause));
    }

    participant_clauses.sort_by(|a, b| b.0.cmp(&a.0));

    let mut roster_labels: Vec<String> = participant_clauses
        .iter()
        .take(2)
        .map(|(_, clause)| clause.clone())
        .collect();
    if participant_clauses.len() > roster_labels.len() {
        roster_labels.push(format!(
            "외 {}명",
            participant_clauses.len() - roster_labels.len()
        ));
    }

    let focus = if !focus_labels.is_empty() {
        focus_labels
            .into_iter()
            .take(2)
            .collect::<Vec<_>>()
            .join(" · ")
    } else if !agenda_compact.is_empty() {
        agenda_compact.clone()
    } else {
        "핵심 전문성".to_string()
    };

    let roster = if roster_labels.is_empty() {
        "선정된 전문가들".to_string()
    } else {
        roster_labels.join(", ")
    };

    let count = participant_count.max(role_ids.len());
    Some(format!(
        "안건인 {agenda_compact}에 맞춰 {roster}를 묶었고, 회의 기록에선 {focus}가 반복돼 총 {count}명 조합으로 정리했어."
    ))
}

fn apply_selection_reason_fallback(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    transcripts: &[serde_json::Value],
) {
    let existing = obj.get("selection_reason").and_then(|value| value.as_str());
    if !selection_reason_needs_fallback(existing) {
        return;
    }

    let agenda = obj
        .get("agenda")
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    let participant_count = obj
        .get("participant_names")
        .and_then(|value| value.as_array())
        .map(|items| items.len())
        .unwrap_or(0);
    let experts = meeting::list_available_agent_options();

    if let Some(derived) =
        derive_selection_reason_from_meeting_data(agenda, participant_count, transcripts, &experts)
    {
        obj.insert("selection_reason".to_string(), json!(derived));
    }
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
        "SELECT id, channel_id, thread_id, title, status, effective_rounds, started_at, completed_at, summary,
                primary_provider, reviewer_provider, participant_names, selection_reason, created_at
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
            obj.insert("entries".to_string(), json!(&transcripts));
            enrich_meeting_with_issue_data(&conn, &mid, obj);
            apply_selection_reason_fallback(obj, &transcripts);
        }
    }

    (StatusCode::OK, Json(json!({"meetings": meetings})))
}

/// GET /api/round-table-meetings/channels
pub async fn list_meeting_channels(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let bindings = settings::list_registered_channel_bindings();
    let registry = state.health_registry.as_ref();
    let available_experts = meeting::list_available_agent_options();

    let mut channels = Vec::new();
    for binding in bindings {
        let channel_id = ChannelId::new(binding.channel_id);
        let fallback_name = binding
            .fallback_name
            .clone()
            .unwrap_or_else(|| format!("channel-{}", binding.channel_id));
        let channel_name = match registry {
            Some(registry) if binding.fallback_name.is_none() => {
                health::fetch_channel_name(registry, channel_id, &binding.owner_provider)
                    .await
                    .unwrap_or(fallback_name)
            }
            _ => fallback_name,
        };

        channels.push(json!({
            "channel_id": binding.channel_id.to_string(),
            "channel_name": channel_name,
            "owner_provider": binding.owner_provider.as_str(),
            "available_experts": available_experts.clone(),
        }));
    }

    channels.sort_by(|left, right| {
        let left_name = left
            .get("channel_name")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let right_name = right
            .get("channel_name")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        left_name.cmp(right_name).then_with(|| {
            left.get("channel_id")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .cmp(
                    right
                        .get("channel_id")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default(),
                )
        })
    });

    (StatusCode::OK, Json(json!({"channels": channels})))
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
        "SELECT id, channel_id, thread_id, title, status, effective_rounds, started_at, completed_at, summary,
                primary_provider, reviewer_provider, participant_names, selection_reason, created_at
         FROM meetings WHERE id = ?1",
        [&id],
        |row| meeting_row_to_json(row),
    ) {
        Ok(mut meeting) => {
            let transcripts = load_transcripts(&conn, &id);
            let obj = meeting.as_object_mut().unwrap();
            obj.insert("transcripts".to_string(), json!(&transcripts));
            obj.insert("entries".to_string(), json!(&transcripts));
            enrich_meeting_with_issue_data(&conn, &id, obj);
            apply_selection_reason_fallback(obj, &transcripts);
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
        "SELECT id, channel_id, thread_id, title, status, effective_rounds, started_at, completed_at, summary,
                primary_provider, reviewer_provider, participant_names, selection_reason, created_at
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

        // Use the shared async/timeout-bounded GitHub helper so this route
        // does not block the Tokio worker on a direct gh subprocess call.
        match crate::github::create_issue(&repo, title, &body_text).await {
            Ok(issue) => {
                let url = issue.url;
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
            "SELECT id, channel_id, thread_id, title, status, effective_rounds, started_at, completed_at, summary,
                    primary_provider, reviewer_provider, participant_names, selection_reason, created_at
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
            "SELECT id, channel_id, thread_id, title, status, effective_rounds, started_at, completed_at, summary,
                    primary_provider, reviewer_provider, participant_names, selection_reason, created_at
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
/// Start a meeting directly via the provider-bound runtime.
pub async fn start_meeting(
    State(state): State<AppState>,
    Json(body): Json<StartMeetingBody>,
) -> (StatusCode, Json<serde_json::Value>) {
    let channel_id_raw = match &body.channel_id {
        Some(id) if !id.is_empty() => id.clone(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "channel_id is required"})),
            );
        }
    };
    let channel_id_value = match channel_id_raw.parse::<u64>() {
        Ok(value) => value,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "channel_id must be a numeric string"})),
            );
        }
    };
    let requested_primary_provider = match parse_meeting_provider(body.primary_provider.as_deref())
    {
        Ok(provider) => provider,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error})));
        }
    };
    let (owner_provider, primary_provider) = match resolve_start_meeting_providers(
        resolve_channel_owner_provider(channel_id_value),
        requested_primary_provider,
    ) {
        Ok(providers) => providers,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error})));
        }
    };
    let Some(registry) = state.health_registry.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "health registry unavailable"})),
        );
    };

    let agenda = body.agenda.as_deref().unwrap_or("General discussion");

    let reviewer_provider = match parse_required_meeting_provider(body.reviewer_provider.as_deref())
    {
        Ok(provider) => provider,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": error})));
        }
    };
    if let Err(error) =
        validate_reviewer_provider(&primary_provider, &reviewer_provider, &owner_provider)
    {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": error})));
    }

    match health::start_direct_meeting(
        registry,
        ChannelId::new(channel_id_value),
        owner_provider,
        primary_provider,
        reviewer_provider,
        agenda.to_string(),
        body.fixed_participants.unwrap_or_default(),
    )
    .await
    {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({"ok": true, "message": "Meeting start scheduled"})),
        ),
        Err(error) => {
            let error_message = normalize_direct_start_error(&error);
            (
                direct_start_error_status(&error_message),
                Json(json!({"ok": false, "error": error_message})),
            )
        }
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
    let selection_reason = body
        .selection_reason
        .as_deref()
        .and_then(normalize_selection_reason);

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
            id, channel_id, thread_id, title, status, effective_rounds, started_at, completed_at, summary,
            primary_provider, reviewer_provider, participant_names, selection_reason, created_at
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
        ON CONFLICT(id) DO UPDATE SET
            channel_id = COALESCE(excluded.channel_id, meetings.channel_id),
            thread_id = COALESCE(excluded.thread_id, meetings.thread_id),
            title = COALESCE(?15, meetings.title),
            status = COALESCE(?16, meetings.status),
            effective_rounds = COALESCE(?17, meetings.effective_rounds),
            started_at = COALESCE(meetings.started_at, excluded.started_at),
            completed_at = COALESCE(?18, meetings.completed_at),
            summary = COALESCE(?19, meetings.summary),
            primary_provider = COALESCE(?20, meetings.primary_provider),
            reviewer_provider = COALESCE(?21, meetings.reviewer_provider),
            participant_names = COALESCE(?22, meetings.participant_names),
            selection_reason = COALESCE(?23, meetings.selection_reason),
            created_at = COALESCE(meetings.created_at, excluded.created_at)",
        rusqlite::params![
            body.id,
            body.channel_id,
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
            selection_reason.clone(),
            started_at,
            agenda_update,
            status_update,
            total_rounds_update,
            body.completed_at,
            summary,
            primary_provider.as_ref().map(ProviderKind::as_str),
            reviewer_provider.as_ref().map(ProviderKind::as_str),
            participant_names_update_json,
            selection_reason,
        ],
    ) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("{e}")})),
        );
    }

    let saved_thread_id: Option<String> = conn
        .query_row(
            "SELECT thread_id FROM meetings WHERE id = ?1",
            [&body.id],
            |row| row.get(0),
        )
        .ok()
        .flatten();
    if let Err(e) = persist_meeting_query_hashes(&conn, &body.id, saved_thread_id.as_deref()) {
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
        "SELECT id, channel_id, thread_id, title, status, effective_rounds, started_at, completed_at, summary,
                primary_provider, reviewer_provider, participant_names, selection_reason, created_at
         FROM meetings WHERE id = ?1",
        [&body.id],
        |row| meeting_row_to_json(row),
    ) {
        Ok(mut meeting) => {
            let transcripts = load_transcripts(&conn, &body.id);
            let obj = meeting.as_object_mut().unwrap();
            obj.insert("transcripts".to_string(), json!(&transcripts));
            obj.insert("entries".to_string(), json!(&transcripts));
            enrich_meeting_with_issue_data(&conn, &body.id, obj);
            apply_selection_reason_fallback(obj, &transcripts);
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

fn parse_required_meeting_provider(raw: Option<&str>) -> Result<ProviderKind, String> {
    parse_meeting_provider(raw)?.ok_or_else(|| "reviewer_provider is required".to_string())
}

fn resolve_channel_owner_provider(channel_id: u64) -> Option<ProviderKind> {
    settings::list_registered_channel_bindings()
        .into_iter()
        .find(|binding| binding.channel_id == channel_id)
        .map(|binding| binding.owner_provider)
}

fn resolve_start_meeting_providers(
    owner_provider: Option<ProviderKind>,
    requested_primary_provider: Option<ProviderKind>,
) -> Result<(ProviderKind, ProviderKind), String> {
    match owner_provider {
        Some(owner_provider) => {
            let primary_provider =
                requested_primary_provider.unwrap_or_else(|| owner_provider.clone());
            Ok((owner_provider, primary_provider))
        }
        None => Err("channel_id is not a registered meeting channel".to_string()),
    }
}

fn validate_reviewer_provider(
    primary_provider: &ProviderKind,
    reviewer_provider: &ProviderKind,
    owner_provider: &ProviderKind,
) -> Result<(), String> {
    if reviewer_provider == owner_provider {
        return Err("reviewer_provider must differ from channel owner provider".to_string());
    }
    if reviewer_provider == primary_provider {
        return Err("reviewer_provider must differ from primary_provider".to_string());
    }
    Ok(())
}

fn normalize_direct_start_error(error: &str) -> String {
    serde_json::from_str::<serde_json::Value>(error)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| error.to_string())
}

fn direct_start_error_status(error: &str) -> StatusCode {
    if error.contains("이미 회의가 진행 중")
        || error.to_ascii_lowercase().contains("already in progress")
    {
        return StatusCode::CONFLICT;
    }

    if error.contains("Too many fixed participants")
        || error.contains("Unknown fixed meeting participant role_id")
        || error.contains("reviewer_provider must differ")
    {
        return StatusCode::BAD_REQUEST;
    }

    StatusCode::INTERNAL_SERVER_ERROR
}

fn build_meeting_start_command(agenda: &str, primary_provider: Option<ProviderKind>) -> String {
    match primary_provider {
        Some(provider) => format!("/meeting start --primary {} {}", provider.as_str(), agenda),
        None => format!("/meeting start {agenda}"),
    }
}

fn short_query_hash(input: &str) -> String {
    use sha2::{Digest, Sha256};

    let digest = Sha256::digest(input.as_bytes());
    hex::encode(&digest[..6])
}

fn meeting_query_hash(meeting_id: &str) -> String {
    format!(
        "#meeting-{}",
        short_query_hash(&format!("meeting:{meeting_id}"))
    )
}

fn thread_query_hash(thread_id: &str) -> String {
    format!(
        "#thread-{}",
        short_query_hash(&format!("thread:{thread_id}"))
    )
}

fn persist_meeting_query_hashes(
    conn: &rusqlite::Connection,
    meeting_id: &str,
    thread_id: Option<&str>,
) -> rusqlite::Result<()> {
    let meeting_hash = meeting_query_hash(meeting_id);
    let normalized_thread_id = thread_id.map(str::trim).filter(|value| !value.is_empty());
    let thread_hash = normalized_thread_id.map(thread_query_hash);
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
        rusqlite::params![
            format!("meeting_query_hash:{meeting_id}"),
            meeting_hash.clone()
        ],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
        rusqlite::params![
            format!("meeting_query_hash_lookup:{meeting_hash}"),
            meeting_id
        ],
    )?;

    if let (Some(thread_id), Some(thread_hash)) = (normalized_thread_id, thread_hash.as_deref()) {
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
            rusqlite::params![format!("meeting_thread_hash:{meeting_id}"), thread_hash],
        )?;
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
            rusqlite::params![
                format!("meeting_thread_hash_lookup:{thread_hash}"),
                json!({"meeting_id": meeting_id, "thread_id": thread_id}).to_string()
            ],
        )?;
    }

    tracing::info!(
        meeting_id = %meeting_id,
        meeting_hash = %meeting_hash,
        thread_hash = thread_hash.as_deref().unwrap_or("-"),
        "[meetings] persisted meeting query hashes"
    );

    Ok(())
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
    let meeting_id = row.get::<_, String>(0)?;
    let channel_id = row.get::<_, Option<String>>(1)?;
    let thread_id = row.get::<_, Option<String>>(2)?;
    let title = row.get::<_, Option<String>>(3)?;
    let effective_rounds = row.get::<_, Option<i64>>(5)?.unwrap_or(0);
    let participant_names = row
        .get::<_, Option<String>>(11)?
        .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
        .unwrap_or_default();
    let started_at = row_optional_timestamp(row, 6).unwrap_or(0);
    let completed_at = row_optional_timestamp(row, 7);
    let created_at = row_optional_timestamp(row, 13).unwrap_or(started_at);
    let thread_hash = thread_id.as_deref().map(thread_query_hash);
    let selection_reason = row
        .get::<_, Option<String>>(12)?
        .as_deref()
        .and_then(normalize_selection_reason);
    Ok(json!({
        "id": meeting_id.clone(),
        "channel_id": channel_id,
        "thread_id": thread_id,
        "meeting_hash": meeting_query_hash(&meeting_id),
        "thread_hash": thread_hash,
        "title": title,
        "status": row.get::<_, Option<String>>(4)?,
        "effective_rounds": effective_rounds,
        "started_at": started_at,
        "completed_at": completed_at,
        "summary": row.get::<_, Option<String>>(8)?,
        "selection_reason": selection_reason,
        // alias fields for frontend compatibility
        "agenda": title,
        "total_rounds": effective_rounds,
        "primary_provider": row.get::<_, Option<String>>(9)?,
        "reviewer_provider": row.get::<_, Option<String>>(10)?,
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
    use axum::{
        Json,
        extract::{Path, State},
    };
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

    #[test]
    fn short_human_selection_reason_does_not_need_fallback() {
        assert!(!selection_reason_needs_fallback(Some("초기 조합 선정")));
        assert!(!selection_reason_needs_fallback(Some(
            "후속 업데이트에서도 선정 사유 유지"
        )));
    }

    #[test]
    fn compact_reason_fragment_preserves_full_text_without_ellipsis() {
        let fragment = compact_reason_fragment("긴   선정   사유   문장 전체");
        assert_eq!(fragment, "긴 선정 사유 문장 전체");
        assert!(!fragment.contains('…'));
    }

    #[test]
    fn validate_reviewer_provider_accepts_distinct_provider() {
        assert_eq!(
            validate_reviewer_provider(
                &ProviderKind::Claude,
                &ProviderKind::Codex,
                &ProviderKind::Gemini,
            ),
            Ok(())
        );
    }

    #[test]
    fn validate_reviewer_provider_rejects_primary_provider_match() {
        assert_eq!(
            validate_reviewer_provider(
                &ProviderKind::Claude,
                &ProviderKind::Claude,
                &ProviderKind::Gemini,
            ),
            Err("reviewer_provider must differ from primary_provider".to_string())
        );
    }

    #[test]
    fn validate_reviewer_provider_rejects_owner_provider_match() {
        assert_eq!(
            validate_reviewer_provider(
                &ProviderKind::Claude,
                &ProviderKind::Gemini,
                &ProviderKind::Gemini,
            ),
            Err("reviewer_provider must differ from channel owner provider".to_string())
        );
    }

    #[test]
    fn resolve_start_meeting_providers_prefers_registered_owner_when_available() {
        assert_eq!(
            resolve_start_meeting_providers(Some(ProviderKind::Claude), None),
            Ok((ProviderKind::Claude, ProviderKind::Claude))
        );
        assert_eq!(
            resolve_start_meeting_providers(Some(ProviderKind::Claude), Some(ProviderKind::Qwen)),
            Ok((ProviderKind::Claude, ProviderKind::Qwen))
        );
    }

    #[test]
    fn resolve_start_meeting_providers_rejects_unregistered_channel() {
        assert_eq!(
            resolve_start_meeting_providers(None, None),
            Err("channel_id is not a registered meeting channel".to_string())
        );
        assert_eq!(
            resolve_start_meeting_providers(None, Some(ProviderKind::Gemini)),
            Err("channel_id is not a registered meeting channel".to_string())
        );
    }

    #[test]
    fn parse_required_meeting_provider_rejects_missing_provider() {
        assert_eq!(
            parse_required_meeting_provider(None),
            Err("reviewer_provider is required".to_string())
        );
    }

    #[tokio::test]
    async fn start_meeting_rejects_unregistered_channel_without_primary_provider() {
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, body) = start_meeting(
            State(state),
            Json(StartMeetingBody {
                agenda: Some("안건".to_string()),
                channel_id: Some("999999999999".to_string()),
                primary_provider: None,
                reviewer_provider: Some("codex".to_string()),
                fixed_participants: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body.0["error"],
            "channel_id is not a registered meeting channel"
        );
    }

    #[tokio::test]
    async fn start_meeting_rejects_unregistered_channel_even_when_primary_provider_is_supplied() {
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, body) = start_meeting(
            State(state),
            Json(StartMeetingBody {
                agenda: Some("안건".to_string()),
                channel_id: Some("999999999999".to_string()),
                primary_provider: Some("qwen".to_string()),
                reviewer_provider: Some("codex".to_string()),
                fixed_participants: None,
            }),
        )
        .await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body.0["error"],
            "channel_id is not a registered meeting channel"
        );
    }

    #[test]
    fn normalize_direct_start_error_extracts_embedded_json_message() {
        let raw = r#"{"ok":false,"error":"provider runtime not registered: codex"}"#;
        assert_eq!(
            normalize_direct_start_error(raw),
            "provider runtime not registered: codex"
        );
    }

    #[test]
    fn direct_start_error_status_maps_known_validation_and_conflict_errors() {
        assert_eq!(
            direct_start_error_status("이 채널에서 이미 회의가 진행 중이야."),
            StatusCode::CONFLICT
        );
        assert_eq!(
            direct_start_error_status("Too many fixed participants: 6 (max 5)"),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            direct_start_error_status("Unknown fixed meeting participant role_id: role-123"),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            direct_start_error_status("provider runtime not registered: codex"),
            StatusCode::INTERNAL_SERVER_ERROR
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
                channel_id: None,
                agenda: Some("기존 안건".to_string()),
                summary: None,
                selection_reason: Some(
                    "고정 전문 에이전트를 유지하고 핵심 전문성을 보완하는 조합으로 선정"
                        .to_string(),
                ),
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
                channel_id: None,
                agenda: None,
                summary: Some("요약 갱신".to_string()),
                selection_reason: Some(
                    "리뷰 반영 후 중복 전문성을 줄이고 핵심 축을 유지하는 조합으로 확정"
                        .to_string(),
                ),
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
                "SELECT title, status, effective_rounds, completed_at, summary, participant_names, selection_reason
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
                        row.get::<_, Option<String>>(6)?,
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
        assert_eq!(
            row.6.as_deref(),
            Some("리뷰 반영 후 중복 전문성을 줄이고 핵심 축을 유지하는 조합으로 확정")
        );
    }

    #[tokio::test]
    async fn upsert_meeting_persists_query_hashes_and_returns_them() {
        let db = test_db();
        let state = AppState::test_state(db.clone(), test_engine(&db));

        let (status, _) = upsert_meeting(
            State(state.clone()),
            Json(UpsertMeetingBody {
                id: "meeting-hash".to_string(),
                channel_id: None,
                agenda: Some("해시 안건".to_string()),
                summary: None,
                selection_reason: Some("해시 검증을 위해 핵심 참여자만 압축 선정".to_string()),
                status: Some("in_progress".to_string()),
                primary_provider: Some("qwen".to_string()),
                reviewer_provider: Some("codex".to_string()),
                participant_names: Some(vec!["Alice".to_string()]),
                total_rounds: Some(1),
                started_at: Some(111),
                completed_at: None,
                thread_id: Some("thread-123".to_string()),
                entries: None,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        let expected_meeting_hash = meeting_query_hash("meeting-hash");
        let expected_thread_hash = thread_query_hash("thread-123");
        let (status, body) = get_meeting(State(state), Path("meeting-hash".to_string())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.0["meeting"]["meeting_hash"], expected_meeting_hash);
        assert_eq!(body.0["meeting"]["thread_hash"], expected_thread_hash);
        assert_eq!(
            body.0["meeting"]["selection_reason"],
            json!("해시 검증을 위해 핵심 참여자만 압축 선정")
        );

        let conn = db.lock().unwrap();
        let stored_meeting_hash: String = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = ?1",
                ["meeting_query_hash:meeting-hash"],
                |row| row.get(0),
            )
            .unwrap();
        let meeting_lookup: String = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = ?1",
                [format!("meeting_query_hash_lookup:{expected_meeting_hash}")],
                |row| row.get(0),
            )
            .unwrap();
        let stored_thread_hash: String = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = ?1",
                ["meeting_thread_hash:meeting-hash"],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(stored_meeting_hash, expected_meeting_hash);
        assert_eq!(meeting_lookup, "meeting-hash");
        assert_eq!(stored_thread_hash, expected_thread_hash);
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
                channel_id: None,
                agenda: Some("안건".to_string()),
                summary: Some("기존 요약".to_string()),
                selection_reason: Some("초기 조합 선정".to_string()),
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
                channel_id: None,
                agenda: None,
                summary: Some("새 요약".to_string()),
                selection_reason: Some("후속 업데이트에서도 선정 사유 유지".to_string()),
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
