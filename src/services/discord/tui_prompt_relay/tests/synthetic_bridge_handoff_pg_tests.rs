use super::*;

#[cfg(unix)]
pub(super) fn spawn_handoff_reader(
    path: &Path, start: u64, tmux: &str, tx: mpsc::Sender<StreamMessage>,
    end: tokio::sync::oneshot::Sender<Result<claude_idle_bridge::IdleReaderCompletion, String>>,
) -> std::thread::JoinHandle<()> {
    let generation = crate::services::discord::turn_bridge::tmux_generation_file_mtime_ns(tmux);
    let path = path.to_str().unwrap().to_owned();
    std::thread::spawn(move || {
        let result = crate::services::session_backend::read_output_file_until_result_with_harvest(
            &path, start, tx, None, crate::services::provider::SessionProbe::process(|| true),
        );
        let _ = end.send(result.map(|(result, stats)| {
            claude_idle_bridge::IdleReaderCompletion::from_harvest(result, stats, generation)
        }).map_err(|error| error.error));
    })
}

#[cfg(unix)]
fn synthetic_bridge_handoff_fixture(
    delayed_save: bool,
    foreign_actor: bool,
    wrong_source: bool,
    postgres: bool,
    recovery: Option<bool>,
    failed_save: bool,
    source_retry: bool,
    empty_tail: Option<bool>,
    postgres_race: bool,
    admission_race: bool,
    prefix_read_error: bool,
    native_compaction: bool,
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
            let provider = ProviderKind::Claude;
            let channel = ChannelId::new(583_300_001);
            let anchor = MessageId::new(583_300_002);
            let tmux = "synthetic-bridge-handoff-5833";
            let generation_path = crate::services::tmux_common::session_temp_path(tmux, "generation");
            std::fs::write(&generation_path, b"1").unwrap();
            let output = temp.path().join("transcript.jsonl");
            let body = "첫 프레임 배달과 실행 중 owner 유지 ".repeat(16);
            let mut assistant = serde_json::json!({"type":"assistant", "message":{"content":[{"type":"text", "text":body}]}});
            if native_compaction { assistant["sessionId"] = "native-auto-session".into(); }
            let previous = if failed_save { "" } else { "{\"type\":\"user\",\"message\":{\"content\":\"previous turn\"}}\n" };
            let source_start = previous.len() as u64;
            std::fs::write(&output, format!("{previous}{assistant}\n")).unwrap();
            crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
                tmux,
                crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                    runtime_kind: RuntimeHandoffKind::ClaudeTui,
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
            lease.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
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
                    &shared, &provider, channel, tmux, "handoff prompt", anchor, &lease, None,
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
                        provider: provider.as_str().into(), tmux_session_name: tmux.into(),
                        prompt: "handoff prompt".into(), observed_at: chrono::Utc::now(),
                        source_event_id: None, external_input_lease_generation: lease.generation,
                        ssh_direct_observation_generation: crate::services::tui_prompt_dedupe::SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
                    };
                    let mut inline_lease = lease.clone();
                    assert!(synthetic_start_wiring::wire_tui_direct_synthetic_turn_start(
                        &shared, provider.as_str(), channel, &observed, anchor, true,
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
            if let Some(decoded_terminal) = empty_tail {
                claim.take().unwrap().await;
                let before = crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get()).unwrap();
                let actor = crate::services::discord::mailbox_snapshot(&shared, channel).await.cancel_token.unwrap();
                if decoded_terminal {
                    std::fs::write(&output, format!("{previous}{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"\"}}\n")).unwrap();
                } else { std::fs::remove_file(&output).unwrap(); }
                tokio::time::timeout(Duration::from_secs(5), claude_idle_tail::run_claude_idle_response_tail(
                    shared.clone(), tmux.into(), channel, output.clone(), source_start,
                    "handoff prompt".into(), lease.clone(),
                )).await.expect("real reader exits without a busy wait");
                if decoded_terminal {
                    assert!(crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get()).is_none());
                    assert!(crate::services::discord::mailbox_snapshot(&shared, channel).await.cancel_token.is_none());
                    assert_eq!(crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux).unwrap().last_offset, std::fs::metadata(&output).unwrap().len());
                    return;
                }
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
            let row = crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get()).unwrap();
            let resumed = synthetic_start::bridge_handoff::resume_unpublished(&shared, &row, &output).await.unwrap();
            // Match the production TUI gateway: terminal edits use this same
            // transport; a headless fixture would wait on an unrelated outbox.
            let gateway = Arc::new(S3Gateway { local_delivery: true, ..Default::default() });
            let (tx, rx) = mpsc::channel();
            let (end_tx, end_rx) = tokio::sync::oneshot::channel();
            let delivery = claude_idle_bridge::stream_tui_idle_response_with_gateway(
                &shared, provider.clone(), channel, tmux, &output, original_start,
                "handoff prompt", Vec::new(), rx, Some(end_rx), &resumed, gateway.clone(), 0,
            );
            let reader = spawn_handoff_reader(&output, original_start, tmux, tx, end_tx);
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
            if prefix_read_error {
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
                tokio::time::timeout(Duration::from_secs(3), async {
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
            let terminal = if native_compaction {
                serde_json::json!({"type":"system", "subtype":"stop_hook_summary", "sessionId":"native-auto-session"})
            } else {
                serde_json::json!({"type":"result", "subtype":"success", "result":body})
            };
            std::fs::OpenOptions::new().append(true).open(&output).unwrap()
                .write_all(format!("{terminal}\n").as_bytes()).unwrap();
            tokio::task::spawn_blocking(move || reader.join().unwrap()).await.unwrap();
            };
            let (delivered, ()) = tokio::join!(tokio::time::timeout(Duration::from_secs(5), delivery), observe);
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
                let reader = spawn_handoff_reader(&output, retained.last_offset, tmux, tx, end_tx);
                tokio::time::timeout(Duration::from_secs(5), claude_idle_bridge::stream_tui_idle_response_with_gateway(
                    &shared, provider.clone(), channel, tmux, &output, retained.last_offset,
                    "handoff prompt", Vec::new(), rx, Some(end_rx), &renewed, gateway.clone(), 0,
                )).await.expect("resumed adapter finishes").expect("the original saved prefix and later terminal remain deliverable");
                tokio::task::spawn_blocking(move || reader.join().unwrap()).await.unwrap();
                let bodies = gateway.bodies.lock().unwrap();
                // Legacy streaming status quotes the last response line; count
                // the completed publication, whose footer has been removed.
                let terminal_body = bodies.last().expect("resumed terminal publication");
                assert!(terminal_body.contains(body.trim()), "terminal must retain the complete saved Unicode body: {terminal_body:?}");
                assert_eq!(terminal_body.matches("첫 프레임").count(), 16, "resume must not append the already saved prefix again; observed bodies={bodies:#?}");
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
        });
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_delivers_first_frame_and_releases_original_actor() {
    synthetic_bridge_handoff_fixture(false, false, false, false, None, false, false, None, false, false, false, false);
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_waits_for_later_claim_save_then_delivers() {
    synthetic_bridge_handoff_fixture(true, false, false, false, None, false, false, None, false, false, false, false);
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_rejects_same_nonce_different_actor() {
    synthetic_bridge_handoff_fixture(false, true, false, false, None, false, false, None, false, false, false, false);
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_rejects_different_source_without_row_mutation() {
    synthetic_bridge_handoff_fixture(false, false, true, false, None, false, false, None, false, false, false, false);
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_upserts_missing_postgres_session_before_first_frame() {
    synthetic_bridge_handoff_fixture(false, false, false, true, None, false, false, None, false, false, false, false);
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_retries_unpublished_row_after_adapter_drops() {
    synthetic_bridge_handoff_fixture(
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
        false,
    );
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_restarts_from_persisted_source_after_mailbox_loss() {
    synthetic_bridge_handoff_fixture(
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
        false,
    );
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_failed_inline_save_retries_original_bytes() {
    synthetic_bridge_handoff_fixture(false, false, false, false, None, true, false, None, false, false, false, false);
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_retry_preserves_original_source_and_cursor() {
    synthetic_bridge_handoff_fixture(false, false, false, false, None, false, true, None, false, false, false, false);
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_reader_error_retains_obligation_then_delivers() {
    synthetic_bridge_handoff_fixture(
        false,
        false,
        false,
        false,
        None,
        false,
        false,
        Some(false),
        false,
        false,
        false,
        false,
    );
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_decoded_empty_terminal_releases_only_original_episode() {
    synthetic_bridge_handoff_fixture(
        false,
        false,
        false,
        false,
        None,
        false,
        false,
        Some(true),
        false,
        false,
        false,
        false,
    );
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_delayed_pg_adapter_preserves_successor_session() {
    synthetic_bridge_handoff_fixture(false, false, false, true, None, false, false, None, true, false, false, false);
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_inline_admission_rejects_replacement_actor() {
    synthetic_bridge_handoff_fixture(false, false, false, false, None, false, false, None, false, true, false, false);
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_prefix_then_read_error_retains_original_obligation() {
    synthetic_bridge_handoff_fixture(false, false, false, false, None, false, false, None, false, false, true, false);
}

#[cfg(unix)]
#[test]
fn synthetic_bridge_handoff_native_auto_compaction_preserves_delivery_and_actor() {
    synthetic_bridge_handoff_fixture(false, false, false, false, None, false, false, None, false, false, false, true);
}
