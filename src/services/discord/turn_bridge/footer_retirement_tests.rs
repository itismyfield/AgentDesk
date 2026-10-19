use super::*;
use crate::services::discord::footer_retirement::{legacy_for_test, retired};
use crate::services::discord::formatting::{self, ReplaceLongMessageOutcome};
use crate::services::discord::gateway::GatewayFuture;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Gateway {
    sent: Mutex<Vec<String>>,
    edits: Mutex<Vec<(MessageId, String)>>,
    missing: bool,
}

impl TurnGateway for Gateway {
    fn send_message<'a>(
        &'a self,
        _: ChannelId,
        text: &'a str,
    ) -> GatewayFuture<'a, Result<MessageId, String>> {
        self.sent.lock().unwrap().push(text.into());
        Box::pin(async { Ok(MessageId::new(900)) })
    }
    fn edit_message<'a>(
        &'a self,
        _: ChannelId,
        id: MessageId,
        text: &'a str,
    ) -> GatewayFuture<'a, Result<(), String>> {
        self.edits.lock().unwrap().push((id, text.into()));
        Box::pin(async move {
            if self.missing {
                Err("404 Unknown Message".into())
            } else {
                Ok(())
            }
        })
    }
    fn replace_message_with_outcome<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        _: &'a str,
    ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
        Box::pin(async { panic!("retirement never replaces a body") })
    }
    fn schedule_retry_with_history<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
        _: &'a str,
    ) -> GatewayFuture<'a, ()> {
        Box::pin(async {})
    }
    fn dispatch_queued_turn<'a>(
        &'a self,
        _: ChannelId,
        _: &'a crate::services::discord::Intervention,
        _: &'a str,
        _: bool,
        _: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
    ) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn validate_live_routing<'a>(&'a self, _: ChannelId) -> GatewayFuture<'a, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn requester_mention(&self) -> Option<String> {
        None
    }
    fn can_chain_locally(&self) -> bool {
        true
    }
    fn bot_owner_provider(&self) -> Option<ProviderKind> {
        Some(ProviderKind::Claude)
    }
}

fn shared() -> Arc<SharedData> {
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared)
        .unwrap()
        .ui
        .status_panel_v2_enabled = true;
    shared
}

fn state(channel: u64) -> InflightTurnState {
    InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        None,
        1,
        41,
        42,
        "turn".into(),
        None,
        None,
        None,
        None,
        0,
    )
}

#[tokio::test]
async fn retired_bridge_creates_no_panel_in_either_layout() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    assert!(retired());
    let shared = shared();
    let gateway = Gateway::default();
    for two_message in [false, true] {
        for footer in [false, true] {
            let mut current = MessageId::new(42);
            let mut panel = None;
            let mut placeholder = None;
            let mut last_edit = String::new();
            let mut state = state(8_071_810);
            let mut generation = 0;
            let mut dirty = true;
            super::super::single_message_footer::maybe_create_bridge_separate_status_panel_response(
                two_message, footer, true, &gateway, ChannelId::new(state.channel_id), "⠸", &mut current, &mut panel,
                &mut placeholder, &mut last_edit, &mut state, &mut generation, 0, "body", &mut dirty, &shared, &ProviderKind::Claude,
            ).await;
            assert_eq!(current, MessageId::new(42));
            assert!(panel.is_none() && placeholder.is_none() && last_edit.is_empty());
            assert_eq!(generation, 0);
            assert!(dirty);
        }
    }
    assert!(gateway.sent.lock().unwrap().is_empty());
    assert!(
        !super::super::two_message_panel::two_message_should_reanchor_panel_on_rollover(true, true)
    );
}

#[tokio::test]
async fn retired_completion_without_panel_or_after_404_never_posts_fallback() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    for panel in [None, Some(MessageId::new(43))] {
        let gateway = Gateway {
            missing: true,
            ..Default::default()
        };
        let mut last = String::new();
        assert!(
            complete_status_panel_v2(
                &shared(),
                &gateway,
                ChannelId::new(8_071_811),
                panel,
                &ProviderKind::Claude,
                1_700_000_000,
                &mut last,
                false,
                false,
                "retirement_test",
                41,
                true,
                false
            )
            .await
        );
        assert!(gateway.sent.lock().unwrap().is_empty());
        assert_eq!(
            gateway.edits.lock().unwrap().len(),
            usize::from(panel.is_some())
        );
        assert!(last.is_empty(), "an absent panel is not falsely committed");
    }
}

