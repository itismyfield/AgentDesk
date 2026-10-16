//! Boot restore merges a bound channel's persisted queue and dispatch marker, reinstalls its
//! role override, and judges its queued placeholder.

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
        voice_announcement: None,
    }
}

fn bind_channels(root: &std::path::Path, channels: [u64; 2]) {
    let config = crate::runtime_layout::config_file_path(root);
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let agents: String = channels
        .iter()
        .map(|id| {
            format!(
                "  - id: fence-{id}\n    name: \"fence {id}\"\n    provider: claude\n    channels:\n      claude:\n        id: \"{id}\"\n        name: \"fence-{id}\"\n"
            )
        })
        .collect();
    std::fs::write(config, format!("server:\n  port: 8791\nagents:\n{agents}")).unwrap();
}

#[test]
fn boot_restore_installs_a_bound_channels_queue_marker_override_and_placeholder() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    let provider = ProviderKind::Claude;
    let legacy = 6_325_581_u64;
    bind_channels(tmp.path(), [legacy, legacy + 50]);
    let shared = make_shared_data_for_tests();
    let token = shared.token_hash.clone();
    let (id, alt) = (ChannelId::new(legacy), Some(legacy + 100));
    crate::services::turn_orchestrator::save_channel_queue(
        &provider,
        &token,
        id,
        &[item(legacy + 1)],
        alt,
    )
    .unwrap();
    crate::services::turn_orchestrator::save_channel_pending_dispatch_marker(
        &provider,
        &token,
        id,
        &item(legacy + 2),
        alt,
    )
    .unwrap();
    super::super::queued_placeholders_store::save_channel_queued_placeholders(
        &provider,
        &token,
        id,
        &[(MessageId::new(legacy + 3), MessageId::new(legacy + 4))],
    );
    let http = Arc::new(serenity::Http::new("Bot test-token"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (stale_cards, live) = runtime.block_on(async {
        let cards = restore_queued_and_inflight_work(&http, &shared, &provider).await;
        (
            cards,
            super::super::queued_placeholders::collect_live_queue_message_ids(&shared).await,
        )
    });

    let legacy_live = live
        .get(&ChannelId::new(legacy))
        .cloned()
        .unwrap_or_default();
    assert_eq!(
        legacy_live,
        [legacy + 1, legacy + 2].into(),
        "legacy queue and marker restored"
    );
    let overrides = &shared.dispatch.role_overrides;
    assert!(overrides.contains_key(&ChannelId::new(legacy)));
    assert!(
        stale_cards.iter().any(|card| card.0.get() == legacy),
        "the bound channel's placeholder is judged: {stale_cards:?}"
    );
}
