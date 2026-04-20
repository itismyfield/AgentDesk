use std::sync::Arc;
use std::time::Duration;

use crate::services::agent_protocol::StreamMessage;
use crate::services::process::kill_pid_tree;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::{claude, codex, gemini, qwen};

pub async fn execute_simple(provider: ProviderKind, prompt: String) -> Result<String, String> {
    tokio::task::spawn_blocking(move || execute_simple_blocking(provider, prompt, None))
        .await
        .map_err(|e| format!("Task join error: {}", e))?
}

pub async fn execute_simple_with_timeout(
    provider: ProviderKind,
    prompt: String,
    timeout: Duration,
    stage_label: String,
) -> Result<String, String> {
    let cancel_for_timeout = Arc::new(CancelToken::new());
    let cancel_for_exec = Arc::clone(&cancel_for_timeout);
    let mut handle = tokio::task::spawn_blocking(move || {
        execute_simple_blocking(provider, prompt, Some(cancel_for_exec))
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
            Err(format!(
                "{stage_label} timed out after {}s",
                timeout.as_secs()
            ))
        }
    }
}

fn execute_simple_blocking(
    provider: ProviderKind,
    prompt: String,
    cancel_token: Option<Arc<CancelToken>>,
) -> Result<String, String> {
    match provider {
        ProviderKind::Claude => {
            claude::execute_command_simple_cancellable(&prompt, cancel_token.as_deref())
        }
        ProviderKind::Codex => {
            codex::execute_command_simple_cancellable(&prompt, cancel_token.as_deref())
        }
        ProviderKind::Gemini => {
            gemini::execute_command_simple_cancellable(&prompt, cancel_token.as_deref())
        }
        ProviderKind::Qwen => {
            qwen::execute_command_simple_cancellable(&prompt, cancel_token.as_deref())
        }
        ProviderKind::Unsupported(name) => Err(format!("Provider '{}' is not installed", name)),
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
    let mut handle = tokio::task::spawn_blocking(move || {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        fs::write(path, contents).unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    #[cfg(unix)]
    fn wait_for_pid_to_exit(pid: &str, timeout: Duration) -> bool {
        let started = std::time::Instant::now();
        while started.elapsed() < timeout {
            let alive = std::process::Command::new("kill")
                .args(["-0", pid])
                .status()
                .map(|status| status.success())
                .unwrap_or(false);
            if !alive {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[cfg(unix)]
    fn wait_for_file(path: &Path, timeout: Duration) -> bool {
        let started = std::time::Instant::now();
        while started.elapsed() < timeout {
            if path.exists() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        false
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_simple_with_timeout_kills_timed_out_codex_process() {
        let _env_guard = crate::services::discord::runtime_store::lock_test_env();
        let temp = tempfile::tempdir().unwrap();
        let fake_codex = temp.path().join("fake-codex");
        let pid_file = temp.path().join("fake-codex.pid");
        let previous_codex_path = std::env::var_os("AGENTDESK_CODEX_PATH");
        let previous_pid_file = std::env::var_os("AGENTDESK_TEST_PID_FILE");

        write_executable(
            &fake_codex,
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$AGENTDESK_TEST_PID_FILE\"\nwhile :; do :; done\n",
        );

        unsafe {
            std::env::set_var("AGENTDESK_CODEX_PATH", &fake_codex);
            std::env::set_var("AGENTDESK_TEST_PID_FILE", &pid_file);
        }

        let result = execute_simple_with_timeout(
            ProviderKind::Codex,
            "pick a meeting participant".to_string(),
            Duration::from_secs(1),
            "participant selection".to_string(),
        )
        .await;

        match previous_codex_path {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_CODEX_PATH", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_CODEX_PATH") },
        }
        match previous_pid_file {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_TEST_PID_FILE", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_TEST_PID_FILE") },
        }

        let error = result.expect_err("expected timeout");
        assert!(error.contains("participant selection timed out"));

        if wait_for_file(&pid_file, Duration::from_secs(2)) {
            let pid = fs::read_to_string(&pid_file).expect("fake codex pid file");
            assert!(
                wait_for_pid_to_exit(pid.trim(), Duration::from_secs(5)),
                "timed out process should be terminated after timeout"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn execute_structured_timeout_kills_timed_out_codex_process() {
        let _env_guard = crate::services::discord::runtime_store::lock_test_env();
        let temp = tempfile::tempdir().unwrap();
        let fake_codex = temp.path().join("fake-codex");
        let pid_file = temp.path().join("fake-codex-structured.pid");
        let previous_codex_path = std::env::var_os("AGENTDESK_CODEX_PATH");
        let previous_pid_file = std::env::var_os("AGENTDESK_TEST_PID_FILE");

        write_executable(
            &fake_codex,
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$AGENTDESK_TEST_PID_FILE\"\nwhile :; do :; done\n",
        );

        unsafe {
            std::env::set_var("AGENTDESK_CODEX_PATH", &fake_codex);
            std::env::set_var("AGENTDESK_TEST_PID_FILE", &pid_file);
        }

        let result = execute_structured(
            ProviderKind::Codex,
            "pick a meeting participant".to_string(),
            temp.path().display().to_string(),
            None,
            Vec::new(),
            None,
            1,
            "structured participant selection",
        )
        .await;

        match previous_codex_path {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_CODEX_PATH", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_CODEX_PATH") },
        }
        match previous_pid_file {
            Some(value) => unsafe { std::env::set_var("AGENTDESK_TEST_PID_FILE", value) },
            None => unsafe { std::env::remove_var("AGENTDESK_TEST_PID_FILE") },
        }

        let error = result.expect_err("expected timeout");
        assert!(error.contains("structured participant selection timeout"));

        if wait_for_file(&pid_file, Duration::from_secs(2)) {
            let pid = fs::read_to_string(&pid_file).expect("fake codex pid file");
            assert!(
                wait_for_pid_to_exit(pid.trim(), Duration::from_secs(5)),
                "timed out structured process should be terminated after timeout"
            );
        }
    }
}
