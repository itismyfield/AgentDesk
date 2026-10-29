//! A Codex turn-mode turn stopped without an anchor, across a restart: the new process's boot
//! confirmation waits on its row, restore settles the row from the saved turn-mode fact, and the
//! channel's turn mode is confirmed again. Rows without that fact keep the Legacy hold.
use super::*;
use crate::services::agent_protocol::{NativeTerminalKind, RuntimeHandoffKind};
use crate::services::tui_o::channel_policy::{Adoption, BootChannels};
use crate::services::tui_o::turn_mode::{TestConfirmation, transcript_turns};

const CHANNEL: u64 = 1_479_671_301_387_207_507;
const USER: u64 = 2;
const ANCHOR: u64 = 3;
const NOTICE: u64 = 900_001;
const CURRENT: u64 = 9;

/// Where the prior process stopped.
#[derive(Clone, Copy, Debug)]
enum Stop {
    /// After its receipt was saved, before the stop was shown.
    AfterReceipt,
    /// After the stop was shown and acked, before the row was cleared.
    AfterNoticeAck,
    /// A native Aborted terminal not yet delivered.
    NativeAborted,
}

/// What the previous process left and what the new one must do with it.
struct Case {
    stop: Stop,
    anchor: u64,
    turn_mode: bool,
    replies: usize,
    settles: bool,
}

/// The new process's boot snapshot: the Codex channel O owns and the turn selection names.
fn boot() -> (
    crate::services::tui_o::cutover::test_override::ChannelsGuard,
    crate::services::tui_o::shadow::tap::TuiOConfig,
) {
    let config: crate::config::Config = serde_json::from_value(serde_json::json!({
        "server": {},
        "agents": [{"id": "writer", "name": "Writer", "channels": {
            "codex": {"id": CHANNEL.to_string(), "runtime": "tui"}
        }}],
        "tui_o": {"writer": {"channels": [CHANNEL]}, "turn": {"channels": [CHANNEL]}},
    }))
    .unwrap();
    let snapshot = BootChannels::validate(&config)
        .unwrap()
        .adopted(Adoption::Committed);
    let guard = crate::services::tui_o::cutover::test_override::force_boot(snapshot);
    (guard, config.tui_o.unwrap())
}

/// The row the previous process saved, written as it left it.
fn saved_row(case: &Case, nonce: &str) {
    let provider = ProviderKind::Codex;
    let name = provider.build_tmux_session_name(&format!("turn-mode-restart-{:?}", case.stop));
    let marker = crate::services::tmux_common::session_temp_path(&name, "host_kind");
    std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, "herdr").unwrap();
    let mut row = inflight::InflightTurnState::new(
        provider.clone(),
        CHANNEL,
        None,
        1,
        USER,
        case.anchor,
        "stopped input".to_string(),
        None,
        Some(name),
        None,
        None,
        0,
    );
    row.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
    row.turn_nonce = Some(nonce.to_string());
    row.anchorless_turn_mode = case.turn_mode;
    let mut value = serde_json::to_value(&row).unwrap();
    match case.stop {
        Stop::NativeAborted => {
            value["born_generation"] = 0.into();
            value["tui_terminal_kind"] = serde_json::to_value(NativeTerminalKind::Aborted).unwrap();
        }
        stop => {
            value["born_generation"] = 4.into();
            let delivered = matches!(stop, Stop::AfterNoticeAck);
            value["herdr_before_start_stop"] = serde_json::json!({
                "protocol": 1,
                "kind": "unsubmitted_before_start_stop",
                "turn_nonce": nonce,
                "admitted_generation": 5,
                "basis": "no_attempt",
                "notice": if delivered { "delivered" } else { "pending" },
            });
            if delivered {
                value["stop_notice_message_id"] = NOTICE.into();
            }
        }
    }
    let path = inflight::inflight_state_path(
        &inflight::inflight_runtime_root().unwrap(),
        &provider,
        CHANNEL,
    );
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
}

