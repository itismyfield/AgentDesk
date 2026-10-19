use super::*;
use crate::services::discord::footer_retirement;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct NoWrites {
    probes: AtomicUsize,
    sends: AtomicUsize,
    deletes: AtomicUsize,
}

impl RecoveryPanelGateway for NoWrites {
    fn probe_panel<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
    ) -> RecoveryPanelFuture<'a, PersistedPanelState> {
        self.probes.fetch_add(1, Ordering::Relaxed);
        Box::pin(async { PersistedPanelState::Missing })
    }
    fn send_panel<'a>(
        &'a self,
        _: ChannelId,
        _: &'a str,
    ) -> RecoveryPanelFuture<'a, Result<MessageId, String>> {
        self.sends.fetch_add(1, Ordering::Relaxed);
        Box::pin(async { Ok(MessageId::new(900)) })
    }
    fn delete_panel<'a>(
        &'a self,
        _: ChannelId,
        _: MessageId,
    ) -> RecoveryPanelFuture<'a, Result<(), String>> {
        self.deletes.fetch_add(1, Ordering::Relaxed);
        Box::pin(async { Ok(()) })
    }
}

fn state() -> inflight::InflightTurnState {
    inflight::InflightTurnState::new(
        ProviderKind::Claude,
        8_071_817,
        None,
        1,
        41,
        300,
        "turn".into(),
        None,
        None,
        None,
        None,
        0,
    )
}

#[tokio::test]
async fn retired_recovery_does_not_probe_recreate_or_reanchor_panels() {
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    assert!(footer_retirement::retired());
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    let ui = &mut Arc::get_mut(&mut shared).unwrap().ui;
    ui.status_panel_v2_enabled = true;
    ui.two_message_panel_enabled = true;
    let gateway = NoWrites::default();
    for panel in [None, Some(200), Some(400)] {
        let mut state = state();
        state.status_message_id = panel;
        let before = serde_json::to_value(&state).unwrap();
        assert!(
            !recover_two_message_panel_with_gateway(
                &gateway,
                &shared,
                &ProviderKind::Claude,
                &mut state
            )
            .await
        );
        assert_eq!(serde_json::to_value(&state).unwrap(), before);
    }
    assert_eq!(gateway.probes.load(Ordering::Relaxed), 0);
    assert_eq!(gateway.sends.load(Ordering::Relaxed), 0);
    assert_eq!(gateway.deletes.load(Ordering::Relaxed), 0);
}

#[test]
fn retired_recovery_completion_keeps_only_an_existing_same_turn_panel() {
    let mut original = state();
    for single in [false, true] {
        assert_eq!(
            super::super::recovery_status_panel::completion_target_for_flags(
                single, true, &original, None
            ),
            None
        );
        original.status_message_id = Some(200);
        assert_eq!(
            super::super::recovery_status_panel::completion_target_for_flags(
                single, true, &original, None
            ),
            Some(Some(MessageId::new(200)))
        );
        assert_eq!(
            super::super::recovery_status_panel::completion_target_for_flags(
                single, false, &original, None
            ),
            None
        );
        let mut persisted = original.clone();
        persisted.status_message_id = Some(400);
        assert_eq!(
            super::super::recovery_status_panel::completion_target_for_flags(
                single,
                true,
                &original,
                Some(&persisted)
            ),
            Some(Some(MessageId::new(400)))
        );
        persisted.user_msg_id += 1;
        assert_eq!(
            super::super::recovery_status_panel::completion_target_for_flags(
                single,
                true,
                &original,
                Some(&persisted)
            ),
            Some(Some(MessageId::new(200)))
        );
        original.status_message_id = None;
    }
}
