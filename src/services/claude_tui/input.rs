use std::process::Output;
use std::time::{Duration, Instant};

const DEFAULT_LITERAL_CHUNK_CHARS: usize = 1800;
const PROMPT_READY_TIMEOUT: Duration = Duration::from_secs(45);
const CLAUDE_TUI_PROMPT_MARKER: &str = "\u{276f}";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TuiInputAction {
    Literal(String),
    PasteBuffer(String),
    Enter,
    Escape,
}

pub fn plan_prompt_submit(prompt: &str) -> Result<Vec<TuiInputAction>, String> {
    validate_prompt_text(prompt)?;
    let mut actions = if prompt.contains('\n') || prompt.contains('\r') {
        vec![TuiInputAction::PasteBuffer(prompt.to_string())]
    } else {
        split_literal_chunks(prompt, DEFAULT_LITERAL_CHUNK_CHARS)
            .into_iter()
            .map(TuiInputAction::Literal)
            .collect::<Vec<_>>()
    };
    actions.push(TuiInputAction::Enter);
    Ok(actions)
}

pub fn plan_cancel() -> Vec<TuiInputAction> {
    vec![TuiInputAction::Escape]
}

pub fn send_prompt(session_name: &str, prompt: &str) -> Result<(), String> {
    wait_for_prompt_ready(session_name, PROMPT_READY_TIMEOUT)?;
    run_actions(session_name, &plan_prompt_submit(prompt)?)
}

pub fn send_cancel(session_name: &str) -> Result<(), String> {
    run_actions(session_name, &plan_cancel())
}

fn run_actions(session_name: &str, actions: &[TuiInputAction]) -> Result<(), String> {
    for action in actions {
        let output = match action {
            TuiInputAction::Literal(text) => {
                crate::services::platform::tmux::send_literal(session_name, text)?
            }
            TuiInputAction::PasteBuffer(text) => {
                let buffer_name = format!("agentdesk-tui-input-{}", uuid::Uuid::new_v4());
                let load_output = crate::services::platform::tmux::load_buffer(&buffer_name, text)?;
                ensure_tmux_success(load_output, action)?;
                crate::services::platform::tmux::paste_buffer(session_name, &buffer_name, true)?
            }
            TuiInputAction::Enter => {
                crate::services::platform::tmux::send_keys(session_name, &["Enter"])?
            }
            TuiInputAction::Escape => {
                crate::services::platform::tmux::send_keys(session_name, &["Escape"])?
            }
        };
        ensure_tmux_success(output, action)?;
    }
    Ok(())
}

fn ensure_tmux_success(output: Output, action: &TuiInputAction) -> Result<(), String> {
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let action_name = match action {
        TuiInputAction::Literal(_) => "literal",
        TuiInputAction::PasteBuffer(_) => "paste-buffer",
        TuiInputAction::Enter => "enter",
        TuiInputAction::Escape => "escape",
    };
    if stderr.is_empty() {
        Err(format!("tmux send {action_name} failed: {}", output.status))
    } else {
        Err(format!("tmux send {action_name} failed: {stderr}"))
    }
}

fn wait_for_prompt_ready(session_name: &str, timeout: Duration) -> Result<(), String> {
    let start = Instant::now();
    let mut wait_interval = Duration::from_millis(100);
    loop {
        if let Some(pane) = crate::services::platform::tmux::capture_pane(session_name, -80)
            && pane_looks_ready_for_prompt(&pane)
        {
            return Ok(());
        }
        if !crate::services::tmux_diagnostics::tmux_session_has_live_pane(session_name) {
            return Err("claude tui session died before prompt input was ready".to_string());
        }
        if start.elapsed() >= timeout {
            return Err("timeout waiting for claude tui prompt input readiness".to_string());
        }
        std::thread::sleep(wait_interval);
        wait_interval = std::cmp::min(wait_interval * 2, Duration::from_millis(1000));
    }
}

fn pane_looks_ready_for_prompt(pane: &str) -> bool {
    pane.lines()
        .any(|line| line.trim_start().starts_with(CLAUDE_TUI_PROMPT_MARKER))
}

fn validate_prompt_text(input: &str) -> Result<(), String> {
    if input
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        return Err("prompt contains unsupported terminal control characters".to_string());
    }
    Ok(())
}

fn split_literal_chunks(input: &str, max_chars: usize) -> Vec<String> {
    if input.is_empty() {
        return Vec::new();
    }
    let max_chars = max_chars.max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_chars = 0usize;
    for ch in input.chars() {
        if current_chars >= max_chars {
            chunks.push(std::mem::take(&mut current));
            current_chars = 0;
        }
        current.push(ch);
        current_chars += 1;
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_submit_uses_literal_chunks_then_enter() {
        let actions = plan_prompt_submit("abc");

        assert_eq!(
            actions.unwrap(),
            vec![
                TuiInputAction::Literal("abc".to_string()),
                TuiInputAction::Enter
            ]
        );
    }

    #[test]
    fn empty_prompt_still_submits_enter() {
        let actions = plan_prompt_submit("");

        assert_eq!(actions.unwrap(), vec![TuiInputAction::Enter]);
    }

    #[test]
    fn split_literal_chunks_preserves_multibyte_char_boundaries() {
        let chunks = split_literal_chunks("가나다abc", 2);

        assert_eq!(chunks, vec!["가나", "다a", "bc"]);
    }

    #[test]
    fn cancel_uses_escape() {
        assert_eq!(plan_cancel(), vec![TuiInputAction::Escape]);
    }

    #[test]
    fn pane_ready_detection_matches_claude_prompt_marker() {
        let pane = "Claude Code v2.1.141\n\n\u{276f} \nstatus";

        assert!(pane_looks_ready_for_prompt(pane));
    }

    #[test]
    fn pane_ready_detection_ignores_non_prompt_status_text() {
        let pane = "Claude Code v2.1.141\nloading plugins\nbypass permissions on";

        assert!(!pane_looks_ready_for_prompt(pane));
    }

    #[test]
    fn multiline_prompt_uses_paste_buffer_before_enter() {
        let actions = plan_prompt_submit("line1\nline2").unwrap();

        assert_eq!(
            actions,
            vec![
                TuiInputAction::PasteBuffer("line1\nline2".to_string()),
                TuiInputAction::Enter
            ]
        );
    }

    #[test]
    fn prompt_rejects_terminal_control_characters() {
        let error = plan_prompt_submit("hello\u{1b}[201~").unwrap_err();

        assert_eq!(
            error,
            "prompt contains unsupported terminal control characters"
        );
    }
}