/// Runs one restart through boot confirmation and restore; returns the stop replies' targets,
/// whether the row is gone, and whether turn mode is confirmed afterwards.
async fn restart(case: &Case) -> (Vec<Option<String>>, bool, bool) {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let (_boot, tui_o) = boot();
    saved_row(
        case,
        &format!("turn-mode-restart-{:?}-{}", case.stop, case.anchor),
    );
    let mut shared = crate::services::discord::make_shared_data_for_tests();
    Arc::get_mut(&mut shared)
        .unwrap()
        .restart
        .current_generation = CURRENT;
    let provider = ProviderKind::Codex;
    let confirmed =
        crate::services::discord::tui_direct_pending_start::turn_retirement::confirm_at_boot(
            &provider,
            Some(&tui_o),
        );
    assert!(
        confirmed.is_empty(),
        "{:?}: the saved row holds boot",
        case.stop
    );
    assert!(!transcript_turns(CHANNEL));
    let discord = super::o_cut_recorder::start(CHANNEL).await;
    super::restore_inflight_turns(&discord.http, &shared, &provider).await;
    let calls = discord.calls();
    for call in &calls {
        assert!(call.route.starts_with("POST "), "{:?}: {call:?}", case.stop);
        assert_eq!(
            call.content.as_deref(),
            Some(super::herdr_admitted_restart::ADMITTED_ABORT_NOTICE)
        );
    }
    let gone = inflight::load_inflight_state(&provider, CHANNEL).is_none();
    let confirmed = transcript_turns(CHANNEL);
    if confirmed {
        assert!(
            crate::services::tui_o::cutover::codex_turn_awaited(CHANNEL).is_none(),
            "a confirmed channel no longer holds its input for turn mode"
        );
        drop(TestConfirmation::confirmed(CHANNEL));
    }
    (
        calls.into_iter().map(|call| call.reply_to).collect(),
        gone,
        confirmed,
    )
}

/// After a receipt, after the notice ack, or at a native Aborted, the anchorless turn-mode row
/// settles on restart and turn mode is confirmed again; a real anchor settles as before, and a
/// row without the saved fact keeps the Legacy hold and boot keeps waiting.
#[tokio::test(flavor = "current_thread")]
async fn a_codex_anchorless_stop_settles_across_restart_and_turn_mode_returns() {
    let cases = [
        Case {
            stop: Stop::AfterReceipt,
            anchor: 0,
            turn_mode: true,
            replies: 1,
            settles: true,
        },
        Case {
            stop: Stop::AfterNoticeAck,
            anchor: 0,
            turn_mode: true,
            replies: 0,
            settles: true,
        },
        Case {
            stop: Stop::NativeAborted,
            anchor: 0,
            turn_mode: true,
            replies: 1,
            settles: true,
        },
        Case {
            stop: Stop::AfterReceipt,
            anchor: ANCHOR,
            turn_mode: false,
            replies: 1,
            settles: true,
        },
        Case {
            stop: Stop::AfterReceipt,
            anchor: 0,
            turn_mode: false,
            replies: 0,
            settles: false,
        },
        Case {
            stop: Stop::NativeAborted,
            anchor: 0,
            turn_mode: false,
            replies: 0,
            settles: false,
        },
    ];
    for case in cases {
        let label = format!(
            "{:?} anchor={} turn_mode={}",
            case.stop, case.anchor, case.turn_mode
        );
        let (replies, gone, confirmed) = restart(&case).await;
        assert_eq!(replies.len(), case.replies, "{label}: {replies:?}");
        let target = if case.anchor == 0 { USER } else { ANCHOR };
        assert!(
            replies
                .iter()
                .all(|to| to.as_deref() == Some(target.to_string().as_str())),
            "{label}: {replies:?}"
        );
        assert_eq!(gone, case.settles, "{label}: the row");
        assert_eq!(confirmed, case.settles, "{label}: turn mode");
    }
}
