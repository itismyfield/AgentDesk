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
use crate::db::meetings::{
    TranscriptEntry, UpsertMeetingParams, delete_meeting_pg, discard_all_issues_pg,
    discard_issue_pg, get_effective_rounds_pg, get_issue_url_pg, get_latest_summary_id_pg,
    get_meeting_issue_repo_pg, get_meeting_summaries_pg, get_meeting_thread_id_pg,
    get_next_transcript_seq_pg, insert_summary_transcript_pg, is_issue_discarded_pg,
    list_meetings_pg, load_meeting_pg, meeting_existed_pg, meeting_exists_pg,
    persist_meeting_query_hashes_pg, replace_transcripts_pg, store_issue_url_pg,
    update_summary_transcript_pg, upsert_issue_repo_pg, upsert_meeting_record_pg,
};
use crate::error::{AppError, AppResult, ErrorCode};
use crate::services::discord::meeting_artifact_store::UpsertMeetingBody;
use crate::services::discord::{health, meeting, settings};
use crate::services::github_issue_creation::{
    GitHubIssueCreateRequest, create_github_issue_with_side_effects,
};
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

// `MeetingEntryBody` and `UpsertMeetingBody` now live beside their
// service-layer producers in `crate::services::discord::meeting_artifact_store`
// so those callers can depend on the request shape they build without a
// server-layer backflow (#3037). `UpsertMeetingBody` is re-`use`d above.

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

    trimmed =
        trimmed.trim_matches(|ch: char| matches!(ch, '"' | '\'' | '`' | '\u{201C}' | '\u{201D}'));
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
pub async fn list_meetings(
    State(state): State<AppState>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Database,
            "postgres pool unavailable",
        ));
    };

    let meetings = list_meetings_pg(pool, apply_selection_reason_fallback)
        .await
        .map_err(|error| {
            AppError::internal(format!("query: {error}")).with_code(ErrorCode::Database)
        })?;
    Ok((StatusCode::OK, Json(json!({"meetings": meetings}))))
}

/// GET /api/round-table-meetings/channels
pub async fn list_meeting_channels(
    State(state): State<AppState>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
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

    Ok((StatusCode::OK, Json(json!({"channels": channels}))))
}

/// GET /api/round-table-meetings/:id
pub async fn get_meeting(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Database,
            "postgres pool unavailable",
        ));
    };

    match load_meeting_pg(pool, &id, apply_selection_reason_fallback).await {
        Ok(Some(meeting)) => Ok((StatusCode::OK, Json(json!({"meeting": meeting})))),
        Ok(None) => return Err(AppError::not_found("meeting not found")),
        Err(e) => return Err(AppError::internal(format!("{e}")).with_code(ErrorCode::Database)),
    }
}

/// DELETE /api/round-table-meetings/:id
pub async fn delete_meeting(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Database,
            "postgres pool unavailable",
        ));
    };

    match delete_meeting_pg(pool, &id).await {
        Ok(true) => Ok((StatusCode::OK, Json(json!({"ok": true})))),
        Ok(false) => Err(AppError::not_found("meeting not found")),
        Err(e) => return Err(AppError::internal(format!("{e}")).with_code(ErrorCode::Database)),
    }
}

