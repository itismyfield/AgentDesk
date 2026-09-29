//! Tests for watcher single-message footer completion payload seams.
//! Kept in a sibling `*_tests.rs` so the production footer module stays within
//! the `src/services/discord/tmux_watcher/**` namespace LoC cap.

use super::completion_producer::complete_watcher_terminal_footer_or_status_panel_with_sniffer;
use super::single_message_footer::complete_watcher_single_message_completion_footer;
use crate::services::provider::ProviderKind;
use serenity::all::{ChannelId, Http};

#[tokio::test]
async fn watcher_single_message_completion_footer_emits_background_agent_pending_payload() {
    let http = std::sync::Arc::new(Http::new("Bot test-token"));
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel_id = ChannelId::new(4_047_101);
    let provider = ProviderKind::Claude;
    let owner = crate::services::discord::footer_view_reconciler::CompletionFooterOwner::new(
        4_047_102,
        1_700_000_000,
    );

    let committed = complete_watcher_single_message_completion_footer(
        &http,
        &shared,
        channel_id,
        None,
        owner,
        &provider,
        1_700_000_000,
        "Final answer",
        "⠸",
        false,
        true,
    )
    .await;

    assert!(committed);
    let rendered = shared
        .ui
        .placeholder_live_events
        .render_completion_footer(channel_id, &provider, "⠸");
    let block = rendered.block.expect("background-agent pending footer");

    assert!(rendered.has_unfinished_entries);
    assert!(block.contains("Background agents"));
    assert!(block.contains("Waiting for background agents ⠸"));
}

#[tokio::test]
async fn watcher_single_message_completion_footer_producer_threads_sniffed_background_agent_pending()
 {
    for (pending, channel_raw) in [(true, 4_047_111), (false, 4_047_112)] {
        let http = std::sync::Arc::new(Http::new("Bot test-token"));
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel_id = ChannelId::new(channel_raw);
        let provider = ProviderKind::Claude;
        let observed_tmux_session = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sniffer_observed_tmux_session = observed_tmux_session.clone();
        let mut spin_idx = 0;
        let mut last_status_panel_text = String::new();

        complete_watcher_terminal_footer_or_status_panel_with_sniffer(
            &http,
            &shared,
            channel_id,
            &provider,
            1_700_000_000,
            true,
            &mut spin_idx,
            None,
            None,
            "Final answer",
            None,
            &mut last_status_panel_text,
            None,
            Some("AgentDesk-claude-watcher-background-test".to_string()),
            move |tmux_session_name| async move {
                sniffer_observed_tmux_session
                    .lock()
                    .expect("observed tmux session lock")
                    .push(tmux_session_name);
                pending
            },
            Some(channel_raw + 1),
            false,
            false,
            false,
        )
        .await;

        assert_eq!(
            observed_tmux_session
                .lock()
                .expect("observed tmux session lock")
                .as_slice(),
            &[Some("AgentDesk-claude-watcher-background-test".to_string())]
        );

        let rendered = shared
            .ui
            .placeholder_live_events
            .render_completion_footer(channel_id, &provider, "⠸");
        let block_has_background_agents = rendered
            .block
            .as_deref()
            .is_some_and(|block| block.contains("Background agents"));

        assert_eq!(rendered.has_unfinished_entries, pending);
        assert_eq!(block_has_background_agents, pending);
    }
}

/// O owns the listed channel's TUI body message, so the footer completion writes nothing onto it.
#[tokio::test(flavor = "current_thread")]
async fn o_delegated_single_message_footer_completion_sends_no_body() {
    use super::single_message_footer::WatcherCompletionFooterTerminalTarget;
    use crate::services::discord::recovery_engine::o_cut_recorder::start;
    const BODY: &str = "ADK-A14B-footer-body";
    let _o = crate::services::tui_o::cutover::test_override::force_channels(&[(
        4_047_122,
        crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
    )]);
    for (delegated, channel_raw) in [(false, 4_047_121u64), (true, 4_047_122)] {
        let recorder = start(channel_raw).await;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let tmux = format!("AgentDesk-claude-o-footer-{channel_raw}");
        let _bound = delegated.then(|| {
            crate::services::tui_o::cutover::test_override::bind_claude_tui_session(
                &tmux,
                "/tmp/adk-o-footer.jsonl",
            )
        });
        let target = WatcherCompletionFooterTerminalTarget {
            msg_id: serenity::all::MessageId::new(9_425_931),
            text: BODY.to_string(),
        };
        let mut last_status_panel_text = String::new();
        complete_watcher_terminal_footer_or_status_panel_with_sniffer(
            &recorder.http,
            &shared,
            ChannelId::new(channel_raw),
            &ProviderKind::Claude,
            1_700_000_000,
            true,
            &mut 0,
            Some(target),
            Some(serenity::all::MessageId::new(9_425_931)),
            BODY,
            None,
            &mut last_status_panel_text,
            None,
            Some(tmux.clone()),
            |_| async { false },
            Some(channel_raw + 1),
            false,
            false,
            false,
        )
        .await;
        let shows_body = recorder.contents().iter().any(|c| c.contains(BODY));
        assert_eq!(shows_body, !delegated, "delegated={delegated}");
    }
}
