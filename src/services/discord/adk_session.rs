use std::path::Path;
use std::sync::Arc;

use poise::serenity_prelude as serenity;

use super::SharedData;
use crate::services::observability::turn_lifecycle::{
    ContextCompactionDetails, TurnEvent, TurnLifecycleEmit, emit_turn_lifecycle,
    provider_session_fingerprint,
};
use crate::services::provider::ProviderKind;

const SESSION_INFO_MAX_CHARS: usize = 60;
pub(super) const CONTEXT_COMPACTION_PRESERVED_SECTIONS: [&str; 5] =
    ["Goal", "Progress", "Decisions", "Files", "Next"];

/// Parse `DISPATCH:<uuid> - <title>` format and return the dispatch_id (uuid part).
pub(super) fn parse_dispatch_id(text: &str) -> Option<String> {
    // Search each line for "DISPATCH:" prefix (the message may have a
    // decorative header line like "── implementation dispatch ──" before it).
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("DISPATCH:") {
            let rest = rest.trim();
            let id = rest.split_whitespace().next().unwrap_or(rest).trim();
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    None
}

/// #222: Look up a pending implementation/rework dispatch for a thread channel
/// via the ADK API. Used as fallback when parse_dispatch_id fails (unified threads
/// where user_text doesn't contain DISPATCH: prefix).
pub(super) async fn lookup_pending_dispatch_for_thread(
    _api_port: u16,
    thread_channel_id: u64,
) -> Option<String> {
    let body = super::internal_api::lookup_pending_dispatch_for_thread(thread_channel_id)
        .await
        .ok()?;
    body.get("dispatch_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

pub(super) fn parse_thread_channel_id_from_name(channel_name: &str) -> Option<u64> {
    let pos = channel_name.rfind("-t")?;
    let suffix = &channel_name[pos + 2..];
    if suffix.len() < 15 || !suffix.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    suffix.parse::<u64>().ok()
}

pub(super) async fn build_adk_session_key(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
) -> Option<String> {
    let channel_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|s| s.channel_name.as_ref())
            .cloned()
    }
    .or_else(|| registered_channel_fallback_name(channel_id, provider))?;
    let tmux_name = provider.build_tmux_session_name(&channel_name);

    Some(build_namespaced_session_key(
        &shared.token_hash,
        provider,
        &tmux_name,
    ))
}

pub(super) fn registered_channel_fallback_name(
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
) -> Option<String> {
    super::settings::list_registered_channel_bindings()
        .into_iter()
        .find(|binding| {
            binding.channel_id == channel_id.get() && binding.owner_provider == *provider
        })
        .and_then(|binding| binding.fallback_name)
}

pub(in crate::services::discord) fn build_namespaced_session_key(
    token_hash: &str,
    provider: &ProviderKind,
    tmux_name: &str,
) -> String {
    let hostname = crate::services::platform::hostname_short();
    format!(
        "{}/{}/{}:{}",
        provider.as_str(),
        token_hash,
        hostname,
        tmux_name
    )
}

pub(in crate::services::discord) fn build_legacy_session_key(tmux_name: &str) -> String {
    let hostname = crate::services::platform::hostname_short();
    format!("{}:{}", hostname, tmux_name)
}

pub(in crate::services::discord) fn build_session_key_candidates(
    token_hash: &str,
    provider: &ProviderKind,
    tmux_name: &str,
) -> [String; 2] {
    [
        build_namespaced_session_key(token_hash, provider, tmux_name),
        build_legacy_session_key(tmux_name),
    ]
}

fn legacy_session_key_from_namespaced(session_key: &str) -> Option<String> {
    let mut parts = session_key.splitn(3, '/');
    let _provider = parts.next()?;
    let _token_hash = parts.next()?;
    let legacy = parts.next()?.trim();
    if legacy.is_empty() {
        return None;
    }
    Some(legacy.to_string())
}

pub(super) fn derive_adk_session_info(
    user_text: Option<&str>,
    channel_name: Option<&str>,
    current_path: Option<&str>,
) -> String {
    if let Some(text) = user_text.and_then(normalize_user_task_summary) {
        return text;
    }

    let base = current_path.and_then(path_label).or_else(|| {
        channel_name
            .and_then(clean_nonempty)
            .map(trim_channel_suffix)
            .map(str::to_string)
    });
    let action = user_text.and_then(infer_generic_task_action);

    if let Some(base) = base {
        return describe_task(&base, action);
    }

    if let Some(action) = action {
        return format!("AgentDesk {} 작업 진행 중", action);
    }

    if let Some(channel) = channel_name.and_then(clean_nonempty) {
        return format!("{} 작업 진행 중", trim_channel_suffix(channel));
    }

    if let Some(label) = current_path.and_then(path_label) {
        return format!("{} 작업 진행 중", label);
    }

    "AgentDesk 작업 진행 중".to_string()
}

pub(super) async fn post_adk_session_status(
    session_key: Option<&str>,
    name: Option<&str>,
    model: Option<&str>,
    status: &str,
    provider: &ProviderKind,
    session_info: Option<&str>,
    tokens: Option<u64>,
    cwd: Option<&str>,
    dispatch_id: Option<&str>,
    thread_channel_id: Option<u64>,
    channel_id: Option<serenity::ChannelId>,
    agent_id: Option<&str>,
    _api_port: u16,
) {
    let Some(session_key) = session_key else {
        return;
    };
    let status = crate::db::session_status::normalize_incoming_session_status(Some(status));

    let body = crate::services::dispatched_sessions::HookSessionBody {
        session_key: session_key.to_string(),
        instance_id: None,
        agent_id: agent_id.map(str::to_string),
        status: Some(status.to_string()),
        provider: Some(provider.as_str().to_string()),
        session_info: session_info.map(str::to_string),
        name: name.and_then(clean_nonempty).map(str::to_string),
        model: model
            .and_then(clean_nonempty)
            .filter(|value| !value.eq_ignore_ascii_case(provider.as_str()))
            .map(str::to_string),
        tokens,
        cwd: cwd.and_then(clean_nonempty).map(str::to_string),
        dispatch_id: dispatch_id.and_then(clean_nonempty).map(str::to_string),
        thread_channel_id: thread_channel_id.map(|id| id.to_string()),
        claude_session_id: None,
        session_id: None,
        channel_id: channel_id.map(|id| id.get().to_string()),
    };

    if let Err(err) = super::internal_api::hook_session(body).await {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!("  [{ts}] ⚠ ADK session POST failed: {err}");
    }
}

/// Delete a session row from the DB by session_key.
/// Used to clean up thread sessions after dispatch completion.
pub(super) async fn delete_adk_session(session_key: &str, _api_port: u16) {
    if let Err(err) = super::internal_api::delete_session(session_key).await {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!("  [{ts}] ⚠ ADK session DELETE failed: {err}");
    }

    if let Some(legacy_key) = legacy_session_key_from_namespaced(session_key) {
        let _ = super::internal_api::delete_session(&legacy_key).await;
    }
}

/// Clear the stored provider session_id from DB for a given session_key.
/// Called when the user runs /clear so the next turn doesn't resume a dead session.
pub(super) async fn clear_provider_session_id(session_key: &str, _api_port: u16) {
    if let Err(err) = super::internal_api::clear_session_id(session_key).await {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!("  [{ts}] ⚠ clear_provider_session_id failed: {err}");
    }

    if let Some(legacy_key) = legacy_session_key_from_namespaced(session_key) {
        let _ = super::internal_api::clear_session_id(&legacy_key).await;
    }
}

/// Save a provider session selector to DB so it survives dcserver restarts.
/// The executable selector stays in the legacy `claude_session_id` column for
/// compatibility, while the raw observed provider session id travels through
/// `session_id` and is persisted separately by the server route.
pub(super) async fn save_provider_session_id(
    session_key: &str,
    session_id: &str,
    raw_provider_session_id: Option<&str>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    _api_port: u16,
) {
    let body = crate::services::dispatched_sessions::HookSessionBody {
        session_key: session_key.to_string(),
        instance_id: None,
        agent_id: None,
        status: None,
        provider: Some(provider.as_str().to_string()),
        session_info: None,
        name: None,
        model: None,
        tokens: None,
        cwd: None,
        dispatch_id: None,
        thread_channel_id: None,
        claude_session_id: Some(session_id.to_string()),
        session_id: raw_provider_session_id.map(str::to_string),
        channel_id: Some(channel_id.get().to_string()),
    };
    if let Err(err) = super::internal_api::hook_session(body).await {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!("  [{ts}] ⚠ save_provider_session_id failed: {err}");
    }
}

pub(crate) fn context_usage_percent(tokens: u64, context_window: u64) -> u64 {
    if context_window == 0 {
        return 0;
    }
    let percent = ((u128::from(tokens) * 100) / u128::from(context_window)) as u64;
    percent.min(100)
}

pub(super) fn context_compaction_details(
    before_pct: u64,
    after_pct: Option<u64>,
) -> ContextCompactionDetails {
    ContextCompactionDetails {
        before_pct,
        after_pct,
        preserved_sections: CONTEXT_COMPACTION_PRESERVED_SECTIONS
            .iter()
            .map(|section| (*section).to_string())
            .collect(),
    }
}

pub(super) async fn emit_context_compacted_lifecycle_for_inflight(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
    before_pct: u64,
    after_pct: Option<u64>,
) -> bool {
    let Some(pool) = shared.pg_pool.as_ref() else {
        return false;
    };
    let Some(inflight) = super::inflight::load_inflight_state(provider, channel_id.get()) else {
        return false;
    };
    if inflight.rebind_origin || inflight.user_msg_id == 0 {
        return false;
    }

    let turn_id = format!("discord:{}:{}", channel_id.get(), inflight.user_msg_id);
    let mut emit = TurnLifecycleEmit::new(
        turn_id.clone(),
        channel_id.get().to_string(),
        TurnEvent::ContextCompacted(context_compaction_details(before_pct, after_pct)),
        "automatic context compaction completed",
    );
    if let Some(session_key) = inflight
        .session_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        emit = emit.session_key(session_key.to_string());
    }
    if let Some(dispatch_id) = inflight
        .dispatch_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        emit = emit.dispatch_id(dispatch_id.to_string());
    }

    match emit_turn_lifecycle(pool, emit).await {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(error) => {
            tracing::warn!(
                "failed to emit context compacted lifecycle event for turn {}: {error}",
                turn_id
            );
            false
        }
    }
}

