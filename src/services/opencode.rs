//! OpenCode provider backend — `opencode serve` HTTP/SSE integration.
//!
//! Architecture: keeps a loopback `opencode serve` warm per workspace/runtime key,
//! drives the HTTP REST + SSE API, and normalizes events to AgentDesk `StreamMessage`.

use std::collections::{HashMap, HashSet, hash_map::Entry};
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::Value;

use crate::services::agent_protocol::StreamMessage;
use crate::services::process::{configure_child_process_group, kill_pid_tree};
use crate::services::provider::{CancelToken, ProviderKind, cancel_requested, register_child_pid};
use crate::services::remote::RemoteProfile;

const HEALTH_TIMEOUT: Duration = Duration::from_secs(30);
const HEALTH_POLL_MS: u64 = 250;
const SSE_READ_TIMEOUT: Duration = Duration::from_secs(120);
const DISPOSE_TIMEOUT: Duration = Duration::from_secs(5);
const MESSAGE_RECOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const STARTUP_OUTPUT_LIMIT: usize = 8 * 1024;
const WARM_SERVER_IDLE_TTL: Duration = Duration::from_secs(20 * 60);

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

    fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

/// Make sure the spawned `opencode serve` child is reaped even when the
/// caller panics or drops the process mid-flight (e.g. the idle-recap
/// `tokio::time::timeout(spawn_blocking)` aborts the outer future while
/// the inner thread is still holding this struct).
///
/// Plain `terminate()` is still preferred — it's idempotent and `Drop`
/// only fires on the unhappy path — but this guarantees we never leak
/// a child process when the renderer times out.
impl Drop for OpenCodeServerProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Clone, Debug, Eq)]
struct OpenCodeServerKey {
    bin: String,
    working_dir: String,
}

impl PartialEq for OpenCodeServerKey {
    fn eq(&self, other: &Self) -> bool {
        self.bin == other.bin && self.working_dir == other.working_dir
    }
}

impl Hash for OpenCodeServerKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.bin.hash(state);
        self.working_dir.hash(state);
    }
}

struct OpenCodeWarmServer {
    key: OpenCodeServerKey,
    base_url: String,
    auth: String,
    startup_output: Arc<Mutex<OpenCodeStartupOutput>>,
    process: Mutex<OpenCodeServerProcess>,
    active_sessions: AtomicUsize,
    last_used: Mutex<Instant>,
}

impl OpenCodeWarmServer {
    fn id(&self) -> u32 {
        let process = self.process.lock().unwrap_or_else(|e| {
            tracing::warn!("Recovered poisoned lock for OpenCodeWarmServer::process");
            e.into_inner()
        });
        process.id()
    }

    fn is_running(&self) -> bool {
        let mut process = self.process.lock().unwrap_or_else(|e| {
            tracing::warn!("Recovered poisoned lock for OpenCodeWarmServer::process");
            e.into_inner()
        });
        process.is_running()
    }

    fn mark_used(&self) {
        let mut last_used = self.last_used.lock().unwrap_or_else(|e| {
            tracing::warn!("Recovered poisoned lock for OpenCodeWarmServer::last_used");
            e.into_inner()
        });
        *last_used = Instant::now();
    }

    fn idle_for(&self) -> Duration {
        let last_used = self.last_used.lock().unwrap_or_else(|e| {
            tracing::warn!("Recovered poisoned lock for OpenCodeWarmServer::last_used");
            e.into_inner()
        });
        Instant::now().saturating_duration_since(*last_used)
    }

    fn shutdown(&self) {
        let mut process = self.process.lock().unwrap_or_else(|e| {
            tracing::warn!("Recovered poisoned lock for OpenCodeWarmServer::process");
            e.into_inner()
        });
        shutdown_server(&mut process, &self.base_url, &self.auth);
    }
}

struct OpenCodeWarmServerLease {
    server: Arc<OpenCodeWarmServer>,
}

impl OpenCodeWarmServerLease {
    fn base_url(&self) -> &str {
        &self.server.base_url
    }

    fn auth(&self) -> &str {
        &self.server.auth
    }

    fn startup_output(&self) -> Arc<Mutex<OpenCodeStartupOutput>> {
        self.server.startup_output.clone()
    }

    fn shared_server(&self) -> Arc<OpenCodeWarmServer> {
        self.server.clone()
    }
}

impl Drop for OpenCodeWarmServerLease {
    fn drop(&mut self) {
        self.server.mark_used();
        // `fetch_sub` returns the *previous* value, so a result of `1` means
        // this drop took `active_sessions` to `0` — i.e. the final lease was
        // released. Without an `acquire_warm_server` call to drive
        // `cleanup_idle_warm_servers`, an idle warm `opencode serve` process
        // would otherwise stay resident indefinitely after `WARM_SERVER_IDLE_TTL`.
        // Schedule a one-shot disposal sweep so the last turn's cleanup does
        // not depend on a future acquire.
        if self.server.active_sessions.fetch_sub(1, Ordering::SeqCst) == 1 {
            schedule_idle_disposal();
        }
    }
}

type OpenCodeServerPool = HashMap<OpenCodeServerKey, Arc<OpenCodeWarmServer>>;

static OPENCODE_SERVER_POOL: OnceLock<Mutex<OpenCodeServerPool>> = OnceLock::new();

fn opencode_server_pool() -> &'static Mutex<OpenCodeServerPool> {
    OPENCODE_SERVER_POOL.get_or_init(|| Mutex::new(HashMap::new()))
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

    let server = acquire_warm_server(&bin, &resolution, working_dir)?;
    // Register the warm server's PID on the caller's CancelToken so timeout
    // callers (which only have `CancelToken.child_pid` to kill) can interrupt a
    // turn that is blocked before `run_session` installs the SSE cancel
    // watchdog. The watchdog itself prefers the shared-server Arc, but this
    // keeps the historical PID-based cancel path working during startup.
    register_child_pid(cancel_token, server.shared_server().id());
    let result = run_session(
        prompt,
        system_prompt,
        allowed_tools,
        model_override.as_ref(),
        server.base_url(),
        server.auth(),
        &sender,
        cancel_token,
        Some(server.startup_output()),
        Some(server.shared_server()),
    );

    match result {
        Ok(()) => Ok(()),
        Err(msg) => send_error(&sender, msg),
    }
}