/// PATCH /api/round-table-meetings/:id/issue-repo
pub async fn update_issue_repo(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<IssueRepoBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Database,
            "postgres pool unavailable",
        ));
    };

    if !meeting_exists_pg(pool, &id).await {
        return Err(AppError::not_found("meeting not found"));
    }

    // Store issue_repo in kv_meta (meetings table doesn't have issue_repo column)
    let repo_str = body.repo.as_deref().unwrap_or("");
    if let Err(e) = upsert_issue_repo_pg(pool, &id, repo_str).await {
        return Err(AppError::internal(format!("{e}")).with_code(ErrorCode::Database));
    }

    match load_meeting_pg(pool, &id, apply_selection_reason_fallback).await {
        Ok(Some(mut meeting)) => {
            meeting
                .as_object_mut()
                .unwrap()
                .insert("issue_repo".to_string(), json!(body.repo));
            Ok((
                StatusCode::OK,
                Json(json!({"ok": true, "meeting": meeting})),
            ))
        }
        Ok(None) => return Err(AppError::not_found("meeting not found")),
        Err(e) => return Err(AppError::internal(format!("{e}")).with_code(ErrorCode::Database)),
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
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Database,
            "postgres pool unavailable",
        ));
    };
    let (repo, summaries) = {
        if !meeting_exists_pg(pool, &id).await {
            return Err(AppError::not_found("meeting not found"));
        }

        // Get issue repo from kv_meta or request body
        let repo: Option<String> = if body.repo.is_some() {
            body.repo.clone()
        } else {
            get_meeting_issue_repo_pg(pool, &id).await
        };

        let Some(repo) = repo else {
            return Err(AppError::bad_request(
                "no repo configured for this meeting — set issue_repo first",
            ));
        };

        let summaries = get_meeting_summaries_pg(pool, &id).await;
        (repo, summaries)
    };

    if summaries.is_empty() {
        return Ok((
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "skipped": true,
                "results": [],
                "summary": {"total": 0, "created": 0, "failed": 0, "discarded": 0, "pending": 0, "all_created": true, "all_resolved": true}
            })),
        ));
    }
    // Create issues from summaries using gh CLI
    let mut results = Vec::new();
    let mut created = 0i64;
    let mut failed = 0i64;

    for (i, summary) in summaries.iter().enumerate() {
        let key = format!("item-{i}");
        // Check if already discarded
        let discarded = is_issue_discarded_pg(pool, &id, &key).await;
        if discarded {
            results.push(json!({"key": key, "title": summary.lines().next().unwrap_or(""), "assignee": "", "ok": true, "discarded": true, "attempted_at": 0}));
            continue;
        }

        // Check if already created
        let already_url = get_issue_url_pg(pool, &id, &key).await;
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

        let issue_create_request = GitHubIssueCreateRequest::github_only(
            "meeting_issue_creation",
            repo.clone(),
            title.to_string(),
            body_text.clone(),
            "meeting issue creation stores issue_url in meeting metadata and intentionally skips kanban/announcement sync",
        );

        match create_github_issue_with_side_effects(Some(pool), issue_create_request).await {
            Ok(result) => {
                let issue_number = result.issue.number;
                let url = result.issue.url.clone();
                let kanban_card_sync = result.kanban.clone();
                let announcement_sync = result.announcement.clone();
                // Store result
                let _ = store_issue_url_pg(pool, &id, &key, &url).await;
                results.push(json!({
                    "key": key,
                    "title": title,
                    "assignee": "",
                    "ok": true,
                    "issue_url": url,
                    "issue_number": issue_number,
                    "issue_creation_origin": result.origin,
                    "issue_creation_mode": result.mode.as_str(),
                    "kanban_card_sync": kanban_card_sync,
                    "announcement_sync": announcement_sync,
                    "attempted_at": chrono::Utc::now().timestamp()
                }));
                created += 1;
            }
            Err(error) => {
                results.push(json!({"key": key, "title": title, "assignee": "", "ok": false, "error": error.to_string(), "attempted_at": chrono::Utc::now().timestamp()}));
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

    Ok((
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
    ))
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
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let key = body.key.as_deref().unwrap_or("");
    if key.is_empty() {
        return Err(AppError::bad_request("key is required"));
    }

    let Some(pool) = state.pg_pool_ref() else {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Database,
            "postgres pool unavailable",
        ));
    };

    let _ = discard_issue_pg(pool, &id, key).await;

    // Return meeting + summary for UI refresh
    let meeting = load_meeting_pg(pool, &id, apply_selection_reason_fallback)
        .await
        .ok()
        .flatten()
        .unwrap_or(json!(null));

    Ok((
        StatusCode::OK,
        Json(
            json!({"ok": true, "meeting": meeting, "summary": {"total": 0, "created": 0, "failed": 0, "discarded": 1, "pending": 0, "all_created": false, "all_resolved": false}}),
        ),
    ))
}

