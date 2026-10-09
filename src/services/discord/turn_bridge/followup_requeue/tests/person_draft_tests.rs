//! A queued or follow-up turn meeting text in a Claude composer it did not type.

use super::*;

/// The hosted warm follow-up entry a queued turn takes, over a scripted pane; returns its
/// result and the pane writes.
#[cfg(unix)]
fn warm_follow_up(
    session: &str,
    pane: &str,
    idle: &std::path::Path,
    prompt: &str,
) -> (Result<(), String>, Vec<String>) {
    use crate::services::claude_tui::host_input::{SpyGuard, SpyState};
    use crate::services::claude_tui::hosting::{
        ClaudeTuiWarmFollowupOutcome, FollowupHost, try_claude_tui_warm_followup,
    };
    let captures = std::iter::repeat_n(pane, 12)
        .chain(["\u{2733} Architecting\u{2026}"])
        .map(|capture| Some(capture.to_string()))
        .collect();
    // A submit that reaches Enter is cancelled there, so the transcript read does not wait.
    let token = Arc::new(crate::services::provider::CancelToken::new());
    let spy = SpyGuard::install(SpyState {
        captures,
        cancel_on: Some(("keys:Enter", 1, token.clone())),
        ..SpyState::default()
    });
    let (sender, _stream) = std::sync::mpsc::channel();
    let path = idle.display().to_string();
    let host = FollowupHost::legacy_tmux(session);
    let dir = idle.parent().expect("transcript dir");
    let outcome = try_claude_tui_warm_followup(
        "s".to_string(),
        idle.to_path_buf(),
        path,
        true,
        dir,
        prompt,
        sender,
        Some(token),
        &host,
        None,
    );
    let ended = match outcome {
        ClaudeTuiWarmFollowupOutcome::Terminal(result) => result,
        ClaudeTuiWarmFollowupOutcome::Recreate(_) => Err("recreate".to_string()),
    };
    let write = |call: &&String| {
        ["keys:", "literal:", "load:", "paste:", "retire:"]
            .iter()
            .any(|k| call.starts_with(k))
    };
    (ended, spy.calls().iter().filter(write).cloned().collect())
}

/// A queued follow-up meeting a person's unsent draft on an idle pane types nothing, waits in
/// the queue with the draft left alone, and is sent once after the person's draft is gone.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn a_queued_follow_up_waits_behind_a_person_draft_and_is_sent_once_after_it() {
    use crate::services::claude_tui::composer_lock::draft_guarded;
    use crate::services::claude_tui::host_input::FakeDraftPane;
    let _root = scoped_runtime_root();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel_id = ChannelId::new(100_000_006_714_001);
    let message_id = MessageId::new(100_000_006_714_002);
    let session = format!("person-draft-{}", uuid::Uuid::new_v4().simple());
    let dir = tempfile::tempdir().expect("transcript dir");
    let idle = dir.path().join("idle.jsonl");
    let turn = r#"{"type":"system","subtype":"turn_duration","sessionId":"s"}"#;
    std::fs::write(&idle, format!("{turn}\n")).expect("idle transcript");
    // A person typed into the idle composer without Enter; the placeholder is drawn faint.
    let drafted = idle_pane_with("", "\x1b[39m\u{276f}\u{a0}D2E3 draft typed by a person");
    let placeholder = "\x1b[39m\u{276f}\u{a0}\x1b[2mTry \"refactor <filepath>\"\x1b[0m";
    let pane = FakeDraftPane::new(&session);
    pane.show(&drafted);

    // Its second row says "running", which made the plain reader call it a stranded prompt.
    let rows = "\x1b[39m\u{276f}\u{a0}Review this task\n  The job keeps running forever";
    let two_rows = idle_pane_with("", rows);
    let other = format!("person-draft-rows-{}", uuid::Uuid::new_v4().simple());
    let (refused, writes) = warm_follow_up(&other, &two_rows, &idle, "queued follow-up");
    assert!(refused.is_err() && writes.is_empty(), "{writes:?}");
    assert!(draft_guarded(&other));

    let (refused, writes) = warm_follow_up(&session, &drafted, &idle, "queued follow-up");
    let error = refused.expect_err("a person's draft takes no follow-up");
    assert_eq!(writes, Vec::<String>::new());
    assert!(draft_guarded(&session));
    let classification =
        super::super::super::streaming_edit_text::classify_raw_tui_error(&provider, &error);
    let runtime = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
    let full_response = format!("Error: {error}");
    let base =
        super::super::super::streaming_edit_text::bridge_claude_tui_followup_busy_readiness_timeout(
            &provider,
            runtime,
            classification,
        ) || bridge_claude_tui_followup_requeue_prompt_error(
            &provider,
            runtime,
            &full_response,
            classification,
        );
    assert!(
        claude_tui_followup_requeue_streaming_aware(base, false),
        "{error}"
    );

    let mut state = inflight(channel_id, message_id);
    state.tmux_session_name = Some(session.clone());
    let outcome = requeue_claude_tui_followup_pre_submit_timeout(
        &shared,
        &provider,
        channel_id,
        &state,
        None,
        None,
        "turn-6714",
    )
    .await;
    assert!(outcome.requeued && !outcome.retry_capped);
    let queued = || async {
        let snapshot = crate::services::discord::mailbox_snapshot(&shared, channel_id).await;
        let entries = snapshot.intervention_queue.iter();
        entries
            .map(|entry| (entry.message_id, entry.text.clone()))
            .collect::<Vec<_>>()
    };
    let waiting = vec![(message_id, "queued follow-up".to_string())];
    assert_eq!(queued().await, waiting);

    let (sent, _hook) = dispatch_on_kickoff(
        channel_id,
        session.clone(),
        idle_pane_with("", placeholder),
        idle,
    );
    let poll = DRAFT_RELEASE_POLL + std::time::Duration::from_millis(1);
    for _ in 0..3 {
        tokio::time::sleep(poll).await;
        assert!(draft_guarded(&session));
        assert!(sent.lock().unwrap().is_empty());
    }
    assert_eq!(queued().await, waiting);
    // The person sends the draft; the composer is back to the faint placeholder.
    pane.show(&idle_pane_with("", placeholder));
    tokio::time::sleep(std::time::Duration::from_secs(60 * 10)).await;
    assert!(!draft_guarded(&session));
    let submit = vec![
        "literal:queued follow-up".to_string(),
        "keys:Enter".to_string(),
    ];
    assert_eq!(*sent.lock().unwrap(), vec![(message_id, submit)]);
    assert_eq!(queued().await, Vec::new());
    crate::services::tui_prompt_dedupe::remove_discord_originated_prompt(
        "claude",
        &session,
        "queued follow-up",
    );
}

