pub(super) fn line_is_codex_fast_context_status(line: &str) -> bool {
    let parts: Vec<&str> = line.split('·').map(str::trim).collect();
    if parts.len() == 3 && matches!(parts[0], "Fast on" | "Fast off") {
        return !parts[1].is_empty() && context_percent(parts[2]).is_some();
    }
    model_first_status(&parts)
}

pub(super) fn line_is_codex_model_first_status(line: &str) -> bool {
    let parts: Vec<&str> = line.split('·').map(str::trim).collect();
    model_first_status(&parts)
}

fn model_first_status(parts: &[&str]) -> bool {
    let Some(fast) = parts
        .iter()
        .skip(1)
        .position(|part| matches!(*part, "Fast on" | "Fast off"))
        .map(|index| index + 1)
    else {
        return false;
    };
    // Model and optional effort are opaque identifiers; the footer structure is the evidence.
    let model: Vec<&str> = parts[0].split_ascii_whitespace().collect();
    fast > 0
        && (1..=2).contains(&model.len())
        && model.iter().all(|token| identifier(token))
        && parts[1..fast].iter().all(|part| quota(part))
        && parts.get(fast + 1).is_some_and(|part| {
            context_percent(part)
                .and_then(|n| n.parse::<u8>().ok())
                .is_some_and(|n| n <= 100)
        })
        && parts.len() <= fast + 3
        && parts[fast + 2..].iter().all(|part| context_window(part))
}

fn identifier(value: &str) -> bool {
    value
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || b"-_.:/".contains(&ch))
}

fn context_percent(value: &str) -> Option<&str> {
    let percent = value.strip_prefix("Context ")?.strip_suffix("% left")?;
    (!percent.is_empty() && percent.bytes().all(|ch| ch.is_ascii_digit())).then_some(percent)
}

fn quota(value: &str) -> bool {
    let parts: Vec<&str> = value.split_ascii_whitespace().collect();
    parts.len() == 3
        && identifier(parts[0])
        && parts[1]
            .strip_suffix('%')
            .filter(|n| !n.is_empty() && n.bytes().all(|ch| ch.is_ascii_digit()))
            .and_then(|n| n.parse::<u8>().ok())
            .is_some_and(|n| n <= 100)
        && parts[2] == "left"
}

fn context_window(value: &str) -> bool {
    let Some(size) = value.strip_suffix(" window") else {
        return false;
    };
    let number = size
        .strip_suffix('K')
        .or_else(|| size.strip_suffix('M'))
        .unwrap_or(size);
    !number.is_empty()
        && number.bytes().all(|ch| ch.is_ascii_digit() || ch == b'.')
        && number
            .parse::<f64>()
            .is_ok_and(|n| n.is_finite() && n > 0.0)
}

#[cfg(test)]
mod tests {
    use super::super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn current_codex_status_fixture_allows_followup_without_kill() {
        use super::super::super::host_input::spy::{SpyGuard, SpyState};
        let pane = include_str!("../../../../tests/fixtures/tui_input/codex-0.160.0-idle.ansi");
        let guard = SpyGuard::install(SpyState {
            captures: [pane, pane, pane, pane]
                .into_iter()
                .map(|p| Some(p.to_string()))
                .collect(),
            ..SpyState::default()
        });
        let session = "composer-current-fixture";
        let snapshot = prompt_readiness_snapshot(session);
        assert!(
            snapshot.composer_marker_detected,
            "actual composer must be ready: {snapshot:?}"
        );
        let ready =
            wait_until_codex_tui_input_ready(session, PromptReadinessKind::PostTurnHandoff, None);
        assert!(ready.is_ok(), "{ready:?}");
        assert!(matches!(
            submit_codex_followup_prompt(session, "fixture follow-up", None),
            CodexFollowupPromptSubmitOutcome::Submitted
        ));
        let calls = guard.calls();
        assert_eq!(
            calls.iter().filter(|c| c.as_str() == "keys:Enter").count(),
            1
        );
        assert_eq!(calls.iter().filter(|c| c.starts_with("kill")).count(), 0);
    }

