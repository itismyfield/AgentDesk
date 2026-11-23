//! The cold Claude retry fixture owner tests share: a KE mailbox drives the KRX TUI leaf.
use crate::services::claude_tui::input::submission::tests::{frame_renderer, screen, wait};

/// Sends a bootstrap retry through the actual cold Claude leaf and reads the renderer's receipts.
pub(crate) async fn owner_retry_leaf(
    mailbox: &crate::services::turn_orchestrator::ChannelMailboxHandle,
    nonce: &str,
) -> (serde_json::Value, Vec<String>) {
    use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    let fixture = Fixture::after_shared_test_env_lock();
    let session = format!("retry-{}", uuid::Uuid::new_v4().simple());
    let execution = uuid::Uuid::new_v4().simple().to_string();
    let frame = format!("frame-{nonce}");
    frame_renderer(
        &fixture,
        &session,
        &frame,
        &screen(""),
        &screen(&frame),
        &screen(""),
    );
    let root = crate::services::tmux_common::current_tmux_owner_marker();
    let context = json!({"schema":1,"provider":"claude","created_at":chrono::Utc::now(),
        "execution_nonce":execution,"tmux_session":session,"channel_id":7272791,
        "owner_runtime_root":root,"host":"owner-retry-test","expected_native_session_id":null,
        "launch_mode":"fresh","provider_root":null});
    let _prepared =
        crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation::create(
            serde_json::from_value(context).unwrap(),
        )
        .unwrap();
    let marker = crate::services::tmux_common::session_temp_path(&session, "spawn_nonce");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, &execution).unwrap();
    let attempt = json!({"nonce":nonce,"provider":"claude","target":{"host":"tmux","owner":session,"root":root,"execution_nonce":execution},"source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}});
    let reserved_frame = frame.clone();
    mailbox
        .v2_request(move |slot| {
            let digest = format!(
                "{:x}",
                <sha2::Sha256 as sha2::Digest>::digest(reserved_frame.as_bytes())
            );
            let payload = json!({"frame":reserved_frame,"digest":digest,"attribution":{}});
            slot.as_mut()
                .ok_or("no owner")?
                .reserve(
                    serde_json::from_value(attempt).map_err(|e| e.to_string())?,
                    serde_json::from_value(payload).map_err(|e| e.to_string())?,
                )
                .map(drop)
        })
        .await
        .unwrap();
    let ticket = Arc::new(Mutex::new(None));
    let transcript = fixture.root.path().join("not-created.jsonl");
    crate::services::claude_tui::readiness::tests::pre_source(&session, &transcript);
    let pane_name = session.clone();
    let submitted_ticket = ticket.clone();
    let leaf = move |frame: String| {
        let submitted = crate::services::claude::first_input::submit_created(
            &transcript.display().to_string(),
            0,
            &session,
            &frame,
        )
        .map_err(|error| format!("{error:?}"))?;
        *submitted_ticket.lock().unwrap() = Some(submitted);
        Ok::<(), String>(())
    };
    let (identity, outcome) = mailbox
        .v2_cold_observed(1, Arc::new(|| 1), leaf)
        .await
        .unwrap();
    assert_eq!(outcome.result, Ok(()));
    assert!(outcome.not_sent.is_none());
    let pane = fixture.root.path().join(&pane_name);
    if ticket.lock().unwrap().is_some() {
        wait("retry payload and Enter receipts", || {
            pane.join("enters").exists()
        });
    }
    let received = std::fs::read_to_string(pane.join("frame")).unwrap_or_default();
    assert_eq!(received, frame, "B's actual payload");
    assert_eq!(
        std::fs::read(pane.join("payloads")).unwrap().len(),
        1,
        "B content once"
    );
    assert_eq!(
        std::fs::read(pane.join("enters")).unwrap().len(),
        1,
        "B Enter once"
    );
    assert!(ticket.lock().unwrap().is_some(), "B's real submit ticket");
    (identity, vec![received])
}
