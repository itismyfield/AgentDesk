//! Turn-cancel observability test that builds discord-layer cleanup policies, moved out of KRX
//! observability.

use crate::services::observability::*;
use serde_json::json;

#[tokio::test]
async fn turn_cancelled_emit_records_normalized_payload_without_pg() {
    let _ = crate::services::observability::events::test_capture::capture_async(
        turn_cancelled_emit_records_normalized_payload_without_pg_scenario(),
    )
    .await;
}

async fn turn_cancelled_emit_records_normalized_payload_without_pg_scenario() {
    let _guard = test_runtime_lock();
    reset_for_tests();
    init_observability(None);

    emit_turn_cancelled(
        Some("Codex"),
        Some(42),
        Some("dispatch-1"),
        Some("session-1"),
        Some("turn-1"),
        turn_lifecycle::TurnCancellationDetails::new(
            " queue-api cancel_turn (preserve) ",
            crate::services::turn_lifecycle::cleanup_policy_observability_surface(
                crate::services::discord::TmuxCleanupPolicy::PreserveSessionAndInflight {
                    restart_mode: crate::services::discord::InflightRestartMode::HotSwapHandoff,
                },
            ),
            "mailbox_canonical",
            false,
            false,
            Some(2),
            Some(true),
            false,
        ),
    );

    let event = crate::services::observability::events::test_capture::one("turn_cancelled");
    assert_eq!(event.channel_id, Some(42));
    assert_eq!(event.provider.as_deref(), Some("codex"));
    assert_eq!(event.payload["reason"], "queue-api cancel_turn (preserve)");
    assert_eq!(event.payload["surface"], "queue_cancel_preserve");
    assert_eq!(event.payload["lifecyclePath"], "mailbox_canonical");
    assert_eq!(event.payload["queueDepth"], 2);
    assert_eq!(event.payload["queuePreserved"], true);
    assert!(event.payload.get("emittedNoOp").is_none());
    assert_eq!(event.payload["dispatch_id"], "dispatch-1");
    assert_eq!(event.payload["session_key"], "session-1");
    assert_eq!(event.payload["turn_id"], "turn-1");
    let _ = json!({});
}