// ---------------------------------------------------------------------------
// Server lifecycle
// ---------------------------------------------------------------------------

fn pool_working_dir(working_dir: &str) -> String {
    std::fs::canonicalize(working_dir)
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| working_dir.to_string())
}

fn warm_server_key(bin: &str, working_dir: &str) -> OpenCodeServerKey {
    OpenCodeServerKey {
        bin: bin.to_string(),
        working_dir: pool_working_dir(working_dir),
    }
}

fn cleanup_idle_warm_servers(pool: &mut OpenCodeServerPool) {
    let expired = pool
        .iter()
        .filter_map(|(key, server)| {
            let active = server.active_sessions.load(Ordering::SeqCst);
            (active == 0 && server.idle_for() >= WARM_SERVER_IDLE_TTL).then(|| key.clone())
        })
        .collect::<Vec<_>>();

    for key in expired {
        if let Some(server) = pool.remove(&key) {
            tracing::info!(
                "Disposing idle OpenCode warm server for {} after {}s",
                key.working_dir,
                WARM_SERVER_IDLE_TTL.as_secs()
            );
            server.shutdown();
        }
    }
}

/// Schedule a one-shot idle-disposal sweep `WARM_SERVER_IDLE_TTL` from now.
///
/// Called when the final lease on a warm server drops, so an idle
/// `opencode serve` process is reclaimed even when no further
/// `acquire_warm_server` ever runs. The sweep re-checks `active_sessions`
/// and `idle_for` under the pool lock, so a server that was re-acquired in
/// the meantime is left untouched.
fn schedule_idle_disposal() {
    thread::spawn(|| {
        thread::sleep(WARM_SERVER_IDLE_TTL);
        let pool = opencode_server_pool();
        let mut pool = pool.lock().unwrap_or_else(|e| {
            tracing::warn!("Recovered poisoned lock for OpenCode server pool");
            e.into_inner()
        });
        cleanup_idle_warm_servers(&mut pool);
    });
}

fn acquire_warm_server(
    bin: &str,
    resolution: &crate::services::platform::BinaryResolution,
    working_dir: &str,
) -> Result<OpenCodeWarmServerLease, String> {
    let key = warm_server_key(bin, working_dir);
    let pool = opencode_server_pool();
    let mut pool = pool.lock().unwrap_or_else(|e| {
        tracing::warn!("Recovered poisoned lock for OpenCode server pool");
        e.into_inner()
    });

    cleanup_idle_warm_servers(&mut pool);

    // Whether the freshly spawned server (if we end up spawning one) should be
    // published into the pool. It is set to `false` only when an existing
    // unhealthy server still has active sessions: in that case we must not
    // disturb the pooled entry, so this acquire gets a private, non-pooled
    // server whose process is reclaimed via `Arc` drop once its lease ends.
    let mut publish_spawn = true;

    if let Some(server) = pool.get(&key).cloned() {
        // Release the pool lock before probing health. `is_running` and
        // `wait_for_health` can block for up to `HEALTH_TIMEOUT` (30s) on a
        // wedged server; holding the global pool mutex across that window
        // would stall every other OpenCode request — including unrelated
        // working directories that could spawn their own server.
        drop(pool);

        let healthy = server.is_running()
            && wait_for_health(&server.base_url, &server.auth, Some(&server.startup_output))
                .is_ok();

        // Re-acquire the pool lock to commit the reuse-or-evict decision.
        let mut pool = opencode_server_pool().lock().unwrap_or_else(|e| {
            tracing::warn!("Recovered poisoned lock for OpenCode server pool");
            e.into_inner()
        });

        if healthy {
            server.active_sessions.fetch_add(1, Ordering::SeqCst);
            server.mark_used();
            tracing::debug!(
                "Reusing OpenCode warm server {} for {}",
                server.base_url,
                key.working_dir
            );
            return Ok(OpenCodeWarmServerLease { server });
        }

        // Unhealthy server. Only evict/shutdown when no other turn is using
        // it; killing a server with active sessions would interrupt those
        // in-flight turns as collateral damage.
        if server.active_sessions.load(Ordering::SeqCst) == 0 {
            // Confirm the pool still holds the same instance before removing,
            // so we don't evict a replacement inserted while the lock was
            // released.
            if pool
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(current, &server))
            {
                pool.remove(&key);
            }
            tracing::warn!(
                "Evicting stale OpenCode warm server {} for {}",
                server.base_url,
                key.working_dir
            );
            server.shutdown();
        } else {
            // Leave the active (but unhealthy) entry pooled and give this
            // acquire a private server instead of publishing over it.
            publish_spawn = false;
            tracing::warn!(
                "OpenCode warm server {} for {} failed health probe but has {} active sessions; spawning a private server for this turn instead of evicting",
                server.base_url,
                key.working_dir,
                server.active_sessions.load(Ordering::SeqCst)
            );
        }
        drop(pool);
    } else {
        drop(pool);
    }

    let port = allocate_port()?;
    let password = generate_password();
    let auth = build_auth_header(&password);
    let base_url = format!("http://127.0.0.1:{port}");
    let server_process = spawn_server(bin, resolution, port, &password, working_dir)?;
    let startup_output = server_process.startup_output.clone();

    if let Err(error) = wait_for_health(&base_url, &auth, Some(&startup_output)) {
        let mut server_process = server_process;
        shutdown_server(&mut server_process, &base_url, &auth);
        return Err(error);
    }

    let server = Arc::new(OpenCodeWarmServer {
        key: key.clone(),
        base_url,
        auth,
        startup_output,
        process: Mutex::new(server_process),
        active_sessions: AtomicUsize::new(1),
        last_used: Mutex::new(Instant::now()),
    });
    tracing::info!(
        "Started OpenCode warm server {} for {}",
        server.base_url,
        server.key.working_dir
    );

    if !publish_spawn {
        // The pooled entry is unhealthy but still serving other turns, so we
        // intentionally do not register this server. Its process is reclaimed
        // when the returned lease (and the cancel watchdog's clone) drop the
        // last `Arc`, via `OpenCodeServerProcess::drop`.
        return Ok(OpenCodeWarmServerLease { server });
    }

    // Re-acquire the pool lock to publish the freshly spawned server. The lock
    // was released across `spawn_server`/`wait_for_health`, so another acquire
    // may have raced us and already published a healthy server for this key.
    let mut pool = opencode_server_pool().lock().unwrap_or_else(|e| {
        tracing::warn!("Recovered poisoned lock for OpenCode server pool");
        e.into_inner()
    });
    match pool.entry(key) {
        Entry::Occupied(existing) => {
            // Another thread won the race. Reuse their server (taking a lease)
            // and discard ours to avoid leaking an orphaned `opencode serve`.
            let winner = existing.get().clone();
            winner.active_sessions.fetch_add(1, Ordering::SeqCst);
            winner.mark_used();
            drop(pool);
            tracing::info!(
                "Discarding duplicate OpenCode warm server {} after losing startup race",
                server.base_url
            );
            server.shutdown();
            Ok(OpenCodeWarmServerLease { server: winner })
        }
        Entry::Vacant(slot) => {
            slot.insert(server.clone());
            Ok(OpenCodeWarmServerLease { server })
        }
    }
}

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
            let mut output = output.lock().unwrap_or_else(|e| {
                tracing::warn!("Recovered poisoned lock for OpenCodeStartupOutput");
                e.into_inner()
            });
            match stream {
                StartupStream::Stdout => append_bounded(&mut output.stdout, &chunk),
                StartupStream::Stderr => append_bounded(&mut output.stderr, &chunk),
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
    let output = output.lock().unwrap_or_else(|e| {
        tracing::warn!("Recovered poisoned lock for OpenCodeStartupOutput");
        e.into_inner()
    });
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
    warm_server: Option<Arc<OpenCodeWarmServer>>,
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
        .get(&format!("{base_url}/global/event"))
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
    consume_sse(
        reader,
        &session_id,
        sender,
        cancel_token,
        base_url,
        auth,
        warm_server,
    )
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

/// Cap on the abort-session POST so neither the in-loop branch nor the
/// scoped cancel-watchdog (issue #2091) can block indefinitely if the
/// opencode server's abort handler stalls. Short enough that a stuck
/// server can't gate cancel observability for long; long enough that a
/// healthy server has room to ack.
const ABORT_SESSION_TIMEOUT: Duration = Duration::from_secs(3);

fn abort_session(base_url: &str, auth: &str, session_id: &str) {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(2))
        .timeout(ABORT_SESSION_TIMEOUT)
        .build();
    let _ = agent
        .post(&format!("{base_url}/session/{session_id}/abort"))
        .set("Authorization", auth)
        .call();
}

