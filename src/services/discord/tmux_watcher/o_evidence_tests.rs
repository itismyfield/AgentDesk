use super::*;
use crate::services::cluster::stream_relay::RelayProducer;
use crate::services::discord::inflight::{
    InflightTurnIdentity, load_inflight_state, save_inflight_state,
};
use crate::services::discord::outbound::o_delivery_evidence::tests::Fixture;
use std::sync::atomic::{AtomicBool, Ordering};

thread_local! {
    static DLQ_TASKS: std::cell::RefCell<Vec<tokio::task::JoinHandle<()>>> = const { std::cell::RefCell::new(Vec::new()) };
}
pub(super) fn retain_dlq_task(task: Option<tokio::task::JoinHandle<()>>) {
    if let Some(task) = task {
        DLQ_TASKS.with(|tasks| tasks.borrow_mut().push(task));
    }
}
pub(super) async fn flush_dlq_tasks() {
    let tasks = DLQ_TASKS.with(|tasks| std::mem::take(&mut *tasks.borrow_mut()));
    for task in tasks {
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
    }
}

struct EvidenceFrame {
    turn_start: u64,
    batch_start: u64,
    tail: String,
}

impl From<(u64, u64)> for EvidenceFrame {
    fn from((turn_start, batch_start): (u64, u64)) -> Self {
        Self {
            turn_start,
            batch_start,
            tail: String::new(),
        }
    }
}

