//! A headless turn into a thread whose stored Claude session nothing verified starts fresh.
use super::*;
use crate::services::discord::router::message_handler::unverified_thread_resume::tests::thread_child;
use crate::services::platform::tmux::PaneLiveness;
use crate::services::tmux_diagnostics::PaneLivenessOverrideGuard as Pane;

#[tokio::test]
async fn an_api_turn_into_an_unverified_thread_starts_fresh_pg() {
    if !api_child(&format!(
        "{TEST}unverified_thread_tests::an_api_turn_into_an_unverified_thread_starts_fresh_pg"
    )) {
        return;
    }
    let fixture = fixture().await;
    fixture._tmux.serve(Tmux::Missing);
    let _policy = thread_child(THREAD.get(), PRIMARY.get());
    let pane = ProviderKind::Claude.build_tmux_session_name("thread-agent");
    let _gone = Pane::set(&pane, PaneLiveness::DeadOrAbsent);
    let started = start_reserved_headless_agent_turn_with_owner_channel(
        &fixture.registry,
        THREAD,
        THREAD,
        ProviderKind::Claude,
        "post the result to the main channel".to_string(),
        Some("api".into()),
        None,
        Some("thread-agent".to_string()),
        None,
        reserve_headless_agent_turn(THREAD),
    )
    .await;
    assert!(started.is_ok(), "{:?}", started.err());
    let row = load_inflight_state_read_only(&ProviderKind::Claude, THREAD.get()).unwrap();
    let cached = fixture.shared.core.lock().await.sessions[&THREAD]
        .session_id
        .clone();
    assert_eq!((row.session_id, cached), (None, None));
    turn_ended(&fixture, THREAD).await;
}
