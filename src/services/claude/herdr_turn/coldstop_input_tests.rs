use super::*;
use crate::services::provider::cancel_token_claude_interrupt::HERDR_SETTLEMENT_OVERRIDE;
use crate::services::provider::herdr_before_start::InputPhase;
#[test]
fn coldstop_claude_closed_never_begins_input_or_mutation() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let owner = HostedOwner {
        provider: "claude".into(),
        discord_token_hash: "hash".into(),
        channel_id: "911".into(),
        logical_key: "AgentDesk-claude-coldinput".into(),
        owner_node: "node".into(),
        runtime_root: "root".into(),
    };
    for kind in ["fresh", "pending", "bound"] {
        let token = CancelToken::new();
        let state = token.prepare_herdr_interrupt(ProviderKind::Claude, &owner);
        state
            .user_stop
            .store(true, std::sync::atomic::Ordering::Release);
        let held = hold(kind).unwrap();
        let calls = std::cell::Cell::new(0);
        let run = observed_input(
            Some(&token),
            || {
                calls.set(calls.get() + 1);
                HerdrTurnStart::at_end_of(kind, Path::new("missing"), None)
            },
            || {
                calls.set(calls.get() + 1);
                InputRun::Applied
            },
        )
        .unwrap();
        assert!(matches!(run, HerdrInputRun::Closed));
        assert_eq!(calls.get(), 0);
        before_start::release_claude_hold(&owner.logical_key, &held);
        assert!(not_held(kind).is_ok());
    }
    for (run, untouched) in [
        (
            InputRun::Refused(crate::services::claude_tui::host_input::InputRefusal::Unknown),
            true,
        ),
        (InputRun::Cancelled { confirmed: 0 }, true),
        (
            InputRun::Indeterminate {
                confirmed: 0,
                cause: crate::services::claude_tui::host_input::StopCause::Send(
                    "unclear paste".into(),
                ),
            },
            false,
        ),
    ] {
        let token = CancelToken::new();
        let state = token.prepare_herdr_interrupt(ProviderKind::Claude, &owner);
        observed_input(
            Some(&token),
            || HerdrTurnStart::at_end_of("nonce", Path::new("missing"), None),
            || run,
        )
        .unwrap();
        assert_eq!(
            state.submission.lock().unwrap().phase,
            if untouched {
                InputPhase::FinishedUntouched
            } else {
                InputPhase::MayHaveWritten
            }
        );
    }
    HERDR_SETTLEMENT_OVERRIDE.set(false);
    let token = CancelToken::new();
    let state = token.prepare_herdr_interrupt(ProviderKind::Claude, &owner);
    state
        .user_stop
        .store(true, std::sync::atomic::Ordering::Release);
    assert!(matches!(
        observed_input(
            Some(&token),
            || panic!("off does not record start"),
            || InputRun::Applied
        )
        .unwrap(),
        HerdrInputRun::Ran(InputRun::Applied)
    ));
    HERDR_SETTLEMENT_OVERRIDE.set(true);
}
