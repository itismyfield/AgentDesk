use serde_json::Value;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::mpsc::Sender;

use crate::services::claude::{self, CancelToken, StreamMessage};
use crate::services::provider::ProviderKind;
use crate::services::remote::RemoteProfile;

static GEMINI_PATH: OnceLock<Option<String>> = OnceLock::new();
pub const DEFAULT_GEMINI_MODEL: &str = "gemini-2.5-flash";

pub fn resolve_gemini_path() -> Option<String> {
    if let Some(path) = crate::services::platform::resolve_binary_with_login_shell("gemini") {
        return Some(path);
    }

    let home = dirs::home_dir().unwrap_or_default();
    let mut known_paths = vec![home.join(".local/bin/gemini"), home.join("bin/gemini")];
    #[cfg(unix)]
    {
        known_paths.push(PathBuf::from("/usr/local/bin/gemini"));
        known_paths.push(PathBuf::from("/opt/homebrew/bin/gemini"));
    }
    #[cfg(windows)]
    {
        known_paths.push(home.join("AppData/Local/Programs/gemini/gemini.exe"));
        known_paths.push(PathBuf::from("C:/Program Files/gemini/gemini.exe"));
    }

    for path in &known_paths {
        if path.is_file() {
            return Some(path.display().to_string());
        }
    }

    None
}

fn get_gemini_path() -> Option<&'static str> {
    GEMINI_PATH.get_or_init(resolve_gemini_path).as_deref()
}

pub fn execute_command_simple(prompt: &str) -> Result<String, String> {
    let gemini_bin = get_gemini_path().ok_or_else(|| "Gemini CLI not found".to_string())?;
    let working_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let output = Command::new(gemini_bin)
        .args(build_exec_args(prompt, None))
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to start Gemini: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !output.status.success() {
        return Err(derive_error_message(
            &stdout,
            &stderr,
            output.status.code(),
            "Gemini",
        ));
    }

    let text = extract_text_from_stream_output(&stdout);
    if text.trim().is_empty() {
        Err("Empty response from Gemini".to_string())
    } else {
        Ok(text)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn execute_command_streaming(
    prompt: &str,
    _session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
    cancel_token: Option<Arc<CancelToken>>,
    remote_profile: Option<&RemoteProfile>,
    _tmux_session_name: Option<&str>,
    _report_channel_id: Option<u64>,
    _report_provider: Option<ProviderKind>,
    model: Option<&str>,
) -> Result<(), String> {
    if remote_profile.is_some() {
        return Err("Gemini provider does not support remote execution yet".to_string());
    }

    let gemini_bin = get_gemini_path().ok_or_else(|| "Gemini CLI not found".to_string())?;
    let prompt = compose_gemini_prompt(prompt, system_prompt, allowed_tools);

    let mut child = Command::new(gemini_bin)
        .args(build_exec_args(&prompt, model))
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start Gemini: {}", e))?;

    if let Some(ref token) = cancel_token {
        *token.child_pid.lock().unwrap() = Some(child.id());
        if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            claude::kill_child_tree(&mut child);
            let _ = child.wait();
            return Ok(());
        }
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture Gemini stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Failed to capture Gemini stderr".to_string())?;

    let stderr_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        let mut reader = BufReader::new(stderr);
        let _ = reader.read_to_string(&mut buf);
        buf
    });

    let mut final_text = String::new();
    let mut raw_stdout = String::new();
    let mut last_session_id: Option<String> = None;
    let mut init_model: Option<String> = None;

    for line_result in BufReader::new(stdout).lines() {
        if let Some(ref token) = cancel_token {
            if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                claude::kill_child_tree(&mut child);
                let _ = child.wait();
                return Ok(());
            }
        }

        let line = match line_result {
            Ok(line) => line,
            Err(e) => return Err(format!("Failed reading Gemini output: {}", e)),
        };
        if line.trim().is_empty() {
            continue;
        }
        raw_stdout.push_str(&line);
        raw_stdout.push('\n');

        let Ok(json) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };

        match json.get("type").and_then(|v| v.as_str()) {
            Some("init") => {
                if let Some(session_id) = json.get("session_id").and_then(|v| v.as_str()) {
                    last_session_id = Some(session_id.to_string());
                    let _ = sender.send(StreamMessage::Init {
                        session_id: session_id.to_string(),
                    });
                }
                init_model = json
                    .get("model")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
            }
            Some("message") => {
                let role = json.get("role").and_then(|v| v.as_str());
                let content = json.get("content").and_then(|v| v.as_str()).unwrap_or("");
                if role == Some("assistant") && !content.is_empty() {
                    final_text.push_str(content);
                    let _ = sender.send(StreamMessage::Text {
                        content: content.to_string(),
                    });
                }
            }
            Some("result") => {
                let stats = json.get("stats");
                let model_name = init_model.clone().or_else(|| {
                    stats
                        .and_then(|value| value.get("models"))
                        .and_then(|value| value.as_object())
                        .and_then(|models| models.keys().next().cloned())
                });
                let input_tokens = stats
                    .and_then(|value| value.get("input_tokens"))
                    .and_then(|value| value.as_u64())
                    .or_else(|| {
                        stats
                            .and_then(|value| value.get("input"))
                            .and_then(|value| value.as_u64())
                    });
                let output_tokens = stats
                    .and_then(|value| value.get("output_tokens"))
                    .and_then(|value| value.as_u64());
                let duration_ms = stats
                    .and_then(|value| value.get("duration_ms"))
                    .and_then(|value| value.as_u64());
                let _ = sender.send(StreamMessage::StatusUpdate {
                    model: model_name,
                    cost_usd: None,
                    total_cost_usd: None,
                    duration_ms,
                    num_turns: None,
                    input_tokens,
                    output_tokens,
                });
            }
            _ => {}
        }
    }

    let status = child
        .wait()
        .map_err(|e| format!("Failed waiting for Gemini: {}", e))?;
    let stderr = stderr_handle.join().unwrap_or_default();

    if !status.success() {
        let _ = sender.send(StreamMessage::Error {
            message: derive_error_message(&raw_stdout, &stderr, status.code(), "Gemini"),
            stdout: raw_stdout,
            stderr,
            exit_code: status.code(),
        });
        return Ok(());
    }

    let result = final_text.trim().to_string();
    if result.is_empty() {
        let _ = sender.send(StreamMessage::Error {
            message: "Empty response from Gemini".to_string(),
            stdout: raw_stdout,
            stderr,
            exit_code: status.code(),
        });
        return Ok(());
    }

    let _ = sender.send(StreamMessage::Done {
        result,
        session_id: last_session_id,
    });
    Ok(())
}

