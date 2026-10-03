//! A handoff on a pane the Herdr admission may withhold stamps only its locator before the claim;
//! the owner follows an admitted claim, a Legacy pane's owner is durable before it, as on main.

use std::sync::mpsc;

use super::*;
use crate::services::discord::inflight::RelayOwnerKind;
use crate::services::discord::outbound::delivery_record::watcher_owner_channel_for_delivery_channel;
use crate::services::tmux_common::session_temp_path;
use crate::services::tui_prompt_dedupe::{admit_herdr_execution, install_herdr_execution};

#[derive(Clone, Copy, Debug)]
enum Pane {
    Legacy,
    Withheld,
    Admitted,
}

/// The owner a handoff leaves: the row's owner field and relay owner, and the delivery context.
#[derive(Debug, PartialEq)]
struct Owner {
    field: Option<u64>,
    relay: RelayOwnerKind,
    context: Option<ChannelId>,
}

fn owner_of(provider: &ProviderKind, channel: u64, tmux: &str) -> Owner {
    let row = load_inflight_state(provider, channel).expect("row");
    let context =
        watcher_owner_channel_for_delivery_channel(provider, ChannelId::new(channel), tmux);
    Owner {
        field: row.watcher_owner_channel_id,
        relay: row.effective_relay_owner_kind(),
        context,
    }
}

fn no_owner() -> Owner {
    Owner {
        field: None,
        relay: RelayOwnerKind::None,
        context: None,
    }
}

fn standby_owner(channel: u64) -> Owner {
    Owner {
        field: Some(channel),
        relay: RelayOwnerKind::StandbyRelay,
        context: Some(ChannelId::new(channel)),
    }
}

