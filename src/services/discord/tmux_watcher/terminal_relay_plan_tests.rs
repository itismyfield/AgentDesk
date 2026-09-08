//! #5175 soft-terminal delivery-authority tests for the terminal relay plan.
//!
//! Split out of `terminal_relay_plan.rs` to keep that module inside the
//! `src/services/discord/tmux_watcher/**` namespace size cap.

use super::*;
use crate::services::discord::inflight::RelayOwnerKind;

const SESSION: &str = "AgentDesk-claude-adk-cc";
const FRAME_START: u64 = 1_534_426;
const TURN_START: u64 = 1_534_500;
const FRAME_END: u64 = 1_650_085;
const WATCHER_NONCE: &str = "nonce-bound-while-consuming-this-turn";

fn row(turn_nonce: Option<&str>, owner: RelayOwnerKind) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        42,
        Some("adk-cc".to_string()),
        7,
        0,
        0,
        "prompt".to_string(),
        None,
        Some(SESSION.to_string()),
        Some("/tmp/out.jsonl".to_string()),
        Some("/tmp/in.fifo".to_string()),
        TURN_START,
    );
    state.turn_start_offset = Some(TURN_START);
    state.turn_nonce = turn_nonce.map(str::to_owned);
    state.set_relay_owner_kind(owner);
    state
}

/// The binding a TUI-direct turn produces: the pre-turn startup snapshot is
/// absent, so the pre-#5175 verdict is false.
fn tui_direct_binding() -> WatcherSoftTerminalAuthority {
    watcher_soft_terminal_has_turn_authority(None, SESSION, FRAME_START, Some(WATCHER_NONCE))
}

#[test]
fn soft_terminal_authority_reads_the_pre_relay_row_not_the_startup_snapshot_5175() {
    let binding = tui_direct_binding();
    assert!(!binding.startup_snapshot_authorized());

    let (authorized, denial) = watcher_soft_terminal_direct_send_authority(
        &binding,
        Some(&row(Some(WATCHER_NONCE), RelayOwnerKind::Watcher)),
        FRAME_END,
        Some(WatcherTerminalKind::SoftStopHookSummary),
    );

    assert!(
        authorized,
        "a TUI-direct soft terminal must be authorized by the inflight row that exists at turn end"
    );
    assert_eq!(denial, None);
}

#[test]
fn missing_pre_relay_row_denies_soft_terminal_direct_send_5175() {
    let (authorized, denial) = watcher_soft_terminal_direct_send_authority(
        &tui_direct_binding(),
        None,
        FRAME_END,
        Some(WatcherTerminalKind::SoftStopHookSummary),
    );

    assert!(!authorized);
    assert_eq!(denial, Some(SoftTerminalAuthorityDenial::NoInflightRow));
}

#[test]
fn forged_soft_terminal_is_denied_even_when_the_startup_snapshot_authorized_5175() {
    // The snapshot verdict is TRUE here (exact resume-floor match on the
    // pre-turn snapshot). If the decision still consulted it, a forged
    // ownerless row at turn end would be waved through.
    let mut snapshot = row(Some(WATCHER_NONCE), RelayOwnerKind::Watcher);
    snapshot.turn_start_offset = Some(FRAME_START);
    snapshot.last_offset = FRAME_START;
    let binding = watcher_soft_terminal_has_turn_authority(
        Some(&snapshot),
        SESSION,
        FRAME_START,
        Some(WATCHER_NONCE),
    );
    assert!(binding.startup_snapshot_authorized());

    let (authorized, denial) = watcher_soft_terminal_direct_send_authority(
        &binding,
        Some(&row(Some(WATCHER_NONCE), RelayOwnerKind::None)),
        FRAME_END,
        Some(WatcherTerminalKind::SoftStopHookSummary),
    );

    assert!(!authorized);
    assert_eq!(denial, Some(SoftTerminalAuthorityDenial::RelayOwnerNone));
}

#[test]
fn compact_forged_nonce_is_denied_at_the_direct_send_seam_5175() {
    let (authorized, denial) = watcher_soft_terminal_direct_send_authority(
        &tui_direct_binding(),
        Some(&row(
            Some("compact-rewritten-nonce"),
            RelayOwnerKind::Watcher,
        )),
        FRAME_END,
        Some(WatcherTerminalKind::SoftStopHookSummary),
    );

    assert!(!authorized);
    assert_eq!(denial, Some(SoftTerminalAuthorityDenial::TurnNonceMismatch));
}

