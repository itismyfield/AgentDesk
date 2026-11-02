use super::super::*;
use super::posted_for_tests;
use crate::services::provider_teardown::tests::test_support::{FakeTmux, cleared};

const CHANNEL: u64 = 9_000_000_006_483;

fn launch(name: &str, cwd: &std::path::Path, session: &str) -> String {
    let (tx, _rx) = std::sync::mpsc::channel();
    let (clearance, endpoint) = (cleared(name), "http://127.0.0.1:9".to_string());
    execute_streaming_local_tui_tmux(
        "hello",
        Some(session),
        cwd.to_str().unwrap(),
        tx,
        None,
        name,
        Some(&clearance),
        Some(CHANNEL),
        None,
        Default::default(),
        None,
        endpoint,
    )
    .expect_err("the launch never completes")
}

// A tmux launch whose requested session has no transcript tells the channel once, however often
// the turn is launched again; a session whose transcript is there tells nothing.
#[test]
fn a_tmux_resume_without_its_transcript_is_announced_once_and_one_with_it_never() {
    const NAME: &str = "adk-i6483-fresh-notice";
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = FakeTmux::install(NAME);
    tmux.mark_missing();
    let (home, cwd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let _home = set("CLAUDE_CONFIG_DIR", home.path());
    let lost = uuid::Uuid::new_v4().to_string();
    let errors = [
        launch(NAME, cwd.path(), &lost),
        launch(NAME, cwd.path(), &lost),
    ];
    assert_eq!(posted_for_tests(&lost), 1, "{errors:?}");

    let kept = uuid::Uuid::new_v4().to_string();
    let transcript = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        cwd.path(),
        &kept,
        None,
    )
    .unwrap();
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, "{}\n").unwrap();
    // The kept launch ends where the lost one did, past the transcript check.
    assert_eq!(launch(NAME, cwd.path(), &kept), errors[0]);
    assert_eq!(posted_for_tests(&kept), 0);
    crate::services::tmux_common::cleanup_session_temp_files(NAME);
}

// A live session whose transcript the cwd lookup misses but its runtime binding still names is
// continued warm, and no notice says it started fresh.
#[test]
fn a_live_session_its_binding_recovers_is_never_announced() {
    const NAME: &str = "adk-i6483-fresh-notice-live";
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _tmux = FakeTmux::install(NAME);
    let (home, cwd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let set = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock;
    let _home = set("CLAUDE_CONFIG_DIR", home.path());
    let live = uuid::Uuid::new_v4().to_string();
    let elsewhere = tempfile::tempdir().unwrap();
    let transcript = elsewhere.path().join(format!("{live}.jsonl"));
    std::fs::write(&transcript, "{}\n").unwrap();
    let binding = crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        output_path: transcript.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(live.clone()),
        last_offset: 0,
        relay_last_offset: None,
    };
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(NAME, binding);
    // A waiting owner refuses the warm write, so the turn ends on the recovered warm branch.
    let refuse = Box::new(|_, _, _, _, _| Err("unused".to_string()));
    let _owner = crate::services::claude_tui::owner_input::install_launch(refuse);
    let error = launch(NAME, cwd.path(), &live);
    assert!(
        error.contains("a live session takes no first prompt"),
        "{error}"
    );
    assert_eq!(posted_for_tests(&live), 0);
    crate::services::tmux_common::cleanup_session_temp_files(NAME);
}
