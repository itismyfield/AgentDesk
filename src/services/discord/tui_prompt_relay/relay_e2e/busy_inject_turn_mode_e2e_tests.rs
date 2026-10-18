//! The live gateway admission must not park safe turn-mode input on a legacy busy veto.
use super::*;
use crate::services::tui_o::turn_mode::TestConfirmation;
use std::io::Write;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bb_live_rollover_uses_real_intake_and_redelivery_is_not_reinjected_pg() {
    let busy = busy().await;
    let rt = &busy.rt;
    rt.hold_mailbox().await;
    let _confirmed = TestConfirmation::new(CHANNEL_ID);
    let first = fresh_id();
    rt.h.deliver_user_message(first, "first input")
        .await
        .unwrap();
    assert_eq!(busy.pane.keys(), ["paste-buffer", "send-keys"]);
    let frame = busy.pane.read("buffer").unwrap();
    let mut transcript = std::fs::OpenOptions::new()
        .append(true)
        .open(busy.pane.path(InjectPane::TRANSCRIPT))
        .unwrap();
    for row in [
        serde_json::json!({"type":"system","subtype":"turn_duration"}),
        serde_json::json!({"type":"user","uuid":"next-native","message":{"content":frame}}),
        serde_json::json!({"type":"assistant","uuid":"next-answer","message":{"content":[{"type":"text","text":"working"}]}}),
        serde_json::json!({"type":"permission-mode","mode":"bypassPermissions"}),
    ] {
        writeln!(transcript, "{row}").unwrap();
    }
    std::fs::remove_file(busy.pane.path("pasted")).unwrap();
    assert!(
        !crate::services::tui_turn_state::observe_claude_jsonl_turn_state(
            &busy.pane.path(InjectPane::TRANSCRIPT)
        )
        .is_busy()
    );
    let second = fresh_id();
    rt.h.deliver_user_message(second, "next Discord input")
        .await
        .unwrap();
    let observed = (
        hook::seen(second).outcomes,
        rt.queue().await,
        busy.pane.keys(),
        rt.marks(second),
        rt.h.checkpoint(),
        rt.starts(),
        on_disk(second),
    );
    rt.h.deliver_user_message(second, "next Discord input")
        .await
        .unwrap();
    let keys_after_redelivery = busy.pane.keys();
    rt.finish().await;
    let taken = (
        vec!["Injected { turn_id: None }".to_string()],
        vec![],
        vec![
            "paste-buffer".to_string(),
            "send-keys".to_string(),
            "paste-buffer".to_string(),
            "send-keys".to_string(),
        ],
        vec!["📥".to_string()],
        Some(second),
        0,
        Some("\"observed\"".to_string()),
    );
    assert_eq!(
        observed, taken,
        "production live intake must not fall back to its queue"
    );
    assert_eq!(
        keys_after_redelivery, observed.2,
        "the source terminal record prevents a second paste"
    );
}
