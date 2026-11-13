use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_o::cutover::test_override;

fn fixture(channel: u64) -> Initialized {
    serde_json::from_value(serde_json::json!({
        "channel": channel, "sources": [], "initial_anchor": 100,
        "build_digest": "fixture", "at": "2026-11-12T00:00:00Z"
    }))
    .unwrap()
}

#[test]
fn thread_children_old_init_bytes_preserved() {
    let init = fixture(42);
    let old = br#"{"channel":42,"sources":[],"initial_anchor":100,"build_digest":"fixture","at":"2026-11-12T00:00:00Z"}"#;
    assert_eq!(serde_json::to_vec(&init).unwrap(), old);
}

#[test]
fn thread_children_recorded_parent_survives_reader() {
    let mut json = serde_json::to_value(fixture(43)).unwrap();
    json["policy_parent"] = serde_json::json!(42);
    let init: Initialized = serde_json::from_value(json).unwrap();
    assert_eq!(serde_json::to_value(init).unwrap()["policy_parent"], 42);
}

#[test]
fn thread_children_outside_config_without_parent_writes_nothing() {
    let _boot = test_override::force_channels(&[(42, RuntimeHandoffKind::ClaudeTui)]);
    let dir = tempfile::tempdir().unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, dir.path())
        .unwrap()
        .unwrap();
    let scope = crate::services::tui_o::cutover::boot_snapshot()
        .unwrap()
        .init_scope_view();
    assert!(
        store
            .init_channel_as(&fixture(43), InitOrigin::Fresh, &scope)
            .is_err()
    );
    assert!(!store.has_channel_dir(43));
    assert!(!dir.path().join("o_store/thread_children.floor").exists());
}

#[test]
fn thread_children_old_reader_seeded_error_is_a_boot_compatibility_hazard() {
    use crate::services::tui_o::channel_policy::{self, BootChannels};
    let root = tempfile::tempdir().unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, root.path())
        .unwrap()
        .unwrap();
    let channel = root.path().join("o_store/43");
    std::fs::create_dir(&channel).unwrap();
    let mut record = serde_json::to_value(fixture(43)).unwrap();
    record["policy_parent"] = serde_json::json!(42);
    std::fs::write(channel.join("init"), serde_json::to_vec(&record).unwrap()).unwrap();
    let config = serde_json::from_value(serde_json::json!({
        "server":{}, "agents":[{"id":"a","name":"A","channels":{"claude":{"id":"42","runtime":"tui"}}}],
        "tui_o":{"writer":{"all_tui":false,"channels":[42]}}
    })).unwrap();
    let error = BootChannels::validate(&config)
        .unwrap()
        .seeded(&config, |selected, _| {
            let committed = store.channels_with_init()?;
            let channels = selected.union(&committed).copied().collect();
            Ok(channel_policy::stored(Some(root.path()), &channels))
        })
        .unwrap_err();
    assert!(format!("{error:#}").contains("channel 43 is not registered"));
}
fn boot() -> crate::services::tui_o::channel_policy::BootChannels {
    let config = serde_json::from_value(serde_json::json!({
        "server": {}, "agents": [{"id":"a","name":"A","channels":{"claude":{"id":"42","runtime":"tui"}}}],
        "tui_o":{"writer":{"all_tui":false,"channels":[42]}}
    })).unwrap();
    crate::services::tui_o::channel_policy::BootChannels::validate(&config).unwrap()
}

fn surface(
    kind: poise::serenity_prelude::ChannelType,
) -> Option<crate::services::session_host::surface_policy::VerifiedSurface> {
    let channel = serde_json::from_value(serde_json::json!({
        "id": "43", "parent_id": "42", "type": kind, "name": "fixture"
    }))
    .unwrap();
    crate::services::session_host::surface_policy::VerifiedSurface::from_discord(
        &channel,
        RuntimeHandoffKind::ClaudeTui,
    )
}

