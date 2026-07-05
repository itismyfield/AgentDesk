use std::io::ErrorKind;
use std::path::PathBuf;

/// Stable marker embedded in the error returned by [`read_gemini_oauth_creds`]
/// when `~/.gemini/oauth_creds.json` is simply absent (`ErrorKind::NotFound`),
/// i.e. Gemini is not configured. Callers match on this (via
/// [`is_gemini_unconfigured_error`]) to suppress repeated "not configured"
/// noise, while genuine I/O failures (PermissionDenied / IsADirectory / other)
/// keep their original message and must stay loud.
pub const GEMINI_OAUTH_NOT_FOUND: &str = "gemini oauth creds not found";

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

/// Operator-facing hint for verifying provider auth. Falls back to env/settings
/// guidance for CLIs (like qwen-code 0.15+) that removed their auth subcommand.
pub fn auth_check_hint(auth_check_argv: Option<&[&str]>, binary_name: &str) -> String {
    auth_check_argv
        .map(|argv| argv.join(" "))
        .unwrap_or_else(|| {
            format!(
                "{binary_name}: no auth subcommand in current CLI — configure provider env keys/settings or run the interactive /auth flow"
            )
        })
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
        "opencode" => detect_opencode_file_auth(),
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

pub async fn claude_oauth_token_blocking() -> Option<String> {
    tokio::task::spawn_blocking(claude_oauth_token)
        .await
        .ok()
        .flatten()
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
    let raw = std::fs::read_to_string(&path).map_err(|error| {
        if error.kind() == ErrorKind::NotFound {
            // File absent = Gemini not configured. Tag with the stable marker so
            // callers can suppress repeated noise; keep the path for context.
            anyhow::anyhow!("{GEMINI_OAUTH_NOT_FOUND}: ~/.gemini/oauth_creds.json")
        } else {
            // PermissionDenied / IsADirectory / transient I/O are real problems —
            // do NOT tag them, so they keep WARNing.
            anyhow::anyhow!("cannot read ~/.gemini/oauth_creds.json: {error}")
        }
    })?;
    let creds = serde_json::from_str(&raw)?;
    Ok((path, creds))
}

/// True when `error` means Gemini is simply not configured (no $HOME, or
/// `~/.gemini/oauth_creds.json` does not exist). Permission/IO/parse failures
/// and corrupt-or-partial credentials (`no access_token` / `no refresh_token`)
/// return `false` so they keep WARNing. (#3566)
pub fn is_gemini_unconfigured_error(error: &anyhow::Error) -> bool {
    let msg = error.to_string();
    msg.contains("no home dir") || msg.contains(GEMINI_OAUTH_NOT_FOUND)
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

    if let Some(settings_path) = expanded_auth_path("~/.qwen/settings.json") {
        if let Some(value) = read_json_path(&settings_path) {
            if let Some(source) = qwen_settings_credential_source(&value, spec.env_keys) {
                return Some(format!("file:~/.qwen/settings.json {source}"));
            }
        }
    }

    spec.credential_paths.iter().copied().find_map(|path| {
        if path.ends_with(".env") {
            detect_env_file_source(path, spec.env_keys)
        } else {
            None
        }
    })
}

/// qwen-code stores headless credentials in settings.json: the `env` block is
/// exported into the CLI process environment, and `modelProviders` entries can
/// carry inline `apiKey` values or `envKey` references.
fn qwen_settings_credential_source(value: &serde_json::Value, env_keys: &[&str]) -> Option<String> {
    if let Some(env_block) = value.get("env").and_then(|env| env.as_object()) {
        for (key, entry) in env_block {
            let non_empty = entry.as_str().is_some_and(|raw| !raw.trim().is_empty());
            if !non_empty {
                continue;
            }
            let recognized = env_keys.iter().any(|expected| expected == key)
                || key.ends_with("_API_KEY")
                || key.ends_with("_TOKEN");
            if recognized {
                return Some(format!("env.{key}"));
            }
        }
    }

    let providers = value.get("modelProviders")?.as_object()?;
    for (provider_id, config) in providers {
        let entries: Vec<&serde_json::Value> = match config {
            serde_json::Value::Array(items) => items.iter().collect(),
            other => vec![other],
        };
        for entry in entries {
            if json_token_present(entry, "apiKey") {
                return Some(format!("modelProviders.{provider_id}.apiKey"));
            }
            if let Some(env_key) = entry.get("envKey").and_then(|key| key.as_str()) {
                if env_value_present(env_key) {
                    return Some(format!("modelProviders.{provider_id}.envKey:{env_key}"));
                }
            }
        }
    }
    None
}

fn detect_opencode_file_auth() -> Option<String> {
    let auth_store = xdg_base_dir("XDG_DATA_HOME", ".local/share")
        .map(|base| base.join("opencode").join("auth.json"));
    if let Some(path) = auth_store {
        if let Some(value) = read_json_path(&path) {
            if opencode_auth_store_has_credentials(&value) {
                return Some("file:~/.local/share/opencode/auth.json".to_string());
            }
        }
    }

    let config_path = xdg_base_dir("XDG_CONFIG_HOME", ".config")
        .map(|base| base.join("opencode").join("opencode.json"))?;
    let value = read_json_path(&config_path)?;
    opencode_config_api_key_source(&value)
        .map(|source| format!("file:~/.config/opencode/opencode.json {source}"))
}

/// `opencode auth login` persists credentials as a non-empty JSON object keyed
/// by provider id.
fn opencode_auth_store_has_credentials(value: &serde_json::Value) -> bool {
    value.as_object().is_some_and(|store| !store.is_empty())
}

/// opencode.json providers may embed `options.apiKey` either as a literal or
/// as an `{env:VAR}` template that resolves against the process environment.
fn opencode_config_api_key_source(value: &serde_json::Value) -> Option<String> {
    let providers = value.get("provider")?.as_object()?;
    for (provider_id, config) in providers {
        let Some(api_key) = config
            .get("options")
            .and_then(|options| options.get("apiKey"))
            .and_then(|key| key.as_str())
            .map(str::trim)
            .filter(|key| !key.is_empty())
        else {
            continue;
        };
        if let Some(env_key) = api_key
            .strip_prefix("{env:")
            .and_then(|rest| rest.strip_suffix('}'))
        {
            if env_value_present(env_key.trim()) {
                return Some(format!(
                    "provider.{provider_id}.options.apiKey env:{env_key}"
                ));
            }
            continue;
        }
        return Some(format!("provider.{provider_id}.options.apiKey"));
    }
    None
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

fn xdg_base_dir(env_key: &str, home_fallback: &str) -> Option<PathBuf> {
    if let Ok(base) = std::env::var(env_key) {
        let base = base.trim();
        if !base.is_empty() {
            return Some(PathBuf::from(base));
        }
    }
    dirs::home_dir().map(|home| home.join(home_fallback))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // Mirrors the `read_to_string` error mapping in `read_gemini_oauth_creds`
    // so the NotFound-vs-other classification can be exercised without touching
    // the real `~/.gemini/oauth_creds.json` path. (#3566)
    fn map_read_error(kind: ErrorKind) -> anyhow::Error {
        let error = std::io::Error::from(kind);
        if error.kind() == ErrorKind::NotFound {
            anyhow::anyhow!("{GEMINI_OAUTH_NOT_FOUND}: ~/.gemini/oauth_creds.json")
        } else {
            anyhow::anyhow!("cannot read ~/.gemini/oauth_creds.json: {error}")
        }
    }

    #[test]
    fn gemini_not_found_is_unconfigured() {
        let err = map_read_error(ErrorKind::NotFound);
        assert!(
            is_gemini_unconfigured_error(&err),
            "missing oauth_creds.json must be treated as unconfigured (suppressible): {err}"
        );
    }

    #[test]
    fn gemini_no_home_dir_is_unconfigured() {
        let err = anyhow::anyhow!("no home dir");
        assert!(is_gemini_unconfigured_error(&err));
    }

    #[test]
    fn gemini_permission_denied_is_not_unconfigured() {
        let err = map_read_error(ErrorKind::PermissionDenied);
        assert!(
            !is_gemini_unconfigured_error(&err),
            "PermissionDenied is a real problem and must keep WARNing: {err}"
        );
    }

    #[test]
    fn gemini_is_a_directory_is_not_unconfigured() {
        // `IsADirectory` is unstable to name directly; reuse a generic non-NotFound
        // kind to prove anything other than NotFound stays loud.
        let err = map_read_error(ErrorKind::Other);
        assert!(!is_gemini_unconfigured_error(&err));
    }

    #[test]
    fn gemini_corrupt_creds_are_not_unconfigured() {
        // Partial creds surface as separate errors and must keep WARNing.
        let err = anyhow::anyhow!("no access_token in oauth_creds.json");
        assert!(!is_gemini_unconfigured_error(&err));
        let err = anyhow::anyhow!("no refresh_token in oauth_creds.json");
        assert!(!is_gemini_unconfigured_error(&err));
    }

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
    fn opencode_auth_store_requires_non_empty_object() {
        assert!(opencode_auth_store_has_credentials(&serde_json::json!({
            "anthropic": { "type": "oauth" }
        })));
        assert!(!opencode_auth_store_has_credentials(&serde_json::json!({})));
        assert!(!opencode_auth_store_has_credentials(&serde_json::json!([])));
    }

    #[test]
    fn opencode_config_api_key_source_accepts_literal_and_resolved_env_template() {
        let _guard = env_guard();
        let literal = serde_json::json!({
            "provider": {
                "nvidia-kimi": { "options": { "apiKey": "secret", "baseURL": "https://x" } }
            }
        });
        assert_eq!(
            opencode_config_api_key_source(&literal).as_deref(),
            Some("provider.nvidia-kimi.options.apiKey")
        );

        let templated = serde_json::json!({
            "provider": {
                "custom": { "options": { "apiKey": "{env:AGENTDESK_OPENCODE_TEST_KEY}" } }
            }
        });
        unsafe {
            std::env::remove_var("AGENTDESK_OPENCODE_TEST_KEY");
        }
        assert_eq!(opencode_config_api_key_source(&templated), None);
        unsafe {
            std::env::set_var("AGENTDESK_OPENCODE_TEST_KEY", "token");
        }
        assert_eq!(
            opencode_config_api_key_source(&templated).as_deref(),
            Some("provider.custom.options.apiKey env:AGENTDESK_OPENCODE_TEST_KEY")
        );
        unsafe {
            std::env::remove_var("AGENTDESK_OPENCODE_TEST_KEY");
        }

        let empty = serde_json::json!({
            "provider": { "custom": { "options": { "apiKey": "  " } } }
        });
        assert_eq!(opencode_config_api_key_source(&empty), None);
    }

    #[test]
    fn qwen_settings_credential_source_reads_env_block_and_model_providers() {
        let _guard = env_guard();
        let env_block = serde_json::json!({
            "env": { "NVIDIA_API_KEY": "secret", "EMPTY_API_KEY": "" },
            "modelProviders": { "openai": [{ "baseUrl": "https://x" }] }
        });
        assert_eq!(
            qwen_settings_credential_source(&env_block, &["DASHSCOPE_API_KEY"]).as_deref(),
            Some("env.NVIDIA_API_KEY")
        );

        let provider_api_key = serde_json::json!({
            "modelProviders": { "openai": [{ "apiKey": "secret" }] }
        });
        assert_eq!(
            qwen_settings_credential_source(&provider_api_key, &[]).as_deref(),
            Some("modelProviders.openai.apiKey")
        );

        let provider_env_key = serde_json::json!({
            "modelProviders": { "openai": { "envKey": "AGENTDESK_QWEN_TEST_KEY" } }
        });
        unsafe {
            std::env::remove_var("AGENTDESK_QWEN_TEST_KEY");
        }
        assert_eq!(
            qwen_settings_credential_source(&provider_env_key, &[]),
            None
        );
        unsafe {
            std::env::set_var("AGENTDESK_QWEN_TEST_KEY", "token");
        }
        assert_eq!(
            qwen_settings_credential_source(&provider_env_key, &[]).as_deref(),
            Some("modelProviders.openai.envKey:AGENTDESK_QWEN_TEST_KEY")
        );
        unsafe {
            std::env::remove_var("AGENTDESK_QWEN_TEST_KEY");
        }

        let unrelated_env = serde_json::json!({
            "env": { "SOME_FLAG": "1" }
        });
        assert_eq!(qwen_settings_credential_source(&unrelated_env, &[]), None);
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