#[tokio::test]
async fn retired_upgrade_panels_complete_by_edit_for_success_and_failure() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    assert!(
        super::super::single_message_footer::bridge_should_complete_separate_status_panel(true)
    );
    for failure in [false, true] {
        let gateway = Gateway::default();
        let mut last = String::new();
        assert!(
            complete_status_panel_v2(
                &shared(),
                &gateway,
                ChannelId::new(8_071_812 + u64::from(failure)),
                Some(MessageId::new(43)),
                &ProviderKind::Claude,
                1_700_000_000,
                &mut last,
                false,
                false,
                "retirement_upgrade",
                41,
                true,
                failure
            )
            .await
        );
        let edits = gateway.edits.lock().unwrap();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].0, MessageId::new(43));
        assert!(edits[0].1.contains(if failure {
            "⚠️ 실패"
        } else {
            "✅ 완료"
        }));
        assert_eq!(last, edits[0].1);
        assert!(gateway.sent.lock().unwrap().is_empty());
    }
    let gateway = Gateway::default();
    assert!(
        complete_status_panel_v2(
            &shared(),
            &gateway,
            ChannelId::new(8_071_814),
            None,
            &ProviderKind::Claude,
            0,
            &mut String::new(),
            false,
            false,
            "retirement_failure",
            41,
            true,
            true
        )
        .await
    );
    assert!(gateway.sent.lock().unwrap().is_empty() && gateway.edits.lock().unwrap().is_empty());
}

#[test]
fn retired_bridge_builder_preserves_actual_footer_budget_input() {
    use crate::services::discord::placeholder_live_events::footer_activity_tests::{
        START, six_tool_fixture,
    };
    let channel = ChannelId::new(8_071_815);
    let shared = six_tool_fixture(crate::config::ToolActivityDisplay::Summary, channel);
    assert!(
        crate::services::discord::single_message_panel_enabled(),
        "fixture exercises default-on footer host"
    );
    for unit in ["x", "한", "```rust\nlet x = 1;\n```\n\n"] {
        for len in [0, 1, 100, 1700, 3000, 6000] {
            let body = unit.repeat(len);
            let builder = || {
                super::super::single_message_footer::build_bridge_single_message_panel_status_block(
                    &shared,
                    channel,
                    &ProviderKind::Claude,
                    START,
                    "⠸",
                    None,
                    None,
                    &body,
                )
            };
            let (old_block, old_text, old_plan) = {
                let _legacy = legacy_for_test();
                let block = builder();
                let text = formatting::build_status_panel_streaming_edit_text(
                    &body,
                    &block,
                    &ProviderKind::Claude,
                );
                let plan = formatting::plan_streaming_rollover(&body, &block);
                (block, text, plan)
            };
            let block = builder();
            assert_eq!(
                block, old_block,
                "budget input must retain the legacy panel, not a spinner"
            );
            assert!(block.len() > formatting::build_processing_status_block("⠸").len());
            if body.len() <= 100 {
                assert!(block.contains("fixture0"));
            }
            let plan = formatting::plan_streaming_rollover(&body, &block);
            assert_eq!(
                plan.as_ref().map(|p| (&p.frozen_chunk, p.split_at)),
                old_plan.as_ref().map(|p| (&p.frozen_chunk, p.split_at))
            );
            let text = formatting::build_status_panel_streaming_edit_text(
                &body,
                &block,
                &ProviderKind::Claude,
            );
            assert_eq!(
                text,
                if body.is_empty() {
                    "⠋ 계속 처리 중"
                } else {
                    old_text.strip_suffix(&format!("\n\n{block}")).unwrap()
                }
            );
        }
    }
}
