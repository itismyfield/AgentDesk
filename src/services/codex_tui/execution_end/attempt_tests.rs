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

#[test]
fn typed_calling_attempt_restores_nested_scope_and_hides_it_from_headless_dispatch() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    attempt::reset_for_tests();
    let names = panes();
    let (first, second) = (&names[0], &names[1]);
    let outer = attempt::begin(Some(CHANNEL), Some(first));
    let outer_id = attempt::current(CHANNEL, first).expect("outer calling token");
    assert_eq!(outer_id.channel(), CHANNEL);
    assert_eq!(outer_id.pane(), first);
    assert_eq!(
        Some(outer_id.root().to_path_buf()),
        crate::config::runtime_root()
    );
    assert!(attempt::current(CHANNEL + 1, first).is_none());
    assert!(attempt::current(CHANNEL, second).is_none());
    {
        let _inner = attempt::begin(Some(CHANNEL), Some(second));
        assert!(attempt::current(CHANNEL, first).is_none());
        let inner = attempt::current(CHANNEL, second).expect("inner calling token");
        assert_ne!(inner, outer_id);
        assert!(
            std::thread::spawn({
                let pane = second.clone();
                move || attempt::current(CHANNEL, &pane)
            })
            .join()
            .unwrap()
            .is_none(),
            "TLS token must stay on its dispatch thread"
        );
    }
    assert_eq!(attempt::current(CHANNEL, first), Some(outer_id.clone()));
    {
        let _headless = attempt::begin(None, None);
        assert!(attempt::current(CHANNEL, first).is_none());
    }
    assert_eq!(attempt::current(CHANNEL, first), Some(outer_id));
    drop(outer);
    assert!(attempt::current(CHANNEL, first).is_none());
    attempt::reset_for_tests();
}

#[test]
fn first_dispatch_claim_is_typed_once_and_cannot_leak_through_a_resume_or_refusal() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    attempt::reset_for_tests();
    let pane = &panes()[0];
    assert!(install(pane, generation(pane), "first-end"));
    {
        let _dispatch = attempt::begin(Some(CHANNEL), Some(pane));
        let id = attempt::current(CHANNEL, pane).unwrap();
        assert_eq!(
            attempt::ticket(CHANNEL, pane),
            None,
            "begin must consume its ticket"
        );
        assert_eq!(attempt::take_ticket(&id).as_deref(), Some("first-end"));
        assert_eq!(attempt::take_ticket(&id), None);
    }
    {
        let _later_fresh = attempt::begin(Some(CHANNEL), Some(pane));
        let id = attempt::current(CHANNEL, pane).unwrap();
        assert_eq!(
            attempt::take_ticket(&id),
            None,
            "later fresh must not reuse first dispatch's End"
        );
    }
    assert!(install(pane, generation(pane), "refused-end"));
    drop(attempt::begin(Some(CHANNEL), Some(pane)));
    let _later = attempt::begin(Some(CHANNEL), Some(pane));
    let id = attempt::current(CHANNEL, pane).unwrap();
    assert_eq!(
        attempt::take_ticket(&id),
        None,
        "a refusal still consumes its first attempt"
    );
}

#[test]
fn overlapping_dispatch_erases_the_old_claim_and_never_substitutes_the_newest_token() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    attempt::reset_for_tests();
    let pane = &panes()[0];
    assert!(install(pane, generation(pane), "overlapped-end"));
    let first = attempt::begin(Some(CHANNEL), Some(pane));
    let first_id = attempt::current(CHANNEL, pane).unwrap();
    assert_eq!(
        attempt::claim_for_tests(&first_id).as_deref(),
        Some("overlapped-end")
    );
    let second = attempt::begin(Some(CHANNEL), Some(pane));
    let second_id = attempt::current(CHANNEL, pane).unwrap();
    assert_ne!(first_id, second_id);
    assert_eq!(
        attempt::claim_for_tests(&first_id),
        None,
        "overlap must erase the stale claim"
    );
    assert_eq!(attempt::take_ticket(&second_id), None);
    drop(second);
    assert_eq!(
        attempt::current(CHANNEL, pane),
        Some(first_id.clone()),
        "caller retains its exact stale token"
    );
    tc::with_tmux_source_authority(pane, |authority| {
        assert!(
            !attempt::bound_under(&first_id, authority, "late-nonce"),
            "epoch rejects delayed old dispatch after overlap ends"
        );
    });
    drop(first);
    assert!(attempt::current(CHANNEL, pane).is_none());
}

#[test]
fn submitted_bound_stream_allows_clear_ticket_while_mayspawn_does_not() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    attempt::reset_for_tests();
    let pane = &panes()[0];
    let dispatch = attempt::begin(Some(CHANNEL), Some(pane));
    let id = attempt::current(CHANNEL, pane).unwrap();
    let epoch = generation(pane);
    assert!(!install(pane, epoch, "before-submit"));
    tc::with_tmux_source_authority(pane, |authority| {
        assert_eq!(
            attempt::phase_under(&id, authority),
            Some(attempt::Phase::MaySpawn)
        );
        assert!(attempt::bound_under(&id, authority, "submitted-nonce"));
        assert_eq!(
            attempt::phase_under(&id, authority),
            Some(attempt::Phase::Bound {
                nonce: "submitted-nonce".into()
            })
        );
        assert!(!attempt::bound_under(&id, authority, "another-nonce"));
    });
    assert!(install(pane, epoch, "during-bound-tail"));
    assert_eq!(
        attempt::ticket(CHANNEL, pane).as_deref(),
        Some("during-bound-tail")
    );
    drop(dispatch);
    tc::with_tmux_source_authority(pane, |authority| {
        assert_eq!(
            attempt::phase_under(&id, authority),
            Some(attempt::Phase::Done)
        );
    });
    let _next = attempt::begin(Some(CHANNEL), Some(pane));
    let next_id = attempt::current(CHANNEL, pane).unwrap();
    assert_eq!(
        attempt::take_ticket(&next_id).as_deref(),
        Some("during-bound-tail")
    );
}

#[test]
fn bound_submit_discards_an_unclaimed_clear_ticket_without_waiting_for_tail_completion() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    attempt::reset_for_tests();
    let pane = &panes()[0];
    assert!(install(pane, generation(pane), "warm-resume-end"));
    let _dispatch = attempt::begin(Some(CHANNEL), Some(pane));
    let id = attempt::current(CHANNEL, pane).unwrap();
    assert_eq!(
        attempt::claim_for_tests(&id).as_deref(),
        Some("warm-resume-end")
    );
    tc::with_tmux_source_authority(pane, |authority| {
        assert!(attempt::bound_under(&id, authority, "existing-execution"));
    });
    assert_eq!(attempt::claim_for_tests(&id), None);
    assert_eq!(attempt::take_ticket(&id), None);
}
