//! Prompt-too-long terminal guidance adapter and delivery regression coverage.

pub(super) fn render_terminal_guidance(existing_guidance: &str) -> String {
    let detail =
        super::super::response_delivery::prompt_too_long_detail_from_guidance(existing_guidance)
            .unwrap_or("provider가 프롬프트가 너무 길다고 보고했습니다.");
    super::super::response_delivery::prompt_too_long_guidance(detail)
}

#[cfg(test)]
mod tests {
    use super::super::cancel_prompt_replace::{
        CancelPromptReplaceContext, CancelPromptReplaceMessage, CancelPromptReplaceState,
        handle_cancel_prompt_replace,
    };
    use super::super::*;
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Mutex;

    type TestGatewayFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

    #[derive(Default)]
    struct CapturingPromptTooLongGateway {
        replacements: Mutex<Vec<String>>,
    }

    impl TurnGateway for CapturingPromptTooLongGateway {
        fn send_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _content: &'a str,
        ) -> TestGatewayFuture<'a, Result<MessageId, String>> {
            panic!("prompt-too-long replacement must not send a new message")
        }

        fn edit_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> TestGatewayFuture<'a, Result<(), String>> {
            panic!("prompt-too-long replacement must use replace_message_with_outcome")
        }

        fn replace_message_with_outcome<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            content: &'a str,
        ) -> TestGatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
            let content = content.to_string();
            Box::pin(async move {
                self.replacements
                    .lock()
                    .expect("replacements lock")
                    .push(content);
                Ok(ReplaceLongMessageOutcome::EditedOriginal)
            })
        }

        fn schedule_retry_with_history<'a>(
            &'a self,
            _channel_id: ChannelId,
            _user_message_id: MessageId,
            _user_text: &'a str,
        ) -> TestGatewayFuture<'a, ()> {
            panic!("prompt-too-long replacement must not schedule a retry")
        }

        fn dispatch_queued_turn<'a>(
            &'a self,
            _channel_id: ChannelId,
            _intervention: &'a Intervention,
            _request_owner_name: &'a str,
            _has_more_queued_turns: bool,
            _dispatch_lease: Option<
                std::sync::Arc<crate::services::turn_orchestrator::DispatchLease>,
            >,
        ) -> TestGatewayFuture<'a, Result<(), String>> {
            panic!("prompt-too-long replacement must not dispatch a queued turn")
        }

        fn validate_live_routing<'a>(
            &'a self,
            _channel_id: ChannelId,
        ) -> TestGatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn requester_mention(&self) -> Option<String> {
            Some("<@4215>".to_string())
        }

        fn can_chain_locally(&self) -> bool {
            true
        }

        fn bot_owner_provider(&self) -> Option<ProviderKind> {
            Some(ProviderKind::Codex)
        }
    }

    #[tokio::test]
    async fn terminal_prompt_too_long_replace_delivers_actionable_folded_guidance() {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let capturing_gateway = Arc::new(CapturingPromptTooLongGateway::default());
        let gateway: Arc<dyn TurnGateway> = capturing_gateway.clone();
        let provider = ProviderKind::Codex;
        let cancel_token = Arc::new(crate::services::provider::CancelToken::new());
        let channel_id = ChannelId::new(4215);
        let current_msg_id = MessageId::new(4216);
        let dispatch_id = None;
        let adk_session_key = None;
        let turn_id = "prompt-too-long-terminal-test".to_string();
        let provider_detail =
            "Error: request failed\nstderr: context_length_exceeded: private provider detail";
        let mut full_response =
            super::super::super::response_delivery::prompt_too_long_guidance(provider_detail);
        let mut active_background_child_session_ids = Vec::new();
        let mut pending_long_running_open_after_state_save = None;
        let mut pending_long_running_retarget_after_state_save = None;
        let mut long_running_placeholder_active = None;
        let mut inflight_state = InflightTurnState::new(
            provider.clone(),
            channel_id.get(),
            Some("prompt-too-long-test".to_string()),
            4215,
            0,
            current_msg_id.get(),
            "oversized prompt".to_string(),
            None,
            None,
            None,
            None,
            0,
        );
        let mut preserve_inflight_for_cleanup_retry = false;
        let mut bridge_skip_holder_owns_inflight = false;
        let mut status_panel_terminal_committed = false;

        handle_cancel_prompt_replace(
            CancelPromptReplaceMessage::PromptTooLong,
            CancelPromptReplaceContext {
                shared_owned: &shared,
                gateway: &gateway,
                provider: &provider,
                cancel_token: &cancel_token,
                channel_id,
                user_msg_id: None,
                current_msg_id,
                dispatch_id: &dispatch_id,
                adk_session_key: &adk_session_key,
                turn_id: &turn_id,
                watcher_owner_channel_id: channel_id,
                tmux_last_offset: None,
                response_sent_offset: 0,
                inflight_generation: 0,
            },
            CancelPromptReplaceState {
                full_response: &mut full_response,
                active_background_child_session_ids: &mut active_background_child_session_ids,
                pending_long_running_open_after_state_save:
                    &mut pending_long_running_open_after_state_save,
                pending_long_running_retarget_after_state_save:
                    &mut pending_long_running_retarget_after_state_save,
                long_running_placeholder_active: &mut long_running_placeholder_active,
                inflight_state: &mut inflight_state,
                preserve_inflight_for_cleanup_retry: &mut preserve_inflight_for_cleanup_retry,
                bridge_skip_holder_owns_inflight: &mut bridge_skip_holder_owns_inflight,
                status_panel_terminal_committed: &mut status_panel_terminal_committed,
            },
        )
        .await;

        let replacements = capturing_gateway
            .replacements
            .lock()
            .expect("replacements lock");
        assert_eq!(replacements.len(), 1);
        let delivered = &replacements[0];
        assert!(delivered.contains("<@4215> ⚠️ 현재 대화가 provider의 컨텍스트 한도를 넘었어요."));
        assert!(delivered.contains("`/compact`"));
        assert!(delivered.contains("요청을 짧게"));
        assert!(delivered.contains("||**상세**\n```text\n__prompt too long__"));
        assert!(delivered.contains(provider_detail));
        assert!(!delivered.contains("다음 메시지를 보내면 자동으로 새 턴이 시작됩니다."));
    }
}
