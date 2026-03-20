use std::sync::Arc;
use std::time::Instant;

use poise::serenity_prelude as serenity;
use serenity::ChannelId;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::SharedData;

/// Per-provider snapshot for the health response.
struct ProviderEntry {
    name: String,
    shared: Arc<SharedData>,
}

/// Registry that providers register with so the health server can query all of them.
/// Also holds Discord HTTP clients for agent-to-agent message routing.
pub struct HealthRegistry {
    providers: tokio::sync::Mutex<Vec<ProviderEntry>>,
    started_at: Instant,
    /// Discord HTTP clients keyed by provider name (for sending messages via correct bot)
    discord_http: tokio::sync::Mutex<Vec<(String, Arc<serenity::Http>)>>,
    /// Dedicated HTTP client for the announce bot (agent-to-agent routing).
    /// This bot's messages are accepted by all agents' allowed_bot_ids.
    announce_http: tokio::sync::Mutex<Option<Arc<serenity::Http>>>,
    /// Dedicated HTTP client for the notify bot (info-only notifications).
    /// Agents do NOT process notify bot messages — use for non-actionable alerts.
    notify_http: tokio::sync::Mutex<Option<Arc<serenity::Http>>>,
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self {
            providers: tokio::sync::Mutex::new(Vec::new()),
            started_at: Instant::now(),
            discord_http: tokio::sync::Mutex::new(Vec::new()),
            announce_http: tokio::sync::Mutex::new(None),
            notify_http: tokio::sync::Mutex::new(None),
        }
    }

    pub(super) async fn register(&self, name: String, shared: Arc<SharedData>) {
        self.providers
            .lock()
            .await
            .push(ProviderEntry { name, shared });
    }

    pub(super) async fn register_http(&self, provider: String, http: Arc<serenity::Http>) {
        self.discord_http.lock().await.push((provider, http));
    }
}

/// Start the health check HTTP server on the given port.
/// Runs forever — intended to be spawned as a background tokio task.
pub async fn serve(registry: Arc<HealthRegistry>, port: u16) {
    // Load announce + notify bot tokens for message routing.
    // Announce bot: agent-to-agent (agents process these messages)
    // Notify bot: info-only alerts (agents do NOT respond)
    if let Some(root) = super::runtime_store::agentdesk_root() {
        for (bot_name, field) in [
            ("announce", &registry.announce_http),
            ("notify", &registry.notify_http),
        ] {
            let new_path = root
                .join("credential")
                .join(format!("{bot_name}_bot_token"));
            if let Ok(token) = std::fs::read_to_string(&new_path) {
                let token = token.trim().to_string();
                if !token.is_empty() {
                    let http = Arc::new(serenity::Http::new(&format!("Bot {token}")));
                    *field.lock().await = Some(http);
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    let emoji = if bot_name == "announce" {
                        "📢"
                    } else {
                        "🔔"
                    };
                    println!("  [{ts}] {emoji} {bot_name} bot loaded for /api/send routing");
                }
            }
        }
    }

    let addr = format!("127.0.0.1:{port}");
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] 🩺 Health check server listening on {addr}");
            l
        }
        Err(e) => {
            eprintln!("  ⚠ Health check server failed to bind {addr}: {e}");
            return;
        }
    };

    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(conn) => conn,
            Err(_) => continue,
        };

        let registry = registry.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 8192]; // Larger buffer for POST bodies
            let n = match stream.read(&mut buf).await {
                Ok(n) => n,
                Err(_) => return,
            };
            let request = String::from_utf8_lossy(&buf[..n]);

            // Parse first line: "GET /api/health HTTP/1.1"
            let first_line = request.lines().next().unwrap_or("");
            let method = first_line.split_whitespace().next().unwrap_or("");
            let path = first_line.split_whitespace().nth(1).unwrap_or("");

            let (status, body) = match (method, path) {
                ("GET", "/api/health") => {
                    let json = build_health_json(&registry).await;
                    let healthy = is_healthy(&registry).await;
                    let code = if healthy {
                        "200 OK"
                    } else {
                        "503 Service Unavailable"
                    };
                    (code, json)
                }
                ("POST", "/api/send") => {
                    // Extract JSON body (after \r\n\r\n)
                    let body_str = request.split("\r\n\r\n").nth(1).unwrap_or("");
                    handle_send(&registry, body_str).await
                }
                ("POST", "/api/senddm") => {
                    let body_str = request.split("\r\n\r\n").nth(1).unwrap_or("");
                    handle_senddm(&registry, body_str).await
                }
                ("POST", "/api/session/start") => {
                    let body_str = request.split("\r\n\r\n").nth(1).unwrap_or("");
                    handle_session_start(&registry, body_str).await
                }
                _ => ("404 Not Found", r#"{"error":"not found"}"#.to_string()),
            };

            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
    }
}

