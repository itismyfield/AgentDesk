use poise::serenity_prelude as serenity;

use super::super::{Context, check_auth};

pub(super) const DISCORD_AUTOCOMPLETE_LIMIT: usize = 25;
pub(super) const DISCORD_CHOICE_NAME_LIMIT: usize = 100;

pub(super) async fn autocomplete_resume_session<'a>(
    ctx: Context<'a>,
    partial: &'a str,
) -> Vec<serenity::AutocompleteChoice> {
    if !check_auth(
        ctx.author().id,
        &ctx.author().name,
        &ctx.data().shared,
        &ctx.data().token,
    )
    .await
    {
        return Vec::new();
    }

    let shared = &ctx.data().shared;
    let Some(pool) = shared.pg_pool.clone() else {
        return Vec::new();
    };
    let Some(session_key) = super::super::adk_session::build_adk_session_key(
        shared,
        ctx.channel_id(),
        &ctx.data().provider,
        None,
    )
    .await
    else {
        return Vec::new();
    };
    let forward_context =
        crate::services::session_forwarding::ForwardCallerContext::from_live_globals(Some(
            pool.clone(),
        ));
    let (status, body) = crate::services::session_resume::dispatch_resume_candidates(
        &pool,
        &forward_context,
        crate::services::session_resume::ResumeForwardingMetadata::default(),
        &session_key,
    )
    .await;
    if !status.is_success() {
        return Vec::new();
    }

    let partial = partial.trim().to_lowercase();
    let candidates = body
        .get("candidates")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_candidate);
    build_choices(candidates, &partial, now_unix_ms())
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct CandidateView {
    pub(super) session_id: String,
    pub(super) cwd: String,
    pub(super) title: Option<String>,
    pub(super) modified_at_ms: u64,
}

fn parse_candidate(value: &serde_json::Value) -> Option<CandidateView> {
    Some(CandidateView {
        session_id: value.get("session_id")?.as_str()?.to_string(),
        cwd: value.get("cwd")?.as_str()?.to_string(),
        title: value
            .get("title")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        modified_at_ms: value.get("modified_at_ms")?.as_u64()?,
    })
}

pub(super) fn build_choices(
    candidates: impl IntoIterator<Item = CandidateView>,
    partial: &str,
    now_ms: u64,
) -> Vec<serenity::AutocompleteChoice> {
    candidates
        .into_iter()
        .filter(|candidate| candidate_matches(candidate, partial))
        .take(DISCORD_AUTOCOMPLETE_LIMIT)
        .map(|candidate| {
            serenity::AutocompleteChoice::new(
                candidate_label(&candidate, now_ms),
                format!("pick:{}", candidate.session_id),
            )
        })
        .collect()
}

pub(super) fn candidate_matches(candidate: &CandidateView, partial: &str) -> bool {
    partial.is_empty()
        || candidate.session_id.to_lowercase().contains(partial)
        || candidate.cwd.to_lowercase().contains(partial)
        || candidate
            .title
            .as_deref()
            .is_some_and(|title| title.to_lowercase().contains(partial))
}

pub(super) fn candidate_label(candidate: &CandidateView, now_ms: u64) -> String {
    let title = candidate
        .title
        .as_deref()
        .map(sanitize_label_part)
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| candidate.session_id.clone());
    let worktree = std::path::Path::new(&candidate.cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .map(sanitize_label_part)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| sanitize_label_part(&candidate.cwd));
    let worktree = if worktree.is_empty() {
        "?".to_string()
    } else {
        worktree
    };
    let relative = relative_time(now_ms, candidate.modified_at_ms);
    let separator_chars = " ·  · ".chars().count();
    let component_budget =
        DISCORD_CHOICE_NAME_LIMIT.saturating_sub(relative.chars().count() + separator_chars);
    let initial_worktree_budget = worktree
        .chars()
        .count()
        .min(30)
        .min(component_budget.saturating_sub(1));
    let title_budget = component_budget.saturating_sub(initial_worktree_budget);
    let title = truncate_chars(&title, title_budget);
    let unused_title_budget = title_budget.saturating_sub(title.chars().count());
    let worktree = truncate_chars(
        &worktree,
        initial_worktree_budget.saturating_add(unused_title_budget),
    );

    format!("{title} · {relative} · {worktree}")
}

fn sanitize_label_part(value: &str) -> String {
    value
        .chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    value.chars().take(max_chars).collect()
}

fn relative_time(now_ms: u64, modified_at_ms: u64) -> String {
    let seconds = now_ms.saturating_sub(modified_at_ms) / 1_000;
    match seconds {
        0..=59 => "방금 전".to_string(),
        60..=3_599 => format!("{}분 전", seconds / 60),
        3_600..=86_399 => format!("{}시간 전", seconds / 3_600),
        _ => format!("{}일 전", seconds / 86_400),
    }
}

fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
