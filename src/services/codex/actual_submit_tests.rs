use super::*;
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;

pub(crate) fn driver(
    context: &BindingContext,
    native_session: &str,
    prompt: &str,
    token: Arc<CancelToken>,
    sender: Sender<StreamMessage>,
) -> Result<(), String> {
    execute_streaming_local_tui_tmux(
        prompt,
        Some(native_session),
        None,
        None,
        None,
        context.provider_root.as_ref().unwrap().to_str().unwrap(),
        sender,
        Some(token),
        &context.tmux_session,
        None,
        context.channel_id,
        Some(ProviderKind::Codex),
        None,
        false,
        None,
        false,
    )
}

/// The production cold launch for an exact resume of `native_session` in `cwd`.
pub(crate) fn cold_driver(
    cwd: &std::path::Path,
    native_session: &str,
    (tmux, channel): (&str, u64),
    token: Arc<CancelToken>,
    sender: Sender<StreamMessage>,
) -> Result<(), String> {
    execute_streaming_local_tui_tmux(
        "resume prompt",
        Some(native_session),
        None,
        None,
        None,
        cwd.to_str().unwrap(),
        sender,
        Some(token),
        tmux,
        None,
        Some(channel),
        Some(ProviderKind::Codex),
        None,
        false,
        None,
        false,
    )
}

#[tokio::test]
async fn s2b1b_actual_driver_missing_episode_refuses_before_transport() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (root, _env) = crate::services::tui_prompt_dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let prepared =
        crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::prepare_pinned(
            "codex",
            crate::services::codex_tui::canary::CANARY_TMUX,
            Some(crate::services::codex_tui::canary::CANARY_CHANNEL),
            None,
            false,
            Some(root.path().to_path_buf()),
            (None, Some("verified".into())),
        )
        .unwrap();
    let context = prepared.context;
    let path =
        crate::services::tmux_common::session_temp_path(&context.tmux_session, "spawn_nonce");
    std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
    std::fs::write(path, &context.execution_nonce).unwrap();
    let fake = crate::services::provider_teardown::tests::test_support::FakeTmux::install(
        &context.tmux_session,
    );
    let _ = fake.take_calls();
    let (sender, _receiver) = std::sync::mpsc::channel();
    let result = tokio::task::spawn_blocking(move || {
        let token = Arc::new(CancelToken::new());
        token.publish_cancel("fixture stop");
        driver(&context, "native", "prompt", token, sender)
    })
    .await
    .unwrap();
    assert!(result.is_ok());
    assert!(
        fake.take_calls().is_empty(),
        "refused ingress cannot mutate or clean up its pane"
    );
}

pub(crate) fn options(native: &str, cwd: &std::path::Path) -> CodexLaunchOptions {
    CodexLaunchOptions::new("first")
        .with_resume_session_id(Some(native))
        .with_reasoning_effort(codex_reasoning_effort_from_env().as_deref())
        .with_cwd(cwd.to_str())
}
