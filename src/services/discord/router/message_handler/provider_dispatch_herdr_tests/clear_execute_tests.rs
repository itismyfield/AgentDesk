use super::*;
use crate::services::herdr_launch::clear_epoch;

#[test]
fn clear_before_bound_claude_execute_sends_no_input_pg() {
    let fx = Fixture::new("warm-clear-execute", None);
    let record = fx.store_bound();
    let launcher = Arc::new(Launcher::default());
    let binding =
        crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(fx.logical())
            .expect("the existing Bound fixture registers its warm Source");
    let session = binding.session_id.clone().expect("a warm native session");
    let path = PathBuf::from(&binding.output_path);
    assert!(path.is_file(), "the warm transcript exists before clear");
    assert!(fx.rig.sends().is_empty());
    let _clear =
        clear_epoch::begin_clear(&fx.owner.provider, &fx.owner.discord_token_hash, CHANNEL);
    let (result, _) = fx.turn(&record, &fx.ports(&launcher), || {
        // If execute loses registration, answer its real write so the mutant does not hang.
        if wait_for(&fx.finished, "the warm prompt", || {
            fx.rig.sends().len() == 2
        }) {
            let user = json!({"type":"user", "sessionId":session,
                "timestamp":chrono::Utc::now().to_rfc3339(),
                "message":{"role":"user","content":"질문"}});
            let answer = json!({"type":"assistant", "sessionId":session,
                "message":{"role":"assistant","stop_reason":"end_turn",
                "content":[{"type":"text","text":"답"}]}});
            let done = json!({"type":"system","subtype":"turn_duration","sessionId":session});
            append(&path, &[user, answer, done]);
        }
    });
    assert_eq!(
        fx.rig.sends().len(),
        0,
        "clear before actual execute permits no paste or Enter"
    );
    assert!(result.is_err());
    assert_eq!(fx.row(), Some(HostedState::Bound));
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 0);
}
