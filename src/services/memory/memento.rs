use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use poise::serenity_prelude::ChannelId;
use serde_json::{Map, Value, json};

use super::{
    CaptureRequest, CaptureResult, LocalMemoryBackend, MemoryBackend, MemoryFuture, RecallMode,
    RecallRequest, RecallResponse, ReflectRequest, TokenUsage, UNBOUND_MEMORY_ROLE_ID,
    extract_token_usage,
    memento_throttle::{
        cached_recall_response, note_memento_dedup_hit, note_memento_remote_call,
        note_memento_tool_feedback_trigger, note_memento_tool_request, should_dedup_remember,
        store_recall_response, store_remember_fingerprint,
    },
};
use crate::runtime_layout;
use crate::services::discord::DispatchProfile;
use crate::services::discord::settings::ResolvedMemorySettings;

const MEMENTO_MCP_PATH: &str = "/mcp";
const MEMENTO_PROTOCOL_VERSION: &str = "2025-11-25";
const MAX_WORKING_MEMORY_LINES: usize = 6;
const MAX_MEMORY_LINES: usize = 6;
const MAX_SKIP_LINES: usize = 4;
const MEMENTO_MODEL_OUTPUT_MAX_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug)]
struct CachedMcpSession {
    endpoint: String,
    session_id: String,
}

#[derive(Clone)]
struct MementoRuntimeConfig {
    endpoint: String,
    access_key: String,
    workspace_override: Option<String>,
}

struct ToolCallResult {
    payload: Value,
    token_usage: TokenUsage,
}

struct ContextFetchResult {
    external_recall: Option<String>,
    token_usage: TokenUsage,
}

#[derive(Debug, PartialEq, Eq)]
struct ModelOutputGuardResult {
    text: String,
    truncated: bool,
    original_bytes: usize,
    limit_bytes: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct MementoRememberRequest {
    pub content: String,
    pub topic: String,
    pub kind: String,
    pub importance: Option<f64>,
    pub keywords: Vec<String>,
    pub source: Option<String>,
    pub workspace: Option<String>,
    pub global: bool,
    pub channel_id: Option<u64>,
    pub channel_name: Option<String>,
    pub agent_id: Option<String>,
    pub case_id: Option<String>,
    pub goal: Option<String>,
    pub outcome: Option<String>,
    pub phase: Option<String>,
    pub resolution_status: Option<String>,
    pub assertion_status: Option<String>,
    pub context_summary: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MementoToolFeedbackRequest {
    pub tool_name: String,
    pub relevant: bool,
    pub sufficient: bool,
    pub session_id: Option<String>,
    pub search_event_id: Option<String>,
    pub fragment_ids: Vec<String>,
    pub suggestion: Option<String>,
    pub context: Option<String>,
    pub trigger_type: Option<String>,
}

#[derive(Clone)]
pub(crate) struct MementoBackend {
    client: reqwest::Client,
    settings: ResolvedMemorySettings,
    local: LocalMemoryBackend,
    mcp_session: Arc<Mutex<Option<CachedMcpSession>>>,
}

impl MementoBackend {
    pub(crate) fn new(settings: ResolvedMemorySettings) -> Self {
        Self {
            client: reqwest::Client::new(),
            settings,
            local: LocalMemoryBackend,
            mcp_session: Arc::new(Mutex::new(None)),
        }
    }

    fn runtime_config(&self) -> Result<MementoRuntimeConfig, String> {
        let root = crate::config::runtime_root().ok_or_else(|| {
            "AGENTDESK runtime root is unavailable; skipping memento backend".to_string()
        })?;
        let config = runtime_layout::load_memory_backend(&root);
        let endpoint = normalize_memento_endpoint(&config.mcp.endpoint);
        if endpoint.is_empty() {
            return Err("memento endpoint is not configured; skipping memento backend".to_string());
        }

        let access_key_env = config.mcp.access_key_env.trim().to_string();
        if access_key_env.is_empty() {
            return Err(
                "memento access key env is not configured; skipping memento backend".to_string(),
            );
        }

        let access_key = std::env::var(&access_key_env)
            .map_err(|_| format!("{access_key_env} is not set; skipping memento backend"))?;

        Ok(MementoRuntimeConfig {
            endpoint,
            access_key,
            workspace_override: env_var_value("MEMENTO_WORKSPACE"),
        })
    }

    fn auth_request(
        &self,
        builder: reqwest::RequestBuilder,
        config: &MementoRuntimeConfig,
    ) -> reqwest::RequestBuilder {
        builder
            .header("Authorization", format!("Bearer {}", config.access_key))
            .header("Accept", "application/json")
            .header("Content-Type", "application/json")
    }

    fn resolve_workspace(
        &self,
        role_id: &str,
        channel_id: u64,
        channel_name: Option<&str>,
        config: &MementoRuntimeConfig,
    ) -> String {
        resolve_memento_workspace_for_channel(
            role_id,
            channel_id,
            channel_name,
            config.workspace_override.as_deref(),
        )
    }

    fn resolve_agent_id(&self, role_id: &str, channel_id: u64) -> String {
        resolve_memento_agent_id(role_id, channel_id)
    }

    fn cached_session_id(&self, endpoint: &str) -> Option<String> {
        let guard = self
            .mcp_session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard
            .as_ref()
            .filter(|session| session.endpoint == endpoint)
            .map(|session| session.session_id.clone())
    }

    fn store_session_id(&self, endpoint: &str, session_id: String) {
        let mut guard = self
            .mcp_session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = Some(CachedMcpSession {
            endpoint: endpoint.to_string(),
            session_id,
        });
    }

    fn clear_session_id(&self, endpoint: &str) {
        let mut guard = self
            .mcp_session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard
            .as_ref()
            .map(|session| session.endpoint == endpoint)
            .unwrap_or(false)
        {
            *guard = None;
        }
    }

    fn capture_session_id(
        &self,
        config: &MementoRuntimeConfig,
        response: &reqwest::Response,
    ) -> Option<String> {
        response
            .headers()
            .get("MCP-Session-Id")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| {
                let session_id = value.to_string();
                self.store_session_id(&config.endpoint, session_id.clone());
                session_id
            })
    }

    async fn initialize_session(&self, config: &MementoRuntimeConfig) -> Result<String, String> {
        // #2049 Finding 12: do *not* include `accessKey` in the JSON-RPC body.
        // Authentication is already carried by the `Authorization: Bearer`
        // header in `auth_request`; duplicating the secret in the body means
        // any reverse-proxy / access-log / error echo that captures the body
        // leaks the credential into tracing logs and (via warnings) into
        // user-facing turn results. Keep the body free of long-lived secrets.
        let response = self
            .auth_request(self.client.post(mcp_url(&config.endpoint)), config)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": MEMENTO_PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {
                        "name": "agentdesk",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }
            }))
            .send()
            .await
            .map_err(|err| format!("memento initialize request failed: {err}"))?;

        let session_id = self.capture_session_id(config, &response);
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|err| format!("memento initialize response read failed: {err}"))?;

        if !status.is_success() {
            // #2049 Finding 12: redact any value resembling the configured
            // access_key before bubbling the error text up the stack.
            let safe = redact_memento_secret(&text, &config.access_key);
            return Err(format!("memento initialize failed with {status}: {safe}"));
        }

        let payload: Value = serde_json::from_str(&text).map_err(|err| {
            let safe = redact_memento_secret(&text, &config.access_key);
            format!("memento initialize response decode failed: {err}; body={safe}")
        })?;
        if let Some(error) = payload.get("error") {
            return Err(format!(
                "memento initialize rpc failed: {}",
                render_rpc_error(error)
            ));
        }

        session_id.ok_or_else(|| {
            let safe = redact_memento_secret(&text, &config.access_key);
            format!("memento initialize succeeded without MCP-Session-Id header; body={safe}")
        })
    }

    async fn ensure_session(&self, config: &MementoRuntimeConfig) -> Result<String, String> {
        if let Some(session_id) = self.cached_session_id(&config.endpoint) {
            return Ok(session_id);
        }
        self.initialize_session(config).await
    }

    async fn call_tool(
        &self,
        config: &MementoRuntimeConfig,
        tool_name: &str,
        arguments: Value,
    ) -> Result<ToolCallResult, String> {
        let mut session_id = self.ensure_session(config).await?;

        for attempt in 0..2 {
            let response = self
                .auth_request(self.client.post(mcp_url(&config.endpoint)), config)
                .header("MCP-Session-Id", session_id.as_str())
                .json(&json!({
                    "jsonrpc": "2.0",
                    "id": 2,
                    "method": "tools/call",
                    "params": {
                        "name": tool_name,
                        "arguments": arguments.clone(),
                    }
                }))
                .send()
                .await
                .map_err(|err| format!("memento {tool_name} request failed: {err}"))?;

            self.capture_session_id(config, &response);

            let status = response.status();
            let text = response
                .text()
                .await
                .map_err(|err| format!("memento {tool_name} response read failed: {err}"))?;

            if !status.is_success() {
                if attempt == 0
                    && (status == reqwest::StatusCode::UNAUTHORIZED || is_session_error(&text))
                {
                    self.clear_session_id(&config.endpoint);
                    session_id = self.initialize_session(config).await?;
                    continue;
                }
                // #2049 Finding 12: redact bearer-like substrings from error
                // bubbling so any memento-side echo cannot expose credentials.
                let safe = redact_memento_secret(&text, &config.access_key);
                return Err(format!("memento {tool_name} failed with {status}: {safe}"));
            }

            let payload: Value = serde_json::from_str(&text).map_err(|err| {
                let safe = redact_memento_secret(&text, &config.access_key);
                format!("memento {tool_name} response decode failed: {err}; body={safe}")
            })?;

            if let Some(error) = payload.get("error") {
                let detail = render_rpc_error(error);
                if attempt == 0 && is_session_error(&detail) {
                    self.clear_session_id(&config.endpoint);
                    session_id = self.initialize_session(config).await?;
                    continue;
                }
                return Err(format!("memento {tool_name} rpc failed: {detail}"));
            }

            return extract_tool_result(&payload, tool_name);
        }

        Err(format!(
            "memento {tool_name} failed after retrying session initialization"
        ))
    }

    async fn fetch_context(
        &self,
        request: &RecallRequest,
        config: &MementoRuntimeConfig,
        workspace: &str,
    ) -> Result<ContextFetchResult, String> {
        let agent_id = self.resolve_agent_id(&request.role_id, request.channel_id);
        let mut args = Map::new();
        args.insert("agentId".to_string(), json!(agent_id));
        args.insert("sessionId".to_string(), json!(request.session_id));
        args.insert("structured".to_string(), json!(true));
        if !workspace.trim().is_empty() {
            args.insert("workspace".to_string(), json!(workspace));
        }
        let result = self
            .call_tool(config, "context", Value::Object(args))
            .await?;
        // #1083: Different formatter per mode — `Full` keeps every section,
        // `IdentityOnly` strips down to the identity + current session lines.
        let external_recall = match request.mode {
            RecallMode::Full => format_context_payload_for_external_recall(&result.payload),
            RecallMode::IdentityOnly => format_context_payload_for_identity_only(&result.payload),
        };
        Ok(ContextFetchResult {
            external_recall,
            token_usage: result.token_usage,
        })
    }

    async fn reflect_transcript(
        &self,
        request: &ReflectRequest,
        config: &MementoRuntimeConfig,
        workspace: &str,
    ) -> Result<TokenUsage, String> {
        let agent_id = self.resolve_agent_id(&request.role_id, request.channel_id);
        let mut args = Map::new();
        args.insert("agentId".to_string(), json!(agent_id));
        args.insert("sessionId".to_string(), json!(request.session_id));
        args.insert(
            "summary".to_string(),
            json!([build_reflect_summary(request)]),
        );
        if !workspace.trim().is_empty() {
            args.insert("workspace".to_string(), json!(workspace));
        }
        note_memento_tool_request("reflect");
        note_memento_remote_call("reflect");
        self.call_tool(config, "reflect", Value::Object(args))
            .await
            .map(|result| result.token_usage)
    }

    pub(crate) async fn remember(
        &self,
        request: MementoRememberRequest,
    ) -> Result<TokenUsage, String> {
        if request.content.trim().is_empty() {
            return Err("memento remember requires non-empty content".to_string());
        }
        if request.topic.trim().is_empty() {
            return Err("memento remember requires non-empty topic".to_string());
        }
        if request.kind.trim().is_empty() {
            return Err("memento remember requires non-empty type".to_string());
        }

        let config = self.runtime_config()?;
        let MementoRememberRequest {
            content,
            topic,
            kind,
            importance,
            keywords,
            source,
            workspace,
            global,
            channel_id,
            channel_name,
            agent_id,
            case_id,
            goal,
            outcome,
            phase,
            resolution_status,
            assertion_status,
            context_summary,
        } = request;
        let normalized_content = normalize_whitespace(&content);
        let normalized_topic = normalize_whitespace(&topic);
        let normalized_kind = normalize_whitespace(&kind);
        if global && workspace.is_some() {
            return Err("memento remember cannot combine global=true with workspace".to_string());
        }
        if matches!(channel_id, Some(0)) {
            return Err(
                "memento remember channel_id must be a non-zero Discord snowflake".to_string(),
            );
        }
        if !global
            && workspace.is_none()
            && channel_id.is_none()
            && config.workspace_override.is_none()
        {
            return Err(
                "memento remember requires workspace, channel_id, global=true, or MEMENTO_WORKSPACE"
                    .to_string(),
            );
        }
        let resolved_workspace = if global {
            None
        } else {
            workspace.or_else(|| {
                channel_id
                    .map(|channel_id| {
                        resolve_memento_workspace_for_channel(
                            UNBOUND_MEMORY_ROLE_ID,
                            channel_id,
                            channel_name.as_deref(),
                            config.workspace_override.as_deref(),
                        )
                    })
                    .or_else(|| config.workspace_override.clone())
            })
        };
        let dedup_key = build_remember_dedup_key(
            &normalized_content,
            &normalized_topic,
            &normalized_kind,
            resolved_workspace.as_deref(),
            agent_id.as_deref(),
            case_id.as_deref(),
        );
        note_memento_tool_request("remember");
        if should_dedup_remember(&dedup_key, importance) {
            note_memento_dedup_hit("remember");
            return Ok(TokenUsage::default());
        }

        let mut args = Map::new();
        args.insert("content".to_string(), json!(normalized_content));
        args.insert("topic".to_string(), json!(normalized_topic));
        args.insert("type".to_string(), json!(normalized_kind));
        if let Some(importance) = importance {
            args.insert("importance".to_string(), json!(importance));
        }

        let keywords = keywords
            .into_iter()
            .map(|value| normalize_whitespace(&value))
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        if !keywords.is_empty() {
            args.insert("keywords".to_string(), json!(keywords));
        }

        insert_optional_arg(&mut args, "source", source);
        insert_optional_arg(&mut args, "workspace", resolved_workspace);
        insert_optional_arg(&mut args, "agentId", agent_id);
        insert_optional_arg(&mut args, "caseId", case_id);
        insert_optional_arg(&mut args, "goal", goal);
        insert_optional_arg(&mut args, "outcome", outcome);
        insert_optional_arg(&mut args, "phase", phase);
        insert_optional_arg(&mut args, "resolutionStatus", resolution_status);
        insert_optional_arg(&mut args, "assertionStatus", assertion_status);
        insert_optional_arg(&mut args, "contextSummary", context_summary);

        note_memento_remote_call("remember");
        self.call_tool(&config, "remember", Value::Object(args))
            .await
            .map(|result| {
                store_remember_fingerprint(dedup_key, importance);
                result.token_usage
            })
    }

    pub(crate) async fn tool_feedback(
        &self,
        request: MementoToolFeedbackRequest,
    ) -> Result<TokenUsage, String> {
        if request.tool_name.trim().is_empty() {
            return Err("memento tool_feedback requires non-empty tool_name".to_string());
        }

        let config = self.runtime_config()?;
        let mut args = Map::new();
        args.insert("tool_name".to_string(), json!(request.tool_name.trim()));
        args.insert("relevant".to_string(), json!(request.relevant));
        args.insert("sufficient".to_string(), json!(request.sufficient));

        insert_optional_arg(&mut args, "sessionId", request.session_id);
        insert_optional_arg(&mut args, "searchEventId", request.search_event_id);

        let fragment_ids = request
            .fragment_ids
            .into_iter()
            .map(|value| normalize_whitespace(&value))
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        if !fragment_ids.is_empty() {
            args.insert("fragmentIds".to_string(), json!(fragment_ids));
        }

        insert_optional_arg(&mut args, "suggestion", request.suggestion);
        insert_optional_arg(&mut args, "context", request.context);
        insert_optional_arg(
            &mut args,
            "triggerType",
            Some(normalize_tool_feedback_trigger_type(request.trigger_type)),
        );

        note_memento_tool_request("tool_feedback");
        note_memento_tool_feedback_trigger(
            args.get("triggerType")
                .and_then(Value::as_str)
                .unwrap_or("voluntary"),
        );
        note_memento_remote_call("tool_feedback");
        self.call_tool(&config, "tool_feedback", Value::Object(args))
            .await
            .map(|result| result.token_usage)
    }
}

