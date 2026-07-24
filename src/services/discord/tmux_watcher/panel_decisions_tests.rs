//! Tests for the watcher status-panel / completion-footer DECISIONS.
//!
//! PURE MOVE of `panel_decisions.rs`'s `#[cfg(test)] mod
//! completion_footer_suppression_tests` (zero logic change), kept in a sibling
//! `*_tests.rs` so the production module stays within the
//! `src/services/discord/tmux_watcher/**` namespace LoC cap (test files are
//! excluded from the cap by the audit's `production_rust_files()` filter).
//! #3969 additions: the `/loop` self-paced (`ExternalInput`) vs Discord-origin
//! (`Managed`) per-class regression coverage for the root-invariant signal.

use super::*;
use crate::services::discord::ProviderKind;
use crate::services::discord::inflight::{InflightTurnState, TurnSource};

const ADK_CC_SESSION: &str = "AgentDesk-claude-adk-cc";

/// Build an inflight row for the orchestrator TUI channel with a given
/// `turn_source`, mirroring the shape the watcher loads as `inflight_before_relay`
/// at the terminal chokepoint (`tmux_watcher.rs:3857`).
fn inflight_with_turn_source(
    turn_source: TurnSource,
    tmux_session_name: Option<&str>,
) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        123,
        Some("adk-cc".to_string()),
        42,
        1001,
        2002,
        "prompt".to_string(),
        Some("session".to_string()),
        tmux_session_name.map(str::to_string),
        Some("/tmp/out.jsonl".to_string()),
        None,
        0,
    );
    state.turn_source = turn_source;
    state
}

#[test]
fn completion_footer_tick_requires_registered_unfinished_target() {
    let interval = std::time::Duration::from_secs(5);
    assert!(watcher_completion_footer_should_tick(
        true, interval, interval
    ));
    assert!(!watcher_completion_footer_should_tick(
        false, interval, interval
    ));
    assert!(!watcher_completion_footer_should_tick(
        true,
        std::time::Duration::from_secs(4),
        interval
    ));
}

#[test]
fn gate_suppresses_for_either_mirror_signal_and_keeps_discord_origin_3964() {
    // cached flag true (row pre-existed) → suppress.
    assert!(watcher_external_input_completion_footer_suppressed(
        true, true, false, false
    ));
    // cached flag FALSE but completion_background true — the #3964 fresh
    // synthetic MonitorAutoTurn / background regression the bridge-style
    // cached flag cannot catch at the terminal chokepoint → still suppress.
    assert!(watcher_external_input_completion_footer_suppressed(
        true, false, true, false
    ));
    assert!(watcher_external_input_completion_footer_suppressed(
        true, true, true, false
    ));
    // #3969: cached flag AND completion_background BOTH false (the /loop
    // ScheduleWakeup self-paced class — stale :1017 flag + no task-notification),
    // but the chokepoint-fresh non-Managed mirror signal is true → suppress.
    // This is the previously-missed leak the root invariant closes.
    assert!(watcher_external_input_completion_footer_suppressed(
        true, false, false, true
    ));
    // genuine Discord-origin turn: ALL three mirror signals false → KEEP the
    // #3089 footer (the Managed turn's only status surface).
    assert!(!watcher_external_input_completion_footer_suppressed(
        true, false, false, false
    ));
    // non-footer-mode is never the single-message footer path.
    assert!(!watcher_external_input_completion_footer_suppressed(
        false, true, true, false
    ));
    assert!(!watcher_external_input_completion_footer_suppressed(
        false, false, true, false
    ));
    assert!(!watcher_external_input_completion_footer_suppressed(
        false, false, false, true
    ));
}

/// #3969 PER-CLASS REGRESSION: the `/loop` ScheduleWakeup self-paced turn class
/// (the case #3961/#3964/#3967 all missed). Its inflight carries
/// `TurnSource::ExternalInput` (created by the claude idle bridge), is NOT a
/// `<task-notification>`, and its `:1017` panel-eligibility flag is stale-`false`
/// — so BOTH pre-#3969 gate disjuncts are `false` and the #3089 footer leaked.
/// The chokepoint-fresh non-Managed signal catches it, and the full gate (with
/// the two stale disjuncts forced `false`, exactly as the live leak presented)
/// now suppresses.
#[test]
fn loop_self_paced_external_input_turn_suppresses_footer_3969() {
    let temp = tempfile::TempDir::new().expect("temp runtime root");
    let _root_guard = crate::config::set_agentdesk_root_for_test(temp.path());
    let inflight = inflight_with_turn_source(TurnSource::ExternalInput, Some(ADK_CC_SESSION));

    let mirror =
        watcher_inflight_is_non_managed_tui_mirror_for_session(Some(&inflight), ADK_CC_SESSION);
    assert!(
        mirror,
        "a /loop self-paced ExternalInput turn is a non-Managed TUI mirror"
    );

    // Reproduce the exact live leak signal values: stale cached flag false,
    // not a background task-notification — only the root-invariant disjunct fires.
    assert!(
        watcher_external_input_completion_footer_suppressed(true, false, false, mirror),
        "the /loop self-paced turn must SUPPRESS the reconstructed #3089 footer"
    );

    // The other two non-Managed mirror origins are caught identically.
    assert!(watcher_inflight_is_non_managed_tui_mirror_for_session(
        Some(&inflight_with_turn_source(
            TurnSource::MonitorTriggered,
            Some(ADK_CC_SESSION)
        )),
        ADK_CC_SESSION,
    ));
    assert!(watcher_inflight_is_non_managed_tui_mirror_for_session(
        Some(&inflight_with_turn_source(
            TurnSource::ExternalAdopted,
            Some(ADK_CC_SESSION)
        )),
        ADK_CC_SESSION,
    ));
}

