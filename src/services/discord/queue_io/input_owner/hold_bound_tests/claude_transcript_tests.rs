use super::{
    BOUND, CLAUDE_IDLE, Capture, ClaudeSource, LOOK, OLD_MESSAGE, RETRY, Rig, SAMPLES,
    WitnessSample, step,
};
use crate::config::TestRuntimeRootGuard;
use crate::services::claude_tui::readiness::{self, tests::Rig as TranscriptRig};
use crate::services::discord::turn_completion_events;
use crate::services::provider::ProviderKind;
use crate::services::tui_turn_state::{self, ToolCalls, TuiTurnState};
use serde_json::json;

fn completed_transcript(rig: &TranscriptRig, subsequent: &str) {
    let mut text = String::new();
    for mut record in [
        json!({"type":"user","message":{"content":"hi"}}),
        json!({"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}),
        json!({"type":"result","result":"done"}),
        json!({"type":"mode","mode":"default"}),
        json!({"type":"permission-mode","mode":"default"}),
        json!({"type":"pr-link","url":"https://example.com/pr/1"}),
    ] {
        record["sessionId"] = json!(rig.ctx.expected_native_session_id);
        text.push_str(&record.to_string());
        text.push('\n');
    }
    text.push_str(subsequent);
    std::fs::write(&rig.path, text).unwrap();
}

fn capture(channel: u64, transcript: &TranscriptRig) -> Capture {
    let capture = Capture::new(channel, WitnessSample::at("herdr", CLAUDE_IDLE));
    SAMPLES
        .lock()
        .unwrap()
        .get_mut(&channel)
        .unwrap()
        .claude_source = Some(ClaudeSource {
        session: transcript.session.clone(),
        path: transcript.path.clone(),
    });
    capture
}

#[tokio::test(start_paused = true)]
async fn hold_bound_target_incident_claude_post_completion_metadata_releases_once() {
    let _root = TestRuntimeRootGuard::with_isolated_config();
    let transcript = TranscriptRig::new();
    completed_transcript(&transcript, "");
    assert_eq!(
        tui_turn_state::observe_claude_jsonl_running_turn_state(&transcript.path),
        TuiTurnState::Unknown
    );
    assert_eq!(
        tui_turn_state::provider_tool_calls(&ProviderKind::Claude, &transcript.path),
        ToolCalls::Closed
    );
    assert!(tui_turn_state::jsonl_completion_scan_idle(
        &ProviderKind::Claude,
        &transcript.path
    ));
    assert!(
        transcript.ready(),
        "the binding has a current readiness proof"
    );

    let rig = Rig::open(84_343, ProviderKind::Claude, None, true).await;
    let capture = capture(rig.channel.get(), &transcript);
    rig.lease(Some(OLD_MESSAGE)).await;
    let mut events = turn_completion_events::subscribe_turn_completion_events(&rig.shared);
    rig.hold().await;

    step(BOUND).await;
    rig.kept().await;
    assert_eq!(capture.reads(), 1);
    step(LOOK).await;
    rig.kept().await;
    assert_eq!(capture.reads(), 2);
    step(LOOK).await;
    rig.accept(&mut events).await;
    assert_eq!(capture.reads(), 3);
    assert!(events.try_recv().is_err());
}

#[tokio::test(start_paused = true)]
async fn hold_bound_target_flicker_claude_post_completion_metadata_requires_current_end() {
    let _root = TestRuntimeRootGuard::with_isolated_config();
    let cases = [
        (
            "user",
            "{\"type\":\"user\",\"message\":{\"content\":\"new prompt\"}}\n",
        ),
        (
            "open tool",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"toolu_fixture\",\"name\":\"Read\"}]}}\n",
        ),
        (
            "streaming",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"not done\"}]}}\n",
        ),
        (
            "new permission boundary",
            "{\"type\":\"permission-mode\",\"mode\":\"default\"}\n",
        ),
        ("partial mode", "{\"type\":\"mode\",\"mode\":"),
        ("resume before new turn", ""),
    ];
    for (index, (case, suffix)) in cases.into_iter().enumerate() {
        let transcript = TranscriptRig::new();
        completed_transcript(&transcript, suffix);
        if case == "resume before new turn" {
            readiness::note_hook(
                "claude",
                &crate::services::claude_tui::hook_server::HookEventKind::SessionStart,
                &transcript.session,
                &json!({
                    "session_id": transcript.ctx.expected_native_session_id,
                    "transcript_path": transcript.path,
                    "source": "resume"
                }),
                Some(&transcript.ctx),
                Some(chrono::Utc::now()),
                false,
            );
            assert!(
                transcript.ready(),
                "resume admits input without a new completion"
            );
        }
        let rig = Rig::open(84_344 + index as u64, ProviderKind::Claude, None, true).await;
        let capture = capture(rig.channel.get(), &transcript);
        rig.lease(Some(OLD_MESSAGE)).await;
        let mut events = turn_completion_events::subscribe_turn_completion_events(&rig.shared);
        rig.hold().await;

        step(BOUND).await;
        step(RETRY).await;
        step(RETRY).await;
        rig.kept().await;
        assert_eq!(capture.reads(), 3, "{case}");
        assert!(events.try_recv().is_err(), "{case}");
    }
}
