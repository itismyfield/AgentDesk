use super::VERSION;

pub fn print_help() {
    println!("AgentDesk {} - AI agent orchestration platform", VERSION);
    println!();
    println!("USAGE:");
    println!("    agentdesk [OPTIONS]");
    println!();
    println!("OPTIONS:");
    println!("    -h, --help              Print help information");
    println!("    -v, --version           Print version information");
    println!(
        "    --dcserver [TOKEN]      Start Discord bot server(s); without TOKEN uses bot_settings.json"
    );
    println!(
        "    --restart-dcserver [--report-channel-id <ID> --report-provider <claude|codex> [--report-message-id <ID>]]"
    );
    println!("    --discord-sendfile <PATH> --channel <ID> --key <HASH>");
    println!("    --discord-sendmessage --channel <ID> --message <TEXT> [--key <HASH>]");
    println!("    --discord-senddm --user <ID> --message <TEXT> [--key <HASH>]");
    println!("    --reset-tmux             Kill all AgentDesk-* tmux sessions");
    println!(
        "    --ismcptool <TOOL>...    Check if MCP tool(s) are registered in .claude/settings.json (CWD)"
    );
    println!(
        "    --addmcptool <TOOL>...   Add MCP tool permission(s) to .claude/settings.json (CWD)"
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
    let cwd = std::env::current_dir().expect("Cannot determine current directory");
    let settings_path = cwd.join(".claude").join("settings.json");

    let allow_list: Vec<String> = if settings_path.exists() {
        let content =
            std::fs::read_to_string(&settings_path).expect("Failed to read .claude/settings.json");
        let json: serde_json::Value =
            serde_json::from_str(&content).expect("Failed to parse .claude/settings.json");
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
    let cwd = std::env::current_dir().expect("Cannot determine current directory");
    let settings_path = cwd.join(".claude").join("settings.json");

    // Read existing file or start with empty object
    let mut json: serde_json::Value = if settings_path.exists() {
        let content =
            std::fs::read_to_string(&settings_path).expect("Failed to read .claude/settings.json");
        serde_json::from_str(&content).expect("Failed to parse .claude/settings.json")
    } else {
        let _ = std::fs::create_dir_all(settings_path.parent().unwrap());
        serde_json::json!({})
    };

    let obj = json
        .as_object_mut()
        .expect("settings.json is not a JSON object");

    // Add tool to permissions.allow array
    let permissions = obj
        .entry("permissions")
        .or_insert_with(|| serde_json::json!({}));
    let allow = permissions
        .as_object_mut()
        .expect("permissions is not an object")
        .entry("allow")
        .or_insert_with(|| serde_json::json!([]));
    let allow_arr = allow.as_array_mut().expect("allow is not an array");

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
    let content = serde_json::to_string_pretty(&json).expect("Failed to serialize JSON");
    std::fs::write(&settings_path, content).expect("Failed to write .claude/settings.json");

    for name in &added {
        println!("Added: {}", name);
    }
    for name in &skipped {
        println!("Already registered: {}", name);
    }
}

pub fn handle_reset_tmux() {
    let hostname = std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "local".to_string());

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

#[cfg(unix)]
fn kill_agentdesk_tmux_sessions_local() -> usize {
    let output = match std::process::Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return 0,
    };

    let mut count = 0;
    for line in output.lines() {
        let name = line.trim();
        if name.starts_with("AgentDesk-") {
            if std::process::Command::new("tmux")
                .args(["kill-session", "-t", name])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
            {
                println!("   killed: {}", name);
                count += 1;
            }
        }
    }
    count
}

#[cfg(not(unix))]
fn kill_agentdesk_tmux_sessions_local() -> usize {
    0
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