/// #3969 NON-REGRESSION: a genuine Discord-USER-message turn on the SAME TUI
/// channel carries `TurnSource::Managed`, so the non-Managed signal is `false`
/// and — with the other two disjuncts also `false` for a real user message — the
/// gate KEEPS the #3089 footer (the Discord user's only status surface). Proves
/// the root-invariant signal can never misclassify a Discord-origin turn as a
/// mirror.
#[test]
fn discord_origin_managed_turn_keeps_footer_3969() {
    let temp = tempfile::TempDir::new().expect("temp runtime root");
    let _root_guard = crate::config::set_agentdesk_root_for_test(temp.path());
    let inflight = inflight_with_turn_source(TurnSource::Managed, Some(ADK_CC_SESSION));

    let mirror =
        watcher_inflight_is_non_managed_tui_mirror_for_session(Some(&inflight), ADK_CC_SESSION);
    assert!(
        !mirror,
        "a Discord-origin Managed turn is NOT a mirror — never suppress its footer"
    );
    assert!(
        !watcher_external_input_completion_footer_suppressed(true, false, false, mirror),
        "the Discord-user-message turn must KEEP its #3089 footer (#3089 non-regression)"
    );
}

/// #3969 guard: the `tmux_session_name` filter ignores a stale inflight bound to
/// a DIFFERENT pane (or a missing row) — a non-matching session never flips the
/// mirror signal true, so it cannot suppress a footer it does not own.
#[test]
fn non_managed_mirror_signal_requires_session_match_3969() {
    let temp = tempfile::TempDir::new().expect("temp runtime root");
    let _root_guard = crate::config::set_agentdesk_root_for_test(temp.path());
    let other_session =
        inflight_with_turn_source(TurnSource::ExternalInput, Some("AgentDesk-claude-other"));
    assert!(!watcher_inflight_is_non_managed_tui_mirror_for_session(
        Some(&other_session),
        ADK_CC_SESSION,
    ));
    let no_session = inflight_with_turn_source(TurnSource::ExternalInput, None);
    assert!(!watcher_inflight_is_non_managed_tui_mirror_for_session(
        Some(&no_session),
        ADK_CC_SESSION,
    ));
    assert!(!watcher_inflight_is_non_managed_tui_mirror_for_session(
        None,
        ADK_CC_SESSION,
    ));
}

#[test]
fn synthetic_tui_mirror_emits_prose_without_tui_chrome_3964() {
    // The exact #3964 corruption: assistant prose + the rendered
    // Context/Tasks/Subagents footer. A suppressed mirror composes with a `None`
    // block → prose ALONE; a Discord-origin turn (block present) keeps the footer.
    let prose = "#3879 작업 완료 — 다음 이슈 대기.";
    let chrome = "📦 166.4k / 1.0M (16%) · auto-compact 60%\n\nTasks\n└ TaskUpdate 4\n\nSubagents\n└ general-purpose Fix #3879";

    let discord_origin =
        crate::services::discord::single_message_panel::compose_completion_footer_text(
            prose,
            Some(chrome),
        );
    assert!(discord_origin.starts_with(prose));
    assert!(discord_origin.contains("📦"));
    assert!(discord_origin.contains("Subagents"));

    let mirror =
        crate::services::discord::single_message_panel::compose_completion_footer_text(prose, None);
    assert_eq!(mirror, prose);
    assert!(!mirror.contains("Context"));
    assert!(!mirror.contains("auto-compact"));
    assert!(!mirror.contains("Subagents"));
}

#[test]
fn synthetic_tui_mirror_finalize_strips_live_status_footer_to_prose_3964() {
    // The non-short-replace path: a live status footer still on the message at
    // completion finalizes (block = None) down to prose, no chrome re-appended.
    let panel = "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
    let body = format!(
        "Final answer\n\n{}",
        crate::services::discord::single_message_panel::compose_footer_status_block("⠸", panel)
    );
    let finalized =
        crate::services::discord::single_message_panel::finalize_streaming_footer_with_completion(
            &body,
            &ProviderKind::Claude,
            None,
        )
        .expect("a live status footer must finalize to a stripped edit");
    assert_eq!(finalized, "Final answer");
    assert!(!finalized.contains("Subagents"));
    assert!(!finalized.contains("진행 중"));
}