async fn plan(
    f: &Fixture,
    shared: &Arc<SharedData>,
    http: &Arc<serenity::Http>,
    before: Option<InflightTurnState>,
    producer: Option<RelayProducer>,
    target: &mut Option<SessionBoundRelayAckTarget>,
    mirrored: bool,
    frame: impl Into<EvidenceFrame>,
) {
    let EvidenceFrame {
        turn_start,
        batch_start,
        tail,
    } = frame.into();
    let authorized = before.as_ref().is_some_and(|row| {
        row.turn_nonce == f.row.turn_nonce
            && row.effective_relay_owner_kind()
                != crate::services::discord::inflight::RelayOwnerKind::None
            && batch_start == row.turn_start_offset.unwrap_or(0)
    });
    let provider = f.row.provider_kind().unwrap();
    let session = f.row.tmux_session_name.clone().unwrap();
    let output = f.source.path.display().to_string();
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        &session,
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: f.row.runtime_kind.unwrap(),
            output_path: output.clone(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let context = TerminalRelayPlanContext {
        evidence_source: Some((f.source.dev, f.source.ino)),
        http,
        shared,
        channel_id: ChannelId::new(f.row.channel_id),
        watcher_provider: &provider,
        tmux_session_name: &session,
        output_path: &output,
        inflight_before_relay: &before,
        cached_relay_producer: &producer,
        prompt_anchor_present_before_relay: false,
        external_input_lease_before_relay: false,
        session_bound_relay_turn_fully_mirrored: mirrored,
        session_bound_relay_turn_first_forwarded_sequence: target.as_ref().map(|t| t.sequence),
        split_trailing_turn_follows: false,
        startup_soft_terminal_authority: watcher_soft_terminal_has_turn_authority(
            Some(&f.row),
            &session,
            batch_start,
            f.row.turn_nonce.as_deref(),
        ),
    };
    let mut claimed = false;
    let mut finished = false;
    let mut synthetic = None;
    let mut generation = None;
    let mut state = TerminalRelayPlanState {
        all_data_session_bound_relay_ack: target,
        monitor_auto_turn_claimed: &mut claimed,
        monitor_auto_turn_finished: &mut finished,
        monitor_auto_turn_synthetic_msg_id: &mut synthetic,
        monitor_auto_turn_ledger_generation: &mut generation,
    };
    let tools = WatcherToolState::new();
    let locals = TerminalRelayPlanLocals {
        current_offset: f.row.last_offset + tail.len() as u64,
        data_start_offset: batch_start,
        turn_data_start_offset: turn_start,
        all_data: &tail,
        full_response: &f.body,
        current_response: &f.body,
        response_sent_offset: 0,
        has_assistant_response: true,
        terminal_kind: Some(WatcherTerminalKind::SoftStopHookSummary),
        task_notification_kind: None,
        assistant_text_seen: true,
        fresh_assistant_text_seen: true,
        tool_state: &tools,
        placeholder_msg_id: None,
        status_panel_msg_id: None,
    };
    let sends = f.sends.load(Ordering::SeqCst);
    let TerminalRelayPlanOutcome::Proceed(result) =
        run_terminal_relay_plan(&context, locals, &mut state).await
    else {
        panic!("orphan seam bypassed")
    };
    crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(&session);
    flush_dlq_tasks().await;
    assert_eq!(result.watcher_direct_fallback_authorized, authorized);
    assert_eq!(
        result.watcher_direct_fallback_after_session_bound_ack,
        authorized
    );
    assert_eq!(
        f.sends.load(Ordering::SeqCst),
        sends,
        "terminal evidence cannot send a legacy body"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn i7441b_actual_o_terminal_denials_and_true_orphans_pg() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "i7441b_terminal",
        "O terminal coverage",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    for (provider_index, provider) in [ProviderKind::Claude, ProviderKind::Codex]
        .into_iter()
        .enumerate()
    {
        for denial in 0..3 {
            for delivered in [true, false] {
                let mut f = Fixture::new(
                    744120 + provider_index as u64 * 100 + denial * 2 + u64::from(delivered),
                    provider.clone(),
                    &["preserved tail"],
                )
                .await;
                let kind = if provider == ProviderKind::Claude {
                    crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui
                } else {
                    crate::services::agent_protocol::RuntimeHandoffKind::CodexTui
                };
                let _o = crate::services::tui_o::cutover::test_override::force_channels(&[(
                    f.row.channel_id,
                    kind,
                )]);
                let (http, _http) = f.http().await;
                let shared = crate::services::discord::make_shared_data_for_tests_with_storage(
                    Some(pool.clone()),
                );
                save_inflight_state(&f.row).unwrap();
                let before = match denial {
                    0 => {
                        crate::services::discord::inflight::clear_inflight_state_for_captured_episode(
                        &provider,
                        f.row.channel_id,
                        &InflightTurnIdentity::from_state(&f.row),
                        f.row.turn_nonce.as_deref(),
                    );
                        assert!(load_inflight_state(&provider, f.row.channel_id).is_none());
                        None
                    }
                    1 => {
                        let mut b = f.row.clone();
                        b.turn_start_offset = Some(f.row.last_offset + 1);
                        save_inflight_state(&b).unwrap();
                        Some(b)
                    }
                    _ => {
                        let mut b = f.row.clone();
                        b.turn_nonce = Some("successor".into());
                        b.relay_owner_kind =
                            crate::services::discord::inflight::RelayOwnerKind::Watcher;
                        save_inflight_state(&b).unwrap();
                        Some(b)
                    }
                };
                if !delivered {
                    f.port.messages.lock().unwrap()[0].content = "wrong payload".into();
                }
                let frontier = shared
                    .tmux_relay_coord(ChannelId::new(f.row.channel_id))
                    .confirmed_end_offset
                    .load(Ordering::Acquire);
                plan(&f, &shared, &http, before, None, &mut None, false, (0, 0)).await;
                assert_eq!(
                    shared
                        .tmux_relay_coord(ChannelId::new(f.row.channel_id))
                        .confirmed_end_offset
                        .load(Ordering::Acquire),
                    frontier
                );
                let rows: Vec<(String, String)> =
                    tokio::time::timeout(std::time::Duration::from_secs(2), async {
                        loop {
                            let rows = sqlx::query_as(
                                "SELECT content,reason FROM relay_dead_letter WHERE channel_id=$1",
                            )
                            .bind(f.row.channel_id.to_string())
                            .fetch_all(&pool)
                            .await
                            .unwrap();
                            if delivered || !rows.is_empty() {
                                break rows;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                        }
                    })
                    .await
                    .expect("DLQ must contain the preserved orphan tail");
                assert_eq!(rows.len(), usize::from(!delivered));
                if delivered {
                    assert!(f.get_calls.load(Ordering::SeqCst) > 0, "exact GET must run");
                }
                if !delivered {
                    assert_eq!(rows[0].0, f.body);
                    assert!(rows[0].1.contains(
                        [
                            "no_inflight_row",
                            "turn_start_outside_frame",
                            "turn_nonce_mismatch"
                        ][denial as usize]
                    ));
                }
                assert_eq!(f.writer.store().ledger().violation(), None);
            }
        }
    }
    pool.close().await;
    db.drop().await;
}

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
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let channel = ChannelId::new(f.row.channel_id);
        f.row.tmux_session_name =
            Some(format!("AgentDesk-claude-i7441b-soft-{}", f.row.channel_id));
        let session = f.row.tmux_session_name.clone().unwrap();
        let _live = InjectedLivenessGuard::set(HostSessionRef::tmux(&session), HostLiveness::Live);
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
        while handle.metrics().snapshot().frames_delivered == 0 {
            tokio::task::yield_now().await;
        }
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
            wait_for_session_bound_relay_delivery_ack(Some(&ack_probe), std::time::Duration::ZERO)
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
        handle.shutdown().await;
    }
    pool.close().await;
    db.drop().await;
}

pub(in crate::services::discord) async fn assert_receipted_fixture_suppressed(
    f: &Fixture,
    pool: &sqlx::PgPool,
) {
    let _o = crate::services::tui_o::cutover::test_override::force_channels(&[(
        f.row.channel_id,
        f.row.runtime_kind.unwrap(),
    )]);
    let (http, _http_guard) = f.http().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    for denial in 0..3 {
        let provider = f.row.provider_kind().unwrap();
        save_inflight_state(&f.row).unwrap();
        let before = if denial == 0 {
            crate::services::discord::inflight::clear_inflight_state_for_captured_episode(
                &provider,
                f.row.channel_id,
                &InflightTurnIdentity::from_state(&f.row),
                f.row.turn_nonce.as_deref(),
            );
            assert!(load_inflight_state(&provider, f.row.channel_id).is_none());
            None
        } else {
            let mut b = f.row.clone();
            if denial == 1 {
                b.turn_start_offset = Some(f.row.last_offset + 1);
            } else {
                b.turn_nonce = Some("next input".into());
                b.relay_owner_kind = crate::services::discord::inflight::RelayOwnerKind::Watcher;
            }
            save_inflight_state(&b).unwrap();
            Some(b)
        };
        let reads = f.get_calls.load(Ordering::SeqCst);
        plan(
            f,
            &shared,
            &http,
            before,
            None,
            &mut None,
            false,
            (
                f.row.turn_start_offset.unwrap_or(0),
                f.row.turn_start_offset.unwrap_or(0),
            ),
        )
        .await;
        assert!(f.get_calls.load(Ordering::SeqCst) > reads);
    }

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM relay_dead_letter WHERE channel_id=$1")
            .bind(f.row.channel_id.to_string())
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(
        count, 0,
        "shared receipt must reach the production terminal seam"
    );
}

#[tokio::test]
async fn i7441b_watcher_delegated_capture_survives_foreign_row_clear() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let f = Fixture::new(744183, ProviderKind::Claude, &["whole prefix. ", "tail"]).await;
    let (http, _guard) = f.http().await;
    let shared = crate::services::discord::make_shared_data_for_tests();
    let _o = crate::services::tui_o::cutover::test_override::force_channels(&[(
        f.row.channel_id,
        crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
    )]);
    let mut b = f.row.clone();
    b.user_msg_id += 99;
    b.turn_nonce = Some("B".into());
    save_inflight_state(&b).unwrap();
    let identity = InflightTurnIdentity::from_state(&f.row);
    let mut generation = None;
    super::o_delegated_arm::consume_delegated_terminal(super::o_delegated_arm::DelegatedTerminal {
        http: &http,
        shared: &shared,
        provider: &ProviderKind::Claude,
        channel_id: ChannelId::new(f.row.channel_id),
        tmux_session_name: f.row.tmux_session_name.as_deref().unwrap(),
        placeholder_msg_id: None,
        inflight_before_relay: Some(&f.row),
        inflight_identity_before_relay: Some(&identity),
        consumed_end: f.row.last_offset,
        response_sent_offset: f.body.len(),
        last_edit_text: &f.body,
        turn_data_start_offset: 0,
        observed_generation_mtime_ns: &mut generation,
        task_card: None,
    })
    .await
    .unwrap();
    crate::services::discord::outbound::completed_turn_ledger::o_pending::project(
        &shared,
        &ProviderKind::Claude,
        f.row.channel_id,
    )
    .await;
    let ids = crate::services::discord::outbound::completed_turn_ledger::settled_user_msg_ids(
        &ProviderKind::Claude,
        f.row.channel_id,
    );
    assert!(ids.contains(&f.row.user_msg_id));
    assert!(!ids.contains(&b.user_msg_id));
    assert_eq!(
        load_inflight_state(&ProviderKind::Claude, f.row.channel_id)
            .unwrap()
            .turn_nonce,
        b.turn_nonce
    );
}

