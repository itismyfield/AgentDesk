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
                !sweep(&run.shared, &record).await.cleared,
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

fn debts(run: &Run) -> Vec<Value> {
    let dir = super::super::super::super::runtime_store::runtime_root()
        .unwrap()
        .join("discord_herdr_body_debt")
        .join(run.provider.as_str());
    let mut records = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "json") {
                records.push(serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap());
            }
        }
    }
    records
}

async fn held_body_retires(rig: herdr::P11TerminationRig, channel: u64) {
    retire_held_body(rig, channel, false).await;
}

fn remove_source_log(channel: u64) {
    let log = crate::services::tui_prompt_dedupe::binding_events::log_path(channel)
        .unwrap()
        .unwrap();
    std::fs::remove_file(log).unwrap();
}

async fn retire_held_body(rig: herdr::P11TerminationRig, channel: u64, no_source: bool) {
    let run = Run::start(
        &rig,
        Setup {
            provider: ProviderKind::Claude,
            channel,
            routed: true,
        },
    )
    .await;
    if no_source {
        remove_source_log(channel);
        assert!(
            super::super::super::logged_source(channel, &run.logical, herdr::EXECUTION).is_err()
        );
    }
    let snapshot = crate::services::tui_o::cutover::boot_snapshot().unwrap();
    let expected = if snapshot
        .candidate(channel)
        .is_some_and(|c| c.peek() == crate::services::tui_o::channel_policy::Adoption::Refused)
    {
        crate::services::tui_o::cutover::IdentityError::AdoptionRefused
    } else {
        crate::services::tui_o::cutover::IdentityError::AwaitingAdoption
    };
    assert_eq!(
        crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel(
            channel,
            run.row.runtime_kind
        ),
        Err(expected)
    );
    run.relay.set(|pane| pane.exited = true);
    let (taken, taking) = run.sentinel().await;
    let http = run.shared.serenity_http_or_token_fallback().unwrap();
    restore_inflight_turns(&http, &run.shared, &run.provider).await;
    run.pass().await;
    assert!(
        run.durable().is_none(),
        "an ended execution no longer holds the channel"
    );
    assert!(!herdr::held(), "the old execution input hold is released");
    assert!(
        run.texts().is_empty(),
        "no Legacy body or invented failure receipt"
    );
    let archived = debts(&run);
    assert!(
        !archived.is_empty(),
        "the old body is durable before its active row disappears"
    );
    let saved = archived
        .iter()
        .find(|record| {
            let row: Value = serde_json::from_str(record["row_json"].as_str().unwrap()).unwrap();
            row["turn_nonce"] == NONCE && row["full_response"] == PARTIAL
        })
        .expect("the old input and complete saved partial survive");
    assert_eq!(saved["disposition"], "retained");
    assert!(saved["recovery_notice"].as_str().unwrap().contains(PARTIAL));
    assert!(
        wait(Duration::from_secs(15), || !taken
            .lock()
            .unwrap()
            .is_empty())
        .await
    );
    assert_eq!(
        *taken.lock().unwrap(),
        vec![false],
        "one input after row removal"
    );
    drop(taking);
    let guard = crate::services::discord::router::thread_guard_test_api::queue_guard_active;
    assert!(!guard(&run.shared, &run.provider, run.channel).await);
    assert_eq!(
        crate::services::discord::health::external_turn_hold_for_start(
            None,
            &run.provider,
            channel
        )
        .await,
        None
    );
    let before = debts(&run);
    let mut successor = run.row.clone();
    successor.turn_nonce = Some("new-turn-after-debt".into());
    successor.user_msg_id += 100;
    successor.born_generation = CURRENT;
    inflight::save_inflight_state(&successor).unwrap();
    assert_eq!(
        debts(&run),
        before,
        "a new active row does not overwrite the old body"
    );
    let path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &run.provider,
        channel,
    );
    let mut next: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    next["born_generation"] = PRIOR.into();
    next["full_response"] = "다음 실행의 다른 본문".into();
    next["started_at"] = (chrono::Utc::now() + chrono::Duration::seconds(60))
        .to_rfc3339()
        .into();
    std::fs::write(&path, serde_json::to_vec(&next).unwrap()).unwrap();
    run.pass().await;
    assert!(run.durable().is_none(), "the later ended row also detaches");
    let after = debts(&run);
    assert!(
        after.iter().any(|record| before.contains(record)),
        "the original immutable snapshot remains"
    );
    assert!(
        after.iter().any(|record| {
            let row: Value = serde_json::from_str(record["row_json"].as_str().unwrap()).unwrap();
            row["full_response"] == "다음 실행의 다른 본문"
        }),
        "the successor body has its own snapshot"
    );
    run.close().await;
}

