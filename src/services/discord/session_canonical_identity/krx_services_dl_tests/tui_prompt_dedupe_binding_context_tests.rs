//! Moved from src/services/tui_prompt_dedupe/binding_context.rs: these cases stamp discord spawn markers.

use crate::config::TestEnvVarGuard as Guard;
use crate::services::discord::stamp_spawn_markers;
use crate::services::platform::tmux::SessionPresence::Missing;
use crate::services::tmux_common as tc;
use crate::services::tui_prompt_dedupe::binding_context::tests::{
    child_context, fake_tmux, fixture, prepared,
};
use crate::services::tui_prompt_dedupe::binding_context::*;
use chrono::Utc;
use std::fs;
use std::path::Path;

#[test]
fn binding_context_t3_env_is_the_only_launch_source() {
    let (_root, _env) = fixture();
    let _value = Guard::capture_after_shared_test_env_lock("AGENTDESK_BINDING_CONTEXT");
    unsafe { std::env::remove_var("AGENTDESK_BINDING_CONTEXT") };
    let p = prepared();
    crate::services::discord::stamp_spawn_markers(&p.context.tmux_session, None).unwrap();
    assert!(matches!(
        observe_spawn_nonce_marker(&p.context.tmux_session),
        SpawnNonceMarker::Known(_)
    ));
    assert_eq!(
        HookBindingEnvelope::capture("claude").context,
        CapturedContext::Absent(AbsentReason::EnvUnset)
    );
    crate::services::discord::stamp_spawn_markers(&p.context.tmux_session, Some(&p)).unwrap();
    assert_eq!(
        HookBindingEnvelope::capture("claude").context,
        CapturedContext::Absent(AbsentReason::EnvUnset)
    );
    child_context(&p.env_lines(), Some(&p.path));
    let p = PreparedIncarnation {
        path: p.path.with_file_name("quote' space.json"),
        ..p
    };
    child_context(&p.env_lines(), Some(&p.path));
}

#[test]
fn binding_context_t16_sweep_before_publication_aborts_launch() {
    let (root, _env) = fixture();
    let _tmux = fake_tmux(root.path());
    let p = prepared();
    let tmux = &p.context.tmux_session;
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let launch = scope.spawn(|| {
            barrier.wait();
            barrier.wait();
            p.finish_spawn(stamp_spawn_markers(tmux, Some(&p)))
        });
        barrier.wait();
        sweep("claude", Utc::now() + chrono::Duration::days(8), 32, |_| {
            Missing
        });
        barrier.wait();
        let error = launch.join().unwrap().unwrap_err();
        assert!(error.contains("ContextNotPublishable"));
    });
    assert_eq!(observe_spawn_nonce_marker(tmux), SpawnNonceMarker::Absent);
    assert!(!Path::new(&tc::session_temp_path(tmux, "generation")).exists());
    let calls = fs::read_to_string(root.path().join("tmux.calls")).unwrap();
    assert!(calls.contains(&format!("kill-session -t ={tmux}:")));
}
