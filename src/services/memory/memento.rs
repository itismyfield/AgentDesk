use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use poise::serenity_prelude::ChannelId;
use serde_json::{Map, Value, json};

use super::memento_writer_guard::{WriterClaim, writer_fingerprint};
use super::{
    CaptureRequest, CaptureResult, MemoryBackend, MemoryFuture, ReflectRequest, TokenUsage,
    UNBOUND_MEMORY_ROLE_ID, extract_token_usage,
    memento_throttle::{
        note_memento_dedup_hit, note_memento_remote_call, note_memento_tool_request,
    },
};
use crate::runtime_layout;
use crate::services::discord::settings::ResolvedMemorySettings;

const MEMENTO_PROTOCOL_VERSION: &str = "2025-11-25";
const MEMENTO_CONTEXT_FULL_TOKEN_BUDGET: u64 = 1_000;

#[path = "memento_anchor.rs"]
mod anchor;
pub(crate) use anchor::{SessionAnchorRequest, load_session_anchor_prompt};
#[path = "memento_endpoint.rs"]
mod endpoint;
#[path = "memento_scope.rs"]
mod scope;
#[cfg(test)]
#[path = "memento_scope_tests.rs"]
mod scope_tests;
#[path = "memento_transport.rs"]
mod transport;
use endpoint::{mcp_url, normalize_memento_endpoint};
pub(crate) use scope::{
    resolve_memento_agent_id, resolve_memento_workspace, resolve_memento_workspace_for_channel,
    sanitize_memento_workspace_segment,
};

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

#[derive(Clone)]
pub(crate) struct MementoBackend {
    client: reqwest::Client,
    settings: ResolvedMemorySettings,
    mcp_session: Arc<Mutex<Option<CachedMcpSession>>>,
}

impl MementoBackend {
    pub(crate) fn new(settings: ResolvedMemorySettings) -> Self {
        Self {
            client: reqwest::Client::new(),
            settings,
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
            config
                .workspace_override
                .as_deref()
                .or(self.settings.memory_group.as_deref()),
        )
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
        let agent_id = scope::resolve_remember_agent_id(
            agent_id,
            global,
            workspace.as_deref(),
            channel_id,
            channel_name.as_deref(),
        );
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
                            config
                                .workspace_override
                                .as_deref()
                                .or(self.settings.memory_group.as_deref()),
                        )
                    })
                    .or_else(|| config.workspace_override.clone())
            })
        };
        note_memento_tool_request("remember");

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

        self.call_tool(&config, "remember", Value::Object(args))
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

impl MemoryBackend for MementoBackend {
    fn capture<'a>(&'a self, request: CaptureRequest) -> MemoryFuture<'a, CaptureResult> {
        Box::pin(async move {
            // Raw turns belong to AgentDesk's transcript store, not durable
            // semantic memory. The previous writer exceeded Memento's 1,000
            // character episode limit and produced hard-cut `...` fragments.
            // Explicit model-owned `remember` calls remain available.
            let _ = request;
            CaptureResult {
                skipped: true,
                ..CaptureResult::default()
            }
        })
    }

    fn reflect<'a>(&'a self, request: ReflectRequest) -> MemoryFuture<'a, CaptureResult> {
        Box::pin(async move {
            // A transcript tail is not a self-contained fact. Passing the
            // former 320-character mechanical summary to `reflect` created a
            // 300+`...` fact and then a duplicate episode. Keep explicit MCP
            // reflect calls model-owned; the automatic backend sink is inert.
            let _ = request;
            CaptureResult {
                skipped: true,
                ..CaptureResult::default()
            }
        })
    }
}

#[cfg(test)]
mod backend_tests {
    use super::*;
    use crate::services::provider::ProviderKind;

    #[tokio::test]
    async fn automatic_turn_capture_and_transcript_reflect_never_reach_memento_writers() {
        let backend = MementoBackend::new(ResolvedMemorySettings {
            backend: crate::services::discord::settings::MemoryBackendKind::Memento,
            ..ResolvedMemorySettings::default()
        });

        // These payloads cross both truncation boundaries that previously
        // yielded 1,003-character episodes and 303-character reflect rows.
        let capture = backend
            .capture(CaptureRequest {
                provider: ProviderKind::Codex,
                role_id: "project-agentdesk".to_string(),
                channel_id: 42,
                session_id: "provider-session".to_string(),
                dispatch_id: None,
                user_text: "u".repeat(2_500),
                assistant_text: "a".repeat(2_500),
            })
            .await;
        let reflect = backend
            .reflect(ReflectRequest {
                provider: ProviderKind::Codex,
                role_id: "project-agentdesk".to_string(),
                channel_id: 42,
                channel_name: Some("agentdesk".to_string()),
                session_id: "provider-session".to_string(),
                reason: crate::services::memory::SessionEndReason::IdleExpiry,
                transcript: "[Assistant]: progress update\n".repeat(40),
            })
            .await;

        for result in [capture, reflect] {
            assert!(result.skipped, "automatic persistence must stay disabled");
            assert!(
                result.warnings.is_empty(),
                "policy skip is not a transport/runtime failure"
            );
            assert!(
                result.token_usage.is_zero(),
                "a skipped writer must not report an MCP call"
            );
        }
    }
}

#[cfg(test)]
#[path = "memento_writer_tests.rs"]
mod writer_tests;