#[test]
fn a_deferred_restarted_herdr_body_releases_its_row_into_durable_debt_pg() {
    in_child(
        concat!(
            module_path!(),
            "::a_deferred_restarted_herdr_body_releases_its_row_into_durable_debt_pg"
        ),
        1_479_671_399_000_151,
        ProviderKind::Claude,
        Owner::Deferred,
        |rig| held_body_retires(rig, 1_479_671_399_000_151),
    );
}

#[test]
fn a_refused_restarted_herdr_body_releases_its_row_into_durable_debt_pg() {
    in_child(
        concat!(
            module_path!(),
            "::a_refused_restarted_herdr_body_releases_its_row_into_durable_debt_pg"
        ),
        1_479_671_399_000_152,
        ProviderKind::Claude,
        Owner::Refused,
        |rig| held_body_retires(rig, 1_479_671_399_000_152),
    );
}

#[test]
fn a_foreign_restarted_herdr_body_releases_its_row_into_durable_debt_pg() {
    in_child(
        concat!(
            module_path!(),
            "::a_foreign_restarted_herdr_body_releases_its_row_into_durable_debt_pg"
        ),
        1_479_671_399_000_153,
        ProviderKind::Claude,
        Owner::Foreign,
        |rig| held_body_retires(rig, 1_479_671_399_000_153),
    );
}

#[test]
fn an_unknown_restarted_herdr_owner_keeps_its_body_row_and_hold_pg() {
    in_child(
        concat!(
            module_path!(),
            "::an_unknown_restarted_herdr_owner_keeps_its_body_row_and_hold_pg"
        ),
        1_479_671_399_000_154,
        ProviderKind::Claude,
        Owner::Unknown,
        |rig| async move {
            let mut run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: 1_479_671_399_000_154,
                    routed: true,
                },
            )
            .await;
            run.row.runtime_kind = None;
            inflight::save_inflight_state(&run.row).unwrap();
            run.row = run.durable().unwrap();
            run.relay.set(|pane| pane.exited = true);
            run.pass().await;
            assert_kept(&run, &run.row, "unknown output identity").await;
            assert!(debts(&run).is_empty());
            run.close().await;
        },
    );
}

#[test]
fn a_sourceless_restarted_herdr_body_releases_only_its_execution_hold_pg() {
    in_child(
        concat!(
            module_path!(),
            "::a_sourceless_restarted_herdr_body_releases_only_its_execution_hold_pg"
        ),
        1_479_671_399_000_155,
        ProviderKind::Claude,
        Owner::Deferred,
        |rig| retire_held_body(rig, 1_479_671_399_000_155, true),
    );
}

