#![cfg(unix)]
use super::*;
use crate::services::claude_tui::input::submission::tests::{frame_renderer, screen};
use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;

#[test]
fn cold_claude_submit_returns_before_missing_source_and_keeps_payload_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    let name = "cold-submit-source-delayed";
    let prompt = "reply + attachments\nvoice _ literal";
    frame_renderer(&f, name, prompt, &screen(""), &screen(prompt), &screen(""));
    let transcript = f.root.path().join("not-created.jsonl");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let nonce = "00112233445566778899aabbccddee02";
    let owner_root = crate::services::tmux_common::current_tmux_owner_marker();
    let context:crate::services::tui_prompt_dedupe::binding_context::BindingContext=serde_json::from_value(serde_json::json!({"schema":1,"provider":"claude","created_at":chrono::Utc::now(),"execution_nonce":nonce,"tmux_session":name,"channel_id":727395,"owner_runtime_root":owner_root,"host":"scope-test","expected_native_session_id":null,"launch_mode":"fresh","provider_root":null})).unwrap();
    let _prepared =
        crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(context)
            .unwrap();
    let marker = crate::services::tmux_common::session_temp_path(name, "spawn_nonce");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, nonce).unwrap();
    let _entered = runtime.enter();
    let registry = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
    let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(727395));
    runtime.block_on(mailbox.v2_cold_fixture(f.root.path().join("cold-owner.json"),serde_json::json!({"nonce":"claude-cold-scope","provider":"claude","target":{"host":"tmux","owner":name,"root":owner_root,"execution_nonce":nonce},"source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}}),prompt.into(),727395)).unwrap();
    let transcript_for_submit = transcript.display().to_string();
    let (_, outcome) = runtime
        .block_on(
            mailbox.v2_cold_bootstrap(1, std::sync::Arc::new(|| 10000), move |frame| {
                assert!(
                    crate::services::tui_input::human_frame::history::v2::scoped_source().is_some(),
                    "actual Claude submit requires exact source scope"
                );
                assert!(
                    crate::services::claude_tui::owner_input::active(),
                    "actual Claude submit requires owner scope"
                );
                submit(&transcript_for_submit, 0, None, name, &frame).map_err(|e| format!("{e:?}"))
            }),
        )
        .unwrap();
    let owner_snapshot = runtime
        .block_on(mailbox.v2_request(|s| s.as_ref().unwrap().json()))
        .unwrap();
    assert_eq!(
        owner_snapshot["head_injection"]["payload_window"]["kind"], "started",
        "actual submit must commit payload in owner scope"
    );
    let submitted = outcome
        .unwrap_or_else(|error| panic!("cold physical submit must finish without Source: {error}"));
    assert_eq!(submitted.start_offset, 0);
    assert!(
        !transcript.exists(),
        "cold submit must not wait for a transcript"
    );
    let received = std::fs::read_to_string(f.root.path().join(name).join("frame")).unwrap();
    assert_eq!(received, prompt, "cold Claude preserves prepared bytes");
    let payloads = std::fs::read(f.root.path().join(name).join("payloads")).unwrap();
    let entered = std::fs::read(f.root.path().join(name).join("enters")).unwrap();
    assert_eq!(payloads.len(), 1, "cold Claude has one content effect");
    assert_eq!(entered.len(), 1, "cold Claude enters at most once");
}

#[test]
fn fresh_submit_held_never_retries_or_writes_payload() {
    use crate::services::claude_tui::{
        composer_lock::{DraftGuard, guard_draft},
        host_input::{SpyGuard, SpyState},
        input,
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    let name = "fresh-branch-hold";
    frame_renderer(&f, name, "own", &screen(""), &screen("own"), &screen(""));
    guard_draft(name, DraftGuard::RecoveryRequired);
    let spy = SpyGuard::install(SpyState {
        captures: [
            Some(screen("")),
            Some(screen("")),
            Some(screen("")),
            Some(screen("")),
            Some("unknown overlay".into()),
        ]
        .into_iter()
        .collect(),
        draft_capture_at: Some((1, Some("unknown overlay".into()))),
        ..Default::default()
    });
    let mut attempts = 0;
    let result = submit_using(
        "/not-created",
        0,
        || {
            attempts += 1;
            input::send_prompt_with_readiness(
                name,
                "own",
                input::PromptReadinessKind::FreshTurn,
                None,
            )
        },
        |_| panic!("Held must not back off"),
        name,
    );
    assert!(
        matches!(result, Err(SubmissionError::Held(_))),
        "fresh Held must remain typed"
    );
    assert_eq!(attempts, 1, "Held must not retry");
    assert!(
        !spy.calls()
            .iter()
            .any(|s| s.starts_with("literal:") || s.starts_with("paste:")),
        "Held has zero payload"
    );
    drop(spy);
}

#[test]
fn fresh_readiness_exhaustion_keeps_error_and_exact_attempt_budget() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let detail = "timeout waiting for claude tui fresh prompt input readiness after 120s";
    let mut attempts = 0;
    let mut sleeps = 0;
    let result = submit_using(
        "/not-created",
        0,
        || {
            attempts += 1;
            Err(SubmissionError::Legacy(detail.into()))
        },
        |_| {
            sleeps += 1;
        },
        "fresh-timeout",
    );
    let Err(SubmissionError::Legacy(error)) = result else {
        panic!("readiness exhaustion must stay Legacy");
    };
    assert_eq!(
        attempts, CLAUDE_TUI_FRESH_PROMPT_MAX_READY_ATTEMPTS,
        "readiness uses MAX_READY_ATTEMPTS"
    );
    assert_eq!(sleeps, attempts - 1);
    assert_eq!(
        error,
        format!("{detail}; fresh prompt readiness attempts exhausted ({attempts} attempts)"),
        "exhaustion must preserve readiness context"
    );
    assert_eq!(
        exhausted(None),
        format!("claude tui fresh prompt readiness attempts exhausted ({attempts} attempts)"),
        "fallback exhaustion message must remain explicit"
    );
}

/// An owned first prompt runs the launch's own submit on the frame the owner hands back and keeps
/// that submit's ticket for the observer that reads next.
#[test]
fn owned_first_prompt_keeps_its_ticket_for_the_observer() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let f = Fixture::after_shared_test_env_lock();
    let name = "cold-owned-ticket";
    let prompt = "owned first prompt";
    frame_renderer(&f, name, prompt, &screen(""), &screen(prompt), &screen(""));
    let nonce = "00112233445566778899aabbccddee03";
    let owner_root = crate::services::tmux_common::current_tmux_owner_marker();
    let context: crate::services::tui_prompt_dedupe::binding_context::BindingContext =
        serde_json::from_value(serde_json::json!({"schema":1,"provider":"claude",
            "created_at":chrono::Utc::now(),"execution_nonce":nonce,"tmux_session":name,
            "channel_id":727396,"owner_runtime_root":owner_root,"host":"owned-test",
            "expected_native_session_id":null,"launch_mode":"fresh","provider_root":null}))
        .unwrap();
    let _prepared =
        crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(context)
            .unwrap();
    let marker = crate::services::tmux_common::session_temp_path(name, "spawn_nonce");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, nonce).unwrap();
    let transcript = f.root.path().join("owned.jsonl");
    let hook: crate::services::claude_tui::owner_input::LaunchHook =
        Box::new(move |_lineage, target, source, _writer, submit| {
            assert_eq!(target["execution_nonce"], nonce);
            assert_eq!(source["expected_session_id"], "owned-sid");
            submit(prompt.into()).map_err(|error| format!("{error:?}"))
        });
    let submitted = submit_owned(
        hook,
        transcript.to_str().unwrap(),
        0,
        None,
        name,
        "owned-sid",
    )
    .unwrap_or_else(|error| panic!("the owner's submit keeps its ticket: {error:?}"));
    assert_eq!(submitted.start_offset, 0);
    let pane = f.root.path().join(name);
    assert_eq!(std::fs::read(pane.join("payloads")).unwrap().len(), 1);
    assert_eq!(std::fs::read(pane.join("enters")).unwrap().len(), 1);
}
