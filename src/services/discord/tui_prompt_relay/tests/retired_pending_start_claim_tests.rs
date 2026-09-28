//! A claim that races the retirement of its finished pending start must not
//! save a row for that turn.
use super::*;
use crate::services::discord::{inflight, tui_direct_pending_start};

#[tokio::test(flavor = "current_thread")]
async fn retire_during_admission_refuses_row_save_and_releases_mailbox() {
    let root = tempfile::tempdir().expect("isolated runtime root");
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let shared = super::super::super::make_shared_data_for_tests();
    let channel = ChannelId::new(6_287_005);
    let tmux = "AgentDesk-claude-6287-retire";
    let transcript = root.path().join("retire.jsonl");
    std::fs::write(&transcript, b"").expect("empty transcript");
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        tmux,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: transcript.to_str().expect("utf8 path").to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let anchor = 6_287_105u64;
    let record = tui_direct_pending_start::TuiDirectPendingStart {
        provider: "claude".to_string(),
        channel_id: channel.get(),
        tmux_session_name: tmux.to_string(),
        prompt_text: "prompt".to_string(),
        anchor_message_id: anchor,
        lease_relay_owner: "bridge_adapter".to_string(),
        lease_runtime_kind: Some("claude_tui".to_string()),
        lease_turn_id: None,
        lease_session_key: None,
        generation: 0,
        created_at_ms: 0,
        observed_at_ms: 0,
        state: tui_direct_pending_start::PendingStartState::Waiting,
        attempt_count: 0,
        captured_source: None,
    };
    tui_direct_pending_start::persist(&record).expect("persist record");

    let entered = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    *synthetic_start::bridge_handoff::ADMISSION_PAUSE
        .lock()
        .unwrap() = Some((channel.get(), entered.clone(), resume.clone()));
    let claiming = tokio::spawn({
        let shared = shared.clone();
        async move {
            synthetic_start::claim_tui_direct_synthetic_turn(
                &shared,
                &ProviderKind::Claude,
                channel,
                tmux,
                "prompt",
                MessageId::new(anchor),
                &s1_lease_5833(Some("turn-6287-retire")),
            )
            .await
        }
    });
    entered.notified().await;
    assert!(tui_direct_pending_start::retire_completed(
        record.key(),
        tmux
    ));
    resume.notify_one();
    let claim = claiming.await.expect("claim task");

    assert!(!claim.claimed, "a retired turn must not be claimed");
    assert!(
        inflight::load_inflight_state(&ProviderKind::Claude, channel.get()).is_none(),
        "no row may be saved for a retired turn"
    );
    let snapshot = super::super::super::mailbox_snapshot(&shared, channel).await;
    assert_ne!(
        snapshot.active_user_message_id,
        Some(MessageId::new(anchor)),
        "the admitted mailbox episode is released"
    );
}
