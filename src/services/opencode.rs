//! OpenCode provider backend — `opencode serve` HTTP/SSE integration.
//!
//! Architecture: spawns `opencode serve --hostname 127.0.0.1 --port <N>`, drives the
//! HTTP REST + SSE API, and normalizes events to AgentDesk `StreamMessage`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::Value;

use crate::services::agent_protocol::StreamMessage;
use crate::services::process::configure_child_process_group;
use crate::services::provider::{CancelToken, ProviderKind, cancel_requested, register_child_pid};
use crate::services::remote::RemoteProfile;

const HEALTH_TIMEOUT: Duration = Duration::from_secs(30);
const HEALTH_POLL_MS: u64 = 250;
const SSE_READ_TIMEOUT: Duration = Duration::from_secs(120);
const DISPOSE_TIMEOUT: Duration = Duration::from_secs(5);
const STARTUP_OUTPUT_LIMIT: usize = 8 * 1024;

#[derive(Debug, Default)]
struct OpenCodeStartupOutput {
    stdout: String,
    stderr: String,
}

struct OpenCodeServerProcess {
    child: Child,
    startup_output: Arc<Mutex<OpenCodeStartupOutput>>,
}

impl OpenCodeServerProcess {
    fn id(&self) -> u32 {
        self.child.id()
    }

