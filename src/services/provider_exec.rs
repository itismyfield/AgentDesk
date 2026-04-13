use std::sync::Arc;
use std::time::Duration;

use crate::services::agent_protocol::StreamMessage;
use crate::services::process::kill_pid_tree;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::{claude, codex, gemini, qwen};

pub async fn execute_simple(provider: ProviderKind, prompt: String) -> Result<String, String> {
    match provider {
        ProviderKind::Claude => {
            tokio::task::spawn_blocking(move || claude::execute_command_simple(&prompt))
                .await
                .map_err(|e| format!("Task join error: {}", e))?
        }
        ProviderKind::Codex => {
            tokio::task::spawn_blocking(move || codex::execute_command_simple(&prompt))
                .await
                .map_err(|e| format!("Task join error: {}", e))?
        }
        ProviderKind::Gemini => {
            tokio::task::spawn_blocking(move || gemini::execute_command_simple(&prompt))
                .await
                .map_err(|e| format!("Task join error: {}", e))?
        }
        ProviderKind::Qwen => {
            tokio::task::spawn_blocking(move || qwen::execute_command_simple(&prompt))
                .await
                .map_err(|e| format!("Task join error: {}", e))?
        }
        ProviderKind::Unsupported(name) => Err(format!("Provider '{}' is not installed", name)),
    }
}

pub async fn execute_simple_with_timeout(
    provider: ProviderKind,
    prompt: String,
    timeout: Duration,
    stage_label: String,
) -> Result<String, String> {
    match tokio::time::timeout(timeout, execute_simple(provider, prompt)).await {
        Ok(result) => result,
        Err(_) => Err(format!(
            "{stage_label} timed out after {}s",
            timeout.as_secs()
        )),
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn execute_structured(
    provider: ProviderKind,
    prompt: String,
    working_dir: String,
    system_prompt: Option<String>,
    allowed_tools: Vec<String>,
    model: Option<String>,
    timeout_secs: u64,
    stage_label: &'static str,
) -> Result<String, String> {
    let cancel_token = Arc::new(CancelToken::new());
    let cancel_for_timeout = Arc::clone(&cancel_token);
    let handle = tokio::task::spawn_blocking(move || {
        let (sender, receiver) = std::sync::mpsc::channel::<StreamMessage>();
        let system_prompt_ref = system_prompt.as_deref();
        let allowed_tools_ref = (!allowed_tools.is_empty()).then_some(allowed_tools.as_slice());
        let model_ref = model.as_deref();
        let result = match provider {
            ProviderKind::Claude => claude::execute_command_streaming(
                &prompt,
                None,
                &working_dir,
                sender.clone(),
                system_prompt_ref,
                allowed_tools_ref,
                Some(Arc::clone(&cancel_token)),
                None,
                None,
                None,
                None,
                model_ref,
                None,
            ),
            ProviderKind::Codex => codex::execute_command_streaming(
                &prompt,
                None,
                &working_dir,
                sender.clone(),
                system_prompt_ref,
                allowed_tools_ref,
                Some(Arc::clone(&cancel_token)),
                None,
                None,
                None,
                None,
                model_ref,
                None,
            ),
            ProviderKind::Gemini => gemini::execute_command_streaming(
                &prompt,
                None,
                &working_dir,
                sender.clone(),
                system_prompt_ref,
                allowed_tools_ref,
                Some(Arc::clone(&cancel_token)),
                None,
                None,
                None,
                None,
                model_ref,
                None,
            ),
            ProviderKind::Qwen => qwen::execute_command_streaming(
                &prompt,
                None,
                &working_dir,
                sender.clone(),
                system_prompt_ref,
                allowed_tools_ref,
                Some(Arc::clone(&cancel_token)),
                None,
                None,
                None,
                None,
                model_ref,
                None,
            ),
            ProviderKind::Unsupported(name) => Err(format!("Provider '{}' is not installed", name)),
        };
        drop(sender);
        collect_stream_result(result, receiver)
    });

    match tokio::time::timeout(Duration::from_secs(timeout_secs), handle).await {
        Ok(joined) => joined.map_err(|err| format!("Task join error: {err}"))?,
        Err(_) => {
            cancel_for_timeout.cancel_with_tmux_cleanup();
            if let Some(pid) = cancel_for_timeout
                .child_pid
                .lock()
                .ok()
                .and_then(|guard| *guard)
            {
                kill_pid_tree(pid);
            }
            Err(format!("{stage_label} timeout after {timeout_secs}s"))
        }
    }
}

fn collect_stream_result(
    provider_result: Result<(), String>,
    receiver: std::sync::mpsc::Receiver<StreamMessage>,
) -> Result<String, String> {
    let mut text = String::new();
    let mut done: Option<String> = None;
    let mut error: Option<String> = provider_result.err();

    for message in receiver.try_iter() {
        match message {
            StreamMessage::Text { content } => text.push_str(&content),
            StreamMessage::Done { result, .. } => {
                if !result.trim().is_empty() {
                    done = Some(result);
                }
            }
            StreamMessage::Error { message, .. } => {
                error = Some(message);
            }
            _ => {}
        }
    }

    if let Some(result) = done {
        return Ok(result.trim().to_string());
    }
    let text = text.trim().to_string();
    if !text.is_empty() {
        return Ok(text);
    }
    Err(error.unwrap_or_else(|| "Empty response from provider".to_string()))
}
