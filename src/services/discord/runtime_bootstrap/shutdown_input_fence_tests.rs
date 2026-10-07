use super::*;
use crate::services::discord::input_runtime::{self, fence::Gate};
use crate::services::turn_orchestrator::{Intervention, InterventionMode, QueuePersistenceContext};

#[cfg(unix)]
#[tokio::test]
async fn c2_sigterm_handler_dispatch_persists_before_consuming_shutdown_slot() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Codex;
    let channel = ChannelId::new(6_325_607);
    shared.last_message_ids.insert(channel, 99);
    // One remaining slot keeps this real handler path away from process::exit.
    shared
        .restart
        .shutdown_remaining
        .store(2, std::sync::atomic::Ordering::SeqCst);
    let (send, receive) = tokio::sync::oneshot::channel();
    SIGTERM_FOR_TEST.with(|slot| *slot.borrow_mut() = Some(receive));
    let handler = run_bot_spawn_sigterm_handler(&shared, provider.clone());
    send.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), handler)
        .await
        .expect("handler completes")
        .expect("handler join");
    let checkpoint = runtime_store::last_message_root()
        .unwrap()
        .join("codex")
        .join(format!("{}.txt", channel.get()));
    assert_eq!(
        std::fs::read_to_string(&checkpoint)
            .ok()
            .map(|value| value.trim().to_owned()),
        Some("99".into()),
        "actual handler omitted persistence"
    );
    assert!(
        shared
            .restart
            .shutdown_counted
            .load(std::sync::atomic::Ordering::SeqCst)
    );
    assert_eq!(
        shared
            .restart
            .shutdown_remaining
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
}

fn item(id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: 1,
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "shutdown input".into(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

#[tokio::test]
async fn c2_sigterm_initial_and_final_snapshots_preserve_held_population_and_save_legacy() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        temp.path(),
    );
    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Codex;
    let (held, legacy) = (ChannelId::new(6_325_590), ChannelId::new(6_325_591));
    let root = inflight::inflight_runtime_root().unwrap();
    for channel in [held, legacy] {
        shared
            .mailboxes
            .handle(channel)
            .replace_queue(
                vec![item(channel.get() + 10)],
                QueuePersistenceContext::new(&provider, &shared.token_hash, None),
            )
            .await;
        let row = InflightTurnState::new(
            provider.clone(),
            channel.get(),
            Some("shutdown-test".into()),
            7,
            channel.get() + 10,
            channel.get() + 20,
            "input".into(),
            None,
            None,
            None,
            None,
            0,
        );
        inflight::save_inflight_state_create_new(&row).unwrap();
        shared.last_message_ids.insert(channel, channel.get() + 30);
    }
    let queue_path = |channel: ChannelId| {
        input_runtime::fence::population_root()
            .unwrap()
            .join(format!(
                "discord_pending_queue/codex/{}/{}.json",
                shared.token_hash,
                channel.get()
            ))
    };
    let held_queue = std::fs::read(queue_path(held)).unwrap();
    let held_row_path = inflight::inflight_state_path(&root, &provider, held.get());
    let mut old: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&held_row_path).unwrap()).unwrap();
    old.as_object_mut().unwrap().remove("finalizer_turn_id");
    std::fs::write(&held_row_path, serde_json::to_vec_pretty(&old).unwrap()).unwrap();
    let held_row = std::fs::read(&held_row_path).unwrap();
    let legacy_row_path = inflight::inflight_state_path(&root, &provider, legacy.get());
    let checkpoint = runtime_store::last_message_root()
        .unwrap()
        .join("codex")
        .join(format!("{}.txt", legacy.get()));
    std::fs::remove_file(queue_path(legacy)).unwrap();
    let gate = Gate::protect(provider.clone(), held.get()).unwrap();
    let _health = input_runtime::fence::test_health::Clear::new(&gate);
    let _closing = gate.close().unwrap();

    let drains = persist_sigterm_state_with_boundaries(
        &shared,
        &provider,
        || {
            assert!(
                queue_path(legacy).exists(),
                "initial queue snapshot missing"
            );
            assert_eq!(
                std::fs::read_to_string(&checkpoint).unwrap().trim(),
                (legacy.get() + 30).to_string()
            );
            let row: InflightTurnState =
                serde_json::from_slice(&std::fs::read(&legacy_row_path).unwrap()).unwrap();
            assert_eq!(
                row.restart_mode, None,
                "marking ran before initial snapshot"
            );
            std::fs::remove_file(queue_path(legacy)).unwrap();
            shared.last_message_ids.insert(legacy, legacy.get() + 31);
        },
        || {
            let row: InflightTurnState =
                serde_json::from_slice(&std::fs::read(&legacy_row_path).unwrap()).unwrap();
            assert_eq!(row.restart_mode, Some(InflightRestartMode::DrainRestart));
            assert!(
                !queue_path(legacy).exists(),
                "final snapshot ran before marking"
            );
        },
    )
    .await;

    for drain in drains {
        assert!(
            drain.persistence_errors.is_empty(),
            "{:?}",
            drain.persistence_errors
        );
        assert_eq!(drain.queued_count, 1);
    }
    assert!(queue_path(legacy).exists(), "final queue snapshot missing");
    assert_eq!(
        std::fs::read_to_string(checkpoint).unwrap().trim(),
        (legacy.get() + 31).to_string()
    );
    assert_eq!(std::fs::read(queue_path(held)).unwrap(), held_queue);
    assert_eq!(std::fs::read(held_row_path).unwrap(), held_row);
    assert!(
        !input_runtime::health_reasons()
            .iter()
            .any(|reason| reason.contains(&format!("channel={}", held.get())))
    );
}