pub(in crate::services::discord) async fn assert_uncovered_fixture_recorded(
    f: &Fixture,
    pool: &sqlx::PgPool,
) {
    let (http, _guard) = f.http().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    plan(
        f,
        &shared,
        &http,
        None,
        None,
        &mut None,
        false,
        (
            f.row.turn_start_offset.unwrap_or(0),
            f.row.turn_start_offset.unwrap_or(0),
        ),
    )
    .await;
    let rows: Vec<String> = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let rows: Vec<String> =
                sqlx::query_scalar("SELECT content FROM relay_dead_letter WHERE channel_id=$1")
                    .bind(f.row.channel_id.to_string())
                    .fetch_all(pool)
                    .await
                    .unwrap();
            if !rows.is_empty() {
                break rows;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(rows, vec![f.body.clone()]);
}

#[tokio::test(flavor = "current_thread")]
async fn i7441b_authorized_direct_drops_unknown_evidence_before_one_second() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let mut f = Fixture::new(744202, ProviderKind::Claude, &["body"]).await;
    f.row
        .set_relay_owner_kind(crate::services::discord::inflight::RelayOwnerKind::Watcher);
    let _o = crate::services::tui_o::cutover::test_override::force_channels(&[(
        f.row.channel_id,
        crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
    )]);
    let _binding = crate::services::tui_o::cutover::test_override::bind_claude_tui_session(
        f.row.tmux_session_name.as_ref().unwrap(),
        f.source.path.to_str().unwrap(),
    );
    let (http, _http) = f.http().await;
    let shared = crate::services::discord::make_shared_data_for_tests();
    save_inflight_state(&f.row).unwrap();
    f.port.messages.lock().unwrap().clear();
    let mut target = None;
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        plan(
            &f,
            &shared,
            &http,
            Some(f.row.clone()),
            None,
            &mut target,
            false,
            (0, 0),
        ),
    )
    .await
    .expect("authorized terminal must drop unresolved evidence");
}

