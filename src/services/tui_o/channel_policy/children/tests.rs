use super::*;
use crate::services::tui_o::cutover::test_override;

fn config() -> crate::config::Config {
    serde_json::from_value(serde_json::json!({
        "server": {},
        "agents": [{"id":"a","name":"A","channels":{"claude":{"id":"42","runtime":"tui"},"codex":{"id":"44","runtime":"tui"}}}],
        "tui_o":{"writer":{"all_tui":false,"channels":[42,44]}}
    })).unwrap()
}

fn init(channel: u64, parent: Option<u64>) -> Initialized {
    serde_json::from_value(serde_json::json!({
        "channel":channel,"policy_parent":parent,"sources":[],"initial_anchor":100,
        "build_digest":"fixture","at":"2026-11-12T00:00:00Z"
    }))
    .unwrap()
}

fn store(root: &Path, init: &Initialized) {
    let channel = root.join("o_store").join(init.channel.to_string());
    std::fs::create_dir_all(&channel).unwrap();
    std::fs::write(channel.join("init"), serde_json::to_vec(init).unwrap()).unwrap();
}

#[tokio::test]
async fn thread_children_recorded_store_only_boot_and_refresh_need_no_rest() {
    let config = config();
    let root = tempfile::tempdir().unwrap();
    store(root.path(), &init(43, Some(42)));
    let boot = seeded_from_store(
        BootChannels::validate(&config).unwrap(),
        &config,
        Some(root.path()),
    )
    .unwrap();
    let clone = boot.clone();
    let entry = clone
        .children
        .entries
        .lock()
        .unwrap()
        .get(&43)
        .unwrap()
        .clone();
    assert_eq!(entry.parent, Some(42));
    assert_eq!(entry.kind, Some(RuntimeHandoffKind::ClaudeTui));
    assert_eq!(entry.candidate.unwrap().peek(), Adoption::Committed);
    assert!(!entry.held);
    boot.children.refresh_records(&boot, Some(root.path()));
    assert!(!clone.children.entries.lock().unwrap()[&43].held);
    assert!(!boot.channels().contains(&43));
    assert!(boot.execution(43).is_none());
    let _forced = test_override::force_boot(boot);
    // The O-a view never grants child output membership or borrows a parent permit.
    assert_eq!(crate::services::tui_o::cutover::boot_ownership().len(), 2);
    use crate::services::tui_o::cutover::{BodyClaim, BodySend, BodySite, claim_then_send_at};
    let mut sends = 0;
    let sent = claim_then_send_at(
        BodySite::WatcherTerminal,
        Some(BodyClaim::new(43, Some(RuntimeHandoffKind::ClaudeTui))),
        || async {
            sends += 1;
        },
    )
    .await
    .unwrap();
    assert!(matches!(sent, BodySend::Sent(())));
    assert_eq!(sends, 1);
    assert!(
        !crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel(
            43,
            Some(RuntimeHandoffKind::ClaudeTui)
        )
        .unwrap()
    );
    assert!(!root.path().join("o_store/thread_children.floor").exists());
}

#[test]
fn thread_children_unrecorded_committed_id_keeps_old_config_error() {
    let config = config();
    let root = tempfile::tempdir().unwrap();
    store(root.path(), &init(43, None));
    let error = seeded_from_store(
        BootChannels::validate(&config).unwrap(),
        &config,
        Some(root.path()),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("channel 43 is not registered"));
}

#[test]
fn thread_children_old_reader_rejects_recorded_thread_init() {
    let config = config();
    let root = tempfile::tempdir().unwrap();
    store(root.path(), &init(43, Some(42)));
    // Preserve the old channels_with_init -> stored_with_committed -> seeded path exactly.
    let error = BootChannels::validate(&config)
        .unwrap()
        .seeded(&config, |channels, _| {
            super::super::adoption::stored_with_committed(Some(root.path()), channels)
        })
        .unwrap_err();
    assert!(format!("{error:#}").contains("channel 43 is not registered"));
}

