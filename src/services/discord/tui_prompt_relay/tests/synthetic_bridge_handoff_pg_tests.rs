use super::*;

pub(super) struct HandoffReader {
    thread: Option<std::thread::JoinHandle<()>>,
    cancel: Arc<CancelToken>,
}

impl HandoffReader {
    pub(super) fn join(mut self) -> std::thread::Result<()> {
        let thread = self.thread.take().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !thread.is_finished() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let timed_out = !thread.is_finished();
        self.cancel
            .cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        thread.join()?;
        if timed_out {
            Err(Box::new("reader did not stop at its terminal"))
        } else {
            Ok(())
        }
    }
}

impl Drop for HandoffReader {
    fn drop(&mut self) {
        self.cancel
            .cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(unix)]
pub(super) fn spawn_handoff_reader(
    path: &Path,
    start: u64,
    tmux: &str,
    tx: mpsc::Sender<StreamMessage>,
    end: tokio::sync::oneshot::Sender<Result<claude_idle_bridge::IdleReaderCompletion, String>>,
) -> HandoffReader {
    let generation = crate::services::discord::turn_bridge::tmux_generation_file_mtime_ns(tmux);
    let path = path.to_str().unwrap().to_owned();
    let cancel = Arc::new(CancelToken::new());
    let reader_cancel = cancel.clone();
    let tmux = tmux.to_owned();
    let thread = std::thread::spawn(move || {
        if crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(&tmux)
            .is_some_and(|binding| binding.runtime_kind == RuntimeHandoffKind::CodexTui)
        {
            let _ = end.send(codex_idle_rollout::read_codex_idle_completion(
                Path::new(&path),
                start,
                tx,
                Some(reader_cancel),
                || true,
                &tmux,
                None,
            ));
            return;
        }
        let result = crate::services::session_backend::read_output_file_until_result_with_harvest(
            &path,
            start,
            tx,
            Some(reader_cancel),
            crate::services::provider::SessionProbe::process(|| true),
        );
        let _ = end.send(
            result
                .map(|(result, stats)| {
                    claude_idle_bridge::IdleReaderCompletion::from_harvest(
                        result, stats, generation,
                    )
                })
                .map_err(|error| error.error),
        );
    });
    HandoffReader {
        thread: Some(thread),
        cancel,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum EmptyTailCase {
    DecodedTerminal,
    MissingSource,
    DeferredError,
}

#[cfg(unix)]
type SyntheticBridgeHandoffParameters = (
    bool,
    bool,
    bool,
    bool,
    Option<bool>,
    bool,
    bool,
    Option<EmptyTailCase>,
    bool,
    bool,
    bool,
    Option<ProviderKind>,
);

#[cfg(unix)]
fn synthetic_bridge_handoff_fixture(
    (
        delayed_save,
        foreign_actor,
        wrong_source,
        postgres,
        recovery,
        failed_save,
        source_retry,
        empty_tail,
        postgres_race,
        admission_race,
        prefix_read_error,
        native_tail,
    ): SyntheticBridgeHandoffParameters,
) {
    let temp = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(temp.path());
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let _boot = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(Box::pin(async {
            if postgres { let _ = tracing_subscriber::fmt().with_test_writer().with_max_level(tracing::Level::INFO).try_init(); }
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
            let native_compaction = native_tail == Some(ProviderKind::Claude);
            let native_codex = native_tail == Some(ProviderKind::Codex);
            let provider = native_tail.clone().unwrap_or(ProviderKind::Claude);
            let runtime = if native_codex { RuntimeHandoffKind::CodexTui } else { RuntimeHandoffKind::ClaudeTui };
            let channel = ChannelId::new(583_300_001);
            let anchor = MessageId::new(583_300_002);
            let tmux = "synthetic-bridge-handoff-5833";
            let generation_path = crate::services::tmux_common::session_temp_path(tmux, "generation");
            std::fs::write(&generation_path, b"1").unwrap();
            let output = temp.path().join("transcript.jsonl");
            let body = "첫 프레임 배달과 실행 중 owner 유지 ".repeat(16);
            let mut assistant = serde_json::json!({"type":"assistant", "message":{"content":[{"type":"text", "text":body}]}});
            if native_codex {
                assistant = serde_json::json!({"type":"response_item", "payload":{"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":body}]}});
            }
            if native_compaction { assistant["sessionId"] = "native-auto-session".into(); }
            let previous = if failed_save { "" } else { "{\"type\":\"user\",\"message\":{\"content\":\"previous turn\"}}\n" };
            let source_start = previous.len() as u64;
            std::fs::write(&output, format!("{previous}{assistant}\n")).unwrap();
            crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
                tmux,
                crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                    runtime_kind: runtime,
                    output_path: output.to_str().unwrap().to_owned(),
                    relay_output_path: None,
                    input_fifo_path: None,
                    session_id: None,
                    last_offset: source_start,
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
            lease.runtime_kind = Some(runtime);
            let lease = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
                provider.as_str(),
                tmux,
                lease,
            );
            if admission_race {
                let entered = Arc::new(tokio::sync::Notify::new());
                let resume = Arc::new(tokio::sync::Notify::new());
                *synthetic_start::bridge_handoff::ADMISSION_PAUSE.lock().unwrap() =
                    Some((channel.get(), entered.clone(), resume.clone()));
                let attempt = synthetic_start::claim_tui_direct_synthetic_turn_inner::<false>(
                    &shared, &provider, channel, tmux, ("handoff prompt", anchor), &lease, (None, None),
                );
                let replace = async {
                    entered.notified().await;
                    let original = crate::services::discord::mailbox_snapshot(&shared, channel).await.cancel_token.unwrap();
                    crate::services::discord::mailbox_finish_turn(&shared, &provider, channel).await;
                    let successor = Arc::new(CancelToken::from_persisted_turn_nonce(original.turn_nonce().map(str::to_owned)));
                    assert!(crate::services::discord::mailbox_try_start_turn(
                        &shared, channel, successor.clone(), serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID), anchor,
                    ).await);
                    resume.notify_one();
                    successor
                };
                let ((claim, _), successor) = tokio::join!(attempt, replace);
                assert!(!claim.claimed, "inline admission cannot adopt its same-nonce successor");
                assert!(crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get()).is_none());
                assert!(Arc::ptr_eq(&crate::services::discord::mailbox_snapshot(&shared, channel).await.cancel_token.unwrap(), &successor));
                return;
            }
            let mut claim = Some(async {
                if failed_save {
                    use crate::services::discord::{inflight, tui_direct_pending_start as pending};
                    let inflight_path = inflight::inflight_state_path(
                        &inflight::inflight_runtime_root().unwrap(), &provider, channel.get());
                    // An existing directory makes the actual guarded atomic save fail.
                    std::fs::create_dir_all(&inflight_path).unwrap();
                    let observed = ObservedTuiPrompt {
                        hosted_source: None,
                        provider: provider.as_str().into(), tmux_session_name: tmux.into(),
                        prompt: "handoff prompt".into(), observed_at: chrono::Utc::now(),
                        source_event_id: None, external_input_lease_generation: lease.generation,
                        ssh_direct_observation_generation: crate::services::tui_prompt_dedupe::SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
                        hook_prompt_id: None, row_prompt_id: None,
                        native_turn_id: None,
                        steer_echo: false,
                    };
                    let mut inline_lease = lease.clone();
                    assert!(synthetic_start_wiring::wire_tui_direct_synthetic_turn_start(
                        &shared, channel, &observed, anchor, true,
                        &relay_observed_prompt_injected_prompt_decision(&observed.prompt),
                        &mut inline_lease,
                    ).await, "failed inline save must hand its retry to the pending worker");
                    assert!(inflight::load_inflight_state_read_only(&provider, channel.get()).is_none());
                    let record = pending::load_all().into_iter().find(|record| record.channel_id == channel.get()).unwrap();
                    assert_eq!(record.anchor_message_id, anchor.get());
                    assert_eq!(record.captured_source, Some((output.to_str().unwrap().into(), 0)));
                    // A later observer consumed cursor is not delivery evidence. The
                    // retry must still publish the original bytes below this cursor.
                    let mut advanced = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux).unwrap();
                    advanced.last_offset = std::fs::metadata(&output).unwrap().len();
                    advanced.relay_last_offset = Some(advanced.last_offset);
                    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(tmux, advanced);
                    // Exhaust the real claim worker while atomic persistence
                    // still fails, then exercise the production startup restore.
                    tokio::time::pause();
                    tokio::task::yield_now().await;
                    for _ in 0..(pending::PENDING_START_MAX_CLAIM_ATTEMPTS + 2) {
                        tokio::time::advance(pending::PENDING_START_BACKSTOP + pending::PENDING_START_CLAIM_RETRY_BACKOFF).await;
                        tokio::task::yield_now().await;
                    }
                    assert!(pending::pending_synthetic_start_abandoned(provider.as_str(), channel.get()));
                    let retained = pending::load_all().into_iter().find(|record| record.channel_id == channel.get()).unwrap();
                    assert_eq!(retained.captured_source, record.captured_source);
                    assert!(retained.attempt_count >= pending::PENDING_START_MAX_CLAIM_ATTEMPTS);
                    tokio::time::resume();
                    pending::reset_present_for_tests();
                    std::fs::remove_dir(&inflight_path).unwrap();
                    synthetic_start::restore_pending_starts(&shared, &provider);
                    tokio::time::timeout(Duration::from_secs(5), async {
                        while pending::load_all().iter().any(|record| record.channel_id == channel.get())
                            || CLAUDE_IDLE_RESPONSE_TAILS.lock().unwrap().contains(tmux)
                        {
                            tokio::time::sleep(Duration::from_millis(25)).await;
                        }
                    }).await.expect("existing pending worker must save the original source");
                    let row = inflight::load_inflight_state_read_only(&provider, channel.get()).unwrap();
                    assert_eq!(row.turn_start_offset, Some(0));
                    // The detached HTTP adapter has no real HTTP in this fixture;
                    // route its saved obligation into the gateway adapter below.
                    crate::services::discord::tui_prompt_relay::synthetic_start::bridge_handoff::resume_unpublished(&shared, &row, &output)
                        .await.expect("pending retry leaves a resumable original episode");
                    return;
                }
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
                if source_retry {
                    let before = crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get()).unwrap();
                    let mut binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux).unwrap();
                    binding.output_path = temp.path().join("contradictory.jsonl").to_str().unwrap().into();
                    std::fs::write(&binding.output_path, format!("{assistant}\n")).unwrap();
                    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(tmux, binding.clone());
                    let rejected = synthetic_start::claim_tui_direct_synthetic_turn(&shared, &provider, channel, tmux, "handoff prompt", anchor, &lease).await;
                    assert!(!rejected.claimed);
                    let unchanged = crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get()).unwrap();
                    assert_eq!(serde_json::to_value(&before).unwrap(), serde_json::to_value(unchanged).unwrap());
                    binding.output_path = output.to_str().unwrap().into();
                    binding.last_offset = std::fs::metadata(&output).unwrap().len();
                    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(tmux, binding);
                    let retry = synthetic_start::claim_tui_direct_synthetic_turn(&shared, &provider, channel, tmux, "handoff prompt", anchor, &lease).await;
                    assert!(retry.claimed);
                    assert_eq!(retry.turn_start_offset, source_start);
                    let after = crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get()).unwrap();
                    assert_eq!(after.last_offset, before.last_offset);
                    assert_eq!(after.turn_start_offset, before.turn_start_offset);
                }
            });
            if foreign_actor || wrong_source {
                claim.take().unwrap().await;
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
            if let Some(empty_case) = empty_tail {
                let decoded_terminal = empty_case == EmptyTailCase::DecodedTerminal;
                claim.take().unwrap().await;
                let before = crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get()).unwrap();
                let actor = crate::services::discord::mailbox_snapshot(&shared, channel).await.cancel_token.unwrap();
                if decoded_terminal {
                    std::fs::write(&output, format!("{previous}{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"\"}}\n")).unwrap();
                } else if empty_case == EmptyTailCase::DeferredError {
                    let error = serde_json::json!({"type":"result", "subtype":"error_during_execution", "is_error":true, "errors":["DEFERRED_READER_FAILURE"]});
                    std::fs::write(&output, format!("{previous}{error}\n")).unwrap();
                } else { std::fs::remove_file(&output).unwrap(); }
                if decoded_terminal || empty_case == EmptyTailCase::DeferredError {
                    let _fault = (empty_case == EmptyTailCase::DeferredError).then(|| {
                        crate::services::provider::read_fault::after_offset(&output, std::fs::metadata(&output).unwrap().len())
                    });
                    let gateway = Arc::new(S3Gateway { local_delivery: true, ..Default::default() });
                    let (tx, rx) = mpsc::channel();
                    let (end_tx, end_rx) = tokio::sync::oneshot::channel();
                    let reader = spawn_handoff_reader(&output, source_start, tmux, tx, end_tx);
                    let result = tokio::time::timeout(Duration::from_secs(5), claude_idle_bridge::stream_tui_idle_response_with_gateway(
                        &shared, provider.clone(), channel,
                        claude_idle_bridge::IdleBridgeSource {
                            tmux_session_name: tmux, output_path: &output, start_offset: source_start,
                            prompt_text: "handoff prompt", lease: &lease,
                        },
                        (Vec::new(), rx, Some(end_rx)), gateway.clone(), 0,
                    )).await.expect("empty/error reader settles within the reader bound");
                    tokio::task::spawn_blocking(move || reader.join().unwrap()).await.unwrap();
                    if empty_case == EmptyTailCase::DeferredError {
                        assert!(result.is_err(), "a deferred error cannot become a terminal error card");
                        assert!(gateway.bodies.lock().unwrap().iter().all(|body| !body.contains("DEFERRED_READER_FAILURE")));
                        let after = crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get()).unwrap();
                        assert_eq!(after.turn_nonce, before.turn_nonce);
                        assert_eq!(after.turn_start_offset, before.turn_start_offset);
                        assert!(!after.terminal_delivery_committed);
                        assert!(after.full_response.is_empty());
                        assert!(Arc::ptr_eq(&actor, &crate::services::discord::mailbox_snapshot(&shared, channel).await.cancel_token.unwrap()));
                        assert_eq!(crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux).unwrap().last_offset, source_start);
                        assert!(std::fs::read_to_string(&output).unwrap().contains("DEFERRED_READER_FAILURE"));
                        return;
                    }
                    let offset = result.expect("empty terminal guidance is delivered").unwrap();
                    assert_eq!(offset, std::fs::metadata(&output).unwrap().len());
                    assert!(gateway.bodies.lock().unwrap().iter().any(|sent| sent.contains("응답 내용 없이 턴을 종료")),
                        "empty terminal must publish recovery guidance, never a zero-byte success");
                    let record = crate::services::discord::outbound::delivery_record::read_record(&provider, channel.get()).unwrap();
                    assert!(record.confirmed_deliveries.iter().any(|receipt| {
                        receipt.source.turn_nonce == actor.turn_nonce().unwrap()
                            && receipt.source.range == (source_start, offset)
                            && receipt.source.tmux_session_name == tmux
                            && receipt.delivery_channel_id == channel.get()
                            && receipt.message_id == anchor.get()
                    }), "empty guidance has the exact original source/anchor receipt");
                    assert!(crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get()).is_none());
                    assert!(crate::services::discord::mailbox_snapshot(&shared, channel).await.cancel_token.is_none());
                    assert!(crate::services::discord::mailbox_try_start_turn(
                        &shared, channel, Arc::new(CancelToken::new()), serenity::UserId::new(583_300_003), MessageId::new(583_300_004),
                    ).await, "receipted empty guidance releases the next input");
                    return;
                }
                tokio::time::timeout(Duration::from_secs(5), claude_idle_tail::run_claude_idle_response_tail(
                    shared.clone(), tmux.into(), channel, output.clone(), source_start,
                    "handoff prompt".into(), lease.clone(),
                )).await.expect("real reader exits without a busy wait");
                let after = crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get()).unwrap();
                assert_eq!(serde_json::to_value(&before).unwrap(), serde_json::to_value(&after).unwrap());
                assert!(Arc::ptr_eq(&actor, &crate::services::discord::mailbox_snapshot(&shared, channel).await.cancel_token.unwrap()));
                assert_eq!(crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux).unwrap().last_offset, source_start);
                std::fs::write(&output, format!("{previous}{assistant}\n")).unwrap();
                synthetic_start::bridge_handoff::resume_unpublished(&shared, &after, &output).await.unwrap();
            }
            if postgres_race {
                claim.take().unwrap().await;
                let pool = shared.pg_pool.as_ref().unwrap();
                sqlx::query("INSERT INTO sessions(session_key,provider,status,channel_id,active_turn_nonce,dispatched_origin_turn_nonce) VALUES($1,'claude','turn_active',$2,'successor-b','successor-b')")
                    .bind(lease.session_key.as_deref().unwrap()).bind(channel.get().to_string()).execute(pool).await.unwrap();
                let before: (serde_json::Value,) = sqlx::query_as("SELECT to_jsonb(sessions) FROM sessions WHERE session_key=$1")
                    .bind(lease.session_key.as_deref().unwrap()).fetch_one(pool).await.unwrap();
                assert!(synthetic_start::bridge_handoff::capture(&shared, &provider, channel, tmux, &output, &lease).await.is_err());
                let after: (serde_json::Value,) = sqlx::query_as("SELECT to_jsonb(sessions) FROM sessions WHERE session_key=$1")
                    .bind(lease.session_key.as_deref().unwrap()).fetch_one(pool).await.unwrap();
                assert_eq!(before, after, "delayed A adapter must preserve every successor B column");
                assert!(crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get()).is_some());
                return;
            }
            let (original_actor, original_start) = if native_codex {
                // Poll the claim future to establish the inflight row and actor.
                claim.take().unwrap().await;
                // Production enters the bridge once. A diagnostic capture/drop
                // would clear the armed original external-input lease.
                drop(claim);
                let row = crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get()).unwrap();
                let actor = crate::services::discord::mailbox_snapshot(&shared, channel).await.cancel_token.unwrap();
                assert_eq!(row.turn_start_offset, Some(source_start));
                (actor, source_start)
            } else {
            let capture = crate::services::discord::tui_prompt_relay::synthetic_start::bridge_handoff::capture(
                &shared, &provider, channel, tmux, &output, &lease,
            );
            let capture = if failed_save || claim.is_none() {
                if let Some(claim) = claim.take() { claim.await; }
                capture.await
            } else {
                let ((), capture) = tokio::join!(claim.take().unwrap(), capture);
                capture
            };
            drop(claim); // The exhausted future must release its borrow before restart.
            let mut capture = capture.expect("same admitted provider execution reaches bridge");
            if let Some(restart) = recovery {
                // Drop the admitted adapter before it can post a frame; the durable
                // episode must remain recoverable through the same idle retry entry.
                let retained_actor = capture.actor.clone();
                drop(capture);
                if !restart { crate::services::discord::mailbox_finish_turn(&shared, &provider, channel).await; }
                if restart {
                    crate::services::discord::inflight::mark_all_inflight_states_restart_mode(
                        &provider, crate::services::discord::InflightRestartMode::DrainRestart,
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
                if !restart { assert!(Arc::ptr_eq(&capture.actor, &retained_actor)); }
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
            let original_actor = capture.actor.clone();
            let original_start = capture.row.turn_start_offset.unwrap();
            drop(capture);
                (original_actor, original_start)
            };
            let row = crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get()).unwrap();
            let resumed = if native_codex { lease.clone() } else {
                synthetic_start::bridge_handoff::resume_unpublished(&shared, &row, &output).await.unwrap()
            };
            // Match the production TUI gateway: terminal edits use this same
            // transport; a headless fixture would wait on an unrelated outbox.
            let gateway = Arc::new(S3Gateway { local_delivery: true, ..Default::default() });
            let (tx, rx) = mpsc::channel();
            let (end_tx, end_rx) = tokio::sync::oneshot::channel();
            let reader = spawn_handoff_reader(&output, original_start, tmux, tx, end_tx);
            let (rx, first) = tokio::task::spawn_blocking(move || {
                let deadline = std::time::Instant::now() + Duration::from_secs(5);
                let mut prefix = Vec::new();
                loop {
                    let first = rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                        .expect("canonical source reader must emit the opening content frame");
                    let content = idle_stream_message_is_content(&first);
                    prefix.push(first);
                    if content { break; }
                }
                (rx, prefix)
            }).await.unwrap();
            let delivery = async {
            let delivered = claude_idle_bridge::stream_tui_idle_response_with_gateway(
                &shared, provider.clone(), channel,
                claude_idle_bridge::IdleBridgeSource {
                    tmux_session_name: tmux, output_path: &output, start_offset: original_start,
                    prompt_text: "handoff prompt", lease: &resumed,
                },
                (first, rx, Some(end_rx)), gateway.clone(), 0,
            ).await;
            delivered
            };
            let observe = async {
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
                &original_actor
            ));
            assert!(
                crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get())
                    .is_some()
            );
            if native_codex {
                let incomplete_end = std::fs::metadata(&output).unwrap().len();
                assert!(!crate::services::discord::session_relay_sink::idle_range_is_committed(
                    &shared, &provider, channel.get(), tmux,
                    Some((original_start, incomplete_end)),
                    Some(crate::services::discord::turn_bridge::tmux_generation_file_mtime_ns(tmux)),
                ), "streaming an incomplete native response is not a delivery receipt");
            }
            if prefix_read_error {
                use std::io::Write;
                let error = serde_json::json!({"type":"result", "subtype":"error_during_execution", "is_error":true, "errors":["DEFERRED_READER_FAILURE"]});
                std::fs::OpenOptions::new().append(true).open(&output).unwrap()
                    .write_all(format!("{error}\n").as_bytes()).unwrap();
                let _fault = crate::services::provider::read_fault::after_offset(
                    &output, std::fs::metadata(&output).unwrap().len(),
                );
                tokio::task::spawn_blocking(move || reader.join().unwrap()).await.unwrap();
                return;
            }
            use std::io::Write;
            if native_compaction {
                // Native Claude trace 2026-09-05, session 62cf3723, lines 276-277:
                // compact_boundary(trigger=auto) then isCompactSummary, followed
                // by same-session assistant output. Bodies/IDs are synthetic;
                // append the observed shape without inventing file truncation.
                let boundary = serde_json::json!({"type":"system", "subtype":"compact_boundary", "sessionId":"native-auto-session", "uuid":"compact-boundary", "parentUuid":null, "compactMetadata":{"trigger":"auto"}});
                let summary = serde_json::json!({"type":"user", "sessionId":"native-auto-session", "parentUuid":"compact-boundary", "uuid":"compact-summary", "isCompactSummary":true, "isVisibleInTranscriptOnly":true, "message":{"role":"user", "content":"PRIVATE_COMPACT_SUMMARY"}});
                let continuation = serde_json::json!({"type":"assistant", "sessionId":"native-auto-session", "parentUuid":"compact-summary", "message":{"content":[{"type":"text", "text":"NATIVE_COMPACT_CONTINUATION"}], "stop_reason":"end_turn"}});
                std::fs::OpenOptions::new().append(true).open(&output).unwrap()
                    .write_all(format!("{boundary}\n{summary}\n{continuation}\n").as_bytes()).unwrap();
                let edit_bound = crate::services::discord::status_update_interval() + Duration::from_secs(2);
                tokio::time::timeout(edit_bound, async {
                    while !gateway.bodies.lock().unwrap().iter().any(|sent| sent.contains("NATIVE_COMPACT_CONTINUATION")) {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                }).await.expect("continuation is delivered before the adapter terminal");
                assert!(Arc::ptr_eq(
                    &crate::services::discord::mailbox_snapshot(&shared, channel).await.cancel_token.unwrap(),
                    &original_actor,
                ), "native compact metadata and assistant end_turn do not release the actor");
                let active = crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get()).unwrap();
                assert_eq!(active.turn_start_offset, Some(original_start));
                assert!(!active.terminal_delivery_committed);
            }
            // The captured native trace has no terminal hook. Use the existing
            // adapter's stop_hook_summary endpoint explicitly, not as trace proof.
            let terminal = if native_codex {
                serde_json::json!({"type":"event_msg", "payload":{"type":"task_complete", "last_agent_message":body}})
            } else if native_compaction {
                serde_json::json!({"type":"system", "subtype":"stop_hook_summary", "sessionId":"native-auto-session"})
            } else {
                serde_json::json!({"type":"result", "subtype":"success", "result":body})
            };
            std::fs::OpenOptions::new().append(true).open(&output).unwrap()
                .write_all(format!("{terminal}\n").as_bytes()).unwrap();
            tokio::task::spawn_blocking(move || reader.join().unwrap()).await.unwrap();
            };
            let delivery_bound = Duration::from_secs(5) + if native_compaction {
                crate::services::discord::status_update_interval()
            } else { Duration::ZERO };
            let (delivered, ()) = tokio::join!(tokio::time::timeout(delivery_bound, delivery), observe);
            let delivered = delivered.expect("actual adapter must finish within the original test bound");
            if prefix_read_error {
                assert!(delivered.is_err(), "a prefix followed by reader failure is not a terminal");
                let retained = crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get()).expect("reader failure retains its durable obligation");
                assert_eq!(retained.full_response, body);
                assert!(!retained.terminal_delivery_committed);
                assert_eq!(retained.turn_start_offset, Some(original_start));
                assert!(Arc::ptr_eq(&crate::services::discord::mailbox_snapshot(&shared, channel).await.cancel_token.unwrap(), &original_actor));
                assert_eq!(crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux).unwrap().last_offset, source_start);
                // Refresh the same captured actor, then resume at the saved read
                // cursor with its body and confirmed prefix intact.
                let renewed = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(provider.as_str(), tmux, lease.clone());
                assert!(synthetic_start::claim_tui_direct_synthetic_turn(
                    &shared, &provider, channel, tmux, "handoff prompt", anchor, &renewed,
                ).await.claimed);
                use std::io::Write;
                let terminal = serde_json::json!({"type":"result", "subtype":"success", "result":body});
                std::fs::OpenOptions::new().append(true).open(&output).unwrap().write_all(format!("{terminal}\n").as_bytes()).unwrap();
                let (tx, rx) = mpsc::channel();
                let (end_tx, end_rx) = tokio::sync::oneshot::channel();
                let resume_from = {
                    let bodies = gateway.bodies.lock().unwrap();
                    let sites = gateway.sites.lock().unwrap();
                    assert_eq!(bodies.len(), sites.len(), "paired pre-resume history: {sites:?} {bodies:?}");
                    assert!(bodies.iter().all(|sent| !sent.starts_with("-# ") && !sent.contains("DEFERRED_READER_FAILURE")), "reader failure cannot post a terminal summary or error: {sites:?} {bodies:?}");
                    bodies.len()
                };
                let reader = spawn_handoff_reader(&output, retained.last_offset, tmux, tx, end_tx);
                tokio::time::timeout(Duration::from_secs(5), claude_idle_bridge::stream_tui_idle_response_with_gateway(
                    &shared, provider.clone(), channel,
                    claude_idle_bridge::IdleBridgeSource {
                        tmux_session_name: tmux, output_path: &output, start_offset: retained.last_offset,
                        prompt_text: "handoff prompt", lease: &renewed,
                    },
                    (Vec::new(), rx, Some(end_rx)), gateway.clone(), 0,
                )).await.expect("resumed adapter finishes").expect("the original saved prefix and later terminal remain deliverable");
                tokio::task::spawn_blocking(move || reader.join().unwrap()).await.unwrap();
                let bodies = gateway.bodies.lock().unwrap().clone();
                let sites = gateway.sites.lock().unwrap().clone();
                assert_eq!(bodies.len(), sites.len(), "paired resumed history: {sites:?} {bodies:?}");
                let resumed_sites = &sites[resume_from..];
                let resumed_bodies = &bodies[resume_from..];
                assert!(resumed_sites.len() >= 2, "terminal body and summary must both publish: {sites:?} {bodies:?}");
                let terminal_index = resumed_sites.len() - 2;
                // Streaming edits may precede the terminal; only the final replace settles it.
                assert!(resumed_sites[..terminal_index].iter().all(|site| *site == "edit_message"), "no extra terminal replace or POST: {sites:?} {bodies:?}");
                assert_eq!(&resumed_sites[terminal_index..], ["replace_message_with_outcome", "send_message"], "terminal body precedes one summary: {sites:?} {bodies:?}");
                for streamed in &resumed_bodies[..terminal_index] {
                    assert_eq!(streamed, &body, "resumed streaming preserves the saved body: {sites:?} {bodies:?}");
                }
                let terminal_body = &resumed_bodies[terminal_index];
                assert_eq!(terminal_body, &body, "terminal must retain the exact saved Unicode body: {sites:?} {bodies:?}");
                assert_eq!(terminal_body.matches("첫 프레임").count(), 16, "resume must not append the already saved prefix again: {sites:?} {bodies:?}");
                let summary = &resumed_bodies[terminal_index + 1];
                let summary_shape = regex::Regex::new(r"\A-# ✅ (?:[0-9]+초|[0-9]+분(?: [0-9]+초)?|[0-9]+시간 [0-9]+분)\z").unwrap();
                assert!(summary_shape.is_match(summary), "one completed summary after the terminal body: {sites:?} {bodies:?}");
                assert!(bodies.iter().all(|sent| !sent.contains("DEFERRED_READER_FAILURE")), "reader error is never published: {sites:?} {bodies:?}");
            } else {
                delivered.expect("actual idle adapter terminal publication completes");
            }
            if native_compaction {
                let bodies = gateway.bodies.lock().unwrap();
                assert!(bodies.iter().all(|sent| !sent.contains("PRIVATE_COMPACT_SUMMARY")));
                assert!(bodies.iter().any(|sent| {
                    sent.contains(&format!("{body}NATIVE_COMPACT_CONTINUATION"))
                        && sent.matches("NATIVE_COMPACT_CONTINUATION").count() == 1
                        && sent.matches("첫 프레임").count() == 16
                }), "one response preserves the prefix and post-compaction continuation");
            }
            let source_end = std::fs::metadata(&output).unwrap().len();
            if native_codex {
                let generation = crate::services::discord::turn_bridge::tmux_generation_file_mtime_ns(tmux);
                assert!(crate::services::discord::session_relay_sink::idle_range_is_committed(
                    &shared, &provider, channel.get(), tmux,
                    Some((original_start, source_end)), Some(generation),
                ), "the canonical native adapter receipt acknowledges the retained generic idle range");
            }
            let record = crate::services::discord::outbound::delivery_record::read_record(
                &provider, channel.get(),
            ).expect("actual adapter leaves durable delivery evidence before owner release");
            assert!(record.confirmed_deliveries.iter().any(|receipt| {
                receipt.source.provider == provider.as_str()
                    && receipt.source.tmux_session_name == tmux
                    && receipt.source.turn_nonce == original_actor.turn_nonce().unwrap()
                    && receipt.source.range == (original_start, source_end)
                    && receipt.delivery_channel_id == channel.get()
                    && receipt.message_id == anchor.get()
            }), "terminal receipt covers exactly the original JSONL episode on its own anchor");
            assert!(
                crate::services::discord::mailbox_snapshot(&shared, channel)
                    .await
                    .cancel_token
                    .is_none(),
                "bridge releases the captured synthetic actor after publication"
            );
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
        }));
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_delivers_first_frame_and_releases_original_actor() {
    synthetic_bridge_handoff_fixture((
        false, false, false, false, None, false, false, None, false, false, false, None,
    ));
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_waits_for_later_claim_save_then_delivers() {
    synthetic_bridge_handoff_fixture((
        true, false, false, false, None, false, false, None, false, false, false, None,
    ));
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_rejects_same_nonce_different_actor() {
    synthetic_bridge_handoff_fixture((
        false, true, false, false, None, false, false, None, false, false, false, None,
    ));
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_rejects_different_source_without_row_mutation() {
    synthetic_bridge_handoff_fixture((
        false, false, true, false, None, false, false, None, false, false, false, None,
    ));
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_upserts_missing_postgres_session_before_first_frame() {
    synthetic_bridge_handoff_fixture((
        false, false, false, true, None, false, false, None, false, false, false, None,
    ));
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_retries_unpublished_row_after_adapter_drops() {
    synthetic_bridge_handoff_fixture((
        false,
        false,
        false,
        false,
        Some(false),
        false,
        false,
        None,
        false,
        false,
        false,
        None,
    ));
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_restarts_from_persisted_source_after_mailbox_loss() {
    synthetic_bridge_handoff_fixture((
        false,
        false,
        false,
        false,
        Some(true),
        false,
        false,
        None,
        false,
        false,
        false,
        None,
    ));
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_failed_inline_save_retries_original_bytes() {
    synthetic_bridge_handoff_fixture((
        false, false, false, false, None, true, false, None, false, false, false, None,
    ));
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_retry_preserves_original_source_and_cursor() {
    synthetic_bridge_handoff_fixture((
        false, false, false, false, None, false, true, None, false, false, false, None,
    ));
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_reader_error_retains_obligation_then_delivers() {
    synthetic_bridge_handoff_fixture((
        false,
        false,
        false,
        false,
        None,
        false,
        false,
        Some(EmptyTailCase::MissingSource),
        false,
        false,
        false,
        None,
    ));
    synthetic_bridge_handoff_fixture((
        false,
        false,
        false,
        false,
        None,
        false,
        false,
        Some(EmptyTailCase::DeferredError),
        false,
        false,
        false,
        None,
    ));
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_decoded_empty_terminal_releases_only_original_episode() {
    synthetic_bridge_handoff_fixture((
        false,
        false,
        false,
        false,
        None,
        false,
        false,
        Some(EmptyTailCase::DecodedTerminal),
        false,
        false,
        false,
        None,
    ));
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_delayed_pg_adapter_preserves_successor_session() {
    synthetic_bridge_handoff_fixture((
        false, false, false, true, None, false, false, None, true, false, false, None,
    ));
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_inline_admission_rejects_replacement_actor() {
    synthetic_bridge_handoff_fixture((
        false, false, false, false, None, false, false, None, false, true, false, None,
    ));
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_prefix_then_read_error_retains_original_obligation() {
    synthetic_bridge_handoff_fixture((
        false, false, false, false, None, false, false, None, false, false, true, None,
    ));
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_native_auto_compaction_preserves_delivery_and_actor() {
    for provider in [ProviderKind::Claude, ProviderKind::Codex] {
        synthetic_bridge_handoff_fixture((
            false,
            false,
            false,
            false,
            None,
            false,
            false,
            None,
            false,
            false,
            false,
            Some(provider),
        ));
    }
}

#[cfg(unix)]
mod herdr_synthetic_identity_pg_tests {
    //! Hosted synthetic claims keep their observed execution across deferred and post-claim races.
    use super::*;
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::db::dispatched_session_canonical_identity::{
        self as canonical, pg_tests as canonical_fixture,
    };
    use crate::db::dispatched_sessions::hosted_execution::{
        ExpectedExecution, HostedExecution, HostedLocation, HostedOwner, HostedState, ProcessStamp,
        SourceRef,
    };
    use crate::services::discord::{inflight, tui_direct_pending_start as pending};
    use crate::services::tui_prompt_dedupe::{self as dedupe, binding_events};
    use std::io::Write;

    const NODE: &str = "synthetic-recovery-test-node";
    const N1: &str = "synthetic-execution-n1";
    const N2: &str = "synthetic-execution-n2";

    struct Fixture {
        db: TestPostgresDb,
        pool: sqlx::PgPool,
        shared: Arc<SharedData>,
        channel: ChannelId,
        logical: String,
        key: String,
        execution: HostedExecution,
        source: binding_events::SourceId,
    }

    impl Fixture {
        async fn new(root: &Path, channel: ChannelId) -> Self {
            let db = TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(
                pool.clone(),
            ));
            let logical = format!("synthetic-recovery-{}", channel.get());
            dedupe::register_tmux_channel(&logical, channel.get());
            let key = crate::services::discord::adk_session::build_namespaced_session_key(
                &shared.token_hash,
                &ProviderKind::Claude,
                &logical,
            );
            let owner = HostedOwner {
                provider: "claude".into(),
                discord_token_hash: shared.token_hash.clone(),
                channel_id: channel.get().to_string(),
                logical_key: logical.clone(),
                owner_node: NODE.into(),
                runtime_root: crate::config::runtime_root().unwrap().display().to_string(),
            };
            let endpoint = crate::config::session_hosts::herdr_endpoint(channel.get()).unwrap();
            let location = HostedLocation {
                host: "herdr".into(),
                execution_node: NODE.into(),
                endpoint_config_key: endpoint.key,
                socket_addr: endpoint.socket_path.display().to_string(),
                named_session: endpoint.herdr_session,
                pane_id: "test-window:test-pane".into(),
            };
            let execution = HostedExecution {
                schema: 1,
                state: HostedState::Bound,
                execution_nonce: N1.into(),
                owner: owner.clone(),
                location: Some(location),
                expected: Some(ExpectedExecution {
                    binding_provider: "claude".into(),
                    binding_nonce: N1.into(),
                    root: ProcessStamp {
                        pid: 100,
                        start: "linux-ticks:100".into(),
                    },
                    provider_process: ProcessStamp {
                        pid: 101,
                        start: "linux-ticks:101".into(),
                    },
                    provenance: "isolated recorded launch".into(),
                }),
                source_ref: SourceRef {
                    runtime_root: owner.runtime_root.clone(),
                    channel: channel.get().to_string(),
                    provider: "claude".into(),
                    logical_key: logical.clone(),
                    execution_nonce: N1.into(),
                    initial_source: None,
                    baseline_event_seq: None,
                },
            };
            canonical::upsert_hook_session_with_identity_pg(
                &pool,
                crate::db::dispatched_sessions::HookSessionUpsert {
                    instance_id: None,
                    ..canonical_fixture::params(&key, &owner.channel_id)
                },
                Some(canonical::CanonicalSessionIdentity {
                    kind: canonical::SessionIdentityKind::DiscordChannel,
                    discord_token_hash: &shared.token_hash,
                    channel_id: &owner.channel_id,
                }),
            )
            .await
            .unwrap();
            canonical_fixture::set_raw(
                &pool,
                &key,
                Some(serde_json::to_value(&execution).unwrap()),
            )
            .await;
            let path = root.join(format!("{}-native-n1.jsonl", channel.get()));
            std::fs::write(&path, b"").unwrap();
            let source =
                crate::services::tui_o::shadow::binding_reader::source_id_for("native-n1", &path)
                    .unwrap();
            let fixture = Self {
                db,
                pool,
                shared,
                channel,
                logical,
                key,
                execution,
                source,
            };
            fixture.publish_source(&fixture.source, N1, 1);
            let marker =
                crate::services::tmux_common::session_temp_path(&fixture.logical, "host_kind");
            std::fs::write(marker, b"herdr").unwrap();
            fixture
        }

        fn publish_source(&self, source: &binding_events::SourceId, nonce: &str, seq: u64) {
            // The fixture writes its execution history explicitly below; publish the cache without
            // also appending a second event for the same sequence through registration.
            let log_root = binding_events::test_root();
            binding_events::set_test_root(None);
            dedupe::register_tmux_runtime_binding(
                &self.logical,
                dedupe::TuiRuntimeBinding {
                    runtime_kind: RuntimeHandoffKind::ClaudeTui,
                    output_path: source.path.display().to_string(),
                    relay_output_path: None,
                    input_fifo_path: None,
                    session_id: Some(source.session_id.clone()),
                    last_offset: 0,
                    relay_last_offset: None,
                },
            );
            binding_events::set_test_root(log_root.as_deref());
            self.shared.tmux_watchers.insert(
                self.channel,
                crate::services::discord::TmuxWatcherHandle {
                    tmux_session_name: self.logical.clone(),
                    output_path: source.path.display().to_string(),
                    paused: Arc::new(false.into()),
                    resume_offset: Arc::new(Mutex::new(None)),
                    cancel: Arc::new(false.into()),
                    pause_epoch: Arc::new(0.into()),
                    turn_delivered: Arc::new(false.into()),
                    last_heartbeat_ts_ms: Arc::new(chrono::Utc::now().timestamp_millis().into()),
                },
            );
            self.append_source_event(source, nonce, seq);
            dedupe::install_herdr_execution(&self.logical, nonce);
            dedupe::admit_herdr_execution(&self.logical, nonce);
        }

        fn append_source_event(&self, source: &binding_events::SourceId, nonce: &str, seq: u64) {
            let now = chrono::Utc::now();
            let event = binding_events::BindingEvent {
                seq,
                channel_id: self.channel.get(),
                provider: "claude".into(),
                tmux_session: self.logical.clone(),
                execution_nonce: Some(nonce.into()),
                old: None,
                new: binding_events::BindingTarget::Source(source.clone()),
                cause: binding_events::BindingCause::Startup,
                parent_hint: None,
                evidence: binding_events::BindingEvidence {
                    hook_event: Some("SessionStart".into()),
                    received_at: now,
                },
                committed_at: now,
            };
            let dir = binding_events::test_root()
                .unwrap()
                .join(binding_events::BINDING_EVENTS_DIR);
            std::fs::create_dir_all(&dir).unwrap();
            let mut log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join(format!("{}.log", self.channel.get())))
                .unwrap();
            writeln!(log, "{}", serde_json::to_string(&event).unwrap()).unwrap();
            log.sync_all().unwrap();
            let history = binding_events::records_strict(self.channel.get())
                .unwrap()
                .unwrap();
            assert_eq!(
                history.len() as u64,
                seq,
                "fixture history must have one event per sequence"
            );
            assert_eq!(
                history.last().unwrap().execution_nonce.as_deref(),
                Some(nonce)
            );
        }

        fn pending(
            &self,
            nonce: &str,
            source: &binding_events::SourceId,
            anchor: u64,
        ) -> pending::TuiDirectPendingStart {
            // Main ignores this additive carrier; the implemented decoder preserves it unchanged.
            serde_json::from_value(serde_json::json!({
                "provider": "claude", "channel_id": self.channel.get(),
                "tmux_session_name": self.logical, "prompt_text": "already submitted input",
                "anchor_message_id": anchor, "lease_relay_owner": "tmux_watcher",
                "lease_runtime_kind": "claude_tui", "lease_turn_id": format!("{nonce}-native-turn"),
                "lease_session_key": self.key, "generation": self.shared.restart.current_generation,
                "created_at_ms": 0, "observed_at_ms": 0,
                "captured_source": [source.path.display().to_string(), 0],
                "hosted_source_checked": true,
                "hosted_source": {
                    "provider": "claude", "channel_id": self.channel.get(), "logical_key": self.logical,
                    "execution_nonce": nonce, "source": source
                }
            }))
            .unwrap()
        }

        async fn replace_execution(&self, state: HostedState, nonce: &str) -> HostedExecution {
            let mut next = self.execution.clone();
            next.state = state;
            next.execution_nonce = nonce.into();
            next.source_ref.execution_nonce = nonce.into();
            next.expected.as_mut().unwrap().binding_nonce = nonce.into();
            canonical_fixture::set_raw(
                &self.pool,
                &self.key,
                Some(serde_json::to_value(&next).unwrap()),
            )
            .await;
            next
        }

        async fn hosted_json(&self) -> serde_json::Value {
            canonical_fixture::raw_of(&self.pool, &self.key)
                .await
                .expect("the fixture has a hosted execution")
        }

        async fn claim(&self, record: &pending::TuiDirectPendingStart) -> bool {
            let claim = synthetic_start::pending_start_claim_fn();
            claim(&self.shared, record).await
        }

        fn publish_pending_lease(
            &self,
            record: &pending::TuiDirectPendingStart,
        ) -> ExternalInputRelayLease {
            let mut lease = ExternalInputRelayLease::unassigned(Some(self.channel.get()));
            lease.turn_id = record.lease_turn_id.clone();
            lease.session_key = record.lease_session_key.clone();
            lease.relay_owner = ExternalInputRelayOwner::TmuxWatcher;
            lease.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
            lease.hosted_source = record.hosted_source.clone();
            dedupe::record_external_input_turn_lease("claude", &self.logical, lease)
        }

        async fn assert_quiet(&self) {
            assert!(
                crate::services::discord::mailbox_snapshot(&self.shared, self.channel)
                    .await
                    .cancel_token
                    .is_none(),
                "a refused hosted claim left an actor"
            );
            assert!(
                inflight::load_inflight_state_read_only(&ProviderKind::Claude, self.channel.get())
                    .is_none(),
                "a refused hosted claim saved inflight"
            );
        }

        async fn close(self) {
            self.pool.close().await;
            self.db.drop().await;
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hook_observer_carries_the_original_hosted_source_through_pending_json_restore_pg() {
        use crate::services::claude_tui::hook_server::{HookEvent, HookEventKind};

        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let _binding = crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(
            root.path(),
        );
        let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let channel = ChannelId::new(7_186_502);
        let _hosts =
            crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
        let fixture = Fixture::new(root.path(), channel).await;
        let (hooks, hook_rx) = tokio::sync::broadcast::channel(4);
        let (observed_tx, mut observed_rx) = tokio::sync::mpsc::unbounded_channel();
        let expected_logical = fixture.logical.clone();
        super::super::super::hook_observer::spawn_tui_prompt_relay_observer(
            "claude".into(),
            hook_rx,
            move |prompt| {
                let observed_tx = observed_tx.clone();
                let expected_logical = expected_logical.clone();
                Box::pin(async move {
                    if prompt.tmux_session_name == expected_logical {
                        observed_tx.send(prompt).expect("live observation receiver");
                    }
                })
            },
        );
        hooks
            .send(HookEvent {
                provider: "claude".into(),
                session_id: fixture.logical.clone(),
                kind: HookEventKind::UserPromptSubmit,
                received_at: chrono::Utc::now(),
                payload: serde_json::json!({
                    "session_id": fixture.source.session_id,
                    "transcript_path": fixture.source.path.display().to_string(),
                    "prompt": "retain this hook's original hosted source",
                    "prompt_id": "original-hosted-source-hook"
                }),
                fanout: None,
                original_context: None,
            })
            .expect("the production observer subscribed synchronously");
        let observed = tokio::time::timeout(Duration::from_secs(5), observed_rx.recv())
            .await
            .expect("the real hook observer produced a relay observation")
            .expect("observed prompt");
        assert_eq!(observed.tmux_session_name, fixture.logical);
        assert_eq!(observed.prompt, "retain this hook's original hosted source");
        let lease = record_observed_external_turn_lease(&fixture.shared, &observed, channel);
        assert_eq!(lease.session_key.as_deref(), Some(fixture.key.as_str()));
        assert!(
            lease.turn_id.is_some(),
            "the hook assigned an original turn identity"
        );
        let worker_lock = pending::channel_lock("claude", channel.get())
            .lock_owned()
            .await;
        synthetic_start::defer_synthetic_turn_start(
            &fixture.shared,
            &ProviderKind::Claude,
            channel,
            &observed,
            MessageId::new(7_186_503),
            &lease,
            Some((fixture.source.path.display().to_string(), 0)),
        );
        let saved = pending::load_all()
            .into_iter()
            .find(|record| record.channel_id == channel.get())
            .expect("defer persisted its original obligation before spawning the blocked worker");
        let serialized = serde_json::to_value(&saved).unwrap();
        assert_eq!(
            serialized["hosted_source"]["execution_nonce"],
            serde_json::json!(N1),
            "the real hook, observed lease and pending writer lost the original execution carrier"
        );
        assert_eq!(
            serialized["hosted_source"]["source"],
            serde_json::to_value(&fixture.source).unwrap()
        );
        let restored: pending::TuiDirectPendingStart =
            serde_json::from_value(serialized.clone()).unwrap();
        assert_eq!(
            restored, saved,
            "the durable pending decoder preserves the original source pin"
        );
        assert_eq!(
            serde_json::to_value(restored).unwrap()["hosted_source"],
            serialized["hosted_source"]
        );
        fixture.assert_quiet().await;
        drop(worker_lock);
        let claimed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let inflight =
                    inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get());
                let still_pending = pending::load_all().iter().any(|record| {
                    record.channel_id == channel.get()
                        && record.anchor_message_id == saved.anchor_message_id
                });
                if let Some(inflight) = inflight.filter(|_| !still_pending) {
                    break inflight;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect(
            "the real deferred worker claimed, saved inflight and completed its pending record",
        );
        assert_eq!(claimed.hosted_execution_nonce.as_deref(), Some(N1));
        assert_eq!(
            claimed.session_id.as_deref(),
            Some(fixture.source.session_id.as_str())
        );
        assert_eq!(claimed.output_path.as_deref(), fixture.source.path.to_str());
        assert_eq!(claimed.external_turn_id, lease.turn_id);
        assert_eq!(claimed.user_msg_id, saved.anchor_message_id);
        drop(hooks);
        fixture.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn modern_unattributed_hook_pending_cannot_adopt_a_source_discovered_after_inline_refusal_pg()
     {
        use crate::services::claude_tui::hook_server::{HookEvent, HookEventKind};

        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let _binding = crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(
            root.path(),
        );
        let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let channel = ChannelId::new(7_186_652);
        let anchor = MessageId::new(7_186_653);
        let _hosts =
            crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
        let fixture = Fixture::new(root.path(), channel).await;
        let (hooks, hook_rx) = tokio::sync::broadcast::channel(4);
        let (observed_tx, mut observed_rx) = tokio::sync::mpsc::unbounded_channel();
        let expected_logical = fixture.logical.clone();
        super::super::super::hook_observer::spawn_tui_prompt_relay_observer(
            "claude".into(),
            hook_rx,
            move |prompt| {
                let observed_tx = observed_tx.clone();
                let expected_logical = expected_logical.clone();
                Box::pin(async move {
                    if prompt.tmux_session_name == expected_logical {
                        observed_tx.send(prompt).expect("live observation receiver");
                    }
                })
            },
        );
        hooks
            .send(HookEvent {
                provider: "claude".into(),
                session_id: fixture.logical.clone(),
                kind: HookEventKind::UserPromptSubmit,
                received_at: chrono::Utc::now(),
                payload: serde_json::json!({
                    "prompt": "input observed before the native source could be proved",
                    "prompt_id": "unattributed-before-new-execution"
                }),
                fanout: None,
                original_context: None,
            })
            .expect("the production hook observer is subscribed");
        let observed = tokio::time::timeout(Duration::from_secs(5), observed_rx.recv())
            .await
            .expect("the actual hook producer published the unattributed original input")
            .expect("observed original input");
        assert!(observed.hosted_source.is_none());
        let lease = record_observed_external_turn_lease(&fixture.shared, &observed, channel);
        assert!(lease.hosted_source.is_none());
        assert!(lease.turn_id.is_some());

        fixture.replace_execution(HostedState::Bound, N2).await;
        let path = root
            .path()
            .join("new-execution-after-unattributed-hook.jsonl");
        std::fs::write(&path, b"").unwrap();
        let source =
            crate::services::tui_o::shadow::binding_reader::source_id_for("native-n2", &path)
                .unwrap();
        fixture.publish_source(&source, N2, 2);
        let (_, watcher) = fixture
            .shared
            .tmux_watchers
            .remove(&channel)
            .expect("the fixture's watcher metadata exists before inline fallback");
        let (inline, captured_source) =
            synthetic_start::claim_tui_direct_synthetic_turn_inner::<false>(
                &fixture.shared,
                &ProviderKind::Claude,
                channel,
                &fixture.logical,
                (&observed.prompt, anchor),
                &lease,
                (None, observed.native_turn_id.as_deref()),
            )
            .await;
        assert!(
            !inline.claimed,
            "the inline fence refused the unproved original input"
        );
        assert_eq!(inline.relay_owner, ExternalInputRelayOwner::BridgeAdapter);
        assert_eq!(
            captured_source.as_ref().map(|(path, _)| path.as_str()),
            source.path.to_str(),
            "inline fallback observed N2 only after the original hook had no source pin"
        );
        fixture.assert_quiet().await;
        let worker_lock = pending::channel_lock("claude", channel.get())
            .lock_owned()
            .await;
        synthetic_start::defer_synthetic_turn_start(
            &fixture.shared,
            &ProviderKind::Claude,
            channel,
            &observed,
            anchor,
            &lease,
            captured_source.clone(),
        );
        let deferred = pending::load_all()
            .into_iter()
            .find(|record| record.channel_id == channel.get())
            .expect("the real defer constructor persisted the original input");
        assert_eq!(deferred.captured_source, captured_source);
        assert_eq!(deferred.lease_turn_id, lease.turn_id);
        assert!(serde_json::to_value(&deferred).unwrap()["hosted_source"].is_null());
        fixture.shared.tmux_watchers.insert(channel, watcher);
        drop(worker_lock);

        let (saved, retained) = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let saved =
                    inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get());
                let retained = pending::load_all()
                    .into_iter()
                    .find(|record| record.channel_id == channel.get());
                if saved.is_some()
                    || retained
                        .as_ref()
                        .is_some_and(|record| record.attempt_count >= 1)
                {
                    break (saved, retained);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the actual deferred worker either claimed or completed a refused claim attempt");
        assert!(
            saved.is_none(),
            "the modern unattributed hook was restamped from N2's later captured path and saved"
        );
        fixture.assert_quiet().await;
        let retained = retained.expect("the unresolved original input remains pending");
        assert!(retained.attempt_count >= 1);
        assert!(serde_json::to_value(&retained).unwrap()["hosted_source"].is_null());
        assert_eq!(retained.lease_turn_id, lease.turn_id);

        let worker_lock = tokio::time::timeout(
            Duration::from_secs(5),
            pending::channel_lock("claude", channel.get()).lock_owned(),
        )
        .await
        .expect("the refused old worker exhausted its bounded retry budget");
        fixture.assert_quiet().await;
        let current = fixture.pending(N2, &source, 7_186_654);
        fixture.publish_pending_lease(&current);
        assert!(
            fixture.claim(&current).await,
            "a separately proved N2 input remains claimable while the unknown original stays pending"
        );
        let saved =
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).unwrap();
        assert_eq!(saved.hosted_execution_nonce.as_deref(), Some(N2));
        assert_eq!(saved.user_msg_id, current.anchor_message_id);
        assert!(pending::load_all().iter().any(|record| {
            record.channel_id == channel.get() && record.anchor_message_id == anchor.get()
        }));
        drop(hooks);
        fixture.close().await;
        drop(worker_lock);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn admitted_herdr_pending_worker_waits_for_recovery_transition_then_claims_pg() {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let _binding = crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(
            root.path(),
        );
        let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let channel = ChannelId::new(7_186_212);
        let _hosts =
            crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
        let fixture = Fixture::new(root.path(), channel).await;
        let record = fixture.pending(N1, &fixture.source, 7_186_213);
        pending::persist(&record).unwrap();
        assert!(
            dedupe::herdr_claim_admission(&fixture.logical, true).is_ok(),
            "the original Herdr execution is admitted before contention"
        );
        let transition = fixture
            .shared
            .session_transition_lock(channel)
            .lock_owned()
            .await;
        let production_claim = Arc::new(synthetic_start::pending_start_claim_fn());
        let first_return = Arc::new(tokio::sync::Notify::new());
        let (result_tx, mut result_rx) = tokio::sync::mpsc::unbounded_channel();
        let claim_fn: pending::ClaimFn = {
            let first_return = first_return.clone();
            Box::new(move |shared, record| {
                let production_claim = production_claim.clone();
                let first_return = first_return.clone();
                let result_tx = result_tx.clone();
                Box::pin(async move {
                    let claimed = production_claim(shared, record).await;
                    result_tx.send(claimed).expect("live test receiver");
                    if !claimed {
                        first_return.notified().await;
                    }
                    claimed
                })
            })
        };
        pending::spawn_worker(
            fixture.shared.clone(),
            record.clone(),
            synthetic_start::pending_start_view_fn(),
            claim_fn,
            synthetic_start::pending_start_abort_cleanup_fn(),
            synthetic_orphan_reclaim::pending_start_reclaim_orphan_fn(),
        );
        let first = tokio::time::timeout(Duration::from_secs(5), result_rx.recv())
            .await
            .expect("actual admitted Herdr worker attempted its production claim")
            .expect("claim result");
        assert!(
            !first,
            "the admitted Herdr worker claimed through recovery's transition"
        );
        fixture.assert_quiet().await;
        assert!(
            pending::load_all().iter().any(|saved| saved == &record),
            "the original pending source remains durable while excluded"
        );
        assert!(
            dedupe::herdr_claim_admission(&fixture.logical, true).is_ok(),
            "contention did not turn admission off to manufacture the rejection"
        );
        drop(transition);
        first_return.notify_one();
        let retry = tokio::time::timeout(Duration::from_secs(5), result_rx.recv())
            .await
            .expect("the same admitted worker retried after exclusion")
            .expect("retry result");
        assert!(
            retry,
            "the valid original pin must pass once the transition is released"
        );
        let saved =
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).unwrap();
        assert_eq!(saved.hosted_execution_nonce.as_deref(), Some(N1));
        assert_eq!(saved.user_msg_id, record.anchor_message_id);
        fixture.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_pinned_known_tmux_pending_worker_keeps_claiming_after_source_rotation_pg() {
        use dedupe::hosted_source::tests::{
            CHANNEL as SOURCE_CHANNEL, Fixture as SourceFixture, LOGICAL as SOURCE_LOGICAL,
            N1 as SOURCE_N1, N2 as SOURCE_N2,
        };

        let source_fixture = SourceFixture::new();
        let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let _hosts = crate::config::session_hosts::force_for_test(None, &[]);
        crate::services::tmux_common::host_marker::record_tmux_host_marker(SOURCE_LOGICAL);
        let original = source_fixture.source(SOURCE_N1, "original-tmux-transcript");
        let pin = dedupe::hosted_source::capture(
            "claude",
            SOURCE_LOGICAL,
            SOURCE_CHANNEL,
            &original.path,
            Some(&original.session_id),
        )
        .unwrap();
        assert!(!pin.hosted_admission_required);
        assert!(crate::config::session_hosts::herdr_endpoint(SOURCE_CHANNEL).is_none());
        assert!(!dedupe::herdr_execution_listed(SOURCE_LOGICAL));
        dedupe::register_tmux_runtime_binding(
            SOURCE_LOGICAL,
            dedupe::TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: original.path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(original.session_id.clone()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let channel = ChannelId::new(SOURCE_CHANNEL);
        let channel_key = SOURCE_CHANNEL.to_string();
        let key = crate::services::discord::adk_session::build_namespaced_session_key(
            &shared.token_hash,
            &ProviderKind::Claude,
            SOURCE_LOGICAL,
        );
        canonical::upsert_hook_session_with_identity_pg(
            &pool,
            crate::db::dispatched_sessions::HookSessionUpsert {
                instance_id: None,
                ..canonical_fixture::params(&key, &channel_key)
            },
            Some(canonical::CanonicalSessionIdentity {
                kind: canonical::SessionIdentityKind::DiscordChannel,
                discord_token_hash: &shared.token_hash,
                channel_id: &channel_key,
            }),
        )
        .await
        .unwrap();
        assert!(canonical_fixture::raw_of(&pool, &key).await.is_none());
        let record: pending::TuiDirectPendingStart = serde_json::from_value(serde_json::json!({
            "provider": "claude", "channel_id": SOURCE_CHANNEL,
            "tmux_session_name": SOURCE_LOGICAL, "prompt_text": "an input waiting across rotation",
            "anchor_message_id": SOURCE_CHANNEL + 1, "lease_relay_owner": "tmux_watcher",
            "lease_runtime_kind": "claude_tui", "lease_turn_id": "tmux-original-observed-turn",
            "lease_session_key": key, "generation": shared.restart.current_generation,
            "created_at_ms": 0, "observed_at_ms": 0, "captured_source": null,
            "hosted_source_checked": true, "hosted_source": pin.clone()
        }))
        .unwrap();
        pending::persist(&record).unwrap();
        let next = source_fixture.source(SOURCE_N2, "rotated-tmux-transcript");
        dedupe::register_tmux_runtime_binding(
            SOURCE_LOGICAL,
            dedupe::TuiRuntimeBinding {
                runtime_kind: RuntimeHandoffKind::ClaudeTui,
                output_path: next.path.display().to_string(),
                relay_output_path: None,
                input_fifo_path: None,
                session_id: Some(next.session_id.clone()),
                last_offset: 0,
                relay_last_offset: None,
            },
        );
        shared.tmux_watchers.insert(
            channel,
            crate::services::discord::session_relay_sink::tests::idle_relay_harness::live_watcher(
                SOURCE_LOGICAL,
                next.path.to_str().unwrap(),
            ),
        );
        assert_ne!(original.path, next.path);
        assert_eq!(record.hosted_source.as_ref(), Some(&pin));
        pending::spawn_worker(
            shared.clone(),
            record.clone(),
            synthetic_start::pending_start_view_fn(),
            synthetic_start::pending_start_claim_fn(),
            synthetic_start::pending_start_abort_cleanup_fn(),
            synthetic_orphan_reclaim::pending_start_reclaim_orphan_fn(),
        );
        let (saved, retained) = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let saved =
                    inflight::load_inflight_state_read_only(&ProviderKind::Claude, SOURCE_CHANNEL);
                let retained = pending::load_all()
                    .into_iter()
                    .find(|saved| saved.channel_id == SOURCE_CHANNEL);
                if (saved.is_some() && retained.is_none())
                    || retained
                        .as_ref()
                        .is_some_and(|saved| saved.attempt_count >= 1)
                {
                    break (saved, retained);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the actual deferred worker completed a production claim attempt");
        assert!(
            saved.is_some() && retained.is_none(),
            "a known non-Herdr pinned input must keep the base deferred claim after source rotation"
        );
        let saved = saved.unwrap();
        assert_eq!(saved.user_msg_id, record.anchor_message_id);
        assert_eq!(saved.output_path.as_deref(), next.path.to_str());
        assert!(saved.hosted_record_id.is_none());
        assert!(saved.hosted_execution_nonce.is_none());
        assert_eq!(
            dedupe::external_input_relay_lease("claude", SOURCE_LOGICAL, SOURCE_CHANNEL)
                .unwrap()
                .hosted_source,
            Some(pin)
        );
        pool.close().await;
        db.drop().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_pinned_herdr_pending_refuses_a_rotated_current_source_pg() {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let _binding = crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(
            root.path(),
        );
        let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let channel = ChannelId::new(7_186_782);
        let _hosts =
            crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
        let fixture = Fixture::new(root.path(), channel).await;
        let mut record = fixture.pending(N1, &fixture.source, 7_186_783);
        record.captured_source = None;
        assert!(
            record
                .hosted_source
                .as_ref()
                .unwrap()
                .hosted_admission_required
        );
        let original_pin = record.hosted_source.clone();
        let hosted_before = fixture.hosted_json().await;
        let path = root
            .path()
            .join("rotated-source-in-the-same-hosted-execution.jsonl");
        std::fs::write(&path, b"").unwrap();
        let next = crate::services::tui_o::shadow::binding_reader::source_id_for(
            "rotated-native-session",
            &path,
        )
        .unwrap();
        fixture.publish_source(&next, N1, 2);
        assert!(dedupe::herdr_claim_admission(&fixture.logical, true).is_ok());
        assert_eq!(fixture.hosted_json().await, hosted_before);
        assert!(
            !fixture.claim(&record).await,
            "Herdr must still refuse the original pin when its current transcript rotates"
        );
        fixture.assert_quiet().await;
        assert_eq!(record.hosted_source, original_pin);
        assert_eq!(fixture.hosted_json().await, hosted_before);
        fixture.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_pending_claim_from_n1_refuses_retired_n1_and_bound_n2_but_accepts_n2_pg() {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let _binding = crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(
            root.path(),
        );
        let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let channel = ChannelId::new(7_186_302);
        let _hosts =
            crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
        let fixture = Fixture::new(root.path(), channel).await;
        let original = fixture.pending(N1, &fixture.source, 7_186_303);
        let retired = fixture.replace_execution(HostedState::Retired, N1).await;
        assert!(
            !fixture.claim(&original).await,
            "the production pending claim admitted Retired N1"
        );
        fixture.assert_quiet().await;
        assert_eq!(
            fixture.hosted_json().await,
            serde_json::to_value(retired).unwrap()
        );

        let n2 = fixture.replace_execution(HostedState::Bound, N2).await;
        dedupe::install_herdr_execution(&fixture.logical, N2);
        dedupe::admit_herdr_execution(&fixture.logical, N2);
        assert!(
            dedupe::herdr_claim_admission(&fixture.logical, true).is_ok(),
            "N2 is admitted, rather than pre-blocked by a false admission flag"
        );
        assert!(
            !fixture.claim(&original).await,
            "old source N1 adopted the new admitted N2 execution"
        );
        fixture.assert_quiet().await;
        assert_eq!(
            fixture.hosted_json().await,
            serde_json::to_value(n2).unwrap()
        );

        let path = root.path().join("native-n2.jsonl");
        std::fs::write(&path, b"").unwrap();
        let source =
            crate::services::tui_o::shadow::binding_reader::source_id_for("native-n2", &path)
                .unwrap();
        fixture.publish_source(&source, N2, 2);
        let current = fixture.pending(N2, &source, 7_186_304);
        fixture.publish_pending_lease(&original);
        assert!(
            fixture.claim(&current).await,
            "the valid N2 source must remain claimable"
        );
        let saved =
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).unwrap();
        assert_eq!(saved.user_msg_id, current.anchor_message_id);
        assert_eq!(saved.output_path.as_deref(), source.path.to_str());
        assert_eq!(saved.hosted_execution_nonce.as_deref(), Some(N2));
        fixture.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn explicit_n2_pending_resumes_a_shared_native_source_without_adopting_n1_or_guessing_old_origin_pg()
     {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let _binding = crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(
            root.path(),
        );
        let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let channel = ChannelId::new(7_186_632);
        let _hosts =
            crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
        let fixture = Fixture::new(root.path(), channel).await;
        let original = fixture.pending(N1, &fixture.source, 7_186_633);
        let mut old_encoded = serde_json::to_value(&original).unwrap();
        old_encoded.as_object_mut().unwrap().remove("hosted_source");
        old_encoded
            .as_object_mut()
            .unwrap()
            .remove("hosted_source_checked");
        let old: pending::TuiDirectPendingStart = serde_json::from_value(old_encoded).unwrap();
        fixture.replace_execution(HostedState::Bound, N2).await;
        fixture.publish_source(&fixture.source, N2, 2);
        assert!(
            dedupe::herdr_claim_admission(&fixture.logical, true).is_ok(),
            "the resumed execution is admitted throughout"
        );
        assert!(
            !fixture.claim(&original).await,
            "an explicit original N1 pin cannot claim current N2 even when its native file is unchanged"
        );
        fixture.assert_quiet().await;
        assert!(
            !fixture.claim(&old).await,
            "an old record with the shared native file cannot guess which execution observed it"
        );
        fixture.assert_quiet().await;

        let resumed = fixture.pending(N2, &fixture.source, 7_186_634);
        fixture.publish_pending_lease(&resumed);
        assert!(
            fixture.claim(&resumed).await,
            "an explicit current N2 pin must validate within its own durable event history"
        );
        let saved =
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).unwrap();
        assert_eq!(saved.hosted_execution_nonce.as_deref(), Some(N2));
        assert_eq!(saved.output_path.as_deref(), fixture.source.path.to_str());
        assert_eq!(
            saved.session_id.as_deref(),
            Some(fixture.source.session_id.as_str())
        );
        fixture.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn legacy_unpinned_pending_holds_a_later_n2_source_then_releases_the_input_gate_pg() {
        let root = tempfile::tempdir().unwrap();
        let log_path = root.path().join("legacy-origin-hold.log");
        crate::logging::test_capture::pin_callsite_interest();
        let _capture = tracing::subscriber::set_default(
            tracing_subscriber::fmt()
                .with_ansi(false)
                .without_time()
                .with_writer(std::fs::File::create(&log_path).unwrap())
                .finish(),
        );
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let _binding = crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(
            root.path(),
        );
        let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let channel = ChannelId::new(7_186_672);
        let anchor = 7_186_673;
        let _hosts =
            crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
        let fixture = Fixture::new(root.path(), channel).await;
        fixture.replace_execution(HostedState::Bound, N2).await;
        let path = root.path().join("later-n2-source-in-legacy-pending.jsonl");
        std::fs::write(&path, b"").unwrap();
        let source =
            crate::services::tui_o::shadow::binding_reader::source_id_for("native-n2", &path)
                .unwrap();
        fixture.publish_source(&source, N2, 2);
        let history = binding_events::records_strict(channel.get())
            .unwrap()
            .unwrap();
        assert_eq!(
            history
                .iter()
                .filter(|event| matches!(&event.new,
                    binding_events::BindingTarget::Source(found) if found == &source))
                .count(),
            1,
            "the later captured path has unique N2 history, which cannot prove the old input"
        );
        let hosted_before = fixture.hosted_json().await;
        let mut encoded = serde_json::to_value(fixture.pending(N1, &source, anchor)).unwrap();
        let fields = encoded.as_object_mut().unwrap();
        fields.remove("hosted_source");
        fields.remove("hosted_source_checked");
        assert!(!fields.contains_key("hosted_source"));
        assert!(!fields.contains_key("hosted_source_checked"));
        let old: pending::TuiDirectPendingStart = serde_json::from_value(encoded.clone()).unwrap();
        assert!(old.hosted_source.is_none());
        assert!(!old.hosted_source_checked);
        assert_eq!(
            old.captured_source.as_ref().map(|source| source.0.as_str()),
            path.to_str()
        );
        pending::persist(&old).unwrap();
        let old_path = crate::services::discord::runtime_store::tui_direct_pending_start_root()
            .unwrap()
            .join(format!("claude_{}_{anchor}.json", channel.get()));
        std::fs::write(&old_path, serde_json::to_vec(&encoded).unwrap()).unwrap();
        let restored = pending::load_all()
            .into_iter()
            .find(|record| record.channel_id == channel.get())
            .expect("the production reader restored JSON predating both source fields");
        assert_eq!(restored, old);
        assert_eq!(restored.attempt_count, 0);
        assert!(pending::pending_synthetic_start_blocks_idle_kickoff(
            "claude",
            channel.get()
        ));
        let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let production_claim = Arc::new(synthetic_start::pending_start_claim_fn());
        let claim_fn: pending::ClaimFn = {
            let entered = entered.clone();
            let completed = completed.clone();
            Box::new(move |shared, record| {
                let entered = entered.clone();
                let completed = completed.clone();
                let production_claim = production_claim.clone();
                Box::pin(async move {
                    entered.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let claimed = production_claim(shared, record).await;
                    completed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    claimed
                })
            })
        };
        pending::spawn_worker(
            fixture.shared.clone(),
            restored,
            synthetic_start::pending_start_view_fn(),
            claim_fn,
            synthetic_start::pending_start_abort_cleanup_fn(),
            synthetic_orphan_reclaim::pending_start_reclaim_orphan_fn(),
        );
        let (saved, retained) = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let saved =
                    inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get());
                let retained = pending::load_all()
                    .into_iter()
                    .find(|record| record.channel_id == channel.get());
                if saved.is_some()
                    || retained.as_ref().is_some_and(|record| {
                        record.attempt_count >= pending::PENDING_START_MAX_CLAIM_ATTEMPTS
                    })
                {
                    break (saved, retained);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the actual worker either claimed or exhausted all five production claim attempts");
        assert!(
            saved.is_none(),
            "legacy input adopted the later N2 source from path history alone"
        );
        fixture.assert_quiet().await;
        assert_eq!(
            entered.load(std::sync::atomic::Ordering::SeqCst),
            pending::PENDING_START_MAX_CLAIM_ATTEMPTS as usize
        );
        assert_eq!(
            completed.load(std::sync::atomic::Ordering::SeqCst),
            pending::PENDING_START_MAX_CLAIM_ATTEMPTS as usize
        );
        let logs = std::fs::read_to_string(&log_path).unwrap();
        let holds: Vec<_> = logs
            .lines()
            .filter(|line| line.contains("legacy_herdr_pending_origin_unproven"))
            .collect();
        assert_eq!(
            holds.len(),
            1,
            "one worker reports its bounded Hold retries once"
        );
        for line in holds {
            assert!(line.contains("INFO"));
            assert!(line.contains(&format!("channel_id={}", channel.get())));
            assert!(line.contains(&format!("pending_id={anchor}")));
            assert!(line.contains("captured_path_present=true"));
        }
        let retained = retained.expect("the original unproved input remains durable");
        assert_eq!(
            retained.attempt_count,
            pending::PENDING_START_MAX_CLAIM_ATTEMPTS
        );
        assert!(retained.hosted_source.is_none());
        assert!(!retained.hosted_source_checked);
        assert_eq!(retained.captured_source, old.captured_source);
        assert_eq!(retained.lease_turn_id, old.lease_turn_id);
        assert_eq!(fixture.hosted_json().await, hosted_before);
        assert!(pending::pending_synthetic_start_present(
            "claude",
            channel.get()
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            while pending::pending_synthetic_start_blocks_idle_kickoff("claude", channel.get()) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the existing gate releases PRESENT after the actual worker has stopped");
        assert!(!pending::pending_synthetic_start_present(
            "claude",
            channel.get()
        ));
        assert!(pending::pending_synthetic_start_abandoned(
            "claude",
            channel.get()
        ));
        assert_eq!(
            pending::load_all()
                .into_iter()
                .find(|record| record.channel_id == channel.get()),
            Some(retained),
            "releasing the next-input gate does not discard or restamp the held old input"
        );
        let current = fixture.pending(N2, &source, anchor + 1);
        assert!(current.hosted_source_checked);
        fixture.publish_pending_lease(&current);
        assert!(
            fixture.claim(&current).await,
            "a normally proved N2 input still claims after gate release"
        );
        let saved =
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).unwrap();
        assert_eq!(saved.hosted_execution_nonce.as_deref(), Some(N2));
        assert_eq!(saved.user_msg_id, current.anchor_message_id);
        assert!(
            old_path.exists(),
            "the unresolved old record remains available for inspection"
        );
        fixture.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn old_pending_with_an_exact_original_source_still_claims_current_n1_pg() {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let _binding = crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(
            root.path(),
        );
        let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let channel = ChannelId::new(7_186_602);
        let _hosts =
            crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
        let fixture = Fixture::new(root.path(), channel).await;
        let mut original = fixture.pending(N1, &fixture.source, 7_186_603);
        original.hosted_source_checked = false;
        assert!(original.hosted_source.is_some());
        assert!(fixture.claim(&original).await);
        let before =
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).unwrap();
        let actor = crate::services::discord::mailbox_snapshot(&fixture.shared, channel)
            .await
            .cancel_token
            .expect("the original source established its own actor");
        let active = fixture
            .shared
            .restart
            .global_active
            .load(std::sync::atomic::Ordering::Relaxed);
        assert!(before.hosted_record_id.is_some_and(|id| id > 0));
        assert_eq!(before.turn_source, inflight::TurnSource::ExternalInput);
        assert_eq!(before.external_turn_id, original.lease_turn_id);
        assert!(
            before
                .external_turn_id
                .as_ref()
                .is_some_and(|id| !id.is_empty())
        );
        assert_eq!(before.session_key.as_deref(), Some(fixture.key.as_str()));
        assert_eq!(before.output_path.as_deref(), fixture.source.path.to_str());
        assert_eq!(before.hosted_execution_nonce.as_deref(), Some(N1));
        assert_eq!(
            before.session_id.as_deref(),
            Some(fixture.source.session_id.as_str())
        );
        let mut encoded = serde_json::to_value(&original).unwrap();
        let encoded_fields = encoded.as_object_mut().unwrap();
        encoded_fields.remove("hosted_source");
        encoded_fields.remove("hosted_source_checked");
        let old: pending::TuiDirectPendingStart = serde_json::from_value(encoded).unwrap();
        assert!(old.hosted_source.is_none());
        assert!(!old.hosted_source_checked);
        assert!(
            fixture.claim(&old).await,
            "the original inflight episode independently proves this old record's execution"
        );
        let saved =
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).unwrap();
        assert_eq!(saved.user_msg_id, old.anchor_message_id);
        assert_eq!(saved.output_path.as_deref(), fixture.source.path.to_str());
        assert_eq!(saved.provider, before.provider);
        assert_eq!(saved.channel_id, before.channel_id);
        assert_eq!(saved.tmux_session_name, before.tmux_session_name);
        assert_eq!(saved.turn_nonce, before.turn_nonce);
        assert_eq!(saved.external_turn_id, before.external_turn_id);
        assert_eq!(saved.session_key, before.session_key);
        assert_eq!(saved.hosted_record_id, before.hosted_record_id);
        assert_eq!(saved.hosted_execution_nonce, before.hosted_execution_nonce);
        assert_eq!(saved.session_id, before.session_id);
        assert_eq!(saved.turn_start_offset, before.turn_start_offset);
        assert_eq!(saved.last_offset, before.last_offset);
        assert_eq!(saved.response_sent_offset, before.response_sent_offset);
        let readopted = crate::services::discord::mailbox_snapshot(&fixture.shared, channel)
            .await
            .cancel_token
            .expect("re-adoption retained the original actor");
        assert!(Arc::ptr_eq(&actor, &readopted));
        assert_eq!(
            fixture
                .shared
                .restart
                .global_active
                .load(std::sync::atomic::Ordering::Relaxed),
            active
        );
        fixture.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn old_pending_without_original_source_holds_without_blocking_valid_n2_pg() {
        old_pending_does_not_block_n2(false, 7_186_612).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn old_pending_with_ambiguous_original_source_holds_without_blocking_valid_n2_pg() {
        old_pending_does_not_block_n2(true, 7_186_622).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn stale_pending_cannot_replace_a_live_n2_lease_before_its_claim_is_refused_pg() {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let _binding = crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(
            root.path(),
        );
        let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let channel = ChannelId::new(7_186_642);
        let _hosts =
            crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
        let fixture = Fixture::new(root.path(), channel).await;
        let stale = fixture.pending(N1, &fixture.source, 7_186_643);
        let mut unknown_encoded = serde_json::to_value(&stale).unwrap();
        unknown_encoded
            .as_object_mut()
            .unwrap()
            .remove("hosted_source");
        unknown_encoded
            .as_object_mut()
            .unwrap()
            .remove("hosted_source_checked");
        unknown_encoded["captured_source"] = serde_json::Value::Null;
        let unknown: pending::TuiDirectPendingStart =
            serde_json::from_value(unknown_encoded).unwrap();

        fixture.replace_execution(HostedState::Bound, N2).await;
        let path = root.path().join("live-n2-before-stale-pending.jsonl");
        std::fs::write(&path, b"").unwrap();
        let source =
            crate::services::tui_o::shadow::binding_reader::source_id_for("native-n2", &path)
                .unwrap();
        fixture.publish_source(&source, N2, 2);
        let current = fixture.pending(N2, &source, 7_186_644);
        let current_lease = fixture.publish_pending_lease(&current);
        assert_eq!(
            current_lease
                .hosted_source
                .as_ref()
                .map(|pin| pin.execution_nonce.as_str()),
            Some(N2)
        );
        assert!(
            dedupe::herdr_claim_admission(&fixture.logical, true).is_ok(),
            "N2 is admitted before either stale pending attempts its claim"
        );

        for rejected in [&stale, &unknown] {
            assert!(
                !fixture.claim(rejected).await,
                "a stale N1 or unknown old origin cannot claim the current N2 execution"
            );
            fixture.assert_quiet().await;
            assert_eq!(
                dedupe::external_input_relay_lease("claude", &fixture.logical, channel.get()),
                Some(current_lease.clone()),
                "a refused pending replaced the live N2 lease identity or original source pin"
            );
        }

        assert!(
            fixture.claim(&current).await,
            "the existing N2 lease remains claimable without being republished after stale attempts"
        );
        let saved =
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get()).unwrap();
        assert_eq!(saved.user_msg_id, current.anchor_message_id);
        assert_eq!(saved.hosted_execution_nonce.as_deref(), Some(N2));
        assert_eq!(saved.output_path.as_deref(), source.path.to_str());
        fixture.close().await;
    }

    async fn old_pending_does_not_block_n2(ambiguous: bool, id: u64) {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let _binding = crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(
            root.path(),
        );
        let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let channel = ChannelId::new(id);
        let _hosts =
            crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
        let fixture = Fixture::new(root.path(), channel).await;
        let mut encoded =
            serde_json::to_value(fixture.pending(N1, &fixture.source, id + 1)).unwrap();
        encoded.as_object_mut().unwrap().remove("hosted_source");
        encoded
            .as_object_mut()
            .unwrap()
            .remove("hosted_source_checked");
        if ambiguous {
            fixture.append_source_event(&fixture.source, "ambiguous-original-execution", 2);
        } else {
            encoded["captured_source"] = serde_json::Value::Null;
        }
        let old: pending::TuiDirectPendingStart = serde_json::from_value(encoded).unwrap();
        pending::persist(&old).unwrap();
        assert!(
            dedupe::herdr_claim_admission(&fixture.logical, true).is_ok(),
            "the old execution is admitted before the source-origin check"
        );
        assert!(
            !fixture.claim(&old).await,
            "old Herdr pending with no unique original execution must hold"
        );
        fixture.assert_quiet().await;
        assert!(
            pending::load_all().iter().any(|record| record == &old),
            "the original unresolved input was retained"
        );

        fixture.replace_execution(HostedState::Bound, N2).await;
        let path = root.path().join(format!("{id}-native-n2.jsonl"));
        std::fs::write(&path, b"").unwrap();
        let source =
            crate::services::tui_o::shadow::binding_reader::source_id_for("native-n2", &path)
                .unwrap();
        fixture.publish_source(&source, N2, if ambiguous { 3 } else { 2 });
        let current = fixture.pending(N2, &source, id + 2);
        fixture.publish_pending_lease(&current);
        assert!(
            fixture.claim(&current).await,
            "an unresolved old pending must not globally block a valid new execution"
        );
        let saved = inflight::load_inflight_state_read_only(&ProviderKind::Claude, id).unwrap();
        assert_eq!(saved.hosted_execution_nonce.as_deref(), Some(N2));
        assert_eq!(saved.user_msg_id, current.anchor_message_id);
        assert!(
            pending::load_all().iter().any(|record| record == &old),
            "the valid N2 claim did not need to delete the unresolved old pending"
        );
        fixture.close().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn hosted_row_change_after_actor_admission_refuses_the_production_save_pg() {
        post_admission_change(false, 7_186_402).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn admitted_nonce_change_after_actor_admission_refuses_the_production_save_pg() {
        post_admission_change(true, 7_186_412).await;
    }

    async fn post_admission_change(change_admitted_nonce: bool, id: u64) {
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let _binding = crate::services::discord::turn_presence::activity::tests::BindingRoot::enter(
            root.path(),
        );
        let _mode = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let channel = ChannelId::new(id);
        let _hosts =
            crate::config::session_hosts::force_for_test(Some(NODE), &[(channel.get(), NODE)]);
        let fixture = Fixture::new(root.path(), channel).await;
        let original = fixture.pending(N1, &fixture.source, id + 1);
        let entered = Arc::new(tokio::sync::Notify::new());
        let resume = Arc::new(tokio::sync::Notify::new());
        *synthetic_start::bridge_handoff::ADMISSION_PAUSE
            .lock()
            .unwrap() = Some((id, entered.clone(), resume.clone()));
        let change = async {
            tokio::time::timeout(Duration::from_secs(5), entered.notified())
                .await
                .expect("the valid original hosted claim actually admitted its actor");
            let actor = crate::services::discord::mailbox_snapshot(&fixture.shared, channel).await;
            assert_eq!(
                actor.active_user_message_id.map(MessageId::get),
                Some(original.anchor_message_id)
            );
            assert!(actor.cancel_token.is_some());
            assert!(inflight::load_inflight_state_read_only(&ProviderKind::Claude, id).is_none());
            let expected = if change_admitted_nonce {
                dedupe::install_herdr_execution(&fixture.logical, N2);
                dedupe::admit_herdr_execution(&fixture.logical, N2);
                fixture.hosted_json().await
            } else {
                serde_json::to_value(fixture.replace_execution(HostedState::Bound, N2).await)
                    .unwrap()
            };
            assert!(
                dedupe::herdr_claim_admission(&fixture.logical, true).is_ok(),
                "the changed execution's admission stays true"
            );
            resume.notify_one();
            expected
        };
        let (claimed, expected) = tokio::join!(fixture.claim(&original), change);
        assert!(
            !claimed,
            "the original hosted identity changed after actor claim but before save"
        );
        fixture.assert_quiet().await;
        assert_eq!(
            fixture.hosted_json().await,
            expected,
            "the foreign execution was not rewritten by rollback"
        );
        fixture.close().await;
    }
}
