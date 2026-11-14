use super::*;
use serde_json::json;
use std::time::Duration;

const EXCLUDED_MEMENTO_TOOLS: &[&str] = &[
    "apply_update",
    "check_update",
    "memory_consolidate",
    "session_rotate",
    "reflect",
];
const STATE_FILE: &str = "antigravity-mcp-sync-state.json";
const MCP_PROTOCOL_VERSION: &str = "2025-11-25";

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
struct SyncState {
    managed_servers: BTreeSet<String>,
    managed_allow: BTreeMap<String, BTreeSet<String>>,
}

pub(crate) fn sync(config: &Config) -> Result<(), String> {
    let home = current_home_dir().ok_or("Antigravity home unavailable")?;
    let root = crate::config::runtime_root().ok_or("AGENTDESK_ROOT_DIR is unavailable")?;
    let state_path = crate::runtime_layout::config_dir(&root).join(STATE_FILE);
    std::fs::create_dir_all(state_path.parent().unwrap())
        .map_err(|_| "Cannot create MCP sync directory")?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(state_path.with_extension("lock"))
        .map_err(|_| "Cannot open Antigravity MCP sync lock")?;
    lock.try_lock()
        .map_err(|_| "Antigravity MCP sync already running")?;
    let mut state: SyncState = serde_json::from_value(read_object(&state_path)?)
        .map_err(|_| "Invalid Antigravity MCP sync state")?;
    let desired = desired_servers(config, &home)?;
    if desired.is_empty() && state.managed_servers.is_empty() && state.managed_allow.is_empty() {
        return Ok(());
    }
    let config_path = home.join(".gemini/config/mcp_config.json");
    let settings_path = home.join(".gemini/antigravity-cli/settings.json");
    let mut target = read_object(&config_path)?;
    let mut settings = read_object(&settings_path)?;
    let previous_settings = settings.clone();
    let servers = object_field(&mut target, "mcpServers")?;
    for removed in state
        .managed_servers
        .iter()
        .filter(|name| !desired.contains_key(*name))
    {
        servers.remove(removed);
    }
    for (name, entry) in &desired {
        servers.insert(name.clone(), entry.clone());
    }
    let permissions = object_field(&mut settings, "permissions")?;
    let allow = permissions
        .entry("allow")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or("Antigravity permissions.allow must be an array")?;
    let catalogs = discover_catalogs(&desired)?;
    let mut failed = false;
    let names: BTreeSet<_> = desired
        .keys()
        .chain(state.managed_allow.keys())
        .cloned()
        .collect();
    for name in names {
        let rules = if !desired.contains_key(&name) {
            BTreeSet::new()
        } else if let Some(Ok(tools)) = catalogs.get(&name) {
            tools
                .iter()
                .filter(|tool| {
                    name != MEMENTO_SERVER_NAME || !EXCLUDED_MEMENTO_TOOLS.contains(&tool.as_str())
                })
                .map(|tool| format!("mcp({name}/{tool})"))
                .collect()
        } else {
            failed = true;
            continue;
        };
        let previous = state.managed_allow.remove(&name).unwrap_or_default();
        // Retain existing order, and never claim a matching rule the user already owned.
        allow.retain(|rule| {
            !rule
                .as_str()
                .is_some_and(|rule| previous.contains(rule) && !rules.contains(rule))
        });
        let mut owned: BTreeSet<_> = previous.intersection(&rules).cloned().collect();
        for rule in rules {
            if !allow.iter().any(|value| value.as_str() == Some(&rule)) {
                allow.push(Value::String(rule.clone()));
                owned.insert(rule);
            }
        }
        if !owned.is_empty() {
            state.managed_allow.insert(name, owned);
        }
    }
    state.managed_servers = desired.keys().cloned().collect();
    publish(&config_path, &target)?;
    if settings != previous_settings {
        publish(&settings_path, &settings)?;
    }
    publish(
        &state_path,
        &serde_json::to_value(state).map_err(|_| "Cannot serialize MCP sync state")?,
    )?;
    if failed {
        Err("Antigravity tool discovery failed; existing allow rules preserved".into())
    } else {
        Ok(())
    }
}

