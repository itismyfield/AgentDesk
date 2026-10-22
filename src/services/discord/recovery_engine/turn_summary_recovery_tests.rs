use super::super::*;
use crate::services::discord::turn_end_summary::tests::{Isolation, row};

#[tokio::test(flavor = "current_thread")]
async fn actual_recovery_completion_never_posts_a_summary_for_empty_suppressed_rebind_or_restart_turns()
 {
    let _isolation = Isolation::new();
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    for (index, kind) in ["empty", "suppressed", "rebind", "planned_restart"]
        .into_iter()
        .enumerate()
    {
        let channel = 586_301 + index as u64;
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        Arc::get_mut(&mut shared)
            .unwrap()
            .ui
            .status_panel_v2_enabled = true;
        let recorder = super::super::o_cut_recorder::start(channel).await;
        let mut state = row(channel, 0, "recovery-episode");
        state.started_at = "2026-10-19 00:00:00".into();
        state.output_path = Some(format!("/fixture/recovery-{channel}.jsonl"));
        state.turn_start_offset = Some(10);
        if kind == "empty" {
            state.full_response.clear();
            state.response_sent_offset = 0;
        }
        state.relay_ownership_only = kind == "suppressed";
        state.rebind_origin = kind == "rebind";
        if kind == "planned_restart" {
            state.set_restart_mode(
                crate::services::discord::restart_mode::InflightRestartMode::DrainRestart,
            );
        }
        assert_ne!(state.effective_finalizer_turn_id(), 0);
        let _ = complete_recovery_visible_turn(
            &recorder.http,
            &shared,
            &ProviderKind::Claude,
            &state,
            false,
            "turn_summary_recovery_test",
        )
        .await;
        assert!(
            recorder.contents().is_empty(),
            "{kind}: {:?}",
            recorder.contents()
        );
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn actual_recovery_old_panel_edit_is_exercised_without_posting_a_terminal_summary() {
    let _isolation = Isolation::new();
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let channel = 586_505;
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared)
        .unwrap()
        .ui
        .status_panel_v2_enabled = true;
    let recorder = super::super::o_cut_recorder::start(channel).await;
    let mut state = row(channel, 0, "recovery-positive-panel");
    state.status_message_id = Some(101);
    assert_ne!(state.effective_finalizer_turn_id(), 0);
    let _ = complete_recovery_visible_turn(
        &recorder.http,
        &shared,
        &ProviderKind::Claude,
        &state,
        false,
        "turn_summary_recovery_positive_test",
    )
    .await;
    let calls = recorder.calls();
    assert_eq!(
        calls.len(),
        1,
        "actual recovery old-panel completion must not POST a summary"
    );
    assert!(calls[0].route.starts_with("PATCH "));
    assert!(calls[0].route.ends_with("/messages/101"));
    assert_eq!(
        calls[0].content.as_deref().unwrap().lines().next(),
        Some("-# ✅ 완료")
    );
}