fn send_sse_done(sender: &Sender<StreamMessage>, session_id: &str, result: String) {
    let _ = sender.send(StreamMessage::Done {
        result,
        session_id: Some(session_id.to_string()),
    });
}

fn recover_sse_eof_from_messages(
    base_url: &str,
    auth: &str,
    session_id: &str,
    sender: &Sender<StreamMessage>,
    state: &mut SseMessageState,
    cancel_token: Option<&CancelToken>,
) -> Option<String> {
    if is_cancelled(cancel_token) {
        return None;
    }

    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(2))
        .timeout_read(MESSAGE_RECOVERY_TIMEOUT)
        .build();
    let payload: Value = agent
        .get(&format!("{base_url}/session/{session_id}/message"))
        .set("Authorization", auth)
        .call()
        .ok()?
        .into_json()
        .ok()?;

    if is_cancelled(cancel_token) {
        return None;
    }

    let result = recover_session_text_from_messages(&payload, sender, state, cancel_token);

    if is_cancelled(cancel_token) {
        return None;
    }

    result
}

fn recover_session_text_from_messages(
    payload: &Value,
    sender: &Sender<StreamMessage>,
    state: &mut SseMessageState,
    cancel_token: Option<&CancelToken>,
) -> Option<String> {
    let messages = payload
        .as_array()
        .or_else(|| payload.get("items").and_then(|items| items.as_array()))?;

    if is_cancelled(cancel_token) {
        return None;
    }
    let message = latest_recoverable_assistant_message(messages)?;
    recover_session_text_from_message(message, sender, state, cancel_token)?;

    let result = state.accumulated_text.trim().to_string();
    (!result.is_empty()).then_some(result)
}

fn latest_recoverable_assistant_message(messages: &[Value]) -> Option<&Value> {
    messages.iter().rev().find(|message| {
        let info = message
            .get("info")
            .or_else(|| message.get("message"))
            .unwrap_or(*message);
        let message_role = role_field_from_value(info).or_else(|| role_field_from_value(message));
        is_assistant_message_role(message_role) && message_has_recoverable_text_part(message, info)
    })
}

fn message_has_recoverable_text_part(message: &Value, info: &Value) -> bool {
    message
        .get("parts")
        .and_then(|parts| parts.as_array())
        .or_else(|| info.get("parts").and_then(|parts| parts.as_array()))
        .is_some_and(|parts| {
            parts.iter().any(|part| {
                part_type_from_value(part) == Some("text")
                    && part
                        .get("text")
                        .and_then(|text| text.as_str())
                        .is_some_and(|text| !text.is_empty())
            })
        })
}

