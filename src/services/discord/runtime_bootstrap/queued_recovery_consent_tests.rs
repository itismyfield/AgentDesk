//! The actual queue recovery junction keeps Legacy queues independent of consent storage.
use super::*;
use crate::services::discord::make_shared_data_for_tests;
use crate::services::turn_orchestrator::{Intervention, InterventionMode};

fn item(id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(7),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: 1,
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("input {id}"),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
    }
}

#[tokio::test(flavor = "current_thread")]
async fn queued_recovery_legacy_preserves_fifo_and_never_reads_consent() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let channel = 891_601;
    let _boot = crate::config::session_hosts::force_for_test(Some("node"), &[(channel, "node")]);
    let root = crate::config::runtime_root().unwrap();
    let config = crate::runtime_layout::config_file_path(&root);
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    std::fs::write(config, format!("server:\n  port: 8791\nagents:\n  - id: consent-legacy\n    name: consent-legacy\n    provider: claude\n    channels:\n      claude:\n        id: \"{channel}\"\n        name: consent-legacy\n")).unwrap();
    let directory = root.join("runtime/herdr_policy/consents");
    std::fs::create_dir_all(&directory).unwrap();
    let poison = directory.join(format!("{}.json", "a".repeat(64)));
    let bytes = b"{malformed preexisting consent snapshot}";
    std::fs::write(&poison, bytes).unwrap();
    let provider = ProviderKind::Claude;
    let shared = make_shared_data_for_tests();
    let before = crate::services::discord::router::herdr_consent::startup::store_reads_for_test();
    let items = [item(channel + 1), item(channel + 2)];
    crate::services::turn_orchestrator::save_channel_queue(
        &provider,
        &shared.token_hash,
        ChannelId::new(channel),
        &items,
        None,
    )
    .unwrap();
    let http = Arc::new(serenity::Http::new("Bot test-token"));
    let result = restore_queued_and_inflight_work(&http, &shared, &provider).await;
    assert!(
        result.is_empty(),
        "legacy_startup_junction_keeps_stale_card_result_contract"
    );
    let live = shared
        .mailbox(ChannelId::new(channel))
        .try_snapshot()
        .await
        .unwrap();
    assert!(
        live.intervention_queue
            .iter()
            .map(|item| item.message_id.get())
            .eq([channel + 1, channel + 2]),
        "legacy_startup_junction_original_fifo_contract"
    );
    assert_eq!(
        crate::services::discord::router::herdr_consent::startup::store_reads_for_test(),
        before,
        "legacy_startup_junction_consent_store_io_zero_contract"
    );
    assert_eq!(std::fs::read(&poison).unwrap(), bytes);
    assert_eq!(
        std::fs::read_dir(&directory).unwrap().count(),
        1,
        "legacy_startup_junction_cards_holds_sidecars_zero_contract"
    );
}