async fn build_health_json(registry: &HealthRegistry) -> String {
    let uptime_secs = registry.started_at.elapsed().as_secs();
    let version = env!("CARGO_PKG_VERSION");

    let providers = registry.providers.lock().await;
    let mut provider_entries = Vec::new();

    for entry in providers.iter() {
        // Use try_lock to avoid blocking the health endpoint when core is
        // held by a long-running turn.  Fall back to atomic counters so the
        // health server always responds promptly.
        let (active_turns, queue_depth, session_count) = match entry.shared.core.try_lock() {
            Ok(data) => {
                let at = data.cancel_tokens.len();
                let qd: usize = data.intervention_queue.values().map(|q| q.len()).sum();
                let sc = data.sessions.len();
                (at, qd, sc)
            }
            Err(_) => {
                // Lock contended — approximate from atomics
                let at = entry
                    .shared
                    .global_active
                    .load(std::sync::atomic::Ordering::Relaxed)
                    as usize;
                (at, 0, 0)
            }
        };

        let restart_pending = entry
            .shared
            .restart_pending
            .load(std::sync::atomic::Ordering::Relaxed);
        let connected = entry
            .shared
            .bot_connected
            .load(std::sync::atomic::Ordering::Relaxed);
        let last_turn_at = entry
            .shared
            .last_turn_at
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .map(|t| format!(r#""{}""#, t))
            .unwrap_or_else(|| "null".to_string());

        provider_entries.push(format!(
            r#"{{"name":"{}","connected":{},"active_turns":{},"queue_depth":{},"sessions":{},"restart_pending":{},"last_turn_at":{}}}"#,
            entry.name, connected, active_turns, queue_depth, session_count, restart_pending, last_turn_at
        ));
    }

    let global_active = if let Some(p) = providers.first() {
        p.shared
            .global_active
            .load(std::sync::atomic::Ordering::Relaxed)
    } else {
        0
    };
    let global_finalizing = if let Some(p) = providers.first() {
        p.shared
            .global_finalizing
            .load(std::sync::atomic::Ordering::Relaxed)
    } else {
        0
    };

    format!(
        r#"{{"status":"{}","version":"{}","uptime_secs":{},"global_active":{},"global_finalizing":{},"providers":[{}]}}"#,
        if is_healthy_inner(&providers) {
            "healthy"
        } else {
            "unhealthy"
        },
        version,
        uptime_secs,
        global_active,
        global_finalizing,
        provider_entries.join(",")
    )
}

async fn is_healthy(registry: &HealthRegistry) -> bool {
    let providers = registry.providers.lock().await;
    is_healthy_inner(&providers)
}

fn is_healthy_inner(providers: &[ProviderEntry]) -> bool {
    // Unhealthy if no providers registered (startup not complete)
    if providers.is_empty() {
        return false;
    }
    for p in providers {
        // Unhealthy if any provider hasn't connected to Discord gateway yet
        if !p
            .shared
            .bot_connected
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return false;
        }
        // Unhealthy if restart is pending (draining)
        if p.shared
            .restart_pending
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return false;
        }
    }
    true
}

/// Resolve the bot HTTP client by name (announce or notify).
async fn resolve_bot_http(
    registry: &HealthRegistry,
    bot: &str,
) -> Result<Arc<serenity::Http>, (&'static str, String)> {
    let (lock, bot_label) = match bot {
        "notify" => (&registry.notify_http, "notify"),
        _ => (&registry.announce_http, "announce"),
    };
    let guard = lock.lock().await;
    match guard.as_ref() {
        Some(http) => {
            let http = http.clone();
            drop(guard);
            Ok(http)
        }
        None => {
            drop(guard);
            Err((
                "503 Service Unavailable",
                format!(
                    r#"{{"ok":false,"error":"{bot_label} bot not configured (missing credential/{bot_label}_bot_token)"}}"#
                ),
            ))
        }
    }
}