fn recover_session_text_from_message(
    message: &Value,
    sender: &Sender<StreamMessage>,
    state: &mut SseMessageState,
    cancel_token: Option<&CancelToken>,
) -> Option<()> {
    let info = message
        .get("info")
        .or_else(|| message.get("message"))
        .unwrap_or(message);
    register_message_role(state, info);

    let message_role = role_field_from_value(info).or_else(|| role_field_from_value(message));
    if is_user_message_role(message_role) {
        return Some(());
    }

    let event_message_id = message_record_id_from_value(info)
        .or_else(|| message_record_id_from_value(message))
        .or_else(|| message_id_from_value(message));

    let Some(parts) = message
        .get("parts")
        .and_then(|parts| parts.as_array())
        .or_else(|| info.get("parts").and_then(|parts| parts.as_array()))
    else {
        return Some(());
    };

    for part in parts {
        if is_cancelled(cancel_token) {
            return None;
        }
        emit_recovered_text_part(part, sender, state, event_message_id, message_role);
    }

    Some(())
}

fn emit_recovered_text_part(
    part: &Value,
    sender: &Sender<StreamMessage>,
    state: &mut SseMessageState,
    event_message_id: Option<&str>,
    event_message_role: Option<&str>,
) {
    if part_type_from_value(part) != Some("text") {
        return;
    }

    let message_role = message_role_from_part(part, event_message_role)
        .map(str::to_string)
        .or_else(|| {
            message_id_from_value(part)
                .or(event_message_id)
                .and_then(|message_id| state.message_roles.get(message_id).cloned())
        });
    if is_user_message_role(message_role.as_deref()) {
        if let Some(snapshot_key) = snapshot_key_from_part(part, event_message_id) {
            suppress_text_part(state, &snapshot_key);
        }
        return;
    }

    emit_text_part(
        part,
        sender,
        state,
        event_message_id,
        message_role.as_deref(),
    );
}

// ---------------------------------------------------------------------------
// SSE stream consumption
// ---------------------------------------------------------------------------

/// Poll interval for the cancel-watchdog thread spawned by [`consume_sse`].
///
/// `BufReader::lines()` blocks on `read_line` until the SSE peer emits a
/// chunk or the OS read returns. The in-loop `is_cancelled` check only
/// fires on the *next* line, so without a watchdog a cancel signal could
/// take up to `SSE_READ_TIMEOUT` (120 s) to be observed — issue #2091.
/// 250 ms is short enough to give snappy cancel UX while keeping the
/// watchdog cost negligible (4 polls/sec, atomic load + sleep).
const CANCEL_WATCHDOG_POLL: Duration = Duration::from_millis(250);

/// Grace window between `abort_session` (graceful close) and the hard
/// `kill_pid_tree` fallback. If the SSE socket doesn't EOF within this
/// window, the watchdog kills the opencode server process so the TCP
/// connection drops and `read_line` returns. Sized to cover a healthy
/// server's abort RTT (~tens of ms) plus the main thread's tail
/// processing without dragging the worst-case cancel latency too high.
const WATCHDOG_KILL_GRACE: Duration = Duration::from_millis(500);

fn consume_sse(
    reader: BufReader<Box<dyn std::io::Read + Send>>,
    session_id: &str,
    sender: &Sender<StreamMessage>,
    cancel_token: Option<&CancelToken>,
    base_url: &str,
    auth: &str,
    warm_server: Option<Arc<OpenCodeWarmServer>>,
) -> Result<(), String> {
    // Watchdog: when the caller's `CancelToken` fires while we're parked
    // inside `reader.lines()`, the in-loop poll wouldn't notice until the
    // peer emits the next chunk. The scoped watchdog thread fires
    // `abort_session` the moment cancel is observed, which closes the
    // upstream SSE connection and unblocks the blocking `read_line` —
    // dropping observed latency from `≤ SSE_READ_TIMEOUT` to `≤
    // CANCEL_WATCHDOG_POLL + abort_session RTT`.
    //
    // Hard fallback: if `abort_session` times out / 5xx's and the SSE
    // socket still hasn't EOF'd within `WATCHDOG_KILL_GRACE`, the
    // watchdog kills the registered opencode server PID tree. The local
    // process exit forces the SSE TCP connection closed regardless of
    // server-side abort behaviour, capping the worst-case cancel
    // observation at `CANCEL_WATCHDOG_POLL + ABORT_SESSION_TIMEOUT +
    // WATCHDOG_KILL_GRACE` instead of `SSE_READ_TIMEOUT`.
    let watchdog_done = Arc::new(AtomicBool::new(false));
    thread::scope(|scope| {
        if let Some(cancel) = cancel_token {
            let stop = watchdog_done.clone();
            let watchdog_base_url = base_url.to_string();
            let watchdog_auth = auth.to_string();
            let watchdog_session_id = session_id.to_string();
            let watchdog_server = warm_server.clone();
            scope.spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if cancel_requested(Some(cancel)) {
                        // Graceful: ask opencode to close the SSE stream.
                        // Bounded by `ABORT_SESSION_TIMEOUT` (3 s).
                        abort_session(&watchdog_base_url, &watchdog_auth, &watchdog_session_id);
                        // Give the main thread a brief moment to observe
                        // the peer-closed connection and exit `read_line`.
                        let kill_deadline = Instant::now() + WATCHDOG_KILL_GRACE;
                        while Instant::now() < kill_deadline && !stop.load(Ordering::Relaxed) {
                            thread::sleep(Duration::from_millis(50));
                        }
                        // Hard fallback: if this warm server is not
                        // shared by another active turn, kill the server
                        // process tree so the SSE socket drops and
                        // `read_line` returns. When multiple OpenCode
                        // turns share the same warm server, killing the
                        // server would interrupt unrelated sessions, so
                        // skip the hard-kill fallback and let the bounded
                        // read timeout surface the cancel/error instead.
                        if !stop.load(Ordering::Relaxed)
                            && let Some(server) = watchdog_server.as_ref()
                        {
                            if server.active_sessions.load(Ordering::SeqCst) <= 1 {
                                kill_pid_tree(server.id());
                            } else {
                                tracing::warn!(
                                    "Skipping OpenCode hard-kill cancel fallback for shared warm server {} with {} active sessions",
                                    server.base_url,
                                    server.active_sessions.load(Ordering::SeqCst)
                                );
                            }
                        }
                        return;
                    }
                    thread::sleep(CANCEL_WATCHDOG_POLL);
                }
            });
        }
        let result = consume_sse_inner(reader, session_id, sender, cancel_token, base_url, auth);
        // Tell the watchdog to exit so `thread::scope` joins promptly even
        // on the happy path. Done *before* the scope's implicit join.
        watchdog_done.store(true, Ordering::Relaxed);
        result
    })
}

