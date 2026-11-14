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
    assert_eq!(rt.marks(message), ["➕"]);
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
    busy.pane.set("cap.before", "Run this tool? allow / deny");
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
    let pane = busy_inject::Pane::with_program(
        &session,
        crate::services::platform::tmux::session_identity_tests::managed_fixture_program(
            busy.pane.path("tmux"),
        ),
    )
    .expect("managed scripted pane");
    let report = busy_inject::inject_report_any_turn(&pane, &request, None, &busy_inject::TIMING);
    assert!(matches!(
        report.outcome,
        busy_inject::Outcome::NotSent(busy_inject::Veto::Modal)
    ));
    assert!(busy.pane.keys().is_empty());
    let frame = crate::services::tui_input::human_frame::inject(
        request.source,
        request.author,
        request.name,
        request.nonce,
        request.text,
    );
    let expected =
        super::super::super::format_ssh_direct_prompt_notification("claude", &session, &frame);
    submit_hook(&tx, &session, &frame);
    assert!(
        wait_until(WAIT, || {
            let found = rt.notices().contains(&expected);
            Box::pin(async move { found })
        })
        .await,
        "the external frame must be announced: {:?}",
        rt.notices()
    );
    let notices = rt.notices();
    rt.finish().await;
    let matching: Vec<_> = notices
        .iter()
        .filter(|text| text.contains(request.nonce))
        .collect();
    assert_eq!(matching.len(), 1, "one external frame notice: {notices:?}");
    let notice = matching[0];
    assert_eq!(notice, &expected);
    assert_eq!(
        notice,
        &format!("-# ⌨️ tui direct input\n```\n{frame}\n```")
    );
}

/// One hook prompt whose own transcript row is a scheduled fire or a typed input, on a channel
/// whose turns are transcript-driven or not; returns the one notice that names `canary`.
async fn announce_hook_prompt(turn_mode: bool, scheduled: bool, canary: &str) -> String {
    let busy = idle().await;
    let rt = &busy.rt;
    let _confirmed = turn_mode.then(|| TestConfirmation::new(CHANNEL_ID));
    assert_eq!(
        crate::services::tui_o::turn_mode::transcript_turns(CHANNEL_ID),
        turn_mode
    );
    let tx = observer(rt);
    let session = busy.pane.session();
    let transcript = busy.pane.path(InjectPane::TRANSCRIPT);
    let prompt_id = format!("i8142-fire-{}", fresh_id());
    let text = format!("{canary}\nsecond line");
    let row = if scheduled {
        // The fire's own row, with the marks Claude Code 2.1.325 writes on it.
        serde_json::json!({"type":"user", "isMeta":true, "promptId":prompt_id,
            "uuid":format!("{prompt_id}-row"), "promptSource":"system",
            "wakeupSource":"schedule_wakeup", "scheduledTaskId":"abbf09df",
            "turnOrigin":"scheduled", "message":{"role":"user","content":text}})
    } else {
        serde_json::json!({"type":"user", "promptId":prompt_id,
            "uuid":format!("{prompt_id}-row"), "turnOrigin":"human",
            "message":{"role":"user","content":text}})
    };
    busy.pane.set(
        InjectPane::TRANSCRIPT,
        &format!("{{\"type\":\"system\",\"subtype\":\"turn_duration\"}}\n{row}\n"),
    );
    tx.send(HookEvent {
        original_context: None,
        provider: "claude".into(),
        session_id: session,
        kind: HookEventKind::UserPromptSubmit,
        received_at: chrono::Utc::now(),
        payload: serde_json::json!({"prompt":text, "prompt_id":prompt_id,
            "transcript_path":transcript}),
        fanout: None,
    })
    .unwrap();
    assert!(
        wait_until(WAIT, || {
            let found = rt.notices().iter().any(|notice| notice.contains(canary));
            Box::pin(async move { found })
        })
        .await,
        "the hook prompt must be announced: {:?}",
        rt.notices()
    );
    let notices = rt.notices();
    rt.finish().await;
    let matching: Vec<_> = notices
        .iter()
        .filter(|notice| notice.contains(canary))
        .collect();
    assert_eq!(matching.len(), 1, "one notice: {notices:?}");
    matching[0].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_fire_hook_is_announced_under_the_scheduled_header_pg() {
    assert_eq!(
        announce_hook_prompt(true, true, "i8142 turn-mode fire").await,
        "-# ⏰ scheduled prompt\n```\ni8142 turn-mode fire\nsecond line\n```"
    );
}

/// A channel whose turns are not transcript-driven posts from the relay's own turn start.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scheduled_fire_hook_without_transcript_turns_keeps_the_scheduled_header_pg() {
    assert_eq!(
        announce_hook_prompt(false, true, "i8142 relay-turn fire").await,
        "-# ⏰ scheduled prompt\n```\ni8142 relay-turn fire\nsecond line\n```"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_hook_prompt_without_transcript_turns_keeps_the_typed_header_pg() {
    assert_eq!(
        announce_hook_prompt(false, false, "i8142 relay-turn typed").await,
        "-# ⌨️ tui direct input\n```\ni8142 relay-turn typed\nsecond line\n```"
    );
}
