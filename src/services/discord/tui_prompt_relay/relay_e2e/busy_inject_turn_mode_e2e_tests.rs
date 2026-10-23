//! The live gateway admission must not park safe turn-mode input on a legacy busy veto.
use super::*;
use crate::services::discord::turn_presence::{
    activity::tests::{BindingRoot, ReadyPane, bind_turn_mode_transcript},
    supervisor::{self, tests::LiveSupervisor},
};
use std::io::Write;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bb_live_rollover_uses_real_intake_and_redelivery_is_not_reinjected_pg() {
    let busy = busy().await;
    let rt = &busy.rt;
    busy.pane.set(InjectPane::TRANSCRIPT, &format!("{}\n{}\n",
        serde_json::json!({"type":"user","uuid":"first-native","message":{"content":"go"}}),
        serde_json::json!({"type":"assistant","uuid":"first-answer","apiBlockIndex":0,"message":{"id":"first-answer","content":[{"type":"text","text":"working"}]}})));
    rt.hold_mailbox().await;
    let events = tempfile::tempdir().unwrap();
    let _events = BindingRoot::enter(events.path());
    let session = busy.pane.session();
    let _ready = ReadyPane::mark(&session);
    let _confirmed = bind_turn_mode_transcript(
        &rt.h.shared,
        &ProviderKind::Claude,
        CHANNEL_ID,
        &session,
        &busy.pane.path(InjectPane::TRANSCRIPT),
    );
    let presence = Arc::new(LiveSupervisor::new(
        rt.h.shared.clone(),
        ProviderKind::Claude,
        CHANNEL_ID,
    ));
    assert!(supervisor::observes(&ProviderKind::Claude, CHANNEL_ID));
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
        serde_json::json!({"type":"assistant","uuid":"next-answer","apiBlockIndex":0,"message":{"id":"next-answer","content":[{"type":"text","text":"working"}]}}),
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
        vec!["⏳".to_string()],
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
    assert_eq!(
        rt.h.shared
            .turn_view_reconciler
            .pending_injected_count(ChannelId::new(CHANNEL_ID)),
        2
    );
    assert!(
        wait_until(WAIT, || {
            let presence = presence.clone();
            Box::pin(async move {
                presence.tick().await;
                presence.state()["activity"] == "busy"
            })
        })
        .await,
        "the real transcript reader must publish Busy before closing the turn: {}",
        presence.state()
    );
    writeln!(
        transcript,
        "{}",
        serde_json::json!({"type":"system","subtype":"turn_duration"})
    )
    .unwrap();
    busy.pane.idle();
    assert!(
        wait_until(WAIT, || {
            let presence = presence.clone();
            let shared = rt.h.shared.clone();
            Box::pin(async move {
                presence.tick().await;
                presence.state()["activity"] == "idle"
                    && [first, second].iter().all(|message| {
                        shared.turn_view_reconciler.ops().iter().any(|op| {
                            op.target.message_id.get() == *message && op.emoji == '✅' && op.add
                        })
                    })
            })
        })
        .await,
        "registered turn-mode intake must complete from the real transcript Idle: {}",
        presence.state()
    );
    for message in [first, second] {
        let changes: Vec<_> =
            rt.h.shared
                .turn_view_reconciler
                .ops()
                .into_iter()
                .filter(|op| op.target.message_id.get() == message)
                .map(|op| (op.emoji, op.add))
                .collect();
        assert_eq!(changes, [('⏳', true), ('⏳', false), ('✅', true)]);
    }
    assert_eq!(
        rt.h.shared
            .turn_view_reconciler
            .pending_injected_count(ChannelId::new(CHANNEL_ID)),
        0
    );
}
