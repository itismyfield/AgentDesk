#![allow(dead_code)]

use super::VERSION;

pub fn print_help() {
    println!("AgentDesk {} - AI agent orchestration platform", VERSION);
    println!();
    println!("USAGE:");
    println!("    agentdesk <COMMAND>");
    println!();
    println!("COMMANDS:");
    println!("    -h, --help              Print help information");
    println!("    -v, --version           Print version information");
    println!(
        "    dcserver [TOKEN]        Start Discord bot server(s); without TOKEN uses configured Discord bots"
    );
    println!(
        "    restart-dcserver [--report-channel-id <ID> --report-provider <claude|codex|gemini|opencode|qwen> [--report-message-id <ID>]]"
    );
    println!("    discord-sendfile <PATH> --channel <ID> --key <HASH>");
    println!("    discord-sendmessage --channel <ID> --message <TEXT> [--key <HASH>]");
    println!("    discord-senddm --user <ID> --message <TEXT> [--key <HASH>]");
    println!(
        "    send-to-agent --from <AGENT> --to <AGENT> --message <TEXT> --expect-reply <true|false> [--channel-kind cc|cdx] [--no-prefix]"
    );
    println!("    reset-tmux              Kill all AgentDesk-* tmux sessions");
    println!(
        "    ismcptool <TOOL>...     Check if MCP tool(s) are registered in .claude/settings.json (CWD)"
    );
    println!(
        "    addmcptool <TOOL>...    Add MCP tool permission(s) to .claude/settings.json (CWD)"
    );
    println!();
}

pub fn print_version() {
    println!("AgentDesk {}", VERSION);
}

pub fn handle_base64(encoded: &str) {
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    match BASE64.decode(encoded) {
        Ok(decoded) => {
            if let Ok(text) = String::from_utf8(decoded) {
                print!("{}", text);
            } else {
                std::process::exit(1);
            }
        }
        Err(_) => {
            std::process::exit(1);
        }
    }
}

pub fn handle_ismcptool(tool_names: &[String]) {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Error: cannot determine current directory: {e}");
            std::process::exit(1);
        }
    };
    let settings_path = cwd.join(".claude").join("settings.json");

    let allow_list: Vec<String> = if settings_path.exists() {
        let content = match std::fs::read_to_string(&settings_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Error: failed to read {}: {e}", settings_path.display());
                std::process::exit(1);
            }
        };
        let json: serde_json::Value = match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Error: failed to parse {}: {e}", settings_path.display());
                std::process::exit(1);
            }
        };
        json.get("permissions")
            .and_then(|p| p.get("allow"))
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };

    for tool_name in tool_names {
        if allow_list.iter().any(|v| v == tool_name) {
            println!("{}: registered", tool_name);
        } else {
            println!("{}: not registered", tool_name);
        }
    }
}

pub fn handle_addmcptool(tool_names: &[String]) {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Error: cannot determine current directory: {e}");
            std::process::exit(1);
        }
    };
    let settings_path = cwd.join(".claude").join("settings.json");

    // Read existing file or start with empty object
    let mut json: serde_json::Value = if settings_path.exists() {
        let content = match std::fs::read_to_string(&settings_path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("Error: failed to read {}: {e}", settings_path.display());
                std::process::exit(1);
            }
        };
        match serde_json::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Error: failed to parse {}: {e}", settings_path.display());
                std::process::exit(1);
            }
        }
    } else {
        if let Some(parent) = settings_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        serde_json::json!({})
    };

    let obj = match json.as_object_mut() {
        Some(o) => o,
        None => {
            eprintln!("Error: settings.json root is not a JSON object");
            std::process::exit(1);
        }
    };

    // Add tool to permissions.allow array
    let permissions = obj
        .entry("permissions")
        .or_insert_with(|| serde_json::json!({}));
    let allow = match permissions.as_object_mut() {
        Some(o) => o,
        None => {
            eprintln!("Error: settings.json 'permissions' is not an object");
            std::process::exit(1);
        }
    }
    .entry("allow")
    .or_insert_with(|| serde_json::json!([]));
    let allow_arr = match allow.as_array_mut() {
        Some(a) => a,
        None => {
            eprintln!("Error: settings.json 'permissions.allow' is not an array");
            std::process::exit(1);
        }
    };

    // Add each tool, skipping duplicates
    let mut added = Vec::new();
    let mut skipped = Vec::new();
    for tool_name in tool_names {
        let already_exists = allow_arr
            .iter()
            .any(|v| v.as_str() == Some(tool_name.as_str()));
        if already_exists {
            skipped.push(tool_name.as_str());
        } else {
            allow_arr.push(serde_json::json!(tool_name));
            added.push(tool_name.as_str());
        }
    }

    // Save
    let content = match serde_json::to_string_pretty(&json) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: failed to serialize JSON: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = std::fs::write(&settings_path, content) {
        eprintln!("Error: failed to write {}: {e}", settings_path.display());
        std::process::exit(1);
    }

    for name in &added {
        println!("Added: {}", name);
    }
    for name in &skipped {
        println!("Already registered: {}", name);
    }
}

