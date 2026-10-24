//! A prior process's Herdr row whose end has a dedicated owner is left to it: the leaked-row sweep
//! neither cancels nor completes it, even where it would demote any other stale row.
use super::*;

/// The dead-frontier row the sweep cancels, but on a Herdr host from a prior process with an
/// admitted terminal, whose own restart owner settles it.
#[test]
fn the_sweep_leaves_a_restarted_herdr_row_to_its_owner() {
    let _guard = worker_test_lock();
    let _env_lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::TempDir::new().expect("runtime root");
    let _env = EnvReset(std::env::var_os("AGENTDESK_ROOT_DIR"));
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
    current_thread_rt().block_on(async {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let provider = crate::services::provider::ProviderKind::Claude;
        let channel_id = 7_289_110;
        let channel = poise::serenity_prelude::ChannelId::new(channel_id);
        let stale_msg = 7_289_210;
        let tmux = "AgentDesk-claude-7289-restarted-herdr";
        let marker = crate::services::tmux_common::session_temp_path(tmux, "host_kind");
        std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(&marker, "herdr").unwrap();
        let output_path = root.path().join("restarted-herdr.jsonl");
        std::fs::write(
            &output_path,
            r#"{"type":"system","subtype":"init","session_id":"s"}"#,
        )
        .expect("write output");
        let token = Arc::new(crate::services::provider::CancelToken::new());
        assert!(
            crate::services::discord::mailbox_try_start_turn(
                &shared,
                channel,
                token.clone(),
                poise::serenity_prelude::UserId::new(1),
                poise::serenity_prelude::MessageId::new(stale_msg),
            )
            .await
        );
        let mut state =
            stale_foreign_state(provider.clone(), channel_id, stale_msg, tmux, &output_path);
        state.turn_nonce = token.turn_nonce().map(str::to_owned);
        state.born_generation = shared.restart.current_generation + 7;
        state.tui_terminal_kind =
            Some(crate::services::agent_protocol::NativeTerminalKind::Completed);
        stamp_claude_ready_for_input_evidence(&mut state, &output_path);
        write_inflight_fixture(root.path(), &provider, &state);
        let mut rec = record("claude", channel_id, 7_289_310);
        rec.tmux_session_name = tmux.to_string();
        assert!(
            stale_foreign_inflight_is_reclaimable_at(&state, &rec, chrono::Utc::now().timestamp()),
            "the sweep's own age and frontier cutoff admit the row"
        );

        assert!(!demote_leaked_foreign_inflight_if_current(&shared, &rec).await);
        let kept = crate::services::discord::inflight::load_inflight_state(&provider, channel_id)
            .expect("the row stays for its owner");
        assert_eq!(kept.turn_nonce, state.turn_nonce);
        assert!(!token.cancelled.load(std::sync::atomic::Ordering::Relaxed));
    });
}