#[test]
fn thread_children_snapshot_clones_share_atomic_idempotent_entries() {
    let boot = BootChannels::validate(&config())
        .unwrap()
        .adopted(Adoption::Committed);
    let cloned_before = boot.clone();
    std::thread::scope(|scope| {
        for _ in 0..8 {
            let boot = &boot;
            scope.spawn(move || {
                for _ in 0..20 {
                    boot.children
                        .record(boot, &init(43, Some(42)), Adoption::Committed);
                }
            });
        }
    });
    let first = cloned_before.children.entries.lock().unwrap()[&43]
        .candidate
        .clone()
        .unwrap();
    *first.lock() = Adoption::Held;
    boot.children
        .record(&boot, &init(43, Some(42)), Adoption::Committed);
    let entries = boot.children.entries.lock().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[&43].candidate.as_ref().unwrap().peek(),
        Adoption::Held
    );
    drop(entries);
    // A sibling and parent have their own state, even after a child conflict.
    boot.children
        .record(&boot, &init(45, Some(42)), Adoption::Committed);
    boot.children
        .record(&boot, &init(43, Some(44)), Adoption::Committed);
    let entries = cloned_before.children.entries.lock().unwrap();
    assert!(entries[&43].held);
    assert_eq!(entries[&43].parent, Some(42));
    assert_eq!(boot.candidate(42).unwrap().peek(), Adoption::Committed);
    assert_eq!(
        entries[&45].candidate.as_ref().unwrap().peek(),
        Adoption::Committed
    );
}

#[test]
fn thread_children_missing_refresh_record_holds_same_candidate() {
    let root = tempfile::tempdir().unwrap();
    let boot = BootChannels::validate(&config()).unwrap();
    store(root.path(), &init(43, Some(42)));
    boot.children
        .record(&boot, &init(43, Some(42)), Adoption::Committed);
    let first = boot.children.entries.lock().unwrap()[&43]
        .candidate
        .clone()
        .unwrap();
    std::fs::write(root.path().join("o_store/43/init"), b"damaged").unwrap();
    boot.children.refresh_records(&boot, Some(root.path()));
    assert_eq!(first.peek(), Adoption::Held);
    assert!(boot.children.entries.lock().unwrap()[&43].held);
    store(root.path(), &init(43, Some(42)));
    boot.children.refresh_records(&boot, Some(root.path()));
    assert_eq!(first.peek(), Adoption::Committed);
    assert!(!boot.children.entries.lock().unwrap()[&43].held);
}

#[test]
fn thread_children_unresolved_non_o_reader_waits_for_existing_refresh_then_resolves_once() {
    let boot = BootChannels::validate(&config()).unwrap();
    let clone = boot.clone();
    boot.children.entries.lock().unwrap().insert(
        43,
        ChildEntry {
            parent: None,
            kind: None,
            candidate: None,
            held: true,
            conflict: false,
            record_unreadable: false,
            boot_discovered: true,
        },
    );
    // Isolated O-b reader contract: O-a does not apply Held to the production gate.
    let mut rest_calls = 0;
    clone.children.resolve_legacy(&boot, |_| {
        rest_calls += 1;
        Err(())
    });
    assert!(boot.children.entries.lock().unwrap()[&43].held);
    let channel = serde_json::from_value(serde_json::json!({
        "id": "43", "parent_id": "99", "type": 11, "name": "fixture"
    }))
    .unwrap();
    let proof = crate::services::session_host::surface_policy::VerifiedSurface::from_discord(
        &channel,
        RuntimeHandoffKind::ClaudeTui,
    )
    .unwrap();
    for _ in 0..2 {
        clone.children.resolve_legacy(&boot, |_| {
            rest_calls += 1;
            Ok(proof)
        });
    }
    let entries = boot.children.entries.lock().unwrap();
    assert_eq!(rest_calls, 2);
    assert!(!entries[&43].held);
    assert!(entries[&43].boot_discovered);
    assert!(entries[&43].candidate.is_none());
}

#[test]
fn thread_children_boot_candidates_carry_the_immutable_init_scope() {
    let boot = BootChannels::validate(&config())
        .unwrap()
        .adopted(Adoption::Committed);
    let first = boot.output_candidate(42).unwrap();
    let second = boot.output_candidate(44).unwrap();
    assert!(first.bound_init_scope());
    assert!(second.bound_init_scope());
    let one = first.init_scope(42).unwrap();
    let two = second.init_scope(44).unwrap();
    assert!(std::sync::Arc::ptr_eq(&one, &two));
    assert!(one.configured(42));
    assert!(!one.configured(43));
    assert_eq!(one.kind(42), Some(RuntimeHandoffKind::ClaudeTui));
    assert!(boot.output_candidate(43).is_none());
    *first.lock() = Adoption::Held;
    assert_eq!(second.peek(), Adoption::Committed);
}
