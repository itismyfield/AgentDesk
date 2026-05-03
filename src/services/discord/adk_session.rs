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
            let id = if let Some(idx) = rest.find(" - ") {
                rest[..idx].trim()
            } else {
                rest.trim()
            };
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
    let tmux_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|s| s.channel_name.as_ref())
            .map(|name| provider.build_tmux_session_name(name))
    }?;

    Some(build_namespaced_session_key(
        &shared.token_hash,
        provider,
        &tmux_name,
    ))
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
    agent_id: Option<&str>,
    _api_port: u16,
) {
    let Some(session_key) = session_key else {
        return;
    };
    let status = crate::db::session_status::normalize_incoming_session_status(Some(status));

    let body = crate::server::routes::dispatched_sessions::HookSessionBody {
        session_key: session_key.to_string(),
        agent_id: agent_id.map(str::to_string),
        status: Some(status.to_string()),
        provider: Some(provider.as_str().to_string()),
        session_info: session_info.map(str::to_string),
        name: name.and_then(clean_nonempty).map(str::to_string),
        model: model.and_then(clean_nonempty).map(str::to_string),
        tokens,
        cwd: cwd.and_then(clean_nonempty).map(str::to_string),
        dispatch_id: dispatch_id.and_then(clean_nonempty).map(str::to_string),
        thread_channel_id: thread_channel_id.map(|id| id.to_string()),
        claude_session_id: None,
        session_id: None,
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
    _api_port: u16,
) {
    let body = crate::server::routes::dispatched_sessions::HookSessionBody {
        session_key: session_key.to_string(),
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
    };
    if let Err(err) = super::internal_api::hook_session(body).await {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!("  [{ts}] ⚠ save_provider_session_id failed: {err}");
    }
}

pub(super) fn context_usage_percent(tokens: u64, context_window: u64) -> u64 {
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
/// Prefer the legacy `claude_session_id` field and fall back to `session_id`
/// only for older rows that never populated the dedicated selector slot.
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
    let selector = json
        .get("claude_session_id")
        .and_then(|v| v.as_str())
        .or_else(|| json.get("session_id").and_then(|v| v.as_str()))
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
        tracing::info!(
            "  [{ts}] [session-restore] provider session lookup returned no usable selector: key={} provider={} has_claude_selector={} has_session_id={}",
            session_key,
            provider.as_str(),
            has_claude_selector,
            has_raw_session_id
        );
    }
    selector
}

