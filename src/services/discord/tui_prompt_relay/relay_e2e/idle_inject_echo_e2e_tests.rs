use super::*;
use crate::services::claude_tui::busy_inject;
use crate::services::claude_tui::hook_server::{HookEvent, HookEventKind};
use crate::services::tui_o::turn_mode::TestConfirmation;

async fn idle() -> Busy {
    let busy = busy_with_announce().await;
    let rt = &busy.rt;
    busy.pane.idle();
    busy.pane.drop_row(&rt.h.shared).await;
    busy.pane.set(
        InjectPane::TRANSCRIPT,
        "{\"type\":\"system\",\"subtype\":\"turn_duration\"}\n",
    );
    busy.pane.set(
        "receipt",
        "{\"type\":\"user\",\"uuid\":\"idle-receipt\",\"message\":{\"content\":\"@BODY@\"}}",
    );
    rt.h.shared.tmux_watchers.insert(
        ChannelId::new(CHANNEL_ID),
        super::super::watcher_handle(
            &busy.pane.session(),
            &busy.pane.path(InjectPane::TRANSCRIPT),
        ),
    );
    rt.h.use_mock_provider_bot(WAIT).await;
    busy
}

fn observer(rt: &Runtime) -> tokio::sync::broadcast::Sender<HookEvent> {
    let (tx, rx) = tokio::sync::broadcast::channel(16);
    let shared = rt.h.shared.clone();
    super::super::super::hook_observer::spawn_tui_prompt_relay_observer(
        "claude".into(),
        rx,
        move |prompt| {
            let shared = shared.clone();
            Box::pin(
                async move { super::super::super::relay_observed_prompt(&shared, prompt).await },
            )
        },
    );
    tx
}

fn submit_hook(tx: &tokio::sync::broadcast::Sender<HookEvent>, session: &str, text: &str) {
    tx.send(HookEvent {
        original_context: None,
        provider: "claude".into(),
        session_id: session.into(),
        kind: HookEventKind::UserPromptSubmit,
        received_at: chrono::Utc::now(),
        payload: serde_json::json!({"prompt":text}),
        fanout: None,
    })
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r1_idle_inject_has_no_direct_terminal_notice_pg() {
    let busy = idle().await;
    let rt = &busy.rt;
    let _confirmed = TestConfirmation::new(CHANNEL_ID);
    let tx = observer(rt);
    let message = fresh_id();
    rt.h.deliver_user_message(message, "idle injected question")
        .await
        .unwrap();
    assert_eq!(busy.pane.keys(), ["paste-buffer", "send-keys"]);
    assert_eq!(rt.marks(message), ["📥"]);
    let frame = busy.pane.read("buffer").unwrap();
    let receipts = busy.pane.read(InjectPane::TRANSCRIPT).unwrap();
    assert_eq!(
        receipts
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|record| record["type"] == "user" && record["message"]["content"] == frame)
            .count(),
        1
    );
    submit_hook(&tx, &busy.pane.session(), &frame);
    // The later external notice is a FIFO relay barrier, not a sleep-based zero assertion.
    submit_hook(&tx, &busy.pane.session(), "external terminal canary");
    assert!(
        wait_until(WAIT, || {
            let found = rt
                .notices()
                .iter()
                .any(|text| text.contains("external terminal canary"));
            Box::pin(async move { found })
        })
        .await,
        "external notice must pass the same observer and relay"
    );
    let notices = rt.notices();
    rt.finish().await;
    assert_eq!(
        notices
            .iter()
            .filter(|text| text.contains("external terminal canary"))
            .count(),
        1
    );
    assert_eq!(
        notices
            .iter()
            .filter(|text| text.contains("idle injected question"))
            .count(),
        0,
        "our live gateway submission must not be announced as terminal input: {notices:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn r1_unsent_full_frame_keeps_external_direct_notice_pg() {
    let busy = idle().await;
    let rt = &busy.rt;
    let _confirmed = TestConfirmation::new(CHANNEL_ID);
    let tx = observer(rt);
    busy.pane.draft();
    let session = busy.pane.session();
    let transcript = busy.pane.path(InjectPane::TRANSCRIPT);
    let request = busy_inject::Request {
        session: &session,
        transcript: &transcript,
        source: "discord",
        author: "200",
        name: "ann",
        nonce: "f00d7108",
        text: "typed externally after refusal",
    };
    let pane = busy_inject::Pane::with_program(&session, busy.pane.path("tmux"));
    let report = busy_inject::inject_report_any_turn(&pane, &request, None, &busy_inject::TIMING);
    assert!(matches!(
        report.outcome,
        busy_inject::Outcome::NotSent(busy_inject::Veto::Draft)
    ));
    assert!(busy.pane.keys().is_empty());
    let frame = crate::services::tui_input::human_frame::inject(
        request.source,
        request.author,
        request.name,
        request.nonce,
        request.text,
    );
    submit_hook(&tx, &session, &frame);
    assert!(
        wait_until(WAIT, || {
            let found = rt.notices().iter().any(|text| text.contains(request.text));
            Box::pin(async move { found })
        })
        .await
    );
    let notices = rt.notices();
    rt.finish().await;
    assert_eq!(
        notices
            .iter()
            .filter(|text| text.contains(request.text))
            .count(),
        1
    );
}