/// Fetch the stored executable provider session selector from DB for a given session_key.
/// Prefer the server-resolved `session_id` field: it may distrust a stale
/// legacy `claude_session_id` when the raw provider selector points at the
/// growing transcript. Legacy fields remain fallbacks for older endpoints.
pub(super) async fn fetch_provider_session_id(
    session_key: &str,
    provider: &ProviderKind,
    api_port: u16,
) -> Option<String> {
    if let Some(found) = fetch_provider_session_id_once(session_key, provider, api_port).await {
        return Some(found);
    }

    let Some(legacy_key) = legacy_session_key_from_namespaced(session_key) else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] [session-restore] no provider session selector for key={} provider={} legacy_key_present=false",
            session_key,
            provider.as_str()
        );
        return None;
    };

    let restored = fetch_provider_session_id_once(&legacy_key, provider, api_port).await;
    if restored.is_none() {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] [session-restore] no provider session selector for key={} provider={} legacy_key_present=true",
            session_key,
            provider.as_str()
        );
    }
    restored
}

async fn fetch_provider_session_id_once(
    session_key: &str,
    provider: &ProviderKind,
    _api_port: u16,
) -> Option<String> {
    let json = match super::internal_api::get_provider_session_id(
        session_key,
        Some(provider.as_str()),
    )
    .await
    {
        Ok(json) => json,
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ [session-restore] provider session lookup failed: key={} provider={} error={}",
                session_key,
                provider.as_str(),
                error
            );
            return None;
        }
    };
    // #107: Filter empty strings — a stale clear path may have stored ""
    // instead of NULL; treat it as no session ID.
    // Also try session_id field as fallback for provider-agnostic lookup.
    // #4091: GET /claude-session-id resolves `session_id` after freshness
    // checking `claude_session_id` against `raw_provider_session_id`. Trust that
    // selected field first so a frozen transient Claude transcript cannot shadow
    // the still-growing raw provider session.
    let selector = json
        .get("session_id")
        .and_then(|v| v.as_str())
        .or_else(|| json.get("claude_session_id").and_then(|v| v.as_str()))
        .or_else(|| json.get("raw_provider_session_id").and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let ts = chrono::Local::now().format("%H:%M:%S");
    if let Some(ref selector) = selector {
        tracing::info!(
            "  [{ts}] [session-restore] provider session selector found: key={} provider={} selector_fp={}",
            session_key,
            provider.as_str(),
            provider_session_fingerprint(selector)
        );
    } else {
        let has_claude_selector = json
            .get("claude_session_id")
            .and_then(|v| v.as_str())
            .map(|value| !value.is_empty())
            .unwrap_or(false);
        let has_raw_session_id = json
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(|value| !value.is_empty())
            .unwrap_or(false);
        let has_raw_provider_session_id = json
            .get("raw_provider_session_id")
            .and_then(|v| v.as_str())
            .map(|value| !value.is_empty())
            .unwrap_or(false);
        tracing::info!(
            "  [{ts}] [session-restore] provider session lookup returned no usable selector: key={} provider={} has_claude_selector={} has_session_id={} has_raw_provider_session_id={}",
            session_key,
            provider.as_str(),
            has_claude_selector,
            has_raw_session_id,
            has_raw_provider_session_id
        );
    }
    selector
}