/// A composer the submit cannot read, or typed text outside the measured layout, holds only
/// this input: nothing is typed and the pane stays unprotected.
#[test]
fn an_unread_composer_holds_the_follow_up_without_protecting_the_pane() {
    use crate::services::claude_tui::composer_lock::draft_guarded;
    use crate::services::claude_tui::host_input::{SpyGuard, SpyState};
    let dir = tempfile::tempdir().expect("transcript dir");
    let idle = dir.path().join("idle.jsonl");
    let turn = r#"{"type":"system","subtype":"turn_duration","sessionId":"s"}"#;
    std::fs::write(&idle, format!("{turn}\n")).expect("idle transcript");
    let ready = idle_pane("");
    let unmeasured = format!("{ready}\u{276f} typed below an unmeasured footer\n");
    let border = "\u{2500}".repeat(60);
    let unclosed = format!("\u{23fa} Done.\n\n{border}\n\u{276f} \n  typed on the second row\n");
    let cases = [
        ("capture failed", None),
        ("unmeasured", Some(unmeasured)),
        ("no closing border", Some(unclosed)),
    ];
    for (name, draft_capture) in cases {
        let session = format!("unread-{}", uuid::Uuid::new_v4().simple());
        let captures = [Some(ready.clone()), Some(ready.clone()), draft_capture];
        let spy = SpyGuard::install(SpyState {
            captures: captures.into_iter().collect(),
            ..SpyState::default()
        });
        let submitted = crate::services::claude_tui::input::send_followup_prompt_or_idle_transcript(
            &session,
            "queued follow-up",
            None,
            &idle,
        );
        let error = submitted.expect_err(name);
        assert!(error.contains("reason=composer_unread"), "{name}: {error}");
        assert!(
            crate::services::claude_tui::input::is_prompt_ready_timeout_error(&error),
            "{name}"
        );
        let writes = ["keys:", "literal:", "load:", "paste:"];
        let calls = spy.calls();
        assert!(
            !calls
                .iter()
                .any(|c| writes.iter().any(|w| c.starts_with(w))),
            "{name}"
        );
        assert!(!draft_guarded(&session), "{name}");
    }
}
