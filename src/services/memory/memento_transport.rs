//! MCP transport and durable remember admission.
use super::*;

struct ToolCallFailure {
    message: String,
    definitely_not_written: bool,
}

impl From<String> for ToolCallFailure {
    fn from(message: String) -> Self {
        Self {
            message,
            definitely_not_written: false,
        }
    }
}

impl MementoBackend {
    pub(super) async fn call_tool(
        &self,
        config: &MementoRuntimeConfig,
        tool_name: &str,
        arguments: Value,
    ) -> Result<ToolCallResult, String> {
        if tool_name == "remember" {
            let root = crate::config::runtime_root()
                .ok_or_else(|| "memento writer requires a runtime root".to_string())?;
            return self
                .remember_guarded(config, arguments, &root.join("state/memento-writer"))
                .await;
        }
        self.call_tool_transport(config, tool_name, arguments, true)
            .await
            .map_err(|error| error.message)
    }

    pub(super) async fn remember_guarded(
        &self,
        config: &MementoRuntimeConfig,
        arguments: Value,
        receipt_dir: &Path,
    ) -> Result<ToolCallResult, String> {
        let key = writer_fingerprint(
            receipt_dir,
            &config.endpoint,
            &config.access_key,
            &arguments,
        )?;
        let Some(claim) = WriterClaim::acquire(receipt_dir, &key)? else {
            note_memento_dedup_hit("remember");
            return Ok(ToolCallResult {
                payload: json!({"skipped": true, "reason": "already_stored"}),
                token_usage: TokenUsage::default(),
            });
        };
        // Initialization cannot have written a fragment. Release only here;
        // any error after dispatch has an ambiguous commit outcome.
        if let Err(error) = self.ensure_session(config).await {
            claim.release_before_send()?;
            return Err(error);
        }
        note_memento_remote_call("remember");
        match self
            .call_tool_transport(config, "remember", arguments, false)
            .await
        {
            Ok(result) => {
                claim.complete()?;
                Ok(result)
            }
            Err(error) => {
                if error.definitely_not_written {
                    claim.release_before_send()?;
                }
                Err(error.message)
            }
        }
    }

    async fn call_tool_transport(
        &self,
        config: &MementoRuntimeConfig,
        tool_name: &str,
        arguments: Value,
        retry_session: bool,
    ) -> Result<ToolCallResult, ToolCallFailure> {
        let mut session_id =
            self.ensure_session(config)
                .await
                .map_err(|message| ToolCallFailure {
                    message,
                    definitely_not_written: true,
                })?;

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
                .map_err(|err| ToolCallFailure {
                    message: format!("memento {tool_name} request failed: {err}"),
                    // Connect failures happen before the HTTP request is sent.
                    // Timeouts and response failures remain ambiguous.
                    definitely_not_written: err.is_connect() || err.is_builder(),
                })?;

            self.capture_session_id(config, &response);

            let status = response.status();
            let text = response
                .text()
                .await
                .map_err(|err| format!("memento {tool_name} response read failed: {err}"))?;

            if !status.is_success() {
                if retry_session
                    && attempt == 0
                    && (status == reqwest::StatusCode::UNAUTHORIZED || is_session_error(&text))
                {
                    self.clear_session_id(&config.endpoint);
                    session_id = self.initialize_session(config).await?;
                    continue;
                }
                // #2049 Finding 12: redact bearer-like substrings from error
                // bubbling so any memento-side echo cannot expose credentials.
                let safe = redact_memento_secret(&text, &config.access_key);
                if status == reqwest::StatusCode::UNAUTHORIZED || is_session_error(&text) {
                    self.clear_session_id(&config.endpoint);
                }
                return Err(ToolCallFailure {
                    message: format!("memento {tool_name} failed with {status}: {safe}"),
                    // Explicit authentication/authorization/rate rejection occurs
                    // before tool execution. Do not permanently poison a new fact.
                    definitely_not_written: matches!(
                        status,
                        reqwest::StatusCode::UNAUTHORIZED
                            | reqwest::StatusCode::FORBIDDEN
                            | reqwest::StatusCode::TOO_MANY_REQUESTS
                    ),
                });
            }

            let payload: Value = serde_json::from_str(&text).map_err(|err| {
                let safe = redact_memento_secret(&text, &config.access_key);
                format!("memento {tool_name} response decode failed: {err}; body={safe}")
            })?;

            if let Some(error) = payload.get("error") {
                let detail = render_rpc_error(error);
                if retry_session && attempt == 0 && is_session_error(&detail) {
                    self.clear_session_id(&config.endpoint);
                    session_id = self.initialize_session(config).await?;
                    continue;
                }
                if is_session_error(&detail) {
                    self.clear_session_id(&config.endpoint);
                }
                return Err(ToolCallFailure {
                    message: format!("memento {tool_name} rpc failed: {detail}"),
                    // Standard dispatch/parameter errors and Memento's documented
                    // SYMBOLIC_POLICY_VIOLATION (-32003) reject storage before
                    // mutation. Unknown application errors remain ambiguous.
                    definitely_not_written: matches!(
                        error.get("code").and_then(Value::as_i64),
                        Some(-32600 | -32601 | -32602 | -32003)
                    ),
                });
            }

            return extract_tool_result(&payload, tool_name).map_err(Into::into);
        }

        Err(format!("memento {tool_name} failed after retrying session initialization").into())
    }
}