fn env_var_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn normalize_memento_endpoint(endpoint: &str) -> String {
    let trimmed = endpoint.trim().trim_end_matches('/');
    trimmed
        .strip_suffix(MEMENTO_MCP_PATH)
        .unwrap_or(trimmed)
        .to_string()
}

fn normalize_tool_feedback_trigger_type(trigger_type: Option<String>) -> String {
    match trigger_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_ascii_lowercase())
        .as_deref()
    {
        Some("automatic") => "automatic".to_string(),
        Some("voluntary") | Some("manual") => "voluntary".to_string(),
        _ => "voluntary".to_string(),
    }
}

fn mcp_url(endpoint: &str) -> String {
    format!(
        "{}{}",
        normalize_memento_endpoint(endpoint),
        MEMENTO_MCP_PATH
    )
}

fn build_recall_dedup_key(
    workspace: &str,
    agent_id: &str,
    session_id: &str,
    dispatch_profile: DispatchProfile,
    mode: RecallMode,
    user_text: &str,
) -> String {
    let profile = match dispatch_profile {
        DispatchProfile::Full => "full",
        DispatchProfile::Lite => "lite",
        DispatchProfile::ReviewLite => "review_lite",
    };
    let mode = match mode {
        RecallMode::Full => "full",
        RecallMode::IdentityOnly => "identity_only",
    };
    [
        workspace.trim(),
        agent_id.trim(),
        session_id.trim(),
        profile,
        mode,
        user_text.trim(),
    ]
    .join("\u{1f}")
}

