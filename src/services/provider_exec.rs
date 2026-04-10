use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
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

async fn await_with_timeout<T, F>(label: &str, timeout: Duration, future: F) -> Result<T, String>
where
    F: Future<Output = Result<T, String>>,
{
    match tokio::time::timeout(timeout, future).await {
        Ok(result) => result,
        Err(_) => Err(format!("{} timed out after {}s", label, timeout.as_secs())),
    }
}

pub async fn execute_simple_with_timeout(
    provider: ProviderKind,
    prompt: String,
    timeout: Duration,
    label: &str,
) -> Result<String, String> {
    let label = label.to_string();
    tokio::task::spawn_blocking(move || match provider {
        ProviderKind::Claude => {
            claude::execute_command_simple_with_timeout(&prompt, timeout, &label)
        }
        ProviderKind::Codex => codex::execute_command_simple_with_timeout(&prompt, timeout, &label),
        ProviderKind::Gemini => {
            gemini::execute_command_simple_with_timeout(&prompt, timeout, &label)
        }
        ProviderKind::Qwen => qwen::execute_command_simple_with_timeout(&prompt, timeout, &label),
        ProviderKind::Unsupported(name) => Err(format!("Provider '{}' is not installed", name)),
    })
    .await
    .map_err(|e| format!("Task join error: {}", e))?
}

#[derive(Clone, Debug)]
pub struct StructuredExecRequest {
    pub working_dir: String,
    pub system_prompt: Option<String>,
    pub allowed_tools: Vec<String>,
    pub model: Option<String>,
}

impl StructuredExecRequest {
    pub fn new(working_dir: String) -> Self {
        Self {
            working_dir,
            system_prompt: None,
            allowed_tools: Vec::new(),
            model: None,
        }
    }
}

pub async fn execute_structured(
    provider: ProviderKind,
    prompt: String,
    request: StructuredExecRequest,
) -> Result<String, String> {
    tokio::task::spawn_blocking(move || {
        execute_structured_blocking(provider, prompt, request, None)
    })
    .await
    .map_err(|e| format!("Task join error: {}", e))?
}

pub async fn execute_structured_with_timeout(
    provider: ProviderKind,
    prompt: String,
    request: StructuredExecRequest,
    timeout: Duration,
    label: &str,
) -> Result<String, String> {
    let label_owned = label.to_string();
    let cancel_token = Arc::new(CancelToken::new());
    let execution_token = cancel_token.clone();
    let handle = tokio::task::spawn_blocking(move || {
        execute_structured_blocking(provider, prompt, request, Some(execution_token))
    });

    match tokio::time::timeout(timeout, handle).await {
        Ok(join_result) => join_result.map_err(|e| format!("Task join error: {}", e))?,
        Err(_) => {
            cancel_provider_token(&cancel_token);
            Err(format!(
                "{} timed out after {}s",
                label_owned,
                timeout.as_secs()
            ))
        }
    }
}

fn execute_structured_blocking(
    provider: ProviderKind,
    prompt: String,
    request: StructuredExecRequest,
    cancel_token: Option<Arc<CancelToken>>,
) -> Result<String, String> {
    let (tx, rx) = mpsc::channel::<StreamMessage>();
    let system_prompt = request.system_prompt.as_deref();
    let allowed_tools = Some(request.allowed_tools.as_slice());
    let working_dir = request.working_dir;
    let model = request.model.as_deref();

    match provider {
        ProviderKind::Claude => claude::execute_command_streaming(
            &prompt,
            None,
            &working_dir,
            tx,
            system_prompt,
            allowed_tools,
            cancel_token.clone(),
            None,
            None,
            None,
            None,
            model,
            None,
        )?,
        ProviderKind::Codex => codex::execute_command_streaming(
            &prompt,
            None,
            &working_dir,
            tx,
            system_prompt,
            allowed_tools,
            cancel_token.clone(),
            None,
            None,
            None,
            None,
            model,
            None,
        )?,
        ProviderKind::Gemini => gemini::execute_command_streaming(
            &prompt,
            None,
            &working_dir,
            tx,
            system_prompt,
            allowed_tools,
            cancel_token.clone(),
            None,
            None,
            None,
            None,
            model,
            None,
        )?,
        ProviderKind::Qwen => qwen::execute_command_streaming(
            &prompt,
            None,
            &working_dir,
            tx,
            system_prompt,
            allowed_tools,
            cancel_token.clone(),
            None,
            None,
            None,
            None,
            model,
            None,
        )?,
        ProviderKind::Unsupported(name) => {
            return Err(format!("Provider '{}' is not installed", name));
        }
    }

    collect_stream_output(rx)
}

fn cancel_provider_token(cancel_token: &CancelToken) {
    cancel_token.cancelled.store(true, Ordering::Relaxed);
    if let Some(pid) = cancel_token.child_pid.lock().ok().and_then(|guard| *guard) {
        kill_pid_tree(pid);
    }
    cancel_token.cancel_with_tmux_cleanup();
}

fn collect_stream_output(rx: mpsc::Receiver<StreamMessage>) -> Result<String, String> {
    let mut text_chunks = Vec::new();
    let mut final_result: Option<String> = None;

    for message in rx.try_iter() {
        match message {
            StreamMessage::Text { content } => {
                if !content.trim().is_empty() {
                    text_chunks.push(content);
                }
            }
            StreamMessage::Done { result, .. } => {
                if !result.trim().is_empty() {
                    final_result = Some(result);
                }
            }
            StreamMessage::Error {
                message, stderr, ..
            } => {
                let detail = stderr.trim();
                return Err(if detail.is_empty() {
                    message
                } else {
                    format!("{message}: {detail}")
                });
            }
            _ => {}
        }
    }

    let result = final_result.unwrap_or_else(|| text_chunks.join(""));
    let trimmed = result.trim().to_string();
    if trimmed.is_empty() {
        Err("Empty response from provider runtime".to_string())
    } else {
        Ok(trimmed)
    }
}

#[cfg(test)]
mod tests {
    use super::await_with_timeout;
    use std::time::Duration;

    #[tokio::test]
    async fn await_with_timeout_returns_inner_result_before_deadline() {
        let result = await_with_timeout("selection stage", Duration::from_millis(50), async {
            Ok::<_, String>("ok".to_string())
        })
        .await
        .expect("result should succeed");

        assert_eq!(result, "ok");
    }

    #[tokio::test]
    async fn await_with_timeout_errors_after_deadline() {
        let error = await_with_timeout("selection stage", Duration::from_millis(10), async {
            tokio::time::sleep(Duration::from_millis(30)).await;
            Ok::<_, String>("late".to_string())
        })
        .await
        .expect_err("timeout should fail");

        assert!(error.contains("selection stage timed out after 0s"));
    }
}