/// POST /api/round-table-meetings/:id/issues/discard-all
pub async fn discard_all_issues(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let Some(pool) = state.pg_pool_ref() else {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Database,
            "postgres pool unavailable",
        ));
    };

    let count = discard_all_issues_pg(pool, &id).await;

    let meeting = load_meeting_pg(pool, &id, apply_selection_reason_fallback)
        .await
        .ok()
        .flatten()
        .unwrap_or(json!(null));

    Ok((
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "meeting": meeting,
            "results": [],
            "summary": {"total": count, "created": 0, "failed": 0, "discarded": count, "pending": 0, "all_created": false, "all_resolved": true}
        })),
    ))
}

/// POST /api/round-table-meetings/start
/// Start a meeting directly via the provider-bound runtime.
pub async fn start_meeting(
    State(state): State<AppState>,
    Json(body): Json<StartMeetingBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    let channel_id_raw = match &body.channel_id {
        Some(id) if !id.is_empty() => id.clone(),
        _ => {
            return Err(AppError::bad_request("channel_id is required"));
        }
    };
    let channel_id_value = match channel_id_raw.parse::<u64>() {
        Ok(value) => value,
        Err(_) => {
            return Err(AppError::bad_request("channel_id must be a numeric string"));
        }
    };
    let requested_primary_provider = match parse_meeting_provider(body.primary_provider.as_deref())
    {
        Ok(provider) => provider,
        Err(error) => {
            return Err(AppError::bad_request(error));
        }
    };
    let (owner_provider, primary_provider) = match resolve_start_meeting_providers(
        resolve_channel_owner_provider(channel_id_value),
        requested_primary_provider,
    ) {
        Ok(providers) => providers,
        Err(error) => {
            return Err(AppError::bad_request(error));
        }
    };
    let Some(registry) = state.health_registry.as_ref() else {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Discord,
            "health registry unavailable",
        ));
    };

    let agenda = body.agenda.as_deref().unwrap_or("General discussion");

    let reviewer_provider = match parse_required_meeting_provider(body.reviewer_provider.as_deref())
    {
        Ok(provider) => provider,
        Err(error) => {
            return Err(AppError::bad_request(error));
        }
    };
    if let Err(error) =
        validate_reviewer_provider(&primary_provider, &reviewer_provider, &owner_provider)
    {
        return Err(AppError::bad_request(error));
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
        Ok(()) => Ok((
            StatusCode::OK,
            Json(json!({"ok": true, "message": "Meeting start scheduled"})),
        )),
        Err(error) => {
            let error_message = normalize_direct_start_error(&error);
            Ok((
                direct_start_error_status(&error_message),
                Json(json!({"ok": false, "error": error_message})),
            ))
        }
    }
}

