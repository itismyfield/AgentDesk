use std::time::Duration;

use reqwest::StatusCode;
use serde_json::{Map, Value, json};

use super::{
    CaptureRequest, CaptureResult, LocalMemoryBackend, MemoryBackend, MemoryFuture, RecallRequest,
    RecallResponse, ReflectRequest, UNBOUND_MEMORY_ROLE_ID,
};
use crate::runtime_layout;
use crate::services::discord::DispatchProfile;
use crate::services::discord::settings::ResolvedMemorySettings;

const MEMENTO_CONTEXT_PATH: &str = "/v1/context";
const MEMENTO_DISTILL_PATH: &str = "/v1/distill";
const MEMENTO_WORKSPACES_PATH: &str = "/v1/workspaces";
const MAX_WORKING_MEMORY_LINES: usize = 6;
const MAX_MEMORY_LINES: usize = 6;
const MAX_SKIP_LINES: usize = 4;

#[derive(Clone)]
struct MementoRuntimeConfig {
    endpoint: String,
    access_key: String,
    workspace_override: Option<String>,
}

#[derive(Clone)]
pub(crate) struct MementoBackend {
    client: reqwest::Client,
    settings: ResolvedMemorySettings,
    local: LocalMemoryBackend,
}

impl MementoBackend {
    pub(crate) fn new(settings: ResolvedMemorySettings) -> Self {
        Self {
            client: reqwest::Client::new(),
            settings,
            local: LocalMemoryBackend,
        }
    }