#[tokio::test(flavor = "current_thread")]
async fn i7441b_legacy_channel_never_uses_stale_o_receipts_pg() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "i7441b_legacy",
        "stale O receipts",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    let f = Fixture::new(744204, ProviderKind::Claude, &["body"]).await;
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    let reads = f.get_calls.load(Ordering::SeqCst);
    assert_uncovered_fixture_recorded(&f, &pool).await;
    assert_eq!(f.get_calls.load(Ordering::SeqCst), reads);
    pool.close().await;
    db.drop().await;
}

/// Records Claude writes after an injected turn's Stop hook summary; none carries a body.
fn append_after_stop_hook(f: &mut Fixture) {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&f.source.path)
        .unwrap();
    for record in [
        serde_json::json!({"type":"system","subtype":"turn_duration","durationMs":1200}),
        serde_json::json!({"type":"attachment","attachment":{"type":"queued_command"}}),
        serde_json::json!({"type":"last-prompt","lastPrompt":"monitor"}),
    ] {
        let line = format!("{record}\n");
        file.write_all(line.as_bytes()).unwrap();
        f.row.last_offset += line.len() as u64;
    }
}

/// An injected turn's frame that ran past its Stop hook summary is recorded only when O did not
/// post its body, and suppression must come from an exact GET.
async fn assert_injected_tail_case(
    pool: &sqlx::PgPool,
    channel: u64,
    outside_frame: bool,
    delivered: bool,
    replayed: bool,
) {
    let mut f = Fixture::injected_with_prefix(
        channel,
        &["background task finished. ", "monitor followup"],
        4096,
    )
    .await;
    append_after_stop_hook(&mut f);
    let _o = crate::services::tui_o::cutover::test_override::force_channels(&[(
        f.row.channel_id,
        crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
    )]);
    let (http, _http) = f.http().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    assert!(load_inflight_state(&ProviderKind::Claude, channel).is_none());
    let before = outside_frame.then(|| {
        let mut row = f.row.clone();
        row.turn_start_offset = Some(f.row.last_offset + 1);
        save_inflight_state(&row).unwrap();
        row
    });
    if !delivered {
        f.port.messages.lock().unwrap()[0].content = "wrong payload".into();
    }
    let turn_start = f.row.turn_start_offset.unwrap();
    // A restart replay hands the leftover turn a read batch that began at the file end.
    let batch_start = if replayed {
        f.row.last_offset
    } else {
        turn_start
    };
    let reads = f.get_calls.load(Ordering::SeqCst);
    plan(
        &f,
        &shared,
        &http,
        before,
        None,
        &mut None,
        false,
        (turn_start, batch_start),
    )
    .await;
    let rows: Vec<String> =
        sqlx::query_scalar("SELECT content FROM relay_dead_letter WHERE channel_id=$1")
            .bind(channel.to_string())
            .fetch_all(pool)
            .await
            .unwrap();
    let expected = if delivered {
        Vec::new()
    } else {
        vec![f.body.clone()]
    };
    assert_eq!(
        rows, expected,
        "outside_frame={outside_frame} delivered={delivered} replayed={replayed}"
    );
    assert!(!delivered || f.get_calls.load(Ordering::SeqCst) > reads);
}

