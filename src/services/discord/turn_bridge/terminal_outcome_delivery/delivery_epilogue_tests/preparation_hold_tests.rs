#![cfg(unix)]
use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::claude_tui::guarded_outcome;
use crate::services::claude_tui::host_input::guarded_submission::executor_tests::{
    install_shim, renderer, screen,
};
use crate::services::claude_tui::host_input::{SpyGuard, SpyState};
use crate::services::tui_input::submission_tmux::guarded::transport_tests::Fixture;
use crate::services::tui_prompt_dedupe::{self, TuiRuntimeBinding};

#[tokio::test(flavor = "current_thread")]
async fn a2_fresh_hold_preserves_binding_through_no_init_and_notifies_once_pg() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
    let f = Fixture::after_shared_test_env_lock();
    install_shim(&f);
    let name = format!("a2-held-bridge-{}", uuid::Uuid::new_v4());
    let empty_view =
        crate::services::claude_tui::host_input::guarded_submission::idle_stash::tests::view(
            "", false,
        );
    renderer(&f, &name, "own", &empty_view, &screen("own"), &empty_view);
    let transcript = f.root.path().join("held.jsonl");
    let owner = f.root.path().join("owner");
    std::fs::write(&transcript, "").unwrap();
    std::fs::write(&owner, "owner").unwrap();
    let binding = TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: transcript.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some("held-sid".into()),
        last_offset: 0,
        relay_last_offset: None,
    };
    tui_prompt_dedupe::register_tmux_runtime_binding(&name, binding.clone());
    crate::services::claude_tui::composer_lock::guard_draft(
        &name,
        crate::services::claude_tui::composer_lock::DraftGuard::RecoveryRequired,
    );
    let calls = f.root.path().join("upper-calls");
    let shim = f.root.path().join("tmux");
    let script = std::fs::read_to_string(&shim).unwrap();
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n{}",
            calls.display(),
            script.strip_prefix("#!/bin/sh\n").unwrap()
        ),
    )
    .unwrap();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().pg_pool = Some(pool.clone());
    shared
        .tmux_watchers
        .remove(&ChannelId::new(driver.inflight.channel_id));
    let mut row = driver.inflight.clone();
    row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    row.tmux_session_name = Some(name.clone());
    row.session_id = Some("held-sid".into());
    row.output_path = Some(transcript.display().to_string());
    row.user_text = "첫째 줄\n입력 원문 보존".into();
    let capability = crate::services::discord::inflight::install_managed_submission_boundary(
        &mut row,
        &crate::services::turn_host::TurnHost::Tmux,
    )
    .unwrap();
    crate::services::discord::inflight::clear_inflight_state(&ProviderKind::Claude, row.channel_id);
    crate::services::discord::inflight::save_inflight_state_create_new(&row).unwrap();
    for _ in 0..2 {
        let spy = SpyGuard::install(SpyState {
            captures: std::iter::repeat_n(Some(screen("")), 20).collect(),
            draft_capture_at: Some((1, Some("unrecognized overlay".into()))),
            ..SpyState::default()
        });
        let result = guarded_outcome::with_owner(Some(capability.observer()), || {
            crate::services::claude::fresh_hold_for_tests(
                &transcript,
                &name,
                &owner,
                &row.user_text,
            )
        });
        assert!(result.is_err());
        assert!(spy.calls().iter().all(|c| !c.starts_with("keys:")
            && !c.starts_with("paste:")
            && !c.starts_with("literal:")));
        drop(spy);
        let (mut ctx, mut state) = driver.parts();
        ctx.watcher_delivery_pin = None;
        ctx.had_prior_session_id_at_turn_start = true;
        ctx.session_handshake_seen = false;
        ctx.rx_disconnected = true;
        state.shared_owned = shared.clone();
        state.inflight_state = row.clone();
        state.user_text_owned = row.user_text.clone();
        state.full_response.clear();
        let output = run_terminal_outcome_delivery(ctx, state).await;
        assert!(matches!(
            output.outcome,
            TerminalOutcomeDeliveryOutcome::Completed
        ));
        let requeue = output
            .busy_requeue_outcome
            .expect("the typed pre-submit hold reaches mailbox handback");
        assert!(requeue.requeued && requeue.preparation_held && !requeue.retry_capped);
        assert!(
            !output.resume_failure_detected && !output.auto_retry.queued(),
            "no-Init from preparation is no stale-session witness"
        );
        assert!(!output.bridge_should_emit_completion && !output.status_panel_terminal_committed);
        assert!(!output.provider_failure && !output.terminal_delivery_committed);
    }
    let budget = crate::services::discord::busy_followup_retry_store::load(
        &ProviderKind::Claude,
        row.channel_id,
        row.user_msg_id,
    )
    .unwrap();
    assert_eq!(
        (
            budget.busy_retry_count,
            budget.active_retry_ms,
            budget.capped
        ),
        (0, 0, false)
    );
    assert_eq!(
        budget.notice_message_id, row.current_msg_id,
        "an outbox id must never replace the actual placeholder binding"
    );
    let queue = crate::services::discord::mailbox_snapshot(&shared, ChannelId::new(row.channel_id))
        .await
        .intervention_queue;
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].text.as_bytes(), row.user_text.as_bytes());
    let notices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM message_outbox WHERE target=$1 AND reason_code='claude.input.preparation_hold'")
        .bind(format!("channel:{}", row.channel_id)).fetch_one(&pool).await.unwrap();
    assert_eq!(notices, 1);
    let notice: String = sqlx::query_scalar("SELECT content FROM message_outbox WHERE target=$1 AND reason_code='claude.input.preparation_hold'")
        .bind(format!("channel:{}", row.channel_id)).fetch_one(&pool).await.unwrap();
    assert!(crate::services::discord::dispatch_policy::has_non_turn_provenance(&notice));
    assert!(
        driver.observations().is_empty(),
        "waiting never publishes an answer or completion chrome"
    );
    assert_eq!(
        tui_prompt_dedupe::runtime_binding_for_tmux_session(&name),
        Some(binding.clone())
    );
    assert!(owner.exists());
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
        loop {
            if crate::services::discord::busy_followup_retry_store::load(
                &ProviderKind::Claude,
                row.channel_id,
                row.user_msg_id,
            )
            .is_some_and(|budget| !budget.draft_hold)
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the existing observer releases the now-empty pane");
    let taken = crate::services::discord::mailbox_take_next_automatic_intervention(
        &shared,
        &ProviderKind::Claude,
        ChannelId::new(row.channel_id),
    )
    .await;
    let input = taken
        .intervention
        .expect("the held source becomes ready once");
    assert_eq!(input.message_id.get(), row.user_msg_id);
    assert_eq!(input.text.as_bytes(), row.user_text.as_bytes());
    let mut resumed = row.clone();
    resumed.turn_nonce = Some(uuid::Uuid::new_v4().to_string());
    resumed.started_at = chrono::Utc::now().to_rfc3339();
    let cap = crate::services::discord::inflight::install_managed_submission_boundary(
        &mut resumed,
        &crate::services::turn_host::TurnHost::Tmux,
    )
    .unwrap();
    crate::services::discord::inflight::clear_inflight_state(&ProviderKind::Claude, row.channel_id);
    crate::services::discord::inflight::save_inflight_state_create_new(&resumed).unwrap();
    let pane = f.root.path().join(&name);
    std::fs::write(pane.join("expected"), &input.text).unwrap();
    std::fs::write(
        pane.join("own"),
        crate::services::claude_tui::host_input::guarded_submission::idle_stash::tests::view(
            &input.text,
            false,
        ),
    )
    .unwrap();
    guarded_outcome::with_owner(Some(cap.observer()), || {
        crate::services::claude_tui::submission_fence::with_scope(Some(cap.fence()), || {
            let _end = guarded_outcome::seal_on_exit(&name);
            crate::services::claude_tui::input::send_prompt_with_readiness(
                &name,
                &input.text,
                crate::services::claude_tui::input::PromptReadinessKind::FreshTurn,
                None,
            )
            .unwrap();
            use std::io::Write;
            writeln!(std::fs::OpenOptions::new().append(true).open(&transcript).unwrap(), "{}", serde_json::json!({"type":"user", "sessionId":"held-sid", "message":{"content":input.text}})).unwrap();
        })
    });
    assert_eq!(std::fs::read(pane.join("payloads")).unwrap().len(), 1);
    assert_eq!(std::fs::read(pane.join("enters")).unwrap().len(), 1);
    assert_eq!(
        std::fs::read(pane.join("frame")).unwrap(),
        row.user_text.as_bytes()
    );
    let (mut ctx, mut state) = driver.parts();
    ctx.watcher_delivery_pin = None;
    state.shared_owned = shared.clone();
    state.inflight_state = resumed;
    state.user_text_owned = row.user_text.clone();
    let output = run_terminal_outcome_delivery(ctx, state).await;
    assert!(
        output.terminal_delivery_committed
            && output.bridge_should_emit_completion
            && output.status_panel_terminal_committed
    );
    assert_eq!(
        driver.completed_publications(),
        1,
        "release produces exactly one answer"
    );
    assert!(
        crate::services::discord::mailbox_snapshot(&shared, ChannelId::new(row.channel_id))
            .await
            .intervention_queue
            .is_empty()
    );
    crate::services::discord::mailbox_abandon_unclaimed_dispatch_after_success(
        &shared,
        &ProviderKind::Claude,
        ChannelId::new(row.channel_id),
        input.message_id,
        taken.dispatch_lease.unwrap(),
    )
    .await;
    let calls = std::fs::read_to_string(calls).unwrap();
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.contains("kill-session"))
            .count(),
        0,
        "{calls}"
    );
    assert_eq!(
        crate::services::platform::tmux::session_presence(&name),
        crate::services::platform::tmux::SessionPresence::Present
    );
}