fn build_provider_session_payload(
    session_key: &str,
    session_id: &str,
    raw_provider_session_id: Option<&str>,
    provider: &ProviderKind,
) -> serde_json::Value {
    serde_json::json!({
        "session_key": session_key,
        "session_id": raw_provider_session_id,
        "claude_session_id": session_id,
        "provider": provider.as_str(),
    })
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

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::{
        CONTEXT_COMPACTION_PRESERVED_SECTIONS, context_compaction_details, context_usage_percent,
        derive_adk_session_info, parse_thread_channel_id_from_name,
    };

    #[test]
    fn derive_uses_user_text_when_human_readable() {
        let summary = derive_adk_session_info(
            Some("회의록 일감 전체 폐기 기능 구현해줘"),
            Some("adk-cdx"),
            Some("/repo"),
        );
        assert_eq!(summary, "회의록 일감 전체 폐기 기능 구현해줘");
    }

    #[test]
    fn derive_skips_raw_commands_and_falls_back() {
        let summary = derive_adk_session_info(
            Some("cargo test --no-run"),
            Some("adk-cdx"),
            Some("/Users/me/AgentDesk"),
        );
        assert_eq!(summary, "AgentDesk 작업 진행 중");
    }

    #[test]
    fn derive_maps_short_generic_request_to_actionable_fallback() {
        let summary =
            derive_adk_session_info(Some("맞춰줘"), Some("adk-cdx"), Some("/Users/me/AgentDesk"));
        assert_eq!(summary, "AgentDesk 개선 작업 진행 중");
    }

    #[test]
    fn derive_maps_short_deploy_request_to_deploy_fallback() {
        let summary =
            derive_adk_session_info(Some("배포해"), Some("adk-cdx"), Some("/Users/me/AgentDesk"));
        assert_eq!(summary, "AgentDesk 배포 작업 진행 중");
    }

    // ── P0 tests ─────────────────────────────────────────────────────────

    #[test]
    fn test_parse_dispatch_id_valid() {
        use super::parse_dispatch_id;
        let result =
            parse_dispatch_id("DISPATCH:550e8400-e29b-41d4-a716-446655440000 - Fix login bug");
        assert_eq!(
            result,
            Some("550e8400-e29b-41d4-a716-446655440000".to_string())
        );
    }

    #[test]
    fn test_parse_dispatch_id_no_title() {
        use super::parse_dispatch_id;
        let result = parse_dispatch_id("DISPATCH:550e8400-e29b-41d4-a716-446655440000");
        assert_eq!(
            result,
            Some("550e8400-e29b-41d4-a716-446655440000".to_string())
        );
    }

    #[test]
    fn test_parse_dispatch_id_invalid() {
        use super::parse_dispatch_id;
        assert_eq!(parse_dispatch_id("random text with no dispatch"), None);
        assert_eq!(parse_dispatch_id("DISPATCH_WRONG:abc"), None);
    }

    #[test]
    fn test_parse_dispatch_id_empty() {
        use super::parse_dispatch_id;
        assert_eq!(parse_dispatch_id(""), None);
        assert_eq!(parse_dispatch_id("DISPATCH:"), None);
        assert_eq!(parse_dispatch_id("DISPATCH:  "), None);
    }

    #[test]
    fn test_parse_thread_channel_id_from_name_valid() {
        assert_eq!(
            parse_thread_channel_id_from_name("adk-cdx-t1485506232256168011"),
            Some(1485506232256168011)
        );
    }

    #[test]
    fn test_parse_thread_channel_id_from_name_invalid() {
        assert_eq!(parse_thread_channel_id_from_name("adk-cdx"), None);
        assert_eq!(parse_thread_channel_id_from_name("adk-cdx-t123"), None);
    }

    #[test]
    fn test_derive_session_info_max_chars() {
        // SESSION_INFO_MAX_CHARS = 60
        // A long user text should be truncated to 60 chars (with ellipsis)
        let long_text = "가나다라마바사아자차카타파하가나다라마바사아자차카타파하가나다라마바사아자차카타파하가나다라마바사아자차카타파하";
        let summary = derive_adk_session_info(Some(long_text), None, None);
        assert!(summary.chars().count() <= 60);
    }

    #[test]
    fn test_build_adk_session_key_format() {
        use crate::services::provider::ProviderKind;
        let tmux_name = ProviderKind::Claude.build_tmux_session_name("my-channel");
        let key = super::build_namespaced_session_key("hash123", &ProviderKind::Claude, &tmux_name);
        assert!(key.starts_with("claude/hash123/"));
        assert!(key.contains(':'));
        assert!(key.ends_with(&tmux_name));
    }

    #[test]
    fn test_build_session_key_candidates_include_legacy_tail() {
        use crate::services::provider::ProviderKind;
        let tmux_name = ProviderKind::Codex.build_tmux_session_name("agentdesk-main");
        let candidates =
            super::build_session_key_candidates("tokenxyz", &ProviderKind::Codex, &tmux_name);
        assert!(candidates[0].starts_with("codex/tokenxyz/"));
        assert_eq!(candidates[1], super::build_legacy_session_key(&tmux_name));
    }

    #[test]
    fn test_legacy_session_key_from_namespaced_round_trip() {
        let key = "codex/tokenxyz/host123:AgentDesk-codex-main";
        assert_eq!(
            super::legacy_session_key_from_namespaced(key),
            Some("host123:AgentDesk-codex-main".to_string())
        );
        assert_eq!(
            super::legacy_session_key_from_namespaced("host123:legacy"),
            None
        );
    }

    #[test]
    fn test_build_provider_session_payload_includes_provider() {
        use crate::services::provider::ProviderKind;

        let payload = super::build_provider_session_payload(
            "host:AgentDesk-codex-adk-cdx",
            "session-123",
            Some("raw-session-123"),
            &ProviderKind::Codex,
        );

        assert_eq!(payload["session_key"], "host:AgentDesk-codex-adk-cdx");
        assert_eq!(payload["session_id"], "raw-session-123");
        assert_eq!(payload["claude_session_id"], "session-123");
        assert_eq!(payload["provider"], "codex");
    }

    #[test]
    fn test_context_usage_percent_uses_context_window() {
        assert_eq!(context_usage_percent(850, 1_000), 85);
        assert_eq!(context_usage_percent(1_780, 1_000), 100);
        assert_eq!(context_usage_percent(1, 0), 0);
    }

    #[test]
    fn test_context_compaction_details_include_preserved_sections() {
        let details = context_compaction_details(91, Some(37));

        assert_eq!(details.before_pct, 91);
        assert_eq!(details.after_pct, Some(37));
        assert_eq!(
            details.preserved_sections,
            CONTEXT_COMPACTION_PRESERVED_SECTIONS
                .iter()
                .map(|section| (*section).to_string())
                .collect::<Vec<_>>()
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

/// Context window management thresholds.
/// Single source of truth used by Rust turn-end compact logic.
/// Provider-specific overrides: `context_compact_percent_codex`, `context_compact_percent_claude`, etc.
pub(super) struct ContextThresholds {
    pub compact_pct: u64,
    /// Provider-specific override (if set). Falls back to compact_pct.
    pub compact_pct_codex: u64,
    pub context_window: u64,
}

impl Default for ContextThresholds {
    fn default() -> Self {
        Self {
            compact_pct: 60,
            compact_pct_codex: 100,
            context_window: 1_000_000,
        }
    }
}

impl ContextThresholds {
    /// Get compact percent for a specific provider.
    pub fn compact_pct_for(&self, provider: &crate::services::provider::ProviderKind) -> u64 {
        match provider {
            crate::services::provider::ProviderKind::Codex => self.compact_pct_codex,
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

    ContextThresholds {
        compact_pct,
        compact_pct_codex,
        context_window: defaults.context_window,
    }
}