/// Handle POST /api/send — agent-to-agent native routing.
/// Accepts JSON: {"target":"channel:<id>", "content":"...", "source":"role-id", "bot":"announce|notify"}
async fn handle_send<'a>(registry: &HealthRegistry, body: &str) -> (&'a str, String) {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return (
            "400 Bad Request",
            r#"{"ok":false,"error":"invalid JSON"}"#.to_string(),
        );
    };

    let target = json.get("target").and_then(|v| v.as_str()).unwrap_or("");
    let content = json.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let source = json
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let bot = json
        .get("bot")
        .and_then(|v| v.as_str())
        .unwrap_or("announce");

    if content.is_empty() {
        return (
            "400 Bad Request",
            r#"{"ok":false,"error":"content is required"}"#.to_string(),
        );
    }

    // Parse "channel:<id>" format
    let channel_id_raw = if let Some(id_str) = target.strip_prefix("channel:") {
        id_str.trim().parse::<u64>().ok()
    } else {
        target.trim().parse::<u64>().ok()
    };

    let Some(channel_id_raw) = channel_id_raw else {
        return (
            "400 Bad Request",
            r#"{"ok":false,"error":"invalid target format (use channel:<id>)"}"#.to_string(),
        );
    };

    let channel_id = ChannelId::new(channel_id_raw);

    // Validate source is a known agent role_id or internal system source
    const INTERNAL_SOURCES: &[&str] = &[
        "kanban-rules",
        "triage-rules",
        "review-automation",
        "auto-queue",
        "pipeline",
        "system",
    ];
    if !INTERNAL_SOURCES.contains(&source) && !super::settings::is_known_agent(source) {
        return (
            "403 Forbidden",
            format!(
                r#"{{"ok":false,"error":"unknown source role: {}"}}"#,
                source
            ),
        );
    }

    // Verify target channel exists in role-map (authorization check)
    if super::settings::resolve_role_binding(channel_id, None).is_none() {
        return (
            "403 Forbidden",
            r#"{"ok":false,"error":"channel not in role-map"}"#.to_string(),
        );
    }

    // Select bot: "announce" (default, agents respond) or "notify" (info-only, agents ignore)
    let http = match resolve_bot_http(registry, bot).await {
        Ok(h) => h,
        Err(resp) => return resp,
    };

    match channel_id.say(&*http, content).await {
        Ok(_) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            let emoji = if bot == "notify" { "🔔" } else { "📨" };
            println!("  [{ts}] {emoji} ROUTE: [{source}] → channel {channel_id} (bot={bot})");
            (
                "200 OK",
                format!(
                    r#"{{"ok":true,"target":"channel:{}","source":"{}","bot":"{}"}}"#,
                    channel_id, source, bot
                ),
            )
        }
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!("  [{ts}] ⚠ ROUTE: failed to send to channel {channel_id}: {e}");
            (
                "500 Internal Server Error",
                format!(r#"{{"ok":false,"error":"Discord send failed: {}"}}"#, e),
            )
        }
    }
}

/// Handle POST /api/senddm — send a DM to a Discord user.
/// Accepts JSON: {"user_id":"...", "content":"...", "bot":"announce|notify"}
/// When using announce bot, user replies trigger a Claude session.
async fn handle_senddm(registry: &HealthRegistry, body: &str) -> (&'static str, String) {
    let parsed: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => {
            return (
                "400 Bad Request",
                r#"{"ok":false,"error":"invalid JSON"}"#.to_string(),
            );
        }
    };

    let user_id_raw: u64 = parsed["user_id"]
        .as_str()
        .and_then(|s| s.parse().ok())
        .or_else(|| parsed["user_id"].as_u64())
        .unwrap_or(0);
    if user_id_raw == 0 {
        return (
            "400 Bad Request",
            r#"{"ok":false,"error":"user_id required (string or number)"}"#.to_string(),
        );
    }

    let content = match parsed["content"].as_str() {
        Some(c) if !c.is_empty() => c,
        _ => {
            return (
                "400 Bad Request",
                r#"{"ok":false,"error":"content required"}"#.to_string(),
            );
        }
    };

    let bot = parsed["bot"].as_str().unwrap_or("announce");
    let http = match resolve_bot_http(registry, bot).await {
        Ok(h) => h,
        Err(resp) => return resp,
    };

    use poise::serenity_prelude::{CreateMessage, UserId};
    let user_id = UserId::new(user_id_raw);
    match user_id.create_dm_channel(&*http).await {
        Ok(dm_channel) => {
            match dm_channel
                .id
                .send_message(&*http, CreateMessage::new().content(content))
                .await
            {
                Ok(_) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] 📨 DM: → user {user_id_raw}");
                    (
                        "200 OK",
                        format!(r#"{{"ok":true,"user_id":"{}"}}"#, user_id_raw),
                    )
                }
                Err(e) => (
                    "500 Internal Server Error",
                    format!(r#"{{"ok":false,"error":"DM send failed: {}"}}"#, e),
                ),
            }
        }
        Err(e) => (
            "500 Internal Server Error",
            format!(
                r#"{{"ok":false,"error":"DM channel creation failed: {}"}}"#,
                e
            ),
        ),
    }
}