fn build_remember_dedup_key(
    content: &str,
    topic: &str,
    kind: &str,
    workspace: Option<&str>,
    agent_id: Option<&str>,
    case_id: Option<&str>,
) -> String {
    [
        content.trim(),
        topic.trim(),
        kind.trim(),
        workspace.unwrap_or("").trim(),
        agent_id.unwrap_or("").trim(),
        case_id.unwrap_or("").trim(),
    ]
    .join("\u{1f}")
}

fn render_rpc_error(error: &Value) -> String {
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown rpc error".to_string());
    match error.get("code").and_then(Value::as_i64) {
        Some(code) => format!("{message} (code={code})"),
        None => message,
    }
}

/// #2049 Finding 12: replace any occurrence of the live memento access key
/// with a fixed placeholder before the value lands in error logs / user-facing
/// warnings. We never want a bearer token to be inlined into tracing output or
/// Discord messages. Short access keys (<12 chars, which would over-match) are
/// passed through unchanged.
fn redact_memento_secret(body: &str, access_key: &str) -> String {
    let key = access_key.trim();
    if key.len() < 12 {
        return body.to_string();
    }
    body.replace(key, "***redacted-memento-secret***")
}

fn is_session_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("session required")
        || message.contains("session not found")
        || message.contains("session expired")
        || message.contains("unauthorized")
        || message.contains("401")
}

fn extract_tool_result(payload: &Value, tool_name: &str) -> Result<ToolCallResult, String> {
    if payload
        .get("result")
        .and_then(|result| result.get("isError"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(format!("memento {tool_name} returned isError=true"));
    }

    let text = payload
        .get("result")
        .and_then(|result| result.get("content"))
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .find_map(|item| item.get("text").and_then(Value::as_str))
        })
        .ok_or_else(|| {
            format!("memento {tool_name} response missing result.content[0].text: {payload}")
        })?;
    let parsed: Value = serde_json::from_str(text).map_err(|err| {
        format!("memento {tool_name} content decode failed: {err}; content={text}")
    })?;
    if parsed
        .get("success")
        .and_then(Value::as_bool)
        .map(|success| !success)
        .unwrap_or(false)
    {
        let error = parsed
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown tool error");
        return Err(format!("memento {tool_name} tool failed: {error}"));
    }
    Ok(ToolCallResult {
        token_usage: extract_token_usage(payload)
            .or_else(|| extract_token_usage(&parsed))
            .unwrap_or_default(),
        payload: parsed,
    })
}

pub(crate) fn resolve_memento_workspace(
    role_id: &str,
    channel_id: u64,
    workspace_override: Option<&str>,
) -> String {
    resolve_memento_workspace_for_channel(role_id, channel_id, None, workspace_override)
}

pub(crate) fn resolve_memento_workspace_for_channel(
    role_id: &str,
    channel_id: u64,
    channel_name: Option<&str>,
    workspace_override: Option<&str>,
) -> String {
    if let Some(workspace) = workspace_override
        .map(str::trim)
        .filter(|workspace| !workspace.is_empty())
    {
        return workspace.to_string();
    }

    if channel_id != 0 {
        if let Some(workspace) = crate::services::discord::settings::resolve_workspace(
            ChannelId::new(channel_id),
            channel_name,
        )
        .and_then(|workspace| memento_workspace_from_channel_workspace(&workspace))
        {
            return workspace;
        }
    }

    let role_id = role_id.trim();
    if role_id.is_empty() || role_id == UNBOUND_MEMORY_ROLE_ID {
        tracing::warn!(
            channel_id,
            "memento workspace fallback used for unregistered channel"
        );
        return format!("channel-{channel_id}");
    }

    format!("agentdesk-{}", sanitize_memento_workspace_segment(role_id))
}

fn memento_workspace_from_channel_workspace(workspace: &str) -> Option<String> {
    let trimmed = workspace.trim();
    if trimmed.is_empty() {
        return None;
    }
    let candidate = if trimmed.contains('/') || trimmed.contains('\\') {
        Path::new(trimmed)
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or(trimmed)
    } else {
        trimmed
    };
    let sanitized = sanitize_memento_workspace_segment(candidate);
    if sanitized == "default" {
        return None;
    }
    if sanitized == "agentdesk" || sanitized.starts_with("agentdesk-") {
        Some(sanitized)
    } else {
        Some(format!("agentdesk-{sanitized}"))
    }
}

pub(crate) fn resolve_memento_agent_id(role_id: &str, channel_id: u64) -> String {
    let role_id = role_id.trim();
    if role_id.is_empty() || role_id == UNBOUND_MEMORY_ROLE_ID {
        return format!("agentdesk-channel-{channel_id}");
    }
    role_id.to_string()
}

pub(crate) fn sanitize_memento_workspace_segment(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    let mut last_was_dash = false;

    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            sanitized.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if !last_was_dash {
            sanitized.push('-');
            last_was_dash = true;
        }
    }

    let trimmed = sanitized.trim_matches('-');
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed.to_string()
    }
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn insert_optional_arg(args: &mut Map<String, Value>, key: &str, value: Option<String>) {
    let Some(value) = value
        .map(|value| normalize_whitespace(&value))
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    args.insert(key.to_string(), json!(value));
}

#[cfg(test)]
mod workspace_scope_tests {
    use super::*;

    #[test]
    fn memento_workspace_from_channel_workspace_uses_path_basename() {
        assert_eq!(
            memento_workspace_from_channel_workspace("~/.adk/release/workspaces/agentdesk"),
            Some("agentdesk".to_string())
        );
        assert_eq!(
            memento_workspace_from_channel_workspace("~/.adk/release/workspaces/project-agentdesk"),
            Some("agentdesk-project-agentdesk".to_string())
        );
        assert_eq!(
            memento_workspace_from_channel_workspace("Project AgentDesk"),
            Some("agentdesk-project-agentdesk".to_string())
        );
    }

    #[test]
    fn unbound_memento_workspace_falls_back_to_channel_scope() {
        assert_eq!(
            resolve_memento_workspace_for_channel(UNBOUND_MEMORY_ROLE_ID, 4242, None, None),
            "channel-4242"
        );
    }
}

fn truncate_text(value: &str, max_chars: usize) -> String {
    let trimmed = value.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    if max_chars <= 3 {
        return trimmed.chars().take(max_chars).collect();
    }
    let mut shortened = trimmed.chars().take(max_chars - 3).collect::<String>();
    shortened.push_str("...");
    shortened
}

fn truncate_to_byte_boundary(value: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(value.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn guard_mcp_output_for_model(value: String, max_bytes: usize) -> ModelOutputGuardResult {
    let original_bytes = value.len();
    if original_bytes <= max_bytes {
        return ModelOutputGuardResult {
            text: value,
            truncated: false,
            original_bytes,
            limit_bytes: max_bytes,
        };
    }

    if max_bytes == 0 {
        return ModelOutputGuardResult {
            text: String::new(),
            truncated: true,
            original_bytes,
            limit_bytes: max_bytes,
        };
    }

    let notice = format!(
        "\n\n[truncated memento MCP output: original_bytes={original_bytes}, limit_bytes={max_bytes}]"
    );
    let text = if notice.len() >= max_bytes {
        truncate_to_byte_boundary(&value, max_bytes).to_string()
    } else {
        let keep_bytes = max_bytes - notice.len();
        format!(
            "{}{}",
            truncate_to_byte_boundary(&value, keep_bytes),
            notice
        )
    };

    ModelOutputGuardResult {
        text,
        truncated: true,
        original_bytes,
        limit_bytes: max_bytes,
    }
}

fn summarize_transcript_line(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() || line == "[Stopped]" || line.starts_with("[System]:") {
        return None;
    }
    let (prefix, content) = if let Some(rest) = line.strip_prefix("[User]:") {
        ("User: ", rest)
    } else if let Some(rest) = line.strip_prefix("[Assistant]:") {
        ("Assistant: ", rest)
    } else if let Some(rest) = line.strip_prefix("[Tool]:") {
        ("Tool: ", rest)
    } else {
        ("", line)
    };
    let content = normalize_whitespace(content);
    if content.is_empty() {
        return None;
    }
    Some(format!("{prefix}{}", truncate_text(&content, 120)))
}

fn summarize_transcript_for_reflect(transcript: &str) -> Option<String> {
    let mut lines = Vec::new();
    for line in transcript.lines().rev() {
        let Some(line) = summarize_transcript_line(line) else {
            continue;
        };
        if lines.iter().any(|existing| existing == &line) {
            continue;
        }
        lines.push(line);
        if lines.len() >= 4 {
            break;
        }
    }
    lines.reverse();
    if lines.is_empty() {
        None
    } else {
        Some(truncate_text(&lines.join(" | "), 320))
    }
}

fn build_reflect_summary(request: &ReflectRequest) -> String {
    summarize_transcript_for_reflect(&request.transcript).unwrap_or_else(|| {
        format!(
            "Session ended via {} for role_id={} channel_id={}",
            request.reason.as_str(),
            request.role_id,
            request.channel_id
        )
    })
}

fn dedup_lines<I>(items: I, limit: usize) -> Vec<String>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut seen = std::collections::HashSet::new();
    let mut lines = Vec::new();

    for (dedup_key, rendered) in items {
        let dedup_key = normalize_whitespace(&dedup_key);
        if dedup_key.is_empty() || !seen.insert(dedup_key) {
            continue;
        }
        lines.push(rendered);
        if lines.len() >= limit {
            break;
        }
    }

    lines
}

fn format_working_memory_line(item: &Map<String, Value>) -> Option<(String, String)> {
    let title = item
        .get("title")
        .and_then(Value::as_str)
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty())?;
    let category = item
        .get("category")
        .and_then(Value::as_str)
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty());
    let next_action = item
        .get("next_action")
        .and_then(Value::as_str)
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty());
    let tags = item
        .get("tags")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|tags| !tags.is_empty());

    let mut line = String::new();
    if let Some(category) = category {
        line.push_str(&format!("[{category}] "));
    }
    line.push_str(&title);
    if let Some(next_action) = next_action {
        line.push_str(&format!(" -> next: {next_action}"));
    }
    if let Some(tags) = tags {
        line.push_str(&format!(" [tags: {}]", tags.join(", ")));
    }

    Some((title, line))
}

