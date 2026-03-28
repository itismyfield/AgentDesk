use std::path::Path;
use std::sync::Arc;

use poise::serenity_prelude as serenity;

use super::SharedData;
use crate::config::local_api_url;
use crate::services::provider::ProviderKind;

const SESSION_INFO_MAX_CHARS: usize = 60;

/// Parse `DISPATCH:<uuid> - <title>` format and return the dispatch_id (uuid part).
pub(super) fn parse_dispatch_id(text: &str) -> Option<String> {
    let trimmed = text.trim();
    let rest = trimmed.strip_prefix("DISPATCH:")?;
    // UUID is the part before " - "
    let id = if let Some(idx) = rest.find(" - ") {
        rest[..idx].trim()
    } else {
        rest.trim()
    };
    if id.is_empty() {
        return None;
    }
    Some(id.to_string())
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

    let hostname = crate::services::platform::hostname_short();

    Some(format!("{}:{}", hostname, tmux_name))
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
    api_port: u16,
) {
    let Some(session_key) = session_key else {
        return;
    };

    let mut body = serde_json::json!({
        "session_key": session_key,
        "status": status,
        "provider": provider.as_str(),
        "session_info": session_info,
    });

    if let Some(name) = name.and_then(clean_nonempty) {
        body["name"] = serde_json::json!(name);
    }
    if let Some(model) = model.and_then(clean_nonempty) {
        body["model"] = serde_json::json!(model);
    }
    if let Some(tokens) = tokens {
        body["tokens"] = serde_json::json!(tokens);
    }
    if let Some(cwd) = cwd.and_then(clean_nonempty) {
        body["cwd"] = serde_json::json!(cwd);
    }
    if let Some(did) = dispatch_id.and_then(clean_nonempty) {
        body["dispatch_id"] = serde_json::json!(did);
    }

    match reqwest::Client::new()
        .post(local_api_url(api_port, "/api/hook/session"))
        .json(&body)
        .send()
        .await
    {
        Ok(resp) if !resp.status().is_success() => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!("  [{ts}] ⚠ ADK session POST failed: HTTP {}", resp.status());
        }
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!("  [{ts}] ⚠ ADK session POST error: {e}");
        }
        _ => {}
    }
}

/// Delete a session row from the DB by session_key.
/// Used to clean up thread sessions after dispatch completion.
pub(super) async fn delete_adk_session(session_key: &str, api_port: u16) {
    let url = local_api_url(api_port, "/api/hook/session");
    match reqwest::Client::new()
        .delete(&url)
        .query(&[("session_key", session_key)])
        .send()
        .await
    {
        Ok(resp) if !resp.status().is_success() => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!(
                "  [{ts}] ⚠ ADK session DELETE failed: HTTP {}",
                resp.status()
            );
        }
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!("  [{ts}] ⚠ ADK session DELETE error: {e}");
        }
        _ => {}
    }
}

/// Clear the stored claude_session_id from DB for a given session_key.
/// Called when the user runs /clear so the next turn doesn't resume a dead session.
pub(super) async fn clear_claude_session_id(session_key: &str, api_port: u16) {
    let body = serde_json::json!({ "session_key": session_key });
    match reqwest::Client::new()
        .post(local_api_url(
            api_port,
            "/api/dispatched-sessions/clear-session-id",
        ))
        .json(&body)
        .send()
        .await
    {
        Ok(resp) if !resp.status().is_success() => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!(
                "  [{ts}] ⚠ clear_claude_session_id failed: HTTP {}",
                resp.status()
            );
        }
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!("  [{ts}] ⚠ clear_claude_session_id error: {e}");
        }
        _ => {}
    }
}

/// Save the Claude CLI session_id to DB so it survives dcserver restarts.
/// Called at turn completion with the session_id returned by the Claude CLI.
pub(super) async fn save_claude_session_id(
    session_key: &str,
    claude_session_id: &str,
    api_port: u16,
) {
    let body = serde_json::json!({
        "session_key": session_key,
        "claude_session_id": claude_session_id,
    });
    match reqwest::Client::new()
        .post(local_api_url(api_port, "/api/hook/session"))
        .json(&body)
        .send()
        .await
    {
        Ok(resp) if !resp.status().is_success() => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!(
                "  [{ts}] ⚠ save_claude_session_id failed: HTTP {}",
                resp.status()
            );
        }
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!("  [{ts}] ⚠ save_claude_session_id error: {e}");
        }
        _ => {}
    }
}

/// Fetch the stored claude_session_id from DB for a given session_key.
/// Returns None if no record exists or if the field is NULL.
pub(super) async fn fetch_claude_session_id(session_key: &str, api_port: u16) -> Option<String> {
    let url = local_api_url(api_port, "/api/dispatched-sessions/claude-session-id");
    let resp = reqwest::Client::new()
        .get(&url)
        .query(&[("session_key", session_key)])
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    // #107: Filter empty strings — a stale clear path may have stored ""
    // instead of NULL; treat it as no session ID.
    json.get("claude_session_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
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
mod tests {
    use super::derive_adk_session_info;

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
    fn test_derive_session_info_max_chars() {
        // SESSION_INFO_MAX_CHARS = 60
        // A long user text should be truncated to 60 chars (with ellipsis)
        let long_text = "가나다라마바사아자차카타파하가나다라마바사아자차카타파하가나다라마바사아자차카타파하가나다라마바사아자차카타파하";
        let summary = derive_adk_session_info(Some(long_text), None, None);
        assert!(summary.chars().count() <= 60);
    }

    #[test]
    fn test_build_adk_session_key_format() {
        // build_adk_session_key is async and needs SharedData, so test the format
        // by verifying the components: "hostname:tmux-session"
        // We test the sub-components instead:
        use crate::services::provider::ProviderKind;
        let tmux_name = ProviderKind::Claude.build_tmux_session_name("my-channel");
        let hostname = "mac-mini";
        let key = format!("{}:{}", hostname, tmux_name);
        assert!(key.contains(':'));
        assert!(key.starts_with("mac-mini:AgentDesk-claude-"));
    }
}

/// Context window management thresholds.
/// Single source of truth used by both Rust turn-end compact and JS onContextCheck.
pub(super) struct ContextThresholds {
    pub compact_pct: u64,
    pub context_window: u64,
}

impl Default for ContextThresholds {
    fn default() -> Self {
        Self {
            compact_pct: 60,
            context_window: 1_000_000,
        }
    }
}

/// Fetch context thresholds from the ADK runtime config API.
/// Falls back to defaults on any error.
pub(super) async fn fetch_context_thresholds(api_port: u16) -> ContextThresholds {
    let defaults = ContextThresholds::default();
    let url = local_api_url(api_port, "/api/settings/runtime-config");
    let resp = match reqwest::Client::new().get(&url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return defaults,
    };
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        _ => return defaults,
    };
    let current = body.get("current").unwrap_or(&body);
    ContextThresholds {
        compact_pct: current
            .get("context_compact_percent")
            .and_then(|v| v.as_u64())
            .unwrap_or(defaults.compact_pct),
        context_window: defaults.context_window,
    }
}
