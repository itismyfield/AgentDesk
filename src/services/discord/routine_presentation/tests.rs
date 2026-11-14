use super::*;

#[test]
fn i8117_policy_and_exact_terminal_negative_matrix() {
    for (provider, source, process, c1) in [
        (ProviderKind::Claude, Some("routine"), true, true),
        (ProviderKind::Claude, Some("routine"), false, false),
        (ProviderKind::Claude, Some("api"), true, false),
        (ProviderKind::Claude, Some("scheduled_message"), true, false),
        (ProviderKind::Claude, None, true, false),
        (ProviderKind::Codex, Some("routine"), true, false),
        (ProviderKind::Gemini, Some("routine"), true, false),
    ] {
        let mut policy = RoutinePresentation::at_start(&provider, source, process);
        assert_eq!(policy.holds_body(), c1);
        policy.observe_terminal(" NO_reply \n");
        assert_eq!(
            policy.terminal_presentation(),
            if c1 {
                TerminalPresentation::NoVisibleResult
            } else {
                TerminalPresentation::Full
            }
        );
    }
    for result in [
        "NO_REPLY を引用",
        "normal NO_REPLY",
        "",
        "normal\nNO_REPLY",
        "[tool] NO_REPLY",
    ] {
        let mut policy =
            RoutinePresentation::at_start(&ProviderKind::Claude, Some("routine"), true);
        assert_eq!(policy.terminal_presentation(), TerminalPresentation::Full);
        policy.observe_terminal(result);
        assert_eq!(
            policy.terminal_presentation(),
            TerminalPresentation::Full,
            "{result:?}"
        );
    }
    let mut duplicate = RoutinePresentation::at_start(&ProviderKind::Claude, Some("routine"), true);
    duplicate.observe_terminal("NO_REPLY");
    duplicate.observe_terminal("NO_REPLY");
    assert_eq!(
        duplicate.terminal_presentation(),
        TerminalPresentation::Full
    );
}
