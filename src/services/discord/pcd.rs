use std::path::Path;
use std::sync::Arc;

use poise::serenity_prelude as serenity;

use super::SharedData;
use crate::services::provider::ProviderKind;

const PCD_SESSION_INFO_MAX_CHARS: usize = 60;

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

pub(super) async fn build_pcd_session_key(
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

    let hostname = std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());

    Some(format!("{}:{}", hostname, tmux_name))
}

pub(super) fn derive_pcd_session_info(
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
        return format!("RemoteCC {} 작업 진행 중", action);
    }

    if let Some(channel) = channel_name.and_then(clean_nonempty) {
        return format!("{} 작업 진행 중", trim_channel_suffix(channel));
    }

    if let Some(label) = current_path.and_then(path_label) {
        return format!("{} 작업 진행 중", label);
    }

    "RemoteCC 작업 진행 중".to_string()
}

pub(super) async fn post_pcd_session_status(
    session_key: Option<&str>,
    name: Option<&str>,
    model: Option<&str>,
    status: &str,
    provider: &ProviderKind,
    session_info: Option<&str>,
    tokens: Option<u64>,
    cwd: Option<&str>,
    dispatch_id: Option<&str>,
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
        .post("http://127.0.0.1:8791/api/hook/session")
        .json(&body)
        .send()
        .await
    {
        Ok(resp) if !resp.status().is_success() => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!("  [{ts}] ⚠ PCD session POST failed: HTTP {}", resp.status());
        }
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!("  [{ts}] ⚠ PCD session POST error: {e}");
        }
        _ => {}
    }
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

    Some(truncate_chars(&collapsed, PCD_SESSION_INFO_MAX_CHARS))
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
        "remotecc ",
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
    use super::derive_pcd_session_info;

    #[test]
    fn derive_uses_user_text_when_human_readable() {
        let summary = derive_pcd_session_info(
            Some("회의록 일감 전체 폐기 기능 구현해줘"),
            Some("remotecc-cdx"),
            Some("/repo"),
        );
        assert_eq!(summary, "회의록 일감 전체 폐기 기능 구현해줘");
    }

    #[test]
    fn derive_skips_raw_commands_and_falls_back() {
        let summary = derive_pcd_session_info(
            Some("cargo test --no-run"),
            Some("remotecc-cdx"),
            Some("/Users/me/remotecc"),
        );
        assert_eq!(summary, "remotecc 작업 진행 중");
    }

    #[test]
    fn derive_maps_short_generic_request_to_actionable_fallback() {
        let summary = derive_pcd_session_info(
            Some("맞춰줘"),
            Some("remotecc-cdx"),
            Some("/Users/me/remotecc"),
        );
        assert_eq!(summary, "remotecc 개선 작업 진행 중");
    }

    #[test]
    fn derive_maps_short_deploy_request_to_deploy_fallback() {
        let summary = derive_pcd_session_info(
            Some("배포해"),
            Some("remotecc-cdx"),
            Some("/Users/me/remotecc"),
        );
        assert_eq!(summary, "remotecc 배포 작업 진행 중");
    }
}
