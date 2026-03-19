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
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self {
            providers: tokio::sync::Mutex::new(Vec::new()),
            started_at: Instant::now(),
            discord_http: tokio::sync::Mutex::new(Vec::new()),
            announce_http: tokio::sync::Mutex::new(None),
        }
    }

    pub(super) async fn register(&self, name: String, shared: Arc<SharedData>) {
        self.providers.lock().await.push(ProviderEntry { name, shared });
    }

    pub(super) async fn register_http(&self, provider: String, http: Arc<serenity::Http>) {
        self.discord_http.lock().await.push((provider, http));
    }
}

/// Start the health check HTTP server on the given port.
/// Runs forever — intended to be spawned as a background tokio task.
pub async fn serve(registry: Arc<HealthRegistry>, port: u16) {
    // Load announce bot token for agent-to-agent routing.
    // This bot is separate from claude/codex bots — its messages are in
    // every agent's allowed_bot_ids, so agents process them.
    if let Some(home) = dirs::home_dir() {
        let root = home.join(".remotecc");
        // TODO: Remove legacy fallback after 2026-03-26
        let new_path = root.join("credential").join("announce_bot_token");
        let legacy = root.join("announce_bot_token");
        let token_path = if new_path.exists() { new_path } else if legacy.exists() { legacy } else { new_path };
        if let Ok(token) = std::fs::read_to_string(&token_path) {
            let token = token.trim().to_string();
            if !token.is_empty() {
                let http = Arc::new(serenity::Http::new(&format!("Bot {token}")));
                *registry.announce_http.lock().await = Some(http);
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] 📢 Announce bot loaded for /api/send routing");
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
                    let code = if healthy { "200 OK" } else { "503 Service Unavailable" };
                    (code, json)
                }
                ("POST", "/api/send") => {
                    // Extract JSON body (after \r\n\r\n)
                    let body_str = request.split("\r\n\r\n").nth(1).unwrap_or("");
                    handle_send(&registry, body_str).await
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
        let data = entry.shared.core.lock().await;
        let active_turns = data.cancel_tokens.len();
        let queue_depth: usize = data.intervention_queue.values().map(|q| q.len()).sum();
        let session_count = data.sessions.len();
        drop(data);

        let restart_pending = entry.shared.restart_pending.load(std::sync::atomic::Ordering::Relaxed);
        let connected = entry.shared.bot_connected.load(std::sync::atomic::Ordering::Relaxed);
        let last_turn_at = entry.shared.last_turn_at.lock()
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
        p.shared.global_active.load(std::sync::atomic::Ordering::Relaxed)
    } else {
        0
    };
    let global_finalizing = if let Some(p) = providers.first() {
        p.shared.global_finalizing.load(std::sync::atomic::Ordering::Relaxed)
    } else {
        0
    };

    format!(
        r#"{{"status":"{}","version":"{}","uptime_secs":{},"global_active":{},"global_finalizing":{},"providers":[{}]}}"#,
        if is_healthy_inner(&providers) { "healthy" } else { "unhealthy" },
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
        if !p.shared.bot_connected.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        // Unhealthy if restart is pending (draining)
        if p.shared.restart_pending.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
    }
    true
}

/// Handle POST /api/send — agent-to-agent native routing.
/// Accepts JSON: {"target":"channel:<id>", "content":"...", "source":"role-id"}
async fn handle_send<'a>(registry: &HealthRegistry, body: &str) -> (&'a str, String) {
    let Ok(json) = serde_json::from_str::<serde_json::Value>(body) else {
        return ("400 Bad Request", r#"{"ok":false,"error":"invalid JSON"}"#.to_string());
    };

    let target = json.get("target").and_then(|v| v.as_str()).unwrap_or("");
    let content = json.get("content").and_then(|v| v.as_str()).unwrap_or("");
    let source = json.get("source").and_then(|v| v.as_str()).unwrap_or("unknown");

    if content.is_empty() {
        return ("400 Bad Request", r#"{"ok":false,"error":"content is required"}"#.to_string());
    }

    // Parse "channel:<id>" format
    let channel_id_raw = if let Some(id_str) = target.strip_prefix("channel:") {
        id_str.trim().parse::<u64>().ok()
    } else {
        target.trim().parse::<u64>().ok()
    };

    let Some(channel_id_raw) = channel_id_raw else {
        return ("400 Bad Request", r#"{"ok":false,"error":"invalid target format (use channel:<id>)"}"#.to_string());
    };

    let channel_id = ChannelId::new(channel_id_raw);

    // Validate source is a known agent role_id (checks org schema agents + role_map bindings)
    if !super::settings::is_known_agent(source) {
        return (
            "403 Forbidden",
            format!(r#"{{"ok":false,"error":"unknown source role: {}"}}"#, source),
        );
    }

    // Verify target channel exists in role-map (authorization check)
    if super::settings::resolve_role_binding(channel_id, None).is_none() {
        return ("403 Forbidden", r#"{"ok":false,"error":"channel not in role-map"}"#.to_string());
    }

    // Use the dedicated announce bot for agent-to-agent routing.
    // The announce bot is in every agent's allowed_bot_ids, so agents process
    // its messages. Claude/codex bots ignore their own messages (mod.rs:552).
    let announce = registry.announce_http.lock().await;
    let Some(http) = announce.as_ref() else {
        return ("503 Service Unavailable",
            r#"{"ok":false,"error":"announce bot not configured (missing ~/.remotecc/credential/announce_bot_token)"}"#.to_string());
    };
    let http = http.clone();
    drop(announce); // Release lock before await

    match channel_id
        .say(&*http, content)
        .await
    {
        Ok(_) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] 📨 ROUTE: [{source}] → channel {channel_id}");
            ("200 OK", format!(r#"{{"ok":true,"target":"channel:{}","source":"{}"}}"#, channel_id, source))
        }
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!("  [{ts}] ⚠ ROUTE: failed to send to channel {channel_id}: {e}");
            ("500 Internal Server Error", format!(r#"{{"ok":false,"error":"Discord send failed: {}"}}"#, e))
        }
    }
}

/// Resolve the health check port from env or default.
pub fn resolve_port() -> u16 {
    std::env::var("REMOTECC_HEALTH_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8793)
}
