//! Fresh-session strategy and resume-token validation.
use super::*;

pub(super) fn validated_resume_session_id(
    session_id: Option<&str>,
) -> Result<Option<&str>, String> {
    let Some(session_id) = session_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if !is_valid_session_id(session_id) {
        return Err(
            "InvalidArgument: Qwen session_id must use a resumable token produced by the CLI"
                .to_string(),
        );
    }
    Ok(Some(session_id))
}
pub(super) fn should_preserve_live_reused_provider_session(
    resume_session_id: Option<&str>,
    has_live_pane: bool,
) -> bool {
    resume_session_id
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
        && has_live_pane
}
pub(crate) fn build_stream_exec_args(
    prompt: &str,
    model: Option<&str>,
    resume_strategy: &QwenResumeStrategy,
) -> Vec<String> {
    let mut args = Vec::new();

    if let Some(model) = model.map(str::trim).filter(|value| !value.is_empty()) {
        args.push("--model".to_string());
        args.push(model.to_string());
    }

    match resume_strategy {
        QwenResumeStrategy::Fresh => {}
        QwenResumeStrategy::Continue => {
            args.push("--continue".to_string());
        }
        QwenResumeStrategy::Resume(session_id) => {
            args.push("--resume".to_string());
            args.push(session_id.clone());
        }
    }

    args.push("-p".to_string());
    args.push(prompt.to_string());
    args.push("--output-format".to_string());
    args.push("stream-json".to_string());
    args.push("--include-partial-messages".to_string());
    args.push("--approval-mode".to_string());
    args.push("yolo".to_string());
    args.push("--sandbox".to_string());
    args.push("false".to_string());
    args
}
pub(crate) fn normalize_resume_strategy(
    session_id: Option<&str>,
    working_dir: &str,
) -> Result<QwenResumeStrategy, String> {
    let Some(session_id) = session_id.map(str::trim).filter(|value| !value.is_empty()) else {
        if has_prior_qwen_chat_cache(working_dir) {
            return Ok(QwenResumeStrategy::Continue);
        }
        return Ok(QwenResumeStrategy::Fresh);
    };

    if !is_valid_session_id(session_id) {
        return Err(
            "InvalidArgument: Qwen session_id must use a resumable token produced by the CLI"
                .to_string(),
        );
    }

    Ok(QwenResumeStrategy::Resume(session_id.to_string()))
}
pub(crate) fn normalize_resume_strategy_for_turn(
    session_id: Option<&str>,
    working_dir: &str,
    force_fresh_provider_session: bool,
) -> Result<QwenResumeStrategy, String> {
    if force_fresh_provider_session {
        return Ok(QwenResumeStrategy::Fresh);
    }
    normalize_resume_strategy(session_id, working_dir)
}
