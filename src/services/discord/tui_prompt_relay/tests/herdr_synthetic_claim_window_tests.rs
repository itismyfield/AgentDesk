//! A pending worker excludes recovery from actor admission through its durable row save.
use super::*;
use crate::services::discord::{inflight, tui_direct_pending_start};

#[tokio::test(flavor = "current_thread")]
async fn pending_worker_owns_transition_between_actor_admission_and_inflight_save() {
    let root = tempfile::tempdir().expect("isolated runtime root");
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(7_186_102);
    let logical = "herdr-recovery-worker-admission-window";
    let transcript = root.path().join("admitted-source.jsonl");
    std::fs::write(&transcript, b"").expect("empty transcript");
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        logical,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: transcript.to_str().expect("utf8 source").to_owned(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let record: tui_direct_pending_start::TuiDirectPendingStart =
        serde_json::from_value(serde_json::json!({
            "provider": "claude",
            "channel_id": channel.get(),
            "tmux_session_name": logical,
            "prompt_text": "already submitted input",
            "anchor_message_id": 7_186_103_u64,
            "lease_relay_owner": "bridge_adapter",
            "lease_runtime_kind": "claude_tui",
            "lease_turn_id": "admission-window-original-turn",
            "lease_session_key": null,
            "generation": 0,
            "created_at_ms": 0,
            "observed_at_ms": 0
        }))
        .expect("main-compatible pending record");
    tui_direct_pending_start::persist(&record).expect("persist original obligation");
    let entered = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::new(tokio::sync::Notify::new());
    *synthetic_start::bridge_handoff::ADMISSION_PAUSE
        .lock()
        .unwrap() = Some((channel.get(), entered.clone(), resume.clone()));
    let production_claim = Arc::new(synthetic_start::pending_start_claim_fn());
    let (result_tx, mut result_rx) = tokio::sync::mpsc::unbounded_channel();
    let claim_fn: tui_direct_pending_start::ClaimFn = Box::new(move |shared, record| {
        let production_claim = production_claim.clone();
        let result_tx = result_tx.clone();
        Box::pin(async move {
            let claimed = production_claim(shared, record).await;
            result_tx.send(claimed).expect("live test receiver");
            claimed
        })
    });
    tui_direct_pending_start::spawn_worker(
        shared.clone(),
        record.clone(),
        synthetic_start::pending_start_view_fn(),
        claim_fn,
        synthetic_start::pending_start_abort_cleanup_fn(),
        synthetic_orphan_reclaim::pending_start_reclaim_orphan_fn(),
    );
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("the real worker reached its post-admission production hook");
    let admitted = crate::services::discord::mailbox_snapshot(&shared, channel).await;
    assert_eq!(
        admitted.active_user_message_id.map(MessageId::get),
        Some(record.anchor_message_id)
    );
    assert!(
        admitted.cancel_token.is_some(),
        "the worker really admitted an actor"
    );
    assert!(
        inflight::load_inflight_state_read_only(&provider, channel.get()).is_none(),
        "the hook must run before the durable save"
    );
    let recovery_attempt = shared.session_transition_lock(channel).try_lock_owned();
    let recovery_was_excluded = recovery_attempt.is_err();
    drop(recovery_attempt);
    resume.notify_one();
    let claimed = tokio::time::timeout(Duration::from_secs(5), result_rx.recv())
        .await
        .expect("the admitted worker completed its production save")
        .expect("claim result");
    assert!(
        claimed,
        "the paused fixture must still execute its normal successful save"
    );
    let saved = inflight::load_inflight_state_read_only(&provider, channel.get())
        .expect("production inflight save");
    assert_eq!(saved.user_msg_id, record.anchor_message_id);
    assert_eq!(
        saved.turn_nonce.as_deref(),
        admitted.active_turn_nonce.as_deref()
    );
    assert!(
        recovery_was_excluded,
        "recovery acquired the session transition after the worker admitted an actor and before its save"
    );
}
