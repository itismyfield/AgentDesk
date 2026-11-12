use super::*;
use crate::test_support::wait::{WAIT_CAP, await_within};
use serde_json::json;

#[tokio::test]
async fn i8117_actual_bridge_uses_exact_terminal_not_progress_or_preview() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    with_isolated_runtime_root(|| async {
        const USER_MESSAGE: u64 = 9_100_000_000_008_117_001;
        const PLACEHOLDER: u64 = 9_100_000_000_008_117_002;
        for (terminal, quiet) in [(" NO_reply ", true), ("완료: 인용 NO_REPLY 포함", false)] {
            let (fixture, _lease) =
                Fixture::with_user_message(ProviderKind::Claude, USER_MESSAGE).await;
            let mut row = inflight::InflightTurnState::new(
                ProviderKind::Claude,
                fixture.channel.get(),
                None,
                7,
                USER_MESSAGE,
                PLACEHOLDER,
                "routine".into(),
                None,
                None,
                None,
                None,
                0,
            );
            row.turn_nonce = fixture.token.turn_nonce().map(str::to_owned);
            row.runtime_kind = Some(RuntimeHandoffKind::ProcessBackend);
            let mut encoded = serde_json::to_value(&row).unwrap();
            encoded["routine_presentation"] =
                json!({"policy":"claude_process_c1", "terminal":"missing"});
            row = serde_json::from_value(encoded).unwrap();
            inflight::save_inflight_state(&row).unwrap();
            crate::services::discord::increment_global_active(&fixture.shared, "i8117_bridge");
            let (completion, completed) = tokio::sync::oneshot::channel();
            let shown = Arc::new(RecordingDirectGateway::default());
            let gateway: Arc<dyn TurnGateway> = shown.clone();
            let context = TurnBridgeContext {
                provider: ProviderKind::Claude,
                gateway,
                channel_id: fixture.channel,
                user_msg_id: Some(MessageId::new(USER_MESSAGE)),
                user_text_owned: "quiet routine".into(),
                request_owner_name: "routine fixture".into(),
                role_binding: None,
                adk_session_key: None,
                adk_session_name: None,
                adk_session_info: None,
                adk_cwd: None,
                dispatch_id: None,
                dispatch_kind: None,
                memory_recall_usage: TokenUsage::default(),
                context_window_tokens: 0,
                context_compact_percent: 0,
                current_msg_id: Some(MessageId::new(PLACEHOLDER)),
                response_sent_offset: 0,
                full_response: String::new(),
                tmux_last_offset: None,
                new_session_id: None,
                defer_watcher_resume: false,
                reuse_status_panel_message: false,
                completion_tx: Some(completion),
                is_external_input_tui_direct: false,
                inflight_state: row,
            };
            let (tx, rx) = std::sync::mpsc::channel();
            spawn_turn_bridge(fixture.shared.clone(), fixture.token.clone(), rx, context);
            for text in ["보고 메시지1\n", "진행2\n", "진행3\n", "진행4\n"] {
                tx.send(StreamMessage::Text {
                    content: text.into(),
                })
                .unwrap();
            }
            tx.send(StreamMessage::Done {
                result: terminal.into(),
                session_id: None,
            })
            .unwrap();
            drop(tx);
            let signal = await_within(completed, WAIT_CAP).await.unwrap();
            assert!(matches!(
                signal,
                crate::services::discord::turn_bridge::BridgeCompletionSignal::Finalized
            ));
            let wire = shown.0.lock().unwrap();
            if quiet {
                assert!(wire.is_empty(), "quiet C1 leaked wire body: {wire:?}");
            } else {
                let bodies: Vec<_> = wire
                    .iter()
                    .filter(|(op, _)| matches!(*op, "replace" | "send"))
                    .collect();
                assert_eq!(bodies.len(), 1, "{wire:?}");
                assert_eq!(bodies[0].1, "보고 메시지1\n진행2\n진행3\n진행4\n");
            }
        }
    })
    .await;
}