/// #2655: marker key that identifies AgentDesk-managed hook entries in a
/// Claude Code `settings.json`. Hook command strings include this marker as
/// the first argument so we can find and overwrite or remove them
/// idempotently. Keep this stable — older AgentDesk installs ship with it
/// embedded in their `~/.claude/settings.json` and uninstall relies on
/// exact-match.
pub(crate) const MEMENTO_HOOK_MARKER: &str = "AGENTDESK_MEMENTO_HOOK";

/// #2655: handler for the `install-memento-session-hook` CLI surface. Writes
/// an idempotent SessionStart hook that loads the Memento `context()` payload
/// and a UserPromptSubmit reminder that nudges the agent to reflect when
/// context window pressure is high.
///
/// The handler does NOT call Memento directly; instead it installs a small
/// shell command (under the marker prefix above) that Claude Code itself
/// runs at the configured event. The shell command is `:` (no-op) when the
/// `mcp__memento__context` MCP tool isn't available, so an environment
/// without Memento does not see hook noise.
pub fn handle_install_memento_session_hook(
    settings_path_override: Option<&str>,
    dry_run: bool,
    uninstall: bool,
) -> Result<(), String> {
    let settings_path = match settings_path_override {
        Some(value) if !value.trim().is_empty() => std::path::PathBuf::from(value),
        _ => {
            let home = std::env::var_os("HOME").ok_or_else(|| {
                "HOME environment variable is not set; pass --settings-path explicitly".to_string()
            })?;
            std::path::PathBuf::from(home)
                .join(".claude")
                .join("settings.json")
        }
    };

    let existing = if settings_path.exists() {
        let raw = std::fs::read_to_string(&settings_path)
            .map_err(|e| format!("read {}: {e}", settings_path.display()))?;
        if raw.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(&raw)
                .map_err(|e| format!("parse {}: {e}", settings_path.display()))?
        }
    } else {
        serde_json::json!({})
    };

    let updated = if uninstall {
        remove_memento_session_hook(existing)
    } else {
        upsert_memento_session_hook(existing)
    };

    let pretty =
        serde_json::to_string_pretty(&updated).map_err(|e| format!("serialize settings: {e}"))?;
    if dry_run {
        println!("{}", pretty);
        return Ok(());
    }

    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create dir {}: {e}", parent.display()))?;
    }
    std::fs::write(&settings_path, format!("{pretty}\n"))
        .map_err(|e| format!("write {}: {e}", settings_path.display()))?;
    println!(
        "{} memento SessionStart hook in {}",
        if uninstall { "Removed" } else { "Installed" },
        settings_path.display()
    );
    Ok(())
}

/// #2655: render the SessionStart hook command. The command echoes a tiny
/// reminder line and exits 0 so Claude Code never blocks session startup.
/// The reminder line is a *system reminder* string Claude Code injects into
/// the model's context, which causes the model to call
/// `mcp__memento__context` on its own (Memento MCP server instructions
/// already direct the model to call `context` on SessionStart — see the
/// memento server's MCP instructions).
///
/// The marker (`MEMENTO_HOOK_MARKER`) is embedded as a leading comment-style
/// argument that the shell parser ignores at runtime but that lets the
/// installer recognise its own entries on future runs.
fn render_memento_session_start_command() -> String {
    format!(
        "{marker} :; printf '[memento] SessionStart auto-context reminder: call mcp__memento__context (structured=true) before responding to the first user message.\\n'",
        marker = MEMENTO_HOOK_MARKER
    )
}

