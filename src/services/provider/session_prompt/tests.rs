use super::*;

fn fresh_token() -> CancelToken {
    let token = CancelToken::new();
    arm(&token, None, &ProviderKind::Claude, 1, "turn", 2);
    token
}

#[test]
fn system_prompt_capture_joins_success_and_native_id_in_either_order_once() {
    for observe_first in [true, false] {
        let token = fresh_token();
        let scope = Scope::enter(&token);
        record(Some("actual launch text"), false);
        let capture = token.prompt_capture.get().unwrap();
        assert!(capture.take_ready().is_none());
        if observe_first {
            observe(&token, "native-session");
        } else {
            scope.success();
        }
        assert!(capture.take_ready().is_none());
        if observe_first {
            scope.success();
        } else {
            observe(&token, "native-session");
        }
        let saved = capture.take_ready().unwrap();
        assert_eq!(saved.system_prompt, "actual launch text");
        assert_eq!(saved.provider_session_id, "native-session");
        assert_eq!(saved.anchor_count, 2);
        assert!(capture.take_ready().is_none());
    }
}

#[test]
fn system_prompt_resume_failure_and_unarmed_turns_store_nothing() {
    for resume in [true, false] {
        let token = fresh_token();
        let scope = Scope::enter(&token);
        record(Some("final"), resume);
        observe(&token, "native-session");
        if resume {
            scope.success();
        }
        assert!(token.prompt_capture.get().unwrap().take_ready().is_none());
    }
    let token = CancelToken::new();
    let scope = Scope::enter(&token);
    record(Some("headless"), false);
    observe(&token, "session");
    scope.success();
    assert!(token.prompt_capture.get().is_none());
}

#[test]
fn system_prompt_records_real_codex_config_and_claude_argv() {
    let token = fresh_token();
    let scope = Scope::enter(&token);
    let text = crate::services::codex::compose_codex_developer_instructions(
        Some("  actual\nanchors  "),
        None,
    )
    .unwrap();
    let options = crate::services::codex::CodexLaunchOptions::new("user")
        .with_developer_instructions(Some(&text));
    let args = crate::services::codex::build_codex_tui_args(&options);
    let final_arg = args
        .iter()
        .find_map(|a| a.strip_prefix("developer_instructions="))
        .unwrap();
    let decoded: String = serde_json::from_str(final_arg).unwrap();
    scope.success();
    observe(&token, "codex");
    assert_eq!(
        token
            .prompt_capture
            .get()
            .unwrap()
            .take_ready()
            .unwrap()
            .system_prompt,
        decoded
    );
    drop(scope);
    let token = fresh_token();
    let scope = Scope::enter(&token);
    claude_args(&[
        "--append-system-prompt".into(),
        "verbatim\n```\nbody".into(),
    ]);
    scope.success();
    observe(&token, "claude");
    assert_eq!(
        token
            .prompt_capture
            .get()
            .unwrap()
            .take_ready()
            .unwrap()
            .system_prompt,
        "verbatim\n```\nbody"
    );
    drop(scope);
    let omitted = fresh_token();
    let scope = Scope::enter(&omitted);
    record(None, false);
    scope.success();
    observe(&omitted, "fallback-fresh");
    let saved = omitted.prompt_capture.get().unwrap().take_ready().unwrap();
    assert!(saved.system_prompt.is_empty());
    assert_eq!(saved.anchor_count, 0);
}

#[test]
fn system_prompt_capture_scope_unwinds_and_final_resume_clears_fresh_candidate() {
    let token = fresh_token();
    let _scope = Scope::enter(&token);
    record(Some("outer"), false);
    let other = fresh_token();
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _inner = Scope::enter(&other);
        record(Some("inner"), false);
        panic!("injected provider failure");
    }));
    record(Some("recovered resume"), true);
    observe(&token, "same-session");
    _scope.success();
    assert!(token.prompt_capture.get().unwrap().take_ready().is_none());
    assert!(
        !other
            .prompt_capture
            .get()
            .unwrap()
            .state
            .lock()
            .unwrap()
            .success
    );
}