fn init_child(store: &OStore, parent: bool, enabled: bool, proof: bool) -> Result<(), StoreError> {
    let mut init = fixture(43);
    if parent {
        init.policy_parent = Some(42);
    }
    let verified = surface(poise::serenity_prelude::ChannelType::PublicThread).unwrap();
    store.init_channel_checked(
        &init,
        InitOrigin::Fresh,
        &boot().init_scope_view(),
        proof.then_some(&verified),
        enabled,
    )
}

#[test]
fn thread_children_only_verified_discord_threads_can_publish() {
    use poise::serenity_prelude::ChannelType;
    assert!(surface(ChannelType::Text).is_none());
    assert!(surface(ChannelType::Category).is_none());
    for kind in [
        ChannelType::PublicThread,
        ChannelType::PrivateThread,
        ChannelType::NewsThread,
    ] {
        assert!(surface(kind).is_some());
    }
    for (parent, enabled, proof) in [
        (false, true, true),
        (true, false, true),
        (true, true, false),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, dir.path())
            .unwrap()
            .unwrap();
        assert!(init_child(&store, parent, enabled, proof).is_err());
        assert_eq!(
            std::fs::read_dir(dir.path().join("o_store"))
                .unwrap()
                .count(),
            0
        );
    }
}

#[test]
fn thread_children_floor_precedes_init_and_survives_init_failure() {
    let dir = tempfile::tempdir().unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, dir.path())
        .unwrap()
        .unwrap();
    // Fail the first attempt to lay out the child: the floor must already be durable.
    let child = dir.path().join("o_store/43");
    std::fs::write(&child, b"blocked").unwrap();
    assert!(init_child(&store, true, true, true).is_err());
    assert_eq!(
        std::fs::read(dir.path().join("o_store/thread_children.floor")).unwrap(),
        b"1\n"
    );
    std::fs::remove_file(child).unwrap();
    init_child(&store, true, true, true).unwrap();
    assert_eq!(
        store.read_init(43).unwrap().unwrap().policy_parent,
        Some(42)
    );
}

#[test]
fn thread_children_floor_failure_prevents_all_init_writes() {
    let dir = tempfile::tempdir().unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, dir.path())
        .unwrap()
        .unwrap();
    let marker = dir.path().join("o_store/thread_children.floor");
    let _fault = fault::plant(
        &dir.path().join("o_store"),
        fault::Step::Write,
        io::ErrorKind::StorageFull,
        None,
    );
    assert!(init_child(&store, true, true, true).is_err());
    assert!(!store.has_channel_dir(43));
    assert!(!marker.exists());
}

#[test]
fn thread_children_malformed_floor_is_not_replaced() {
    for bytes in [b"".as_slice(), b"2\n", b"garbage"] {
        let dir = tempfile::tempdir().unwrap();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, dir.path())
            .unwrap()
            .unwrap();
        let marker = dir.path().join("o_store/thread_children.floor");
        std::fs::write(&marker, bytes).unwrap();
        assert!(init_child(&store, true, true, true).is_err());
        assert!(!store.has_channel_dir(43));
        assert_eq!(std::fs::read(&marker).unwrap(), bytes);
    }
}

#[test]
fn thread_children_production_capability_keeps_init_floor_writer_counts_zero() {
    let _boot = test_override::force_channels(&[(42, RuntimeHandoffKind::ClaudeTui)]);
    let dir = tempfile::tempdir().unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, dir.path())
        .unwrap()
        .unwrap();
    let mut init = fixture(43);
    init.policy_parent = Some(42);
    let scope = crate::services::tui_o::cutover::boot_snapshot()
        .unwrap()
        .init_scope_view();
    assert!(
        store
            .init_channel_as(&init, InitOrigin::Fresh, &scope)
            .is_err()
    );
    assert!(
        store
            .begin_era_as(&init, InitOrigin::Fresh, &scope)
            .is_err()
    );
    assert_eq!(store.channels_with_init().unwrap().len(), 0);
    assert!(store.read_era().unwrap().is_none());
    assert!(!dir.path().join("o_store/thread_children.floor").exists());
    assert!(
        crate::services::tui_o::cutover::boot_ownership()
            .iter()
            .all(|(id, _, _)| *id != 43)
    );
}

