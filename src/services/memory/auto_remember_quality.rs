use std::time::Duration;

use regex::Regex;
use reqwest::Url;
use serde::Deserialize;
use serde_json::json;

use crate::runtime_layout;
use crate::services::discord::settings::ResolvedAutoRememberImproverSettings;
use crate::services::provider::ProviderKind;

#[cfg(test)]
use std::sync::{LazyLock, Mutex};

const AUTO_REMEMBER_IMPROVER_BACKEND_ENV: &str = "AGENTDESK_AUTO_REMEMBER_IMPROVER_BACKEND";
const AUTO_REMEMBER_AGENT_PROVIDER_ENV: &str = "AGENTDESK_AUTO_REMEMBER_AGENT_PROVIDER";
const AUTO_REMEMBER_AGENT_MODEL_ENV: &str = "AGENTDESK_AUTO_REMEMBER_AGENT_MODEL";
const AUTO_REMEMBER_AGENT_LABEL_ENV: &str = "AGENTDESK_AUTO_REMEMBER_AGENT_LABEL";
const AUTO_REMEMBER_LOCAL_REWRITE_SCHEMA_VERSION: &str = "1";
const AUTO_REMEMBER_LOCAL_REWRITE_BASE_URL_ENV: &str = "AGENTDESK_AUTO_REMEMBER_LOCAL_BASE_URL";
const AUTO_REMEMBER_LOCAL_REWRITE_MODEL_ENV: &str = "AGENTDESK_AUTO_REMEMBER_LOCAL_MODEL";
const DEFAULT_AUTO_REMEMBER_LOCAL_REWRITE_BASE_URL: &str =
    "http://127.0.0.1:1234/v1/chat/completions";
