//! Phase 1 of the claude-e rollout — PTY-backed `claude -p`-style adapter.
//!
//! `execute_streaming` mirrors the surface of `services::claude::
//! execute_command_streaming` for the cases that map cleanly onto a
//! per-turn `claude-e` spawn: local execution, no tmux, no remote SSH.
//! TUI hosting and tmux-wrapper runtime branches stay in `services::
//! claude` and are reached through `ProviderSessionDriver::TuiHosting`
//! or `ProviderSessionDriver::LegacyPrompt` instead.

use std::io::{BufRead, BufReader, Write};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::mpsc::Sender;

use serde_json::Value;

use crate::services::agent_protocol::{RuntimeHandoff, StreamMessage, is_valid_session_id};
use crate::services::claude_command::{
    ClaudeBinary, ClaudeCommandBuilder, ClaudeLaunchEnv, ClaudeLaunchIntent,
};
use crate::services::claude_compact_context::{
    apply_auto_compact_window_to_command, launch_auto_compact_window,
};
use crate::services::process::kill_child_tree;
use crate::services::provider::{
    CancelToken, ProviderKind, cancel_requested, register_child_pid, spawn_cancel_watchdog,
};
use crate::services::session_backend::{
    StreamLineState, emit_status_events_from_stream_json, observe_stream_context,
    parse_assistant_extra_tool_uses, parse_stream_message_with_state,
};

