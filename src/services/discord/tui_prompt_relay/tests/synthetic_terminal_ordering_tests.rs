//! The actual synthetic adapter must retain A until terminal transport settles.
use super::*;

#[derive(Default)]
pub(super) struct TerminalBarrier {
    pub(super) entered: tokio::sync::Notify,
    pub(super) release: tokio::sync::Notify,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceRace {
    Unchanged,
    Generation,
    FileIdentity,
    Session,
    RevalidationIo,
    PinGeneration,
    LiveHolder,
}

fn terminal_ordering_fixture(
    replace_actor: bool,
    replace_after_delivery: bool,
    empty_terminal: bool,
    source_race: Option<SourceRace>,
    provider: ProviderKind,
) {
    let _telemetry = crate::services::observability::test_runtime_lock();
    crate::services::observability::reset_for_tests();
    let temp = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(temp.path());
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
        .block_on(async {
            let shared = crate::services::discord::make_shared_data_for_tests();
            let runtime = if provider == ProviderKind::Codex { RuntimeHandoffKind::CodexTui } else { RuntimeHandoffKind::ClaudeTui };
            let channel = ChannelId::new(583_310_001);
            let anchor = MessageId::new(if source_race.is_some() { 583_310_002_000_000 } else { 583_310_002 });
            let tmux = "synthetic-terminal-ordering-5833";
            let generation_path = crate::services::tmux_common::session_temp_path(tmux, "generation");
            std::fs::write(&generation_path, b"1").unwrap();
            let generation_time = std::fs::metadata(&generation_path).unwrap().modified().unwrap();
            let output = temp.path().join("transcript.jsonl");
            let body = if empty_terminal { String::new() } else {
                "synthetic terminal publication keeps its original actor ".repeat(12)
            };
            let assistant = serde_json::json!({"type":"assistant", "sessionId":"native-ordering-session", "message":{"content":[{"type":"text", "text":body}]}});
            let terminal = serde_json::json!({"type":"result", "session_id":"native-ordering-session", "subtype":"success", "result":body});
            let (assistant, terminal) = if provider == ProviderKind::Codex {
                (serde_json::json!({"type":"response_item", "payload":{"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":body}]}}),
                 serde_json::json!({"type":"event_msg", "payload":{"type":"task_complete", "last_agent_message":body}}))
            } else { (assistant, terminal) };
            std::fs::write(&output, format!("{assistant}\n{terminal}\n")).unwrap();
            crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(tmux,
                crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                    runtime_kind: runtime,
                    output_path: output.to_str().unwrap().into(),
                    relay_output_path: None, input_fifo_path: None,
                    session_id: None, last_offset: 0, relay_last_offset: None,
                });
            let mut lease = ExternalInputRelayLease::unassigned(Some(channel.get()));
            lease.turn_id = Some("external-5833-terminal-ordering".into());
            lease.relay_owner = ExternalInputRelayOwner::BridgeAdapter;
            lease.runtime_kind = Some(runtime);
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
            if source_race.is_some() {
                use crate::services::discord::turn_view_reconciler::{TurnViewTarget, TurnViewOwner, TurnViewIdentity};
                assert!(shared.turn_view_reconciler.note_turn_started(
                    &shared, TurnViewTarget::intake_user_message(channel, anchor),
                    TurnViewOwner::for_message(channel, anchor, original_row.born_generation),
                    TurnViewIdentity::IntakeShared, "admitted_source_race_fixture",
                ).await);
            }
            let pending_view = shared.turn_view_reconciler.ops();
            let barrier = Arc::new(TerminalBarrier::default());
            let prepare = source_race.map(|_| Arc::new(crate::services::discord::turn_bridge::TerminalPrepareTestHook {
                channel_id: channel.get(), ..Default::default()
            }));
            *crate::services::discord::turn_bridge::TERMINAL_PREPARE_TEST_HOOK.lock().unwrap() = prepare.clone();
            let row_path = crate::services::discord::inflight::inflight_state_path(
                &crate::services::discord::inflight::inflight_runtime_root().unwrap(), &provider, channel.get());
            let lock_path = row_path.with_extension("json.lock");
            let holder_key = crate::services::discord::DeliveryLeaseKey::from_inflight_state_for_site(
                channel, shared.restart.current_generation, &original_row, "bridge");
            let holder = crate::services::discord::LeaseHolder::Watcher { instance_id: 5833 };
            let cell = shared.delivery_lease(channel);
            let gateway = Arc::new(S3Gateway {
                local_delivery: true, terminal_barrier: source_race.is_none().then(|| barrier.clone()),
                ..Default::default()
            });
            let (tx, rx) = mpsc::channel();
            let (reader_end_tx, reader_end_rx) = tokio::sync::oneshot::channel();
            let reader = super::synthetic_bridge_handoff_pg_tests::spawn_handoff_reader(
                &output, 0, tmux, tx, reader_end_tx,
            );
            let delivery = claude_idle_bridge::stream_tui_idle_response_with_gateway(
                &shared, provider.clone(), channel,
                claude_idle_bridge::IdleBridgeSource {
                    tmux_session_name: tmux, output_path: &output, start_offset: 0,
                    prompt_text: "terminal ordering prompt", lease: &lease,
                },
                (Vec::new(), rx, Some(reader_end_rx)), gateway.clone(), 0,
            );
            let observe = async {
                let entered = if let Some(prepare) = prepare.as_ref() { &prepare.entered } else { &barrier.entered };
                tokio::time::timeout(Duration::from_secs(5), entered.notified())
                    .await.expect("actual adapter reaches the admitted terminal publication boundary");
                let before = crate::services::discord::mailbox_snapshot(&shared, channel).await;
                assert!(before.cancel_token.as_ref().is_some_and(|token| Arc::ptr_eq(token, &original_actor)),
                    "the original synthetic actor must still own the turn DURING terminal transport");
                assert!(!original_actor.cancelled.load(std::sync::atomic::Ordering::Acquire));
                let row = crate::services::discord::inflight::load_inflight_state_read_only(
                    &provider, channel.get()).expect("terminal transport retains its delivery obligation");
                assert_eq!(row.turn_nonce, original_row.turn_nonce);
                assert_eq!(row.current_msg_id, anchor.get(),
                    "capture binds the legitimate anchor before source mutation");
                assert_eq!(row.session_id.as_deref(), Some("native-ordering-session"));
                assert_eq!(crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux)
                    .unwrap().session_id.as_deref(), Some("native-ordering-session"),
                    "the actual source witness connects the original missing session before publication");
                if let Some(race) = source_race {
                    assert_eq!(shared.turn_view_reconciler.ops(), pending_view,
                        "admitted Claude keeps its pending view until confirmed publication");
                    use std::os::unix::fs::MetadataExt;
                    let metadata = std::fs::metadata(&output).unwrap();
                    assert_eq!(row.tui_terminal_source_file_identity, Some((metadata.dev(), metadata.ino())),
                        "real reader admission captured the original opened FD before mutation");
                    assert_eq!(row.tui_terminal_generation_mtime_ns,
                        Some(crate::services::discord::turn_bridge::tmux_generation_file_mtime_ns(tmux)));
                    assert_eq!(row.last_offset, metadata.len());
                    assert_eq!(row.full_response, body);
                    assert!(!row.terminal_delivery_committed);
                    match race {
                        SourceRace::Unchanged => {}
                        SourceRace::Generation => {
                            std::fs::File::open(&generation_path).unwrap().set_times(std::fs::FileTimes::new()
                                .set_modified(generation_time + Duration::from_secs(1))).unwrap();
                        }
                        SourceRace::FileIdentity => {
                            std::fs::rename(&output, output.with_extension("original")).unwrap();
                            std::fs::copy(output.with_extension("original"), &output).unwrap();
                        }
                        SourceRace::Session => {
                            let mut binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux).unwrap();
                            binding.session_id = Some("different-known-session".into());
                            crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(tmux, binding);
                        }
                        SourceRace::RevalidationIo => {
                            match std::fs::rename(&lock_path, lock_path.with_extension("saved")) {
                                Ok(()) => {}
                                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                                Err(error) => panic!("cannot preserve revalidation lock: {error}"),
                            }
                            std::fs::create_dir(&lock_path).unwrap();
                            assert!(crate::services::discord::inflight::lock_inflight_state_path(&row_path).is_err(),
                                "directory collision must fail the actual revalidation lock open");
                        }
                        SourceRace::PinGeneration => {
                            let generation_path = generation_path.clone();
                            *prepare.as_ref().unwrap().after_revalidation.lock().unwrap() = Some(Box::new(move || {
                                std::fs::File::open(generation_path).unwrap().set_times(std::fs::FileTimes::new()
                                    .set_modified(generation_time + Duration::from_secs(1))).unwrap();
                            }));
                        }
                        SourceRace::LiveHolder => {
                            assert!(cell.try_acquire(holder_key.clone(), holder, 0, metadata.len(),
                                crate::services::discord::lease_now_ms() + 30_000));
                        }
                    }
                }
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
                    Some((actor, row.clone()))
                } else { None };
                if let Some(prepare) = prepare.as_ref() { prepare.release.notify_one(); }
                else { barrier.release.notify_one(); }
                (replacement, row)
            };
            let (delivered, (replacement, admitted_row)) = tokio::join!(
                tokio::time::timeout(Duration::from_secs(5), delivery), observe);
            let delivered = delivered.expect("terminal transport must settle");
            tokio::task::spawn_blocking(move || reader.join().unwrap()).await.unwrap();
            *crate::services::discord::turn_bridge::TERMINAL_PREPARE_TEST_HOOK.lock().unwrap() = None;
            if let Some(race) = source_race.filter(|race| *race != SourceRace::Unchanged) {
                assert!(delivered.is_err(), "{race:?} cannot signal terminal completion");
                let expected_turn_id = format!("discord:{}:{}", channel.get(), anchor.get());
                let is_episode_quality = |event: &crate::services::observability::events::StructuredEvent|
                    event.channel_id == Some(channel.get()) && event.event_type == "agent_quality_event"
                        && event.payload["source_event_id"].as_str() == Some(expected_turn_id.as_str())
                        && event.payload["payload"]["turn_id"].as_str() == Some(expected_turn_id.as_str());
                let is_terminal_quality = |event: &crate::services::observability::events::StructuredEvent|
                    is_episode_quality(event) && matches!(event.payload["quality_event_type"].as_str(),
                        Some("turn_error" | "turn_complete"));
                tokio::time::timeout(Duration::from_secs(5), async {
                    while shared.restart.finalizing_turns.load(std::sync::atomic::Ordering::Acquire) != 0
                        || !crate::services::observability::events::recent(200).iter().any(&is_terminal_quality) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }).await.expect("unresolved bridge finishes its postlude within a bound");
                let events = crate::services::observability::events::recent(200);
                let finished: Vec<_> = events.iter().filter(|event|
                    event.channel_id == Some(channel.get()) && event.event_type == "turn_finished"
                        && event.payload["turn_id"].as_str() == Some(expected_turn_id.as_str())).collect();
                assert_eq!(finished.len(), 1);
                let expected_outcome = if race == SourceRace::LiveHolder { "delivery_pending" } else { "delivery_unresolved" };
                assert_eq!(finished[0].payload["status"], expected_outcome);
                // The bridge emits turn_start before its terminal quality event.
                assert_eq!(events.iter().filter(|event| is_episode_quality(event)
                    && event.payload["quality_event_type"] == "turn_start").count(), 1);
                assert_eq!(events.iter().filter(|event| is_episode_quality(event)
                    && event.payload["quality_event_type"] == "turn_complete").count(), 0,
                    "unconfirmed delivery never emits a completed quality event");
                let quality: Vec<_> = events.iter().filter(|event| is_terminal_quality(event)).collect();
                assert_eq!(quality.len(), 1);
                assert_eq!(quality[0].payload["quality_event_type"], "turn_error");
                assert_eq!(quality[0].payload["payload"]["details"]["outcome"], expected_outcome);
                assert_eq!(quality[0].payload["payload"]["details"]["terminal_delivery_committed"], false);
                assert_eq!(shared.turn_view_reconciler.ops(), pending_view,
                    "source loss does not clear or complete the pending view");
                assert!(gateway.bodies.lock().unwrap().is_empty(), "{race:?} must execute zero gateway sends/edits");
                assert!(crate::services::discord::outbound::delivery_record::read_record(&provider, channel.get())
                    .is_none_or(|record| record.confirmed_deliveries.is_empty()), "no exact receipt before transport");
                assert_eq!(crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux).unwrap().last_offset, 0);
                let retained = crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get())
                    .expect("source loss retains the original durable delivery obligation");
                assert_eq!(retained.turn_nonce, original_row.turn_nonce);
                assert_eq!(retained.current_msg_id, admitted_row.current_msg_id);
                assert_eq!(retained.full_response, admitted_row.full_response);
                assert_eq!(retained.turn_start_offset, admitted_row.turn_start_offset);
                assert_eq!(retained.last_offset, admitted_row.last_offset);
                assert_eq!(retained.output_path, admitted_row.output_path);
                assert_eq!(retained.session_id, admitted_row.session_id);
                assert_eq!(retained.tui_terminal_source_file_identity, admitted_row.tui_terminal_source_file_identity);
                assert_eq!(retained.tui_terminal_generation_mtime_ns, admitted_row.tui_terminal_generation_mtime_ns);
                assert!(!retained.terminal_delivery_committed);
                let actor = replacement.as_ref().map(|(actor, _)| actor).unwrap_or(&original_actor);
                assert!(crate::services::discord::mailbox_snapshot(&shared, channel).await.cancel_token
                    .is_some_and(|current| Arc::ptr_eq(&current, actor)), "source loss preserves the actual actor");
                assert!(!actor.cancelled.load(std::sync::atomic::Ordering::Acquire));
                if race == SourceRace::LiveHolder {
                    assert!(cell.release(holder, holder_key.clone(), 0, retained.last_offset), "real Skip leaves its actual holder intact");
                } else {
                    assert!(cell.try_acquire(holder_key.clone(), holder, 0, retained.last_offset,
                        crate::services::discord::lease_now_ms() + 30_000), "failed preparation releases its own lease");
                    assert!(cell.release(holder, holder_key.clone(), 0, retained.last_offset));
                }
                if race == SourceRace::RevalidationIo {
                    std::fs::remove_dir(&lock_path).unwrap();
                    if lock_path.with_extension("saved").exists() {
                        std::fs::rename(lock_path.with_extension("saved"), &lock_path).unwrap();
                    }
                }
                if replace_actor { return; }
                match race {
                    SourceRace::Generation | SourceRace::PinGeneration => {
                        std::fs::File::open(&generation_path).unwrap().set_times(std::fs::FileTimes::new()
                            .set_modified(generation_time)).unwrap();
                    }
                    SourceRace::FileIdentity => {
                        std::fs::remove_file(&output).unwrap();
                        std::fs::rename(output.with_extension("original"), &output).unwrap();
                    }
                    SourceRace::Session => {
                        let mut binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux).unwrap();
                        binding.session_id = Some("native-ordering-session".into());
                        crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(tmux, binding);
                    }
                    _ => {}
                }
                assert!(crate::services::tui_prompt_dedupe::external_input_relay_lease(
                    provider.as_str(), tmux, channel.get()).is_none(), "the original bridge lease was retired");
                let http = Arc::new(serenity::Http::new("Bot test-token"));
                let recovered = tokio::time::timeout(Duration::from_secs(5),
                    crate::services::discord::recovery_engine::recover_idle_partial_response_from_ready_source(
                        &http, &shared, &retained, &output, gateway.as_ref(),
                    )).await.unwrap_or_else(|_| panic!("{race:?} dormant recovery exceeds its bound"));
                if !recovered {
                    let current = crate::services::discord::mailbox_snapshot(&shared, channel).await;
                    let extracted = crate::services::discord::recovery_engine::extract_response_from_output_pub(
                        &output.to_string_lossy(), retained.turn_start_offset.unwrap());
                    panic!("{race:?} dormant recovery did not settle the original exact source: \
                        same_actor={}, cancelled={}, relay_in_flight={}, range={:?}..{}, file_end={}, \
                        body_bytes={}/{}, body_matches={}, generation={:?}/{}, row_path={:?}, caller_path={:?}",
                        current.cancel_token.as_ref().is_some_and(|actor| Arc::ptr_eq(actor, &original_actor)),
                        original_actor.cancelled.load(std::sync::atomic::Ordering::Acquire),
                        shared.relay_emission_in_flight(channel), retained.turn_start_offset, retained.last_offset,
                        std::fs::metadata(&output).unwrap().len(), retained.full_response.len(), extracted.len(),
                        retained.full_response == extracted, retained.tui_terminal_generation_mtime_ns,
                        crate::services::discord::turn_bridge::tmux_generation_file_mtime_ns(tmux),
                        retained.output_path, output);
                }
            }
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
                Some((actor, original_row.clone()))
            } else { replacement };
            let after = crate::services::discord::mailbox_snapshot(&shared, channel).await;
            if let Some((replacement, replacement_row)) = replacement {
                assert!(after.cancel_token.as_ref().is_some_and(|active| Arc::ptr_eq(active, &replacement)),
                    "same-nonce RecoveryKickoff actor survives A's terminal and postlude");
                assert!(!replacement.cancelled.load(std::sync::atomic::Ordering::Acquire));
                let row = crate::services::discord::inflight::load_inflight_state_read_only(
                    &provider, channel.get()).expect("same-episode successor's row survives");
                assert_eq!(row.current_msg_id, replacement_row.current_msg_id,
                    "A must not change the anchor held when B acquired the mailbox");
                assert_eq!(crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(tmux).unwrap().last_offset, 0,
                    "replaced actor never advances the original source cursor");
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
                if source_race.is_none_or(|race| race == SourceRace::Unchanged) {
                    delivered.expect("original synthetic delivery completes");
                }
                let record = crate::services::discord::outbound::delivery_record::read_record(&provider, channel.get()).unwrap();
                assert_eq!(record.confirmed_deliveries.len(), 1, "one exact receipt settles the original obligation");
                assert_eq!(record.confirmed_deliveries[0].source.range, (0, std::fs::metadata(&output).unwrap().len()));
                assert_eq!(record.confirmed_deliveries[0].source.turn_nonce, original_actor.turn_nonce().unwrap());
                assert!(crate::services::discord::inflight::load_inflight_state_read_only(&provider, channel.get()).is_none());
                assert!(after.cancel_token.is_none(), "A releases only after successful publication");
                if source_race.is_some() {
                    assert_eq!(gateway.bodies.lock().unwrap().len(), 1, "one confirmed publication settles the pending body");
                }
                assert!(gateway.bodies.lock().unwrap().iter().any(|sent| !sent.trim().is_empty() && sent.contains(&body)));
                let next = Arc::new(CancelToken::new());
                assert!(crate::services::discord::mailbox_try_start_turn(
                    &shared, channel, next, serenity::UserId::new(583_310_003), MessageId::new(583_310_004),
                ).await, "next input can obtain the released owner");
            }
        });
}

