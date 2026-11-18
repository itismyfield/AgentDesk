//! An explicit routine model starts Codex before the owner can submit its first prompt.
use super::*;
use crate::services::claude_tui::owner_input::{self, LaunchHook, Lineage};
use crate::services::tui_input::routine_model as proof;
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;

pub(super) fn active() -> bool {
    proof::current().is_some()
}

pub(super) fn launch_args(options: &CodexLaunchOptions) -> Vec<String> {
    let mut args = build_codex_tui_args(options);
    if active() {
        // Keep the option separator so resume and injected hook options use the same parser.
        let _ = args.pop();
    }
    args
}

pub(super) fn start_error(error: String) -> String {
    if active() && !error.starts_with("NotSent(") {
        return format!("NotSent({error:?})");
    }
    error
}

pub(super) fn spawn(
    prepared: &PreparedIncarnation,
    working_dir: &str,
    command: &str,
) -> Result<std::process::Output, String> {
    let context = &prepared.context;
    let session = &context.tmux_session;
    let target = serde_json::json!({"host":"tmux","owner":session,
        "execution_nonce":context.execution_nonce});
    proof::before_payload(&target)?;
    crate::services::tui_input::writer_lease::admits("tmux", session).map_err(start_error)?;
    crate::services::platform::tmux::create_session_with_identity(
        session,
        Some(working_dir),
        command,
        &prepared.tmux_identity().map_err(start_error)?,
    )
    .map_err(start_error)
}

fn await_composer(
    session: &str,
    cancel: Option<&std::sync::Arc<CancelToken>>,
) -> Result<(), String> {
    use crate::services::codex_tui::input::{
        PromptReadinessKind, wait_until_codex_tui_input_ready,
    };
    wire_cancel_token_to_tmux_session(cancel, session);
    wait_until_codex_tui_input_ready(session, PromptReadinessKind::FreshTurn, cancel)
        .map_err(start_error)
}

pub(super) fn submit(
    hook: LaunchHook,
    context: &BindingContext,
    tail: &crate::services::codex_tui::tail_launch::ValidatedTail,
    cancel: Option<&std::sync::Arc<CancelToken>>,
    selection: &crate::services::codex_tui::session::CodexTuiSessionSelection,
    prompt: &str,
) -> Result<(), String> {
    let session = &context.tmux_session;
    await_composer(session, cancel)?;
    crate::services::codex_tui::tail_launch::record_resume_proof(context, tail)
        .map_err(start_error)?;
    let target = owner_input::created_tmux_target("codex", session).map_err(start_error)?;
    if target["execution_nonce"] != context.execution_nonce {
        return Err("NotSent(\"routine_model_execution_changed\")".into());
    }
    let channel = context
        .channel_id
        .ok_or("NotSent(\"routine_model_channel_missing\")")?;
    let source = serde_json::json!({"kind":"pre_source",
        "expected_session_id":selection.resume_session_id(),
        "expected_transcript":selection.rollout_path,"binding_watermark":null});
    proof::spawned("codex", session, &context.execution_nonce).map_err(start_error)?;
    let submit =
        crate::services::discord::created_codex_prompt(session, channel, prompt, cancel.cloned())
            .map_err(start_error)?;
    hook(Lineage::Created, target, source, None, submit)
}