/// Phase 1 entry point. The signature matches the subset of
/// `claude::execute_command_streaming` parameters that are meaningful
/// for a per-turn `claude-e` invocation. TUI / tmux / remote profile
/// arguments are intentionally absent — those routes do not flow
/// through this adapter.
pub fn execute_streaming(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    system_prompt: Option<&str>,
    cancel_token: Option<Arc<CancelToken>>,
    report_channel_id: Option<u64>,
    _report_provider: Option<ProviderKind>,
    model_override: Option<&str>,
    fast_mode_enabled: Option<bool>,
    compact_percent: Option<u64>,
    compact_lower_bound_tokens: u64,
    _cache_ttl_minutes: Option<u32>,
    dispatch_type: Option<&str>,
) -> Result<(), String> {
    if let Some(sid) = session_id {
        if !is_valid_session_id(sid) {
            return Err("Invalid session ID format".to_string());
        }
    }

    let claude_e_bin = which::which("claude-e").map_err(|_| {
        "claude-e CLI not found. Install with `npm install -g claude-e`.".to_string()
    })?;
    let (claude_bin, _claude_resolution) = ClaudeBinary::resolve()?;

    let mut args: Vec<String> = vec!["--output-format".to_string(), "stream-json".to_string()];
    claude_bin.append_claude_e_bin_arg(&mut args);
    args.push("--no-session-footer".to_string());
    crate::services::claude::append_claude_mcp_config_arg(&mut args, dispatch_type);
    crate::services::claude::append_claude_fast_mode_arg(&mut args, fast_mode_enabled);
    if let Some(model) = model_override {
        args.push("--model".to_string());
        args.push(model.to_string());
    }
    if let Some(sp) = system_prompt {
        if !sp.is_empty() {
            args.push("--append-system-prompt".to_string());
            args.push(sp.to_string());
        }
    }
    if let Some(sid) = session_id {
        args.push("--resume".to_string());
        args.push(sid.to_string());
    }

    tracing::info!(
        binary = %claude_e_bin.display(),
        claude_binary = "resolved",
        working_dir = working_dir,
        session_id = session_id,
        model = model_override,
        "claude_e.execute_streaming spawning"
    );

    // claude-e is a fresh per-turn process and its real Claude child inherits
    // these variables, so it uses the same guarded gateway decision as native
    // fresh launches. The chokepoint builder applies that decision
    // by-construction; claude-e is a wrapper program, so no Claude binary
    // resolution PATH is applied here (claude-e is resolved separately above).
    // The launch env is resolved once so the same gateway decision drives BOTH
    // the compact-window computation (#4591) and the by-construction gateway
    // guard (#4559).
    let launch_env = ClaudeLaunchEnv::resolve(ClaudeLaunchIntent::Turn);
    let auto_compact_window = compact_percent.and_then(|percent| {
        launch_auto_compact_window(
            model_override,
            percent,
            compact_lower_bound_tokens,
            launch_env.gateway_proxy_env(),
        )
    });
    let mut builder = ClaudeCommandBuilder::for_wrapper_with_env(&claude_e_bin, launch_env);
    {
        let command = builder.command_mut();
        crate::services::process::configure_child_process_group(command);
        command
            .args(&args)
            .current_dir(working_dir)
            .env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "64000")
            .env("BASH_DEFAULT_TIMEOUT_MS", "86400000")
            .env("BASH_MAX_TIMEOUT_MS", "86400000")
            .env("CLAUDE_E_SKIP_STAR_PROMPT", "1")
            .env_remove("CLAUDECODE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Compact-window overlay (#4591): applied on top of the chokepoint's
        // by-construction gateway env so claude-e's real Claude child inherits
        // the freshly resolved immutable-launch threshold (or has any stale
        // parent value cleared when there is none).
        apply_auto_compact_window_to_command(command, auto_compact_window);
    }
    let mut command = builder.into_command();

    let mut child = command
        .spawn()
        .map_err(|e| format!("Failed to start claude-e: {}", e))?;

    let child_pid = child.id();
    register_child_pid(cancel_token.as_deref(), child_pid);
    let _cancel_watchdog =
        spawn_cancel_watchdog(cancel_token.clone(), child_pid, "claude-e-stream");

    // Send the RuntimeReady handoff so the turn-bridge stamps the
    // runtime kind for this dispatch. `output_path` is empty for
    // claude-e — Phase 1 streams JSONL directly through `sender` and
    // does not (yet) persist a per-turn transcript file. Phase 1.x can
    // wire a sidecar capture path here if recovery needs one.
    let session_name = session_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("claude-e-{}", std::process::id()));
    let _ = sender.send(StreamMessage::RuntimeReady {
        handoff: RuntimeHandoff::ClaudeEAdapter {
            output_path: String::new(),
            session_name,
            last_offset: 0,
            pid: child_pid,
        },
    });

    if let Some(mut stdin) = child.stdin.take() {
        if let Err(error) = stdin.write_all(prompt.as_bytes()) {
            tracing::warn!(?error, "claude-e stdin write failed");
        }
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture claude-e stdout".to_string())?;
    let reader = BufReader::new(stdout);

    let mut last_session_id: Option<String> = None;
    let mut last_model: Option<String> = None;
    let mut last_call_input_tokens: u64 = 0;
    let mut last_call_cache_create_tokens: u64 = 0;
    let mut last_call_cache_read_tokens: u64 = 0;
    let mut cumulative_output_tokens: u64 = 0;
    let mut saw_per_message_usage = false;
    let mut final_result: Option<String> = None;
    // Phase 1 counter-review MINOR-1: claude-e emits both
    // `system stop_hook_summary` (parser → Done with empty `result`)
    // and `result` (parser → Done with real content). Suppress the
    // empty Done; if no real Done arrives, surface the empty one at
    // end-of-stream as a fallback.
    let mut suppressed_empty_done: Option<StreamMessage> = None;
    let mut stream_state = StreamLineState::new();
    let mut line_count = 0u64;

    for line in reader.lines() {
        if cancel_requested(cancel_token.as_deref()) {
            kill_child_tree(&mut child);
            tracing::info!(
                line_count,
                session_id = last_session_id.as_deref(),
                "claude-e cancel observed; child killed"
            );
            return Ok(());
        }
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                let _ = sender.send(StreamMessage::Error {
                    message: format!("Failed to read claude-e output: {}", error),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
                break;
            }
        };
        line_count += 1;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(json) = serde_json::from_str::<Value>(&line) else {
            tracing::debug!(line = %line.chars().take(200).collect::<String>(), "claude-e non-JSON line skipped");
            continue;
        };

        // Mirror the per-record token accounting from
        // `claude::execute_command_streaming` so that StatusUpdate
        // tokens are derived consistently even though claude-e omits
        // `result.duration_ms` / `total_cost_usd` / `modelUsage`. The
        // missing-field handling collapses to `None` automatically via
        // `serde_json::Value::get`.
        let msg_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if msg_type == "assistant" {
            if let Some(msg_obj) = json.get("message") {
                if let Some(model) = msg_obj.get("model").and_then(|v| v.as_str()) {
                    last_model = Some(model.to_string());
                }
                if let Some(usage) = msg_obj.get("usage") {
                    saw_per_message_usage = true;
                    last_call_input_tokens = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    last_call_cache_read_tokens = usage
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    last_call_cache_create_tokens = usage
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    if let Some(out) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
                        cumulative_output_tokens = cumulative_output_tokens.saturating_add(out);
                    }
                }
            }
        }
        if msg_type == "result" {
            let cost_usd = json.get("cost_usd").and_then(|v| v.as_f64());
            let total_cost_usd = json.get("total_cost_usd").and_then(|v| v.as_f64());
            let duration_ms = json.get("duration_ms").and_then(|v| v.as_u64());
            let num_turns = json
                .get("num_turns")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            if !saw_per_message_usage {
                if let Some(usage) = json.get("usage") {
                    last_call_input_tokens = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    last_call_cache_read_tokens = usage
                        .get("cache_read_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    last_call_cache_create_tokens = usage
                        .get("cache_creation_input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    cumulative_output_tokens = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                }
            }
            if cost_usd.is_some() || total_cost_usd.is_some() || last_model.is_some() {
                let _ = sender.send(StreamMessage::StatusUpdate {
                    model: last_model.clone(),
                    cost_usd,
                    total_cost_usd,
                    duration_ms,
                    num_turns,
                    input_tokens: (last_call_input_tokens > 0).then_some(last_call_input_tokens),
                    cache_create_tokens: (last_call_cache_create_tokens > 0)
                        .then_some(last_call_cache_create_tokens),
                    cache_read_tokens: (last_call_cache_read_tokens > 0)
                        .then_some(last_call_cache_read_tokens),
                    output_tokens: (cumulative_output_tokens > 0)
                        .then_some(cumulative_output_tokens),
                });
            }
        }

        observe_stream_context(&json, &mut stream_state);
        if !emit_status_events_from_stream_json(&json, &sender) {
            kill_child_tree(&mut child);
            return Ok(());
        }

        let Some(msg) = parse_stream_message_with_state(&json, &stream_state) else {
            continue;
        };
        match &msg {
            StreamMessage::Init { session_id, .. } => {
                last_session_id = Some(session_id.clone());
            }
            StreamMessage::Done { result, session_id } => {
                if let Some(sid) = session_id {
                    last_session_id = Some(sid.clone());
                }
                if result.is_empty() && final_result.is_none() {
                    // Phase 1 counter-review MINOR-1 fix: buffer the
                    // empty Done synthesized from `stop_hook_summary`
                    // until we know whether a real `result` Done is
                    // coming. Emitted at end-of-stream only if no
                    // non-empty Done shows up.
                    suppressed_empty_done = Some(msg.clone());
                    continue;
                }
                final_result = Some(result.clone());
            }
            _ => {}
        }
        if sender.send(msg).is_err() {
            // Consumer hung up — kill child and stop.
            kill_child_tree(&mut child);
            return Ok(());
        }
        // Phase 1 counter-review MAJOR-1 fix: an assistant content
        // array can carry `[text, tool_use_a, tool_use_b, ...]`, but
        // `parse_stream_message_with_state` returns only the first
        // record. `claude::execute_command_streaming` follows up with
        // `parse_assistant_extra_tool_uses` for the remaining
        // tool_use entries; claude-e dispatch must mirror that or
        // multi-tool turns lose tool_use trace events.
        for extra in parse_assistant_extra_tool_uses(&json) {
            if sender.send(extra).is_err() {
                kill_child_tree(&mut child);
                return Ok(());
            }
        }
    }

    // Stream ended naturally. Wait for child + propagate exit code on error.
    let exit_status = child
        .wait()
        .map_err(|e| format!("claude-e wait failed: {}", e))?;
    if !exit_status.success() {
        let mut stderr_buf = String::new();
        if let Some(mut stderr) = child.stderr.take() {
            use std::io::Read;
            let _ = stderr.read_to_string(&mut stderr_buf);
        }
        let _ = sender.send(StreamMessage::Error {
            message: format!("claude-e exited with code {:?}", exit_status.code()),
            stdout: String::new(),
            stderr: stderr_buf,
            exit_code: exit_status.code(),
        });
    }

    // Phase 1: end-of-stream Done handling.
    //   1. A non-empty Done already flowed through `sender` → nothing more
    //      to do (final_result is Some).
    //   2. Only the empty `stop_hook_summary`-derived Done arrived
    //      (suppressed_empty_done is Some) → emit it now so the turn
    //      bridge can finalise even without a `result` record.
    //   3. No Done at all (claude-e crashed before either record) →
    //      synthesize an empty Done if the child exited cleanly so the
    //      turn bridge does not stall.
    if final_result.is_none() {
        if let Some(empty_done) = suppressed_empty_done.take() {
            let _ = sender.send(empty_done);
        } else if exit_status.success() {
            let _ = sender.send(StreamMessage::Done {
                result: String::new(),
                session_id: last_session_id.clone(),
            });
        }
    }

    let _ = report_channel_id;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::claude_command::ClaudeLaunchEnv;

    fn wrapper_command_env(
        launch_env: ClaudeLaunchEnv,
        auto_compact_window: Option<u64>,
    ) -> std::collections::HashMap<String, Option<String>> {
        // Mirror the production claude-e spawn: a wrapper builder (no Claude
        // binary resolution) applies the gateway env by construction, then the
        // compact-window overlay (#4591) is applied on top.
        let mut builder = ClaudeCommandBuilder::build_for_test("claude-e", None, launch_env);
        apply_auto_compact_window_to_command(builder.command_mut(), auto_compact_window);
        builder
            .into_command()
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    #[test]
    fn claude_e_command_receives_authoritative_launch_env() {
        // Inject: the chokepoint applies the gateway env by construction and the
        // compact-window overlay pins the freshly resolved immutable threshold.
        let injected = wrapper_command_env(
            ClaudeLaunchEnv::inject_for_test("http://127.0.0.1:10100"),
            Some(700_000),
        );
        assert_eq!(
            injected.get("ANTHROPIC_BASE_URL"),
            Some(&Some("http://127.0.0.1:10100".to_string()))
        );
        assert_eq!(
            injected.get("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"),
            Some(&Some("1".to_string()))
        );
        assert_eq!(
            injected.get("CLAUDE_CODE_AUTO_COMPACT_WINDOW"),
            Some(&Some("700000".to_string()))
        );

        // Scrub: the gateway vars are removed and, with no resolved window, any
        // inherited absolute compact window is cleared rather than propagated.
        let scrubbed = wrapper_command_env(ClaudeLaunchEnv::scrub_for_test(), None);
        assert_eq!(scrubbed.get("ANTHROPIC_BASE_URL"), Some(&None));
        assert_eq!(
            scrubbed.get("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY"),
            Some(&None)
        );
        assert_eq!(scrubbed.get("CLAUDE_CODE_AUTO_COMPACT_WINDOW"), Some(&None));
    }
}