fn normalize_user_task_summary(input: &str) -> Option<String> {
    let first_line = input
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("```"))?;

    let collapsed = collapse_whitespace(trim_leading_marker(
        first_line.replace('`', " ").replace("```", " ").trim(),
    ));

    if collapsed.is_empty()
        || looks_like_raw_command_or_path(&collapsed)
        || looks_like_generic_user_ack(&collapsed)
    {
        return None;
    }

    Some(truncate_chars(&collapsed, SESSION_INFO_MAX_CHARS))
}

fn trim_leading_marker(input: &str) -> &str {
    let mut text = input.trim();
    loop {
        let trimmed = text.trim_start_matches(['-', '*', '#', '>', ' ']);
        if trimmed != text {
            text = trimmed.trim_start();
            continue;
        }

        let bytes = text.as_bytes();
        let mut idx = 0;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx > 0 && idx < bytes.len() && (bytes[idx] == b'.' || bytes[idx] == b')') {
            text = text[idx + 1..].trim_start();
            continue;
        }

        break;
    }
    text.trim()
}

fn looks_like_raw_command_or_path(text: &str) -> bool {
    let lower = text.to_lowercase();
    let command_prefixes = [
        "/",
        "~/",
        "./",
        "cd ",
        "git ",
        "cargo ",
        "npm ",
        "pnpm ",
        "yarn ",
        "sed ",
        "cat ",
        "rg ",
        "ls ",
        "find ",
        "curl ",
        "python ",
        "python3 ",
        "bash ",
        "zsh ",
        "sh ",
        "launchctl ",
        "tmux ",
        "agentdesk ",
        "agentdesk ",
    ];

    command_prefixes
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

fn looks_like_generic_user_ack(text: &str) -> bool {
    let lower = text.trim().to_lowercase();
    let char_count = lower.chars().count();
    let exact_matches = [
        "ㅇㅇ",
        "ㅇㅋ",
        "ㄱㄱ",
        "고고",
        "ok",
        "okay",
        "yes",
        "응",
        "그래",
        "좋아",
        "알겠어",
        "알겠음",
        "됐다",
        "됐어",
        "진행해",
        "계속해",
        "맞춰줘",
        "고쳐줘",
        "고쳐",
        "해줘",
        "해봐",
        "봐줘",
        "검증해",
        "테스트해",
        "배포해",
        "재시작해",
        "확인해",
    ];

    if exact_matches.contains(&lower.as_str()) {
        return true;
    }

    char_count <= 8
        && (lower.ends_with("해줘")
            || lower.ends_with("해봐")
            || lower.ends_with("해")
            || lower.ends_with("봐줘"))
}

fn infer_generic_task_action(input: &str) -> Option<&'static str> {
    let lower = input.trim().to_lowercase();

    if lower.is_empty() {
        return None;
    }

    if ["검증", "테스트", "스모크", "확인", "체크"]
        .iter()
        .any(|keyword| lower.contains(keyword))
    {
        return Some("검증");
    }
    if ["배포", "릴리즈", "설치", "promote"]
        .iter()
        .any(|keyword| lower.contains(keyword))
    {
        return Some("배포");
    }
    if ["재시작", "restart", "kickstart"]
        .iter()
        .any(|keyword| lower.contains(keyword))
    {
        return Some("재시작");
    }
    if ["고쳐", "수정", "맞춰", "개선", "다듬", "정리"]
        .iter()
        .any(|keyword| lower.contains(keyword))
    {
        return Some("개선");
    }
    if ["구현", "추가", "만들", "작성"]
        .iter()
        .any(|keyword| lower.contains(keyword))
    {
        return Some("구현");
    }

    None
}

