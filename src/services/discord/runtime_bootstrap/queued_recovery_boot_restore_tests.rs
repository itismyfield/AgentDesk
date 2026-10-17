//! Boot restore merges bound channels' persisted queues (with or without a dispatch marker),
//! reinstalls the role override, and judges the queued placeholder.

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
    // A queue persisted by Enqueue before any TakeNextSoft has no marker to absorb it.
    let queue_only = ChannelId::new(legacy + 50);
    crate::services::turn_orchestrator::save_channel_queue(
        &provider,
        &token,
        queue_only,
        &[item(legacy + 51)],
        None,
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
    assert_eq!(
        live.get(&queue_only).cloned().unwrap_or_default(),
        [legacy + 51].into(),
        "a queue without a dispatch marker is restored"
    );
    let overrides = &shared.dispatch.role_overrides;
    assert!(overrides.contains_key(&ChannelId::new(legacy)));
    assert!(
        stale_cards.iter().any(|card| card.0.get() == legacy),
        "the bound channel's placeholder is judged: {stale_cards:?}"
    );
}

/// A boot restore run by `restore_around`, with the root it ran under kept until the test reads it.
struct Restored {
    stale: Vec<(ChannelId, MessageId, MessageId)>,
    token: String,
    memory: Vec<u64>,
    _root: (crate::config::TestEnvVarGuard, tempfile::TempDir),
}

/// Runs the boot restore of a bound channel holding `[queued]` and a placeholder for it,
/// with `between` run after the boot read and before the merge.
fn restore_around<F: std::future::Future<Output = ()>>(
    channel: u64,
    queued: u64,
    between: impl FnOnce(Arc<SharedData>, std::path::PathBuf) -> F,
) -> Restored {
    let tmp = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    let provider = ProviderKind::Claude;
    bind_channels(tmp.path(), [channel, channel + 50]);
    let shared = make_shared_data_for_tests();
    let (id, token) = (ChannelId::new(channel), shared.token_hash.clone());
    let save = crate::services::turn_orchestrator::save_channel_queue;
    save(&provider, &token, id, &[item(queued)], None).unwrap();
    let card = (MessageId::new(queued), MessageId::new(queued + 1));
    super::super::queued_placeholders_store::save_channel_queued_placeholders(
        &provider,
        &token,
        id,
        &[card],
    );
    let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    let path = root
        .join(provider.as_str())
        .join(&token)
        .join(format!("{channel}.json"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (stale, memory) = runtime.block_on(async {
        // The restore reads the queue files, then waits on the settings this guard holds.
        let settings = shared.settings.write().await;
        let (http, task_shared) = (
            Arc::new(serenity::Http::new("Bot test-token")),
            shared.clone(),
        );
        let restore = tokio::spawn(async move {
            restore_queued_and_inflight_work(&http, &task_shared, &ProviderKind::Claude).await
        });
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        between(shared.clone(), path.clone()).await;
        drop(settings);
        let stale = restore.await.unwrap();
        let queue = shared.mailbox(id).snapshot().await.intervention_queue;
        (
            stale,
            queue.iter().map(|item| item.message_id.get()).collect(),
        )
    });
    let _root = (_env, tmp);
    Restored {
        stale,
        token,
        memory,
        _root,
    }
}

/// The message ids the channel's queue file holds.
fn disk_ids(token: &str, channel: u64) -> Vec<u64> {
    let mut queues =
        crate::services::turn_orchestrator::load_pending_queues(&ProviderKind::Claude, token).0;
    let queue = queues.remove(&ChannelId::new(channel)).unwrap_or_default();
    queue.iter().map(|item| item.message_id.get()).collect()
}

#[test]
fn a_queue_cleared_while_boot_restore_waits_is_not_restored() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (channel, queued) = (6_258_301_u64, 6_258_311_u64);
    let restored = restore_around(channel, queued, |shared, _| async move {
        let id = ChannelId::new(channel);
        let context = crate::services::discord::queue_persistence_context;
        let persistence = context(&shared, &ProviderKind::Claude, id);
        shared.mailbox(id).clear(persistence).await;
    });
    assert!(restored.memory.is_empty(), "Q is not merged back");
    assert_eq!(
        disk_ids(&restored.token, channel),
        Vec::<u64>::new(),
        "Q is not written back"
    );
    assert!(
        restored.stale.iter().any(|card| card.1.get() == queued),
        "the cleared item's card is stale: {:?}",
        restored.stale
    );
}

#[test]
fn a_queue_the_merge_cannot_read_keeps_its_file_and_placeholder() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (channel, queued) = (6_258_401_u64, 6_258_411_u64);
    let (id, provider) = (ChannelId::new(channel), ProviderKind::Claude);
    let load = crate::services::turn_orchestrator::load_channel_pending_queue_checked;
    let torn = br#"[{"message_id": 6258411, "te"#;
    let restored = restore_around(channel, queued, |shared, path| async move {
        // The boot read already holds Q; the file the merge reads is torn.
        std::fs::write(&path, torn).unwrap();
        let read = load(&ProviderKind::Claude, &shared.token_hash, id);
        assert!(read.is_err(), "the fixture makes the merge's read fail");
    });
    let token = restored.token.as_str();
    assert!(
        load(&provider, token, id).is_err(),
        "the file still cannot be read"
    );
    assert!(
        restored.memory.is_empty(),
        "Q is not merged: {:?}",
        restored.memory
    );
    let root = crate::services::discord::runtime_store::discord_pending_queue_root().unwrap();
    let path = root
        .join(provider.as_str())
        .join(token)
        .join(format!("{channel}.json"));
    assert_eq!(
        std::fs::read(path).unwrap(),
        torn,
        "the file's bytes are left as they were"
    );
    assert!(
        !restored.stale.iter().any(|card| card.1.get() == queued),
        "an unread queue's card is not deleted: {:?}",
        restored.stale
    );
    let cards = super::super::queued_placeholders_store::load_queued_placeholders(&provider, token);
    assert!(
        cards.contains_key(&(id, MessageId::new(queued))),
        "the placeholder row is kept: {cards:?}"
    );
}