fn compose_gemini_prompt(
    prompt: &str,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
) -> String {
    let mut sections = Vec::new();

    if let Some(system_prompt) = system_prompt
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        sections.push(format!(
            "[Authoritative Instructions]\n{}\n\nThese instructions are authoritative for this turn. Follow them over any generic assistant persona unless the user explicitly asks to inspect or compare them.",
            system_prompt
        ));
    }

    if let Some(allowed_tools) = allowed_tools.filter(|tools| !tools.is_empty()) {
        sections.push(format!(
            "[Tool Policy]\nIf tools are needed, stay within this allowlist unless the user explicitly asks to change it: {}",
            allowed_tools.join(", ")
        ));
    }

    if sections.is_empty() {
        return prompt.to_string();
    }

    sections.push(format!("[User Request]\n{}", prompt));
    sections.join("\n\n")
}

fn build_exec_args(prompt: &str, model: Option<&str>) -> Vec<String> {
    let mut args = Vec::new();
    let model = model
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_GEMINI_MODEL);

    args.push("-m".to_string());
    args.push(model.to_string());
    args.push("-p".to_string());
    args.push(prompt.to_string());
    args.push("--output-format".to_string());
    args.push("stream-json".to_string());
    args.push("-y".to_string());
    args.push("--sandbox".to_string());
    args.push("false".to_string());
    args
}

fn extract_text_from_stream_output(output: &str) -> String {
    let mut final_text = String::new();
    for line in output.lines() {
        let Ok(json) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        let is_assistant = json.get("type").and_then(|v| v.as_str()) == Some("message")
            && json.get("role").and_then(|v| v.as_str()) == Some("assistant");
        if !is_assistant {
            continue;
        }
        let content = json.get("content").and_then(|v| v.as_str()).unwrap_or("");
        if !content.is_empty() {
            final_text.push_str(content);
        }
    }
    final_text.trim().to_string()
}

fn derive_error_message(stdout: &str, stderr: &str, exit_code: Option<i32>, label: &str) -> String {
    if !stderr.trim().is_empty() {
        return stderr.trim().to_string();
    }

    if let Some(last) = stdout.lines().rev().find(|line| !line.trim().is_empty()) {
        let last = last.trim();
        if !last.is_empty() {
            return last.to_string();
        }
    }

    format!("{} exited with code {:?}", label, exit_code)
}
