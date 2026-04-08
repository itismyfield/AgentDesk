use std::time::Duration;

use reqwest::StatusCode;
use serde_json::{Map, Value, json};

use super::{
    CaptureRequest, CaptureResult, LocalMemoryBackend, MemoryBackend, MemoryFuture, RecallRequest,
    RecallResponse, TokenUsage, extract_token_usage,
};
use crate::services::discord::DispatchProfile;
use crate::services::discord::settings::ResolvedMemorySettings;

const MEM0_ADD_PATH: &str = "/v1/memories/";
const MEM0_SEARCH_PATH: &str = "/v2/memories/search";
const MEM0_SYNTHETIC_USER_ID: &str = "agentdesk";
const DEFAULT_MEM0_UNIQUE_LIMIT: usize = 5;
const STRICT_MEM0_UNIQUE_LIMIT: usize = 3;
const MAX_RELATION_LINES: usize = 10;

#[derive(Clone)]
struct Mem0RuntimeConfig {
    api_key: String,
    base_url: String,
    org_id: Option<String>,
    project_id: Option<String>,
}

#[derive(Clone, Copy)]
struct Mem0ProfilePolicy {
    threshold: f64,
    top_k: u64,
    unique_limit: usize,
}

struct ExternalRecallResult {
    content: Option<String>,
    token_usage: TokenUsage,
}

#[derive(Clone)]
pub(crate) struct Mem0Backend {
    client: reqwest::Client,
    settings: ResolvedMemorySettings,
    local: LocalMemoryBackend,
}

impl Mem0Backend {
    pub(crate) fn new(settings: ResolvedMemorySettings) -> Self {
        Self {
            client: reqwest::Client::new(),
            settings,
            local: LocalMemoryBackend,
        }
    }

    fn runtime_config(&self) -> Result<Mem0RuntimeConfig, String> {
        let api_key = std::env::var("MEM0_API_KEY")
            .map_err(|_| "MEM0_API_KEY is not set; skipping mem0 backend".to_string())?;
        let base_url = std::env::var("MEM0_BASE_URL")
            .map_err(|_| "MEM0_BASE_URL is not set; skipping mem0 backend".to_string())?;
        Ok(Mem0RuntimeConfig {
            api_key,
            base_url: base_url.trim_end_matches('/').to_string(),
            org_id: std::env::var("MEM0_ORG_ID")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            project_id: std::env::var("MEM0_PROJECT_ID")
                .ok()
                .filter(|value| !value.trim().is_empty()),
        })
    }

    fn profile_policy(&self) -> Mem0ProfilePolicy {
        match self.settings.mem0.profile.as_str() {
            "strict" => Mem0ProfilePolicy {
                threshold: 0.55,
                top_k: 6,
                unique_limit: STRICT_MEM0_UNIQUE_LIMIT,
            },
            _ => Mem0ProfilePolicy {
                threshold: 0.3,
                top_k: 10,
                unique_limit: DEFAULT_MEM0_UNIQUE_LIMIT,
            },
        }
    }

    fn auth_request(
        &self,
        builder: reqwest::RequestBuilder,
        config: &Mem0RuntimeConfig,
    ) -> reqwest::RequestBuilder {
        builder
            .header("Authorization", format!("Token {}", config.api_key))
            .header("Accept", "application/json")
    }

    fn append_scope_fields(&self, body: &mut Map<String, Value>, config: &Mem0RuntimeConfig) {
        if let Some(org_id) = &config.org_id {
            body.insert("org_id".to_string(), json!(org_id));
        }
        if let Some(project_id) = &config.project_id {
            body.insert("project_id".to_string(), json!(project_id));
        }
    }

