//! One-shot provider dispatch (#1100 boundary doc).
//!
//! This module is the single execution dispatch point for short-lived,
//! prompt-in / text-out provider invocations: `execute_simple_with_timeout`
//! and `execute_structured`. It owns the
//! `ProviderKind` → provider-specific helper match, timeout/cancel wiring, and
//! the small `collect_stream_result` helper.
//!
//! It deliberately does NOT own:
//! - long-lived session lifecycle, child stdin handles, or the in-memory
//!   process session registry — those live in
//!   [`crate::services::session_backend`] (`SessionBackend` trait,
//!   `ProcessBackend`, `insert_process_session`/`process_session_*`).
//! - JSONL stream parsing — also in `session_backend`
//!   (`parse_stream_message`, `process_stream_line`, …).
//! - shared low-level utilities used by every provider wrapper (line stream
//!   reader, allowed-tool compat) — those live in
//!   [`crate::services::provider_runtime`].
//!
//! See `docs/config-domains.md` for the domain boundaries this dispatch
//! consumes (runtime-config), and `docs/source-of-truth.md` for canonical
//! provider/session config write paths.

use std::sync::Arc;
use std::time::Duration;

use crate::services::agent_protocol::StreamMessage;
use crate::services::platform::with_provider_execution_context;
use crate::services::process::kill_pid_tree;
use crate::services::provider::{CancelToken, ProviderExecutionAdapter, ProviderKind};
use crate::services::provider_cli::ProviderExecutionContext;
use crate::services::{claude, codex, gemini, opencode, qwen};

pub async fn execute_simple_with_timeout(
    provider: ProviderKind,
    prompt: String,
    timeout: Duration,
    stage_label: String,
) -> Result<String, String> {
    execute_simple_with_timeout_and_context(provider, prompt, timeout, stage_label, None).await
}

pub async fn execute_simple_with_timeout_and_context(
    provider: ProviderKind,
    prompt: String,
    timeout: Duration,
    stage_label: String,
    context: Option<ProviderExecutionContext>,
) -> Result<String, String> {
    let cancel_for_timeout = Arc::new(CancelToken::new());
    let cancel_for_exec = Arc::clone(&cancel_for_timeout);
    let mut handle = tokio::task::spawn_blocking(move || {
        execute_simple_blocking(provider, prompt, Some(cancel_for_exec), context)
    });

    tokio::select! {
        joined = &mut handle => joined.map_err(|e| format!("Task join error: {}", e))?,
        _ = tokio::time::sleep(timeout) => {
            cancel_for_timeout.cancel_with_tmux_cleanup();
            if let Some(pid) = cancel_for_timeout
                .child_pid
                .lock()
                .ok()
                .and_then(|guard| *guard)
            {
                kill_pid_tree(pid);
            }
            let _ = tokio::time::timeout(Duration::from_secs(3), &mut handle).await;
            Err(simple_timeout_error(&stage_label, timeout))
        }
    }
}

fn execute_simple_blocking(
    provider: ProviderKind,
    prompt: String,
    cancel_token: Option<Arc<CancelToken>>,
    context: Option<ProviderExecutionContext>,
) -> Result<String, String> {
    let run = || execute_simple_blocking_inner(provider, prompt, cancel_token);
    if let Some(context) = context {
        with_provider_execution_context(context, run)
    } else {
        run()
    }
}

fn execute_simple_blocking_inner(
    provider: ProviderKind,
    prompt: String,
    cancel_token: Option<Arc<CancelToken>>,
) -> Result<String, String> {
    let Some(adapter) = provider.execution_adapter() else {
        return Err(format!("Provider '{}' is not installed", provider.as_str()));
    };
    match adapter {
        ProviderExecutionAdapter::Claude => {
            claude::execute_command_simple_cancellable(&prompt, cancel_token.as_deref())
        }
        ProviderExecutionAdapter::Codex => {
            codex::execute_command_simple_cancellable(&prompt, cancel_token.as_deref())
        }
        ProviderExecutionAdapter::Gemini => {
            gemini::execute_command_simple_cancellable(&prompt, cancel_token.as_deref())
        }
        ProviderExecutionAdapter::OpenCode => {
            opencode::execute_command_simple_cancellable(&prompt, cancel_token.as_deref())
        }
        ProviderExecutionAdapter::Qwen => {
            qwen::execute_command_simple_cancellable(&prompt, cancel_token.as_deref())
        }
    }
}

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
    execute_structured_with_context(
        provider,
        prompt,
        working_dir,
        system_prompt,
        allowed_tools,
        model,
        timeout_secs,
        stage_label,
        None,
    )
    .await
}

pub async fn execute_structured_with_context(
    provider: ProviderKind,
    prompt: String,
    working_dir: String,
    system_prompt: Option<String>,
    allowed_tools: Vec<String>,
    model: Option<String>,
    timeout_secs: u64,
    stage_label: &'static str,
    context: Option<ProviderExecutionContext>,
) -> Result<String, String> {
    let Some(adapter) = provider.execution_adapter() else {
        return Err(format!("Provider '{}' is not installed", provider.as_str()));
    };
    let cancel_token = Arc::new(CancelToken::new());
    let cancel_for_timeout = Arc::clone(&cancel_token);
    let mut handle = tokio::task::spawn_blocking(move || {
        let run = || {
            let (sender, receiver) = std::sync::mpsc::channel::<StreamMessage>();
            let system_prompt_ref = system_prompt.as_deref();
            let allowed_tools_ref = (!allowed_tools.is_empty()).then_some(allowed_tools.as_slice());
            let model_ref = model.as_deref();
            let result = match adapter {
                ProviderExecutionAdapter::Claude => claude::execute_command_streaming(
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
                    None,
                    crate::services::claude_compact_context::DEFAULT_CONTEXT_COMPACT_LOWER_BOUND_TOKENS,
                    None,
                    None,
                ),
                ProviderExecutionAdapter::Codex => codex::execute_command_streaming(
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
                    None,
                    None,
                    false,
                ),
                ProviderExecutionAdapter::Gemini => gemini::execute_command_streaming(
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
                ProviderExecutionAdapter::OpenCode => opencode::execute_command_streaming(
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
                ProviderExecutionAdapter::Qwen => qwen::execute_command_streaming(
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
            };
            drop(sender);
            collect_stream_result(result, receiver)
        };
        if let Some(context) = context {
            with_provider_execution_context(context, run)
        } else {
            run()
        }
    });

    tokio::select! {
        joined = &mut handle => joined.map_err(|err| format!("Task join error: {err}"))?,
        _ = tokio::time::sleep(Duration::from_secs(timeout_secs)) => {
            cancel_for_timeout.cancel_with_tmux_cleanup();
            if let Some(pid) = cancel_for_timeout
                .child_pid
                .lock()
                .ok()
                .and_then(|guard| *guard)
            {
                kill_pid_tree(pid);
            }
            if tokio::time::timeout(Duration::from_secs(3), &mut handle).await.is_err() {
                handle.abort();
            }
            Err(structured_timeout_error(stage_label, timeout_secs))
        }
    }
}

pub(crate) fn simple_timeout_error(stage_label: &str, timeout: Duration) -> String {
    format!("{stage_label} timed out after {}s", timeout.as_secs())
}

pub(crate) fn structured_timeout_error(stage_label: &str, timeout_secs: u64) -> String {
    format!("{stage_label} timeout after {timeout_secs}s")
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

    if let Some(error) = error {
        return Err(error);
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
