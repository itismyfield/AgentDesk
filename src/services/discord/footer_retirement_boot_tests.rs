use super::*;
use crate::services::discord::{
    inflight, runtime_store, status_panel_orphan_store as orphans,
    status_panel_singleton_store as singletons,
};
use crate::services::provider::ProviderKind;

fn seed(provider: &ProviderKind, token: &str, channel: u64, panel: u64) -> std::path::PathBuf {
    let path = runtime_store::discord_status_panel_singletons_root()
        .unwrap()
        .join(provider.as_str())
        .join(token)
        .join(format!("{channel}.json"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        serde_json::to_vec(&singletons::StatusPanelSingletonBinding {
            panel_message_id: panel,
            generation: 3,
        })
        .unwrap(),
    )
    .unwrap();
    path
}

#[test]
fn boot_retirement_queues_only_this_bot_completed_singletons_idempotently() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Claude;
    let binding = seed(&provider, "mine", 586_301, 101);
    let original = std::fs::read(&binding).unwrap();
    seed(&provider, "other-token", 586_302, 102);
    seed(&ProviderKind::Codex, "mine", 586_303, 103);
    assert_eq!(enqueue_completed_singletons(&provider, "mine"), 1);
    assert_eq!(enqueue_completed_singletons(&provider, "mine"), 1);
    assert_eq!(orphans::load_pending(&provider, "mine"), [(586_301, 101)]);
    assert!(orphans::load_pending(&provider, "other-token").is_empty());
    assert!(orphans::load_pending(&ProviderKind::Codex, "mine").is_empty());
    assert_eq!(
        std::fs::read(binding).unwrap(),
        original,
        "queueing does not add direct-delete or binding-clear authority"
    );
}

#[test]
fn boot_retirement_preserves_open_corrupt_and_lock_contended_turns() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Claude;
    let root = runtime_store::discord_inflight_root().unwrap();
    let mut state = inflight::InflightTurnState::new(
        provider.clone(),
        586_304,
        None,
        1,
        41,
        42,
        "open".into(),
        None,
        None,
        None,
        None,
        0,
    );
    state.status_message_id = Some(104);
    inflight::save_inflight_state(&state).unwrap();
    let open_path = inflight::inflight_state_path(&root, &provider, state.channel_id);
    let original = std::fs::read(&open_path).unwrap();
    seed(&provider, "mine", state.channel_id, 104);
    seed(&provider, "mine", 586_305, 105);
    std::fs::write(
        inflight::inflight_state_path(&root, &provider, 586_305),
        b"corrupt",
    )
    .unwrap();
    seed(&provider, "mine", 586_306, 106);
    let locked = inflight::inflight_state_path(&root, &provider, 586_306);
    let _lock = inflight::lock_inflight_state_path(&locked).unwrap();
    assert_eq!(enqueue_completed_singletons(&provider, "mine"), 0);
    assert!(orphans::load_pending(&provider, "mine").is_empty());
    assert_eq!(std::fs::read(open_path).unwrap(), original);
}

#[test]
fn boot_retirement_skips_malformed_zero_and_synthetic_panel_bindings() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Claude;
    let bad = seed(&provider, "mine", 586_307, 107);
    std::fs::write(bad, "malformed").unwrap();
    seed(&provider, "mine", 586_308, 0);
    seed(&provider, "mine", 586_309, 9_100_000_000_000_000_123);
    let directory = runtime_store::discord_status_panel_singletons_root()
        .unwrap()
        .join(provider.as_str())
        .join("mine");
    std::fs::write(
        directory.join("not-a-channel.json"),
        r#"{"panel_message_id":108,"generation":3}"#,
    )
    .unwrap();
    assert_eq!(enqueue_completed_singletons(&provider, "mine"), 0);
    assert!(orphans::load_pending(&provider, "mine").is_empty());
}

#[test]
fn boot_retirement_keeps_binding_when_orphan_queue_cannot_be_written() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let provider = ProviderKind::Claude;
    let binding = seed(&provider, "mine", 586_310, 110);
    let orphan_root = runtime_store::discord_status_panel_orphans_root().unwrap();
    std::fs::create_dir_all(orphan_root.parent().unwrap()).unwrap();
    std::fs::write(orphan_root, "not a directory").unwrap();
    assert_eq!(enqueue_completed_singletons(&provider, "mine"), 0);
    assert!(binding.exists());
}