#[test]
fn every_body_copy_and_directory_flush_failure_keeps_the_herdr_row_pg() {
    use crate::services::discord::runtime_store::durable_probe::{Plan, Step, Watch};
    in_child(
        concat!(
            module_path!(),
            "::every_body_copy_and_directory_flush_failure_keeps_the_herdr_row_pg"
        ),
        1_479_671_399_000_156,
        ProviderKind::Claude,
        Owner::Deferred,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: 1_479_671_399_000_156,
                    routed: true,
                },
            )
            .await;
            run.relay.set(|pane| pane.exited = true);
            let directory = super::super::super::super::runtime_store::runtime_root()
                .unwrap()
                .join("discord_herdr_body_debt");
            for step in [
                Step::CreateTemp,
                Step::Write,
                Step::FileSync,
                Step::ParentOpen,
                Step::ParentSync,
                Step::Ancestors,
            ] {
                let watch = Watch::new(
                    &directory,
                    Plan {
                        fail: vec![step],
                        block: None,
                    },
                );
                run.pass().await;
                assert!(
                    watch.count(step) > 0,
                    "the real I/O stage {step:?} was reached"
                );
                assert_kept(&run, &run.row, &format!("{step:?}")).await;
                drop(watch);
            }
            run.pass().await;
            assert!(
                run.durable().is_none(),
                "retry after flush recovery detaches the same row"
            );
            assert!(!debts(&run).is_empty());
            assert!(run.texts().is_empty());
            run.close().await;
        },
    );
}

#[test]
fn a_successor_after_the_body_archive_is_never_cleared_pg() {
    in_child(
        concat!(
            module_path!(),
            "::a_successor_after_the_body_archive_is_never_cleared_pg"
        ),
        1_479_671_399_000_157,
        ProviderKind::Claude,
        Owner::Deferred,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: 1_479_671_399_000_157,
                    routed: true,
                },
            )
            .await;
            run.relay.set(|pane| pane.exited = true);
            let mut successor = run.row.clone();
            successor.turn_nonce = Some("after-archive-successor".into());
            successor.born_generation = CURRENT;
            let next = successor.clone();
            on("after_archive", move || {
                let next = next.clone();
                Box::pin(async move {
                    inflight::save_inflight_state(&next).unwrap();
                })
            });
            run.pass().await;
            assert_eq!(run.durable().unwrap().turn_nonce, successor.turn_nonce);
            assert!(!debts(&run).is_empty(), "the old body is still retained");
            assert!(run.texts().is_empty());
            run.close().await;
        },
    );
}

#[test]
fn an_o_owner_without_old_range_proof_still_archives_the_herdr_body_pg() {
    in_child(
        concat!(
            module_path!(),
            "::an_o_owner_without_old_range_proof_still_archives_the_herdr_body_pg"
        ),
        1_479_671_399_000_158,
        ProviderKind::Claude,
        Owner::O,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: 1_479_671_399_000_158,
                    routed: true,
                },
            )
            .await;
            assert_eq!(
                crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel(
                    run.channel.get(),
                    run.row.runtime_kind
                ),
                Ok(true)
            );
            remove_source_log(run.channel.get());
            run.relay.set(|pane| pane.exited = true);
            run.pass().await;
            assert!(run.durable().is_none());
            assert!(
                debts(&run).iter().any(|record| {
                    let row: Value =
                        serde_json::from_str(record["row_json"].as_str().unwrap()).unwrap();
                    row["full_response"] == PARTIAL
                }),
                "new ownership is not old-body range evidence"
            );
            assert!(!run.texts().iter().any(|text| text.contains(PARTIAL)));
            run.close().await;
        },
    );
}

