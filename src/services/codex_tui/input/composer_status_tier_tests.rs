use super::super::*;

// Post-turn captures whose status row names the service tier `Standard` instead of `Fast off`.
const DELETES_TAIL: &str = include_str!(
    "../../../../tests/fixtures/tui_input/codex-status-standard-tier-deletes-tail.ansi"
);
const WORKED_TAIL: &str = include_str!(
    "../../../../tests/fixtures/tui_input/codex-status-standard-tier-worked-tail.ansi"
);
const COMPOSER: &str = "\x1b[1m›\x1b[0m \x1b[2mAsk Codex to do anything\x1b[0m";

#[test]
fn standard_tier_status_row_under_dim_composer_is_input_ready() {
    for pane in [DELETES_TAIL, WORKED_TAIL] {
        let (ready, draft, _) = prompt_readiness_from_ansi_pane(pane);
        assert!(ready && !draft, "ready={ready} draft={draft}\n{pane}");
        assert!(submission_readiness_from_ansi_pane(pane).0, "{pane}");
    }
}

#[test]
fn standard_tier_status_row_without_idle_composer_is_not_input_ready() {
    for pane in [DELETES_TAIL, WORKED_TAIL] {
        for (case, blocked) in [
            (
                "turn running",
                pane.replace(
                    COMPOSER,
                    &format!("• Working (2s • esc to interrupt)\n{COMPOSER}"),
                ),
            ),
            (
                "approval modal",
                pane.replace(COMPOSER, &format!("Approval required\n{COMPOSER}")),
            ),
            (
                "typed draft",
                pane.replace(COMPOSER, "\x1b[1m›\x1b[0m typed reply"),
            ),
            ("no composer", pane.replace(&format!("{COMPOSER}\n\n"), "")),
        ] {
            assert_ne!(blocked, pane, "{case}: fixture edit must apply");
            let (ready, _, _) = prompt_readiness_from_ansi_pane(&blocked);
            assert!(!ready, "{case}\n{blocked}");
            assert!(!submission_readiness_from_ansi_pane(&blocked).0, "{case}");
        }
    }
}
