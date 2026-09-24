//! #5704: a TUI-direct synthetic turn claimed while the watcher waits on the
//! terminal relay ack is invisible to the pre-relay snapshot. The terminal commit
//! that clears its row must release its exact mailbox episode in the same pass.
use super::*;
use crate::services::tui_prompt_dedupe::{
    ExternalInputRelayLease, ExternalInputRelayOwner, TuiRuntimeBinding,
};

const WATCHER_SESSION: &str = "synthetic-mailbox-release-5704";

#[derive(Clone, Copy, PartialEq, Eq)]
enum LateRow {
    /// The row is this committed range's turn (the live 07:30:32 shape).
    InRange,
    /// The row belongs to another tmux session than the committing watcher.
    OtherSession,
    /// The row starts after this committed range (a newer turn).
    Newer,
    /// The on-disk row was re-stamped after the late read, so the guarded clear
    /// refuses: this pass clears nothing and must release nothing.
    Replaced,
}

fn register_binding(tmux: &str, output: &std::path::Path) {
    crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
        tmux,
        TuiRuntimeBinding {
            runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            output_path: output.to_str().unwrap().into(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        },
    );
}

async fn claim(
    shared: &Arc<SharedData>,
    channel: serenity::ChannelId,
    tmux: &str,
    output: &std::path::Path,
    anchor: serenity::MessageId,
) -> bool {
    register_binding(tmux, output);
    let mut lease = ExternalInputRelayLease::unassigned(Some(channel.get()));
    lease.turn_id = Some(format!("external-5704-{}", anchor.get()));
    lease.relay_owner = ExternalInputRelayOwner::BridgeAdapter;
    lease.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui);
    let lease =
        crate::services::tui_prompt_dedupe::record_external_input_turn_lease("claude", tmux, lease);
    crate::services::discord::tui_prompt_relay::claim_tui_direct_synthetic_turn_for_tests(
        shared,
        &ProviderKind::Claude,
        channel,
        tmux,
        "tui-direct prompt",
        anchor,
        &lease,
    )
    .await
}

async fn commit(
    shared: &Arc<SharedData>,
    channel: serenity::ChannelId,
    output: &str,
    inflight_before_relay: &Option<InflightTurnState>,
    inflight_state: &Option<InflightTurnState>,
    data_start_offset: u64,
    current_offset: u64,
) {
    let tmux = WATCHER_SESSION.to_string();
    let output = output.to_string();
    let completion_is_stale_for_newer_turn = committed_completion_is_stale_for_newer_turn(
        inflight_before_relay.as_ref(),
        inflight_state.as_ref(),
        &tmux,
        current_offset,
    );
    let anchor_cleanup_is_stale_for_newer_turn = committed_anchor_cleanup_is_stale_for_newer_turn(
        inflight_before_relay.as_ref(),
        inflight_state.as_ref(),
        &tmux,
        current_offset,
    );
    let context = TerminalCommitEpilogueContext {
        shared,
        channel_id: channel,
        watcher_provider: &ProviderKind::Claude,
        provider_kind: &ProviderKind::Claude,
        tmux_session_name: &tmux,
        output_path: &output,
        relay_coord: &Arc::new(TmuxRelayCoord::new(channel)),
        turn_delivered: &Arc::new(AtomicBool::new(false)),
    };
    run_terminal_commit_epilogue(
        &context,
        TerminalCommitEpilogueLocals {
            terminal_output_committed: true,
            lifecycle_stage_paused: false,
            relay_suppressed: false,
            has_assistant_response: true,
            completion_is_stale_for_newer_turn,
            anchor_cleanup_is_stale_for_newer_turn,
            inflight_state,
            inflight_before_relay,
            full_response: &"synthetic terminal body".to_string(),
            // The watcher attached before the synthetic row existed: no nonce.
            watcher_turn_nonce: &None,
            resolved_did: &None,
            dispatch_ok: true,
            terminal_delivery_committed: true,
            watcher_tui_gate_outcome: TuiCompletionGateOutcome::NotGated,
            tui_direct_anchor_terminal_body_visible: false,
            terminal_kind: None,
            terminal_evidence_offset: Some(current_offset.saturating_sub(1)),
            finish_mailbox_on_completion: false,
            pre_panel_release_drove_finalize: false,
            current_offset,
            data_start_offset,
        },
        &mut TerminalCommitEpilogueState {
            turn_result_relayed: &mut false,
            watcher_direct_terminal_idle_committed: &mut true,
            monitor_auto_turn_claimed: &mut false,
            monitor_auto_turn_finished: &mut false,
            monitor_auto_turn_synthetic_msg_id: &mut None,
            monitor_auto_turn_ledger_generation: &mut None,
        },
    )
    .await;
}

