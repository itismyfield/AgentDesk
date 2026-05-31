use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderAuthSpec {
    pub credential_paths: &'static [&'static str],
    pub env_keys: &'static [&'static str],
    pub auth_check_argv: Option<&'static [&'static str]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderCredentialStatus {
    pub credential_present: bool,
    pub source: Option<String>,
}

impl ProviderCredentialStatus {
    fn present(source: impl Into<String>) -> Self {
        Self {
            credential_present: true,
            source: Some(source.into()),
        }
    }

    fn missing() -> Self {
        Self {
            credential_present: false,
            source: None,
        }
    }
}

pub fn detect_provider_credentials(
    provider_id: &str,
    spec: &ProviderAuthSpec,
) -> ProviderCredentialStatus {
    let provider = provider_id.trim().to_ascii_lowercase();
    let file_source = detect_provider_file_auth(&provider, spec);
    if let Some(source) = file_source {
        return ProviderCredentialStatus::present(source);
    }

    if let Some(env_key) = spec
        .env_keys
        .iter()
        .copied()
        .find(|key| env_value_present(key))
    {
        return ProviderCredentialStatus::present(format!("env:{env_key}"));
    }

    ProviderCredentialStatus::missing()
}

fn detect_provider_file_auth(provider: &str, spec: &ProviderAuthSpec) -> Option<String> {
    match provider {
        "claude" => detect_claude_oauth_source(),
        "codex" => detect_codex_access_token_source(),
        "gemini" => detect_gemini_oauth_source(),
        "qwen" => detect_qwen_file_auth(spec),
        _ => None,
    }
}

pub fn claude_oauth_token() -> Option<String> {
    read_claude_keychain_token().or_else(|| {
        let path = expanded_auth_path("~/.claude/.credentials.json")?;
        read_json_path(&path)
            .and_then(|creds| {
                creds
                    .get("claudeAiOauth")
                    .and_then(|oauth| oauth.get("accessToken"))
                    .and_then(|value| value.as_str())
                    .map(str::to_string)
            })
            .filter(|token| !token.trim().is_empty())
    })
}