fn consume_sse_inner(
    reader: BufReader<Box<dyn std::io::Read + Send>>,
    session_id: &str,
    sender: &Sender<StreamMessage>,
    cancel_token: Option<&CancelToken>,
    base_url: &str,
    auth: &str,
) -> Result<(), String> {
    let mut state = SseMessageState::default();
    let mut current_data = String::new();
    let mut terminal_seen = false;
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
                if terminal_seen {
                    break;
                }
                // If the watchdog tripped abort_session, the upstream
                // connection closes and `read_line` returns an error here.
                // Surface the cancel-shaped error so callers (and tests)
                // see the same shape as the in-loop cancel branch.
                if is_cancelled(cancel_token) {
                    return Err("OpenCode request cancelled".to_string());
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
                    terminal_seen = true;
                    if !state.terminal_error {
                        send_sse_done(
                            sender,
                            session_id,
                            state.accumulated_text.trim().to_string(),
                        );
                    }
                    break;
                }
            }
        }
    }

    if !terminal_seen && !current_data.is_empty() {
        let should_stop =
            process_sse_event(&current_data, session_id, sender, &mut state).unwrap_or(false);
        if should_stop {
            terminal_seen = true;
            if !state.terminal_error {
                send_sse_done(
                    sender,
                    session_id,
                    state.accumulated_text.trim().to_string(),
                );
            }
        }
    }

    if !terminal_seen {
        let result = state.accumulated_text.trim().to_string();
        if !result.is_empty() {
            send_sse_done(sender, session_id, result);
            return Ok(());
        }
        if is_cancelled(cancel_token) {
            abort_session(base_url, auth, session_id);
            return Err("OpenCode request cancelled".to_string());
        }
        if let Some(result) = recover_sse_eof_from_messages(
            base_url,
            auth,
            session_id,
            sender,
            &mut state,
            cancel_token,
        ) {
            if is_cancelled(cancel_token) {
                abort_session(base_url, auth, session_id);
                return Err("OpenCode request cancelled".to_string());
            }
            send_sse_done(sender, session_id, result);
            return Ok(());
        }
        if is_cancelled(cancel_token) {
            abort_session(base_url, auth, session_id);
            return Err("OpenCode request cancelled".to_string());
        }
        return Err("OpenCode stream ended without a terminal event".to_string());
    }

    Ok(())
}

#[derive(Default)]
struct SseMessageState {
    accumulated_text: String,
    message_roles: HashMap<String, String>,
    text_part_snapshots: HashMap<String, String>,
    part_types: HashMap<String, String>,
    pending_text_deltas: HashMap<String, String>,
    prompt_echo_candidates: HashMap<String, String>,
    suppressed_text_parts: HashSet<String>,
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

fn text_part_snapshot_key(part_id: &str, message_id: Option<&str>) -> String {
    message_id
        .map(|message_id| format!("{message_id}:{part_id}"))
        .unwrap_or_else(|| part_id.to_string())
}

fn move_text_tracking_value(
    map: &mut HashMap<String, String>,
    from_key: &str,
    to_key: &str,
    merge_as_prefix: bool,
) {
    if from_key == to_key {
        return;
    }

    let Some(value) = map.remove(from_key) else {
        return;
    };

    match map.entry(to_key.to_string()) {
        Entry::Vacant(entry) => {
            entry.insert(value);
        }
        Entry::Occupied(mut entry) if merge_as_prefix => {
            let mut merged = value;
            merged.push_str(entry.get());
            *entry.get_mut() = merged;
        }
        Entry::Occupied(_) => {}
    }
}

fn move_text_tracking_set(set: &mut HashSet<String>, from_key: &str, to_key: &str) {
    if from_key == to_key {
        return;
    }
    if set.remove(from_key) {
        set.insert(to_key.to_string());
    }
}

fn text_part_tracking_key(
    state: &mut SseMessageState,
    part_id: &str,
    message_id: Option<&str>,
) -> String {
    let snapshot_key = text_part_snapshot_key(part_id, message_id);
    if message_id.is_none() {
        return snapshot_key;
    }

    let unqualified_key = text_part_snapshot_key(part_id, None);
    move_text_tracking_value(
        &mut state.pending_text_deltas,
        &unqualified_key,
        &snapshot_key,
        true,
    );
    move_text_tracking_value(
        &mut state.text_part_snapshots,
        &unqualified_key,
        &snapshot_key,
        false,
    );
    move_text_tracking_value(
        &mut state.part_types,
        &unqualified_key,
        &snapshot_key,
        false,
    );
    move_text_tracking_value(
        &mut state.prompt_echo_candidates,
        &unqualified_key,
        &snapshot_key,
        true,
    );
    move_text_tracking_set(
        &mut state.suppressed_text_parts,
        &unqualified_key,
        &snapshot_key,
    );

    snapshot_key
}

fn part_id_from_value(value: &Value) -> Option<&str> {
    value
        .get("id")
        .or_else(|| value.get("partID"))
        .or_else(|| value.get("partId"))
        .or_else(|| value.get("part_id"))
        .and_then(|v| v.as_str())
}

fn part_type_from_value(value: &Value) -> Option<&str> {
    value.get("type").and_then(|v| v.as_str())
}

fn known_message_role(role: &str) -> Option<&str> {
    match role {
        "assistant" | "user" => Some(role),
        _ => None,
    }
}

fn role_field_from_value(value: &Value) -> Option<&str> {
    value
        .get("role")
        .and_then(|v| v.as_str())
        .and_then(known_message_role)
}

fn message_role_from_props(props: &Value) -> Option<&str> {
    props
        .get("message")
        .and_then(role_field_from_value)
        .or_else(|| {
            props
                .get("messageRole")
                .or_else(|| props.get("message_role"))
                .and_then(|v| v.as_str())
                .and_then(known_message_role)
        })
        .or_else(|| role_field_from_value(props))
}

fn message_role_from_part<'a>(
    part: &'a Value,
    event_message_role: Option<&'a str>,
) -> Option<&'a str> {
    part.get("message")
        .and_then(role_field_from_value)
        .or_else(|| {
            part.get("messageRole")
                .or_else(|| part.get("message_role"))
                .and_then(|v| v.as_str())
                .and_then(known_message_role)
        })
        .or_else(|| role_field_from_value(part))
        .or(event_message_role)
}