fn render_memento_user_prompt_command() -> String {
    // Emits a soft reminder for the model. Cheap printf, exits 0. Claude Code
    // forwards the stdout into the next user-turn context as a system message
    // when the hook output starts with `[memento]`.
    format!(
        "{marker} :; printf '[memento] If context window utilisation feels high (>= 50%%), call mcp__memento__reflect to consolidate before continuing.\\n'",
        marker = MEMENTO_HOOK_MARKER
    )
}

fn upsert_memento_session_hook(mut existing: serde_json::Value) -> serde_json::Value {
    let root = existing
        .as_object_mut()
        .map(std::mem::take)
        .unwrap_or_default();
    let mut root = root;

    let hooks = root
        .entry("hooks".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let hooks_obj = match hooks.as_object_mut() {
        Some(o) => o,
        None => {
            *hooks = serde_json::json!({});
            hooks.as_object_mut().expect("just initialised")
        }
    };

    upsert_hook_event(
        hooks_obj,
        "SessionStart",
        &render_memento_session_start_command(),
    );
    upsert_hook_event(
        hooks_obj,
        "UserPromptSubmit",
        &render_memento_user_prompt_command(),
    );

    serde_json::Value::Object(root)
}

fn upsert_hook_event(
    hooks: &mut serde_json::Map<String, serde_json::Value>,
    event: &str,
    command: &str,
) {
    let entry = hooks
        .entry(event.to_string())
        .or_insert_with(|| serde_json::json!([]));
    let arr = match entry.as_array_mut() {
        Some(a) => a,
        None => {
            *entry = serde_json::json!([]);
            entry.as_array_mut().expect("just initialised")
        }
    };

    // Drop any existing AgentDesk-managed entry (marker prefix), then append
    // the fresh rendering. Other (operator-managed) hook entries are left
    // untouched.
    arr.retain(|matcher| !matcher_block_contains_marker(matcher));

    arr.push(serde_json::json!({
        "hooks": [
            {
                "type": "command",
                "command": command,
                "async": true
            }
        ]
    }));
}

fn matcher_block_contains_marker(matcher: &serde_json::Value) -> bool {
    let Some(hooks) = matcher.get("hooks").and_then(|v| v.as_array()) else {
        return false;
    };
    hooks.iter().any(|hook| {
        hook.get("command")
            .and_then(|v| v.as_str())
            .map(|cmd| cmd.contains(MEMENTO_HOOK_MARKER))
            .unwrap_or(false)
    })
}

fn remove_memento_session_hook(mut existing: serde_json::Value) -> serde_json::Value {
    if let Some(root) = existing.as_object_mut() {
        if let Some(hooks) = root.get_mut("hooks").and_then(|v| v.as_object_mut()) {
            for event in ["SessionStart", "UserPromptSubmit"] {
                if let Some(entry) = hooks.get_mut(event).and_then(|v| v.as_array_mut()) {
                    entry.retain(|matcher| !matcher_block_contains_marker(matcher));
                    if entry.is_empty() {
                        hooks.remove(event);
                    }
                }
            }
        }
    }
    existing
}

pub fn handle_reset_tmux() {
    let hostname = crate::services::platform::hostname_short();

    // Kill local AgentDesk-* sessions.
    println!("[{}] Cleaning AgentDesk-* tmux sessions...", hostname);
    let killed = kill_agentdesk_tmux_sessions_local();
    if killed == 0 {
        println!("   No AgentDesk-* sessions found.");
    } else {
        println!("   Killed {} session(s).", killed);
    }

    // Also clean /tmp/agentdesk-* temp files
    let cleaned = clean_agentdesk_tmp_files();
    if cleaned > 0 {
        println!("   Cleaned {} temp file(s).", cleaned);
    }

    println!("Done.");
}

fn kill_agentdesk_tmux_sessions_local() -> usize {
    let names = match crate::services::platform::tmux::list_session_names() {
        Ok(n) => n,
        Err(_) => return 0,
    };

    let mut count = 0;
    for name in &names {
        if name.starts_with("AgentDesk-") {
            if crate::services::platform::tmux::kill_session(
                name,
                "CLI cleanup of all AgentDesk tmux sessions",
            ) {
                println!("   killed: {}", name);
                count += 1;
            }
        }
    }
    count
}

fn clean_agentdesk_tmp_files() -> usize {
    let mut count = 0;
    if let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("agentdesk-")
                && (name_str.ends_with(".jsonl")
                    || name_str.ends_with(".input")
                    || name_str.ends_with(".prompt"))
            {
                if std::fs::remove_file(entry.path()).is_ok() {
                    count += 1;
                }
            }
        }
    }
    count
}