#[tokio::test]
async fn a2_may_have_submitted_and_old_or_contaminated_records_never_confirm_completion() {
    use std::io::Write;
    for (fenced, old, suffix, expected) in [
        (true, true, "", false),
        (true, false, " extra", false),
        (false, false, "", false),
        (true, false, "", true),
    ] {
        let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
        let (ctx, mut state) = driver.parts();
        let session = format!("a2-exact-{}", uuid::Uuid::new_v4());
        let path = driver._temp.path().join("proof.jsonl");
        let frame = "입력 원문\n바이트 일치";
        let record = |body: &str| {
            format!(
                "{}\n",
                serde_json::json!({"type":"user","sessionId":"proof-sid","message":{"content":body}})
            )
        };
        std::fs::write(&path, if old { record(frame) } else { String::new() }).unwrap();
        tui_prompt_dedupe::register_tmux_runtime_binding(
            &session,
            TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some("proof-sid".into()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        let row = &mut state.inflight_state;
        row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        row.tmux_session_name = Some(session.clone());
        row.output_path = Some(path.display().to_string());
        row.user_text = frame.into();
        state.user_text_owned = frame.into();
        let capability = crate::services::discord::inflight::install_managed_submission_boundary(
            row,
            &crate::services::turn_host::TurnHost::Tmux,
        )
        .unwrap();
        crate::services::discord::inflight::clear_inflight_state(
            &ProviderKind::Claude,
            row.channel_id,
        );
        crate::services::discord::inflight::save_inflight_state_create_new(row).unwrap();
        capability.observer().receipt(
            crate::services::claude_tui::input::UserReceipt::before(&session, frame).unwrap(),
        );
        if fenced {
            capability.fence().before_first_payload().unwrap();
        }
        if !old {
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(record(&format!("{frame}{suffix}")).as_bytes())
                .unwrap();
        }
        capability.observer().receipt_finished(&session);
        let output = run_terminal_outcome_delivery(ctx, state).await;
        assert!(
            output.terminal_delivery_committed,
            "the answer delivery really commits"
        );
        assert_eq!(driver.completed_publications(), 1);
        assert_eq!(output.bridge_should_emit_completion, expected);
        assert_eq!(output.status_panel_terminal_committed, expected);
        assert_eq!(driver.marker(), expected);
        tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
    }
}

#[tokio::test]
async fn a2_uncertain_payload_keeps_the_binding_and_notice_without_resubmission_pg() {
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared).unwrap().pg_pool = Some(pool.clone());
    let (mut ctx, mut state) = driver.parts();
    ctx.had_prior_session_id_at_turn_start = true;
    ctx.session_handshake_seen = false;
    ctx.rx_disconnected = true;
    state.shared_owned = shared.clone();
    state.full_response.clear();
    let row = &mut state.inflight_state;
    row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    let name = format!("a2-uncertain-{}", uuid::Uuid::new_v4());
    row.tmux_session_name = Some(name.clone());
    let path = driver._temp.path().join("uncertain.jsonl");
    std::fs::write(&path, "").unwrap();
    let binding = TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some("uncertain-sid".into()),
        last_offset: 0,
        relay_last_offset: None,
    };
    tui_prompt_dedupe::register_tmux_runtime_binding(&name, binding.clone());
    let cap = crate::services::discord::inflight::install_managed_submission_boundary(
        row,
        &crate::services::turn_host::TurnHost::Tmux,
    )
    .unwrap();
    crate::services::discord::inflight::clear_inflight_state(&ProviderKind::Claude, row.channel_id);
    crate::services::discord::inflight::save_inflight_state_create_new(row).unwrap();
    cap.fence().before_first_payload().unwrap();
    guarded_outcome::with_owner(Some(cap.observer()), || {
        guarded_outcome::SubmissionError::Held(
            crate::services::claude_tui::input::PreparationHold::uncertain(
                &name,
                "uncertain ACK".into(),
            ),
        )
        .into_legacy()
    });
    let (channel, source, notice) = (row.channel_id, row.user_msg_id, row.current_msg_id);
    crate::services::discord::busy_followup_retry_store::bind_notice_if_absent(
        &ProviderKind::Claude,
        channel,
        source,
        notice,
    )
    .unwrap();
    let output = run_terminal_outcome_delivery(ctx, state).await;
    assert!(!output.auto_retry.queued() && !output.resume_failure_detected);
    assert!(output.preserve_inflight_for_cleanup_retry);
    assert!(
        !output.terminal_delivery_committed
            && !output.bridge_should_emit_completion
            && !output.status_panel_terminal_committed
    );
    assert!(output.busy_requeue_outcome.is_none());
    assert!(driver.observations().is_empty());
    assert!(
        crate::services::discord::mailbox_snapshot(&shared, ChannelId::new(channel))
            .await
            .intervention_queue
            .is_empty()
    );
    assert_eq!(
        tui_prompt_dedupe::runtime_binding_for_tmux_session(&name),
        Some(binding)
    );
    let retained = crate::services::discord::busy_followup_retry_store::load(
        &ProviderKind::Claude,
        channel,
        source,
    );
    assert!(
        retained.is_some(),
        "an uncertain payload must retain the actual notice binding"
    );
    assert_eq!(retained.unwrap().notice_message_id, notice);
}