#[test]
fn hard_result_terminal_keeps_its_recovery_fallback_and_reports_no_denial_5175() {
    // Control group: the `hard_result` watcher_direct lane that already
    // worked on other channels must stay authorized with no inflight row at
    // all, and must not be blamed for a soft-contract denial.
    for terminal_kind in [Some(WatcherTerminalKind::HardResult), None] {
        let (authorized, denial) = watcher_soft_terminal_direct_send_authority(
            &tui_direct_binding(),
            None,
            FRAME_END,
            terminal_kind,
        );
        assert!(authorized, "hard terminal fallback must be preserved");
        assert_eq!(denial, None);
    }
}

#[test]
fn production_call_site_feeds_the_pre_relay_inflight_row_5175() {
    // The unit tests above pin the decision; this pins the WIRING, which is
    // where #5175 actually lived. Rewiring the call site back to the
    // pre-turn snapshot (or starving it of the row) must not be silent.
    let source = include_str!("terminal_relay_plan.rs");
    let call_site = source
        .split_once("let (watcher_direct_fallback_authorized, soft_terminal_authority_denial) =")
        .expect("the terminal relay plan must decide soft-terminal authority")
        .1
        .split_once(");")
        .expect("the authority call must terminate")
        .0;
    assert!(
        call_site.contains("watcher_soft_terminal_direct_send_authority("),
        "authority must be decided by the seam these tests cover"
    );
    assert!(
        call_site.contains("inflight_before_relay.as_ref()"),
        "authority must be decided against the PRE-RELAY inflight row (#5175)"
    );
    assert!(
        call_site.contains("current_offset"),
        "the offset containment term needs the consumed offset (#5175)"
    );
}