pub fn migrate_config_dir() {
    if let Some(home) = dirs::home_dir() {
        let old_dir = home.join(".cokacdir");
        let new_dir = home.join(".adk");
        if old_dir.exists() && !new_dir.exists() {
            if let Err(e) = std::fs::rename(&old_dir, &new_dir) {
                eprintln!("Warning: failed to migrate ~/.cokacdir to ~/.adk: {}", e);
            }
        }
    }
}

pub fn print_goodbye_message() {
    println!("AgentDesk process ended.");
}

#[cfg(test)]
mod memento_hook_install_tests {
    use super::*;

    #[test]
    fn install_into_empty_settings_creates_both_events() {
        let updated = upsert_memento_session_hook(serde_json::json!({}));
        let hooks = updated.get("hooks").and_then(|v| v.as_object()).unwrap();
        assert!(hooks.contains_key("SessionStart"));
        assert!(hooks.contains_key("UserPromptSubmit"));
        // Marker is embedded so re-installs can recognise the entry.
        let cmd = hooks["SessionStart"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(cmd.contains(MEMENTO_HOOK_MARKER));
        assert!(cmd.contains("mcp__memento__context"));
    }

    #[test]
    fn install_is_idempotent_and_preserves_operator_entries() {
        let operator_owned = serde_json::json!({
            "hooks": {
                "SessionStart": [
                    {
                        "hooks": [
                            { "type": "command", "command": "echo operator-owned" }
                        ]
                    }
                ]
            }
        });
        let first = upsert_memento_session_hook(operator_owned);
        let second = upsert_memento_session_hook(first.clone());
        assert_eq!(
            first, second,
            "second invocation must produce identical settings"
        );

        let session_start = second["hooks"]["SessionStart"].as_array().unwrap();
        // Exactly one operator entry + one memento entry.
        assert_eq!(session_start.len(), 2);
        let has_operator = session_start.iter().any(|m| {
            m["hooks"][0]["command"]
                .as_str()
                .map(|c| c.contains("operator-owned"))
                .unwrap_or(false)
        });
        let has_memento = session_start.iter().any(matcher_block_contains_marker);
        assert!(has_operator, "operator-owned hook must be preserved");
        assert!(has_memento, "memento hook must be present after upsert");
    }

    #[test]
    fn uninstall_removes_only_memento_entries() {
        let mixed = upsert_memento_session_hook(serde_json::json!({
            "hooks": {
                "SessionStart": [
                    {
                        "hooks": [
                            { "type": "command", "command": "echo keep-me" }
                        ]
                    }
                ]
            }
        }));
        let stripped = remove_memento_session_hook(mixed);
        let session_start = stripped["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(session_start.len(), 1);
        let surviving = session_start[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(surviving.contains("keep-me"));
        // The UserPromptSubmit event had only the memento entry, so the
        // whole event key should now be removed.
        assert!(stripped["hooks"].get("UserPromptSubmit").is_none());
    }

    #[test]
    fn install_replaces_stale_memento_command_on_re_run() {
        // Simulate an older AgentDesk install that emitted a slightly
        // different command string. The marker must let the installer find
        // and replace it on the next run instead of leaving both around.
        let mut existing = serde_json::json!({
            "hooks": {
                "SessionStart": [
                    {
                        "hooks": [
                            {
                                "type": "command",
                                "command": format!("{MEMENTO_HOOK_MARKER} :; echo old payload"),
                                "async": true
                            }
                        ]
                    }
                ]
            }
        });
        existing = upsert_memento_session_hook(existing);
        let session_start = existing["hooks"]["SessionStart"].as_array().unwrap();
        // Only one memento entry survives, and it has the fresh payload.
        let memento_entries: Vec<_> = session_start
            .iter()
            .filter(|m| matcher_block_contains_marker(m))
            .collect();
        assert_eq!(memento_entries.len(), 1);
        let cmd = memento_entries[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(!cmd.contains("old payload"));
        assert!(cmd.contains("mcp__memento__context"));
    }
}