    fn build_capture_body(&self, request: &CaptureRequest, config: &Mem0RuntimeConfig) -> Value {
        let mut metadata = Map::new();
        metadata.insert("source".to_string(), json!("agentdesk"));
        metadata.insert("provider".to_string(), json!(request.provider.as_str()));
        metadata.insert(
            "channel_id".to_string(),
            json!(request.channel_id.to_string()),
        );
        metadata.insert(
            "memory_profile".to_string(),
            json!(self.settings.mem0.profile.as_str()),
        );
        if let Some(dispatch_id) = &request.dispatch_id {
            metadata.insert("dispatch_id".to_string(), json!(dispatch_id));
        }
        if let Some(confidence) = self.settings.mem0.ingestion.confidence_threshold {
            metadata.insert("confidence_threshold".to_string(), json!(confidence));
        }

        let mut body = Map::new();
        body.insert(
            "messages".to_string(),
            json!([
                { "role": "user", "content": request.user_text },
                { "role": "assistant", "content": request.assistant_text }
            ]),
        );
        body.insert("user_id".to_string(), json!(MEM0_SYNTHETIC_USER_ID));
        body.insert("agent_id".to_string(), json!(request.role_id));
        body.insert("run_id".to_string(), json!(request.session_id));
        body.insert("metadata".to_string(), Value::Object(metadata));
        body.insert("version".to_string(), json!("v2"));
        body.insert("output_format".to_string(), json!("v1.1"));
        body.insert("async_mode".to_string(), json!(true));
        if let Some(infer) = self.settings.mem0.ingestion.infer {
            body.insert("infer".to_string(), json!(infer));
        }
        if let Some(custom_instructions) = &self.settings.mem0.ingestion.custom_instructions {
            body.insert(
                "custom_instructions".to_string(),
                json!(custom_instructions),
            );
        }
        self.append_scope_fields(&mut body, config);
        Value::Object(body)
    }

    fn build_search_body(&self, request: &RecallRequest, config: &Mem0RuntimeConfig) -> Value {
        let policy = self.profile_policy();
        let mut body = Map::new();
        body.insert("query".to_string(), json!(request.user_text));
        body.insert("user_id".to_string(), json!(MEM0_SYNTHETIC_USER_ID));
        body.insert("limit".to_string(), json!(policy.top_k));
        body.insert(
            "filters".to_string(),
            json!({
                "AND": [
                    { "agent_id": request.role_id },
                    { "run_id": request.session_id }
                ]
            }),
        );
        body.insert("version".to_string(), json!("v2"));
        body.insert("top_k".to_string(), json!(policy.top_k));
        body.insert("threshold".to_string(), json!(policy.threshold));
        body.insert(
            "fields".to_string(),
            json!(["memory", "metadata", "categories", "relations"]),
        );
        self.append_scope_fields(&mut body, config);
        Value::Object(body)
    }

    async fn search_external_recall(
        &self,
        request: &RecallRequest,
    ) -> Result<ExternalRecallResult, String> {
        let config = self.runtime_config()?;
        let url = format!("{}{}", config.base_url, MEM0_SEARCH_PATH);
        let body = self.build_search_body(request, &config);
        let response = self
            .auth_request(self.client.post(url), &config)
            .json(&body)
            .send()
            .await
            .map_err(|err| format!("mem0 search request failed: {err}"))?;

        if response.status() != StatusCode::OK {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("mem0 search failed with {status}: {body}"));
        }

        let text = response
            .text()
            .await
            .map_err(|err| format!("mem0 search response read failed: {err}"))?;
        let payload: Value = serde_json::from_str(&text)
            .map_err(|err| format!("mem0 search response decode failed: {err}; body={text}"))?;
        Ok(ExternalRecallResult {
            content: format_search_payload_for_external_recall(
                &payload,
                self.profile_policy().unique_limit,
            ),
            token_usage: extract_token_usage(&payload).unwrap_or_default(),
        })
    }

    async fn add_capture(&self, request: &CaptureRequest) -> Result<CaptureResult, String> {
        let config = self.runtime_config()?;
        let url = format!("{}{}", config.base_url, MEM0_ADD_PATH);
        let body = self.build_capture_body(request, &config);
        let response = self
            .auth_request(self.client.post(url), &config)
            .json(&body)
            .send()
            .await
            .map_err(|err| format!("mem0 add request failed: {err}"))?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("mem0 add failed with {status}: {body}"));
        }

        let body = response.text().await.unwrap_or_default();
        let token_usage = if body.trim().is_empty() {
            TokenUsage::default()
        } else {
            serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|payload| extract_token_usage(&payload))
                .unwrap_or_default()
        };

        Ok(CaptureResult {
            token_usage,
            ..CaptureResult::default()
        })
    }
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn extract_result_items<'a>(payload: &'a Value) -> Vec<&'a Map<String, Value>> {
    match payload {
        Value::Array(items) => items.iter().filter_map(Value::as_object).collect(),
        Value::Object(map) => {
            for key in ["results", "data", "items", "memories"] {
                if let Some(items) = map.get(key).and_then(Value::as_array) {
                    return items.iter().filter_map(Value::as_object).collect();
                }
            }
            if let Some(response) = map.get("response").and_then(Value::as_object) {
                for key in ["results", "data", "items", "memories"] {
                    if let Some(items) = response.get(key).and_then(Value::as_array) {
                        return items.iter().filter_map(Value::as_object).collect();
                    }
                }
            }
            if ["id", "memory", "text", "content", "summary"]
                .iter()
                .any(|key| map.contains_key(*key))
            {
                return vec![map];
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

fn extract_relation_items<'a>(payload: &'a Value) -> Vec<&'a Map<String, Value>> {
    let Some(map) = payload.as_object() else {
        return Vec::new();
    };

    if let Some(items) = map.get("relations").and_then(Value::as_array) {
        return items.iter().filter_map(Value::as_object).collect();
    }

    map.get("response")
        .and_then(Value::as_object)
        .and_then(|response| response.get("relations"))
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_object).collect())
        .unwrap_or_default()
}

