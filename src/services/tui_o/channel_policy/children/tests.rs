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
    // The recorded child is hosted under its own adoption; it borrows no parent permit.
    let owned = crate::services::tui_o::cutover::boot_ownership();
    assert_eq!(owned.len(), 3);
    let (_, kind, candidate) = owned.iter().find(|(id, _, _)| *id == 43).unwrap();
    assert_eq!(*kind, Some(RuntimeHandoffKind::ClaudeTui));
    assert_eq!(candidate.as_ref().unwrap().peek(), Adoption::Committed);
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
    assert!(matches!(sent, BodySend::OwnedByO));
    assert_eq!(sends, 0, "Legacy sends no body for a child O owns");
    assert!(
        crate::services::tui_o::cutover::peek_o_owns_tui_output_for_channel(
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

fn thread(
    channel: u64,
    parent: u64,
) -> crate::services::session_host::surface_policy::VerifiedSurface {
    let channel = serde_json::from_value(serde_json::json!({
        "id": channel.to_string(), "parent_id": parent.to_string(), "type": 11, "name": "fixture"
    }))
    .unwrap();
    crate::services::session_host::surface_policy::VerifiedSurface::from_discord(
        &channel,
        RuntimeHandoffKind::ClaudeTui,
    )
    .unwrap()
}

fn unresolved(boot: &BootChannels, channel: u64, boot_discovered: bool) {
    boot.children.entries.lock().unwrap().insert(
        channel,
        ChildEntry {
            parent: None,
            kind: None,
            candidate: None,
            held: true,
            conflict: false,
            record_unreadable: false,
            boot_discovered,
        },
    );
}

/// Whether Legacy sent a body offered for `channel` at a census body site.
async fn legacy_sent(
    channel: u64,
    kind: RuntimeHandoffKind,
) -> Result<bool, crate::services::tui_o::cutover::IdentityError> {
    use crate::services::tui_o::cutover::{BodyClaim, BodySend, BodySite, claim_then_send_at};
    let claim = Some(BodyClaim::new(channel, Some(kind)));
    let sent = claim_then_send_at(BodySite::BridgeTerminal, claim, || async {}).await?;
    Ok(matches!(sent, BodySend::Sent(())))
}

#[tokio::test]
async fn thread_children_gate_never_hands_an_unresolved_or_undecided_child_to_legacy() {
    use crate::services::tui_o::cutover::IdentityError;
    use RuntimeHandoffKind::{ClaudeTui, CodexTui};
    let boot = BootChannels::validate(&config())
        .unwrap()
        .adopted(Adoption::Committed);
    boot.children
        .record(&boot, &init(43, Some(42)), Adoption::Committed);
    boot.children
        .record(&boot, &init(45, Some(42)), Adoption::Committed);
    boot.children
        .record(&boot, &init(45, Some(44)), Adoption::Committed);
    for (channel, boot_discovered) in [(46, true), (47, false), (48, true), (49, true)] {
        unresolved(&boot, channel, boot_discovered);
    }
    let _forced = test_override::force_boot(boot.clone());
    assert_eq!(
        legacy_sent(46, ClaudeTui).await,
        Err(IdentityError::ChildHeld)
    );
    boot.children
        .resolve_legacy(&boot, |channel| match channel {
            46 => Err(()),
            49 => Ok(thread(49, 99)),
            channel => Ok(thread(channel, 42)),
        });
    // Unresolved and conflicting children are held; a kind other than the parent's is refused.
    assert_eq!(
        legacy_sent(46, ClaudeTui).await,
        Err(IdentityError::ChildHeld)
    );
    assert_eq!(
        legacy_sent(45, ClaudeTui).await,
        Err(IdentityError::ChildHeld)
    );
    assert!(matches!(
        legacy_sent(43, CodexTui).await,
        Err(IdentityError::KindMismatch { .. })
    ));
    // An undecided adoption is held as it is: the claim does not release it to Legacy.
    let undecided = boot.child(47).unwrap().unwrap().candidate;
    assert_eq!(
        legacy_sent(47, ClaudeTui).await,
        Err(IdentityError::ChildHeld)
    );
    assert_eq!(undecided.peek(), Adoption::Pending);
    // A late reservation leaves its open turn to Legacy and counts that send.
    let late = boot.child(48).unwrap().unwrap().candidate;
    assert_eq!(legacy_sent(48, ClaudeTui).await, Ok(true));
    assert_eq!((late.peek(), late.sends()), (Adoption::Deferred, (1, 1)));
    // A thread whose parent O does not select stays an ordinary Legacy channel.
    assert!(boot.child(49).is_none());
    assert_eq!(legacy_sent(49, ClaudeTui).await, Ok(true));
    // The parent and the sibling keep their own state throughout.
    assert_eq!(legacy_sent(43, ClaudeTui).await, Ok(false));
    assert_eq!(boot.candidate(42).unwrap().peek(), Adoption::Committed);
    assert_eq!(boot.candidate(42).unwrap().sends(), (0, 0));
}

#[test]
fn thread_children_intake_holds_a_child_until_its_own_writer_is_ready() {
    use crate::services::tui_o::cutover::intake_route::{self, IntakeRoute};
    const PARENT: u64 = 8_118_142;
    let config: crate::config::Config = serde_json::from_value(serde_json::json!({
        "server": {},
        "agents": [{"id":"a","name":"A","channels":{"claude":{"id":PARENT.to_string(),"runtime":"tui"}}}],
        "tui_o":{"writer":{"all_tui":false,"channels":[PARENT]}}
    }))
    .unwrap();
    let boot = BootChannels::validate(&config)
        .unwrap()
        .adopted(Adoption::Committed);
    let (owned, held, undecided, late) = (PARENT + 1, PARENT + 2, PARENT + 3, PARENT + 4);
    boot.children
        .record(&boot, &init(owned, Some(PARENT)), Adoption::Committed);
    unresolved(&boot, held, true);
    unresolved(&boot, undecided, false);
    unresolved(&boot, late, true);
    boot.children.resolve_legacy(&boot, |channel| {
        (channel != held).then(|| thread(channel, PARENT)).ok_or(())
    });
    let _forced = test_override::force_boot(boot.clone());
    let hold = |channel| matches!(intake_route::route("claude", channel), IntakeRoute::Hold(_));
    // No writer hosts these ids in this process, so an O-owned child waits instead of placing.
    assert!(hold(owned) && hold(held) && hold(undecided));
    assert_eq!(intake_route::route("claude", late), IntakeRoute::Unselected);
    assert!(matches!(
        intake_route::route_for_placement("claude", undecided),
        IntakeRoute::Hold(_)
    ));
    let adoption = boot.child(undecided).unwrap().unwrap().candidate.peek();
    assert_eq!(adoption, Adoption::Pending, "a placement releases no child");
    let waiting = intake_route::held_channels("claude");
    assert!(waiting.contains(&owned.to_string()) && waiting.contains(&undecided.to_string()));
    assert!(waiting.contains(&held.to_string()));
    assert!(!waiting.contains(&late.to_string()));
}

#[test]
fn thread_children_resolution_reads_outside_the_entry_lock_and_rechecks_the_entry() {
    let boot = BootChannels::validate(&config()).unwrap();
    unresolved(&boot, 46, true);
    let mut reads = 0;
    boot.children.resolve_legacy(&boot, |channel| {
        reads += 1;
        assert!(
            boot.children.entries.try_lock().is_ok(),
            "the checked read holds no entry lock"
        );
        // A conflicting record lands while the read is in flight.
        boot.children
            .record(&boot, &init(channel, Some(44)), Adoption::Committed);
        Ok(thread(channel, 42))
    });
    assert_eq!(reads, 1);
    assert!(matches!(boot.child(46), Some(Err(ChildHeld))));
    assert!(boot.children.entries.lock().unwrap()[&46].parent.is_none());
}
