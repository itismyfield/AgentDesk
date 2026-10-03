use super::*;
use crate::services::discord::recovery_engine::{
    ManualRebindOverrides, RebindError, rebind_inflight_for_channel,
};
use crate::services::tui_o::turn_mode::TestConfirmation;
use crate::services::tui_prompt_dedupe::ObservedTuiPrompt;

fn run(body: impl std::future::Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(body);
}

#[test]
fn n1a_confirmed_direct_prompt_posts_only_its_notice() {
    run(async {
        let h = RelayE2eHarness::start_with_health_registry().await;
        h.cache_relay_transport();
        h.answer_placeholders_immediately();
        h.use_mock_notify_bot(Duration::from_secs(5)).await;
        let tmux = "n1a-confirmed-direct-prompt";
        h.attach_tmux_watcher(tmux, "n1a-direct.jsonl");
        let _confirmed = TestConfirmation::new(CHANNEL_ID);
        let existing_request = 6_325_901;
        h.shared.ui.placeholder_live_events.set_turn_request_anchor(
            h.channel_id,
            Some(existing_request),
            None,
        );
        let prompt = ObservedTuiPrompt {
            provider: PROVIDER_KEY.into(),
            tmux_session_name: tmux.into(),
            prompt: "N1a direct prompt remains visible".into(),
            source_event_id: None,
            observed_at: chrono::Utc::now(),
            external_input_lease_generation:
                dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
            ssh_direct_observation_generation: dedupe::SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
            hook_prompt_id: None,
        };
        super::super::relay_observed_prompt(&h.shared, prompt).await;

        assert!(
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL_ID).is_none(),
            "confirmed direct prompt must not create an inflight row"
        );
        assert_eq!(h.local_note_posts(), 1);
        assert_eq!(h.placeholder_posts(), 0);
        assert_eq!(h.messages().len(), 1);
        assert!(
            h.messages()[0]
                .1
                .contains("N1a direct prompt remains visible")
        );
        assert!(h.prompt_anchor(tmux).is_none());
        assert!(!h.relay_lease_present(tmux));
        assert!(h.mailbox().await.cancel_token.is_none());
        assert!(crate::services::discord::tui_direct_pending_start::load_all().is_empty());
        assert_eq!(
            h.shared
                .ui
                .placeholder_live_events
                .request_user_msg_id_for_test(h.channel_id),
            Some(existing_request),
            "notification-only observation must preserve the existing Discord request anchor"
        );
        assert!(h.unhandled_requests().is_empty());
    });
}

fn install_fake_tmux(root: &std::path::Path) -> crate::config::TestEnvVarGuard {
    use std::os::unix::fs::PermissionsExt;
    assert!(
        crate::config::shared_test_env_lock().try_lock().is_err(),
        "fixture must hold shared env lock before PATH mutation"
    );
    let path = root.join("tmux");
    let calls = root.join("n1a-tmux-calls");
    let script = format!(
        "#!/bin/sh\n[ \"$1\" = -u ] && shift\nprintf '%s\\n' \"$*\" >> '{}'\n\
         case \"$1\" in has-session) exit 0;; list-panes) echo 0; exit 0;; esac\n\
         exit 97\n",
        calls.display()
    );
    std::fs::write(&path, script).expect("fake tmux");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
        .expect("executable fake tmux");
    crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock("PATH", root)
}

