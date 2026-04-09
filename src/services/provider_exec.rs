use std::sync::mpsc;

use crate::services::agent_protocol::StreamMessage;
use crate::services::provider::ProviderKind;
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
    tokio::task::spawn_blocking(move || execute_structured_blocking(provider, prompt, request))
        .await
        .map_err(|e| format!("Task join error: {}", e))?
}

fn execute_structured_blocking(
    provider: ProviderKind,
    prompt: String,
    request: StructuredExecRequest,
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
            None,
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
            None,
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
            None,
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
            None,
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
