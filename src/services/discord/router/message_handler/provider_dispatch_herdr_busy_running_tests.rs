//! The managed running-turn entry follows the verified source after its native session changes.
use super::*;
use crate::services::claude_tui::host_input::busy::BusyState;
use crate::services::discord::router::message_handler::provider_dispatch as dispatch;
use crate::services::tui_prompt_dedupe::{self, binding_events};

#[test]
fn active_herdr_busy_input_rebinds_the_source_after_session_replacement_pg() {
    for case in ["missing", "old", "foreign", "unknown", "prompt"] {
        let fx = Fixture::with_root(
            &format!("busy-active-rebind-{case}"),
            None,
            crate::config::TestRuntimeRootGuard::new(),
        );
        let launcher = Arc::new(Launcher::default());
        let (_, started) = reconnect::launch(&fx, &launcher);
        let before = fx.rig.sends().len();
        let _registry = fx.rig.registry_on_this_thread();
        let _admission = open_admission();
        let intake = Intake::new(&fx, &started);
        let native = uuid();
        let path = started.path.with_file_name(format!("{native}.jsonl"));
        let working = json!({"type":"assistant","sessionId":native,"uuid":uuid(),
            "message":{"role":"assistant","content":[{"type":"text","text":"working"}]}});
        append(&path, std::slice::from_ref(&working));
        assert_eq!(
            started.hook(
                &fx,
                "PreToolUse",
                &json!({"session_id":native,"transcript_path":path})
            ),
            202
        );
        let binding = tui_prompt_dedupe::runtime_binding_for_tmux_session(fx.logical()).unwrap();
        assert_eq!(binding.session_id.as_deref(), Some(native.as_str()));
        let pin = binding_events::pinned_source(CHANNEL, fx.logical())
            .unwrap()
            .unwrap();
        assert_eq!(pin.path, path);
        assert_eq!(pin.session_id, native);
        if case == "foreign" {
            std::fs::write(
                &path,
                format!(
                    "{}\n",
                    json!({"type":"assistant", "sessionId":uuid(),
                "message":{"role":"assistant","content":[]}})
                ),
            )
            .unwrap();
        } else if case == "unknown" {
            append(
                &path,
                &[json!({"type":"permission-mode","sessionId":native})],
            );
        } else if case == "prompt" {
            // What Claude writes after a submitted prompt, before the first reply record.
            let records = [
                "user",
                "file-history-snapshot",
                "last-prompt",
                "permission-mode",
            ]
            .map(|kind| json!({"type":kind,"sessionId":native,"message":{"content":"next"}}));
            append(&path, &records);
        }
        let heads = Arc::new(Mutex::new(Vec::<Value>::new()));
        let (seen, mailbox, runtime) = (
            heads.clone(),
            intake.mailbox.clone(),
            intake.runtime.handle().clone(),
        );
        fx.rig.on_send(move |_| {
            let snapshot = runtime
                .block_on(mailbox.v2_request(|slot| slot.as_ref().unwrap().json()))
                .unwrap();
            seen.lock()
                .unwrap()
                .push(snapshot["head_injection"].clone());
        });
        intake.runtime.block_on(async {
            intake.install_holder(&fx, &started).await;
            if case == "missing" {
                let mut row =
                    inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL)
                        .unwrap();
                row.output_path = None;
                row.session_id = None;
                inflight::save_inflight_state(&row).unwrap();
            }
            let front = intake.front().await;
            let host = crate::services::turn_host::for_turn(
                Some(&fx.pool),
                &ProviderKind::Claude,
                CHANNEL,
                inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL)
                    .unwrap()
                    .session_key
                    .as_deref(),
            )
            .await;
            let expected = if matches!(case, "foreign" | "unknown") {
                BusyState::Held
            } else {
                BusyState::Busy
            };
            assert!(
                host::channel_accepts(CHANNEL),
                "{case}: source checks run under a ready O writer"
            );
            assert_eq!(
                claude_handoff::observe_busy(
                    CHANNEL,
                    fx.logical(),
                    &host,
                    &intake.shared.token_hash
                )
                .state(),
                expected
            );
            let result = crate::test_support::wait::await_within(
                dispatch::hand_busy(
                    (intake.mailbox.clone(), Some(fx.pool.clone())),
                    ProviderKind::Claude,
                    (CHANNEL, front),
                    Some(intake.shared.clone()),
                ),
                LIMIT,
            )
            .await;
            let sends = fx.rig.sends();
            if expected == BusyState::Held {
                assert!(
                    result.is_err_and(|error| error.starts_with("NotSent(")),
                    "{case}"
                );
                assert_eq!(
                    sends.len(),
                    before,
                    "{case}: an unverified source cannot key the pane"
                );
            } else {
                assert_eq!(
                    result,
                    Ok(()),
                    "{case}: verified busy input must not wait for turn end"
                );
                assert_eq!(sends.len() - before, 2, "{case}: {sends:?}");
                assert!(sends[before]["text"].as_str().unwrap().contains(INPUT));
                assert_eq!(sends[before + 1]["keys"], json!(["enter"]));
                assert_eq!(heads.lock().unwrap().len(), 2);
                for head in heads.lock().unwrap().iter() {
                    assert_eq!(head["attempt"]["source"]["session_id"], native);
                    assert_eq!(
                        head["attempt"]["source"]["transcript"],
                        path.to_str().unwrap()
                    );
                    assert_eq!(head["attempt"]["source"]["file"], json!([pin.dev, pin.ino]));
                }
                assert!(
                    crate::services::tui_turn_state::observe_claude_jsonl_running_turn_state(&path)
                        .is_busy()
                );
            }
            assert_eq!(intake.mailbox.has_active_turn().await, Ok(true));
        });
    }
}