#[test]
fn a_restarted_herdr_debt_survives_each_copy_clear_closed_crash_cut_pg() {
    const CHANNEL: u64 = 1_479_671_399_000_159;
    if herdr::is_child() {
        let phase = std::env::var(PHASE).unwrap();
        let root = PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap());
        if phase == "after"
            && !root
                .join("runtime/discord_inflight/claude")
                .join(format!("{CHANNEL}.json"))
                .exists()
        {
            let env = crate::config::set_agentdesk_root_for_test(&root);
            assert_eq!(
                inflight::herdr_body_debt::reconcile(&ProviderKind::Claude),
                vec![CHANNEL]
            );
            let directory = root.join("runtime/discord_herdr_body_debt/claude");
            assert!(std::fs::read_dir(directory).unwrap().any(|entry| {
                let path = entry.unwrap().path();
                path.extension().is_some_and(|ext| ext == "json")
                    && std::fs::read_to_string(path).unwrap().contains(PARTIAL)
            }));
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            assert_eq!(
                runtime.block_on(
                    crate::services::discord::health::external_turn_hold_for_start(
                        None,
                        &ProviderKind::Claude,
                        CHANNEL
                    )
                ),
                None
            );
            drop(env);
            return;
        }
        return child(
            CHANNEL,
            ProviderKind::Claude,
            Owner::Deferred,
            |rig| async move {
                let run = Run::start(
                    &rig,
                    Setup {
                        provider: ProviderKind::Claude,
                        channel: CHANNEL,
                        routed: true,
                    },
                )
                .await;
                run.relay.set(|pane| pane.exited = true);
                if phase == "before" {
                    let point = match std::env::var("ADK_TEST_HERDR_DEBT_CUT").unwrap().as_str() {
                        "before_archive" => "before_archive",
                        "after_archive" => "after_archive",
                        "before_clear" => "before_clear",
                        "before_closed" => "before_closed",
                        _ => panic!("a named crash cut"),
                    };
                    on(point, || Box::pin(async { std::process::exit(73) }));
                    run.pass().await;
                    panic!("the selected crash cut was not reached");
                }
                run.pass().await;
                assert!(run.durable().is_none());
                assert!(
                    debts(&run)
                        .iter()
                        .any(|record| record["row_json"].as_str().unwrap().contains(PARTIAL))
                );
                assert!(run.texts().is_empty());
                run.close().await;
            },
        );
    }
    let path = concat!(
        module_path!(),
        "::a_restarted_herdr_debt_survives_each_copy_clear_closed_crash_cut_pg"
    );
    for point in [
        "before_archive",
        "after_archive",
        "before_clear",
        "before_closed",
    ] {
        let root = tempfile::tempdir().unwrap();
        for phase in ["before", "after"] {
            let home = tempfile::tempdir().unwrap();
            let output = herdr::child_command(path, CHANNEL, home.path())
                .env(PHASE, phase)
                .env("ADK_TEST_HERDR_DEBT_CUT", point)
                .env("AGENTDESK_ROOT_DIR", root.path())
                .output()
                .unwrap();
            let expected = if phase == "before" { Some(73) } else { Some(0) };
            assert_eq!(
                output.status.code(),
                expected,
                "{point}/{phase}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

#[test]
fn a_native_herdr_answer_survives_source_rotation_in_its_body_debt_pg() {
    in_child(
        concat!(
            module_path!(),
            "::a_native_herdr_answer_survives_source_rotation_in_its_body_debt_pg"
        ),
        1_479_671_399_000_160,
        ProviderKind::Claude,
        Owner::Deferred,
        |rig| async move {
            let run = Run::start(
                &rig,
                Setup {
                    provider: ProviderKind::Claude,
                    channel: 1_479_671_399_000_160,
                    routed: true,
                },
            )
            .await;
            append(&run.source, &terminal(&run.provider, &run.row));
            run.relay.set(|pane| pane.screen = Some(READY.into()));
            run.pass().await;
            assert!(run.durable().is_none());
            assert!(run.texts().is_empty());
            std::fs::rename(&run.source, run.source.with_extension("rotated")).unwrap();
            let records = debts(&run);
            let record = records
                .iter()
                .find(|record| record["evidence"]["outcome"]["kind"] == "terminal")
                .expect("the native answer, rather than only the older saved partial, is retained");
            let native = &record["evidence"]["outcome"];
            let text = native["output"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect::<String>();
            assert!(text.contains(ANSWER) || native["result"].as_str().unwrap().contains(ANSWER));
            assert!(native["end"].as_u64().unwrap() > native["start"].as_u64().unwrap());
            assert!(record["recovery_notice"].as_str().unwrap().contains(ANSWER));
            assert_eq!(record["disposition"], "retained");
            run.close().await;
        },
    );
}