fn format_memory_line(item: &Map<String, Value>) -> Option<(String, String)> {
    let content = item
        .get("content")
        .and_then(Value::as_str)
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty())?;
    let score = item.get("score").and_then(Value::as_f64);
    let memory_type = item
        .get("type")
        .and_then(Value::as_str)
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty());
    let tags = item
        .get("tags")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|tags| !tags.is_empty());

    let mut line = match score {
        Some(score) => format!("({score:.2}) {content}"),
        None => content.clone(),
    };
    if let Some(memory_type) = memory_type {
        line.push_str(&format!(" [type: {memory_type}]"));
    }
    if let Some(tags) = tags {
        line.push_str(&format!(" [tags: {}]", tags.join(", ")));
    }

    Some((content, line))
}

fn format_ranked_memory_line(item: &Map<String, Value>) -> Option<(String, String)> {
    let content = item
        .get("content")
        .or_else(|| item.get("title"))
        .and_then(Value::as_str)
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty())?;
    let score = item
        .get("score")
        .or_else(|| item.get("importance"))
        .and_then(Value::as_f64);
    let memory_type = item
        .get("type")
        .and_then(Value::as_str)
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty());
    let topic = item
        .get("topic")
        .and_then(Value::as_str)
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty());

    let mut line = match score {
        Some(score) => format!("({score:.2}) {content}"),
        None => content.clone(),
    };
    if let Some(memory_type) = memory_type {
        line.push_str(&format!(" [type: {memory_type}]"));
    }
    if let Some(topic) = topic {
        line.push_str(&format!(" [topic: {topic}]"));
    }

    Some((content, line))
}

fn format_core_memory_line(label: &str, item: &Map<String, Value>) -> Option<(String, String)> {
    let (dedup_key, line) = format_ranked_memory_line(item)?;
    Some((format!("{label}:{dedup_key}"), format!("[{label}] {line}")))
}

fn format_skip_line(item: &Map<String, Value>) -> Option<(String, String)> {
    let skip_item = item
        .get("item")
        .and_then(Value::as_str)
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty())?;
    let reason = item
        .get("reason")
        .and_then(Value::as_str)
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty())?;
    let expires = item
        .get("expires")
        .and_then(Value::as_str)
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty());

    let mut line = format!("{skip_item} -> {reason}");
    if let Some(expires) = expires {
        line.push_str(&format!(" [expires: {expires}]"));
    }

    Some((skip_item, line))
}

/// #1083: Identity-only memento payload — emitted on default session-start
/// turns when no recall trigger fires. Only the `identity` and the active
/// `current_session` lines are kept so the model still knows who it is talking
/// to without paying the full context cost.
fn format_context_payload_for_identity_only(payload: &Value) -> Option<String> {
    let mut sections = vec!["[External Recall — Identity Lite]".to_string()];

    if let Some(hint) = search_event_feedback_hint(payload) {
        sections.push(hint);
    }

    if let Some(identity) = payload
        .get("identity")
        .and_then(Value::as_str)
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty())
    {
        sections.push(format!("Identity from Memento:\n{identity}"));
    }

    let working_session_lines = payload
        .get("working")
        .and_then(Value::as_object)
        .and_then(|working| working.get("current_session"))
        .and_then(Value::as_array)
        .map(|items| {
            dedup_lines(
                items
                    .iter()
                    .filter_map(Value::as_object)
                    .filter_map(|item| format_core_memory_line("session", item)),
                MAX_WORKING_MEMORY_LINES,
            )
        })
        .unwrap_or_default();
    if !working_session_lines.is_empty() {
        sections.push(format!(
            "Current session context from Memento:\n- {}",
            working_session_lines.join("\n- ")
        ));
    }

    if sections.len() == 1 {
        None
    } else {
        Some(sections.join("\n"))
    }
}

fn format_context_payload_for_external_recall(payload: &Value) -> Option<String> {
    let mut sections = vec!["[External Recall]".to_string()];

    if let Some(hint) = search_event_feedback_hint(payload) {
        sections.push(hint);
    }

    if let Some(identity) = payload
        .get("identity")
        .and_then(Value::as_str)
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty())
    {
        sections.push(format!("Identity from Memento:\n{identity}"));
    }

    let working_memory_lines = payload
        .get("working_memory")
        .and_then(Value::as_object)
        .and_then(|working_memory| working_memory.get("items"))
        .and_then(Value::as_array)
        .map(|items| {
            dedup_lines(
                items
                    .iter()
                    .filter_map(Value::as_object)
                    .filter_map(format_working_memory_line),
                MAX_WORKING_MEMORY_LINES,
            )
        })
        .unwrap_or_default();
    if !working_memory_lines.is_empty() {
        sections.push(format!(
            "Active working memory from Memento:\n- {}",
            working_memory_lines.join("\n- ")
        ));
    }

    let working_session_lines = payload
        .get("working")
        .and_then(Value::as_object)
        .and_then(|working| working.get("current_session"))
        .and_then(Value::as_array)
        .map(|items| {
            dedup_lines(
                items
                    .iter()
                    .filter_map(Value::as_object)
                    .filter_map(|item| format_core_memory_line("session", item)),
                MAX_WORKING_MEMORY_LINES,
            )
        })
        .unwrap_or_default();
    if !working_session_lines.is_empty() {
        sections.push(format!(
            "Current session context from Memento:\n- {}",
            working_session_lines.join("\n- ")
        ));
    }

    let memory_lines = payload
        .get("memories")
        .and_then(Value::as_object)
        .and_then(|memories| memories.get("matches"))
        .and_then(Value::as_array)
        .map(|items| {
            dedup_lines(
                items
                    .iter()
                    .filter_map(Value::as_object)
                    .filter_map(format_memory_line),
                MAX_MEMORY_LINES,
            )
        })
        .unwrap_or_default();
    if !memory_lines.is_empty() {
        sections.push(format!(
            "Relevant memories from Memento:\n- {}",
            memory_lines.join("\n- ")
        ));
    }

    let ranked_memory_lines = payload
        .get("rankedInjection")
        .and_then(Value::as_object)
        .and_then(|ranked| ranked.get("items"))
        .and_then(Value::as_array)
        .map(|items| {
            dedup_lines(
                items
                    .iter()
                    .filter_map(Value::as_object)
                    .filter_map(format_ranked_memory_line),
                MAX_MEMORY_LINES,
            )
        })
        .unwrap_or_default();
    if !ranked_memory_lines.is_empty() {
        sections.push(format!(
            "Ranked context from Memento:\n- {}",
            ranked_memory_lines.join("\n- ")
        ));
    }

    let core_memory_lines = payload
        .get("core")
        .and_then(Value::as_object)
        .map(|core| {
            let categories = [
                ("preferences", "preference"),
                ("errors", "error"),
                ("decisions", "decision"),
                ("procedures", "procedure"),
            ];
            dedup_lines(
                categories.into_iter().flat_map(|(field, label)| {
                    core.get(field)
                        .and_then(Value::as_array)
                        .into_iter()
                        .flat_map(move |items| {
                            items
                                .iter()
                                .filter_map(Value::as_object)
                                .filter_map(move |item| format_core_memory_line(label, item))
                        })
                }),
                MAX_MEMORY_LINES,
            )
        })
        .unwrap_or_default();
    if !core_memory_lines.is_empty() {
        sections.push(format!(
            "Core memory from Memento:\n- {}",
            core_memory_lines.join("\n- ")
        ));
    }

    if ranked_memory_lines.is_empty() {
        let anchor_lines = payload
            .get("anchors")
            .and_then(Value::as_object)
            .and_then(|anchors| anchors.get("permanent"))
            .and_then(Value::as_array)
            .map(|items| {
                dedup_lines(
                    items
                        .iter()
                        .filter_map(Value::as_object)
                        .filter_map(format_ranked_memory_line),
                    MAX_MEMORY_LINES,
                )
            })
            .unwrap_or_default();
        if !anchor_lines.is_empty() {
            sections.push(format!(
                "Anchored memory from Memento:\n- {}",
                anchor_lines.join("\n- ")
            ));
        }
    }

    let skip_lines = payload
        .get("skip_matches")
        .and_then(Value::as_array)
        .map(|items| {
            dedup_lines(
                items
                    .iter()
                    .filter_map(Value::as_object)
                    .filter_map(format_skip_line),
                MAX_SKIP_LINES,
            )
        })
        .unwrap_or_default();
    if !skip_lines.is_empty() {
        sections.push(format!(
            "Skip list warnings from Memento:\n- {}",
            skip_lines.join("\n- ")
        ));
    }

    if let Some(suggestion) = payload
        .get("_memento_hint")
        .and_then(Value::as_object)
        .and_then(|hint| hint.get("suggestion"))
        .and_then(Value::as_str)
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty())
    {
        sections.push(format!("Memento hint:\n{suggestion}"));
    }

    if sections.len() == 1 {
        None
    } else {
        let guarded =
            guard_mcp_output_for_model(sections.join("\n"), MEMENTO_MODEL_OUTPUT_MAX_BYTES);
        if guarded.truncated {
            tracing::warn!(
                "[memory] truncated memento MCP output before model injection: original_bytes={} limit_bytes={}",
                guarded.original_bytes,
                guarded.limit_bytes
            );
        }
        Some(guarded.text)
    }
}