/// Parks this thread's next claim at its admission and reads the durable owner there.
fn owner_at_claim(provider: &ProviderKind, channel: u64, tmux: &str) -> mpsc::Receiver<Owner> {
    let (paused_tx, paused_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    crate::services::discord::tmux::CLAIM_PAUSE.set(Some((paused_tx, resume_rx)));
    let (seen_tx, seen_rx) = mpsc::channel();
    let (provider, tmux) = (provider.clone(), tmux.to_string());
    std::thread::spawn(move || {
        paused_rx.recv().expect("the claim reached its admission");
        seen_tx.send(owner_of(&provider, channel, &tmux)).unwrap();
        resume_tx.send(()).unwrap();
    });
    seen_rx
}

fn message(tmux_ready: bool, tmux: &str, output: &str) -> RuntimeHandoffLoopMessage {
    let (tmux_session_name, last_offset) = (tmux.to_string(), 2_048);
    if tmux_ready {
        return RuntimeHandoffLoopMessage::TmuxReady {
            output_path: output.to_string(),
            input_fifo_path: format!("{output}.input"),
            tmux_session_name,
            last_offset,
        };
    }
    let handoff = RuntimeHandoff::CodexTui {
        rollout_path: output.to_string(),
        thread_id: Some("codex-thread-p8-4".to_string()),
        tmux_session_name,
        last_offset,
    };
    RuntimeHandoffLoopMessage::RuntimeReady { handoff }
}

// C1 (RuntimeReady) and C2 (TmuxReady): a withheld pane leaves no owner, context, relay owner or
// watcher; an admitted one gets its owner after the claim, a Legacy one before it, as on main.
#[tokio::test(flavor = "current_thread")]
async fn a_withheld_handoff_records_no_owner_and_a_legacy_one_records_it_before_the_claim() {
    let _lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("runtime root");
    let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let provider = ProviderKind::Codex;
    // Legacy runs last, after another pane was listed: only its own listing could withhold it.
    let cases = [Pane::Withheld, Pane::Admitted, Pane::Legacy];
    for (index, pane) in cases.into_iter().enumerate() {
        for tmux_ready in [false, true] {
            let channel = 5_340_840 + 2 * index as u64 + u64::from(tmux_ready);
            let case = format!("{pane:?} tmux_ready={tmux_ready}");
            let tmux = format!("AgentDesk-codex-p8-4-{channel}");
            let output = format!("/runtime/p8-4-{channel}.jsonl");
            let mut state = runtime_seed(provider.clone(), channel);
            state.watcher_owner_channel_id = None;
            save_inflight_state(&state).expect("seed row");
            // The delivery context is read only against a live tmux generation.
            std::fs::write(session_temp_path(&tmux, "generation"), "p8-4").unwrap();
            let shared = crate::services::discord::make_shared_data_for_tests();
            shared.http.cached_bot_token.set("p8-4".into()).unwrap();
            if !matches!(pane, Pane::Legacy) {
                std::fs::write(session_temp_path(&tmux, "host_kind"), "herdr").unwrap();
            }
            if matches!(pane, Pane::Admitted) {
                install_herdr_execution(&tmux, "p8-4-n1");
                admit_herdr_execution(&tmux, "p8-4-n1");
            }
            if !matches!(pane, Pane::Withheld) {
                let incumbent = live_watcher_handle(&tmux, &output);
                shared
                    .tmux_watchers
                    .insert(ChannelId::new(channel), incumbent);
            }
            let at_claim = owner_at_claim(&provider, channel, &tmux);
            let mut dirty = false;
            let handoff = message(tmux_ready, &tmux, &output);
            let observed =
                dispatch_process_handoff(&shared, &provider, &mut state, handoff, &mut dirty, true)
                    .await;
            crate::services::discord::tmux::CLAIM_PAUSE.set(None);

            let at_claim = at_claim.recv().expect("owner read at the claim");
            let after = owner_of(&provider, channel, &tmux);
            assert_eq!(observed.outcome, Some(GuardedSaveOutcome::Saved), "{case}");
            let row = load_inflight_state(&provider, channel).unwrap();
            assert_eq!(
                row.tmux_session_name.as_deref(),
                Some(tmux.as_str()),
                "{case}: locator"
            );
            // The ready frame ends the terminal drain wait; only a watcher the runtime handoff
            // adopted owes a drain, as on main the tmux handoff weighs none.
            assert!(observed.terminal_control_drain_until.is_none(), "{case}");
            let adopted = !matches!(pane, Pane::Withheld) && !tmux_ready;
            assert_eq!(
                observed.adopted_after_done, adopted,
                "{case}: watcher drain"
            );
            let reused = WatcherHandoffClaimOutcome::ReusedExisting;
            match pane {
                Pane::Legacy => {
                    assert_eq!(
                        at_claim,
                        standby_owner(channel),
                        "{case}: owner before claim"
                    );
                    assert_eq!(after, standby_owner(channel), "{case}");
                    assert_eq!(observed.claim_outcome, reused, "{case}");
                    assert!(observed.tmux_handed_off, "{case}");
                }
                Pane::Withheld => {
                    assert_eq!(at_claim, no_owner(), "{case}: owner before claim");
                    assert_eq!(after, no_owner(), "{case}: no owner effect");
                    let none = WatcherHandoffClaimOutcome::None;
                    assert_eq!(observed.claim_outcome, none, "{case}");
                    assert!(!observed.tmux_handed_off, "{case}");
                    assert!(!observed.watcher_relay_available, "{case}");
                    assert!(observed.watcher_delivery_pin.is_none(), "{case}");
                    assert_eq!(observed.watcher_slots, 0, "{case}: no watcher");
                    assert_eq!(
                        observed.watcher_owner_channel_id,
                        ChannelId::new(channel),
                        "{case}: local owner untouched"
                    );
                    assert_eq!(state.watcher_owner_channel_id, None, "{case}: owner setter");
                    assert_eq!(
                        state.effective_relay_owner_kind(),
                        RelayOwnerKind::None,
                        "{case}"
                    );
                }
                Pane::Admitted => {
                    assert_eq!(at_claim, no_owner(), "{case}: owner waits for the claim");
                    assert_eq!(after, standby_owner(channel), "{case}: owner after claim");
                    assert_eq!(observed.claim_outcome, reused, "{case}");
                    assert!(observed.tmux_handed_off, "{case}");
                }
            }
        }
    }
}