    fn terminate(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn resolve_opencode_path() -> Option<String> {
    resolve_opencode_binary().resolved_path
}

fn resolve_opencode_binary() -> crate::services::platform::BinaryResolution {
    crate::services::platform::resolve_provider_binary("opencode")
}

pub fn probe_serve_health(working_dir: &str) -> Result<String, String> {
    let resolution = resolve_opencode_binary();
    let bin = resolution.resolved_path.clone().ok_or_else(|| {
        "OpenCode CLI not found — install with: npm install -g opencode-ai".to_string()
    })?;
    let port = allocate_port()?;
    let password = generate_password();
    let auth = build_auth_header(&password);
    let base_url = format!("http://127.0.0.1:{port}");
    let mut server = spawn_server(&bin, &resolution, port, &password, working_dir)?;
    let result = wait_for_health(&base_url, &auth, Some(&server.startup_output))
        .map(|_| format!("serve health ok at {base_url}"));
    shutdown_server(&mut server, &base_url, &auth);
    result
}

pub fn execute_command_simple_cancellable(
    prompt: &str,
    cancel_token: Option<&CancelToken>,
) -> Result<String, String> {
    let local_cancel;
    let effective_cancel = match cancel_token {
        Some(token) => Some(token),
        None => {
            local_cancel = CancelToken::new();
            Some(&local_cancel)
        }
    };
    let (tx, rx) = std::sync::mpsc::channel::<StreamMessage>();
    let working_dir = std::env::current_dir()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());

    execute_command_streaming_inner(
        prompt,
        None,
        &working_dir,
        tx,
        None,
        None,
        effective_cancel,
        None,
        None,
        None,
        None,
        None,
        None,
    )?;

    let mut done_result: Option<String> = None;
    let mut text = String::new();
    let mut error: Option<String> = None;

    for msg in rx.try_iter() {
        match msg {
            StreamMessage::Done { result, .. } => {
                if !result.trim().is_empty() {
                    done_result = Some(result);
                }
            }
            StreamMessage::Text { content } => text.push_str(&content),
            StreamMessage::Error { message, .. } => error = Some(message),
            _ => {}
        }
    }

    if let Some(result) = done_result {
        return Ok(result);
    }
    if let Some(error) = error {
        return Err(error);
    }
    let text = text.trim().to_string();
    if !text.is_empty() {
        return Ok(text);
    }
    Err("Empty response from OpenCode".to_string())
}

#[allow(clippy::too_many_arguments)]
pub fn execute_command_streaming(
    prompt: &str,
    _session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
    cancel_token: Option<Arc<CancelToken>>,
    remote_profile: Option<&RemoteProfile>,
    _tmux_session_name: Option<&str>,
    _report_channel_id: Option<u64>,
    _report_provider: Option<ProviderKind>,
    model: Option<&str>,
    _compact_percent: Option<u64>,
) -> Result<(), String> {
    execute_command_streaming_inner(
        prompt,
        _session_id,
        working_dir,
        sender,
        system_prompt,
        allowed_tools,
        cancel_token.as_deref(),
        remote_profile,
        _tmux_session_name,
        _report_channel_id,
        _report_provider,
        model,
        _compact_percent,
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_command_streaming_inner(
    prompt: &str,
    _session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
    cancel_token: Option<&CancelToken>,
    remote_profile: Option<&RemoteProfile>,
    _tmux_session_name: Option<&str>,
    _report_channel_id: Option<u64>,
    _report_provider: Option<ProviderKind>,
    model: Option<&str>,
    _compact_percent: Option<u64>,
) -> Result<(), String> {
    if remote_profile.is_some() {
        return send_error(
            &sender,
            "OpenCode does not support remote profiles".to_string(),
        );
    }

    let model_override = parse_model_override(model)?;

    let resolution = resolve_opencode_binary();
    let bin = resolution.resolved_path.clone().ok_or_else(|| {
        "OpenCode CLI not found — install with: npm install -g opencode-ai".to_string()
    })?;

    let port = allocate_port()?;
    let password = generate_password();
    let auth = build_auth_header(&password);
    let base_url = format!("http://127.0.0.1:{port}");

    let mut server = spawn_server(&bin, &resolution, port, &password, working_dir)?;
    register_child_pid(cancel_token, server.id());

    let result = run_session(
        prompt,
        system_prompt,
        allowed_tools,
        model_override.as_ref(),
        &base_url,
        &auth,
        &sender,
        cancel_token,
        Some(server.startup_output.clone()),
    );

    shutdown_server(&mut server, &base_url, &auth);

    match result {
        Ok(()) => Ok(()),
        Err(msg) => send_error(&sender, msg),
    }
}

// ---------------------------------------------------------------------------
// Server lifecycle
// ---------------------------------------------------------------------------

fn spawn_server(
    bin: &str,
    resolution: &crate::services::platform::BinaryResolution,
    port: u16,
    password: &str,
    working_dir: &str,
) -> Result<OpenCodeServerProcess, String> {
    let mut cmd = Command::new(bin);
    crate::services::platform::apply_binary_resolution(&mut cmd, resolution);
    configure_child_process_group(&mut cmd);
    cmd.arg("serve")
        .arg("--hostname")
        .arg("127.0.0.1")
        .arg("--port")
        .arg(port.to_string())
        .env("OPENCODE_SERVER_PASSWORD", password)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn opencode serve: {e}"))?;
    let startup_output = Arc::new(Mutex::new(OpenCodeStartupOutput::default()));
    if let Some(stdout) = child.stdout.take() {
        drain_startup_output(stdout, startup_output.clone(), StartupStream::Stdout);
    }
    if let Some(stderr) = child.stderr.take() {
        drain_startup_output(stderr, startup_output.clone(), StartupStream::Stderr);
    }
    Ok(OpenCodeServerProcess {
        child,
        startup_output,
    })
}

enum StartupStream {
    Stdout,
    Stderr,
}

fn drain_startup_output<R>(
    mut reader: R,
    output: Arc<Mutex<OpenCodeStartupOutput>>,
    stream: StartupStream,
) where
    R: Read + Send + 'static,
{
    let _ = thread::spawn(move || {
        let mut buffer = [0_u8; 1024];
        loop {
            let read = match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(_) => break,
            };
            let chunk = String::from_utf8_lossy(&buffer[..read]);
            if let Ok(mut output) = output.lock() {
                match stream {
                    StartupStream::Stdout => append_bounded(&mut output.stdout, &chunk),
                    StartupStream::Stderr => append_bounded(&mut output.stderr, &chunk),
                }
            }
        }
    });
}

fn append_bounded(target: &mut String, chunk: &str) {
    target.push_str(chunk);
    if target.len() <= STARTUP_OUTPUT_LIMIT {
        return;
    }
    let mut split_at = target.len().saturating_sub(STARTUP_OUTPUT_LIMIT);
    while !target.is_char_boundary(split_at) && split_at < target.len() {
        split_at += 1;
    }
    target.drain(..split_at);
}

fn summarize_startup_output(output: &Arc<Mutex<OpenCodeStartupOutput>>) -> String {
    let Ok(output) = output.lock() else {
        return String::new();
    };
    let stdout = compact_log_fragment(&output.stdout);
    let stderr = compact_log_fragment(&output.stderr);
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!("stdout={stdout}"),
        (true, false) => format!("stderr={stderr}"),
        (false, false) => format!("stdout={stdout}; stderr={stderr}"),
    }
}

fn compact_log_fragment(raw: &str) -> String {
    raw.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn dispose_server(base_url: &str, auth: &str) {
    let agent = ureq::AgentBuilder::new().timeout(DISPOSE_TIMEOUT).build();
    let _ = agent
        .post(&format!("{base_url}/instance/dispose"))
        .set("Authorization", auth)
        .call();
}

fn shutdown_server(server: &mut OpenCodeServerProcess, base_url: &str, auth: &str) {
    dispose_server(base_url, auth);
    server.terminate();
}

// ---------------------------------------------------------------------------
// Session flow
// ---------------------------------------------------------------------------

fn run_session(
    prompt: &str,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
    model: Option<&OpenCodeModelRef>,
    base_url: &str,
    auth: &str,
    sender: &Sender<StreamMessage>,
    cancel_token: Option<&CancelToken>,
    startup_output: Option<Arc<Mutex<OpenCodeStartupOutput>>>,
) -> Result<(), String> {
    // 1. Wait for server to be ready
    wait_for_health(base_url, auth, startup_output.as_ref())?;

    if is_cancelled(cancel_token) {
        return Err("OpenCode request cancelled before session start".to_string());
    }

    // 2. Create session
    let session_id = create_session(base_url, auth)?;

    // 3. Announce session
    let _ = sender.send(StreamMessage::Init {
        session_id: session_id.clone(),
        raw_session_id: None,
    });

    // 4. Connect SSE stream first, then send prompt
    let sse_agent = ureq::AgentBuilder::new()
        .timeout_read(SSE_READ_TIMEOUT)
        .build();

    let sse_response = sse_agent
        .get(&format!("{base_url}/event"))
        .set("Authorization", auth)
        .set("Accept", "text/event-stream")
        .call()
        .map_err(|e| format!("Failed to connect to OpenCode SSE stream: {e}"))?;

    // 5. Send prompt (non-blocking — server processes it while we read SSE)
    send_prompt(
        base_url,
        auth,
        &session_id,
        prompt,
        system_prompt,
        allowed_tools,
        model,
    )?;

    // 6. Read SSE stream
    let reader: BufReader<Box<dyn std::io::Read + Send>> =
        BufReader::new(Box::new(sse_response.into_reader()));
    consume_sse(reader, &session_id, sender, cancel_token, base_url, auth)
}

fn wait_for_health(
    base_url: &str,
    auth: &str,
    startup_output: Option<&Arc<Mutex<OpenCodeStartupOutput>>>,
) -> Result<(), String> {
    let deadline = Instant::now() + HEALTH_TIMEOUT;
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(2))
        .build();

    loop {
        if Instant::now() >= deadline {
            let output = startup_output
                .map(|output| summarize_startup_output(output))
                .filter(|summary| !summary.is_empty())
                .map(|summary| format!("; startup output: {summary}"))
                .unwrap_or_default();
            return Err(format!(
                "OpenCode server health check timed out after {}s",
                HEALTH_TIMEOUT.as_secs()
            ) + &output);
        }
        match agent
            .get(&format!("{base_url}/global/health"))
            .set("Authorization", auth)
            .call()
        {
            Ok(r) if r.status() == 200 => return Ok(()),
            _ => std::thread::sleep(Duration::from_millis(HEALTH_POLL_MS)),
        }
    }
}

fn create_session(base_url: &str, auth: &str) -> Result<String, String> {
    let response = ureq::post(&format!("{base_url}/session"))
        .set("Authorization", auth)
        .set("Content-Type", "application/json")
        .send_json(serde_json::json!({}))
        .map_err(|e| format!("Failed to create OpenCode session: {e}"))?;

    let json: Value = response
        .into_json()
        .map_err(|e| format!("Failed to parse session response: {e}"))?;

    // Accept "id", "sessionID", or "session_id"
    ["id", "sessionID", "session_id"]
        .iter()
        .find_map(|key| json.get(key).and_then(|v| v.as_str()))
        .map(|s| s.to_string())
        .ok_or_else(|| format!("Session response missing ID field: {json}"))
}

fn send_prompt(
    base_url: &str,
    auth: &str,
    session_id: &str,
    prompt: &str,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
    model: Option<&OpenCodeModelRef>,
) -> Result<(), String> {
    let body = build_prompt_body(prompt, system_prompt, allowed_tools, model);

    let resp = ureq::post(&format!("{base_url}/session/{session_id}/prompt_async"))
        .set("Authorization", auth)
        .set("Content-Type", "application/json")
        .send_json(body)
        .map_err(|e| format!("Failed to send prompt to OpenCode: {e}"))?;

    let status = resp.status();
    if status == 204 || (200..300).contains(&(status as u32)) {
        Ok(())
    } else {
        Err(format!("prompt_async returned unexpected status: {status}"))
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct OpenCodeModelRef {
    provider_id: String,
    model_id: String,
}

fn parse_model_override(model: Option<&str>) -> Result<Option<OpenCodeModelRef>, String> {
    let Some(raw) = model.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if raw.eq_ignore_ascii_case("default") {
        return Ok(None);
    }

    let Some((provider_id, model_id)) = raw.split_once('/') else {
        return Err(format!(
            "OpenCode model override must use providerID/modelID syntax, got `{raw}`"
        ));
    };
    let provider_id = provider_id.trim();
    let model_id = model_id.trim();
    if provider_id.is_empty() || model_id.is_empty() {
        return Err(format!(
            "OpenCode model override must use providerID/modelID syntax, got `{raw}`"
        ));
    }

    Ok(Some(OpenCodeModelRef {
        provider_id: provider_id.to_string(),
        model_id: model_id.to_string(),
    }))
}

fn build_prompt_body(
    prompt: &str,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
    model: Option<&OpenCodeModelRef>,
) -> Value {
    let mut body = serde_json::json!({
        "parts": [{"type": "text", "text": prompt}]
    });

    if let Some(system) = compose_system_prompt(system_prompt, allowed_tools)
        && let Some(object) = body.as_object_mut()
    {
        object.insert("system".to_string(), Value::String(system));
    }

    if let Some(model) = model
        && let Some(object) = body.as_object_mut()
    {
        object.insert(
            "model".to_string(),
            serde_json::json!({
                "providerID": model.provider_id,
                "modelID": model.model_id,
            }),
        );
    }

    body
}

fn compose_system_prompt(
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(system_prompt) = system_prompt
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        parts.push(system_prompt.to_string());
    }

    if let Some(tools) = allowed_tools.filter(|tools| !tools.is_empty()) {
        let mut names = tools
            .iter()
            .map(|tool| tool.trim())
            .filter(|tool| !tool.is_empty())
            .collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();
        if !names.is_empty() {
            parts.push(format!(
                "AgentDesk allowed tools advisory: requested allowed tools are {}. OpenCode permission-key mapping is not verified in this runtime; follow this allowlist while AgentDesk enforces outbound safety separately.",
                names.join(", ")
            ));
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n\n"))
    }
}

fn abort_session(base_url: &str, auth: &str, session_id: &str) {
    let _ = ureq::post(&format!("{base_url}/session/{session_id}/abort"))
        .set("Authorization", auth)
        .call();
}

// ---------------------------------------------------------------------------
// SSE stream consumption
// ---------------------------------------------------------------------------

fn consume_sse(
    reader: BufReader<Box<dyn std::io::Read + Send>>,
    session_id: &str,
    sender: &Sender<StreamMessage>,
    cancel_token: Option<&CancelToken>,
    base_url: &str,
    auth: &str,
) -> Result<(), String> {
    let mut state = SseMessageState::default();
    let mut current_data = String::new();
    let mut idle_seen = false;
    let mut last_event = Instant::now();

    for line_result in reader.lines() {
        if is_cancelled(cancel_token) {
            abort_session(base_url, auth, session_id);
            return Err("OpenCode request cancelled".to_string());
        }

        if Instant::now().duration_since(last_event) > SSE_READ_TIMEOUT {
            abort_session(base_url, auth, session_id);
            return Err(format!(
                "OpenCode SSE stream idle for >{}s",
                SSE_READ_TIMEOUT.as_secs()
            ));
        }

        let line = match line_result {
            Ok(l) => l,
            Err(e) => {
                if idle_seen {
                    break;
                }
                return Err(format!("OpenCode SSE stream read error: {e}"));
            }
        };

        last_event = Instant::now();

        // Keep-alive comment
        if line.starts_with(':') || line.starts_with("event:") {
            continue;
        }

        if let Some(data) = line.strip_prefix("data:") {
            current_data.push_str(data.trim());
            continue;
        }

        // Empty line → dispatch accumulated data
        if line.is_empty() && !current_data.is_empty() {
            let data = current_data.clone();
            current_data.clear();

            if let Some(should_stop) = process_sse_event(&data, session_id, sender, &mut state) {
                if should_stop {
                    idle_seen = true;
                    if !state.terminal_error {
                        let _ = sender.send(StreamMessage::Done {
                            result: state.accumulated_text.trim().to_string(),
                            session_id: Some(session_id.to_string()),
                        });
                    }
                    break;
                }
            }
        }
    }

    if !idle_seen && state.accumulated_text.trim().is_empty() {
        return Err("OpenCode stream ended without a terminal event".to_string());
    }

    Ok(())
}

#[derive(Default)]
struct SseMessageState {
    accumulated_text: String,
    text_part_snapshots: HashMap<String, String>,
    terminal_error: bool,
}

fn append_text_delta(sender: &Sender<StreamMessage>, state: &mut SseMessageState, text: &str) {
    if text.is_empty() {
        return;
    }
    state.accumulated_text.push_str(text);
    let _ = sender.send(StreamMessage::Text {
        content: text.to_string(),
    });
}

fn emit_text_part(part: &Value, sender: &Sender<StreamMessage>, state: &mut SseMessageState) {
    let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
    if text.is_empty() {
        return;
    }

    let Some(part_id) = part
        .get("id")
        .or_else(|| part.get("partID"))
        .and_then(|v| v.as_str())
    else {
        append_text_delta(sender, state, text);
        return;
    };

    let previous = state
        .text_part_snapshots
        .get(part_id)
        .map(String::as_str)
        .unwrap_or("");
    let delta = text.strip_prefix(previous).unwrap_or(text);
    state
        .text_part_snapshots
        .insert(part_id.to_string(), text.to_string());
    append_text_delta(sender, state, delta);
}

fn stream_value(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

fn emit_tool_part(part: &Value, sender: &Sender<StreamMessage>) {
    let name = part
        .get("tool")
        .or_else(|| part.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let state = part.get("state").unwrap_or(&Value::Null);
    let status = state
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let input = state
        .get("input")
        .or_else(|| part.get("input"))
        .map(stream_value)
        .unwrap_or_default();

    if !input.is_empty() || matches!(status, "pending" | "running" | "completed" | "error") {
        let _ = sender.send(StreamMessage::ToolUse { name, input });
    }

    let output = state
        .get("output")
        .or_else(|| state.get("error"))
        .or_else(|| state.get("title"))
        .or_else(|| part.get("output"))
        .or_else(|| part.get("content"));
    if let Some(output) = output {
        let is_error = status == "error"
            || state
                .get("isError")
                .or_else(|| state.get("is_error"))
                .or_else(|| part.get("isError"))
                .or_else(|| part.get("is_error"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
        let _ = sender.send(StreamMessage::ToolResult {
            content: stream_value(output),
            is_error,
        });
    }
}

fn emit_part(part: &Value, sender: &Sender<StreamMessage>, state: &mut SseMessageState) {
    let part_type = part.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match part_type {
        "text" => emit_text_part(part, sender, state),
        "thinking" | "redactedThinking" | "reasoning" => {
            let summary = part
                .get("thinking")
                .or_else(|| part.get("summary"))
                .or_else(|| part.get("text"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let _ = sender.send(StreamMessage::Thinking { summary });
        }
        "tool" => emit_tool_part(part, sender),
        "tool-use" => {
            let name = part
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let input = part.get("input").map(|v| v.to_string()).unwrap_or_default();
            let _ = sender.send(StreamMessage::ToolUse {
                name: name.to_string(),
                input,
            });
        }
        "tool-result" => {
            let content = part
                .get("output")
                .or_else(|| part.get("content"))
                .map(stream_value)
                .unwrap_or_default();
            let is_error = part
                .get("isError")
                .or_else(|| part.get("is_error"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let _ = sender.send(StreamMessage::ToolResult { content, is_error });
        }
        _ => {}
    }
}

/// Returns `Some(true)` if the session is done (idle), `Some(false)` to continue, `None` to ignore.
fn process_sse_event(
    data: &str,
    session_id: &str,
    sender: &Sender<StreamMessage>,
    state: &mut SseMessageState,
) -> Option<bool> {
    let event: Value = serde_json::from_str(data).ok()?;
    let event_type = event.get("type").and_then(|v| v.as_str())?;
    let props = event.get("properties");

    // Filter events by sessionID where applicable
    let event_session = props
        .and_then(|p| p.get("sessionID").and_then(|v| v.as_str()))
        .or_else(|| props.and_then(|p| p.get("sessionId").and_then(|v| v.as_str())))
        .or_else(|| {
            props
                .and_then(|p| p.get("info"))
                .and_then(|i| i.get("id"))
                .and_then(|v| v.as_str())
        })
        .or_else(|| {
            props.and_then(|p| p.get("part")).and_then(|part| {
                part.get("sessionID")
                    .or_else(|| part.get("sessionId"))
                    .and_then(|v| v.as_str())
            })
        })
        .or_else(|| {
            props.and_then(|p| p.get("message")).and_then(|message| {
                message
                    .get("sessionID")
                    .or_else(|| message.get("sessionId"))
                    .and_then(|v| v.as_str())
            })
        });

    if let Some(sid) = event_session {
        if sid != session_id {
            return None; // Wrong session — filter out (issue #9650)
        }
    }

    match event_type {
        "session.created" | "server.connected" => None,

        "session.status" => {
            if let Some(status) = props
                .and_then(|p| p.get("status"))
                .and_then(|v| v.as_str())
                .or_else(|| {
                    props
                        .and_then(|p| p.get("info"))
                        .and_then(|i| i.get("status"))
                        .and_then(|v| v.as_str())
                })
            {
                let info = props.and_then(|p| p.get("info"));
                let _ = sender.send(StreamMessage::StatusUpdate {
                    model: None,
                    cost_usd: None,
                    total_cost_usd: None,
                    duration_ms: None,
                    num_turns: None,
                    input_tokens: None,
                    cache_create_tokens: None,
                    cache_read_tokens: None,
                    output_tokens: props
                        .and_then(|p| p.get("outputTokens"))
                        .or_else(|| info.and_then(|i| i.get("outputTokens")))
                        .and_then(|v| v.as_u64()),
                });
                let _ = status;
            }
            Some(false)
        }

        "part" => {
            let part = props.and_then(|p| p.get("part"))?;
            emit_part(part, sender, state);
            Some(false)
        }

        "message.part.delta" => {
            if props.and_then(|p| p.get("field")).and_then(|v| v.as_str()) == Some("text") {
                let delta = props
                    .and_then(|p| p.get("delta"))
                    .and_then(|v| v.as_str())?;
                if let Some(part_id) = props.and_then(|p| p.get("partID")).and_then(|v| v.as_str())
                {
                    let entry = state
                        .text_part_snapshots
                        .entry(part_id.to_string())
                        .or_default();
                    entry.push_str(delta);
                }
                append_text_delta(sender, state, delta);
            }
            Some(false)
        }

        "message.part.updated" => {
            let part = props.and_then(|p| p.get("part"))?;
            emit_part(part, sender, state);
            Some(false)
        }

        "message.completed" => {
            // Full assembled message — emit any final text parts not yet streamed
            if let Some(message) = props.and_then(|p| p.get("message")) {
                if let Some(parts) = message.get("parts").and_then(|p| p.as_array()) {
                    for part in parts {
                        if part.get("type").and_then(|v| v.as_str()) == Some("text") {
                            if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                                // Only emit if we haven't already streamed it
                                if state.accumulated_text.is_empty() && !text.trim().is_empty() {
                                    append_text_delta(sender, state, text);
                                }
                            }
                        }
                    }
                }
            }
            Some(false)
        }

        "session.idle" => {
            // Turn is complete
            Some(true)
        }

        "session.error" | "error" => {
            let msg = props
                .and_then(|p| p.get("message").or_else(|| p.get("error")))
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown OpenCode error")
                .to_string();
            let _ = sender.send(StreamMessage::Error {
                message: msg,
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
            });
            state.terminal_error = true;
            Some(true) // Stop on error events
        }

        _ => None, // Unknown event type — silently ignore
    }
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

fn allocate_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|e| format!("Failed to allocate a free port: {e}"))?;
    Ok(listener.local_addr().unwrap().port())
    // listener drops here, freeing the port with a brief race window
}

fn generate_password() -> String {
    let bytes: Vec<u8> = (0..16).map(|_| rand::random::<u8>()).collect();
    BASE64.encode(&bytes)
}

fn build_auth_header(password: &str) -> String {
    let credentials = format!("opencode:{password}");
    format!("Basic {}", BASE64.encode(credentials.as_bytes()))
}

fn is_cancelled(cancel_token: Option<&CancelToken>) -> bool {
    cancel_requested(cancel_token)
}

fn send_error(sender: &Sender<StreamMessage>, message: String) -> Result<(), String> {
    let _ = sender.send(StreamMessage::Error {
        message,
        stdout: String::new(),
        stderr: String::new(),
        exit_code: None,
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    // Helper to process a raw SSE data string and collect messages
    fn parse_event(data: &str, session_id: &str) -> (Vec<StreamMessage>, Option<bool>) {
        let (tx, rx) = mpsc::channel::<StreamMessage>();
        let mut state = SseMessageState::default();
        let stop = process_sse_event(data, session_id, &tx, &mut state);
        drop(tx);
        (rx.try_iter().collect(), stop)
    }

    fn parse_events(events: &[&str], session_id: &str) -> (Vec<StreamMessage>, Vec<Option<bool>>) {
        let (tx, rx) = mpsc::channel::<StreamMessage>();
        let mut state = SseMessageState::default();
        let stops = events
            .iter()
            .map(|event| process_sse_event(event, session_id, &tx, &mut state))
            .collect::<Vec<_>>();
        drop(tx);
        (rx.try_iter().collect(), stops)
    }

    #[test]
    fn test_from_str_roundtrip() {
        let pk = crate::services::provider::ProviderKind::from_str("opencode");
        assert_eq!(pk, Some(crate::services::provider::ProviderKind::OpenCode));
        assert_eq!(
            crate::services::provider::ProviderKind::OpenCode.as_str(),
            "opencode"
        );
    }

    #[test]
    fn test_wrong_session_id_ignored() {
        let data = r#"{"type":"part","properties":{"sessionID":"other-session","part":{"type":"text","text":"hello"}}}"#;
        let (msgs, stop) = parse_event(data, "my-session");
        assert!(msgs.is_empty(), "wrong-session events must be filtered");
        assert_eq!(stop, None);
    }

    #[test]
    fn test_text_part_emitted() {
        let data = r#"{"type":"part","properties":{"sessionID":"s1","part":{"type":"text","text":"hello world"}}}"#;
        let (msgs, stop) = parse_event(data, "s1");
        assert_eq!(stop, Some(false));
        assert!(
            msgs.iter()
                .any(|m| matches!(m, StreamMessage::Text { content } if content == "hello world"))
        );
    }

    #[test]
    fn test_message_part_updated_text_emitted() {
        let data = r#"{"type":"message.part.updated","properties":{"sessionID":"s1","part":{"id":"part-1","type":"text","text":"OK"}}}"#;
        let (msgs, stop) = parse_event(data, "s1");
        assert_eq!(stop, Some(false));
        assert!(
            msgs.iter()
                .any(|m| matches!(m, StreamMessage::Text { content } if content == "OK"))
        );
    }

    #[test]
    fn test_message_part_updated_wrong_session_ignored() {
        let data = r#"{"type":"message.part.updated","properties":{"sessionID":"other-session","part":{"id":"part-1","type":"text","text":"OK"}}}"#;
        let (msgs, stop) = parse_event(data, "s1");
        assert!(
            msgs.is_empty(),
            "wrong-session updated text must be ignored"
        );
        assert_eq!(stop, None);
    }

    #[test]
    fn test_message_part_updated_filters_nested_part_session() {
        let data = r#"{"type":"message.part.updated","properties":{"part":{"id":"part-1","sessionID":"other-session","type":"text","text":"OK"}}}"#;
        let (msgs, stop) = parse_event(data, "s1");
        assert!(msgs.is_empty(), "wrong nested part session must be ignored");
        assert_eq!(stop, None);
    }

    #[test]
    fn test_message_part_delta_then_updated_does_not_duplicate_text() {
        let events = [
            r#"{"type":"message.part.delta","properties":{"sessionID":"s1","messageID":"m1","partID":"part-1","field":"text","delta":"O"}}"#,
            r#"{"type":"message.part.delta","properties":{"sessionID":"s1","messageID":"m1","partID":"part-1","field":"text","delta":"K"}}"#,
            r#"{"type":"message.part.updated","properties":{"sessionID":"s1","part":{"id":"part-1","messageID":"m1","sessionID":"s1","type":"text","text":"OK"}}}"#,
        ];
        let (msgs, stops) = parse_events(&events, "s1");
        assert_eq!(stops, vec![Some(false), Some(false), Some(false)]);
        let text = msgs
            .iter()
            .filter_map(|msg| match msg {
                StreamMessage::Text { content } => Some(content.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(text, "OK");
    }

    #[test]
    fn test_parse_model_override_accepts_provider_model_pair() {
        assert_eq!(
            parse_model_override(Some("anthropic/claude-sonnet-4-5")).unwrap(),
            Some(OpenCodeModelRef {
                provider_id: "anthropic".to_string(),
                model_id: "claude-sonnet-4-5".to_string(),
            })
        );
        assert_eq!(parse_model_override(Some("default")).unwrap(), None);
    }

    #[test]
    fn test_parse_model_override_rejects_bare_model_id() {
        let err = parse_model_override(Some("claude-sonnet-4-5")).unwrap_err();
        assert!(err.contains("providerID/modelID"));
    }

    #[test]
    fn test_prompt_body_keeps_system_separate_from_user_parts() {
        let model = OpenCodeModelRef {
            provider_id: "anthropic".to_string(),
            model_id: "claude-sonnet-4-5".to_string(),
        };
        let tools = vec!["Read".to_string(), "Bash".to_string()];
        let body = build_prompt_body(
            "visible request",
            Some("hidden system"),
            Some(&tools),
            Some(&model),
        );
        assert_eq!(body["parts"][0]["text"], "visible request");
        assert_eq!(
            body["model"],
            serde_json::json!({"providerID":"anthropic","modelID":"claude-sonnet-4-5"})
        );
        let system = body["system"].as_str().unwrap();
        assert!(system.contains("hidden system"));
        assert!(system.contains("AgentDesk allowed tools advisory"));
        assert!(
            !body["parts"][0]["text"]
                .as_str()
                .unwrap()
                .contains("hidden system")
        );
    }

    #[test]
    fn test_tool_use_emitted() {
        let data = r#"{"type":"part","properties":{"sessionID":"s1","part":{"type":"tool-use","name":"bash","input":{"command":"ls"}}}}"#;
        let (msgs, _) = parse_event(data, "s1");
        assert!(
            msgs.iter()
                .any(|m| matches!(m, StreamMessage::ToolUse { name, .. } if name == "bash"))
        );
    }

    #[test]
    fn test_tool_result_emitted() {
        let data = r#"{"type":"part","properties":{"sessionID":"s1","part":{"type":"tool-result","output":"file.txt","isError":false}}}"#;
        let (msgs, _) = parse_event(data, "s1");
        assert!(msgs
            .iter()
            .any(|m| matches!(m, StreamMessage::ToolResult { content, is_error } if content == "file.txt" && !is_error)));
    }

    #[test]
    fn test_opencode_tool_part_emits_use_and_result() {
        let data = r#"{"type":"message.part.updated","properties":{"part":{"id":"tool-1","sessionID":"s1","type":"tool","tool":"bash","state":{"status":"completed","input":{"command":"ls"},"output":"file.txt"}}}}"#;
        let (msgs, stop) = parse_event(data, "s1");
        assert_eq!(stop, Some(false));
        assert!(
            msgs.iter().any(
                |m| matches!(m, StreamMessage::ToolUse { name, input } if name == "bash" && input.contains("ls"))
            )
        );
        assert!(msgs
            .iter()
            .any(|m| matches!(m, StreamMessage::ToolResult { content, is_error } if content == "file.txt" && !is_error)));
    }

    #[test]
    fn test_session_idle_signals_done() {
        let data = r#"{"type":"session.idle","properties":{"sessionID":"s1"}}"#;
        let (_, stop) = parse_event(data, "s1");
        assert_eq!(stop, Some(true), "session.idle must return Some(true)");
    }

    #[test]
    fn test_thinking_part_emitted() {
        let data = r#"{"type":"part","properties":{"sessionID":"s1","part":{"type":"thinking","thinking":"step 1"}}}"#;
        let (msgs, _) = parse_event(data, "s1");
        assert!(msgs
            .iter()
            .any(|m| matches!(m, StreamMessage::Thinking { summary } if summary.as_deref() == Some("step 1"))));
    }

    #[test]
    fn test_reasoning_part_emitted() {
        let data = r#"{"type":"part","properties":{"sessionID":"s1","part":{"type":"reasoning","text":"step 1"}}}"#;
        let (msgs, _) = parse_event(data, "s1");
        assert!(msgs
            .iter()
            .any(|m| matches!(m, StreamMessage::Thinking { summary } if summary.as_deref() == Some("step 1"))));
    }

    #[test]
    fn test_error_event_signals_stop() {
        let data = r#"{"type":"error","properties":{"message":"boom","sessionID":"s1"}}"#;
        let (msgs, stop) = parse_event(data, "s1");
        assert_eq!(stop, Some(true));
        assert!(
            msgs.iter()
                .any(|m| matches!(m, StreamMessage::Error { .. }))
        );
    }

    #[test]
    fn test_error_event_does_not_emit_done() {
        let data =
            br#"data: {"type":"session.error","properties":{"message":"boom","sessionID":"s1"}}

"#
            .to_vec();
        let reader: BufReader<Box<dyn std::io::Read + Send>> =
            BufReader::new(Box::new(std::io::Cursor::new(data)));
        let (tx, rx) = mpsc::channel::<StreamMessage>();

        let result = consume_sse(reader, "s1", &tx, None, "http://127.0.0.1:9", "");
        drop(tx);
        let msgs = rx.try_iter().collect::<Vec<_>>();

        assert!(result.is_ok());
        assert!(
            msgs.iter()
                .any(|m| matches!(m, StreamMessage::Error { message, .. } if message == "boom"))
        );
        assert!(
            !msgs.iter().any(|m| matches!(m, StreamMessage::Done { .. })),
            "OpenCode error terminal events must not be converted into Done"
        );
    }

    #[test]
    fn test_unknown_event_ignored() {
        let data = r#"{"type":"server.connected","properties":{}}"#;
        let (msgs, stop) = parse_event(data, "s1");
        assert!(msgs.is_empty());
        assert_eq!(stop, None);
    }

    #[test]
    fn test_basic_auth_header_does_not_log_password() {
        let password = "super-secret";
        let header = build_auth_header(password);
        assert!(header.starts_with("Basic "));
        assert!(
            !header.contains(password),
            "raw password must not appear in auth header"
        );
    }

    #[test]
    fn test_resolve_opencode_path_env_override() {
        let key = "AGENTDESK_OPENCODE_PATH";
        let original = std::env::var_os(key);
        let temp = tempfile::tempdir().unwrap();
        let opencode = temp.path().join("opencode");
        std::fs::write(&opencode, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = std::fs::metadata(&opencode).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&opencode, permissions).unwrap();
        }

        unsafe { std::env::set_var(key, &opencode) };
        assert_eq!(
            resolve_opencode_path(),
            Some(opencode.to_string_lossy().into_owned())
        );
        match original {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }
}
