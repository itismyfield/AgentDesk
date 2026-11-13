use super::*;

#[test]
fn owned_tmux_busy_front_enters_common_operation_once() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig(
        "i8114-tmux-common-operation",
        8_114_711,
        "normal",
        view("", false),
        BUSY,
    );
    let front = rig.granted_front();
    let transcript = rig.transcript();
    let result = rig.handoff(
        front.clone(),
        ProviderKind::Claude,
        target::Phase::Busy,
        &transcript,
    );
    assert_eq!(result, Ok(()));
    assert_eq!(rig.events(), ["paste", "Enter"]);
    rig.delivered();
    let observed = crate::services::claude_tui::host_input::busy::observed;
    assert_eq!(
        observed(&rig.nonce),
        1,
        "the real dispatcher must enter the same operation as the Herdr dispatcher"
    );
    assert_eq!(
        rig.handoff(
            front,
            ProviderKind::Claude,
            target::Phase::Busy,
            &transcript,
        ),
        Err("NotSent(\"front_not_granted\")".into())
    );
    assert_eq!(rig.events(), ["paste", "Enter"]);
    assert_eq!(
        observed(&rig.nonce),
        1,
        "a spent front never re-enters input"
    );
}
