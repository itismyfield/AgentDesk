//! How a restarted turn's row settles once its end is seen: silently when O relayed the turn, with
//! a notice only when nothing named its end, and with new idle starts held until the clear.
use super::*;

/// Says neither that the turn finished nor that it was cut off.
const NEUTRAL: &str = "이전 요청의 완료 여부는 확인하지 못했습니다";
const SPINNER: &str = "⠋ Processing...";

/// The prompt's interrupt marker: the turn aborted.
fn aborted(row: &inflight::InflightTurnState) -> String {
    let interrupt = json!({"type": "user", "timestamp": stamp(row), "message": {"role": "user",
        "content": [{"type": "text", "text": "[Request interrupted by user]"}]}});
    format!("{interrupt}\n")
}

/// A second prompt before the turn's terminal, then that turn's own reply and end.
fn later_prompt(row: &inflight::InflightTurnState) -> String {
    let at = stamp(row);
    let records = [
        json!({"type": "user", "timestamp": at, "message": {"role": "user", "content": "follow-up"}}),
        json!({"type": "assistant", "timestamp": at, "message": {"content": [{"type": "text", "text": "the follow-up's reply"}]}}),
        json!({"type": "system", "subtype": "stop_hook_summary", "timestamp": at}),
        json!({"type": "system", "subtype": "turn_duration", "timestamp": at, "durationMs": 900}),
    ];
    records.map(|record| format!("{record}\n")).concat()
}

/// The row ended without a recovery notice, a neutral one or the turn's body on Discord.
async fn assert_silent(run: &Run) {
    let ended = wait(Duration::from_secs(30), || run.durable().is_none()).await;
    assert!(ended, "the row ends: {:?}", run.texts());
    let marker = interrupted_recovery_message(&run.row, "");
    let texts = run.texts();
    let written = |text: &str| texts.iter().any(|t| t.contains(text));
    assert!(
        !written(&marker) && !written(NEUTRAL),
        "no notice: {texts:?}"
    );
    assert!(
        !written(ANSWER) && !written(PARTIAL),
        "O owns the body: {texts:?}"
    );
    assert!(!herdr::held(), "the execution's input hold is released");
    let pending = crate::services::discord::runtime_store::runtime_root()
        .unwrap()
        .join("discord_pending_o_deliveries")
        .join(run.provider.as_str())
        .join(format!("{}.json", run.row.channel_id));
    let pending: serde_json::Value =
        serde_json::from_slice(&std::fs::read(pending).unwrap()).unwrap();
    assert!(
        pending
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == run.row.user_msg_id
                && p["nonce"] == run.row.turn_nonce.as_deref().unwrap())
    );
    assert!(
        !crate::services::discord::outbound::completed_turn_ledger::settled_user_msg_ids(
            &run.provider,
            run.row.channel_id
        )
        .contains(&run.row.user_msg_id)
    );
}

/// The turn's own card was deleted once, off the clear's path.
async fn assert_card_deleted(run: &Run) {
    let card = run.row.current_msg_id;
    let deleted = wait(Duration::from_secs(10), || run.mock.deleted() == vec![card]).await;
    assert!(
        deleted,
        "the card is deleted once: {:?}",
        run.mock.deleted()
    );
}

const HOLD: u64 = 1_479_671_399_000_121;

