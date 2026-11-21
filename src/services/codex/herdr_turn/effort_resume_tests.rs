//! The launch an effort restart owes resumes exactly the thread it ended, once.
use super::super::restart_intent::EffortResume;
use super::greeting::{ended_source, launch_owing, retired};
use crate::db::dispatched_sessions::hosted_execution::HostedRecord;

fn owed(native: &str, retired_nonce: &str, required: bool) -> Option<EffortResume> {
    Some(EffortResume {
        operation: "effort-operation".into(),
        native_session: native.into(),
        retired_nonce: retired_nonce.into(),
        required,
    })
}

fn nonce(row: &HostedRecord) -> String {
    match row {
        HostedRecord::Known(record) => record.execution_nonce.clone(),
        _ => unreachable!("fixture rows are known"),
    }
}

#[tokio::test]
async fn effort_restart_launch_resumes_the_thread_it_ended() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (source, _) = ended_source(0);
    let row = retired();
    for required in [false, true] {
        let launch = launch_owing(
            &row,
            false,
            owed(&source.session_id, &nonce(&row), required),
        )
        .expect("the ended thread resumes");
        assert!(launch.resume, "an owed effort resume asks for a resume");
        assert_eq!(
            launch.expected_native_session_id.as_deref(),
            Some(source.session_id.as_str())
        );
        let start = launch.resume_start.expect("a captured boundary");
        assert_eq!(start.operation, "effort-operation");
        assert_eq!((start.dev, start.ino), (source.dev, source.ino));
    }
}

#[tokio::test]
async fn effort_restart_launch_refuses_another_thread_before_any_pane() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    ended_source(0);
    let row = retired();
    let other = uuid::Uuid::new_v4().to_string();
    for required in [false, true] {
        let refused = launch_owing(&row, false, owed(&other, &nonce(&row), required));
        assert!(
            refused.is_err(),
            "a live cursor naming another thread neither resumes it nor falls back to fresh"
        );
    }
}

#[tokio::test]
async fn effort_resume_is_spent_once_another_execution_retired() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (source, _) = ended_source(0);
    let row = retired();
    let stale = launch_owing(
        &row,
        false,
        owed(&source.session_id, "another-nonce", false),
    )
    .expect("a spent idle resume launches as before");
    assert!(
        !stale.resume && stale.expected_native_session_id.is_none(),
        "an old restart's resume never takes over a later execution's retired row"
    );
    let waiting = launch_owing(&row, false, owed(&source.session_id, "another-nonce", true));
    assert!(
        waiting.is_err(),
        "a waiting continuation never runs on a thread its restart did not end"
    );
}

#[tokio::test]
async fn effort_resume_without_its_boundary_never_launches_fresh() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let row = retired();
    let bare = launch_owing(&row, false, owed("any-thread", &nonce(&row), false));
    assert!(bare.is_err(), "no O cursor proves the ended thread");
}

#[tokio::test]
async fn a_herdr_codex_launch_takes_the_channel_effort_over_the_environment() {
    use crate::services::codex::tui_session_launch::launch_policy::codex_tui_option_args;
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _env = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_CODEX_REASONING_EFFORT",
        std::ffi::OsStr::new("low"),
    );
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://unused@127.0.0.1/unused")
        .unwrap();
    let turn = |effort| super::super::CodexHerdrTurn {
        pool: &pool,
        owner: super::greeting::owner(),
        channel_id: 8_128_002,
        endpoint: crate::services::herdr_launch::HerdrLaunchEndpoint {
            execution_node: "test".into(),
            config_key: "test".into(),
            socket_addr: "/test/herdr.sock".into(),
            herdr_session: "test".into(),
        },
        row: None,
        prompt: "continue",
        working_dir: "/tmp",
        system_prompt: None,
        allowed_tools: &[],
        model: None,
        effort,
        fast_mode: None,
        goals: None,
        compact_token_limit: None,
        cancel: None,
        bootstrap: false,
    };
    let chosen = super::super::launch_options(&turn(Some("ultra")));
    assert_eq!(chosen.reasoning_effort.as_deref(), Some("ultra"));
    let args = codex_tui_option_args(&chosen);
    assert!(
        args.windows(2)
            .any(|pair| pair[0] == "-c" && pair[1] == "model_reasoning_effort=\"ultra\""),
        "the pane argv carries the saved level: {args:?}"
    );
    let unset = super::super::launch_options(&turn(None));
    assert_eq!(
        unset.reasoning_effort.as_deref(),
        Some("low"),
        "no saved level keeps the default"
    );
    assert_ne!(
        super::super::kept_options("n", &chosen),
        super::super::kept_options("n", &unset),
        "a changed level is a changed launch, so the old pane never takes the new options"
    );
}
