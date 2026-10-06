//! Boot restore leaves an input-protected channel's queue, markers, placeholders and rows on disk
//! for its move or handback, and restores an unprotected channel as before.

use super::*;
use crate::services::discord::input_runtime::{self, fence::Gate};
use crate::services::discord::{inflight, make_shared_data_for_tests};
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

/// Every non-lock file under `root` named for `channel`, with its bytes.
fn files_mentioning(root: &std::path::Path, channel: u64) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.file_name().is_some_and(|name| {
                let name = name.to_string_lossy();
                name.contains(&channel.to_string()) && !name.ends_with(".lock")
            }) {
                found.push((path.clone(), std::fs::read(&path).unwrap()));
            }
        }
    }
    found.sort();
    found
}

#[test]
fn c2_boot_restore_installs_nothing_for_an_input_protected_channel() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let tmp = tempfile::tempdir().unwrap();
    let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        tmp.path(),
    );
    let provider = ProviderKind::Claude;
    let (protected, legacy, rebind) = (6_325_580_u64, 6_325_581_u64, 6_325_582_u64);
    bind_channels(tmp.path(), [protected, legacy]);
    let shared = make_shared_data_for_tests();
    let token = shared.token_hash.clone();
    let generation = shared.restart.current_generation;
    for channel in [protected, legacy] {
        let id = ChannelId::new(channel);
        let alt = Some(channel + 100);
        crate::services::turn_orchestrator::save_channel_queue(
            &provider,
            &token,
            id,
            &[item(channel + 1)],
            alt,
        )
        .unwrap();
        crate::services::turn_orchestrator::save_channel_pending_dispatch_marker(
            &provider,
            &token,
            id,
            &item(channel + 2),
            alt,
        )
        .unwrap();
        super::super::queued_placeholders_store::save_channel_queued_placeholders(
            &provider,
            &token,
            id,
            &[(MessageId::new(channel + 3), MessageId::new(channel + 4))],
        );
    }
    let mut stale = inflight::InflightTurnState::new(
        provider.clone(),
        protected,
        None,
        42,
        protected + 5,
        protected + 5,
        "prompt".into(),
        None,
        None,
        None,
        None,
        0,
    );
    stale.restart_generation = Some(generation.wrapping_sub(1));
    assert!(inflight::save_inflight_state_if_absent(&stale).unwrap());
    let mut rebound = inflight::InflightTurnState::new(
        provider.clone(),
        rebind,
        None,
        0,
        0,
        0,
        String::new(),
        None,
        None,
        None,
        None,
        0,
    );
    rebound.rebind_origin = true;
    assert!(inflight::save_inflight_state_if_absent(&rebound).unwrap());
    let root = input_runtime::fence::population_root().unwrap();
    let before = [protected, rebind].map(|channel| files_mentioning(&root, channel));
    let names: Vec<_> = before[0]
        .iter()
        .map(|(path, _)| path.strip_prefix(&root).unwrap())
        .collect();
    assert_eq!(
        names.len(),
        4,
        "queue, marker, placeholder and row: {names:?}"
    );
    let gates =
        [protected, rebind].map(|channel| Gate::protect(provider.clone(), channel).unwrap());
    let _health = gates
        .each_ref()
        .map(input_runtime::fence::test_health::Clear::new);

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
    assert!(
        !live.contains_key(&ChannelId::new(protected)),
        "nothing merged: {live:?}"
    );
    let overrides = &shared.dispatch.role_overrides;
    assert!(overrides.contains_key(&ChannelId::new(legacy)));
    assert!(
        !overrides.contains_key(&ChannelId::new(protected)),
        "no override installed"
    );
    assert!(stale_cards.iter().any(|card| card.0.get() == legacy));
    assert!(
        stale_cards.iter().all(|card| card.0.get() != protected),
        "no placeholder judged: {stale_cards:?}"
    );
    let after = [protected, rebind].map(|channel| files_mentioning(&root, channel));
    assert_eq!(after, before, "protected files byte-identical");
    for channel in [protected, rebind] {
        assert!(
            !input_runtime::health_reasons()
                .iter()
                .any(|reason| reason.contains(&format!("channel={channel}"))),
            "no effect on channel {channel} was attempted"
        );
    }
}
