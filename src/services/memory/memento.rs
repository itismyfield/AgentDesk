use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::{Map, Value, json};

use super::{
    CaptureRequest, CaptureResult, LocalMemoryBackend, MemoryBackend, MemoryFuture, RecallRequest,
    RecallResponse, ReflectRequest, TokenUsage, UNBOUND_MEMORY_ROLE_ID, extract_token_usage,
};
use crate::runtime_layout;
use crate::services::discord::DispatchProfile;
use crate::services::discord::settings::ResolvedMemorySettings;

const MEMENTO_MCP_PATH: &str = "/mcp";
const MEMENTO_PROTOCOL_VERSION: &str = "2025-11-25";
const MAX_WORKING_MEMORY_LINES: usize = 6;
const MAX_MEMORY_LINES: usize = 6;
const MAX_SKIP_LINES: usize = 4;

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
        config: &MementoRuntimeConfig,
    ) -> String {
        if let Some(workspace) = config.workspace_override.as_deref() {
            return workspace.to_string();
        }

        let role_id = role_id.trim();
        if role_id.is_empty() || role_id == UNBOUND_MEMORY_ROLE_ID {
            return format!("agentdesk-channel-{channel_id}");
        }

        format!("agentdesk-{}", sanitize_workspace_segment(role_id))
    }

    fn resolve_agent_id(&self, role_id: &str, channel_id: u64) -> String {
        let role_id = role_id.trim();
        if role_id.is_empty() || role_id == UNBOUND_MEMORY_ROLE_ID {
            return format!("agentdesk-channel-{channel_id}");
        }
        role_id.to_string()
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
                    "accessKey": config.access_key,
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
            return Err(format!("memento initialize failed with {status}: {text}"));
        }

        let payload: Value = serde_json::from_str(&text).map_err(|err| {
            format!("memento initialize response decode failed: {err}; body={text}")
        })?;
        if let Some(error) = payload.get("error") {
            return Err(format!(
                "memento initialize rpc failed: {}",
                render_rpc_error(error)
            ));
        }

        session_id.ok_or_else(|| {
            format!("memento initialize succeeded without MCP-Session-Id header; body={text}")
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
                if attempt == 0 && is_session_error(&text) {
                    self.clear_session_id(&config.endpoint);
                    session_id = self.initialize_session(config).await?;
                    continue;
                }
                return Err(format!("memento {tool_name} failed with {status}: {text}"));
            }

            let payload: Value = serde_json::from_str(&text).map_err(|err| {
                format!("memento {tool_name} response decode failed: {err}; body={text}")
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
        Ok(ContextFetchResult {
            external_recall: format_context_payload_for_external_recall(&result.payload),
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
        self.call_tool(config, "reflect", Value::Object(args))
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

fn mcp_url(endpoint: &str) -> String {
    format!(
        "{}{}",
        normalize_memento_endpoint(endpoint),
        MEMENTO_MCP_PATH
    )
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

fn is_session_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("session required")
        || message.contains("session not found")
        || message.contains("session expired")
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

fn sanitize_workspace_segment(value: &str) -> String {
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

fn format_context_payload_for_external_recall(payload: &Value) -> Option<String> {
    let mut sections = vec!["[External Recall]".to_string()];

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
        Some(sections.join("\n"))
    }
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
            let workspace = self.resolve_workspace(&request.role_id, request.channel_id, &config);

            match tokio::time::timeout(
                Duration::from_millis(self.settings.recall_timeout_ms),
                self.fetch_context(&request, &config, &workspace),
            )
            .await
            {
                Ok(Ok(result)) => RecallResponse {
                    external_recall: result.external_recall,
                    token_usage: result.token_usage,
                    ..RecallResponse::default()
                },
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
            let workspace = self.resolve_workspace(&request.role_id, request.channel_id, &config);

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

#[cfg(test)]
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
        let temp = tempfile::tempdir().unwrap();
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        let previous_key = std::env::var_os("MEMENTO_TEST_KEY");
        let previous_workspace = std::env::var_os("MEMENTO_WORKSPACE");
        let config_dir = temp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("memory-backend.json"),
            serde_json::to_string_pretty(&json!({
                "version": 2,
                "backend": "memento",
                "mcp": {
                    "endpoint": base_url,
                    "access_key_env": "MEMENTO_TEST_KEY"
                }
            }))
            .unwrap(),
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

    fn memento_settings() -> ResolvedMemorySettings {
        ResolvedMemorySettings {
            backend: crate::services::discord::settings::MemoryBackendKind::Memento,
            ..ResolvedMemorySettings::default()
        }
    }

    #[test]
    fn test_sanitize_workspace_segment_normalizes_role_ids() {
        assert_eq!(
            sanitize_workspace_segment("Project-AgentDesk"),
            "project-agentdesk"
        );
        assert_eq!(sanitize_workspace_segment("ch adk/cdx"), "ch-adk-cdx");
        assert_eq!(sanitize_workspace_segment("___"), "default");
    }

    #[test]
    fn test_format_context_payload_for_external_recall_returns_none_when_empty() {
        assert!(format_context_payload_for_external_recall(&json!({})).is_none());
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
                session_id: "session-1".to_string(),
                dispatch_profile: DispatchProfile::Full,
                user_text: "What do we know about #344?".to_string(),
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
                session_id: "session-1".to_string(),
                dispatch_profile: DispatchProfile::Full,
                user_text: "Need previous context".to_string(),
            })
            .await;

        handle.abort();
        restore_memento_runtime(previous_root, previous_key, previous_workspace);

        assert!(recall.external_recall.is_none());
        assert!(
            recall
                .warnings
                .iter()
                .any(|warning| warning.contains("memento recall timed out"))
        );
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