/// Returns (row still on disk, mailbox active id after commit, follow-up claimed).
fn run(late_row: LateRow, snapshot_has_row: bool) -> (bool, Option<serenity::MessageId>, bool) {
    let _env = crate::services::observability::lock_env_then_runtime();
    let temp = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let shared = crate::services::discord::make_shared_data_for_tests();
            let channel = serenity::ChannelId::new(570_400_001);
            let anchor = serenity::MessageId::new(1_552_447_036_652_134_502);
            let output = temp.path().join("transcript.jsonl");
            // Prior turns already filled the transcript: the synthetic turn starts mid-file.
            std::fs::write(&output, "{}\n".repeat(512)).unwrap();
            let row_session = if late_row == LateRow::OtherSession {
                "synthetic-mailbox-release-5704-other"
            } else {
                WATCHER_SESSION
            };
            for tmux in [WATCHER_SESSION, row_session] {
                let generation =
                    crate::services::tmux_common::session_temp_path(tmux, "generation");
                std::fs::write(generation, b"1").unwrap();
            }

            // Terminal preflight snapshot: taken before the synthetic row exists.
            let preflight = crate::services::discord::inflight::load_inflight_state(
                &ProviderKind::Claude,
                channel.get(),
            );
            assert!(preflight.is_none());
            // TUI-direct prompt claims the synthetic turn during the relay ack wait.
            assert!(claim(&shared, channel, row_session, &output, anchor).await);
            assert_eq!(
                crate::services::discord::mailbox_snapshot(&shared, channel)
                    .await
                    .active_user_message_id,
                Some(anchor)
            );
            // Late re-read after the relay: the synthetic row is now on disk.
            let late = crate::services::discord::inflight::load_inflight_state(
                &ProviderKind::Claude,
                channel.get(),
            );
            let row = late.clone().expect("synthetic claim persisted its row");
            assert_eq!(row.user_msg_id, anchor.get());
            let start = row.turn_start_offset.unwrap_or(row.last_offset);
            let (data_start_offset, current_offset) = if late_row == LateRow::Newer {
                (start.saturating_sub(64), start)
            } else {
                (start + 1, start + 64)
            };
            let inflight_before_relay = if snapshot_has_row {
                late.clone()
            } else {
                preflight
            };
            if late_row == LateRow::Replaced {
                let mut restamped = row.clone();
                restamped.started_at = "2026-09-24 07:30:31".into();
                crate::services::discord::inflight::save_inflight_state(&restamped).unwrap();
            }
            commit(
                &shared,
                channel,
                output.to_str().unwrap(),
                &inflight_before_relay,
                &late,
                data_start_offset,
                current_offset,
            )
            .await;

            let row_on_disk = crate::services::discord::inflight::load_inflight_state(
                &ProviderKind::Claude,
                channel.get(),
            )
            .is_some();
            let active = crate::services::discord::mailbox_snapshot(&shared, channel)
                .await
                .active_user_message_id;
            // The next TUI prompt must be able to claim the mailbox.
            let follow_up = if row_on_disk {
                false
            } else {
                claim(
                    &shared,
                    channel,
                    WATCHER_SESSION,
                    &output,
                    serenity::MessageId::new(1_552_447_044_399_009_824),
                )
                .await
            };
            (row_on_disk, active, follow_up)
        })
}

#[test]
fn synthetic_row_born_after_preflight_releases_its_mailbox_on_terminal_commit() {
    let (row_on_disk, active, follow_up) = run(LateRow::InRange, false);
    assert!(!row_on_disk, "the terminal commit clears the synthetic row");
    assert_eq!(
        active, None,
        "clearing the synthetic row must also finish its mailbox episode (#5704)"
    );
    assert!(
        follow_up,
        "the next TUI-direct prompt must not see 'mailbox already owns a different turn'"
    );
}

#[test]
fn synthetic_row_pinned_by_preflight_still_releases_its_mailbox() {
    let (row_on_disk, active, follow_up) = run(LateRow::InRange, true);
    assert!(!row_on_disk);
    assert_eq!(active, None);
    assert!(follow_up);
}

#[test]
fn late_row_from_another_session_is_never_finished_by_this_watcher() {
    let (_, active, _) = run(LateRow::OtherSession, false);
    assert_eq!(
        active,
        Some(serenity::MessageId::new(1_552_447_036_652_134_502)),
        "a watcher must not finish another session's mailbox episode"
    );
}

#[test]
fn newer_late_row_keeps_its_row_and_mailbox() {
    let (row_on_disk, active, _) = run(LateRow::Newer, false);
    assert!(
        row_on_disk,
        "a newer turn's row is not this range's to clear"
    );
    assert_eq!(
        active,
        Some(serenity::MessageId::new(1_552_447_036_652_134_502)),
        "a newer turn's mailbox episode is never finished by an older range"
    );
}

#[test]
fn refused_guarded_clear_releases_nothing() {
    let (row_on_disk, active, _) = run(LateRow::Replaced, false);
    assert!(
        row_on_disk,
        "the re-stamped row is not the row this pass read"
    );
    assert_eq!(
        active,
        Some(serenity::MessageId::new(1_552_447_036_652_134_502)),
        "release is tied to this pass's own clear, never to a row it did not clear"
    );
}
