//! A live Codex turn whose provider died, ended by the row pass while its executor still reads.
use super::super::herdr_tests::live_end::LiveEnd;
use super::*;
use crate::services::session_host::herdr_socket_rig_tests::SHELL;

/// A pane back at its shell while the provider lives ends neither the read nor the turn; once the
/// provider is gone the pass settles it once, wakes the reader and lets the queued input start.
#[test]
fn a_live_codex_turn_ends_once_only_after_its_provider_is_gone_pg() {
    let fx = Fixture::admitted("liveend");
    let launcher = Arc::new(Launcher::default());
    let ports = fx.ports(&launcher);
    let url = fx.db.as_ref().unwrap().database_url.clone();
    let mut seen = None;
    let (result, messages) = fx.turn(&HostedRecord::Legacy, &ports, || {
        let Some(nonce) = fx.start_provider(&launcher, true) else {
            return;
        };
        if !wait_for(&fx.finished, "the prompt", || fx.rig.sends().len() == 2) {
            return;
        }
        let session = uuid::Uuid::new_v4().to_string();
        let path = fx.rollout(&session);
        assert_eq!(fx.session_start(&context_of(&nonce), &session, &path), 202);
        append(&path, &started_lines("t1", "부분"));
        let complete = turn_lines("t1", "부분").pop().unwrap().to_string();
        write_raw(&path, &complete[..complete.len() / 2]);
        let token = fx.cancel.lock().unwrap().clone();
        let state = token
            .herdr_interrupt_state()
            .expect("the turn's Herdr state");
        if !wait_for(&fx.finished, "the turn start", || {
            state.turn_start.get().is_some()
        }) {
            return;
        }
        let _switch = crate::services::turn_host::force_codex_switch_for_test(Some(true));
        let _registry = fx.rig.registry_on_this_thread();
        let live = LiveEnd::start(
            &url,
            TOKEN,
            (ProviderKind::Codex, CHANNEL, fx.logical()),
            &token,
        );
        live.route();
        fx.rig.show_panes(&[PANE]);
        fx.rig.foreground(&[SHELL]);
        fx.rig.provider_seen(&context_of(&nonce), true);
        live.pass(Duration::from_secs(3));
        std::thread::sleep(Duration::from_secs(2));
        let alive = (
            fx.finished.load(Ordering::SeqCst),
            live.texts(),
            live.row().is_some(),
        );
        assert_eq!(
            alive,
            (false, Vec::new(), true),
            "a live provider keeps its turn"
        );
        assert_eq!(fx.row(), Some(HostedState::Bound));
        fx.rig.provider_seen(&context_of(&nonce), false);
        live.pass(Duration::from_secs(15));
        let ended = (
            live.texts(),
            live.row().is_none(),
            live.rt.block_on(live.held()),
        );
        seen = Some((ended, token.is_completion_cleanup(), live.wait_taken()));
    });
    let (ended, woken, taken) = seen.expect("the turn reached its start");
    assert_eq!(ended, (vec![LiveEnd::notice()], true, false));
    assert!(woken, "the settled turn's reader was woken");
    assert_eq!(
        taken,
        vec![false],
        "the queued input started once, after the row"
    );
    assert_eq!(result, Ok(()), "the woken reader joined");
    assert!(
        !messages
            .iter()
            .any(|m| matches!(m, StreamMessage::RuntimeReady { .. })),
        "{messages:?}"
    );
    assert_eq!(
        fx.rig.sends(),
        prompt_sends(),
        "no pane input after the prompt"
    );
    assert_eq!(fx.row(), Some(HostedState::Retired));
}

/// A task_complete the dying provider wrote without its newline is the reader's: the pass leaves
/// it, the reader takes it at the file's end and its bridge settles the turn once.
#[test]
fn a_codex_terminal_left_without_its_newline_is_settled_once_by_its_own_reader_pg() {
    let fx = Fixture::admitted("liveraw");
    let launcher = Arc::new(Launcher::default());
    let ports = fx.ports(&launcher);
    let url = fx.db.as_ref().unwrap().database_url.clone();
    let mut seen = None;
    let (result, messages) = fx.turn(&HostedRecord::Legacy, &ports, || {
        let Some(nonce) = fx.start_provider(&launcher, true) else {
            return;
        };
        if !wait_for(&fx.finished, "the prompt", || fx.rig.sends().len() == 2) {
            return;
        }
        let session = uuid::Uuid::new_v4().to_string();
        let path = fx.rollout(&session);
        assert_eq!(fx.session_start(&context_of(&nonce), &session, &path), 202);
        let mut lines = turn_lines("t1", "답");
        let complete = lines.pop().unwrap().to_string();
        append(&path, &lines);
        let token = fx.cancel.lock().unwrap().clone();
        let state = token
            .herdr_interrupt_state()
            .expect("the turn's Herdr state");
        if !wait_for(&fx.finished, "the turn start", || {
            state.turn_start.get().is_some()
        }) {
            return;
        }
        write_raw(&path, &complete);
        let _switch = crate::services::turn_host::force_codex_switch_for_test(Some(true));
        let _registry = fx.rig.registry_on_this_thread();
        let live = LiveEnd::start(
            &url,
            TOKEN,
            (ProviderKind::Codex, CHANNEL, fx.logical()),
            &token,
        );
        live.route();
        fx.rig.show_panes(&[PANE]);
        fx.rig.foreground(&[SHELL]);
        fx.rig.provider_seen(&context_of(&nonce), false);
        live.pass(Duration::from_secs(3));
        let left = (live.texts(), live.row().is_some(), fx.row());
        seen = Some((
            left,
            std::fs::metadata(&path).unwrap().len(),
            token.clone(),
            live,
        ));
    });
    let (left, len, token, live) = seen.expect("the turn reached its start");
    assert_eq!(left, (Vec::new(), true, Some(HostedState::Bound)));
    assert_eq!(result, Ok(()), "the reader joined");
    let ends: Vec<u64> = messages
        .iter()
        .filter_map(|m| match m {
            StreamMessage::CodexTuiTerminalDone {
                complete_record_end,
                ..
            } => Some(*complete_record_end),
            _ => None,
        })
        .collect();
    assert_eq!(ends, [len], "one terminal, taken at the file's end");
    let _registry = fx.rig.registry_on_this_thread();
    let _switch = crate::services::turn_host::force_codex_switch_for_test(Some(true));
    let finalized = crate::services::discord::turn_bridge::BridgeCompletionSignal::Finalized;
    assert_eq!(
        live.bridge(&token, messages),
        Some(finalized),
        "settled once"
    );
    let settled = (live.row().is_none(), live.rt.block_on(live.held()));
    assert_eq!(
        settled,
        (true, false),
        "the bridge freed the row and the channel"
    );
    assert_eq!(live.wait_taken().len(), 1, "the queued input started once");
    let texts = live.texts();
    let delivered = texts.iter().any(|text| text.contains("답"));
    assert!(
        delivered && !texts.contains(&LiveEnd::notice()),
        "{texts:?}"
    );
}