#[tokio::test(flavor = "current_thread")]
async fn i7441d_injected_turn_past_stop_hook_records_only_unposted_body_pg() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "i7441d_tail",
        "injected turn tail",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    for (index, (outside_frame, delivered)) in
        [(false, true), (false, false), (true, true), (true, false)]
            .into_iter()
            .enumerate()
    {
        assert_injected_tail_case(
            &pool,
            744400 + index as u64,
            outside_frame,
            delivered,
            false,
        )
        .await;
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn i7441d_replayed_leftover_turn_proves_from_turn_start_pg() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "i7441d_replay",
        "leftover turn replay",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    for (index, delivered) in [true, false].into_iter().enumerate() {
        assert_injected_tail_case(&pool, 744410 + index as u64, false, delivered, true).await;
    }
    pool.close().await;
    db.drop().await;
}

async fn residual_frame(replayed: bool, delivered: bool, large: bool) {
    residual_frame_with_duration(replayed, delivered, large, false).await;
}

async fn residual_frame_with_duration(
    replayed: bool,
    delivered: bool,
    large: bool,
    duration: bool,
) {
    use std::io::Write;
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "i7441r_large_frame",
        "long terminal evidence frame",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    let mut f = Fixture::continued_frame(
        744500
            + u64::from(replayed)
            + 2 * u64::from(delivered)
            + 4 * u64::from(large)
            + 8 * u64::from(duration),
        large,
        duration,
    )
    .await;
    assert_eq!(f.port.messages.lock().unwrap().len(), 2);
    append_after_stop_hook(&mut f);
    let raw = std::fs::read(&f.source.path).unwrap();
    let turn_start = f.row.turn_start_offset.unwrap();
    let start_after_prompt = turn_start
        + raw[turn_start as usize..]
            .iter()
            .position(|b| *b == b'\n')
            .unwrap() as u64
        + 1;
    let mut buffer = String::from_utf8(raw[start_after_prompt as usize..].to_vec()).unwrap();
    let mut parsed = String::new();
    let outcome = crate::services::discord::tmux::process_watcher_lines_for_turn(
        &mut buffer,
        &mut crate::services::session_backend::StreamLineState::new(),
        &mut parsed,
        &mut WatcherToolState::new(),
        Some(start_after_prompt),
        Some(turn_start),
    );
    assert!(outcome.soft_terminal_candidate && !outcome.found_result);
    assert_eq!(
        parsed, f.body,
        "the watcher includes the late body after the soft Stop hint"
    );
    let tail = if replayed {
        let tail = format!(
            "{}\n",
            serde_json::json!({"type":"user","uuid":"next-turn","message":{"content":"next input"}})
        );
        std::fs::OpenOptions::new()
            .append(true)
            .open(&f.source.path)
            .unwrap()
            .write_all(tail.as_bytes())
            .unwrap();
        tail
    } else {
        String::new()
    };
    let batch_start = if replayed {
        f.row.last_offset + tail.len() as u64
    } else {
        start_after_prompt
    };
    let mut before = f.row.clone();
    before.turn_start_offset = Some(turn_start);
    save_inflight_state(&before).unwrap();
    let authority = watcher_soft_terminal_has_turn_authority(
        Some(&before),
        f.row.tmux_session_name.as_deref().unwrap(),
        batch_start,
        before.turn_nonce.as_deref(),
    );
    assert_eq!(
        authority
            .authorize_pre_relay_inflight(Some(&before), f.row.last_offset + tail.len() as u64),
        Err(SoftTerminalAuthorityDenial::TurnStartOutsideFrame)
    );
    assert_eq!(
        f.row.last_offset - start_after_prompt > 2 * 1024 * 1024,
        large
    );
    if !delivered {
        if replayed && !duration {
            f.port.messages.lock().unwrap().clear();
        } else {
            let mut messages = f.port.messages.lock().unwrap();
            let missing = messages.pop().unwrap();
            assert!(missing.content.contains("monitor followup"));
            assert_eq!(messages.len(), 1);
            assert!(messages[0].content.contains("background task finished."));
        }
    }
    let _o = crate::services::tui_o::cutover::test_override::force_channels(&[(
        f.row.channel_id,
        crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
    )]);
    let (http, _http_guard) = f.http().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let reads = f.get_calls.load(Ordering::SeqCst);
    plan(
        &f,
        &shared,
        &http,
        Some(before),
        None,
        &mut None,
        false,
        EvidenceFrame {
            turn_start: if replayed {
                turn_start
            } else {
                start_after_prompt
            },
            batch_start,
            tail,
        },
    )
    .await;
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT content, reason FROM relay_dead_letter WHERE channel_id=$1")
            .bind(f.row.channel_id.to_string())
            .fetch_all(&pool)
            .await
            .unwrap();
    eprintln!(
        "RESIDUAL replayed={replayed} delivered={delivered} large={large} duration={duration} frame_bytes={} jsonl_start={batch_start} jsonl_end={} rows={} gets={}",
        f.row.last_offset - start_after_prompt,
        f.row.last_offset,
        rows.len(),
        f.get_calls.load(Ordering::SeqCst) - reads
    );
    if delivered {
        assert!(
            rows.is_empty(),
            "already posted body was recorded: {rows:?}"
        );
        assert!(f.get_calls.load(Ordering::SeqCst) > reads);
    } else {
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, f.body);
        assert!(rows[0].1.contains("denial=turn_start_outside_frame"));
    }
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn i7441r_restart_replay_posted_large_frame_has_no_dead_letter_pg() {
    residual_frame(true, true, true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn i7441r_live_midturn_posted_large_frame_has_no_dead_letter_pg() {
    residual_frame(false, true, true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn i7441r_live_midturn_soft_hint_keeps_late_posted_body_pg() {
    residual_frame(false, true, false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn i7441r_unposted_large_frames_remain_recorded_pg() {
    for large in [true, false] {
        residual_frame(true, false, large).await;
        residual_frame(false, false, large).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn i7441r1_duration_small_continuation_posted_has_no_dead_letter_pg() {
    residual_frame_with_duration(false, true, false, true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn i7441r1_duration_large_live_posted_has_no_dead_letter_pg() {
    residual_frame_with_duration(false, true, true, true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn i7441r1_duration_large_replay_posted_has_no_dead_letter_pg() {
    residual_frame_with_duration(true, true, true, true).await;
}

#[tokio::test(flavor = "current_thread")]
async fn i7441r1_duration_missing_late_body_remains_recorded_pg() {
    for large in [false, true] {
        for replayed in [false, true] {
            residual_frame_with_duration(replayed, false, large, true).await;
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum HeadlessOutbox {
    Sent,
    SessionBanner,
    Missing,
    EarlierTurn,
    Pending,
    OtherBody,
    OtherSource,
    UntimedBody,
}

/// Restart's first headless turn: the bridge posted via the outbox and cleared its row before the
/// watcher framed the late body O never captured; only that turn's sent row replaces the record.
async fn headless_restart_frame(pool: &sqlx::PgPool, channel: u64, outbox: HeadlessOutbox) {
    let mut f = Fixture::new(channel, ProviderKind::Claude, &["[E2E:E1:OK]"]).await;
    let body = f.body.clone();
    let written = chrono::Utc::now() - chrono::Duration::seconds(1);
    let mut late = serde_json::json!({"type":"assistant","uuid":"a-late","apiBlockIndex":0,
        "message":{"id":"body-late","content":[{"type":"text","text":body}]}});
    if !matches!(outbox, HeadlessOutbox::UntimedBody) {
        late["timestamp"] = written.to_rfc3339().into();
    }
    let bridge_range = [
        serde_json::json!({"type":"user","uuid":"input","sessionId":"native-session","message":{"content":"E1"}}),
        serde_json::json!({"type":"system","subtype":"stop_hook_summary"}),
    ]
    .map(|record| format!("{record}\n"))
    .concat();
    let next = format!(
        "{}\n",
        serde_json::json!({"type":"user","uuid":"next","sessionId":"native-session","message":{"content":"E2"}})
    );
    let path = f.root.join(format!("restart-{channel}.jsonl"));
    std::fs::write(&path, format!("{bridge_range}{late}\n{next}")).unwrap();
    f.source =
        crate::services::tui_o::shadow::binding_reader::source_id_for("native-session", &path)
            .unwrap();
    f.row.turn_start_offset = Some(bridge_range.len() as u64);
    f.row.last_offset = (bridge_range.len() + late.to_string().len() + 1) as u64;
    let banner = format!("⚙ session restored\n\n{body}");
    let (content, source, status, age) = match outbox {
        HeadlessOutbox::SessionBanner => (Some(banner.as_str()), "headless_turn", "sent", 0),
        HeadlessOutbox::Missing => (None, "headless_turn", "sent", 0),
        HeadlessOutbox::EarlierTurn => (Some(body.as_str()), "headless_turn", "sent", 3600),
        HeadlessOutbox::Pending => (Some(body.as_str()), "headless_turn", "pending", 0),
        HeadlessOutbox::OtherBody => (Some("[E2E:E0:OK]"), "headless_turn", "sent", 0),
        HeadlessOutbox::OtherSource => (Some(body.as_str()), "system", "sent", 0),
        HeadlessOutbox::Sent | HeadlessOutbox::UntimedBody => {
            (Some(body.as_str()), "headless_turn", "sent", 0)
        }
    };
    if let Some(content) = content {
        sqlx::query(
            "INSERT INTO message_outbox (target, content, bot, source, status, created_at, sent_at)
             VALUES ($1, $2, 'claude', $3, $4, NOW() - make_interval(secs => $5), NOW())",
        )
        .bind(format!("channel:{channel}"))
        .bind(content)
        .bind(source)
        .bind(status)
        .bind(age as f64)
        .execute(pool)
        .await
        .unwrap();
    }
    let _o = crate::services::tui_o::cutover::test_override::force_channels(&[(
        channel,
        crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
    )]);
    let (http, _http) = f.http().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let turn_start = f.row.turn_start_offset.unwrap();
    plan(
        &f,
        &shared,
        &http,
        None,
        None,
        &mut None,
        false,
        EvidenceFrame {
            turn_start,
            batch_start: turn_start,
            tail: next,
        },
    )
    .await;
    let rows: Vec<String> =
        sqlx::query_scalar("SELECT content FROM relay_dead_letter WHERE channel_id=$1")
            .bind(channel.to_string())
            .fetch_all(pool)
            .await
            .unwrap();
    let expected = match outbox {
        HeadlessOutbox::Sent | HeadlessOutbox::SessionBanner => Vec::new(),
        _ => vec![body],
    };
    assert_eq!(rows, expected, "{outbox:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn i7441r3_headless_bridge_delivery_before_late_frame_needs_its_sent_outbox_row_pg() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
        "i7441r3_headless",
        "headless outbox terminal evidence",
    )
    .await;
    let pool = db.connect_and_migrate().await;
    for (index, outbox) in [
        HeadlessOutbox::Sent,
        HeadlessOutbox::SessionBanner,
        HeadlessOutbox::Missing,
        HeadlessOutbox::EarlierTurn,
        HeadlessOutbox::Pending,
        HeadlessOutbox::OtherBody,
        HeadlessOutbox::OtherSource,
        HeadlessOutbox::UntimedBody,
    ]
    .into_iter()
    .enumerate()
    {
        headless_restart_frame(&pool, 744_130 + index as u64, outbox).await;
    }
    pool.close().await;
    db.drop().await;
}