#[test]
fn n1a_confirmed_claude_idle_binding_does_not_resume_its_synthetic_row() {
    run(async {
        use crate::services::agent_protocol::RuntimeHandoffKind;
        use crate::services::discord::inflight::RelayOwnerKind;
        use crate::services::tui_prompt_dedupe::{ExternalInputRelayLease, TuiRuntimeBinding};

        let h = RelayE2eHarness::start().await;
        h.cache_relay_transport();
        let (http_calls, _http) =
            crate::services::discord::shared_state::test_rest::recording_mock(
                6_325_990, CHANNEL_ID,
            )
            .await;
        let tmux = "n1a-confirmed-idle-synthetic";
        let output = h.attach_tmux_watcher(tmux, "n1a-idle.jsonl");
        // Preserve channel ownership while making this abandoned watcher unable to own delivery.
        h.shared
            .tmux_watchers
            .get(&h.channel_id)
            .unwrap()
            .cancel
            .store(true, Ordering::Release);
        let binding = TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: output.to_str().unwrap().into(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        };
        dedupe::register_tmux_runtime_binding(tmux, binding.clone());
        let mut lease = ExternalInputRelayLease::unassigned(Some(CHANNEL_ID));
        lease.turn_id = Some("n1a-idle-unpublished-episode".into());
        lease.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        let mut row = super::super::synthetic_start::build_tui_direct_synthetic_inflight_state(
            ProviderKind::Claude,
            h.channel_id,
            serenity::MessageId::new(6_325_902),
            None,
            "unpublished direct prompt",
            tmux,
            Some(&output),
            0,
            &lease,
            RelayOwnerKind::None,
        );
        row.turn_start_offset = Some(0);
        row.turn_nonce = Some("n1a-idle-unpublished-nonce".into());
        inflight::save_inflight_state(&row).expect("seed resumable synthetic row");
        let root = inflight::inflight_runtime_root().unwrap();
        let row_path = inflight::inflight_state_path(&root, &ProviderKind::Claude, CHANNEL_ID);
        let before = std::fs::read(&row_path).unwrap();
        let _confirmed = TestConfirmation::new(CHANNEL_ID);

        super::super::claude_idle_runtime::relay_idle_claude_bindings(&h.shared).await;

        assert!(
            !h.relay_lease_present(tmux),
            "idle binding must not resume the synthetic lease"
        );
        assert!(
            !super::super::CLAUDE_IDLE_RESPONSE_TAILS
                .lock()
                .unwrap()
                .contains(tmux),
            "idle binding must not spawn a synthetic response tail"
        );
        assert_eq!(
            std::fs::read(&row_path).unwrap(),
            before,
            "idle binding leaves row bytes untouched"
        );
        assert!(h.mailbox().await.cancel_token.is_none());
        assert_eq!(h.shared.restart.global_active.load(Ordering::Relaxed), 0);
        assert_eq!(
            dedupe::runtime_binding_for_tmux_session(tmux),
            Some(binding)
        );
        assert_eq!(h.local_note_posts(), 0);
        assert_eq!(h.placeholder_posts(), 0);
        assert!(h.messages().is_empty());
        assert!(
            http_calls.lock().unwrap().is_empty(),
            "idle binding must issue no HTTP request"
        );
        assert!(h.unhandled_requests().is_empty());
        dedupe::clear_tmux_runtime_binding(tmux);
    });
}

#[test]
fn n1a_manual_rebind_creates_rows_only_on_unconfirmed_channels() {
    run(async {
        for confirmed in [false, true] {
            let h = RelayE2eHarness::start().await;
            let _path = install_fake_tmux(h.root.path());
            let config_dir = h.root.path().join("claude");
            std::fs::create_dir_all(config_dir.join("projects")).expect("Claude project root");
            let _claude = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
                "CLAUDE_CONFIG_DIR",
                &config_dir,
            );
            let session = "63250000-0000-4000-8000-000000000091";
            let tmux = if confirmed {
                "n1a-rebind-confirmed"
            } else {
                "n1a-rebind-legacy"
            };
            let output = h
                .attach_tmux_watcher(tmux, &format!("claude/projects/{session}.jsonl"))
                .canonicalize()
                .expect("canonical transcript path");
            h.shared
                .tmux_watchers
                .insert(h.channel_id, watcher_handle(tmux, &output));
            let overrides = ManualRebindOverrides::validated(
                &ProviderKind::Claude,
                Some(output.to_str().expect("transcript path")),
                Some(session),
            )
            .expect("validated rebind coordinates");
            let _confirmation = confirmed.then(|| TestConfirmation::new(CHANNEL_ID));
            let result = rebind_inflight_for_channel(
                &h.ctx.http,
                &h.shared,
                &ProviderKind::Claude,
                CHANNEL_ID,
                Some(tmux.into()),
                overrides,
                None,
            )
            .await;
            let row = inflight::load_inflight_state_read_only(&ProviderKind::Claude, CHANNEL_ID);
            if confirmed {
                assert!(
                    row.is_none(),
                    "confirmed manual rebind must not create a synthetic row"
                );
                assert!(matches!(result, Err(RebindError::Internal(ref reason))
                    if reason.contains("transcript turn mode refuses synthetic rebind creation")));
            } else {
                let row = row.expect("unconfirmed rebind preserves synthetic row creation");
                assert!(row.rebind_origin);
                assert_eq!(row.user_msg_id, 0);
                let outcome = result.expect("unconfirmed rebind remains available");
                assert!(
                    !outcome.watcher_spawned,
                    "fixture reuses its incumbent watcher"
                );
            }
            assert!(h.unhandled_requests().is_empty());
            let calls = std::fs::read_to_string(h.root.path().join("n1a-tmux-calls"))
                .expect("fake tmux recorded liveness probes");
            assert!(calls.lines().any(|line| line.starts_with("has-session ")));
            assert!(calls.lines().any(|line| line.starts_with("list-panes ")));
            assert!(calls.lines().all(|line| line.starts_with("has-session ")
                || line.starts_with("list-panes ")));
        }
    });
}