const AUTO_REMEMBER_LOCAL_REWRITE_TIMEOUT_MS: u64 = 20_000;
const AUTO_REMEMBER_AGENT_REWRITE_TIMEOUT_MS: u64 = 45_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AutoRememberQualityInput {
    pub(crate) signal_kind: String,
    pub(crate) raw_content: String,
    pub(crate) supporting_evidence: Vec<String>,
    pub(crate) entity_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AutoRememberRewriteOutput {
    pub(crate) content: String,
    pub(crate) keyword_suggestions: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AutoRememberImproverBackend {
    None,
    LocalLlm,
    Agent,
    Both,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct LocalRewriteRuntimeConfig {
    base_url: String,
    model: String,
    timeout: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AgentRewriteRuntimeConfig {
    provider: ProviderKind,
    model: Option<String>,
    label: String,
    timeout: Duration,
}

#[derive(Debug, Deserialize)]
struct LocalRewriteChatResponse {
    choices: Vec<LocalRewriteChoice>,
}

#[derive(Debug, Deserialize)]
struct LocalRewriteChoice {
    message: LocalRewriteMessage,
}

#[derive(Debug, Deserialize)]
struct LocalRewriteMessage {
    content: String,
}

#[derive(Debug, Deserialize)]
struct LocalRewritePayload {
    schema_version: String,
    content: String,
    #[serde(default)]
    keywords: Vec<String>,
}

#[cfg(test)]
type TestAgentImprover = Box<
    dyn Fn(ProviderKind, Option<String>, &str, Duration, &str) -> Result<String, String>
        + Send
        + Sync,
>;

#[cfg(test)]
static TEST_AGENT_IMPROVER: LazyLock<Mutex<Option<TestAgentImprover>>> =
    LazyLock::new(|| Mutex::new(None));

pub(crate) async fn improve_candidate(
    candidate: &AutoRememberQualityInput,
) -> Result<AutoRememberRewriteOutput, String> {
    improve_candidate_with_config(candidate, None).await
}

pub(crate) async fn improve_candidate_with_config(
    candidate: &AutoRememberQualityInput,
    override_cfg: Option<&ResolvedAutoRememberImproverSettings>,
) -> Result<AutoRememberRewriteOutput, String> {
    match configured_backend(override_cfg) {
        AutoRememberImproverBackend::None => Err("auto-remember improver disabled".to_string()),
        AutoRememberImproverBackend::LocalLlm => rewrite_with_local_llm(candidate).await,
        AutoRememberImproverBackend::Agent => rewrite_with_agent(candidate, override_cfg).await,
        AutoRememberImproverBackend::Both => {
            let local_error = match rewrite_with_local_llm(candidate).await {
                Ok(output) => return Ok(output),
                Err(error) => error,
            };
            rewrite_with_agent(candidate, override_cfg)
                .await
                .map_err(|agent_error| {
                    format!(
                        "auto-remember local rewrite failed: {local_error}; agent rewrite failed: {agent_error}"
                    )
                })
        }
    }
}

pub(crate) fn rewrite_supported() -> bool {
    rewrite_supported_with_config(None)
}

pub(crate) fn rewrite_supported_with_config(
    override_cfg: Option<&ResolvedAutoRememberImproverSettings>,
) -> bool {
    configured_backend(override_cfg) != AutoRememberImproverBackend::None
}

#[cfg(test)]
pub(crate) fn set_test_agent_improver(override_fn: Option<TestAgentImprover>) {
    *TEST_AGENT_IMPROVER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = override_fn;
}

#[cfg(test)]
pub(crate) fn local_rewrite_runtime_config_for_tests() -> Result<(String, String, Duration), String>
{
    let config = local_rewrite_runtime_config()?;
    Ok((config.base_url, config.model, config.timeout))
}

#[cfg(test)]
pub(crate) fn agent_rewrite_runtime_config_for_tests()
-> Result<(ProviderKind, Option<String>, String, Duration), String> {
    let config = agent_rewrite_runtime_config()?;
    Ok((config.provider, config.model, config.label, config.timeout))
}

#[cfg(test)]
pub(crate) fn configured_backend_for_tests() -> AutoRememberImproverBackend {
    configured_backend(None)
}

fn configured_backend(
    override_cfg: Option<&ResolvedAutoRememberImproverSettings>,
) -> AutoRememberImproverBackend {
    parse_improver_mode(
        std::env::var(AUTO_REMEMBER_IMPROVER_BACKEND_ENV).ok(),
        override_cfg
            .map(|cfg| cfg.mode.clone())
            .or_else(configured_runtime_improver_mode),
    )
}

fn local_rewrite_runtime_config() -> Result<LocalRewriteRuntimeConfig, String> {
    let base_url = std::env::var(AUTO_REMEMBER_LOCAL_REWRITE_BASE_URL_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_AUTO_REMEMBER_LOCAL_REWRITE_BASE_URL.to_string());
    let parsed_url = Url::parse(&base_url)
        .map_err(|error| format!("auto-remember local rewrite base URL is invalid: {error}"))?;
    if !is_loopback_url(&parsed_url) {
        return Err("auto-remember local rewrite base URL must stay on loopback".to_string());
    }

    let model = std::env::var(AUTO_REMEMBER_LOCAL_REWRITE_MODEL_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| std::env::var("LOCAL_LLM_MODEL").ok())
        .or_else(|| std::env::var("LMSTUDIO_MODEL").ok())
        .map(|value| normalize_whitespace(&value))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            format!(
                "auto-remember local rewrite model is not configured; set {}",
                AUTO_REMEMBER_LOCAL_REWRITE_MODEL_ENV
            )
        })?;

    Ok(LocalRewriteRuntimeConfig {
        base_url,
        model,
        timeout: Duration::from_millis(AUTO_REMEMBER_LOCAL_REWRITE_TIMEOUT_MS),
    })
}

fn agent_rewrite_runtime_config() -> Result<AgentRewriteRuntimeConfig, String> {
    agent_rewrite_runtime_config_with_override(None)
}

fn agent_rewrite_runtime_config_with_override(
    override_cfg: Option<&ResolvedAutoRememberImproverSettings>,
) -> Result<AgentRewriteRuntimeConfig, String> {
    let provider = parse_provider_override(
        std::env::var(AUTO_REMEMBER_AGENT_PROVIDER_ENV).ok(),
        override_cfg
            .and_then(|cfg| cfg.agent.provider.clone())
            .or_else(configured_runtime_agent_provider),
    )?;
    let model = std::env::var(AUTO_REMEMBER_AGENT_MODEL_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| override_cfg.and_then(|cfg| cfg.agent.model.clone()))
        .or_else(configured_runtime_agent_model)
        .map(|value| normalize_whitespace(&value))
        .filter(|value| !value.is_empty());
    let label = std::env::var(AUTO_REMEMBER_AGENT_LABEL_ENV)
        .ok()
        .map(|value| normalize_whitespace(&value))
        .filter(|value| !value.is_empty())
        .or_else(|| override_cfg.and_then(|cfg| cfg.agent.label.clone()))
        .or_else(configured_runtime_agent_label)
        .unwrap_or_else(|| provider.as_str().to_string());

    Ok(AgentRewriteRuntimeConfig {
        provider,
        model,
        label,
        timeout: Duration::from_millis(AUTO_REMEMBER_AGENT_REWRITE_TIMEOUT_MS),
    })
}

fn parse_improver_mode(
    env_value: Option<String>,
    config_value: Option<String>,
) -> AutoRememberImproverBackend {
    match env_value
        .or(config_value)
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("none") | Some("off") | Some("disabled") => AutoRememberImproverBackend::None,
        Some("agent") => AutoRememberImproverBackend::Agent,
        Some("both") | Some("cascade") => AutoRememberImproverBackend::Both,
        Some("local_llm") | Some("local") | None | Some("") => {
            AutoRememberImproverBackend::LocalLlm
        }
        Some(other) => {
            eprintln!(
                "  [memory] Warning: unknown auto-remember improver mode '{other}', falling back to local_llm"
            );
            AutoRememberImproverBackend::LocalLlm
        }
    }
}

fn parse_provider_override(
    env_value: Option<String>,
    config_value: Option<String>,
) -> Result<ProviderKind, String> {
    match env_value
        .or(config_value)
        .map(|value| value.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("claude") => Ok(ProviderKind::Claude),
        Some("gemini") => Ok(ProviderKind::Gemini),
        Some("qwen") => Ok(ProviderKind::Qwen),
        Some("codex") | None | Some("") => Ok(ProviderKind::Codex),
        Some(other) => Err(format!(
            "auto-remember agent provider '{other}' is invalid; expected claude|codex|gemini|qwen"
        )),
    }
}

fn configured_runtime_memory_backend() -> Option<runtime_layout::MemoryBackendConfig> {
    crate::config::runtime_root().map(|root| runtime_layout::load_memory_backend(&root))
}

fn configured_runtime_improver_mode() -> Option<String> {
    configured_runtime_memory_backend().map(|config| config.auto_remember.improver.mode)
}

fn configured_runtime_agent_provider() -> Option<String> {
    configured_runtime_memory_backend()
        .and_then(|config| config.auto_remember.improver.agent.provider)
}

fn configured_runtime_agent_model() -> Option<String> {
    configured_runtime_memory_backend().and_then(|config| config.auto_remember.improver.agent.model)
}

fn configured_runtime_agent_label() -> Option<String> {
    configured_runtime_memory_backend().and_then(|config| config.auto_remember.improver.agent.label)
}

fn is_loopback_url(url: &Url) -> bool {
    matches!(
        url.host_str(),
        Some("127.0.0.1") | Some("localhost") | Some("::1")
    )
}

fn build_rewrite_prompt(candidate: &AutoRememberQualityInput) -> String {
    let mut rules = vec![
        "- Use only the provided evidence.".to_string(),
        "- Return exactly one self-contained sentence in English.".to_string(),
        "- Do not use pronouns or deictic phrases like this, that, it, they, 해당, 이번."
            .to_string(),
        "- Do not add uncertainty or speculate beyond the evidence.".to_string(),
        "- Keep the sentence atomic and concrete.".to_string(),
        format!(
            "- Return JSON only with keys schema_version, content, keywords. schema_version must be {}.",
            AUTO_REMEMBER_LOCAL_REWRITE_SCHEMA_VERSION
        ),
    ];
    if let Some(entity_key) = candidate.entity_key.as_deref() {
        rules.push(format!(
            "- The sentence must include the exact config key `{entity_key}`."
        ));
    }

    let evidence = candidate
        .supporting_evidence
        .iter()
        .map(|line| format!("- {line}"))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "Signal kind: {}\nRaw candidate: {}\nRules:\n{}\nEvidence:\n{}\nJSON:",
        candidate.signal_kind,
        candidate.raw_content,
        rules.join("\n"),
        evidence,
    )
}