fn is_user_message_role(role: Option<&str>) -> bool {
    matches!(role, Some("user"))
}

fn is_assistant_message_role(role: Option<&str>) -> bool {
    matches!(role, Some("assistant"))
}

fn is_agentdesk_user_prompt_echo(text: &str) -> bool {
    let Some(first_line) = text.lines().next() else {
        return false;
    };
    first_line.starts_with("[User: ") && first_line.contains(" (ID: ") && first_line.ends_with(']')
}

fn could_be_agentdesk_user_prompt_echo_prefix(text: &str) -> bool {
    const PREFIX: &str = "[User: ";
    if text.is_empty() {
        return false;
    }
    PREFIX.starts_with(text) || (text.starts_with(PREFIX) && !text.contains('\n'))
}

fn suppress_text_part(state: &mut SseMessageState, snapshot_key: &str) {
    state.pending_text_deltas.remove(snapshot_key);
    state.text_part_snapshots.remove(snapshot_key);
    state.prompt_echo_candidates.remove(snapshot_key);
    state.suppressed_text_parts.insert(snapshot_key.to_string());
}

enum TextDeltaVisibility {
    Emitted(String),
    Deferred,
    Suppressed,
}

fn combine_prompt_echo_candidate(previous: Option<String>, text: &str) -> String {
    match previous {
        Some(previous) if text.starts_with(&previous) => text.to_string(),
        Some(previous) => format!("{previous}{text}"),
        None => text.to_string(),
    }
}

fn append_text_delta_if_visible(
    sender: &Sender<StreamMessage>,
    state: &mut SseMessageState,
    message_role: Option<&str>,
    snapshot_key: Option<&str>,
    text: &str,
) -> TextDeltaVisibility {
    if text.is_empty() {
        return TextDeltaVisibility::Emitted(String::new());
    }

    if let Some(snapshot_key) = snapshot_key {
        if state.suppressed_text_parts.contains(snapshot_key) {
            return TextDeltaVisibility::Suppressed;
        }
    }

    if is_user_message_role(message_role) {
        if let Some(snapshot_key) = snapshot_key {
            suppress_text_part(state, snapshot_key);
        }
        return TextDeltaVisibility::Suppressed;
    }

    if is_assistant_message_role(message_role) {
        let visible_text = snapshot_key
            .and_then(|snapshot_key| state.prompt_echo_candidates.remove(snapshot_key))
            .map(|previous| combine_prompt_echo_candidate(Some(previous), text))
            .unwrap_or_else(|| text.to_string());
        append_text_delta(sender, state, &visible_text);
        return TextDeltaVisibility::Emitted(visible_text);
    }

    if let Some(snapshot_key) = snapshot_key {
        let previous = state.prompt_echo_candidates.remove(snapshot_key);
        let combined = combine_prompt_echo_candidate(previous, text);

        if is_agentdesk_user_prompt_echo(&combined) {
            suppress_text_part(state, snapshot_key);
            return TextDeltaVisibility::Suppressed;
        }

        if could_be_agentdesk_user_prompt_echo_prefix(&combined) {
            state
                .prompt_echo_candidates
                .insert(snapshot_key.to_string(), combined);
            return TextDeltaVisibility::Deferred;
        }

        append_text_delta(sender, state, &combined);
        return TextDeltaVisibility::Emitted(combined);
    } else if is_agentdesk_user_prompt_echo(text) {
        return TextDeltaVisibility::Suppressed;
    }

    append_text_delta(sender, state, text);
    TextDeltaVisibility::Emitted(text.to_string())
}

fn update_visible_snapshot(snapshot_text: &mut String, visibility: TextDeltaVisibility) -> bool {
    match visibility {
        TextDeltaVisibility::Emitted(text) => {
            snapshot_text.push_str(&text);
            true
        }
        TextDeltaVisibility::Deferred => true,
        TextDeltaVisibility::Suppressed => false,
    }
}

fn is_reasoning_part_type(part_type: &str) -> bool {
    matches!(part_type, "thinking" | "redactedThinking" | "reasoning")
}

fn part_type_from_delta_props<'a>(
    props: &'a Value,
    state: &'a SseMessageState,
    snapshot_key: Option<&str>,
) -> Option<&'a str> {
    props
        .get("part")
        .and_then(part_type_from_value)
        .or_else(|| props.get("partType").and_then(|v| v.as_str()))
        .or_else(|| props.get("part_type").and_then(|v| v.as_str()))
        .or_else(|| snapshot_key.and_then(|key| state.part_types.get(key).map(String::as_str)))
}

fn snapshot_key_from_part(part: &Value, event_message_id: Option<&str>) -> Option<String> {
    let part_id = part_id_from_value(part)?;
    Some(text_part_snapshot_key(
        part_id,
        message_id_from_value(part).or(event_message_id),
    ))
}

fn register_part_type(
    part: &Value,
    state: &mut SseMessageState,
    event_message_id: Option<&str>,
) -> Option<String> {
    let part_type = part_type_from_value(part)?;
    let part_id = part_id_from_value(part)?;
    let snapshot_key = text_part_tracking_key(
        state,
        part_id,
        message_id_from_value(part).or(event_message_id),
    );
    state
        .part_types
        .insert(snapshot_key.clone(), part_type.to_string());
    if is_reasoning_part_type(part_type) || part_type != "text" {
        state.pending_text_deltas.remove(&snapshot_key);
        state.prompt_echo_candidates.remove(&snapshot_key);
    }
    Some(snapshot_key)
}

