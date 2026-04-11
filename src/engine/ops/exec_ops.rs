use rquickjs::{Ctx, Function, Object, Result as JsResult};

// ── Exec ops ──────────────────────────────────────────────────────
//
// agentdesk.exec(command, args) → stdout string
// Runs a local command synchronously. Limited to safe commands.

fn exec_override_env_var(cmd: &str) -> String {
    format!(
        "AGENTDESK_{}_PATH",
        cmd.replace('-', "_").to_ascii_uppercase()
    )
}

pub(super) fn register_exec_ops<'js>(ctx: &Ctx<'js>) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;

    ad.set(
        "exec",
        Function::new(ctx.clone(), |cmd: String, args_json: String| -> String {
            // Only allow safe commands (tmux for read-only session queries)
            let allowed = ["gh", "git", "tmux"];
            if !allowed.contains(&cmd.as_str()) {
                return format!("ERROR: command '{}' not allowed", cmd);
            }

            let args: Vec<String> = serde_json::from_str(&args_json).unwrap_or_default();
            let command_path = std::env::var_os(exec_override_env_var(&cmd))
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| std::ffi::OsString::from(&cmd));
            match std::process::Command::new(&command_path)
                .args(&args)
                .output()
            {
                Ok(output) if output.status.success() => {
                    String::from_utf8_lossy(&output.stdout).trim().to_string()
                }
                Ok(output) => {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    format!("ERROR: {}", stderr.trim())
                }
                Err(e) => format!("ERROR: {}", e),
            }
        })?,
    )?;

    // JS wrapper to accept array directly
    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            var rawExec = agentdesk.exec;
            agentdesk.exec = function(cmd, args) {
                return rawExec(cmd, JSON.stringify(args || []));
            };
        })();
    "#,
    )?;

    // agentdesk.inflight.list() — list active inflight turns with started_at
    let inflight_obj = rquickjs::Object::new(ctx.clone())?;
    inflight_obj.set(
        "list",
        Function::new(ctx.clone(), || -> String {
            let mut results = Vec::new();
            if let Some(root) = crate::cli::agentdesk_runtime_root() {
                let inflight_dir = root.join("runtime/discord_inflight");
                for provider in &["claude", "codex", "gemini", "qwen"] {
                    let dir = inflight_dir.join(provider);
                    if let Ok(entries) = std::fs::read_dir(&dir) {
                        for entry in entries.flatten() {
                            let path = entry.path();
                            if path.extension().is_some_and(|e| e == "json")
                                && let Ok(content) = std::fs::read_to_string(&path)
                                && let Ok(data) =
                                    serde_json::from_str::<serde_json::Value>(&content)
                            {
                                let channel_id = path
                                    .file_stem()
                                    .and_then(|s| s.to_str())
                                    .unwrap_or("")
                                    .to_string();
                                // Map inflight file fields to output:
                                // channel_name → for agent identification
                                // tmux_session_name → for diagnostics
                                // session_id → Claude session ID
                                // session_key, dispatch_id → for long-turn detection (#130)
                                let channel_name = data
                                    .get("channel_name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                let tmux_name = data
                                    .get("tmux_session_name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                results.push(serde_json::json!({
                                    "channel_id": channel_id,
                                    "provider": provider,
                                    "started_at": data.get("started_at").and_then(|v| v.as_str()).unwrap_or(""),
                                    "updated_at": data.get("updated_at").and_then(|v| v.as_str()).unwrap_or(""),
                                    "channel_name": channel_name,
                                    "tmux_session_name": tmux_name,
                                    "session_id": data.get("session_id").and_then(|v| v.as_str()).unwrap_or(""),
                                    "session_key": data.get("session_key").and_then(|v| v.as_str()).unwrap_or(""),
                                    "dispatch_id": data.get("dispatch_id").and_then(|v| v.as_str()).unwrap_or(""),
                                }));
                            }
                        }
                    }
                }
            }
            serde_json::to_string(&results).unwrap_or_else(|_| "[]".to_string())
        }),
    )?;
    inflight_obj.set(
        "remove",
        Function::new(
            ctx.clone(),
            |provider: String, channel_id: String| -> String {
                if let Some(root) = crate::cli::agentdesk_runtime_root() {
                    let path = root
                        .join("runtime/discord_inflight")
                        .join(&provider)
                        .join(format!("{channel_id}.json"));
                    if path.exists() {
                        let _ = std::fs::remove_file(&path);
                        return format!(r#"{{"ok":true,"removed":"{}"}}"#, path.display());
                    }
                }
                r#"{"ok":false,"error":"not found"}"#.to_string()
            },
        ),
    )?;
    ad.set("inflight", inflight_obj)?;

    // JS wrapper
    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            var rawList = agentdesk.inflight.list;
            var rawRemove = agentdesk.inflight.remove;
            agentdesk.inflight.list = function() {
                return JSON.parse(rawList());
            };
            agentdesk.inflight.remove = function(provider, channelId) {
                return JSON.parse(rawRemove(provider, "" + channelId));
            };
        })();
    "#,
    )?;

    // agentdesk.session.sendCommand(sessionKey, command) — inject a slash command into a tmux session
    let session_obj = rquickjs::Object::new(ctx.clone())?;
    session_obj.set(
        "sendCommand",
        rquickjs::Function::new(
            ctx.clone(),
            |session_key: String, command: String| -> String {
                // session_key may be "hostname:tmux_name"
                let tmux_name = session_key
                    .split_once(':')
                    .map(|(_, name)| name)
                    .unwrap_or(&session_key);
                match crate::services::platform::tmux::send_keys(tmux_name, &[&command, "Enter"]) {
                    Ok(out) if out.status.success() => {
                        format!(
                            r#"{{"ok":true,"session":"{}","command":"{}"}}"#,
                            session_key, command
                        )
                    }
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        format!(r#"{{"ok":false,"error":"tmux: {}"}}"#, stderr.trim())
                    }
                    Err(e) => {
                        format!(r#"{{"ok":false,"error":"{}"}}"#, e)
                    }
                }
            },
        ),
    )?;

    // agentdesk.session.kill(sessionKey) — force-kill a tmux session (for deadlock recovery)
    session_obj.set(
        "kill",
        rquickjs::Function::new(ctx.clone(), |session_key: String| -> String {
            // session_key is "hostname:tmux_name"; tmux interprets colon as
            // session:window separator, so extract only the tmux_name part.
            let tmux_name = session_key
                .split_once(':')
                .map(|(_, name)| name)
                .unwrap_or(&session_key);
            crate::services::termination_audit::record_termination_for_tmux(
                tmux_name,
                None,
                "policy_engine",
                "session_kill_api",
                Some("force-kill via agentdesk.session.kill()"),
                None,
            );
            match crate::services::platform::tmux::kill_session_output(tmux_name) {
                Ok(out) if out.status.success() => {
                    format!(r#"{{"ok":true,"session":"{}"}}"#, session_key)
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    format!(r#"{{"ok":false,"error":"tmux: {}"}}"#, stderr.trim())
                }
                Err(e) => {
                    format!(r#"{{"ok":false,"error":"{}"}}"#, e)
                }
            }
        }),
    )?;

    ad.set("session", session_obj)?;

    Ok(())
}