/// Resolve the health check port from env or default.
pub fn resolve_port() -> u16 {
    std::env::var("AGENTDESK_HEALTH_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8793)
}

/// Handle POST /api/session/start — start a session via API.
/// Accepts JSON: {"channel_id":"<id>", "path":"/some/path", "provider":"claude"}
/// Creates a DiscordSession in the provider's SharedData and responds.
async fn handle_session_start<'a>(registry: &HealthRegistry, body: &str) -> (&'a str, String) {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return (
            "400 Bad Request",
            r#"{"ok":false,"error":"invalid JSON"}"#.to_string(),
        );
    };

    let channel_id_str = json
        .get("channel_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let path = json.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let provider_hint = json.get("provider").and_then(|v| v.as_str()).unwrap_or("");

    let Some(channel_id_raw) = channel_id_str.parse::<u64>().ok() else {
        return (
            "400 Bad Request",
            r#"{"ok":false,"error":"channel_id must be a numeric string"}"#.to_string(),
        );
    };

    // Resolve path — expand ~ and . to absolute
    let effective_path = if path == "." || path.is_empty() {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string())
    } else if path.starts_with('~') {
        dirs::home_dir()
            .map(|h| path.replacen('~', &h.to_string_lossy(), 1))
            .unwrap_or_else(|| path.to_string())
    } else {
        path.to_string()
    };

    let channel_id = ChannelId::new(channel_id_raw);

    // Find the matching provider
    let providers = registry.providers.lock().await;

    // Try to match by provider hint, or by channel name suffix
    let target_provider = if !provider_hint.is_empty() {
        providers.iter().find(|p| p.name == provider_hint)
    } else {
        // Try to detect from channel_id via role binding
        let binding = super::settings::resolve_role_binding(channel_id, None);
        let bound_provider = binding.as_ref().and_then(|b| b.provider.as_ref());
        match bound_provider {
            Some(p) => providers.iter().find(|e| &e.name == p.as_str()),
            None => providers.first(),
        }
    };

    let Some(provider_entry) = target_provider else {
        return (
            "404 Not Found",
            r#"{"ok":false,"error":"no matching provider found"}"#.to_string(),
        );
    };

    // Create session
    {
        let mut data = provider_entry.shared.core.lock().await;
        let session = data
            .sessions
            .entry(channel_id)
            .or_insert_with(|| super::DiscordSession {
                session_id: None,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                channel_name: None,
                category_name: None,
                remote_profile_name: None,
                channel_id: Some(channel_id_raw),
                last_active: tokio::time::Instant::now(),
                worktree: None,
                last_shared_memory_ts: None,
                born_generation: super::runtime_store::load_generation(),
            });
        session.current_path = Some(effective_path.clone());
        session.last_active = tokio::time::Instant::now();
    }

    let response = format!(
        r#"{{"ok":true,"channel_id":"{}","path":"{}","provider":"{}"}}"#,
        channel_id_raw, effective_path, provider_entry.name
    );
    ("200 OK", response)
}

