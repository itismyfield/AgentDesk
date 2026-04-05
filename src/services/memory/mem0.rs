use std::collections::HashMap;
use std::time::Duration;

use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{
    CaptureRequest, CaptureResult, LocalMemoryBackend, MemoryBackend, MemoryFuture, RecallRequest,
    RecallResponse,
};
use crate::services::discord::DispatchProfile;
use crate::services::discord::settings::ResolvedMemorySettings;

const MEM0_ADD_PATH: &str = "/v1/memories/";
const MEM0_SEARCH_PATH: &str = "/v2/memories/search";

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
}

#[derive(Clone)]
pub(crate) struct Mem0Backend {
    client: reqwest::Client,
    settings: ResolvedMemorySettings,
    local: LocalMemoryBackend,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SearchResponseEnvelope {
    Plain(Vec<SearchMemoryItem>),
    Wrapped { results: Vec<SearchMemoryItem> },
}

#[derive(Debug, Deserialize)]
struct SearchMemoryItem {
    memory: String,
    #[serde(default)]
    categories: Vec<String>,
    #[serde(default)]
    metadata: HashMap<String, Value>,
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
                top_k: 3,
            },
            _ => Mem0ProfilePolicy {
                threshold: 0.3,
                top_k: 5,
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
            json!(["memory", "metadata", "categories"]),
        );
        self.append_scope_fields(&mut body, config);
        Value::Object(body)
    }

    async fn search_external_recall(
        &self,
        request: &RecallRequest,
    ) -> Result<Option<String>, String> {
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
        let envelope: SearchResponseEnvelope = serde_json::from_str(&text)
            .map_err(|err| format!("mem0 search response decode failed: {err}; body={text}"))?;
        let results = match envelope {
            SearchResponseEnvelope::Plain(results) => results,
            SearchResponseEnvelope::Wrapped { results } => results,
        };

        Ok(format_external_recall(&results))
    }

    async fn add_capture(&self, request: &CaptureRequest) -> Result<(), String> {
        let config = self.runtime_config()?;
        let url = format!("{}{}", config.base_url, MEM0_ADD_PATH);
        let body = self.build_capture_body(request, &config);
        let response = self
            .auth_request(self.client.post(url), &config)
            .json(&body)
            .send()
            .await
            .map_err(|err| format!("mem0 add request failed: {err}"))?;

        if response.status() != StatusCode::OK {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("mem0 add failed with {status}: {body}"));
        }

        Ok(())
    }
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
                Ok(Ok(Some(external_recall))) => {
                    response.external_recall = Some(external_recall);
                }
                Ok(Ok(None)) => {}
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
                };
            }

            match tokio::time::timeout(
                Duration::from_millis(self.settings.capture_timeout_ms),
                self.add_capture(&request),
            )
            .await
            {
                Ok(Ok(())) => CaptureResult::default(),
                Ok(Err(err)) => CaptureResult {
                    warnings: vec![err],
                    skipped: true,
                },
                Err(_) => CaptureResult {
                    warnings: vec![format!(
                        "mem0 capture timed out after {}ms; skipping capture",
                        self.settings.capture_timeout_ms
                    )],
                    skipped: true,
                },
            }
        })
    }
}

fn format_external_recall(results: &[SearchMemoryItem]) -> Option<String> {
    let lines: Vec<String> = results
        .iter()
        .filter_map(|item| {
            let memory = item.memory.trim();
            if memory.is_empty() {
                return None;
            }
            let mut line = memory.to_string();
            if !item.categories.is_empty() {
                line.push_str(&format!(" [categories: {}]", item.categories.join(", ")));
            }
            if let Some(source) = item
                .metadata
                .get("source")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            {
                line.push_str(&format!(" [source: {source}]"));
            }
            Some(line)
        })
        .collect();

    if lines.is_empty() {
        None
    } else {
        Some(format!(
            "[External Recall]\nRelevant memories from Mem0 for this session:\n- {}",
            lines.join("\n- ")
        ))
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
            .unwrap();
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
            .unwrap();
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
        assert!(format_external_recall(&[]).is_none());
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
        assert_eq!(search["filters"]["AND"][0]["agent_id"], "codex");
        assert_eq!(search["filters"]["AND"][1]["run_id"], "run-1");
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
        assert_eq!(capture["agent_id"], "codex");
        assert_eq!(capture["run_id"], "run-9");
        assert_eq!(capture["metadata"]["channel_id"], "9");
        assert_eq!(capture["metadata"]["dispatch_id"], "dispatch-1");
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
}
