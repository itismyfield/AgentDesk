use super::attempt;
use crate::services::tmux_common as tc;

const SCOPES: usize = 64;
const CHANNEL: u64 = 7_410;

fn generation(pane: &str) -> u64 {
    tc::with_tmux_source_authority(pane, |authority| attempt::generation(CHANNEL, authority))
}

fn install(pane: &str, epoch: u64, end_id: &str) -> bool {
    tc::with_tmux_source_authority(pane, |authority| {
        attempt::install_ticket(CHANNEL, authority, epoch, end_id)
    })
}

fn panes() -> Vec<String> {
    let id = uuid::Uuid::new_v4().simple().to_string();
    (0..=SCOPES)
        .map(|index| format!("a1-cap-{id}-{index}"))
        .collect()
}

#[test]
fn saturated_registry_keeps_active_scopes_and_blocks_tickets_until_overflow_returns() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    attempt::reset_for_tests();
    let panes = panes();
    let mut active: Vec<_> = panes[..SCOPES]
        .iter()
        .map(|pane| attempt::begin(Some(CHANNEL), Some(pane)))
        .collect();
    assert_eq!(active.len(), SCOPES);
    let epochs: Vec<_> = panes[..SCOPES]
        .iter()
        .map(|pane| generation(pane))
        .collect();
    assert!(epochs.iter().all(|epoch| *epoch != 0));

    let overflow = attempt::begin(Some(CHANNEL), Some(&panes[SCOPES]));
    assert_eq!(
        generation(&panes[SCOPES]),
        0,
        "all 64 tracked scopes are active"
    );
    for (pane, epoch) in panes[..SCOPES].iter().zip(&epochs) {
        assert_eq!(
            generation(pane),
            *epoch,
            "overflow must not evict active {pane}"
        );
    }

    let idle = &panes[SCOPES - 1];
    let idle_epoch = epochs[SCOPES - 1];
    drop(active.pop().unwrap());
    assert_eq!(active.len(), SCOPES - 1);
    assert_eq!(generation(idle), idle_epoch);
    assert!(!install(idle, idle_epoch, "blocked-by-overflow"));
    assert_eq!(attempt::ticket(CHANNEL, idle), None);

    drop(overflow);
    assert!(install(idle, idle_epoch, "after-overflow"));
    assert_eq!(
        attempt::ticket(CHANNEL, idle).as_deref(),
        Some("after-overflow")
    );
    assert!(!install(&panes[0], epochs[0], "still-active"));
    drop(active);
    attempt::reset_for_tests();
}

#[test]
fn evicting_an_idle_ticket_never_allows_its_old_epoch_to_install_again() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    attempt::reset_for_tests();
    let panes = panes();
    let first = &panes[0];
    let old_epoch = generation(first);
    assert_ne!(old_epoch, 0);
    assert!(install(first, old_epoch, "old-end"));
    for pane in &panes[1..SCOPES] {
        let epoch = generation(pane);
        assert_ne!(epoch, 0);
        assert!(install(pane, epoch, "other-end"));
    }
    assert_eq!(attempt::ticket(CHANNEL, first).as_deref(), Some("old-end"));

    let newest = &panes[SCOPES];
    let newest_epoch = generation(newest);
    assert_ne!(newest_epoch, 0);
    assert!(install(newest, newest_epoch, "newest-end"));
    assert_eq!(attempt::ticket(CHANNEL, first), None);
    assert!(!install(first, old_epoch, "stale-end"));

    let replacement_epoch = generation(first);
    assert_ne!(replacement_epoch, old_epoch);
    assert_eq!(attempt::ticket(CHANNEL, first), None);
    assert!(!install(first, old_epoch, "stale-end"));
    assert!(install(first, replacement_epoch, "replacement-end"));
    assert_eq!(
        attempt::ticket(CHANNEL, first).as_deref(),
        Some("replacement-end")
    );
    attempt::reset_for_tests();
}

#[test]
fn process_restart_discards_pending_tickets_and_rejects_the_previous_generation() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    attempt::reset_for_tests();
    let pane = format!("a1-restart-{}", uuid::Uuid::new_v4().simple());
    let old_epoch = generation(&pane);
    assert_ne!(old_epoch, 0);
    assert!(install(&pane, old_epoch, "before-restart"));
    assert_eq!(
        attempt::ticket(CHANNEL, &pane).as_deref(),
        Some("before-restart")
    );

    attempt::reset_for_tests();
    assert_eq!(attempt::ticket(CHANNEL, &pane), None);
    assert!(!install(&pane, old_epoch, "restored-ticket"));
    let new_epoch = generation(&pane);
    assert_ne!(new_epoch, 0);
    assert_ne!(new_epoch, old_epoch);
    assert!(!install(&pane, old_epoch, "restored-ticket"));
    assert!(install(&pane, new_epoch, "after-restart"));
    assert_eq!(
        attempt::ticket(CHANNEL, &pane).as_deref(),
        Some("after-restart")
    );
    attempt::reset_for_tests();
}