pub fn codex_access_token() -> Option<String> {
    let path = expanded_auth_path("~/.codex/auth.json")?;
    read_json_path(&path)
        .and_then(|auth| {
            auth.get("tokens")
                .and_then(|tokens| tokens.get("access_token"))
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .filter(|token| !token.trim().is_empty())
}

pub fn read_gemini_oauth_creds() -> Result<(PathBuf, serde_json::Value), anyhow::Error> {
    let path = expanded_auth_path("~/.gemini/oauth_creds.json")
        .ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let raw = std::fs::read_to_string(&path)
        .map_err(|error| anyhow::anyhow!("cannot read ~/.gemini/oauth_creds.json: {error}"))?;
    let creds = serde_json::from_str(&raw)?;
    Ok((path, creds))
}

fn detect_claude_oauth_source() -> Option<String> {
    if read_claude_keychain_token().is_some() {
        return Some("keychain:Claude Code-credentials".to_string());
    }
    detect_json_token_source(
        "~/.claude/.credentials.json",
        &["claudeAiOauth.accessToken"],
    )
}

fn detect_codex_access_token_source() -> Option<String> {
    codex_access_token().map(|_| "file:~/.codex/auth.json".to_string())
}

fn detect_gemini_oauth_source() -> Option<String> {
    let (_, creds) = read_gemini_oauth_creds().ok()?;
    if json_token_present(&creds, "access_token") || json_token_present(&creds, "refresh_token") {
        Some("file:~/.gemini/oauth_creds.json".to_string())
    } else {
        None
    }
}

fn detect_qwen_file_auth(spec: &ProviderAuthSpec) -> Option<String> {
    if let Some(source) = detect_json_token_source(
        "~/.qwen/oauth_creds.json",
        &["access_token", "refresh_token"],
    ) {
        return Some(source);
    }

    spec.credential_paths.iter().copied().find_map(|path| {
        if path.ends_with(".env") {
            detect_env_file_source(path, spec.env_keys)
        } else {
            None
        }
    })
}

fn detect_json_token_source(path: &str, keys: &[&str]) -> Option<String> {
    let expanded = expanded_auth_path(path)?;
    let value = read_json_path(&expanded)?;
    if keys.iter().any(|key| json_token_present(&value, key)) {
        Some(format!("file:{path}"))
    } else {
        None
    }
}

fn detect_env_file_source(path: &str, keys: &[&str]) -> Option<String> {
    let expanded = expanded_auth_path(path)?;
    let raw = std::fs::read_to_string(&expanded).ok()?;
    if env_file_contains_key(&raw, keys) {
        Some(format!("file:{path}"))
    } else {
        None
    }
}

fn env_file_contains_key(raw: &str, keys: &[&str]) -> bool {
    raw.lines()
        .filter_map(parse_env_assignment_key)
        .any(|key| keys.iter().any(|expected| expected == &key))
}

fn parse_env_assignment_key(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
    let (key, value) = line.split_once('=')?;
    if value.trim().is_empty() {
        return None;
    }
    let key = key.trim();
    if key.is_empty()
        || !key
            .chars()
            .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
        || key.chars().next().is_some_and(|ch| ch.is_ascii_digit())
    {
        return None;
    }
    Some(key)
}

fn read_claude_keychain_token() -> Option<String> {
    let output = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    let raw = raw.trim();
    let creds: serde_json::Value = serde_json::from_str(raw).ok()?;
    creds
        .get("claudeAiOauth")
        .and_then(|oauth| oauth.get("accessToken"))
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .filter(|token| !token.trim().is_empty())
}

fn read_json_path(path: &PathBuf) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn json_token_present(value: &serde_json::Value, dotted_key: &str) -> bool {
    let mut current = value;
    for segment in dotted_key.split('.') {
        current = match current.get(segment) {
            Some(next) => next,
            None => return false,
        };
    }
    current
        .as_str()
        .is_some_and(|token| !token.trim().is_empty())
}

fn env_value_present(key: &str) -> bool {
    std::env::var(key)
        .ok()
        .is_some_and(|value| !value.trim().is_empty())
}

fn expanded_auth_path(raw: &str) -> Option<PathBuf> {
    if let Some(rest) = raw.strip_prefix("~/") {
        return dirs::home_dir().map(|home| home.join(rest));
    }
    Some(PathBuf::from(raw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn env_guard() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap()
    }

    #[test]
    fn json_token_present_supports_nested_keys() {
        let value = serde_json::json!({
            "tokens": { "access_token": "abc" },
            "empty": ""
        });
        assert!(json_token_present(&value, "tokens.access_token"));
        assert!(!json_token_present(&value, "empty"));
        assert!(!json_token_present(&value, "tokens.missing"));
    }

    #[test]
    fn env_file_contains_key_requires_non_empty_supported_key() {
        let raw = r#"
            # DASHSCOPE_API_KEY=commented
            export QWEN_API_KEY=
            OTHER_KEY=value
            DASHSCOPE_API_KEY=secret
        "#;
        assert!(env_file_contains_key(
            raw,
            &["DASHSCOPE_API_KEY", "QWEN_API_KEY"]
        ));
        assert!(!env_file_contains_key(raw, &["OPENAI_API_KEY"]));
    }

    #[test]
    fn parse_env_assignment_key_rejects_comments_empty_values_and_bad_names() {
        assert_eq!(
            parse_env_assignment_key("export QWEN_API_KEY=abc"),
            Some("QWEN_API_KEY")
        );
        assert_eq!(
            parse_env_assignment_key("QWEN_API_KEY=\"abc\""),
            Some("QWEN_API_KEY")
        );
        assert_eq!(parse_env_assignment_key("# QWEN_API_KEY=abc"), None);
        assert_eq!(parse_env_assignment_key("QWEN_API_KEY="), None);
        assert_eq!(parse_env_assignment_key("1BAD=abc"), None);
    }

    #[test]
    fn detect_provider_credentials_reports_env_presence_without_auth_claim() {
        let _guard = env_guard();
        let original = std::env::var_os("AGENTDESK_PROVIDER_AUTH_TEST_KEY");
        unsafe {
            std::env::set_var("AGENTDESK_PROVIDER_AUTH_TEST_KEY", "token");
        }
        let spec = ProviderAuthSpec {
            credential_paths: &[],
            env_keys: &["AGENTDESK_PROVIDER_AUTH_TEST_KEY"],
            auth_check_argv: None,
        };

        let status = detect_provider_credentials("test-provider", &spec);

        assert!(status.credential_present);
        assert_eq!(
            status.source.as_deref(),
            Some("env:AGENTDESK_PROVIDER_AUTH_TEST_KEY")
        );
        unsafe {
            match original {
                Some(value) => std::env::set_var("AGENTDESK_PROVIDER_AUTH_TEST_KEY", value),
                None => std::env::remove_var("AGENTDESK_PROVIDER_AUTH_TEST_KEY"),
            }
        }
    }
}