#[test]
fn late_watcher_after_partial_bridge_commit_is_stopped_before_direct_fallback_5071() {
    use super::super::pre_emit_guard::*;
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
            let channel = serenity::ChannelId::new(5_071_090_001);
            let session = "AgentDesk-claude-late-feeder-5071".to_string();
            let output = root.path().join("late.jsonl").display().to_string();
            std::fs::write(&output, vec![b' '; 1024]).unwrap();
            let generation_path =
                crate::services::tmux_common::session_temp_path(&session, "generation");
            std::fs::create_dir_all(std::path::Path::new(&generation_path).parent().unwrap())
                .unwrap();
            std::fs::write(generation_path, "same-generation").unwrap();
            let generation = dr::current_generation_mtime_ns(&session);
            let mut active = row(Some(WATCHER_NONCE), RelayOwnerKind::None);
            active.channel_id = channel.get();
            active.tmux_session_name = Some(session.clone());
            active.output_path = Some(output.clone());
            active.turn_start_offset = Some(0);
            active.last_offset = 64;
            active.turn_source = crate::services::discord::inflight::TurnSource::ExternalInput;
            active.full_response = "prefix suffix".to_string();
            active.response_sent_offset = "prefix ".len();
            crate::services::discord::inflight::save_inflight_state(&active).unwrap();
            let active = crate::services::discord::inflight::load_inflight_state(
                &ProviderKind::Claude,
                channel.get(),
            )
            .unwrap();
            dr::write_delivered_frontier(
                &ProviderKind::Claude,
                channel.get(),
                &session,
                dr::DeliveredCommit {
                    range: (0, 64),
                    generation_mtime_ns: generation,
                    attempts: 1,
                    panel_msg_id: None,
                    panel_channel_id: None,
                },
            )
            .unwrap();
            let row_before = serde_json::to_value(&active).unwrap();
            let http = Arc::new(serenity::Http::new("test-token"));
            let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let delivered = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (mut claimed, mut finished, mut mirrored) = (false, false, false);
            let (mut synthetic_id, mut ledger_generation, mut ack) = (None, None, None);
            let (mut first_sequence, mut last_offset, mut last_generation) = (None, None, None);
            let mut data_start = 128;
            let mut all_data = "{\"type\":\"result\",\"result\":\"prefix suffix\"}\n".to_string();
            let mut full_response = "prefix suffix".to_string();
            let pre_emit = run_pre_emit_guard(
                &PreEmitGuardContext {
                    http: &http,
                    shared: &shared,
                    channel_id: channel,
                    watcher_provider: &ProviderKind::Claude,
                    tmux_session_name: &session,
                    output_path: &output,
                    paused: &paused,
                    pause_epoch: &epoch,
                    turn_delivered: &delivered,
                },
                PreEmitGuardLocals {
                    epoch_snapshot: 0,
                    monitor_auto_turn_deferred: false,
                    placeholder_msg_id: None,
                    turn_data_start_offset: 128,
                    current_offset: 256,
                    response_sent_offset: 0,
                    data_start_offset: 128,
                    stale_resume_detected: false,
                    last_edit_text: &String::new(),
                },
                &mut PreEmitGuardState {
                    monitor_auto_turn_claimed: &mut claimed,
                    monitor_auto_turn_finished: &mut finished,
                    monitor_auto_turn_synthetic_msg_id: &mut synthetic_id,
                    monitor_auto_turn_ledger_generation: &mut ledger_generation,
                    all_data: &mut all_data,
                    all_data_start_offset: &mut data_start,
                    all_data_fully_mirrored_to_session_relay: &mut mirrored,
                    all_data_session_bound_relay_ack: &mut ack,
                    all_data_first_forwarded_relay_sequence: &mut first_sequence,
                    last_relayed_offset: &mut last_offset,
                    last_observed_generation_mtime_ns: &mut last_generation,
                    full_response: &mut full_response,
                },
            )
            .await;
            let mut duplicate_candidate = false;
            if matches!(pre_emit, PreEmitGuardOutcome::Proceed) {
                let inflight = Some(active.clone());
                let tool_state = WatcherToolState::new();
                let plan = run_terminal_relay_plan(
                    &TerminalRelayPlanContext {
                        http: &http,
                        shared: &shared,
                        channel_id: channel,
                        watcher_provider: &ProviderKind::Claude,
                        tmux_session_name: &session,
                        output_path: &output,
                        inflight_before_relay: &inflight,
                        cached_relay_producer: &None,
                        prompt_anchor_present_before_relay: false,
                        external_input_lease_before_relay: true,
                        session_bound_relay_turn_fully_mirrored: false,
                        session_bound_relay_turn_first_forwarded_sequence: None,
                        split_trailing_turn_follows: false,
                        startup_soft_terminal_authority: watcher_soft_terminal_has_turn_authority(
                            Some(&active),
                            &session,
                            128,
                            Some(WATCHER_NONCE),
                        ),
                    },
                    TerminalRelayPlanLocals {
                        current_offset: 256,
                        data_start_offset: 128,
                        all_data: &all_data,
                        full_response: &full_response,
                        current_response: &full_response,
                        response_sent_offset: 0,
                        has_assistant_response: true,
                        terminal_kind: Some(WatcherTerminalKind::HardResult),
                        task_notification_kind: None,
                        assistant_text_seen: true,
                        fresh_assistant_text_seen: true,
                        tool_state: &tool_state,
                        placeholder_msg_id: None,
                        status_panel_msg_id: None,
                    },
                    &mut TerminalRelayPlanState {
                        all_data_session_bound_relay_ack: &mut ack,
                        monitor_auto_turn_claimed: &mut claimed,
                        monitor_auto_turn_finished: &mut finished,
                        monitor_auto_turn_synthetic_msg_id: &mut synthetic_id,
                        monitor_auto_turn_ledger_generation: &mut ledger_generation,
                    },
                )
                .await;
                if let TerminalRelayPlanOutcome::Proceed(plan) = plan {
                    duplicate_candidate = plan.watcher_direct_fallback_after_session_bound_ack
                        && plan.has_direct_terminal_response
                        && plan.direct_terminal_response.starts_with("prefix");
                }
            }
            let after = crate::services::discord::inflight::load_inflight_state(
                &ProviderKind::Claude,
                channel.get(),
            )
            .unwrap();
            assert_eq!(serde_json::to_value(&after).unwrap(), row_before);
            assert_eq!(
                dr::read_record(&ProviderKind::Claude, channel.get())
                    .unwrap()
                    .delivered_frontier
                    .unwrap()
                    .range,
                (0, 64)
            );
            assert_eq!(&after.full_response[after.response_sent_offset..], "suffix");
            assert!(
                !duplicate_candidate,
                "late watcher reaches the actual direct fallback with the already delivered prefix"
            );
            assert!(matches!(pre_emit, PreEmitGuardOutcome::ContinueWatcherLoop));
        });
}
