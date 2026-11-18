use super::*;
use crate::services::herdr_launch::clear_epoch;

#[test]
fn clear_before_bound_codex_execute_sends_no_followup_input_pg() {
    let fx = Fixture::admitted("warm-clear-execute");
    let launcher = Arc::new(Launcher::default());
    let (nonce, path) = launch(&fx, &fx.ports(&launcher));
    assert_eq!(fx.row(), Some(HostedState::Bound));
    let warm = binding(&fx).expect("the cold turn really published and attached its Source");
    assert_eq!(Path::new(&warm.output_path), path.as_path());
    assert!(path.is_file());
    assert_eq!(fx.rig.sends(), prompt_sends());
    fx.rig.answer("pane.read", screen(READY));
    let _clear =
        clear_epoch::begin_clear(&fx.owner.provider, &fx.owner.discord_token_hash, CHANNEL);
    let (result, _) = fx.turn(&fx.record(), &fx.ports(&launcher), || {
        // Reply only if the mutant really writes; normal early refusal ends wait_for at once.
        reply(&fx, &path, 4, "after-clear", "a forbidden follow-up");
    });
    assert_eq!(
        fx.rig.sends().len(),
        2,
        "only the cold prompt exists; clear permits no follow-up paste or Enter"
    );
    assert!(result.is_err());
    assert_eq!(fx.row(), Some(HostedState::Bound));
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
    assert!(!hold_of(&nonce).exists());
}
