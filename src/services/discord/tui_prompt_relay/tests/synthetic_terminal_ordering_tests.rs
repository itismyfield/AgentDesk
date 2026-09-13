//! The actual synthetic adapter must retain A until terminal transport settles.
use super::*;

#[derive(Default)]
pub(super) struct TerminalBarrier {
    pub(super) entered: tokio::sync::Notify,
    pub(super) release: tokio::sync::Notify,
}

fn terminal_ordering_fixture(replace_actor: bool, replace_after_delivery: bool) {
    let temp = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(temp.path());
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
        .block_on(async {
            let shared = crate::services::discord::make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let channel = ChannelId::new(583_310_001);
            let anchor = MessageId::new(583_310_002);
            let tmux = "synthetic-terminal-ordering-5833";
            let generation_path = crate::services::tmux_common::session_temp_path(tmux, "generation");
            std::fs::write(&generation_path, b"1").unwrap();
            let output = temp.path().join("transcript.jsonl");
            let body = "synthetic terminal publication keeps its original actor ".repeat(12);
            let assistant = serde_json::json!({"type":"assistant", "message":{"content":[{"type":"text", "text":body}]}});
            let terminal = serde_json::json!({"type":"result", "subtype":"success", "result":body});
            std::fs::write(&output, format!("{assistant}\n{terminal}\n")).unwrap();
            crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(tmux,
                crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                    runtime_kind: RuntimeHandoffKind::ClaudeTui,
                    output_path: output.to_str().unwrap().into(),
                    relay_output_path: None, input_fifo_path: None,
                    session_id: None, last_offset: 0, relay_last_offset: None,
                });
            let mut lease = ExternalInputRelayLease::unassigned(Some(channel.get()));
            lease.turn_id = Some("external-5833-terminal-ordering".into());
            lease.relay_owner = ExternalInputRelayOwner::BridgeAdapter;
            lease.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
            let lease = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
                provider.as_str(), tmux, lease);
            assert!(synthetic_start::claim_tui_direct_synthetic_turn(
                &shared, &provider, channel, tmux, "terminal ordering prompt", anchor, &lease,
            ).await.claimed);
            // Inspect A without acquiring a BridgeClaim: dropping that claim
            // clears its external-input lease before the real adapter captures it.
            let original_actor = crate::services::discord::mailbox_snapshot(&shared, channel)
                .await.cancel_token.expect("synthetic admission retains A");
            let original_row = crate::services::discord::inflight::load_inflight_state_read_only(
                &provider, channel.get()).expect("synthetic admission persists A's row");
            assert_eq!(original_actor.turn_nonce(), original_row.turn_nonce.as_deref());
            let barrier = Arc::new(TerminalBarrier::default());
            let gateway = Arc::new(S3Gateway {
                local_delivery: true, terminal_barrier: Some(barrier.clone()),
                ..Default::default()
            });
            let (tx, rx) = mpsc::channel();
            let (reader_end_tx, reader_end_rx) = tokio::sync::oneshot::channel();
            let reader = super::synthetic_bridge_handoff_pg_tests::spawn_handoff_reader(
                &output, 0, tmux, tx, reader_end_tx,
            );
            let delivery = claude_idle_bridge::stream_tui_idle_response_with_gateway(
                &shared, provider.clone(), channel, tmux, &output, 0,
                "terminal ordering prompt", Vec::new(), rx, Some(reader_end_rx), &lease, gateway.clone(), 0,
            );
            let observe = async {
                tokio::time::timeout(Duration::from_secs(5), barrier.entered.notified())
                    .await.expect("actual adapter enters terminal transport");
                let before = crate::services::discord::mailbox_snapshot(&shared, channel).await;
                assert!(before.cancel_token.as_ref().is_some_and(|token| Arc::ptr_eq(token, &original_actor)),
                    "the original synthetic actor must still own the turn DURING terminal transport");
                assert!(!original_actor.cancelled.load(std::sync::atomic::Ordering::Acquire));
                let row = crate::services::discord::inflight::load_inflight_state_read_only(
                    &provider, channel.get()).expect("terminal transport retains its delivery obligation");
                assert_eq!(row.turn_nonce, original_row.turn_nonce);
                let replacement = if replace_actor {
                    let actor = Arc::new(CancelToken::from_persisted_turn_nonce(
                        original_actor.turn_nonce().map(str::to_owned)));
                    crate::services::discord::mailbox_recovery_kickoff(
                        &shared, channel, actor.clone(),
                        serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID), Some(anchor),
                    ).await;
                    let swapped = crate::services::discord::mailbox_snapshot(&shared, channel).await;
                    assert!(swapped.cancel_token.as_ref().is_some_and(|active| Arc::ptr_eq(active, &actor)));
                    assert_eq!(swapped.active_turn_nonce, before.active_turn_nonce);
                    Some(actor)
                } else { None };
                barrier.release.notify_one();
                replacement
            };
            let (delivered, replacement) = tokio::join!(
                tokio::time::timeout(Duration::from_secs(5), delivery), observe);
            let delivered = delivered.expect("terminal transport must settle");
            tokio::task::spawn_blocking(move || reader.join().unwrap()).await.unwrap();
            let replacement = if replace_after_delivery {
                delivered.as_ref().expect("A completed before the duplicate-finalizer race");
                let after_a = crate::services::discord::mailbox_snapshot(&shared, channel).await;
                assert!(after_a.cancel_token.is_none());
                let actor = Arc::new(CancelToken::from_persisted_turn_nonce(
                    original_actor.turn_nonce().map(str::to_owned)));
                crate::services::discord::mailbox_recovery_kickoff(
                    &shared, channel, actor.clone(),
                    serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID), Some(anchor),
                ).await;
                crate::services::discord::inflight::save_inflight_state(&original_row).unwrap();
                Some(actor)
            } else { replacement };
            let after = crate::services::discord::mailbox_snapshot(&shared, channel).await;
            if let Some(replacement) = replacement {
                assert!(after.cancel_token.as_ref().is_some_and(|active| Arc::ptr_eq(active, &replacement)),
                    "same-nonce RecoveryKickoff actor survives A's terminal and postlude");
                assert!(!replacement.cancelled.load(std::sync::atomic::Ordering::Acquire));
                let row = crate::services::discord::inflight::load_inflight_state_read_only(
                    &provider, channel.get()).expect("same-episode successor's row survives");
                assert_eq!(row.current_msg_id, original_row.current_msg_id);
                // Re-submit the exact old actor after its first submission path
                // has completed. Duplicate cleanup must retain the actor guard.
                let mut snapshot = crate::services::discord::turn_finalizer::SyntheticClaimSnapshot::from_row(&original_row);
                snapshot.recovery_actor = Some(Arc::downgrade(&original_actor));
                let duplicate_outcome = shared.turn_finalizer.submit_terminal_with_claim_snapshot(
                    crate::services::discord::turn_finalizer::TurnKey::new(
                        channel, original_row.effective_finalizer_turn_id(), shared.restart.current_generation,
                    ).with_episode_nonce(original_row.turn_nonce.as_deref()),
                    provider.clone(), crate::services::discord::turn_finalizer::TerminalEvent::Complete,
                    crate::services::discord::turn_finalizer::FinalizeContext::bridge(), Some(snapshot), shared.clone(),
                ).await;
                if replace_after_delivery {
                    assert!(matches!(duplicate_outcome,
                        crate::services::discord::turn_finalizer::FinalizeOutcome::AlreadyFinalized));
                }
                let duplicate = crate::services::discord::mailbox_snapshot(&shared, channel).await;
                assert!(duplicate.cancel_token.as_ref().is_some_and(|active| Arc::ptr_eq(active, &replacement)));
                assert!(!replacement.cancelled.load(std::sync::atomic::Ordering::Acquire));
            } else {
                delivered.expect("original synthetic delivery completes");
                assert!(after.cancel_token.is_none(), "A releases only after successful publication");
                assert!(gateway.bodies.lock().unwrap().iter().any(|sent| sent.contains(&body)));
                let next = Arc::new(CancelToken::new());
                assert!(crate::services::discord::mailbox_try_start_turn(
                    &shared, channel, next, serenity::UserId::new(583_310_003), MessageId::new(583_310_004),
                ).await, "next input can obtain the released owner");
            }
        });
}

#[test]
fn synthetic_terminal_gateway_retains_original_actor_until_publication() {
    terminal_ordering_fixture(false, false);
}

#[test]
fn synthetic_terminal_gateway_preserves_same_nonce_recovery_actor() {
    terminal_ordering_fixture(true, false);
}

#[test]
fn synthetic_terminal_duplicate_finalizer_preserves_same_nonce_recovery_actor() {
    terminal_ordering_fixture(false, true);
}
