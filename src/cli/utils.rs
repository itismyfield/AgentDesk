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
        // A name whose host marker is not tmux belongs to another host; it is left running.
        let refusal = crate::services::discord::admin_host_guard::marker_refusal;
        if let Some(reason) = name
            .starts_with("AgentDesk-")
            .then(|| refusal(name))
            .flatten()
        {
            println!("   skipped: {} ({})", name, reason);
            continue;
        }
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

#[cfg(test)]
#[cfg(unix)]
mod reset_tmux_host_tests {
    // `reset-tmux` leaves a listed session another host's marker claims untouched; an
    // unmarked AgentDesk session is killed as in main.
    #[test]
    fn reset_tmux_leaves_another_hosts_session_running() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let tmux = crate::services::discord::host_defer_gate::tests::ScriptedTmux::install();
        let (herdr, legacy) = (
            "AgentDesk-claude-p4c2-reset-h",
            "AgentDesk-claude-p4c2-reset-t",
        );
        let marker = crate::services::tmux_common::session_temp_path(herdr, "host_kind");
        std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(&marker, "herdr").unwrap();
        tmux.list(&[herdr, legacy]);

        super::kill_agentdesk_tmux_sessions_local();
        let calls = tmux.take_calls();
        let kills = |name: &str| {
            let kill = |call: &&String| call.starts_with("kill-session") && call.contains(name);
            calls.iter().filter(kill).count()
        };
        assert_eq!(kills(herdr), 0, "{calls:?}");
        assert_eq!(kills(legacy), 1, "{calls:?}");
    }
}