fn read_object(path: &Path) -> Result<Value, String> {
    let value = match std::fs::read_to_string(path) {
        Ok(raw) => {
            serde_json::from_str::<Value>(&raw).map_err(|_| "Invalid MCP synchronization JSON")?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({}),
        Err(_) => return Err("Cannot read MCP synchronization JSON".into()),
    };
    if !value.is_object() {
        return Err("MCP synchronization JSON must be an object".into());
    }
    Ok(value)
}

fn object_field<'a>(root: &'a mut Value, key: &str) -> Result<&'a mut Map<String, Value>, String> {
    root.as_object_mut()
        .ok_or("MCP configuration must be an object")?
        .entry(key)
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| format!("MCP configuration field {key} must be an object"))
}

fn publish(path: &Path, value: &Value) -> Result<(), String> {
    let rendered =
        serde_json::to_string_pretty(value).map_err(|_| "Cannot serialize MCP configuration")?;
    atomic_write(path, &rendered)
}

fn desired_servers(config: &Config, home: &Path) -> Result<BTreeMap<String, Value>, String> {
    let mut desired = BTreeMap::new();
    for (name, server) in resolved_mcp_servers(config, None) {
        let mut entry = json!({"disabled":false,"serverUrl":server.url});
        if let Some(token) = server
            .bearer_token_env_var
            .and_then(|name| std::env::var(name).ok())
        {
            entry["headers"] = json!({"Authorization":format!("Bearer {}", token.trim())});
        }
        desired.insert(name, entry);
    }
    // Only the user-scope catalog is imported; project entries and other keys stay private.
    let source = read_object(&claude_user_mcp_config_path(home))?;
    if let Some(servers) = source.get("mcpServers") {
        let servers = servers
            .as_object()
            .ok_or("Claude mcpServers must be an object")?;
        for (name, server) in servers {
            let Some(url) = server.get("url").and_then(Value::as_str) else {
                continue;
            };
            let mut entry = json!({"disabled":false,"serverUrl":url});
            if let Some(headers) = server.get("headers") {
                entry["headers"] = headers.clone();
            }
            desired.insert(name.clone(), entry);
        }
    }
    if desired.iter().any(|(name, entry)| {
        !valid_atom(name)
            || !reqwest::Url::parse(entry["serverUrl"].as_str().unwrap_or_default())
                .is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
    }) {
        return Err("Invalid Antigravity MCP server name or HTTP URL".into());
    }
    Ok(desired)
}

fn valid_atom(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || b"_.:-".contains(&ch))
}

type Catalogs = BTreeMap<String, Result<BTreeSet<String>, ()>>;

fn discover_catalogs(servers: &BTreeMap<String, Value>) -> Result<Catalogs, String> {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| "Cannot initialize MCP discovery runtime")?;
                runtime.block_on(async {
                    use futures::StreamExt;
                    let client = reqwest::Client::builder()
                        .redirect(reqwest::redirect::Policy::none())
                        .connect_timeout(Duration::from_secs(3))
                        .timeout(Duration::from_secs(5))
                        .build()
                        .map_err(|_| "Cannot initialize MCP discovery client")?;
                    Ok(futures::stream::iter(servers.iter().map(|(name, entry)| {
                        let client = &client;
                        async move {
                            let tools = tokio::time::timeout(
                                Duration::from_secs(10),
                                discover_tools(client, entry),
                            )
                            .await
                            .unwrap_or(Err(()));
                            (name.clone(), tools)
                        }
                    }))
                    .buffer_unordered(4)
                    .collect()
                    .await)
                })
            })
            .join()
            .map_err(|_| "MCP discovery worker failed")?
    })
}