fn take_pending_text_delta(state: &mut SseMessageState, snapshot_key: &str) -> Option<String> {
    let pending = state.pending_text_deltas.remove(snapshot_key)?;
    if pending.is_empty() {
        return None;
    }
    Some(pending)
}

fn message_id_from_value(value: &Value) -> Option<&str> {
    value
        .get("messageID")
        .or_else(|| value.get("messageId"))
        .or_else(|| value.get("message_id"))
        .and_then(|v| v.as_str())
}

fn message_record_id_from_value(value: &Value) -> Option<&str> {
    value
        .get("id")
        .or_else(|| value.get("messageID"))
        .or_else(|| value.get("messageId"))
        .or_else(|| value.get("message_id"))
        .and_then(|v| v.as_str())
}

fn register_message_role(state: &mut SseMessageState, message: &Value) {
    let Some(message_id) = message_record_id_from_value(message) else {
        return;
    };
    let Some(role) = role_field_from_value(message) else {
        return;
    };
    state
        .message_roles
        .insert(message_id.to_string(), role.to_string());
}

fn emit_text_part(
    part: &Value,
    sender: &Sender<StreamMessage>,
    state: &mut SseMessageState,
    event_message_id: Option<&str>,
    message_role: Option<&str>,
) {
    let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
    if text.is_empty() {
        return;
    }

    let part_id = part_id_from_value(part);
    let message_id = message_id_from_value(part).or(event_message_id);
    let snapshot_key = part_id.map(|part_id| text_part_tracking_key(state, part_id, message_id));

    if let Some(snapshot_key) = snapshot_key.as_deref() {
        if state.suppressed_text_parts.contains(snapshot_key) {
            return;
        }
    }

    let Some(snapshot_key) = snapshot_key else {
        let _ = append_text_delta_if_visible(sender, state, message_role, None, text);
        return;
    };
    state
        .part_types
        .insert(snapshot_key.clone(), "text".to_string());
    let pending = take_pending_text_delta(state, &snapshot_key);

    let mut snapshot_text = state
        .text_part_snapshots
        .get(&snapshot_key)
        .cloned()
        .unwrap_or_default();
    let mut should_store_snapshot = true;
    if let Some(pending) = pending {
        should_store_snapshot = update_visible_snapshot(
            &mut snapshot_text,
            append_text_delta_if_visible(
                sender,
                state,
                message_role,
                Some(&snapshot_key),
                &pending,
            ),
        );
    }
    if should_store_snapshot {
        let delta = if text.starts_with(&snapshot_text) {
            &text[snapshot_text.len()..]
        } else {
            snapshot_text.clear();
            text
        };
        should_store_snapshot = update_visible_snapshot(
            &mut snapshot_text,
            append_text_delta_if_visible(sender, state, message_role, Some(&snapshot_key), delta),
        );
    }
    if should_store_snapshot && !state.suppressed_text_parts.contains(&snapshot_key) {
        state
            .text_part_snapshots
            .insert(snapshot_key, snapshot_text);
    }
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

    let tool_use_id = opencode_tool_call_id(part);
    if !input.is_empty() || matches!(status, "pending" | "running" | "completed" | "error") {
        let _ = sender.send(StreamMessage::ToolUse {
            name,
            input,
            tool_use_id: tool_use_id.clone(),
        });
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
            tool_use_id,
        });
    }
}

/// Extracts the OpenCode tool-call identifier (`callID`/`callId`/`call_id`/`id`)
/// from a tool part so a `ToolResult` can be paired back to its `ToolUse`.
fn opencode_tool_call_id(part: &Value) -> Option<String> {
    ["callID", "callId", "call_id", "id"]
        .into_iter()
        .find_map(|key| part.get(key).and_then(|v| v.as_str()))
        .map(str::to_string)
}

