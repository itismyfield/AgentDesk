use crate::services::process::{configure_child_process_group, wait_with_output_timeout};
use rquickjs::{Ctx, Function, Object, Result as JsResult};
use std::ffi::OsStr;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

// ── Exec ops ──────────────────────────────────────────────────────
//
// agentdesk.exec(command, args, options?) → stdout string
// Runs a local command synchronously with a bounded timeout. Limited to safe commands.

const DEFAULT_EXEC_TIMEOUT_MS: u64 = 30_000;

fn exec_override_env_var(cmd: &str) -> String {
    format!(
        "AGENTDESK_{}_PATH",
        cmd.replace('-', "_").to_ascii_uppercase()
    )
}

fn parse_exec_args(args_json: &str) -> Vec<String> {
    if let Ok(args) = serde_json::from_str::<Vec<String>>(args_json) {
        return args;
    }

    serde_json::from_str::<String>(args_json)
        .ok()
        .and_then(|inner| serde_json::from_str(&inner).ok())
        .unwrap_or_default()
}

fn resolve_exec_timeout_ms(timeout_ms: Option<u64>) -> u64 {
    timeout_ms
        .filter(|timeout_ms| *timeout_ms > 0)
        .unwrap_or(DEFAULT_EXEC_TIMEOUT_MS)
}

fn run_exec_command(
    label: &str,
    command_path: &OsStr,
    args: &[String],
    timeout_ms: u64,
) -> Result<Output, String> {
    let mut command = Command::new(command_path);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_child_process_group(&mut command);

    let child = command
        .spawn()
        .map_err(|error| format!("Failed to start {}: {}", label, error))?;
    wait_with_output_timeout(child, Duration::from_millis(timeout_ms), label)
}

pub(super) fn register_exec_ops<'js>(ctx: &Ctx<'js>) -> JsResult<()> {
    let ad: Object<'js> = ctx.globals().get("agentdesk")?;

    ad.set(
        "exec",
        Function::new(
            ctx.clone(),
            |cmd: String, args_json: String, timeout_ms: Option<u64>| -> String {
                // Only allow safe commands (tmux for read-only session queries)
                let allowed = ["gh", "git", "tmux"];
                if !allowed.contains(&cmd.as_str()) {
                    return format!("ERROR: command '{}' not allowed", cmd);
                }

                let args = parse_exec_args(&args_json);
                let command_path = std::env::var_os(exec_override_env_var(&cmd))
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| std::ffi::OsString::from(&cmd));
                match run_exec_command(
                    &cmd,
                    command_path.as_os_str(),
                    &args,
                    resolve_exec_timeout_ms(timeout_ms),
                ) {
                    Ok(output) if output.status.success() => {
                        String::from_utf8_lossy(&output.stdout).trim().to_string()
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        format!("ERROR: {}", stderr.trim())
                    }
                    Err(error) => format!("ERROR: {}", error),
                }
            },
        )?,
    )?;

    // JS wrapper to accept array directly
    let _: rquickjs::Value = ctx.eval(
        r#"
        (function() {
            var rawExec = agentdesk.exec;
            function normalizeExecArgs(args) {
                if (typeof args === "string") return args;
                return JSON.stringify(args || []);
            }

            function normalizeExecTimeoutMs(options) {
                if (typeof options === "number" && isFinite(options) && options > 0) {
                    return Math.floor(options);
                }
                if (!options || typeof options !== "object") {
                    return 30000;
                }
                var raw = options.timeout_ms;
                if (!(typeof raw === "number" && isFinite(raw) && raw > 0)) {
                    raw = options.timeoutMs;
                }
                if (!(typeof raw === "number" && isFinite(raw) && raw > 0)) {
                    raw = options.timeout;
                }
                if (!(typeof raw === "number" && isFinite(raw) && raw > 0)) {
                    return 30000;
                }
                return Math.floor(raw);
            }

            agentdesk.exec = function(cmd, args, options) {
                return rawExec(cmd, normalizeExecArgs(args), normalizeExecTimeoutMs(options));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_exec_args_accepts_json_array_and_nested_json_string() {
        assert_eq!(
            parse_exec_args(r#"["--version"]"#),
            vec!["--version".to_string()]
        );
        assert_eq!(
            parse_exec_args(r#""[\"--version\"]""#),
            vec!["--version".to_string()]
        );
    }

    #[test]
    fn exec_wrapper_accepts_array_and_stringified_args() {
        let rt = rquickjs::Runtime::new().expect("runtime");
        let ctx = rquickjs::Context::full(&rt).expect("context");

        ctx.with(|ctx| {
            let globals = ctx.globals();
            let agentdesk = Object::new(ctx.clone()).expect("agentdesk object");
            globals.set("agentdesk", agentdesk).expect("set agentdesk");
            register_exec_ops(&ctx).expect("register exec ops");

            let array_result: String = ctx
                .eval(r#"agentdesk.exec("git", ["--version"], { timeout_ms: 1000 })"#)
                .expect("array exec");
            let string_result: String = ctx
                .eval(r#"agentdesk.exec("git", JSON.stringify(["--version"]), { timeout: 1000 })"#)
                .expect("string exec");

            assert!(
                array_result.contains("git version"),
                "expected git version output, got: {array_result}"
            );
            assert_eq!(array_result, string_result);
        });
    }

    #[test]
    fn exec_wrapper_runs_gh_with_timeout() {
        let rt = rquickjs::Runtime::new().expect("runtime");
        let ctx = rquickjs::Context::full(&rt).expect("context");

        ctx.with(|ctx| {
            let globals = ctx.globals();
            let agentdesk = Object::new(ctx.clone()).expect("agentdesk object");
            globals.set("agentdesk", agentdesk).expect("set agentdesk");
            register_exec_ops(&ctx).expect("register exec ops");

            let gh_version: String = ctx
                .eval(r#"agentdesk.exec("gh", ["--version"], { timeout_ms: 1000 })"#)
                .expect("gh exec");

            assert!(
                gh_version.contains("gh version"),
                "expected gh version output, got: {gh_version}"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn run_exec_command_times_out() {
        let args = vec!["-c".to_string(), "sleep 5".to_string()];
        let error = run_exec_command("test child", OsStr::new("sh"), &args, 20)
            .expect_err("expected timeout");

        assert!(error.contains("timed out"), "unexpected error: {error}");
    }
}
