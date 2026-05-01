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

#[cfg(windows)]
fn is_powershell_script(path: &OsStr) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("ps1"))
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn is_powershell_script(_path: &OsStr) -> bool {
    false
}

fn run_exec_command(
    label: &str,
    command_path: &OsStr,
    args: &[String],
    timeout_ms: u64,
) -> Result<Output, String> {
    let mut command = if cfg!(windows) && is_powershell_script(command_path) {
        let mut command = Command::new("pwsh");
        command
            .arg("-NoProfile")
            .arg("-ExecutionPolicy")
            .arg("Bypass")
            .arg("-File")
            .arg(command_path);
        command
    } else {
        Command::new(command_path)
    };
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
        "__execRaw",
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
                return agentdesk.__execRaw(
                    cmd,
                    normalizeExecArgs(args),
                    normalizeExecTimeoutMs(options)
                );
            };
        })();
    "#,
    )?;

    // agentdesk.inflight.list() — list active inflight turns with started_at
    let inflight_obj = rquickjs::Object::new(ctx.clone())?;
    inflight_obj.set(
        "__listRaw",
        Function::new(ctx.clone(), || -> String {
            let mut results = Vec::new();
            if let Some(root) = crate::cli::agentdesk_runtime_root() {
                let inflight_dir = root.join("runtime/discord_inflight");
                for provider in crate::services::provider::supported_provider_ids() {
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
                                // synthetic rebind fields → let policies distinguish adopted
                                // tmux sessions from real Discord foreground turns.
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
                                    "session_id": data.get("session_id").cloned().unwrap_or(serde_json::Value::Null),
                                    "request_owner_user_id": data.get("request_owner_user_id").cloned().unwrap_or_else(|| serde_json::json!(0)),
                                    "user_msg_id": data.get("user_msg_id").cloned().unwrap_or_else(|| serde_json::json!(0)),
                                    "any_tool_used": data.get("any_tool_used").cloned().unwrap_or_else(|| serde_json::json!(false)),
                                    "has_post_tool_text": data.get("has_post_tool_text").cloned().unwrap_or_else(|| serde_json::json!(false)),
                                    "rebind_origin": data.get("rebind_origin").cloned().unwrap_or_else(|| serde_json::json!(false)),
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
        "__removeRaw",
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
            agentdesk.inflight.list = function() {
                return JSON.parse(agentdesk.inflight.__listRaw());
            };
            agentdesk.inflight.remove = function(provider, channelId) {
                return JSON.parse(agentdesk.inflight.__removeRaw(provider, "" + channelId));
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
            match crate::services::platform::tmux::kill_session_output_with_reason(
                tmux_name,
                "force-kill via agentdesk.session.kill()",
            ) {
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
mod inflight_list_tests {
    use super::*;

    struct EnvVarOverride {
        _guard: std::sync::MutexGuard<'static, ()>,
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarOverride {
        fn set_path(key: &'static str, value: &std::path::Path) -> Self {
            let guard = crate::config::shared_test_env_lock()
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let previous = std::env::var_os(key);
            unsafe { std::env::set_var(key, value) };
            Self {
                _guard: guard,
                key,
                previous,
            }
        }
    }

    impl Drop for EnvVarOverride {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(self.key, value) },
                None => unsafe { std::env::remove_var(self.key) },
            }
        }
    }

    #[test]
    fn inflight_list_exposes_synthetic_rebind_fields() {
        let temp = tempfile::TempDir::new().expect("temp root");
        let _env = EnvVarOverride::set_path("AGENTDESK_ROOT_DIR", temp.path());
        let inflight_dir = temp.path().join("runtime/discord_inflight/claude");
        std::fs::create_dir_all(&inflight_dir).expect("inflight dir");
        std::fs::write(
            inflight_dir.join("12345.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "started_at": "2026-05-01T00:00:00Z",
                "updated_at": "2026-05-01T00:02:00Z",
                "channel_name": "project-agentdesk",
                "tmux_session_name": "AgentDesk-claude-project-agentdesk",
                "session_id": null,
                "request_owner_user_id": 0,
                "user_msg_id": 0,
                "any_tool_used": false,
                "has_post_tool_text": false,
                "rebind_origin": true,
                "session_key": "claude:AgentDesk-claude-project-agentdesk",
                "dispatch_id": "dispatch-1"
            }))
            .expect("serialize inflight"),
        )
        .expect("write inflight");

        let rt = rquickjs::Runtime::new().expect("runtime");
        let ctx = rquickjs::Context::full(&rt).expect("context");

        ctx.with(|ctx| {
            let globals = ctx.globals();
            let agentdesk = Object::new(ctx.clone()).expect("agentdesk object");
            globals.set("agentdesk", agentdesk).expect("set agentdesk");
            register_exec_ops(&ctx).expect("register exec ops");

            let listed: String = ctx
                .eval(
                    r#"
                    const inf = agentdesk.inflight.list().find(function(item) {
                        return item.provider === "claude" && item.channel_id === "12345";
                    });
                    JSON.stringify(inf || null);
                    "#,
                )
                .expect("list inflights");
            let parsed: serde_json::Value = serde_json::from_str(&listed).expect("json");

            assert_ne!(
                parsed,
                serde_json::Value::Null,
                "expected inflight: {listed}"
            );
            assert_eq!(parsed["session_id"], serde_json::Value::Null);
            assert_eq!(parsed["request_owner_user_id"], 0);
            assert_eq!(parsed["user_msg_id"], 0);
            assert_eq!(parsed["any_tool_used"], false);
            assert_eq!(parsed["has_post_tool_text"], false);
            assert_eq!(parsed["rebind_origin"], true);
            assert_eq!(
                parsed["session_key"],
                "claude:AgentDesk-claude-project-agentdesk"
            );
            assert_eq!(parsed["dispatch_id"], "dispatch-1");
        });
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
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
    fn exec_wrapper_runs_git_with_timeout() {
        let rt = rquickjs::Runtime::new().expect("runtime");
        let ctx = rquickjs::Context::full(&rt).expect("context");
        let version_timeout_ms = 5_000;

        ctx.with(|ctx| {
            let globals = ctx.globals();
            let agentdesk = Object::new(ctx.clone()).expect("agentdesk object");
            globals.set("agentdesk", agentdesk).expect("set agentdesk");
            register_exec_ops(&ctx).expect("register exec ops");

            let git_version: String = ctx
                .eval(format!(
                    r#"agentdesk.exec("git", ["--version"], {{ timeout_ms: {} }})"#,
                    version_timeout_ms
                ))
                .expect("git exec");

            assert!(
                git_version.contains("git version"),
                "expected git version output, got: {git_version}"
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