async fn discover_tools(client: &reqwest::Client, entry: &Value) -> Result<BTreeSet<String>, ()> {
    let (init, session) = rpc(
        client,
        entry,
        None,
        MCP_PROTOCOL_VERSION,
        "initialize",
        Some(1),
        json!({
            "protocolVersion":MCP_PROTOCOL_VERSION, "capabilities":{},
            "clientInfo":{"name":"agentdesk-provider-sync","version":env!("CARGO_PKG_VERSION")}
        }),
    )
    .await?;
    let version = init["result"]["protocolVersion"].as_str().ok_or(())?;
    if ![
        MCP_PROTOCOL_VERSION,
        "2025-06-18",
        "2025-03-26",
        "2024-11-05",
    ]
    .contains(&version)
    {
        return Err(());
    }
    rpc(
        client,
        entry,
        session.as_deref(),
        version,
        "notifications/initialized",
        None,
        json!({}),
    )
    .await?;
    let mut tools = BTreeSet::new();
    let mut cursor = None;
    let mut seen = BTreeSet::new();
    for page in 0..32 {
        let params = cursor
            .as_ref()
            .map_or_else(|| json!({}), |cursor| json!({"cursor":cursor}));
        let (value, _) = rpc(
            client,
            entry,
            session.as_deref(),
            version,
            "tools/list",
            Some(page + 2),
            params,
        )
        .await?;
        for tool in value["result"]["tools"].as_array().ok_or(())? {
            let name = tool["name"]
                .as_str()
                .filter(|name| valid_atom(name))
                .ok_or(())?;
            tools.insert(name.to_string());
        }
        match value["result"].get("nextCursor") {
            None | Some(Value::Null) => return Ok(tools),
            Some(Value::String(next)) if !next.is_empty() && seen.insert(next.clone()) => {
                cursor = Some(next.clone())
            }
            _ => return Err(()),
        }
    }
    Err(())
}

async fn rpc(
    client: &reqwest::Client,
    entry: &Value,
    session: Option<&str>,
    version: &str,
    method: &str,
    id: Option<u64>,
    params: Value,
) -> Result<(Value, Option<String>), ()> {
    let mut request = client
        .post(entry["serverUrl"].as_str().ok_or(())?)
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", version);
    if let Some(headers) = entry.get("headers") {
        for (key, value) in headers.as_object().ok_or(())? {
            request = request.header(key, value.as_str().ok_or(())?);
        }
    }
    if let Some(session) = session {
        request = request.header("MCP-Session-Id", session);
    }
    let mut body = json!({"jsonrpc":"2.0","method":method,"params":params});
    if let Some(id) = id {
        body["id"] = json!(id);
    }
    let mut response = request.json(&body).send().await.map_err(|_| ())?;
    if !response.status().is_success() {
        return Err(());
    }
    let session = response
        .headers()
        .get("MCP-Session-Id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let Some(id) = id else {
        return Ok((Value::Null, session));
    };
    let sse = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"));
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        bytes.extend_from_slice(&chunk);
        if bytes.len() > 1_048_576 {
            return Err(());
        }
        if sse {
            let text = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
            for event in text.split("\n\n").take(text.split("\n\n").count() - 1) {
                let data = event
                    .lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .map(str::trim_start)
                    .collect::<Vec<_>>()
                    .join("\n");
                if let Ok(value) = serde_json::from_str::<Value>(&data)
                    && value.get("id") == Some(&json!(id))
                {
                    return rpc_result(value, session);
                }
            }
        }
    }
    if sse {
        return Err(());
    }
    rpc_result(serde_json::from_slice(&bytes).map_err(|_| ())?, session).and_then(
        |(value, session)| {
            if value.get("id") == Some(&json!(id)) {
                Ok((value, session))
            } else {
                Err(())
            }
        },
    )
}

fn rpc_result(value: Value, session: Option<String>) -> Result<(Value, Option<String>), ()> {
    if value.get("error").is_some() || value.get("result").is_none() {
        Err(())
    } else {
        Ok((value, session))
    }
}

#[cfg(test)]
#[path = "antigravity_tests.rs"]
mod tests;
