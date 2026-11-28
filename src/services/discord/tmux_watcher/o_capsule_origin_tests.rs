use super::o_seed_install_tests as seed_cases;
use crate::services::discord::inflight::load_inflight_state_read_only_result;
use crate::services::discord::outbound::delivery_record as dr;
use crate::services::discord::tmux::tmux_watcher::cancel_handoff;
use crate::services::discord::tmux::tmux_watcher::tests::streaming_harness_tests::{
    CLAUDE, isolated_in_module, said,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rowless_legacy_capsule_keeps_partial_utf8_and_source_authority() {
    let test = "rowless_legacy_capsule_keeps_partial_utf8_and_source_authority";
    if !isolated_in_module(module_path!(), test, &[]) {
        return;
    }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (fixture, _discord) = seed_cases::fixture(632_513_001).await;
    assert!(
        load_inflight_state_read_only_result(&CLAUDE, fixture.channel.get())
            .unwrap()
            .is_none()
    );
    let old = said("Legacy carried scalar 안 remains unfinished");
    let scalar_start = old.find('안').unwrap();
    let split = scalar_start + 1;
    seed_cases::append(&fixture, &old.as_bytes()[..split]);
    let predecessor_observed = seed_cases::observe(&fixture);
    let predecessor = seed_cases::spawn_legacy(&fixture, scalar_start as u64, None);
    seed_cases::until(
        &predecessor_observed,
        "actual split scalar decode checkpoints before text is available",
        |s| s.event_count("checkpoint") > 0 && s.event_count("outer_eof") > 0,
    )
    .await;
    seed_cases::cancel_join(&predecessor).await;
    let predecessor_seen = predecessor_observed.snapshot();
    assert!(
        predecessor_seen.decoded_chunks.is_empty(),
        "{predecessor_seen:?}"
    );
    assert!(predecessor_seen.writable_inflight_load_calls > 0);
    drop(predecessor_observed);
    assert!(
        load_inflight_state_read_only_result(&CLAUDE, fixture.channel.get())
            .unwrap()
            .is_none()
    );
    let legacy =
        cancel_handoff::capsule_observation::pending(&fixture.shared, fixture.channel).await;
    assert_eq!(legacy.len(), 1, "{legacy:?}");
    let capsule = &legacy[0];
    assert!(capsule.has_utf8_carry, "{legacy:?}");
    assert!(
        capsule.body.is_empty() && capsule.buffer.is_empty(),
        "{legacy:?}"
    );
    assert_eq!(capsule.offset, split as u64);
    assert!(
        !capsule.has_identity && capsule.identity.is_none() && capsule.nonce.is_none(),
        "{legacy:?}"
    );
    assert!(
        !capsule.has_startup_snapshot && capsule.startup_snapshot.is_none(),
        "{legacy:?}"
    );
    assert!(
        !capsule.has_restored_seed && capsule.restored_response_seed.is_empty(),
        "{legacy:?}"
    );
    assert!(
        !capsule.has_rewind_key && capsule.rewind_attempts == 0,
        "{legacy:?}"
    );
    assert!(!capsule.has_turn_identity_for_panel, "{legacy:?}");
    assert!(capsule.utf8_state.contains("pending: [236]"), "{legacy:?}");
    assert_ne!(capsule.authority.generation_mtime_ns, 0);
    assert_eq!(capsule.opened_source, capsule.authority.source_file);
    assert_eq!(
        capsule.authority.generation_mtime_ns,
        dr::current_generation_mtime_ns(&fixture.session)
    );
    assert_eq!(
        capsule.authority.reset_incarnation,
        fixture
            .shared
            .relay_frontier_token(fixture.channel)
            .reset_incarnation
    );
}

mod evidence_ack {
    use crate::services::discord::inflight::{
        InflightTurnIdentity, load_inflight_state, save_inflight_state,
    };
    use crate::services::discord::outbound::o_delivery_evidence::tests::{Fixture, bounded};
    use crate::services::discord::tmux::WatcherTerminalKind;
    use crate::services::discord::tmux::tmux_watcher::loop_poll_prologue::WatcherSourceAuthority;
    use crate::services::discord::tmux::tmux_watcher::o_evidence_tests::plan;
    use crate::services::discord::tmux::tmux_watcher::session_bound_ack::{
        SessionBoundRelayAckOutcome, wait_for_session_bound_relay_delivery_ack,
    };
    use crate::services::discord::tmux::tmux_watcher::turn_stream_collector::*;
    use crate::services::discord::tmux::tmux_watcher::utf8_chunk_decoder::Utf8ChunkDecoder;
    use crate::services::discord::tmux::{HostSnapshot, WatchHost};
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::ChannelId;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[tokio::test(flavor = "current_thread")]
    async fn i7441b_claude_soft_frameaccepted_ack_and_o_lookup_share_deadline_pg() {
        soft_ack_cases(&[0, 1, 2], false).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn i7441c_injected_soft_stop_posted_after_85mb_suppresses_pg() {
        soft_ack_cases(&[0], true).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn i7441c_missing_mismatched_foreign_or_unreadable_evidence_records_pg() {
        soft_ack_cases(&[1, 2, 4, 5, 6], true).await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn i7441c_partial_range_past_terminal_seal_records_pg() {
        soft_ack_cases(&[3], true).await;
    }

    async fn soft_ack_cases(cases: &[u64], injected: bool) {
        use crate::services::cluster::{
            relay_producer_registry::RelayProducerRegistry,
            session_matcher::MatchedChannel,
            stream_relay::{SourceFileIdentity, spawn_stream_relay},
        };
        use crate::services::discord::session_relay_sink::SessionBoundDiscordRelaySink;
        use crate::services::session_host::{
            HostLiveness, HostSessionRef, test_support::InjectedLivenessGuard,
        };
        let root = tempfile::tempdir().unwrap();
        let _root = crate::config::set_agentdesk_root_for_test(root.path());
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "i7441b_soft",
            "O soft terminal ACK",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        for &proof in cases {
            let mut f = if injected {
                assert!(load_inflight_state(&ProviderKind::Claude, 744260 + proof).is_none());
                Fixture::injected_with_prefix(
                    744260 + proof,
                    &["background task finished. ", "monitor followup"],
                    85_419_517,
                )
                .await
            } else {
                Fixture::new(744160 + proof, ProviderKind::Claude, &["soft answer"]).await
            };
            let _o = crate::services::tui_o::cutover::test_override::force_channels(&[(
                f.row.channel_id,
                crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            )]);
            let (http, _http_guard) = f.http().await;
            let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(
                pool.clone(),
            ));
            let channel = ChannelId::new(f.row.channel_id);
            f.row.tmux_session_name =
                Some(format!("AgentDesk-claude-i7441b-soft-{}", f.row.channel_id));
            let session = f.row.tmux_session_name.clone().unwrap();
            let _live =
                InjectedLivenessGuard::set(HostSessionRef::tmux(&session), HostLiveness::Live);
            if !injected {
                save_inflight_state(&f.row).unwrap();
                assert_eq!(
                    crate::services::discord::inflight::clear_inflight_state_for_captured_episode(
                        &ProviderKind::Claude,
                        f.row.channel_id,
                        &InflightTurnIdentity::from_state(&f.row),
                        f.row.turn_nonce.as_deref()
                    ),
                    crate::services::discord::inflight::GuardedClearOutcome::Cleared
                );
            }
            assert!(load_inflight_state(&ProviderKind::Claude, f.row.channel_id).is_none());
            let health = Arc::new(crate::services::discord::health::HealthRegistry::new());
            health.register("claude".into(), shared.clone()).await;
            let sink = SessionBoundDiscordRelaySink::new(health);
            sink.enable_delivery_for_test();
            let handle = spawn_stream_relay(
                MatchedChannel {
                    channel_id: f.row.channel_id.to_string(),
                    agent_id: "soft-fixture".into(),
                    provider: ProviderKind::Claude,
                    expected_session_name: session.clone(),
                    expected_rollout_path: f.source.path.display().to_string(),
                },
                Arc::new(sink),
            );
            let registry = Arc::new(RelayProducerRegistry::new());
            registry.register(session.clone(), handle.producer());
            let file = std::fs::File::open(&f.source.path).unwrap();
            let raw = std::fs::read(&f.source.path).unwrap();
            let base = f.row.turn_start_offset.unwrap_or(0) as usize;
            let mut start = base + raw[base..].iter().position(|b| *b == b'\n').unwrap() + 1;
            let data = raw[start..].to_vec();
            let source_authority = WatcherSourceAuthority {
                source_file: SourceFileIdentity::from_open_file(&file),
                generation_mtime_ns: 1,
                reset_incarnation: shared.relay_frontier_token(channel).reset_incarnation,
                source_stamp: None,
            };
            let ctx = TurnStreamCollectorContext {
                http: http.clone(),
                shared: shared.clone(),
                channel_id: channel,
                watcher_provider: ProviderKind::Claude,
                tmux_session_name: session.clone(),
                output_path: f.source.path.display().to_string(),
                input_fifo_path: String::new(),
                watcher_thread_channel_id: None,
                cancel: Arc::new(AtomicBool::new(false)),
                paused: Arc::new(AtomicBool::new(false)),
                pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                turn_delivered: Arc::new(AtomicBool::new(false)),
                last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(0)),
                jsonl_notify: Arc::new(tokio::sync::Notify::new()),
                dead_marker_notify: Arc::new(tokio::sync::Notify::new()),
                turn_result_relayed: false,
                restored_injected_prompt_message_id: None,
                host: Arc::new(HostSnapshot::new(WatchHost::Legacy)),
                watcher_instance_id: 1,
            };
            let retained = Some(Arc::new(file));
            let mut continuation = None;
            let mut offset = raw.len() as u64;
            let mut all_data = String::new();
            let mut buffer_start = start as u64;
            let mut decoder = Utf8ChunkDecoder::default();
            let mut pending = None;
            let mut restored = None;
            let mut rewind = None;
            let mut attempts = 0;
            let identity = None;
            let mut heartbeat = None;
            let mut reacquire = false;
            let mut parser = TurnParseState {
                retained_source: &retained,
                continuation: &mut continuation,
                current_offset: &mut offset,
                all_data: &mut all_data,
                all_data_start_offset: &mut buffer_start,
                utf8_decoder: &mut decoder,
                pending_terminal_rewind_seed: &mut pending,
                restored_turn: &mut restored,
                terminal_rewind_attempt_key: &mut rewind,
                terminal_rewind_attempts: &mut attempts,
                watcher_turn_identity: &identity,
                watcher_turn_nonce: &None,
                last_activity_heartbeat_at: &mut heartbeat,
                active_stream_inflight_reacquire_logged: &mut reacquire,
            };
            let mut cached = None;
            let mut mirrored = true;
            let mut ack = None;
            let mut first = None;
            let mut relay = SupervisorRelayState {
                producer_registry: &registry,
                cached_relay_producer: &mut cached,
                all_data_fully_mirrored_to_session_relay: &mut mirrored,
                all_data_session_bound_relay_ack: &mut ack,
                all_data_first_forwarded_relay_sequence: &mut first,
            };
            let outcome = tokio::time::timeout(
                std::time::Duration::from_secs(8),
                collect_turn_stream_until_terminal(
                    &ctx,
                    TurnStreamCollectorIo {
                        data,
                        data_start_offset: start as u64,
                        epoch_snapshot: 0,
                        source_authority,
                    },
                    &mut parser,
                    &mut relay,
                    &mut MonitorAutoTurnState::default(),
                    &mut RenderSeedState::default(),
                ),
            )
            .await
            .expect("soft collector must finish");
            let CollectOutcome::Fallthrough(turn) = outcome else {
                panic!("collector discarded soft completion")
            };
            assert_eq!(
                turn.terminal_kind,
                Some(WatcherTerminalKind::SoftStopHookSummary)
            );
            assert!(!turn.found_result);
            assert_eq!(turn.full_response, f.body);
            let target = ack
                .as_ref()
                .expect("ordinary frame still carries an ACK target");
            bounded("stream relay frame delivery", async {
                while handle.metrics().snapshot().frames_delivered == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            assert_eq!(
                handle.metrics().snapshot().last_delivered_sequence,
                Some(target.sequence),
                "the target sequence was FrameAccepted"
            );
            assert_eq!(
                target
                    .metrics
                    .terminal_outcome_for_sequence(target.sequence),
                None,
                "FrameAccepted is not a terminal ACK"
            );
            assert!(load_inflight_state(&ProviderKind::Claude, f.row.channel_id).is_none());
            let ack_probe = target.clone();
            if proof == 1 {
                if injected {
                    std::fs::write(
                        f.root
                            .join("o_store")
                            .join(f.row.channel_id.to_string())
                            .join("ledger.jsonl"),
                        b"",
                    )
                    .unwrap();
                } else {
                    f.port.messages.lock().unwrap().clear();
                }
            } else if proof == 2 {
                f.port.messages.lock().unwrap()[0].content = "mismatch".into();
            } else if proof == 3 {
                use std::io::Write;
                let extra = b"{\"type\":\"assistant\",\"uuid\":\"a-next\",\"apiBlockIndex\":0,\"message\":{\"id\":\"body-next\",\"content\":[{\"type\":\"text\",\"text\":\"next turn\"}]}}\n";
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&f.source.path)
                    .unwrap()
                    .write_all(extra)
                    .unwrap();
                f.row.last_offset += extra.len() as u64;
            } else if proof == 4 {
                let other = f.source.path.with_extension("replacement");
                let replacement = String::from_utf8(raw.clone())
                    .unwrap()
                    .replace("native-session", "different-session");
                start = base
                    + replacement.as_bytes()[base..]
                        .iter()
                        .position(|b| *b == b'\n')
                        .unwrap()
                    + 1;
                f.row.last_offset = replacement.len() as u64;
                std::fs::write(&other, replacement).unwrap();
                std::fs::rename(other, &f.source.path).unwrap();
                f.source = crate::services::tui_o::shadow::binding_reader::source_id_for(
                    "different-session",
                    &f.source.path,
                )
                .unwrap();
            } else if proof == 5 {
                std::fs::remove_file(&f.source.path).unwrap();
            } else if proof == 6 {
                f.port.messages.lock().unwrap().pop();
            }
            let frontier = shared
                .tmux_relay_coord(channel)
                .confirmed_end_offset
                .load(Ordering::Acquire);
            let began = tokio::time::Instant::now();
            plan(
                &f,
                &shared,
                &http,
                None,
                cached,
                &mut ack,
                mirrored,
                (start as u64, start as u64),
            )
            .await;
            assert!(
                began.elapsed() < std::time::Duration::from_secs(10),
                "the complete terminal decision stays within 10s"
            );
            assert_eq!(
                wait_for_session_bound_relay_delivery_ack(
                    Some(&ack_probe),
                    std::time::Duration::ZERO
                )
                .await,
                SessionBoundRelayAckOutcome::TimedOut
            );
            assert_eq!(
                shared
                    .tmux_relay_coord(channel)
                    .confirmed_end_offset
                    .load(Ordering::Acquire),
                frontier
            );
            let rows: Vec<String> =
                sqlx::query_scalar("SELECT content FROM relay_dead_letter WHERE channel_id=$1")
                    .bind(channel.to_string())
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            let reasons: Vec<String> =
                sqlx::query_scalar("SELECT reason FROM relay_dead_letter WHERE channel_id=$1")
                    .bind(channel.to_string())
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            for reason in &reasons {
                assert!(reason.contains("denial=no_inflight_row"));
                assert!(reason.contains("frame_ack_outcome=TimedOut"));
            }
            eprintln!(
                "soft-stop injected={injected} proof={proof} rows={} reasons={reasons:?}",
                rows.len()
            );
            assert_eq!(
                rows,
                if proof == 0 {
                    Vec::<String>::new()
                } else {
                    vec![f.body.clone()]
                }
            );
            if !injected || proof == 0 {
                assert!(
                    f.get_calls.load(Ordering::SeqCst) > 0,
                    "O lookup must execute before ACK timeout"
                );
            }
            bounded("stream relay shutdown", handle.shutdown()).await;
        }
        pool.close().await;
        db.drop().await;
    }
}