/// Idle right after a held pass (t0) with input queued: the edge's kick ends the row silently by
/// t0 + 5 s, the kickoff follows within 1 s, and the real start begins the input's turn by 15 s.
#[test]
fn a_queue_hold_ends_a_relayed_restarted_turn_before_the_next_input_pg() {
    let path = concat!(
        module_path!(),
        "::a_queue_hold_ends_a_relayed_restarted_turn_before_the_next_input_pg"
    );
    in_child(
        path,
        HOLD,
        ProviderKind::Claude,
        Owner::O,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: HOLD,
                    routed: true,
                },
            )
            .await;
            let _routing = run.real_start().await;
            run.mock.seed_message(run.row.current_msg_id, SPINNER);
            run.relay.set(|pane| pane.screen = Some(BUSY.into()));
            // The queued input stays for the real kickoff: the sentinel's own taking hook is dropped.
            drop(run.sentinel().await);
            let kicks = Arc::new(Mutex::new(Vec::new()));
            let seen = kicks.clone();
            let channel = run.channel;
            let first = run.row.user_msg_id;
            let _kicks = crate::services::discord::queue_io::set_idle_queue_kick_hook_for_tests(
                Arc::new(move |_, provider, kicked, _| {
                    if kicked == channel {
                        let row = inflight::load_inflight_state_read_only(&provider, kicked.get());
                        let present = row.is_some_and(|row| row.user_msg_id == first);
                        seen.lock().unwrap().push((Instant::now(), present));
                    }
                    Box::pin(async { None })
                }),
            );
            // Boot restore's pass holds the busy turn; no source tick runs after it.
            let http = run.shared.serenity_http_or_token_fallback().unwrap();
            restore_inflight_turns(&http, &run.shared, &run.provider).await;
            settled_tasks().await;
            assert!(run.durable().is_some(), "the busy pass keeps the row");
            let t0 = Instant::now();
            append(&run.source, &terminal(&ProviderKind::Claude, &run.row));
            run.relay.set(|pane| pane.screen = Some(READY.into()));
            crate::services::discord::schedule_deferred_idle_queue_kickoff_immediate(
                run.shared.clone(),
                run.provider.clone(),
                run.channel,
                "turn_presence_idle",
            );
            let input = |run: &Run| run.durable().filter(|row| row.user_msg_id == SENTINEL);
            let mut t_clear = None;
            while t0.elapsed() <= Duration::from_secs(5) {
                if run.durable().is_none_or(|row| row.user_msg_id == SENTINEL) {
                    t_clear = Some(Instant::now());
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let t_clear =
                t_clear.unwrap_or_else(|| panic!("no clear by t0 + 5 s: {:?}", run.texts()));
            let mut t_input = None;
            while t_clear.elapsed() <= Duration::from_secs(15) {
                if input(&run).is_some() {
                    t_input = Some(Instant::now());
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let t_input = t_input.expect("the queued input's turn starts by t_clear + 15 s");
            let after = kicks
                .lock()
                .unwrap()
                .iter()
                .find(|(_, present)| !present)
                .copied();
            let (t_kick, _) = after.expect("a kickoff after the clear");
            assert!(
                t_kick.saturating_duration_since(t_clear) <= Duration::from_secs(1),
                "the kickoff came {:?} after the clear",
                t_kick.saturating_duration_since(t_clear)
            );
            assert!(t_input <= t_clear + Duration::from_secs(15));
            let texts = run.texts();
            let marker = interrupted_recovery_message(&run.row, "");
            assert!(
                !texts
                    .iter()
                    .any(|t| t.contains(&marker) || t.contains(NEUTRAL)),
                "no notice: {texts:?}"
            );
            assert_card_deleted(&run).await;
            // The input's turn ends with the test: its reader would otherwise outlive the runtime.
            let active = crate::services::discord::mailbox_snapshot(&run.shared, run.channel).await;
            if let Some(token) = active.cancel_token {
                token
                    .cancelled
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            run.close().await;
        },
    );
}

const ABORTED: u64 = 1_479_671_399_000_122;

/// An O-owned turn that aborted ends silently too, and its own spinner goes.
#[test]
fn an_o_owned_aborted_turn_clears_its_spinner_silently_pg() {
    let path = concat!(
        module_path!(),
        "::an_o_owned_aborted_turn_clears_its_spinner_silently_pg"
    );
    in_child(
        path,
        ABORTED,
        ProviderKind::Claude,
        Owner::O,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: ABORTED,
                    routed: true,
                },
            )
            .await;
            run.mock.seed_message(run.row.current_msg_id, SPINNER);
            append(&run.source, &aborted(&run.row));
            run.relay.set(|pane| pane.screen = Some(READY.into()));
            run.pass().await;
            assert_silent(&run).await;
            assert_card_deleted(&run).await;
            run.close().await;
        },
    );
}

const LATER: u64 = 1_479_671_399_000_123;

/// The input reached the provider and a later prompt's turn closed before its terminal: on an
/// O-owned channel nothing was lost, so the row ends silently.
#[test]
fn an_o_owned_turn_with_a_later_prompt_ends_silently_pg() {
    let path = concat!(
        module_path!(),
        "::an_o_owned_turn_with_a_later_prompt_ends_silently_pg"
    );
    in_child(
        path,
        LATER,
        ProviderKind::Claude,
        Owner::O,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: LATER,
                    routed: true,
                },
            )
            .await;
            append(&run.source, &later_prompt(&run.row));
            run.relay.set(|pane| pane.screen = Some(READY.into()));
            run.pass().await;
            assert_silent(&run).await;
            run.close().await;
        },
    );
}

const ENDED: u64 = 1_479_671_399_000_124;

/// The execution ended right after its terminal: E reads the source after the end, so an O-owned
/// turn ends silently, the execution retired, never with the interrupt marker.
#[test]
fn an_o_owned_turn_ended_after_its_terminal_ends_silently_pg() {
    let path = concat!(
        module_path!(),
        "::an_o_owned_turn_ended_after_its_terminal_ends_silently_pg"
    );
    in_child(
        path,
        ENDED,
        ProviderKind::Claude,
        Owner::O,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: ENDED,
                    routed: true,
                },
            )
            .await;
            rig.restore_shell();
            append(&run.source, &terminal(&ProviderKind::Claude, &run.row));
            run.pass().await;
            assert_silent(&run).await;
            let retired = (herdr::HostedState::Retired, herdr::EXECUTION.to_string());
            assert_eq!(run.hosted().await, Some(retired));
            run.close().await;
        },
    );
}