async fn rewrite_with_local_llm(
    candidate: &AutoRememberQualityInput,
) -> Result<AutoRememberRewriteOutput, String> {
    let config = local_rewrite_runtime_config()?;
    let prompt = build_rewrite_prompt(candidate);
    let client = reqwest::Client::builder()
        .timeout(config.timeout)
        .build()
        .map_err(|error| format!("auto-remember local rewrite client build failed: {error}"))?;

    let response = client
        .post(&config.base_url)
        .json(&json!({
            "model": config.model,
            "temperature": 0,
            "response_format": { "type": "json_object" },
            "messages": [
                {
                    "role": "system",
                    "content": "You only rewrite a candidate memory sentence into one self-contained sentence. Use only the provided evidence. Return JSON only."
                },
                {
                    "role": "user",
                    "content": prompt,
                }
            ]
        }))
        .send()
        .await
        .map_err(|error| format!("auto-remember local rewrite request failed: {error}"))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!(
            "auto-remember local rewrite failed with {status}: {body}"
        ));
    }

    let payload: LocalRewriteChatResponse = response
        .json()
        .await
        .map_err(|error| format!("auto-remember local rewrite response decode failed: {error}"))?;
    let raw_content = payload
        .choices
        .into_iter()
        .next()
        .map(|choice| choice.message.content)
        .ok_or_else(|| "auto-remember local rewrite returned no choices".to_string())?;
    parse_rewrite_output(&raw_content)
}