#[test]
fn a2_owner_cannot_restart_a_finished_receipt_window() {
    use std::io::Write;
    let driver = TerminalDeliveryDriver::new(ReplaceBehaviour::Edited, 0);
    let path = driver._temp.path().join("ended.jsonl");
    std::fs::write(&path, "").unwrap();
    let session = format!("a2-ended-owner-{}", uuid::Uuid::new_v4());
    tui_prompt_dedupe::register_tmux_runtime_binding(
        &session,
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("sid".into()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let mut row = driver.inflight.clone();
    row.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
    row.tmux_session_name = Some(session.clone());
    let cap = crate::services::discord::inflight::install_managed_submission_boundary(
        &mut row,
        &crate::services::turn_host::TurnHost::Tmux,
    )
    .unwrap();
    crate::services::discord::inflight::clear_inflight_state(&ProviderKind::Claude, row.channel_id);
    crate::services::discord::inflight::save_inflight_state_create_new(&row).unwrap();
    cap.fence().before_first_payload().unwrap();
    cap.observer().receipt(
        crate::services::claude_tui::input::UserReceipt::before(&session, "same input").unwrap(),
    );
    guarded_outcome::with_owner(Some(cap.observer()), || {
        let _end = guarded_outcome::seal_on_exit(&session);
    });
    cap.observer().receipt(
        crate::services::claude_tui::input::UserReceipt::before(&session, "same input").unwrap(),
    );
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap(),
        "{}",
        serde_json::json!({"type":"user", "sessionId":"sid", "message":{"content":"same input"}})
    )
    .unwrap();
    cap.observer().receipt_finished(&session);
    assert!(
        !cap.exact_user_receipt(),
        "an ended attempt cannot accept a late receipt callback"
    );
    let serialized = serde_json::to_value(&row).unwrap();
    let restored: InflightTurnState = serde_json::from_value(serialized.clone()).unwrap();
    assert!(restored.managed_submission_capability().is_none());
    assert!(
        restored
            .with_live_submission_from(&row)
            .managed_submission_capability()
            .is_some(),
        "a live owner survives a same-attempt persisted row"
    );
    for (field, value) in [
        ("turn_nonce", serde_json::json!("foreign")),
        ("tmux_session_name", serde_json::json!("foreign-pane")),
        ("managed_submission", serde_json::Value::Null),
        (
            "managed_submission",
            serde_json::json!({"protocol":99,"phase":"waiting"}),
        ),
    ] {
        let mut changed = serialized.clone();
        changed[field] = value;
        let restored: InflightTurnState = serde_json::from_value(changed).unwrap();
        assert!(
            restored
                .with_live_submission_from(&row)
                .managed_submission_capability()
                .is_none(),
            "a foreign or unmarked row cannot inherit the live owner: {field}"
        );
    }
    tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
}