    fn runtime_config(&self) -> Result<MementoRuntimeConfig, String> {
        let root = crate::config::runtime_root().ok_or_else(|| {
            "AGENTDESK runtime root is unavailable; skipping memento backend".to_string()
        })?;
        let config = runtime_layout::load_memory_backend(&root);
        let endpoint = config.mcp.endpoint.trim().trim_end_matches('/').to_string();
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
        workspace: &str,
    ) -> reqwest::RequestBuilder {
        let builder = builder
            .header("Authorization", format!("Bearer {}", config.access_key))
            .header("Accept", "application/json");

        if workspace.trim().is_empty() {
            builder
        } else {
            builder.header("X-Memento-Workspace", workspace)
        }
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

    async fn ensure_workspace(
        &self,
        config: &MementoRuntimeConfig,
        workspace: &str,
    ) -> Result<(), String> {
        let url = format!("{}{}", config.endpoint, MEMENTO_WORKSPACES_PATH);
        let response = self
            .auth_request(self.client.post(url), config, "")
            .json(&json!({ "name": workspace }))
            .send()
            .await
            .map_err(|err| format!("memento workspace init request failed: {err}"))?;

        if response.status().is_success() || response.status() == StatusCode::CONFLICT {
            return Ok(());
        }

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(format!(
            "memento workspace init failed with {status}: {body}"
        ))
    }

    async fn post_json_with_workspace_retry(
        &self,
        config: &MementoRuntimeConfig,
        workspace: &str,
        path: &str,
        body: &Value,
        label: &str,
    ) -> Result<reqwest::Response, String> {
        let url = format!("{}{}", config.endpoint, path);
        let mut retried_after_init = false;

        loop {
            let response = self
                .auth_request(self.client.post(url.clone()), config, workspace)
                .json(body)
                .send()
                .await
                .map_err(|err| format!("memento {label} request failed: {err}"))?;

            if response.status() != StatusCode::NOT_FOUND || retried_after_init {
                return Ok(response);
            }

            self.ensure_workspace(config, workspace).await?;
            retried_after_init = true;
        }
    }

    async fn fetch_context(
        &self,
        request: &RecallRequest,
        config: &MementoRuntimeConfig,
        workspace: &str,
    ) -> Result<Option<String>, String> {
        let body = json!({
            "message": request.user_text,
            "include": ["working_memory", "memories", "skip_list", "identity"],
            "include_graph": false,
        });
        let response = self
            .post_json_with_workspace_retry(
                config,
                workspace,
                MEMENTO_CONTEXT_PATH,
                &body,
                "context",
            )
            .await?;

        if response.status() != StatusCode::OK {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(format!("memento context failed with {status}: {body}"));
        }

        let text = response
            .text()
            .await
            .map_err(|err| format!("memento context response read failed: {err}"))?;
        let payload: Value = serde_json::from_str(&text)
            .map_err(|err| format!("memento context response decode failed: {err}; body={text}"))?;
        Ok(format_context_payload_for_external_recall(&payload))
    }

    async fn distill_transcript(
        &self,
        request: &ReflectRequest,
        config: &MementoRuntimeConfig,
        workspace: &str,
    ) -> Result<(), String> {
        let transcript = format!(
            "[System]: Session ended via {} for provider={} role_id={} channel_id={} session_id={}\n{}",
            request.reason.as_str(),
            request.provider.as_str(),
            request.role_id,
            request.channel_id,
            request.session_id,
            request.transcript.trim()
        );
        let body = json!({ "transcript": transcript });

        let response = self
            .post_json_with_workspace_retry(
                config,
                workspace,
                MEMENTO_DISTILL_PATH,
                &body,
                "distill",
            )
            .await?;

        if response.status().is_success() {
            return Ok(());
        }

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        Err(format!("memento distill failed with {status}: {body}"))
    }
}

fn env_var_value(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
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
                Ok(Ok(Some(external_recall))) => RecallResponse {
                    external_recall: Some(external_recall),
                    ..RecallResponse::default()
                },
                Ok(Ok(None)) => RecallResponse::default(),
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
                };
            }

            let config = match self.runtime_config() {
                Ok(config) => config,
                Err(err) => {
                    return CaptureResult {
                        warnings: vec![err],
                        skipped: true,
                    };
                }
            };
            let workspace = self.resolve_workspace(&request.role_id, request.channel_id, &config);

            match tokio::time::timeout(
                Duration::from_millis(self.settings.capture_timeout_ms),
                self.distill_transcript(&request, &config, &workspace),
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
                        "memento reflect timed out after {}ms; skipping reflect",
                        self.settings.capture_timeout_ms
                    )],
                    skipped: true,
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

    async fn spawn_fixed_response_server(
        status_line: &'static str,
        body: &'static str,
    ) -> (
        String,
        tokio::sync::oneshot::Receiver<String>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let response = format!(
            "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let (request_tx, request_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let _ = request_tx.send(String::from_utf8_lossy(&buf[..n]).to_string());
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        (format!("http://{}", addr), request_rx, handle)
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
    async fn test_memento_recall_formats_context_endpoint_payload() {
        let (base_url, request_rx, handle) = spawn_fixed_response_server(
            "200 OK",
            r#"{
                "working_memory": {
                    "items": [
                        {
                            "category": "active_work",
                            "title": "Finish #344",
                            "next_action": "Replace placeholder Memento backend",
                            "tags": ["agentdesk", "memory"]
                        }
                    ]
                },
                "memories": {
                    "matches": [
                        {
                            "id": "m1",
                            "content": "Use /v1/health for Memento health checks.",
                            "type": "decision",
                            "tags": ["memento", "health"],
                            "score": 0.93
                        }
                    ]
                },
                "skip_matches": [
                    {
                        "item": "fallback-only memento",
                        "reason": "Real transport already exists now",
                        "expires": null
                    }
                ],
                "identity": "I keep AgentDesk memory behavior coherent across sessions."
            }"#,
        )
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

        let request_dump = request_rx.await.unwrap();
        handle.abort();
        restore_memento_runtime(previous_root, previous_key, previous_workspace);

        let request_dump_lower = request_dump.to_lowercase();
        assert!(request_dump_lower.contains("post /v1/context"));
        assert!(request_dump_lower.contains("x-memento-workspace: agentdesk-project-agentdesk"));
        assert!(request_dump.contains("\"message\":\"What do we know about #344?\""));
        let external_recall = recall.external_recall.unwrap_or_default();
        assert!(external_recall.contains("Finish #344"));
        assert!(external_recall.contains("Replace placeholder Memento backend"));
        assert!(external_recall.contains("Use /v1/health for Memento health checks."));
        assert!(external_recall.contains("fallback-only memento"));
        assert!(
            external_recall.contains("I keep AgentDesk memory behavior coherent across sessions.")
        );
        assert!(recall.shared_knowledge.is_none());
        assert!(recall.longterm_catalog.is_none());
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
    async fn test_memento_reflect_posts_distill_transcript() {
        let (base_url, request_rx, handle) =
            spawn_fixed_response_server("201 Created", r#"{"stored":[{"id":"abc123"}]}"#).await;
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

        let request_dump = request_rx.await.unwrap();
        handle.abort();
        restore_memento_runtime(previous_root, previous_key, previous_workspace);

        let request_dump_lower = request_dump.to_lowercase();
        assert!(request_dump_lower.contains("post /v1/distill"));
        assert!(request_dump_lower.contains("x-memento-workspace: agentdesk-project-agentdesk"));
        assert!(request_dump.contains("idle_expiry"));
        assert!(request_dump.contains("[User]: hi"));
        assert!(request_dump.contains("[Assistant]: hello"));
        assert_eq!(result, CaptureResult::default());
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