async fn rewrite_with_agent(
    candidate: &AutoRememberQualityInput,
    override_cfg: Option<&ResolvedAutoRememberImproverSettings>,
) -> Result<AutoRememberRewriteOutput, String> {
    let config = agent_rewrite_runtime_config_with_override(override_cfg)?;
    let prompt = format!(
        "You are the auto-remember quality improver agent `{}`.\nReturn JSON only.\n{}",
        config.label,
        build_rewrite_prompt(candidate)
    );
    let provider = config.provider.clone();
    let timeout = config.timeout;
    let label = format!("auto-remember agent rewrite ({})", config.label);

    let raw_content = tokio::task::spawn_blocking(move || {
        execute_agent_prompt(provider, config.model, &prompt, timeout, &label)
    })
    .await
    .map_err(|error| format!("auto-remember agent rewrite join failed: {error}"))??;

    parse_rewrite_output(&raw_content)
}

fn execute_agent_prompt(
    provider: ProviderKind,
    model: Option<String>,
    prompt: &str,
    timeout: Duration,
    label: &str,
) -> Result<String, String> {
    #[cfg(test)]
    {
        if let Some(override_fn) = TEST_AGENT_IMPROVER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            return override_fn(provider.clone(), model.clone(), prompt, timeout, label);
        }
    }

    let model_ref = model.as_deref();
    match (provider, model_ref) {
        (ProviderKind::Claude, None) => {
            crate::services::claude::execute_command_simple_with_timeout(prompt, timeout, label)
        }
        (ProviderKind::Claude, Some(model)) => run_with_timeout(
            {
                let prompt = prompt.to_string();
                let model = model.to_string();
                move || {
                    crate::services::claude::execute_command_simple_with_model(
                        &prompt,
                        Some(&model),
                    )
                }
            },
            timeout,
            label,
        ),
        (ProviderKind::Codex, None) => {
            crate::services::codex::execute_command_simple_with_timeout(prompt, timeout, label)
        }
        (ProviderKind::Codex, Some(model)) => run_with_timeout(
            {
                let prompt = prompt.to_string();
                let model = model.to_string();
                move || {
                    crate::services::codex::execute_command_simple_with_model(&prompt, Some(&model))
                }
            },
            timeout,
            label,
        ),
        (ProviderKind::Gemini, None) => {
            crate::services::gemini::execute_command_simple_with_timeout(prompt, timeout, label)
        }
        (ProviderKind::Gemini, Some(model)) => run_with_timeout(
            {
                let prompt = prompt.to_string();
                let model = model.to_string();
                move || {
                    crate::services::gemini::execute_command_simple_with_model(
                        &prompt,
                        Some(&model),
                    )
                }
            },
            timeout,
            label,
        ),
        (ProviderKind::Qwen, None) => {
            let prompt = prompt.to_string();
            run_with_timeout(
                move || crate::services::qwen::execute_command_simple(&prompt),
                timeout,
                label,
            )
        }
        (ProviderKind::Qwen, Some(model)) => run_with_timeout(
            {
                let prompt = prompt.to_string();
                let model = model.to_string();
                move || {
                    crate::services::qwen::execute_command_simple_with_model(&prompt, Some(&model))
                }
            },
            timeout,
            label,
        ),
        (ProviderKind::Unsupported(other), _) => Err(format!(
            "auto-remember agent provider '{}' is unsupported",
            other
        )),
    }
}