/// Self-watchdog: runs on a dedicated OS thread (not tokio) to detect
/// runtime hangs.  Periodically opens a raw TCP connection to the health
/// port and expects a response within a few seconds.  If the check fails
/// `max_failures` times in a row the process is force-killed so launchd
/// (or systemd) can restart it.
pub fn spawn_watchdog(port: u16) {
    const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
    const TCP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    const MAX_FAILURES: u32 = 3;
    // Grace period: skip checks for the first 30s after startup so the
    // runtime has time to initialise Discord bots and register providers.
    const STARTUP_GRACE: std::time::Duration = std::time::Duration::from_secs(30);

    std::thread::Builder::new()
        .name("health-watchdog".into())
        .spawn(move || {
            std::thread::sleep(STARTUP_GRACE);

            let mut consecutive_failures: u32 = 0;

            loop {
                std::thread::sleep(CHECK_INTERVAL);

                let ok = (|| -> bool {
                    use std::io::{Read, Write};
                    let addr = format!("127.0.0.1:{port}");
                    let mut stream =
                        match std::net::TcpStream::connect_timeout(
                            &addr.parse().unwrap(),
                            TCP_TIMEOUT,
                        ) {
                            Ok(s) => s,
                            Err(_) => return false,
                        };
                    let _ = stream.set_read_timeout(Some(TCP_TIMEOUT));
                    let _ = stream.set_write_timeout(Some(TCP_TIMEOUT));
                    let req = "GET /api/health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
                    if stream.write_all(req.as_bytes()).is_err() {
                        return false;
                    }
                    let mut buf = [0u8; 512];
                    match stream.read(&mut buf) {
                        Ok(n) if n > 0 => {
                            let resp = String::from_utf8_lossy(&buf[..n]);
                            resp.contains("200 OK")
                        }
                        _ => false,
                    }
                })();

                if ok {
                    if consecutive_failures > 0 {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        eprintln!(
                            "  [{ts}] 🩺 watchdog: health recovered after {consecutive_failures} failure(s)"
                        );
                    }
                    consecutive_failures = 0;
                } else {
                    consecutive_failures += 1;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    eprintln!(
                        "  [{ts}] 🩺 watchdog: health check failed ({consecutive_failures}/{MAX_FAILURES})"
                    );
                    if consecutive_failures >= MAX_FAILURES {
                        eprintln!(
                            "  [{ts}] 🩺 watchdog: runtime unresponsive — forcing exit"
                        );
                        std::process::exit(1);
                    }
                }
            }
        })
        .expect("Failed to spawn watchdog thread");
}

/// Parse a /api/send JSON body and extract (target, content, source).
/// Returns Err with an error message on invalid input.
/// Factored out of handle_send for testability.
fn parse_send_body(body: &str) -> Result<(String, String, String), &'static str> {
    let json: serde_json::Value = serde_json::from_str(body).map_err(|_| "invalid JSON")?;
    let content = json
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if content.is_empty() {
        return Err("content is required");
    }
    let target = json
        .get("target")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let source = json
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    Ok((target, content, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_send_request_valid_json() {
        let body = r#"{"target":"channel:123","content":"hello","source":"agent-a"}"#;
        let result = parse_send_body(body);
        assert!(result.is_ok(), "Valid JSON should parse successfully");
        let (target, content, source) = result.unwrap();
        assert_eq!(target, "channel:123");
        assert_eq!(content, "hello");
        assert_eq!(source, "agent-a");
    }

    #[test]
    fn test_parse_send_request_missing_content() {
        let body = r#"{"target":"channel:123"}"#;
        let result = parse_send_body(body);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "content is required");
    }

    #[test]
    fn test_parse_send_request_empty_content() {
        let body = r#"{"target":"channel:123","content":""}"#;
        let result = parse_send_body(body);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "content is required");
    }

    #[test]
    fn test_parse_send_request_invalid_json() {
        let body = "not json at all";
        let result = parse_send_body(body);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), "invalid JSON");
    }

    #[test]
    fn test_parse_send_request_missing_target_defaults_empty() {
        let body = r#"{"content":"hello world"}"#;
        let result = parse_send_body(body);
        assert!(result.is_ok());
        let (target, content, source) = result.unwrap();
        assert_eq!(target, "");
        assert_eq!(content, "hello world");
        assert_eq!(source, "unknown");
    }

    #[test]
    fn test_parse_send_request_missing_source_defaults_unknown() {
        let body = r#"{"target":"channel:999","content":"msg"}"#;
        let result = parse_send_body(body);
        assert!(result.is_ok());
        let (_, _, source) = result.unwrap();
        assert_eq!(source, "unknown");
    }

    #[test]
    fn test_resolve_port_default() {
        let _lock = super::super::runtime_store::test_env_lock().lock().unwrap();
        unsafe { std::env::remove_var("AGENTDESK_HEALTH_PORT") };
        assert_eq!(resolve_port(), 8793);
    }

    #[test]
    fn test_resolve_port_env_override() {
        let _lock = super::super::runtime_store::test_env_lock().lock().unwrap();
        unsafe { std::env::set_var("AGENTDESK_HEALTH_PORT", "9999") };
        assert_eq!(resolve_port(), 9999);
        unsafe { std::env::remove_var("AGENTDESK_HEALTH_PORT") };
    }

    #[test]
    fn test_resolve_port_invalid_env() {
        let _lock = super::super::runtime_store::test_env_lock().lock().unwrap();
        unsafe { std::env::set_var("AGENTDESK_HEALTH_PORT", "not-a-number") };
        assert_eq!(resolve_port(), 8793);
        unsafe { std::env::remove_var("AGENTDESK_HEALTH_PORT") };
    }
}