    #[test]
    fn current_codex_status_fixture_keeps_busy_draft_and_history_guards() {
        let idle = include_str!("../../../../tests/fixtures/tui_input/codex-0.160.0-idle.ansi");
        let draft = include_str!("../../../../tests/fixtures/tui_input/codex-0.160.0-draft.ansi");
        let (marker, has_draft, _) = prompt_readiness_from_ansi_pane(draft);
        assert!(!marker);
        assert!(has_draft);
        for pane in [
            idle.replace("Context 97% left", "Context unknown"),
            idle.replace("Fast off", "slow prose"),
            idle.replace("weekly 99% left", "assistant quota prose"),
            idle.replace("weekly 99% left", "weekly 101% left"),
            idle.replace("weekly 99% left", "weekly +99% left"),
            idle.replace("258K window", "assistant prose"),
            idle.replace("Context 97% left", "Context +97% left"),
            idle.replace("GPT-6.1-Sol xhigh", "assistant prose here"),
            idle.replace(
                "Worked for 2s • 7:26 AM",
                "• Working (2s • esc to interrupt)",
            ),
            format!(
                "{idle}
new output
new output
new output
new output"
            ),
        ] {
            assert!(!prompt_readiness_from_ansi_pane(&pane).0, "{pane}");
        }
        assert!(!prompt_readiness_from_ansi_pane("Worked for 6s • 6:26 AM").0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn generalized_codex_footer_submits_followup_for_model_and_effort_variants() {
        use super::super::super::host_input::spy::{SpyGuard, SpyState};
        let idle = include_str!("../../../../tests/fixtures/tui_input/codex-0.160.0-idle.ansi");
        for model in [
            "o3 none",
            "codex-next balanced",
            "local/model",
            "custom:tag auto",
            "approval required",
            "Fast off",
            "legacy-fast-first",
        ] {
            let pane = if model == "legacy-fast-first" {
                include_str!("../../../../tests/fixtures/tui_input/codex-idle-dim.ansi").replace(
                    "\x1b[0;1m›",
                    "Approval required: this is answer prose.\n\n\x1b[0;1m›",
                )
            } else {
                idle.replace("GPT-6.1-Sol xhigh", model).replace(
                    "FIXTURE_READY",
                    "Fixture answer\nApproval required: this is answer prose.",
                )
            };
            let guard = SpyGuard::install(SpyState {
                captures: vec![Some(pane); 3].into(),
                ..SpyState::default()
            });
            let session = "composer-generalized-fixture";
            assert!(
                matches!(
                    submit_codex_followup_prompt(session, "fixture follow-up", None),
                    CodexFollowupPromptSubmitOutcome::Submitted
                ),
                "warm follow-up must submit for {model}"
            );
            assert_eq!(
                guard
                    .calls()
                    .iter()
                    .filter(|c| c.as_str() == "keys:Enter")
                    .count(),
                1
            );
            assert_eq!(
                guard
                    .calls()
                    .iter()
                    .filter(|c| c.starts_with("kill"))
                    .count(),
                0
            );
            assert!(
                wait_until_codex_tui_input_ready(
                    session,
                    PromptReadinessKind::PostTurnHandoff,
                    None
                )
                .is_ok()
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn generalized_codex_footer_blocks_submit_during_work_and_approval() {
        use super::super::super::host_input::spy::{SpyGuard, SpyState};
        let idle = include_str!("../../../../tests/fixtures/tui_input/codex-0.160.0-idle.ansi");
        for state in [
            "• Working (2s • esc to interrupt)",
            "Approval required",
            "Allow command",
            "Allow this action",
            "Confirm to continue",
            "Do you trust this folder?",
            "Sign in to continue",
            "Authentication required",
        ] {
            let pane = idle
                .replace("GPT-6.1-Sol xhigh", "codex-next none")
                .replace("Worked for 2s • 7:26 AM", state);
            for pane in [
                pane.clone(),
                pane.replace(
                    "\x1b[1m›\x1b[0m \x1b[2mAsk Codex to do anything\x1b[0m",
                    "› 1. Yes, proceed (y)",
                ),
            ] {
                let guard = SpyGuard::install(SpyState {
                    captures: vec![Some(pane); 2].into(),
                    ..SpyState::default()
                });
                let outcome = submit_codex_followup_prompt(
                    "composer-blocked-fixture",
                    "fixture follow-up",
                    None,
                );
                assert!(
                    !guard
                        .calls()
                        .iter()
                        .any(|c| c == "keys:Enter" || c.starts_with("literal:")),
                    "blocked composer must receive no input: {state}"
                );
                assert!(
                    matches!(
                        outcome,
                        CodexFollowupPromptSubmitOutcome::NotSubmitted { .. }
                    ),
                    "blocked composer must not submit: {state}"
                );
            }
        }
    }
}