#[test]
fn thread_children_concurrent_floor_publication_is_idempotent() {
    let dir = std::sync::Arc::new(tempfile::tempdir().unwrap());
    let store = std::sync::Arc::new(
        OStore::open_if_enabled(&StoreConfig { enabled: true }, dir.path())
            .unwrap()
            .unwrap(),
    );
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut workers = Vec::new();
    for _ in 0..2 {
        let (store, barrier, root) = (store.clone(), barrier.clone(), dir.clone());
        workers.push(std::thread::spawn(move || {
            let _root = root;
            barrier.wait();
            init_child(&store, true, true, true).is_ok()
        }));
    }
    let successes: usize = workers
        .into_iter()
        .map(|worker| {
            usize::from(crate::test_support::wait::join_within(
                worker,
                std::time::Duration::from_secs(5),
            ))
        })
        .sum();
    assert_eq!(successes, 1);
    assert_eq!(
        std::fs::read(dir.path().join("o_store/thread_children.floor")).unwrap(),
        b"1\n"
    );
    assert_eq!(
        store.read_init(43).unwrap().unwrap().policy_parent,
        Some(42)
    );
}

#[test]
fn thread_children_raw_parent_and_wrong_provider_are_rejected_before_floor() {
    let dir = tempfile::tempdir().unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, dir.path())
        .unwrap()
        .unwrap();
    let valid = surface(poise::serenity_prelude::ChannelType::PublicThread).unwrap();
    for parent in [0, 43, 99] {
        let mut init = fixture(43);
        init.policy_parent = Some(parent);
        assert!(
            store
                .init_channel_checked(
                    &init,
                    InitOrigin::Fresh,
                    &boot().init_scope_view(),
                    Some(&valid),
                    true
                )
                .is_err()
        );
    }
    let channel = serde_json::from_value(serde_json::json!({
        "id": "43", "parent_id": "42", "type": 10, "name": "fixture"
    }))
    .unwrap();
    let wrong = crate::services::session_host::surface_policy::VerifiedSurface::from_discord(
        &channel,
        RuntimeHandoffKind::CodexTui,
    )
    .unwrap();
    let mut init = fixture(43);
    init.policy_parent = Some(42);
    assert!(
        store
            .init_channel_checked(
                &init,
                InitOrigin::Fresh,
                &boot().init_scope_view(),
                Some(&wrong),
                true
            )
            .is_err()
    );
    assert_eq!(
        std::fs::read_dir(dir.path().join("o_store"))
            .unwrap()
            .count(),
        0
    );
}

#[cfg(unix)]
#[test]
fn thread_children_symlink_floor_never_authorizes_init() {
    let dir = tempfile::tempdir().unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, dir.path())
        .unwrap()
        .unwrap();
    let target = dir.path().join("target");
    std::fs::write(&target, b"1\n").unwrap();
    std::os::unix::fs::symlink(&target, dir.path().join("o_store/thread_children.floor")).unwrap();
    assert!(init_child(&store, true, true, true).is_err());
    assert!(!store.has_channel_dir(43));
    assert_eq!(std::fs::read(target).unwrap(), b"1\n");
}

#[test]
fn thread_children_raw_metadata_without_verified_surface_writes_zero() {
    let dir = tempfile::tempdir().unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, dir.path())
        .unwrap()
        .unwrap();
    assert!(init_child(&store, true, true, false).is_err());
    assert_eq!(
        std::fs::read_dir(dir.path().join("o_store"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn thread_children_floor_only_does_not_enter_the_old_init_enumeration() {
    let dir = tempfile::tempdir().unwrap();
    let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, dir.path())
        .unwrap()
        .unwrap();
    std::fs::write(dir.path().join("o_store/thread_children.floor"), b"1\n").unwrap();
    assert!(store.channels_with_init().unwrap().is_empty());
}
