use super::*;

#[cfg(unix)]
fn synthetic_bridge_handoff_fixture(
    delayed_save: bool,
    foreign_actor: bool,
    wrong_source: bool,
    postgres: bool,
    recovery: Option<bool>,
) {
    let temp = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(temp.path());
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let database = if postgres {
                Some(crate::db::auto_queue::test_support::TestPostgresDb::create().await)
            } else {
                None
            };
            let mut shared = crate::services::discord::make_shared_data_for_tests();
            if let Some(database) = database.as_ref() {
                Arc::get_mut(&mut shared).unwrap().pg_pool =
                    Some(database.connect_and_migrate().await);
            }
            let provider = ProviderKind::Claude;
            let channel = ChannelId::new(583_300_001);
            let anchor = MessageId::new(583_300_002);
            let tmux = "synthetic-bridge-handoff-5833";
            let output = temp.path().join("transcript.jsonl");
            let body = "첫 프레임 배달과 실행 중 owner 유지 ".repeat(16);
            let assistant = serde_json::json!({"type":"assistant", "message":{"content":[{"type":"text", "text":body}]}});
            std::fs::write(&output, format!("{assistant}\n")).unwrap();
            crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
                tmux,
                crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                    runtime_kind: RuntimeHandoffKind::ClaudeTui,
                    output_path: output.to_str().unwrap().to_owned(),
                    relay_output_path: None,
                    input_fifo_path: None,
                    session_id: None,
                    last_offset: 0,
                    relay_last_offset: None,
                },
            );
            let mut lease = ExternalInputRelayLease::unassigned(Some(channel.get()));
            lease.turn_id = Some("external-5833-same-provider-execution".into());
            lease.session_key = Some(crate::services::discord::adk_session::build_namespaced_session_key(
                &shared.token_hash,
                &provider,
                tmux,
            ));
            lease.relay_owner = ExternalInputRelayOwner::BridgeAdapter;
            lease.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
            let lease = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
                provider.as_str(),
                tmux,
                lease,
            );
            let claim = async {
                if delayed_save {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
                let lock = crate::services::discord::tui_direct_pending_start::channel_lock(
                    provider.as_str(),
                    channel.get(),
                );
                let _guard = lock.lock().await;
                let result = crate::services::discord::tui_prompt_relay::synthetic_start::claim_tui_direct_synthetic_turn(
                    &shared,
                    &provider,
                    channel,
                    tmux,
                    "handoff prompt",
                    anchor,
                    &lease,
                )
                .await;
                assert!(result.claimed);
                assert_eq!(result.relay_owner, ExternalInputRelayOwner::BridgeAdapter);
            };
            if foreign_actor || wrong_source {
                claim.await;
                let original = crate::services::discord::mailbox_snapshot(&shared, channel)
                    .await
                    .cancel_token
                    .unwrap();
                if foreign_actor {
                    crate::services::discord::mailbox_finish_turn(&shared, &provider, channel).await;
                    let replacement = Arc::new(CancelToken::from_persisted_turn_nonce(
                        original.turn_nonce().map(str::to_owned),
                    ));
                    assert!(
                        crate::services::discord::mailbox_try_start_turn(
                            &shared,
                            channel,
                            replacement,
                            serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID),
                            anchor,
                        )
                        .await
                    );
                }
                if foreign_actor {
                    let retry = crate::services::discord::tui_prompt_relay::synthetic_start::claim_tui_direct_synthetic_turn(
                        &shared, &provider, channel, tmux, "handoff prompt", anchor, &lease,
                    ).await;
                    assert!(!retry.claimed, "claim refresh cannot replace the retained original allocation witness");
                }
                let before =
                    crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get())
                        .unwrap();
                let supplied_source = if wrong_source {
                    temp.path().join("different.jsonl")
                } else {
                    output.clone()
                };
                assert!(
                    crate::services::discord::tui_prompt_relay::synthetic_start::bridge_handoff::capture(
                        &shared,
                        &provider,
                        channel,
                        tmux,
                        &supplied_source,
                        &lease,
                    )
                    .await
                    .is_err()
                );
                let after =
                    crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get())
                        .unwrap();
                assert_eq!(
                    serde_json::to_value(before).unwrap(),
                    serde_json::to_value(after).unwrap()
                );
                return;
            }
            let capture = crate::services::discord::tui_prompt_relay::synthetic_start::bridge_handoff::capture(
                &shared, &provider, channel, tmux, &output, &lease,
            );
            let ((), capture) = tokio::join!(claim, capture);
            let mut capture = capture.expect("same admitted provider execution reaches bridge");
            if let Some(restart) = recovery {
                // Drop the admitted adapter before it can post a frame; the durable
                // episode must remain recoverable through the same idle retry entry.
                drop(capture);
                if restart {
                    crate::services::discord::inflight::mark_all_inflight_states_restart_mode(
                        &provider, crate::services::discord::inflight::InflightRestartMode::DrainRestart,
                    );
                    let next_generation = shared.restart.current_generation + 1;
                    shared = crate::services::discord::make_shared_data_for_tests();
                    Arc::get_mut(&mut shared).unwrap().restart.current_generation = next_generation;
                }
                let row = crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get()).unwrap();
                let lease = crate::services::discord::tui_prompt_relay::synthetic_start::bridge_handoff::resume_unpublished(&shared, &row, &output).await
                    .expect("persisted original source obtains a valid delivery actor");
                capture = crate::services::discord::tui_prompt_relay::synthetic_start::bridge_handoff::capture(&shared, &provider, channel, tmux, &output, &lease)
                    .await.expect("resumed actor enters the actual bridge");
            }
            assert_eq!(
                capture.row.current_msg_id,
                anchor.get(),
                "reuse the injected anchor"
            );
            let owner = crate::services::discord::mailbox_snapshot(&shared, channel).await;
            assert!(Arc::ptr_eq(
                owner.cancel_token.as_ref().unwrap(),
                &capture.actor
            ));
            if let Some(pool) = shared.pg_pool.as_ref() {
                let persisted: (String, Option<String>, Option<String>) = sqlx::query_as(
                "SELECT status, channel_id, active_turn_nonce FROM sessions WHERE session_key = $1"
            ).bind(lease.session_key.as_deref().unwrap()).fetch_one(pool).await.unwrap();
                assert_eq!(persisted.0, "turn_active");
                assert_eq!(
                    persisted.1.as_deref(),
                    Some(channel.get().to_string().as_str())
                );
                assert_eq!(persisted.2.as_deref(), capture.actor.turn_nonce());
            }
            let gateway = Arc::new(S3Gateway::default());
            let (tx, rx) = mpsc::channel();
            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            let bridge = TurnBridgeContext {
                provider: provider.clone(),
                gateway: gateway.clone(),
                channel_id: channel,
                user_msg_id: Some(anchor),
                user_text_owned: "handoff prompt".into(),
                request_owner_name: "TUI direct".into(),
                role_binding: None,
                adk_session_key: lease.session_key.clone(),
                adk_session_name: Some(tmux.into()),
                adk_session_info: None,
                adk_cwd: None,
                dispatch_id: None,
                dispatch_kind: None,
                memory_recall_usage: TokenUsage::default(),
                context_window_tokens: 0,
                context_compact_percent: 0,
                current_msg_id: Some(anchor),
                response_sent_offset: 0,
                full_response: String::new(),
                tmux_last_offset: Some(0),
                new_session_id: None,
                defer_watcher_resume: false,
                reuse_status_panel_message: false,
                completion_tx: Some(done_tx),
                is_external_input_tui_direct: true,
                inflight_state: capture.row.clone(),
            };
            crate::services::discord::turn_bridge::spawn_turn_bridge_with_pin(
                shared.clone(),
                capture.actor.clone(),
                rx,
                bridge,
                None,
            );
            let reader_path = output.to_str().unwrap().to_owned();
            let original_start = capture.row.turn_start_offset.unwrap();
            let reader = std::thread::spawn(move || {
                crate::services::session_backend::read_output_file_until_result(
                    &reader_path, original_start, tx, None,
                    crate::services::provider::SessionProbe::process(|| true),
                )
            });
            tokio::time::timeout(Duration::from_secs(5), async {
                while !gateway
                    .bodies
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|sent| sent.contains("첫 프레임"))
                {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .expect("gateway must receive first frame before Done");
            let active = crate::services::discord::mailbox_snapshot(&shared, channel).await;
            assert!(Arc::ptr_eq(
                active.cancel_token.as_ref().unwrap(),
                &capture.actor
            ));
            assert!(
                crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get())
                    .is_some()
            );
            use std::io::Write;
            let terminal = serde_json::json!({"type":"result", "subtype":"success", "result":body});
            std::fs::OpenOptions::new().append(true).open(&output).unwrap()
                .write_all(format!("{terminal}\n").as_bytes()).unwrap();
            tokio::task::spawn_blocking(move || reader.join().unwrap().unwrap()).await.unwrap();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(5), done_rx)
                    .await
                    .unwrap()
                    .unwrap(),
                crate::services::discord::turn_bridge::BridgeCompletionSignal::Finalized
            );
            assert!(
                crate::services::discord::mailbox_snapshot(&shared, channel)
                    .await
                    .cancel_token
                    .is_none(),
                "bridge releases the captured synthetic actor after publication"
            );
            drop(capture);
            let next = Arc::new(CancelToken::new());
            assert!(
                crate::services::discord::mailbox_try_start_turn(
                    &shared,
                    channel,
                    next,
                    serenity::UserId::new(583_300_003),
                    MessageId::new(583_300_004)
                )
                .await,
                "next input is admitted after the captured actor completes"
            );
            assert!(
                gateway.deleted.lock().unwrap().is_empty(),
                "no foreign anchor is deleted"
            );
        });
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_delivers_first_frame_and_releases_original_actor() {
    synthetic_bridge_handoff_fixture(false, false, false, false, None);
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_waits_for_later_claim_save_then_delivers() {
    synthetic_bridge_handoff_fixture(true, false, false, false, None);
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_rejects_same_nonce_different_actor() {
    synthetic_bridge_handoff_fixture(false, true, false, false, None);
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_rejects_different_source_without_row_mutation() {
    synthetic_bridge_handoff_fixture(false, false, true, false, None);
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_upserts_missing_postgres_session_before_first_frame() {
    synthetic_bridge_handoff_fixture(false, false, false, true, None);
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_retries_unpublished_row_after_adapter_drops() {
    synthetic_bridge_handoff_fixture(false, false, false, false, Some(false));
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_restarts_from_persisted_source_after_mailbox_loss() {
    synthetic_bridge_handoff_fixture(false, false, false, false, Some(true));
}
