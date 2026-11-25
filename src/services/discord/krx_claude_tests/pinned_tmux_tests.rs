//! Moved from src/services/claude/session_handoff/pinned_tmux.rs: stamps discord spawn markers.

use crate::services::claude::session_handoff::pinned_tmux::tests::Server;
use crate::services::claude::session_handoff::pinned_tmux::*;

#[tokio::test]
async fn an_agentdesk_name_in_hangul_closes_only_its_exact_pane() {
    use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;
    let root_guard = crate::config::TestRuntimeRootGuard::new();
    let server = Server {
        dir: tempfile::tempdir().unwrap(),
    };
    let logical =
        crate::services::provider::ProviderKind::Claude.build_tmux_session_name("게임-cc");
    server.start_named(&logical);
    server.start_named("i8177-bystander");
    let prepared =
        PreparedIncarnation::prepare("claude", &logical, Some(8177), Some("argv-a"), true).unwrap();
    crate::services::tmux_common::spawn_markers::stamp_spawn_markers(&logical, Some(&prepared))
        .unwrap();
    let result = end_with_for_test(server.command(), &logical, 8177, None, |_| async {
        Ok(true)
    })
    .await;
    assert_eq!(result, Ok(()));
    let names = server.run(&["-N", "list-sessions", "-F", "#{session_name}"]);
    assert_eq!(names.lines().collect::<Vec<_>>(), ["i8177-bystander"]);
    drop(root_guard);
}