fn describe_task(base: &str, action: Option<&str>) -> String {
    match action {
        Some(action) => format!("{} {} 작업 진행 중", base, action),
        None => format!("{} 작업 진행 중", base),
    }
}

fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(input: &str, max_chars: usize) -> String {
    let char_count = input.chars().count();
    if char_count <= max_chars {
        return input.to_string();
    }

    if max_chars <= 1 {
        return "…".to_string();
    }

    input.chars().take(max_chars - 1).collect::<String>() + "…"
}

fn trim_channel_suffix(input: &str) -> &str {
    input
        .strip_suffix("-cc")
        .or_else(|| input.strip_suffix("-cdx"))
        .unwrap_or(input)
}

fn path_label(path: &str) -> Option<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return None;
    }

    Path::new(trimmed)
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(clean_nonempty)
        .map(|name| name.to_string())
}

fn clean_nonempty(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

#[cfg(test)]
mod parse_dispatch_id_tests {
    use super::parse_dispatch_id;

    #[test]
    fn parse_dispatch_id_strips_profile_label() {
        let result = parse_dispatch_id(
            "DISPATCH:550e8400-e29b-41d4-a716-446655440000 [review] - #2762 Review",
        );
        assert_eq!(
            result,
            Some("550e8400-e29b-41d4-a716-446655440000".to_string())
        );
    }
}

#[cfg(test)]
mod context_usage_tests {
    use super::context_usage_percent;

    #[test]
    fn context_usage_percent_is_bounded_to_window() {
        assert_eq!(context_usage_percent(850, 1_000), 85);
        assert_eq!(context_usage_percent(1_780, 1_000), 100);
        assert_eq!(context_usage_percent(1, 0), 0);
    }
}

#[cfg(test)]
mod compact_threshold_tests {
    use super::ContextThresholds;
    use crate::services::provider::ProviderKind;

    /// A configured `context_compact_percent_claude` value must remain distinct
    /// from the generic and Codex percentages.
    #[test]
    fn compact_pct_for_claude_uses_claude_specific_override() {
        let thresholds = ContextThresholds {
            compact_pct: 80,
            compact_pct_codex: 100,
            compact_pct_claude: 60,
            compact_lower_bound_tokens: 300_000,
            context_window: 1_000_000,
        };
        // Claude takes its own override, distinct from the generic value.
        assert_eq!(thresholds.compact_pct_for(&ProviderKind::Claude), 60);
        // Codex still uses its own override.
        assert_eq!(thresholds.compact_pct_for(&ProviderKind::Codex), 100);
        // Other providers fall back to the generic value.
        assert_eq!(thresholds.compact_pct_for(&ProviderKind::Gemini), 80);
        // The launch path turns this percentage into an absolute safe window.
        assert!(thresholds.compact_pct_for(&ProviderKind::Claude) > 0);
    }

    /// When only the generic threshold is configured, Claude inherits it.
    /// `fetch_context_thresholds` defaults `compact_pct_claude` to the generic
    /// `compact_pct`, so this mirrors the runtime fallback behaviour.
    #[test]
    fn compact_pct_for_claude_falls_back_to_generic() {
        let thresholds = ContextThresholds {
            compact_pct: 55,
            compact_pct_codex: 100,
            compact_pct_claude: 55,
            compact_lower_bound_tokens: 300_000,
            context_window: 1_000_000,
        };
        assert_eq!(thresholds.compact_pct_for(&ProviderKind::Claude), 55);
    }
}

/// Context window management thresholds.
/// Single source of truth used by Rust turn-end compact logic.
/// Provider-specific overrides: `context_compact_percent_codex`, `context_compact_percent_claude`, etc.
pub(super) struct ContextThresholds {
    pub compact_pct: u64,
    /// Provider-specific override (if set). Falls back to compact_pct.
    pub compact_pct_codex: u64,
    /// Claude-specific override (if set). Falls back to compact_pct.
    pub compact_pct_claude: u64,
    /// Provider-neutral minimum occupancy before compacting. Claude applies it
    /// to its model-aware context window; unset settings default to 300k.
    pub compact_lower_bound_tokens: u64,
    pub context_window: u64,
}

impl Default for ContextThresholds {
    fn default() -> Self {
        Self {
            compact_pct: 60,
            compact_pct_codex: 100,
            // Default to the generic compact_pct default so Claude inherits the
            // shared threshold unless `context_compact_percent_claude` is set.
            compact_pct_claude: 60,
            compact_lower_bound_tokens:
                crate::services::claude_compact_context::DEFAULT_CONTEXT_COMPACT_LOWER_BOUND_TOKENS,
            context_window: 1_000_000,
        }
    }
}

impl ContextThresholds {
    /// Get compact percent for a specific provider.
    pub fn compact_pct_for(&self, provider: &crate::services::provider::ProviderKind) -> u64 {
        match provider {
            crate::services::provider::ProviderKind::Codex => self.compact_pct_codex,
            crate::services::provider::ProviderKind::Claude => self.compact_pct_claude,
            _ => self.compact_pct,
        }
    }
}

/// Fetch context thresholds from the ADK config API (individual kv_meta keys).
/// Falls back to defaults on any error.
/// Supports provider-specific overrides: `context_compact_percent_codex`, etc.
pub(super) async fn fetch_context_thresholds(_api_port: u16) -> ContextThresholds {
    let defaults = ContextThresholds::default();
    let body = match super::internal_api::get_config_entries().await {
        Ok(body) => body,
        Err(_) => return defaults,
    };
    let entries = body.get("entries").and_then(|v| v.as_array());

    let find_u64 = |key: &str| -> Option<u64> {
        entries
            .and_then(|arr| {
                arr.iter()
                    .find(|e| e.get("key").and_then(|k| k.as_str()) == Some(key))
            })
            .and_then(|e| e.get("value"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<u64>().ok())
    };

    let compact_pct = find_u64("context_compact_percent").unwrap_or(defaults.compact_pct);
    let compact_pct_codex =
        find_u64("context_compact_percent_codex").unwrap_or(defaults.compact_pct_codex);
    // #3097: read the Claude-specific override. Fall back to the *generic*
    // `compact_pct` (not a fixed default) so a user who only sets the generic
    // value still applies it to Claude, while `context_compact_percent_claude`
    // takes precedence when present.
    let compact_pct_claude = find_u64("context_compact_percent_claude").unwrap_or(compact_pct);
    let compact_lower_bound_tokens = find_u64("context_compact_lower_bound_tokens")
        .unwrap_or(defaults.compact_lower_bound_tokens);

    ContextThresholds {
        compact_pct,
        compact_pct_codex,
        compact_pct_claude,
        compact_lower_bound_tokens,
        context_window: defaults.context_window,
    }
}

/// #2849 + #3262: at a watcher-completed turn boundary (pane idle), backfill the
/// exact final context usage onto the status panel AND — for Claude only — inject
/// `/compact` if live usage just crossed the configured
/// `context_compact_percent_claude` threshold.
///
/// Extracted from the tmux_watcher completion path so the trigger rides the same
/// turn-idle signal the panel backfill already uses (exact usage, resolvable
/// window). Degrades safely: no exact usage, a `0` window, or a non-Claude
/// provider simply skips. The compact injection itself is once-per-fill-cycle and
/// pane-ready-gated inside `claude_compact_trigger` / `send_followup_prompt`.
pub(super) async fn backfill_completed_panel_usage_and_maybe_inject_compact(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    state: &crate::services::session_backend::StreamLineState,
    provider: &ProviderKind,
    tmux_session_name: &str,
) {
    // #3262: the usage/window/threshold signal is needed by BOTH the v2 panel
    // backfill AND the Claude `/compact` trigger. Compute it once, unconditionally
    // — gating it behind `status_panel_v2_enabled` (as the original wire-in did)
    // silently disabled auto-compact whenever the v2 panel was turned off. The
    // trigger must run on every turn-completion (pane idle) regardless of the v2
    // feature flag; only the panel `set_context_panel_usage` write stays v2-gated.
    let usage = crate::db::turns::TurnTokenUsage {
        input_tokens: state.accum_input_tokens,
        cache_create_tokens: state.accum_cache_create_tokens,
        cache_read_tokens: state.accum_cache_read_tokens,
        output_tokens: state.accum_output_tokens,
    };
    let occupied = usage.context_occupancy_input_tokens();
    // Claude's real window belongs to the launch-time gateway provenance, not
    // today's live config. Keep the provider fallback for panel display when a
    // legacy/unregistered pane cannot prove launch provenance, but never use it
    // to drive the authoritative compact trigger.
    let claude_launch_window = matches!(provider, ProviderKind::Claude)
        .then(|| {
            crate::services::claude_compact_context::context_window_for_turn(
                tmux_session_name,
                state.last_model.as_deref(),
            )
        })
        .flatten();
    let context_window = claude_launch_window
        .unwrap_or_else(|| provider.resolve_context_window(state.last_model.as_deref()));
    let ctx_cfg = fetch_context_thresholds(shared.api_port).await;
    let compact_pct = ctx_cfg.compact_pct_for(provider);

    // v2-gated panel backfill: the Context line write legitimately needs a real
    // usage signal — skip it when there is no occupancy (`occupied == 0`) or no
    // resolvable window (`context_window == 0`). This guard belongs to the panel
    // write ONLY; it must NOT gate the compact trigger below (#3262 issue #4: a
    // post-compact drop to ~0 usage is exactly the re-arm signal the trigger
    // needs, so a zero-usage turn-completion must still reach the trigger).
    if occupied != 0 && context_window != 0 && shared.ui.status_panel_v2_enabled {
        shared.ui.placeholder_live_events.set_context_panel_usage(
            channel_id,
            state.last_session_id.as_deref(),
            usage.input_tokens,
            usage.cache_create_tokens,
            usage.cache_read_tokens,
            context_window,
            compact_pct,
        );
    }

    // The token trigger runs even when `occupied == 0`: a post-compact usage
    // reset is the observable re-arm signal handled inside the trigger. It is
    // deliberately gated on a proven launch-bound Claude window
    // (`claude_launch_window`), rather than inventing a live-config fallback, so
    // a window this turn cannot prove fails closed to no-inject. Idempotency is
    // keyed on the observable USAGE occupancy (`occupied`), never on a cosmetic
    // `auto_compacted` string heuristic.
    if matches!(provider, ProviderKind::Claude)
        && let Some(turn_identity) =
            super::ManagedCompactTurnIdentity::capture_live(channel_id.get(), tmux_session_name)
    {
        crate::services::claude_compact_trigger::maybe_inject_compact(
            turn_identity,
            provider,
            occupied,
            claude_launch_window,
            compact_pct,
            ctx_cfg.compact_lower_bound_tokens,
        );
    }
}