/// POST /api/round-table-meetings
/// Persist completed/cancelled meeting payloads posted back from the Discord runtime.
pub async fn upsert_meeting(
    State(state): State<AppState>,
    Json(body): Json<UpsertMeetingBody>,
) -> AppResult<(StatusCode, Json<serde_json::Value>)> {
    if body.id.trim().is_empty() {
        return Err(AppError::bad_request("meeting id is required"));
    }

    let primary_provider = match parse_meeting_provider(body.primary_provider.as_deref()) {
        Ok(provider) => provider,
        Err(error) => {
            return Err(AppError::bad_request(error));
        }
    };
    let reviewer_provider = match parse_meeting_provider(body.reviewer_provider.as_deref()) {
        Ok(provider) => provider.or_else(|| primary_provider.clone().map(|p| p.counterpart())),
        Err(error) => {
            return Err(AppError::bad_request(error));
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

    let Some(pool) = state.pg_pool_ref() else {
        return Err(AppError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Database,
            "postgres pool unavailable",
        ));
    };

    // #2050 P1 finding 1 — detect whether this upsert creates or updates a meeting
    // so we can broadcast the correct round_table_new / round_table_update event.
    let meeting_existed = meeting_existed_pg(pool, &body.id).await;

    let started_at_dt = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(started_at)
        .unwrap_or_else(chrono::Utc::now);
    let completed_at_dt = body
        .completed_at
        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis);

    if let Err(e) = upsert_meeting_record_pg(
        pool,
        &UpsertMeetingParams {
            id: &body.id,
            channel_id: body.channel_id.as_deref(),
            thread_id: body.thread_id.as_deref(),
            agenda,
            status,
            total_rounds,
            started_at_dt,
            completed_at_dt,
            summary,
            primary_provider: primary_provider.as_ref().map(ProviderKind::as_str),
            reviewer_provider: reviewer_provider.as_ref().map(ProviderKind::as_str),
            participant_names_json: &participant_names_json,
            selection_reason: selection_reason.as_deref(),
            created_at: started_at,
            agenda_update,
            status_update,
            total_rounds_update,
            participant_names_update_json: participant_names_update_json.as_deref(),
        },
    )
    .await
    {
        return Err(AppError::internal(format!("{e}")).with_code(ErrorCode::Database));
    }

    let saved_thread_id = get_meeting_thread_id_pg(pool, &body.id).await;
    if let Err(e) =
        persist_meeting_query_hashes_pg(pool, &body.id, saved_thread_id.as_deref()).await
    {
        return Err(AppError::internal(format!("{e}")).with_code(ErrorCode::Database));
    }

    let mut next_seq = get_next_transcript_seq_pg(pool, &body.id).await;
    let entries = body.entries;
    let replacing_entries = entries.is_some();

    if let Some(entries) = entries {
        let transcript_entries: Vec<TranscriptEntry> = entries
            .into_iter()
            .enumerate()
            .map(|(idx, entry)| {
                let seq = entry.seq.unwrap_or((idx as i64) + 1);
                TranscriptEntry {
                    seq,
                    round: entry.round,
                    speaker_role_id: entry.speaker_role_id,
                    speaker_name: entry.speaker_name,
                    content: entry.content,
                    is_summary: entry.is_summary.unwrap_or(false),
                }
            })
            .collect();

        match replace_transcripts_pg(pool, &body.id, &transcript_entries).await {
            Ok(new_next_seq) => {
                next_seq = new_next_seq;
            }
            Err(e) => {
                return Err(AppError::internal(format!("{e}")).with_code(ErrorCode::Database));
            }
        }
    }

    if let Some(summary_text) = summary {
        let summary_round = get_effective_rounds_pg(pool, &body.id)
            .await
            .unwrap_or(total_rounds);
        let existing_summary_id = if replacing_entries {
            None
        } else {
            get_latest_summary_id_pg(pool, &body.id).await
        };

        let summary_result = if let Some(summary_id) = existing_summary_id {
            update_summary_transcript_pg(pool, summary_id, summary_round, summary_text).await
        } else {
            insert_summary_transcript_pg(pool, &body.id, next_seq, summary_round, summary_text)
                .await
        };

        if let Err(e) = summary_result {
            return Err(AppError::internal(format!("{e}")).with_code(ErrorCode::Database));
        }
    }

    match load_meeting_pg(pool, &body.id, apply_selection_reason_fallback).await {
        Ok(Some(meeting)) => {
            // #2050 P1 finding 1 — broadcast round_table_new / round_table_update so
            // other dashboard clients reflect the upsert without manual refresh.
            let event_name = if meeting_existed {
                "round_table_update"
            } else {
                "round_table_new"
            };
            crate::server::ws::emit_event(&state.broadcast_tx, event_name, meeting.clone());
            Ok((
                StatusCode::OK,
                Json(json!({"ok": true, "meeting": meeting})),
            ))
        }
        Ok(None) => {
            return Err(
                AppError::internal("meeting was not persisted").with_code(ErrorCode::Database)
            );
        }
        Err(e) => return Err(AppError::internal(format!("{e}")).with_code(ErrorCode::Database)),
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