fn extract_text(item: &Map<String, Value>) -> Option<String> {
    for key in ["memory", "text", "content", "summary"] {
        if let Some(value) = item.get(key).and_then(Value::as_str) {
            return Some(value.to_string());
        }
    }
    item.get("data")
        .and_then(Value::as_object)
        .and_then(extract_text)
}

fn format_memory_line(item: &Map<String, Value>) -> Option<(String, String)> {
    let memory = normalize_whitespace(&extract_text(item)?);
    if memory.is_empty() {
        return None;
    }

    let mut line = memory.clone();
    if let Some(categories) = item
        .get("categories")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|items| !items.is_empty())
    {
        line.push_str(&format!(" [categories: {}]", categories.join(", ")));
    }
    if let Some(source) = item
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get("source"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        line.push_str(&format!(" [source: {source}]"));
    }

    Some((memory, line))
}

fn format_relation_line(item: &Map<String, Value>) -> Option<String> {
    let source = item
        .get("source")
        .or_else(|| item.get("from"))
        .and_then(Value::as_str)
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty())?;
    let relationship = item
        .get("relationship")
        .or_else(|| item.get("relation"))
        .and_then(Value::as_str)
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty())?;
    let destination = item
        .get("destination")
        .or_else(|| item.get("target"))
        .or_else(|| item.get("to"))
        .and_then(Value::as_str)
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty())?;
    Some(format!("{source} -- {relationship} -- {destination}"))
}

fn dedup_lines<I>(items: I, limit: usize) -> Vec<String>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut seen = std::collections::HashSet::new();
    let mut lines = Vec::new();
    for (dedup_key, rendered) in items {
        let normalized_key = normalize_whitespace(&dedup_key);
        if normalized_key.is_empty() || !seen.insert(normalized_key) {
            continue;
        }
        lines.push(rendered);
        if lines.len() >= limit {
            break;
        }
    }
    lines
}

fn format_external_recall(memory_lines: &[String], relation_lines: &[String]) -> Option<String> {
    if memory_lines.is_empty() && relation_lines.is_empty() {
        return None;
    }

    let mut sections = vec!["[External Recall]".to_string()];
    if !memory_lines.is_empty() {
        sections.push(format!(
            "Relevant memories from Mem0 for this session:\n- {}",
            memory_lines.join("\n- ")
        ));
    }
    if !relation_lines.is_empty() {
        sections.push(format!(
            "Relevant graph relations from Mem0 for this session:\n- {}",
            relation_lines.join("\n- ")
        ));
    }

    Some(sections.join("\n"))
}

fn format_search_payload_for_external_recall(
    payload: &Value,
    unique_limit: usize,
) -> Option<String> {
    let memory_lines = dedup_lines(
        extract_result_items(payload)
            .into_iter()
            .filter_map(format_memory_line),
        unique_limit,
    );
    let relation_lines = dedup_lines(
        extract_relation_items(payload)
            .into_iter()
            .filter_map(|item| format_relation_line(item).map(|line| (line.clone(), line))),
        MAX_RELATION_LINES,
    );

    format_external_recall(&memory_lines, &relation_lines)
}