fn run_with_timeout<F>(f: F, timeout: Duration, label: &str) -> Result<String, String>
where
    F: FnOnce() -> Result<String, String> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => Err(format!("{label} timeout after {}s", timeout.as_secs())),
    }
}

fn parse_rewrite_output(raw: &str) -> Result<AutoRememberRewriteOutput, String> {
    let payload = parse_rewrite_payload(raw)?;
    if payload.schema_version != AUTO_REMEMBER_LOCAL_REWRITE_SCHEMA_VERSION {
        return Err(format!(
            "auto-remember rewrite schema mismatch: expected {}, got {}",
            AUTO_REMEMBER_LOCAL_REWRITE_SCHEMA_VERSION, payload.schema_version
        ));
    }

    let content = normalize_whitespace(&payload.content);
    if content.is_empty() {
        return Err("auto-remember rewrite returned empty content".to_string());
    }

    Ok(AutoRememberRewriteOutput {
        content,
        keyword_suggestions: payload
            .keywords
            .into_iter()
            .map(|keyword| normalize_keyword_suggestion(&keyword))
            .filter(|keyword| !keyword.is_empty())
            .collect(),
    })
}

fn parse_rewrite_payload(raw: &str) -> Result<LocalRewritePayload, String> {
    let trimmed = raw.trim();
    if let Ok(payload) = serde_json::from_str::<LocalRewritePayload>(trimmed) {
        return Ok(payload);
    }

    let stripped = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```"))
        .map(str::trim)
        .and_then(|value| value.strip_suffix("```"))
        .map(str::trim)
        .unwrap_or(trimmed);
    if let Ok(payload) = serde_json::from_str::<LocalRewritePayload>(stripped) {
        return Ok(payload);
    }

    let start = stripped
        .find('{')
        .ok_or_else(|| "auto-remember rewrite did not return JSON".to_string())?;
    let end = stripped
        .rfind('}')
        .ok_or_else(|| "auto-remember rewrite did not return JSON".to_string())?;
    serde_json::from_str::<LocalRewritePayload>(&stripped[start..=end]).map_err(|error| {
        format!("auto-remember rewrite JSON parse failed: {error}; body={stripped}")
    })
}

fn normalize_keyword_suggestion(value: &str) -> String {
    let token_re = Regex::new(r"[A-Za-z0-9_.:/-]{3,}").unwrap();
    token_re
        .find(&normalize_whitespace(value))
        .map(|matched| matched.as_str().to_ascii_lowercase())
        .unwrap_or_default()
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}
