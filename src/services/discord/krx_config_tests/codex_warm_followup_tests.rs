//! Codex warm follow-up pane-kill test that saves a discord-layer actual episode, moved out of KRX
//! codex_tui.

use crate::services::codex_tui::execution_end::attempt::{self, Phase};
use crate::services::codex_tui::host_input::spy::{SpyGuard, SpyState, known, resolved};
use crate::services::codex_tui::input::{
    CodexFollowupPromptSubmitOutcome, PromptReadinessSnapshot,
};
use crate::services::codex_tui::warm_followup::submit::settle_submit;
use crate::services::codex_tui::warm_followup::submit::tests::install_clear_end;
use crate::services::codex_tui::warm_followup::{
    CodexWarmFallbackReason, CodexWarmFollowupOutcome,
};
use crate::services::discord::codex_absence::replacement::Probe;
use crate::services::provider::{CancelToken, cancel_requested};
use crate::services::session_host::HostKind;
use crate::services::tmux_common as tc;
use crate::services::tui_o::store::codex_end;
use std::sync::Arc;

#[test]
fn tracked_retry_safe_draft_carries_replacement_through_its_pane_kill() {
    use crate::services::codex_tui::execution_end::producer::LaunchObservation;
    use crate::services::tui_prompt_dedupe::binding_context::{
        BindingContext, PreparedIncarnation,
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    crate::services::discord::terminal_delivery_custody::install_dl_ports_for_tests();
    attempt::reset_for_tests();
    let _mode = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_CODEX_DIRECT_TUI_SOURCE_MODE",
        "legacy".as_ref(),
    );
    let channel = 62_841_779;
    let tmux = "AgentDesk-codex-replacement-draft";
    let token = Arc::new(CancelToken::new());
    let row_path =
        crate::services::discord::save_actual_episode_for_tests(channel, tmux, 30, &token);
    let before_row = std::fs::read(&row_path).unwrap();
    let row: serde_json::Value = serde_json::from_slice(&before_row).unwrap();
    assert_eq!(row["request_owner_user_id"], 8);
    assert_eq!(row["turn_nonce"].as_str(), token.turn_nonce());
    let end = install_clear_end(channel, tmux);
    let dispatch = attempt::begin(Some(channel), Some(tmux));
    let id = attempt::current(channel, tmux).unwrap();
    let mut observation = LaunchObservation::new(Some(channel), tmux);
    assert!(attempt::take_ticket(&id).is_none());
    let path = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(path.path(), "{}\n").unwrap();
    let prompt = "keep this draft";
    let draft = PromptReadinessSnapshot {
        composer_marker_detected: true,
        prompt_draft_detected: true,
        tmux_pane_alive: true,
        capture_available: true,
        pane_tail: format!(
            "old output\n╭────────────────────────────────────────╮\n│ {prompt} ▌ │\n╰────────────────────────────────────────╯\n  ⏎ send"
        ),
    };
    let target = resolved(known(HostKind::Tmux));
    let spy = SpyGuard::install(SpyState {
        pane_pid: Some(4242),
        ..Default::default()
    });
    let probe = Probe::new(channel, tmux);
    let outcome = settle_submit(
        &target,
        tmux,
        CodexFollowupPromptSubmitOutcome::RetrySafeDraft {
            first: draft.clone(),
            second: draft,
        },
        path.path(),
        3,
        prompt,
        (Some(channel), Some(&token)),
    );
    let Some(CodexWarmFollowupOutcome::FallbackAfterPaneKill(reason, Some(replacement))) = outcome
    else {
        panic!("tracked pane kill must transfer its active replacement to cold launch");
    };
    assert_eq!(reason, CodexWarmFallbackReason::SubmitFailed);
    assert!(probe.defer_death());
    assert!(probe.cleanup().is_none());
    assert_eq!(std::fs::read(&row_path).unwrap(), before_row);
    assert_eq!(row["turn_nonce"].as_str(), token.turn_nonce());
    assert!(!cancel_requested(Some(token.as_ref())));
    assert_eq!(
        spy.calls(),
        [
            "pane_pid",
            "kill_tree",
            "kill_session:Codex TUI warm follow-up submit failed with draft preserved",
            "stopped"
        ]
    );
    let mut context: BindingContext =
        serde_json::from_value(serde_json::to_value(&end.context).unwrap()).unwrap();
    context.execution_nonce = "b".repeat(32);
    context.launch_mode = "resume".into();
    context.expected_native_session_id = Some(end.source.session_id.clone());
    let prepared = PreparedIncarnation::create(context).unwrap();
    observation.prepare(&prepared.context);
    std::fs::write(
        tc::session_temp_path(tmux, "spawn_nonce"),
        &prepared.context.execution_nonce,
    )
    .unwrap();
    observation.spawned(&prepared.context);
    tc::with_tmux_source_authority(tmux, |authority| {
        assert_eq!(
            attempt::phase_under(&id, authority),
            Some(Phase::Bound {
                nonce: prepared.context.execution_nonce.clone(),
            })
        );
    });
    assert!(probe.defer_death());
    assert!(probe.cleanup().is_none());
    assert_eq!(std::fs::read(&row_path).unwrap(), before_row);
    assert!(!cancel_requested(Some(token.as_ref())));
    let root = crate::config::runtime_root().unwrap();
    assert_eq!(
        codex_end::read(&root, channel).unwrap().as_slice(),
        std::slice::from_ref(&end)
    );
    replacement.complete();
    assert!(!Probe::new(channel, tmux).defer_death());
    drop(dispatch);
    let _later_fresh = attempt::begin(Some(channel), Some(tmux));
    let later = attempt::current(channel, tmux).unwrap();
    assert!(attempt::take_ticket(&later).is_none());
    assert_eq!(codex_end::read(&root, channel).unwrap(), [end]);
}