#[test]
fn synthetic_terminal_gateway_retains_original_actor_until_publication() {
    terminal_ordering_fixture(false, false, false, None, ProviderKind::Claude);
    terminal_ordering_fixture(false, false, true, None, ProviderKind::Claude);
}

#[test]
fn synthetic_terminal_gateway_preserves_same_nonce_recovery_actor() {
    terminal_ordering_fixture(true, false, false, None, ProviderKind::Claude);
    terminal_ordering_fixture(true, false, true, None, ProviderKind::Claude);
}

#[test]
fn synthetic_terminal_duplicate_finalizer_preserves_same_nonce_recovery_actor() {
    terminal_ordering_fixture(false, true, false, None, ProviderKind::Claude);
}

#[test]
fn synthetic_terminal_gateway_rejects_lost_admitted_source() {
    for race in [
        SourceRace::Unchanged,
        SourceRace::Generation,
        SourceRace::FileIdentity,
        SourceRace::Session,
        SourceRace::RevalidationIo,
        SourceRace::PinGeneration,
        SourceRace::LiveHolder,
    ] {
        terminal_ordering_fixture(false, false, false, Some(race), ProviderKind::Claude);
    }
    for race in [
        SourceRace::FileIdentity,
        SourceRace::RevalidationIo,
        SourceRace::PinGeneration,
    ] {
        terminal_ordering_fixture(false, false, false, Some(race), ProviderKind::Codex);
    }
}

#[test]
fn synthetic_terminal_gateway_source_loss_preserves_same_nonce_successor() {
    for race in [
        SourceRace::Generation,
        SourceRace::FileIdentity,
        SourceRace::Session,
    ] {
        terminal_ordering_fixture(true, false, false, Some(race), ProviderKind::Claude);
    }
}
