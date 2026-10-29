use super::*;

fn observed(tmux: &str, body: &str) -> ObservedTuiPrompt {
    ObservedTuiPrompt {
        hosted_source: None,
        provider: "claude".to_string(),
        tmux_session_name: tmux.to_string(),
        prompt: body.to_string(),
        source_event_id: Some(format!("{tmux}-row")),
        observed_at: chrono::Utc::now(),
        external_input_lease_generation:
            crate::services::tui_prompt_dedupe::EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
        ssh_direct_observation_generation:
            crate::services::tui_prompt_dedupe::SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
        hook_prompt_id: None,
        row_prompt_id: None,
        native_turn_id: None,
        steer_echo: false,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_confirmed_claude_terminal_input_leaves_its_notice_to_the_writer() {
    use crate::services::discord::admin_host_guard::tests::Recorder;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let api = Recorder::start().await;
    let registry = Arc::new(super::super::health::HealthRegistry::new());
    let mut shared = super::super::make_shared_data_for_tests_with_storage(None);
    Arc::get_mut(&mut shared).unwrap().health_registry = Arc::downgrade(&registry);
    let channel = ChannelId::new(74_190_290);
    shared.settings.write().await.allowed_channel_ids = vec![channel.get()];
    registry.register("claude".into(), shared.clone()).await;
    let _http = super::super::shared_state::test_rest::install(api.http.clone());
    let _confirmed = crate::services::tui_o::turn_mode::TestConfirmation::new(channel.get());
    // The writer owns only the human input class; a slash control keeps its live note.
    for (raw, human, posts) in [
        ("terminal input the writer owns", true, 0),
        ("/loop inspect relay", false, 1),
    ] {
        let prompt = observed(&format!("writer-notice-{}", uuid::Uuid::new_v4()), raw);
        let decision = relay_observed_prompt_injected_prompt_decision(raw);
        assert_eq!(decision.injected_class.is_human_active_turn(), human);
        let observation =
            task_notification_prompt::observe(&shared, &prompt, channel, decision.injected_class);
        let returned = tokio::time::timeout(
            Duration::from_secs(5),
            synthetic_start_wiring::post_passive_prompt_notice(
                &shared,
                &prompt,
                channel,
                &decision,
                observation,
            ),
        )
        .await
        .expect("passive notice path finishes");
        assert!(returned.is_none());
        assert_eq!(api.take().len(), posts, "{raw}");
    }
}