const UNTIMED: u64 = 1_479_671_399_000_125;

/// A row whose input time does not parse cannot name the input's turn: the neutral notice, after
/// the saved partial, never the interrupt message.
#[test]
fn a_row_without_its_input_time_ends_with_the_neutral_notice_pg() {
    let path = concat!(
        module_path!(),
        "::a_row_without_its_input_time_ends_with_the_neutral_notice_pg"
    );
    in_child(
        path,
        UNTIMED,
        ProviderKind::Claude,
        Owner::Legacy,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: UNTIMED,
                    routed: true,
                },
            )
            .await;
            append(&run.source, &terminal(&ProviderKind::Claude, &run.row));
            let path = inflight::inflight_state_path(
                &inflight::inflight_runtime_root().unwrap(),
                &run.provider,
                run.channel.get(),
            );
            let mut json: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            json["started_at"] = "not a time".into();
            std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
            run.relay.set(|pane| pane.screen = Some(READY.into()));
            let (taken, _reset) = run.sentinel().await;
            run.pass().await;
            assert_ended(&run, NEUTRAL, &taken).await;
            let texts = run.texts();
            assert!(
                !texts.iter().any(|t| t.contains(&run.interrupted())),
                "{texts:?}"
            );
            assert!(!texts.iter().any(|t| t.contains(ANSWER)), "{texts:?}");
            run.close().await;
        },
    );
}

const REST: u64 = 1_479_671_399_000_126;

/// A restarted Herdr row holds every new start on its channel: Discord intake's queue guard reads
/// it as an active turn and `/turn/start` as an external one, so both queue instead of starting.
#[test]
fn a_restarted_herdr_row_holds_gateway_and_rest_starts_pg() {
    let path = concat!(
        module_path!(),
        "::a_restarted_herdr_row_holds_gateway_and_rest_starts_pg"
    );
    in_child(
        path,
        REST,
        ProviderKind::Claude,
        Owner::Legacy,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: REST,
                    routed: true,
                },
            )
            .await;
            let queue_guard =
                crate::services::discord::router::thread_guard_test_api::queue_guard_active;
            assert!(
                queue_guard(&run.shared, &run.provider, run.channel).await,
                "Discord intake queues"
            );
            let hold = crate::services::discord::health::external_turn_hold_for_start(
                None,
                &run.provider,
                run.channel.get(),
            )
            .await;
            assert_eq!(
                hold,
                Some(crate::services::discord::health::ExternalHold::Row),
                "REST queues"
            );
            run.close().await;
        },
    );
}

const SWEPT: u64 = 1_479_671_399_000_127;

/// An end pass that held and stopped, with no source tick left to wake it: the leaked-row sweep
/// leaves the row alone but asks its end once more, and that pass ends it.
#[test]
fn the_sweep_wakes_a_stalled_end_pass_pg() {
    let path = concat!(module_path!(), "::the_sweep_wakes_a_stalled_end_pass_pg");
    in_child(
        path,
        SWEPT,
        ProviderKind::Claude,
        Owner::Legacy,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: SWEPT,
                    routed: true,
                },
            )
            .await;
            run.relay.set(|pane| pane.screen = Some(BUSY.into()));
            let http = run.shared.serenity_http_or_token_fallback().unwrap();
            restore_inflight_turns(&http, &run.shared, &run.provider).await;
            run.pass().await;
            assert_kept(&run, &run.row, "busy").await;
            // The turn ends with no relay running, so nothing but the sweep asks for another pass.
            append(&run.source, &terminal(&ProviderKind::Claude, &run.row));
            run.relay.set(|pane| pane.screen = Some(READY.into()));
            let record =
                crate::services::discord::tui_direct_pending_start::TuiDirectPendingStart {
                    provider: run.provider.as_str().to_string(),
                    channel_id: SWEPT,
                    tmux_session_name: run.logical.clone(),
                    anchor_message_id: u64::MAX,
                    generation: run.shared.restart.current_generation,
                    ..Default::default()
                };
            let sweep = crate::services::discord::tui_direct_pending_start::demote_leaked_foreign_inflight_if_current;
            assert!(
                !sweep(&run.shared, &record).await,
                "the sweep never ends the row itself"
            );
            let ended = wait(Duration::from_secs(30), || run.durable().is_none()).await;
            assert!(ended, "the woken pass ends the row: {:?}", run.texts());
            settled_tasks().await;
            let texts = run.texts();
            assert_eq!(
                texts.iter().filter(|t| t.contains(ANSWER)).count(),
                1,
                "{texts:?}"
            );
            assert!(!herdr::held(), "the execution's input hold is released");
            run.close().await;
        },
    );
}