fn emit_part(
    part: &Value,
    sender: &Sender<StreamMessage>,
    state: &mut SseMessageState,
    event_message_id: Option<&str>,
    event_message_role: Option<&str>,
) {
    let part_type = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let message_role = message_role_from_part(part, event_message_role)
        .map(str::to_string)
        .or_else(|| {
            message_id_from_value(part)
                .or(event_message_id)
                .and_then(|message_id| state.message_roles.get(message_id).cloned())
        });
    register_part_type(part, state, event_message_id);
    if is_user_message_role(message_role.as_deref()) {
        if let Some(snapshot_key) = snapshot_key_from_part(part, event_message_id) {
            suppress_text_part(state, &snapshot_key);
        }
        return;
    }

    match part_type {
        "text" => emit_text_part(
            part,
            sender,
            state,
            event_message_id,
            message_role.as_deref(),
        ),
        "thinking" | "redactedThinking" | "reasoning" => {
            let _ = sender.send(StreamMessage::redacted_thinking());
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
                tool_use_id: opencode_tool_call_id(part),
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
            let _ = sender.send(StreamMessage::ToolResult {
                content,
                is_error,
                tool_use_id: opencode_tool_call_id(part),
            });
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
    let raw_event: Value = serde_json::from_str(data).ok()?;
    let event = raw_event
        .get("payload")
        .filter(|payload| payload.get("type").is_some())
        .unwrap_or(&raw_event);
    let event_type = event.get("type").and_then(|v| v.as_str())?;
    let props = event.get("properties");

    // Filter events by sessionID where applicable
    let event_session = props
        .and_then(|p| p.get("sessionID").and_then(|v| v.as_str()))
        .or_else(|| props.and_then(|p| p.get("sessionId").and_then(|v| v.as_str())))
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

        "message.updated" => {
            let props = props?;
            if let Some(message) = props.get("info").or_else(|| props.get("message")) {
                register_message_role(state, message);
            }
            Some(false)
        }

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
            let props = props?;
            let part = props.get("part")?;
            let message_id = message_id_from_value(props);
            let message_role = message_role_from_props(props);
            emit_part(part, sender, state, message_id, message_role);
            Some(false)
        }

        "message.part.delta" => {
            if props.and_then(|p| p.get("field")).and_then(|v| v.as_str()) == Some("text") {
                let props = props?;
                let delta = props.get("delta").and_then(|v| v.as_str())?;
                let part_id = props
                    .get("partID")
                    .or_else(|| props.get("partId"))
                    .or_else(|| props.get("part_id"))
                    .and_then(|v| v.as_str())
                    .or_else(|| props.get("part").and_then(part_id_from_value));
                let message_id = message_id_from_value(props)
                    .or_else(|| props.get("part").and_then(message_id_from_value));
                let message_role = props
                    .get("part")
                    .and_then(|part| message_role_from_part(part, message_role_from_props(props)))
                    .or_else(|| message_role_from_props(props))
                    .map(str::to_string)
                    .or_else(|| {
                        message_id
                            .and_then(|message_id| state.message_roles.get(message_id).cloned())
                    });
                let snapshot_key =
                    part_id.map(|part_id| text_part_tracking_key(state, part_id, message_id));
                if is_user_message_role(message_role.as_deref()) {
                    if let Some(snapshot_key) = snapshot_key.as_deref() {
                        suppress_text_part(state, snapshot_key);
                    }
                    return Some(false);
                }
                let part_type = part_type_from_delta_props(props, state, snapshot_key.as_deref())
                    .map(str::to_string);

                match (snapshot_key, part_type.as_deref()) {
                    (Some(snapshot_key), Some("text")) => {
                        let pending = take_pending_text_delta(state, &snapshot_key);
                        state
                            .part_types
                            .insert(snapshot_key.clone(), "text".to_string());
                        let mut snapshot_text = state
                            .text_part_snapshots
                            .remove(&snapshot_key)
                            .unwrap_or_default();
                        let mut should_store_snapshot = true;
                        if let Some(pending) = pending {
                            should_store_snapshot = update_visible_snapshot(
                                &mut snapshot_text,
                                append_text_delta_if_visible(
                                    sender,
                                    state,
                                    message_role.as_deref(),
                                    Some(&snapshot_key),
                                    &pending,
                                ),
                            );
                        }
                        if should_store_snapshot {
                            should_store_snapshot = update_visible_snapshot(
                                &mut snapshot_text,
                                append_text_delta_if_visible(
                                    sender,
                                    state,
                                    message_role.as_deref(),
                                    Some(&snapshot_key),
                                    delta,
                                ),
                            );
                        }
                        if should_store_snapshot
                            && !state.suppressed_text_parts.contains(&snapshot_key)
                        {
                            state
                                .text_part_snapshots
                                .insert(snapshot_key, snapshot_text);
                        }
                    }
                    (Some(snapshot_key), Some(part_type)) if is_reasoning_part_type(part_type) => {
                        state.pending_text_deltas.remove(&snapshot_key);
                        state.prompt_echo_candidates.remove(&snapshot_key);
                    }
                    (Some(snapshot_key), Some(_)) => {
                        state.pending_text_deltas.remove(&snapshot_key);
                        state.prompt_echo_candidates.remove(&snapshot_key);
                    }
                    (Some(snapshot_key), None) => {
                        state
                            .pending_text_deltas
                            .entry(snapshot_key)
                            .or_default()
                            .push_str(delta);
                    }
                    (None, Some("text")) => {
                        let _ = append_text_delta_if_visible(
                            sender,
                            state,
                            message_role.as_deref(),
                            None,
                            delta,
                        );
                    }
                    (None, _) => {}
                }
            }
            Some(false)
        }

        "message.part.updated" => {
            let props = props?;
            let part = props.get("part")?;
            let message_id = message_id_from_value(props);
            let message_role = message_role_from_props(props);
            emit_part(part, sender, state, message_id, message_role);
            Some(false)
        }

        "message.completed" => {
            // Full assembled message — emit any final text parts not yet streamed
            if let Some(message) = props.and_then(|p| p.get("message")) {
                register_message_role(state, message);
                let message_role =
                    role_field_from_value(message)
                        .map(str::to_string)
                        .or_else(|| {
                            message_record_id_from_value(message)
                                .and_then(|message_id| state.message_roles.get(message_id).cloned())
                        });
                if is_user_message_role(message_role.as_deref()) {
                    return Some(false);
                }
                if let Some(parts) = message.get("parts").and_then(|p| p.as_array()) {
                    for part in parts {
                        let part_message_role =
                            message_role_from_part(part, message_role.as_deref())
                                .map(str::to_string)
                                .or_else(|| {
                                    message_id_from_value(part).and_then(|message_id| {
                                        state.message_roles.get(message_id).cloned()
                                    })
                                });
                        if is_user_message_role(part_message_role.as_deref()) {
                            continue;
                        }
                        if part.get("type").and_then(|v| v.as_str()) == Some("text") {
                            if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                                // Only emit if we haven't already streamed it
                                if state.accumulated_text.is_empty() && !text.trim().is_empty() {
                                    append_text_delta_if_visible(
                                        sender,
                                        state,
                                        part_message_role.as_deref(),
                                        None,
                                        text,
                                    );
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
    let addr = listener
        .local_addr()
        .map_err(|e| format!("Failed to resolve local address: {e}"))?;
    Ok(addr.port())
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

    #[test]
    fn opencode_warm_server_key_canonicalizes_equivalent_working_dirs() {
        let cwd = std::env::current_dir().expect("current dir");
        let key_from_dot = warm_server_key("opencode", ".");
        let key_from_cwd = warm_server_key("opencode", &cwd.to_string_lossy());

        assert_eq!(key_from_dot, key_from_cwd);
    }

    #[test]
    fn opencode_warm_server_key_separates_working_dirs() {
        let root = std::env::temp_dir().join(format!(
            "agentdesk-opencode-key-test-{}",
            std::process::id()
        ));
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir_all(&first).expect("create first temp dir");
        std::fs::create_dir_all(&second).expect("create second temp dir");

        let first_key = warm_server_key("opencode", &first.to_string_lossy());
        let second_key = warm_server_key("opencode", &second.to_string_lossy());

        assert_ne!(first_key, second_key);

        let _ = std::fs::remove_dir_all(root);
    }
}
