//! The real pending worker must share the session transition before it claims an actor.
use super::*;
use crate::services::discord::{inflight, tui_direct_pending_start};
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_worker_defers_its_production_claim_while_recovery_holds_transition() {
    let root = tempfile::tempdir().expect("isolated runtime root");
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(7_186_202);
    let logical = "herdr-recovery-transition-pending-worker";
    let transcript = root.path().join("transition-source.jsonl");
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
    // This legacy fixture isolates the common exclusion from Herdr's later identity checks.
    let record: tui_direct_pending_start::TuiDirectPendingStart =
        serde_json::from_value(serde_json::json!({
            "provider": "claude",
            "channel_id": channel.get(),
            "tmux_session_name": logical,
            "prompt_text": "already submitted input",
            "anchor_message_id": 7_186_203_u64,
            "lease_relay_owner": "bridge_adapter",
            "lease_runtime_kind": "claude_tui",
            "lease_turn_id": "transition-pending-original-turn",
            "lease_session_key": null,
            "generation": 0,
            "created_at_ms": 0,
            "observed_at_ms": 0
        }))
        .expect("main-compatible durable pending record");
    tui_direct_pending_start::persist(&record).expect("persist original obligation");

    let transition = shared.session_transition_lock(channel).lock_owned().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let production_claim = Arc::new(synthetic_start::pending_start_claim_fn());
    let first_return = Arc::new(tokio::sync::Notify::new());
    let (result_tx, mut result_rx) = tokio::sync::mpsc::unbounded_channel();
    let claim_fn: tui_direct_pending_start::ClaimFn = {
        let attempts = attempts.clone();
        let first_return = first_return.clone();
        Box::new(move |shared, record| {
            let production_claim = production_claim.clone();
            let attempts = attempts.clone();
            let first_return = first_return.clone();
            let result_tx = result_tx.clone();
            Box::pin(async move {
                let claimed = production_claim(shared, record).await;
                let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                result_tx
                    .send((attempt, claimed))
                    .expect("live test receiver");
                if attempt == 1 {
                    first_return.notified().await;
                }
                claimed
            })
        })
    };
    tui_direct_pending_start::spawn_worker(
        shared.clone(),
        record.clone(),
        synthetic_start::pending_start_view_fn(),
        claim_fn,
        synthetic_start::pending_start_abort_cleanup_fn(),
        synthetic_orphan_reclaim::pending_start_reclaim_orphan_fn(),
    );

    let first = tokio::time::timeout(Duration::from_secs(5), result_rx.recv())
        .await
        .expect("the worker ran its actual production claim")
        .expect("claim result");
    assert!(
        shared
            .session_transition_lock(channel)
            .try_lock_owned()
            .is_err(),
        "recovery still excludes another transition owner"
    );
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        1,
        "one observed production attempt"
    );
    assert!(
        !first.1,
        "a pending worker claimed through the recovery transition"
    );
    assert!(
        crate::services::discord::mailbox_snapshot(&shared, channel)
            .await
            .cancel_token
            .is_none(),
        "the excluded worker must not create an actor"
    );
    assert!(
        inflight::load_inflight_state_read_only(&provider, channel.get()).is_none(),
        "the excluded worker must not write inflight"
    );
    assert!(
        tui_direct_pending_start::load_all()
            .iter()
            .any(|saved| saved == &record),
        "transition contention preserves the durable input obligation"
    );

    drop(transition);
    first_return.notify_one();
    let second = tokio::time::timeout(Duration::from_secs(5), result_rx.recv())
        .await
        .expect("the same worker retries after recovery releases the transition")
        .expect("retry result");
    assert_eq!(
        second,
        (2, true),
        "the fixture can really claim once exclusion ends"
    );
    let row = inflight::load_inflight_state_read_only(&provider, channel.get())
        .expect("the successful retry persisted its own actor");
    assert_eq!(row.user_msg_id, record.anchor_message_id);
    assert_eq!(row.tmux_session_name.as_deref(), Some(logical));
    assert!(
        crate::services::discord::mailbox_snapshot(&shared, channel)
            .await
            .cancel_token
            .is_some(),
        "the positive control must produce a real actor"
    );
}
