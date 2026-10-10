use super::*;

/// Hint of Codex's dismissible inline banner with actions. While the composer is
/// empty it takes Esc and digits 1-9, so a digit-first literal prompt would pick an action.
const DISMISSIBLE_ACTION_BANNER_HINT: &str =
    "Press a number to choose · esc to dismiss · type to continue";

pub(super) fn pane_has_dismissible_action_banner(pane: &str) -> bool {
    let recent: Vec<&str> = pane
        .lines()
        .rev()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(4)
        .collect();
    let [status, composer, hint, ..] = recent[..] else {
        return false;
    };
    // The hint wraps onto two rows on narrow panes.
    let wrapped = recent.get(3).map(|first| format!("{first} {hint}"));
    (line_is_codex_compact_status_line(status) || line_is_codex_fast_context_status(status))
        && line_is_codex_compact_prompt_marker(composer)
        && (hint == DISMISSIBLE_ACTION_BANNER_HINT
            || wrapped.as_deref() == Some(DISMISSIBLE_ACTION_BANNER_HINT))
}

/// Sends a single Esc, which the banner consumes (Codex disables Esc-Esc backtrack
/// while it is dismissible), then re-reads the pane. Callers never repeat it.
pub(super) fn dismiss_action_banner_once(session_name: &str) -> PromptReadinessSnapshot {
    match host_input::legacy_keys(session_name, &[HostKey::Escape]) {
        Ok(output) if output.status.success() => {}
        Ok(output) => tracing::warn!(
            tmux_session_name = session_name,
            status = %output.status,
            "failed to dismiss Codex TUI inline banner"
        ),
        Err(error) => tracing::warn!(
            tmux_session_name = session_name,
            error,
            "failed to dismiss Codex TUI inline banner"
        ),
    }
    std::thread::sleep(PROMPT_SUBMIT_INITIAL_SETTLE);
    prompt_readiness_snapshot(session_name)
}

#[cfg(test)]
mod tests {
    use super::super::super::host_input::spy::{SpyGuard, SpyState};
    use super::super::*;

    const BANNER: &str =
        include_str!("../../../../tests/fixtures/tui_input/codex-0.160.0-security-banner.ansi");

    /// The same pane after Codex drops the dismissed banner rows.
    fn dismissed() -> String {
        let start = BANNER
            .find("  \x1b[1mSet up security for Daybreak mode")
            .unwrap();
        let composer = BANNER.find("\x1b[1m›\x1b[0m \x1b[2mAsk Codex").unwrap();
        format!("{}{}", &BANNER[..start], &BANNER[composer..])
    }

    fn sent(calls: &[String], call: &str) -> usize {
        calls.iter().filter(|c| c.as_str() == call).count()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn security_banner_pane_is_ready_and_followup_dismisses_it_once() {
        let mut captures = vec![Some(BANNER.to_string()); 3];
        captures.extend(vec![Some(dismissed()); 2]);
        let own_draft = dismissed().replace(
            "\x1b[2mAsk Codex to do anything\x1b[0m",
            "1 fixture follow-up",
        );
        captures.extend([Some(own_draft), Some(dismissed())]);
        let guard = SpyGuard::install(SpyState {
            captures: captures.into(),
            ..SpyState::default()
        });
        let session = "composer-security-banner";
        let snapshot = prompt_readiness_snapshot(session);
        assert!(snapshot.composer_marker_detected, "{snapshot:?}");
        let ready = wait_until_codex_tui_input_ready(session, PromptReadinessKind::Followup, None);
        assert!(ready.is_ok(), "{ready:?}");
        // A digit-first prompt is what the banner would take as its menu choice.
        let outcome = submit_codex_followup_prompt(session, "1 fixture follow-up", None);
        assert!(
            matches!(outcome, CodexFollowupPromptSubmitOutcome::Submitted),
            "{outcome:?}"
        );
        let calls = guard.calls();
        assert_eq!(sent(&calls, "keys:Escape"), 1, "{calls:?}");
        assert_eq!(sent(&calls, "keys:Enter"), 1, "{calls:?}");
        let escape = calls.iter().position(|c| c == "keys:Escape").unwrap();
        let literal = calls
            .iter()
            .position(|c| c.starts_with("literal:"))
            .unwrap();
        assert!(escape < literal, "{calls:?}");
        assert!(!calls.iter().any(|c| c.starts_with("kill")), "{calls:?}");
    }

    #[test]
    fn banner_that_survives_one_escape_gets_no_second_escape_or_input() {
        let guard = SpyGuard::install(SpyState {
            captures: vec![Some(BANNER.to_string()); 4].into(),
            ..SpyState::default()
        });
        let outcome =
            submit_codex_followup_prompt("composer-sticky-banner", "fixture follow-up", None);
        assert!(
            matches!(
                outcome,
                CodexFollowupPromptSubmitOutcome::NotSubmitted { .. }
            ),
            "{outcome:?}"
        );
        let calls = guard.calls();
        assert_eq!(sent(&calls, "keys:Escape"), 1, "{calls:?}");
        assert!(
            !calls
                .iter()
                .any(|c| c == "keys:Enter" || c.starts_with("literal:")),
            "{calls:?}"
        );
    }

    #[test]
    fn blocking_menu_above_the_composer_gets_no_escape() {
        for hint in [
            "Press a number to choose",
            "Press enter to confirm or esc to cancel",
        ] {
            let pane = BANNER.replace(
                "Press a number to choose · esc to dismiss · type to continue",
                hint,
            );
            let guard = SpyGuard::install(SpyState {
                captures: vec![Some(pane); 4].into(),
                ..SpyState::default()
            });
            let _ = submit_codex_followup_prompt("composer-menu", "fixture follow-up", None);
            assert_eq!(sent(&guard.calls(), "keys:Escape"), 0, "{hint}");
        }
    }
}
