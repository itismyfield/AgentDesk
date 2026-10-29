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

/// Appends one ledger line for `channel` under the test runtime root, as the writer would.
fn ledger_line(channel: u64, entry: crate::services::tui_o::store::ledger::LedgerEntry) {
    use std::io::Write;
    let dir = crate::config::runtime_root()
        .unwrap()
        .join("o_store")
        .join(channel.to_string());
    std::fs::create_dir_all(&dir).unwrap();
    let mut line =
        serde_json::to_vec(&serde_json::json!({"at": chrono::Utc::now(), "entry": entry})).unwrap();
    line.push(b'\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("ledger.jsonl"))
        .unwrap();
    file.write_all(&line).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn a_confirmed_claude_terminal_input_leaves_its_notice_to_the_writer_once_its_cohort_is_frozen()
 {
    use crate::services::discord::admin_host_guard::tests::Recorder;
    use crate::services::tui_o::store::ledger::{LedgerEntry, input_notices};
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
    let human = "terminal input the writer owns";
    let post = |raw: &'static str| {
        let (shared, api) = (shared.clone(), &api);
        async move {
            let prompt = observed(&format!("writer-notice-{}", uuid::Uuid::new_v4()), raw);
            let decision = relay_observed_prompt_injected_prompt_decision(raw);
            let observation = task_notification_prompt::observe(
                &shared,
                &prompt,
                channel,
                decision.injected_class,
            );
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
            api.take().len()
        }
    };
    // Until the writer's frozen cohort is durable, the live owner still announces the input.
    assert_eq!(post(human).await, 1);
    ledger_line(
        channel.get(),
        LedgerEntry::Excluded {
            unit_key: crate::services::tui_o::shadow::UnitKey {
                channel_id: channel.get(),
                provider: crate::services::tui_o::shadow::ShadowProvider::Claude,
                native_key: "migration_started".into(),
                kind: crate::services::tui_o::shadow::UnitKind::InputNotice,
            },
            reason: "migration_started".into(),
        },
    );
    assert_eq!(
        post(human).await,
        1,
        "only a real migration entry hands over"
    );
    ledger_line(
        channel.get(),
        LedgerEntry::InputNotice {
            entry: Box::new(input_notices::NoticeEntry::MigrationStarted {
                snapshot: input_notices::MigrationSnapshot {
                    sources: Vec::new(),
                    binding_checkpoint: 0,
                    history: input_notices::MigrationHistory::Fixed { lower: 1, upper: 1 },
                },
            }),
        },
    );
    assert_eq!(post(human).await, 0);
    // The writer owns only the human input class; a slash control keeps its live note.
    assert_eq!(post("/loop inspect relay").await, 1);
}