fn search_event_feedback_hint(payload: &Value) -> Option<String> {
    let search_event_id = payload
        .get("_meta")
        .and_then(Value::as_object)
        .and_then(|meta| {
            meta.get("searchEventId")
                .or_else(|| meta.get("search_event_id"))
                .or_else(|| meta.get("_searchEventId"))
        })
        .or_else(|| payload.get("searchEventId"))
        .or_else(|| payload.get("search_event_id"))
        .or_else(|| payload.get("_searchEventId"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())?;

    Some(format!(
        "Memento searchEventId: {search_event_id}\ntool_feedback(search_event_id={search_event_id}, relevant, sufficient) before turn end"
    ))
}

impl MemoryBackend for MementoBackend {
    fn recall<'a>(&'a self, request: RecallRequest) -> MemoryFuture<'a, RecallResponse> {
        Box::pin(async move {
            if request.dispatch_profile == DispatchProfile::ReviewLite {
                return RecallResponse::default();
            }

            let config = match self.runtime_config() {
                Ok(config) => config,
                Err(err) => {
                    let mut fallback = self.local.recall(request.clone()).await;
                    fallback.warnings.push(err);
                    return fallback;
                }
            };
            let workspace = self.resolve_workspace(
                &request.role_id,
                request.channel_id,
                request.channel_name.as_deref(),
                &config,
            );
            let agent_id = self.resolve_agent_id(&request.role_id, request.channel_id);
            let dedup_key = build_recall_dedup_key(
                &workspace,
                &agent_id,
                &request.session_id,
                request.dispatch_profile,
                request.mode,
                &normalize_whitespace(&request.user_text),
            );
            note_memento_tool_request("recall");
            if let Some(external_recall) = cached_recall_response(&dedup_key) {
                note_memento_dedup_hit("recall");
                return RecallResponse {
                    external_recall,
                    ..RecallResponse::default()
                };
            }
            note_memento_remote_call("recall");

            match tokio::time::timeout(
                Duration::from_millis(self.settings.recall_timeout_ms),
                self.fetch_context(&request, &config, &workspace),
            )
            .await
            {
                Ok(Ok(result)) => {
                    store_recall_response(dedup_key, result.external_recall.clone());
                    RecallResponse {
                        external_recall: result.external_recall,
                        memento_context_loaded: true,
                        token_usage: result.token_usage,
                        ..RecallResponse::default()
                    }
                }
                Ok(Err(err)) => {
                    let mut fallback = self.local.recall(request.clone()).await;
                    fallback.warnings.push(err);
                    fallback
                }
                Err(_) => {
                    let mut fallback = self.local.recall(request.clone()).await;
                    fallback.warnings.push(format!(
                        "memento recall timed out after {}ms; falling back to local memory",
                        self.settings.recall_timeout_ms
                    ));
                    fallback
                }
            }
        })
    }

    fn capture<'a>(&'a self, request: CaptureRequest) -> MemoryFuture<'a, CaptureResult> {
        Box::pin(async move {
            let _ = request;
            CaptureResult {
                skipped: true,
                ..CaptureResult::default()
            }
        })
    }

    fn reflect<'a>(&'a self, request: ReflectRequest) -> MemoryFuture<'a, CaptureResult> {
        Box::pin(async move {
            if request.transcript.trim().is_empty() {
                return CaptureResult {
                    warnings: vec![
                        "memento reflect skipped because session transcript is empty".to_string(),
                    ],
                    skipped: true,
                    ..CaptureResult::default()
                };
            }

            let config = match self.runtime_config() {
                Ok(config) => config,
                Err(err) => {
                    return CaptureResult {
                        warnings: vec![err],
                        skipped: true,
                        ..CaptureResult::default()
                    };
                }
            };
            let workspace = self.resolve_workspace(
                &request.role_id,
                request.channel_id,
                request.channel_name.as_deref(),
                &config,
            );

            match tokio::time::timeout(
                Duration::from_millis(self.settings.capture_timeout_ms),
                self.reflect_transcript(&request, &config, &workspace),
            )
            .await
            {
                Ok(Ok(token_usage)) => CaptureResult {
                    token_usage,
                    ..CaptureResult::default()
                },
                Ok(Err(err)) => CaptureResult {
                    warnings: vec![err],
                    skipped: true,
                    ..CaptureResult::default()
                },
                Err(_) => CaptureResult {
                    warnings: vec![format!(
                        "memento reflect timed out after {}ms; skipping reflect",
                        self.settings.capture_timeout_ms
                    )],
                    skipped: true,
                    ..CaptureResult::default()
                },
            }
        })
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use crate::services::provider::ProviderKind;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn restore_env(name: &str, previous: Option<std::ffi::OsString>) {
        match previous {
            Some(value) => unsafe { std::env::set_var(name, value) },
            None => unsafe { std::env::remove_var(name) },
        }
    }

    fn install_memento_runtime(
        base_url: &str,
        workspace: Option<&str>,
    ) -> (
        std::sync::MutexGuard<'static, ()>,
        tempfile::TempDir,
        Option<std::ffi::OsString>,
        Option<std::ffi::OsString>,
        Option<std::ffi::OsString>,
    ) {
        let guard = crate::services::discord::runtime_store::test_env_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        super::super::memento_throttle::reset_memento_throttle_for_tests();
        let temp = tempfile::tempdir().unwrap();
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let previous_key = std::env::var_os("MEMENTO_TEST_KEY");
        let previous_workspace = std::env::var_os("MEMENTO_WORKSPACE");
        let config_dir = temp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("agentdesk.yaml"),
            format!(
                "server:\n  port: 8791\nmemory:\n  backend: memento\n  mcp:\n    endpoint: {base_url}\n    access_key_env: MEMENTO_TEST_KEY\n"
            ),
        )
        .unwrap();
        unsafe {
            std::env::set_var("AGENTDESK_ROOT_DIR", temp.path());
            std::env::set_var("MEMENTO_TEST_KEY", "memento-key");
        }
        match workspace {
            Some(workspace) => unsafe { std::env::set_var("MEMENTO_WORKSPACE", workspace) },
            None => unsafe { std::env::remove_var("MEMENTO_WORKSPACE") },
        }
        (guard, temp, previous_root, previous_key, previous_workspace)
    }

    fn restore_memento_runtime(
        previous_root: Option<std::ffi::OsString>,
        previous_key: Option<std::ffi::OsString>,
        previous_workspace: Option<std::ffi::OsString>,
    ) {
        restore_env("AGENTDESK_ROOT_DIR", previous_root);
        restore_env("MEMENTO_TEST_KEY", previous_key);
        restore_env("MEMENTO_WORKSPACE", previous_workspace);
    }

    struct MockHttpResponse {
        status_line: &'static str,
        headers: Vec<(&'static str, &'static str)>,
        body: String,
    }

    async fn spawn_response_sequence_server(
        responses: Vec<MockHttpResponse>,
    ) -> (
        String,
        tokio::sync::oneshot::Receiver<Vec<String>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (requests_tx, requests_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let mut requests = Vec::new();
            for response in responses {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 32768];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                requests.push(String::from_utf8_lossy(&buf[..n]).to_string());

                let mut raw_response = format!(
                    "HTTP/1.1 {}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n",
                    response.status_line,
                    response.body.len()
                );
                for (header, value) in response.headers {
                    raw_response.push_str(&format!("{header}: {value}\r\n"));
                }
                raw_response.push_str("\r\n");
                raw_response.push_str(&response.body);

                let _ = stream.write_all(raw_response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
            let _ = requests_tx.send(requests);
        });
        (format!("http://{}", addr), requests_rx, handle)
    }

    async fn spawn_hanging_http_server() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        });
        (format!("http://{}", addr), handle)
    }

    #[test]
    fn recall_dedup_key_separates_dispatch_profile_and_mode() {
        let full = build_recall_dedup_key(
            "agentdesk",
            "project-agentdesk",
            "session-1",
            DispatchProfile::Full,
            RecallMode::Full,
            "same prompt",
        );
        let lite = build_recall_dedup_key(
            "agentdesk",
            "project-agentdesk",
            "session-1",
            DispatchProfile::Lite,
            RecallMode::IdentityOnly,
            "same prompt",
        );
        let lite_full = build_recall_dedup_key(
            "agentdesk",
            "project-agentdesk",
            "session-1",
            DispatchProfile::Lite,
            RecallMode::Full,
            "same prompt",
        );

        assert_ne!(full, lite);
        assert_ne!(lite, lite_full);
    }

    fn memento_settings() -> ResolvedMemorySettings {
        ResolvedMemorySettings {
            backend: crate::services::discord::settings::MemoryBackendKind::Memento,
            ..ResolvedMemorySettings::default()
        }
    }

    #[test]
    fn test_sanitize_workspace_segment_normalizes_role_ids() {
        assert_eq!(
            sanitize_memento_workspace_segment("Project-AgentDesk"),
            "project-agentdesk"
        );
        assert_eq!(
            sanitize_memento_workspace_segment("ch adk/cdx"),
            "ch-adk-cdx"
        );
        assert_eq!(sanitize_memento_workspace_segment("___"), "default");
    }

    #[test]
    fn test_format_context_payload_for_external_recall_returns_none_when_empty() {
        assert!(format_context_payload_for_external_recall(&json!({})).is_none());
    }

    #[test]
    fn context_formatter_puts_feedback_hint_next_to_search_event_id() {
        let payload = json!({
            "_meta": {
                "searchEventId": "search-1344"
            },
            "rankedInjection": {
                "items": [
                    {
                        "content": "Use tool feedback before ending the turn.",
                        "type": "procedure",
                        "score": 0.8
                    }
                ]
            }
        });

        let external = format_context_payload_for_external_recall(&payload).unwrap();

        assert!(external.contains("Memento searchEventId: search-1344"));
        assert!(external.contains(
            "tool_feedback(search_event_id=search-1344, relevant, sufficient) before turn end"
        ));
    }

    #[test]
    fn test_mcp_output_guard_preserves_threshold_boundary() {
        let exact = "x".repeat(MEMENTO_MODEL_OUTPUT_MAX_BYTES);
        let exact_guarded =
            guard_mcp_output_for_model(exact.clone(), MEMENTO_MODEL_OUTPUT_MAX_BYTES);
        assert_eq!(exact_guarded.text, exact);
        assert!(!exact_guarded.truncated);

        let oversized = format!("{}y", "x".repeat(MEMENTO_MODEL_OUTPUT_MAX_BYTES));
        let oversized_guarded =
            guard_mcp_output_for_model(oversized, MEMENTO_MODEL_OUTPUT_MAX_BYTES);
        assert!(oversized_guarded.truncated);
        assert!(oversized_guarded.text.len() <= MEMENTO_MODEL_OUTPUT_MAX_BYTES);
        assert!(
            oversized_guarded
                .text
                .contains("truncated memento MCP output")
        );
    }

    #[test]
    fn test_mcp_output_guard_truncates_on_utf8_boundary() {
        let guarded = guard_mcp_output_for_model("가나다라마바사".to_string(), 10);

        assert!(guarded.truncated);
        assert!(guarded.text.len() <= 10);
        assert!(std::str::from_utf8(guarded.text.as_bytes()).is_ok());
    }

    #[tokio::test]
    async fn test_memento_recall_calls_context_tool_over_mcp() {
        let context_content = serde_json::to_string(&json!({
            "success": true,
            "structured": true,
            "working": {
                "current_session": [
                    {
                        "content": "Replace placeholder Memento backend",
                        "type": "procedure"
                    }
                ]
            },
            "core": {
                "preferences": [],
                "errors": [],
                "decisions": [
                    {
                        "content": "Use /health for Memento health checks.",
                        "type": "decision",
                        "topic": "memento"
                    }
                ],
                "procedures": []
            },
            "rankedInjection": {
                "items": [
                    {
                        "content": "Finish #344",
                        "type": "procedure",
                        "score": 0.93
                    }
                ]
            },
            "_memento_hint": {
                "suggestion": "Remember to clear resolved errors."
            }
        }))
        .unwrap();
        let initialize_response = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": MEMENTO_PROTOCOL_VERSION
            }
        }))
        .unwrap();
        let tool_response = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "usage": {
                    "input_tokens": 123,
                    "output_tokens": 9
                },
                "content": [
                    {
                        "type": "text",
                        "text": context_content
                    }
                ],
                "isError": false
            }
        }))
        .unwrap();
        let (base_url, request_rx, handle) = spawn_response_sequence_server(vec![
            MockHttpResponse {
                status_line: "200 OK",
                headers: vec![("MCP-Session-Id", "session-123")],
                body: initialize_response,
            },
            MockHttpResponse {
                status_line: "200 OK",
                headers: vec![("MCP-Session-Id", "session-123")],
                body: tool_response,
            },
        ])
        .await;
        let (_guard, _temp, previous_root, previous_key, previous_workspace) =
            install_memento_runtime(&base_url, None);
        let backend = MementoBackend::new(memento_settings());

        let recall = backend
            .recall(RecallRequest {
                provider: ProviderKind::Codex,
                role_id: "project-agentdesk".to_string(),
                channel_id: 42,
                channel_name: None,
                session_id: "session-1".to_string(),
                dispatch_profile: DispatchProfile::Full,
                user_text: "What do we know about #344?".to_string(),
                mode: crate::services::memory::RecallMode::Full,
            })
            .await;

        let requests = request_rx.await.unwrap();
        handle.abort();
        restore_memento_runtime(previous_root, previous_key, previous_workspace);

        assert_eq!(requests.len(), 2);
        let init_request_lower = requests[0].to_lowercase();
        assert!(init_request_lower.contains("post /mcp"));
        assert!(requests[0].contains("\"method\":\"initialize\""));

        let tool_request_lower = requests[1].to_lowercase();
        assert!(tool_request_lower.contains("post /mcp"));
        assert!(tool_request_lower.contains("mcp-session-id: session-123"));
        assert!(requests[1].contains("\"method\":\"tools/call\""));
        assert!(requests[1].contains("\"name\":\"context\""));
        assert!(requests[1].contains("\"workspace\":\"agentdesk-project-agentdesk\""));
        assert!(requests[1].contains("\"agentId\":\"project-agentdesk\""));
        assert!(requests[1].contains("\"sessionId\":\"session-1\""));
        assert!(requests[1].contains("\"structured\":true"));
        let external_recall = recall.external_recall.unwrap_or_default();
        assert!(external_recall.contains("Finish #344"));
        assert!(external_recall.contains("Replace placeholder Memento backend"));
        assert!(external_recall.contains("Use /health for Memento health checks."));
        assert!(external_recall.contains("Remember to clear resolved errors."));
        assert!(recall.shared_knowledge.is_none());
        assert!(recall.longterm_catalog.is_none());
        assert!(recall.memento_context_loaded);
        assert!(recall.warnings.is_empty());
        assert_eq!(
            recall.token_usage,
            crate::services::memory::TokenUsage {
                input_tokens: 123,
                output_tokens: 9,
            }
        );
    }

    #[tokio::test]
    async fn test_memento_recall_dedups_identical_query_within_window() {
        let context_content = serde_json::to_string(&json!({
            "success": true,
            "structured": true,
            "rankedInjection": {
                "items": [
                    {
                        "content": "Reuse cached memento recall result",
                        "type": "procedure",
                        "score": 0.81
                    }
                ]
            }
        }))
        .unwrap();
        let initialize_response = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": MEMENTO_PROTOCOL_VERSION
            }
        }))
        .unwrap();
        let tool_response = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "usage": {
                    "input_tokens": 55,
                    "output_tokens": 7
                },
                "content": [
                    {
                        "type": "text",
                        "text": context_content
                    }
                ],
                "isError": false
            }
        }))
        .unwrap();
        let (base_url, request_rx, handle) = spawn_response_sequence_server(vec![
            MockHttpResponse {
                status_line: "200 OK",
                headers: vec![("MCP-Session-Id", "session-123")],
                body: initialize_response,
            },
            MockHttpResponse {
                status_line: "200 OK",
                headers: vec![("MCP-Session-Id", "session-123")],
                body: tool_response,
            },
        ])
        .await;
        let (_guard, _temp, previous_root, previous_key, previous_workspace) =
            install_memento_runtime(&base_url, None);
        let backend = MementoBackend::new(memento_settings());
        let request = RecallRequest {
            provider: ProviderKind::Codex,
            role_id: "project-agentdesk".to_string(),
            channel_id: 42,
            channel_name: None,
            session_id: "session-1".to_string(),
            dispatch_profile: DispatchProfile::Full,
            user_text: "What do we know about #344?".to_string(),
            mode: crate::services::memory::RecallMode::Full,
        };

        let first = backend.recall(request.clone()).await;
        let second = backend.recall(request).await;

        let requests = request_rx.await.unwrap();
        handle.abort();
        restore_memento_runtime(previous_root, previous_key, previous_workspace);

        assert_eq!(requests.len(), 2);
        assert_eq!(first.external_recall, second.external_recall);
        assert!(first.memento_context_loaded);
        assert!(!second.memento_context_loaded);
        assert_eq!(
            first.token_usage,
            crate::services::memory::TokenUsage {
                input_tokens: 55,
                output_tokens: 7,
            }
        );
        assert_eq!(
            second.token_usage,
            crate::services::memory::TokenUsage::default()
        );
    }

    #[tokio::test]
    async fn test_memento_recall_marks_context_loaded_even_when_payload_is_empty() {
        let context_content = serde_json::to_string(&json!({
            "success": true,
            "structured": true
        }))
        .unwrap();
        let initialize_response = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": MEMENTO_PROTOCOL_VERSION
            }
        }))
        .unwrap();
        let tool_response = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "usage": {
                    "input_tokens": 5,
                    "output_tokens": 1
                },
                "content": [
                    {
                        "type": "text",
                        "text": context_content
                    }
                ],
                "isError": false
            }
        }))
        .unwrap();
        let (base_url, request_rx, handle) = spawn_response_sequence_server(vec![
            MockHttpResponse {
                status_line: "200 OK",
                headers: vec![("MCP-Session-Id", "session-empty")],
                body: initialize_response,
            },
            MockHttpResponse {
                status_line: "200 OK",
                headers: vec![("MCP-Session-Id", "session-empty")],
                body: tool_response,
            },
        ])
        .await;
        let (_guard, _temp, previous_root, previous_key, previous_workspace) =
            install_memento_runtime(&base_url, None);
        let backend = MementoBackend::new(memento_settings());

        let recall = backend
            .recall(RecallRequest {
                provider: ProviderKind::Codex,
                role_id: "project-agentdesk".to_string(),
                channel_id: 42,
                channel_name: None,
                session_id: "session-1".to_string(),
                dispatch_profile: DispatchProfile::Full,
                user_text: "What is empty?".to_string(),
                mode: crate::services::memory::RecallMode::Full,
            })
            .await;

        let requests = request_rx.await.unwrap();
        handle.abort();
        restore_memento_runtime(previous_root, previous_key, previous_workspace);

        assert_eq!(requests.len(), 2);
        assert!(recall.external_recall.is_none());
        assert!(recall.memento_context_loaded);
        assert!(recall.warnings.is_empty());
    }

    #[tokio::test]
    async fn test_memento_recall_timeout_warns_and_falls_back_to_local() {
        let (base_url, handle) = spawn_hanging_http_server().await;
        let (_guard, _temp, previous_root, previous_key, previous_workspace) =
            install_memento_runtime(&base_url, None);
        let backend = MementoBackend::new(ResolvedMemorySettings {
            backend: crate::services::discord::settings::MemoryBackendKind::Memento,
            recall_timeout_ms: 25,
            ..ResolvedMemorySettings::default()
        });

        let recall = backend
            .recall(RecallRequest {
                provider: ProviderKind::Codex,
                role_id: "project-agentdesk".to_string(),
                channel_id: 42,
                channel_name: None,
                session_id: "session-1".to_string(),
                dispatch_profile: DispatchProfile::Full,
                user_text: "Need previous context".to_string(),
                mode: crate::services::memory::RecallMode::Full,
            })
            .await;

        handle.abort();
        restore_memento_runtime(previous_root, previous_key, previous_workspace);

        assert!(recall.external_recall.is_none());
        assert!(!recall.memento_context_loaded);
        assert!(
            recall
                .warnings
                .iter()
                .any(|warning| warning.contains("memento recall timed out"))
        );
    }

    #[tokio::test]
    async fn test_memento_review_lite_does_not_mark_context_loaded() {
        let backend = MementoBackend::new(memento_settings());

        let recall = backend
            .recall(RecallRequest {
                provider: ProviderKind::Codex,
                role_id: "project-agentdesk".to_string(),
                channel_id: 42,
                channel_name: None,
                session_id: "session-1".to_string(),
                dispatch_profile: DispatchProfile::ReviewLite,
                user_text: "Review this quickly".to_string(),
                mode: crate::services::memory::RecallMode::Full,
            })
            .await;

        assert!(recall.external_recall.is_none());
        assert!(!recall.memento_context_loaded);
        assert!(recall.warnings.is_empty());
    }

    // #1083: identity-only formatter keeps just the identity + active session
    // sections so default-turn recalls stay small.
    #[test]
    fn identity_only_formatter_keeps_identity_and_session_drops_memories() {
        let payload = json!({
            "identity": "AgentDesk Discord control plane",
            "working": {
                "current_session": [
                    {"content": "User asked about #1083", "topic": "1083"},
                ]
            },
            "memories": {
                "matches": [
                    {"content": "DROP ME — full context only", "type": "fact"},
                ]
            },
            "rankedInjection": {
                "items": [
                    {"content": "DROP ME — ranked memory", "type": "fact", "score": 0.9}
                ]
            },
            "core": {
                "decisions": [
                    {"content": "DROP ME — decision", "topic": "drop"}
                ]
            }
        });

        let identity = format_context_payload_for_identity_only(&payload).expect("payload");
        assert!(identity.contains("Identity Lite"));
        assert!(identity.contains("AgentDesk Discord control plane"));
        assert!(identity.contains("User asked about #1083"));
        assert!(!identity.contains("Ranked context from Memento"));
        assert!(!identity.contains("Relevant memories from Memento"));
        assert!(!identity.contains("Core memory from Memento"));

        let full = format_context_payload_for_external_recall(&payload).expect("full payload");
        assert!(full.contains("Ranked context from Memento") || full.contains("DROP ME"));
        // Identity-only payload must be smaller than full payload — proves the
        // throttling actually saves bytes.
        assert!(identity.len() < full.len());
    }

    #[test]
    fn identity_only_formatter_returns_none_when_payload_lacks_identity_and_session() {
        let payload = json!({
            "memories": {
                "matches": [
                    {"content": "Only memories here", "type": "fact"}
                ]
            }
        });
        assert!(format_context_payload_for_identity_only(&payload).is_none());
    }

    #[tokio::test]
    async fn test_memento_reflect_calls_reflect_tool_over_mcp() {
        let reflect_content = serde_json::to_string(&json!({
            "success": true,
            "count": 1
        }))
        .unwrap();
        let initialize_response = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": MEMENTO_PROTOCOL_VERSION
            }
        }))
        .unwrap();
        let tool_response = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "usage": {
                    "promptTokenCount": 33,
                    "completionTokenCount": 4
                },
                "content": [
                    {
                        "type": "text",
                        "text": reflect_content
                    }
                ],
                "isError": false
            }
        }))
        .unwrap();
        let (base_url, request_rx, handle) = spawn_response_sequence_server(vec![
            MockHttpResponse {
                status_line: "200 OK",
                headers: vec![("MCP-Session-Id", "session-456")],
                body: initialize_response,
            },
            MockHttpResponse {
                status_line: "200 OK",
                headers: vec![("MCP-Session-Id", "session-456")],
                body: tool_response,
            },
        ])
        .await;
        let (_guard, _temp, previous_root, previous_key, previous_workspace) =
            install_memento_runtime(&base_url, None);
        let backend = MementoBackend::new(memento_settings());

        let result = backend
            .reflect(ReflectRequest {
                provider: ProviderKind::Codex,
                role_id: "project-agentdesk".to_string(),
                channel_id: 42,
                channel_name: None,
                session_id: "session-1".to_string(),
                reason: super::super::SessionEndReason::IdleExpiry,
                transcript: "[User]: hi\n[Assistant]: hello".to_string(),
            })
            .await;

        let requests = request_rx.await.unwrap();
        handle.abort();
        restore_memento_runtime(previous_root, previous_key, previous_workspace);

        assert_eq!(requests.len(), 2);
        let tool_request_lower = requests[1].to_lowercase();
        assert!(tool_request_lower.contains("post /mcp"));
        assert!(tool_request_lower.contains("mcp-session-id: session-456"));
        assert!(requests[1].contains("\"method\":\"tools/call\""));
        assert!(requests[1].contains("\"name\":\"reflect\""));
        assert!(requests[1].contains("\"workspace\":\"agentdesk-project-agentdesk\""));
        assert!(requests[1].contains("\"agentId\":\"project-agentdesk\""));
        assert!(requests[1].contains("\"sessionId\":\"session-1\""));
        assert!(requests[1].contains("User: hi | Assistant: hello"));
        assert_eq!(
            result,
            CaptureResult {
                warnings: Vec::new(),
                skipped: false,
                token_usage: crate::services::memory::TokenUsage {
                    input_tokens: 33,
                    output_tokens: 4,
                },
            }
        );
    }

    #[tokio::test]
    async fn test_memento_remember_calls_remember_tool_over_mcp() {
        let remember_content = serde_json::to_string(&json!({
            "success": true,
            "id": "memory-episode-1",
            "usage": {
                "inputTokens": 21,
                "outputTokens": 3
            }
        }))
        .unwrap();
        let initialize_response = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": MEMENTO_PROTOCOL_VERSION
            }
        }))
        .unwrap();
        let tool_response = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "content": [
                    {
                        "type": "text",
                        "text": remember_content
                    }
                ],
                "isError": false
            }
        }))
        .unwrap();
        let (base_url, request_rx, handle) = spawn_response_sequence_server(vec![
            MockHttpResponse {
                status_line: "200 OK",
                headers: vec![("MCP-Session-Id", "session-789")],
                body: initialize_response,
            },
            MockHttpResponse {
                status_line: "200 OK",
                headers: vec![("MCP-Session-Id", "session-789")],
                body: tool_response,
            },
        ])
        .await;
        let (_guard, _temp, previous_root, previous_key, previous_workspace) =
            install_memento_runtime(&base_url, None);
        let backend = MementoBackend::new(memento_settings());

        let usage = backend
            .remember(MementoRememberRequest {
                content: "Issue #418 retrospective".to_string(),
                topic: "issue-418".to_string(),
                kind: "episode".to_string(),
                importance: Some(0.7),
                keywords: vec!["issue-418".to_string(), "success".to_string()],
                source: Some("card:card-418/dispatch:dispatch-418".to_string()),
                workspace: Some("agentdesk".to_string()),
                global: false,
                channel_id: None,
                channel_name: None,
                agent_id: Some("default".to_string()),
                case_id: Some("issue-418".to_string()),
                goal: Some("Record terminal card retrospective".to_string()),
                outcome: Some("resolved".to_string()),
                phase: Some("retrospective".to_string()),
                resolution_status: Some("resolved".to_string()),
                assertion_status: Some("verified".to_string()),
                context_summary: Some("Completed implementation card summary".to_string()),
            })
            .await
            .unwrap();

        let requests = request_rx.await.unwrap();
        handle.abort();
        restore_memento_runtime(previous_root, previous_key, previous_workspace);

        assert_eq!(requests.len(), 2);
        let tool_request_lower = requests[1].to_lowercase();
        assert!(tool_request_lower.contains("post /mcp"));
        assert!(tool_request_lower.contains("mcp-session-id: session-789"));
        assert!(requests[1].contains("\"method\":\"tools/call\""));
        assert!(requests[1].contains("\"name\":\"remember\""));
        assert!(requests[1].contains("\"content\":\"Issue #418 retrospective\""));
        assert!(requests[1].contains("\"topic\":\"issue-418\""));
        assert!(requests[1].contains("\"type\":\"episode\""));
        assert!(requests[1].contains("\"importance\":0.7"));
        assert!(requests[1].contains("\"workspace\":\"agentdesk\""));
        assert!(requests[1].contains("\"agentId\":\"default\""));
        assert!(requests[1].contains("\"caseId\":\"issue-418\""));
        assert!(requests[1].contains("\"resolutionStatus\":\"resolved\""));
        assert!(requests[1].contains("\"assertionStatus\":\"verified\""));
        assert!(
            requests[1].contains("\"contextSummary\":\"Completed implementation card summary\"")
        );
        assert_eq!(
            usage,
            crate::services::memory::TokenUsage {
                input_tokens: 21,
                output_tokens: 3,
            }
        );
    }

    #[tokio::test]
    async fn test_memento_remember_dedups_same_fact_when_importance_is_lower() {
        let remember_content = serde_json::to_string(&json!({
            "success": true,
            "id": "memory-fact-1",
            "usage": {
                "inputTokens": 11,
                "outputTokens": 2
            }
        }))
        .unwrap();
        let initialize_response = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": MEMENTO_PROTOCOL_VERSION
            }
        }))
        .unwrap();
        let tool_response = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "content": [
                    {
                        "type": "text",
                        "text": remember_content
                    }
                ],
                "isError": false
            }
        }))
        .unwrap();
        let (base_url, request_rx, handle) = spawn_response_sequence_server(vec![
            MockHttpResponse {
                status_line: "200 OK",
                headers: vec![("MCP-Session-Id", "session-789")],
                body: initialize_response,
            },
            MockHttpResponse {
                status_line: "200 OK",
                headers: vec![("MCP-Session-Id", "session-789")],
                body: tool_response,
            },
        ])
        .await;
        let (_guard, _temp, previous_root, previous_key, previous_workspace) =
            install_memento_runtime(&base_url, None);
        let backend = MementoBackend::new(memento_settings());
        let request = MementoRememberRequest {
            content: "ADK API release port is 8791".to_string(),
            topic: "agentdesk-config".to_string(),
            kind: "fact".to_string(),
            importance: Some(0.6),
            keywords: vec!["agentdesk".to_string(), "port".to_string()],
            source: Some("config".to_string()),
            workspace: Some("agentdesk".to_string()),
            global: false,
            channel_id: None,
            channel_name: None,
            agent_id: Some("default".to_string()),
            case_id: Some("issue-927".to_string()),
            goal: None,
            outcome: None,
            phase: Some("verification".to_string()),
            resolution_status: None,
            assertion_status: Some("verified".to_string()),
            context_summary: Some("Port confirmation".to_string()),
        };

        let first = backend.remember(request.clone()).await.unwrap();
        let second = backend
            .remember(MementoRememberRequest {
                importance: Some(0.5),
                ..request
            })
            .await
            .unwrap();

        let requests = request_rx.await.unwrap();
        handle.abort();
        restore_memento_runtime(previous_root, previous_key, previous_workspace);

        assert_eq!(requests.len(), 2);
        assert_eq!(
            first,
            crate::services::memory::TokenUsage {
                input_tokens: 11,
                output_tokens: 2,
            }
        );
        assert_eq!(second, crate::services::memory::TokenUsage::default());
    }

    #[tokio::test]
    async fn test_memento_tool_feedback_calls_tool_feedback_over_mcp() {
        let feedback_content = serde_json::to_string(&json!({
            "success": true,
            "usage": {
                "input_tokens": 8,
                "output_tokens": 2
            }
        }))
        .unwrap();
        let initialize_response = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": MEMENTO_PROTOCOL_VERSION
            }
        }))
        .unwrap();
        let tool_response = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "content": [
                    {
                        "type": "text",
                        "text": feedback_content
                    }
                ],
                "isError": false
            }
        }))
        .unwrap();
        let (base_url, request_rx, handle) = spawn_response_sequence_server(vec![
            MockHttpResponse {
                status_line: "200 OK",
                headers: vec![("MCP-Session-Id", "session-999")],
                body: initialize_response,
            },
            MockHttpResponse {
                status_line: "200 OK",
                headers: vec![("MCP-Session-Id", "session-999")],
                body: tool_response,
            },
        ])
        .await;
        let (_guard, _temp, previous_root, previous_key, previous_workspace) =
            install_memento_runtime(&base_url, None);
        let backend = MementoBackend::new(memento_settings());

        let usage = backend
            .tool_feedback(MementoToolFeedbackRequest {
                tool_name: "recall".to_string(),
                relevant: true,
                sufficient: false,
                session_id: Some("session-1".to_string()),
                search_event_id: Some("search-1".to_string()),
                fragment_ids: vec!["frag-1".to_string(), "frag-2".to_string()],
                suggestion: Some("Keep this search path".to_string()),
                context: Some("auto-generated after missing in-turn feedback".to_string()),
                trigger_type: Some("automatic".to_string()),
            })
            .await
            .unwrap();

        let requests = request_rx.await.unwrap();
        handle.abort();
        restore_memento_runtime(previous_root, previous_key, previous_workspace);

        assert_eq!(requests.len(), 2);
        let tool_request_lower = requests[1].to_lowercase();
        assert!(tool_request_lower.contains("post /mcp"));
        assert!(tool_request_lower.contains("mcp-session-id: session-999"));
        assert!(requests[1].contains("\"method\":\"tools/call\""));
        assert!(requests[1].contains("\"name\":\"tool_feedback\""));
        assert!(requests[1].contains("\"tool_name\":\"recall\""));
        assert!(requests[1].contains("\"relevant\":true"));
        assert!(requests[1].contains("\"sufficient\":false"));
        assert!(requests[1].contains("\"sessionId\":\"session-1\""));
        assert!(requests[1].contains("\"searchEventId\":\"search-1\""));
        assert!(requests[1].contains("\"fragmentIds\":[\"frag-1\",\"frag-2\"]"));
        assert!(requests[1].contains("\"triggerType\":\"automatic\""));
        assert_eq!(
            usage,
            crate::services::memory::TokenUsage {
                input_tokens: 8,
                output_tokens: 2,
            }
        );
    }

    #[tokio::test]
    async fn test_memento_tool_feedback_reinitializes_after_unauthorized_response() {
        let feedback_content = serde_json::to_string(&json!({
            "success": true,
            "usage": {
                "input_tokens": 5,
                "output_tokens": 1
            }
        }))
        .unwrap();
        let initialize_response = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": MEMENTO_PROTOCOL_VERSION
            }
        }))
        .unwrap();
        let tool_response = serde_json::to_string(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "content": [
                    {
                        "type": "text",
                        "text": feedback_content
                    }
                ],
                "isError": false
            }
        }))
        .unwrap();
        let (base_url, request_rx, handle) = spawn_response_sequence_server(vec![
            MockHttpResponse {
                status_line: "200 OK",
                headers: vec![("MCP-Session-Id", "session-initial")],
                body: initialize_response.clone(),
            },
            MockHttpResponse {
                status_line: "401 Unauthorized",
                headers: vec![("MCP-Session-Id", "session-initial")],
                body: "Unauthorized".to_string(),
            },
            MockHttpResponse {
                status_line: "200 OK",
                headers: vec![("MCP-Session-Id", "session-retry")],
                body: initialize_response,
            },
            MockHttpResponse {
                status_line: "200 OK",
                headers: vec![("MCP-Session-Id", "session-retry")],
                body: tool_response,
            },
        ])
        .await;
        let (_guard, _temp, previous_root, previous_key, previous_workspace) =
            install_memento_runtime(&base_url, None);
        let backend = MementoBackend::new(memento_settings());

        let usage = backend
            .tool_feedback(MementoToolFeedbackRequest {
                tool_name: "recall".to_string(),
                relevant: true,
                sufficient: true,
                session_id: Some("session-keep".to_string()),
                search_event_id: Some("search-1".to_string()),
                fragment_ids: vec!["frag-1".to_string()],
                suggestion: None,
                context: None,
                trigger_type: Some("automatic".to_string()),
            })
            .await
            .unwrap();

        let requests = request_rx.await.unwrap();
        handle.abort();
        restore_memento_runtime(previous_root, previous_key, previous_workspace);

        assert_eq!(requests.len(), 4);
        assert!(
            requests[1]
                .to_lowercase()
                .contains("mcp-session-id: session-initial")
        );
        assert!(
            requests[3]
                .to_lowercase()
                .contains("mcp-session-id: session-retry")
        );
        assert!(requests[3].contains("\"sessionId\":\"session-keep\""));
        assert_eq!(
            usage,
            crate::services::memory::TokenUsage {
                input_tokens: 5,
                output_tokens: 1,
            }
        );
    }

    #[tokio::test]
    async fn test_memento_capture_is_noop_by_design() {
        let backend = MementoBackend::new(memento_settings());
        let result = backend
            .capture(CaptureRequest {
                provider: ProviderKind::Codex,
                role_id: "project-agentdesk".to_string(),
                channel_id: 42,
                session_id: "session-1".to_string(),
                dispatch_id: None,
                user_text: "user".to_string(),
                assistant_text: "assistant".to_string(),
            })
            .await;

        assert!(result.skipped);
        assert!(result.warnings.is_empty());
    }
}