impl MemoryBackend for Mem0Backend {
    fn recall<'a>(&'a self, request: RecallRequest) -> MemoryFuture<'a, RecallResponse> {
        Box::pin(async move {
            let mut response = self.local.recall(request.clone()).await;
            if request.dispatch_profile == DispatchProfile::ReviewLite {
                return response;
            }

            match tokio::time::timeout(
                Duration::from_millis(self.settings.recall_timeout_ms),
                self.search_external_recall(&request),
            )
            .await
            {
                Ok(Ok(external)) => {
                    response.external_recall = external.content;
                    response
                        .token_usage
                        .saturating_add_assign(external.token_usage);
                }
                Ok(Err(err)) => response.warnings.push(err),
                Err(_) => response.warnings.push(format!(
                    "mem0 recall timed out after {}ms; falling back to local memory",
                    self.settings.recall_timeout_ms
                )),
            }

            response
        })
    }

    fn capture<'a>(&'a self, request: CaptureRequest) -> MemoryFuture<'a, CaptureResult> {
        Box::pin(async move {
            if request.user_text.trim().is_empty() || request.assistant_text.trim().is_empty() {
                return CaptureResult {
                    warnings: vec![
                        "mem0 capture skipped because turn content is empty".to_string(),
                    ],
                    skipped: true,
                    ..CaptureResult::default()
                };
            }

            match tokio::time::timeout(
                Duration::from_millis(self.settings.capture_timeout_ms),
                self.add_capture(&request),
            )
            .await
            {
                Ok(Ok(result)) => result,
                Ok(Err(err)) => CaptureResult {
                    warnings: vec![err],
                    skipped: true,
                    ..CaptureResult::default()
                },
                Err(_) => CaptureResult {
                    warnings: vec![format!(
                        "mem0 capture timed out after {}ms; skipping capture",
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
    use crate::services::discord::settings::{MemoryBackendKind, ResolvedMemorySettings};
    use crate::services::provider::ProviderKind;
    use std::time::Duration;

    async fn spawn_hanging_http_server() -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::AsyncReadExt;
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

    async fn spawn_fixed_response_server(
        status_line: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (format!("http://{}", addr), handle)
    }

    fn clear_mem0_env() -> (
        std::sync::MutexGuard<'static, ()>,
        Option<std::ffi::OsString>,
        Option<std::ffi::OsString>,
    ) {
        let guard = crate::services::discord::runtime_store::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev_api_key = std::env::var_os("MEM0_API_KEY");
        let prev_base_url = std::env::var_os("MEM0_BASE_URL");
        unsafe {
            std::env::remove_var("MEM0_API_KEY");
            std::env::remove_var("MEM0_BASE_URL");
        }
        (guard, prev_api_key, prev_base_url)
    }

    fn restore_mem0_env(
        prev_api_key: Option<std::ffi::OsString>,
        prev_base_url: Option<std::ffi::OsString>,
    ) {
        match prev_api_key {
            Some(value) => unsafe { std::env::set_var("MEM0_API_KEY", value) },
            None => unsafe { std::env::remove_var("MEM0_API_KEY") },
        }
        match prev_base_url {
            Some(value) => unsafe { std::env::set_var("MEM0_BASE_URL", value) },
            None => unsafe { std::env::remove_var("MEM0_BASE_URL") },
        }
    }

    fn set_mem0_env(
        base_url: &str,
    ) -> (
        std::sync::MutexGuard<'static, ()>,
        Option<std::ffi::OsString>,
        Option<std::ffi::OsString>,
    ) {
        let guard = crate::services::discord::runtime_store::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev_api_key = std::env::var_os("MEM0_API_KEY");
        let prev_base_url = std::env::var_os("MEM0_BASE_URL");
        unsafe {
            std::env::set_var("MEM0_API_KEY", "test-key");
            std::env::set_var("MEM0_BASE_URL", base_url);
        }
        (guard, prev_api_key, prev_base_url)
    }

    fn mem0_settings() -> ResolvedMemorySettings {
        ResolvedMemorySettings {
            backend: MemoryBackendKind::Mem0,
            ..ResolvedMemorySettings::default()
        }
    }

    #[test]
    fn test_format_external_recall_returns_none_for_empty_results() {
        assert!(format_external_recall(&[], &[]).is_none());
    }

    #[test]
    fn test_build_search_and_capture_body_include_scope_fields() {
        let backend = Mem0Backend::new(mem0_settings());
        let config = Mem0RuntimeConfig {
            api_key: "key".to_string(),
            base_url: "http://localhost:8080".to_string(),
            org_id: Some("org".to_string()),
            project_id: Some("project".to_string()),
        };
        let search = backend.build_search_body(
            &RecallRequest {
                provider: ProviderKind::Codex,
                role_id: "codex".to_string(),
                channel_id: 1,
                session_id: "run-1".to_string(),
                dispatch_profile: DispatchProfile::Full,
                user_text: "What do you know?".to_string(),
            },
            &config,
        );
        assert_eq!(search["user_id"], MEM0_SYNTHETIC_USER_ID);
        assert_eq!(search["limit"], 10);
        assert_eq!(search["top_k"], 10);
        assert_eq!(search["filters"]["AND"][0]["agent_id"], "codex");
        assert_eq!(search["filters"]["AND"][1]["run_id"], "run-1");
        assert_eq!(
            search["fields"],
            json!(["memory", "metadata", "categories", "relations"])
        );
        assert_eq!(search["org_id"], "org");
        assert_eq!(search["project_id"], "project");

        let capture = backend.build_capture_body(
            &CaptureRequest {
                provider: ProviderKind::Codex,
                role_id: "codex".to_string(),
                channel_id: 9,
                session_id: "run-9".to_string(),
                dispatch_id: Some("dispatch-1".to_string()),
                user_text: "hi".to_string(),
                assistant_text: "hello".to_string(),
            },
            &config,
        );
        assert_eq!(capture["user_id"], MEM0_SYNTHETIC_USER_ID);
        assert_eq!(capture["agent_id"], "codex");
        assert_eq!(capture["run_id"], "run-9");
        assert_eq!(capture["metadata"]["channel_id"], "9");
        assert_eq!(capture["metadata"]["dispatch_id"], "dispatch-1");
    }

    #[test]
    fn test_build_search_body_uses_strict_profile_overfetch_policy() {
        let backend = Mem0Backend::new(ResolvedMemorySettings {
            backend: MemoryBackendKind::Mem0,
            mem0: crate::services::discord::settings::Mem0ResolvedSettings {
                profile: "strict".to_string(),
                ..Default::default()
            },
            ..ResolvedMemorySettings::default()
        });
        let config = Mem0RuntimeConfig {
            api_key: "key".to_string(),
            base_url: "http://localhost:8080".to_string(),
            org_id: None,
            project_id: None,
        };
        let search = backend.build_search_body(
            &RecallRequest {
                provider: ProviderKind::Codex,
                role_id: "critic".to_string(),
                channel_id: 7,
                session_id: "run-7".to_string(),
                dispatch_profile: DispatchProfile::Full,
                user_text: "What graph database does Critic use?".to_string(),
            },
            &config,
        );

        assert_eq!(search["limit"], 6);
        assert_eq!(search["top_k"], 6);
        assert_eq!(search["user_id"], MEM0_SYNTHETIC_USER_ID);
    }

    #[test]
    fn test_format_search_payload_dedups_memories_and_relations() {
        let payload = json!({
            "results": [
                {
                    "memory": "AgentDesk   uses   Neo4j",
                    "categories": ["graph"],
                    "metadata": {"source": "agentdesk"}
                },
                {
                    "memory": "AgentDesk uses Neo4j",
                    "categories": ["graph"],
                    "metadata": {"source": "agentdesk"}
                },
                {
                    "memory": "Critic uses FalkorDB",
                    "metadata": {"source": "agentdesk"}
                }
            ],
            "relations": [
                {
                    "source": "agentdesk",
                    "relationship": "uses",
                    "destination": "neo4j"
                },
                {
                    "from": "agentdesk",
                    "relation": "uses",
                    "to": "neo4j"
                },
                {
                    "source": "critic",
                    "relationship": "uses",
                    "destination": "falkordb"
                }
            ]
        });

        let formatted = format_search_payload_for_external_recall(&payload, 5)
            .expect("formatted recall should exist");
        assert!(formatted.contains("Relevant memories from Mem0 for this session:"));
        assert!(formatted.contains("Relevant graph relations from Mem0 for this session:"));
        assert_eq!(formatted.matches("AgentDesk uses Neo4j").count(), 1);
        assert_eq!(formatted.matches("agentdesk -- uses -- neo4j").count(), 1);
        assert!(formatted.contains("critic -- uses -- falkordb"));
    }

    #[test]
    fn test_format_search_payload_reads_wrapped_relations() {
        let payload = json!({
            "response": {
                "results": [
                    {
                        "memory": "Planner uses Neo4j for architecture notes."
                    }
                ],
                "relations": [
                    {
                        "source": "planner",
                        "relationship": "uses",
                        "destination": "neo4j"
                    }
                ]
            }
        });

        let formatted = format_search_payload_for_external_recall(&payload, 5)
            .expect("formatted wrapped recall should exist");
        assert!(formatted.contains("Planner uses Neo4j for architecture notes."));
        assert!(formatted.contains("planner -- uses -- neo4j"));
    }

    #[tokio::test]
    async fn test_mem0_recall_falls_back_to_local_when_env_missing() {
        let (_guard, prev_api_key, prev_base_url) = clear_mem0_env();
        let backend = Mem0Backend::new(mem0_settings());
        let response = backend
            .recall(RecallRequest {
                provider: ProviderKind::Codex,
                role_id: "codex".to_string(),
                channel_id: 1,
                session_id: "run-1".to_string(),
                dispatch_profile: DispatchProfile::Full,
                user_text: "hi".to_string(),
            })
            .await;
        restore_mem0_env(prev_api_key, prev_base_url);
        assert!(
            response
                .warnings
                .iter()
                .any(|warning| warning.contains("MEM0_API_KEY"))
        );
    }

    #[tokio::test]
    async fn test_mem0_capture_skips_when_env_missing() {
        let (_guard, prev_api_key, prev_base_url) = clear_mem0_env();
        let backend = Mem0Backend::new(mem0_settings());
        let result = backend
            .capture(CaptureRequest {
                provider: ProviderKind::Codex,
                role_id: "codex".to_string(),
                channel_id: 1,
                session_id: "run-1".to_string(),
                dispatch_id: None,
                user_text: "user".to_string(),
                assistant_text: "assistant".to_string(),
            })
            .await;
        restore_mem0_env(prev_api_key, prev_base_url);
        assert!(result.skipped);
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("MEM0_API_KEY"))
        );
    }

    #[tokio::test]
    async fn test_mem0_recall_skips_external_lookup_for_review_lite() {
        let backend = Mem0Backend::new(mem0_settings());
        let response = backend
            .recall(RecallRequest {
                provider: ProviderKind::Codex,
                role_id: "codex".to_string(),
                channel_id: 1,
                session_id: "run-1".to_string(),
                dispatch_profile: DispatchProfile::ReviewLite,
                user_text: "hi".to_string(),
            })
            .await;
        assert!(response.external_recall.is_none());
        assert!(response.warnings.is_empty());
    }

    #[tokio::test]
    async fn test_mem0_recall_timeout_warns_and_keeps_local_result() {
        let (base_url, server_handle) = spawn_hanging_http_server().await;
        let (_guard, prev_api_key, prev_base_url) = set_mem0_env(&base_url);
        let backend = Mem0Backend::new(ResolvedMemorySettings {
            backend: MemoryBackendKind::Mem0,
            recall_timeout_ms: 25,
            ..ResolvedMemorySettings::default()
        });

        let response = backend
            .recall(RecallRequest {
                provider: ProviderKind::Codex,
                role_id: "codex".to_string(),
                channel_id: 1,
                session_id: "run-1".to_string(),
                dispatch_profile: DispatchProfile::Full,
                user_text: "hi".to_string(),
            })
            .await;

        server_handle.abort();
        restore_mem0_env(prev_api_key, prev_base_url);

        assert!(response.external_recall.is_none());
        assert!(
            response
                .warnings
                .iter()
                .any(|warning| warning.contains("timed out"))
        );
    }

    #[tokio::test]
    async fn test_mem0_capture_timeout_skips_with_warning() {
        let (base_url, server_handle) = spawn_hanging_http_server().await;
        let (_guard, prev_api_key, prev_base_url) = set_mem0_env(&base_url);
        let backend = Mem0Backend::new(ResolvedMemorySettings {
            backend: MemoryBackendKind::Mem0,
            capture_timeout_ms: 25,
            ..ResolvedMemorySettings::default()
        });

        let result = backend
            .capture(CaptureRequest {
                provider: ProviderKind::Codex,
                role_id: "codex".to_string(),
                channel_id: 1,
                session_id: "run-1".to_string(),
                dispatch_id: None,
                user_text: "user".to_string(),
                assistant_text: "assistant".to_string(),
            })
            .await;

        server_handle.abort();
        restore_mem0_env(prev_api_key, prev_base_url);

        assert!(result.skipped);
        assert!(
            result
                .warnings
                .iter()
                .any(|warning| warning.contains("timed out"))
        );
    }

    #[tokio::test]
    async fn test_mem0_capture_accepts_202_success() {
        let (base_url, server_handle) = spawn_fixed_response_server(
            "202 Accepted",
            r#"{"usage":{"inputTokens":8,"outputTokens":2}}"#,
        )
        .await;
        let (_guard, prev_api_key, prev_base_url) = set_mem0_env(&base_url);
        let backend = Mem0Backend::new(mem0_settings());

        let result = backend
            .capture(CaptureRequest {
                provider: ProviderKind::Codex,
                role_id: "codex".to_string(),
                channel_id: 1,
                session_id: "run-1".to_string(),
                dispatch_id: None,
                user_text: "user".to_string(),
                assistant_text: "assistant".to_string(),
            })
            .await;

        server_handle.abort();
        restore_mem0_env(prev_api_key, prev_base_url);

        assert!(!result.skipped);
        assert!(result.warnings.is_empty());
        assert_eq!(
            result.token_usage,
            TokenUsage {
                input_tokens: 8,
                output_tokens: 2,
            }
        );
    }

    #[tokio::test]
    async fn test_mem0_recall_http_error_warns_and_falls_back() {
        let (base_url, server_handle) =
            spawn_fixed_response_server("500 Internal Server Error", "{\"error\":\"boom\"}").await;
        let (_guard, prev_api_key, prev_base_url) = set_mem0_env(&base_url);
        let backend = Mem0Backend::new(mem0_settings());

        let response = backend
            .recall(RecallRequest {
                provider: ProviderKind::Codex,
                role_id: "codex".to_string(),
                channel_id: 1,
                session_id: "run-1".to_string(),
                dispatch_profile: DispatchProfile::Full,
                user_text: "hi".to_string(),
            })
            .await;

        server_handle.abort();
        restore_mem0_env(prev_api_key, prev_base_url);

        assert!(response.external_recall.is_none());
        assert!(
            response
                .warnings
                .iter()
                .any(|warning| warning.contains("mem0 search failed with 500"))
        );
    }

    #[tokio::test]
    async fn test_mem0_recall_formats_wrapped_relations_payload() {
        let body = r#"{
            "results": [
                {
                    "memory": "AgentDesk uses Neo4j",
                    "metadata": {"source": "agentdesk"}
                },
                {
                    "memory": "AgentDesk uses Neo4j",
                    "metadata": {"source": "agentdesk"}
                }
            ],
            "relations": [
                {
                    "source": "agentdesk",
                    "relationship": "uses",
                    "destination": "neo4j"
                }
            ],
            "usage": {
                "request_tokens": 17,
                "response_tokens": 6
            }
        }"#;
        let (base_url, server_handle) = spawn_fixed_response_server("200 OK", body).await;
        let (_guard, prev_api_key, prev_base_url) = set_mem0_env(&base_url);
        let backend = Mem0Backend::new(mem0_settings());

        let response = backend
            .recall(RecallRequest {
                provider: ProviderKind::Codex,
                role_id: "codex".to_string(),
                channel_id: 1,
                session_id: "run-1".to_string(),
                dispatch_profile: DispatchProfile::Full,
                user_text: "Which graph database does AgentDesk use?".to_string(),
            })
            .await;

        server_handle.abort();
        restore_mem0_env(prev_api_key, prev_base_url);

        let external = response
            .external_recall
            .expect("expected external recall block");
        assert!(external.contains("Relevant memories from Mem0 for this session:"));
        assert!(external.contains("Relevant graph relations from Mem0 for this session:"));
        assert!(external.contains("agentdesk -- uses -- neo4j"));
        assert_eq!(external.matches("AgentDesk uses Neo4j").count(), 1);
        assert_eq!(
            response.token_usage,
            TokenUsage {
                input_tokens: 17,
                output_tokens: 6,
            }
        );
    }
}
